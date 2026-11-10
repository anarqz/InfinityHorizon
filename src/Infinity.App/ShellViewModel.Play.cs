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
    /// <returns>False only when a Hardcore launch could not hold the Treasure Map off: that launch must not start.</returns>
    /// <remarks>
    /// Review 3.1.1: what it needs of the view model is read on the UI thread, then every file step (FM5's held folders and receipts, the
    /// load order, the inis, the title) runs on a worker, so a launch never holds the dispatcher here.
    /// </remarks>
    private async Task<bool> PrepareAzimuthAsync(AzimuthInstall? install, bool hardcore = false)
    {
        if (install is null) return true;
        var game = CurrentGame;
        var packageRoot = ContentLocation?.PackageRoot;
        var mods = game.FullFeatures || ExtraOptionsLayout.HasModsFolder(game.Id) ? ExtraOptionsLaunchInputsFor(game.Id) : null;
        var state = _stateDirectory;
        var (ok, fm5) = await Task.Run(() => PrepareAzimuthFiles(install, hardcore, game, packageRoot, mods, state));
        _fm5LaunchState = fm5;
        return ok;
    }

    /// <summary>The file steps of <see cref="PrepareAzimuthAsync"/>: no view-model state but its inputs and the log (runs on a worker).</summary>
    private (bool Ok, Fm5LaunchState? Fm5) PrepareAzimuthFiles(AzimuthInstall install, bool hardcore, GameDefinition game, string? packageRoot,
        ExtraOptionsLaunchInputs? mods, string stateDirectory)
    {
        // OD-233: the old flat mods folder has already moved to the per-game folders: both launches (PlayAsync, PlayOfflineAsync) await
        // PrepareModsLayoutAsync, off the UI thread, before they read any mod; a stop there ended the launch with its reason
        // OD-206: a game without FH2's features only needs naming, and without that the launch stops: Azimuth would start the game named before
        Fm5LaunchState? fm5 = null;
        if (!game.FullFeatures)
        {
            // OD-233: FM5's ExtraOptions 0.6.0 loads mods (catalog only) and reads its own ini with the server's locks
            if (mods is not null) fm5 = PrepareExtraOptionsMods(install, hardcore, mods);
            try { Log("extraoptions: " + ExtraOptionsInfinity.EnforceLocks(install.Root, game)); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidOperationException or Infinity.Core.Mods.ModException)
            { Log("extraoptions: locks not written (" + e.GetType().Name + ")"); }
            try
            {
                Log("azimuth: " + AzimuthTitles.EnsureTitle(install.Root, game, packageRoot));
                return (AzimuthTitles.Names(install.Root, game), fm5);
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                Log("azimuth: title not written (" + e.GetType().Name + ")");
                return (false, fm5);
            }
        }
        if (mods is not null) fm5 = PrepareExtraOptionsMods(install, hardcore, mods);
        try { Log("azimuth: " + AzimuthTitles.EnsureFh2(install.Root, packageRoot)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("azimuth: title not written (" + e.GetType().Name + ")"); }
        try { Log("extraoptions: " + ExtraOptionsInfinity.EnsureIni(install.Root)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidOperationException) { Log("extraoptions: ini not written (" + e.GetType().Name + ")"); }
        // OD-233: the server's settings (GenerateMissingSaveCommits, OfflineVIP, UnlockDLC) written off before every launch, as patch 0002 forces them
        try { Log("extraoptions: " + ExtraOptionsInfinity.EnforceLocks(install.Root, GameDefinition.Fh2)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidOperationException or Infinity.Core.Mods.ModException)
        { Log("extraoptions: locks not written (" + e.GetType().Name + ")"); }
        // owner 2026-10-29: the online tabs on (once, they were Infinity's lock), the Treasure Map held off for Hardcore
        try
        {
            Log("extraoptions: " + ExtraOptionsInfinity.EnableOnlineTabsOnce(install.Root, Path.Combine(stateDirectory, "extraoptions-online-tabs-on")));
            if (ExtraOptionsInfinity.HoldTreasureMapForHardcore(install.Root, hardcore, Path.Combine(stateDirectory, "extraoptions-treasure-map-held")) is { } note)
                Log("extraoptions: " + note);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidOperationException or Infinity.Core.Mods.ModException)
        {
            Log("extraoptions: launch settings not applied (" + e.GetType().Name + ")");
            return (!hardcore, fm5); // fail closed: a Hardcore profile never plays with the Treasure Map
        }
        return (true, fm5);
    }

    private string PrepareAzimuthFailed => _modsLayoutFailure ?? S[CurrentGame.FullFeatures ? "extraoptions.treasure_map_hold_failed" : "play.setup_needed"];

    private string? _playStatus;
    private string? _hookLine;
    private bool _waitingForAdmission;
    private bool _queued;
    private bool _leavingQueue;
    private QueuePlace? _queuePlace;
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

    public string LeaveQueueLabel => S[_leavingQueue ? "queue.leaving" : "queue.cancel"];

    public string PlayGuidance => SetupNeeded ? S["play.setup_needed"] : S[Playing ? "play.session_active" : "play.ready"];

    /// <summary>The detected Azimuth folder (<c>--azimuth</c>, <c>FI_AZIMUTH</c>), or null.</summary>
    public AzimuthInstall? Install
    {
        get => _install;
        // the synchronous form, for the callers that read the hook right after (a hook install, T30's replace before Play, a folder
        // chosen by hand); the searches use SetInstallAsync
        set
        {
            SelectInstall(value);
            if (value is not null)
            {
                SwitchCaches(value.Root, online: null);
                StartMods();
            }
            RefreshHook();
            FillPlayBoard(); // owner 2026-11-08: PLAY's buttons say when Azimuth is missing
            Changed(string.Empty);
        }
    }

    /// <summary>
    /// Owner 2026-11-10 ("never looks frozen"): <see cref="Install"/> with its slow parts off the UI thread: the mode caches' recovery
    /// (a process list and folder moves) and the hook's read (hashes, signatures). The mods start (<see cref="StartMods"/>) runs its move
    /// and reads in the background as before. For the searches (start-up, Setup's search, Check again), which await it.
    /// </summary>
    private async Task SetInstallAsync(AzimuthInstall? value)
    {
        SelectInstall(value);
        if (value is not null)
        {
            await SwitchCachesAsync(value.Root, online: null);
            if (_install != value) return; // another folder was chosen meanwhile: its own run finishes the work
            StartMods();
        }
        await RefreshHookAsync();
        if (_install != value) return;
        FillPlayBoard(); // owner 2026-11-08: PLAY's buttons say when Azimuth is missing
        Changed(string.Empty);
    }

    private void SelectInstall(AzimuthInstall? value)
    {
        if (value == _install) return;
        ClearIntegrity();
        _install = value;
        _contentLocationLoaded = false;
        // OD-206: the games on offer are this folder's
        if (OfflineMode) FillOfflineGames();
        else if (_session is { } session) FillServerGames(session.Info.Games ?? []);
    }

    public string InstallLabel => _install is null
        ? S["games.missing"] + " " + S["games.missing_hint"]
        : $"Azimuth {_install.Version ?? "?"} · " + (_install.HookVersion is { } hook ? S.Format("supervisor.hook", ("version", hook)) : S["supervisor.hook_unknown"]);

    public bool Playing => _play is not null;

    public bool CanPlay => Ready && !HasDeletionPending && _install is not null && _session is not null && !Playing && !_busy && CrashIdle && GameConfigurationReady && IntegrityCanStart && LauncherUpdateAllowsPlay && OnlineServiceAllowsPlay && !ModPlayBlocked && !JobsBlockPlay;

    public string? PlayStatus => _playStatus;

    public string? HookLine => _hookLine;

    public async Task PlayAsync()
    {
        // OD-118: without an account (or with a profile not on it) Play is the offline launch.
        RefreshChosenLocal(); // OD-118: an upload since it was chosen makes it play online
        // owner 2026-11-10: a cloud save going up (or another should_block_play job) holds every launch, online and offline, and says so
        if (!Playing && PlayHeldByJobs is { } held) { Message = held; return; }
        if (!PlaysOnline) { await PlayOfflineAsync(); return; }
        _modLaunchFailure = null;
        if (!OnlineServiceAllowsPlay) { OpenOnlineStatus(); Message = OnlineServiceMessage; return; }
        if (!LauncherUpdateAllowsPlay) { _updateSupport = false; Message = LauncherUpdateStatus; Changed(string.Empty); return; }
        await RefreshHookAsync(); // the hook as it is now, not as it was when the screen last looked (read on a worker)
        // T30: a signed hook whose bytes are not the package's is replaced first (game closed), or Play is refused
        if (!Playing && !EnsureHookCurrentForPlay()) return;
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
        // OD-233: the per-game mods folders before anything reads the mods (a stop says why and blocks Play)
        if (!await PrepareModsLayoutAsync(receiptsOnly: false, background: false)) { Message = PrepareAzimuthFailed; return; }
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
        // OD-232: an enabled mod in a retired format still changes the game (half a media override, a loader DLL), so it never goes
        // online as vanilla: refused until it is disabled or removed on Mods (offline it stays a warning).
        if (mods.RetiredOnlineMods is { Count: > 0 } retired)
        {
            Log("mods: online refused, retired mods enabled: " + string.Join(", ", retired.Select(m => m.Id)));
            Message = RetiredOnlineRefusal(retired.Select(m => m.Text("name", S.Locale)));
            await RefreshModsAsync(); return;
        }
        if (!ReferenceEquals(_session, startingSession) || !CanPlay) return;
        var startingInstall = _install;
        var startingGame = _gate.Game;
        if (!await RefreshOnlineServiceForPlayAsync()) return;
        if (!ReferenceEquals(_session, startingSession) || !ReferenceEquals(_install, startingInstall) || _gate.Game != startingGame || !CanPlay) return;
        if (!await RefreshLauncherPolicyAsync()) return;
        if (!ReferenceEquals(_session, startingSession) || !CanPlay) return;
        var futureSessionId = Guid.CreateVersion7().ToString();
        // OD-113: only a profile with the check (IntegrityApplies) is checked, silently; any other declares no car database.
        var (integrityProceed, integrity) = await IntegrityForPlayAsync(futureSessionId);
        if (!integrityProceed || !ReferenceEquals(_session, startingSession) || !ReferenceEquals(_install, startingInstall) || _gate.Game != startingGame
            || !CanPlay) return;
        var profile = _session.Me?.Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey && p.Game == _gate.Game);
        if (profile is null)
        {
            return;
        }
        // OD-206: a game without the integrity check still declares its car database: the gate compares it with the hook's
        var carDb = integrity?.CarDbSha256 ?? (CurrentGame.FullFeatures ? string.Empty : await CarDatabaseDigestAsync(CurrentGame));
        var development = DevServer;
        var (keys, policy) = _config.Production
            ? (Infinity.Core.Auth.ProductionDeployment.LaunchKeys(), TicketPolicy.Default with { Production = true })
            // CR-L39: the keys this session's validated /info published and the realm's pin matched.
            : _config.Custom ? (TicketKeys.FromTrust(_session.Info.Trust!), TicketPolicy.ForCustom)
            : TicketKeys.Trust(development, Environment.GetEnvironmentVariable(TicketKeys.DevTicketKeyEnv));
        // the load order first (Hardcore turns its mods off there), then the lobby set from what ExtraOptions will load
        if (!await PrepareAzimuthAsync(_install, ProfileHardcore)) { Message = PrepareAzimuthFailed; return; }
        if (!ReferenceEquals(_session, startingSession) || !ReferenceEquals(_install, startingInstall) || _gate.Game != startingGame || !CanPlay) return;
        var modSet = await ExtraOptionsModSetAsync(_install, DateTime.UtcNow);
        if (modSet is null) { Message = ModSetRefusal; await RefreshModsAsync(); return; }
        if (!ReferenceEquals(_session, startingSession) || !CanPlay) return;
        using var onlineActivity = new OnlineActivity();
        StartOverlayStatusFeed(_session); // CR-L40 §1.3: the guide's server status and avatar
        var config = new SupervisorConfig(profile.Game, profile.Xuid, _install, Version, Environment.ProcessPath ?? "infinity.exe", keys, policy, _stateDirectory)
        {
            Overlay = await LoadOverlayPackageAsync(),
            OverlayStatus = _overlayStatusFeed,
            NativeDllOverrides = mods.DllOverrides,
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
            // owner 2026-11-10: the end of a session never holds the UI thread (folder moves, hashes and the save scan on workers)
            if (caches)
            {
                await SwitchCachesAsync(_install.Root, online: false);
            }
            Changed(nameof(ProfileRows));
            _play.Dispose();
            _play = null;
            FinishSaveUploadJob(); // the session (and its post-game upload) is over: the upload toast ends with it
            SavesChanged(); // a session uploads: the cached cloud revisions of this game are read again when next shown
            _waitingForAdmission = _queued = _leavingQueue = false;
            try
            {
                await RefreshHookAsync();
                await PrimeLocalFactsQuietlyAsync();
            }
            catch (Exception error) when (!IsFatal(error)) { Log("play: after-session refresh failed: " + error.GetType().Name); } // the session's end still completes
            _commands = null;
            if (ReferenceEquals(_playOnlineActivity, onlineActivity)) _playOnlineActivity = null;
            _playWorlds = null;
            _playModSet = null;
            // Owner 2026-11-10: a full mod read only at start, before Play and on Refresh; the cards only redraw for the session's end
            RefreshModCards();
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
    private bool SwitchCaches(string root, bool? online) => SwitchCachesFor(root, online, CurrentGame);

    /// <summary>Owner 2026-11-10: <see cref="SwitchCaches"/> on a worker (a process list and folder moves), for a session's end.</summary>
    private Task<bool> SwitchCachesAsync(string root, bool? online)
    {
        var game = CurrentGame;
        return Task.Run(() => SwitchCachesFor(root, online, game));
    }

    private bool SwitchCachesFor(string root, bool? online, GameDefinition game)
    {
        if (AzimuthRunning())
        {
            Log("mode caches: azimuth.exe is running, left as they are");
            return false;
        }
        try
        {
            switch (online)
            {
                case true:
                    ModeCaches.For(root, game).BeginOnline();
                    break;
                case false:
                    ModeCaches.For(root, game).EndOnline();
                    break;
                default:
                    // OD-206: whichever game's session was interrupted, not only the one chosen now
                    foreach (var each in GameDefinition.All)
                    {
                        if (ModeCaches.For(root, each).Recover())
                        {
                            Log("mode caches: an interrupted online session's caches were put away");
                        }
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
        PresenceOn(e); // OD-136: the Discord card
        switch (e)
        {
            case SupervisorEvent.ModsFailed failed:
                // no modded matchmaking for this launch (ExtraOptions loaded none of the planned mods); the game itself goes on
                Log("mods: lobby mod set failed (" + failed.Reason + ")");
                Message = S["mods.online_failed"];
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
                ObserveRestoreBlocked(); // OD-133: a restored career waiting for the cloud may be set aside
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
