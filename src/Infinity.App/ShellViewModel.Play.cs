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

namespace Infinity.App;

/// <summary>Play: the supervisor runs off the UI thread; its events come back through the UI context.</summary>
public sealed partial class ShellViewModel
{
    private readonly SynchronizationContext _ui = SynchronizationContext.Current ?? new SynchronizationContext();
    private AzimuthInstall? _install;
    private CancellationTokenSource? _play;
    private Channel<SupervisorCommand>? _commands;
    private Task? _playTask;
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
            ClearIntegrity();
            _install = value;
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

    public bool CanPlay => Ready && !HasDeletionPending && _install is not null && _session is not null && !Playing && !_busy && CrashIdle && GameConfigurationReady && IntegrityCanStart && LauncherUpdateAllowsPlay && OnlineServiceAllowsPlay;

    public string? PlayStatus => _playStatus;

    public string? HookLine => _hookLine;

    public async Task PlayAsync()
    {
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
        var startingSession = _session;
        var startingInstall = _install;
        var startingGame = _gate.Game;
        if (!await RefreshOnlineServiceForPlayAsync()) return;
        if (!ReferenceEquals(_session, startingSession) || !ReferenceEquals(_install, startingInstall) || _gate.Game != startingGame || !CanPlay) return;
        if (!await RefreshLauncherPolicyAsync()) return;
        if (!ReferenceEquals(_session, startingSession) || !CanPlay) return;
        var futureSessionId = Guid.CreateVersion7().ToString();
        await CheckIntegrityAsync(futureSessionId);
        if (!ReferenceEquals(_session, startingSession) || !ReferenceEquals(_install, startingInstall) || _gate.Game != startingGame
            || !CanPlay || !IntegrityReady || _integrity is null)
        { ConfigurationMessage(IntegrityStatus, error: true); return; }
        var profile = _session.Me?.Profiles.FirstOrDefault(p => p.Gamertag == _gate.ProfileTag && p.Game == _gate.Game);
        if (profile is null)
        {
            return;
        }
        var integrity = _integrity!;
        if (integrity.CarDbSha256 is not { Length: 64 } carDb) { ConfigurationMessage(S["integrity.inconclusive"], error: true); return; }
        var development = DevNet.PlainHttpUrl(_session.Api.Core.GetLeftPart(UriPartial.Authority));
        var (keys, policy) = TicketKeys.Trust(development, Environment.GetEnvironmentVariable(TicketKeys.DevTicketKeyEnv));
        using var onlineActivity = new OnlineActivity();
        var config = new SupervisorConfig(profile.Game, profile.Xuid, _install, Version, Environment.ProcessPath ?? "infinity.exe", keys, policy, _stateDirectory)
        {
            Overlay = LoadOverlayPackage(),
            Saves = SessionSavesFor(profile),
            Worlds = WorldsFor(profile.Mode, profile.Xuid, onlineActivity),
            OnlineActivity = onlineActivity,
            Text = S,
            CrashReports = new Infinity.Core.Diagnostics.CrashIncidentStore(_stateDirectory),
            AccountId = _session.Me?.AccountId,
            CarDbSha256 = carDb,
            InitialSessionId = futureSessionId,
            OnlineAccessLost = integrity.Revoke,
            PrepareAdmission = async (id, ct) =>
            {
                if (!ReferenceEquals(_integrity, integrity)) throw new InvalidDataException("Content check changed");
                await integrity.PrepareAdmissionAsync(id, ct).ConfigureAwait(false);
                if (!ReferenceEquals(_integrity, integrity)) throw new InvalidDataException("Content check changed");
            },
        };
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
        // OD-45: ExtraOptions' switches that conflict with Infinity stay off.
        EnforceModLocks();
        // BUG-0001: the online session's drivatar caches never reach offline play.
        var caches = SwitchCaches(_install.Root, online: true);
        _playOnlineActivity = onlineActivity;
        try
        {
            _playTask = Task.Run(() => supervisor.RunAsync(_play.Token));
            await _playTask;
        }
        finally
        {
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
            Changed(string.Empty);
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
            Directory.CreateDirectory(_stateDirectory);
            File.AppendAllText(Path.Combine(_stateDirectory, "launcher.log"), $"{DateTimeOffset.Now:yyyy-MM-dd HH:mm:ss.fff zzz} {line}{Environment.NewLine}");
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
            case SupervisorEvent.Exited { Disposition: ExitDisposition.Crashed c }:
                SetStatus(S.Format("supervisor.crashed", ("code", $"0x{c.Code:x8}")));
                break;
            case SupervisorEvent.CrashCaptured captured:
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
                if (ended.Reason is not "error") ClearRuntimeConfigurationIssue();
                var text = ended.Reason switch
                {
                    "queue_cancelled" => S[ended.Left ? "queue.left" : "queue.leave_pending"],
                    "queue_ended" => S["queue.ended"],
                    "save_blocked" => _playStatus ?? S["saves.blocked"],
                    "relaunch_loop" => S["supervisor.relaunch_loop"],
                    "error" => _playStatus ?? S["supervisor.launch_refused"],
                    _ when ended.Reason.StartsWith("crashed", StringComparison.Ordinal) => _playStatus ?? S["status.ended"],
                    _ => S["status.ended"],
                };
                SetStatus(ended.Left || ended.Reason is "queue_cancelled" or "error" ? text : text + " " + S["supervisor.leave_failed"]);
                break;
            case SupervisorEvent.HookOffline:
                ConfigurationMessage(S["supervisor.hook_offline"], error: true);
                SetStatus(string.Empty);
                break;
            case SupervisorEvent.Error err when err.Message.StartsWith("save: ", StringComparison.Ordinal):
                SetStatus(Infinity.Core.Overlay.OverlayNotices.SaveBlocked(S, err.Message[6..]).Text.Replace("\n", " · ", StringComparison.Ordinal));
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
                _integrity?.Revoke();
                _integrityStatus = S[content.Message == ErrorCodes.ContentPolicyUnavailable ? "integrity.policy_unavailable"
                    : content.Message == ErrorCodes.ContentIncompatible ? "integrity.incompatible" : "integrity.inconclusive"];
                ConfigurationMessage(_integrityStatus, error: true);
                SetStatus(string.Empty);
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
