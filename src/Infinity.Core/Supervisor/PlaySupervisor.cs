// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading.Channels;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.Process;
using Infinity.Core.Session;
using Infinity.Core.Windows;
using Infinity.Core.Wire;
using Infinity.Core.Diagnostics;
using Infinity.Core.Online;

namespace Infinity.Core.Supervisor;

/// <summary>What one Play needs (ORCHESTRATOR §2.2 <c>SupervisorConfig</c>).</summary>
public sealed record SupervisorConfig(
    string Game,
    string Xuid,
    AzimuthInstall Install,
    string LauncherVersion,
    string LauncherExe,
    TicketKeys Keys,
    TicketPolicy Policy,
    string StateDirectory)
{
    public IReadOnlyList<string> NativeDllOverrides { get; init; } = [];
    public OnlineActivity? OnlineActivity { get; init; }
    /// <summary>
    /// B1: the profile's own offline identity (its XUID in decimal and the name it plays under), named to the hook on an online start
    /// too: when the online identity cannot be set (ticket refused, handshake late or lost) the game plays offline on that save, never
    /// on Azimuth's own <c>User_9000000000000</c>. Null: the hook ends the game before it reads a save instead.
    /// </summary>
    public (string Xuid, string Gamertag)? FallbackIdentity { get; init; }
    /// <summary>How often the profile's folder is read while the game runs: a fresh start's first save (CR-S51), checkpoints (OD122-L3).</summary>
    public TimeSpan SavesPoll { get; init; } = TimeSpan.FromSeconds(3);
    /// <summary>First hook supporting authenticated terminal post-handshake suspension.</summary>
    public uint MinimumOnlineHookBuild { get; init; } = BuildProfile.ProductionOnly ? 79u : 76u;
    /// <summary>One bounded accept (ORCHESTRATOR §1.4: called again for the child's whole life).</summary>
    public TimeSpan AcceptBound { get; init; } = TimeSpan.FromSeconds(10);

    /// <summary>The player's Quit: WM_CLOSE, then TerminateProcess after this.</summary>
    public TimeSpan QuitGrace { get; init; } = TimeSpan.FromSeconds(20);

    /// <summary>After the game exits, how long to wait for the job to empty before the leave.</summary>
    public TimeSpan JobDrain { get; init; } = TimeSpan.FromSeconds(5);

    /// <summary>At most one heartbeat forced by the hook's <c>RefreshRequest</c> per this interval.</summary>
    public TimeSpan ForcedBeatEvery { get; init; } = TimeSpan.FromSeconds(30);

    /// <summary>The overlay DLL the hook loads after the handshake (OD-50); null: no overlay.</summary>
    public OverlayPackage? Overlay { get; init; }

    /// <summary>CR-L40 §1.3: the guide's server status and avatar (sent on the overlay's hello, refreshed while the guide is open); null: none.</summary>
    public Overlay.OverlayServerStatusFeed? OverlayStatus { get; init; }

    /// <summary>The overlay's Worlds tab (OD-54): the bridge for a session, given its current game ticket; null: none.</summary>
    public Func<Func<string>, Matchmaking.WorldsBridge>? Worlds { get; init; }

    /// <summary>
    /// OD-112: the session's multiplayer mod set (the same instance the <see cref="Worlds"/> bridge uses).
    /// A planned mod that counts as modded (<see cref="Matchmaking.IModSetSource.Modded"/>, OD-236) admits the session as <c>modded</c> (GATE_API.md §2).
    /// </summary>
    public Matchmaking.IModSetSource? Mods { get; init; }

    /// <summary>
    /// How long after the hook's handshake an <see cref="Matchmaking.ExtraOptionsModSetSource"/> waits for ExtraOptions' log of this run.
    /// Past it the set settles with what the log says (no modded matchmaking when nothing loaded): never waited on forever.
    /// </summary>
    public TimeSpan ModReportDeadline { get; init; } = TimeSpan.FromSeconds(90);

    /// <summary>The cloud save around the game (OD-52, OD-60); null: no cloud save step.</summary>
    public Saves.SessionSaves? Saves { get; init; }

    /// <summary>The shell's English/pt-BR catalog for notices; headless callers use English.</summary>
    public Strings? Text { get; init; }
    public CrashIncidentStore? CrashReports { get; init; }
    public string? AccountId { get; init; }
    public string CarDbSha256 { get; init; } = "";
    public Func<string, CancellationToken, Task>? PrepareAdmission { get; init; }
    public Action? OnlineAccessLost { get; init; }
    public string ConnectionEndpoints { get; init; } = "";
    public string? InitialSessionId { get; init; }
}

/// <summary>What the shell renders (ORCHESTRATOR §2.2 <c>SupervisorEvent</c>, §2.3).</summary>
public abstract record SupervisorEvent
{
    private SupervisorEvent()
    {
    }

    public sealed record Queued(QueuePlace Place, Pool? Pool = null) : SupervisorEvent;

    public sealed record RequestingAdmission : SupervisorEvent;

    public sealed record AdmissionRetry(string Code, TimeSpan Delay, string? Until) : SupervisorEvent;

    public sealed record Admitted(Pool Pool) : SupervisorEvent;

    public sealed record Spawned(uint Pid) : SupervisorEvent;

    /// <summary>Every accept bound until <c>Hello</c> (after 15 s: "Press Start in Azimuth's window").</summary>
    public sealed record WaitingForHook(int SinceSeconds) : SupervisorEvent;

    public sealed record Handshake(string HookVersion) : SupervisorEvent;

    /// <summary>The hook was answered <c>Refused</c>: the game plays offline, still supervised.</summary>
    public sealed record Refused(string Reason) : SupervisorEvent;

    public sealed record Running : SupervisorEvent;

    public sealed record TokenPushed(string ExpiresAt) : SupervisorEvent;

    public sealed record HookEvent(byte Kind, string Detail) : SupervisorEvent;

    public sealed record Capture(byte What, bool Ok) : SupervisorEvent
    {
        public long? ReceivedTimestamp { get; init; }
    }

    public sealed record NoticeSent(byte Level, string Text) : SupervisorEvent;

    public sealed record Exited(ExitDisposition Disposition) : SupervisorEvent;
    public sealed record CrashCaptured(CrashIncident Incident) : SupervisorEvent;

    public sealed record Relaunching(int Attempt) : SupervisorEvent;


    /// <summary>The play session is over; <paramref name="Left"/> tells whether the gate leave was sent.</summary>
    public sealed record Ended(string Reason, bool Left) : SupervisorEvent;

    public sealed record Error(string Message, bool DuringAdmission = false) : SupervisorEvent
    {
        /// <summary>CR-S62: 403 <c>SESSIONS.KICKED</c> on admission, how long the kick's cooldown still lasts (null: unknown).</summary>
        public TimeSpan? Wait { get; init; }
    }
    public sealed record OnlinePaused(OnlinePause Pause) : SupervisorEvent;

    /// <summary>Something the launcher decided for this launch, for the log (Wine overrides, ...).</summary>
    public sealed record Note(string Text) : SupervisorEvent;

    /// <summary>The hook stopped listening (it refused the ticket and went offline, or its pipe broke): the game plays offline.</summary>
    public sealed record HookOffline : SupervisorEvent;

    /// <summary>The overlay said hello on the UI pipe (its version).</summary>
    public sealed record OverlayConnected(string Version) : SupervisorEvent;

    /// <summary>CR-L41.3: the hook's reading of an <c>UpdateAll</c> it sent (the game's reported level and credits), for the Discord presence.
    /// <see cref="Level"/> is the step inside the current colour band, not the driver level (CR-S81): the presence never shows it.</summary>
    public sealed record GameProgress(uint Level, ulong Credits) : SupervisorEvent;

    /// <summary>
    /// CR-L41.3: the local player's current car changed: its car key (the raw i32 at current+0x40, &gt; 0,
    /// <c>Data_Car.Id × 1000</c>; name through <see cref="Process.CarNames.Name"/>), for the Discord presence.
    /// </summary>
    public sealed record GameCar(int CarKey) : SupervisorEvent;

    /// <summary>The session's mod set failed (ExtraOptions loaded none of the planned mods, or its log never answered): no modded matchmaking for this launch.</summary>
    public sealed record ModsFailed(string Reason) : SupervisorEvent;

    /// <summary>Something the overlay reported (Overlay.UiEventKind: first frame, passthrough, ...).</summary>
    public sealed record OverlayEvent(byte Kind, string Detail) : SupervisorEvent;
}

/// <summary>What the shell asks while a session runs.</summary>
public abstract record SupervisorCommand
{
    private SupervisorCommand()
    {
    }

    /// <summary>The player confirmed "Quit game".</summary>
    public sealed record Quit : SupervisorCommand;

    /// <summary>An overlay notice (0 info, 1 warning, 2 error), also sent to compatible hooks.</summary>
    public sealed record Notice(byte Level, string Text) : SupervisorCommand;

    /// <summary>The overlay's status line (account, tier, state), already localised.</summary>
    public sealed record OverlayStatus(Overlay.UiFrame.Status Status) : SupervisorCommand;

    /// <summary>A toast in the game, already localised (queued until the overlay connects).</summary>
    public sealed record OverlayToast(Overlay.UiFrame.Toast Toast) : SupervisorCommand;

    /// <summary>
    /// OD-204 (T-L3): an invite prompt for the overlay's modal (or its withdrawal, <c>ExpiresInSec</c> 0), sent only to a protocol 6
    /// overlay and never through the quiet rule. The launcher's inbox poll that sends it is T-L4/T-L5's; today only tests send it.
    /// </summary>
    public sealed record OverlayInvite(Overlay.UiFrame.InvitePrompt Prompt) : SupervisorCommand;
}

/// <summary>
/// One Play, start to end (ORCHESTRATOR §1-§2; the Rust <c>fi_launcher_core::supervisor</c>):
/// admission with this launch's nonce (queue polled), the launch ticket verified, the control
/// pipe created before the child, <c>azimuth.exe</c> spawned with <c>FI_LAUNCH</c>, the bounded
/// accept repeated for the child's whole life, <c>Hello</c> answered once with the ticket and the
/// newest game token, a <c>Token</c> after every heartbeat, and after the exit: the job drained
/// (bounded), the gate left, <c>session.json</c> closed. Exit <c>0x46490001</c> relaunches (at most
/// 3 in 10 minutes). It never kills the game except for the player's Quit. Cancelling
/// <c>cancellation</c> (the shell is closing) leaves the gate and returns, leaving the game running.
/// </summary>
public sealed class PlaySupervisor
{
    public const int MaxRelaunches = 3;

    public static readonly TimeSpan RelaunchWindow = TimeSpan.FromMinutes(10);

    private readonly SupervisorConfig _config;
    private readonly IGate _gate;
    private readonly IGameSpawner _spawner;
    private readonly Func<Handoff, IControlPipe> _pipes;
    private readonly Action<SupervisorEvent> _emit;
    private readonly ChannelReader<SupervisorCommand> _commands;
    private readonly TimeProvider _clock;
    private readonly Func<Handoff, IControlPipe>? _uiPipes;
    private readonly OnlineActivity _online;
    private readonly bool _ownsOnline;
    private int _pauseEmitted;

    // the session's current game access ticket (matchmaking's X-Fi-Session), renewed by each heartbeat
    private volatile string _ticket = string.Empty;
    private bool _admissionPending;

    public PlaySupervisor(
        SupervisorConfig config,
        IGate gate,
        IGameSpawner spawner,
        Func<Handoff, IControlPipe> pipes,
        Action<SupervisorEvent> emit,
        ChannelReader<SupervisorCommand> commands,
        TimeProvider? clock = null,
        Func<Handoff, IControlPipe>? uiPipes = null)
    {
        _uiPipes = uiPipes;
        _config = config ?? throw new ArgumentNullException(nameof(config));
        _gate = gate ?? throw new ArgumentNullException(nameof(gate));
        _spawner = spawner ?? throw new ArgumentNullException(nameof(spawner));
        _pipes = pipes ?? throw new ArgumentNullException(nameof(pipes));
        _emit = emit ?? throw new ArgumentNullException(nameof(emit));
        _commands = commands ?? throw new ArgumentNullException(nameof(commands));
        _clock = clock ?? TimeProvider.System;
        _online = config.OnlineActivity ?? new OnlineActivity();
        _ownsOnline = config.OnlineActivity is null;
    }

    /// <summary>Runs until the play session ends (relaunches included).</summary>
    public async Task RunAsync(CancellationToken cancellation)
    {
        try { await RunCoreAsync(cancellation).ConfigureAwait(false); }
        finally { if (_ownsOnline) _online.Dispose(); }
    }

    private async Task RunCoreAsync(CancellationToken cancellation)
    {
        var relaunches = new Queue<DateTimeOffset>();
        var initialSessionId = _config.InitialSessionId;
        while (true)
        {
            ExitDisposition? disposition;
            try
            {
                var sessionId = initialSessionId;
                initialSessionId = null;
                disposition = await OneSessionAsync(sessionId, cancellation).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
            {
                return;
            }
            catch (Exception e) when (e is ApiException or HttpRequestException or TicketException or SignatureException or ProtocolException or IOException or InvalidDataException or JsonException or System.ComponentModel.Win32Exception)
            {
                if (e is ApiException apiClosed && OnlinePause.From(apiClosed) is { } pause)
                { _online.TryPause(pause); EmitPause(pause); }
                _emit(new SupervisorEvent.Error(e is ApiException api ? api.Code : e.Message, DuringAdmission: _admissionPending)
                { Wait = e is ApiException kicked ? Shell.KickCooldown.Remaining(kicked, _clock.GetUtcNow()) : null });
                _emit(new SupervisorEvent.Ended("error", false));
                return;
            }
            if (disposition is not ExitDisposition.Relaunch)
            {
                return;
            }
            if (_online.IsPaused) return; // Operator reopen never re-admits a running/ended activity automatically.
            var now = _clock.GetUtcNow();
            while (relaunches.Count > 0 && now - relaunches.Peek() > RelaunchWindow)
            {
                relaunches.Dequeue();
            }
            if (relaunches.Count >= MaxRelaunches)
            {
                _emit(new SupervisorEvent.Ended("relaunch_loop", true));
                return;
            }
            relaunches.Enqueue(now);
            _emit(new SupervisorEvent.Relaunching(relaunches.Count));
        }
    }

    /// <summary>Review 3.1.2 (finding 5): set by <see cref="CancelBeforeSpawn"/>; read only at the checkpoints before the game is started.</summary>
    private volatile bool _cancelBeforeSpawn;

    /// <summary>
    /// Review 3.1.2 (finding 5): the player's Cancel of a launch the supervisor is preparing. It never interrupts a step (a cloud save being
    /// installed or uploaded finishes first): the next checkpoint before the game is started leaves the gate and ends as a cancelled queue.
    /// Once the game is started it changes nothing; the shell's <see cref="SupervisorCommand.Quit"/> closes it then.
    /// </summary>
    public void CancelBeforeSpawn() => _cancelBeforeSpawn = true;

    /// <summary>One gate session and one game process; null when it ended without a game exit.</summary>
    private async Task<ExitDisposition?> OneSessionAsync(string? initialSessionId, CancellationToken cancellation)
    {
        _admissionPending = false;
        using var preparing =CancellationTokenSource.CreateLinkedTokenSource(cancellation, _online.Cancellation);
        if (_online.IsPaused) { EmitPause(await _online.Paused.ConfigureAwait(false)); return null; }
        var handoff = new Handoff(HandoffChannel.Pipe, Launch.NewNonce());
        // The pipe exists before the child (ORCHESTRATOR §1.2): the hook's 5 s wait always finds it.
        using var pipe = _pipes(handoff);
        // the UI pipe too, before the child: the overlay connects once the hook loaded it
        using var uiPipe = _config.Overlay is not null && _uiPipes is not null ? _uiPipes(handoff) : null;
        _admissionPending = true;
        // OD-204 (MATCHMAKING_API §2.10): the session declares private_official_v1; a server that does not know the member refuses it,
        // and the admission is asked again without it (private creates then stay off this session)
        var gateAdmission = new GateAdmission(_gate, _emit, _clock);
        var admission = await gateAdmission.WaitAsync(
            id => new PlaySessionCreate(id, _config.Game, _config.Xuid, handoff.NonceHex, _config.LauncherVersion, _config.Install.HookSha256, _config.CarDbSha256, 0, false,
                Modded: Matchmaking.ModSets.Modded(_config.Mods), Features: [PlaySessionFeatures.PrivateOfficialV1]),
            preparing.Token, _config.PrepareAdmission, initialSessionId).ConfigureAwait(false);
        _privateOfficial = !gateAdmission.FeaturesRefused;
        if (admission is null)
        {
            if (_online.IsPaused) EmitPause(await _online.Paused.ConfigureAwait(false));
            return null;
        }
        var (sessionId, active) = admission.Value;
        var state = new SessionRecord(sessionId, _config.Game, _config.Xuid, handoff.NonceHex);
        var fence = active.Lease?.FenceToken;
        if (await CancelBeforeSpawnAsync(sessionId, fence, preparing.Token).ConfigureAwait(false)) return null;
        _ticket = active.Ticket.Token;
        _emit(new SupervisorEvent.Admitted(active.Pool));
        byte[] claims, signature;
        try
        {
            claims = Convert.FromBase64String(active.LaunchTicket.Claims);
            signature = Convert.FromBase64String(active.LaunchTicket.Signature);
            var verified = TicketVerifier.Verify(claims, signature, _config.Keys, handoff.Nonce, (ulong)_clock.GetUtcNow().ToUnixTimeSeconds(), _config.Policy);
            state = state with { Phase = "ticketed", TicketExpUnix = verified.Exp };
        }
        catch (Exception e) when (e is FormatException or TicketException or SignatureException)
        {
            await LeaveQuietly(sessionId, "player", fence).ConfigureAwait(false);
            throw new TicketException($"the server's launch ticket was refused: {e.Message}");
        }
        _admissionPending = false;
        Write(state);

        // The server decides which profiles keep a cloud line (per server: any of Noob/Hardcore × free/Supporter): its admission
        // carries one (lease and save). A line the launcher cannot serve (no saves service named) never plays unsynced.
        if (_config.Saves is null && Saves.CloudSaveLine.FromGate(active) is not null)
        {
            var leftUnserved = await LeaveQuietly(sessionId, "player", fence).ConfigureAwait(false);
            _emit(new SupervisorEvent.Error("save: SAVES.SERVICE_MISSING"));
            _emit(new SupervisorEvent.Ended("save_blocked", leftUnserved));
            return null;
        }
        // OD-52 / OD-60: the cloud save before the game (seed, download, install); never over an unknown state.
        if (_config.Saves is { } saves)
        {
            try
            {
                await saves.BeforeSpawnAsync(active, preparing.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (preparing.IsCancellationRequested)
            {
                await CancelBeforeSpawnAsync(sessionId, saves.FenceToken ?? fence, preparing.Token).ConfigureAwait(false);
                return null;
            }
            catch (Exception e) when (e is Saves.SaveBlockedException or ApiException or HttpRequestException or IOException or UnauthorizedAccessException)
            {
                var closure = e is ApiException apiError ? OnlinePause.From(apiError)
                    : e is Saves.SaveBlockedException blockedError ? OnlinePause.From(blockedError.Code, System.Net.HttpStatusCode.ServiceUnavailable) : null;
                if (closure is not null) { _online.TryPause(closure); EmitPause(closure); }
                var leftBlocked = await LeaveQuietly(sessionId, "player", saves.FenceToken ?? fence).ConfigureAwait(false);
                _emit(new SupervisorEvent.Error("save: " + (e is Saves.SaveBlockedException blocked ? Overlay.OverlayNotices.BlockedDetail(blocked.Code, blocked.RetryAfter)
                    : e is ApiException api ? api.Code : e.Message)));
                _emit(new SupervisorEvent.Ended("save_blocked", leftBlocked));
                return null;
            }
        }

        var token = new GameToken(new Secret(active.Ticket.Token), Unix(active.Ticket.ExpiresAt));
        // CR-L39: a custom server's ticket reaches the hook with the keys it verified against (CustomTrust, hook build 89+).
        var endpoint = new LauncherEndpoint(new Admission.Ticket(claims, signature), _config.MinimumOnlineHookBuild,
            _config.Policy.Custom ? new Frame.CustomTrust(_config.Keys.All) : null);
        List<(string, string)> env = [(Launch.EnvLaunch, handoff.EnvValue), (Launch.EnvLauncherExe, _config.LauncherExe)];
        // B1: the profile's own save if the online identity cannot be set (account.c account_fallback_identity)
        if (_config.FallbackIdentity is { } fallback)
        {
            env.Add((Launch.EnvOfflineXuid, fallback.Xuid));
            env.Add((Launch.EnvOfflineGamertag, fallback.Gamertag));
        }
        if (WineHost.Detected)
        {
            // OD-125: the hook is ExtraOptions.Infinity's winmm.dll; Wine loads it only native-first (AppDefaults)
            _emit(new SupervisorEvent.Note("wine: " + WineHost.HookModule + " " + WineHost.EnsureNativeDllOverride(WineHost.HookModule, true)));
            var overrides = Environment.GetEnvironmentVariable("WINEDLLOVERRIDES");
            overrides = WineHost.WithNativeDll(overrides, WineHost.HookModule) ?? overrides;
            foreach (var module in _config.NativeDllOverrides)
            {
                overrides = WineHost.WithNativeDll(overrides, module) ?? overrides;
                // The AppDefaults registry entry is set and removed by ManagedModFiles.ReconcileWineOverrides (L12).
            }
            if (overrides is not null) env.Add(("WINEDLLOVERRIDES", overrides));
        }
        // B1: an inherited FI_OFFLINE_* (Steam launch options, a shell) never names the identity: only FallbackIdentity does
        // Azimuth 1.0 opens its own launcher without arguments: --skip-launcher starts the added FH2 directly
        var plan = new LaunchPlan(_config.Install.HostExe, [AzimuthSetup.SkipLauncher], _config.Install.Root, env, [Launch.EnvOfflineXuid, Launch.EnvOfflineGamertag]);
        var crashContext = new CrashContext(_clock);
        var startedUtc = _clock.GetUtcNow();
        CrashDumpSnapshot? dumpSnapshot = null;
        if (_config.CrashReports is not null)
            try { dumpSnapshot = CrashDumpCollector.Snapshot(_config.Install.Root); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { _emit(new SupervisorEvent.Note("crash: dump snapshot unavailable")); }
        IGameChild child;
        if (await CancelBeforeSpawnAsync(sessionId, _config.Saves?.FenceToken ?? fence, preparing.Token).ConfigureAwait(false)) return null;
        try
        {
            if (!_online.TryStart(() => _spawner.Spawn(plan), out child))
            {
                await CancelBeforeSpawnAsync(sessionId, _config.Saves?.FenceToken ?? fence, preparing.Token).ConfigureAwait(false);
                return null;
            }
        }
        catch
        {
            await LeaveQuietly(sessionId, "player", fence).ConfigureAwait(false);
            throw;
        }
        using (child)
        {
            state = state with { Phase = "spawned", ChildPid = child.Pid, ChildStarted = child.Started, SpawnedUnix = (ulong)_clock.GetUtcNow().ToUnixTimeSeconds() };
            Write(state);
            _emit(new SupervisorEvent.Spawned(child.Pid));
            // while the game runs: a fresh-start career's first save (CR-S51) and checkpoints at the server's cadence (OD122-L3)
            using var watchStop = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            var savesWatch = _config.Saves is { Watches: true } watched
                ? watched.WatchAsync(_config.SavesPoll, watchStop.Token) : Task.CompletedTask;
            uint code;
            try
            {
                (code, state) = await SuperviseAsync(child, pipe, uiPipe, endpoint, sessionId, token, active.Ticket.HeartbeatEveryMs, fence, state, crashContext, cancellation).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
            {
                // The shell is closing: leave the gate first, never touch the game (§1.4 Shutdown).
                var left = await LeaveQuietly(sessionId, "player", fence).ConfigureAwait(false);
                Write(state with { Phase = "ended", EndedUnix = (ulong)_clock.GetUtcNow().ToUnixTimeSeconds() });
                _emit(new SupervisorEvent.Ended("shutdown", left));
                throw;
            }
            finally
            {
                // the watcher ends before the exit commit, which then sends the first career if the watcher did not
                await watchStop.CancelAsync().ConfigureAwait(false);
                // nothing the watcher throws may skip the drain, the exit commit and the gate's leave that follow
#pragma warning disable CA1031
                try { await savesWatch.ConfigureAwait(false); }
                catch (Exception e) { _emit(new SupervisorEvent.Note("save: watch ended (" + e.GetType().Name + ")")); }
#pragma warning restore CA1031
            }
            {
                await DrainJob(child).ConfigureAwait(false);
                var disposition = ExitDisposition.Classify(code);
                _emit(new SupervisorEvent.Exited(disposition));
                if (disposition is ExitDisposition.Crashed && _config.CrashReports is { } reports && _config.AccountId is { } account)
                {
                    try
                    {
                        var ended = crashContext.FrozenAt ?? _clock.GetUtcNow();
                        var dump = dumpSnapshot is null ? null : CrashDumpCollector.Capture(dumpSnapshot, checked((int)child.Pid), startedUtc, ended);
                        var incident = new CrashIncident(Guid.CreateVersion7().ToString(), account, sessionId, _config.Game, _config.Xuid,
                            ended, _config.LauncherVersion, _config.Install.HookVersion, _config.Install.Version, code,
                            dump?.Info, dump?.Reason.ToString() ?? "snapshot_unavailable", crashContext.Freeze());
                        reports.Save(incident, dump is null ? [] : dump.Bytes.AsSpan());
                        _emit(new SupervisorEvent.CrashCaptured(incident));
                    }
                    catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidOperationException or ArgumentException)
                    { _emit(new SupervisorEvent.Note("crash: diagnostic capture unavailable")); }
                }
                // the session's progress to the cloud before the leave, with the line's newest fence
                ulong? endRevision = null;
                if (_config.Saves is { } after)
                {
                    try
                    {
                        using var drain = new CancellationTokenSource(TimeSpan.FromSeconds(30));
                        endRevision = await after.AfterExitAsync(drain.Token).WaitAsync(drain.Token).ConfigureAwait(false);
                    }
                    catch (Exception e) when (e is ApiException or HttpRequestException or IOException or UnauthorizedAccessException or InvalidOperationException or OperationCanceledException)
                    {
                        _emit(new SupervisorEvent.Error("save: " + (e is ApiException api ? api.Code : e.Message)));
                    }
                }
                var leftGate = await LeaveQuietly(sessionId, "game_exit", _config.Saves?.FenceToken ?? fence, endRevision).ConfigureAwait(false);
                Write(state with
                {
                    Phase = disposition is ExitDisposition.Crashed ? "crashed" : "ended",
                    ExitCode = code,
                    EndedUnix = (ulong)_clock.GetUtcNow().ToUnixTimeSeconds(),
                });
                if (disposition is not ExitDisposition.Relaunch)
                {
                    _emit(new SupervisorEvent.Ended(disposition is ExitDisposition.Crashed c ? $"crashed 0x{c.Code:x8}" : "game_exit", leftGate));
                }
                return disposition;
            }
        }
    }

    private async Task<bool> CancelBeforeSpawnAsync(string sessionId, ulong? fence, CancellationToken cancellation)
    {
        if (!cancellation.IsCancellationRequested && !_online.IsPaused && !_cancelBeforeSpawn) return false;
        if (_online.IsPaused) EmitPause(await _online.Paused.ConfigureAwait(false));
        var left = await LeaveQuietly(sessionId, "player", fence).ConfigureAwait(false);
        _emit(new SupervisorEvent.Ended("queue_cancelled", left));
        return true;
    }

    /// <summary>The running session: accept, frames, heartbeats, commands, until the child exits.</summary>
    private async Task<(uint Code, SessionRecord State)> SuperviseAsync(
        IGameChild child, IControlPipe pipe, IControlPipe? uiPipe, LauncherEndpoint endpoint, string sessionId, GameToken token, uint heartbeatEveryMs, ulong? fence, SessionRecord state, CrashContext crashContext, CancellationToken cancellation)
    {
        using var stop = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        using var onlineStop = CancellationTokenSource.CreateLinkedTokenSource(stop.Token, _online.Cancellation);
        using var overlay = uiPipe is null ? null : new OverlayChannel(uiPipe, child.Pid, new Overlay.UiFrame.Status("", Overlay.UiTier.Free, Overlay.UiState.Running, ""));
        Task<bool>? overlayNext = null;
        Task? inboxRun = null; // T-L4: the invite inbox's poll (one per session, from the overlay's first hello)
        Task? connectionQuery = null;
        long? lastConnectionQuery = null;
        if (overlay is not null)
        {
            _ = overlay.RunAsync(child.Pid, child.Exited, _config.AcceptBound, stop.Token);
            overlayNext = overlay.Incoming.WaitToReadAsync(stop.Token).AsTask();
        }
        // the overlay's Worlds tab: answers off the loop, the seat renewed in the background
        var worlds = overlay is null ? null : _config.Worlds?.Invoke(() => _ticket);
        if (worlds is not null && Process.GameDefinition.ById(_config.Game) is { } worldsGame) worlds.HasPoolWorlds = worldsGame.HasPoolWorlds; // CR-S103
        if (worlds is not null && Process.GameDefinition.ById(_config.Game) is { } friendsGame) worlds.SessionGame = friendsGame.Id; // W15: "Add friend" names a profile of it
        // W7b: SessionReady frames waiting for the hook's SessionReadyResult, by nonce
        var sessionReadyWaits = new System.Collections.Concurrent.ConcurrentDictionary<uint, TaskCompletionSource<byte>>();
        var sessionReadyNonce = 0;
        if (worlds is not null)
        {
            overlay!.Quiet = () => worlds.InOfficialLobby; // OD-192: no overlay notice in an official lobby
            worlds.SendRelayContext=async (context,ct)=>
            {
                if(!endpoint.Greeted || endpoint.OnlineSuspended || _online.IsPaused)throw new IOException("Control context unavailable");
                if(context.Payload.Length==89)
                {
                    if(worlds.TrustedServerNow?.Invoke() is not {} serverTime)throw new IOException("Trusted server time unavailable");
                    await pipe.WriteAsync(new Frame.RelayClock((ulong)serverTime.ToUnixTimeSeconds()).Encode(),ct).ConfigureAwait(false);
                }
                var bytes=context.Encode();
                try {await pipe.WriteAsync(bytes,ct).ConfigureAwait(false);}
                finally {System.Security.Cryptography.CryptographicOperations.ZeroMemory(bytes);}
            };
            worlds.SendRelayCandidates=async (candidates,ct)=>
            {
                if(!endpoint.Greeted || endpoint.OnlineSuspended || _online.IsPaused)throw new IOException("Control context unavailable");
                await pipe.WriteAsync(candidates.Encode(),ct).ConfigureAwait(false);
            };

            // FollowHost (0x23): the bridge's queued move (OD-149's anti-cheat move) goes to the game's own matchmaking
            worlds.SendFollowHost=async (follow,ct)=>
            {
                if(!endpoint.Greeted || endpoint.OnlineSuspended || _online.IsPaused)throw new IOException("Control context unavailable");
                await pipe.WriteAsync(follow.Encode(),ct).ConfigureAwait(false);
            };
            // W7b (OD-206, CR-S99 §4, lobby spec §11.3-11.4): an accepted invite's session for the hook's Party GameSessionReady, after
            // the join was granted; only once the hook reported CaptureWhat.SessionReady (WorldsBridge.SessionReadyCapable)
            worlds.SendSessionReady=async (session,ct)=>
            {
                if(!endpoint.Greeted || endpoint.OnlineSuspended || _online.IsPaused)throw new IOException("Control context unavailable");
                var nonce=unchecked((uint)Interlocked.Increment(ref sessionReadyNonce));
                if(nonce==0)nonce=unchecked((uint)Interlocked.Increment(ref sessionReadyNonce)); // 0 is never a nonce
                var answer=new TaskCompletionSource<byte>(TaskCreationOptions.RunContinuationsAsynchronously);
                sessionReadyWaits[nonce]=answer;
                try
                {
                    await pipe.WriteAsync(new Frame.SessionReady(nonce,session.Scid,session.TemplateName,session.Name).Encode(),ct).ConfigureAwait(false);
                    return await answer.Task.WaitAsync(ct).ConfigureAwait(false);
                }
                finally {sessionReadyWaits.TryRemove(nonce,out _);}
            };
            // a lobby ask's seat that never played within WorldsBridge.SeatGiveUp: the game's FINDING SESSION is cancelled
            worlds.SendCancelMatchmaking=async ct=>
            {
                if(!endpoint.Greeted || endpoint.OnlineSuspended || _online.IsPaused)throw new IOException("Control context unavailable");
                await pipe.WriteAsync(new Frame.CancelMatchmaking().Encode(),ct).ConfigureAwait(false);
            };
            // AC-2: a seated Hardcore player's hook measures the other players' cars against these rules (only once it reported
            // CaptureWhat.Watch); the forced test episode exists only in a prerelease build (WatchTestSwitch)
            worlds.SendWatch=async (watch,ct)=>
            {
                if(!endpoint.Greeted || endpoint.OnlineSuspended || _online.IsPaused)throw new IOException("Control context unavailable");
                await pipe.WriteAsync(watch.Encode(),ct).ConfigureAwait(false);
            };
            worlds.WatchForceSpeed = Matchmaking.WatchTestSwitch.ForceSpeedFromEnvironment();
            if (worlds.WatchForceSpeed) _emit(new SupervisorEvent.Note("watch: TEST build: FI_TEST_FORCE_OBSERVATION=speed forces one speed episode"));
            worlds.CrashContext = crashContext;
            _ = SeatLoopAsync(worlds, overlay!, onlineStop.Token);
        }
        var exit = child.WaitForExitAsync(stop.Token);
        var frames = Channel.CreateUnbounded<ReceivedFrame>();
        var since = _clock.GetUtcNow();
        Task<AcceptResult>? accept = pipe.AcceptAsync(child.Pid, child.Exited, _config.AcceptBound, stop.Token);
        Task<bool>? nextFrame = null;
        var heartbeatEvery = TimeSpan.FromMilliseconds(Math.Max(heartbeatEveryMs, 5000));
        Task? heartbeat = Task.Delay(heartbeatEvery, _clock, stop.Token);
        Task<GateHeartbeatResponse>? beatRequest = null;
        var beatRetry = new OperationBackoff();
        var tokensOpen = true;
        var suspended = false;
        var hookGone = false;
        ulong pushedExp = 0; // the expiry of the last token handed to the hook
        var lastBeat = DateTimeOffset.MinValue;
        Task<bool>? quit = null;
        var command = _commands.WaitToReadAsync(stop.Token).AsTask();
        async Task SuspendAsync(OnlineSuspendReason reason, string message, OnlinePause? pause = null)
        {
            if (suspended) return;
            suspended = true; tokensOpen = false; heartbeat = null; _ticket = string.Empty;
            token = new GameToken(new Secret(string.Empty), 0);
            await onlineStop.CancelAsync().ConfigureAwait(false);
            _config.OnlineAccessLost?.Invoke();
            if (pause is not null) EmitPause(pause);
            // IPC is control, Notice is display only. Dropping a failed write triggers the new hook's fail-closed pipe path.
            if (endpoint.Suspend(reason) is { } frame)
            {
                using var writeBound = new CancellationTokenSource(TimeSpan.FromSeconds(2));
                try { await pipe.WriteAsync(frame.Encode(), writeBound.Token).WaitAsync(writeBound.Token).ConfigureAwait(false); }
                catch (Exception error) when (error is IOException or OperationCanceledException or ObjectDisposedException or TimeoutException)
                { pipe.Dispose(); hookGone = true; nextFrame = null; }
            }
            if (worlds is not null) _ = worlds.SuspendOnlineAsync();
            if (overlay is not null)
            {
                await overlay.SendFrameAsync(new Overlay.UiFrame.WorldAccess(false, message), stop.Token).ConfigureAwait(false);
                await overlay.SetStatusAsync(new Overlay.UiFrame.Status("", Overlay.UiTier.Free, Overlay.UiState.Offline, message), stop.Token).ConfigureAwait(false);
            }
            await Notice(pipe, endpoint, overlay, 2, message, stop.Token).ConfigureAwait(false);
        }
        try
        {
            while (true)
            {
                if (!suspended && (_online.IsPaused || worlds?.OnlineActivity.IsPaused == true))
                {
                    var pause = _online.IsPaused ? await _online.Paused.ConfigureAwait(false) : await worlds!.OnlineActivity.Paused.ConfigureAwait(false);
                    _online.TryPause(pause);
                    await SuspendAsync(OnlineSuspendReason.Policy, PauseText(pause), pause).ConfigureAwait(false);
                }
                var waits = new List<Task> { exit, command };
                if (accept is not null) waits.Add(accept);
                if (nextFrame is not null) waits.Add(nextFrame);
                if (heartbeat is not null) waits.Add(heartbeat);
                if (beatRequest is not null) waits.Add(beatRequest);
                if (!suspended) { waits.Add(_online.Paused); if (worlds is not null) waits.Add(worlds.OnlineActivity.Paused); }
                if (quit is not null) waits.Add(quit);
                if (overlayNext is not null) waits.Add(overlayNext);
                var done = await Task.WhenAny(waits).ConfigureAwait(false);
                if (done == exit)
                {
                    crashContext.Freeze(child.ExitTimeUtc);
                    // the hook's last frames can arrive together with its exit: the display-only ones still count
                    if (nextFrame is not null && endpoint.Greeted) await DrainPresenceAsync(frames.Reader).ConfigureAwait(false);
                    var exitCode = await exit.ConfigureAwait(false);
                    return (exitCode, state);
                }
                if (done == accept)
                {
                    var result = await accept.ConfigureAwait(false);
                    accept = null;
                    if (result == AcceptResult.Connected)
                    {
                        StartReader(pipe.Stream, frames.Writer, _clock);
                        nextFrame = frames.Reader.WaitToReadAsync(stop.Token).AsTask();
                    }
                    else if (result == AcceptResult.TimedOut)
                    {
                        _emit(new SupervisorEvent.WaitingForHook((int)(_clock.GetUtcNow() - since).TotalSeconds));
                        accept = pipe.AcceptAsync(child.Pid, child.Exited, _config.AcceptBound, stop.Token);
                    }
                    // ChildExited: the exit task completes next.
                }
                else if (done == nextFrame)
                {
                    if (!await nextFrame.ConfigureAwait(false))
                    {
                        worlds?.GameReadiness.Invalidate();
                        if (overlay is not null) await overlay.SendFrameAsync(new Overlay.UiFrame.GameReadiness(false), stop.Token).ConfigureAwait(false);
                        nextFrame = null; // The hook closed the pipe; the process handle stays the truth.
                        hookGone = true;
                        _emit(new SupervisorEvent.HookOffline());
                        await SuspendAsync(OnlineSuspendReason.ControlLost, SessionEndedText()).ConfigureAwait(false);
                        continue;
                    }
                    while (frames.Reader.TryRead(out var received))
                    {
                        if (!suspended && _online.IsPaused)
                        {
                            var pause = await _online.Paused.ConfigureAwait(false);
                            await SuspendAsync(OnlineSuspendReason.Policy, PauseText(pause), pause).ConfigureAwait(false);
                        }
                        var frame = received.Frame;
                        // the retired native mod runtime's report (OD-113; OD-232): no launch plans one, so nothing reads it
                        if (frame is Frame.ModsReport) continue;
                        if(frame is Frame.UdpDiagnostic diagnostic)
                        {
                            if(endpoint.Greeted && !suspended && tokensOpen)worlds?.RecordUdpDiagnostic(diagnostic);
                            continue;
                        }
                        if(frame is Frame.RelayNetworkHint hint)
                        {
                            if(endpoint.Greeted && !suspended && tokensOpen && worlds is not null)
                                _=worlds.NetworkHintAsync(hint,onlineStop.Token);
                            continue;
                        }
                        // AC-2: the hook's measurement of another player's car (only after a Watch frame); posted off this loop
                        if(frame is Frame.Observed observed)
                        {
                            if(endpoint.Greeted && !suspended && tokensOpen)worlds?.HookObserved(observed);
                            continue;
                        }
                        if(frame is Frame.Capture{What:CaptureWhat.Watch} watchCapability && endpoint.Greeted && worlds is not null)
                            worlds.WatchCapable=watchCapability.Ok;
                        // NR-5: the hook measures speed against the per-car floor and reports the held speed and the car
                        if(frame is Frame.Capture{What:CaptureWhat.WatchPerCar} perCarCapability && endpoint.Greeted && worlds is not null)
                            worlds.WatchPerCarCapable=perCarCapability.Ok;
                        // NR-6: another player's part rows, read by the hook (only after a Watch asking for them); judged by the bridge
                        if(frame is Frame.PlayerParts playerParts)
                        {
                            if(endpoint.Greeted && !suspended && tokensOpen)worlds?.HookPlayerParts(playerParts);
                            continue;
                        }
                        if(frame is Frame.Capture{What:CaptureWhat.Parts} partsCapability && endpoint.Greeted && worlds is not null)
                            worlds.PartsCapable=partsCapability.Ok;
                        // T-L6 (OD-204): the hook can hold the game's private create; answered Capture{12, 1} only when this session's
                        // server took features: ["private_official_v1"] (else the hook keeps today's path and the write goes on)
                        if (frame is Frame.Capture { What: CaptureWhat.PrivateCreate } privateCapability && endpoint.Greeted && worlds is not null
                            && !suspended && tokensOpen && PrivateCreateReply(privateCapability, _privateOfficial) is { } reply)
                        {
                            try
                            {
                                await pipe.WriteAsync(reply.Encode(), stop.Token).ConfigureAwait(false);
                                worlds.PrivateCreateCapable = true;
                                worlds.PrivateCreateActivity = Process.GameDefinition.ById(_config.Game)?.PrivateCreateActivity; // CR-S99
                                _emit(new SupervisorEvent.Note("worlds: private creates on (the hook holds the game's private session write)"));
                            }
                            catch (Exception error) when (error is IOException or ObjectDisposedException or TimeoutException)
                            { _emit(new SupervisorEvent.Note("worlds: private creates off (Capture 12 not sent: " + error.GetType().Name + ")")); }
                        }
                        // T-L5: the hook takes FollowHost's move flag (an accepted invite moves a game already in online free roam)
                        if (frame is Frame.Capture { What: CaptureWhat.FollowMove } followMove && endpoint.Greeted && worlds is not null)
                            worlds.FollowMoveCapable = followMove.Ok;
                        // W7b: the hook fires an accepted invite's session as the game's Party GameSessionReady (FM5's route 1)
                        if (frame is Frame.Capture { What: CaptureWhat.SessionReady } sessionReadyCapability && endpoint.Greeted && worlds is not null)
                            worlds.SessionReadyCapable = sessionReadyCapability.Ok;
                        if (frame is Frame.SessionReadyResult sessionReadyResult && endpoint.Greeted)
                        {
                            _emit(new SupervisorEvent.Note($"worlds: invite: the hook answered SessionReady {sessionReadyResult.Nonce}: result "
                                + $"{sessionReadyResult.Result} (0x{sessionReadyResult.HResult:x8})"));
                            if (sessionReadyWaits.TryGetValue(sessionReadyResult.Nonce, out var waiting)) waiting.TrySetResult(sessionReadyResult.Result);
                            continue;
                        }
                        if (frame is Frame.PrivateSession privateSession && endpoint.Greeted)
                        {
                            worlds?.PrivateSessionWritten(privateSession.Scid, privateSession.TemplateName, privateSession.Name);
                            continue;
                        }
                        // T-L4 (OD-204, WORLDS_OFFICIAL §12.5): the game's Invite friends, completed in the hook: the friend picker or a notice
                        if (frame is Frame.InviteFriends && endpoint.Greeted)
                        {
                            if (!suspended && tokensOpen && worlds is not null && overlay is not null)
                                _ = InviteFriendsAsync(worlds, overlay, fromGame: true, onlineStop.Token);
                            else _emit(new SupervisorEvent.Note("worlds: invite: the game's Invite friends with no online lobby session; nothing to open"));
                            continue;
                        }
                        try { state = await OnFrame(frame, endpoint, pipe, token, state, tokensOpen ? onlineStop.Token : stop.Token).ConfigureAwait(false); }
                        catch (OperationCanceledException) when (_online.IsPaused && !stop.IsCancellationRequested)
                        { pipe.Dispose(); hookGone = true; nextFrame = null; break; }
                        // CR-L40: FREE ROAM ONLINE waits in the hook until a lobby is ready; answered off this loop
                        if (frame is Frame.FreeRoamNeed need && endpoint.Greeted)
                        {
                            var bridge = worlds;
                            var ensure = !suspended && tokensOpen && !_online.IsPaused && bridge is not null
                                ? token => bridge.EnsureFreeRoamWorldAsync(need.Kind, token) : (Func<CancellationToken, Task<byte>>?)null;
                            // off the reader loop from the first line: the access check may do synchronous work
                            var (freeRoamOnline, freeRoamStop) = (onlineStop.Token, stop.Token);
                            _ = Task.Run(() => AnswerFreeRoamAsync(ensure, need.Nonce, pipe.WriteAsync, FreeRoamBudget, _clock,
                                text => _emit(new SupervisorEvent.Note(text)), freeRoamOnline, freeRoamStop));
                        }
                        if (frame is Frame.Event { Kind: HookEvents.GameReadiness } && worlds is not null)
                        {
                            ObserveReadiness(received, worlds.GameReadiness);
                            if (overlay is not null) await overlay.SendFrameAsync(new Overlay.UiFrame.GameReadiness(worlds.GameReadiness.CurrentGeneration is not null), stop.Token).ConfigureAwait(false);
                        }
                        // OD-73: the hook's notices (Event 6) are overlay toasts, not pause-menu lines
                        if (frame is Frame.Event { Kind: HookEvents.Notice } notice && overlay is not null
                            && HookEvents.NoticeToast(notice.Detail, _config.Text) is { } noticeToast)
                        {
                            await overlay.ToastAsync(noticeToast, stop.Token).ConfigureAwait(false);
                        }
                        // OD-121: the server's idle countdown (Event 7 with evict_in) warns in the overlay
                        if (frame is Frame.Event { Kind: HookEvents.Activity } activity && overlay is not null && !suspended
                            && HookEvents.IdleWarning(activity.Detail, _config.Text) is { } idleToast)
                        {
                            await overlay.ToastAsync(idleToast, stop.Token).ConfigureAwait(false);
                        }
                        // OD-148 (refines OD-57): the game left free roam (the hook's Event 4). A car meet or an online
                        // Road Trip leaves it before asking for its lobby, so the world (or seat) is kept for
                        // WorldsBridge.FreeRoamLeftGrace; a FreeRoamNeed meanwhile keeps it, else the seat is left as before.
                        // Seat renewals go on meanwhile; the session's end still acts at once.
                        if (frame is Frame.Event { Kind: HookEvents.FreeRoamLeft } && worlds is not null)
                        {
                            _ = worlds.FreeRoamLeftAsync(stop.Token);
                        }
                        // OD-211 (CR-S102): the game's rich-presence screen (Event 9): the online toasts' "none while racing"
                        if (frame is Frame.Event { Kind: HookEvents.Screen } screen && worlds is not null)
                        {
                            worlds.ObserveScreen(screen.Detail);
                            // OD-223 revised: on the screens that take invites (FM5: its hoppers screen) a held invite becomes the accept
                            // toast, off them a notice again; never declined
                            if (worlds.Inbox is { } screenInbox && overlay is not null && inboxRun is not null)
                                _ = screenInbox.SyncScreenAsync(overlay.SendIfAcceptedAsync, onlineStop.Token);
                        }
                        // OD-208: the game left the online world on its own (its car database changed): the seat goes now
                        if (frame is Frame.Event { Kind: HookEvents.WorldLeft } left && worlds is not null)
                        {
                            _ = worlds.GameLeftOnlineAsync(left.Detail, stop.Token);
                        }
                        // OD-224 (T-1108-L): the game's lobby kind (Event 10, FM5): leaving its private match ends that seat at once
                        if (frame is Frame.Event { Kind: HookEvents.LobbyKind } lobbyKind && worlds is not null)
                        {
                            _ = worlds.ObserveLobbyKindAsync(lobbyKind.Detail, stop.Token);
                        }
                        // live (test.9): back from a Road Trip the game opens its free roam connection (Event 3) around
                        // closing the Road Trip one (Event 4); joining again calls off the pending close or leave
                        if (frame is Frame.Event { Kind: HookEvents.FreeRoamEntered } && worlds is not null)
                        {
                            // the game in its seat's world: the seat plays (no 15 s give-up). OD-208: any detail (an older hook's
                            // host:<port> too) is a guest's join, since players host no world
                            worlds.GameJoined();
                        }
                        if (frame is Frame.RefreshRequest && tokensOpen && !_online.IsPaused)
                        {
                            // A hook whose requests keep failing may ask again and again: one forced beat per
                            // ForcedBeatEvery; inside it the token in hand (≤ 10 min) is sent again
                            // without a call. (The 5 s cadence seen live was the local gate's
                            // heartbeatEveryMs, lease TTL 20 s / 5, not refresh requests.)
                            if (beatRequest is null && _clock.GetUtcNow() - lastBeat >= _config.ForcedBeatEvery)
                            {
                                heartbeat = Task.CompletedTask; // beat now
                            }
                            else if (endpoint.Greeted)
                            {
                                await pipe.WriteAsync(token.ToFrame().Encode(), stop.Token).ConfigureAwait(false);
                            }
                        }
                    }
                    nextFrame = frames.Reader.WaitToReadAsync(stop.Token).AsTask();
                }
                else if (done == heartbeat)
                {
                    heartbeat = null;
                    lastBeat = _clock.GetUtcNow();
                    if (tokensOpen && !_online.IsPaused)
                    {
                        try { beatRequest = _gate.HeartbeatAsync(sessionId, fence, onlineStop.Token); }
                        catch (Exception error) when (error is ApiException or HttpRequestException or TimeoutException or IOException or OperationCanceledException)
                        { beatRequest = Task.FromException<GateHeartbeatResponse>(error); }
                    }
                }
                else if (done == beatRequest)
                {
                    var request = beatRequest!; beatRequest = null;
                    try
                    {
                        var beat = await request.ConfigureAwait(false);
                        if (!tokensOpen || _online.IsPaused) continue; // A cancelled HTTP request may still return a late success.
                        beatRetry.Success();
                        fence = beat.Lease?.FenceToken ?? fence;
                        _config.Saves?.OnLease(beat.Lease);
                        token = new GameToken(new Secret(beat.Ticket.Token), Unix(beat.Ticket.ExpiresAt));
                        _ticket = beat.Ticket.Token;
                        if (!beat.GamePlane.Open)
                        {
                            await SuspendAsync(OnlineSuspendReason.SessionEnded, SessionEndedText()).ConfigureAwait(false);
                        }
                        else if (endpoint.Greeted && !hookGone && token.ExpiresUnix >= pushedExp + TokenPushEvery)
                        {
                            // a fresh token every heartbeat (5 s on the local stack) is not news to the hook:
                            // hand it over when the one it holds has aged TokenPushEvery (it lasts 10 minutes)
                            pushedExp = token.ExpiresUnix;
                            try
                            {
                                await pipe.WriteAsync(token.ToFrame().Encode(), stop.Token).ConfigureAwait(false);
                                _emit(new SupervisorEvent.TokenPushed(beat.Ticket.ExpiresAt));
                            }
                            catch (IOException)
                            {
                                // the hook closed its end (it refused the ticket and plays offline): say it once, stop writing
                                hookGone = true;
                                _emit(new SupervisorEvent.HookOffline());
                            }
                        }
                        state = state with { TicketExpUnix = token.ExpiresUnix };
                        if (tokensOpen)
                        {
                            heartbeat = Task.Delay(TimeSpan.FromMilliseconds(Math.Max(beat.Ticket.HeartbeatEveryMs, 5000)), _clock, stop.Token);
                        }
                    }
                    catch (ApiException error) when (OnlinePause.From(error) is { })
                    {
                        var pause = OnlinePause.From(error)!; _online.TryPause(pause);
                        await SuspendAsync(OnlineSuspendReason.Policy, PauseText(pause), pause).ConfigureAwait(false);
                    }
                    catch (ApiException e) when (e.Code is ErrorCodes.SessionsEnded or ErrorCodes.AccountsFrozen or ErrorCodes.GameUpdateRequired
                        or ErrorCodes.ContentIncompatible or ErrorCodes.ContentInconclusive or ErrorCodes.ContentRecheckRequired or ErrorCodes.ContentPolicyChanged)
                    {
                        if (e.Code.StartsWith("CONTENT.", StringComparison.Ordinal) || e.Code == ErrorCodes.GameUpdateRequired) _emit(new SupervisorEvent.Error(e.Code));
                        await SuspendAsync(e.Code == ErrorCodes.GameUpdateRequired ? OnlineSuspendReason.UpdateRequired : OnlineSuspendReason.SessionEnded, e.Code == ErrorCodes.AccountsFrozen
                            ? _config.Text?["crash.frozen"] ?? "Online access is temporarily frozen for security review. Sign-in and support remain available."
                            : e.Code == ErrorCodes.GameUpdateRequired
                                ? _config.Text?["updater.session_required"] ?? "Update Infinity Launcher after this game session to return online. Your progress keeps saving."
                                : e.Code == ErrorCodes.SessionsEnded && SessionEndedReason(e) == "idle" ? IdleEndedText()
                                : Shell.KickCooldown.IsKickedEnd(e) ? KickedEndedText() : SessionEndedText()).ConfigureAwait(false);
                    }
                    catch (OperationCanceledException) when (suspended || _online.IsPaused) { }
                    catch (Exception e) when (OperationBackoff.Transient(e) && !stop.IsCancellationRequested)
                    {
                        // A missed beat: try again soon; the ticket in hand lasts 10 minutes.
                        _emit(new SupervisorEvent.Error($"heartbeat: {(e is ApiException api ? api.Code : e.Message)}"));
                        if (tokensOpen && !_online.IsPaused) heartbeat = Task.Delay(beatRetry.Failed(e), _clock, onlineStop.Token);
                    }
                    catch (ApiException error)
                    {
                        _emit(new SupervisorEvent.Error(error.Code));
                        await SuspendAsync(OnlineSuspendReason.SessionEnded, SessionEndedText()).ConfigureAwait(false);
                    }
                }
                else if (done == command)
                {
                    if (!await command.ConfigureAwait(false))
                    {
                        command = new TaskCompletionSource<bool>().Task; // the shell closed its writer: no more commands
                        continue;
                    }
                    while (_commands.TryRead(out var next))
                    {
                        switch (next)
                        {
                            case SupervisorCommand.Quit when quit is null:
                                quit = child.QuitAsync(_config.QuitGrace, stop.Token);
                                break;
                            case SupervisorCommand.Notice n:
                                await Notice(pipe, endpoint, overlay, n.Level, n.Text, stop.Token).ConfigureAwait(false);
                                break;
                            case SupervisorCommand.OverlayStatus s when overlay is not null:
                                if (!suspended) await overlay.SetStatusAsync(s.Status, stop.Token).ConfigureAwait(false);
                                break;
                            case SupervisorCommand.OverlayToast toast when overlay is not null:
                                await overlay.ToastAsync(toast.Toast, stop.Token).ConfigureAwait(false);
                                break;
                            case SupervisorCommand.OverlayInvite invite when overlay is not null:
                                await overlay.SendIfAcceptedAsync(invite.Prompt, stop.Token).ConfigureAwait(false);
                                break;
                        }
                    }
                    command = _commands.WaitToReadAsync(stop.Token).AsTask();
                }
                else if (done == overlayNext)
                {
                    if (!await overlayNext.ConfigureAwait(false))
                    {
                        overlayNext = null; // the overlay went away; the game goes on
                        continue;
                    }
                    while (overlay!.Incoming.TryRead(out var incoming))
                    {
                        switch (incoming)
                        {
                            case Overlay.UiIncoming.Hello hello:
                                _emit(new SupervisorEvent.OverlayConnected(hello.OverlayVersion));
                                worlds?.FriendsOverlayHello(); // CR-S102: a new overlay has no friend avatar yet
                                await overlay.SendFrameAsync(new Overlay.UiFrame.GameReadiness(worlds?.GameReadiness.CurrentGeneration is not null), stop.Token).ConfigureAwait(false);
                                await overlay.SendFrameAsync(worlds is null
                                    ? new Overlay.UiFrame.WorldAccess(false, _config.Text?["worlds.integrity_inconclusive"] ?? "Online installation verification is unavailable. Return to the launcher for help.")
                                    : await worlds.AccessFrameAsync(stop.Token).ConfigureAwait(false), stop.Token).ConfigureAwait(false);
                                // OD-154: the voice settings at once, so the guide's Settings are right before any lobby
                                if (worlds?.VoiceFrame() is { } helloVoice) await overlay.SendFrameAsync(helloVoice, stop.Token).ConfigureAwait(false);
                                if (_config.OverlayStatus is { } overlayStatus)
                                    _ = overlayStatus.AttachAsync(overlay.SendFrameAsync, stop.Token);
                                // T-L4: the invite inbox polls while online once an overlay that shows prompts (protocol 6+) is there; every
                                // hello sends the pending prompts again (one sent before it was dropped: T-L3's open item)
                                if (worlds?.Inbox is { } inbox && !suspended && tokensOpen && overlay.Protocol >= 6)
                                {
                                    var inboxOverlay = overlay;
                                    var inboxStop = onlineStop.Token;
                                    inbox.Observer ??= worlds.OnInvitesPolledAsync; // W15: the friends sidebar's join requests and online toasts
                                    inbox.Holding ??= worlds.IsRacingNow; // INVITE_UX: no invite toast while racing; it comes after the race
                                    inbox.Acceptable ??= worlds.InviteScreenAllows; // OD-223 revised: off the game's invite screens, held as a notice
                                    inboxRun ??= Task.Run(() => inbox.RunAsync(inboxOverlay.SendIfAcceptedAsync, inboxStop), inboxStop);
                                    _ = inbox.ResendAsync(inboxOverlay.SendIfAcceptedAsync, inboxStop);
                                }
                                break;
                            case Overlay.UiIncoming.PickerAsk when worlds is not null && !suspended:
                                // T-L4: the Private Lobby pane's Invite friends
                                _ = InviteFriendsAsync(worlds, overlay, fromGame: false, onlineStop.Token);
                                break;
                            case Overlay.UiIncoming.InviteSend send when worlds is not null && !suspended:
                                // T-L4: a friend chosen in the picker: one invite, one notice, the picker refreshed
                                _ = InviteSendAsync(worlds, overlay, send.Request.Xuid, onlineStop.Token);
                                break;
                            case Overlay.UiIncoming.JoinModesSet joinModes when worlds is not null && !suspended:
                                // OD-210 (CR-S101, W14): the private lobby's leader chose who can join; one notice, the lobby again
                                _ = JoinModesSetAsync(worlds, overlay, joinModes.Request.Modes, onlineStop.Token);
                                break;
                            case Overlay.UiIncoming.FriendsAct friendsAct when worlds is not null && !suspended:
                                // OD-211 (CR-S102, W15): the right FRIENDS sidebar's action; one result, then the state as it is now
                                _ = FriendsActAsync(worlds, overlay, friendsAct.Request, onlineStop.Token);
                                break;
                            case Overlay.UiIncoming.FriendsAct friendsAct:
                                _ = overlay.SendIfAcceptedAsync(new Overlay.UiFrame.FriendsResult(friendsAct.Request.Kind, Overlay.UiFriendsResult.Failed,
                                    _config.Text?["friends.failed"] ?? "That did not work. Try again."), stop.Token);
                                break;
                            case Overlay.UiIncoming.Panel panel:
                                _config.OverlayStatus?.Panel(panel.Open);
                                break;
                            case Overlay.UiIncoming.Event { Kind: Overlay.UiEventKind.Quit } when quit is null:
                                quit = child.QuitAsync(_config.QuitGrace, stop.Token);
                                break;
                            case Overlay.UiIncoming.Event { Kind: Overlay.UiEventKind.ConnectionDetails, Detail: "" }
                                when (connectionQuery is null || connectionQuery.IsCompleted)
                                    && (lastConnectionQuery is null || _clock.GetElapsedTime(lastConnectionQuery.Value) >= TimeSpan.FromSeconds(5)):
                                lastConnectionQuery = _clock.GetTimestamp();
                                connectionQuery = SendConnectionDetailsAsync(overlay, suspended ? null : worlds, stop.Token, onlineStop.Token);
                                break;
                            case Overlay.UiIncoming.Event e:
                                _emit(new SupervisorEvent.OverlayEvent(e.Kind, e.Detail));
                                break;
                            case Overlay.UiIncoming.PlayerDetails details when worlds is not null:
                                // OD-112: the player card's pane, from what this launcher already holds (no request); a pane
                                // that cannot be encoded closes instead of leaving the overlay waiting.
                                _ = ReplyAsync(overlay, () => Task.FromResult<Overlay.UiFrame>(worlds.PlayerDetails(details.Request)), stop.Token,
                                    () => new Overlay.UiFrame.DetailPane(string.Empty, string.Empty, string.Empty, string.Empty, [], []), always: true);
                                break;
                            case Overlay.UiIncoming.PaneClick click when worlds is not null:
                                worlds.PaneClick(click.Request);
                                break;
                            case Overlay.UiIncoming.PaneClick click:
                                _emit(new SupervisorEvent.Note($"overlay: pane action {click.Request.ActionId} ignored"));
                                break;
                            case Overlay.UiIncoming.Mine when worlds is not null && !suspended:
                                // the world, then its voice chat (OD-154; a protocol 3 overlay only: the channel drops it for older ones)
                                // OD-181 (CR-L48): then each player's declared mods (a protocol 4 overlay only)
                                _ = ReplyAsync(overlay, async () => await worlds.MyWorldAsync(stop.Token).ConfigureAwait(false), stop.Token)
                                    .ContinueWith(async _ =>
                                    {
                                        await overlay.SendIfAcceptedAsync(worlds.RosterModsFrame(), stop.Token).ConfigureAwait(false);
                                        await overlay.SendIfAcceptedAsync(worlds.JoinModesFrame(), stop.Token).ConfigureAwait(false); // OD-210 (protocol 8)
                                        if (worlds.VoiceFrame() is { } voice) await overlay.SendFrameAsync(voice, stop.Token).ConfigureAwait(false);
                                    }, CancellationToken.None, TaskContinuationOptions.None, TaskScheduler.Default).Unwrap();
                                break;
                            case Overlay.UiIncoming.Voice v when worlds is not null && !suspended:
                                if (worlds.VoiceAct(v.Request) is { } voiceState) _ = overlay.SendFrameAsync(voiceState, stop.Token);
                                break;
                            case Overlay.UiIncoming.VoiceDevice d when worlds is not null && !suspended: // §15
                                if (worlds.VoiceDevice(d.Request) is { } deviceState) _ = overlay.SendFrameAsync(deviceState, stop.Token);
                                break;
                            case Overlay.UiIncoming.PlayerStateAsk ask when worlds?.PlayerActions is { } stateActions && !suspended:
                                // OD-193 F-2: the friend item of the game's PLAYER ACTIONS for this player
                                _ = ReplyAsync(overlay, async () => await stateActions.StateAsync(ask.Request, stop.Token).ConfigureAwait(false), stop.Token);
                                break;
                            case Overlay.UiIncoming.PlayerAct act when worlds?.PlayerActions is { } playerActions:
                                // OD-193 F-2: one friend action or report; always answered (paused: refused), so the game's popup says something
                                _ = ReplyAsync(overlay, async () => suspended
                                        ? new Overlay.UiFrame.PlayerActionDone(act.Request.Id, act.Request.Xuid,
                                            act.Request.Kind == Overlay.UiPlayerAction.Report ? Overlay.UiPlayerResult.ReportFailed : Overlay.UiPlayerResult.ActionFailed,
                                            Overlay.UiPlayerState.Unknown)
                                        : await playerActions.ActAsync(act.Request, stop.Token).ConfigureAwait(false), stop.Token, always: true);
                                break;
                            case Overlay.UiIncoming.PlayerAct unanswerable:
                                // no player actions in this session (offline, no matchmaking): the game's popup still says it failed
                                _ = ReplyAsync(overlay, () => Task.FromResult<Overlay.UiFrame>(new Overlay.UiFrame.PlayerActionDone(unanswerable.Request.Id,
                                    unanswerable.Request.Xuid, unanswerable.Request.Kind == Overlay.UiPlayerAction.Report ? Overlay.UiPlayerResult.ReportFailed
                                        : Overlay.UiPlayerResult.ActionFailed, Overlay.UiPlayerState.Unknown)), stop.Token, always: true);
                                break;
                            case Overlay.UiIncoming.InviteAnswer answer when worlds?.Inbox is { } answered:
                                // OD-204 (T-L4): Decline calls the decline route; T-L5: Accept joins the invite's private lobby and moves the game
                                // (paused or suspended: no lobby bridge, so an accept changes nothing)
                                _ = InviteAnswerAsync(answered, suspended ? null : worlds, overlay, answer.Answer, onlineStop.Token);
                                break;
                            case Overlay.UiIncoming.InviteAnswer answer:
                                _emit(new SupervisorEvent.Note($"overlay: invite {(answer.Answer.Accept ? "accepted" : "declined")} with no inbox in this session"));
                                break;
                            case Overlay.UiIncoming.Act a when worlds is not null && !suspended:
                                // the result, then the world as it is now
                                _ = ReplyAsync(overlay, async () => await worlds.ActAsync(a.Request, stop.Token).ConfigureAwait(false), stop.Token)
                                    .ContinueWith(_ => ReplyAsync(overlay, async () => await worlds.MyWorldAsync(stop.Token).ConfigureAwait(false), stop.Token),
                                        CancellationToken.None, TaskContinuationOptions.None, TaskScheduler.Default).Unwrap();
                                break;
                        }
                    }
                    overlayNext = overlay.Incoming.WaitToReadAsync(stop.Token).AsTask();
                }
                else if (done == quit)
                {
                    quit = null; // forced or not, the exit task reports the end
                }
            }
        }
        finally
        {
            await stop.CancelAsync().ConfigureAwait(false);
            if (worlds is not null)
            {
                using var cleanup = new CancellationTokenSource(TimeSpan.FromSeconds(10));
                try
                {
                    if (suspended) await worlds.SuspendOnlineAsync().WaitAsync(cleanup.Token).ConfigureAwait(false);
                    else await worlds.LeaveAsync(cleanup.Token).WaitAsync(cleanup.Token).ConfigureAwait(false);
                }
                catch (Exception error) when (error is ApiException or HttpRequestException or IOException or OperationCanceledException or TimeoutException) { }
            }
        }
    }

    private void EmitPause(OnlinePause pause)
    {
        if (Interlocked.Exchange(ref _pauseEmitted, 1) == 0) _emit(new SupervisorEvent.OnlinePaused(pause));
    }

    private string PauseText(OnlinePause pause) => pause.Reason == "budget"
        ? _config.Text?["online.budget"] ?? "Online play is paused because the service has reached its monthly budget. You can keep playing offline; your progress stays on this PC."
        : _config.Text?["online.operator"] ?? "Online services are temporarily paused. You can keep playing offline; your progress stays on this PC.";

    private async Task SendConnectionDetailsAsync(OverlayChannel overlay, Matchmaking.WorldsBridge? worlds, CancellationToken cancellation, CancellationToken onlineCancellation)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation, worlds is null ? cancellation : onlineCancellation);
        bound.CancelAfter(TimeSpan.FromSeconds(10));
        try
        {
            await overlay.SendFrameAsync(new Overlay.UiFrame.ConnectionDetails(0, _config.Text?["connection.checking"] ?? "Checking connection…"), bound.Token).ConfigureAwait(false);
            var result = worlds is null
                ? new Overlay.UiFrame.ConnectionDetails(2, _config.ConnectionEndpoints + "\n\n" + (_config.Text?["connection.unavailable"] ?? "Connection details unavailable."))
                : await worlds.ConnectionDetailsAsync(_config.ConnectionEndpoints, bound.Token).WaitAsync(bound.Token).ConfigureAwait(false);
            bound.Token.ThrowIfCancellationRequested();
            if (overlay.Ready) await overlay.SendFrameAsync(result, bound.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (!cancellation.IsCancellationRequested && !onlineCancellation.IsCancellationRequested)
        { /* The native checking state expires at the same ten-second bound. */ }
        catch (OperationCanceledException) { }
        catch (Exception error) when (error is IOException or ObjectDisposedException or TimeoutException) { }
    }

    /// <summary>
    /// One answer to the overlay. A frame that cannot be encoded (<see cref="ProtocolException"/>: past a cap, a bad
    /// pane) is logged and replaced by <paramref name="fallback"/> (a failed list, a closed pane) so the overlay never
    /// waits forever. <paramref name="always"/>: answered while online is paused too (no network behind it).
    /// </summary>
    private async Task ReplyAsync(OverlayChannel overlay, Func<Task<Overlay.UiFrame>> work, CancellationToken cancellation,
        Func<Overlay.UiFrame>? fallback = null, bool always = false)
    {
        try
        {
            var frame = await work().ConfigureAwait(false);
            if ((_online.IsPaused && !always) || cancellation.IsCancellationRequested) return;
            try
            {
                await overlay.SendFrameAsync(frame, cancellation).ConfigureAwait(false);
            }
            catch (ProtocolException error)
            {
                _emit(new SupervisorEvent.Note($"overlay: answer type 0x{frame.Type:x2} not sent: {error.Message}"));
                if (fallback is not null) await overlay.SendFrameAsync(fallback(), cancellation).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException)
        {
            // the session ended first
        }
        catch (ProtocolException error)
        {
            _emit(new SupervisorEvent.Note("overlay: answer not sent: " + error.Message));
        }
    }

    /// <summary>
    /// T-L4 (OD-204, WORLDS_OFFICIAL §12.5): Invite friends (the hook's InviteFriends, or the Private Lobby pane's 0x26): in a private
    /// lobby this player leads, the FRIENDS sidebar's invite mode (INVITE_UX, protocol 12: a fresh 0x2A, its avatars, then 0x2E), or the
    /// friend picker for an overlay of protocol 7-11; else one notice marked always.
    /// </summary>
    private async Task InviteFriendsAsync(Matchmaking.WorldsBridge worlds, OverlayChannel overlay, bool fromGame, CancellationToken cancellation)
    {
        try
        {
            var answer = await worlds.InviteFriendsAsync(fromGame, cancellation, sidebar: overlay.Protocol >= 12).ConfigureAwait(false);
            if (_online.IsPaused || cancellation.IsCancellationRequested) return;
            if (answer.Sidebar)
            {
                await overlay.SendIfAcceptedAsync(worlds.FriendsStateFrame(), cancellation).ConfigureAwait(false);
                await worlds.SendFriendAvatarsAsync(overlay.SendIfAcceptedAsync, cancellation).ConfigureAwait(false);
                await overlay.SendIfAcceptedAsync(new Overlay.UiFrame.FriendsOpen(Overlay.UiFriendsOpen.Invite), cancellation).ConfigureAwait(false);
            }
            if (answer.Picker is { } picker)
            {
                if (overlay.Accepts(picker)) await overlay.SendIfAcceptedAsync(picker, cancellation).ConfigureAwait(false);
                else _emit(new SupervisorEvent.Note($"worlds: invite: this overlay (protocol {overlay.Protocol}) has no friend picker"));
            }
            if (answer.Notice is { } notice) await overlay.ToastAsync(notice, cancellation).ConfigureAwait(false);
        }
        catch (Exception error) when (error is OperationCanceledException or ProtocolException or IOException or InvalidOperationException or HttpRequestException)
        {
            if (!cancellation.IsCancellationRequested) _emit(new SupervisorEvent.Note("worlds: invite: Invite friends not answered (" + error.GetType().Name + ")"));
        }
    }

    /// <summary>T-L4: a friend chosen in the picker: one invite call, one notice, the picker refreshed.</summary>
    private async Task InviteSendAsync(Matchmaking.WorldsBridge worlds, OverlayChannel overlay, string xuid, CancellationToken cancellation)
    {
        try
        {
            var answer = await worlds.InviteSendAsync(xuid, cancellation).ConfigureAwait(false);
            if (_online.IsPaused || cancellation.IsCancellationRequested) return;
            await overlay.ToastAsync(answer.Notice, cancellation).ConfigureAwait(false);
            if (answer.Picker is { } picker) await overlay.SendIfAcceptedAsync(picker, cancellation).ConfigureAwait(false);
        }
        catch (Exception error) when (error is OperationCanceledException or ProtocolException or IOException or InvalidOperationException or HttpRequestException)
        {
            if (!cancellation.IsCancellationRequested) _emit(new SupervisorEvent.Note("worlds: invite: not sent (" + error.GetType().Name + ")"));
        }
    }

    /// <summary>
    /// OD-211 (CR-S102, W15): one action of the FRIENDS sidebar (0x2B): the call, its result (0x2C, a notice that always shows), and the
    /// state (0x2A) as the lists are now.
    /// </summary>
    private async Task FriendsActAsync(Matchmaking.WorldsBridge worlds, OverlayChannel overlay, Overlay.UiFrame.FriendsAction action, CancellationToken cancellation)
    {
        try
        {
            var result = await worlds.FriendsActAsync(action, cancellation).ConfigureAwait(false);
            if (_online.IsPaused || cancellation.IsCancellationRequested) return;
            if (result is not null) await overlay.SendIfAcceptedAsync(result, cancellation).ConfigureAwait(false);
            await overlay.SendIfAcceptedAsync(worlds.FriendsStateFrame(), cancellation).ConfigureAwait(false);
            await worlds.SendFriendAvatarsAsync(overlay.SendIfAcceptedAsync, cancellation).ConfigureAwait(false); // CR-S102 (0x2D)
        }
        catch (Exception error) when (error is OperationCanceledException or ProtocolException or IOException or InvalidOperationException or HttpRequestException)
        {
            if (!cancellation.IsCancellationRequested) _emit(new SupervisorEvent.Note("friends: action not answered (" + error.GetType().Name + ")"));
        }
    }

    /// <summary>
    /// OD-210 (CR-S101, W14): the private lobby's "Who can join" from its leader: one <c>PUT …/modes</c>, one notice, then the lobby
    /// and its control as they are now (a changed one at once, the refused one as it was).
    /// </summary>
    private async Task JoinModesSetAsync(Matchmaking.WorldsBridge worlds, OverlayChannel overlay, byte choice, CancellationToken cancellation)
    {
        try
        {
            if (await worlds.SetJoinModesAsync(choice, cancellation).ConfigureAwait(false) is not { } answer) return;
            if (_online.IsPaused || cancellation.IsCancellationRequested) return;
            await overlay.ToastAsync(answer.Notice, cancellation).ConfigureAwait(false);
            await overlay.SendIfAcceptedAsync(answer.Frame ?? worlds.JoinModesFrame(), cancellation).ConfigureAwait(false);
        }
        catch (Exception error) when (error is OperationCanceledException or ProtocolException or IOException or InvalidOperationException or HttpRequestException)
        {
            if (!cancellation.IsCancellationRequested) _emit(new SupervisorEvent.Note("worlds: who can join: not answered (" + error.GetType().Name + ")"));
        }
    }

    /// <summary>
    /// T-L4: the player's answer to an invite prompt (Decline: the decline route). T-L5: Accept goes to the bridge
    /// (<see cref="Matchmaking.WorldsBridge.AcceptInviteAsync"/>: the join, the proof, FollowHost), and the prompt is withdrawn whatever
    /// the outcome (an accept that could not be served now comes back later from the inbox).
    /// </summary>
    private static async Task InviteAnswerAsync(Matchmaking.InviteInbox inbox, Matchmaking.WorldsBridge? worlds, OverlayChannel overlay,
        Overlay.UiFrame.InviteAnswer answer, CancellationToken cancellation)
    {
        try
        {
            var notice = await inbox.AnswerAsync(answer.InviteId, answer.Accept, cancellation,
                answer.Accept && worlds is not null ? worlds.AcceptInviteAsync : null).ConfigureAwait(false);
            if (cancellation.IsCancellationRequested) return;
            if (answer.Accept)
            {
                await overlay.SendIfAcceptedAsync(Overlay.UiFrame.InvitePrompt.Withdraw(answer.InviteId), cancellation).ConfigureAwait(false);
                // OD-223 revised: an accept that came off the invite screens stays held: shown again as a notice now
                await inbox.SyncScreenAsync(overlay.SendIfAcceptedAsync, cancellation).ConfigureAwait(false);
            }
            if (notice is not null) await overlay.ToastAsync(notice, cancellation).ConfigureAwait(false);
        }
        catch (Exception error) when (error is OperationCanceledException or ProtocolException or IOException)
        {
        }
    }

    /// <summary>Polls the seat at <see cref="Matchmaking.WorldsBridge.SeatLoopEvery"/>; each scheduler decides when work is due.</summary>
    private static async Task SeatLoopAsync(Matchmaking.WorldsBridge worlds, OverlayChannel overlay, CancellationToken cancellation)
    {
        while (!cancellation.IsCancellationRequested)
        {
            try
            {
                await Task.Delay(Matchmaking.WorldsBridge.SeatLoopEvery, cancellation).ConfigureAwait(false);
                await overlay.SendFrameAsync(await worlds.AccessFrameAsync(cancellation).ConfigureAwait(false), cancellation).ConfigureAwait(false);
                if (await worlds.TickAsync(cancellation).ConfigureAwait(false) is { } ended)
                {
                    await overlay.ToastAsync(new Overlay.UiFrame.Toast(Overlay.UiLevel.Warn, 2, 50, ended), cancellation).ConfigureAwait(false);
                }

                var mine = await worlds.PollMyWorldAsync(cancellation).ConfigureAwait(false);
                await overlay.SendFrameAsync(mine, cancellation).ConfigureAwait(false);
                await overlay.SendIfAcceptedAsync(worlds.RosterModsFrame(), cancellation).ConfigureAwait(false); // OD-181
                await overlay.SendIfAcceptedAsync(worlds.JoinModesFrame(), cancellation).ConfigureAwait(false); // OD-210 (W14)
                foreach (var toast in worlds.LobbyToasts()) await overlay.ToastAsync(toast, cancellation).ConfigureAwait(false);
                await worlds.FollowHostAsync(cancellation).ConfigureAwait(false);
                await worlds.WatchTickAsync(cancellation).ConfigureAwait(false); // AC-2: rules to the hook or world server, observations posted
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }
    }

    // T-L6: this session's server took features: ["private_official_v1"] (OneSessionAsync's admission)
    private bool _privateOfficial;

    /// <summary>
    /// T-L6 (OD-204): the launcher's answer to the hook's <see cref="CaptureWhat.PrivateCreate"/>: <c>Capture{12, 1}</c> back (the hook
    /// then holds the game's private create for FreeRoamNeed kinds 3/4) only when the hook reported it ok and this session's server
    /// took <c>private_official_v1</c>; else nothing, and the hook keeps today's path.
    /// </summary>
    internal static Frame.Capture? PrivateCreateReply(Frame.Capture hook, bool serverTookFeatures) =>
        hook is { What: CaptureWhat.PrivateCreate, Ok: true } && serverTookFeatures ? new Frame.Capture(CaptureWhat.PrivateCreate, true) : null;

    /// <summary>CR-L40: how long the launcher looks for a lobby before it answers <see cref="Frame.FreeRoamReady"/> (the hook waits 20 s).</summary>
    public static readonly TimeSpan FreeRoamBudget = TimeSpan.FromSeconds(18);

    /// <summary>
    /// CR-L40 (LOBBIES_AND_SERVER_STATUS.md §3): the hook's <see cref="Frame.FreeRoamNeed"/>, answered with the same nonce.
    /// <paramref name="ensure"/> (the bridge's <c>EnsureFreeRoamWorldAsync</c>) gets <paramref name="budget"/>; without it, or with
    /// online gone, the answer is <see cref="Matchmaking.FreeRoamResult.NotOnline"/>, and a failure or timeout is
    /// <see cref="Matchmaking.FreeRoamResult.None"/>. A failed write is dropped: the hook goes on at its own deadline.
    /// </summary>
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1031:Do not catch general exception types",
        Justification = "A fire-and-forget answer must never leave an unobserved task exception; the failure is logged by type only and answered None.")]
    internal static async Task<byte> AnswerFreeRoamAsync(Func<CancellationToken, Task<byte>>? ensure, uint nonce,
        Func<byte[], CancellationToken, Task> write, TimeSpan budget, TimeProvider clock, Action<string>? note,
        CancellationToken online, CancellationToken cancellation)
    {
        var result = Matchmaking.FreeRoamResult.NotOnline;
        if (ensure is not null && !online.IsCancellationRequested)
        {
            using var timeout = new CancellationTokenSource(budget, clock);
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(online, timeout.Token);
            try
            {
                result = await ensure(bound.Token).WaitAsync(bound.Token).ConfigureAwait(false);
                if (result > Matchmaking.FreeRoamResult.NotOnline) result = Matchmaking.FreeRoamResult.None;
            }
            catch (OperationCanceledException)
            {
                result = online.IsCancellationRequested ? Matchmaking.FreeRoamResult.NotOnline : Matchmaking.FreeRoamResult.None;
                note?.Invoke("free roam: no lobby within " + (int)budget.TotalSeconds + " s");
            }
            catch (Exception error)
            {
                result = Matchmaking.FreeRoamResult.None;
                note?.Invoke("free roam: the lobby search failed (" + error.GetType().Name + ")");
            }
        }
        try { await write(new Frame.FreeRoamReady(nonce, result).Encode(), cancellation).ConfigureAwait(false); }
        catch (Exception error) when (error is IOException or ObjectDisposedException or OperationCanceledException or TimeoutException) { }
        return result;
    }

    /// <summary>How often <see cref="WatchExtraOptionsModsAsync"/> reads ExtraOptions' log until the set settles.</summary>
    public static readonly TimeSpan ExtraOptionsLogPoll = TimeSpan.FromSeconds(2);

    /// <summary>
    /// Owner 2026-10-28: after the handshake, ExtraOptions' log of this run is read until it answers for every planned mod, and the
    /// lobby set is what it loaded; at <see cref="SupervisorConfig.ModReportDeadline"/> the set settles with what the log says.
    /// </summary>
    private async Task WatchExtraOptionsModsAsync(Matchmaking.ExtraOptionsModSetSource mods, CancellationToken cancellation)
    {
        var deadline = _clock.GetUtcNow() + _config.ModReportDeadline;
        try
        {
            while (!mods.Refresh())
            {
                if (_clock.GetUtcNow() >= deadline) { mods.Expire("timeout"); break; }
                await Task.Delay(ExtraOptionsLogPoll, _clock, cancellation).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) { return; }
        if (mods.State.Phase == Matchmaking.ModSetPhase.Failed) _emit(new SupervisorEvent.ModsFailed(mods.State.Reason ?? "not_loaded"));
    }

    private async Task<SessionRecord> OnFrame(Frame frame, LauncherEndpoint endpoint, IControlPipe pipe, GameToken token, SessionRecord state, CancellationToken cancellation)
    {
        var (replies, evt) = endpoint.Handle(frame, token);
        foreach (var reply in replies)
        {
            await pipe.WriteAsync(reply.Encode(), cancellation).ConfigureAwait(false);
        }
        switch (evt)
        {
            case EndpointEvent.HookConnected connected:
                _emit(new SupervisorEvent.Handshake(connected.HookVersion));
                state = state with { Phase = "running", HookVersion = connected.HookVersion };
                Write(state);
                if (replies is [Frame.Refused refused])
                {
                    _emit(new SupervisorEvent.Refused(refused.Reason));
                }
                else
                {
                    _emit(new SupervisorEvent.Running());
                    if (_config.Mods is Matchmaking.ExtraOptionsModSetSource extraOptionsMods) _ = WatchExtraOptionsModsAsync(extraOptionsMods, cancellation);
                    if (_config.Overlay is { } package)
                    {
                        // right after the handshake: the hook checks the file against this hash and loads it
                        await pipe.WriteAsync(new Frame.Overlay(true, package.Sha256, package.Path).Encode(), cancellation).ConfigureAwait(false);
                    }
                }
                break;
            case EndpointEvent.HookEvent e when e.Kind != HookEvents.GameReadiness && e.Kind != HookEvents.Screen: // both repeat; never a log line each
                _emit(new SupervisorEvent.HookEvent(e.Kind, e.Detail));
                break;
            case EndpointEvent.Capture c:
                _emit(new SupervisorEvent.Capture(c.What, c.Ok) { ReceivedTimestamp = TimeProvider.System.GetTimestamp() });
                break;
            // CR-L41.3: the Discord presence's level, credits and car; only emitted, nothing waits on them
            case EndpointEvent.ProgressReported progress:
                _emit(new SupervisorEvent.GameProgress(progress.Level, progress.Credits));
                break;
            case EndpointEvent.CarChanged car:
                _emit(new SupervisorEvent.GameCar(car.CarKey));
                break;
        }
        return state;
    }

    private string SessionEndedText() => _config.Text?["overlay.session_ended_body"]
        ?? "The online session ended. Progress keeps saving on this PC.";

    /// <summary>OD-121 (GATE_API.md §4a): the seat went to a waiting player; the next Play joins at the end of the queue.</summary>
    private string IdleEndedText() => _config.Text?["overlay.session_ended_idle"]
        ?? "You were away while players waited, so your seat went to the next in line. Progress keeps saving on this PC. Close the game and press Play to queue again.";

    /// <summary>CR-S62 (GATE_API.md §4): a moderator ended the session; online is suspended as after an idle end, never re-queued.</summary>
    private string KickedEndedText() => _config.Text?["overlay.session_ended_kicked"]
        ?? "You were removed by a moderator. The game keeps running offline and progress keeps saving on this PC.";

    private static string? SessionEndedReason(ApiException error)
    {
        try
        {
            return error.Details is { ValueKind: System.Text.Json.JsonValueKind.Object } details && details.TryGetProperty("reason", out var value)
                && value.ValueKind == System.Text.Json.JsonValueKind.String ? value.GetString() : null;
        }
        catch (InvalidOperationException)
        {
            return null;
        }
    }

    internal async Task Notice(IControlPipe pipe, LauncherEndpoint endpoint, OverlayChannel? overlay, byte level, string text, CancellationToken cancellation)
    {
        _emit(new SupervisorEvent.NoticeSent(level, text)); // visible launcher fallback even if either pipe fails
        if (overlay is not null)
        {
            await overlay.ToastAsync(Overlay.OverlayNotices.Create(level, text, _config.Text), cancellation).ConfigureAwait(false);
        }
        if (endpoint.Greeted)
        {
            try { await pipe.WriteAsync(new Frame.Notice(level, text).Encode(), cancellation).ConfigureAwait(false); }
            catch (Exception error) when (error is IOException or ObjectDisposedException or TimeoutException) { /* Display never substitutes for suspension control. */ }
        }
    }

    /// <summary>The pipe's reader on its own thread: one thread never both blocks on a read and heartbeats.</summary>
    internal sealed record ReceivedFrame(Frame Frame, long Timestamp);

    internal static void ObserveReadiness(ReceivedFrame received, GameReadiness readiness)
    {
        if (received.Frame is Frame.Event { Kind: HookEvents.GameReadiness } sample)
            readiness.Observe(sample.Detail, received.Timestamp);
    }

    /// <summary>
    /// After the game's exit, the frames its hook sent last (the pipe drains within a moment, ≤ 1 s): only the display ones are
    /// emitted (CR-L41.3's progress and car, a capture report), since nothing waits on them; a frame that needs an answer or
    /// changes the session means nothing any more.
    /// </summary>
    private async Task DrainPresenceAsync(ChannelReader<ReceivedFrame> frames)
    {
        await Task.WhenAny(frames.Completion, Task.Delay(TimeSpan.FromSeconds(1), TimeProvider.System)).ConfigureAwait(false);
        while (frames.TryRead(out var received))
        {
            if (received.Frame is Frame.Progress progress) _emit(new SupervisorEvent.GameProgress(progress.Level, progress.Credits));
            else if (received.Frame is Frame.Car car) _emit(new SupervisorEvent.GameCar(car.CarKey));
            else if (received.Frame is Frame.Capture capture) _emit(new SupervisorEvent.Capture(capture.What, capture.Ok) { ReceivedTimestamp = received.Timestamp });
        }
    }

    internal static void StartReader(Stream stream, ChannelWriter<ReceivedFrame> writer, TimeProvider clock) =>
        _ = Task.Factory.StartNew(() =>
        {
            try
            {
                foreach (var frame in Frames.ReadAll(stream, Frame.Decode))
                {
                    writer.TryWrite(new ReceivedFrame(frame, clock.GetTimestamp()));
                }
                writer.TryComplete();
            }
            catch (ProtocolException e)
            {
                writer.TryComplete(e);
            }
        }, CancellationToken.None, TaskCreationOptions.LongRunning, TaskScheduler.Default);

    /// <summary>A relay that outlives the game a moment must not race the leave: wait ≤ JobDrain for the job to empty.</summary>
    private async Task DrainJob(IGameChild child)
    {
        using var bound = new CancellationTokenSource(_config.JobDrain, _clock);
        try
        {
            await foreach (var e in child.JobEvents.ReadAllAsync(bound.Token).ConfigureAwait(false))
            {
                if (e.Message == JobMessage.ActiveProcessZero)
                {
                    return;
                }
            }
        }
        catch (OperationCanceledException)
        {
            // Stragglers: logged by the shell, ignored here.
        }
    }

    private async Task<bool> LeaveQuietly(string sessionId, string reason, ulong? fence, ulong? endRevision = null)
    {
        try
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(10));
            await _gate.LeaveAsync(sessionId, reason, fence, endRevision, bound.Token).ConfigureAwait(false);
            return true;
        }
        catch (Exception e) when (e is ApiException or HttpRequestException or OperationCanceledException or IOException or ObjectDisposedException)
        {
            return false; // The gate slot lapses after 5 minutes without heartbeats (ADR 0015).
        }
    }

    /// <summary>Seconds of token lifetime gained before a newer one is handed to the hook.</summary>
    private const ulong TokenPushEvery = 120;

    private static ulong Unix(string instant) => (ulong)Math.Max(0, TokenManager.ParseInstant(instant).ToUnixTimeSeconds());

    private void Write(SessionRecord state) => SessionRecord.Write(_config.StateDirectory, state);
}

/// <summary>
/// <c>&lt;state&gt;\session.json</c> (ORCHESTRATOR §2.4), rewritten atomically at every phase change.
/// Never a bearer token: the launch ticket's claims are public and the game ticket is re-minted.
/// </summary>
public sealed record SessionRecord(
    [property: JsonPropertyName("sid")] string Sid,
    [property: JsonPropertyName("game")] string Game,
    [property: JsonPropertyName("xuid")] string Xuid,
    [property: JsonPropertyName("nonce")] string Nonce)
{
    public const string FileName = "session.json";

    [JsonPropertyName("version")] public int Version { get; init; } = 1;

    [JsonPropertyName("phase")] public string Phase { get; init; } = "admitting";

    [JsonPropertyName("child_pid")] public uint? ChildPid { get; init; }

    [JsonPropertyName("child_started")] public ulong? ChildStarted { get; init; }

    [JsonPropertyName("spawned_unix")] public ulong? SpawnedUnix { get; init; }

    [JsonPropertyName("hook_version")] public string? HookVersion { get; init; }

    [JsonPropertyName("ticket_exp_unix")] public ulong? TicketExpUnix { get; init; }

    [JsonPropertyName("exit_code")] public uint? ExitCode { get; init; }

    [JsonPropertyName("ended_unix")] public ulong? EndedUnix { get; init; }

    public static void Write(string stateDirectory, SessionRecord state)
    {
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        var temp = path + ".tmp";
        File.WriteAllBytes(temp, JsonSerializer.SerializeToUtf8Bytes(state));
        File.Move(temp, path, overwrite: true);
    }

    public static SessionRecord? Read(string stateDirectory)
    {
        try
        {
            return JsonSerializer.Deserialize<SessionRecord>(File.ReadAllBytes(Path.Combine(stateDirectory, FileName)));
        }
        catch (Exception e) when (e is IOException or JsonException)
        {
            return null;
        }
    }
}
