// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net.Sockets;
using Infinity.Core.Diagnostics;
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
    Justification = "The session's async-only semaphore never creates a native wait handle; pending reads retain it until cancellation drains them.")]
public sealed class WorldsBridge(IMatchmakingApi mm, Func<string, string> text, TimeProvider? clock = null,
    string mode = "noob", Func<uint, IHostRelay?>? startRelay = null, Func<IReadOnlyList<string>>? lanAddresses = null, string? ownXuid = null, Action<string>? log = null,
    string relayUnavailableKey = "worlds.integrated_unavailable", Func<IHostRelay, WorldAnnounced, CancellationToken, Task>? updateRelayWorld = null,
    Func<IHostRelay, CancellationToken, Task>? refreshRelayAdmissions = null, Session.GameReadiness? gameReadiness = null,
    Func<CancellationToken, Task<WorldAccessDecision>>? checkWorldAccess = null, Action<string>? worldAccessDenied = null)
{
    // the world's open and close in the launcher's log (ids only, never addresses)
    private readonly Action<string> _log = log ?? (_ => { });
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
    private DateTimeOffset _nextAdmissionsRefresh;
    private readonly IMatchmakingApi _mm = mm ?? throw new ArgumentNullException(nameof(mm));
    private readonly Func<string, string> _text = text ?? throw new ArgumentNullException(nameof(text));
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly SeatKeeper _seat = new();
    private readonly SemaphoreSlim _mineRead = new(1, 1);
    private readonly object _mineState = new();
    private readonly LobbyRoster _roster = new(ownXuid);
    private readonly PublicWorldWarning _publicWarning = new();
    private UiFrame.MyWorld _mineSnapshot = NoWorld;
    private long _mineNextTicks, _mineEpoch;

    public IReadOnlyList<UiFrame.Toast> LobbyToasts()
    {
        var toasts = new List<UiFrame.Toast>();
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
            if (reset)
            {
                _roster.Reset();
                _publicWarning.Reset();
                CrashContext?.ObserveWorld(null);
                Volatile.Write(ref _mineSnapshot, NoWorld);
                (_mineId, _mineRole) = (null, UiWorldRole.None);
            }
        }
    }

    public Session.GameReadiness GameReadiness { get; init; } = gameReadiness ?? new(clock);

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
        if (checkWorldAccess is null) return WorldAccessDecision.Compatible;
        try
        {
            var access = await checkWorldAccess(cancellation).ConfigureAwait(false);
            return access is not null && (access.Allowed || !string.IsNullOrEmpty(access.MessageKey))
                ? access : WorldAccessDecision.Inconclusive;
        }
        catch (Exception e) when (e is HttpRequestException or IOException or InvalidOperationException or TimeoutException
            || e is OperationCanceledException && !cancellation.IsCancellationRequested)
        {
            return WorldAccessDecision.Inconclusive;
        }
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
            var answer = await _mm.ListAsync(filter, query.NotFull, cancellation).ConfigureAwait(false);
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
                return new(query.Filter, 2, DeniedMessage(changed, "list"), []);
            if (!answer.Ok || answer.Value is not { } list)
            {
                var key = answer.Error?.Kind == MmErrorKind.SupporterRequired ? "worlds.friends_supporters" : "worlds.list_failed";
                return new UiFrame.WorldsList(query.Filter, 2, _text(key), []);
            }
            var rows = list.Items.Select(w => Row(w, false)).Concat(list.Locked.Select(w => Row(w, true))).ToList();
            return new UiFrame.WorldsList(query.Filter, 0, string.Empty, rows);
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
        {
            return new UiFrame.WorldsList(query.Filter, 2, _text("worlds.list_failed"), []);
        }
    }

    /// <summary>A join by id or code. One live seat per profile: a seat elsewhere is left first (the player chose this world).</summary>
    public async Task<UiFrame.JoinResult> JoinAsync(UiFrame.JoinWorld request, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
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
        try
        {
            for (var attempt = 0; attempt < 2; ++attempt)
            {
                if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
                    return new(UiJoin.NotAllowed, DeniedMessage(changed, "join"));
                if (!GameReadiness.Matches(generation)) return new(UiJoin.NotAllowed, _text("worlds.load_game"));
                var answer = await _mm.JoinAsync(worldId, code, cancellation).ConfigureAwait(false);
                WorldAccessDecision after;
                try { after = await AccessAsync(cancellation).ConfigureAwait(false); }
                catch (OperationCanceledException)
                {
                    if (answer.Value?.Grant is { } canceled) await _mm.LeaveGrantAsync(canceled.Id, CancellationToken.None).ConfigureAwait(false);
                    throw;
                }
                if (!after.Allowed)
                {
                    if (answer.Value?.Grant is { } stale) await _mm.LeaveGrantAsync(stale.Id, CancellationToken.None).ConfigureAwait(false);
                    return new(UiJoin.NotAllowed, DeniedMessage(after, "join"));
                }
                if (answer.Value is not { } join)
                {
                    return new UiFrame.JoinResult(UiJoin.Closed, _text(answer.Error?.Kind == MmErrorKind.WorldNotFound ? "worlds.closed" : "worlds.join_failed"));
                }
                switch (join.Status)
                {
                    case JoinStatuses.Granted when join.Grant is { } grant:
                        if (worldId is not null && grant.WorldId != worldId)
                        {
                            await _mm.LeaveGrantAsync(grant.Id, CancellationToken.None).ConfigureAwait(false);
                            return new(UiJoin.NotAllowed, DeniedMessage(WorldAccessDecision.Inconclusive, "join"));
                        }
                        if (!GameReadiness.Matches(generation))
                        {
                            await _mm.LeaveGrantAsync(grant.Id, CancellationToken.None).ConfigureAwait(false);
                            return new(UiJoin.NotAllowed, _text("worlds.load_game"));
                        }
                        _seat.Granted(grant, _clock.GetUtcNow());
                        InvalidateMine(reset: true);
                        return new UiFrame.JoinResult(UiJoin.Reserved, _text("worlds.reserved"));
                    case JoinStatuses.Locked when attempt == 0:
                        // the seat of another world: leave it, then ask again once
                        await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
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
                    case JoinStatuses.NotAllowed:
                        return new UiFrame.JoinResult(UiJoin.NotAllowed, _text("worlds.not_allowed_" + (join.Reason ?? "host")));
                }
                break;
            }
            return new UiFrame.JoinResult(UiJoin.NotAllowed, _text("worlds.join_failed"));
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
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
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
        {
            var hadSeat = _seat.GrantId is not null;
            await LeaveAsync(cancellation).ConfigureAwait(false);
            return hadSeat ? _text(denied.MessageKey) : null;
        }
        var now = _clock.GetUtcNow();
        var next = _seat.Next(now);
        if (next.Step == SeatStep.Stop && next.Why is SeatDecision.Lapsed or SeatDecision.HoldReached)
        {
            _seat.Left();
            InvalidateMine(reset: true);
            return _text("worlds.seat_ended");
        }
        if (next.Step != SeatStep.Renew || next.GrantId is not { } grantId)
        {
            return null;
        }
        try
        {
            var renewed = await _mm.RenewSeatAsync(grantId, cancellation).ConfigureAwait(false);
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
            {
                await LeaveAsync(CancellationToken.None).ConfigureAwait(false);
                return _text(changed.MessageKey);
            }
            if (!renewed.Ok || renewed.Value is not { } seat)
            {
                _seat.RenewFailed(now, renewed.RetryAfter);
                return null;
            }
            var after = _seat.Observe(seat, now);
            if (after.Step == SeatStep.Stop) InvalidateMine(reset: true);
            return after.Step == SeatStep.Stop && after.Why != SeatDecision.LeftSeat ? _text("worlds.seat_ended") : null;
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
        {
            _seat.RenewFailed(now);
            return null;
        }
    }

    /// <summary>The world this launcher hosts, when open.</summary>
    public string? HostedWorldId => Volatile.Read(ref _hostedWorldId);

    /// <summary>Friend requests and reports from the Worlds tab (OD-63); null: refused with <c>worlds.action_failed</c>.</summary>
    public IWorldSocial? Social { get; init; }

    private static readonly UiFrame.MyWorld NoWorld = new(UiWorldRole.None, string.Empty, string.Empty, string.Empty, string.Empty, 0, 0, 0, false, []);

    // the world the overlay last showed, for the actions on it
    private string? _mineId;
    private byte _mineRole;

    /// <summary>
    /// The overlay's "my world" (OD-63): the world the profile hosts or holds a seat in
    /// (<c>GET /mm/v1/seat</c>), its code for the host, and its players (<c>GET …/members</c>;
    /// only the host when the server gives no roster).
    /// </summary>
    public async Task<UiFrame.MyWorld> MyWorldAsync(CancellationToken cancellation)
    {
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false })
        {
            InvalidateMine(reset: true);
            return NoWorld;
        }
        await _mineRead.WaitAsync(cancellation).ConfigureAwait(false);
        var epoch = Volatile.Read(ref _mineEpoch);
        try
        {
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false })
            {
                InvalidateMine(reset: true);
                return NoWorld;
            }
            if (_clock.GetUtcNow().UtcTicks < Volatile.Read(ref _mineNextTicks)) return _mineSnapshot;
            Interlocked.Exchange(ref _mineNextTicks, (_clock.GetUtcNow() + TimeSpan.FromSeconds(5)).UtcTicks);
            var answer = await _mm.SeatAsync(cancellation).ConfigureAwait(false);
            if (epoch != Volatile.Read(ref _mineEpoch) || !answer.Ok || answer.Value is null) return _mineSnapshot;
            if (answer.Value?.Seat is not { } seat)
            {
                lock (_mineState)
                {
                    if (epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
                    (_mineId, _mineRole) = (null, UiWorldRole.None);
                    _roster.Reset();
                    _publicWarning.Reset();
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
            var members = new List<MemberRow>();
            var roster = await _mm.MembersAsync(seat.WorldId, cancellation).ConfigureAwait(false);
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false })
            {
                InvalidateMine(reset: true);
                return NoWorld;
            }
            if (epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
            var named = roster is { Ok: true, Value: not null } && (roster.Value.WorldId is null || roster.Value.WorldId == seat.WorldId);
            if (named)
            {
                members.AddRange(roster.Value!.Items.Select(Member));
            }
            else
            {
                members.Add(new MemberRow(world.Host.Xuid, world.Host.Gamertag, world.Host.Tier == "vip" ? UiTier.Supporter : UiTier.Free, 0,
                    role == UiWorldRole.Hosting ? UiFriend.Self : UiFriend.None));
            }
            var row = Row(world, false);
            lock (_mineState)
            {
                if (epoch != Volatile.Read(ref _mineEpoch)) return _mineSnapshot;
                (_mineId, _mineRole) = (seat.WorldId, role);
                if (named) _roster.Observe(seat.WorldId, roster.Value!);
                if (role is UiWorldRole.Hosting or UiWorldRole.Seated) _publicWarning.Observe(seat.WorldId, world.Audience);
                CrashContext?.ObserveWorld(role is UiWorldRole.Hosting or UiWorldRole.Seated ? new(seat.WorldId, world.Audience, _clock.GetUtcNow(), named,
                    members.Take(12).Select(m => new CrashParticipant(m.Xuid, m.Gamertag,
                        m.Role switch { 0 => "host", 1 => "seated", _ => "reserved" })).ToArray()) : null);
                return _mineSnapshot = new UiFrame.MyWorld(role, seat.WorldId, world.Name, world.Host.Gamertag, world.Code ?? string.Empty, row.Audience, row.Players, row.Max, named, members);
            }
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
        {
            return _mineSnapshot;
        }
        finally { _mineRead.Release(); }
    }

    /// <summary>One player of the roster as the overlay draws it (the XUID is the action key only).</summary>
    public static MemberRow Member(WorldMember member)
    {
        ArgumentNullException.ThrowIfNull(member);
        return new MemberRow(member.Xuid, member.Gamertag, member.Tier == "vip" ? UiTier.Supporter : UiTier.Free,
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
                    InvalidateMine(reset: true);
                    _seat.Left();
                    var left = await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
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
                    var kicked = await _mm.ModerationAsync(id, new Moderation { Action = ModerationActions.Kick, Xuid = request.Xuid }, cancellation).ConfigureAwait(false);
                    if (kicked.Ok && kickedGeneration == Volatile.Read(ref _hostGeneration) && HostedWorldId == id)
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
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
        {
            return Refused("worlds.action_failed");
        }
    }

    /// <summary>
    /// The overlay's "Create world": the relay started, the world opened on matchmaking (PUT with
    /// the relay's port and this PC's LAN addresses), then announced every 15 s by <see cref="HostTickAsync"/>.
    /// The host then picks FREE ROAM ONLINE and the server lands the game on its own relay (§4a).
    /// </summary>
    public async Task<UiFrame.Hosted> CreateAsync(UiFrame.CreateWorld request, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
            return new(1, string.Empty, string.Empty, DeniedMessage(denied, "create"));
        if (GameReadiness.CurrentGeneration is not { } generation)
            return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
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
            if (!GameReadiness.Matches(generation)) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
            await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
            if (!IsCurrent(operation) || stop.IsCancellationRequested) return HostingFailed();
            if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } beforeRelay)
                return new(1, string.Empty, string.Empty, DeniedMessage(beforeRelay, "create"));
            var players = (uint)Math.Clamp((int)request.MaxPlayers, 2, 12);
            _relay = _startRelay?.Invoke(players);
            if (_relay is null) return new UiFrame.Hosted(2, string.Empty, string.Empty, _text(relayUnavailableKey));
            if (_relay is IntegratedWorldRelay integrated) integrated.CrashContext = CrashContext;
            var relay = _relay;
            var settings = new WorldSettings
            {
                Name = string.IsNullOrWhiteSpace(request.Name) ? "Infinity" : request.Name.Trim()[..Math.Min(48, request.Name.Trim().Length)],
                Mode = mode,
                MaxPlayers = players,
                Audience = request.Audience switch { 1 => WorldAudiences.Supporters, 2 => WorldAudiences.Friends, 3 => WorldAudiences.Private, _ => WorldAudiences.Everyone },
                MaxRttMs = request.Latency switch { 1 => 150u, 2 => 100u, 3 => 60u, _ => null },
                Races = true,
                Endpoint = new AnnouncePort { Port = relay.Port, Lan = _lan() is { Count: > 0 } lan ? lan : null },
            };
            for (var attempt = 0; attempt < 2; ++attempt)
            {
                stop.Token.ThrowIfCancellationRequested();
                if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } beforePut)
                    return new(1, string.Empty, string.Empty, DeniedMessage(beforePut, "create"));
                if (!GameReadiness.Matches(generation)) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                openingId = WorldHostScheduler.NewWorldId();
                var pending = _mm.PutWorldAsync(openingId, settings, stop.Token);
                _ = CloseStalePutAsync(pending, openingId, operation, stop.Token);
                var opened = await pending.WaitAsync(stop.Token).ConfigureAwait(false);
                stop.Token.ThrowIfCancellationRequested();
                if (!IsCurrent(operation)) throw new OperationCanceledException(stop.Token);
                if (opened is { Ok: true, Value: { } world })
                {
                    if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterPut)
                        return new(1, string.Empty, string.Empty, DeniedMessage(afterPut, "create"));
                    if (!GameReadiness.Matches(generation)) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                    if (world.Id != openingId)
                        return new(1, string.Empty, string.Empty, DeniedMessage(WorldAccessDecision.Inconclusive, "create"));
                    if (!GameReadiness.Matches(generation)) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                    if (updateRelayWorld is not null)
                        await updateRelayWorld(relay, world, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false);
                    stop.Token.ThrowIfCancellationRequested();
                    if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterPolicy)
                        return new(1, string.Empty, string.Empty, DeniedMessage(afterPolicy, "create"));
                    if (!GameReadiness.Matches(generation)) return new(1, string.Empty, string.Empty, _text("worlds.load_game"));
                    lock (_hostState)
                    {
                        if (_hostGeneration != operation.Generation) throw new OperationCanceledException(stop.Token);
                        _host.Hosting(openingId, _clock.GetUtcNow());
                        _host.Opened(_clock.GetUtcNow());
                        _nextAdmissionsRefresh = _clock.GetUtcNow() + TimeSpan.FromSeconds(3);
                        Volatile.Write(ref _hostedWorldId, openingId);
                        _publicWarning.Observe(openingId, world.Audience);
                    }
                    hosted = true;
                    _log($"worlds: world {openingId} open ({settings.Mode}, {settings.Audience}, {players} players)");
                    return new UiFrame.Hosted(0, openingId, world.Code ?? string.Empty, string.Empty);
                }
                openingId = null; // an explicit refusal did not open this id
                if (opened.Error is { Kind: MmErrorKind.AlreadyHosting, WorldId: { } old } && attempt == 0)
                {
                    await _mm.CloseAsync(old, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false);
                    continue;
                }
                await StopRelayAsync().ConfigureAwait(false);
                _log($"worlds: open refused: {opened.Error?.Kind}");
                var key = opened.Error?.Kind switch
                {
                    MmErrorKind.SupporterRequired => "worlds.host_supporters",
                    MmErrorKind.ModeMismatch => "worlds.host_mode",
                    MmErrorKind.RateLimited => "worlds.slow_down",
                    _ => "worlds.host_failed",
                };
                return new UiFrame.Hosted(1, string.Empty, string.Empty, _text(key));
            }
        }
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
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
        {
            var hadHost = HostedWorldId is not null;
            await CloseHostAsync(cancellation).ConfigureAwait(false);
            return hadHost ? _text(denied.MessageKey) : null;
        }
        var operation = QueueHostOperation(replace: false, create: false);
        await operation.Previous.ConfigureAwait(false);
        using var stop = CancellationTokenSource.CreateLinkedTokenSource(operation.Stop, cancellation);
        try
        {
            if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } beforeHost)
            {
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text(beforeHost.MessageKey);
            }
            if (!IsCurrent(operation) || !_host.IsOpen || _host.WorldId is not { } id || _relay is not { } relay) return null;
            if (!relay.Running)
            {
                _log("worlds: the relay ended on its own");
                await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                return _text("worlds.host_ended");
            }
            var now = _clock.GetUtcNow();
            if (_host.Next(now).Step != HostStep.Announce)
            {
                if (refreshRelayAdmissions is null || now < _nextAdmissionsRefresh) return null;
                _nextAdmissionsRefresh = now + TimeSpan.FromSeconds(3);
                try
                {
                    await refreshRelayAdmissions(relay, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false);
                    stop.Token.ThrowIfCancellationRequested();
                    if (await AccessAsync(stop.Token).ConfigureAwait(false) is { Allowed: false } afterRefresh)
                    {
                        await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                        return _text(afterRefresh.MessageKey);
                    }
                    if (IsCurrent(operation)) _nextAdmissionsRefresh = _clock.GetUtcNow() + TimeSpan.FromSeconds(3);
                }
                catch (InvalidOperationException)
                {
                    await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
                    return _text("worlds.host_ended");
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
            var beat = await _mm.AnnounceAsync(id, new Heartbeat { Peers = peers }, stop.Token).WaitAsync(stop.Token).ConfigureAwait(false);
            stop.Token.ThrowIfCancellationRequested();
            if (!IsCurrent(operation)) return null;
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
                _host.Announced(_clock.GetUtcNow());
                _nextAdmissionsRefresh = _clock.GetUtcNow() + TimeSpan.FromSeconds(3);
                foreach (var punch in world.Punches.Where(p => p.Kicked))
                {
                    relay.Kick(punch.Xuid);
                }
                return null;
            }
            if (beat.Error is { } error && _host.Failed(error, _clock.GetUtcNow()).Outcome is HostOutcome.WorldGone or HostOutcome.Refused)
            {
                await CloseCurrentHostAsync(id, CancellationToken.None).ConfigureAwait(false);
                _log($"worlds: world {id} ended by the server: {error.Kind}");
                return _text("worlds.host_ended");
            }
        }
        catch (OperationCanceledException)
        {
            await CloseCurrentHostAsync(CancellationToken.None).ConfigureAwait(false);
        }
        catch (Exception e) when (HostFailure(e)) { if (IsCurrent(operation)) _host.Transient(_clock.GetUtcNow()); }
        finally { operation.Completion.TrySetResult(); }
        return null;
    }

    /// <summary>The host left free roam, closed the world, or the session ended: <c>DELETE</c> and the relay stopped.</summary>
    public async Task CloseHostAsync(CancellationToken cancellation)
    {
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
        var hadRelay = _relay is not null;
        await StopRelayAsync().ConfigureAwait(false);
        if (hadRelay)
        {
            _log("worlds: relay stopped");
        }
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
            _log(closed.Ok ? $"worlds: world {id} closed (DELETE ok)" : $"worlds: world {id} DELETE refused: {closed.Error?.Kind}");
        }
        catch (Exception e) when (HostFailure(e))
        {
            // it lapses 45 s after the last announce
            _log($"worlds: world {id} DELETE failed ({e.GetType().Name}); it lapses 45 s after the last announce");
        }
    }

    private async Task StopRelayAsync()
    {
        var relay = _relay;
        _relay = null;
        if (relay is IAsyncDisposable asynchronous) await asynchronous.DisposeAsync().ConfigureAwait(false);
        else if (relay is not null) await Task.Run(relay.Dispose).ConfigureAwait(false);
    }

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
            previousStop.Cancel();
            previousStop.Dispose();
        }
        return operation;
    }

    private bool IsCurrent(HostOperation operation) => Volatile.Read(ref _hostGeneration) == operation.Generation;
    private UiFrame.Hosted HostingFailed() => new(1, string.Empty, string.Empty, _text("worlds.host_failed"));
    private static bool HostFailure(Exception exception) => exception is HttpRequestException or OperationCanceledException or InvalidOperationException or IOException or SocketException or ArgumentException;

    // A canceled HTTP wait can still succeed on a transport that ignored cancellation. Observe it
    // until completion and delete only that operation's minted id, never a replacement world's id.
    private async Task CloseStalePutAsync(Task<MmResult<WorldAnnounced>> pending, string id, HostOperation operation, CancellationToken cancellation)
    {
        try
        {
            var answer = await pending.ConfigureAwait(false);
            if (answer.Ok && (!IsCurrent(operation) || cancellation.IsCancellationRequested)) await CloseWorldAsync(id, CancellationToken.None).ConfigureAwait(false);
        }
        catch (Exception e) when (HostFailure(e)) { }
    }

    /// <summary>The play session is ending: the seat is let go (idempotent on the server).</summary>
    public async Task LeaveAsync(CancellationToken cancellation)
    {
        InvalidateMine(reset: true);
        if (_seat.GrantId is null)
        {
            return;
        }
        _seat.Left();
        try
        {
            await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
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
            "tier" => 1,
            "friends" => 2,
            "code" => 3,
            "full" => 4,
            "latency" => 5,
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
