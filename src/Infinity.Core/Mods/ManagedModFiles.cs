// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO.Compression;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Mods;

public sealed record ManagedModFile(string Root, string Path, string? Before, string? After, bool Preserve);
/// <summary>A folder an installation created (relative to its root); removed again when it is empty.</summary>
public sealed record ManagedModDirectory(string Root, string Path);
public sealed record ManagedModReceipt(string Id, string ManifestHash, bool Enabled, IReadOnlyList<ManagedModFile> Files, string? GameRoot = null, string? AzimuthRoot = null,
    string? TransactionId = null, IReadOnlyList<ManagedModDirectory>? CreatedDirectories = null);
public sealed record ModFileChange(string Root, string Path, string? Before, string? After);
public sealed record ModFileJournal(string ReceiptName, ManagedModReceipt Receipt, IReadOnlyList<ModFileChange> Changes);

/// <summary>A file whose current bytes differ from what the receipt expects (another writer, a game update).</summary>
public sealed record ModDrift(string Root, string Path, string? Expected, string? Actual);

/// <summary>
/// Receipt-driven, journaled changes to the Azimuth and game folders for manifest mods. Every
/// mutation holds the bucket lock, requires the game closed, journals before it writes and rolls
/// back on failure. Mod state never blocks ordinary Play by itself: see <see cref="Inspect"/>.
/// </summary>
public sealed partial class ManagedModFiles
{
    private readonly string _azimuth;
    private readonly string? _game;
    private readonly string _state;
    private readonly Func<bool> _gameRunning;
    private readonly string _bucket;
    private readonly string? _packageCache;
    private readonly List<string> _protected;
    private readonly IReadOnlyList<string> _launcherRoots;
    private static readonly JsonSerializerOptions Json = new() { WriteIndented = true };
    public string BackupRoot { get; }
    /// <summary>The Azimuth folder this manager was built for (its full path, as selected).</summary>
    public string AzimuthRoot => _azimuth;

    /// <summary>Tests only: called at each journal boundary; throwing <see cref="SimulatedCrashException"/> leaves the state a crash would.</summary>
    internal Action<string>? FaultPoint { get; set; }

    /// <summary>Wine/Proton DLL load-order overrides; tests substitute a recorder.</summary>
    internal IModWineOverrides WineOverrides { get; set; } = SystemWineOverrides.Instance;

    public ManagedModFiles(string azimuth, string? game, string stateDirectory, Func<bool> gameRunning, string? packageCache = null, IReadOnlyList<string>? launcherRoots = null)
    {
        _azimuth = Path.TrimEndingDirectorySeparator(Path.GetFullPath(azimuth));
        _game = game is null ? null : Path.TrimEndingDirectorySeparator(Path.GetFullPath(game));
        _gameRunning = gameRunning;
        _packageCache = packageCache is null ? null : Path.TrimEndingDirectorySeparator(Path.GetFullPath(packageCache));
        _state = Path.TrimEndingDirectorySeparator(Path.GetFullPath(stateDirectory));
        _bucket = Path.Combine(_state, "game_backup");
        BackupRoot = Path.Combine(_bucket, Identity(_azimuth));
        _launcherRoots = launcherRoots ?? [AppContext.BaseDirectory];
        _protected = ProtectedPaths(_azimuth, _game, _state, _launcherRoots);
        // H1: a linked selected Azimuth folder is the player's (a junctioned library); a linked state or backup folder is refused.
        RejectLinks(BackupRoot);
    }

    /// <summary>The bucket folder name for an Azimuth folder: SHA-256 of its upper-cased full path.</summary>
    public static string Identity(string azimuth) =>
        ModManifest.Hash(Encoding.UTF8.GetBytes(Path.TrimEndingDirectorySeparator(Path.GetFullPath(azimuth)).TrimEnd('\\', '/').ToUpperInvariant()));

    /// <summary>Any interrupted operation in the bucket (this Azimuth folder or another one): mod changes wait for recovery.</summary>
    public bool RecoveryRequired => PendingFolders().Any();
    /// <summary>An interrupted operation of this Azimuth folder: its files may be half changed.</summary>
    public bool RecoveryRequiredHere => File.Exists(Path.Combine(BackupRoot, "pending.json")) || File.Exists(SessionJournal);

    private IEnumerable<string> PendingFolders() => !Directory.Exists(_bucket) ? [] : Directory.EnumerateDirectories(_bucket)
        .Where(folder => File.Exists(Path.Combine(folder, "pending.json")) || File.Exists(Path.Combine(folder, "database-session.json")));

    private ManagedModReceipt? ReadReceipt(string id)
    {
        var path = ReceiptPath(id);
        if (!File.Exists(path)) return null;
        var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)) ?? throw new ModException("invalid receipt", ModErrors.Recovery);
        if (receipt.Id != id || receipt.Files is null || receipt.ManifestHash is null) throw new ModException("invalid receipt", ModErrors.Recovery);
        ValidateGameRoot(receipt);
        foreach (var file in receipt.Files) _ = Target(file.Root, file.Path);
        return receipt;
    }

    /// <summary>The receipt of exactly this manifest; another installed version is an update to review.</summary>
    public ManagedModReceipt? Receipt(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        var receipt = ReadReceipt(manifest.Id);
        if (receipt is not null && receipt.ManifestHash != manifest.Digest)
            throw new ModException("installed manifest differs; update requires review", ModErrors.Unsupported);
        return receipt;
    }

    /// <summary>The receipt for an id regardless of manifest version (null when not installed).</summary>
    public ManagedModReceipt? ReceiptFor(string id) => ReadReceipt(id);

    public bool Verified(ModManifest manifest) => !RecoveryRequiredHere && Receipt(manifest) is not null && Drift(manifest).Count == 0;

    /// <summary>
    /// OD-113: the mapped files a multiplayer fingerprint names (every file but licences, the settings file and seeded files) whose
    /// current bytes are not the manifest's, as <c>root/path</c>. The lobby set is built only when this is empty, so a data mod's
    /// entry stands for the exact files the game will read; an accepted re-baseline (H3) of such a file counts as a mismatch.
    /// </summary>
    public IReadOnlyList<string> ManifestMismatches(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        return manifest.Files.Where(f => f.Role is not ("license" or "config") && f.Mode != "seed")
            .Where(f => !string.Equals(Current(Target(f.Root, f.Destination)), f.Hash, StringComparison.OrdinalIgnoreCase))
            .Select(f => f.Root + "/" + f.Destination).ToList();
    }

    /// <summary>Managed files that differ from the receipt's expected state.</summary>
    public IReadOnlyList<ModDrift> Drift(ModManifest manifest)
    {
        var receipt = Receipt(manifest);
        return receipt is null ? [] : DriftOf(receipt);
    }

    private List<ModDrift> DriftOf(ManagedModReceipt receipt) => receipt.Files.Where(f => !f.Preserve)
        .Select(f => (File: f, Actual: Current(Target(f.Root, f.Path)), Expected: receipt.Enabled ? f.After : f.Before))
        .Where(x => x.Actual != x.Expected).Select(x => new ModDrift(x.File.Root, x.File.Path, x.Expected, x.Actual)).ToList();

    /// <summary>OD-112 follow-up: on a Hardcore profile no mod that affects multiplayer may become enabled.</summary>
    private static void CheckProfile(ModManifest manifest, bool hardcore)
    {
        if (hardcore && ModMultiplayer.AffectsMultiplayer(manifest))
            throw new ModException("Hardcore profiles cannot enable mods that affect multiplayer", ModErrors.Hardcore);
    }

    public Task InstallAsync(HttpClient http, ModManifest manifest, IReadOnlyCollection<string> downloadHosts, CancellationToken cancellation,
        IProgress<string>? progress = null, bool update = false, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(downloadHosts);
        return InstallAsync(http, manifest, host => downloadHosts.Contains(host, StringComparer.OrdinalIgnoreCase), cancellation, progress, update, hardcore);
    }

    /// <summary>The same with a host rule: an OD-116 catalog allows any HTTPS host, every package pinned by its SHA-256 and size.</summary>
    public async Task InstallAsync(HttpClient http, ModManifest manifest, Func<string, bool> downloadHosts, CancellationToken cancellation,
        IProgress<string>? progress = null, bool update = false, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(manifest); ArgumentNullException.ThrowIfNull(downloadHosts);
        if (!update) CheckProfile(manifest, hardcore);
        if (!manifest.SupportedForInstall) throw new ModException("this manifest uses unsupported installation capabilities", ModErrors.Unsupported);
        if (manifest.IsCustom) throw new ModException("custom mods install from their local folder", ModErrors.Unsupported);
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
                if (new FileInfo(cached).Length != size) throw new ModException("cached package size mismatch", ModErrors.Digest);
                bytes = await File.ReadAllBytesAsync(cached, cancellation).ConfigureAwait(false);
            }
            else
            {
                using var response = await ModDownloads.DownloadAsync(http, new Uri(spec.GetProperty("url").GetString()!), downloadHosts, cancellation).ConfigureAwait(false);
                response.EnsureSuccessStatusCode();
                await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
                bytes = await DevelopmentModCatalog.ReadBoundedAsync(body, Math.Min(size, 64 * 1024 * 1024), cancellation).ConfigureAwait(false);
            }
            if (bytes.LongLength != size || ModManifest.Hash(bytes) != hash) throw new ModException("package digest mismatch", ModErrors.Digest);
            packages.Add(name, bytes);
        }
        progress?.Report("verify");
        await Task.Run(() => InstallVerifiedPackages(manifest, packages, progress, update, hardcore), cancellation).ConfigureAwait(false);
    }

    public void InstallVerifiedPackage(ModManifest manifest, byte[] package, IProgress<string>? progress = null, bool update = false, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(manifest); ArgumentNullException.ThrowIfNull(package);
        InstallVerifiedPackages(manifest, new Dictionary<string, byte[]> { ["primary"] = package }, progress, update, hardcore);
    }

    private static Dictionary<ManifestFile, Func<byte[]>> VerifyPackages(ModManifest manifest, IReadOnlyDictionary<string, byte[]> packages)
    {
        if (manifest.IsCustom) throw new ModException("custom mods install from their local folder", ModErrors.Unsupported);
        var verified = new Dictionary<ManifestFile, Func<byte[]>>();
        foreach (var (name, spec) in PackageSpecs(manifest))
        {
            if (!packages.TryGetValue(name, out var package) || package.LongLength != spec.GetProperty("bytes").GetInt64()
                || ModManifest.Hash(package) != spec.GetProperty("sha256").GetString()) throw new ModException("package digest mismatch", ModErrors.Digest);
            using var zip = new ZipArchive(new MemoryStream(package), ZipArchiveMode.Read);
            if (zip.Entries.Count > 4096 || zip.Entries.Select(e => e.FullName).Distinct(StringComparer.OrdinalIgnoreCase).Count() != zip.Entries.Count)
                throw new ModException("duplicate or excessive archive entries", ModErrors.Digest);
            foreach (var file in manifest.Files.Where(f => f.Package == name))
            {
                var entry = zip.GetEntry(file.Source) ?? throw new ModException("archive entry missing", ModErrors.Digest);
                if (entry.Length != file.Bytes) throw new ModException("archive entry length mismatch", ModErrors.Digest);
                using var stream = entry.Open();
                var content = new byte[checked((int)file.Bytes)]; stream.ReadExactly(content);
                if (stream.ReadByte() != -1 || ModManifest.Hash(content) != file.Hash) throw new ModException("file digest mismatch", ModErrors.Digest);
                verified.Add(file, () => content);
            }
        }
        if (verified.Count != manifest.Files.Count) throw new ModException("unresolved package mapping", ModErrors.Digest);
        return verified;
    }

    internal void InstallVerifiedPackages(ModManifest manifest, IReadOnlyDictionary<string, byte[]> packages, IProgress<string>? progress = null, bool update = false, bool hardcore = false) =>
        InstallFrom(manifest, () => VerifyPackages(manifest, packages), progress, update, hardcore);

    /// <summary>
    /// The files of a custom mod (OD-114) from its own folder, <c>&lt;home&gt;/mods/custom/&lt;id&gt;/files/&lt;root&gt;/&lt;destination&gt;</c>,
    /// installed like a package once the player confirmed this exact manifest digest. Every file's size and SHA-256 is checked
    /// before anything is written and again as it is read, one file at a time.
    /// </summary>
    public void InstallCustom(ModManifest manifest, IProgress<string>? progress = null, bool update = false, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (!manifest.IsCustom) throw new ModException("not a custom mod", ModErrors.Unsupported);
        RequireCustomConfirmation(manifest);
        InstallFrom(manifest, () => VerifyCustomFiles(manifest), progress, update, hardcore);
    }

    private void InstallFrom(ModManifest manifest, Func<Dictionary<ManifestFile, Func<byte[]>>> verify, IProgress<string>? progress, bool update, bool hardcore)
    {
        if (!manifest.SupportedForInstall)
            throw new ModException("runtime or dependencies require coordination", ModErrors.Unsupported);
        using var gate = Lock();
        var installed = ReadReceipt(manifest.Id);
        if (installed is not null && !update) throw new ModException("already managed; use enable or disable", installed.ManifestHash == manifest.Digest ? ModErrors.Failed : ModErrors.Unsupported);
        if (installed is not null && installed.ManifestHash == manifest.Digest) throw new ModException("this version is already installed");
        if (installed is not null)
        {
            if (installed.Enabled) CheckProfile(manifest, hardcore); // an enabled mod stays enabled across its update
            Update(manifest, installed, verify(), progress); return;
        }
        CheckProfile(manifest, hardcore);
        CheckDependencies(manifest);
        var verified = verify();
        var owned = new List<ManagedModFile>();
        var changes = new List<ModFileChange>();
        foreach (var (file, source) in verified)
        {
            var target = Target(file.Root, file.Destination);
            var before = Current(target);
            if (file.Mode == "seed" && before is not null && file.Role != "config") continue;
            if (file.Mode == "add" && before is not null || file.Mode == "replace" && before is null) throw new ModException("file precondition failed", ModErrors.Conflict);
            if (before == file.Hash && file.Role == "binary") throw new ModException("existing installation needs explicit adoption", ModErrors.Conflict);
            if (before is not null) Store(File.ReadAllBytes(target));
            var bytes = source();
            var afterBytes = bytes;
            if (file.Role == "config" && manifest.ConfigPath == file.Destination)
            {
                var ini = new ManifestIni(manifest, before is null ? bytes : File.ReadAllBytes(target));
                ini.EnforceLocks(); afterBytes = ini.ToBytes();
            }
            var after = Store(afterBytes);
            var preserve = file.Role == "config" || file.Mode == "seed"; // seeded player files are the player's once written
            owned.Add(new(file.Root, file.Destination, before, after, preserve));
            changes.Add(new(file.Root, file.Destination, before, after));
        }
        AddConfig(manifest, owned, changes);
        RejectOverlaps(owned, manifest.Id);
        SaveSnapshot(manifest);
        Execute(new(manifest.Id + ".receipt.json", new(manifest.Id, manifest.Digest, true, owned,
            owned.Any(f => f.Root == "game") ? _game : null, _azimuth, null, CreatedDirectories(owned)), changes), progress);
        ReconcileWineOverrides();
    }

    private void AddConfig(ModManifest manifest, List<ManagedModFile> owned, List<ModFileChange> changes)
    {
        if (manifest.ConfigPath is null || owned.Any(f => f.Root == "azimuth" && f.Path == manifest.ConfigPath)) return;
        var config = Target("azimuth", manifest.ConfigPath!);
        var before = Current(config);
        var iniBytes = before is null ? ManifestIni.Defaults(manifest) : File.ReadAllBytes(config);
        var ini = new ManifestIni(manifest, iniBytes);
        ini.EnforceLocks(); iniBytes = ini.ToBytes();
        if (before is not null) Store(File.ReadAllBytes(config));
        if (before != ModManifest.Hash(iniBytes)) changes.Add(new("azimuth", manifest.ConfigPath!, before, Store(iniBytes)));
        owned.Add(new("azimuth", manifest.ConfigPath!, before, Store(iniBytes), true));
    }

    /// <summary>
    /// M5 update to a reviewed manifest with another digest: originals recorded by the first install
    /// stay the originals, removed mappings are restored, new ones are backed up first, and the
    /// player's settings and seeded files are kept (new locks applied). A disabled mod stays disabled.
    /// </summary>
    private void Update(ModManifest manifest, ManagedModReceipt installed, Dictionary<ManifestFile, Func<byte[]>> verified, IProgress<string>? progress)
    {
        if (installed.Enabled) { CheckDependencies(manifest); CheckDependents(manifest.Id, manifest); }
        if (DriftOf(installed) is { Count: > 0 }) throw new ModException("files changed outside the launcher", ModErrors.Drift);
        var old = installed.Files.ToDictionary(f => f.Root + "/" + f.Path, StringComparer.OrdinalIgnoreCase);
        var owned = new List<ManagedModFile>(); var changes = new List<ModFileChange>();
        foreach (var (file, source) in verified)
        {
            var key = file.Root + "/" + file.Destination;
            var target = Target(file.Root, file.Destination);
            var current = Current(target);
            if (old.Remove(key, out var previous) && (previous.Preserve || file.Mode == "seed"))
            {
                owned.Add(previous with { Preserve = true }); // settings and seeded player files stay as they are
                continue;
            }
            var after = Store(source());
            if (previous is not null)
            {
                owned.Add(previous with { After = after, Preserve = false });
                if (installed.Enabled && current != after) changes.Add(new(file.Root, file.Destination, current, after));
                continue;
            }
            if (file.Mode == "seed" && current is not null && file.Role != "config") continue;
            if (file.Mode == "add" && current is not null && installed.Enabled || file.Mode == "replace" && current is null) throw new ModException("file precondition failed", ModErrors.Conflict);
            if (current is not null) Store(File.ReadAllBytes(target));
            owned.Add(new(file.Root, file.Destination, current, after, file.Role == "config" || file.Mode == "seed"));
            if (installed.Enabled && current != after) changes.Add(new(file.Root, file.Destination, current, after));
        }
        foreach (var removed in old.Values)
        {
            // Mappings the new version no longer has: restored (an enabled mod) and dropped; kept player files stay recorded.
            if (removed.Preserve) { owned.Add(removed); continue; }
            if (installed.Enabled && removed.Before != removed.After) changes.Add(new(removed.Root, removed.Path, removed.After, removed.Before));
        }
        AddConfig(manifest, owned, changes);
        if (installed.Enabled && manifest.ConfigPath is { } config && Current(Target("azimuth", config)) is { } currentIni
            && !changes.Any(c => c.Root == "azimuth" && c.Path == config))
        {
            // The player's settings are kept; the new manifest's locks still apply.
            var ini = new ManifestIni(manifest, File.ReadAllBytes(Target("azimuth", config))); ini.EnforceLocks();
            var locked = Store(ini.ToBytes());
            if (locked != currentIni) { Store(File.ReadAllBytes(Target("azimuth", config))); changes.Add(new("azimuth", config, currentIni, locked)); }
        }
        RejectOverlaps(owned, manifest.Id);
        SaveSnapshot(manifest);
        var created = (installed.CreatedDirectories ?? []).Concat(CreatedDirectories(owned)).Distinct().ToList();
        Execute(new(manifest.Id + ".receipt.json", installed with { ManifestHash = manifest.Digest, Files = owned,
            GameRoot = owned.Any(f => f.Root == "game") ? _game : null, CreatedDirectories = created }, changes), progress);
        RemoveCreatedDirectories(created);
        ReconcileWineOverrides();
    }

    /// <summary>
    /// Enables or disables from the receipt. A disabled mod whose originals changed meanwhile (a game
    /// update, H3) takes the current files as the new originals, backed up first. Disabling refuses
    /// drift: the player chooses <see cref="AcceptCurrentFiles"/> or <see cref="DisableKeepingCurrentFiles"/>.
    /// </summary>
    public void SetEnabled(ModManifest manifest, bool enabled, IProgress<string>? progress = null, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (enabled) CheckProfile(manifest, hardcore); // disabling is always allowed
        if (enabled && manifest.IsCustom) RequireCustomConfirmation(manifest);
        // OD-117: an external mod's database base obeys the same rules on enable as on install (runtime, one base at a time).
        if (enabled && manifest.IsExternal && manifest.DatabaseBase is not null)
        {
            if (!DataSqlRuntimeAvailable()) throw new ModException("a database base needs the mod runtime", ModErrors.RuntimeRequired);
            CheckDatabaseBase(manifest);
        }
        using var gate = Lock();
        var receipt = Receipt(manifest) ?? throw new ModException("mod has no installation receipt");
        if (receipt.Enabled == enabled) return;
        if (enabled) CheckDependencies(manifest); else CheckDependents(manifest.Id);
        var drift = DriftOf(receipt);
        if (!enabled && drift.Count > 0) throw new ModException("files changed outside the launcher", ModErrors.Drift);
        if (enabled && drift.Count > 0) receipt = Rebased(receipt, drift);
        var changes = receipt.Files.Where(f => !f.Preserve).Select(f => new ModFileChange(f.Root, f.Path,
            enabled ? f.Before : f.After, enabled ? f.After : f.Before)).Where(c => c.Before != c.After).ToList();
        if (enabled && manifest.ConfigPath is { } config && Current(Target("azimuth", config)) is { } currentIni)
        {
            var ini = new ManifestIni(manifest, File.ReadAllBytes(Target("azimuth", config))); ini.EnforceLocks();
            var locked = Store(ini.ToBytes());
            if (locked != currentIni) { Store(File.ReadAllBytes(Target("azimuth", config))); changes.Add(new("azimuth", config, currentIni, locked)); }
        }
        Execute(new(manifest.Id + ".receipt.json", receipt with { Enabled = enabled }, changes), progress);
        if (!enabled) RemoveCreatedDirectories(receipt.CreatedDirectories ?? []);
        ReconcileWineOverrides();
    }

    /// <summary>H3 "accept current files": the files now on disk become what the receipt expects. No game file is written.</summary>
    public void AcceptCurrentFiles(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        using var gate = Lock();
        var receipt = Receipt(manifest) ?? throw new ModException("mod has no installation receipt");
        var drift = DriftOf(receipt);
        if (drift.Count == 0) return;
        Execute(new(manifest.Id + ".receipt.json", Rebased(receipt, drift), []), null);
    }

    /// <summary>H3 "disable keeping current files": unchanged mod files are restored; changed ones stay and become the originals.</summary>
    public void DisableKeepingCurrentFiles(ModManifest manifest, IProgress<string>? progress = null)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        using var gate = Lock();
        var receipt = Receipt(manifest) ?? throw new ModException("mod has no installation receipt");
        if (!receipt.Enabled) return;
        CheckDependents(manifest.Id);
        var drifted = DriftOf(receipt).Select(d => d.Root + "/" + d.Path).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var files = new List<ManagedModFile>(); var changes = new List<ModFileChange>();
        foreach (var file in receipt.Files)
        {
            if (file.Preserve || !drifted.Contains(file.Root + "/" + file.Path))
            {
                files.Add(file);
                if (!file.Preserve && file.Before != file.After) changes.Add(new(file.Root, file.Path, file.After, file.Before));
                continue;
            }
            var path = Target(file.Root, file.Path);
            var current = File.Exists(path) ? Store(File.ReadAllBytes(path)) : null;
            files.Add(file with { Before = current });
        }
        Execute(new(manifest.Id + ".receipt.json", receipt with { Enabled = false, Files = files }, changes), progress);
        RemoveCreatedDirectories(receipt.CreatedDirectories ?? []);
        ReconcileWineOverrides();
    }

    /// <summary>
    /// Disables (when enabled) and forgets the installation: the receipt is archived, created empty
    /// folders are removed, the settings file and backups stay (pruned later). Refuses drift.
    /// </summary>
    public void Uninstall(string id, IProgress<string>? progress = null)
    {
        using var gate = Lock();
        var receipt = ReadReceipt(id) ?? throw new ModException("mod has no installation receipt");
        CheckDependents(id);
        if (receipt.Enabled)
        {
            if (DriftOf(receipt).Count > 0) throw new ModException("files changed outside the launcher", ModErrors.Drift);
            var changes = receipt.Files.Where(f => !f.Preserve && f.Before != f.After).Select(f => new ModFileChange(f.Root, f.Path, f.After, f.Before)).ToList();
            Execute(new(id + ".receipt.json", receipt with { Enabled = false }, changes), progress);
        }
        var archive = Safe(BackupRoot, "uninstalled-" + id + "-" + Guid.NewGuid().ToString("N") + ".json");
        File.Move(ReceiptPath(id), archive);
        RemoveCreatedDirectories(receipt.CreatedDirectories ?? []);
        ReconcileWineOverrides();
    }

    private ManagedModReceipt Rebased(ManagedModReceipt receipt, IReadOnlyList<ModDrift> drift)
    {
        var changed = drift.ToDictionary(d => d.Root + "/" + d.Path, StringComparer.OrdinalIgnoreCase);
        var files = receipt.Files.Select(file =>
        {
            if (!changed.ContainsKey(file.Root + "/" + file.Path)) return file;
            var path = Target(file.Root, file.Path);
            var current = File.Exists(path) ? Store(File.ReadAllBytes(path)) : null;
            if (receipt.Enabled)
            {
                return file with { After = current };
            }
            return file with { Before = current };
        }).ToList();
        return receipt with { Files = files };
    }

    private void Execute(ModFileJournal journal, IProgress<string>? progress)
    {
        journal = journal with { Receipt = journal.Receipt with { TransactionId = Guid.NewGuid().ToString("N") } };
        CheckGame();
        foreach (var change in journal.Changes)
        {
            if (Current(Target(change.Root, change.Path)) != change.Before) throw new ModException("files changed outside the launcher", ModErrors.Drift);
            if (change.Before is not null) _ = ObjectBytes(change.Before);
            if (change.After is not null) _ = ObjectBytes(change.After);
        }
        var pending = Path.Combine(BackupRoot, "pending.json");
        Atomic(pending, JsonSerializer.SerializeToUtf8Bytes(journal, Json));
        FaultPoint?.Invoke("journal");
        progress?.Report("apply");
        try
        {
            var index = 0;
            foreach (var change in journal.Changes)
            {
                CheckGame();
                if (Current(Target(change.Root, change.Path)) != change.Before) throw new ModException("files changed outside the launcher", ModErrors.Drift);
                Put(Target(change.Root, change.Path), change.After);
                FaultPoint?.Invoke("applied:" + index++);
            }
            Atomic(Safe(BackupRoot, journal.ReceiptName), JsonSerializer.SerializeToUtf8Bytes(journal.Receipt, Json));
            FaultPoint?.Invoke("receipt");
        }
        catch (Exception error) when (error is not SimulatedCrashException)
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
            if (actual != change.Before && actual != change.After && !DeletionInProgress(change, actual))
                throw new ModException("recovery conflicts with external changes", ModErrors.Recovery);
            if (change.Before is not null) _ = ObjectBytes(change.Before);
        }
        var index = 0;
        foreach (var change in journal.Changes.Reverse())
        {
            CheckGame();
            var target = Target(change.Root, change.Path);
            if (Current(target) != change.Before) Put(target, change.Before);
            FaultPoint?.Invoke("rollback:" + index++);
        }
    }

    /// <summary>The empty file <see cref="RemoveFile"/> leaves when it is interrupted between its replace and its delete.</summary>
    private static readonly string EmptyHash = ModManifest.Hash([]);
    private static bool DeletionInProgress(ModFileChange change, string? actual) => actual == EmptyHash && (change.Before is null || change.After is null);

    private void ArchiveJournal(string pending)
    {
        File.Move(pending, Path.Combine(BackupRoot, "transaction-" + Guid.NewGuid().ToString("N") + ".json"));
        FaultPoint?.Invoke("archived");
    }

    private void ValidateGameRoot(ManagedModReceipt receipt)
    {
        if (!string.Equals(_azimuth, receipt.AzimuthRoot, StringComparison.OrdinalIgnoreCase))
            throw new ModException("azimuth folder differs from the installation receipt", ModErrors.Relink);
        if (receipt.Files.Any(f => f.Root == "game") && !string.Equals(_game, receipt.GameRoot, StringComparison.OrdinalIgnoreCase))
            throw new ModException("game folder differs from the installation receipt", ModErrors.Relink);
    }

    /// <summary>
    /// Refuses a path another installation owns. An Azimuth folder that no longer exists (moved, M9)
    /// owns no Azimuth paths; its game paths still count until it is relinked.
    /// </summary>
    private void RejectOverlaps(IReadOnlyList<ManagedModFile> proposed, string ownId)
    {
        // Canonical forms too: one Azimuth folder selected through a junction and through its real path is one folder.
        var targets = proposed.Select(f => Target(f.Root, f.Path)).SelectMany(t => new[] { t, Canonical(t) }).ToHashSet(StringComparer.OrdinalIgnoreCase);
        if (!Directory.Exists(_bucket)) return;
        foreach (var folder in Directory.EnumerateDirectories(_bucket))
        {
            RejectLinks(folder);
            foreach (var path in Directory.EnumerateFiles(folder, "*.receipt.json"))
            {
                RejectLinks(path);
                var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)) ?? throw new ModException("invalid receipt", ModErrors.Recovery);
                if (receipt.Id == ownId && string.Equals(folder, BackupRoot, StringComparison.OrdinalIgnoreCase)) continue;
                foreach (var existing in receipt.Files)
                {
                    var root = existing.Root switch
                    {
                        "azimuth" => receipt.AzimuthRoot ?? throw new ModException("missing receipt root"),
                        "game" => receipt.GameRoot ?? throw new ModException("missing receipt root"),
                        _ => throw new ModException("invalid receipt root"),
                    };
                    if (existing.Root == "azimuth" && !Directory.Exists(root)) continue;
                    var owned = Resolve(root, existing.Path);
                    if (targets.Contains(owned) || targets.Contains(Canonical(owned))) throw new ModException("another mod owns this path", ModErrors.Conflict);
                }
            }
        }
    }

    private List<ManagedModDirectory> CreatedDirectories(IEnumerable<ManagedModFile> files)
    {
        var created = new List<ManagedModDirectory>();
        foreach (var file in files)
        {
            var parts = file.Path.Split('/');
            for (var depth = parts.Length - 1; depth >= 1; depth--)
            {
                var relative = string.Join('/', parts.Take(depth));
                if (Directory.Exists(Resolve(RootPath(file.Root), relative))) break;
                var directory = new ManagedModDirectory(file.Root, relative);
                if (!created.Contains(directory)) created.Add(directory);
            }
        }
        return created;
    }

    /// <summary>Removes folders an installation created, deepest first, only while empty and unlinked.</summary>
    private void RemoveCreatedDirectories(IEnumerable<ManagedModDirectory> directories)
    {
        foreach (var directory in directories.OrderByDescending(d => d.Path.Count(c => c == '/')).ThenByDescending(d => d.Path.Length))
        {
            try
            {
                var path = Safe(RootPath(directory.Root), directory.Path);
                RejectProtected(path);
                if (Directory.Exists(path) && !Directory.EnumerateFileSystemEntries(path).Any()) Directory.Delete(path, recursive: false);
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or ModException) { }
        }
    }

    private void SaveSnapshot(ModManifest manifest)
    {
        Atomic(Safe(BackupRoot, "manifests/" + manifest.Digest + ".json"), manifest.Snapshot());
        Atomic(Path.Combine(BackupRoot, manifest.Id + ".manifest.json"), manifest.Snapshot());
    }

    /// <summary>The exact manifest a receipt was written for: its content-addressed snapshot, or the legacy per-id copy when it matches.</summary>
    private ModManifest SnapshotFor(ManagedModReceipt receipt)
    {
        foreach (var name in new[] { "manifests/" + receipt.ManifestHash + ".json", receipt.Id + ".manifest.json" })
        {
            var path = Safe(BackupRoot, name);
            if (File.Exists(path) && ModManifest.Hash(File.ReadAllBytes(path)) == receipt.ManifestHash) return new ModManifest(File.ReadAllBytes(path));
        }
        throw new ModException("installed manifest snapshot missing", ModErrors.Recovery);
    }

    private string RootPath(string root) => root switch
    {
        "azimuth" => _azimuth,
        "game" => _game ?? throw new ModException("game folder not selected", ModErrors.GameFolder),
        _ => throw new ModException("invalid root"),
    };

    private string ReceiptPath(string id) => Safe(BackupRoot, id + ".receipt.json");

    private FileStream Lock(bool allowRecovery = false)
    {
        CheckGame(); RejectLinks(BackupRoot);
        Directory.CreateDirectory(BackupRoot);
        FileStream gate;
        try { gate = new FileStream(Path.Combine(_bucket, "operation.lock"), FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None); }
        catch (IOException error) { throw new ModException("another launcher is changing mods: " + error.GetType().Name, ModErrors.Busy); }
        try
        {
            foreach (var folder in Directory.EnumerateDirectories(_bucket))
            {
                RejectLinks(folder);
                if (!allowRecovery && (File.Exists(Path.Combine(folder, "pending.json")) || File.Exists(Path.Combine(folder, "database-session.json"))))
                    throw new ModException("interrupted mod operation needs recovery", ModErrors.Recovery);
            }
        }
        catch { gate.Dispose(); throw; }
        return gate;
    }

    private void CheckGame() { if (_gameRunning()) throw new ModException("close the game first", ModErrors.GameRunning); }
}

/// <summary>Tests only: thrown from a fault point to leave the files and journal as a process crash would.</summary>
internal sealed class SimulatedCrashException() : Exception("simulated crash");
