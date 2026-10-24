// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>A custom mod folder that could not be listed: its folder name and a stable reason (logged, never shown raw).</summary>
public sealed record CustomModProblem(string Folder, string Reason);

/// <summary>The custom mods found in the player's custom folder (OD-114).</summary>
public sealed record CustomModScan(IReadOnlyList<ModManifest> Mods, IReadOnlyList<CustomModProblem> Problems)
{
    public static CustomModScan Empty { get; } = new([], []);
}

/// <summary>
/// Custom (player-supplied, unreviewed) mods, OD-114. Each lives in its own folder under the launcher's stable data home,
/// <c>&lt;home&gt;/mods/custom/&lt;id&gt;/</c>, with <c>infinity-mod.json</c> (the ordinary manifest schema, an id with the
/// reserved <c>custom.</c> prefix and <c>payload: {format: "folder", path: "files"}</c>) and the payload as a plain
/// <c>files/&lt;root&gt;/&lt;destination&gt;</c> tree. They need no catalog signature; instead the player confirms each exact
/// manifest digest before its first install or enable, and the confirmation is recorded outside the mod's folder so a
/// folder cannot arrive pre-confirmed.
/// </summary>
public sealed partial class ManagedModFiles
{
    public const string CustomManifestName = "infinity-mod.json";
    public const int MaxCustomMods = 64;

    /// <summary>
    /// Whether this launcher can apply a data mod's SQL (FIMOD004): a verified runtime package and a hook that support it. A SQL data
    /// mod is installed and enabled only then; until the runtime ships it, the mod is listed but refused (<see cref="ModErrors.RuntimeRequired"/>).
    /// </summary>
    public Func<bool> DataSqlRuntimeAvailable { get; init; } = () => false;

    /// <summary>The folder custom mods are read from: <c>&lt;home&gt;/mods/custom</c>.</summary>
    public static string CustomModsFolder(string home) => Path.Combine(Path.GetFullPath(home), "mods", "custom");

    private string CustomConfirmationsPath => Path.Combine(_state, "mods", "custom-confirmations.json");

    /// <summary>
    /// Lists the custom folder: every subfolder named exactly like its manifest's custom id with a valid manifest. Anything else
    /// (a linked folder, a missing or invalid manifest, an official id, a name mismatch) is a problem, never a listed mod.
    /// Reads only; a missing folder is an empty list.
    /// </summary>
    public static CustomModScan ScanCustomMods(string home)
    {
        var root = CustomModsFolder(home);
        if (!Directory.Exists(root)) return CustomModScan.Empty;
        var mods = new List<ModManifest>(); var problems = new List<CustomModProblem>();
        foreach (var folder in Directory.EnumerateDirectories(root).Order(StringComparer.Ordinal))
        {
            var name = Path.GetFileName(folder);
            if (name.StartsWith('.')) continue;
            if (mods.Count >= MaxCustomMods) { problems.Add(new(name, "too_many")); continue; }
            try
            {
                var manifestPath = Path.Combine(folder, CustomManifestName);
                if (IsLink(folder) || IsLink(manifestPath)) throw new ModException("linked custom mod folder", ModErrors.Linked);
                if (!File.Exists(manifestPath)) throw new ModException("custom mod manifest missing", ModErrors.Unsupported);
                if (new FileInfo(manifestPath).Length > ModCatalogTrust.MaxManifestBytes) throw new ModException("custom mod manifest too large", ModErrors.Unsupported);
                var manifest = new ModManifest(File.ReadAllBytes(manifestPath));
                if (!manifest.IsCustom) throw new ModException("a custom folder may hold only custom ids", ModErrors.Unsupported);
                if (!string.Equals(manifest.Id, name, StringComparison.Ordinal)) throw new ModException("custom folder name differs from its id", ModErrors.Unsupported);
                mods.Add(manifest);
            }
            catch (Exception error) when (IsStateError(error) || error is KeyNotFoundException or InvalidOperationException or FormatException)
            {
                problems.Add(new(name, Code(error)));
            }
        }
        return new(mods.AsReadOnly(), problems.AsReadOnly());
    }

    /// <summary>Whether the player confirmed this exact custom manifest (id and digest).</summary>
    public bool CustomConfirmed(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        return manifest.IsCustom && ReadConfirmations().Any(c => c.Id == manifest.Id && c.Digest == manifest.Digest);
    }

    /// <summary>Records the player's explicit confirmation of this exact custom manifest digest; another digest needs its own.</summary>
    public void ConfirmCustom(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (!manifest.IsCustom) throw new ModException("not a custom mod", ModErrors.Unsupported);
        var confirmed = ReadConfirmations();
        if (confirmed.Any(c => c.Id == manifest.Id && c.Digest == manifest.Digest)) return;
        confirmed.Add(new(manifest.Id, manifest.Digest));
        var path = CustomConfirmationsPath;
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        RejectLinks(path);
        // Bounded: the newest 1024 confirmations stay.
        Atomic(path, JsonSerializer.SerializeToUtf8Bytes(new CustomConfirmations(1, confirmed.TakeLast(1024).ToList()), Json));
    }

    private void RequireCustomConfirmation(ModManifest manifest)
    {
        // Executable code from a custom mod runs only through the development native runtime until custom signing is designed.
        if (manifest.IsNative && BuildProfile.ProductionOnly) throw new ModException("custom native mods are development only", ModErrors.Unsupported);
        if ((manifest.HasDatabaseSql || manifest.DatabaseBase is not null) && !DataSqlRuntimeAvailable())
            throw new ModException("data mod database changes need the mod runtime", ModErrors.RuntimeRequired);
        CheckDatabaseBase(manifest);
        if (manifest.CustomBlocked) throw new ModException("custom mod lists unresolved blockers", ModErrors.Unsupported);
        if (!CustomConfirmed(manifest)) throw new ModException("custom mod not confirmed by the player", ModErrors.Unconfirmed);
    }

    private sealed record CustomConfirmation(string Id, string Digest);
    private sealed record CustomConfirmations(int SchemaVersion, List<CustomConfirmation> Confirmed);

    private List<CustomConfirmation> ReadConfirmations()
    {
        var path = CustomConfirmationsPath;
        if (!File.Exists(path)) return [];
        RejectLinks(path);
        var document = JsonSerializer.Deserialize<CustomConfirmations>(File.ReadAllBytes(path));
        if (document is not { SchemaVersion: 1, Confirmed: { } list } || list.Any(c => c?.Id is null || c.Digest is not { Length: 64 }))
            throw new ModException("custom confirmations unreadable", ModErrors.Recovery);
        return list;
    }

    /// <summary>
    /// The custom mod's files, checked before any write: its folder still holds this exact manifest, and every mapped file under
    /// <c>files/</c> has the manifest's size and SHA-256. Each is re-read and re-checked when it is installed, one at a time.
    /// </summary>
    private Dictionary<ManifestFile, Func<byte[]>> VerifyCustomFiles(ModManifest manifest)
    {
        var folder = Safe(_state, "mods/custom/" + manifest.Id);
        var manifestPath = Safe(folder, CustomManifestName);
        if (!File.Exists(manifestPath) || new FileInfo(manifestPath).Length > ModCatalogTrust.MaxManifestBytes
            || ModManifest.Hash(File.ReadAllBytes(manifestPath)) != manifest.Digest)
            throw new ModException("the custom mod folder holds another manifest", ModErrors.Digest);
        var payload = Path.Combine(folder, ModManifest.CustomPayloadFolder);
        var verified = new Dictionary<ManifestFile, Func<byte[]>>();
        foreach (var file in manifest.Files)
        {
            var path = Safe(payload, file.Source);
            if (!File.Exists(path)) throw new ModException("custom mod file missing", ModErrors.Digest);
            using (var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read))
                if (stream.Length != file.Bytes || Convert.ToHexStringLower(SHA256.HashData(stream)) != file.Hash)
                    throw new ModException("custom mod file digest mismatch", ModErrors.Digest);
            verified.Add(file, () =>
            {
                RejectLinks(path);
                var bytes = File.ReadAllBytes(path);
                if (bytes.LongLength != file.Bytes || ModManifest.Hash(bytes) != file.Hash) throw new ModException("custom mod file changed while installing", ModErrors.Digest);
                return bytes;
            });
        }
        if (manifest.DatabaseBase is { } databaseBase)
        {
            var path = Safe(folder, databaseBase.Path);
            if (!File.Exists(path)) throw new ModException("database base missing", ModErrors.Digest);
            foreach (var sidecar in new[] { "-wal", "-shm", "-journal" })
                if (File.Exists(path + sidecar)) throw new ModException("database base has SQLite sidecars", ModErrors.Unsupported);
            if (new FileInfo(path).Length != databaseBase.Bytes) throw new ModException("database base resized", ModErrors.Digest);
            var bytes = File.ReadAllBytes(path);
            if (bytes.LongLength != databaseBase.Bytes || ModManifest.Hash(bytes) != databaseBase.Sha256) throw new ModException("database base digest mismatch", ModErrors.Digest);
            if (!StandaloneSqlite(bytes)) throw new ModException("database base must be a checkpointed rollback-journal SQLite file", ModErrors.Unsupported);
            if (Store(bytes) != databaseBase.Sha256) throw new ModException("database base changed while installing", ModErrors.Digest);
        }
        if (manifest.DatabaseSql is { } sql)
        {
            // Kept as a backup object (named by the manifest snapshot), so a launch never depends on the folder staying in place.
            var bytes = CustomSqlBytes(Safe(folder, sql.Path), sql);
            if (Store(bytes) != sql.Sha256) throw new ModException("data mod SQL changed while installing", ModErrors.Digest);
        }
        return verified;
    }

    /// <summary>OD-114 follow-up: one database base at a time; a second is refused naming the enabled one (<see cref="ModException.Detail"/>).</summary>
    public void CheckDatabaseBase(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (manifest.DatabaseBase is null) return;
        foreach (var other in InstalledManifests())
            if (other.Id != manifest.Id && other.DatabaseBase is not null && Receipt(other) is { Enabled: true })
                throw new ModException("another enabled mod replaces the game database", ModErrors.DatabaseBase) { Detail = other.Id };
    }

    /// <summary>The installed car pack's database from the backup object store, checked again (session base at launch).</summary>
    private byte[] InstalledBase(ModManifest manifest)
    {
        var base_ = manifest.DatabaseBase ?? throw new ModException("no database base");
        var bytes = ObjectBytes(base_.Sha256);
        if (bytes.LongLength != base_.Bytes || !StandaloneSqlite(bytes)) throw new ModException("database base damaged", ModErrors.Digest);
        return bytes;
    }

    /// <summary>SQLite 3 with the rollback journal (bytes 18/19 = 1): what the runtime's helper accepts as a base. Integrity, value lengths and
    /// foreign keys are checked by make_custom_mod.py's pre-flight and by the helper (the launcher carries no SQLite engine).</summary>
    private static bool StandaloneSqlite(byte[] bytes) => bytes.Length >= 100 && bytes.AsSpan(0, 16).SequenceEqual("SQLite format 3\0"u8) && bytes[18] == 1 && bytes[19] == 1;

    private static byte[] CustomSqlBytes(string path, DataModSqlFile sql)
    {
        if (!File.Exists(path) || new FileInfo(path).Length != sql.Bytes) throw new ModException("data mod SQL missing or resized", ModErrors.Digest);
        var bytes = File.ReadAllBytes(path);
        if (bytes.LongLength != sql.Bytes || ModManifest.Hash(bytes) != sql.Sha256) throw new ModException("data mod SQL digest mismatch", ModErrors.Digest);
        _ = DataModSql.Statements(bytes);
        return bytes;
    }

    /// <summary>The installed data mod's SQL, from the backup object store, checked again (FIMOD004 launch).</summary>
    private byte[] InstalledSql(ModManifest manifest)
    {
        var sql = manifest.DatabaseSql ?? throw new ModException("no data mod SQL");
        var bytes = ObjectBytes(sql.Sha256);
        if (bytes.LongLength != sql.Bytes) throw new ModException("data mod SQL damaged", ModErrors.Digest);
        _ = DataModSql.Statements(bytes);
        return bytes;
    }
}
