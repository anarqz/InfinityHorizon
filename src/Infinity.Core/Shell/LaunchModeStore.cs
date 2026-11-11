// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// OD-118 (G2): how the last run was played, in <c>launch-mode</c> of the state folder. <c>offline</c>: the next start opens offline
/// and never resumes or refreshes a session. <c>online</c>: a kept session resumes. Missing or anything else: the welcome screen.
/// </summary>
public static class LaunchModeStore
{
    public const string Online = "online", Offline = "offline";
    private const string FileName = "launch-mode";

    public static string? Read(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, FileName);
            if (!File.Exists(path)) return null;
            var value = File.ReadAllText(path).Trim();
            return value is Online or Offline ? value : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>OD-118 T1c: the local profile last chosen offline (its id), selected again on the next offline start.</summary>
    public static string? ReadLastProfile(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, LastProfileName);
            if (!File.Exists(path) || new FileInfo(path).Length > 128) return null;
            var id = File.ReadAllText(path).Trim();
            return Guid.TryParse(id, out _) ? id : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    public static void WriteLastProfile(string stateDirectory, string id)
    {
        if (!Guid.TryParse(id, out _)) throw new ArgumentException("not a local profile id", nameof(id));
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, LastProfileName);
        File.WriteAllText(path + ".tmp", id);
        File.Move(path + ".tmp", path, overwrite: true);
    }

    private const string LastProfileName = "last-local-profile";

    /// <summary>The game last chosen in the picker (its id), chosen again on the next start.</summary>
    public static string? ReadLastGame(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, LastGameName);
            if (!File.Exists(path) || new FileInfo(path).Length > 32) return null;
            var id = File.ReadAllText(path).Trim();
            return id.Length > 0 && id.All(char.IsAsciiLetterOrDigit) ? id : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    public static void WriteLastGame(string stateDirectory, string id)
    {
        if (id.Length == 0 || !id.All(char.IsAsciiLetterOrDigit)) throw new ArgumentException("not a game id", nameof(id));
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, LastGameName);
        File.WriteAllText(path + ".tmp", id);
        File.Move(path + ".tmp", path, overwrite: true);
    }

    private const string LastGameName = "last-game";

    /// <summary>
    /// OD-240: the XUID of the account's Azimuth identity at the last sign-in (decimal), so offline plays under the same gamertag
    /// (<see cref="AzimuthIdentity.Local"/>); null when none was kept.
    /// </summary>
    public static string? ReadAzimuthXuid(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, AzimuthXuidName);
            if (!File.Exists(path) || new FileInfo(path).Length > 32) return null;
            var xuid = File.ReadAllText(path).Trim();
            return xuid.Length > 0 && xuid.All(char.IsAsciiDigit) ? xuid : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    public static void WriteAzimuthXuid(string stateDirectory, string xuid)
    {
        if (xuid.Length == 0 || !xuid.All(char.IsAsciiDigit)) throw new ArgumentException("not a XUID", nameof(xuid));
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, AzimuthXuidName);
        File.WriteAllText(path + ".tmp", xuid);
        File.Move(path + ".tmp", path, overwrite: true);
    }

    private const string AzimuthXuidName = "azimuth-identity";

    public static void Write(string stateDirectory, string mode)
    {
        if (mode is not (Online or Offline)) throw new ArgumentOutOfRangeException(nameof(mode));
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        var temporary = path + ".tmp";
        File.WriteAllText(temporary, mode);
        File.Move(temporary, path, overwrite: true);
    }
}
