// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Process;

/// <summary>
/// Owner 2026-10-27, "Azimuth is found before sign-in, once per PC, for every server": a folder the player chose (Azimuth, the
/// game content) belongs to the PC, not to a server. It is kept once in the launcher root, the folder every server's state folder
/// sits under (<c>production</c>, <c>custom\&lt;server&gt;</c>, development in the root itself), and copied into the server's own
/// folder too, which older launchers read.
/// </summary>
public static class PcLocation
{
    /// <summary>Every server's state folder under <paramref name="root"/>: the root (development), <c>production</c>, <c>custom\*</c>.</summary>
    public static IEnumerable<string> StateFolders(string root)
    {
        ArgumentException.ThrowIfNullOrEmpty(root);
        yield return root;
        yield return Path.Combine(root, "production");
        var custom = Path.Combine(root, "custom");
        string[] servers;
        try { servers = Directory.Exists(custom) ? Directory.GetDirectories(custom) : []; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { servers = []; }
        foreach (var server in servers.Order(StringComparer.OrdinalIgnoreCase)) yield return server;
    }

    /// <summary>
    /// The remembered value and the folder it came from: the root's, else the current server folder's, else the newest
    /// (<paramref name="fileName"/>'s last write) of the other server folders whose copy still loads. <paramref name="load"/> returns
    /// null for a file that is missing or no longer resolves (a moved or deleted folder), which is then skipped.
    /// </summary>
    public static (T Value, string From)? Resolve<T>(string root, string stateDirectory, string fileName, Func<string, T?> load) where T : class
    {
        ArgumentNullException.ThrowIfNull(load);
        if (Load(load, root) is { } atRoot) return (atRoot, root);
        if (!Same(stateDirectory, root) && Load(load, stateDirectory) is { } own) return (own, stateDirectory);
        var others = StateFolders(root)
            .Where(dir => !Same(dir, root) && !Same(dir, stateDirectory))
            .Select(dir => (Dir: dir, Written: WrittenAt(Path.Combine(dir, fileName))))
            .Where(candidate => candidate.Written is not null)
            .OrderByDescending(candidate => candidate.Written)
            .ToList();
        foreach (var (dir, _) in others)
        {
            if (Load(load, dir) is { } value) return (value, dir);
        }
        return null;
    }

    /// <summary>The same folder, ignoring case and a trailing separator (Windows paths).</summary>
    public static bool Same(string a, string b)
    {
        try
        {
            return string.Equals(Path.TrimEndingDirectorySeparator(Path.GetFullPath(a)), Path.TrimEndingDirectorySeparator(Path.GetFullPath(b)),
                StringComparison.OrdinalIgnoreCase);
        }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException)
        {
            return false;
        }
    }

    private static T? Load<T>(Func<string, T?> load, string dir) where T : class
    {
        try { return load(dir); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException) { return null; }
    }

    private static DateTime? WrittenAt(string path)
    {
        try { return File.Exists(path) ? File.GetLastWriteTimeUtc(path) : null; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException) { return null; }
    }
}
