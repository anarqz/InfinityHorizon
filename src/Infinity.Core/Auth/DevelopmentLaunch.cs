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
    public static IReadOnlyList<string> RestartArguments(IReadOnlyList<string> original, bool customChosen)
    {
        ArgumentNullException.ThrowIfNull(original);
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
