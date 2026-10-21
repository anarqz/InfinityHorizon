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

    private GameContentLocation? ContentLocation
    {
        get
        {
            if (!_contentLocationLoaded) { _contentLocation = GameContentLocation.Load(_stateDirectory); _contentLocationLoaded = true; }
            return _contentLocation;
        }
    }

    public bool IntegrityChecking => _integrityChecking;
    public bool CanCheckIntegrity => SignedIn && ContentLocation is not null && !Playing && !_busy && !_integrityChecking;
    public bool CanChooseContent => SignedIn && !Playing && !_busy && !_integrityChecking;
    public bool IntegrityReady => !_integrityChecking && !_integrityFailed && (_integrity is { State: InstallationState.Compatible, FilesCurrent: true } || CachedIntegrityReady);
    public bool IntegrityCanStart => !_integrityChecking && ContentLocation is not null && (_integrity is null || IntegrityReady);
    public bool ShowPlayBrand => ShowPlayAction;
    public string IntegrityLocationLabel => ContentLocation is { } location ? location.PackageRoot : S["integrity.content_unset"];
    public bool ShowIntegritySupport => !_integrityChecking && (_integrityFailed || _integrity is not null && !IntegrityReady);
    public string IntegrityCardStatus => S[_integrityChecking ? "configure.checking" : IntegrityReady ? "configure.ready"
        : ShowIntegritySupport ? "configure.attention" : "configure.pending"];
    public string IntegrityStatus => _integrityChecking ? S["integrity.checking"] : _integrityStatus ?? S[CachedIntegrityReady ? "integrity.cached" : "integrity.not_checked"];
    public string IntegrityFindings => _integrity is null ? "" : string.Join("\n", _integrity.Findings.Take(12)
        .Select(f => (f.Path.Length == 0 ? "" : f.Path + ": ") + S["integrity.reason." + f.Reason]));

    public async Task ChooseContentFolderAsync(string folder)
    {
        if (!CanChooseContent) return;
        VerificationCache.Invalidate();
        ClearIntegrity();
        var location = GameContentLocation.Detect(folder);
        if (location is null) { _configurationHasError = true; _integrityFailed = true; _integrityStatus = S["integrity.invalid_content"]; Changed(string.Empty); return; }
        try
        {
            location.Save(_stateDirectory);
            ClearIntegrity(); _contentLocation = location; _contentLocationLoaded = true;
            _integrityStatus = S["integrity.not_checked"];
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { _configurationHasError = true; _integrityFailed = true; _integrityStatus = S["integrity.local_failed"]; Changed(string.Empty); return; }
        Changed(string.Empty);
        await CheckIntegrityAsync();
    }

    private void ClearIntegrity()
    {
        _integrityStop?.Cancel();
        _integrity?.Dispose(); _integrity = null;
        _receiverReadiness.Disconnected();
        _configurationHasError = false;
        _configurationRuntimeIssue = false;
        if (_messageForConfiguration) Message = null;
        _integrityStatus = null;
        _integrityFailed = false;
    }

    public void CancelIntegrityCheck() => _integrityStop?.Cancel();

    public async Task CheckIntegrityAsync(string? futureSessionId = null)
    {
        if (!CanCheckIntegrity || _session is not { } session) return;
        VerificationCache.Invalidate();
        var install = _install;
        ClearIntegrity();
        if (ContentLocation is not { } location) { _integrityStatus = S["integrity.content_unset"]; Changed(string.Empty); return; }
        using var stop = new CancellationTokenSource(TimeSpan.FromMinutes(5));
        _integrityStop = stop; _integrityChecking = true; Changed(string.Empty);
        OnlineContentSession? check = null;
        try
        {
            check = await Task.Run(() => OnlineContentSession.CheckAsync(session.Api, ct => session.Tokens.AccessAsync(ct), location.Mount,
                futureSessionId, cancellation: stop.Token), stop.Token);
            if (stop.IsCancellationRequested || !ReferenceEquals(_session, session) || !ReferenceEquals(_install, install)) return;
            _integrity = check; check = null;
            _integrityFailed = _integrity.State != InstallationState.Compatible;
            if (!_integrityFailed && _integrity.Manifest is { } manifest && VerificationScope is { } scope)
                VerificationCache.Remember(scope, location.Mount, manifest.Files.Select(f => f.Path).ToArray());
            _integrityStatus = _integrity.Reason == "policy_unavailable" ? S["integrity.policy_unavailable"]
                : S[_integrity.State == InstallationState.Compatible ? "integrity.compatible"
                    : _integrity.State == InstallationState.Incompatible ? "integrity.incompatible" : "integrity.inconclusive"];
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or InvalidDataException or UnauthorizedAccessException or JsonException or ArgumentException or OperationCanceledException or ObjectDisposedException)
        {
            if (ReferenceEquals(_session, session) && ReferenceEquals(_install, install))
            {
                _integrityStatus = S[error is OperationCanceledException ? "integrity.cancelled" : "integrity.inconclusive"];
                _integrityFailed = error is not OperationCanceledException;
            }
        }
        finally
        {
            check?.Dispose();
            if (ReferenceEquals(_integrityStop, stop)) { _integrityStop = null; _integrityChecking = false; }
            Changed(string.Empty);
        }
    }

    public string CopyableIntegrityReport() => InstallationSupportReport.Format(_integrity?.State ?? InstallationState.Inconclusive,
        _integrity?.Reason ?? "not_checked", _integrity?.Findings ?? []);

    public void IntegrityCopyResult(bool success) => Message = S[success ? "integrity.copied" : "integrity.copy_failed"];

    private async Task<WorldAccessDecision> CheckWorldAccessAsync(OnlineContentSession? integrity, CancellationToken cancellation)
    {
        if (!LauncherUpdateAllowsPlay) return new(false, "updater.session_required");
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
