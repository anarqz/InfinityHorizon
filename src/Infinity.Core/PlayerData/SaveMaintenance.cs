// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using Infinity.Core.Saves;

namespace Infinity.Core.PlayerData;

public enum SaveDataAction { Backup, Copy, Move, Erase, Swap }

/// <summary>Local-only maintenance. Originals are archived before installs; erase is a recoverable rename.</summary>
public sealed class SaveMaintenance
{
    internal const string WorkRoot = "data/fi-launcher/my-data";
    private readonly DataFiles _files;
    private readonly Func<bool> _gameRunning;
    private readonly HashSet<string> _localProfiles;
    private sealed record SwapJournal(string First, string Second, string FirstFingerprint, string SecondFingerprint);

    public SaveMaintenance(string azimuthRoot) : this(azimuthRoot, [], GameRunning) { }
    public SaveMaintenance(string azimuthRoot, IEnumerable<string> localProfiles) : this(azimuthRoot, localProfiles, GameRunning) { }
    internal SaveMaintenance(string azimuthRoot, IEnumerable<string> localProfiles, Func<bool> gameRunning)
    {
        _files = new(azimuthRoot);
        _gameRunning = gameRunning;
        _localProfiles = localProfiles.Append(AzimuthSaves.FolderFor(AzimuthSaves.OfflineXuid)).ToHashSet(StringComparer.OrdinalIgnoreCase);
    }

    private static bool GameRunning()
    {
        var processes = System.Diagnostics.Process.GetProcessesByName("azimuth");
        try { return processes.Length > 0; }
        finally { foreach (var process in processes) process.Dispose(); }
    }

    public static bool HasPendingSwap(string root)
    {
        var files = new DataFiles(root);
        var work = files.Check(Path.Combine(root, WorkRoot));
        return Directory.Exists(work) && Directory.EnumerateDirectories(work)
            .Any(dir => File.Exists(files.Check(Path.Combine(dir, "pending.json"))));
    }

    internal FileStream Lock()
    {
        EnsureIdle();
        var dir = _files.Check(Path.Combine(_files.Root, WorkRoot));
        Directory.CreateDirectory(dir);
        return new FileStream(_files.Check(Path.Combine(dir, "maintenance.lock")), FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
    }

    internal void EnsureIdle()
    {
        if (_gameRunning()) throw new IOException("Close Azimuth before changing player data.");
    }

    public string Apply(SaveDataAction action, DataSave selected, string? targetFolder = null)
    {
        ArgumentNullException.ThrowIfNull(selected);
        using var maintenance = Lock();
        if (HasPendingSwap(_files.Root)) throw new IOException("Recover the interrupted swap before changing saves.");
        var source = Validate(selected);
        if (source.Location == DataSaveLocation.Active && !_localProfiles.Contains(source.Folder) && action is SaveDataAction.Erase or SaveDataAction.Move)
            throw new IOException("Active Hardcore and unknown profiles cannot be changed locally; back up or copy out instead.");
        if (action == SaveDataAction.Backup) return Backup(source, "manual");
        if (action == SaveDataAction.Erase)
        {
            var destination = Slot(source.Folder, "removed");
            EnsureIdle();
            _files.Move(source.Path, destination);
            return destination;
        }
        if (!source.Sealed || source.LooseData) throw new IOException("Only verified revisions can be installed into a profile.");
        if (targetFolder is null || !SaveDiscovery.IsUserFolder(targetFolder) || !_localProfiles.Contains(targetFolder))
            throw new IOException("Choose an offline or known Noob profile destination. Hardcore imports are not permitted.");
        var targetProfile = Profile(targetFolder);
        if (source.Location == DataSaveLocation.Active && string.Equals(Profile(source.Folder), targetProfile, StringComparison.OrdinalIgnoreCase))
            throw new IOException("Choose a different profile; use Backup for this profile.");
        if (action == SaveDataAction.Swap) return Swap(source, targetFolder);

        Backup(source, "source");
        BackupProfile(targetFolder);
        var staging = Slot(targetFolder, "staging");
        _files.Copy(source.Path, staging, source.Fingerprint!);
        var destinationPath = Next(targetFolder);
        EnsureIdle();
        _files.Move(staging, destinationPath);
        if (action == SaveDataAction.Move)
        {
            // Keep a recoverable copy at the source's archive location; no recursive deletion.
            Validate(source);
            EnsureIdle();
            _files.Move(source.Path, Slot(source.Folder, "moved-source"));
        }
        return destinationPath;
    }

    private DataSave Validate(DataSave selected)
    {
        var current = new SaveDiscovery(_files.Root).Read(selected.Path, selected.LooseData);
        if (!current.Manageable || current.Fingerprint is null || current.Fingerprint != selected.Fingerprint)
            throw new IOException("Save changed or is not a revision folder. Refresh before continuing.");
        return current;
    }

    private string Backup(DataSave source, string label)
    {
        var destination = Slot(source.Folder, label);
        _files.Copy(source.Path, destination, source.Fingerprint!);
        return destination;
    }

    private void BackupProfile(string folder)
    {
        foreach (var (_, path) in AzimuthSaves.RevisionFolders(Profile(folder)))
        {
            _files.Check(path);
            var snapshot = new SaveDiscovery(_files.Root).Read(path);
            if (!snapshot.Manageable) throw new IOException("A destination revision cannot be backed up safely.");
            Backup(snapshot, "before-install");
        }
    }

    private string Swap(DataSave source, string folder)
    {
        var sourceRevisions = AzimuthSaves.RevisionFolders(Profile(source.Folder));
        if (source.Location != DataSaveLocation.Active || !_localProfiles.Contains(source.Folder) || sourceRevisions.Count == 0 || sourceRevisions[^1].Path != source.Path)
            throw new IOException("Swap requires the latest active revision of two different profiles.");
        var targetRevisions = AzimuthSaves.RevisionFolders(Profile(folder));
        var targetPath = targetRevisions.Count == 0 ? null : targetRevisions[^1].Path;
        if (targetPath is null) throw new IOException("The destination has no save to swap.");
        var target = new SaveDiscovery(_files.Root).Read(targetPath);
        if (!target.Manageable || !target.Sealed) throw new IOException("The destination save is not verified.");
        BackupProfile(source.Folder);
        BackupProfile(folder);
        var transaction = _files.Check(Path.Combine(_files.Root, WorkRoot, Guid.NewGuid().ToString("N")));
        Directory.CreateDirectory(transaction);
        var copyA = Path.Combine(transaction, source.Folder, "copy-a");
        var copyB = Path.Combine(transaction, target.Folder, "copy-b");
        Directory.CreateDirectory(Path.GetDirectoryName(copyA)!);
        Directory.CreateDirectory(Path.GetDirectoryName(copyB)!);
        _files.Copy(source.Path, copyA, source.Fingerprint!);
        _files.Copy(target.Path, copyB, target.Fingerprint!);
        var first = Next(source.Folder);
        var second = Next(folder);
        var pending = Path.Combine(transaction, "pending.json");
        var journal = new SwapJournal(Path.GetRelativePath(_files.Root, first), Path.GetRelativePath(_files.Root, second), target.Fingerprint!, source.Fingerprint!);
        using (var output = new FileStream(pending, FileMode.CreateNew, FileAccess.Write, FileShare.None))
        {
            JsonSerializer.Serialize(output, journal);
            output.Flush(flushToDisk: true);
        }
        try
        {
            Validate(source); Validate(target);
            EnsureIdle();
            _files.Move(copyB, first);
            EnsureIdle();
            _files.Move(copyA, second);
            File.Move(pending, Path.Combine(transaction, "complete.json"));
        }
        catch
        {
            RecoverTransaction(transaction);
            throw;
        }
        return transaction;
    }

    /// <summary>Rolls back only new revisions published by an interrupted swap. Old revisions stay untouched.</summary>
    public void RecoverSwaps()
    {
        using var maintenance = Lock();
        var root = _files.Check(Path.Combine(_files.Root, WorkRoot));
        foreach (var dir in Directory.EnumerateDirectories(root))
            if (File.Exists(_files.Check(Path.Combine(dir, "pending.json")))) RecoverTransaction(dir);
    }

    private void RecoverTransaction(string transaction)
    {
        var pending = _files.Check(Path.Combine(transaction, "pending.json"));
        if (new FileInfo(pending).Length > 4096) throw new IOException("Invalid recovery journal.");
        var journal = JsonSerializer.Deserialize<SwapJournal>(File.ReadAllText(pending)) ?? throw new IOException("Invalid recovery journal.");
        if (string.IsNullOrWhiteSpace(journal.First) || string.IsNullOrWhiteSpace(journal.Second) ||
            journal.FirstFingerprint is not { Length: 64 } || !journal.FirstFingerprint.All(Uri.IsHexDigit) ||
            journal.SecondFingerprint is not { Length: 64 } || !journal.SecondFingerprint.All(Uri.IsHexDigit) ||
            journal.First.Equals(journal.Second, StringComparison.OrdinalIgnoreCase)) throw new IOException("Invalid recovery journal.");
        var entries = new[] { (journal.First, journal.FirstFingerprint, "rolled-back-a"), (journal.Second, journal.SecondFingerprint, "rolled-back-b") };
        foreach (var (relative, fingerprint, _) in entries)
        {
            var path = _files.Check(Path.Combine(_files.Root, relative));
            var profile = Path.GetDirectoryName(Path.GetDirectoryName(path))!;
            if (!SaveDiscovery.IsUserFolder(Path.GetFileName(profile)) || !string.Equals(Profile(Path.GetFileName(profile)), profile, StringComparison.OrdinalIgnoreCase) ||
                Path.GetFileName(Path.GetDirectoryName(path)) != "revisions" || Path.GetFileName(path).Length != 16 || !Path.GetFileName(path).All(Uri.IsHexDigit))
                throw new IOException("Recovery path is not an active save revision.");
            if (Directory.Exists(path) && _files.Fingerprint(path) != fingerprint) throw new IOException("Recovery revision changed. Keep all files and contact support.");
        }
        foreach (var (relative, _, label) in entries)
        {
            var path = Path.Combine(_files.Root, relative);
            if (Directory.Exists(path)) { EnsureIdle(); _files.Move(path, Path.Combine(transaction, label)); }
        }
        File.Move(pending, Path.Combine(transaction, "recovered.json"));
    }

    private string Profile(string folder) => _files.Check(Path.Combine(AzimuthSaves.SavesRoot(_files.Root), "player-1", folder));
    private string Next(string folder) => _files.Check(Path.Combine(Profile(folder), "revisions", AzimuthSaves.NextRevisionNumber(Profile(folder)).ToString("x16", CultureInfo.InvariantCulture)));
    private string Slot(string folder, string label) => _files.Check(Path.Combine(AzimuthSaves.SavesRoot(_files.Root), AzimuthSaves.ArchiveDir,
        folder, label + "-" + Guid.NewGuid().ToString("N")));
}
