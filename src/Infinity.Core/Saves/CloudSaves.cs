// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Security.Cryptography;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>The player commit kinds of <c>X-Fi-Commit-Kind</c> (SAVES_API.md §4.3).</summary>
public static class CommitKinds
{
    public const string Upload = "upload";
    public const string ResolveKeepLocal = "resolve_keep_local";
    public const string ImportOffline = "import_offline";

    /// <summary>CR-S51: a Hardcore line's revision 1 from Azimuth's own first save (<c>If-Match: "0"</c>, line <c>start: fresh_start</c>).</summary>
    public const string FreshStart = "fresh_start";

    /// <summary>
    /// CR-S52: a Hardcore line's revision 1 from another of the account's profiles' save, checked on receipt (<c>If-Match: "0"</c>,
    /// <c>X-Fi-Seed-Source</c>, line <c>start: fresh_start</c>, only where <c>/info saves.hardcoreFromProfile</c>).
    /// </summary>
    public const string SeedFromProfile = "seed_from_profile";

    public static bool IsPlayerKind(string kind) => kind is Upload or ResolveKeepLocal or ImportOffline or FreshStart or SeedFromProfile;
}

/// <summary>
/// The play session's save lease, the one-writer rule on this side (SAVES_API.md §1.1): every write
/// carries <see cref="SessionId"/> and the current <see cref="FenceToken"/>. Heartbeats may hand a
/// newer token (a resumed lease); a token never goes back. Once <see cref="Lost"/>
/// (<c>SAVES.LEASE_LOST</c>), no write is sent again under this session.
/// </summary>
public sealed class SaveLease
{
    private readonly Lock _lock = new();
    private ulong _fence;
    private bool _lost;

    public SaveLease(string sessionId, Lease lease)
    {
        ArgumentNullException.ThrowIfNull(lease);
        SessionId = sessionId ?? throw new ArgumentNullException(nameof(sessionId));
        _fence = lease.FenceToken;
    }

    public string SessionId { get; }

    public ulong FenceToken
    {
        get
        {
            lock (_lock)
            {
                return _fence;
            }
        }
    }

    public bool Lost
    {
        get
        {
            lock (_lock)
            {
                return _lost;
            }
        }
    }

    /// <summary>A heartbeat's lease: adopted when its token is not older than the one held.</summary>
    public void Adopt(Lease? lease)
    {
        if (lease is null)
        {
            return;
        }
        lock (_lock)
        {
            if (lease.FenceToken >= _fence)
            {
                _fence = lease.FenceToken;
            }
        }
    }

    public void MarkLost()
    {
        lock (_lock)
        {
            _lost = true;
        }
    }
}

/// <summary>A profile's cloud save line for one play session: its lease and the head at admission.</summary>
public sealed record CloudSaveLine(SaveLease Lease, SaveHead Head)
{
    /// <summary>OD-52: only a Supporter's (<c>vip</c>) Hardcore profile keeps data on the servers.</summary>
    public static bool Eligible(string tier, string mode) => tier == "vip" && mode == "hardcore";

    /// <summary>
    /// The line of a gate admission, or null when the gate gave none (<c>lease</c> and <c>save</c> only
    /// for an eligible profile, GATE_API.md §2, OD-52): then the session plays without cloud saves and
    /// nothing is uploaded or downloaded.
    /// </summary>
    public static CloudSaveLine? FromGate(GateActive active)
    {
        ArgumentNullException.ThrowIfNull(active);
        if (active.Lease is not { } lease || active.Save is not { ValueKind: JsonValueKind.Object } save)
        {
            return null;
        }
        var head = save.Deserialize<SaveHead>(ApiClient.Json) ?? throw new JsonException("save is not a SaveHead");
        return new CloudSaveLine(new SaveLease(active.SessionId, lease), head);
    }
}

/// <summary>What a save call came to, mapped from the contract's codes (SAVES_API.md §6, SAVE_SYNC.md §4).</summary>
public enum SaveFailureKind
{
    /// <summary>Global online closure: preserve pending bytes without an automatic retry.</summary>
    OnlineClosed,
    /// <summary>Unreachable, 429 (<c>SAVES.RATE_LIMITED</c>), 503 (<c>SAVES.UNAVAILABLE</c>), any 5xx: retry with backoff, same key and bytes.</summary>
    Transient,

    /// <summary>The access token was refused: refresh it, then retry with the same key.</summary>
    Unauthorized,

    /// <summary><c>SAVES.LEASE_EXPIRED</c>: heartbeat (it may resume the lease under a new token), then retry with the same key.</summary>
    LeaseExpired,

    /// <summary><c>SAVES.LEASE_LOST</c>: another session holds the profile. Stop writing; the next launch reconciles.</summary>
    LeaseLost,

    /// <summary><c>SAVES.REVISION_CONFLICT</c> <c>{expected, head}</c>: never retry blindly; re-read the head and decide again.</summary>
    RevisionConflict,

    /// <summary><c>SAVES.GATE_REFUSED</c> <c>{reason, parkedId}</c>: Hardcore progress too fast for the time played; parked by the server, kept on the PC.</summary>
    GateRefused,

    /// <summary><c>SAVES.SUMMARY_UNREADABLE</c> <c>{parkedId}</c>: parked for support; kept on the PC.</summary>
    SummaryUnreadable,

    /// <summary><c>SAVES.BUNDLE_INVALID</c> <c>{reason}</c>: these bytes never again; wait for a newer sealed revision.</summary>
    BundleInvalid,

    /// <summary><c>SAVES.DIGEST_MISMATCH</c>: re-read the revision from disk, new key, retry once.</summary>
    DigestMismatch,

    /// <summary><c>SAVES.HARDCORE_NEEDS_FRESH_CAREER</c>.</summary>
    NeedsFreshCareer,

    /// <summary><c>SAVES.IDEMPOTENCY_KEY_REUSED</c>: a client bug (one key per commit).</summary>
    KeyReused,

    /// <summary><c>SAVES.PROFILE_NOT_FOUND</c>: refresh <c>/me</c> (also a retired line, OD-52).</summary>
    ProfileNotFound,

    /// <summary><c>SAVES.REVISION_NOT_FOUND</c>: outside retention.</summary>
    RevisionNotFound,

    /// <summary><c>SESSIONS.ENDED</c> <c>{reason}</c>: start a new session.</summary>
    SessionEnded,

    /// <summary><c>REQUEST.TOO_LARGE</c>: over 384 KiB.</summary>
    TooLarge,

    /// <summary>What was downloaded or installed did not verify (digests, bundle checks, the install's own checks). Nothing local changed.</summary>
    Integrity,

    /// <summary>Any other refusal (<c>SAVES.INVALID_ARGUMENT</c>, unknown codes): permanent, the bytes are set aside.</summary>
    Rejected,

    /// <summary>A 2xx whose body did not read (<c>HTTP.BAD_BODY</c>, <c>HTTP.EMPTY</c>): the commit may have happened; replay the same key and bytes.</summary>
    Unconfirmed,

    /// <summary>Another upload of the profile is still pending: send it (or set it aside) first. Nothing was sent.</summary>
    UploadPending,

    /// <summary>A file of this PC's newest revision is open for writing (Azimuth is saving): wait and decide again. Nothing was moved.</summary>
    Busy,

    /// <summary>This PC's newest sealed revision does not read as a save (an unknown file, an unreadable file).</summary>
    LocalUnreadable,

    /// <summary>This PC's newest sealed revision is not the one the decision was made on: decide again. Nothing was installed.</summary>
    LocalChanged,
}

/// <summary>A failed save call: kind, the server's code and message, and the details the client acts on.</summary>
public sealed record SaveFailure(
    SaveFailureKind Kind,
    string Code,
    HttpStatusCode? Status,
    string Message,
    TimeSpan? RetryAfter = null,
    ulong? Head = null,
    string? ParkedId = null,
    string? Reason = null,
    string? Rule = null,
    string? Review = null)
{
    /// <summary>
    /// Kinds that keep a pending upload for a retry with the same key and bytes (SAVES_API.md §6). Every
    /// other answer (a permanent refusal, <c>REVISION_CONFLICT</c>, <c>LEASE_LOST</c>, unknown codes) sets
    /// the bytes aside, kept on disk: a conflict is decided again on a fresh head, never retried blindly,
    /// and a pending upload that can never succeed must not block every later commit.
    /// </summary>
    public bool KeepsPending => Kind is SaveFailureKind.Transient or SaveFailureKind.Unauthorized or SaveFailureKind.LeaseExpired
        or SaveFailureKind.SessionEnded or SaveFailureKind.Unconfirmed or SaveFailureKind.OnlineClosed;

    /// <summary>Maps the contract's error body (SAVES_API.md §6).</summary>
    public static SaveFailure From(HttpStatusCode status, ErrorBody error, TimeSpan? retryAfter)
    {
        ArgumentNullException.ThrowIfNull(error);
        var details = error.Details is { ValueKind: JsonValueKind.Object } d ? d : (JsonElement?)null;
        string? Text(string name) => details is { } o && o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
        ulong? Number(string name) => details is { } o && o.TryGetProperty(name, out var v) && v.TryGetUInt64(out var n) ? n : null;
        var kind = error.Code switch
        {
            // A 2xx with an unreadable body (HTTP.BAD_BODY, HTTP.EMPTY): the server may have committed.
            _ when (int)status is >= 200 and <= 299 => SaveFailureKind.Unconfirmed,
            ErrorCodes.SavesLeaseLost => SaveFailureKind.LeaseLost,
            ErrorCodes.SavesLeaseExpired => SaveFailureKind.LeaseExpired,
            ErrorCodes.SavesRevisionConflict => SaveFailureKind.RevisionConflict,
            ErrorCodes.ServiceBudgetClosed or ErrorCodes.ServiceOnlineClosed when status == HttpStatusCode.ServiceUnavailable => SaveFailureKind.OnlineClosed,
            ErrorCodes.SavesGateRefused => SaveFailureKind.GateRefused,
            ErrorCodes.SavesSummaryUnreadable => SaveFailureKind.SummaryUnreadable,
            ErrorCodes.SavesBundleInvalid => SaveFailureKind.BundleInvalid,
            ErrorCodes.SavesDigestMismatch => SaveFailureKind.DigestMismatch,
            ErrorCodes.SavesHardcoreNeedsFreshCareer => SaveFailureKind.NeedsFreshCareer,
            ErrorCodes.SavesIdempotencyKeyReused => SaveFailureKind.KeyReused,
            ErrorCodes.SavesProfileNotFound => SaveFailureKind.ProfileNotFound,
            ErrorCodes.SavesRevisionNotFound => SaveFailureKind.RevisionNotFound,
            ErrorCodes.SessionsEnded => SaveFailureKind.SessionEnded,
            ErrorCodes.RequestTooLarge => SaveFailureKind.TooLarge,
            ErrorCodes.SavesRateLimited or ErrorCodes.SavesUnavailable or ErrorCodes.RequestRateLimited => SaveFailureKind.Transient,
            ErrorCodes.AuthTokenExpired or ErrorCodes.AuthTokenInvalid => SaveFailureKind.Unauthorized,
            _ when status == HttpStatusCode.TooManyRequests || (int)status >= 500 => SaveFailureKind.Transient,
            _ => SaveFailureKind.Rejected,
        };
        return new SaveFailure(kind, error.Code, status, error.Message, retryAfter, Number("head"), Text("parkedId"), Text("reason"), Text("rule"), Text("review"));
    }

    internal static SaveFailure Network(Exception e) => new(SaveFailureKind.Transient, "NETWORK", null, e.Message);

    internal static SaveFailure Integrity(string message) => new(SaveFailureKind.Integrity, "INTEGRITY", null, message);

    internal static SaveFailure Local(SaveFailureKind kind, string message) => new(kind, "LOCAL." + kind.ToString().ToUpperInvariant(), null, message);
}

/// <summary>
/// <see cref="CloudSaves.Decide"/>'s outcome: the decision and the local content digest it was made on
/// (pass it to <see cref="CloudSaves.DownloadAsync"/>), or why no decision could be made.
/// </summary>
public sealed record LocalDecision(SyncDecision? Decision, string? LocalContent, SaveFailure? Failure)
{
    public bool Ok => Failure is null;
}

/// <summary>
/// A commit's outcome: <see cref="Committed"/> on 201 (or 200 replayed), <see cref="Unchanged"/>
/// when the cloud already holds this content on that head (nothing sent), else <see cref="Failure"/>.
/// <see cref="Pending"/> is the upload still recorded for a retry (null once confirmed or set aside).
/// </summary>
public sealed record CommitOutcome(CommitResponse? Committed, bool Replayed, bool Unchanged, SaveFailure? Failure, PendingUpload? Pending, string? SetAsidePath = null)
{
    public bool Ok => Failure is null;
}

/// <summary>
/// A download's outcome: the installed revision and its bundle, or why it was not installed.
/// <see cref="LandedPath"/>: the revision did land in <c>revisions</c> but its final check failed (the
/// mark did not move); the folder is there and the caller must decide again before launching.
/// </summary>
public sealed record DownloadOutcome(InstalledRevision? Installed, ValidBundle? Bundle, SaveFailure? Failure, string? LandedPath = null)
{
    public bool Ok => Failure is null;

    public bool Landed => Installed is not null || LandedPath is not null;
}

/// <summary>Backoff of transient save failures: 1, 2, 4 … 60 s with jitter, or <c>Retry-After</c> when longer.</summary>
public static class CommitBackoff
{
    public static TimeSpan Delay(int attempt, TimeSpan? retryAfter, double jitter)
    {
        var seconds = Math.Min(60, Math.Pow(2, Math.Clamp(attempt, 0, 6)));
        var delay = TimeSpan.FromSeconds(Math.Min(60, seconds * (0.75 + (0.5 * Math.Clamp(jitter, 0, 1)))));
        return retryAfter is { } after && after > delay ? after : delay;
    }
}

/// <summary>
/// The launcher's cloud save client (SAVES_API.md §4-§5, SAVE_SYNC.md §2-§4), the C# side of the
/// Rust launcher's save sync. Pure of HTTP: the routes are an <see cref="ISaveApi"/>, the local
/// state a <see cref="SaveSyncStore"/>, Azimuth's folders <see cref="CloudSaveFiles"/>.
/// <list type="bullet">
/// <item>One writer: every write carries the session's <see cref="SaveLease"/>; after <c>LEASE_LOST</c> none is sent.</item>
/// <item>CAS on the head: every commit names <c>If-Match</c> (0 on an empty line); a conflict is returned, never retried blindly.</item>
/// <item>Exactly once: the bundle and its key are persisted before the first send and resent unchanged by every retry.</item>
/// <item>Nothing the PC has is thrown away: an install copies every revision it shadows to the archive first; refused bytes are set aside, not deleted.</item>
/// </list>
/// Callers use it only for an eligible profile (<see cref="CloudSaveLine.FromGate"/> non-null, OD-52).
/// </summary>
public sealed class CloudSaves
{
    private readonly ISaveApi _api;
    private readonly SaveSyncStore _store;
    private readonly string _realm;

    public CloudSaves(ISaveApi api, SaveSyncStore store, string realm)
    {
        _api = api ?? throw new ArgumentNullException(nameof(api));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _realm = realm ?? throw new ArgumentNullException(nameof(realm));
    }

    private static string Wire(ulong xuid) => xuid.ToString(CultureInfo.InvariantCulture);

    public ProfileSyncState? State(ulong xuid) => _store.Get(_realm, xuid);

    /// <summary>CR-S61: the profile was deleted from the cloud; its sync state on this server goes (pending bytes set aside).</summary>
    public bool Forget(ulong xuid) => _store.Forget(_realm, xuid);

    /// <summary>
    /// Before every reconcile and launch: staging leftovers removed, unsealed folders above the newest
    /// sealed one archived. Busy folders (a file open for writing) stay; decide again once they settle.
    /// </summary>
    public static PreparedFolders PrepareFolders(string savesRoot, ulong xuid)
    {
        CloudSaveFiles.CleanIncoming(savesRoot);
        return CloudSaveFiles.QuarantineUnsealed(savesRoot, xuid);
    }

    /// <summary>
    /// The decision for <paramref name="head"/> from this PC's newest sealed revision and the stored
    /// mark, with the local digest it was made on. A folder being written above the newest sealed one
    /// gives <see cref="SaveFailureKind.Busy"/>; a newest revision that does not read as a save,
    /// <see cref="SaveFailureKind.LocalUnreadable"/>. Never throws for the PC's files.
    /// </summary>
    public LocalDecision Decide(string savesRoot, ulong xuid, SaveHead head)
    {
        ArgumentNullException.ThrowIfNull(head);
        var (local, failure) = LocalNewest(savesRoot, xuid);
        return failure is not null
            ? new LocalDecision(null, null, failure)
            : new LocalDecision(SaveReconcile.Decide(local, State(xuid)?.Mark, CloudHead.From(head)), local, null);
    }

    /// <summary>The content digest of this PC's newest sealed revision (null when none), or why it cannot be read.</summary>
    private static (string? Content, SaveFailure? Failure) LocalNewest(string savesRoot, ulong xuid)
    {
        var profileDir = CloudSaveFiles.ProfileDir(savesRoot, xuid);
        try
        {
            var newest = AzimuthSaves.NewestSealed(profileDir);
            if (CloudSaveFiles.BusyAbove(profileDir, newest?.Number) is { Count: > 0 } busy)
            {
                return (null, SaveFailure.Local(SaveFailureKind.Busy, busy[0] + " is being written"));
            }
            return (newest is null ? null : Fh2Bundle.ContentDigest(CloudSaveFiles.ReadFiles(newest)), null);
        }
        catch (IOException e) when (AzimuthSaves.IsBusy(e))
        {
            return (null, SaveFailure.Local(SaveFailureKind.Busy, e.Message));
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            return (null, SaveFailure.Local(SaveFailureKind.LocalUnreadable, e.Message));
        }
    }

    /// <summary><c>InSync</c>: the mark follows the head.</summary>
    public void MarkInSync(ulong xuid, ulong revision, string content) =>
        _store.SetMark(_realm, xuid, new SyncMark(revision, SaveReconcile.NormalizeDigest(content) ?? throw new ArgumentException("not a digest", nameof(content))));

    public async Task<(SaveHead? Head, SaveFailure? Failure)> HeadAsync(string access, ulong xuid, CancellationToken cancellation = default)
    {
        try
        {
            var result = await _api.HeadAsync(access, Wire(xuid), cancellation).ConfigureAwait(false);
            return result.Ok ? (result.Value, null) : (null, SaveFailure.From(result.Status, result.Error!, result.RetryAfter));
        }
        catch (Exception e) when (IsNetwork(e, cancellation))
        {
            return (null, SaveFailure.Network(e));
        }
    }

    /// <summary>Test seam: runs right after an install's rename into <c>revisions</c>, before its final check.</summary>
    internal Action<string>? AfterInstallMove { get; set; }

    /// <summary>
    /// Downloads and installs the revision <paramref name="head"/> names. The creation path of a new
    /// Hardcore profile (OD-60): the caller obtains the seeded revision 1's head from the server and
    /// passes it here; the profile's container may be empty or missing. <paramref name="expectedLocal"/>
    /// as in <see cref="DownloadAsync"/>.
    /// </summary>
    public Task<DownloadOutcome> InstallHeadAsync(string access, string savesRoot, ulong xuid, SaveHead head, string? expectedLocal, string label, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(head);
        return head.Revision == 0
            ? Task.FromResult(new DownloadOutcome(null, null, SaveFailure.Integrity("the line is empty: nothing to install")))
            : head.IsCloudExpired() // retention (SAVES_API §4.1): never requested, the local save stays
            ? Task.FromResult(new DownloadOutcome(null, null, SaveFailure.Integrity("the cloud copy expired: nothing to install")))
            : DownloadAsync(access, savesRoot, xuid, head.Revision, head.ContentSha256 ?? string.Empty, expectedLocal, label, cancellation);
    }

    /// <summary>
    /// Downloads <paramref name="revision"/>, verifies it (size, <c>Repr-Digest</c>, the served revision,
    /// the declared and the recomputed content digest, every bundle check), installs it as the newest
    /// local revision (every existing folder copied to the archive as <paramref name="label"/> first) and
    /// moves the mark to it. Only while the game is not running. <paramref name="expectedLocal"/> is the
    /// local content digest the decision was made on (<see cref="LocalDecision.LocalContent"/>, null for
    /// none): when this PC's newest sealed revision is another one now, nothing is installed
    /// (<see cref="SaveFailureKind.LocalChanged"/>). On any failure before the install's rename nothing
    /// local changed; a failed final check reports <see cref="DownloadOutcome.LandedPath"/>. The mark
    /// moves only on success.
    /// </summary>
    public async Task<DownloadOutcome> DownloadAsync(string access, string savesRoot, ulong xuid, ulong revision, string expectedContent, string? expectedLocal, string label, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(expectedContent);
        if (SaveReconcile.NormalizeDigest(expectedContent) is not { } expected)
        {
            return new DownloadOutcome(null, null, SaveFailure.Integrity("the head has no content digest"));
        }
        SaveApiResult<SaveBundleDownload> result;
        try
        {
            result = await _api.BundleAsync(access, Wire(xuid), revision, cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (IsNetwork(e, cancellation))
        {
            return new DownloadOutcome(null, null, SaveFailure.Network(e));
        }
        if (!result.Ok)
        {
            return new DownloadOutcome(null, null, SaveFailure.From(result.Status, result.Error!, result.RetryAfter));
        }
        var download = result.Value!;
        if (download.Revision is { } served && served != revision)
        {
            return new DownloadOutcome(null, null, SaveFailure.Integrity(string.Create(CultureInfo.InvariantCulture, $"asked for revision {revision}, the server sent {served}")));
        }
        if (download.Bytes.Length > Fh2Bundle.MaxBundle)
        {
            return new DownloadOutcome(null, null, SaveFailure.Integrity("the bundle is over 384 KiB"));
        }
        if (download.BundleSha256 is null || download.BundleSha256 != Convert.ToHexStringLower(SHA256.HashData(download.Bytes)))
        {
            return new DownloadOutcome(null, null, SaveFailure.Integrity("the bundle does not match its Repr-Digest"));
        }
        if (download.ContentSha256 is { } declared && declared != expected)
        {
            return new DownloadOutcome(null, null, SaveFailure.Integrity("the server declared another content digest than the head's"));
        }
        ValidBundle bundle;
        try
        {
            bundle = Fh2Bundle.Validate(download.Bytes);
        }
        catch (BundleException e)
        {
            return new DownloadOutcome(null, null, SaveFailure.Integrity(e.Message));
        }
        if (bundle.ContentSha256 != expected)
        {
            return new DownloadOutcome(null, bundle, SaveFailure.Integrity("the downloaded save does not match the head's content digest"));
        }
        var (local, localFailure) = LocalNewest(savesRoot, xuid);
        if (localFailure is not null)
        {
            return new DownloadOutcome(null, bundle, localFailure);
        }
        if (local != SaveReconcile.NormalizeDigest(expectedLocal) || (expectedLocal is not null && local is null))
        {
            return new DownloadOutcome(null, bundle, SaveFailure.Local(SaveFailureKind.LocalChanged,
                "this PC's newest save is not the one the decision was made on: decide again"));
        }
        InstalledRevision installed;
        try
        {
            installed = CloudSaveFiles.Install(savesRoot, xuid, bundle.Files, expected, Guid.CreateVersion7().ToString("N"), label, AfterInstallMove);
        }
        catch (SaveException e) when (e.Landed)
        {
            // In revisions but its final check failed: reported as landed, the mark stays.
            return new DownloadOutcome(null, bundle, SaveFailure.Integrity(e.Message), e.LandedPath ?? CloudSaveFiles.ProfileDir(savesRoot, xuid));
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
        {
            return new DownloadOutcome(null, bundle, SaveFailure.Integrity(e.Message));
        }
        _store.SetMark(_realm, xuid, new SyncMark(revision, expected));
        return new DownloadOutcome(installed, bundle, null);
    }

    /// <summary>
    /// Commits <paramref name="revision"/> on top of <paramref name="expected"/> (the head the decision
    /// was made on; <c>If-Match: "0"</c> only on an empty line). <see cref="CommitOutcome.Unchanged"/> without a
    /// request when the mark already says the cloud holds this content at <paramref name="expected"/>.
    /// The bundle and a new key are persisted before the send; a failure that keeps them is retried with
    /// <see cref="SendPendingAsync"/>. Refused while another upload of the profile is pending.
    /// </summary>
    public async Task<CommitOutcome> UploadAsync(string access, ulong xuid, SaveLease lease, LocalRevision revision, ulong expected, string kind = CommitKinds.Upload, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(lease);
        ArgumentNullException.ThrowIfNull(revision);
        if (!CommitKinds.IsPlayerKind(kind))
        {
            throw new ArgumentException("upload, resolve_keep_local, import_offline or fresh_start", nameof(kind));
        }
        if (lease.Lost)
        {
            return new CommitOutcome(null, false, false, LeaseLostLocally(), State(xuid)?.PendingUpload);
        }
        PackedRevision packed;
        try
        {
            packed = CloudSaveFiles.Pack(revision);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            // Not sealed any more (Azimuth writing it): nothing is sent; the next sealed revision is tried.
            return new CommitOutcome(null, false, false, SaveFailure.Integrity(e.Message), null);
        }
        var state = State(xuid);
        if (state is { Mark: { } mark } && mark.Revision == expected && mark.Content == packed.ContentSha256)
        {
            return new CommitOutcome(null, false, true, null, null);
        }
        if (state?.PendingUpload is { } waiting)
        {
            return new CommitOutcome(null, false, false, PendingFirst(waiting), waiting);
        }
        try
        {
            _store.BeginUpload(_realm, xuid, Guid.CreateVersion7().ToString(), expected, kind, packed.Number, packed.ContentSha256, packed.Bundle);
        }
        catch (InvalidOperationException) when (State(xuid)?.PendingUpload is { } raced)
        {
            return new CommitOutcome(null, false, false, PendingFirst(raced), raced);
        }
        return await SendPendingAsync(access, xuid, lease, cancellation).ConfigureAwait(false);
    }

    /// <summary>
    /// Records <paramref name="revision"/> as the profile's pending upload without sending it (no token needed): its bytes are kept
    /// whatever happens next and <see cref="SendPendingAsync"/> sends exactly them. CR-S51: a new career's first save is captured the
    /// moment it is seen. Null when recorded; a failure when the revision is no longer sealed or another upload is pending.
    /// </summary>
    public SaveFailure? RecordUpload(ulong xuid, LocalRevision revision, ulong expected, string kind)
    {
        ArgumentNullException.ThrowIfNull(revision);
        if (!CommitKinds.IsPlayerKind(kind)) throw new ArgumentException("a player commit kind", nameof(kind));
        if (State(xuid)?.PendingUpload is { } waiting) return PendingFirst(waiting);
        PackedRevision packed;
        try { packed = CloudSaveFiles.Pack(revision); }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        { return SaveFailure.Integrity(e.Message); }
        try { _store.BeginUpload(_realm, xuid, Guid.CreateVersion7().ToString(), expected, kind, packed.Number, packed.ContentSha256, packed.Bundle); }
        catch (InvalidOperationException) when (State(xuid)?.PendingUpload is { } raced) { return PendingFirst(raced); }
        return null;
    }

    /// <summary>
    /// CR-S52: the seed the player confirmed, recorded as the profile's pending <c>seed_from_profile</c> upload on top of revision 0:
    /// exactly <paramref name="packed"/>'s bytes (the ones the dry run checked) and <paramref name="sourceXuid"/> for
    /// <c>X-Fi-Seed-Source</c>. Nothing is sent: the profile's first launch sends it before the game starts. Null when recorded;
    /// <see cref="SaveFailureKind.UploadPending"/> when another upload of the profile is pending.
    /// </summary>
    public SaveFailure? RecordSeed(ulong xuid, PackedRevision packed, ulong localRevision, ulong sourceXuid)
    {
        ArgumentNullException.ThrowIfNull(packed);
        if (State(xuid)?.PendingUpload is { } waiting) return PendingFirst(waiting);
        try
        {
            _store.BeginUpload(_realm, xuid, Guid.CreateVersion7().ToString(), 0, CommitKinds.SeedFromProfile, localRevision, packed.ContentSha256,
                packed.Bundle, Wire(sourceXuid));
        }
        catch (InvalidOperationException) when (State(xuid)?.PendingUpload is { } raced) { return PendingFirst(raced); }
        return null;
    }

    private static SaveFailure PendingFirst(PendingUpload waiting) => SaveFailure.Local(SaveFailureKind.UploadPending,
        $"commit {waiting.CommitId} is still pending for this profile: send it or set it aside first");

    /// <summary>
    /// Sends the profile's pending upload: exactly its bytes, its key, its <c>If-Match</c>, with the
    /// lease's current token. A 2xx whose revision is <c>Expected + 1</c> and whose content digest is the
    /// pending one moves the mark to the committed head (any other 2xx is an
    /// <see cref="SaveFailureKind.Integrity"/> failure: bytes set aside, mark unmoved). Retryable answers
    /// (<see cref="SaveFailure.KeepsPending"/>) keep the bytes for the next retry; every other answer,
    /// <c>REVISION_CONFLICT</c> and <c>LEASE_LOST</c> included, sets them aside (kept on disk) and is
    /// returned typed, so the caller decides again. <c>LEASE_LOST</c> marks the lease lost. Only the
    /// upload that was sent is finished or set aside: a newer one begun meanwhile is left untouched.
    /// </summary>
    public async Task<CommitOutcome> SendPendingAsync(string access, ulong xuid, SaveLease lease, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(lease);
        if (lease.Lost)
        {
            return new CommitOutcome(null, false, false, LeaseLostLocally(), State(xuid)?.PendingUpload);
        }
        var pending = State(xuid)?.PendingUpload ?? throw new InvalidOperationException("no upload is pending for this profile");
        byte[] bytes;
        try
        {
            bytes = _store.PendingBytes(pending);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
        {
            // The recorded bytes are gone or changed: nothing is sent under this key; the record stays for support.
            return new CommitOutcome(null, false, false, SaveFailure.Integrity(e.Message), pending);
        }
        var request = new SaveCommitRequest(pending.Expected, pending.CommitId, lease.SessionId, lease.FenceToken, pending.Kind, pending.Content, bytes,
            pending.Kind == CommitKinds.SeedFromProfile ? pending.SeedSource : null);
        SaveApiResult<CommitResponse> result;
        try
        {
            result = await _api.CommitAsync(access, Wire(xuid), request, cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (IsNetwork(e, cancellation))
        {
            return new CommitOutcome(null, false, false, SaveFailure.Network(e), pending);
        }
        if (result.Ok)
        {
            var committed = result.Value!;
            var content = SaveReconcile.NormalizeDigest(pending.Content);
            if (content is null || SaveReconcile.NormalizeDigest(committed.ContentSha256) != content
                || pending.Expected == ulong.MaxValue || committed.Revision != pending.Expected + 1)
            {
                // Not the commit that was sent: the mark stays, the bytes are kept (set aside) for support.
                var failure = SaveFailure.Integrity(string.Create(CultureInfo.InvariantCulture,
                    $"the server confirmed revision {committed.Revision} ({committed.ContentSha256}) for a commit on {pending.Expected} with {pending.Content}"));
                return new CommitOutcome(committed, result.Replayed, false, failure, null, _store.SetAside(_realm, xuid, pending.CommitId));
            }
            _store.FinishUpload(_realm, xuid, pending.CommitId, new SyncMark(committed.Revision, content));
            return new CommitOutcome(committed, result.Replayed, false, null, null);
        }
        var refused = SaveFailure.From(result.Status, result.Error!, result.RetryAfter);
        if (refused.Kind == SaveFailureKind.LeaseLost)
        {
            lease.MarkLost();
        }
        // CR-S52: a seed lost with its lease stays recorded: the next Play sends the same checked bytes (no second dry run spent)
        if (refused.KeepsPending || (refused.Kind == SaveFailureKind.LeaseLost && pending.Kind == CommitKinds.SeedFromProfile))
        {
            return new CommitOutcome(null, false, false, refused, pending);
        }
        return new CommitOutcome(null, false, false, refused, null, _store.SetAside(_realm, xuid, pending.CommitId));
    }

    /// <summary>Drops the profile's pending upload without touching the mark (its bytes move to <c>pending\set-aside</c>), e.g. after a conflict the player resolved.</summary>
    public string? SetAsidePending(ulong xuid) => _store.SetAside(_realm, xuid);

    private static SaveFailure LeaseLostLocally() =>
        new(SaveFailureKind.LeaseLost, ErrorCodes.SavesLeaseLost, null, "this session lost the profile's lease; nothing more is written");

    private static bool IsNetwork(Exception e, CancellationToken cancellation) =>
        e is HttpRequestException || (e is TaskCanceledException && !cancellation.IsCancellationRequested);
}
