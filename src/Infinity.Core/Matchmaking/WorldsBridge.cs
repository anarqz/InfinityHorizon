// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net.Sockets;
using System.Net;
using Infinity.Core.Diagnostics;
using Infinity.Core.Api;
using Infinity.Core.Online;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// The account's actions on a player of the Worlds tab (OD-63), with the launcher session's token:
/// each answers whether it went through and the <c>worlds.*</c> key of what to show.
/// </summary>
public interface IWorldSocial
{
    /// <summary><c>POST /launcher/v1/friends {game, gamertag}</c> (a request, or the acceptance of theirs).</summary>
    Task<(bool Ok, string Key)> AddFriendAsync(string gamertag, CancellationToken cancellation);

    /// <summary><c>POST /launcher/v1/reports {game, xuid, reason, worldId}</c> (CR-L25, OD-63).</summary>
    Task<(bool Ok, string Key)> ReportAsync(string xuid, string reason, string? worldId, CancellationToken cancellation);
}

/// <summary>
/// The overlay's Worlds tab against matchmaking (OD-54, OD-57, OD-58): the overlay asks through the
/// UI pipe (it holds no tokens), this answers with frames. A granted join reserves a seat that is
/// renewed every 20 s (<see cref="SeatKeeper"/>) while the player walks to FREE ROAM ONLINE; the
/// game's own free roam then lands in it (MATCHMAKING_API.md §3.2, §4a). <paramref name="text"/>
/// gives the localised message for a key (<c>worlds.*</c>), so the overlay shows the shell's words.
/// </summary>
[System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1001:Types that own disposable fields should be disposable",
    Justification = "Session cancellation state remains available to late transports until their canceled operations drain.")]
public sealed partial class WorldsBridge(IMatchmakingApi mm, Func<string, string> text, TimeProvider? clock = null,
    string mode = "noob", Func<uint, IHostRelay?>? startRelay = null, Func<IReadOnlyList<string>>? lanAddresses = null, string? ownXuid = null, Action<string>? log = null,
    string relayUnavailableKey = "worlds.integrated_unavailable", Func<IHostRelay, WorldAnnounced, CancellationToken, Task>? updateRelayWorld = null,
    Func<IHostRelay, CancellationToken, Task>? refreshRelayAdmissions = null, Session.GameReadiness? gameReadiness = null,
    Func<CancellationToken, Task<WorldAccessDecision>>? checkWorldAccess = null, Action<string>? worldAccessDenied = null,
    OnlineActivity? onlineActivity = null, bool idleWhenNoWorld = false, Func<double>? retryJitter = null,
    bool requireGameReadiness = true, bool closeWhenAlone = true)
{
    // OD-109: a dedicated host (the launcher with no game) opens a world without a loaded game and
    // never applies the client alone-close; the overlay's bridge keeps both defaults.
    private readonly bool _requireGameReadiness = requireGameReadiness;
    private readonly bool _closeWhenAlone = closeWhenAlone;
    // the session profile's mode: the mode of every world it opens, and what a matching lobby has (CR-L40)
    private readonly string _mode = mode;
    private string? _hostedCode;
    private uint _hostedPlayers;
    // OD-150: the hosted world's activity as this launcher last told the server (the PUT, then a heartbeat that carried a change)
    private string? _hostedActivity;

    /// <summary>The hosted world's join code (friends/private audiences only), as the last PUT answered.</summary>
    public string? HostedCode => HostedWorldId is null ? null : Volatile.Read(ref _hostedCode);

    private int _creating;

    /// <summary>
    /// A world is hosted or being opened by this bridge (OD-109: the launcher's dedicated lobby is not
    /// created meanwhile, so the two never race for the relay's UDP port).
    /// </summary>
    public bool HostsOrOpensWorld => Volatile.Read(ref _creating) > 0 || HostingOrOpening;

    /// <summary>
    /// The catalog key for a relay that could not start, chosen after the attempt (e.g. its port is in
    /// use); null keeps the constructor's <c>relayUnavailableKey</c>.
    /// </summary>
    public Func<string?>? RelayFailureKey { get; init; }

    /// <summary>The hosted world's players as the last successful PUT or announce answered (no extra read).</summary>
    public uint HostedPlayers => HostedWorldId is null ? 0 : Volatile.Read(ref _hostedPlayers);

    // the world's open and close in the launcher's log (ids only, never addresses)
    private readonly Action<string> _log = log ?? (_ => { });
    private readonly Diagnostics.UdpDiagnostics _udpDiagnostics=new(log??(_=>{}),clock);
    public void RecordUdpDiagnostic(Session.Frame.UdpDiagnostic frame)=>_udpDiagnostics.Native(frame);
    // the host's own game is a player of its relay too, but never one of the world's peers (Mac, 2026-10-19)
    private readonly string? _ownXuid = ownXuid;
    private readonly Func<IReadOnlyList<string>> _lan = lanAddresses ?? HostRelay.LanAddresses;
    // hosting (OD-54): the relay the launcher runs, the world it opened, its 15 s heartbeat
    private readonly Func<uint, IHostRelay?>? _startRelay = startRelay;
    private readonly WorldHostScheduler _host = new();
    private IHostRelay? _relay;
    private readonly object _hostState = new();
    private Task _hostOperation = Task.CompletedTask;
    private long _hostGeneration;
    private CancellationTokenSource? _hostStop;
    private string? _hostedWorldId;

    /// <summary>A car meet invitation's shared session for the world this launcher hosts now; null when there is none or it was refused.</summary>
    private async Task<(string Name, string Template)?> GameModeSessionAsync(string kind, IReadOnlyList<string> xuids, CancellationToken cancellation)
    {
        if (HostedWorldId is not { } worldId || xuids.Count is 0 or > GameModeSessionRequest.MaxMembers) return null;
        var answer = await _mm.GameModeSessionAsync(worldId, new GameModeSessionRequest { Kind = kind, Xuids = xuids }, cancellation).ConfigureAwait(false);
        if (answer is not { Ok: true, Value: { } session }) { _log?.Invoke($"world: car meet session refused ({answer.Error?.Code ?? "no answer"})"); return null; }
        return (session.Name, session.TemplateName);
    }
    private DateTimeOffset _nextAdmissionsRefresh;
    // OD-107: punches are read every 3 s only while a granted player is still connecting; else 10 s
    // (the 15 s announce carries punches too). A world with only its host closes after 5 minutes.
    public static readonly TimeSpan JoiningAdmissionsEvery = TimeSpan.FromSeconds(3);
    public static readonly TimeSpan QuietAdmissionsEvery = TimeSpan.FromSeconds(10);
    public static readonly TimeSpan AloneWarnAfter = TimeSpan.FromMinutes(4);
    public static readonly TimeSpan AloneCloseAfter = TimeSpan.FromMinutes(5);
    private bool _hostJoining = true;
    // monotonic: a forward wall-clock jump cannot close a world early
    private long? _hostAloneSince;
    private bool _hostAloneWarned;
    private TimeSpan AdmissionsEvery => _hostJoining ? JoiningAdmissionsEvery : QuietAdmissionsEvery;
    private int? _announcedExternalPort;
    private DateTimeOffset _hostLastSuccess;
    private long _hostLastStamp;
    private readonly IMatchmakingApi _mm = mm ?? throw new ArgumentNullException(nameof(mm));
    private readonly Func<string, string> _text = text ?? throw new ArgumentNullException(nameof(text));
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly object _timeState = new();
    private readonly DateTimeOffset _timeAnchor = (clock ?? TimeProvider.System).GetUtcNow();
    private readonly long _timeStamp = (clock ?? TimeProvider.System).GetTimestamp();
    private DateTimeOffset _observedNow;
    private readonly SeatKeeper _seat = new();
    private readonly object _mineState = new();
    private readonly CancellationTokenSource _onlineStop = new();
    private readonly OnlineActivity _online = onlineActivity ?? new();
    private int _suspended;
    private int _observingPause;
    private Task? _suspending;
    private Task<UiFrame.MyWorld>? _mineRead;
    private bool _mineReconciled, _mineUncertain = true;
    private DateTimeOffset? _pendingRequestUntil;
    private string? _pendingRequestId;
    private DateTimeOffset? _lastRosterAt;
    private DateTimeOffset? _lastSeatAt;
    private readonly WorldOperationRetry _seatReadRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _membersRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _listRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _joinRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _renewRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _createRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _announceRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _admissionsRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly WorldOperationRetry _actionRetry = new(clock ?? TimeProvider.System, retryJitter);
    private readonly LobbyRoster _roster = new(ownXuid);
    private readonly PublicWorldWarning _publicWarning = new();
    private UiFrame.MyWorld _mineSnapshot = NoWorld;
    private long _mineNextTicks, _mineEpoch, _mineReadTicks;
    // OD-107: settled background reads (no pending request, join or uncertainty) slow down.
    public static readonly TimeSpan SeatReadEvery = TimeSpan.FromSeconds(5);
    public static readonly TimeSpan QuietSeatReadInWorld = TimeSpan.FromSeconds(15);
    public static readonly TimeSpan QuietSeatReadNoWorld = TimeSpan.FromSeconds(30);

    public IReadOnlyList<UiFrame.Toast> LobbyToasts()
    {
        var toasts = new List<UiFrame.Toast>();
        while (_freeRoamToasts.TryDequeue(out var freeRoam)) toasts.Add(freeRoam); // CR-L40: the lobby FREE ROAM ONLINE joined or opened
        if (_publicWarning.Take()) toasts.Add(new(UiLevel.Warn, 2, 150, _text("worlds.public_warning")));
        toasts.AddRange(_roster.Drain().Select(change =>
            new UiFrame.Toast(UiLevel.Info, 0, 50, _text(change.Joined ? "worlds.player_joined" : "worlds.player_left")
                .Replace("{player}", change.Gamertag, StringComparison.Ordinal))));
        return toasts;
    }

    private void InvalidateMine(bool reset = false)
    {
        lock (_mineState)
        {
            Interlocked.Increment(ref _mineEpoch);
            Interlocked.Exchange(ref _mineNextTicks, 0);
            _mineUncertain = true;
            if (reset)
            {
                _roster.Reset();
                _publicWarning.Reset();
                _seatActivity = null; // a new seat's first read is never a change
                Volatile.Write(ref _followHost, null); // nor does a change of a seat that is gone move the game
                CrashContext?.ObserveWorld(null);
                Volatile.Write(ref _mineSnapshot, NoWorld);
                (_mineId, _mineRole) = (null, UiWorldRole.None);
                _lastRosterAt = null;
            }
        }
    }

    public Session.GameReadiness GameReadiness { get; init; } = gameReadiness ?? new(clock);

    /// <summary>Explicit diagnostic only: no seats, joins, hosting or authority changes.</summary>
    public async Task<UiFrame.ConnectionDetails> ConnectionDetailsAsync(string endpoints, CancellationToken cancellation)
    {
        var started = System.Diagnostics.Stopwatch.StartNew();
        string result;
        byte state;
        try
        {
            var reply = await _mm.ListAsync("global", cancellation: cancellation).ConfigureAwait(false);
            result = _text(reply.Ok ? "connection.accepted" : "connection.refused") + " (HTTP " + (int)reply.Status + ")";
            state = reply.Ok ? (byte)1 : (byte)2;
        }
        catch (Exception error) when (HostFailure(error) || error is System.Text.Json.JsonException)
        { result = _text("connection.unavailable"); state = 2; }
        WorldServer.RelayNetworkStatus? network;
        lock (_hostState) network = _relay switch
        {
            IntegratedWorldRelay relay => relay.NetworkStatus,
            IntegratedHostRelay relay => relay.NetworkStatus,
            _ => null,
        };
        var unknown = _text("connection.not_measured");
        var publicAddress = network?.PublicAddresses.Count > 0 ? string.Join(", ", network.PublicAddresses.Take(4).Select(a => a.Address + ":" + a.Port)) : unknown;
        var nat = network?.Nat switch { WorldServer.RelayNatKind.Cone => _text("connection.nat_cone"), WorldServer.RelayNatKind.Symmetric => _text("connection.nat_symmetric"), _ => unknown };
        var ipv6 = network?.PublicAddresses.Any(a => System.Net.IPAddress.TryParse(a.Address, out var ip) && ip.AddressFamily == System.Net.Sockets.AddressFamily.InterNetworkV6) == true
            ? _text("connection.yes") : unknown;
        var text = endpoints + "\n\n" + _text("connection.matchmaking") + ": " + result + "\n" + _text("connection.response") + ": " + started.ElapsedMilliseconds + " ms"
            + "\n\n" + _text("connection.public_address") + ": " + publicAddress + "\nIPv6: " + ipv6 + "\nNAT: " + nat
            + "\nUPnP: " + (network is null ? unknown : _text("connection.upnp." + network.Upnp.ToString().ToLowerInvariant()))
            + (TransportText() is { } transport ? "\n" + _text("connection.transport") + ": " + transport : "") + "\n\n" + _text("connection.snapshot");
        return new(state, text);
    }

    /// <summary>The current session's bounded diagnostic context; frozen by the supervisor before cleanup.</summary>
    public CrashContext? CrashContext { get; set; }

    /// <summary>A fresh local installation decision for the overlay; no file details cross the UI pipe.</summary>
    public async Task<UiFrame.WorldAccess> AccessFrameAsync(CancellationToken cancellation)
    {
        var access = await AccessAsync(cancellation).ConfigureAwait(false);
        return new(access.Allowed, access.Allowed ? string.Empty : _text(access.MessageKey));
    }

    private async Task<WorldAccessDecision> AccessAsync(CancellationToken cancellation)
    {
        if (!OnlineOpen) { _ = SuspendOnlineAsync(); return WorldAccessDecision.Inconclusive; }
        if (checkWorldAccess is null) return WorldAccessDecision.Compatible;
        try
        {
            var access = await checkWorldAccess(cancellation).WaitAsync(cancellation).ConfigureAwait(false);
            if (!OnlineOpen) { _ = SuspendOnlineAsync(); return WorldAccessDecision.Inconclusive; }
            cancellation.ThrowIfCancellationRequested();
            return access is not null && (access.Allowed || !string.IsNullOrEmpty(access.MessageKey))
                ? access : WorldAccessDecision.Inconclusive;
        }
        catch (ApiException error) when (Pause(error)) { return WorldAccessDecision.Inconclusive; }
        catch (OperationCanceledException) when (!OnlineOpen) { return WorldAccessDecision.Inconclusive; }
        catch (Exception e) when (e is HttpRequestException or IOException or InvalidOperationException or TimeoutException
            || e is OperationCanceledException && !cancellation.IsCancellationRequested)
        {
            return WorldAccessDecision.Inconclusive;
        }
    }

    public OnlineActivity OnlineActivity => _online;

    // Wire deadlines remain server instants; a local rollback cannot extend their remaining lifetime.
    private DateTimeOffset Now
    {
        get
        {
            lock (_timeState)
            {
                var monotonic = _timeAnchor + _clock.GetElapsedTime(_timeStamp);
                var wall = _clock.GetUtcNow();
                if (monotonic > _observedNow) _observedNow = monotonic;
                if (wall > _observedNow) _observedNow = wall;
                return _observedNow;
            }
        }
    }

    private bool OnlineOpen => Volatile.Read(ref _suspended) == 0 && !_online.IsPaused;

    private CancellationTokenSource OnlineCancellation(CancellationToken cancellation)
    {
        if (Interlocked.Exchange(ref _observingPause, 1) == 0) _ = ObservePauseAsync();
        return CancellationTokenSource.CreateLinkedTokenSource(cancellation, _onlineStop.Token, _online.Cancellation);
    }

    private async Task ObservePauseAsync()
    {
        using var stop = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token, _online.Cancellation);
        try { await _online.Paused.WaitAsync(stop.Token).ConfigureAwait(false); await SuspendOnlineAsync().ConfigureAwait(false); }
        catch (OperationCanceledException) when (stop.IsCancellationRequested)
        { if (_online.IsPaused) await SuspendOnlineAsync().ConfigureAwait(false); }
    }

    private bool Pause(ApiException error)
    {
        if (_online.TryPause(error) || OnlinePause.From(error) is not null) { _ = SuspendOnlineAsync(); return true; }
        return false;
    }

    private void Observe<T>(MmResult<T> result)
    {
        if (result.Error is { } error && _online.TryPause(error.Code, result.Status, error.Details))
            _ = SuspendOnlineAsync();
    }

    private async Task<MmResult<T>?> OperationAsync<T>(WorldOperationRetry retry,
        Func<CancellationToken, Task<MmResult<T>>> operation, CancellationToken cancellation,
        Func<MmResult<T>, Task>? discardLate = null, TimeSpan? remaining = null)
    {
        if (!OnlineOpen || !retry.TryBegin(out var generation)) return null;
        var duration = remaining is { } limit && limit < TimeSpan.FromSeconds(10) ? limit : TimeSpan.FromSeconds(10);
        if (duration <= TimeSpan.Zero) { retry.Cancel(generation); return null; }
        using var deadline = new CancellationTokenSource(duration, _clock);
        using var attempt = CancellationTokenSource.CreateLinkedTokenSource(cancellation, deadline.Token);
        var token = attempt.Token;
        try
        {
            var pending = operation(token);
            var discarded = 0;
            async Task DiscardOnce(MmResult<T> result)
            {
                if (discardLate is not null && Interlocked.Exchange(ref discarded, 1) == 0)
                    await discardLate(result).ConfigureAwait(false);
            }
            // An obsolete operation cannot publish a world, but a typed global closure still ends this session.
            _ = DiscardLateAsync(pending, DiscardOnce, token);
            var answer = await pending.WaitAsync(token).ConfigureAwait(false);
            Observe(answer);
            if (!OnlineOpen || token.IsCancellationRequested)
            {
                await DiscardOnce(answer).ConfigureAwait(false);
                retry.Cancel(generation); return null;
            }
            var transient = answer.Status == HttpStatusCode.TooManyRequests || (int)answer.Status >= 500
                || answer.Error?.Kind is MmErrorKind.Unavailable or MmErrorKind.RateLimited;
            var after = answer.RetryAfter ?? answer.Error?.RetryAfter;
            if (answer.Value is JoinAnswer { RetryAfterSeconds: { } seconds }) after = TimeSpan.FromSeconds(seconds);
            retry.Complete(generation, transient, after);
            return answer;
        }
        catch (ApiException error)
        {
            if (Pause(error)) { retry.Cancel(generation); return null; }
            retry.Complete(generation, transient: true, error.RetryAfter); throw;
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        { retry.Cancel(generation); throw; }
        catch (OperationCanceledException) when (deadline.IsCancellationRequested)
        { retry.Complete(generation, transient: true); throw new TimeoutException("The matchmaking attempt timed out."); }
        catch (Exception error) when (HostFailure(error) || error is TimeoutException)
        { retry.Complete(generation, transient: true); throw; }
        finally { retry.Cancel(generation); }
    }

    private async Task DiscardLateAsync<T>(Task<MmResult<T>> pending, Func<MmResult<T>, Task> discard, CancellationToken cancellation)
    {
        try
        {
            var result = await pending.ConfigureAwait(false);
            Observe(result);
            if (!OnlineOpen || cancellation.IsCancellationRequested) await discard(result).ConfigureAwait(false);
        }
        catch (Exception error) when (HostFailure(error) || error is TimeoutException) { }
    }

    /// <summary>Terminal for this play session: cancel work and release its relay before any late result can be published.</summary>
    public Task SuspendOnlineAsync()
    {
        lock (_mineState) return _suspending ??= SuspendCoreAsync();
    }

    private async Task SuspendCoreAsync()
    {
        if (Interlocked.Exchange(ref _suspended, 1) != 0) return;
        _mm.BeginCleanup();
        try
        {
            try { _onlineStop.Cancel(); } catch (AggregateException) { }
            CancellationTokenSource? stop;
            string? world;
            lock (_hostState)
            {
                stop = _hostStop; _hostStop = null;
                _hostGeneration++;
                world = HostedWorldId;
                Volatile.Write(ref _hostedWorldId, null);
                _host.Stop();
            }
            try { stop?.Cancel(); } catch (AggregateException) { }
            stop?.Dispose();
            string? grant;
            lock (_mineState)
            {
                grant = _seat.GrantId;
                _seat.Left();
                InvalidateMine(reset: true);
            }
            using var budget = new CancellationTokenSource(TimeSpan.FromSeconds(10));
            // the host's relay first (game exit, session end, pause): its legs stop with its socket
            var stopping = StopRelayAsync();
            try {await ClearProofAsync(CancellationToken.None).ConfigureAwait(false);}
            catch(Exception error) when(HostFailure(error)) { }
            await stopping.WaitAsync(budget.Token).ConfigureAwait(false);
            var close = world is not null ? CloseWorldAsync(world, budget.Token) : Task.CompletedTask;
            var leave = grant is not null ? ReleaseGrantAsync(grant, budget.Token) : Task.CompletedTask;
            await Task.WhenAll(close, leave).WaitAsync(budget.Token).ConfigureAwait(false);
        }
        catch (Exception error) when (HostFailure(error) || error is TimeoutException) { }
        finally { _mm.EndCleanup(); }
    }

    private async Task ReleaseGrantAsync(string grant, CancellationToken cancellation = default)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        timeout.CancelAfter(TimeSpan.FromSeconds(5));
        try { await _mm.LeaveGrantAsync(grant, timeout.Token).WaitAsync(timeout.Token).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error) || error is TimeoutException) { }
    }

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1031:Do not catch general exception types",
        Justification = "Optional diagnostic telemetry must never bypass denial or prevent admission rollback.")]
    private string DeniedMessage(WorldAccessDecision decision, string action)
    {
        try { worldAccessDenied?.Invoke(action); }
        catch (Exception) { }
        return _text(decision.MessageKey);
    }

    /// <summary>The seat this session holds, when one is reserved (for the shell's status).</summary>
    public string? GrantId => _seat.GrantId;

    /// <summary>Integrated admission needs the contract's three-second read cadence; Rust keeps its existing loop.</summary>
    public TimeSpan HostPollInterval => TimeSpan.FromSeconds(refreshRelayAdmissions is null ? 5 : 3);

    public async Task<UiFrame.WorldsList> ListAsync(UiFrame.WorldsQuery query, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(query);
        RequestReconcile();
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
            return new(query.Filter, 2, DeniedMessage(denied, "list"), []);
        var filter = query.Filter switch
        {
            1 => WorldFilters.Friends,
            2 => WorldFilters.Near,
            _ => WorldFilters.Global,
        };
        try
        {
            var answer = await OperationAsync(_listRetry, token => _mm.ListAsync(filter, query.NotFull, token), cancellation).ConfigureAwait(false);
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
                return new(query.Filter, 2, DeniedMessage(changed, "list"), []);
            if (answer is not { Ok: true, Value: { } list })
            {
                return new UiFrame.WorldsList(query.Filter, 2, _text("worlds.list_failed"), []);
            }
            foreach (var world in list.Items.Concat(list.Locked)) _listedActivity[world.Id] = world.Activity;
            var rows = list.Items.Select(w => WithActivity(Row(w, false), w)).Concat(list.Locked.Select(w => WithActivity(Row(w, true), w))).ToList();
            return new UiFrame.WorldsList(query.Filter, 0, string.Empty, rows);
        }
        catch (Exception e) when (HostFailure(e))
        {
            return new UiFrame.WorldsList(query.Filter, 2, _text("worlds.list_failed"), []);
        }
    }

    /// <summary>A join by id or code. One live seat per profile: a seat elsewhere is left first (the player chose this world).</summary>
    private int _joinAttempt;
    public Task<UiFrame.JoinResult> JoinAsync(UiFrame.JoinWorld request,CancellationToken cancellation) => JoinAsync(request, null, cancellation);

    /// <summary>As <see cref="JoinAsync(UiFrame.JoinWorld, CancellationToken)"/>, asking for <paramref name="activity"/> (OD-150; null: any).</summary>
    /// <paramref name="kind"/>: the lobby ask the seat is taken for (<see cref="FreeRoamKind"/>; the overlay's join is free roam).
    private async Task<UiFrame.JoinResult> JoinAsync(UiFrame.JoinWorld request, string? activity, CancellationToken cancellation, byte kind = FreeRoamKind.FreeRoam)
    {
        if(Interlocked.CompareExchange(ref _joinAttempt,1,0)!=0)return new(UiJoin.SlowDown,_text("worlds.slow_down"));
        CancelFreeRoamLeave(); // the new seat is never let go by an older leave of free roam
        try {return await JoinCoreAsync(request,activity,kind,cancellation).ConfigureAwait(false);}
        finally {Volatile.Write(ref _joinAttempt,0);}
    }
    private async Task<UiFrame.JoinResult> JoinCoreAsync(UiFrame.JoinWorld request, string? activity, byte kind, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
        Volatile.Write(ref _lastJoinRefusal, null);
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        if (HostingOrOpening || Volatile.Read(ref _mineSnapshot).Role == UiWorldRole.Hosting)
        {
            _udpDiagnostics.Managed(Diagnostics.UdpDiagnostics.ManagedPhase.JoinHostBlocked);
            return new(UiJoin.NotAllowed, _text("worlds.close_before_join"));
        }
        RequestReconcile();
        var joinEpoch=Volatile.Read(ref _mineEpoch);
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
            return new(UiJoin.NotAllowed, DeniedMessage(denied, "join"));
        if (GameReadiness.CurrentGeneration is not { } generation)
            return new(UiJoin.NotAllowed, _text("worlds.load_game"));
        var worldId = string.IsNullOrEmpty(request.WorldId) ? null : request.WorldId;
        var code = string.IsNullOrEmpty(request.Code) ? null : request.Code;
        if (code is not null && WorldCode.Normalize(code) is null)
        {
            return new UiFrame.JoinResult(UiJoin.Code, _text("worlds.code_wrong"));
        }
        // OD-112: the session's set goes with every join; planned mods not confirmed yet never join as vanilla.
        var (mods, modsRefusal) = WireMods();
        if (modsRefusal is not null) return new(UiJoin.NotAllowed, _text(modsRefusal));
        // CR-S57: only a hook that reported the relay capability gets nat and relays (also {}); without it both stay out (direct only).
        string? nat = null;
        IReadOnlyDictionary<string, int>? relays = null;
        if (RelayJoinerCapable)
        {
            var report = await RelayReportAsync(cancellation).ConfigureAwait(false);
            (nat, relays) = (report?.Nat, report?.Relays ?? new Dictionary<string, int>());
        }
        try
        {
            for (var attempt = 0; attempt < 2; ++attempt)
            {
                if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
                    return new(UiJoin.NotAllowed, DeniedMessage(changed, "join"));
                if (!GameReadiness.Matches(generation)) return new(UiJoin.NotAllowed, _text("worlds.load_game"));
                var answer = await OperationAsync(_joinRetry, token => _mm.JoinAsync(worldId, code, mods, nat, relays, RelayTransport, activity, token), cancellation,
                    result => result.Value?.Grant is { } late ? ReleaseGrantAsync(late.Id) : Task.CompletedTask).ConfigureAwait(false);
                if (answer is null) return new(UiJoin.SlowDown, _text("worlds.slow_down"));
                WorldAccessDecision after;
                try { after = await AccessAsync(cancellation).ConfigureAwait(false); }
                catch (OperationCanceledException)
                {
                    if (answer.Value?.Grant is { } canceled) await ReleaseGrantAsync(canceled.Id, CancellationToken.None).ConfigureAwait(false);
                    throw;
                }
                if (!after.Allowed)
                {
                    if (answer.Value?.Grant is { } stale) await ReleaseGrantAsync(stale.Id, CancellationToken.None).ConfigureAwait(false);
                    return new(UiJoin.NotAllowed, DeniedMessage(after, "join"));
                }
                if(HostingOrOpening)
                {
                    if(answer.Value?.Grant is {} staleHost)await ReleaseGrantAsync(staleHost.Id,CancellationToken.None).ConfigureAwait(false);
                    return new(UiJoin.NotAllowed,_text("worlds.close_before_join"));
                }
                if (answer.Value is not { } join)
                {
                    if (answer.Error is { Kind: MmErrorKind.Conflict, Reason: ConflictReasons.Activity })
                    {
                        // OD-150: the world runs another activity now; like a closed world, the free-roam step tries the next one
                        _log($"worlds: join refused: {worldId ?? "the code's world"} does not run {activity ?? "the asked activity"} now");
                        return new UiFrame.JoinResult(UiJoin.Closed, _text("worlds.closed"));
                    }
                    if (ConflictReasons.RelayKey(answer.Error) is { } relayKey)
                    {
                        _log($"worlds: join refused: {answer.Error?.Reason}");
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, _text(relayKey));
                    }
                    return new UiFrame.JoinResult(UiJoin.Closed, _text(answer.Error?.Kind == MmErrorKind.WorldNotFound ? "worlds.closed" : "worlds.join_failed"));
                }
                switch (join.Status)
                {
                    case JoinStatuses.Granted when join.Grant is { } grant:
                        if (worldId is not null && grant.WorldId != worldId)
                        {
                            await ReleaseGrantAsync(grant.Id, CancellationToken.None).ConfigureAwait(false);
                            return new(UiJoin.NotAllowed, DeniedMessage(WorldAccessDecision.Inconclusive, "join"));
                        }
                        if (!GameReadiness.Matches(generation))
                        {
                            await ReleaseGrantAsync(grant.Id, CancellationToken.None).ConfigureAwait(false);
                            return new(UiJoin.NotAllowed, _text("worlds.load_game"));
                        }
                        var proofInstalled=false;
                        try {proofInstalled=await SendProofAsync(grant.Relay,grant.WorldId,grant.Id,joinEpoch,cancellation,grant.Transport).ConfigureAwait(false);}
                        catch(Exception error) when(HostFailure(error)) { _log($"worlds: socket proof context failed: {error.GetType().Name}"); }
                        if(!proofInstalled || joinEpoch!=Volatile.Read(ref _mineEpoch))
                        {
                            await ReleaseGrantAsync(grant.Id,CancellationToken.None).ConfigureAwait(false);
                            return new(UiJoin.NotAllowed,_text("worlds.join_failed"));
                        }
                        var accepted = false;
                        lock (_mineState)
                        {
                            if (OnlineOpen && !cancellation.IsCancellationRequested && GameReadiness.Matches(generation) && joinEpoch==Volatile.Read(ref _mineEpoch))
                            {
                                _seat.Granted(grant, Now);
            Volatile.Write(ref _seatWorld, grant.WorldId);
                                SeatTakenFor(grant.Id, kind);
                                InvalidateMine(reset: true);
                                accepted = OnlineOpen && !cancellation.IsCancellationRequested;
                                if (!accepted) _seat.Left();
                            }
                        }
                        if (!accepted)
                        {
                            await ReleaseGrantAsync(grant.Id, CancellationToken.None).ConfigureAwait(false);
                            return new(UiJoin.NotAllowed, _text("worlds.action_failed"));
                        }
                        await ForceRelayAsync(grant.Id, cancellation).ConfigureAwait(false); // FI_RELAY_FORCE only
                        // where the host is now, so the player picks that mode in the game (never "go to free roam" blindly)
                        return new UiFrame.JoinResult(UiJoin.Reserved, ListedActivity(grant.WorldId) is { } hostActivity
                            ? _text("worlds.reserved_mode").Replace("{mode}", ActivityName(hostActivity), StringComparison.Ordinal)
                            : _text("worlds.reserved"));
                    case JoinStatuses.Locked when attempt == 0:
                        // A used seat in this lobby is also Locked. An invite code does not
                        // reveal its target here, so never destroy a live seat on that ambiguity.
                        if (join.Locked is not { } locked || worldId is null || locked.WorldId == worldId)
                        {
                            _udpDiagnostics.Managed(Diagnostics.UdpDiagnostics.ManagedPhase.JoinSeatPreserved);
                            return new(UiJoin.NotAllowed, _text("worlds.already_in_lobby"));
                        }
                        if (HostingOrOpening || joinEpoch!=Volatile.Read(ref _mineEpoch))
                            return new(UiJoin.NotAllowed, _text("worlds.close_before_join"));
                        var left = await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
                        Observe(left);
                        if (!left.Ok) return new(UiJoin.NotAllowed, _text("worlds.join_failed"));
                        if (!OnlineOpen) return new(UiJoin.NotAllowed, _text("worlds.action_failed"));
                        _seat.Left();
                        continue;
                    case JoinStatuses.Full:
                        return new UiFrame.JoinResult(UiJoin.Full, _text("worlds.full"));
                    case JoinStatuses.Code:
                        return new UiFrame.JoinResult(UiJoin.Code, _text(code is null ? "worlds.code_needed" : "worlds.code_wrong"));
                    case JoinStatuses.Closed:
                        return new UiFrame.JoinResult(UiJoin.Closed, _text("worlds.closed"));
                    case JoinStatuses.RateLimited:
                        return new UiFrame.JoinResult(UiJoin.SlowDown, _text("worlds.slow_down"));
                    case JoinStatuses.NotAllowed when join.Reason == NotAllowedReasons.Mods:
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, ModsRefusal(join.Details));
                    case JoinStatuses.NotAllowed:
                        Volatile.Write(ref _lastJoinRefusal, join.Reason);
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, _text("worlds.not_allowed_" + (join.Reason ?? "host")));
                }
                break;
            }
            return new UiFrame.JoinResult(UiJoin.NotAllowed, _text("worlds.join_failed"));
        }
        catch (Exception e) when (HostFailure(e))
        {
            return new UiFrame.JoinResult(UiJoin.Closed, _text("worlds.join_failed"));
        }
    }

    /// <summary>
    /// Called every few seconds: renews the reserved seat when due. Returns a toast text when the
    /// seat ended (lapsed, the world closed), else null.
    /// </summary>
    public async Task<string?> TickAsync(CancellationToken cancellation)
    {
        if(Volatile.Read(ref _joinAttempt)!=0)return null;
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
        {
            var hadSeat = _seat.GrantId is not null;
            await LeaveAsync(cancellation).ConfigureAwait(false);
            return hadSeat ? _text(denied.MessageKey) : null;
        }
        var now = Now;
        var next = _seat.Next(now);
        if (next.Step == SeatStep.Stop && next.Why is SeatDecision.Lapsed or SeatDecision.HoldReached)
        {
            _seat.Left();
            InvalidateMine(reset: true);
            await ClearSeatEndAsync().ConfigureAwait(false); // the hook's context and relay leg end with the seat
            return _text("worlds.seat_ended");
        }
        if (next.Step != SeatStep.Renew || next.GrantId is not { } grantId)
        {
            return null;
        }
        try
        {
            var epoch = Volatile.Read(ref _mineEpoch);
            var renewed = await OperationAsync(_renewRetry, token => _mm.RenewSeatAsync(grantId, token), cancellation).ConfigureAwait(false);
            if (renewed is null || epoch != Volatile.Read(ref _mineEpoch) || _seat.GrantId != grantId) return null;
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
            {
                await LeaveAsync(CancellationToken.None).ConfigureAwait(false);
                return _text(changed.MessageKey);
            }
            if(renewed is {Ok:true,Value:{} proofSeat} && (!ClaimSeatProof(proofSeat,ref epoch,grantId)
                || !await SendSeatProofAsync(proofSeat.Seat,epoch,cancellation).ConfigureAwait(false)))return null;
            lock (_mineState)
            {
                if (!OnlineOpen || cancellation.IsCancellationRequested || epoch != Volatile.Read(ref _mineEpoch)
                    || _seat.GrantId != grantId) return null;
                if (!renewed.Ok || renewed.Value is not { } seat)
                {
                    _seat.RenewFailed(Now, renewed.RetryAfter);
                    // CR-S57: a relay refusal is shown (the seat itself stays until it lapses)
                    return ConflictReasons.RelayKey(renewed.Error) is { } relayKey ? _text(relayKey) : null;
                }
                if (seat.Seat?.GrantId is { } returned && returned != grantId) { RequestReconcile(); return null; }
                var seatAt = DateTimeOffset.TryParse(seat.GeneratedAt, System.Globalization.CultureInfo.InvariantCulture,
                    System.Globalization.DateTimeStyles.AssumeUniversal, out var generated) ? generated : (DateTimeOffset?)null;
                if (seatAt is { } observed && _lastSeatAt is { } previous && observed < previous)
                {
                    RequestReconcile();
                    return null;
                }
                var after = _seat.Observe(seat, Now);
                if (seat.Seat is { } renewedSeat) ObserveActivity(renewedSeat);
                // A successful renew is authoritative too: an older in-flight GET cannot undo it.
                Interlocked.Increment(ref _mineEpoch);
                if (seatAt is not null) _lastSeatAt = seatAt;
                if (after.Step == SeatStep.Stop) InvalidateMine(reset: true);
                return after.Step == SeatStep.Stop && after.Why != SeatDecision.LeftSeat ? _text("worlds.seat_ended") : null;
            }
        }
        catch (Exception e) when (HostFailure(e))
        {
            _seat.RenewFailed(now);
            return null;
        }
    }

    /// <summary>The world this launcher hosts, when open.</summary>
    public string? HostedWorldId => Volatile.Read(ref _hostedWorldId);
    private bool HostingOrOpening {get {lock(_hostState)return _hostedWorldId is not null || _hostStop is not null && !_hostOperation.IsCompleted;}}

    /// <summary>Friend requests and reports from the Worlds tab (OD-63); null: refused with <c>worlds.action_failed</c>.</summary>
    public IWorldSocial? Social { get; init; }

    private static readonly UiFrame.MyWorld NoWorld = new(UiWorldRole.None, string.Empty, string.Empty, string.Empty, string.Empty, 0, 0, 0, false, []);

    // the world the overlay last showed, for the actions on it
    private string? _mineId;
    private byte _mineRole;

    /// <summary>
    /// The overlay's "my world" (OD-63): the world the profile hosts or holds a seat in
    /// (<c>GET /mm/v1/seat?includeMembers=true</c>), its code for the host, and its players.
    /// An omitted roster uses the bounded legacy members route; a failed read retains valid state.
    /// </summary>
    public Task<UiFrame.MyWorld> MyWorldAsync(CancellationToken cancellation) => MineAsync(background: false, cancellation);

    /// <summary>The players of the world as last read (no request): the launcher's moderation panel lists them (CR-S62).</summary>
    public IReadOnlyList<MemberRow> LastMembers => Volatile.Read(ref _mineSnapshot).Members;

    /// <summary>"My world" as last read (no request): the launcher's Discord presence reads it (OD-136, CR-L41.1).</summary>
    public UiFrame.MyWorld LastMyWorld => Volatile.Read(ref _mineSnapshot);

    /// <summary>The <c>not_allowed</c> reason of the last join (e.g. <c>mode</c>), null when it was not one: a link join offers a profile of that mode (CR-L41.2).</summary>
    public string? LastJoinRefusal => Volatile.Read(ref _lastJoinRefusal);
    private string? _lastJoinRefusal;

    /// <summary>Periodic reconciliation sleeps after an idle free session has a confirmed empty seat.</summary>
    public Task<UiFrame.MyWorld> PollMyWorldAsync(CancellationToken cancellation) => MineAsync(background: true, cancellation);

    /// <summary>An explicit UI/action/reconnect asks for one fresh reconciliation without erasing a good roster.</summary>
    public void RequestReconcile()
    {
        lock (_mineState)
        {
            _mineUncertain = true;
            Interlocked.Increment(ref _mineEpoch);
            Interlocked.Exchange(ref _mineNextTicks, 0);
        }
    }

    private Task<UiFrame.MyWorld> MineAsync(bool background, CancellationToken cancellation)
    {
        lock (_mineState)
        {
            if (!OnlineOpen) { _ = SuspendOnlineAsync(); return Task.FromResult(NoWorld); }
            if(Volatile.Read(ref _joinAttempt)!=0)return Task.FromResult(_mineSnapshot);
            if (_mineRead is { IsCompleted: false }) return _mineRead.WaitAsync(cancellation);
            var settled = _mineReconciled && !_mineUncertain && _pendingRequestUntil is null && !_joinRetry.Running && !_createRetry.Running;
            if (background && idleWhenNoWorld && settled && HostedWorldId is null && _seat.GrantId is null && _mineSnapshot.Role == UiWorldRole.None)
                return Task.FromResult(_mineSnapshot);
            var quiet = _mineSnapshot.Role == UiWorldRole.None ? QuietSeatReadNoWorld : QuietSeatReadInWorld;
            if (background && settled && Now.UtcTicks < Volatile.Read(ref _mineReadTicks) + quiet.Ticks) return Task.FromResult(_mineSnapshot);
            _mineRead = ReadMineAsync(cancellation);
            return _mineRead;
        }
    }

    private async Task<UiFrame.MyWorld> ReadMineAsync(CancellationToken cancellation)
    {
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false })
        {
            InvalidateMine(reset: true);
            return NoWorld;
        }
        var epoch = Volatile.Read(ref _mineEpoch);
        try
        {
            if (Now.UtcTicks < Volatile.Read(ref _mineNextTicks)) return _mineSnapshot;
            Interlocked.Exchange(ref _mineNextTicks, (Now + SeatReadEvery).UtcTicks);
            Interlocked.Exchange(ref _mineReadTicks, Now.UtcTicks);
            var answer = await OperationAsync(_seatReadRetry, _mm.SeatWithMembersAsync, cancellation).ConfigureAwait(false);
            if (!OnlineOpen) return NoWorld;
            if (epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
            if (answer is not { Ok: true, Value: { } value }) { _mineUncertain = true; return _mineSnapshot; }
            var seatAt = DateTimeOffset.TryParse(value.GeneratedAt, System.Globalization.CultureInfo.InvariantCulture,
                System.Globalization.DateTimeStyles.AssumeUniversal, out var observedAt) ? observedAt : (DateTimeOffset?)null;
            if (seatAt is { } observed && _lastSeatAt is { } previous && observed < previous) return _mineSnapshot;
            if(!ClaimSeatProof(value,ref epoch) || !await SendSeatProofAsync(value.Seat,epoch,cancellation).ConfigureAwait(false))
            { _mineUncertain=true;return _mineSnapshot; }
            if (value.Seat is not { } seat)
            {
                lock (_mineState)
                {
                    if (!OnlineOpen || epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
                    ObserveRequest(value.Request);
                    _mineReconciled = true; _mineUncertain = false;
                    if (seatAt is not null) _lastSeatAt = seatAt;
                    _seat.Observe(value, Now);
                    (_mineId, _mineRole) = (null, UiWorldRole.None);
                    _lastRosterAt = null;
                    _roster.Reset();
                    _publicWarning.Reset();
                    _seatActivity = null;
                    CrashContext?.ObserveWorld(null);
                    return _mineSnapshot = NoWorld;
                }
            }
            var role = seat.State switch
            {
                SeatStates.Hosting => UiWorldRole.Hosting,
                SeatStates.Seated => UiWorldRole.Seated,
                _ => UiWorldRole.Reserved,
            };
            var world = seat.World;
            if (world.Id != seat.WorldId) { _mineUncertain = true; return _mineSnapshot; }
            var members = new List<MemberRow>();
            // Only an omitted roster uses the bounded legacy route. An embedded but unbound roster is not a fallback signal.
            var embedded = value.Members is not null;
            var roster = embedded ? new MmResult<WorldMembers>(HttpStatusCode.OK, value.Members, null, null)
                : await OperationAsync(_membersRetry, token => _mm.MembersAsync(seat.WorldId, token), cancellation).ConfigureAwait(false);
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false })
            {
                InvalidateMine(reset: true);
                return NoWorld;
            }
            if (epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
            if (embedded && value.Members!.WorldId != seat.WorldId) { _mineUncertain = true; return _mineSnapshot; }
            var named = roster is { Ok: true, Value.Items: { } items } && items.All(member => member is not null)
                && (roster.Value.WorldId is null || roster.Value.WorldId == seat.WorldId);
            var boundedRoster = named ? roster!.Value! with { Items = roster.Value!.Items.Take(12).ToArray() } : null;
            if (named && _mineId == seat.WorldId && _lastRosterAt is { } latest
                && DateTimeOffset.TryParse(roster!.Value!.GeneratedAt, System.Globalization.CultureInfo.InvariantCulture,
                    System.Globalization.DateTimeStyles.AssumeUniversal, out var generated) && generated <= latest)
                return _mineSnapshot;
            if (named)
            {
                members.AddRange(boundedRoster!.Items.Select(Member));
            }
            else
            {
                if (_mineId == seat.WorldId && _mineSnapshot.Named) { _mineUncertain = true; return _mineSnapshot; }
                members.Add(new MemberRow(world.Host.Xuid, world.Host.Gamertag, UiTier.Of(world.Host.Tier), 0,
                    role == UiWorldRole.Hosting ? UiFriend.Self : UiFriend.None));
            }
            var row = Row(world, false);
            lock (_mineState)
            {
                if (!OnlineOpen || epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
                _mineReconciled = true; _mineUncertain = false;
                if (seatAt is not null) _lastSeatAt = seatAt;
                ObserveRequest(value.Request);
                if (seat.State != SeatStates.Reserved || seat.GrantId is not null && seat.ExpiresAt is not null
                    && seat.HoldUntil is not null && _seat.GrantId != seat.GrantId) _seat.Observe(value, Now);
                if (_mineId != seat.WorldId) { _lastRosterAt = null; _roster.Reset(); }
                if (_mineId != seat.WorldId || _mineRole != role) Interlocked.Increment(ref _mineEpoch);
                (_mineId, _mineRole) = (seat.WorldId, role);
                if (named)
                {
                    _roster.Observe(seat.WorldId, boundedRoster!);
                    _lastRosterAt = DateTimeOffset.TryParse(boundedRoster!.GeneratedAt, System.Globalization.CultureInfo.InvariantCulture,
                        System.Globalization.DateTimeStyles.AssumeUniversal, out var time) ? time : null;
                }
                if (role is UiWorldRole.Hosting or UiWorldRole.Seated) _publicWarning.Observe(seat.WorldId, world.Audience);
                ObserveActivity(seat);
                CrashContext?.ObserveWorld(role is UiWorldRole.Hosting or UiWorldRole.Seated ? new(seat.WorldId, world.Audience, Now, named,
                    members.Take(12).Select(m => new CrashParticipant(m.Xuid, m.Gamertag,
                        m.Role switch { 0 => "host", 1 => "seated", _ => "reserved" })).ToArray()) : null);
                return _mineSnapshot = new UiFrame.MyWorld(role, seat.WorldId, world.Name, world.Host.Gamertag, world.Code ?? string.Empty, row.Audience, row.Players, row.Max, named, members,
                    WorldModes.OpenToBoth(world));
            }
        }
        catch (Exception e) when (HostFailure(e) || e is TimeoutException)
        {
            if (!OnlineOpen) return NoWorld;
            _mineUncertain = true;
            return _mineSnapshot;
        }
    }

    private void ObserveRequest(SeatRequest? request)
    {
        if (request is { State: OfferStates.Pending } && DateTimeOffset.TryParse(request.ExpiresAt,
            System.Globalization.CultureInfo.InvariantCulture, System.Globalization.DateTimeStyles.AssumeUniversal, out var expires)
            && expires > Now)
        {
            if (_pendingRequestId != request.Id) { _pendingRequestId = request.Id; _pendingRequestUntil = expires; }
            else if (_pendingRequestUntil <= Now) { _pendingRequestId = null; _pendingRequestUntil = null; }
            else if (expires < _pendingRequestUntil) _pendingRequestUntil = expires;
        }
        else { _pendingRequestId = null; _pendingRequestUntil = null; }
    }

    /// <summary>One player of the roster as the overlay draws it (the XUID is the action key only).</summary>
    public static MemberRow Member(WorldMember member)
    {
        ArgumentNullException.ThrowIfNull(member);
        return new MemberRow(member.Xuid, member.Gamertag, UiTier.Of(member.Tier),
            member.Role switch { "host" => (byte)0, "seated" => (byte)1, _ => (byte)2 },
            member.Friend switch
            {
                "friends" => UiFriend.Friends,
                "outgoing" => UiFriend.Outgoing,
                "incoming" => UiFriend.Incoming,
                "self" => UiFriend.Self,
                _ => UiFriend.None,
            });
    }

    /// <summary>The overlay's actions on its world (OD-63): close, leave, kick, add a friend, report.</summary>
    public async Task<UiFrame.ActionResult> ActAsync(UiFrame.WorldAction request, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
        UiFrame.ActionResult Done(string key) => new(request.Action, 0, _text(key));
        UiFrame.ActionResult Refused(string key) => new(request.Action, 1, _text(key));
        if (!OnlineOpen) { _ = SuspendOnlineAsync(); return Refused("worlds.action_failed"); }
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        try
        {
            switch (request.Action)
            {
                case UiWorldAction.Close:
                    if (HostedWorldId is null)
                    {
                        return Refused("worlds.not_hosting");
                    }
                    await CloseHostAsync(cancellation).ConfigureAwait(false);
                    _log("worlds: closed from the overlay");
                    return Done("worlds.closed_done");
                case UiWorldAction.Leave:
                    CancelFreeRoamLeave();
                    InvalidateMine(reset: true);
                    // left on purpose: never the world the game's next "find me a lobby" goes back to
                    LeftOnPurpose();
                    _seat.Left();
                    await ClearSeatEndAsync().ConfigureAwait(false); // no relay leg outlives the seat
                    var left = await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
                    Observe(left);
                    _log(left.Ok ? "worlds: seat left from the overlay" : $"worlds: leave refused: {left.Error?.Kind}");
                    return left.Ok ? Done("worlds.left") : Refused("worlds.action_failed");
                case UiWorldAction.Kick:
                    if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
                        return Refused(denied.MessageKey);
                    if (HostedWorldId is not { } id || string.IsNullOrEmpty(request.Xuid) || request.Xuid == _ownXuid)
                    {
                        return Refused("worlds.not_hosting");
                    }
                    var kickedRelay = _relay;
                    var kickedGeneration = Volatile.Read(ref _hostGeneration);
                    var kicked = await OperationAsync(_actionRetry,
                        token => _mm.ModerationAsync(id, new Moderation { Action = ModerationActions.Kick, Xuid = request.Xuid }, token), cancellation).ConfigureAwait(false);
                    if (kicked is null) return Refused("worlds.action_failed");
                    if (OnlineOpen && kicked.Ok && kickedGeneration == Volatile.Read(ref _hostGeneration) && HostedWorldId == id)
                        kickedRelay?.Kick(request.Xuid); // at once; the punches confirm it
                    _log(kicked.Ok ? $"worlds: kicked a player from {id}" : $"worlds: kick refused: {kicked.Error?.Kind}");
                    return kicked.Ok ? Done("worlds.kicked") : Refused("worlds.action_failed");
                case UiWorldAction.AddFriend:
                    if (Social is null || string.IsNullOrEmpty(request.Gamertag))
                    {
                        return Refused("worlds.action_failed");
                    }
                    var (friendOk, friendKey) = await Social.AddFriendAsync(request.Gamertag, cancellation).ConfigureAwait(false);
                    return friendOk ? Done(friendKey) : Refused(friendKey);
                case UiWorldAction.Report:
                    if (Social is null || string.IsNullOrEmpty(request.Xuid) || request.Xuid == _ownXuid)
                    {
                        return Refused("worlds.action_failed");
                    }
                    var reason = request.Reason switch { UiReport.Cheating => "cheating", UiReport.Abuse => "abuse", _ => "other" };
                    var (reportOk, reportKey) = await Social.ReportAsync(request.Xuid, reason, _mineId ?? HostedWorldId, cancellation).ConfigureAwait(false);
                    return reportOk ? Done(reportKey) : Refused(reportKey);
                default:
                    return Refused("worlds.action_failed");
            }
        }
        catch (ApiException error) when (Pause(error)) { return Refused("worlds.action_failed"); }
        catch (Exception e) when (HostFailure(e))
        {
            return Refused("worlds.action_failed");
        }
        finally { RequestReconcile(); }
    }

    /// <summary>
    /// The overlay's "Create world": the relay started, the world opened on matchmaking (PUT with
    /// the relay's port and this PC's LAN addresses), then announced every 15 s by <see cref="HostTickAsync"/>.
    /// The host then picks FREE ROAM ONLINE and the server lands the game on its own relay (§4a).
    /// </summary>
    public Task<UiFrame.Hosted> CreateAsync(UiFrame.CreateWorld request, CancellationToken cancellation) => CreateAsync(request, null, cancellation);

    /// <summary>As <see cref="CreateAsync(UiFrame.CreateWorld, CancellationToken)"/>, opening the world with <paramref name="activity"/> (OD-150; null: free roam).</summary>
    private async Task<UiFrame.Hosted> CreateAsync(UiFrame.CreateWorld request, string? activity, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
        CancelFreeRoamLeave(); // the new world is never closed by an older leave of free roam
        Interlocked.Increment(ref _creating);
        try { return await CreateCoreAsync(request, activity, cancellation).ConfigureAwait(false); }
        finally { Interlocked.Decrement(ref _creating); }
    }

    private async Task<UiFrame.Hosted> CreateCoreAsync(UiFrame.CreateWorld request, string? activity, CancellationToken cancellation)
    {
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
            return new(1, string.Empty, string.Empty, DeniedMessage(denied, "create"));
        uint? generation = null;
        if (_requireGameReadiness)
        {
            if (GameReadiness.CurrentGeneration is not { } current)
                return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
            generation = current;
        }
        bool GameStillReady() => generation is not { } loaded || GameReadiness.Matches(loaded);
        // OD-112: a world carries the session's set from its PUT on; planned mods not confirmed yet never open a vanilla world.
        var (mods, modsRefusal) = WireMods();
        if (modsRefusal is not null) return new(1, string.Empty, string.Empty, _text(modsRefusal));
        InvalidateMine(reset: true);
        var operation = QueueHostOperation(replace: true, create: true);
        await operation.Previous.ConfigureAwait(false);
        using var stop = CancellationTokenSource.CreateLinkedTokenSource(operation.Stop, cancellation);
        string? openingId = null;
        var hosted = false;
        try
        {
            if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } changed)
                return new(1, string.Empty, string.Empty, DeniedMessage(changed, "create"));
            if (!GameStillReady()) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
            await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
            if (!IsCurrent(operation) || stop.IsCancellationRequested) return HostingFailed();
            if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } beforeRelay)
                return new(1, string.Empty, string.Empty, DeniedMessage(beforeRelay, "create"));
            var players = (uint)Math.Clamp((int)request.MaxPlayers, 2, 12);
            _relay = _startRelay?.Invoke(players);
            if (_relay is null) return new UiFrame.Hosted(2, string.Empty, string.Empty, _text(RelayFailureKey?.Invoke() ?? relayUnavailableKey));
            if (_relay is IntegratedWorldRelay integrated)
            {
                integrated.CrashContext = CrashContext; integrated.DiagnosticLog = _log;
                integrated.GameSessions = GameModeSessionAsync;
            }
            var relay = _relay;
            // CR-S57: the host's NAT test and relay round trips, read when the world opens (the host's relay legs already exist)
            var relayReport = await RelayReportAsync(stop.Token).ConfigureAwait(false);
            // OD-150: a world opened for a Road Trip ask says so from its PUT, when its server runs one; every other world is free roam
            var opening = activity == WorldActivities.RoadTrip && RunsRoadTrip(relay) ? WorldActivities.RoadTrip : WorldActivities.FreeRoam;
            var settings = new WorldSettings
            {
                Name = string.IsNullOrWhiteSpace(request.Name) ? "Infinity" : request.Name.Trim()[..Math.Min(48, request.Name.Trim().Length)],
                Mode = _mode,
                // OD-141 (CR-L43): the host's own mode (the default) or open to Hardcore and Noob; fixed once the world opens
                Modes = request.AnyMode ? WorldModes.Any : _mode,
                MaxPlayers = players,
                Audience = request.Audience switch { 1 => WorldAudiences.Supporters, 2 => WorldAudiences.Friends, 3 => WorldAudiences.Private, _ => WorldAudiences.Everyone },
                MaxRttMs = request.Latency switch { 1 => 150u, 2 => 100u, 3 => 60u, _ => null },
                Races = true,
                Endpoint = Announcement(relay),
                Mods = mods,
                Nat = relayReport?.Nat,
                Relays = relayReport?.Relays,
                Transport = RelayTransport,
                Lobbies = relay.Lobbies,
                Activity = opening,
            };
            for (var attempt = 0; attempt < 2; ++attempt)
            {
                stop.Token.ThrowIfCancellationRequested();
                if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } beforePut)
                    return new(1, string.Empty, string.Empty, DeniedMessage(beforePut, "create"));
                if (!GameStillReady()) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                openingId = WorldHostScheduler.NewWorldId();
                var minted = openingId;
                var pending = OperationAsync(_createRetry, token => _mm.PutWorldAsync(minted, settings, token), stop.Token,
                    result => result.Ok ? CloseWorldAsync(minted, CancellationToken.None) : Task.CompletedTask);
                var opened = await pending.ConfigureAwait(false);
                stop.Token.ThrowIfCancellationRequested();
                if (!IsCurrent(operation)) throw new OperationCanceledException(stop.Token);
                if (opened is { Ok: true, Value: { } world })
                {
                    if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterPut)
                        return new(1, string.Empty, string.Empty, DeniedMessage(afterPut, "create"));
                    if (!GameStillReady()) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                    if (world.Id != openingId)
                        return new(1, string.Empty, string.Empty, DeniedMessage(WorldAccessDecision.Inconclusive, "create"));
                    if (!GameStillReady()) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                    if (updateRelayWorld is not null)
                        await updateRelayWorld(relay, world, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false);
                    stop.Token.ThrowIfCancellationRequested();
                    if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterPolicy)
                        return new(1, string.Empty, string.Empty, DeniedMessage(afterPolicy, "create"));
                    if (!GameStillReady()) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                    lock (_hostState)
                    {
                        if (_hostGeneration != operation.Generation) throw new OperationCanceledException(stop.Token);
                        _host.Hosting(openingId, Now);
                        _host.Opened(Now);
                        _announcedExternalPort = null;
                        _hostLastSuccess = Now;
                        _hostLastStamp = _clock.GetTimestamp();
                        (_hostJoining, _hostAloneSince, _hostAloneWarned) = (true, _clock.GetTimestamp(), false);
                        _nextAdmissionsRefresh = Now + AdmissionsEvery;
                        Volatile.Write(ref _hostedCode, world.Code);
                        Volatile.Write(ref _hostedMods, mods ?? []);
                        Volatile.Write(ref _hostedPlayers, world.Players);
                        Volatile.Write(ref _hostedTransport, world.Transport);
                        Volatile.Write(ref _hostedRelayName, world.Relay?.Name);
                        Volatile.Write(ref _hostedActivity, opening);
                        Volatile.Write(ref _hostedWorldId, openingId);
                        _publicWarning.Observe(openingId, world.Audience);
                    }
                    hosted = true;
                    _log($"worlds: world {openingId} open ({settings.Mode}{(request.AnyMode ? " + any" : string.Empty)}, {settings.Audience}, {players} players{(opening == WorldActivities.RoadTrip ? ", road trip" : string.Empty)})");
                    return new UiFrame.Hosted(0, openingId, world.Code ?? string.Empty, string.Empty);
                }
                openingId = null; // an explicit refusal did not open this id
                if (opened?.Error is { Kind: MmErrorKind.AlreadyHosting, WorldId: { } old } && attempt == 0)
                {
                    await _mm.CloseAsync(old, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false);
                    continue;
                }
                await StopRelayAsync().ConfigureAwait(false);
                _log($"worlds: open refused: {opened?.Error?.Kind}");
                var key = opened?.Error?.Kind switch
                {
                    MmErrorKind.ModeMismatch => "worlds.host_mode",
                    MmErrorKind.Conflict when opened?.Error?.Reason == ConflictReasons.Modes => "worlds.host_modes_fixed",
                    MmErrorKind.Invalid when opened?.Error?.Field == "modes" => "worlds.host_mode",
                    MmErrorKind.Conflict when opened?.Error?.Reason == ConflictReasons.Mods => "worlds.mods_changed",
                    MmErrorKind.Conflict when ConflictReasons.RelayKey(opened?.Error) is { } relayKey => relayKey,
                    MmErrorKind.Invalid when opened?.Error?.Field == "mods" => "worlds.mods_failed",
                    MmErrorKind.RateLimited => "worlds.slow_down",
                    _ => "worlds.host_failed",
                };
                return new UiFrame.Hosted(1, string.Empty, string.Empty, _text(key));
            }
        }
        catch (ApiException error) when (Pause(error)) { }
        catch (Exception e) when (HostFailure(e))
        {
            // The finally block also covers exceptions from relay startup, settings and the policy callback.
        }
        finally
        {
            try
            {
                if (!hosted)
                {
                    _host.Stop();
                    Volatile.Write(ref _hostedWorldId, null);
                    await StopRelayAsync().ConfigureAwait(false);
                    if (openingId is not null) await CloseWorldAsync(openingId, CancellationToken.None).ConfigureAwait(false);
                }
            }
            finally { operation.Completion.TrySetResult(); }
        }
        return HostingFailed();
    }

    /// <summary>Every few seconds while hosting: the announce when due (with the relay's players), the server's kicks passed on. A toast text when the world ended.</summary>
    public async Task<string?> HostTickAsync(CancellationToken cancellation)
    {
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
        {
            var hadHost = HostedWorldId is not null;
            await CloseHostAsync(cancellation).ConfigureAwait(false);
            return hadHost ? _text(denied.MessageKey) : null;
        }
        var operation = QueueHostOperation(replace: false, create: false);
        await operation.Previous.ConfigureAwait(false);
        var hostRemaining = TimeSpan.FromSeconds(45) - _clock.GetElapsedTime(_hostLastStamp);
        var wallRemainingAtStart = _hostLastSuccess.AddSeconds(45) - Now;
        if (wallRemainingAtStart < hostRemaining) hostRemaining = wallRemainingAtStart;
        var budget = hostRemaining > TimeSpan.Zero && hostRemaining < TimeSpan.FromSeconds(10) ? hostRemaining : TimeSpan.FromSeconds(10);
        using var deadline = new CancellationTokenSource(budget, _clock);
        using var stop = CancellationTokenSource.CreateLinkedTokenSource(operation.Stop, cancellation, deadline.Token);
        try
        {
            if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } beforeHost)
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text(beforeHost.MessageKey);
            }
            if (!IsCurrent(operation) || !_host.IsOpen || _host.WorldId is not { } id || _relay is not { } relay) return null;
            if (!HostedModsCurrent())
            {
                // a world's set is fixed (§2.7): a session whose set changed closes it; the player opens a new one
                _log("worlds: the session's mod set changed; closing the world");
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.mods_changed");
            }
            if (!relay.Running)
            {
                _log("worlds: the relay ended on its own");
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.host_ended");
            }
            var now = Now;
            if (HostExpired())
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.host_ended");
            }
            var step = _host.Next(now).Step;
            // OD-150: the host's game changed activity: said on this heartbeat, sent now instead of at the next one
            var activity = ActivityChange(relay);
            if (step != HostStep.Announce && !(step == HostStep.Wait && (activity is not null || ExternalPortMoved(relay, now))))
            {
                if (refreshRelayAdmissions is null || now < _nextAdmissionsRefresh) return null;
                _nextAdmissionsRefresh = now + AdmissionsEvery;
                try
                {
                    if (!_admissionsRetry.TryBegin(out var refreshGeneration)) return null;
                    try
                    {
                        await refreshRelayAdmissions(relay, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false);
                        stop.Token.ThrowIfCancellationRequested();
                        if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterRefresh)
                        {
                            await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                            return _text(afterRefresh.MessageKey);
                        }
                        if (IsCurrent(operation)) _nextAdmissionsRefresh = Now + AdmissionsEvery;
                        _admissionsRetry.Complete(refreshGeneration, transient: false);
                    }
                    catch (ApiException admissionFailure)
                    { _admissionsRetry.Complete(refreshGeneration, transient: true, admissionFailure.RetryAfter); throw; }
                    catch (OperationCanceledException) when (stop.IsCancellationRequested)
                    { _admissionsRetry.Cancel(refreshGeneration); throw; }
                    catch (Exception refreshError) when (HostFailure(refreshError) || refreshError is TimeoutException)
                    { _admissionsRetry.Complete(refreshGeneration, transient: true); throw; }
                }
                catch (InvalidOperationException)
                {
                    await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                    return _text("worlds.host_ended");
                }
                catch (ApiException readFailure) when (OnlinePause.From(readFailure) is null)
                {
                    // The retry circuit retains its header; the host and signed grants keep their original deadlines.
                }
                catch (Exception e) when (e is HttpRequestException or IOException or SocketException or TimeoutException
                    || e is OperationCanceledException && !stop.IsCancellationRequested)
                {
                    // Keep the last policy until its signed admissions expire; this does not renew the host lease.
                }
                return null;
            }
            var peers = (await relay.PeersAsync(stop.Token).WaitAsync(stop.Token).ConfigureAwait(false)).Where(x => x != _ownXuid).ToList();
            if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } beforeBeat)
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text(beforeBeat.MessageKey);
            }
            var remaining = TimeSpan.FromSeconds(45) - _clock.GetElapsedTime(_hostLastStamp);
            var wallRemaining = _hostLastSuccess.AddSeconds(45) - Now;
            if (wallRemaining < remaining) remaining = wallRemaining;
            var endpoint = Announcement(relay);
            var sentExternalPort = endpoint.External?.Port ?? 0;
            var beat = await OperationAsync(_announceRetry, token => _mm.AnnounceAsync(id, new Heartbeat { Peers = peers, Endpoint = endpoint, Activity = activity }, token), stop.Token,
                remaining: remaining).ConfigureAwait(false);
            if (beat is null) return null;
            stop.Token.ThrowIfCancellationRequested();
            if (!IsCurrent(operation)) return null;
            if (HostExpired())
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.host_ended");
            }
            if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterBeat)
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text(afterBeat.MessageKey);
            }
            if (beat.Error is { Code: "AUTH.SESSION_EXPIRED" })
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.host_ended");
            }
            if (beat is { Ok: true, Value: { } world })
            {
                if (world.Id != id)
                {
                    await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                    return _text("worlds.integrity_inconclusive");
                }
                if (updateRelayWorld is not null)
                {
                    try { await updateRelayWorld(relay, world, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false); }
                    catch (Exception e) when (HostFailure(e))
                    {
                        await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                        return _text("worlds.host_ended");
                    }
                }
                stop.Token.ThrowIfCancellationRequested();
                if (!IsCurrent(operation)) return null;
                if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterUpdate)
                {
                    await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                    return _text(afterUpdate.MessageKey);
                }
                _host.Announced(Now);
                _announcedExternalPort = sentExternalPort;
                if (activity is not null)
                {
                    // said once: a heartbeat the server answered carries a change only until it is taken
                    _log($"worlds: activity {Interlocked.Exchange(ref _hostedActivity, activity) ?? "unknown"} -> {activity}"
                        + (world.Activity == activity ? string.Empty : $" (the server keeps {world.Activity})"));
                }
                _hostLastSuccess = Now;
                _hostLastStamp = _clock.GetTimestamp();
                foreach (var punch in world.Punches.Where(p => p.Kicked))
                {
                    relay.Kick(punch.Xuid);
                }
                // A granted player who is not synced yet is still connecting: keep the fast cadence for them.
                _hostJoining = world.Punches.Any(p => !p.Kicked && !peers.Contains(p.Xuid, StringComparer.Ordinal));
                _nextAdmissionsRefresh = Now + AdmissionsEvery;
                Volatile.Write(ref _hostedPlayers, world.Players);
                // A dedicated host has no game of its own and stays open while empty (OD-109).
                if (!_closeWhenAlone) return null;
                // Only the host, no grant, no held seat (pending invites and requests hold one, §3.4).
                var alone = peers.Count == 0 && world.Punches.Count == 0 && world.Players <= 1 && world.OpenSeats + 1 >= world.MaxPlayers;
                if (!alone) { (_hostAloneSince, _hostAloneWarned) = (null, false); return null; }
                var aloneFor = _clock.GetElapsedTime(_hostAloneSince ??= _clock.GetTimestamp());
                if (aloneFor >= AloneCloseAfter)
                {
                    await CloseCurrentHostAsync(id, CancellationToken.None).ConfigureAwait(false);
                    _log($"worlds: world {id} closed after {(int)aloneFor.TotalSeconds} s with no other player");
                    return _text("worlds.host_idle_closed");
                }
                if (aloneFor >= AloneWarnAfter && !_hostAloneWarned)
                {
                    _hostAloneWarned = true;
                    return _text("worlds.host_idle_warning");
                }
                return null;
            }
            if (beat.Error is { } error && _host.Failed(error, Now).Outcome is HostOutcome.WorldGone or HostOutcome.Refused)
            {
                // The server closes a lonely world from its own open time, usually just before ours (OD-107).
                var idle = _closeWhenAlone && _hostAloneWarned && _hostAloneSince is { } since && _clock.GetElapsedTime(since) >= AloneWarnAfter;
                await CloseCurrentHostAsync(id, CancellationToken.None).ConfigureAwait(false);
                _log($"worlds: world {id} ended by the server: {error.Kind}");
                return _text(idle && error.Kind == MmErrorKind.WorldNotFound ? "worlds.host_idle_closed" : "worlds.host_ended");
            }
        }
        catch (ApiException error) when (Pause(error))
        {
            await SuspendOnlineAsync().ConfigureAwait(false);
            return _text("worlds.host_ended");
        }
        catch (OperationCanceledException) when (deadline.IsCancellationRequested && !operation.Stop.IsCancellationRequested && !cancellation.IsCancellationRequested)
        {
            if (IsCurrent(operation) && HostExpired())
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.host_ended");
            }
            if (IsCurrent(operation)) _host.Transient(Now);
        }
        catch (OperationCanceledException)
        {
            await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
        }
        catch (Exception e) when (HostFailure(e))
        {
            if (IsCurrent(operation) && HostExpired())
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.host_ended");
            }
            if (IsCurrent(operation)) _host.Transient(Now);
        }
        finally { operation.Completion.TrySetResult(); }
        return null;
    }

    /// <summary>The host left free roam, closed the world, or the session ended: <c>DELETE</c> and the relay stopped.</summary>
    public async Task CloseHostAsync(CancellationToken cancellation)
    {
        CancelFreeRoamLeave();
        InvalidateMine(reset: true);
        var operation = QueueHostOperation(replace: true, create: false);
        await operation.Previous.ConfigureAwait(false);
        try { await CloseCurrentHostAsync(cancellation).ConfigureAwait(false); }
        finally { operation.Completion.TrySetResult(); }
    }

    private Task CloseCurrentHostAsync(CancellationToken cancellation) => CloseCurrentHostAsync(null, cancellation);

    private async Task CloseCurrentHostAsync(string? worldId, CancellationToken cancellation)
    {
        var id = _host.IsOpen ? _host.WorldId : worldId;
        if (id is not null) InvalidateMine(reset: true);
        _host.Stop();
        Volatile.Write(ref _hostedWorldId, null);
        await StopRelayAsync().ConfigureAwait(false);
        if (id is null)
        {
            return;
        }
        await CloseWorldAsync(id, cancellation).ConfigureAwait(false);
    }

    private async Task CloseWorldAsync(string id, CancellationToken cancellation)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        timeout.CancelAfter(TimeSpan.FromSeconds(5));
        try
        {
            var closed = await _mm.CloseAsync(id, timeout.Token).WaitAsync(timeout.Token).ConfigureAwait(false);
            Observe(closed);
            _log(closed.Ok ? $"worlds: world {id} closed (DELETE ok)" : $"worlds: world {id} DELETE refused: {closed.Error?.Kind}");
        }
        catch (Exception e) when (HostFailure(e))
        {
            // it lapses 45 s after the last announce
            _log($"worlds: world {id} DELETE failed ({e.GetType().Name}); it lapses 45 s after the last announce");
        }
    }

    /// <summary>
    /// Every end of a hosted world comes here (overlay close, free roam left, the server's end, a failed open, the session's
    /// suspension at game exit): the relay's socket closes, so no hello, keep-alive or relay datagram leaves for its grants.
    /// </summary>
    private async Task StopRelayAsync()
    {
        var relay = Interlocked.Exchange(ref _relay, null);
        if (relay is null) return;
        if (relay is IAsyncDisposable asynchronous) await asynchronous.DisposeAsync().ConfigureAwait(false);
        else await Task.Run(relay.Dispose).ConfigureAwait(false);
        _log("worlds: relay stopped");
    }

    /// <summary>The relay's mapped port moved since the last heartbeat: announce early so new hints carry
    /// the live port. Never early to retract one (a moment between STUN answers can read as not cone). At
    /// most one heartbeat per 12 s keeps the account within mm.announce (6/min).</summary>
    private bool ExternalPortMoved(IHostRelay relay, DateTimeOffset now) => _announcedExternalPort is { } announced
        && now - _hostLastSuccess >= TimeSpan.FromSeconds(12) && Announcement(relay).External is { Port: var port } && port != announced;

    // OD-150: the world's server runs a Road Trip (its lobbies), so the world may say it runs one
    private static bool RunsRoadTrip(IHostRelay relay) => relay.Lobbies?.Contains(LobbyKinds.RoadTrip) == true;

    /// <summary>
    /// OD-150: the activity the host's own game runs now (<see cref="IHostRelay.Activity"/>) when it differs from what the
    /// world last said; null when unknown or unchanged. A Road Trip is never said by a world whose server does not run one
    /// (the server would refuse it).
    /// </summary>
    private string? ActivityChange(IHostRelay relay)
    {
        var current = relay.Activity;
        if (current is not (WorldActivities.FreeRoam or WorldActivities.RoadTrip)) return null;
        if (current == WorldActivities.RoadTrip && !RunsRoadTrip(relay)) return null;
        return current == Volatile.Read(ref _hostedActivity) ? null : current;
    }

    private AnnouncePort Announcement(IHostRelay relay)=>RelayAnnouncement.Build(relay.Port,_lan(),relay switch
    { IntegratedWorldRelay integrated=>integrated.NetworkStatus,IntegratedHostRelay host=>host.NetworkStatus,_=>null });

    private sealed record HostOperation(long Generation, Task Previous, TaskCompletionSource Completion, CancellationToken Stop);

    // Queue order and generation changes are atomic. Only the queue owner mutates host/relay state;
    // replacing it cancels network waits before the next owner needs to acquire that state.
    private HostOperation QueueHostOperation(bool replace, bool create)
    {
        CancellationTokenSource? previousStop = null;
        HostOperation operation;
        lock (_hostState)
        {
            if (replace)
            {
                previousStop = _hostStop;
                _hostStop = create ? new CancellationTokenSource() : null;
                _hostGeneration++;
                Volatile.Write(ref _hostedWorldId, null);
            }
            var completion = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            operation = new(_hostGeneration, _hostOperation, completion, _hostStop?.Token ?? new CancellationToken(true));
            _hostOperation = completion.Task;
        }
        if (previousStop is not null)
        {
            try { previousStop.Cancel(); } catch (AggregateException) { }
            previousStop.Dispose();
        }
        return operation;
    }

    private bool IsCurrent(HostOperation operation) => Volatile.Read(ref _hostGeneration) == operation.Generation;
    private UiFrame.Hosted HostingFailed() => new(1, string.Empty, string.Empty, _text("worlds.host_failed"));
    private bool HostExpired() => Now >= _hostLastSuccess.AddSeconds(45)
        || _clock.GetElapsedTime(_hostLastStamp) >= TimeSpan.FromSeconds(45);
    private static bool HostFailure(Exception exception) => exception is HttpRequestException or OperationCanceledException or InvalidOperationException or IOException or SocketException or ArgumentException or TimeoutException;

    /// <summary>A seat that ended here (left, lapsed): the hook's socket context and relay leg are cleared; a failed write is not the seat's.</summary>
    private async Task ClearSeatEndAsync()
    {
        try { await ClearProofAsync(CancellationToken.None).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error)) { }
    }

    /// <summary>The play session is ending: the seat is let go (idempotent on the server).</summary>
    public async Task LeaveAsync(CancellationToken cancellation)
    {
        CancelFreeRoamLeave();
        InvalidateMine(reset: true);
        await ClearProofAsync(CancellationToken.None).ConfigureAwait(false);
        if (_seat.GrantId is null)
        {
            return;
        }
        // the game may come straight back (a failed car meet step retries at once): it rejoins this world first
        if (Volatile.Read(ref _seatWorld) is { } leftWorld) Volatile.Write(ref _recentlyLeft, new RecentWorld(leftWorld, Now));
        _seat.Left();
        try
        {
            var left = await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
            Observe(left);
        }
        catch (Exception e) when (HostFailure(e))
        {
            // the seat lapses on its own within 60 s
        }
    }

    /// <summary>One matchmaking row as the overlay draws it (never an address).</summary>
    public static WorldRow Row(WorldView world, bool lockedSection)
    {
        ArgumentNullException.ThrowIfNull(world);
        var why = world.CanJoin && !lockedSection ? null : lockedSection ? "tier" : world.Why;
        byte lockReason = why switch
        {
            null => 0,
            "tier" => 1, // CR-S69: only an older server's; the overlay says "Not available", never an upsell
            "friends" => 2,
            "code" => 3,
            "full" => 4,
            "latency" => 5,
            // CR-L40: the all list carries other modes' worlds; CR-L43: never a world open to both modes (an
            // inconsistent answer is "not available", never dimmed as another mode's)
            "mode" => WorldModes.OpenToBoth(world) ? (byte)6 : ModeLock(world.Mode),
            _ => 6,
        };
        byte ping = world.PingClass switch
        {
            PingClasses.Local or PingClasses.Near => 1,
            PingClasses.Far => 2,
            PingClasses.Remote => 3,
            _ => 0,
        };
        byte audience = world.Audience switch
        {
            WorldAudiences.Supporters => 1,
            WorldAudiences.Friends => 2,
            WorldAudiences.Private => 3,
            _ => 0,
        };
        return new WorldRow(world.Id, world.Name, world.Host.Gamertag, (byte)Math.Min(world.Players, 255u), (byte)Math.Min(world.MaxPlayers, 255u),
            world.Kind == WorldKinds.Supporter ? (byte)1 : (byte)0, audience, lockReason, ping, world.FriendHost);
    }
}
