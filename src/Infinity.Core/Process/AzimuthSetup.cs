// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Process;

/// <summary>Where the setup stands, step by step (OD-46, docs/launcher/AZIMUTH_SETUP.md).</summary>
public enum AzimuthStatus
{
    /// <summary>No folder with azimuth.exe was found or chosen.</summary>
    Missing,

    /// <summary>azimuth.exe found, but another version than <see cref="AzimuthSetup.CompatibleVersion"/>.</summary>
    WrongVersion,

    /// <summary>The compatible Azimuth; the Infinity hook is not in <c>data\bin</c> yet.</summary>
    NoHook,

    /// <summary>The compatible Azimuth with the Infinity hook: ready to play.</summary>
    Ready,
}

/// <summary>
/// OD-46: finding Azimuth and saying what is missing. The launcher ships and downloads no Azimuth
/// or game files; it points to the official page and remembers the folder the player chose
/// (<c>&lt;state&gt;\azimuth.json</c>).
/// </summary>
public static class AzimuthSetup
{
    /// <summary>The Azimuth version Infinity and the hook support.</summary>
    public const string CompatibleVersion = "0.9.10";

    /// <summary>The Forza Horizon 2 (Xbox One) build Azimuth 0.9.10 runs.</summary>
    public const string GameVersion = "1.0.0.64";

    /// <summary>Azimuth's official page (Almost There Games): the only place to get it.</summary>
    public const string OfficialPage = "https://almost-there-games.itch.io/azimuth";

    public const string FileName = "azimuth.json";

    private sealed record Saved(string Path);

    /// <summary>
    /// The status of an install (null: none found). With <paramref name="hook"/> (OD-47), Ready
    /// also needs the installed hook's signature verified; without it, any Infinity build counts.
    /// </summary>
    public static AzimuthStatus StatusOf(AzimuthInstall? install, InstalledHook? hook = null) => install switch
    {
        null => AzimuthStatus.Missing,
        { Version: not CompatibleVersion } => AzimuthStatus.WrongVersion,
        _ when (hook is not null ? !hook.Verified : install.HookVersion is null) => AzimuthStatus.NoHook,
        _ => AzimuthStatus.Ready,
    };

    /// <summary>
    /// The first folder holding azimuth.exe, in order: the explicit one (<c>--azimuth</c>,
    /// <c>FI_AZIMUTH</c>), the one the player chose before, the launcher's own folder and its
    /// parent, then a shallow look (two levels) under the usual places (Desktop, Downloads,
    /// Documents, <c>&lt;drive&gt;:\Games</c>). Among several found by the look-around, a
    /// compatible version wins.
    /// </summary>
    public static AzimuthInstall? Find(string? explicitFolder, string stateDirectory, string launcherFolder, IEnumerable<string>? searchRoots = null)
    {
        foreach (var folder in new[] { explicitFolder, LoadChoice(stateDirectory), launcherFolder, Path.GetDirectoryName(launcherFolder.TrimEnd('\\', '/')) })
        {
            if (!string.IsNullOrEmpty(folder) && Detect(folder) is { } install)
            {
                return install;
            }
        }
        var found = new List<AzimuthInstall>();
        foreach (var root in searchRoots ?? DefaultRoots())
        {
            foreach (var dir in Shallow(root, depth: 2))
            {
                if (Detect(dir) is { } install)
                {
                    found.Add(install);
                }
            }
        }
        return found.FirstOrDefault(i => i.Version == CompatibleVersion) ?? found.FirstOrDefault();
    }

    /// <summary>The usual places a player extracts a download to.</summary>
    public static IEnumerable<string> DefaultRoots()
    {
        var profile = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        yield return Environment.GetFolderPath(Environment.SpecialFolder.DesktopDirectory);
        yield return Path.Combine(profile, "Downloads");
        yield return Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments);
        foreach (var drive in DriveInfo.GetDrives().Where(d => d.DriveType == DriveType.Fixed && d.IsReady))
        {
            yield return Path.Combine(drive.RootDirectory.FullName, "Games");
        }
    }

    /// <summary>Remembers the folder the player chose (only one that holds azimuth.exe).</summary>
    public static bool SaveChoice(string stateDirectory, string folder)
    {
        if (Detect(folder) is null)
        {
            return false;
        }
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        File.WriteAllText(path + ".tmp", JsonSerializer.Serialize(new Saved(Path.GetFullPath(folder))));
        File.Move(path + ".tmp", path, overwrite: true);
        return true;
    }

    public static string? LoadChoice(string stateDirectory)
    {
        try
        {
            return JsonSerializer.Deserialize<Saved>(File.ReadAllText(Path.Combine(stateDirectory, FileName)))?.Path;
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    private static AzimuthInstall? Detect(string folder)
    {
        try
        {
            return AzimuthInstall.Detect(folder);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            return null;
        }
    }

    /// <summary>The folder and its subfolders down to <paramref name="depth"/>, skipping what cannot be read.</summary>
    private static IEnumerable<string> Shallow(string root, int depth)
    {
        if (string.IsNullOrEmpty(root) || !Directory.Exists(root))
        {
            yield break;
        }
        var level = new List<string> { root };
        for (var d = 0; d <= depth && level.Count > 0; d++)
        {
            var next = new List<string>();
            foreach (var dir in level)
            {
                yield return dir;
                if (d == depth)
                {
                    continue;
                }
                try
                {
                    next.AddRange(Directory.EnumerateDirectories(dir).Take(200));
                }
                catch (Exception e) when (e is IOException or UnauthorizedAccessException)
                {
                    // unreadable: skipped
                }
            }
            level = next;
        }
    }
}
