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
        // A pending upload from an earlier session goes first, with this session's lease.
        if (_saves.State(_xuid)?.PendingUpload is not null)
        {
            var sent = await _saves.SendPendingAsync(access, _xuid, _line.Lease, cancellation).ConfigureAwait(false);
            if (sent.Failure is { } f && f.KeepsPending)
            {
                _report(SaveStep.Kept, f.Code);
            }
        }
        var head = _line.Head;
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

    /// <summary>After the game exited: the newest sealed revision, sent. The revision the cloud now has, or null.</summary>
    public async Task<ulong?> AfterExitAsync(CancellationToken cancellation)
    {
        if (_line is null)
        {
            return null;
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
            if (sent.Failure is not { KeepsPending: true } f || f.Kind == SaveFailureKind.SessionEnded)
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
