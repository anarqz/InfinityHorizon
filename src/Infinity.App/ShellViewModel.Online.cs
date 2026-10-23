// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Api;
using Infinity.Core.Online;
using Infinity.Core.Update;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    private readonly CancellationTokenSource _onlineStop = new();
    private CancellationTokenSource? _onlineCooldownRefresh;
    private TimeProvider _onlineClock = TimeProvider.System;
    private OnlineStatus? _onlineStatus;
    private OnlinePause? _onlinePause;
    private OnlineActivity? _playOnlineActivity;
    private bool _onlineChecking, _onlineStarted, _onlineSupport, _onlineViewing;
    private long _onlinePauseEpoch, _onlineRetryTimestamp;
    private long? _onlineConfirmedTimestamp;
    private TimeSpan _onlineRetryDelay;

    public bool OnlineServiceAllowsPlay => !_onlineChecking && _onlinePause is null && _onlineStatus?.State == "open";
    public bool CanStartOnlineAccount => OnlineServiceAllowsPlay && !Busy && !LauncherUpdateBusy;
    public bool OnlineServicePaused => _onlinePause is not null;
    public bool CanCheckOnlineStatus => !_onlineChecking && OnlineRetryRemaining <= TimeSpan.Zero;
    public bool ShowOnlineStatusPage => (!OnlineServiceAllowsPlay || _onlineViewing) && !Playing && !_onlineSupport && !(DevelopmentMods && ShowMods)
        && !_updateAwaitingCommit && _launcherUpdatePhase is not (UpdatePhase.Downloading or UpdatePhase.Verifying or UpdatePhase.Applying or UpdatePhase.RecoveryRequired);
    public bool ShowOnlineStatusBanner => !OnlineServiceAllowsPlay && !ShowOnlineStatusPage;
    public string OnlineServiceTitle => S[_onlinePause?.Reason switch
    { "budget" => "online.budget_title", "operator" => "online.operator_title", _ => "online.title" }];
    public string OnlineServiceMessage => S[_onlinePause?.Reason switch
    {
        "budget" => "online.budget", "operator" => "online.operator",
        _ => _onlineChecking ? "online.checking" : OnlineServiceAllowsPlay ? "online.open" : "online.unknown",
    }];
    public string OnlineRetryHint => OnlineRetryRemaining is { } remaining && remaining > TimeSpan.Zero
        ? S.Format("online.retry_wait", ("minutes", Math.Ceiling(remaining.TotalMinutes).ToString(CultureInfo.CurrentCulture))) : "";
    public string OnlineServiceHelp => S[OnlineServiceAllowsPlay ? "online.available_help" : "online.support_help"];

    private TimeSpan OnlineRetryRemaining => _onlineRetryDelay <= TimeSpan.Zero ? TimeSpan.Zero
        : _onlineRetryDelay - _onlineClock.GetElapsedTime(_onlineRetryTimestamp);

    public void OpenOnlineSupport() { _onlineSupport = true; _onlineViewing = false; _updateSupport = true; Changed(string.Empty); }
    public void OpenOnlineStatus() { _onlineSupport = false; _onlineViewing = true; Changed(string.Empty); }
    public void CloseOnlineStatus() { if (OnlineServiceAllowsPlay) { _onlineViewing = false; Changed(string.Empty); } }

    /// <summary>One startup request, then explicit user checks. The timer only refreshes the retry button.</summary>
    public async Task CheckOnlineStatusAsync(bool manual = true)
    {
        if (_onlineChecking || !CanCheckOnlineStatus || !manual && _onlineStarted) { Changed(string.Empty); return; }
        _onlineStarted = true;
        _onlineChecking = true;
        var epoch = _onlinePauseEpoch;
        Changed(string.Empty);
        try
        {
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token);
            bound.CancelAfter(TimeSpan.FromSeconds(10));
            Task<OnlineStatus>? request = null;
            PrepareOnlineStatusRequest(ref request, bound.Token);
            var status = await (request ?? ApiClient.OnlineStatusAsync(Http, _config.Server, ClientHeader, bound.Token));
            OnlineStatusRules.Validate(status);
            // A closure observed after this read began wins over an in-flight cached open reply.
            if (epoch != _onlinePauseEpoch) return;
            // Revisions diagnose the responding controller; edge and platform are not one ordered authority.
            if (status.State == "closed")
            {
                ObserveOnlinePause(OnlinePause.FromStatus(status));
                ExtendOnlineCooldown(TimeSpan.FromSeconds(status.RetryAfterSeconds));
            }
            else
            {
                _onlineStatus = status;
                _onlineConfirmedTimestamp = _onlineClock.GetTimestamp();
                _onlinePause = null;
                _onlineViewing = false;
                _onlineSupport = false;
                // An explicit open snapshot permits a NEW Play; it never resets an existing activity latch.
                ArmOnlineCooldown(TimeSpan.FromSeconds(status.RetryAfterSeconds));
            }
        }
        catch (ApiException error) when (OnlinePause.From(error) is { } pause)
        {
            ObserveOnlinePause(pause);
            ExtendOnlineCooldown(error.RetryAfter);
        }
        catch (Exception error) when (UpdateFailure(error))
        {
            _onlineStatus = null;
            ExtendOnlineCooldown(TimeSpan.FromSeconds(_onlinePause is null ? 5 : 300));
            Log("online status: unavailable (" + error.GetType().Name + ")");
        }
        finally { _onlineChecking = false; Changed(string.Empty); }
    }

    partial void PrepareOnlineStatusRequest(ref Task<OnlineStatus>? request, CancellationToken cancellation);

    private async Task<bool> RefreshOnlineServiceForPlayAsync()
    {
        if (!OnlineServiceAllowsPlay) return false;
        if (_onlineConfirmedTimestamp is not { } confirmed || _onlineClock.GetElapsedTime(confirmed) >= TimeSpan.FromSeconds(5))
            await CheckOnlineStatusAsync();
        if (!OnlineServiceAllowsPlay) { OpenOnlineStatus(); Message = OnlineServiceMessage; }
        return OnlineServiceAllowsPlay;
    }

    /// <summary>A terminal decision for the current session; it never cancels or kills the game.</summary>
    public void ObserveOnlinePause(OnlinePause pause)
    {
        ArgumentNullException.ThrowIfNull(pause);
        _onlinePauseEpoch++;
        _onlinePause = pause;
        _onlineStatus = null;
        _onlineSupport = false;
        _playOnlineActivity?.TryPause(pause);
        _dedicated?.Pause(pause);
        _integrity?.Revoke();
        _nativeFlow.Clear();
        _signIn?.Cancel();
        ExtendOnlineCooldown(TimeSpan.FromSeconds(300));
        Message = OnlineServiceMessage;
        if (Playing) SetStatus(OnlineServiceMessage);
        Changed(string.Empty);
    }

    private void ExtendOnlineCooldown(TimeSpan? retryAfter)
    {
        if (retryAfter is { } delay && delay > OnlineRetryRemaining) ArmOnlineCooldown(delay);
    }

    private void ArmOnlineCooldown(TimeSpan delay)
    {
        _onlineRetryTimestamp = _onlineClock.GetTimestamp();
        _onlineRetryDelay = delay;
        _onlineCooldownRefresh?.Cancel();
        _onlineCooldownRefresh?.Dispose();
        _onlineCooldownRefresh = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token);
        _ = RefreshOnlineRetryAsync(delay, _onlineCooldownRefresh.Token);
    }

    private async Task RefreshOnlineRetryAsync(TimeSpan delay, CancellationToken cancellation)
    {
        try
        {
            // Task.Delay has a platform timer bound; long server cooldowns remain enforced by the monotonic check.
            await Task.Delay(delay > TimeSpan.FromDays(1) ? TimeSpan.FromDays(1) : delay, _onlineClock, cancellation);
            if (!cancellation.IsCancellationRequested) Changed(string.Empty);
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { }
    }

    private void DisposeOnlineStatus()
    {
        _onlineStop.Cancel();
        _onlineCooldownRefresh?.Cancel();
        _onlineCooldownRefresh?.Dispose();
    }
}
