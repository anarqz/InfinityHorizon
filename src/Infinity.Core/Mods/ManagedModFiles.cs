// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO.Compression;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Mods;

public sealed record ManagedModFile(string Root, string Path, string? Before, string After, bool Preserve);
public sealed record ManagedModReceipt(string Id, string ManifestHash, bool Enabled, IReadOnlyList<ManagedModFile> Files, string? GameRoot = null, string? AzimuthRoot = null, string? TransactionId = null);
public sealed record ModFileChange(string Root, string Path, string? Before, string? After);
public sealed record ModFileJournal(string ReceiptName, ManagedModReceipt Receipt, IReadOnlyList<ModFileChange> Changes);

public sealed partial class ManagedModFiles
{
    private readonly string _azimuth;
    private readonly string? _game;
    private readonly string _state;
    private readonly Func<bool> _gameRunning;
    private readonly string _bucket;
    private readonly string? _packageCache;
    private static readonly JsonSerializerOptions Json = new() { WriteIndented = true };
    public string BackupRoot { get; }

    public ManagedModFiles(string azimuth, string? game, string stateDirectory, Func<bool> gameRunning, string? packageCache = null)
    {
        _azimuth = Path.TrimEndingDirectorySeparator(Path.GetFullPath(azimuth));
        _game = game is null ? null : Path.TrimEndingDirectorySeparator(Path.GetFullPath(game));
        _gameRunning = gameRunning;
        _packageCache = packageCache is null ? null : Path.TrimEndingDirectorySeparator(Path.GetFullPath(packageCache));
        var identity = ModManifest.Hash(Encoding.UTF8.GetBytes(_azimuth.TrimEnd('\\', '/').ToUpperInvariant()));
        _state = Path.TrimEndingDirectorySeparator(Path.GetFullPath(stateDirectory));
        _bucket = Path.Combine(_state, "game_backup");
        BackupRoot = Path.Combine(_bucket, identity);
        RejectLinks(_azimuth); RejectLinks(BackupRoot);
    }

    public bool RecoveryRequired => File.Exists(Path.Combine(BackupRoot, "pending.json")) || File.Exists(SessionJournal);

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
        if (!manifest.SupportedForInstall) throw new ModException("this manifest uses unsupported installation capabilities");
        var packages = new Dictionary<string, byte[]>();
        foreach (var (name, spec) in PackageSpecs(manifest))
        {
            progress?.Report("download");
            var hash = spec.GetProperty("sha256").GetString()!;
            var size = spec.GetProperty("bytes").GetInt64();
            var cached = _packageCache is null ? null : Safe(_packageCache, hash + ".zip");
            byte[] bytes;
            if (cached is not null && File.Exists(cached))
            {
                if (new FileInfo(cached).Length != size) throw new ModException("cached package size mismatch");
                bytes = await File.ReadAllBytesAsync(cached, cancellation).ConfigureAwait(false);
            }
            else
            {
                using var response = await ModDownloads.DownloadAsync(http, new Uri(spec.GetProperty("url").GetString()!), cancellation).ConfigureAwait(false);
                response.EnsureSuccessStatusCode();
                await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
                bytes = await DevelopmentModCatalog.ReadBoundedAsync(body, Math.Min(size, 64 * 1024 * 1024), cancellation).ConfigureAwait(false);
            }
            if (bytes.LongLength != size || ModManifest.Hash(bytes) != hash) throw new ModException("package digest mismatch");
            packages.Add(name, bytes);
        }
        progress?.Report("verify");
        await Task.Run(() => InstallVerifiedPackages(manifest, packages, progress), cancellation).ConfigureAwait(false);
    }

    public void InstallVerifiedPackage(ModManifest manifest, byte[] package, IProgress<string>? progress = null)
    {
        ArgumentNullException.ThrowIfNull(manifest); ArgumentNullException.ThrowIfNull(package);
        InstallVerifiedPackages(manifest, new Dictionary<string, byte[]> { ["primary"] = package }, progress);
    }

    internal void InstallVerifiedPackages(ModManifest manifest, IReadOnlyDictionary<string, byte[]> packages, IProgress<string>? progress = null)
    {
        if (!manifest.SupportedForInstall)
            throw new ModException("runtime or dependencies require coordination");
        using var gate = Lock();
        if (Receipt(manifest) is not null) throw new ModException("already managed; use enable or disable");
        CheckDependencies(manifest);
        var verified = new Dictionary<ManifestFile, byte[]>();
        foreach (var (name, spec) in PackageSpecs(manifest))
        {
            if (!packages.TryGetValue(name, out var package) || package.LongLength != spec.GetProperty("bytes").GetInt64()
                || ModManifest.Hash(package) != spec.GetProperty("sha256").GetString()) throw new ModException("package digest mismatch");
            using var zip = new ZipArchive(new MemoryStream(package), ZipArchiveMode.Read);
            if (zip.Entries.Count > 4096 || zip.Entries.Select(e => e.FullName).Distinct(StringComparer.OrdinalIgnoreCase).Count() != zip.Entries.Count)
                throw new ModException("duplicate or excessive archive entries");
            foreach (var file in manifest.Files.Where(f => f.Package == name))
            {
                var entry = zip.GetEntry(file.Source) ?? throw new ModException("archive entry missing");
                if (entry.Length != file.Bytes) throw new ModException("archive entry length mismatch");
                using var stream = entry.Open();
                var content = new byte[checked((int)file.Bytes)]; stream.ReadExactly(content);
                if (stream.ReadByte() != -1 || ModManifest.Hash(content) != file.Hash) throw new ModException("file digest mismatch");
                verified.Add(file, content);
            }
        }
        if (verified.Count != manifest.Files.Count) throw new ModException("unresolved package mapping");
        var owned = new List<ManagedModFile>();
        var changes = new List<ModFileChange>();
        foreach (var (file, bytes) in verified)
        {
            var target = Target(file.Root, file.Destination);
            var before = Current(target);
            if (file.Mode == "seed" && before is not null && file.Role != "config") continue;
            if (file.Mode == "add" && before is not null || file.Mode == "replace" && before is null) throw new ModException("file precondition failed");
            if (before == file.Hash && file.Role == "binary") throw new ModException("existing installation needs explicit adoption");
            if (before is not null) Store(File.ReadAllBytes(target));
            var afterBytes = bytes;
            if (file.Role == "config" && manifest.ConfigPath == file.Destination)
            {
                var ini = new ManifestIni(manifest, before is null ? bytes : File.ReadAllBytes(target));
                ini.EnforceLocks(); afterBytes = ini.ToBytes();
            }
            var after = Store(afterBytes);
            var preserve = file.Role == "config";
            owned.Add(new(file.Root, file.Destination, before, after, preserve));
            changes.Add(new(file.Root, file.Destination, before, after));
        }
        if (manifest.ConfigPath is not null && !owned.Any(f => f.Root == "azimuth" && f.Path == manifest.ConfigPath))
        {
            var config = Target("azimuth", manifest.ConfigPath!);
            var before = Current(config);
            var iniBytes = before is null ? ManifestIni.Defaults(manifest) : File.ReadAllBytes(config);
            var ini = new ManifestIni(manifest, iniBytes);
            ini.EnforceLocks(); iniBytes = ini.ToBytes();
            if (before is not null) Store(File.ReadAllBytes(config));
            if (before != ModManifest.Hash(iniBytes)) changes.Add(new("azimuth", manifest.ConfigPath!, before, Store(iniBytes)));
            owned.Add(new("azimuth", manifest.ConfigPath!, before, Store(iniBytes), true));
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
        if (enabled) CheckDependencies(manifest); else CheckDependents(manifest.Id);
        var changes = receipt.Files.Where(f => !f.Preserve).Select(f => new ModFileChange(f.Root, f.Path,
            enabled ? f.Before : f.After, enabled ? f.After : f.Before)).ToList();
        Execute(new(manifest.Id + ".receipt.json", receipt with { Enabled = enabled }, changes), progress);
    }

    public void Recover()
    {
        using var gate = Lock(allowRecovery: true);
        RestoreSessionDatabase();
        var path = Path.Combine(BackupRoot, "pending.json");
        if (!File.Exists(path)) return;
        var journal = JsonSerializer.Deserialize<ModFileJournal>(File.ReadAllBytes(path)) ?? throw new ModException("invalid journal");
        ValidateGameRoot(journal.Receipt);
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
        journal = journal with { Receipt = journal.Receipt with { TransactionId = Guid.NewGuid().ToString("N") } };
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
                if (receipt.Files.Any(existing => targets.Contains(Resolve(existing.Root switch
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
                if ((File.Exists(Path.Combine(folder, "pending.json")) || File.Exists(Path.Combine(folder, "database-session.json")))
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
            throw new ModException("databases require session coordination");
        var target = Safe(root switch { "azimuth" => _azimuth, "game" => _game ?? throw new ModException("game folder not selected"), _ => throw new ModException("invalid root") }, relative);
        if (Within(Path.Combine(_azimuth, "data"), target) || Within(Path.Combine(_azimuth, "azimuth.exe"), target)
            || Within(Path.Combine(_azimuth, "infinity.dll"), target) || Within(_state, target))
            throw new ModException("protected launcher path");
        return target;
    }
    private string Safe(string root, string relative)
    {
        var target = Resolve(root, relative);
        RejectLinks(target);
        return target;
    }
    private static string Resolve(string root, string relative)
    {
        ModManifest.ValidatePath(relative);
        var target = Path.GetFullPath(Path.Combine(root, relative));
        if (!target.StartsWith(root.TrimEnd('\\', '/') + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase)) throw new ModException("path escaped root");
        return target;
    }
    private void RejectLinks(string path)
    {
        var full = Path.GetFullPath(path);
        var boundary = new[] { _azimuth, _game, _state, _packageCache }.Where(root => root is not null && Within(root, full))
            .MaxBy(root => root!.Length) ?? throw new ModException("path escaped root");
        RejectLinks(full, boundary, Process.WineHost.Detected);
    }
    private static void RejectLinks(string path, string boundary, bool linkedBoundary)
    {
        boundary = Path.TrimEndingDirectorySeparator(Path.GetFullPath(boundary));
        for (var current = Path.GetFullPath(path); current is not null; current = Path.GetDirectoryName(current))
        {
            var root = string.Equals(Path.TrimEndingDirectorySeparator(current), boundary, StringComparison.OrdinalIgnoreCase);
            if (!(root && linkedBoundary) && (File.Exists(current) || Directory.Exists(current))
                && (File.GetAttributes(current) & FileAttributes.ReparsePoint) != 0)
                throw new ModException("linked paths are not supported for managed mods");
            if (root) return;
        }
        throw new ModException("path escaped root");
    }
    internal static bool Within(string root, string path)
    {
        root = Path.TrimEndingDirectorySeparator(Path.GetFullPath(root));
        path = Path.GetFullPath(path);
        return string.Equals(Path.TrimEndingDirectorySeparator(path), root, StringComparison.OrdinalIgnoreCase)
            || path.StartsWith(Path.EndsInDirectorySeparator(root) ? root : root + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase);
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
    private void Atomic(string path, byte[] bytes)
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
