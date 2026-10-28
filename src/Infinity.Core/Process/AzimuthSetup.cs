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
/// (<c>&lt;state&gt;\azimuth.json</c>; owner 2026-10-27: once per PC, in the launcher root, for every server).
/// </summary>
public static class AzimuthSetup
{
    /// <summary>The Azimuth version Infinity and the hook support.</summary>
    public const string CompatibleVersion = "1.0.3";

    /// <summary>Azimuth 1.0's flag that starts the added game directly instead of Azimuth's own launcher.</summary>
    public const string SkipLauncher = "--skip-launcher";

    /// <summary>OD-125: the exact <c>azimuth.exe</c> of <see cref="CompatibleVersion"/> (SHA-256); any other build is refused.</summary>
    public const string CompatibleHostSha256 = "ebfd5d43a165dcec5d4007b9d41ed5b2ed122fcd02ecfef7e4ed35b40826821f";

    /// <summary>The Forza Horizon 2 (Xbox One) build Azimuth runs (unchanged from 0.9.10 to 1.0.3).</summary>
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
        { HostSha256: not CompatibleHostSha256 } => AzimuthStatus.WrongVersion,
        _ when (hook is not null ? !hook.Verified : install.HookVersion is null) => AzimuthStatus.NoHook,
        _ => AzimuthStatus.Ready,
    };

    /// <summary>
    /// The first folder holding azimuth.exe, in order: the explicit one (<c>--azimuth</c>,
    /// <c>FI_AZIMUTH</c>), the one the player chose before, the launcher's own folder and its
    /// parent, then a shallow look (two levels) under the usual places (Desktop, Downloads,
    /// Documents, <c>&lt;drive&gt;:\Games</c>). Among several found by the look-around, a
    /// compatible version wins. With <paramref name="launcherRoot"/> the choice is the PC's (<see cref="LoadRemembered"/>): kept in
    /// the launcher root and found in any server's folder.
    /// </summary>
    public static AzimuthInstall? Find(string? explicitFolder, string stateDirectory, string launcherFolder, IEnumerable<string>? searchRoots = null,
        string? launcherRoot = null, Action<string>? log = null)
    {
        var remembered = launcherRoot is null ? LoadChoice(stateDirectory) : LoadRemembered(stateDirectory, launcherRoot, log);
        foreach (var folder in new[] { explicitFolder, remembered, launcherFolder, Path.GetDirectoryName(launcherFolder.TrimEnd('\\', '/')) })
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
        return found.FirstOrDefault(i => i.Version == CompatibleVersion && i.HostSha256 == CompatibleHostSha256)
            ?? found.FirstOrDefault(i => i.Version == CompatibleVersion) ?? found.FirstOrDefault();
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

    /// <summary>
    /// Remembers the folder the player chose (only one that holds azimuth.exe): in the server's folder and, with
    /// <paramref name="launcherRoot"/>, once for the PC in the launcher root, which every server's folder reads.
    /// </summary>
    public static bool SaveChoice(string stateDirectory, string folder, string? launcherRoot = null)
    {
        if (Detect(folder) is null)
        {
            return false;
        }
        var full = Path.GetFullPath(folder);
        Write(stateDirectory, full);
        if (launcherRoot is not null && !PcLocation.Same(launcherRoot, stateDirectory)) Write(launcherRoot, full);
        return true;
    }

    /// <summary>
    /// The PC's remembered Azimuth folder (owner 2026-10-27): the launcher root's, else this server's folder's, else the newest of the
    /// other servers' folders (<c>production</c>, <c>custom\*</c>, the root) that still holds azimuth.exe. One found elsewhere is
    /// copied into the root and this server's folder (best effort), so the next start and every other server read it directly.
    /// </summary>
    public static string? LoadRemembered(string stateDirectory, string launcherRoot, Action<string>? log = null)
    {
        var found = PcLocation.Resolve(launcherRoot, stateDirectory, FileName, dir => LoadChoice(dir) is { } path && Detect(path) is not null ? path : null);
        if (found is not { } choice) return null;
        try
        {
            foreach (var dir in new[] { launcherRoot, stateDirectory }.Distinct(StringComparer.OrdinalIgnoreCase))
            {
                if (!string.Equals(LoadChoice(dir), choice.Value, StringComparison.OrdinalIgnoreCase))
                {
                    Write(dir, choice.Value);
                    log?.Invoke($"setup: Azimuth location from {choice.From} kept for this PC in {dir}");
                }
            }
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            log?.Invoke("setup: Azimuth location not copied: " + e.GetType().Name);
        }
        return choice.Value;
    }

    private static void Write(string dir, string folder)
    {
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, FileName);
        File.WriteAllText(path + ".tmp", JsonSerializer.Serialize(new Saved(folder)));
        File.Move(path + ".tmp", path, overwrite: true);
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
