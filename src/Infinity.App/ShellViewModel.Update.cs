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
    public bool ShowLauncherUpdate => !ShowOnlineStatusPage && !LauncherUpdateAllowsPlay && !Playing && !_updateSupport
        && !(DevelopmentMods && ShowMods && !_updateAwaitingCommit && !_updateRecoveryBlocked
            && _launcherUpdatePhase is not (UpdatePhase.Downloading or UpdatePhase.Verifying or UpdatePhase.Applying or UpdatePhase.RecoveryRequired));
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

    private string? _githubNewer;
    private bool _githubAdvisoryDismissed;

    /// <summary>
    /// OD-118 (T1b): offline (before Sign in), a newer signed release on GitHub is shown as advice with a link to the official release
    /// page. It comes from the GitHub discovery alone (the player's PC to GitHub), never blocks Play and sets no floor.
    /// </summary>
    public bool ShowGitHubAdvisory => !_onlineEngaged && _githubNewer is not null && !_githubAdvisoryDismissed && !Playing;

    public string GitHubAdvisoryText => _githubNewer is { } version ? S.Format("updater.github_newer", ("version", version)) : "";

    /// <summary>The GitHub discovery's result (its verified version, or null when none): the advice shows when it is newer.</summary>
    internal void ObserveDiscoveredRelease(string? discovered)
    {
        _githubNewer = LauncherAdvisory.NewerOnGitHub(Version, discovered);
        if (_githubNewer is not null) Log("update: a newer launcher is on GitHub (" + _githubNewer + "); advisory while offline");
        Changed(string.Empty);
    }

    public void DismissGitHubAdvisory() { _githubAdvisoryDismissed = true; Changed(string.Empty); }

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
            if (!_onlineEngaged || !OnlineServiceAllowsPlay)
            {
                // OD-118 (G1, T1c): before SIGN IN, or while the service is not open, only GitHub's release list is read; the release
                // floor (/launcher/v1/info) waits for Sign in and an open service.
                var read = await DiscoverLauncherReleaseAsync(bound.Token);
                if (_updateRecoveryBlocked) return;
                _launcherUpdatePhase = UpdatePhase.Current; _updateTarget = null;
                ObserveDiscoveredRelease(DiscoveredVersion);
                if (!read && !_onlineEngaged) _ = RetryGitHubDiscoveryAsync(); // T1e
                Log("update: release floor deferred (" + (_onlineEngaged ? "service not open" : "offline") + ")");
                return;
            }
            var infoTask = UpdateServerInfoAsync(bound.Token);
            var releaseTask = DiscoverLauncherReleaseAsync(bound.Token);
            await Task.WhenAll(infoTask, releaseTask);
            if (_updateRecoveryBlocked) return;
            var decision = _updater.Evaluate(Version, await infoTask, _gate.Game);
            (_launcherUpdatePhase, _updateTarget) = (decision.Phase, decision.TargetVersion);
        }
        catch (Exception error) when (UpdateFailure(error))
        { _launcherUpdatePhase = UpdatePhase.Unavailable; Log("update: check unavailable (" + error.GetType().Name + ")"); }
        finally { _updateChecking = false; KeepOfflineUpdatePhase(); Changed(string.Empty); }
    }

    /// <summary>
    /// OD-118 T1c: offline the release floor does not apply, so a server-side verdict (unavailable, required, checking) never gates the
    /// offline launcher, even one that arrives after the player chose Play offline. Local install states (download, apply, recovery) stay.
    /// </summary>
    private void KeepOfflineUpdatePhase()
    {
        if (!_onlineEngaged && !_updateAwaitingCommit && !_updateRecoveryBlocked
            && _launcherUpdatePhase is UpdatePhase.Unavailable or UpdatePhase.Required or UpdatePhase.ReleaseUnavailable or UpdatePhase.Checking)
        {
            _launcherUpdatePhase = UpdatePhase.Current; _updateTarget = null; _updateSupport = false;
        }
    }

    /// <summary>GitHub's signed release list (never an Infinity host): true when it was read.</summary>
    private async Task<bool> DiscoverLauncherReleaseAsync(CancellationToken cancellation)
    {
        try
        {
            if (DiscoveryForTests is { } fake) _discoveredForTests = await fake(cancellation);
            else await _updater!.DiscoverAsync(cancellation);
            return true;
        }
        catch (Exception error) when (UpdateFailure(error))
        { Log("update: release discovery unavailable: " + ErrorChain(error)); return false; }
    }

    /// <summary>Tests only: replaces the GitHub discovery (returns the discovered version, or throws as a failed read).</summary>
    internal Func<CancellationToken, Task<string?>>? DiscoveryForTests { get; set; }
    private string? _discoveredForTests;
    private string? DiscoveredVersion => DiscoveryForTests is not null ? _discoveredForTests : _updater?.DiscoveredVersion;

    /// <summary>OD-118 T1e: how long after a failed offline GitHub read the one retry waits (a first-run Wine TLS hiccup).</summary>
    internal TimeSpan GitHubRetryDelay { get; set; } = TimeSpan.FromSeconds(30);
    private bool _githubRetryScheduled;

    /// <summary>
    /// OD-118 T1e: one delayed retry of GitHub's release list after an offline read failed, so a transient failure does not hide the
    /// advice for the whole run. GitHub only (no Infinity request); skipped once the player chose SIGN IN (the online flow owns the
    /// release check then), while a game runs, or when the launcher closes.
    /// </summary>
    private async Task RetryGitHubDiscoveryAsync()
    {
        if (_githubRetryScheduled) return;
        _githubRetryScheduled = true;
        try { await Task.Delay(GitHubRetryDelay, _updateStop.Token); }
        catch (OperationCanceledException) { return; }
        if (_onlineEngaged || Playing || _updateChecking || _updater is null && DiscoveryForTests is null) { Log("update: GitHub retry skipped"); return; }
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(_updateStop.Token);
        bound.CancelAfter(TimeSpan.FromSeconds(25));
        Log("update: retrying GitHub's release list once");
        if (await DiscoverLauncherReleaseAsync(bound.Token) && !_onlineEngaged) ObserveDiscoveredRelease(DiscoveredVersion);
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
        { _launcherUpdatePhase = UpdatePhase.Failed; Log("update: apply unavailable (" + error.GetType().Name + ": " + error.Message + ")"); Changed(string.Empty); }
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
        _integrity?.Revoke();
        StopDedicated("lobby.update_required");
        Changed(string.Empty);
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
