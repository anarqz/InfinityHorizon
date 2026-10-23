// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO.Compression;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Mods;

public sealed record ManagedModFile(string Root, string Path, string? Before, string After, bool Preserve);
public sealed record ManagedModReceipt(string Id, string ManifestHash, bool Enabled, IReadOnlyList<ManagedModFile> Files, string? GameRoot = null, string? AzimuthRoot = null);
public sealed record ModFileChange(string Root, string Path, string? Before, string? After);
public sealed record ModFileJournal(string ReceiptName, ManagedModReceipt Receipt, IReadOnlyList<ModFileChange> Changes);

/// <summary>
/// File-only transactions. Immutable SHA-256 objects live in game_backup, scoped to this exact
/// installation. A durable journal precedes the first write; interrupted work blocks new work
/// until explicitly recovered. Database writers are refused until runtime coordination exists.
/// </summary>
public sealed class ManagedModFiles
{
    private readonly string _azimuth;
    private readonly string? _game;
    private readonly Func<bool> _gameRunning;
    private readonly string _bucket;
    private static readonly JsonSerializerOptions Json = new() { WriteIndented = true };
    public string BackupRoot { get; }

    public ManagedModFiles(string azimuth, string? game, string stateDirectory, Func<bool> gameRunning)
    {
        _azimuth = Path.TrimEndingDirectorySeparator(Path.GetFullPath(azimuth));
        _game = game is null ? null : Path.TrimEndingDirectorySeparator(Path.GetFullPath(game));
        _gameRunning = gameRunning;
        var identity = ModManifest.Hash(Encoding.UTF8.GetBytes(_azimuth.TrimEnd('\\', '/').ToUpperInvariant()));
        _bucket = Path.GetFullPath(Path.Combine(stateDirectory, "game_backup"));
        BackupRoot = Path.Combine(_bucket, identity);
        RejectLinks(_azimuth); RejectLinks(BackupRoot);
    }

    public bool RecoveryRequired => File.Exists(Path.Combine(BackupRoot, "pending.json"));

    public ManagedModReceipt? Receipt(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        var path = ReceiptPath(manifest.Id);
        if (!File.Exists(path)) return null;
        var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)) ?? throw new ModException("invalid receipt");
        if (receipt.Id != manifest.Id || receipt.ManifestHash != manifest.Digest) throw new ModException("installed manifest differs; update requires review");
        ValidateGameRoot(receipt);
        foreach (var file in receipt.Files) _ = Target(file.Root, file.Path);
        return receipt;
    }

    public bool Verified(ModManifest manifest)
    {
        var receipt = Receipt(manifest);
        return !RecoveryRequired && receipt is not null && receipt.Files.Where(f => !f.Preserve).All(f =>
            Current(Target(f.Root, f.Path)) == (receipt.Enabled ? f.After : f.Before));
    }

    public async Task InstallAsync(HttpClient http, ModManifest manifest, CancellationToken cancellation, IProgress<string>? progress = null)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        // This is deliberately independent of remote review.status and catalog availability.
        if (!manifest.ApprovedForFileInstall) throw new ModException("this manifest is not approved for installation");
        progress?.Report("download");
        using var response = await ModManager.DownloadAsync(http, new Uri(manifest.PackageUrl), cancellation).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        var bytes = await DevelopmentModCatalog.ReadBoundedAsync(body, Math.Min(manifest.PackageBytes, 64 * 1024 * 1024), cancellation).ConfigureAwait(false);
        cancellation.ThrowIfCancellationRequested();
        progress?.Report("verify");
        await Task.Run(() => InstallVerifiedPackage(manifest, bytes, progress), cancellation).ConfigureAwait(false);
    }

    /// <summary>Verifies all archive bytes before touching any installation file.</summary>
    public void InstallVerifiedPackage(ModManifest manifest, byte[] package, IProgress<string>? progress = null)
    {
        ArgumentNullException.ThrowIfNull(manifest); ArgumentNullException.ThrowIfNull(package);
        if (manifest.NeedsDatabaseCoordination || manifest.Dependencies.Count > 0) throw new ModException("runtime or dependencies require coordination");
        if (manifest.PackageBytes != package.Length || ModManifest.Hash(package) != manifest.PackageHash) throw new ModException("package digest mismatch");
        using var gate = Lock();
        if (Receipt(manifest) is not null) throw new ModException("already managed; use enable or disable");
        using var zip = new ZipArchive(new MemoryStream(package), ZipArchiveMode.Read);
        if (zip.Entries.Count > 4096 || zip.Entries.Select(e => e.FullName).Distinct(StringComparer.OrdinalIgnoreCase).Count() != zip.Entries.Count)
            throw new ModException("duplicate or excessive archive entries");
        var verified = new Dictionary<ManifestFile, byte[]>();
        foreach (var file in manifest.Files)
        {
            var entry = zip.GetEntry(file.Source) ?? throw new ModException("archive entry missing");
            if (entry.Length != file.Bytes) throw new ModException("archive entry length mismatch");
            using var stream = entry.Open();
            var content = new byte[checked((int)file.Bytes)];
            stream.ReadExactly(content);
            if (stream.ReadByte() != -1 || ModManifest.Hash(content) != file.Hash) throw new ModException("file digest mismatch");
            verified.Add(file, content);
        }
        var owned = new List<ManagedModFile>();
        var changes = new List<ModFileChange>();
        foreach (var (file, bytes) in verified)
        {
            var target = Target(file.Root, file.Destination);
            var before = Current(target);
            if (file.Mode == "seed" && before is not null && !(manifest.Id == ModCatalog.ExtraOptionsId && file.Role == "config")) continue;
            if (file.Mode == "add" && before is not null || file.Mode == "replace" && before is null) throw new ModException("file precondition failed");
            // An existing identical binary has no known predecessor. Do not invent an original
            // that would leave the mod loaded after Off. Legacy management remains available.
            if (before == file.Hash && file.Role == "binary") throw new ModException("existing mod requires legacy management");
            if (before is not null) Store(File.ReadAllBytes(target));
            var afterBytes = bytes;
            if (manifest.Id == ModCatalog.ExtraOptionsId && file.Role == "config")
            {
                var ini = before is not null ? ExtraOptionsIni.Load(target) : ExtraOptionsIni.Parse(Encoding.UTF8.GetString(bytes));
                ini.EnforceInfinityLocks();
                // Preserve a pre-existing INI's exact encoding through its own serializer.
                afterBytes = ini.ToBytes();
            }
            var after = Store(afterBytes);
            var preserve = file.Role == "config";
            owned.Add(new(file.Root, file.Destination, before, after, preserve));
            changes.Add(new(file.Root, file.Destination, before, after));
        }
        RejectOverlaps(owned);
        Atomic(Path.Combine(BackupRoot, manifest.Id + ".manifest.json"), manifest.Snapshot());
        Execute(new(manifest.Id + ".receipt.json", new(manifest.Id, manifest.Digest, true, owned,
            owned.Any(f => f.Root == "game") ? _game : null, _azimuth), changes), progress);
    }

    public void SetEnabled(ModManifest manifest, bool enabled, IProgress<string>? progress = null)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        using var gate = Lock();
        var receipt = Receipt(manifest) ?? throw new ModException("mod has no installation receipt");
        if (receipt.Enabled == enabled) return;
        var changes = receipt.Files.Where(f => !f.Preserve).Select(f => new ModFileChange(f.Root, f.Path,
            enabled ? f.Before : f.After, enabled ? f.After : f.Before)).ToArray();
        Execute(new(manifest.Id + ".receipt.json", receipt with { Enabled = enabled }, changes), progress);
    }

    /// <summary>Conservative rollback: refuses any target changed by another writer since the interruption.</summary>
    public void Recover()
    {
        using var gate = Lock(allowRecovery: true);
        var path = Path.Combine(BackupRoot, "pending.json");
        if (!File.Exists(path)) return;
        var journal = JsonSerializer.Deserialize<ModFileJournal>(File.ReadAllBytes(path)) ?? throw new ModException("invalid journal");
        ValidateGameRoot(journal.Receipt);
        // If the receipt was durably committed, complete the commit rather than rolling it back.
        var receiptPath = Safe(BackupRoot, journal.ReceiptName);
        if (File.Exists(receiptPath) && File.ReadAllText(receiptPath) == JsonSerializer.Serialize(journal.Receipt, Json))
        {
            foreach (var change in journal.Changes)
                if (Current(Target(change.Root, change.Path)) != change.After) throw new ModException("committed files changed");
        }
        else Rollback(journal);
        ArchiveJournal(path);
    }

    private void Execute(ModFileJournal journal, IProgress<string>? progress)
    {
        CheckGame();
        foreach (var change in journal.Changes)
        {
            if (Current(Target(change.Root, change.Path)) != change.Before) throw new ModException("files changed outside the launcher");
            if (change.Before is not null) _ = ObjectBytes(change.Before);
            if (change.After is not null) _ = ObjectBytes(change.After);
        }
        var pending = Path.Combine(BackupRoot, "pending.json");
        Atomic(pending, JsonSerializer.SerializeToUtf8Bytes(journal, Json));
        progress?.Report("apply");
        try
        {
            foreach (var change in journal.Changes)
            {
                CheckGame();
                if (Current(Target(change.Root, change.Path)) != change.Before) throw new ModException("files changed outside the launcher");
                Put(Target(change.Root, change.Path), change.After);
            }
            Atomic(Safe(BackupRoot, journal.ReceiptName), JsonSerializer.SerializeToUtf8Bytes(journal.Receipt, Json));
        }
        catch
        {
            // Keep the journal if rollback fails (for example an antivirus lock), so another
            // operation cannot silently continue with a partly changed installation.
            Rollback(journal);
            ArchiveJournal(pending);
            throw;
        }
        ArchiveJournal(pending);
    }

    private void Rollback(ModFileJournal journal)
    {
        CheckGame();
        foreach (var change in journal.Changes)
        {
            var actual = Current(Target(change.Root, change.Path));
            if (actual != change.Before && actual != change.After) throw new ModException("recovery conflicts with external changes");
            if (change.Before is not null) _ = ObjectBytes(change.Before);
        }
        foreach (var change in journal.Changes.Reverse())
        {
            CheckGame();
            var target = Target(change.Root, change.Path);
            if (Current(target) != change.Before) Put(target, change.Before);
        }
    }

    private void ArchiveJournal(string pending) => File.Move(pending, Path.Combine(BackupRoot, "transaction-" + Guid.NewGuid().ToString("N") + ".json"));
    private void ValidateGameRoot(ManagedModReceipt receipt)
    {
        if (!string.Equals(_azimuth, receipt.AzimuthRoot, StringComparison.OrdinalIgnoreCase))
            throw new ModException("azimuth folder differs from the installation receipt");
        if (receipt.Files.Any(f => f.Root == "game") && !string.Equals(_game, receipt.GameRoot, StringComparison.OrdinalIgnoreCase))
            throw new ModException("game folder differs from the installation receipt");
    }
    private void RejectOverlaps(IReadOnlyList<ManagedModFile> proposed)
    {
        var targets = proposed.Select(f => Target(f.Root, f.Path)).ToHashSet(StringComparer.OrdinalIgnoreCase);
        foreach (var folder in Directory.EnumerateDirectories(_bucket))
        {
            RejectLinks(folder);
            foreach (var path in Directory.EnumerateFiles(folder, "*.receipt.json"))
            {
                RejectLinks(path);
                var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)) ?? throw new ModException("invalid receipt");
                if (receipt.Files.Any(existing => targets.Contains(Safe(existing.Root switch
                    {
                        "azimuth" => receipt.AzimuthRoot ?? throw new ModException("missing receipt root"),
                        "game" => receipt.GameRoot ?? throw new ModException("missing receipt root"),
                        _ => throw new ModException("invalid receipt root"),
                    }, existing.Path))))
                    throw new ModException("another mod owns this path");
            }
        }
    }
    private string ReceiptPath(string id) => Safe(BackupRoot, id + ".receipt.json");
    private FileStream Lock(bool allowRecovery = false)
    {
        CheckGame(); RejectLinks(BackupRoot);
        Directory.CreateDirectory(BackupRoot);
        var gate = new FileStream(Path.Combine(_bucket, "operation.lock"), FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
        try
        {
            foreach (var folder in Directory.EnumerateDirectories(_bucket))
            {
                RejectLinks(folder);
                if (File.Exists(Path.Combine(folder, "pending.json"))
                    && !(allowRecovery && string.Equals(folder, BackupRoot, StringComparison.OrdinalIgnoreCase)))
                    throw new ModException("interrupted mod operation needs recovery");
            }
        }
        catch { gate.Dispose(); throw; }
        return gate;
    }
    private void CheckGame() { if (_gameRunning()) throw new ModException("close the game first"); }
    private string Target(string root, string relative)
    {
        if (relative.EndsWith(".slt", StringComparison.OrdinalIgnoreCase) || relative.EndsWith(".db", StringComparison.OrdinalIgnoreCase)
            || relative.EndsWith(".sqlite", StringComparison.OrdinalIgnoreCase) || relative.EndsWith(".sqlite3", StringComparison.OrdinalIgnoreCase))
            throw new ModException("databases require migrations");
        if (root == "azimuth" && (relative.StartsWith("data/", StringComparison.OrdinalIgnoreCase)
            || relative.Equals("azimuth.exe", StringComparison.OrdinalIgnoreCase) || relative.Equals("infinity.dll", StringComparison.OrdinalIgnoreCase)))
            throw new ModException("protected launcher path");
        return Safe(root switch { "azimuth" => _azimuth, "game" => _game ?? throw new ModException("game folder not selected"), _ => throw new ModException("invalid root") }, relative);
    }
    private static string Safe(string root, string relative)
    {
        ModManifest.ValidatePath(relative);
        var target = Path.GetFullPath(Path.Combine(root, relative));
        if (!target.StartsWith(root.TrimEnd('\\', '/') + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase)) throw new ModException("path escaped root");
        RejectLinks(target);
        return target;
    }
    private static void RejectLinks(string path)
    {
        for (var current = Path.GetFullPath(path); current is not null; current = Path.GetDirectoryName(current))
            if ((File.Exists(current) || Directory.Exists(current)) && (File.GetAttributes(current) & FileAttributes.ReparsePoint) != 0)
                throw new ModException("linked paths are not supported for managed mods");
    }
    private static string? Current(string path) => File.Exists(path) ? ModManifest.Hash(File.ReadAllBytes(path)) : null;
    private string Store(byte[] bytes)
    {
        var digest = ModManifest.Hash(bytes);
        var path = Safe(BackupRoot, "objects/" + digest);
        if (!File.Exists(path)) Atomic(path, bytes);
        else if (Current(path) != digest) throw new ModException("backup object damaged");
        return digest;
    }
    private byte[] ObjectBytes(string digest)
    {
        if (digest.Length != 64 || digest.Any(c => !char.IsAsciiHexDigit(c))) throw new ModException("invalid backup reference");
        var bytes = File.ReadAllBytes(Safe(BackupRoot, "objects/" + digest));
        if (ModManifest.Hash(bytes) != digest) throw new ModException("backup object damaged");
        return bytes;
    }
    private void Put(string path, string? digest)
    {
        if (digest is null) File.Delete(path);
        else Atomic(path, ObjectBytes(digest));
    }
    private static void Atomic(string path, byte[] bytes)
    {
        RejectLinks(path);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var staging = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        try
        {
            using (var stream = new FileStream(staging, FileMode.CreateNew, FileAccess.Write, FileShare.None))
            { stream.Write(bytes); stream.Flush(true); }
            File.Move(staging, path, overwrite: true);
        }
        finally { if (File.Exists(staging)) File.Delete(staging); }
    }
}
