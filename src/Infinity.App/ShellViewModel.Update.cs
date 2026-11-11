// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.Process;
using Infinity.Core.Shell;
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

    public bool LauncherUpdateAllowsPlay => !_updateAwaitingCommit && !_updateRecoveryBlocked && (_launcherUpdatePhase is UpdatePhase.Current or UpdatePhase.ReleaseUnavailable || OfflineUpdateQuiet);

    /// <summary>
    /// OD-118 (T2b; SAURON saw it stick under --mods): offline, the server-side phases (checking, unavailable, required, release
    /// unavailable) show nothing, so no "checking … online compatibility" text appears before SIGN IN or while the check runs, and
    /// a check that never ran (the --mods route) never leaves it up. Local install states (download, apply, recovery) still show.
    /// </summary>
    private bool OfflineUpdateQuiet => !_onlineEngaged && !_updateAwaitingCommit && !_updateRecoveryBlocked
        && _launcherUpdatePhase is UpdatePhase.Checking or UpdatePhase.Unavailable or UpdatePhase.Required or UpdatePhase.ReleaseUnavailable;
    public bool LauncherReleaseUnavailable => _launcherUpdatePhase == UpdatePhase.ReleaseUnavailable;

    /// <summary>
    /// The release floor (<c>/launcher/v1/info</c>) could not be read at the last check: not a forced update (nothing says this launcher is
    /// too old). AUTO_UPDATE.md: a backend policy failure keeps Play restricted until the policy is readable; account operations, the
    /// sign-in included, stay available. The banner says so, with a retry, instead of the full update screen.
    /// </summary>
    public bool LauncherFloorUnread => _updateFloorUnread && _launcherUpdatePhase == UpdatePhase.Unavailable && _onlineEngaged
        && !_updateAwaitingCommit && !_updateRecoveryBlocked;
    private volatile bool _updateFloorUnread;

    public bool LauncherUpdateBusy => _updateAwaitingCommit || _updateChecking || _launcherUpdatePhase is UpdatePhase.Downloading or UpdatePhase.Verifying or UpdatePhase.Applying;
    public bool ShowLauncherUpdate => !ShowOnlineStatusPage && !LauncherUpdateAllowsPlay && !Playing && !_updateSupport && !LauncherFloorUnread
        && !(DevelopmentMods && ShowMods && !_updateAwaitingCommit && !_updateRecoveryBlocked
            && _launcherUpdatePhase is not (UpdatePhase.Downloading or UpdatePhase.Verifying or UpdatePhase.Applying or UpdatePhase.RecoveryRequired));
    public bool ShowLauncherUpdateBanner => !OfflineUpdateQuiet && !ShowOnlineStatusBanner && !ShowOnlineStatusPage && (LauncherReleaseUnavailable && LauncherUpdateAllowsPlay || !LauncherUpdateAllowsPlay && !ShowLauncherUpdate);
    public string LauncherUpdateBannerAction => S[LauncherReleaseUnavailable || LauncherFloorUnread ? "updater.retry" : "updater.open"];

    /// <summary>The banner's button: a read that failed (GitHub's list, or the floor) is tried again; any other state opens the update screen.</summary>
    public async Task LauncherUpdateBannerAsync()
    {
        if (LauncherReleaseUnavailable || LauncherFloorUnread) await CheckLauncherUpdatesAsync();
        else OpenLauncherUpdate();
    }
    public bool CanRetryLauncherUpdate => !LauncherUpdateBusy && !Playing;
    public bool CanCancelLauncherUpdate => _updateDownload is not null && _launcherUpdatePhase is UpdatePhase.Downloading or UpdatePhase.Verifying;
    public bool CanApplyLauncherUpdate => _launcherUpdatePhase == UpdatePhase.Required && !LauncherUpdateBusy && !_updateRecoveryBlocked && !Playing && !IntegrityChecking && !_busy && CrashIdle;
    public string LauncherUpdateVersion => S.Format("updater.versions", ("current", Version), ("target", _updateTarget ?? "—"));
    public string LauncherUpdateStatus => _updateAwaitingCommit ? S["updater.applying"] : _updateFailureKey is { } failure ? S[failure]
        : LauncherFloorUnread ? S["updater.floor_unread"] : S["updater." + (_launcherUpdatePhase switch
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
    /// page. It comes from the GitHub discovery alone (the player's PC to GitHub), never blocks Play and sets no floor. A development
    /// build (T-1111-AZIMUTH) is never forced to a stable release, so it shows the same advice signed in too.
    /// </summary>
    public bool ShowGitHubAdvisory => (!_onlineEngaged || _updater?.DevelopmentBuild == true) && _githubNewer is not null && !_githubAdvisoryDismissed && !Playing;

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
        _updateFloorUnread = false;
        _updateChecking = true; _launcherUpdatePhase = UpdatePhase.Checking; Changed(string.Empty);
        var run = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        _updateCheckRun = run.Task;
        _updater ??= new UpdateCoordinator(_stateDirectory) { Log = Log };
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
            var decision = UpdateDecisionForTests is { } fake ? fake(await infoTask) : _updater.Evaluate(Version, await infoTask, _gate.Game);
            (_launcherUpdatePhase, _updateTarget) = (decision.Phase, decision.TargetVersion);
            if (_updater.DevelopmentBuild) ObserveDiscoveredRelease(DiscoveredVersion); // a newer stable release: advice, never forced
        }
        catch (Exception error) when (UpdateFailure(error))
        {
            // the floor was not read: "couldn't check the launcher version" with a retry, never the forced update (LauncherFloorUnread)
            _launcherUpdatePhase = UpdatePhase.Unavailable; _updateFloorUnread = true;
            Log("update: check unavailable (" + error.GetType().Name + "); the release floor is unknown, Play waits for it");
        }
        finally
        {
            _updateChecking = false; KeepOfflineUpdatePhase(); Changed(string.Empty);
            run.TrySetResult();
            // every exit (the deferred GitHub-only read too): a stopped start continues once its screen cleared
            ContinueStoppedStartup();
        }
    }

    /// <summary>The check in flight, or the last one (completed): a start's floor step waits for it instead of reading "checking".</summary>
    private Task _updateCheckRun = Task.CompletedTask;

    /// <summary>Completes when the just-installed launcher's commit was awaited (<see cref="ConfirmUpdatedStartupAsync"/>).</summary>
    private TaskCompletionSource? _updateCommitWait;

    /// <summary>
    /// T-1111-UPDATE-RESUME: a resumed online start reads the floor itself (after the status), so the update confirmation's re-check
    /// stays out of it while that start runs, is about to run or is stopped (it continues through the floor step).
    /// </summary>
    private bool StartupOwnsFloor => _onlineEngaged && (_startupRunning || _startupStopped is not null);

    /// <summary>
    /// The start's floor step (T-1111-UPDATE-RESUME): an update's commit or a check already in flight (the confirmation's, a background
    /// one) is waited for first, then the floor is read by this start, with the service status it already read. Returns the verdict
    /// (<see cref="StartupFlow.LauncherVerdict"/>): null only when a new check or commit began meanwhile (the step runs again).
    /// </summary>
    private async Task<bool?> ReadStartupFloorAsync()
    {
        while (_updateAwaitingCommit && _updateCommitWait is { } commit) await commit.Task;
        while (_updateChecking) await _updateCheckRun;
        await CheckLauncherUpdatesAsync();
        return StartupFlow.LauncherVerdict(_launcherUpdatePhase, _updateChecking, _updateAwaitingCommit, LauncherUpdateAllowsPlay);
    }

    /// <summary>The just-installed launcher's commit is being awaited: Play and the floor wait for it.</summary>
    private void BeginAwaitingCommit()
    {
        _updateAwaitingCommit = true;
        _updateCommitWait = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        Changed(string.Empty);
    }

    /// <summary>
    /// The commit was awaited (or failed into recovery): a waiting start's floor step goes on. The re-check after a commit runs only when
    /// no online start owns the floor (<see cref="StartupOwnsFloor"/>): one that does reads it itself, after the service status.
    /// </summary>
    private async Task EndAwaitingCommitAsync(bool committed)
    {
        _updateAwaitingCommit = false;
        _updateCommitWait?.TrySetResult();
        _updateCommitWait = null;
        Changed(string.Empty);
        if (!committed) return;
        if (_launcherUpdatePhase != UpdatePhase.Current && !StartupOwnsFloor) await CheckLauncherUpdatesAsync();
        else ContinueStoppedStartup();
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

    /// <summary>Tests only: the release-floor decision for a synthetic /launcher/v1/info (no signed GitHub release in a check).</summary>
    internal Func<ServerInfo, UpdateDecision>? UpdateDecisionForTests { get; set; }

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
        catch (TrustChangedException change) { ObserveTrustChange(change); _serverReachable = true; throw; }
        catch { _serverReachable = false; throw; }
    }

    /// <summary>
    /// The floor of the /launcher/v1/info the sign-in read (the start's own read had failed): the release floor decides from it as the check
    /// would have, with GitHub's release list as already discovered; no request. A floor above this launcher is the forced update then.
    /// </summary>
    private void AdoptSignInFloor(ServerInfo info)
    {
        if (_updater is null || _updateChecking || !LauncherFloorUnread) return;
        try
        {
            var decision = UpdateDecisionForTests is { } fake ? fake(info) : _updater.Evaluate(Version, info, _gate.Game);
            (_launcherUpdatePhase, _updateTarget) = (decision.Phase, decision.TargetVersion);
            _updateFloorUnread = false;
            Log("update: release floor taken from the sign-in's /launcher/v1/info (" + decision.Phase + ")");
        }
        catch (Exception error) when (UpdateFailure(error)) { Log("update: the sign-in's floor not decided (" + error.GetType().Name + "); the banner keeps the retry"); }
        Changed(string.Empty);
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
            _updateFloorUnread = false;
        }
        catch (Exception error) when (UpdateFailure(error)) { _launcherUpdatePhase = UpdatePhase.Unavailable; _updateFloorUnread = true; }
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
            // CR-L39: a custom server is the saved choice (ServerChoice), which the restarted launcher reads again.
            var arguments = _config.Custom ? new[] { "--locale", S.Locale }
                : new[] { "--environment", _config.Production ? "production" : "development", "--server", _config.Server.ToString(), "--portal", _config.Portal.ToString(), "--locale", S.Locale };
            var progress = new ThrottledProgress(new Progress<UpdateProgress>(value =>
            { if (!acceptingProgress) return; (_launcherUpdatePhase, _updateBytes, _updateTotal) = (value.Phase, value.ReceivedBytes, value.TotalBytes); Changed(string.Empty); }));
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
            BeginAwaitingCommit();
            var planPath = arguments[transaction + 1];
            var plan = UpdateInstaller.ReadPlan(planPath);
            var verifier = new UpdatePackageVerifier();
            var descriptor = verifier.VerifyPlanDescriptor(plan);
            if (Version != descriptor.Version) throw new InvalidDataException("Replacement version differs from release");
            await Task.Run(() => UpdateInstaller.AcknowledgeStartup(planPath, arguments[nonce + 1]), _updateStop.Token);
            await UpdateInstaller.WaitForCommitAsync(plan, TimeSpan.FromSeconds(40), _updateStop.Token);
            await EndAwaitingCommitAsync(committed: true);
        }
        catch (Exception error) when (UpdateFailure(error))
        {
            _updateRecoveryBlocked = true; _launcherUpdatePhase = UpdatePhase.RecoveryRequired;
            await EndAwaitingCommitAsync(committed: false); // a start waiting at its floor step reads the recovery screen
        }
        Changed(string.Empty);
    }

    // TrustChangedException (CR-L39): the changed-keys question is already up; the update read simply did not happen
    private static bool UpdateFailure(Exception error) => error is ApiException or TrustChangedException or HttpRequestException or IOException or InvalidDataException or UnauthorizedAccessException
        or JsonException or InvalidOperationException or ArgumentException or OperationCanceledException or TimeoutException or System.ComponentModel.Win32Exception;

    /// <summary>
    /// The download and the verification report on every read (thousands per package), and each report refreshes the whole
    /// window: queued faster than it can draw, the window froze. Only a new phase, the last bytes of a phase or a quarter second
    /// since the last report pass on, so the UI never falls behind.
    /// </summary>
    private sealed class ThrottledProgress(IProgress<UpdateProgress> inner) : IProgress<UpdateProgress>
    {
        private readonly Lock _gate = new();
        private UpdatePhase? _phase;
        private long _last;

        public void Report(UpdateProgress value)
        {
            var now = Environment.TickCount64;
            lock (_gate)
            {
                if (value.Phase == _phase && value.ReceivedBytes < value.TotalBytes && now - _last < 250) return;
                (_phase, _last) = (value.Phase, now);
            }
            inner.Report(value);
        }
    }
}
