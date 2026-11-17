// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Auth;

/// <summary>
/// DEVLAUNCH (batch 1118): what keeps a development build away from the real launcher's state and Azimuth. A development build
/// (not <see cref="BuildProfile.ProductionOnly"/>) keeps every state file under its own root, <c>launcher-dev</c>, beside the
/// release launcher's <c>launcher</c>: its server choice, the PC's Azimuth location, sessions, logs and caches never touch the real
/// launcher's. It uses only the Azimuth folder its command line names with <c>--azimuth</c>, and a restart keeps its arguments.
/// </summary>
public static class DevelopmentLaunch
{
    /// <summary>The release launcher's state folder name and the development build's.</summary>
    public const string ReleaseStateFolder = "launcher", DevelopmentStateFolder = "launcher-dev";

    /// <summary>The state root under <paramref name="localAppData"/> for a release (<paramref name="productionBuild"/>) or a development build.</summary>
    public static string StateRootFor(string localAppData, bool productionBuild)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(localAppData);
        return Path.Combine(localAppData, "ForzaInfinity", productionBuild ? ReleaseStateFolder : DevelopmentStateFolder);
    }

    /// <summary>This build's state root (<c>%LOCALAPPDATA%\ForzaInfinity\launcher</c> or <c>…\launcher-dev</c>).</summary>
    public static string StateRoot =>
        StateRootFor(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), BuildProfile.ProductionOnly);

    /// <summary>The folder a development build was given with <c>--azimuth &lt;folder&gt;</c>, else null (it then uses none).</summary>
    public static string? AzimuthFromArguments(IReadOnlyList<string> arguments)
    {
        ArgumentNullException.ThrowIfNull(arguments);
        var at = arguments.ToList().IndexOf("--azimuth");
        return at >= 0 && at + 1 < arguments.Count && arguments[at + 1] is { Length: > 0 } folder && !folder.StartsWith("--", StringComparison.Ordinal)
            ? folder : null;
    }

    /// <summary>Whether <paramref name="folder"/> is the given <paramref name="pinned"/> folder (none given: no folder is).</summary>
    public static bool AllowsAzimuth(string? pinned, string? folder)
    {
        if (pinned is null || string.IsNullOrWhiteSpace(folder)) return false;
        try
        {
            return string.Equals(Path.TrimEndingDirectorySeparator(Path.GetFullPath(pinned)), Path.TrimEndingDirectorySeparator(Path.GetFullPath(folder)),
                StringComparison.OrdinalIgnoreCase);
        }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException)
        {
            return false;
        }
    }

    /// <summary>
    /// The arguments a restart on a new server choice starts with: the original ones, in order (the Azimuth folder, the locale, the
    /// environment, the server...). Left out are only the one-shot hand-offs that this launch already consumed (the updater's
    /// <c>--updated-transaction</c> / <c>--updated-nonce</c> pairs and an <c>infinity-horizon:</c> join link), and, when a custom
    /// server was chosen, the <c>--server</c> / <c>--portal</c> pairs: a development build's explicit <c>--server</c> wins over the
    /// saved choice (<see cref="ServerChoice.Apply"/>), so the choice would otherwise be lost.
    /// </summary>
    /// <remarks>
    /// Back from a custom server to "This build's server" (<paramref name="customChosen"/> false): when the arguments no longer name a
    /// server (that custom launch was restarted without them), <paramref name="commandLineServer"/> (<see cref="ReadCommandLineServer"/>)
    /// is added as <c>--server</c>/<c>--portal</c>, so the row always means the server the build was started with.
    /// </remarks>
    public static IReadOnlyList<string> RestartArguments(IReadOnlyList<string> original, bool customChosen, (Uri Server, Uri Portal)? commandLineServer = null)
    {
        ArgumentNullException.ThrowIfNull(original);
        var kept = Filtered(original, customChosen);
        if (!customChosen && commandLineServer is { } started && !kept.Contains("--server"))
        {
            kept.Add("--server"); kept.Add(started.Server.ToString());
            if (!kept.Contains("--portal")) { kept.Add("--portal"); kept.Add(started.Portal.ToString()); }
        }
        return kept;
    }

    /// <summary>The file in a development build's state root that keeps the server it was last started on (not a custom one).</summary>
    public const string CommandLineServerFile = "development-server.json";

    private sealed record SavedServer(string? Server, string? Portal);

    private static readonly System.Text.Json.JsonSerializerOptions Json = new(System.Text.Json.JsonSerializerDefaults.Web);

    /// <summary>Keeps the development server a launch started on (its command line's, or the build default when none was named).</summary>
    public static void SaveCommandLineServer(string root, Uri server, Uri portal)
    {
        ArgumentNullException.ThrowIfNull(server);
        ArgumentNullException.ThrowIfNull(portal);
        ServerSelection.Validate(server, portal, production: false);
        Directory.CreateDirectory(root);
        var path = Path.Combine(root, CommandLineServerFile);
        var temporary = path + ".tmp";
        File.WriteAllText(temporary, System.Text.Json.JsonSerializer.Serialize(new SavedServer(server.ToString(), portal.ToString()), Json));
        File.Move(temporary, path, overwrite: true);
    }

    /// <summary>The kept development server, or null (none kept, unreadable, or not a development server).</summary>
    public static (Uri Server, Uri Portal)? ReadCommandLineServer(string root)
    {
        try
        {
            var path = Path.Combine(root, CommandLineServerFile);
            if (!File.Exists(path)) return null;
            var saved = System.Text.Json.JsonSerializer.Deserialize<SavedServer>(File.ReadAllText(path), Json);
            if (saved is not { Server: { Length: > 0 } server, Portal: { Length: > 0 } portal }) return null;
            var (s, p) = (new Uri(server, UriKind.Absolute), new Uri(portal, UriKind.Absolute));
            ServerSelection.Validate(s, p, production: false);
            return (s, p);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.Text.Json.JsonException or ArgumentException or UriFormatException)
        {
            return null;
        }
    }

    private static List<string> Filtered(IReadOnlyList<string> original, bool customChosen)
    {
        var kept = new List<string>(original.Count);
        for (var i = 0; i < original.Count; i++)
        {
            var argument = original[i];
            if (argument is "--updated-transaction" or "--updated-nonce" || customChosen && argument is "--server" or "--portal")
            {
                if (i + 1 < original.Count && !original[i + 1].StartsWith("--", StringComparison.Ordinal)) i++;
                continue;
            }
            if (argument.StartsWith(Presence.JoinLink.Scheme + ":", StringComparison.OrdinalIgnoreCase)) continue;
            kept.Add(argument);
        }
        return kept;
    }
}
