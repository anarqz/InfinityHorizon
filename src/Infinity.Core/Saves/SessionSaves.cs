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

/// <summary>What the player sees of a conflict: the cloud's head and this PC's newest sealed revision (each number null when unknown).</summary>
public sealed record SaveConflict(SaveHead Cloud, int? LocalLevel, int? LocalCredits, DateTime? LocalSavedUtc)
{
    /// <summary>An empty line (revision 0) has nothing to install: only this PC's save, or not now, can be offered.</summary>
    public bool CanKeepCloud => Cloud.Revision > 0;
}

/// <summary>A save step that stops the launch: the game is not started over an unknown state.</summary>
public sealed class SaveBlockedException(string code, string message) : Exception(message)
{
    public string Code { get; } = code;
}

/// <summary>
/// The cloud save around one Play (SAVES_API.md §3-§5, OD-52, OD-60), between the gate's admission
/// and <c>azimuth.exe</c>, and after its exit, before the gate's leave. Only a Supporter's Hardcore
/// profile has a line (<see cref="CloudSaveLine.FromGate"/>); everything else returns at once.
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
            return; // free, Noob, or a lapsed Hardcore: no cloud line (OD-52)
        }
        var access = await _access(cancellation).ConfigureAwait(false);
        var xuid = _xuid.ToString(CultureInfo.InvariantCulture);
        var prepared = CloudSaves.PrepareFolders(_savesRoot, _xuid);
        if (prepared.Busy.Count > 0)
        {
            throw new SaveBlockedException("SAVES.LOCAL_BUSY", "a save of this profile is still being written");
        }
        // CR-S51: a Hardcore line at revision 0 that starts "fresh_start" has no seed: Azimuth writes the first career, and its first
        // sealed revision is the line's revision 1. Anything this PC kept for the profile goes to the archive first, so the game starts
        // a new career; a pending upload for a line at 0 could never be taken, so it is set aside.
        var head = _line.Head;
        if (head.Revision == 0 && head.Start == SaveStarts.FreshStart && IsHardcore(active))
        {
            // the first career an earlier session recorded but could not send (SAVES_API §4.9) goes now, before anything is archived
            if (_saves.State(_xuid)?.PendingUpload is { Kind: CommitKinds.FreshStart, Expected: 0 })
            {
                var first = await _saves.SendPendingAsync(access, _xuid, _line.Lease, cancellation).ConfigureAwait(false);
                if (first.Failure is { KeepsPending: true } kept)
                {
                    throw new SaveBlockedException(kept.Code, "the new Hardcore career's first save is still waiting to reach the cloud");
                }
                head = await FreshHeadAsync(access, head, cancellation).ConfigureAwait(false);
            }
            if (head.Revision == 0)
            {
                if (_saves.State(_xuid)?.PendingUpload is not null) _saves.SetAsidePending(_xuid); // nothing else is taken at revision 0
                ArchiveForFreshStart();
                _freshStart = true;
                _report(SaveStep.FreshStart, null);
                return;
            }
        }
        // A pending upload from an earlier session goes first, with this session's lease; then the head is read again, so the decision
        // is made on what the cloud holds now (never on the admission's head, which that upload may have moved).
        else if (_saves.State(_xuid)?.PendingUpload is not null)
        {
            var sent = await _saves.SendPendingAsync(access, _xuid, _line.Lease, cancellation).ConfigureAwait(false);
            if (sent.Failure is { } f && f.KeepsPending)
            {
                _report(SaveStep.Kept, f.Code);
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

    /// <summary>The profile's whole folder moves to <c>fi-launcher-archive\User_x\fresh-start-&lt;stamp&gt;</c> (same volume, nothing deleted).</summary>
    private void ArchiveForFreshStart()
    {
        var profile = CloudSaveFiles.ProfileDir(_savesRoot, _xuid);
        if (!Directory.Exists(profile)) return;
        var archive = Path.Combine(_savesRoot, AzimuthSaves.ArchiveDir, AzimuthSaves.FolderFor(_xuid));
        Directory.CreateDirectory(archive);
        var target = Path.Combine(archive, "fresh-start-" + DateTime.UtcNow.ToString("yyyyMMdd'T'HHmmss'Z'", CultureInfo.InvariantCulture));
        for (var n = 1; Directory.Exists(target); n++) target = Path.Combine(archive, $"fresh-start-{DateTime.UtcNow:yyyyMMdd'T'HHmmss'Z'}-{n}");
        Directory.Move(profile, target);
    }

    /// <summary>
    /// CR-S51 while the game runs: Azimuth's first sealed revision goes up as <c>fresh_start</c> as soon as it appears (the server's
    /// ceilings are those of a new career, before the intro race's rewards, and Azimuth keeps only its two newest revisions). Ends when
    /// it is committed or refused, or with <paramref name="cancellation"/>.
    /// </summary>
    public async Task WatchFreshStartAsync(TimeSpan every, CancellationToken cancellation)
    {
        var wait = every;
        while (AwaitsFreshStart && !cancellation.IsCancellationRequested)
        {
            try { await Task.Delay(wait, cancellation).ConfigureAwait(false); }
            catch (OperationCanceledException) { return; }
            wait = every;
            try
            {
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
                    _report(SaveStep.Uploaded, committed.Revision.ToString(CultureInfo.InvariantCulture));
                    return null;
                case { Failure: { Kind: SaveFailureKind.GateRefused } refused }:
                    // NOT_FRESH_START: parked by the server for a moderator; nothing more is sent on this line this session
                    _freshSettled = true;
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
            sent = _saves.State(_xuid)?.PendingUpload is not null
                ? await _saves.SendPendingAsync(access, _xuid, _line.Lease, cancellation).ConfigureAwait(false)
                : await _saves.UploadAsync(access, _xuid, _line.Lease, newest, mark?.Revision ?? 0, CommitKinds.Upload, cancellation).ConfigureAwait(false);
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
