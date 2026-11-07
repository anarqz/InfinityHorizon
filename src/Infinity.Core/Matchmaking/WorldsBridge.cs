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
    string mode = "noob", Func<IReadOnlyList<string>>? lanAddresses = null, string? ownXuid = null, Action<string>? log = null,
    Session.GameReadiness? gameReadiness = null,
    Func<CancellationToken, Task<WorldAccessDecision>>? checkWorldAccess = null, Action<string>? worldAccessDenied = null,
    OnlineActivity? onlineActivity = null, bool idleWhenNoWorld = false, Func<double>? retryJitter = null)
{
    // the session profile's mode: what a matching lobby has (CR-L40)
    private readonly string _mode = mode;
    // the seat's join and end in the launcher's log (ids only, never addresses)
    private readonly Action<string> _log = log ?? (_ => { });
    private readonly Diagnostics.UdpDiagnostics _udpDiagnostics=new(log??(_=>{}),clock);
    public void RecordUdpDiagnostic(Session.Frame.UdpDiagnostic frame)=>_udpDiagnostics.Native(frame);
    // this profile's XUID: its own roster row and actions
    private readonly string? _ownXuid = ownXuid;
    private readonly Func<IReadOnlyList<string>> _lan = lanAddresses ?? LocalNetwork.LanAddresses;
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
    private readonly LobbyRoster _roster = new(ownXuid);
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
            var reply = await _mm.SeatAsync(cancellation).ConfigureAwait(false); // a plain read of the caller's seat: reachability only
            result = _text(reply.Ok ? "connection.accepted" : "connection.refused") + " (HTTP " + (int)reply.Status + ")";
            state = reply.Ok ? (byte)1 : (byte)2;
        }
        catch (Exception error) when (HostFailure(error) || error is System.Text.Json.JsonException)
        { result = _text("connection.unavailable"); state = 2; }
        // OD-208: players host no world, so no relay of this PC measures a public address, NAT or UPnP any more
        var text = endpoints + "\n\n" + _text("connection.matchmaking") + ": " + result + "\n" + _text("connection.response") + ": " + started.ElapsedMilliseconds + " ms"
            + (TransportText() is { } transport ? "\n" + _text("connection.transport") + ": " + transport : "")
            + (ConnectionNotice() is { } notice ? "\n" + notice : "") + "\n\n" + _text("connection.snapshot"); // OD-160's notice
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

    /// <summary>Terminal for this play session: cancel work and release its seat before any late result can be published.</summary>
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
            string? grant;
            lock (_mineState)
            {
                grant = _seat.GrantId;
                _seat.Left();
                InvalidateMine(reset: true);
            }
            using var budget = new CancellationTokenSource(TimeSpan.FromSeconds(10));
            try {await ClearProofAsync("online suspended or the session ended",CancellationToken.None).ConfigureAwait(false);}
            catch(Exception error) when(HostFailure(error)) { }
            if (grant is not null) await ReleaseGrantAsync(grant, budget.Token).WaitAsync(budget.Token).ConfigureAwait(false);
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

    /// <summary>The supervisor's seat loop cadence (renewals, the seat read, the watch); each scheduler decides when its work is due.</summary>
    public static readonly TimeSpan SeatLoopEvery = TimeSpan.FromSeconds(3);

    /// <summary>A join by world id (OD-208: no lobby code). One live seat per profile: a seat elsewhere is left first (the player chose this world).</summary>
    private int _joinAttempt;
    public Task<UiFrame.JoinResult> JoinAsync(UiFrame.JoinWorld request,CancellationToken cancellation)
    {
        return JoinAsync(request, null, cancellation);
    }

    /// <summary>As <see cref="JoinAsync(UiFrame.JoinWorld, CancellationToken)"/>, asking for <paramref name="activity"/> (OD-150; null: any).</summary>
    /// <paramref name="kind"/>: the lobby ask the seat is taken for (<see cref="FreeRoamKind"/>; the overlay's join is free roam).
    /// <paramref name="privateCreate"/> (T-L6): the private create, <c>POST /mm/v1/join {activity, private: true}</c>.
    private async Task<UiFrame.JoinResult> JoinAsync(UiFrame.JoinWorld request, string? activity, CancellationToken cancellation, byte kind = FreeRoamKind.FreeRoam,
        bool privateCreate = false)
    {
        if(Interlocked.CompareExchange(ref _joinAttempt,1,0)!=0)return new(UiJoin.SlowDown,_text("worlds.slow_down"));
        CancelFreeRoamLeave(); // the new seat is never let go by an older leave of free roam
        try {return await JoinCoreAsync(request,activity,kind,privateCreate,cancellation).ConfigureAwait(false);}
        finally {Volatile.Write(ref _joinAttempt,0);}
    }
    private async Task<UiFrame.JoinResult> JoinCoreAsync(UiFrame.JoinWorld request, string? activity, byte kind, bool privateCreate, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
        Volatile.Write(ref _lastJoinRefusal, null);
        Volatile.Write(ref _lastPoolUntilTicks, 0);
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        RequestReconcile();
        var joinEpoch=Volatile.Read(ref _mineEpoch);
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
            return new(UiJoin.NotAllowed, DeniedMessage(denied, "join"));
        if (GameReadiness.CurrentGeneration is not { } generation)
            return new(UiJoin.NotAllowed, _text("worlds.load_game"));
        var worldId = string.IsNullOrEmpty(request.WorldId) ? null : request.WorldId; // empty: a quick join
        // OD-112: the session's set goes with every join; planned mods not confirmed yet never join as vanilla.
        var (mods, modsRefusal) = WireMods();
        if (modsRefusal is not null) return new(UiJoin.NotAllowed, _text(modsRefusal));
        // OD-208: an official join is direct only, so neither nat nor relays is ever sent (WORLDS_OFFICIAL §3)
        try
        {
            for (var attempt = 0; attempt < 2; ++attempt)
            {
                if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
                    return new(UiJoin.NotAllowed, DeniedMessage(changed, "join"));
                if (!GameReadiness.Matches(generation)) return new(UiJoin.NotAllowed, _text("worlds.load_game"));
                var answer = await OperationAsync(_joinRetry, token => privateCreate && activity is not null
                        ? _mm.PrivateCreateAsync(activity, mods, token)
                        : _mm.JoinAsync(worldId, mods, activity, token), cancellation,
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

                if (answer.Value is not { } join)
                {
                    if (PoolRefusal(answer.Error) is { } poolRefusal)
                    {
                        Volatile.Write(ref _lastJoinRefusal, ConflictReasons.Pool); // OD-190: a quick join's pool answer starts the anti-cheat move
                        Volatile.Write(ref _lastPoolUntilTicks, PrivateLobbyNotices.PoolUntil(answer.Error)?.UtcTicks ?? 0); // A10 (T-L3)
                        return poolRefusal;
                    }
                    if (answer.Error is { Kind: MmErrorKind.Conflict, Reason: ConflictReasons.LobbiesFull } lobbiesFull)
                    {
                        // OD-190 (WORLDS_OFFICIAL §11.2): no official world of the activity has a free seat; the quick join waits Retry-After
                        Volatile.Write(ref _lobbiesFullAfter, (answer.RetryAfter ?? lobbiesFull.RetryAfter ?? TimeSpan.Zero).Ticks);
                        return new UiFrame.JoinResult(UiJoin.Full, _text("worlds.lobbies_full"));
                    }
                    if (answer.Error is { Kind: MmErrorKind.Conflict, Reason: ConflictReasons.Activity })
                    {
                        Volatile.Write(ref _lastJoinRefusal, ConflictReasons.Activity); // T-L3: a private create words it (PrivateLobbyNotices)
                        // OD-150: the world runs another activity now; like a closed world, the free-roam step tries the next one
                        _log($"worlds: join refused: {worldId ?? "the world"} does not run {activity ?? "the asked activity"} now");
                        return new UiFrame.JoinResult(UiJoin.Closed, _text("worlds.closed"));
                    }
                    if (answer.Error is { Kind: MmErrorKind.Conflict, Reason: ConflictReasons.ClientUpdate })
                    {
                        // OD-204 (MATCHMAKING_API §2.10): this session did not declare private_official_v1 (or the server did not take it)
                        Volatile.Write(ref _lastJoinRefusal, ConflictReasons.ClientUpdate);
                        _log("worlds: join refused: client_update (private lobbies need this session's features)");
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, _text("worlds.client_update"));
                    }
                    if (privateCreate && answer.Error is { Kind: MmErrorKind.Invalid, Field: "private" })
                    {
                        // T-L6: a server before CR-S96 refuses the unknown member
                        Volatile.Write(ref _lastJoinRefusal, "private_unsupported");
                        _log("worlds: private create refused: this server has no private lobbies yet");
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, _text("worlds.private_unavailable"));
                    }
                    if (ConflictReasons.RelayKey(answer.Error) is { } relayKey)
                    {
                        _log($"worlds: join refused: {answer.Error?.Reason}");
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, _text(relayKey));
                    }
                    if (answer.Error is { Kind: MmErrorKind.Conflict, Reason: PrivateLobbyNotices.NotLeader or PrivateLobbyNotices.Region } privateRefusal)
                    {
                        // OD-204 (T-L3): CR-S96's private lobby refusals, worded by the one table (T-L4/T-L5 reuse it)
                        Volatile.Write(ref _lastJoinRefusal, privateRefusal.Reason);
                        _log($"worlds: join refused: {privateRefusal.Reason}");
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, PrivateLobbyNotices.Text(_text, privateRefusal.Reason!));
                    }
                    // OD-181 (CR-S84): a modded session's join without its set is 400 {field: "mods"} (WireMods never sends one)
                    if (answer.Error is { Kind: MmErrorKind.Invalid, Field: "mods" })
                    {
                        _log("worlds: join refused: the server wants this session's mods");
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, _text("worlds.mods_failed"));
                    }
                    if (answer.Error?.Kind == MmErrorKind.WorldNotFound) Volatile.Write(ref _lastJoinRefusal, PrivateLobbyNotices.Closed); // T-L5's wording
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
                                SeatWorldIs(grant.WorldId); // OD-190: a new seat forgets the last one's host notices
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
                        return new UiFrame.JoinResult(UiJoin.Reserved, _text("worlds.reserved"));
                    case JoinStatuses.Locked when attempt == 0:
                        // A used seat in this lobby is also Locked: never destroy a live seat on that ambiguity.
                        if (join.Locked is not { } locked || worldId is null || locked.WorldId == worldId)
                        {
                            _udpDiagnostics.Managed(Diagnostics.UdpDiagnostics.ManagedPhase.JoinSeatPreserved);
                            return new(UiJoin.NotAllowed, _text("worlds.already_in_lobby"));
                        }
                        if (joinEpoch!=Volatile.Read(ref _mineEpoch))
                            return new(UiJoin.NotAllowed, _text("worlds.join_failed"));
                        var left = await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
                        Observe(left);
                        if (!left.Ok) return new(UiJoin.NotAllowed, _text("worlds.join_failed"));
                        if (!OnlineOpen) return new(UiJoin.NotAllowed, _text("worlds.action_failed"));
                        _seat.Left();
                        continue;
                    case JoinStatuses.Full:
                        return new UiFrame.JoinResult(UiJoin.Full, _text("worlds.full"));
                    case JoinStatuses.Closed:
                        Volatile.Write(ref _lastJoinRefusal, PrivateLobbyNotices.Closed); // T-L5's wording
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
        await RenewStagedAsync(cancellation).ConfigureAwait(false); // W-6: a staged seat is kept alive until it is installed
        var now = Now;
        var next = _seat.Next(now);
        if (next.Step == SeatStep.Stop && next.Why is SeatDecision.Lapsed or SeatDecision.HoldReached)
        {
            _seat.Left();
            InvalidateMine(reset: true);
            await ClearSeatEndAsync(next.Why == SeatDecision.Lapsed ? "the seat lapsed" : "the seat's hold ended").ConfigureAwait(false); // the hook's context and relay leg end with the seat
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
                // OD-191: the renewed seat's world (a grant taken over from it too)
                if (seat.Seat is { GrantId: { } renewedGrant } renewedWorld && _seat.GrantId == renewedGrant) SeatWorldIs(renewedWorld.WorldId);
                if (seat.Seat is { } renewedSeat) ObserveActivity(renewedSeat);
                ObservePool(seat.Pool);
                // A successful renew is authoritative too: an older in-flight GET cannot undo it.
                Interlocked.Increment(ref _mineEpoch);
                if (seatAt is not null) _lastSeatAt = seatAt;
                if (after.Step == SeatStep.Stop)
                {
                    InvalidateMine(reset: true);
                    _log(SeatEndedLine(seat.Ended, grantId)); // T35: with its reason
                    ObserveSeatEnded(seat.Ended, grantId);
                }
                // OD-149: isolated by the anti-cheat system: the seat in a clean world ended, the launcher moves the player
                if (after.Step == SeatStep.Stop && SeatEndedForPool(seat.Ended, grantId)) return _text("worlds.pool_moved");
                return after.Step == SeatStep.Stop && after.Why != SeatDecision.LeftSeat ? _text("worlds.seat_ended") : null;
            }
        }
        catch (Exception e) when (HostFailure(e))
        {
            _seat.RenewFailed(now);
            return null;
        }
    }


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
    public async Task<UiFrame.MyWorld> MyWorldAsync(CancellationToken cancellation) =>
        WithConnection(await MineAsync(background: false, cancellation).ConfigureAwait(false));

    /// <summary>The players of the world as last read (no request): the launcher's moderation panel lists them (CR-S62).</summary>
    public IReadOnlyList<MemberRow> LastMembers => Volatile.Read(ref _mineSnapshot).Members;

    /// <summary>"My world" as last read (no request): the launcher's Discord presence reads it (OD-136, CR-L41.1).</summary>
    public UiFrame.MyWorld LastMyWorld => Volatile.Read(ref _mineSnapshot);

    /// <summary>The <c>not_allowed</c> reason of the last join (e.g. <c>mode</c>), null when it was not one: a link join offers a profile of that mode (CR-L41.2).</summary>
    public string? LastJoinRefusal => Volatile.Read(ref _lastJoinRefusal);
    private string? _lastJoinRefusal;
    // A10 (T-L3): the last 409 pool's details.until, as UTC ticks (0: none said)
    private long _lastPoolUntilTicks;

    /// <summary>Periodic reconciliation sleeps after an idle free session has a confirmed empty seat.</summary>
    public async Task<UiFrame.MyWorld> PollMyWorldAsync(CancellationToken cancellation) =>
        WithConnection(await MineAsync(background: true, cancellation).ConfigureAwait(false));

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
            var settled = _mineReconciled && !_mineUncertain && _pendingRequestUntil is null && !_joinRetry.Running;
            if (background && idleWhenNoWorld && settled && _seat.GrantId is null && _mineSnapshot.Role == UiWorldRole.None)
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
            // W-6: a new seat in another world while the game plays here is staged: the hook's context and leg stay
            if (StageSeat(value)) return _mineSnapshot;
            DropStaged(value);
            if(!ClaimSeatProof(value,ref epoch) || !await SendSeatProofAsync(value.Seat,epoch,cancellation).ConfigureAwait(false))
            { _mineUncertain=true;return _mineSnapshot; }
            ObservePool(value.Pool);
            if (value.Seat is not { } seat)
            {
                lock (_mineState)
                {
                    if (!OnlineOpen || epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
                    ObserveRequest(value.Request);
                    _mineReconciled = true; _mineUncertain = false;
                    if (seatAt is not null) _lastSeatAt = seatAt;
                    var heldGrant = _seat.GrantId;
                    // T35: the joiner's seat ending on the server is said with its reason (released: the host's heartbeat no
                    // longer listed this player), so a cleared relay leg is never silent
                    if (heldGrant is not null) _log(SeatEndedLine(value.Ended, heldGrant));
                    ObserveSeatEnded(value.Ended, heldGrant); // T-L6: a released private seat is rejoined first
                    ObservePrivateRejoin(value.PrivateWorldId, value.Ended, value.GeneratedAt); // T-L4 (a): the server's private world to rejoin comes first
                    _seat.Observe(value, Now);
                    // OD-149: the seat this launcher held ended because the anti-cheat system isolated the player: it is moved
                    if (SeatEndedForPool(value.Ended, heldGrant)) _freeRoamToasts.Enqueue(PoolToast());
                    (_mineId, _mineRole) = (null, UiWorldRole.None);
                    _lastRosterAt = null;
                    _roster.Reset();
                    _seatActivity = null;
                    CrashContext?.ObserveWorld(null);
                    return _mineSnapshot = NoWorld;
                }
            }
            var role = seat.State switch
            {
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
                // §2.9: an official world's host is no player: no host row (its name is the world's host below)
            }
            // CR-S67/S68: the world's voice mode for a joiner, this player's own voice block
            if (role == UiWorldRole.Seated && epoch == Volatile.Read(ref _mineEpoch))
                ObserveVoice(seat.WorldId, role, world, named ? boundedRoster : null);
            lock (_mineState)
            {
                if (!OnlineOpen || epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
                _mineReconciled = true; _mineUncertain = false;
                if (seatAt is not null) _lastSeatAt = seatAt;
                ObserveRequest(value.Request);
                if (seat.State != SeatStates.Reserved || seat.GrantId is not null && seat.ExpiresAt is not null
                    && seat.HoldUntil is not null && _seat.GrantId != seat.GrantId) _seat.Observe(value, Now);
                // OD-191 (W-4 review): a grant taken over from the read (a group seat, an accepted request, a restart) names its world
                if (seat.GrantId is { } readGrant && _seat.GrantId == readGrant)
                {
                    ObserveGroupSeat(Volatile.Read(ref _seatWorld), seat); // OD-191: a car meet seat's move to its group world
                    SeatWorldIs(seat.WorldId);
                }
                if (_mineId != seat.WorldId) { _lastRosterAt = null; _roster.Reset(); }
                if (_mineId != seat.WorldId || _mineRole != role) Interlocked.Increment(ref _mineEpoch);
                (_mineId, _mineRole) = (seat.WorldId, role);
                if (named)
                {
                    _roster.Observe(seat.WorldId, boundedRoster!);
                    _lastRosterAt = DateTimeOffset.TryParse(boundedRoster!.GeneratedAt, System.Globalization.CultureInfo.InvariantCulture,
                        System.Globalization.DateTimeStyles.AssumeUniversal, out var time) ? time : null;
                }
                ObserveNoticeWorld(world); // OD-160's notice: the world's modes and transport
                ObserveJoinModes(world); // OD-210 (W14): the private lobby's "Who can join"
                ObserveActivity(seat);
                CrashContext?.ObserveWorld(role == UiWorldRole.Seated ? new(seat.WorldId, world.Private == true ? "private" : "everyone", Now, named,
                    members.Take(12).Select(m => new CrashParticipant(m.Xuid, m.Gamertag,
                        m.Role switch { 0 => "host", 1 => "seated", _ => "reserved" })).ToArray()) : null);
                // OD-181 (CR-L48): every player's declared mods, for the overlay's roster and the player details
                Volatile.Write(ref _rosterMods, RosterModsOf(seat.WorldId, named
                    ? boundedRoster!.Items.Select(i => (i.Xuid, i.Role == "host", i.Mods)) : [], world.Mods, ModName));
                // OD-204 (T-L6): only with the official flag (a private world is an official one; the frame refuses it alone)
                var privateMine = (world.Official == true || Volatile.Read(ref _seatOfficial) == seat.WorldId)
                    && (world.Private == true || SeatPrivate(seat.WorldId));
                return _mineSnapshot = new UiFrame.MyWorld(role, seat.WorldId, world.Name, world.Host.DisplayName, string.Empty, privateMine ? (byte)3 : (byte)0,
                    (byte)Math.Min(world.Players, 255u), (byte)Math.Min(world.MaxPlayers, 255u), named, members, WorldModes.OpenToBoth(world),
                    Official: world.Official == true || Volatile.Read(ref _seatOfficial) == seat.WorldId, // OD-192 (OD-208: the lobby code and audience bytes stay 0)
                    // OD-204 (T-L6): only with the official flag (a private world is an official one; the frame refuses it alone)
                    Private: privateMine,
                    // OD-204 (T-L4, CR-S96 c94d4039): the seat read's world.leader (private worlds, seated readers, omitted while nobody
                    // leads); empty (unknown) lets nobody invite. Only a decimal XUID rides the frame (the frame refuses anything else).
                    Leader: privateMine && world.Leader is { Length: > 0 and <= 20 } leader && leader.All(char.IsAsciiDigit) ? leader : string.Empty);
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

    /// <summary>The overlay's actions on its world (OD-63): leave, add a friend, report.</summary>
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
                case UiWorldAction.Leave:
                    CancelFreeRoamLeave();
                    InvalidateMine(reset: true);
                    // left on purpose: never the world the game's next "find me a lobby" goes back to
                    await ReleaseUnplayedSeatAsync().ConfigureAwait(false); // W-10 Q4: a search toward it is released first
                    LeftOnPurpose();
                    _seat.Left();
                    await ClearSeatEndAsync("the player left from the overlay").ConfigureAwait(false); // no relay leg outlives the seat
                    var left = await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
                    Observe(left);
                    _log(left.Ok ? "worlds: seat left from the overlay" : $"worlds: leave refused: {left.Error?.Kind}");
                    return left.Ok ? Done("worlds.left") : Refused("worlds.action_failed");
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
                    var (reportOk, reportKey) = await Social.ReportAsync(request.Xuid, reason, _mineId, cancellation).ConfigureAwait(false);
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

    private static bool HostFailure(Exception exception) => exception is HttpRequestException or OperationCanceledException or InvalidOperationException or IOException or SocketException or ArgumentException or TimeoutException;

    /// <summary>A seat that ended here (left, lapsed): the hook's socket context and relay leg are cleared; a failed write is not the seat's.</summary>
    private async Task ClearSeatEndAsync(string why)
    {
        try { await ClearProofAsync(why, CancellationToken.None).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error)) { }
    }

    /// <summary>The play session is ending: the seat is let go (idempotent on the server).</summary>
    public async Task LeaveAsync(CancellationToken cancellation)
    {
        CancelFreeRoamLeave();
        InvalidateMine(reset: true);
        await ClearProofAsync("the seat is let go (leave)", CancellationToken.None).ConfigureAwait(false);
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
}
