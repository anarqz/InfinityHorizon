// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using System.Text.RegularExpressions;

namespace Infinity.Core.Mods;

/// <summary>One installed mod as read from its receipt; <see cref="Problem"/> is the error code when it could not be read.</summary>
public sealed record ModInstallation(string Id, ModManifest? Manifest, ManagedModReceipt? Receipt, IReadOnlyList<ModDrift> Drift,
    IReadOnlyList<ManifestIniIssue> SettingsIssues, string? Problem)
{
    public bool Enabled => Receipt?.Enabled == true;
}

/// <summary>
/// A read-only view of the mod state of one Azimuth folder and its bucket (M10: built off the UI
/// thread and cached). Only <see cref="BlocksPlay"/> may stop ordinary Play, and it is always shown
/// with its resolution; everything else is a warning.
/// </summary>
public sealed record ModHealth(bool RecoveryHere, bool RecoveryElsewhere, IReadOnlyList<ModInstallation> Installed,
    IReadOnlyList<string> RelinkCandidates, IReadOnlyList<string> Problems)
{
    /// <summary>Receipts recorded for a game folder that moved to the selected one (<see cref="ManagedModFiles.RelinkGame"/>).</summary>
    public bool GameRelink { get; init; }
    public static ModHealth Empty { get; } = new(false, false, [], [], []);
    public bool BlocksPlay => RecoveryHere;
    public bool AnyEnabled => Installed.Any(i => i.Enabled);
    /// <summary>OD-232: an installed mod of a retired format, still on, is a warning until the player removes it.</summary>
    public bool HasWarnings => Problems.Count > 0 || RecoveryElsewhere || RelinkCandidates.Count > 0
        || Installed.Any(i => i.Problem is not null || i.Enabled && (i.Drift.Count > 0 || i.SettingsIssues.Count > 0 || i.Manifest?.Retired == true));
}

/// <summary>
/// A session database the retired native mod runtime published for one launch (OD-113, retired by OD-232): kept only so an interrupted
/// session of an earlier launcher is still restored by recovery (<see cref="ManagedModFiles.Recover"/>, <see cref="ManagedModFiles.ResolveRecovery"/>).
/// </summary>
public sealed record SessionDatabaseJournal(string AzimuthRoot, string GameRoot, string Target, string Baseline, string Prepared, bool Existed,
    IReadOnlyList<string>? CreatedDirectories = null);

public sealed partial class ManagedModFiles
{
    /// <summary>Retention (documented in MODS.md): archived records kept 30 days and at least the newest 50; session folders one day.</summary>
    public static readonly TimeSpan ArchiveRetention = TimeSpan.FromDays(30);
    public const int ArchiveMinimum = 50;
    public static readonly TimeSpan SessionRetention = TimeSpan.FromDays(1);

    /// <summary>Reads everything the Mods page and Play need; content problems are reported, never thrown. Writes nothing.</summary>
    public ModHealth Inspect()
    {
        var problems = new List<string>();
        var installed = new List<ModInstallation>();
        var elsewhere = false; var here = false;
        try
        {
            here = RecoveryRequiredHere;
            elsewhere = PendingFolders().Any(folder => !string.Equals(folder, BackupRoot, StringComparison.OrdinalIgnoreCase));
            if (Directory.Exists(BackupRoot))
                foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json"))
                    installed.Add(Inspect(Path.GetFileName(path)[..^".receipt.json".Length]));
        }
        catch (Exception error) when (IsStateError(error)) { problems.Add(Code(error)); }
        IReadOnlyList<string> relink = [];
        try { relink = installed.Count == 0 ? RelinkCandidates() : []; }
        catch (Exception error) when (IsStateError(error)) { problems.Add(Code(error)); }
        var gameRelink = false;
        try { gameRelink = GameRelinkCandidates().Count > 0; }
        catch (Exception error) when (IsStateError(error)) { problems.Add(Code(error)); }
        return new(here, elsewhere, installed, relink, problems) { GameRelink = gameRelink };
    }

    /// <summary>M9 for the game folder: receipts recorded for a game folder that no longer exists whose game files all match, as recorded, in the selected one.</summary>
    public IReadOnlyList<string> GameRelinkCandidates()
    {
        if (_game is null || !Directory.Exists(BackupRoot)) return [];
        var result = new List<string>();
        foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json"))
        {
            RejectLinks(path);
            var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path));
            if (receipt?.GameRoot is not { } old || string.Equals(old, _game, StringComparison.OrdinalIgnoreCase) || Directory.Exists(old)
                || !string.Equals(receipt.AzimuthRoot, _azimuth, StringComparison.OrdinalIgnoreCase)) continue;
            if (receipt.Files.Where(f => f.Root == "game" && !f.Preserve).All(f => Current(Resolve(_game, f.Path)) == (receipt.Enabled ? f.After : f.Before)))
                result.Add(receipt.Id);
        }
        return result;
    }

    /// <summary>Rewrites the game folder of <see cref="GameRelinkCandidates"/> to the selected one (journaled like any receipt change).</summary>
    public void RelinkGame()
    {
        using var gate = Lock();
        foreach (var id in GameRelinkCandidates())
        {
            var path = ReceiptPath(id);
            var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path))!;
            Execute(new(id + ".receipt.json", receipt with { GameRoot = _game }, []), null);
        }
        ReconcileWineOverrides();
    }

    private ModInstallation Inspect(string id)
    {
        try
        {
            var receipt = ReadReceipt(id) ?? throw new ModException("receipt vanished", ModErrors.Recovery);
            var manifest = SnapshotFor(receipt);
            var drift = DriftOf(receipt);
            IReadOnlyList<ManifestIniIssue> issues = [];
            if (manifest.ConfigPath is { } config && File.Exists(Target("azimuth", config))) issues = ReadSettings(manifest).Issues;
            return new(id, manifest, receipt, drift, issues, null);
        }
        catch (Exception error) when (IsStateError(error)) { return new(id, null, null, [], [], Code(error)); }
    }

    public static bool IsStateError(Exception error) => error is ModException or IOException or UnauthorizedAccessException or JsonException
        or InvalidOperationException or KeyNotFoundException or FormatException or ArgumentException or OverflowException or NullReferenceException;
    public static string Code(Exception error) => error is ModException mod ? mod.Code : error is JsonException or KeyNotFoundException or FormatException ? ModErrors.Recovery : ModErrors.Failed;

    /// <summary>
    /// H4: finishes or rolls back every interrupted operation in the bucket, including other Azimuth
    /// folders' (their recorded roots). What cannot be decided automatically (a committed change
    /// edited since, a refused database restore, a missing folder) is left for <see cref="ResolveRecovery"/>.
    /// </summary>
    public void Recover()
    {
        using var gate = Lock(allowRecovery: true);
        var failures = new List<string>();
        foreach (var folder in PendingFolders().ToArray())
        {
            try
            {
                if (ForFolder(folder) is { } manager) manager.RecoverHere();
                else failures.Add("installation unavailable");
            }
            catch (Exception error) when (IsStateError(error) && error is not SimulatedCrashException) { failures.Add(Code(error)); }
        }
        if (failures.Count > 0) throw new ModException("recovery needs a decision: " + string.Join(", ", failures), ModErrors.Recovery);
    }

    /// <summary>
    /// H4 "accept current state and archive journal": every interrupted operation is closed without
    /// changing mod files (drift then shows as a warning with its own actions); a session database is
    /// returned to its original state with the current copy kept as a backup; unreadable receipts are
    /// archived so the mod can be adopted or installed again. Always available.
    /// </summary>
    public void ResolveRecovery()
    {
        using var gate = Lock(allowRecovery: true);
        foreach (var folder in PendingFolders().ToArray())
        {
            try
            {
                if (ForFolder(folder) is { } manager) { manager.ResolveHere(); continue; }
            }
            catch (Exception error) when (IsStateError(error) && error is not SimulatedCrashException) { } // one folder never stops the others
            foreach (var name in new[] { "pending.json", "database-session.json" })
            {
                var path = Path.Combine(folder, name);
                RejectLinks(path);
                if (File.Exists(path)) File.Move(path, Path.Combine(folder, Path.GetFileNameWithoutExtension(name) + "-unresolved-" + Guid.NewGuid().ToString("N") + ".json"));
            }
        }
        if (!Directory.Exists(BackupRoot)) return;
        foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json").ToArray())
        {
            var id = Path.GetFileName(path)[..^".receipt.json".Length];
            try { _ = SnapshotFor(ReadReceipt(id)!); }
            catch (ModException error) when (error.Code is ModErrors.Relink or ModErrors.Layout) { } // intact record of a moved folder, or of the old layout (OD-233): relink or move, never archive
            catch (Exception error) when (error is JsonException || error is ModException { Code: ModErrors.Recovery } || error is NullReferenceException)
            {
                File.Move(path, Path.Combine(BackupRoot, "corrupt-" + Guid.NewGuid().ToString("N") + ".json"));
            }
        }
    }

    private void RecoverHere()
    {
        RestoreSessionDatabase();
        var path = Path.Combine(BackupRoot, "pending.json");
        if (!File.Exists(path)) return;
        RejectLinks(path);
        var journal = JsonSerializer.Deserialize<ModFileJournal>(File.ReadAllBytes(path)) ?? throw new ModException("invalid journal", ModErrors.Recovery);
        ValidateGameRoot(journal.Receipt);
        var receiptPath = Safe(BackupRoot, journal.ReceiptName);
        // Committed when the receipt on disk is the journal's own (same transaction, same content; compared after one deserialization each).
        var committed = File.Exists(receiptPath) && JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(receiptPath)) is { } written
            && written.TransactionId == journal.Receipt.TransactionId && JsonSerializer.Serialize(written) == JsonSerializer.Serialize(journal.Receipt);
        if (committed)
        {
            foreach (var change in journal.Changes)
            {
                var target = Target(change.Root, change.Path); var actual = Current(target);
                if (actual == change.After) continue;
                if (change.After is null && DeletionInProgress(change, actual)) { RemoveFile(target); continue; } // finish an interrupted delete
                throw new ModException("committed files changed", ModErrors.Recovery);
            }
        }
        else Rollback(journal);
        ArchiveJournal(path);
    }

    private void ResolveHere()
    {
        ResolveSessionDatabase();
        var path = Path.Combine(BackupRoot, "pending.json");
        RejectLinks(path);
        if (File.Exists(path)) File.Move(path, Path.Combine(BackupRoot, "transaction-accepted-" + Guid.NewGuid().ToString("N") + ".json"));
    }

    // An earlier launcher's native mod session (OD-113, retired by OD-232): nothing publishes a session database any more, but one an
    // interrupted session left is still put back exactly as before, so the stock database is never shadowed by a session copy.

    private string SessionJournal => Path.Combine(BackupRoot, "database-session.json");

    private static void NoDatabaseSidecars(string path)
    {
        foreach (var suffix in new[] { "-wal", "-shm", "-journal" })
            if (File.Exists(path + suffix)) throw new ModException("close SQLite writers and checkpoint the database first");
    }

    private SessionDatabaseJournal ReadSessionJournal()
    {
        RejectLinks(SessionJournal);
        var journal = JsonSerializer.Deserialize<SessionDatabaseJournal>(File.ReadAllBytes(SessionJournal)) ?? throw new ModException("invalid database session journal", ModErrors.Recovery);
        if (!string.Equals(journal.AzimuthRoot, _azimuth, StringComparison.OrdinalIgnoreCase)
            || !string.Equals(journal.GameRoot, _game, StringComparison.OrdinalIgnoreCase)) throw new ModException("database session belongs to another installation", ModErrors.Recovery);
        return journal;
    }

    /// <summary>
    /// Ends a session database: the baseline returns when an override existed before; otherwise the
    /// session's own copy is removed with the folders the session created (M11), so nothing shadows
    /// the stock database afterwards. The stock source is never touched.
    /// </summary>
    private void RestoreSessionDatabase()
    {
        CheckGame();
        if (!File.Exists(SessionJournal)) return;
        var journal = ReadSessionJournal();
        var target = Safe(_game!, journal.Target); NoDatabaseSidecars(target);
        var current = Current(target);
        if (current != journal.Baseline && current != journal.Prepared && !(current is null && !journal.Existed))
            throw new ModException("session database changed outside the runtime; preserved for recovery", ModErrors.Recovery);
        var original = ObjectBytes(journal.Baseline); _ = ObjectBytes(journal.Prepared);
        CheckGame();
        FinishSessionDatabase(journal, target, original);
    }

    private void FinishSessionDatabase(SessionDatabaseJournal journal, string target, byte[] original)
    {
        if (journal.Existed) Atomic(target, original);
        else
        {
            RemoveFile(target);
            foreach (var directory in (journal.CreatedDirectories ?? []).OrderByDescending(d => d.Length))
            {
                var path = Safe(_game!, directory);
                if (Directory.Exists(path) && !Directory.EnumerateFileSystemEntries(path).Any()) Directory.Delete(path, recursive: false);
            }
        }
        File.Move(SessionJournal, Path.Combine(BackupRoot, "database-session-" + Guid.NewGuid().ToString("N") + ".json"));
    }

    /// <summary>
    /// H4c resolution for a refused restore: the current database and its SQLite sidecars are kept as
    /// backup objects (listed in the archived record), then the original state returns as above.
    /// </summary>
    private void ResolveSessionDatabase()
    {
        CheckGame();
        if (!File.Exists(SessionJournal)) return;
        SessionDatabaseJournal journal;
        try { journal = ReadSessionJournal(); }
        catch (Exception error) when (error is ModException or JsonException)
        {
            File.Move(SessionJournal, Path.Combine(BackupRoot, "database-session-unreadable-" + Guid.NewGuid().ToString("N") + ".json"));
            return;
        }
        var target = Safe(_game!, journal.Target);
        var kept = new List<string>();
        foreach (var path in new[] { target, target + "-wal", target + "-shm", target + "-journal" })
        {
            if (!File.Exists(path)) continue;
            if (new FileInfo(path).Length > 512L * 1024 * 1024) throw new ModException("database too large to keep a backup", ModErrors.Recovery);
            kept.Add(Path.GetFileName(path) + "=" + Store(File.ReadAllBytes(path)));
        }
        Atomic(Path.Combine(BackupRoot, "database-session-kept-" + Guid.NewGuid().ToString("N") + ".json"), JsonSerializer.SerializeToUtf8Bytes(kept));
        foreach (var sidecar in new[] { "-wal", "-shm", "-journal" }) RemoveFile(target + sidecar);
        FinishSessionDatabase(journal, target, ObjectBytes(journal.Baseline));
    }

    /// <summary>A manager for another bucket folder, built from the roots its own records name; null when that Azimuth folder is gone.</summary>
    private ManagedModFiles? ForFolder(string folder)
    {
        if (string.Equals(Path.TrimEndingDirectorySeparator(folder), BackupRoot, StringComparison.OrdinalIgnoreCase)) return this;
        string? azimuth = null, game = null;
        try
        {
            var session = Path.Combine(folder, "database-session.json");
            var pending = Path.Combine(folder, "pending.json");
            if (File.Exists(session) && JsonSerializer.Deserialize<SessionDatabaseJournal>(File.ReadAllBytes(session)) is { } s) { azimuth = s.AzimuthRoot; game = s.GameRoot; }
            if (File.Exists(pending) && JsonSerializer.Deserialize<ModFileJournal>(File.ReadAllBytes(pending)) is { } p) { azimuth ??= p.Receipt?.AzimuthRoot; game ??= p.Receipt?.GameRoot; }
        }
        catch (Exception error) when (IsStateError(error)) { return null; }
        if (azimuth is null || !Directory.Exists(azimuth) || game is not null && !Directory.Exists(game)
            || !string.Equals(Identity(azimuth), Path.GetFileName(Path.TrimEndingDirectorySeparator(folder)), StringComparison.OrdinalIgnoreCase)) return null;
        return new ManagedModFiles(azimuth, game, _state, _gameRunning, _packageCache, _launcherRoots) { FaultPoint = FaultPoint, WineOverrides = WineOverrides };
    }

    /// <summary>
    /// M9: bucket folders whose Azimuth folder no longer exists and whose managed Azimuth files are
    /// all present, as recorded, in this folder (the installation moved here). Only offered while this
    /// folder manages nothing.
    /// </summary>
    public IReadOnlyList<string> RelinkCandidates()
    {
        if (!Directory.Exists(_bucket) || Directory.Exists(BackupRoot) && Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Any()) return [];
        var result = new List<string>();
        foreach (var folder in Directory.EnumerateDirectories(_bucket))
        {
            if (string.Equals(folder, BackupRoot, StringComparison.OrdinalIgnoreCase)) continue;
            try { if (RelinkReceipts(folder) is { Count: > 0 }) result.Add(Path.GetFileName(folder)); }
            catch (Exception error) when (IsStateError(error)) { }
        }
        return result;
    }

    private List<(string Path, ManagedModReceipt Receipt)>? RelinkReceipts(string folder)
    {
        RejectLinks(folder);
        if (File.Exists(Path.Combine(folder, "pending.json")) || File.Exists(Path.Combine(folder, "database-session.json"))) return null;
        var receipts = Directory.EnumerateFiles(folder, "*.receipt.json")
            .Select(path => (path, JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)) ?? throw new ModException("invalid receipt", ModErrors.Recovery))).ToList();
        if (receipts.Count == 0 || receipts.Select(r => r.Item2.AzimuthRoot).Distinct(StringComparer.OrdinalIgnoreCase).Count() != 1) return null;
        var old = receipts[0].Item2.AzimuthRoot;
        if (old is null || Directory.Exists(old) || !string.Equals(Identity(old), Path.GetFileName(folder), StringComparison.OrdinalIgnoreCase)) return null;
        // OD-233: an installation recorded in the old flat layout whose folders here have already moved to their game's folder (the move
        // ran before the relink) is checked, and relinked, where its files now are, as MigrateLayout would have rewritten it.
        receipts = [.. receipts.Select(r => (r.Item1, FollowsItsFolder(r.Item2, SnapshotGame(folder, r.Item2), _azimuth) ?? r.Item2))];
        foreach (var (_, receipt) in receipts)
            foreach (var file in receipt.Files.Where(f => !f.Preserve))
            {
                var expected = receipt.Enabled ? file.After : file.Before;
                var root = file.Root == "azimuth" ? _azimuth : receipt.GameRoot is { } recorded && Directory.Exists(recorded) ? recorded : _game;
                if (root is null || Current(Resolve(root, file.Path)) != expected) return null;
            }
        return receipts;
    }

    /// <summary>M9 "relink": moves a candidate's records to this Azimuth folder and rewrites their roots.</summary>
    public void Relink(string folderName)
    {
        ArgumentNullException.ThrowIfNull(folderName);
        if (folderName.Length != 64 || folderName.Any(c => !char.IsAsciiHexDigitLower(c) && !char.IsAsciiDigit(c))) throw new ModException("invalid installation record", ModErrors.Relink);
        using var gate = Lock();
        if (Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Any()) throw new ModException("this folder already manages mods", ModErrors.Relink);
        var folder = Path.Combine(_bucket, folderName);
        var receipts = RelinkReceipts(folder) ?? throw new ModException("files differ from the recorded installation", ModErrors.Relink);
        foreach (var entry in Directory.EnumerateFileSystemEntries(folder).ToArray())
        {
            var destination = Path.Combine(BackupRoot, Path.GetFileName(entry));
            if (Directory.Exists(entry))
            {
                Directory.CreateDirectory(destination);
                foreach (var file in Directory.EnumerateFiles(entry))
                {
                    var target = Path.Combine(destination, Path.GetFileName(file));
                    if (!File.Exists(target)) File.Move(file, target);
                }
            }
            else if (!entry.EndsWith(".receipt.json", StringComparison.Ordinal) && !File.Exists(destination)) File.Move(entry, destination);
        }
        foreach (var (path, receipt) in receipts)
        {
            var game = receipt.GameRoot is { } recorded && Directory.Exists(recorded) ? recorded : receipt.Files.Any(f => f.Root == "game") ? _game : null;
            Atomic(Path.Combine(BackupRoot, Path.GetFileName(path)), JsonSerializer.SerializeToUtf8Bytes(receipt with { AzimuthRoot = _azimuth, GameRoot = game }, Json));
            File.Move(path, Path.Combine(folder, "relinked-" + Guid.NewGuid().ToString("N") + ".json"));
        }
        File.WriteAllText(Path.Combine(folder, "relinked-to.txt"), Path.GetFileName(BackupRoot));
        ReconcileWineOverrides();
    }

    /// <summary>
    /// M5 retention, under the lock and never during recovery: crash leftovers of atomic writes go;
    /// archived records older than <see cref="ArchiveRetention"/> beyond the newest
    /// <see cref="ArchiveMinimum"/> go; backup objects and manifest snapshots no remaining record names
    /// go; session folders older than <see cref="SessionRetention"/> go.
    /// </summary>
    public void Prune(DateTimeOffset now)
    {
        if (!Directory.Exists(BackupRoot)) return;
        using var gate = Lock();
        CleanStaging();
        var archives = Directory.EnumerateFiles(BackupRoot, "*.json").Where(path => Path.GetFileName(path) is var name
                && (name.StartsWith("transaction-", StringComparison.Ordinal) || name.StartsWith("database-session-", StringComparison.Ordinal)
                    || name.StartsWith("uninstalled-", StringComparison.Ordinal) || name.StartsWith("corrupt-", StringComparison.Ordinal)))
            .OrderByDescending(File.GetLastWriteTimeUtc).ToArray();
        foreach (var old in archives.Skip(ArchiveMinimum).Where(path => now - File.GetLastWriteTimeUtc(path) > ArchiveRetention)) RemoveFile(old);
        var receiptIds = Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Select(p => Path.GetFileName(p)[..^".receipt.json".Length]).ToHashSet(StringComparer.Ordinal);
        foreach (var legacy in Directory.EnumerateFiles(BackupRoot, "*.manifest.json"))
            if (!receiptIds.Contains(Path.GetFileName(legacy)[..^".manifest.json".Length])) RemoveFile(legacy);
        RemoveUnreferenced();
        var sessions = Path.Combine(BackupRoot, "sessions");
        if (!Directory.Exists(sessions)) return;
        RejectLinks(sessions);
        foreach (var session in Directory.EnumerateDirectories(sessions).ToArray())
        {
            if (now - Directory.GetLastWriteTimeUtc(session) <= SessionRetention) continue;
            try
            {
                RejectLinks(session);
                foreach (var file in Directory.EnumerateFiles(session).ToArray()) RemoveFile(file);
                Directory.Delete(session, recursive: false);
            }
            catch (IOException) { } // a session still in use keeps its files
        }
    }

    /// <summary>Backup objects and manifest snapshots no remaining record (receipts, archives, held mods) names go.</summary>
    private void RemoveUnreferenced()
    {
        var referenced = new HashSet<string>(StringComparer.Ordinal);
        foreach (var record in Directory.EnumerateFiles(BackupRoot, "*.json"))
            foreach (Match match in Digests().Matches(File.ReadAllText(record))) referenced.Add(match.Value);
        foreach (var store in new[] { "objects", "manifests" })
        {
            var directory = Path.Combine(BackupRoot, store);
            if (!Directory.Exists(directory)) continue;
            RejectLinks(directory);
            foreach (var path in Directory.EnumerateFiles(directory).ToArray())
                if (!referenced.Contains(Path.GetFileNameWithoutExtension(path))) RemoveFile(path);
        }
    }

    /// <summary>L12: <c>Atomic</c>/<see cref="RemoveFile"/> crash leftovers in the backup folder and next to managed files.</summary>
    private void CleanStaging()
    {
        foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.tmp", SearchOption.AllDirectories).Where(p => IsStagingLeftover(Path.GetFileName(p))).ToArray())
        {
            RejectLinks(path); RemoveFile(path);
        }
        foreach (var path in Directory.EnumerateFiles(_bucket, "*.tmp").Where(p => IsStagingLeftover(Path.GetFileName(p))).ToArray()) RemoveFile(path);
        foreach (var id in Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Select(p => Path.GetFileName(p)[..^".receipt.json".Length]).ToArray())
        {
            ManagedModReceipt? receipt;
            try { receipt = ReadReceipt(id); }
            catch (Exception error) when (IsStateError(error)) { continue; }
            foreach (var group in (receipt?.Files ?? []).GroupBy(f => Path.GetDirectoryName(Target(f.Root, f.Path))!, StringComparer.OrdinalIgnoreCase))
            {
                if (!Directory.Exists(group.Key)) continue;
                var names = group.Select(f => Path.GetFileName(f.Path)).ToArray();
                foreach (var path in Directory.EnumerateFiles(group.Key, "*.tmp").ToArray())
                {
                    var name = Path.GetFileName(path);
                    if (IsStagingLeftover(name) && (name.StartsWith('.') || names.Any(n => name.StartsWith(n + ".", StringComparison.OrdinalIgnoreCase))))
                    {
                        RejectLinks(path); RemoveFile(path);
                    }
                }
            }
        }
    }

    [GeneratedRegex("[0-9a-f]{64}")]
    private static partial Regex Digests();
}
