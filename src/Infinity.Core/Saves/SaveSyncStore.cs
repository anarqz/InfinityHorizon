// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Saves;

/// <summary>
/// A commit not confirmed yet: every retry resends exactly <see cref="BundleFile"/>'s bytes under
/// <see cref="CommitId"/> (the <c>Idempotency-Key</c>), even after Azimuth pruned the folder. <see cref="SeedSource"/>: the source
/// profile's decimal XUID of a <c>seed_from_profile</c> commit (CR-S52, <c>X-Fi-Seed-Source</c>); null for every other kind.
/// </summary>
public sealed record PendingUpload(
    string CommitId,
    ulong Expected,
    string Kind,
    ulong LocalRevision,
    string Content,
    string BundleFile,
    string BundleSha256,
    string? SeedSource = null);

/// <summary>One profile's sync state, keyed by the server realm (<c>/info</c> <c>realm</c>) and the XUID.</summary>
public sealed record ProfileSyncState(string Realm, ulong Xuid, SyncMark? Mark, PendingUpload? PendingUpload);

/// <summary><c>sync-state.json</c> (saves.rs <c>SyncStateFile</c>, version 2).</summary>
public sealed record SyncStateFile(uint Version, IReadOnlyList<ProfileSyncState> Profiles);

/// <summary>
/// Per-profile marks and pending uploads in the launcher's state folder (saves.rs <c>SyncStore</c>):
/// one owner per process (every change under one lock, written whole through a temporary file),
/// pending bundles in <c>pending\&lt;commitId&gt;.fh2save</c>. The mark is written only after a 2xx
/// commit or a verified install and never cleared by an error. A damaged state file is kept aside
/// (<c>sync-state.json.damaged-*</c>) and reads as empty: no mark decides Conflict, never an upload.
/// Bytes a pending upload stops needing are moved to <c>pending\set-aside</c>, not deleted.
/// </summary>
public sealed class SaveSyncStore
{
    public const string FileName = "sync-state.json";
    public const string PendingDir = "pending";
    public const string SetAsideDir = "set-aside";

    private static readonly JsonSerializerOptions Options = new(JsonSerializerDefaults.Web) { WriteIndented = true };

    private readonly string _dir;
    private readonly Lock _lock = new();

    public SaveSyncStore(string stateDirectory)
    {
        _dir = stateDirectory ?? throw new ArgumentNullException(nameof(stateDirectory));
    }

    private string FilePath => Path.Combine(_dir, FileName);

    public SyncStateFile Snapshot()
    {
        lock (_lock)
        {
            return Load();
        }
    }

    public ProfileSyncState? Get(string realm, ulong xuid) =>
        Snapshot().Profiles.FirstOrDefault(p => p.Realm == realm && p.Xuid == xuid);

    /// <summary>Changes one profile's entry under the store's lock and writes the file; returns the new entry.</summary>
    public ProfileSyncState Update(string realm, ulong xuid, Func<ProfileSyncState, ProfileSyncState> change)
    {
        ArgumentNullException.ThrowIfNull(realm);
        ArgumentNullException.ThrowIfNull(change);
        lock (_lock)
        {
            var file = Load();
            var profiles = file.Profiles.ToList();
            var index = profiles.FindIndex(p => p.Realm == realm && p.Xuid == xuid);
            var current = index >= 0 ? profiles[index] : new ProfileSyncState(realm, xuid, null, null);
            var next = change(current) with { Realm = realm, Xuid = xuid };
            if (index >= 0)
            {
                profiles[index] = next;
            }
            else
            {
                profiles.Add(next);
            }
            Save(file with { Version = 2, Profiles = profiles });
            return next;
        }
    }

    /// <summary>The mark after an install or an in-sync check (never on an error).</summary>
    public void SetMark(string realm, ulong xuid, SyncMark mark)
    {
        ArgumentNullException.ThrowIfNull(mark);
        Update(realm, xuid, s => s with { Mark = mark });
    }

    /// <summary>
    /// Writes the bundle to <c>pending\&lt;commitId&gt;.fh2save</c> and records the pending upload, both
    /// before the first send. Refused while another pending upload of the profile exists: it is sent
    /// (or set aside) first, so one key never names two different bundles.
    /// </summary>
    public PendingUpload BeginUpload(string realm, ulong xuid, string commitId, ulong expected, string kind, ulong localRevision, string content, byte[] bundle,
        string? seedSource = null)
    {
        ArgumentNullException.ThrowIfNull(commitId);
        ArgumentNullException.ThrowIfNull(bundle);
        if (!Guid.TryParse(commitId, out var parsed) || parsed.ToString() != commitId)
        {
            throw new ArgumentException("a commit id is a lower-case UUID", nameof(commitId));
        }
        var relative = $"{PendingDir}/{commitId}.fh2save";
        var pending = new PendingUpload(commitId, expected, kind, localRevision, content, relative, Convert.ToHexStringLower(SHA256.HashData(bundle)), seedSource);
        lock (_lock)
        {
            if (Get(realm, xuid)?.PendingUpload is { } existing)
            {
                throw new InvalidOperationException($"commit {existing.CommitId} is still pending for this profile: send or set it aside first");
            }
            WriteAtomically(Path.Combine(_dir, PendingDir, commitId + ".fh2save"), bundle);
            Update(realm, xuid, s => s with { PendingUpload = pending });
        }
        return pending;
    }

    /// <summary>The exact bytes of a pending upload, checked against their recorded SHA-256.</summary>
    public byte[] PendingBytes(PendingUpload pending)
    {
        ArgumentNullException.ThrowIfNull(pending);
        var path = Path.Combine(_dir, pending.BundleFile.Replace('/', Path.DirectorySeparatorChar));
        var info = new FileInfo(path);
        if (!info.Exists || info.Length > Fh2Bundle.MaxBundle)
        {
            throw new SaveException(pending.BundleFile + " is missing or too large");
        }
        var bytes = File.ReadAllBytes(path);
        return Convert.ToHexStringLower(SHA256.HashData(bytes)) == pending.BundleSha256
            ? bytes
            : throw new SaveException(pending.BundleFile + " does not match its recorded hash");
    }

    /// <summary>
    /// After a 2xx for the upload <paramref name="commitId"/> (the key that was sent). When it is still
    /// the pending one, the mark moves to the committed head, the entry goes and its bytes are removed
    /// (the cloud has them); returns true. When another upload became pending meanwhile (a late answer),
    /// that newer one and its bytes are left untouched, and the mark moves only forward (the commit is
    /// a fact, but a mark never goes back); returns false.
    /// </summary>
    public bool FinishUpload(string realm, ulong xuid, string commitId, SyncMark mark)
    {
        ArgumentNullException.ThrowIfNull(commitId);
        ArgumentNullException.ThrowIfNull(mark);
        PendingUpload? done = null;
        Update(realm, xuid, s =>
        {
            if (s.PendingUpload is { } pending && pending.CommitId == commitId)
            {
                done = pending;
                return s with { Mark = mark, PendingUpload = null };
            }
            return s.Mark is null || s.Mark.Revision < mark.Revision ? s with { Mark = mark } : s;
        });
        if (done is null)
        {
            return false;
        }
        TryDelete(Path.Combine(_dir, done.BundleFile.Replace('/', Path.DirectorySeparatorChar)));
        return true;
    }

    /// <summary>
    /// Drops the pending record without touching the mark (the server refused these bytes for good,
    /// or the player chose another resolution); the bytes move to <c>pending\set-aside</c>. With
    /// <paramref name="commitId"/>, only when that upload is still the pending one (a late answer never
    /// sets a newer upload aside); null sets aside whatever is pending (the player's choice).
    /// </summary>
    public string? SetAside(string realm, ulong xuid, string? commitId = null)
    {
        PendingUpload? dropped = null;
        Update(realm, xuid, s =>
        {
            if (s.PendingUpload is not { } pending || (commitId is not null && pending.CommitId != commitId))
            {
                return s;
            }
            dropped = pending;
            return s with { PendingUpload = null };
        });
        if (dropped is null)
        {
            return null;
        }
        var from = Path.Combine(_dir, dropped.BundleFile.Replace('/', Path.DirectorySeparatorChar));
        var to = Path.Combine(_dir, PendingDir, SetAsideDir, Path.GetFileName(from));
        if (!File.Exists(from))
        {
            return null;
        }
        Directory.CreateDirectory(Path.GetDirectoryName(to)!);
        File.Move(from, to, overwrite: true);
        return to;
    }

    /// <summary>
    /// CR-S61: the profile was deleted from the cloud (its XUID is retired, never reused): its entry goes, mark included. A pending
    /// upload's bytes are moved to <c>pending\set-aside</c>, never deleted. True when there was an entry.
    /// </summary>
    public bool Forget(string realm, ulong xuid)
    {
        ArgumentNullException.ThrowIfNull(realm);
        SetAside(realm, xuid);
        lock (_lock)
        {
            var file = Load();
            var profiles = file.Profiles.Where(p => !(p.Realm == realm && p.Xuid == xuid)).ToList();
            if (profiles.Count == file.Profiles.Count)
            {
                return false;
            }
            Save(file with { Version = 2, Profiles = profiles });
            return true;
        }
    }

    private SyncStateFile Load()
    {
        try
        {
            if (!File.Exists(FilePath))
            {
                return new SyncStateFile(2, []);
            }
            var file = JsonSerializer.Deserialize<SyncStateFile>(File.ReadAllText(FilePath), Options);
            return file is { Profiles: not null } && file.Profiles.All(p => p is { Realm: not null }
                    && (p.Mark is null || p.Mark.Content is not null)
                    && (p.PendingUpload is null || (p.PendingUpload.CommitId is not null && p.PendingUpload.Kind is not null
                        && p.PendingUpload.Content is not null && p.PendingUpload.BundleFile is not null && p.PendingUpload.BundleSha256 is not null)))
                ? file
                : throw new JsonException("no profiles, or a null entry");
        }
        catch (JsonException)
        {
            // Kept for support, never deleted; an empty state decides Conflict, never an upload.
            KeepDamagedAside();
            return new SyncStateFile(2, []);
        }
    }

    /// <summary>Moves the damaged state file aside; when the move fails (locked, no rights), copies it instead.</summary>
    private void KeepDamagedAside()
    {
        var aside = $"{FilePath}.damaged-{Guid.CreateVersion7():N}";
        try
        {
            File.Move(FilePath, aside);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            try
            {
                File.Copy(FilePath, aside);
            }
            catch (Exception copy) when (copy is IOException or UnauthorizedAccessException)
            {
                // Neither move nor copy worked: the state still reads as empty (Conflict, never an upload).
            }
        }
    }

    private void Save(SyncStateFile file) => WriteAtomically(FilePath, JsonSerializer.SerializeToUtf8Bytes(file, Options));

    private static void WriteAtomically(string path, byte[] bytes)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var temp = path + ".tmp";
        using (var stream = new FileStream(temp, FileMode.Create, FileAccess.Write, FileShare.None))
        {
            stream.Write(bytes);
            stream.Flush(flushToDisk: true);
        }
        File.Move(temp, path, overwrite: true);
    }

    private static void TryDelete(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (IOException)
        {
        }
    }
}
