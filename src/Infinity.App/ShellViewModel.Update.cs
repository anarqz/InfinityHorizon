// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.Process;
using Infinity.Core.Update;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    private readonly CancellationTokenSource _updateStop = new();
    private CancellationTokenSource? _updateDownload;
    private UpdateCoordinator? _updater;
    private volatile UpdatePhase _launcherUpdatePhase = UpdatePhase.Checking;
    private volatile bool _updateSupport, _updateChecking, _updateRecoveryBlocked, _updateAwaitingCommit;
    private string? _updateFailureKey;
    private string? _updateTarget;
    private long _updateBytes, _updateTotal;
    public event Action? UpdateRestartRequested;

    public bool LauncherUpdateAllowsPlay => !_updateAwaitingCommit && !_updateRecoveryBlocked && _launcherUpdatePhase is UpdatePhase.Current or UpdatePhase.ReleaseUnavailable;
    public bool LauncherReleaseUnavailable => _launcherUpdatePhase == UpdatePhase.ReleaseUnavailable;
    public bool LauncherUpdateBusy => _updateAwaitingCommit || _updateChecking || _launcherUpdatePhase is UpdatePhase.Downloading or UpdatePhase.Verifying or UpdatePhase.Applying;
    public bool ShowLauncherUpdate => !ShowOnlineStatusPage && !LauncherUpdateAllowsPlay && !Playing && !_updateSupport;
    public bool ShowLauncherUpdateBanner => !ShowOnlineStatusBanner && !ShowOnlineStatusPage && (LauncherReleaseUnavailable && LauncherUpdateAllowsPlay || !LauncherUpdateAllowsPlay && !ShowLauncherUpdate);
    public string LauncherUpdateBannerAction => S[LauncherReleaseUnavailable ? "updater.retry" : "updater.open"];
    public bool CanRetryLauncherUpdate => !LauncherUpdateBusy && !Playing;
    public bool CanCancelLauncherUpdate => _updateDownload is not null && _launcherUpdatePhase is UpdatePhase.Downloading or UpdatePhase.Verifying;
    public bool CanApplyLauncherUpdate => _launcherUpdatePhase == UpdatePhase.Required && !LauncherUpdateBusy && !_updateRecoveryBlocked && !Playing && !IntegrityChecking && !_busy && CrashIdle;
    public string LauncherUpdateVersion => S.Format("updater.versions", ("current", Version), ("target", _updateTarget ?? "—"));
    public string LauncherUpdateStatus => _updateAwaitingCommit ? S["updater.applying"] : _updateFailureKey is { } failure ? S[failure] : S["updater." + (_launcherUpdatePhase switch
    {
        UpdatePhase.Checking => "checking", UpdatePhase.Current => "current", UpdatePhase.Required => "required",
        UpdatePhase.Downloading => "downloading", UpdatePhase.Verifying => "verifying", UpdatePhase.Applying => "applying",
        UpdatePhase.RecoveryRequired => "recovery", UpdatePhase.Failed => "failed", UpdatePhase.ReleaseUnavailable => "release_unavailable", _ => "unavailable",
    })];
    public string LauncherUpdateProgress => _updateTotal > 0 && _launcherUpdatePhase is UpdatePhase.Downloading or UpdatePhase.Verifying or UpdatePhase.Applying ? S.Format("updater.progress",
        ("received", (_updateBytes / 1048576d).ToString("F1", CultureInfo.CurrentCulture)),
        ("total", (_updateTotal / 1048576d).ToString("F1", CultureInfo.CurrentCulture))) : "";

    public void OpenLauncherUpdate() { _onlineSupport = true; _updateSupport = false; Changed(string.Empty); }
    public void CancelLauncherUpdate() { if (CanCancelLauncherUpdate) _updateDownload?.Cancel(); }
    public void OpenUpdateSupport() { if (_launcherUpdatePhase == UpdatePhase.Applying) return; _updateSupport = true; Changed(string.Empty); }

    public async Task CheckLauncherUpdatesAsync()
    {
        if (Playing) return;
        if (_updateRecoveryBlocked) { _launcherUpdatePhase = UpdatePhase.RecoveryRequired; Changed(string.Empty); return; }
        if (LauncherUpdateBusy && _launcherUpdatePhase != UpdatePhase.Checking) return;
        if (_updateChecking) return;
        _updateFailureKey = null;
        _updateChecking = true; _launcherUpdatePhase = UpdatePhase.Checking; Changed(string.Empty);
        _updater ??= new UpdateCoordinator(_stateDirectory);
        try
        {
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(_updateStop.Token);
            bound.CancelAfter(TimeSpan.FromSeconds(25));
            var infoTask = UpdateServerInfoAsync(bound.Token);
            var releaseTask = DiscoverLauncherReleaseAsync(bound.Token);
            await Task.WhenAll(infoTask, releaseTask);
            if (_updateRecoveryBlocked) return;
            var decision = _updater.Evaluate(Version, await infoTask, _gate.Game);
            (_launcherUpdatePhase, _updateTarget) = (decision.Phase, decision.TargetVersion);
        }
        catch (Exception error) when (UpdateFailure(error))
        { _launcherUpdatePhase = UpdatePhase.Unavailable; Log("update: check unavailable (" + error.GetType().Name + ")"); }
        finally { _updateChecking = false; Changed(string.Empty); }
    }

    private async Task DiscoverLauncherReleaseAsync(CancellationToken cancellation)
    {
        try { await _updater!.DiscoverAsync(cancellation); }
        catch (Exception error) when (UpdateFailure(error))
        { Log("update: release discovery unavailable (" + error.GetType().Name + ")"); }
    }

    private async Task<ServerInfo> UpdateServerInfoAsync(CancellationToken cancellation)
    {
        try
        {
            var (info, _) = await AccountSession.ConnectAsync(Http, _config, ClientHeader, cancellation);
            _serverReachable = true;
            ObserveRegistration(info);
            return info;
        }
        catch (ApiException error) when (Infinity.Core.Online.OnlinePause.From(error) is { } pause)
        { ObserveOnlinePause(pause); ExtendOnlineCooldown(error.RetryAfter); _serverReachable = true; throw; }
        catch { _serverReachable = false; throw; }
    }

    private async Task<bool> RefreshLauncherPolicyAsync()
    {
        if (_updateRecoveryBlocked) return false;
        try
        {
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(_updateStop.Token);
            bound.CancelAfter(TimeSpan.FromSeconds(10));
            var info = await UpdateServerInfoAsync(bound.Token);
            var decision = _updater?.Evaluate(Version, info, _gate.Game) ?? new(UpdatePhase.Unavailable, null);
            (_launcherUpdatePhase, _updateTarget) = (decision.Phase, decision.TargetVersion);
        }
        catch (Exception error) when (UpdateFailure(error)) { _launcherUpdatePhase = UpdatePhase.Unavailable; }
        if (!LauncherUpdateAllowsPlay) _updateSupport = false;
        Changed(string.Empty);
        return LauncherUpdateAllowsPlay;
    }

    public async Task ApplyLauncherUpdateAsync()
    {
        if (!CanApplyLauncherUpdate || _updater is null) return;
        UpdateApplyPlan? prepared = null;
        var handedOff = false;
        var acceptingProgress = true;
        using var download = CancellationTokenSource.CreateLinkedTokenSource(_updateStop.Token);
        _updateDownload = download;
        _launcherUpdatePhase = UpdatePhase.Downloading; Changed(string.Empty);
        try
        {
            if (UpdateInstaller.IsGameDirectory(AppContext.BaseDirectory))
            { _updateFailureKey = "updater.location"; _launcherUpdatePhase = UpdatePhase.Failed; Changed(string.Empty); return; }
            var arguments = new[] { "--environment", _config.Production ? "production" : "development", "--server", _config.Server.ToString(), "--portal", _config.Portal.ToString(), "--locale", S.Locale };
            var progress = new Progress<UpdateProgress>(value =>
            { if (!acceptingProgress) return; (_launcherUpdatePhase, _updateBytes, _updateTotal) = (value.Phase, value.ReceivedBytes, value.TotalBytes); Changed(string.Empty); });
            var plan = await Task.Run(() => _updater.StageAsync(AppContext.BaseDirectory, arguments, progress, download.Token), download.Token);
            acceptingProgress = false;
            prepared = plan;
            download.Token.ThrowIfCancellationRequested();
            if (Playing || _busy || !CrashIdle) throw new InvalidOperationException("Launcher is busy");
            var helper = Path.Combine(plan.StageRoot, "infinity-updater.exe");
            if (!ReleaseSignature.IsOfficialFile(helper)) throw new InvalidDataException("Updater signature");
            var transaction = Path.GetDirectoryName(plan.StageRoot)!;
            var worker = Path.Combine(transaction, "worker.exe");
            File.Copy(helper, worker, false); File.Copy(helper + ".sig", worker + ".sig", false);
            if (!ReleaseSignature.IsOfficialFile(worker)) throw new InvalidDataException("Updater copy signature");
            var planPath = UpdateInstaller.WritePlan(plan);
            download.Token.ThrowIfCancellationRequested();
            _updateDownload = null; Changed(string.Empty);
            var start = new ProcessStartInfo(worker) { UseShellExecute = false, CreateNoWindow = true, WorkingDirectory = transaction };
            start.ArgumentList.Add("--transaction"); start.ArgumentList.Add(planPath);
            using var process = Process.Start(start) ?? throw new IOException("Updater could not start");
            await UpdateInstaller.WaitForHelperReadyAsync(plan, process, TimeSpan.FromSeconds(10), _updateStop.Token);
            handedOff = true;
            _launcherUpdatePhase = UpdatePhase.Applying; Changed(string.Empty);
            UpdateRestartRequested?.Invoke();
        }
        catch (Exception error) when (UpdateFailure(error))
        { _launcherUpdatePhase = UpdatePhase.Failed; Log("update: apply unavailable (" + error.GetType().Name + ")"); Changed(string.Empty); }
        finally
        {
            acceptingProgress = false;
            _updateDownload = null;
            if (!handedOff && prepared is not null)
                try { UpdateInstaller.DiscardPreparedStage(prepared.InstallRoot, prepared.TransactionId); }
                catch (Exception error) when (UpdateFailure(error)) { Log("update: stage cleanup deferred (" + error.GetType().Name + ")"); }
            Changed(string.Empty);
        }
    }

    private void RequireLauncherUpdate()
    {
        _launcherUpdatePhase = UpdatePhase.Required; _updateSupport = false;
        _integrity?.Revoke(); Changed(string.Empty);
    }

    public async Task<bool> RecoverInterruptedUpdateAsync(string[] arguments)
    {
        // The just-installed process acknowledges after first render; its helper still owns recovery.
        if (Array.IndexOf(arguments, "--updated-transaction") >= 0) return false;
        if (!Directory.Exists(Path.Combine(AppContext.BaseDirectory, ".infinity-update"))) return false;
        try
        {
            var recovery = UpdateInstaller.FindPendingRecovery(AppContext.BaseDirectory);
            if (recovery is null)
            {
                // Housekeeping cannot turn a compatible read-only installation into a recovery incident.
                if (Directory.Exists(Path.Combine(AppContext.BaseDirectory, ".infinity-update")))
                    try
                    {
                        UpdateInstaller.ReclaimAbandonedStages(AppContext.BaseDirectory);
                        UpdateInstaller.PruneCompleted(AppContext.BaseDirectory);
                    }
                    catch (Exception error) when (UpdateFailure(error)) { Log("update: history cleanup deferred (" + error.GetType().Name + ")"); }
                return false;
            }
            _updateRecoveryBlocked = true; _launcherUpdatePhase = UpdatePhase.RecoveryRequired; Changed(string.Empty);
            if (!ReleaseSignature.IsOfficialFile(recovery.WorkerPath)) return true;
            var plan = UpdateInstaller.ReadPlan(recovery.PlanPath);
            using var parent = Process.GetCurrentProcess();
            plan = plan with { ParentProcessId = parent.Id, ParentStartTimeUtcTicks = parent.StartTime.ToUniversalTime().Ticks };
            var start = new ProcessStartInfo(recovery.WorkerPath) { UseShellExecute = false, CreateNoWindow = true };
            foreach (var value in new[] { "--recover", recovery.PlanPath, "--parent-pid", parent.Id.ToString(CultureInfo.InvariantCulture),
                "--parent-start-ticks", plan.ParentStartTimeUtcTicks.ToString(CultureInfo.InvariantCulture) }) start.ArgumentList.Add(value);
            using var worker = Process.Start(start) ?? throw new IOException("Recovery worker unavailable");
            await UpdateInstaller.WaitForHelperReadyAsync(plan, worker, TimeSpan.FromSeconds(10), _updateStop.Token);
            UpdateRestartRequested?.Invoke();
            return true;
        }
        catch (Exception error) when (UpdateFailure(error))
        {
            _updateRecoveryBlocked = true; _launcherUpdatePhase = UpdatePhase.RecoveryRequired;
            if (UpdateInstaller.IsGameDirectory(AppContext.BaseDirectory)) _updateFailureKey = "updater.location";
            Changed(string.Empty); return true;
        }
    }

    public async Task ConfirmUpdatedStartupAsync(string[] arguments)
    {
        var transaction = Array.IndexOf(arguments, "--updated-transaction");
        var nonce = Array.IndexOf(arguments, "--updated-nonce");
        if (transaction < 0) return;
        try
        {
            if (nonce < 0 || transaction + 1 >= arguments.Length || nonce + 1 >= arguments.Length) throw new InvalidDataException("Incomplete update handoff");
            _updateAwaitingCommit = true; Changed(string.Empty);
            var planPath = arguments[transaction + 1];
            var plan = UpdateInstaller.ReadPlan(planPath);
            var verifier = new UpdatePackageVerifier();
            var descriptor = verifier.VerifyPlanDescriptor(plan);
            if (Version != descriptor.Version) throw new InvalidDataException("Replacement version differs from release");
            await Task.Run(() => UpdateInstaller.AcknowledgeStartup(planPath, arguments[nonce + 1]), _updateStop.Token);
            await UpdateInstaller.WaitForCommitAsync(plan, TimeSpan.FromSeconds(40), _updateStop.Token);
            _updateAwaitingCommit = false;
            if (_launcherUpdatePhase != UpdatePhase.Current) await CheckLauncherUpdatesAsync();
        }
        catch (Exception error) when (UpdateFailure(error))
        { _updateAwaitingCommit = false; _updateRecoveryBlocked = true; _launcherUpdatePhase = UpdatePhase.RecoveryRequired; Changed(string.Empty); }
        Changed(string.Empty);
    }

    private static bool UpdateFailure(Exception error) => error is ApiException or HttpRequestException or IOException or InvalidDataException or UnauthorizedAccessException
        or JsonException or InvalidOperationException or ArgumentException or OperationCanceledException or TimeoutException or System.ComponentModel.Win32Exception;
}
