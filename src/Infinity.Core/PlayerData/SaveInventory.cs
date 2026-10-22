// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Saves;

namespace Infinity.Core.PlayerData;

public enum DataSaveLocation { Active, Backup, Misplaced }
public sealed record DataSave(string Path, string RelativePath, string Folder, ulong Number, DataSaveLocation Location,
    bool Sealed, bool Manageable, bool LooseData, int? Level, int? Credits, DateTime LastWriteUtc, long Bytes, string? Fingerprint);
public sealed record SaveInventory(IReadOnlyList<DataSave> Saves, int Skipped, bool Limited);

public sealed class SaveDiscovery(string azimuthRoot)
{
    private readonly DataFiles _files = new(azimuthRoot);
    private const int MaxDirectories = 100_000;

    public SaveInventory Scan(CancellationToken cancellation = default)
    {
        var found = new List<DataSave>();
        var pending = new Stack<(string Path, int Depth)>();
        pending.Push((_files.Root, 0));
        var visited = 0;
        var skipped = 0;
        var limited = false;
        while (pending.TryPop(out var current))
        {
            cancellation.ThrowIfCancellationRequested();
            if (++visited > MaxDirectories || found.Count >= 5_000) { limited = true; break; }
            try
            {
                _files.Check(current.Path);
                var data = _files.Check(Path.Combine(current.Path, "data"));
                var loose = File.Exists(Path.Combine(current.Path, "ProfileData")) &&
                    (File.Exists(Path.Combine(current.Path, "SaveState")) || File.Exists(Path.Combine(current.Path, "PlayerDatabase")));
                var parent = Directory.GetParent(current.Path);
                var archivedSlot = parent is not null && IsUserFolder(parent.Name) &&
                    parent.Parent?.Name.Equals(AzimuthSaves.ArchiveDir, StringComparison.OrdinalIgnoreCase) == true;
                if (File.Exists(Path.Combine(current.Path, "commit")) ||
                    Fh2Bundle.SaveFiles.Skip(1).Any(file => File.Exists(Path.Combine(data, file[5..]))) || loose || archivedSlot)
                {
                    var save = Read(current.Path, loose, cancellation);
                    found.Add(save);
                    if (save.Manageable) continue;
                }
                if (current.Depth >= 64) { limited = true; continue; }
                foreach (var directory in Directory.EnumerateDirectories(current.Path)) pending.Push((directory, current.Depth + 1));
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { skipped++; }
        }
        return new(found.OrderBy(s => s.Location).ThenByDescending(s => s.LastWriteUtc).ToArray(), skipped, limited);
    }

    public DataSave Read(string path, bool loose = false, CancellationToken cancellation = default)
    {
        path = _files.Check(path);
        var name = Path.GetFileName(path);
        _ = ulong.TryParse(name, NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out var number);
        var folder = OwnerFolder(path);
        var canonical = Path.Combine(AzimuthSaves.SavesRoot(_files.Root), "player-1", folder, "revisions");
        var active = name.Length == 16 && name.All(Uri.IsHexDigit) &&
            string.Equals(Path.GetDirectoryName(path), canonical, StringComparison.OrdinalIgnoreCase);
        var relative = Path.GetRelativePath(_files.Root, path);
        var backup = relative.Split(Path.DirectorySeparatorChar).Any(p => p.Equals(AzimuthSaves.ArchiveDir, StringComparison.OrdinalIgnoreCase)) ||
            relative.StartsWith(Path.Combine("data", "fi-launcher", "my-data"), StringComparison.OrdinalIgnoreCase);
        var children = Directory.EnumerateDirectories(path).Select(Path.GetFileName).ToArray();
        var data = _files.Check(Path.Combine(path, "data"));
        // A maintenance unit is a revision leaf (or loose blob leaf), never an install or profile container.
        var knownBlobs = Fh2Bundle.SaveFiles.Skip(1).Select(file => file[5..]).ToHashSet(StringComparer.Ordinal);
        var manageable = !string.Equals(path, _files.Root, StringComparison.OrdinalIgnoreCase) &&
            children.All(child => child == "data") && (loose || children.Contains("data")) &&
            Directory.EnumerateFiles(path).All(file => Path.GetFileName(file) == "commit" || loose && knownBlobs.Contains(Path.GetFileName(file))) &&
            (!Directory.Exists(data) || !Directory.EnumerateDirectories(data).Any() &&
                Directory.EnumerateFiles(data).All(file => knownBlobs.Contains(Path.GetFileName(file))));
        var files = manageable ? _files.Tree(path, cancellation) : [];
        var fingerprint = manageable ? _files.Fingerprint(path, cancellation) : null;
        if (manageable) _files.Check(Path.Combine(path, "commit"));
        var revision = loose || !manageable ? null : AzimuthSaves.ReadRevision(number, path);
        var sealedSave = manageable && revision is not null && AzimuthSaves.Verify(revision).Count == 0;
        var profilePath = Path.Combine(path, loose ? string.Empty : "data", "ProfileData");
        byte[]? profile = null;
        if (manageable && File.Exists(profilePath) && new FileInfo(profilePath).Length <= 4 * 1024 * 1024)
            profile = File.ReadAllBytes(_files.Check(profilePath));
        var commit = Path.Combine(path, "commit");
        var time = File.Exists(commit) ? File.GetLastWriteTimeUtc(commit) : Directory.GetLastWriteTimeUtc(path);
        return new(path, relative, folder, number, active ? DataSaveLocation.Active : backup ? DataSaveLocation.Backup : DataSaveLocation.Misplaced,
            sealedSave, manageable, loose, profile is null ? null : AzimuthSaves.ProfileInt(profile, "Level") + 1,
            profile is null ? null : AzimuthSaves.ProfileInt(profile, "Credits"), time,
            files.Sum(file => new FileInfo(file).Length), fingerprint);
    }

    internal static string OwnerFolder(string path)
    {
        for (var dir = path; dir is not null; dir = Path.GetDirectoryName(dir))
            if (IsUserFolder(Path.GetFileName(dir))) return Path.GetFileName(dir);
        return AzimuthSaves.FolderFor(AzimuthSaves.OfflineXuid);
    }

    public static bool IsUserFolder(string name) => name.StartsWith("User_", StringComparison.OrdinalIgnoreCase) &&
        name.Length is > 5 and <= 21 && ulong.TryParse(name.AsSpan(5), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out _);
}
