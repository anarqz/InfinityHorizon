// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Integrity;
using Infinity.Core.Matchmaking;
using Infinity.Core.Session;
using Infinity.Core.Supervisor;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    private OnlineContentSession? _integrity;
    private CancellationTokenSource? _integrityStop;
    private readonly CancellationTokenSource _integrityLifetime = new();
    private bool _integrityRefreshPending;
    private readonly ReceiverReadinessLease _receiverReadiness = new();
    private GameContentLocation? _contentLocation;
    private bool _contentLocationLoaded, _integrityChecking, _integrityFailed;
    private bool? _lastReceiverReady;
    private string? _integrityStatus;
    private GameVerificationCache? _verificationCache;
    private GameVerificationCache VerificationCache => _verificationCache ??= new(_stateDirectory);
    private string? VerificationScope => _session?.Me?.AccountId is { } account && ContentLocation is { } location
        ? GameVerificationCache.Scope(_session.Api.Core.AbsoluteUri, _session.Api.Gate.AbsoluteUri, account, _gate.Game ?? "fh2", location.Mount) : null;
    private bool CachedIntegrityReady => VerificationScope is { } scope && ContentLocation is { } location && VerificationCache.IsCurrent(scope, location.Mount);
    private Task _integrityRun = Task.CompletedTask;

    /// <summary>OD-113: only a profile the server checks (a Hardcore cloud line) runs the integrity check; every other profile plays without it.</summary>
    public bool IntegrityApplies
    {
        // CR-S51: the server says per profile (contentRequired); a server before CR-S51 leaves it out, which means OD-52's rule
        // (that older server's own Supporter + Hardcore admission; CR-S69 keeps the fallback so Play matches what it enforces)
        get { var applies = LobbyProfile?.ContentRequired ?? IntegrityPolicy.Applies(_session?.Me?.Access.Tier, LobbyProfile?.Mode); InspectIntegrityApplies(ref applies); return applies; }
    }
    partial void InspectIntegrityApplies(ref bool applies);

    /// <summary>A real mismatch the server decided (an incompatible install): the only integrity state shown as a problem.</summary>
    private bool IntegrityMismatch => !_integrityChecking && _integrity?.State == InstallationState.Incompatible;

    /// <summary>Configure needs the player only for a Hardcore profile with no game folder or a real mismatch (Setup).</summary>
    private bool IntegrityConfigurationReady => !IntegrityPolicy.NeedsConfiguration(IntegrityApplies, ContentLocation is not null,
        IntegrityMismatch ? InstallationState.Incompatible : null);

    private GameContentLocation? ContentLocation
    {
        get
        {
            if (!_contentLocationLoaded) { _contentLocation = GameContentLocation.Load(_stateDirectory, StateRoot); _contentLocationLoaded = true; }
            return _contentLocation;
        }
    }

    public bool IntegrityChecking => _integrityChecking;
    public bool CanCheckIntegrity => SignedIn && IntegrityApplies && ContentLocation is not null && !Playing && !_busy && !_integrityChecking;
    /// <summary>The manual re-check is offered only where the check applies (OD-113); it is never required.</summary>
    public bool ShowIntegrityCheck => IntegrityApplies;
    public bool CanChooseContent => (InShell || DevelopmentMods) && !Playing && !_busy && !_integrityChecking;
    public bool IntegrityReady => !IntegrityApplies
        || !_integrityChecking && !_integrityFailed && (_integrity is { State: InstallationState.Compatible, FilesCurrent: true } || CachedIntegrityReady);
    // OD-113: a pending or running background check never blocks Play (Play waits for it); only a real mismatch does.
    public bool IntegrityCanStart => !IntegrityApplies || ContentLocation is not null && !IntegrityMismatch;
    public bool ShowPlayBrand => ShowPlayAction;
    public string IntegrityLocationLabel => ContentLocation is { } location ? location.PackageRoot : S["integrity.content_unset"];
    public bool ShowIntegritySupport => IntegrityApplies && !_integrityChecking && (_integrityFailed || _integrity is not null && !IntegrityReady);
    public string IntegrityCardStatus => S[_integrityChecking ? "configure.checking" : IntegrityReady ? "configure.ready"
        : ShowIntegritySupport ? "configure.attention" : "configure.pending"];
    public string IntegrityStatus => !IntegrityApplies ? S["integrity.not_required"] : _integrityChecking ? S["integrity.checking"]
        : _integrityStatus ?? S[CachedIntegrityReady ? "integrity.cached" : "integrity.not_checked"];
    public string IntegrityFindings => _integrity is null ? "" : string.Join("\n", _integrity.Findings.Take(12)
        .Select(f => (f.Path.Length == 0 ? "" : f.Path + ": ") + S["integrity.reason." + f.Reason]));

    public async Task ChooseContentFolderAsync(string folder)
    {
        if (!CanChooseContent) return;
        VerificationCache.Invalidate();
        ClearIntegrity();
        var location = GameContentLocation.Detect(folder);
        // Refused where the player is looking too (the message bar), not only on the Configure card.
        if (location is null) { _configurationHasError = true; _integrityFailed = true; _integrityStatus = S["integrity.invalid_content"]; Message = _integrityStatus; Changed(string.Empty); return; }
        try
        {
            location.Save(_stateDirectory, StateRoot);
            ClearIntegrity(); _contentLocation = location; _contentLocationLoaded = true;
            _integrityStatus = S["integrity.not_checked"];
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { _configurationHasError = true; _integrityFailed = true; _integrityStatus = S["integrity.local_failed"]; Changed(string.Empty); return; }
        RefreshMods(); Changed(string.Empty);
        if (SignedIn && IntegrityApplies) await CheckIntegrityAsync();
    }

    private void ClearIntegrity(bool preserveConfiguration = false)
    {
        if (_integrityChecking) _integrityRefreshPending = true;
        _integrityStop?.Cancel();
        _integrity?.Dispose(); _integrity = null;
        _receiverReadiness.Disconnected();
        if (!preserveConfiguration)
        {
            _configurationHasError = _configurationRuntimeIssue = _configurationErrorFromIntegrity = false;
            if (_messageForConfiguration) Message = null;
        }
        _integrityStatus = null;
        _integrityFailed = false;
    }

    public void CancelIntegrityCheck() => _integrityStop?.Cancel();

    /// <summary>The manual re-check (Configure): rehashes every listed file.</summary>
    public Task CheckIntegrityAsync() => RunIntegrityCheck(null, automatic: false);

    /// <summary>
    /// One event-driven background check (sign-in, profile selection, after a game exit); no timer/retry loop. Only a
    /// profile with the check (<see cref="IntegrityApplies"/>) and a selected folder is checked (OD-113); unchanged files reuse their digests.
    /// </summary>
    public Task RefreshIntegrityAutomaticallyAsync() => RunIntegrityCheck(null, automatic: true);

    private Task RunIntegrityCheck(string? futureSessionId, bool automatic)
    {
        var run = CheckIntegrityCoreAsync(futureSessionId, automatic);
        if (!run.IsCompleted) _integrityRun = run;
        return run;
    }

    /// <summary>
    /// Play's integrity step (OD-113), silent: <c>null</c> when this profile has no check; else the compatible check
    /// to admit with (a running background check is awaited; a current one reused; otherwise one runs now). A real
    /// mismatch is shown with its message; anything unverified stops Play with a message and is checked again next Play.
    /// </summary>
    private async Task<(bool Proceed, OnlineContentSession? Check)> IntegrityForPlayAsync(string futureSessionId)
    {
        if (!IntegrityApplies) return (true, null);
        await _integrityRun;
        // OD-115: the media overrides hold only files of enabled client-only mods, with their manifests' bytes (launcher-side,
        // CONTENT_API §1); checked fresh on every Hardcore Play, unchanged files' digests reused.
        if (await MediaOverrideFindingsAsync() is { Count: > 0 } overrides)
        {
            Message = MediaOverrideMessage(overrides); Changed(string.Empty); return (false, null);
        }
        if (IntegrityPolicy.BeforePlay(IntegrityApplies, _integrity?.State, _integrity?.FilesCurrent == true) == IntegrityStep.Check)
            await RunIntegrityCheck(futureSessionId, automatic: true);
        if (!IntegrityApplies) return (true, null);
        switch (IntegrityPolicy.After(_integrity?.State, _integrity?.FilesCurrent == true, _integrity?.CarDbSha256))
        {
            case IntegrityOutcome.Ready: return (true, _integrity);
            case IntegrityOutcome.Mismatch: Message = S["integrity.incompatible"]; Changed(string.Empty); return (false, null);
            default: Message = IntegrityStatus; Changed(string.Empty); return (false, null);
        }
    }

    /// <summary>OD-115: the Hardcore media-override findings (ids and paths only); an unreadable folder is itself a finding.</summary>
    private async Task<IReadOnlyList<Infinity.Core.Mods.MediaOverrideFinding>> MediaOverrideFindingsAsync()
    {
        if (ModManagerFactory() is not { } factory || ContentLocation is null) return [];
        try
        {
            var findings = await Task.Run(() => factory().AuditMediaOverrides());
            foreach (var finding in findings.Take(20)) Log("integrity: media override " + finding.Reason + ": " + finding.Path);
            return findings;
        }
        catch (Exception error) when (Infinity.Core.Mods.ManagedModFiles.IsStateError(error))
        {
            Log("integrity: media overrides unreadable: " + Infinity.Core.Mods.ManagedModFiles.Code(error));
            return [new(Infinity.Core.Mods.ModManifest.MediaOverrideFolder, "unreadable")];
        }
    }

    private string MediaOverrideMessage(IReadOnlyList<Infinity.Core.Mods.MediaOverrideFinding> findings) => S.Format("integrity.mediaoverride",
        ("paths", string.Join(", ", findings.Take(5).Select(f => f.Path)) + (findings.Count > 5 ? ", +" + (findings.Count - 5).ToString(System.Globalization.CultureInfo.CurrentCulture) : "")));

    /// <summary>A content refusal of the server during a session (OD-113): only a real mismatch is shown; anything else is re-checked silently.</summary>
    private void IntegrityRefused(string code, bool duringAdmission = false)
    {
        if (Playing) _integrity?.Revoke();
        if (IntegrityPolicy.IsMismatch(code))
        {
            VerificationCache.Invalidate();
            _integrityFailed = true;
            _integrityStatus = S["integrity.incompatible"];
            Message = _integrityStatus;
        }
        else
        {
            Log("integrity: " + code + ", checked again in the background");
            if (duringAdmission) Message = S["queue.failed"]; // as any admission error: Play says so, Configure is not involved
            if (!Playing) _ = RefreshIntegrityAutomaticallyAsync();
        }
        Changed(string.Empty);
    }

    private async Task CheckIntegrityCoreAsync(string? futureSessionId, bool automatic)
    {
        if (_integrityLifetime.IsCancellationRequested || _session is not { } session) return;
        if (!IntegrityApplies)
        {
            // OD-113: no check for this profile; a previous profile's result is not shown or used.
            if (_integrity is not null || _integrityFailed || _integrityStatus is not null) { ClearIntegrity(preserveConfiguration: !_configurationErrorFromIntegrity); Changed(string.Empty); }
            return;
        }
        if (automatic)
        {
            if (!SignedIn || !OnlineServiceAllowsPlay || HasDeletionPending || ContentLocation is null || Playing || _searching || _integrityChecking) return;
        }
        else if (!CanCheckIntegrity) return;
        // The manual re-check rehashes everything; a background or Play check reuses the digests of unchanged files.
        if (!automatic) VerificationCache.Invalidate();
        var known = automatic && VerificationScope is { } knownScope && ContentLocation is { } knownLocation
            ? VerificationCache.Known(knownScope, knownLocation.Mount) : [];
        var install = _install;
        ClearIntegrity(preserveConfiguration: automatic && !_configurationErrorFromIntegrity);
        if (ContentLocation is not { } location) { _integrityStatus = S["integrity.content_unset"]; Changed(string.Empty); return; }
        using var stop = CancellationTokenSource.CreateLinkedTokenSource(_integrityLifetime.Token);
        stop.CancelAfter(TimeSpan.FromMinutes(5));
        _integrityStop = stop; _integrityChecking = true; Changed(string.Empty);
        OnlineContentSession? check = null;
        try
        {
            check = await Task.Run(() => OnlineContentSession.CheckAsync(session.Api, ct => session.Tokens.AccessAsync(ct), location.Mount,
                known, futureSessionId, cancellation: stop.Token), stop.Token);
            if (stop.IsCancellationRequested || !ReferenceEquals(_session, session) || !ReferenceEquals(_install, install)) return;
            _integrity = check; check = null;
            _integrityFailed = _integrity.State != InstallationState.Compatible;
            if (!_integrityFailed && VerificationScope is { } scope)
                VerificationCache.Remember(scope, location.Mount, _integrity.Digests);
            else if (_integrity.State == InstallationState.Incompatible) VerificationCache.Invalidate();
            if (_integrity.ReusedDigests > 0) Log("integrity: " + _integrity.ReusedDigests + " unchanged file(s) not rehashed");
            _integrityStatus = _integrity.Reason == "policy_unavailable" ? S["integrity.policy_unavailable"]
                : S[_integrity.State == InstallationState.Compatible ? "integrity.compatible"
                    : _integrity.State == InstallationState.Incompatible ? "integrity.incompatible" : "integrity.inconclusive"];
            // OD-115: the background check also states unexpected media overrides before Play (Play checks them again).
            if (_integrity.State == InstallationState.Compatible && await MediaOverrideFindingsAsync() is { Count: > 0 } overrides)
                _integrityStatus = MediaOverrideMessage(overrides);
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or InvalidDataException or UnauthorizedAccessException or JsonException or ArgumentException or OperationCanceledException or ObjectDisposedException)
        {
            if (!_integrityLifetime.IsCancellationRequested && ReferenceEquals(_session, session) && ReferenceEquals(_install, install))
            {
                // Unverified now (offline, unreadable, cancelled): stated on the card, never a configuration error; checked again later.
                _integrityStatus = S[error is OperationCanceledException ? "integrity.cancelled" : "integrity.inconclusive"];
                _integrityFailed = error is not OperationCanceledException;
            }
        }
        finally
        {
            check?.Dispose();
            if (ReferenceEquals(_integrityStop, stop)) { _integrityStop = null; _integrityChecking = false; }
            Changed(string.Empty);
            if (_integrityRefreshPending)
            {
                _integrityRefreshPending = false;
                await RefreshIntegrityAutomaticallyAsync();
            }
        }
    }

    public string CopyableIntegrityReport() => InstallationSupportReport.Format(_integrity?.State ?? InstallationState.Inconclusive,
        _integrity?.Reason ?? "not_checked", _integrity?.Findings ?? []);

    public void IntegrityCopyResult(bool success) => Message = S[success ? "integrity.copied" : "integrity.copy_failed"];

    private async Task<WorldAccessDecision> CheckWorldAccessAsync(bool integrityApplies, OnlineContentSession? integrity, CancellationToken cancellation)
    {
        if (!LauncherUpdateAllowsPlay) return new(false, "updater.session_required");
        // OD-113: a session without the integrity check is not gated by it.
        if (!integrityApplies) return WorldAccessDecision.Compatible;
        if (integrity is null || !ReferenceEquals(_integrity, integrity))
            return new(false, "worlds.integrity_inconclusive");
        var allowed = await integrity.OnlineAllowedAsync(cancellation).ConfigureAwait(false);
        if (allowed && ReferenceEquals(_integrity, integrity)) return new(true, "");
        return new(false, integrity.State == InstallationState.Incompatible ? "worlds.integrity_incompatible" : "worlds.integrity_inconclusive");
    }

    private void IntegrityOn(SupervisorEvent value)
    {
        switch (value)
        {
            case SupervisorEvent.Handshake: _receiverReadiness.Connected(); _lastReceiverReady = null; break;
            case SupervisorEvent.Capture { What: CaptureWhat.RemotePaintReceiver } capture:
                if (capture.ReceivedTimestamp is { } stamp) _receiverReadiness.Observe(capture.Ok, stamp);
                else _receiverReadiness.Observe(capture.Ok);
                if (_lastReceiverReady != _receiverReadiness.Ready) { _lastReceiverReady = _receiverReadiness.Ready; Log("receiver: " + (_lastReceiverReady == true ? "ready" : "unavailable")); }
                break;
            case SupervisorEvent.Exited or SupervisorEvent.HookOffline or SupervisorEvent.Refused or SupervisorEvent.Ended: _receiverReadiness.Disconnected(); _lastReceiverReady = null; break;
        }
    }

}
