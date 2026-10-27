// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Security.Cryptography;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Matchmaking;
using Infinity.Core.Online;
using Infinity.Core.Session;
using Infinity.Core.Wire;

namespace Infinity.Core.Supervisor;

/// <summary>What the lobby pane asks for (OD-109): the overlay's Create World fields, mode and races fixed.</summary>
/// <param name="Audience">0 everyone, 1 supporters, 2 friends, 3 private (as <see cref="Overlay.UiFrame.CreateWorld"/>).</param>
/// <param name="MaxPlayers">Guests, 2-12: the dedicated host is not a player.</param>
/// <param name="Latency">0 any, 1 150 ms, 2 100 ms, 3 60 ms.</param>
public sealed record DedicatedLobbySettings(string Name, byte Audience, byte MaxPlayers, byte Latency);

public enum DedicatedPhase
{
    Idle,
    Starting,
    Running,
    Restarting,
    Stopping,
}

/// <summary>One snapshot for the pane. <see cref="LastError"/> is already localised.</summary>
public sealed record DedicatedStatus(DedicatedPhase Phase, string? WorldId, string? Code, uint Players, uint MaxPlayers, TimeSpan Uptime, string? LastError)
{
    public static DedicatedStatus Idle { get; } = new(DedicatedPhase.Idle, null, null, 0, 0, TimeSpan.Zero, null);

    public bool Active => Phase != DedicatedPhase.Idle;
}

public sealed record DedicatedHostOptions(string Game, string Xuid, string LauncherVersion)
{
    /// <summary>The world's <c>DELETE</c> on stop gets at most this long (real time).</summary>
    public TimeSpan DeleteBound { get; init; } = TimeSpan.FromSeconds(5);

    /// <summary>The gate leave on stop gets its own budget after the <c>DELETE</c> (real time).</summary>
    public TimeSpan LeaveBound { get; init; } = TimeSpan.FromSeconds(4);

    /// <summary>A whole stop: both budgets and a margin; the shell's synchronous wait (12 s) is longer.</summary>
    public TimeSpan StopBound => DeleteBound + LeaveBound + TimeSpan.FromSeconds(1);

    /// <summary>The wait before the one automatic re-admission after an unexpected end.</summary>
    public TimeSpan RestartDelay { get; init; } = TimeSpan.FromSeconds(15);

    /// <summary>A run that lasted this long earns a fresh automatic re-admission.</summary>
    public TimeSpan StableAfter { get; init; } = TimeSpan.FromMinutes(10);

    /// <summary>The <c>*.</c> key of why this launcher may not start (or restart) hosting now, or null.</summary>
    public Func<string?>? HostingDenied { get; init; }

    /// <summary>The launcher's log: ids, phases and codes only.</summary>
    public Action<string>? Log { get; init; }

    /// <summary>
    /// OD-112: the launcher's enabled set for this lobby's launch plan (no game runs, so no runtime report);
    /// a non-empty plan admits the session as <c>modded</c>. The bridges get the same instance.
    /// </summary>
    public Matchmaking.IModSetSource? Mods { get; init; }
}

/// <summary>
/// A dedicated lobby (OD-109, docs/launcher/DEDICATED_LOBBIES.md §3): the launcher's integrated world
/// server hosts a matchmaking world with no game process. It admits a dedicated play session at the
/// gate (<c>dedicated: true</c>; admins only, never queued, no content scan), keeps it alive with its
/// own heartbeat loop, opens the world through its own <see cref="WorldsBridge"/> with that session's
/// ticket and ticks it every few seconds. Stop deletes the world, then leaves the session, within
/// <see cref="DedicatedHostOptions.StopBound"/>. A lapse (an expired session after sleep or a network
/// loss, a lost world) is followed by one automatic re-admission; refusals, takeover, revocation,
/// a required update and online closure are final.
/// </summary>
public sealed class DedicatedHost : IAsyncDisposable
{
    /// <summary>SHA-256 of the empty input: the dedicated session runs no hook (GATE_API, OD-109).</summary>
    public static readonly string EmptyHookSha256 = Convert.ToHexStringLower(SHA256.HashData(ReadOnlySpan<byte>.Empty));

    private readonly IGate _gate;
    private readonly Func<Func<string>, OnlineActivity, WorldsBridge> _bridges;
    private readonly Func<string, string> _text;
    private readonly DedicatedHostOptions _options;
    private readonly TimeProvider _clock;
    private readonly Action<string> _log;
    private readonly object _sync = new();
    private Task? _run;
    private Task? _stopping;
    private CancellationTokenSource? _stop;
    private OnlineActivity? _activity;
    private OnlinePause? _pendingPause;
    private int _active;
    private DedicatedPhase _phase;
    private string? _worldId, _code, _lastError;
    private uint _players, _maxPlayers;
    private long? _since;
    private int _pauseRaised;

    /// <param name="bridges">A world bridge for one admitted session: its current ticket and its online latch.</param>
    /// <param name="text">The shell's catalog (<c>lobby.*</c>, <c>worlds.*</c>).</param>
    public DedicatedHost(IGate gate, Func<Func<string>, OnlineActivity, WorldsBridge> bridges, Func<string, string> text,
        DedicatedHostOptions options, TimeProvider? clock = null)
    {
        _gate = gate ?? throw new ArgumentNullException(nameof(gate));
        _bridges = bridges ?? throw new ArgumentNullException(nameof(bridges));
        _text = text ?? throw new ArgumentNullException(nameof(text));
        _options = options ?? throw new ArgumentNullException(nameof(options));
        _clock = clock ?? TimeProvider.System;
        _log = options.Log ?? (_ => { });
    }

    /// <summary>Raised off the UI thread on every phase change and after every host tick.</summary>
    public event Action<DedicatedStatus>? Changed;

    /// <summary>The server closed online activity while hosting (not raised for a pause the caller passed to <see cref="Pause"/>).</summary>
    public event Action<OnlinePause>? OnlinePaused;

    /// <summary>The gate answered <c>GAME.UPDATE_REQUIRED</c>.</summary>
    public event Action? UpdateRequired;

    /// <summary>True from <see cref="StartAsync"/> until the stop finished: the play session's lobbies are unavailable meanwhile.</summary>
    public bool Active => Volatile.Read(ref _active) != 0;

    public DedicatedStatus Status
    {
        get
        {
            lock (_sync) return StatusUnlocked();
        }
    }

    /// <summary>
    /// The play session's <c>checkWorldAccess</c> while a dedicated host may run on this launcher:
    /// lobbies are unavailable in the overlay (<c>worlds.dedicated_running</c>), the game still plays.
    /// </summary>
    public static Func<CancellationToken, Task<WorldAccessDecision>> GuardPlayWorlds(Func<bool> dedicatedActive,
        Func<CancellationToken, Task<WorldAccessDecision>> inner)
    {
        ArgumentNullException.ThrowIfNull(dedicatedActive);
        ArgumentNullException.ThrowIfNull(inner);
        return cancellation => dedicatedActive() ? Task.FromResult(WorldAccessDecision.DedicatedRunning) : inner(cancellation);
    }

    /// <summary>Admits, opens the world and returns once it is running or refused (then <see cref="DedicatedStatus.LastError"/> says why).</summary>
    public async Task<DedicatedStatus> StartAsync(DedicatedLobbySettings settings, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(settings);
        var name = string.IsNullOrWhiteSpace(settings.Name) ? "Infinity" : settings.Name.Trim();
        settings = settings with
        {
            Name = name[..Math.Min(48, name.Length)],
            Audience = settings.Audience > 3 ? (byte)0 : settings.Audience,
            MaxPlayers = (byte)Math.Clamp((int)settings.MaxPlayers, 2, 12),
            Latency = settings.Latency > 3 ? (byte)0 : settings.Latency,
        };
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        CancellationTokenSource stop;
        lock (_sync)
        {
            if (_run is not null) return StatusUnlocked();
            if (_options.HostingDenied?.Invoke() is { } denied)
            {
                _lastError = _text(denied);
                return StatusUnlocked();
            }
            stop = _stop = new CancellationTokenSource();
            _stopping = null;
            _pendingPause = null;
            Volatile.Write(ref _active, 1);
            (_phase, _worldId, _code, _players, _maxPlayers, _since, _lastError) =
                (DedicatedPhase.Starting, null, null, 0, settings.MaxPlayers, null, null);
            _run = Task.Run(() => RunAsync(settings, started, stop.Token), CancellationToken.None);
        }
        Publish();
        using (cancellation.Register(() => _ = StopAsync()))
            await started.Task.ConfigureAwait(false);
        return Status;
    }

    /// <summary>Idempotent and bounded: the world's <c>DELETE</c>, then the gate leave; safe to call from any thread.</summary>
    public Task StopAsync()
    {
        Task run;
        CancellationTokenSource? stop;
        lock (_sync)
        {
            if (_run is null) return Task.CompletedTask;
            if (_stopping is not null) return _stopping;
            run = _run;
            stop = _stop;
            _phase = DedicatedPhase.Stopping;
            // Cancellation runs continuations inline: never on the caller's (UI) thread or under the lock.
            _stopping = Task.Run(async () =>
            {
                try { await (stop?.CancelAsync() ?? Task.CompletedTask).ConfigureAwait(false); }
                catch (ObjectDisposedException) { }
                try { await run.WaitAsync(_options.StopBound).ConfigureAwait(false); }
                catch (TimeoutException) { _log("lobby: stop timed out; the world lapses 45 s after its last announce"); }
            });
        }
        Publish();
        return _stopping;
    }

    /// <summary>
    /// A terminal online closure observed elsewhere in the launcher: the host stops without a
    /// re-admission, also between runs (while it waits to re-admit). The caller already knows the
    /// pause, so <see cref="OnlinePaused"/> is not raised for it.
    /// </summary>
    public bool Pause(OnlinePause pause)
    {
        ArgumentNullException.ThrowIfNull(pause);
        OnlineActivity? activity;
        lock (_sync)
        {
            if (_run is null) return false;
            _pendingPause ??= pause;
            activity = _activity;
            _lastError = _text("lobby.online_paused");
        }
        Volatile.Write(ref _pauseRaised, 1);
        if (activity?.TryPause(pause) == true) return true;
        // No live run to latch (restarting, or the run is ending): stop outright.
        _ = StopAsync();
        return true;
    }

    public async ValueTask DisposeAsync() => await StopAsync().ConfigureAwait(false);

    private enum EndKind
    {
        /// <summary>Stop or cancellation: no message, no re-admission.</summary>
        Stopped,
        /// <summary>A refusal, takeover, revocation, update or closure: shown, never re-admitted.</summary>
        Final,
        /// <summary>A lapse (expired session, lost world): earns the one re-admission.</summary>
        Lapsed,
    }

    private sealed record End(EndKind Kind, string? Message)
    {
        public static End Stopped { get; } = new(EndKind.Stopped, null);
    }

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1031:Do not catch general exception types",
        Justification = "A dedicated host is a long-lived background run: any unforeseen fault must end it with a visible error, never fault silently.")]
    private async Task RunAsync(DedicatedLobbySettings settings, TaskCompletionSource started, CancellationToken cancellation)
    {
        long? lastRestart = null;
        try
        {
            while (true)
            {
                // A refusal reaches the caller only from finally, once the error and the idle phase are set.
                var end = await OneRunAsync(settings, started, cancellation).ConfigureAwait(false);
                if (cancellation.IsCancellationRequested || end.Kind == EndKind.Stopped) return;
                lock (_sync) _lastError = PendingPauseText() ?? end.Message;
                if (end.Kind != EndKind.Lapsed || PausePending) return;
                if (lastRestart is { } restarted && _clock.GetElapsedTime(restarted) < _options.StableAfter) return;
                if (_options.HostingDenied?.Invoke() is { } denied) { lock (_sync) _lastError = _text(denied); return; }
                lock (_sync) (_worldId, _code, _players, _since) = (null, null, 0, null);
                SetPhase(DedicatedPhase.Restarting);
                _log("lobby: ended unexpectedly; one re-admission after " + (int)_options.RestartDelay.TotalSeconds + " s");
                Publish();
                await Task.Delay(_options.RestartDelay, _clock, cancellation).ConfigureAwait(false);
                if (PausePending) return;
                if (_options.HostingDenied?.Invoke() is { } later) { lock (_sync) _lastError = _text(later); return; }
                lastRestart = _clock.GetTimestamp();
                SetPhase(DedicatedPhase.Starting);
                Publish();
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { }
        catch (Exception error)
        {
            _log("lobby: ended by an unexpected fault (" + error.GetType().Name + ")");
            lock (_sync) _lastError = _text("lobby.start_failed");
        }
        finally
        {
            CancellationTokenSource? stop;
            lock (_sync)
            {
                (_phase, _worldId, _code, _players, _since, _activity) = (DedicatedPhase.Idle, null, null, 0, null, null);
                stop = _stop; _stop = null;
                _run = null;
                Volatile.Write(ref _active, 0);
            }
            stop?.Dispose();
            Publish();
            started.TrySetResult();
        }
    }

    private bool PausePending
    {
        get { lock (_sync) return _pendingPause is not null; }
    }

    private string? PendingPauseText() => _pendingPause is null ? null : _text("lobby.online_paused");

    /// <summary>One admitted session and its world, until it ends; cleanup always runs before it returns.</summary>
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Reliability", "CA2000:Dispose objects before losing scope",
        Justification = "The bridge's late transports drain on their own; its relay is stopped by CloseHostAsync/SuspendOnlineAsync.")]
    private async Task<End> OneRunAsync(DedicatedLobbySettings settings, TaskCompletionSource started, CancellationToken cancellation)
    {
        // Disposing the run's latch also ends the bridge's pause observer with the run.
        using var activity = new OnlineActivity();
        OnlinePause? pending;
        lock (_sync) { _activity = activity; pending = _pendingPause; }
        Volatile.Write(ref _pauseRaised, pending is null ? 0 : 1);
        if (pending is not null) { activity.TryPause(pending); return new(EndKind.Final, _text("lobby.online_paused")); }
        try
        {
            using var run = CancellationTokenSource.CreateLinkedTokenSource(cancellation, activity.Cancellation);
            (string SessionId, GateActive Active)? admitted;
            try
            {
                admitted = await new GateAdmission(_gate, Note, _clock).WaitAsync(id => new PlaySessionCreate(id, _options.Game, _options.Xuid,
                    Hex.Encode(Launch.NewNonce()), _options.LauncherVersion, EmptyHookSha256, null, 0, false, Dedicated: true,
                    Modded: Matchmaking.ModSets.Modded(_options.Mods)), run.Token).ConfigureAwait(false);
            }
            catch (ApiException error)
            {
                _log("lobby: admission refused " + error.Code);
                var refused = Classify(error, activity, admission: true);
                // An admission is never itself re-admitted: a lapse here is a failed start.
                return refused.Kind == EndKind.Lapsed ? new(EndKind.Final, _text("lobby.start_failed")) : refused;
            }
            catch (Exception error) when (error is HttpRequestException or IOException or TimeoutException or JsonException
                or System.Net.Sockets.SocketException or InvalidOperationException)
            {
                _log("lobby: admission unavailable (" + error.GetType().Name + ")");
                return new(EndKind.Final, _text("lobby.start_failed"));
            }
            if (admitted is not { } session)
                return activity.IsPaused ? PausedEnd(activity) : cancellation.IsCancellationRequested ? End.Stopped : new(EndKind.Final, _text("lobby.start_failed"));
            return await HostAsync(settings, session.SessionId, session.Active, activity, run, started).ConfigureAwait(false);
        }
        finally
        {
            lock (_sync) if (ReferenceEquals(_activity, activity)) _activity = null;
        }
    }

    private async Task<End> HostAsync(DedicatedLobbySettings settings, string sessionId, GateActive active, OnlineActivity activity,
        CancellationTokenSource run, TaskCompletionSource started)
    {
        var fence = active.Lease?.FenceToken;
        var ticket = active.Ticket.Token;
        _log("lobby: dedicated session admitted " + sessionId);
        WorldsBridge? bridge = null;
        try
        {
            bridge = _bridges(() => Volatile.Read(ref ticket), activity);
            var hosted = await bridge.CreateAsync(new Overlay.UiFrame.CreateWorld(settings.Name, settings.Audience, settings.MaxPlayers, settings.Latency),
                run.Token).ConfigureAwait(false);
            if (activity.IsPaused) return PausedEnd(activity);
            if (run.IsCancellationRequested) return End.Stopped;
            if (hosted.Result != 0 || bridge.HostedWorldId is null)
            {
                _log("lobby: world not opened");
                return new(EndKind.Final, string.IsNullOrEmpty(hosted.Message) ? _text("worlds.host_failed") : hosted.Message);
            }
            lock (_sync)
            {
                if (_phase != DedicatedPhase.Stopping) _phase = DedicatedPhase.Running;
                (_worldId, _code, _players, _since) = (hosted.WorldId, string.IsNullOrEmpty(hosted.Code) ? null : hosted.Code,
                    bridge.HostedPlayers, _clock.GetTimestamp());
            }
            _log("lobby: world " + hosted.WorldId + " open (dedicated)");
            started.TrySetResult();
            Publish();
            using var loops = CancellationTokenSource.CreateLinkedTokenSource(run.Token);
            var beat = HeartbeatLoopAsync(sessionId, fence, active.Ticket.HeartbeatEveryMs, value => Volatile.Write(ref ticket, value),
                value => fence = value, activity, loops.Token);
            var tick = TickLoopAsync(bridge, activity, loops.Token);
            await Task.WhenAny(beat, tick).ConfigureAwait(false);
            await loops.CancelAsync().ConfigureAwait(false);
            var ends = await Task.WhenAll(beat, tick).ConfigureAwait(false);
            if (activity.IsPaused) return PausedEnd(activity);
            // Either loop may notice first: a final reason (takeover, revocation) wins over a lapse.
            return ends.FirstOrDefault(e => e.Kind == EndKind.Final) ?? ends.FirstOrDefault(e => e.Kind == EndKind.Lapsed) ?? End.Stopped;
        }
        catch (OperationCanceledException) when (run.IsCancellationRequested)
        {
            return activity.IsPaused ? PausedEnd(activity) : End.Stopped;
        }
        catch (Exception error) when (error is HttpRequestException or IOException or InvalidOperationException or TimeoutException
            or ApiException or JsonException or System.Net.Sockets.SocketException)
        {
            _log("lobby: hosting failed (" + error.GetType().Name + ")");
            return new(EndKind.Final, _text("worlds.host_failed"));
        }
        finally
        {
            await CleanupAsync(bridge, sessionId, fence, activity).ConfigureAwait(false);
        }
    }

    private async Task CleanupAsync(WorldsBridge? bridge, string sessionId, ulong? fence, OnlineActivity activity)
    {
        if (bridge is not null)
        {
            // DELETE first: a guest must never see a world whose host session already left. The tick's
            // own cancellation path may already have deleted it; then this only waits for that.
            using var delete = new CancellationTokenSource(_options.DeleteBound);
            try
            {
                if (activity.IsPaused) await bridge.SuspendOnlineAsync().WaitAsync(delete.Token).ConfigureAwait(false);
                else await bridge.CloseHostAsync(delete.Token).WaitAsync(delete.Token).ConfigureAwait(false);
            }
            catch (Exception error) when (error is ApiException or HttpRequestException or IOException or OperationCanceledException or TimeoutException or InvalidOperationException)
            {
                _log("lobby: world close incomplete (" + error.GetType().Name + "); it lapses 45 s after its last announce");
            }
        }
        using var leave = new CancellationTokenSource(_options.LeaveBound);
        try
        {
            await _gate.LeaveAsync(sessionId, "player", fence, leave.Token).WaitAsync(leave.Token).ConfigureAwait(false);
            _log("lobby: dedicated session left " + sessionId);
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or OperationCanceledException or TimeoutException or InvalidOperationException)
        {
            _log("lobby: session leave incomplete (" + (error is ApiException api ? api.Code : error.GetType().Name) + "); the gate expires it");
        }
    }

    private async Task<End> HeartbeatLoopAsync(string sessionId, ulong? fence, uint heartbeatEveryMs, Action<string> ticket, Action<ulong?> fenced,
        OnlineActivity activity, CancellationToken cancellation)
    {
        var retry = new OperationBackoff();
        var delay = TimeSpan.FromMilliseconds(Math.Max(heartbeatEveryMs, 5000));
        try
        {
            while (true)
            {
                await Task.Delay(delay, _clock, cancellation).ConfigureAwait(false);
                if (activity.IsPaused) return PausedEnd(activity);
                try
                {
                    var beat = await _gate.HeartbeatAsync(sessionId, fence, cancellation).ConfigureAwait(false);
                    cancellation.ThrowIfCancellationRequested();
                    if (activity.IsPaused) return PausedEnd(activity);
                    retry.Success();
                    fence = beat.Lease?.FenceToken ?? fence;
                    fenced(fence);
                    ticket(beat.Ticket.Token);
                    if (!beat.GamePlane.Open)
                    {
                        _log("lobby: game plane closed (" + (beat.GamePlane.Reason ?? "none") + ")");
                        return new(EndKind.Final, _text("lobby.session_ended"));
                    }
                    delay = TimeSpan.FromMilliseconds(Math.Max(beat.Ticket.HeartbeatEveryMs, 5000));
                }
                catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { throw; }
                catch (ApiException error) when (OnlinePause.From(error) is null && OperationBackoff.Transient(error))
                {
                    delay = retry.Failed(error);
                    _log("lobby: heartbeat " + error.Code + "; next in " + (int)delay.TotalSeconds + " s");
                }
                catch (ApiException error)
                {
                    _log("lobby: heartbeat ended the session " + error.Code + (Reason(error) is { } reason ? " (" + reason + ")" : string.Empty));
                    return Classify(error, activity, admission: false);
                }
                catch (Exception error) when (OperationBackoff.Transient(error) || error is System.Net.Sockets.SocketException)
                {
                    // A missed beat (sleep, network loss): keep trying; the server answers a lapse explicitly.
                    delay = retry.Failed(error);
                    _log("lobby: heartbeat " + error.GetType().Name + "; next in " + (int)delay.TotalSeconds + " s");
                }
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
            return End.Stopped;
        }
    }

    private async Task<End> TickLoopAsync(WorldsBridge bridge, OnlineActivity activity, CancellationToken cancellation)
    {
        try
        {
            while (true)
            {
                await Task.Delay(bridge.HostPollInterval, _clock, cancellation).ConfigureAwait(false);
                // A paused bridge answers with an integrity message; the pause is the reason to show.
                if (activity.IsPaused) return PausedEnd(activity);
                var ended = await bridge.HostTickAsync(cancellation).ConfigureAwait(false);
                if (activity.IsPaused) return PausedEnd(activity);
                cancellation.ThrowIfCancellationRequested();
                if (ended is not null || bridge.HostedWorldId is null)
                {
                    // A lost world or an expired ticket (WorldNotFound, AUTH.SESSION_EXPIRED, 45 s lapse): heal.
                    _log("lobby: world ended");
                    return new(EndKind.Lapsed, ended ?? _text("worlds.host_ended"));
                }
                lock (_sync) { _players = bridge.HostedPlayers; _code = bridge.HostedCode ?? _code; }
                Publish();
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
            return End.Stopped;
        }
    }

    /// <summary>
    /// What an answer means for the dedicated session (GATE_API.md §4): an expired session (its
    /// heartbeat answers <c>SAVES.LEASE_LOST</c>, or <c>SESSIONS.ENDED</c> with an expiry reason) and an
    /// unauthorised access token are lapses; takeover, revocation, bans, refusals, a required update
    /// and online closure are final.
    /// </summary>
    private End Classify(ApiException error, OnlineActivity activity, bool admission)
    {
        if (OnlinePause.From(error) is { } pause)
        {
            activity.TryPause(pause);
            return PausedEnd(activity, pause);
        }
        if (error.Code == ErrorCodes.GameUpdateRequired) Raise(UpdateRequired);
        if (error.Code == ErrorCodes.SavesLeaseLost) return new(EndKind.Lapsed, _text("lobby.session_lapsed"));
        if (error.Code == ErrorCodes.SessionsEnded && Reason(error) is { } reason && reason.Contains("expired", StringComparison.Ordinal))
            return new(EndKind.Lapsed, _text("lobby.session_lapsed"));
        if (error.Status == HttpStatusCode.Unauthorized && error.Code is not (ErrorCodes.AuthRefreshInvalid or ErrorCodes.AuthRefreshReused))
            return new(EndKind.Lapsed, _text("lobby.session_lapsed"));
        return new(EndKind.Final, _text(error.Code switch
        {
            ErrorCodes.AuthRoleRequired => "lobby.role_required",
            ErrorCodes.RequestInvalid when admission && error.Status == HttpStatusCode.BadRequest && UnsupportedField(error) => "lobby.unsupported",
            ErrorCodes.SessionsInUseElsewhere => "lobby.in_use_elsewhere",
            ErrorCodes.SessionsEnded => Reason(error) switch { "takeover" => "lobby.taken_over", "kicked" => "lobby.kicked", _ => "lobby.session_ended" },
            ErrorCodes.SessionsKicked => "lobby.kicked", // CR-S62: the cooldown of a kick still refuses admission

            ErrorCodes.AccountsFrozen => "crash.frozen",
            ErrorCodes.GameUpdateRequired => "lobby.update_required",
            _ => admission ? "lobby.start_failed" : "lobby.session_ended",
        }));
    }

    /// <summary>
    /// A server without dedicated support refuses the unknown <c>dedicated</c> field (or the body shape,
    /// e.g. the absent <c>carDbSha256</c>); another field (<c>hookSha256</c>, <c>game</c>) is a real refusal.
    /// </summary>
    private static bool UnsupportedField(ApiException error)
    {
        if (Detail(error, "field") is not { } field) return true;
        return field is "dedicated" or "carDbSha256" or "body";
    }

    private static string? Reason(ApiException error) => Detail(error, "reason");

    private static string? Detail(ApiException error, string key)
    {
        try
        {
            return error.Details is { ValueKind: JsonValueKind.Object } details && details.TryGetProperty(key, out var value)
                && value.ValueKind == JsonValueKind.String ? value.GetString() : null;
        }
        catch (InvalidOperationException) { return null; }
    }

    private End PausedEnd(OnlineActivity activity, OnlinePause? pause = null)
    {
        pause ??= activity.Paused.IsCompletedSuccessfully ? activity.Paused.Result : null;
        if (pause is not null && Interlocked.Exchange(ref _pauseRaised, 1) == 0)
        {
            var handlers = OnlinePaused;
            try { handlers?.Invoke(pause); }
            catch (Exception error) when (error is InvalidOperationException or ObjectDisposedException) { }
        }
        lock (_sync) _pendingPause ??= pause;
        return new(EndKind.Final, _text("lobby.online_paused"));
    }

    private void SetPhase(DedicatedPhase phase)
    {
        // A requested stop is never hidden by a later phase of the run it is ending.
        lock (_sync) if (_phase != DedicatedPhase.Stopping) _phase = phase;
    }

    private void Note(SupervisorEvent e)
    {
        switch (e)
        {
            case SupervisorEvent.AdmissionRetry retry:
                _log("lobby: admission retry " + retry.Code + " after " + (int)retry.Delay.TotalSeconds + " s");
                break;
            case SupervisorEvent.Ended ended:
                _log("lobby: admission ended " + ended.Reason);
                break;
        }
    }

    private DedicatedStatus StatusUnlocked() => new(_phase, _worldId, _code, _players, _maxPlayers,
        _since is { } since ? _clock.GetElapsedTime(since) : TimeSpan.Zero, _lastError);

    private readonly object _publish = new();

    /// <summary>
    /// The status as it is now, to every handler. Reading and raising are one step under <c>_publish</c>, so two
    /// threads never deliver out of order: a stop's own "stopping" read before the run ended can no longer reach
    /// the shell after the run's final "idle" (every change is followed by a publish, so the last one is current).
    /// Handlers only post or record and never block.
    /// </summary>
    private void Publish()
    {
        lock (_publish)
        {
            var handlers = Changed;
            if (handlers is null) return;
            var status = Status;
            try { handlers(status); }
            catch (Exception error) when (error is InvalidOperationException or ObjectDisposedException) { }
        }
    }

    private static void Raise(Action? handlers)
    {
        try { handlers?.Invoke(); }
        catch (Exception error) when (error is InvalidOperationException or ObjectDisposedException) { }
    }
}
