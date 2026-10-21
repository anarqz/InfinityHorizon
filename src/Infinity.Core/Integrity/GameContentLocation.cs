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
            var root = Path.GetFullPath(directory);
            if (Path.GetFileName(root).Equals("Mount", StringComparison.OrdinalIgnoreCase)) root = Path.GetDirectoryName(root)!;
            var mount = Path.Combine(root, "Mount");
            if (root.StartsWith(@"\\", StringComparison.Ordinal) || !CriticalFileReader.SafeAncestors(mount)
                || !File.Exists(Path.Combine(mount, "AppxManifest.xml")) || !File.Exists(Path.Combine(mount, Executable))) return null;
            return new(root, mount);
        }
        catch (Exception error) when (CriticalFileReader.FileError(error)) { return null; }
    }

    public static GameContentLocation? Load(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, "game-content-location.txt");
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
        var path = Path.Combine(stateDirectory, "game-content-location.txt");
        if (!CriticalFileReader.SafeAncestors(path)) throw new IOException("Unsafe local settings location");
        File.WriteAllText(path, PackageRoot, new UTF8Encoding(false));
    }
}
