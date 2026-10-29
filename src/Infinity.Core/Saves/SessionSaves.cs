// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>What a Play's save step did, for the shell's status line and the in-game toasts.</summary>
public enum SaveStep
{
    /// <summary>OD-60: the Hardcore line is being seeded ("Creating Hardcore profile…").</summary>
    Seeding,

    /// <summary>A cloud revision is coming to this PC.</summary>
    Downloading,

    /// <summary>A cloud revision is in place as the newest local revision.</summary>
    Installed,

    /// <summary>This PC and the cloud already agree.</summary>
    InSync,

    /// <summary>The session's progress is being sent.</summary>
    Uploading,

    /// <summary>The cloud took it (detail: the revision).</summary>
    Uploaded,

    /// <summary>Nothing changed in the game: nothing to send.</summary>
    Unchanged,

    /// <summary>Not sent, kept on this PC (detail: the server's code); retried at the next launch.</summary>
    Kept,

    /// <summary>The server's Hardcore save gate refused it (OD-52): parked there, kept on this PC.</summary>
    Refused,

    /// <summary>CR-S51: a new Hardcore career starts in the game; its first save goes to the cloud as soon as Azimuth writes it.</summary>
    FreshStart,

    /// <summary>Retention: the cloud copy of this profile's save expired; this PC's save is kept and nothing came down.</summary>
    CloudExpired,

    /// <summary>
    /// OD-133: the line's first save was refused as past its start and the player chose "not now": this session plays on this PC only
    /// and sends nothing; the next Play asks again.
    /// </summary>
    FreshStartHeld,

    /// <summary>
    /// OD-133: the restored career's commit was refused (detail: the gate's rule): this PC's save from before the restore is back; the
    /// restored career stays in its backup folder.
    /// </summary>
    RestoreRefused,

    /// <summary>
    /// OD-133: the restored career will not be committed but this PC's earlier save could not be put back yet (detail: why the restore
    /// failed): the game does not start on the refused career; every launch tries again, and the shell offers to keep it.
    /// </summary>
    RestoreUndoFailed,

    /// <summary>CR-S52: the server took the career the player chose to start from (<c>seed_from_profile</c>; detail: the revision).</summary>
    SeedAccepted,

    /// <summary>
    /// CR-S52: the seed did not become revision 1 (detail: <see cref="ProfileSeed.CommitDetail"/>, read with
    /// <see cref="ProfileSeed.CommitTextKey"/>): refused by the server's final check, not sent, or answered for another revision; a
    /// <see cref="FreshStart"/> follows when the line is still empty.
    /// </summary>
    SeedRefused,
}

/// <summary>The player's answer to a save conflict (owner 2026-10-26, "Cloud or PC, per the contract"; SAVES_API.md §4.4/§5).</summary>
public enum ConflictChoice
{
    /// <summary>Nothing changes: the launch stops, as before.</summary>
    NotNow,

    /// <summary>The cloud's revision is installed; this PC's revisions are archived first.</summary>
    KeepCloud,

    /// <summary>This PC's newest sealed revision is committed on top of the cloud's (<c>resolve_keep_local</c>); the cloud keeps its history.</summary>
    KeepThisPc,
}

/// <summary>OD-133: the player's answer when a line's first save was refused as past a new career's start.</summary>
public enum FreshStartChoice
{
    /// <summary>Nothing is archived and nothing is sent this session; asked again at the next Play.</summary>
    NotNow,

    /// <summary>The current career is archived (<c>fresh-start-*</c>) and the game starts a new one, whose first save goes up as <c>fresh_start</c>.</summary>
    StartNewCareer,
}

/// <summary>OD-133: what the player is told of a refused first save: the gate's rule (null when unknown) and when (UTC, ISO 8601).</summary>
public sealed record FreshStartRefusal(string? Rule, string? RefusedAtUtc);

/// <summary>What the player sees of a conflict: the cloud's head and this PC's newest sealed revision (each number null when unknown).</summary>
public sealed record SaveConflict(SaveHead Cloud, int? LocalLevel, int? LocalCredits, DateTime? LocalSavedUtc)
{
    /// <summary>An empty line (revision 0) has nothing to install: only this PC's save, or not now, can be offered.</summary>
    public bool CanKeepCloud => Cloud.Revision > 0;
}

/// <summary>A save step that stops the launch: the game is not started over an unknown state.</summary>
public sealed class SaveBlockedException(string code, string message, TimeSpan? retryAfter = null) : Exception(message)
{
    public string Code { get; } = code;

    /// <summary>The server's <c>Retry-After</c> (CR-S52: a seed commit past the day's 5 checks), when it gave one.</summary>
    public TimeSpan? RetryAfter { get; } = retryAfter;
}

/// <summary>
/// The cloud save around one Play (SAVES_API.md §3-§5, OD-52, OD-60), between the gate's admission
/// and <c>azimuth.exe</c>, and after its exit, before the gate's leave. Only a profile the
/// server gave a line has one (<see cref="CloudSaveLine.FromGate"/>, CR-S51; never the tier, OD-140); everything else returns at once.
/// Before: a line at revision 0 is seeded (revision 1, the server's clean snapshot) and installed,
/// a newer cloud revision is installed, and anything uncertain (a conflict, a busy or unreadable
/// newest revision) stops the launch rather than play over it. After: the newest sealed revision
/// is sent with the session's lease; whatever the answer, nothing on this PC is deleted.
/// </summary>
public sealed class SessionSaves
{
    private readonly CloudSaves _saves;
    private readonly SavesApiClient _api;
    private readonly Func<CancellationToken, Task<string>> _access;
    private readonly string _savesRoot;
    private readonly ulong _xuid;
    private readonly Action<SaveStep, string?> _report;
    private CloudSaveLine? _line;

    public SessionSaves(CloudSaves saves, SavesApiClient api, Func<CancellationToken, Task<string>> access, string savesRoot, ulong xuid, Action<SaveStep, string?> report)
    {
        _saves = saves ?? throw new ArgumentNullException(nameof(saves));
        _api = api ?? throw new ArgumentNullException(nameof(api));
        _access = access ?? throw new ArgumentNullException(nameof(access));
        _savesRoot = savesRoot;
        _xuid = xuid;
        _report = report ?? throw new ArgumentNullException(nameof(report));
    }

    /// <summary>Asks the player how to settle a conflict; null keeps the old rule (the launch stops).</summary>
    public Func<SaveConflict, CancellationToken, Task<ConflictChoice>>? ResolveConflict { get; init; }

    /// <summary>The session holds the profile's line (a lease and a head).</summary>
    public bool HasLine => _line is not null;

    /// <summary>OD122-L3: the server's checkpoint cadence (<c>GET /info</c> <c>checkpoints</c>); null or disabled keeps the exit commit only.</summary>
    public CheckpointPolicy? Checkpoints { get; init; }

    /// <summary>The clock checkpoints are scheduled on (tests move it).</summary>
    public Func<DateTimeOffset> Clock { get; init; } = () => DateTimeOffset.UtcNow;

    private CheckpointScheduler? _scheduler;
    private DateTimeOffset _sessionStart;
    private bool _checkpointing;
    private (ulong Number, string Content)? _seen;

    /// <summary>Something runs while the game does: a fresh start's first save, or checkpoints (SAVES_API §4.4).</summary>
    public bool Watches => _line is not null && !_holdUploads && (AwaitsFreshStart || _checkpointing);

    /// <summary>The line's fence now (heartbeats may have moved it); null without a line.</summary>
    public ulong? FenceToken => _line?.Lease.FenceToken;

    /// <summary>The newest lease from the gate's heartbeat (the fence never goes back).</summary>
    public void OnLease(Lease? lease) => _line?.Lease.Adopt(lease);

    /// <summary>Before the game starts. Throws <see cref="SaveBlockedException"/> when it must not start.</summary>
    public async Task BeforeSpawnAsync(GateActive active, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(active);
        _line = CloudSaveLine.FromGate(active);
        if (_line is null)
        {
            // OD-133: a lapsed line still never plays a refused restore: the owed undo is paid from the backup (no head to fall back on)
            if (RestoreUndo.Owed(_saves, _savesRoot, _xuid) is not null) await PayUndoAsync(string.Empty, null, cancellation).ConfigureAwait(false);
            return; // free, Noob, or a lapsed Hardcore: no cloud line (OD-52)
        }
        // the pre-launch sync counts as the previous commit (SAVES_API §4.4); a relaunch under a new lease starts the schedule over
        _sessionStart = Clock();
        _checkpointing = Checkpoints is { Enabled: true };
        _scheduler = null;
        _seen = null;
        _freshStart = false;
        _freshSettled = false;
        _holdUploads = false;
        var access = await _access(cancellation).ConfigureAwait(false);
        var xuid = _xuid.ToString(CultureInfo.InvariantCulture);
        var prepared = CloudSaves.PrepareFolders(_savesRoot, _xuid);
        if (prepared.Busy.Count > 0)
        {
            throw new SaveBlockedException("SAVES.LOCAL_BUSY", "a save of this profile is still being written");
        }
        // OD-133: a restore whose upload is gone without being committed (an interruption) owes its undo; an owed undo is paid before
        // anything else, and while it cannot be, nothing of this profile is sent (the restored career plays on this PC only)
        RestoreUndo.Reconcile(_saves, _savesRoot, _xuid, Clock().UtcDateTime, Note);
        if (RestoreUndo.Owed(_saves, _savesRoot, _xuid) is not null)
        {
            await PayUndoAsync(access, _line.Head, cancellation).ConfigureAwait(false); // unpaid: the launch stops here
        }
        // CR-S51: a Hardcore line at revision 0 that starts "fresh_start" has no seed: Azimuth writes the first career, and its first
        // sealed revision is the line's revision 1. Anything this PC kept for the profile goes to the archive first, so the game starts
        // a new career; a pending upload for a line at 0 could never be taken, so it is set aside.
        var head = _line.Head;
        if (head.Revision == 0 && head.Start == SaveStarts.FreshStart && IsHardcore(active))
        {
            // the first career an earlier session recorded but could not send (SAVES_API §4.9), or the career the player chose to start
            // from at creation (CR-S52, §4.10: seed_from_profile, checked again on receipt), goes now, before anything is archived
            if (_saves.State(_xuid)?.PendingUpload is { Kind: CommitKinds.FreshStart or CommitKinds.SeedFromProfile, Expected: 0 } first)
            {
                var seed = first.Kind == CommitKinds.SeedFromProfile;
                if (seed) _report(SaveStep.Uploading, null);
                var sent = await _saves.SendPendingAsync(access, _xuid, _line.Lease, cancellation).ConfigureAwait(false);
                if (sent.Failure is { KeepsPending: true } kept)
                {
                    // the bytes stay recorded: a later Play sends them again (429 SAVES.RATE_LIMITED waits Retry-After)
                    throw new SaveBlockedException(kept.Code, seed ? "the career this Hardcore profile starts from is still waiting for the server's check"
                        : "the new Hardcore career's first save is still waiting to reach the cloud", kept.RetryAfter);
                }
                if (seed && sent.Failure is { Kind: SaveFailureKind.LeaseLost } lost)
                {
                    // the seed stays pending (CloudSaves keeps a seed on a lease loss): the next Play retries it without a new dry run
                    throw new SaveBlockedException(lost.Code, "another session holds this profile"); // never a fresh start under a lost lease
                }
                if (seed)
                {
                    // refused (422 GATE_REFUSED NOT_PLAUSIBLE_SEED, 409 source_used or the option off, 400/404 on the source): the bytes were
                    // set aside by the send, and the line, still at revision 0, starts a new career below (§4.10: fall back to fresh_start).
                    // Not sent (the recorded bytes missing), or a 2xx for another revision: the head read below decides; told as such.
                    if (sent is { Committed: { } committed, Failure: null }) _report(SaveStep.SeedAccepted, committed.Revision.ToString(CultureInfo.InvariantCulture));
                    else _report(SaveStep.SeedRefused, ProfileSeed.CommitDetail(sent));
                }
                else if (sent.Failure is { Kind: SaveFailureKind.GateRefused } firstRefused)
                {
                    RecordFirstSaveRefused(firstRefused); // OD-133: the next fresh start asks the player instead of archiving or resending
                }
                head = await FreshHeadAsync(access, head, cancellation).ConfigureAwait(false);
            }
            if (head.Revision == 0)
            {
                if (_saves.State(_xuid)?.PendingUpload is not null) _saves.SetAsidePending(_xuid); // nothing else is taken at revision 0
                if (!await FreshStartOnceAsync(cancellation).ConfigureAwait(false))
                {
                    // OD-133: the first save was refused as past its start and the player chose "not now": nothing is sent this session
                    _holdUploads = true;
                    _report(SaveStep.FreshStartHeld, null);
                    return;
                }
                _freshStart = true;
                _report(SaveStep.FreshStart, null);
                return;
            }
        }
        // A pending upload from an earlier session goes first, with this session's lease; then the head is read again, so the decision
        // is made on what the cloud holds now (never on the admission's head, which that upload may have moved).
        else if (_saves.State(_xuid)?.PendingUpload is { Restore: not null } restore)
        {
            await SendRestoreAsync(access, head, restore, cancellation).ConfigureAwait(false);
            head = await FreshHeadAsync(access, head, cancellation).ConfigureAwait(false);
        }
        else if (_saves.State(_xuid)?.PendingUpload is not null)
        {
            var sent = await _saves.SendPendingAsync(access, _xuid, _line.Lease, cancellation).ConfigureAwait(false);
            if (sent.Failure is { } f && f.KeepsPending)
            {
                _report(SaveStep.Kept, f.Code);
            }
            else if (sent.Failure is { Kind: SaveFailureKind.GateRefused } refused)
            {
                Note?.Invoke($"save: the pending upload was refused ({refused.Rule ?? refused.Reason ?? refused.Code}); parked by the server");
                _report(SaveStep.Refused, refused.Reason); // this PC's save is kept
            }
            head = await FreshHeadAsync(access, head, cancellation).ConfigureAwait(false);
        }
        if (head.Revision == 0 && active.LaunchTicket is not null && IsHardcore(active))
        {
            _report(SaveStep.Seeding, null);
            var key = Guid.NewGuid().ToString();
            SaveApiResult<SaveHead>? seeded = null;
            for (var attempt = 0; attempt < 3; ++attempt)
            {
                seeded = await _api.SeedAsync(access, xuid, key, _line.Lease.SessionId, _line.Lease.FenceToken, cancellation).ConfigureAwait(false);
                // 503 is "no seed yet" (SAVES_API.md §4.8): the player is told now, not after retries
                if (seeded.Ok || (int)seeded.Status < 500 || seeded.Status == System.Net.HttpStatusCode.ServiceUnavailable)
                {
                    break;
                }
                await Task.Delay(CommitBackoff.Delay(attempt, seeded.RetryAfter, 0.2), cancellation).ConfigureAwait(false);
            }
            if (seeded is not { Ok: true, Value: { } seededHead })
            {
                throw new SaveBlockedException(seeded?.Error?.Code ?? "SAVES.SEED_FAILED", seeded?.Error?.Message ?? "the Hardcore profile could not be created");
            }
            head = seededHead;
        }
        var decision = _saves.Decide(_savesRoot, _xuid, head);
        if (decision.Failure is { } failure)
        {
            throw new SaveBlockedException(failure.Code, failure.Message);
        }
        // retention (SAVES_API.md §4.1, §5): the head's file is gone; nothing comes down (no download, fast-forward, keep-cloud or
        // lapsed install): the game plays on this PC's save and the player is told the cloud copy expired
        if (head.IsCloudExpired() && decision.Decision is not (SyncDecision.InSync or SyncDecision.Empty))
        {
            _report(SaveStep.CloudExpired, null);
            return;
        }
        // SAVES_API.md §5: a renewed line (save.lapsedSince) takes the cloud's head whatever the decision; this PC's save stays only
        // as a backup (archived by the install), never uploaded or kept
        if (head.LapsedSince is not null && decision.Decision is SyncDecision.Upload or SyncDecision.FirstUpload or SyncDecision.Conflict)
        {
            await InstallCloudAsync(access, head, decision.LocalContent, "lapsed", cancellation).ConfigureAwait(false);
            return;
        }
        if (decision.Decision is SyncDecision.Conflict && ResolveConflict is { } ask)
        {
            var conflict = LocalConflict(head);
            switch (await ask(conflict, cancellation).ConfigureAwait(false))
            {
                case ConflictChoice.KeepCloud when conflict.CanKeepCloud:
                    await InstallCloudAsync(access, head, decision.LocalContent, "conflict", cancellation).ConfigureAwait(false);
                    return;
                case ConflictChoice.KeepThisPc:
                    await KeepThisPcAsync(access, head, cancellation).ConfigureAwait(false);
                    return;
            }
        }
        switch (decision.Decision)
        {
            case SyncDecision.Download or SyncDecision.FastForward:
                _report(SaveStep.Downloading, null);
                var outcome = await _saves.InstallHeadAsync(access, _savesRoot, _xuid, head, decision.LocalContent, "cloud", cancellation).ConfigureAwait(false);
                if (outcome.Failure is { } installFailure)
                {
                    throw new SaveBlockedException(installFailure.Code, installFailure.Message);
                }
                _report(SaveStep.Installed, head.Revision.ToString(CultureInfo.InvariantCulture));
                break;
            case SyncDecision.Conflict:
                // The PC and the cloud both moved on: never pick for the player here.
                throw new SaveBlockedException("SAVES.REVISION_CONFLICT", "this PC and the cloud both have newer progress");
            case SyncDecision.InSync or SyncDecision.Empty:
                // the cloud's head is this PC's save: it becomes the mark the session's commits build on. A state folder without one
                // (a new server origin, a reinstall) would otherwise send If-Match "0" and every commit would meet REVISION_CONFLICT.
                if (decision.Decision is SyncDecision.InSync && head.Revision > 0 && head.ContentSha256 is { } content
                    && _saves.State(_xuid)?.Mark?.Revision != head.Revision)
                    _saves.MarkInSync(_xuid, head.Revision, content);
                _report(SaveStep.InSync, null);
                break;
            default:
                // Upload / FirstUpload: this PC is ahead; the game plays on it and the exit sends it.
                break;
        }
    }

    /// <summary>The line's head as the server has it now (after a pending upload moved it); the launch stops when it cannot be read.</summary>
    private async Task<SaveHead> FreshHeadAsync(string access, SaveHead admitted, CancellationToken cancellation)
    {
        var (head, failure) = await _saves.HeadAsync(access, _xuid, cancellation).ConfigureAwait(false);
        if (head is null) throw new SaveBlockedException(failure?.Code ?? "SAVES.UNAVAILABLE", failure?.Message ?? "the cloud save could not be read");
        // the admission's lapse and start describe the line, not one revision: kept when the plain read leaves them out
        return head with { LapsedSince = head.LapsedSince ?? admitted.LapsedSince, Start = head.Start ?? admitted.Start };
    }

    /// <summary>The cloud's head installed as the newest local revision; every revision it shadows is archived under <paramref name="label"/>.</summary>
    private async Task InstallCloudAsync(string access, SaveHead head, string? localContent, string label, CancellationToken cancellation)
    {
        // the player (or the lapse rule) chose the cloud: an older upload still recorded for this PC is set aside, never sent over it
        if (_saves.State(_xuid)?.PendingUpload is not null) _saves.SetAsidePending(_xuid);
        _report(SaveStep.Downloading, null);
        var outcome = await _saves.InstallHeadAsync(access, _savesRoot, _xuid, head, localContent, label, cancellation).ConfigureAwait(false);
        if (outcome.Failure is { } installFailure)
        {
            throw new SaveBlockedException(installFailure.Code, installFailure.Message);
        }
        _report(SaveStep.Installed, head.Revision.ToString(CultureInfo.InvariantCulture));
    }

    /// <summary>The player kept this PC's save: its newest sealed revision goes on top of the cloud's head (<c>resolve_keep_local</c>, CAS on it).</summary>
    private async Task KeepThisPcAsync(string access, SaveHead head, CancellationToken cancellation)
    {
        var newest = AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(_savesRoot, _xuid))
            ?? throw new SaveBlockedException("SAVES.LOCAL_UNREADABLE", "this PC has no complete save to keep");
        _report(SaveStep.Uploading, null);
        var outcome = await _saves.UploadAsync(access, _xuid, _line!.Lease, newest, head.Revision, CommitKinds.ResolveKeepLocal, cancellation).ConfigureAwait(false);
        if (outcome.Failure is { } commitFailure)
        {
            throw new SaveBlockedException(commitFailure.Code, commitFailure.Message);
        }
        _report(SaveStep.Uploaded, outcome.Committed?.Revision.ToString(CultureInfo.InvariantCulture));
    }

    /// <summary>The conflict as the player sees it: the cloud's head and this PC's newest sealed revision (level, credits, when).</summary>
    private SaveConflict LocalConflict(SaveHead head)
    {
        var folder = AzimuthSaves.FolderFor(_xuid);
        SaveProfile? local = null;
        try { local = AzimuthSaves.List(_savesRoot).FirstOrDefault(s => string.Equals(s.Folder, folder, StringComparison.OrdinalIgnoreCase)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        return new SaveConflict(head, local?.Level, local?.Credits, local?.LastPlayedUtc);
    }

    private bool _freshStart;
    private volatile bool _freshSettled;
    private int _committing; // one fresh-start commit at a time (the watcher and the exit never overlap; the supervisor awaits the watcher)

    /// <summary>CR-S51: this session starts a Hardcore career whose first save is not in the cloud yet.</summary>
    public bool AwaitsFreshStart => _freshStart && !_freshSettled;

    /// <summary>Each save decision in words for the launcher's log (ids, revisions and folder names only).</summary>
    public Action<string>? Note { get; init; }

    /// <summary>
    /// OD-133: asked before the game starts when the line is still at revision 0 and its first save was refused as past a new career's
    /// start: start a new career (this one archived first) or not now (nothing archived, nothing sent this session). Null: not now.
    /// </summary>
    public Func<FreshStartRefusal, CancellationToken, Task<FreshStartChoice>>? ResolveRefusedFreshStart { get; init; }

    private bool _holdUploads;

    /// <summary>
    /// OD-133: a profile's career is moved aside for a fresh start at most once per cloud line. The first fresh start is recorded in the
    /// profile's sync state (<see cref="FreshStartRecord"/>) and, when there was nothing to move, by an empty
    /// <c>fresh-start-&lt;stamp&gt;-empty</c> marker folder too; a later launch with the line still at revision 0 keeps the profile's
    /// current career and lets its <c>fresh_start</c> upload proceed. A <c>fresh-start-*</c> folder without a record (a launcher before
    /// this rule, or a lost state file) counts as that one fresh start. When the recorded line's first save was refused by the server's
    /// gate (the career is past its start), the player is asked (<see cref="ResolveRefusedFreshStart"/>): only their "start a new career"
    /// archives the folder again. False when the fresh start does not happen this session (the player said not now).
    /// </summary>
    private async Task<bool> FreshStartOnceAsync(CancellationToken cancellation)
    {
        var now = Clock().UtcDateTime;
        var stamp = now.ToString("O", CultureInfo.InvariantCulture);
        if (_saves.State(_xuid)?.FreshStart is { RefusedAtUtc: not null } refused)
        {
            var choice = ResolveRefusedFreshStart is { } ask
                ? await ask(new FreshStartRefusal(refused.RefusedRule, refused.RefusedAtUtc), cancellation).ConfigureAwait(false)
                : FreshStartChoice.NotNow;
            Note?.Invoke($"save: the line's first save was refused ({refused.RefusedRule ?? "gate"}) at {refused.RefusedAtUtc}; the player chose {choice}");
            if (choice != FreshStartChoice.StartNewCareer) return false;
            var again = ArchiveForFreshStart(now);
            _saves.RecordFreshStart(_xuid, new FreshStartRecord(stamp, again)); // a new fresh start, the refusal cleared
            Note?.Invoke(again is null ? "save: new career: no career on this PC to move aside" : $"save: new career: the refused career moved to {again}");
            return true;
        }
        if (_saves.State(_xuid)?.FreshStart is { } done)
        {
            Note?.Invoke($"save: fresh start already set up for this line at {done.AtUtc} ({done.Archive ?? "nothing archived"}): the current career stays");
            return true;
        }
        var earlier = FreshStartArchives.Names(_savesRoot, _xuid);
        if (earlier.Count > 0)
        {
            var newest = earlier.OrderByDescending(n => FreshStartArchives.ArchivedAt(n) ?? DateTime.MinValue).ThenByDescending(n => n, StringComparer.Ordinal).First();
            _saves.RecordFreshStart(_xuid, new FreshStartRecord(stamp, FreshStartArchives.IsMarker(newest) ? null : newest));
            Note?.Invoke($"save: {earlier.Count} fresh-start folder(s) already on this PC ({newest}): counted as this line's fresh start; the current career stays");
            return true;
        }
        var moved = ArchiveForFreshStart(now);
        if (moved is null) MarkEmptyFreshStart(now);
        // recorded after the move: a crash in between leaves the archive (or the marker), which the rule above counts
        _saves.RecordFreshStart(_xuid, new FreshStartRecord(stamp, moved));
        Note?.Invoke(moved is null ? "save: fresh start: no career on this PC to move aside" : $"save: fresh start: the career on this PC moved to {moved}");
        return true;
    }

    /// <summary>OD-133: an empty <c>fresh-start-&lt;stamp&gt;-empty</c> folder, the record of a fresh start that had nothing to move, outside sync-state.json.</summary>
    private void MarkEmptyFreshStart(DateTime nowUtc)
    {
        var archive = FreshStartArchives.ArchiveRoot(_savesRoot, _xuid);
        var stamp = FreshStartArchives.Stamp(nowUtc);
        var marker = Path.Combine(archive, FreshStartArchives.Prefix + stamp + FreshStartArchives.MarkerSuffix);
        for (var n = 1; Directory.Exists(marker); n++) marker = Path.Combine(archive, $"{FreshStartArchives.Prefix}{stamp}-{n}{FreshStartArchives.MarkerSuffix}");
        Directory.CreateDirectory(marker);
    }

    /// <summary>OD-133: the server refused the line's first save; the next fresh start asks the player.</summary>
    private void RecordFirstSaveRefused(SaveFailure refused)
    {
        var rule = refused.Rule ?? refused.Reason ?? refused.Code;
        _saves.RecordFreshStartRefused(_xuid, rule, Clock().UtcDateTime);
        Note?.Invoke($"save: the line's first save was refused ({rule}); the next Play asks before archiving anything");
    }

    /// <summary>
    /// OD-133: a restored career's upload, sent first under this session's lease. Committed: settled. A retryable answer, a lost lease
    /// or bytes that cannot be read now keep it and stop the launch (a restored career never plays unsynced; the shell offers to set it
    /// aside). A conflict hands it to the conflict screen (the record cleared before the bytes are set aside, so a crash leaves the conflict
    /// path, not an undo). Every other answer (refused, unreadable, invalid, not eligible, digest or integrity mismatch), and recorded
    /// bytes that are gone, owe the undo before the bytes are set aside, then pay it.
    /// </summary>
    private async Task<bool> SendRestoreAsync(string access, SaveHead head, PendingUpload restore, CancellationToken cancellation)
    {
        switch (_saves.PendingBytesOf(_xuid))
        {
            case PendingBytesState.Unreadable:
                throw new SaveBlockedException("SAVES.LOCAL_BUSY", "the restored career's recorded bytes cannot be read right now");
            case PendingBytesState.Gone:
                Note?.Invoke($"save: the restored career's recorded bytes ({restore.CommitId}) are gone or changed: set aside");
                RestoreUndo.Owe(_saves, _savesRoot, _xuid, "LOCAL.INTEGRITY", Clock().UtcDateTime, Note); // owed before the set-aside
                _saves.SetAsidePending(_xuid);
                await PayUndoAsync(access, head, cancellation).ConfigureAwait(false);
                return true;
        }
        _report(SaveStep.Uploading, null);
        var sent = await _saves.SendPendingAsync(access, _xuid, _line!.Lease, beforeSetAside: failure =>
        {
            if (failure.Kind == SaveFailureKind.RevisionConflict) _saves.SetRestore(_xuid, null); // the conflict screen decides
            else RestoreUndo.Owe(_saves, _savesRoot, _xuid, Reason(failure), Clock().UtcDateTime, Note);
        }, cancellation).ConfigureAwait(false);
        switch (sent)
        {
            case { Committed: { } committed, Failure: null }:
                _saves.SetRestore(_xuid, null);
                Note?.Invoke($"save: the restored career is revision {committed.Revision}");
                _report(SaveStep.Uploaded, committed.Revision.ToString(CultureInfo.InvariantCulture));
                return true;
            case { Failure: { } kept, Pending: not null }:
                // retryable, the lease lost (kept as a seed is), or the bytes not read now: still recorded; the game does not start on it
                throw new SaveBlockedException(kept.Code, "the restored career is still waiting to reach the cloud", kept.RetryAfter);
            case { Failure: { Kind: SaveFailureKind.RevisionConflict } }:
                Note?.Invoke("save: the head moved since the restore was checked: the conflict screen decides");
                return true;
            case { Failure: { } failure }:
                Note?.Invoke($"save: the restored career was not committed ({failure.Code} {Reason(failure)})");
                await PayUndoAsync(access, head, cancellation).ConfigureAwait(false);
                return true;
            default:
                return true;
        }

        static string Reason(SaveFailure failure) => failure.Kind == SaveFailureKind.GateRefused ? failure.Rule ?? failure.Reason ?? failure.Code : failure.Code;
    }

    /// <summary>
    /// OD-133: pays the owed undo (<see cref="RestoreUndo.UndoAsync"/>) and tells the player. Unpaid: the game does not start on the
    /// refused career (<see cref="RestoreUndo.OwedCode"/>); the shell offers to try again or, when it cannot be paid, to keep the career.
    /// </summary>
    private async Task PayUndoAsync(string access, SaveHead? head, CancellationToken cancellation)
    {
        var owed = RestoreUndo.Owed(_saves, _savesRoot, _xuid);
        if (await RestoreUndo.UndoAsync(_saves, access, _savesRoot, _xuid, head, Clock().UtcDateTime, Note, cancellation).ConfigureAwait(false))
        {
            _report(SaveStep.RestoreRefused, owed?.OwedReason);
            return;
        }
        _report(SaveStep.RestoreUndoFailed, owed?.OwedReason);
        throw new SaveBlockedException(RestoreUndo.OwedCode, "this PC's save from before the restore could not be put back yet");
    }
    /// <summary>The profile's whole folder moves to <c>fi-launcher-archive\User_x\fresh-start-&lt;stamp&gt;</c> (same volume, nothing deleted); its name, or null when there was no folder.</summary>
    private string? ArchiveForFreshStart(DateTime nowUtc)
    {
        var profile = CloudSaveFiles.ProfileDir(_savesRoot, _xuid);
        if (!Directory.Exists(profile)) return null;
        var archive = FreshStartArchives.ArchiveRoot(_savesRoot, _xuid);
        Directory.CreateDirectory(archive);
        var stamp = FreshStartArchives.Stamp(nowUtc);
        var target = Path.Combine(archive, FreshStartArchives.Prefix + stamp);
        for (var n = 1; Directory.Exists(target); n++) target = Path.Combine(archive, $"{FreshStartArchives.Prefix}{stamp}-{n}");
        Directory.Move(profile, target);
        return Path.GetFileName(target);
    }
    /// <summary>
    /// CR-S51 while the game runs: Azimuth's first sealed revision goes up as <c>fresh_start</c> as soon as it appears (the server's
    /// ceilings are those of a new career, before the intro race's rewards, and Azimuth keeps only its two newest revisions). Ends when
    /// it is committed or refused, or with <paramref name="cancellation"/>.
    /// </summary>
    public async Task WatchAsync(TimeSpan every, CancellationToken cancellation)
    {
        var wait = every;
        while (Watches && !cancellation.IsCancellationRequested)
        {
            try { await Task.Delay(wait, cancellation).ConfigureAwait(false); }
            catch (OperationCanceledException) { return; }
            wait = every;
            try
            {
                if (!AwaitsFreshStart)
                {
                    await CheckpointAsync(cancellation).ConfigureAwait(false);
                    continue;
                }
                // the bytes are recorded the moment the first save is seen (before any token or request), so a later failure keeps them
                if (!CaptureFirstSave()) continue;
                var retry = await SendFirstSaveAsync(cancellation).ConfigureAwait(false);
                if (retry is { } later && later > every) wait = later;
            }
            catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { return; }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or HttpRequestException or ApiException
                or System.Text.Json.JsonException or SaveException or InvalidOperationException)
            {
                // one bad tick (a token refresh in a network blip, a file being written) never ends the watch
                _report(SaveStep.Kept, e.GetType().Name);
                wait = TimeSpan.FromSeconds(Math.Max(every.TotalSeconds, 15));
            }
        }
    }

    /// <summary>
    /// OD122-L3, SAVES_API §4.4 "During play": the newest sealed revision, only when its content changed and has settled, at the
    /// server's cadence (by default at least 300 s apart, at most 30 per session hour, two kept for the exit), as <c>upload</c> on top
    /// of the mark. A recorded upload that could not go yet goes first. A crash then loses at most one interval in the cloud.
    /// </summary>
    internal async Task CheckpointAsync(CancellationToken cancellation)
    {
        if (!_checkpointing || Checkpoints is not { } policy) return;
        var mark = _saves.State(_xuid)?.Mark;
        if (mark is null && _freshStart) { _checkpointing = false; return; } // a refused first career sends nothing more
        _scheduler ??= new CheckpointScheduler(policy, _sessionStart, mark?.Content);
        if (_scheduler.StoppedBecause is not null) { _checkpointing = false; return; }
        if (AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(_savesRoot, _xuid)) is not { } newest) return;
        // a revision's folder never changes once sealed: its digest is read once
        if (_seen?.Number != newest.Number) _seen = (newest.Number, Fh2Bundle.ContentDigest(CloudSaveFiles.ReadFiles(newest)));
        var content = _seen.Value.Content;
        var now = Clock();
        if (!_scheduler.ShouldCommit(now, content)) return;
        if (Interlocked.CompareExchange(ref _committing, 1, 0) != 0) return;
        try
        {
            var access = await _access(cancellation).ConfigureAwait(false);
            _scheduler.Sent(now);
            var sent = _saves.State(_xuid)?.PendingUpload is not null
                ? await _saves.SendPendingAsync(access, _xuid, _line!.Lease, cancellation).ConfigureAwait(false)
                : await _saves.UploadAsync(access, _xuid, _line!.Lease, newest, mark?.Revision ?? 0, CommitKinds.Upload, cancellation).ConfigureAwait(false);
            switch (sent)
            {
                case { Committed: { } committed }:
                    _scheduler.Committed(Clock(), _saves.State(_xuid)?.Mark?.Content ?? content);
                    _report(SaveStep.Uploaded, committed.Revision.ToString(CultureInfo.InvariantCulture));
                    break;
                case { Unchanged: true }:
                    _scheduler.Committed(Clock(), content);
                    break;
                case { Failure: { Kind: SaveFailureKind.GateRefused } refused }:
                    _scheduler.Refused(content); // parked by the server; a later revision is tried
                    _report(SaveStep.Refused, refused.Reason);
                    break;
                case { Failure: { Kind: SaveFailureKind.SummaryUnreadable or SaveFailureKind.BundleInvalid } unreadable }:
                    _scheduler.Refused(content);
                    _report(SaveStep.Kept, unreadable.Code);
                    break;
                case { Failure: { Kind: SaveFailureKind.Integrity or SaveFailureKind.DigestMismatch or SaveFailureKind.UploadPending } local }:
                    _report(SaveStep.Kept, local.Code); // a file read while it changed, or a check to redo: the next tick tries again
                    break;
                case { Failure: { Kind: SaveFailureKind.OnlineClosed or SaveFailureKind.SessionEnded } closed }:
                    // kept for the next launch, never retried automatically: no more checkpoints this session
                    _scheduler.Stop(closed.Code);
                    _checkpointing = false;
                    _report(SaveStep.Kept, closed.Code);
                    break;
                case { Failure: { KeepsPending: true } waiting }:
                    _report(SaveStep.Kept, waiting.Code); // recorded; sent again at the next checkpoint or at the exit
                    break;
                case { Failure: { } stop }:
                    // REVISION_CONFLICT, LEASE_LOST, NOT_ELIGIBLE…: no more checkpoints; the exit and the next launch decide on a fresh head
                    _scheduler.Stop(stop.Code);
                    _checkpointing = false;
                    _report(SaveStep.Kept, stop.Code);
                    break;
            }
        }
        finally
        {
            Volatile.Write(ref _committing, 0);
        }
    }

    /// <summary>The first sealed revision recorded as the pending <c>fresh_start</c> upload (once). False when there is none yet.</summary>
    private bool CaptureFirstSave()
    {
        if (_saves.State(_xuid)?.PendingUpload is { Kind: CommitKinds.FreshStart }) return true;
        if (AzimuthSaves.OldestSealed(CloudSaveFiles.ProfileDir(_savesRoot, _xuid)) is not { } first) return false;
        return _saves.RecordUpload(_xuid, first, 0, CommitKinds.FreshStart) is null;
    }

    /// <summary>
    /// Sends the recorded first save (<c>fresh_start</c>, expected 0). Settled once committed, refused by the server's check, or refused
    /// for good (the line moved, the lease is gone): nothing more is sent this session. A retryable answer keeps it pending and returns
    /// the wait the server asked for.
    /// </summary>
    private async Task<TimeSpan?> SendFirstSaveAsync(CancellationToken cancellation)
    {
        if (Interlocked.CompareExchange(ref _committing, 1, 0) != 0) return null;
        try
        {
            if (!AwaitsFreshStart) return null;
            var access = await _access(cancellation).ConfigureAwait(false);
            _report(SaveStep.Uploading, null);
            var sent = await _saves.SendPendingAsync(access, _xuid, _line!.Lease, cancellation).ConfigureAwait(false);
            switch (sent)
            {
                case { Committed: { } committed }:
                    _freshSettled = true;
                    _sessionStart = Clock(); // the first save is the previous commit of the checkpoints that follow (§4.4)
                    _report(SaveStep.Uploaded, committed.Revision.ToString(CultureInfo.InvariantCulture));
                    return null;
                case { Failure: { Kind: SaveFailureKind.GateRefused } refused }:
                    // NOT_FRESH_START: parked by the server for a moderator; nothing more is sent on this line this session, and the next
                    // fresh start asks the player before archiving anything (OD-133)
                    _freshSettled = true;
                    RecordFirstSaveRefused(refused);
                    _report(SaveStep.Refused, refused.Reason);
                    return null;
                case { Failure: { KeepsPending: true } waiting }:
                    _report(SaveStep.Kept, waiting.Code);
                    return waiting.RetryAfter;
                case { Failure: { } other }:
                    // permanent (the line moved, the lease is gone, not eligible): the bytes were set aside; never repeated every tick
                    _freshSettled = true;
                    _report(SaveStep.Kept, other.Code);
                    return null;
                default:
                    return null;
            }
        }
        finally
        {
            Volatile.Write(ref _committing, 0);
        }
    }

    /// <summary>After the game exited: the newest sealed revision, sent. The revision the cloud now has, or null.</summary>
    public async Task<ulong?> AfterExitAsync(CancellationToken cancellation)
    {
        if (_line is null)
        {
            return null;
        }
        if (_holdUploads)
        {
            Note?.Invoke("save: nothing sent: the line's first save was refused and the player chose not now");
            return null;
        }
        // CR-S51: a first career the watcher did not send yet goes now: the recorded first save, or else the oldest sealed revision on
        // disk (never the newest, which carries the intro race's rewards), with the same retries as any exit commit. Still pending after
        // that, it is sent before anything else at the next launch; a refused one sends nothing more.
        if (_freshStart)
        {
            for (var attempt = 0; attempt < 4 && AwaitsFreshStart && CaptureFirstSave(); ++attempt)
            {
                var retryAfter = await SendFirstSaveAsync(cancellation).ConfigureAwait(false);
                if (!AwaitsFreshStart) break;
                await Task.Delay(CommitBackoff.Delay(attempt, retryAfter, 0.2), cancellation).ConfigureAwait(false);
            }
            if (AwaitsFreshStart || _saves.State(_xuid)?.Mark is null) return null;
        }
        var access = await _access(cancellation).ConfigureAwait(false);
        CloudSaves.PrepareFolders(_savesRoot, _xuid);
        var newest = AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(_savesRoot, _xuid));
        var mark = _saves.State(_xuid)?.Mark;
        if (newest is null)
        {
            _report(SaveStep.Unchanged, null);
            return mark?.Revision;
        }
        _report(SaveStep.Uploading, null);
        CommitOutcome sent = new(null, false, false, null, null);
        for (var attempt = 0; attempt < 4; ++attempt)
        {
            var pending = _saves.State(_xuid)?.PendingUpload is not null;
            sent = pending
                ? await _saves.SendPendingAsync(access, _xuid, _line.Lease, cancellation).ConfigureAwait(false)
                : await _saves.UploadAsync(access, _xuid, _line.Lease, newest, mark?.Revision ?? 0, CommitKinds.Upload, cancellation).ConfigureAwait(false);
            if (pending && sent.Committed is not null)
            {
                // an earlier upload (a checkpoint cut off or kept) went first: the session's newest save goes on top of it now
                mark = _saves.State(_xuid)?.Mark;
                --attempt;
                continue;
            }
            if (sent.Failure is not { KeepsPending: true } f || f.Kind is SaveFailureKind.SessionEnded or SaveFailureKind.OnlineClosed)
            {
                break;
            }
            await Task.Delay(CommitBackoff.Delay(attempt, f.RetryAfter, 0.2), cancellation).ConfigureAwait(false);
            access = await _access(cancellation).ConfigureAwait(false);
        }
        switch (sent)
        {
            case { Unchanged: true }:
                _report(SaveStep.Unchanged, null);
                return mark?.Revision;
            case { Committed: { } committed }:
                _report(SaveStep.Uploaded, committed.Revision.ToString(CultureInfo.InvariantCulture));
                return committed.Revision;
            case { Failure: { Kind: SaveFailureKind.GateRefused } refused }:
                _report(SaveStep.Refused, refused.Reason);
                return null;
            case { Failure: { } other }:
                _report(SaveStep.Kept, other.Code);
                return null;
            default:
                return null;
        }
    }

    private static bool IsHardcore(GateActive active)
    {
        try
        {
            var claims = Convert.FromBase64String(active.LaunchTicket.Claims);
            using var json = System.Text.Json.JsonDocument.Parse(claims);
            return json.RootElement.TryGetProperty("mode", out var mode) && mode.GetString() == "hardcore";
        }
        catch (Exception e) when (e is FormatException or System.Text.Json.JsonException or InvalidOperationException)
        {
            return false;
        }
    }
}
