// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;
using Infinity.Core.Process;

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

    /// <summary>
    /// <see cref="Detect(string?)"/> for one game: both games' executable has the same name, so the manifest's <c>TitleId</c> decides.
    /// FH2 is refused only when the manifest names another title (a folder remembered before this check stays good); any other game
    /// must be named.
    /// </summary>
    public static GameContentLocation? Detect(string? directory, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(game);
        if (Detect(directory) is not { } location) return null;
        var title = ManifestTitle(location.Mount);
        return string.Equals(title, game.TitleId, StringComparison.OrdinalIgnoreCase) || (title is null && game.FullFeatures) ? location : null;
    }

    private const int MaxManifestBytes = 1024 * 1024;

    /// <summary>The <c>TitleId</c> of the mount's <c>AppxManifest.xml</c>; null when it names none or cannot be read.</summary>
    private static string? ManifestTitle(string mount)
    {
        try
        {
            var file = new FileInfo(Path.Combine(mount, "AppxManifest.xml"));
            if (!file.Exists || file.Length > MaxManifestBytes) return null;
            var match = System.Text.RegularExpressions.Regex.Match(File.ReadAllText(file.FullName), "\\bTitleId\\s*=\\s*\"([0-9A-Fa-f]{8})\"");
            return match.Success ? match.Groups[1].Value : null;
        }
        catch (Exception error) when (CriticalFileReader.FileError(error) || error is ArgumentException) { return null; }
    }

    private static string FileNameOf(GameDefinition game) => game.FullFeatures ? FileName : "game-content-location-" + game.Id + ".txt";

    /// <summary>The folder remembered for one game; FH2 keeps <see cref="FileName"/>. A folder of another game is not one.</summary>
    public static GameContentLocation? Load(string stateDirectory, string launcherRoot, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(game);
        if (PcLocation.Resolve(launcherRoot, stateDirectory, FileNameOf(game), dir => LoadFile(dir, FileNameOf(game), game)) is not { } found) return null;
        foreach (var dir in new[] { launcherRoot, stateDirectory })
        {
            try { if (LoadFile(dir, FileNameOf(game), game) != found.Value) found.Value.Save(dir, game); }
            catch (Exception error) when (CriticalFileReader.FileError(error)) { /* the location stands; the copy is retried next time */ }
        }
        return found.Value;
    }

    public void Save(string stateDirectory, string launcherRoot, GameDefinition game)
    {
        Save(stateDirectory, game);
        if (!PcLocation.Same(launcherRoot, stateDirectory)) Save(launcherRoot, game);
    }

    private void Save(string stateDirectory, GameDefinition game)
    {
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileNameOf(game));
        if (!CriticalFileReader.SafeAncestors(path)) throw new IOException("Unsafe local settings location");
        File.WriteAllText(path, PackageRoot, new UTF8Encoding(false));
    }

    private static GameContentLocation? LoadFile(string stateDirectory, string fileName, GameDefinition game)
    {
        try
        {
            var path = Path.Combine(stateDirectory, fileName);
            if (!CriticalFileReader.SafeAncestors(path) || !File.Exists(path) || new FileInfo(path).Length > 32768) return null;
            return Detect(new UTF8Encoding(false, true).GetString(File.ReadAllBytes(path)), game);
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
