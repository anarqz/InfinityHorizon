// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Threading.Channels;
using Infinity.Core;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Session;
using Infinity.Core.Shell;
using Infinity.Core.Supervisor;
using Infinity.Core.Windows;
using Infinity.Core.Online;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>Play: the supervisor runs off the UI thread; its events come back through the UI context.</summary>
public sealed partial class ShellViewModel
{
    private readonly SynchronizationContext _ui = SynchronizationContext.Current ?? new SynchronizationContext();
    private AzimuthInstall? _install;
    private CancellationTokenSource? _play;
    private Channel<SupervisorCommand>? _commands;
    private Task? _playTask;
    /// <summary>
    /// OD-125, before every launch: FH2 added to Azimuth 1.0's <c>data\azimuth.ini</c> (so <c>--skip-launcher</c> starts it)
    /// with the game folder chosen in Configure, and ExtraOptions.Infinity's <c>FH2_ExtraOptions.ini</c> when there is none.
    /// A failure is logged and never stops the launch (without its ini ExtraOptions refuses; the hook still runs).
    /// OD-126: the mods of <c>mods\</c> are prepared for the profile (Hardcore online plays without them).
    /// </summary>
    private void PrepareAzimuth(AzimuthInstall? install, bool hardcore = false)
    {
        if (install is null) return;
        PrepareExtraOptionsMods(install, hardcore);
        try { Log("azimuth: " + AzimuthTitles.EnsureFh2(install.Root, ContentLocation?.PackageRoot)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("azimuth: title not written (" + e.GetType().Name + ")"); }
        try { Log("extraoptions: " + ExtraOptionsInfinity.EnsureIni(install.Root)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidOperationException) { Log("extraoptions: ini not written (" + e.GetType().Name + ")"); }
    }

    private string? _playStatus;
    private string? _hookLine;
    private bool _waitingForAdmission;
    private bool _queued;
    private bool _leavingQueue;
    private QueuePlace? _queuePlace;
    private string? _queueTier;
    private string? _admissionTitle;
    private string? _admissionDetail;

    public bool Queued => _queued;

    public bool WaitingForAdmission => _waitingForAdmission;

    public bool CanLeaveQueue => _waitingForAdmission && !_leavingQueue;

    public bool GameStarted => Playing && !_waitingForAdmission;

    public bool ShowPlayAction => !_waitingForAdmission;

    public string AdmissionTitle => _admissionTitle ?? S["queue.requesting"];

    public string AdmissionDetail => _admissionDetail ?? S["queue.connecting_detail"];

    public string QueueNumber => _queuePlace is { } place ? "#" + place.Position.ToString(CultureInfo.InvariantCulture) : string.Empty;

    public string QueueEstimate => _queuePlace is { } place ? QueueText.Estimate(S, place) : string.Empty;

    public string QueueAhead => _queuePlace is { } place ? S.Format("queue.ahead", ("n", place.Ahead.ToString(CultureInfo.InvariantCulture))) : string.Empty;

    public bool ShowPriorityHint => Queued && _queueTier == "free";

    public string LeaveQueueLabel => S[_leavingQueue ? "queue.leaving" : "queue.cancel"];

    public string PlayGuidance => SetupNeeded ? S["play.setup_needed"] : S[Playing ? "play.session_active" : "play.ready"];

    /// <summary>The detected Azimuth folder (<c>--azimuth</c>, <c>FI_AZIMUTH</c>), or null.</summary>
    public AzimuthInstall? Install
    {
        get => _install;
        set
        {
            if (value != _install) { ClearIntegrity(); _install = value; }
            if (value is not null)
            {
                SwitchCaches(value.Root, online: null);
                StartMods();
            }
            RefreshHook();
            Changed(string.Empty);
        }
    }

    public string InstallLabel => _install is null
        ? S["games.missing"] + " " + S["games.missing_hint"]
        : $"Azimuth {_install.Version ?? "?"} · " + (_install.HookVersion is { } hook ? S.Format("supervisor.hook", ("version", hook)) : S["supervisor.hook_unknown"]);

    public bool Playing => _play is not null;

    public bool CanPlay => Ready && !HasDeletionPending && _install is not null && _session is not null && !Playing && !_busy && CrashIdle && GameConfigurationReady && IntegrityCanStart && LauncherUpdateAllowsPlay && OnlineServiceAllowsPlay && !ModPlayBlocked;

    public string? PlayStatus => _playStatus;

    public string? HookLine => _hookLine;

    public async Task PlayAsync()
    {
        // OD-118: without an account (or with a profile not on it) Play is the offline launch.
        RefreshChosenLocal(); // OD-118: an upload since it was chosen makes it play online
        if (!PlaysOnline) { await PlayOfflineAsync(); return; }
        _modLaunchFailure = null;
        if (!OnlineServiceAllowsPlay) { OpenOnlineStatus(); Message = OnlineServiceMessage; return; }
        if (!LauncherUpdateAllowsPlay) { _updateSupport = false; Message = LauncherUpdateStatus; Changed(string.Empty); return; }
        RefreshHook(); // the hook as it is now, not as it was when the screen last looked
        if (!GameConfigurationReady && !Playing)
        {
            Message = S["play.setup_needed"];
            return;
        }
        if (!CanPlay || _session is null || _install is null || !OperatingSystem.IsWindows())
        {
            Message = Playing ? S["supervisor.already"] : S["supervisor.not_ready"];
            return;
        }
        // CR-S52: the first Play of a Hardcore profile whose cloud line is still empty asks how it starts (scratch or a checked career)
        if (_session.Me?.Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey && p.Game == _gate.Game) is { } chosen
            && await OfferSeedBeforePlayAsync(chosen)) return;
        if (_session is null || !CanPlay) return;
        var startingSession = _session;
        // §1.1: mod state never blocks ordinary Play by itself. Locks and Wine overrides are applied off the UI
        // thread; an unreadable state is logged and shown, and only stops a launch that has enabled mods to prepare.
        ModLaunchPreparation mods;
        try { mods = await PrepareModsForLaunchAsync(hardcore: LobbyProfile?.Mode == "hardcore"); }
        catch (ModHardcoreException error)
        {
            // OD-112 follow-up: refused before anything is written; the Mods page offers Disable.
            Message = HardcoreRefusal(error.Mods.Select(m => m.Text("name", S.Locale)));
            await RefreshModsAsync(); return;
        }
        catch (ModException error) when (error.Code == ModErrors.Recovery)
        { Message = S["mods.play_blocked"]; await RefreshModsAsync(); return; }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        {
            if (_modHealth.AnyEnabled) { Message = ModLaunchFailed("launch preparation", error); await RefreshModsAsync(); return; }
            Log("mods: launch preparation failed: " + error.GetType().Name + (error is ModException code ? " " + code.Code : "") + ": " + error.Message);
            mods = ModLaunchPreparation.None;
        }
        // OD-113: native mods play online (modded lobbies only) when the signed runtime and a hook that hosts it are present.
        ModRuntimePackage? nativeRuntime = null;
        if (mods.HasEnabledNative)
        {
            var (runtime, refusal) = await NativeModsForOnlineAsync(mods.HasEnabledDataSql);
            if (refusal is not null) { Message = refusal; return; }
            nativeRuntime = runtime;
        }
        if (!ReferenceEquals(_session, startingSession) || !CanPlay) return;
        var startingInstall = _install;
        var startingGame = _gate.Game;
        if (!await RefreshOnlineServiceForPlayAsync()) return;
        if (!ReferenceEquals(_session, startingSession) || !ReferenceEquals(_install, startingInstall) || _gate.Game != startingGame || !CanPlay) return;
        if (!await RefreshLauncherPolicyAsync()) return;
        if (!ReferenceEquals(_session, startingSession) || !CanPlay) return;
        var futureSessionId = Guid.CreateVersion7().ToString();
        // OD-113: only a Supporter + Hardcore profile is checked, silently; any other declares no car database.
        var (integrityProceed, integrity) = await IntegrityForPlayAsync(futureSessionId);
        if (!integrityProceed || !ReferenceEquals(_session, startingSession) || !ReferenceEquals(_install, startingInstall) || _gate.Game != startingGame
            || !CanPlay) return;
        var profile = _session.Me?.Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey && p.Game == _gate.Game);
        if (profile is null)
        {
            return;
        }
        var carDb = integrity?.CarDbSha256 ?? string.Empty;
        var development = DevServer;
        var (keys, policy) = _config.Production
            ? (Infinity.Core.Auth.ProductionDeployment.LaunchKeys(), TicketPolicy.Default with { Production = true })
            // CR-L39: the keys this session's validated /info published and the realm's pin matched.
            : _config.Custom ? (TicketKeys.FromTrust(_session.Info.Trust!), TicketPolicy.ForCustom)
            : TicketKeys.Trust(development, Environment.GetEnvironmentVariable(TicketKeys.DevTicketKeyEnv));
        // OD-112: the session's lobby mod set, built off the UI thread from the enabled managed mods, each one's mapped files
        // checked against its manifest (OD-113: data mods enter it from this verified plan); the gate's `modded` flag and the
        // overlay's bridge share it.
        var launchModSet = await Infinity.Core.Matchmaking.LaunchPlanModSetSource.FromManagedModsAsync(ManagedMods);
        if (launchModSet.State.Phase == Infinity.Core.Matchmaking.ModSetPhase.Failed)
        {
            Log("mods: lobby mod set unavailable (" + launchModSet.State.Reason + ")");
            // never online as vanilla with multiplayer mods enabled: the session would be admitted unmodded
            if (mods.MultiplayerMods.Count > 0) { Message = S["mods.online_unverified"]; await RefreshModsAsync(); return; }
        }
        if (!ReferenceEquals(_session, startingSession) || !CanPlay) return;
        // N-H2/OD-113: a launch with native mods carries a FIMOD003 plan (this launch's nonce) to the hook and uses the runtime
        // source: ready only on a verified ModsReport for that nonce and digest; file mods in it are ready from the verified plan.
        NativeModPlan? nativePlan = null;
        if (nativeRuntime is not null)
        {
            nativePlan = await PrepareOnlineNativePlanAsync(nativeRuntime);
            if (nativePlan is null) return;
            if (!ReferenceEquals(_session, startingSession) || !CanPlay) { await EndNativeModSessionAsync(nativePlan); return; }
        }
        var modSet = Infinity.Core.Matchmaking.ModSets.ForLaunch(launchModSet, nativePlan);
        using var onlineActivity = new OnlineActivity();
        PrepareAzimuth(_install, ProfileHardcore);
        var config = new SupervisorConfig(profile.Game, profile.Xuid, _install, Version, Environment.ProcessPath ?? "infinity.exe", keys, policy, _stateDirectory)
        {
            Overlay = LoadOverlayPackage(),
            NativeDllOverrides = mods.DllOverrides,
            ModEnvironment = nativePlan?.Environment ?? [],
            FallbackIdentity = FallbackIdentityFor(profile),
            Saves = SessionSavesFor(profile),
            Worlds = WorldsFor(profile.Mode, profile.Xuid, onlineActivity, modSet),
            Mods = modSet,
            OnlineActivity = onlineActivity,
            Text = S,
            CrashReports = new Infinity.Core.Diagnostics.CrashIncidentStore(_stateDirectory),
            AccountId = _session.Me?.AccountId,
            ConnectionEndpoints = "Core: " + _session.Api.Core.GetComponents(UriComponents.SchemeAndServer, UriFormat.UriEscaped)
                + "\nGate: " + _session.Api.Gate.GetComponents(UriComponents.SchemeAndServer, UriFormat.UriEscaped)
                + "\nMatchmaking: " + (Uri.TryCreate(_session.Info.Services?.Matchmaking, UriKind.Absolute, out var matchmakingOrigin)
                    ? matchmakingOrigin.GetComponents(UriComponents.SchemeAndServer, UriFormat.UriEscaped) : S["connection.unavailable"]),
            CarDbSha256 = carDb,
            InitialSessionId = futureSessionId,
            OnlineAccessLost = integrity is null ? null : integrity.Revoke,
            PrepareAdmission = integrity is null ? null : async (id, ct) =>
            {
                if (!ReferenceEquals(_integrity, integrity)) throw new InvalidDataException("Content check changed");
                await integrity.PrepareAdmissionAsync(id, ct).ConfigureAwait(false);
                if (!ReferenceEquals(_integrity, integrity)) throw new InvalidDataException("Content check changed");
            },
        };
        if (nativePlan is not null) config = config with { MinimumOnlineHookBuild = Math.Max(config.MinimumOnlineHookBuild,
            mods.HasEnabledDataSql ? DataModSqlHookBuild : NativeModOnlineHookBuild) };
        _commands = Channel.CreateUnbounded<SupervisorCommand>();
        _play = new CancellationTokenSource();
        _waitingForAdmission = true;
        _queued = _leavingQueue = false;
        _queuePlace = null;
        _admissionTitle = _admissionDetail = null;
        _hookLine = null;
        ClearRuntimeConfigurationIssue();
        SetStatus(S["queue.requesting"]);
        var supervisor = new PlaySupervisor(config, new GateClient(_session.Api, _session.Tokens), new WindowsSpawner(),
            handoff => new WindowsPipe(PipeServer.Create(handoff.PipeName)),
            e => _ui.Post(_ => Render(e), null), _commands.Reader,
            uiPipes: handoff => new WindowsPipe(PipeServer.Create(handoff.UiPipeName)));
        // BUG-0001: the online session's drivatar caches never reach offline play.
        var caches = SwitchCaches(_install.Root, online: true);
        _playOnlineActivity = onlineActivity;
        _playModSet = modSet;
        try
        {
            _playTask = Task.Run(() => supervisor.RunAsync(_play.Token));
            await _playTask;
        }
        finally
        {
            // OD-113: the session database restore runs after the game exited, as offline, before Play can start again
            if (nativePlan is not null) await EndNativeModSessionAsync(nativePlan);
            if (caches)
            {
                SwitchCaches(_install.Root, online: false);
            }
            Changed(nameof(ProfileRows));
            _play.Dispose();
            _play = null;
            _waitingForAdmission = _queued = _leavingQueue = false;
            RefreshHook();
            _commands = null;
            if (ReferenceEquals(_playOnlineActivity, onlineActivity)) _playOnlineActivity = null;
            _playWorlds = null;
            _playModSet = null;
            RefreshMods();
            Changed(string.Empty);
            if (ReferenceEquals(_session, startingSession) && ReferenceEquals(_install, startingInstall))
                await RefreshIntegrityAutomaticallyAsync();
        }
    }

    /// <summary>
    /// <see cref="ModeCaches"/> around an online session (and its recovery when the install is
    /// found). Only with no azimuth.exe running: a running game would be reading the caches. A
    /// failure is logged and play goes on; the next start's recovery finishes the switch.
    /// </summary>
    private bool SwitchCaches(string root, bool? online)
    {
        if (System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0)
        {
            Log("mode caches: azimuth.exe is running, left as they are");
            return false;
        }
        try
        {
            var caches = ModeCaches.Fh2(root);
            switch (online)
            {
                case true:
                    caches.BeginOnline();
                    break;
                case false:
                    caches.EndOnline();
                    break;
                default:
                    if (caches.Recover())
                    {
                        Log("mode caches: an interrupted online session's caches were put away");
                    }
                    break;
            }
            return true;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log("mode caches: " + e.Message);
            return false;
        }
    }

    /// <summary>The player confirmed "Close the game".</summary>
    public void QuitGame()
    {
        if (_offlineChild is not null) { QuitOfflineGame(); return; } // OD-118: no supervisor offline
        if (_waitingForAdmission) return;
        if (_commands?.Writer.TryWrite(new SupervisorCommand.Quit()) == true)
        {
            SetStatus(S["supervisor.quit_asked"]);
        }
    }

    /// <summary>Cancel the admission attempt; GateAdmission sends one bounded leave before ending.</summary>
    public void LeaveQueue()
    {
        if (!CanLeaveQueue) return;
        _leavingQueue = true;
        StopPlay();
        Changed(string.Empty);
    }

    /// <summary><c>&lt;state&gt;\launcher.log</c>: ids, phases and codes only, never a token.</summary>
    partial void SuppressLogging(ref bool suppressed);

    private void Log(string line)
    {
        var suppressed = false;
        SuppressLogging(ref suppressed);
        if (suppressed) return;
        try
        {
            _runLog.Append(line);
        }
        catch (IOException)
        {
            // Logging never breaks the shell.
        }
    }

    private void Render(SupervisorEvent e)
    {
        if (e is not SupervisorEvent.Capture { What: CaptureWhat.RemotePaintReceiver }) Log(e is SupervisorEvent.Admitted a ? $"Admitted pool={a.Pool.Name} tier={a.Pool.Tier}"
            : e is SupervisorEvent.Queued qlog ? $"Queued position={qlog.Place.Position} ahead={qlog.Place.Ahead}"
            : e is SupervisorEvent.AdmissionRetry retryLog ? $"Admission retry {retryLog.Code} after={retryLog.Delay.TotalSeconds:F0}s" : e.ToString());
        IntegrityOn(e);
        OverlayOn(e);
        switch (e)
        {
            case SupervisorEvent.ModsReported reported:
                // N-H2: the supervisor fed the hook's ModsReport (0x1b) to this launch's runtime mod set; a report for another nonce or plan is ignored.
                Log(reported.Accepted switch
                {
                    true => "mods: runtime report accepted (" + _playModSet?.State.Phase + ")",
                    false => "mods: runtime report ignored (not this launch's plan)",
                    null => "mods: runtime report ignored (no native plan)",
                });
                foreach (var entry in reported.Report.Mods)
                    Log($"mods: report {entry.Id} {entry.Version} state={entry.State switch { 1 => "ready", 2 => "failed", _ => "starting" }} compat={Convert.ToHexStringLower(entry.CompatibilitySha256)[..16]}");
                break;
            case SupervisorEvent.ModsFailed failed:
                // OD-113: no modded matchmaking for this launch; the game itself goes on (or the runtime already ended it)
                Log("mods: runtime mod set failed (" + failed.Reason + ")");
                var why = RuntimeReason();
                Message = why is null ? S["mods.online_failed"] : S["mods.online_failed"] + " (" + why + ")";
                _modLaunchFailure = Message; Changed(nameof(ModPlayNotice)); Changed(nameof(ShowModPlayNotice));
                SetStatus(Message);
                break;
            case SupervisorEvent.RequestingAdmission:
                _waitingForAdmission = true;
                _queued = false;
                _admissionTitle = S["queue.requesting"];
                _admissionDetail = S["queue.connecting_detail"];
                SetStatus(_admissionTitle);
                break;
            case SupervisorEvent.Queued q:
                _waitingForAdmission = _queued = true;
                _queuePlace = q.Place;
                _queueTier = q.Pool?.Tier;
                _admissionTitle = S["queue.waiting_title"];
                _admissionDetail = S.Format("queue.join_message", ("n", q.Place.Position.ToString(CultureInfo.InvariantCulture)));
                SetStatus(_admissionDetail);
                break;
            case SupervisorEvent.AdmissionRetry retry:
                _waitingForAdmission = true;
                _queued = false;
                _admissionTitle = S[retry.Code == Infinity.Core.Api.ErrorCodes.ServiceMaintenance ? "queue.maintenance_title"
                    : retry.Code == Infinity.Core.Api.ErrorCodes.PoolFull ? "queue.full_title" : "queue.retry_title"];
                var until = DateTimeOffset.TryParse(retry.Until, CultureInfo.InvariantCulture, DateTimeStyles.RoundtripKind, out var time)
                    ? " " + S.Format("queue.maintenance_until", ("time", time.ToLocalTime().ToString("g", CultureInfo.GetCultureInfo(S.Locale)))) : string.Empty;
                _admissionDetail = (retry.Code == Infinity.Core.Api.ErrorCodes.ServiceMaintenance ? S["queue.maintenance"] + until
                    : retry.Code == Infinity.Core.Api.ErrorCodes.PoolFull ? S["queue.full"] : S["queue.rate_limited"]) + " " +
                    S.Format("queue.retry_after", ("s", Math.Ceiling(retry.Delay.TotalSeconds).ToString(CultureInfo.InvariantCulture)));
                SetStatus(_admissionDetail);
                break;
            case SupervisorEvent.Admitted:
                _waitingForAdmission = _queued = false;
                SetStatus(S["queue.admitted_toast"]);
                break;
            case SupervisorEvent.Spawned:
                SetStatus(S["supervisor.starting"]);
                break;
            case SupervisorEvent.WaitingForHook w:
                SetStatus(S["supervisor.waiting_start"] + (w.SinceSeconds >= 15 ? " " + S["supervisor.press_start"] : string.Empty));
                break;
            case SupervisorEvent.Handshake h:
                _hookLine = S.Format("supervisor.hook", ("version", h.HookVersion));
                Changed(nameof(HookLine));
                break;
            case SupervisorEvent.Refused r:
                ConfigurationMessage(S.Format("supervisor.refused", ("reason", r.Reason)), error: true);
                SetStatus(string.Empty);
                break;
            case SupervisorEvent.Running:
                ClearRuntimeConfigurationIssue();
                SetStatus(S["supervisor.running"]);
                break;
            case SupervisorEvent.TokenPushed t:
                ClearRuntimeConfigurationIssue();
                _hookLine = (_hookLine?.Split(" · ")[0] ?? string.Empty) + " · " +
                    S.Format("supervisor.ticket_renewed", ("time", DateTimeOffset.UtcNow.ToString("HH:mm:ss", CultureInfo.InvariantCulture)));
                Changed(nameof(HookLine));
                Changed(string.Empty);
                break;
            case SupervisorEvent.Exited { Disposition: ExitDisposition.Crashed { Code: ModRuntimeExitCode } }:
                // the hook ended the session on purpose: the selected mods failed after they began loading (N-M4), not a crash
                _modRuntimeEnded = true;
                var reason = RuntimeReason();
                Message = reason is null ? S["mods.runtime_ended"] : S["mods.runtime_ended"] + " (" + reason + ")";
                _modLaunchFailure = Message; Changed(nameof(ModPlayNotice)); Changed(nameof(ShowModPlayNotice));
                SetStatus(Message);
                break;
            case SupervisorEvent.Exited { Disposition: ExitDisposition.IdentityRefused }:
                // B1: the hook closed the game before it read a save: no profile identity could be set (never Azimuth's own career)
                Message = S["supervisor.identity_refused"];
                SetStatus(Message);
                _identityRefused = true;
                break;
            case SupervisorEvent.Exited { Disposition: ExitDisposition.Crashed c }:
                SetStatus(S.Format("supervisor.crashed", ("code", $"0x{c.Code:x8}")));
                break;
            case SupervisorEvent.CrashCaptured captured:
                if (_modRuntimeEnded) { _modRuntimeEnded = false; Log("crash: not presented (the mod runtime ended the session)"); break; }
                PresentCrash(captured.Incident);
                break;
            case SupervisorEvent.Exited:
                SetStatus(S["supervisor.closing"]);
                break;
            case SupervisorEvent.Relaunching:
                SetStatus(S["supervisor.relaunching"]);
                break;
            case SupervisorEvent.Ended ended:
                _waitingForAdmission = _queued = false;
                if (ended.Reason is not "error") { ClearRuntimeConfigurationIssue(); ClearHookOfflineNotice(); }
                var text = ended.Reason switch
                {
                    "queue_cancelled" => S[ended.Left ? "queue.left" : "queue.leave_pending"],
                    "queue_ended" => S["queue.ended"],
                    "queue_kicked" => S["kick.removed"], // CR-S62: a moderator ended the queued session; no re-queue
                    "save_blocked" => _playStatus ?? S["saves.blocked"],
                    "relaunch_loop" => S["supervisor.relaunch_loop"],
                    "error" => _playStatus ?? S["supervisor.launch_refused"],
                    _ when ended.Reason.StartsWith("crashed", StringComparison.Ordinal) => _playStatus ?? S["status.ended"],
                    // B1: the game ended because no profile identity could be set: that reason stays on the status line
                    _ when _identityRefused => S["supervisor.identity_refused"],
                    _ => S["status.ended"],
                };
                _identityRefused = false;
                SetStatus(ended.Left || ended.Reason is "queue_cancelled" or "error" ? text : text + " " + S["supervisor.leave_failed"]);
                break;
            case SupervisorEvent.HookOffline:
                // The hook closes its pipe whenever the game quits, a moment before the process exit: a real problem only
                // while the game keeps running, so Ended takes the notice back instead of leaving Configure flagged.
                ConfigurationMessage(S["supervisor.hook_offline"], error: true);
                _hookOfflineNotice = true;
                SetStatus(string.Empty);
                break;
            case SupervisorEvent.Error err when err.Message.StartsWith("save: ", StringComparison.Ordinal):
                SetStatus(Infinity.Core.Overlay.OverlayNotices.SaveBlocked(S, err.Message[6..]).Text.Replace("\n", " · ", StringComparison.Ordinal));
                ObserveSaveBlocked(err.Message[6..]); // CR-S52: a seed waiting on the day's checks may be given up for a new career
                break;
            case SupervisorEvent.NoticeSent notice:
                Message = notice.Text;
                SetStatus(notice.Text);
                break;
            case SupervisorEvent.HookEvent { Kind: Infinity.Core.Session.HookEvents.Notice } notice
                when Infinity.Core.Session.HookEvents.NoticeToast(notice.Detail, S) is { } toast:
                Message = toast.Text;
                SetStatus(toast.Text.Replace("\n", " · ", StringComparison.Ordinal));
                break;
            case SupervisorEvent.Error { Message: Infinity.Core.Api.ErrorCodes.AccountsFrozen }:
                SetStatus(S["crash.frozen"]);
                break;
            case SupervisorEvent.OnlinePaused paused:
                ObserveOnlinePause(paused.Pause);
                break;
            case SupervisorEvent.Error { Message: ErrorCodes.GameUpdateRequired }:
                RequireLauncherUpdate();
                SetStatus(S["updater.session_required"]);
                break;
            case SupervisorEvent.Error content when content.Message is ErrorCodes.ContentIncompatible or ErrorCodes.ContentInconclusive or ErrorCodes.ContentRecheckRequired or ErrorCodes.ContentPolicyChanged or ErrorCodes.ContentPolicyUnavailable:
                IntegrityRefused(content.Message, content.DuringAdmission); // OD-113: a mismatch is shown; any other refusal is re-checked silently
                SetStatus(string.Empty);
                break;
            case SupervisorEvent.Error { Message: ErrorCodes.SessionsKicked } kicked:
                // CR-S62: a kick's cooldown refuses Play; the minutes left come from Retry-After (or details.until)
                Message = KickedRefusalText(kicked.Wait);
                SetStatus(Message);
                break;
            case SupervisorEvent.Error { DuringAdmission: true }:
                // Admission errors belong on Play. Keep technical installation diagnostics in Configure.
                Message = S["queue.failed"];
                SetStatus(Message);
                break;
            case SupervisorEvent.Error err:
                ConfigurationMessage(S["supervisor.launch_refused"] + " (" + err.Message + ")", error: _configurationHasError);
                _configurationRuntimeIssue = true;
                SetStatus(string.Empty);
                break;
        }
    }

    private void SetStatus(string text)
    {
        _playStatus = text;
        Changed(string.Empty);
    }

    private void StopPlay()
    {
        try
        {
            _play?.Cancel();
        }
        catch (ObjectDisposedException)
        {
            // Already over.
        }
    }
}
