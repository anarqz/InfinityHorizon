// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Auth;

/// <summary>
/// CR-L39: the server the player chose on the screen, <c>server-choice.json</c> in the launcher's state root (shared by every
/// server's state folder). Absent or unreadable means the official server (development builds: their command line).
/// </summary>
public static class ServerChoice
{
    public const string FileName = "server-choice.json";

    private sealed record Saved(string? Custom);

    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);

    /// <summary>The saved custom server, or null for the official one.</summary>
    public static Uri? Read(string root)
    {
        try
        {
            var path = Path.Combine(root, FileName);
            if (!File.Exists(path)) return null;
            return JsonSerializer.Deserialize<Saved>(File.ReadAllText(path), Json)?.Custom is { Length: > 0 } text ? CustomServer.ParseOrigin(text) : null;
        }
        catch (Exception e) when (e is IOException or JsonException or ArgumentException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    /// <summary>Saves <paramref name="custom"/> (null: back to the official server).</summary>
    public static void Write(string root, Uri? custom)
    {
        Directory.CreateDirectory(root);
        var path = Path.Combine(root, FileName);
        var temporary = path + ".tmp";
        File.WriteAllText(temporary, JsonSerializer.Serialize(new Saved(custom is null ? null : CustomServer.ParseOrigin(custom.ToString()).GetLeftPart(UriPartial.Authority)), Json));
        File.Move(temporary, path, overwrite: true);
    }

    /// <summary>
    /// The configuration a launch uses: a saved custom server wins over the default (in a release build it always wins, since its
    /// command line can only name the official server); a development build's explicit non-official <c>--server</c> still wins.
    /// </summary>
    public static ServerConfig Apply(ServerConfig fromCommandLine, string root, bool explicitServer)
    {
        ArgumentNullException.ThrowIfNull(fromCommandLine);
        if (Read(root) is not { } custom || (!BuildProfile.ProductionOnly && explicitServer && !fromCommandLine.Production))
            return fromCommandLine;
        var config = ServerConfig.ForCustom(custom);
        // the pins live in that server's own state folder: two launchers on two servers never share the file
        return config with { Pins = new TrustPins(Path.Combine(config.StateDirectory(root), "custom-trust.json")) };
    }
}
