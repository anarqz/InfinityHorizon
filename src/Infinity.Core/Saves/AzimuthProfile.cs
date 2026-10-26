// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Saves;

/// <summary>
/// Azimuth's own player (<c>data\azimuth.ini</c> <c>[launcher] player_name</c>, "Player 1" by default): the name of the career in
/// <c>User_9000000000000</c> (<see cref="AzimuthSaves.OfflineXuid"/>). A new Infinity profile with the same name adopts that career
/// (owner, 2026-10-26: "Adopt the Azimuth career"). Read only; the launcher never writes Azimuth's settings.
/// </summary>
public static class AzimuthProfile
{
    private const int MaxIniBytes = 64 * 1024;

    public static string IniPath(string azimuthRoot)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        return Path.Combine(azimuthRoot, "data", "azimuth.ini");
    }

    /// <summary>The <c>player_name</c> of <c>[launcher]</c>, trimmed; null when the file, the section or the key is missing or unreadable.</summary>
    public static string? PlayerName(string azimuthRoot)
    {
        try
        {
            var path = IniPath(azimuthRoot);
            var file = new FileInfo(path);
            if (!file.Exists || file.Length > MaxIniBytes) return null;
            return PlayerNameIn(File.ReadAllText(path));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    /// <summary>The parsing rule of <see cref="PlayerName"/>: the first <c>player_name</c> inside <c>[launcher]</c>; comments and other sections ignored.</summary>
    public static string? PlayerNameIn(string ini)
    {
        ArgumentNullException.ThrowIfNull(ini);
        var section = string.Empty;
        foreach (var raw in ini.Split('\n'))
        {
            var line = raw.Trim();
            if (line.Length == 0 || line[0] is ';' or '#') continue;
            if (line[0] == '[' && line.EndsWith(']')) { section = line[1..^1].Trim(); continue; }
            var equals = line.IndexOf('=');
            if (equals <= 0 || !section.Equals("launcher", StringComparison.OrdinalIgnoreCase)) continue;
            if (!line[..equals].Trim().Equals("player_name", StringComparison.OrdinalIgnoreCase)) continue;
            var value = line[(equals + 1)..].Trim().Trim('"').Trim();
            return value.Length == 0 ? null : value;
        }
        return null;
    }

    /// <summary>
    /// The profile name is Azimuth's player (case-insensitive, as players read names). Azimuth's default "Player 1" never matches: an
    /// Infinity name has no spaces (ProfileName).
    /// </summary>
    public static bool SameName(string? azimuthName, string? profileName) =>
        azimuthName is { Length: > 0 } && profileName?.Trim() is { Length: > 0 } name
        && string.Equals(azimuthName.Trim(), name, StringComparison.OrdinalIgnoreCase);
}
