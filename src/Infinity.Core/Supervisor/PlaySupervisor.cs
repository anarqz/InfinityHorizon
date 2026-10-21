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
    public OnlineActivity? OnlineActivity { get; init; }
    /// <summary>First hook supporting authenticated terminal post-handshake suspension.</summary>
    public uint MinimumOnlineHookBuild { get; init; } = 76;
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

    /// <summary>The overlay's Worlds tab (OD-54): the bridge for a session, given its current game ticket; null: none.</summary>
    public Func<Func<string>, Matchmaking.WorldsBridge>? Worlds { get; init; }

    /// <summary>The cloud save around the game (OD-52, OD-60); null: no cloud save step.</summary>
    public Saves.SessionSaves? Saves { get; init; }

    /// <summary>The shell's English/pt-BR catalog for notices; headless callers use English.</summary>
    public Strings? Text { get; init; }
    public CrashIncidentStore? CrashReports { get; init; }
    public string? AccountId { get; init; }
    public string CarDbSha256 { get; init; } = "";
    public Func<string, CancellationToken, Task>? PrepareAdmission { get; init; }
    public Action? OnlineAccessLost { get; init; }
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

    public sealed record Error(string Message) : SupervisorEvent;
    public sealed record OnlinePaused(OnlinePause Pause) : SupervisorEvent;

    /// <summary>Something the launcher decided for this launch, for the log (Wine overrides, ...).</summary>
    public sealed record Note(string Text) : SupervisorEvent;

    /// <summary>The hook stopped listening (it refused the ticket and went offline, or its pipe broke): the game plays offline.</summary>
    public sealed record HookOffline : SupervisorEvent;

    /// <summary>The overlay said hello on the UI pipe (its version).</summary>
    public sealed record OverlayConnected(string Version) : SupervisorEvent;

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
                _emit(new SupervisorEvent.Error(e is ApiException api ? api.Code : e.Message));
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

    /// <summary>One gate session and one game process; null when it ended without a game exit.</summary>
    private async Task<ExitDisposition?> OneSessionAsync(string? initialSessionId, CancellationToken cancellation)
    {
        using var preparing = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _online.Cancellation);
        if (_online.IsPaused) { EmitPause(await _online.Paused.ConfigureAwait(false)); return null; }
        var handoff = new Handoff(HandoffChannel.Pipe, Launch.NewNonce());
        // The pipe exists before the child (ORCHESTRATOR §1.2): the hook's 5 s wait always finds it.
        using var pipe = _pipes(handoff);
        // the UI pipe too, before the child: the overlay connects once the hook loaded it
        using var uiPipe = _config.Overlay is not null && _uiPipes is not null ? _uiPipes(handoff) : null;
        var admission = await new GateAdmission(_gate, _emit, _clock).WaitAsync(
            id => new PlaySessionCreate(id, _config.Game, _config.Xuid, handoff.NonceHex, _config.LauncherVersion, _config.Install.HookSha256, _config.CarDbSha256, 0, false),
            preparing.Token, _config.PrepareAdmission, initialSessionId).ConfigureAwait(false);
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
        Write(state);

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
                _emit(new SupervisorEvent.Error("save: " + (e is Saves.SaveBlockedException blocked ? blocked.Code : e is ApiException api ? api.Code : e.Message)));
                _emit(new SupervisorEvent.Ended("save_blocked", leftBlocked));
                return null;
            }
        }

        var token = new GameToken(new Secret(active.Ticket.Token), Unix(active.Ticket.ExpiresAt));
        var endpoint = new LauncherEndpoint(new Admission.Ticket(claims, signature), _config.MinimumOnlineHookBuild);
        List<(string, string)> env = [(Launch.EnvLaunch, handoff.EnvValue), (Launch.EnvLauncherExe, _config.LauncherExe)];
        // Wine / Proton: ExtraOptions' winmm.dll next to azimuth.exe loads only with winmm=n,b
        if (WineHost.Detected)
        {
            var winmm = File.Exists(Path.Combine(_config.Install.Root, "winmm.dll"));
            var inherited = Environment.GetEnvironmentVariable("WINEDLLOVERRIDES");
            var overrides = winmm ? WineHost.WithWinmm(inherited) : null;
            if (overrides is not null)
            {
                env.Add(("WINEDLLOVERRIDES", overrides));
            }
            // the environment alone does not reach Wine's loader in the child: the per-app registry override does
            var appDefaults = WineHost.EnsureWinmmOverride(winmm);
            _emit(new SupervisorEvent.Note($"wine ({WineHost.How}): winmm.dll next to azimuth.exe {(winmm ? "yes" : "no")}; " +
                $"AppDefaults azimuth.exe winmm=native,builtin {appDefaults}; " +
                $"WINEDLLOVERRIDES for the game: {overrides ?? inherited ?? "(unset)"}{(overrides is null ? " (unchanged)" : "")}"));
        }
        var plan = new LaunchPlan(_config.Install.HostExe, [], _config.Install.Root, env, []);
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

    private async Task<bool> CancelBeforeSpawnAsync(string sessionId, ulong? fence, CancellationToken cancellation)
    {
        if (!cancellation.IsCancellationRequested && !_online.IsPaused) return false;
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
        if (overlay is not null)
        {
            _ = overlay.RunAsync(child.Pid, child.Exited, _config.AcceptBound, stop.Token);
            overlayNext = overlay.Incoming.WaitToReadAsync(stop.Token).AsTask();
        }
        // the overlay's Worlds tab: answers off the loop, the seat renewed in the background
        var worlds = overlay is null ? null : _config.Worlds?.Invoke(() => _ticket);
        if (worlds is not null)
        {
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
                    return (await exit.ConfigureAwait(false), state);
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
                        try { state = await OnFrame(frame, endpoint, pipe, token, state, tokensOpen ? onlineStop.Token : stop.Token).ConfigureAwait(false); }
                        catch (OperationCanceledException) when (_online.IsPaused && !stop.IsCancellationRequested)
                        { pipe.Dispose(); hookGone = true; nextFrame = null; break; }
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
                        // OD-57: the world closes when its host leaves free roam (the hook''s Event 4)
                        if (frame is Frame.Event { Kind: HookEvents.FreeRoamLeft } && worlds?.HostedWorldId is not null)
                        {
                            _ = worlds.CloseHostAsync(stop.Token);
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
                                : SessionEndedText()).ConfigureAwait(false);
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
                                await overlay.SendFrameAsync(new Overlay.UiFrame.GameReadiness(worlds?.GameReadiness.CurrentGeneration is not null), stop.Token).ConfigureAwait(false);
                                await overlay.SendFrameAsync(worlds is null
                                    ? new Overlay.UiFrame.WorldAccess(false, _config.Text?["worlds.integrity_inconclusive"] ?? "Online installation verification is unavailable. Return to the launcher for help.")
                                    : await worlds.AccessFrameAsync(stop.Token).ConfigureAwait(false), stop.Token).ConfigureAwait(false);
                                break;
                            case Overlay.UiIncoming.Event { Kind: Overlay.UiEventKind.Quit } when quit is null:
                                quit = child.QuitAsync(_config.QuitGrace, stop.Token);
                                break;
                            case Overlay.UiIncoming.Event e:
                                _emit(new SupervisorEvent.OverlayEvent(e.Kind, e.Detail));
                                break;
                            case Overlay.UiIncoming.Worlds w when worlds is not null && !suspended:
                                _ = ReplyAsync(overlay, async () => await worlds.ListAsync(w.Query, stop.Token).ConfigureAwait(false), stop.Token);
                                break;
                            case Overlay.UiIncoming.Join j when worlds is not null && !suspended:
                                _ = ReplyAsync(overlay, async () => await worlds.JoinAsync(j.Request, stop.Token).ConfigureAwait(false), stop.Token);
                                break;
                            case Overlay.UiIncoming.Create c when worlds is not null && !suspended:
                                _ = ReplyAsync(overlay, async () => await worlds.CreateAsync(c.Request, stop.Token).ConfigureAwait(false), stop.Token);
                                break;
                            case Overlay.UiIncoming.Mine when worlds is not null && !suspended:
                                _ = ReplyAsync(overlay, async () => await worlds.MyWorldAsync(stop.Token).ConfigureAwait(false), stop.Token);
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
                    else await Task.WhenAll(worlds.LeaveAsync(cleanup.Token), worlds.CloseHostAsync(cleanup.Token)).WaitAsync(cleanup.Token).ConfigureAwait(false);
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

    private async Task ReplyAsync(OverlayChannel overlay, Func<Task<Overlay.UiFrame>> work, CancellationToken cancellation)
    {
        try
        {
            var frame = await work().ConfigureAwait(false);
            if (_online.IsPaused || cancellation.IsCancellationRequested) return;
            await overlay.SendFrameAsync(frame, cancellation).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            // the session ended first
        }
    }

    /// <summary>Polls seats and hosting at the selected relay's cadence; each scheduler decides when work is due.</summary>
    private static async Task SeatLoopAsync(Matchmaking.WorldsBridge worlds, OverlayChannel overlay, CancellationToken cancellation)
    {
        while (!cancellation.IsCancellationRequested)
        {
            try
            {
                await Task.Delay(worlds.HostPollInterval, cancellation).ConfigureAwait(false);
                await overlay.SendFrameAsync(await worlds.AccessFrameAsync(cancellation).ConfigureAwait(false), cancellation).ConfigureAwait(false);
                if (await worlds.TickAsync(cancellation).ConfigureAwait(false) is { } ended)
                {
                    await overlay.ToastAsync(new Overlay.UiFrame.Toast(Overlay.UiLevel.Warn, 2, 50, ended), cancellation).ConfigureAwait(false);
                }
                if (await worlds.HostTickAsync(cancellation).ConfigureAwait(false) is { } closed)
                {
                    await overlay.ToastAsync(new Overlay.UiFrame.Toast(Overlay.UiLevel.Warn, 2, 50, closed), cancellation).ConfigureAwait(false);
                }
                var mine = await worlds.PollMyWorldAsync(cancellation).ConfigureAwait(false);
                await overlay.SendFrameAsync(mine, cancellation).ConfigureAwait(false);
                foreach (var toast in worlds.LobbyToasts()) await overlay.ToastAsync(toast, cancellation).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }
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
                    if (_config.Overlay is { } package)
                    {
                        // right after the handshake: the hook checks the file against this hash and loads it
                        await pipe.WriteAsync(new Frame.Overlay(true, package.Sha256, package.Path).Encode(), cancellation).ConfigureAwait(false);
                    }
                }
                break;
            case EndpointEvent.HookEvent e when e.Kind != HookEvents.GameReadiness:
                _emit(new SupervisorEvent.HookEvent(e.Kind, e.Detail));
                break;
            case EndpointEvent.Capture c:
                _emit(new SupervisorEvent.Capture(c.What, c.Ok) { ReceivedTimestamp = TimeProvider.System.GetTimestamp() });
                break;
        }
        return state;
    }

    private string SessionEndedText() => _config.Text?["overlay.session_ended_body"]
        ?? "The online session ended. Progress keeps saving on this PC.";

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
