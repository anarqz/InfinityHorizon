// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Integrity;

/// <summary>Explicit remembered game folder. No account/launcher INI inspection or directory search.</summary>
public sealed record GameContentLocation(string PackageRoot, string Mount)
{
    public const string Executable = "forza_durango_release_final.exe";
    public static GameContentLocation? Detect(string? directory)
    {
        if (string.IsNullOrWhiteSpace(directory)) return null;
        try
        {
            var root = ContentPath.NormalizeSelection(directory);
            var directMount = File.Exists(Path.Combine(root, "AppxManifest.xml")) && File.Exists(Path.Combine(root, Executable));
            var mount = ContentPath.ResolveDirectory(directMount ? root : Path.Combine(root, "Mount"));
            if (directMount && Path.GetFileName(root).Equals("Mount", StringComparison.OrdinalIgnoreCase)) root = Path.GetDirectoryName(root)!;
            if (!ContentPath.SafeWithin(mount, mount)
                || !File.Exists(Path.Combine(mount, "AppxManifest.xml")) || !File.Exists(Path.Combine(mount, Executable))) return null;
            return new(root, mount);
        }
        catch (Exception error) when (CriticalFileReader.FileError(error)) { return null; }
    }

    public static GameContentLocation? Load(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, FileName);
            if (!CriticalFileReader.SafeAncestors(path) || !File.Exists(path)) return null;
            using var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read, 4096);
            if (input.Length > 32768) return null;
            using var bounded = new MemoryStream();
            var buffer = new byte[4096]; int count;
            while ((count = input.Read(buffer)) > 0)
            {
                if (bounded.Length + count > 32768) return null;
                bounded.Write(buffer, 0, count);
            }
            return Detect(new UTF8Encoding(false, true).GetString(bounded.ToArray()));
        }
        catch (Exception error) when (CriticalFileReader.FileError(error)) { return null; }
    }

    public void Save(string stateDirectory)
    {
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        if (!CriticalFileReader.SafeAncestors(path)) throw new IOException("Unsafe local settings location");
        File.WriteAllText(path, PackageRoot, new UTF8Encoding(false));
    }

    public const string FileName = "game-content-location.txt";

    /// <summary>Owner 2026-10-27: the game folder is the PC's. Saved in the server's folder and once in the launcher root.</summary>
    public void Save(string stateDirectory, string launcherRoot)
    {
        Save(stateDirectory);
        if (!Infinity.Core.Process.PcLocation.Same(launcherRoot, stateDirectory)) Save(launcherRoot);
    }

    /// <summary>
    /// The PC's remembered game folder: the launcher root's, else this server's folder's, else the newest other server folder's that
    /// still resolves (<see cref="Infinity.Core.Process.PcLocation"/>). One found elsewhere is copied into the root and this folder (best effort).
    /// </summary>
    public static GameContentLocation? Load(string stateDirectory, string launcherRoot)
    {
        if (Infinity.Core.Process.PcLocation.Resolve(launcherRoot, stateDirectory, FileName, Load) is not { } found) return null;
        foreach (var dir in new[] { launcherRoot, stateDirectory })
        {
            try { if (Load(dir) != found.Value) found.Value.Save(dir); }
            catch (Exception error) when (CriticalFileReader.FileError(error)) { /* the location stands; the copy is retried next time */ }
        }
        return found.Value;
    }
}
