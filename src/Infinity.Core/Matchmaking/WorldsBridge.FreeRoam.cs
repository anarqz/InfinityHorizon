// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.Text;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>The control pipe's <c>FreeRoamReady.result</c> (CR-L40, LOBBIES_AND_SERVER_STATUS.md §3.1).</summary>
public static class FreeRoamResult
{
    public const byte Ready = 0;
    public const byte None = 1;
    public const byte NotOnline = 2;
}

/// <summary>CR-L40 §3.1: what the game's lobby search is for (FreeRoamNeed's kind), so the world must run that lobby.</summary>
public static class FreeRoamKind
{
    public const byte FreeRoam = 0;
    public const byte CarMeet = 1;
    public const byte RoadTrip = 2;
}

// CR-L40 §3.2, OD-190 (WORLDS_OFFICIAL §11.3): before the game's FreeRoam, car meet or Road Trip hopper request the hook asks;
// a world of its own, or a seat in a world whose host runs that activity now, stays; otherwise the launcher quick joins an
// official world of the activity (the overlay's own join, with its access, game-readiness and mods checks). It never opens a
// world for a ticket: a player world exists only when the player creates one.
public sealed partial class WorldsBridge
{
    /// <summary>How many pineapple lobbies an isolated player's ask tries from the list (OD-149; full or closed answers move on).</summary>
    public const int FreeRoamJoinAttempts = 3;

    /// <summary>The players of the pineapple lobby an anti-cheat move opens (OD-149; since OD-190 no game's ask opens one).</summary>
    public const byte FreeRoamPlayers = 8;

    /// <summary>OD-190 (§11.3 item 3): how long a quick join answered <c>lobbies_full</c> keeps retrying before it says so.</summary>
    public static readonly TimeSpan LobbiesFullBudget = TimeSpan.FromSeconds(10);

    /// <summary>The longest one ask may take, reads included: inside the hook's 18 s (PlaySupervisor.FreeRoamBudget), so its notice shows.</summary>
    public static readonly TimeSpan FreeRoamAskBudget = TimeSpan.FromSeconds(15);

    // OD-190: the Retry-After of the last quick join answered lobbies_full, as ticks; -1: the last join was not one
    private long _lobbiesFullAfter = -1;
    // OD-190: the seat's world when it is an official one (a quick join's, or a seat read or world read said so)
    private string? _seatOfficial;
    // OD-190: the seat's world whose host changed activity while this guest held it, and what it ran before (a later other ask
    // "couldn't follow"); cleared when the seat changes, the host comes back to that activity, or the guest is with its host again
    private HostMoved? _hostMoved;
    private sealed record HostMoved(string World, string From);
    // OD-190: the held seat ended because its world closed (the host quit or closed it): the next placement says so
    private RecentWorld? _hostLeft;

    /// <summary>The longest lobby name the server keeps (the Create screen and the dedicated lobby cut there too).</summary>
    public const int MaxLobbyName = 48;

    /// <summary>The session profile's gamertag, for the name of a lobby opened for it (CR-L40 §4); null: "Infinity".</summary>
    public string? Gamertag { get; init; }

    private readonly ConcurrentQueue<UiFrame.Toast> _freeRoamToasts = new();

    /// <summary>How long a world the game just left stays its first choice for the next world it asks for.</summary>
    public static readonly TimeSpan RejoinWindow = TimeSpan.FromSeconds(60);
    private sealed record RecentWorld(string WorldId, DateTimeOffset LeftAt);
    private string? _seatWorld;
    private RecentWorld? _recentlyLeft;
    private RecentWorld? _leftOnPurpose;

    /// <summary>The player left its world itself (the overlay's Leave): no rejoin, and that world is not picked for a while.</summary>
    private void LeftOnPurpose()
    {
        Volatile.Write(ref _recentlyLeft, null);
        Volatile.Write(ref _hostLeft, null);
        if (Interlocked.Exchange(ref _seatWorld, null) is { } world) Volatile.Write(ref _leftOnPurpose, new RecentWorld(world, Now));
    }
    private readonly object _freeRoamState = new();
    private Task<byte>? _freeRoamRun;
    private byte _freeRoamRunKind;

    /// <summary>
    /// How long the world outlives the game leaving free roam (the hook's Event 4): a car meet or an online Road Trip
    /// leaves its free roam session first and asks for its lobby (<c>FreeRoamNeed</c>) seconds later, in the same world.
    /// </summary>
    public static readonly TimeSpan FreeRoamLeftGrace = TimeSpan.FromSeconds(30);
    private CancellationTokenSource? _freeRoamLeave;

    // when a lobby ask was last answered with the world kept (Ready), as Now's UTC ticks; 0: none since the last leave
    private long _lobbyReadyTicks;

    /// <summary>
    /// OD-148 (refines OD-57): the game left free roam. After <see cref="FreeRoamLeftGrace"/> with no lobby ask the hosted
    /// world closes (<see cref="CloseHostAsync"/>) or the seat is let go (<see cref="LeaveAsync"/>); a lobby ask, the game
    /// joining a world again (<see cref="GameJoined"/>), the overlay's own close, leave, join or open, the session's end or
    /// another leave of free roam ends this wait first. A lobby ask answered Ready within the grace BEFORE this leave counts
    /// too (live, test.9: back from a Road Trip the game asks for free roam, opens it, and only then closes its Road Trip
    /// connection): no wait starts. Only the close or leave waits: the world's announces and the seat's renewals go on
    /// meanwhile. OD-180.2: a hosted world that still has other players (<see cref="OtherPlayersAsync"/>) is not closed: with
    /// any at the leave it is held for them at once, else after the grace; it is weighed again every grace, the relay, the
    /// announces and the admissions go on, and it closes (DELETE) once the last of them is gone. The host's game joining
    /// again (<see cref="GameJoined"/>) or asking for its lobby keeps the same world; the session's end still closes it.
    /// </summary>
    public async Task FreeRoamLeftAsync(CancellationToken cancellation)
    {
        var asked = Interlocked.Exchange(ref _lobbyReadyTicks, 0); // one ask covers one leave
        if (asked != 0 && Now - new DateTimeOffset(asked, TimeSpan.Zero) is var ago && ago >= TimeSpan.Zero && ago <= FreeRoamLeftGrace)
        {
            CancelFreeRoamLeave();
            if ((HostedWorldId ?? Volatile.Read(ref _seatWorld)) is { } kept)
                _log($"worlds: free roam left; {kept} stays: the game asked for its next lobby {(int)ago.TotalSeconds} s ago");
            return;
        }
        var pending = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _onlineStop.Token);
        CancelFreeRoamLeave(Interlocked.Exchange(ref _freeRoamLeave, pending)); // a new leave starts the wait again
        var world = HostedWorldId ?? Volatile.Read(ref _seatWorld);
        var due = false;
        var held = false;
        long heldSince = 0, heldHours = 0;
        try
        {
            // OD-180.2: a host whose game leaves while other players are in its world keeps it for them from this moment on
            if (HostedWorldId is { } stays && await OtherPlayersAsync(pending.Token).ConfigureAwait(false) is var others and > 0)
            {
                (held, heldSince) = (true, _clock.GetTimestamp());
                _log($"worlds: the host's game left {stays}; it stays open while {others} other player(s) remain");
            }
            else if (world is not null) _log($"worlds: free roam left; {world} is kept {(int)FreeRoamLeftGrace.TotalSeconds} s for the game's next lobby");
            while (true)
            {
                await Task.Delay(FreeRoamLeftGrace, _clock, pending.Token).ConfigureAwait(false);
                if (Volatile.Read(ref _freeRoamLeave) != pending) break; // called off meanwhile
                // a hosted world that still has other players (free roam, Road Trip or car meet) stays for them (live, test.8 and
                // T28: the host's game left its session after a race while the other player drove on); it is weighed again later
                if (HostedWorldId is { } kept && await OtherPlayersAsync(pending.Token).ConfigureAwait(false) is var players and > 0)
                {
                    // CR-L49: past the grace the host is away: its heartbeat says so until its game is back in the world
                    SetHostAway(true, $"its game left free roam {(int)FreeRoamLeftGrace.TotalSeconds} s ago");
                    if (!held)
                    {
                        (held, heldSince) = (true, _clock.GetTimestamp());
                        _log($"worlds: free roam left; {kept} stays: {players} other player(s) remain");
                    }
                    // bounded in the log, not in time: a world kept with no host game says so once an hour
                    else if ((long)(_clock.GetElapsedTime(heldSince) / HeldWorldLogEvery) is var hours && hours > heldHours)
                    {
                        heldHours = hours;
                        _log($"worlds: {kept} kept {hours} h with no host game; {players} other player(s) remain");
                    }
                    continue;
                }
                due = Interlocked.CompareExchange(ref _freeRoamLeave, null, pending) == pending; // nothing called it off meanwhile
                break;
            }
        }
        catch (OperationCanceledException) { }
        finally
        {
            Interlocked.CompareExchange(ref _freeRoamLeave, null, pending);
            pending.Dispose();
        }
        if (!due) return;
        var hosted = HostedWorldId;
        if ((hosted ?? Volatile.Read(ref _seatWorld)) is { } gone)
            _log(held && hosted is not null ? $"worlds: free roam left; closing {gone}: no other player left"
                : $"worlds: free roam left {(int)FreeRoamLeftGrace.TotalSeconds} s ago with no lobby asked; {(hosted is null ? "leaving" : "closing")} {gone}");
        await (hosted is not null ? CloseHostAsync(cancellation) : LeaveAsync(cancellation)).ConfigureAwait(false);
    }

    /// <summary>How often a hosted world kept for its other players with no host game says so in the log (OD-180.2).</summary>
    public static readonly TimeSpan HeldWorldLogEvery = TimeSpan.FromHours(1);

    /// <summary>
    /// OD-180.2: the players of the hosted relay other than the host's own game: those synced in any of its worlds (free roam,
    /// Road Trip, car meets; <see cref="IHostRelay.PeersAsync"/> without the host's XUID), at least its Road Trip and car meet
    /// players (<see cref="IHostRelay.LobbyPlayers"/>); 0 without a running relay or when it cannot be read.
    /// </summary>
    private async Task<int> OtherPlayersAsync(CancellationToken cancellation)
    {
        IHostRelay? relay;
        lock (_hostState) relay = _relay;
        if (relay is not { Running: true }) return 0;
        var others = 0;
        try { others = (await relay.PeersAsync(cancellation).ConfigureAwait(false)).Count(x => x != _ownXuid); }
        catch (Exception error) when (error is ObjectDisposedException or InvalidOperationException) { } // the relay just stopped
        // CR-L49: and the reserved or seated places the server counted on the last PUT or heartbeat (a guest on its way in)
        return Math.Max(Math.Max(others, relay.LobbyPlayers), Volatile.Read(ref _hostedOtherSeats));
    }

    /// <summary>
    /// The game joined a world again (the hook's Event 3, <c>host:&lt;port&gt;</c> or <c>join</c>): a pending close or leave
    /// after free roam is called off, the game is in the world (live, test.9: a joiner's Road Trip connection ended just
    /// before its free roam one opened).
    /// </summary>
    public void GameJoined(bool join = false)
    {
        if (join) SeatPlays("the game joined"); // a guest's join (detail join:…) is the seat's proof: no give-up
        else SetHostAway(false, "its game joined it again"); // CR-L49: the host's game is back in its world
        if (Interlocked.Exchange(ref _freeRoamLeave, null) is not { } pending) return;
        CancelFreeRoamLeave(pending);
        if ((HostedWorldId ?? Volatile.Read(ref _seatWorld)) is { } world) _log($"worlds: the game joined again; {world} stays");
    }

    /// <summary>A pending close or leave after free roam is called off (the world, or the player's own choice, stays).</summary>
    private void CancelFreeRoamLeave() => CancelFreeRoamLeave(Interlocked.Exchange(ref _freeRoamLeave, null));

    private static void CancelFreeRoamLeave(CancellationTokenSource? pending)
    {
        try { pending?.Cancel(); }
        catch (ObjectDisposedException) { } // its wait already ended
        catch (AggregateException) { }
    }

    private static string KindName(byte kind) => kind switch
    {
        FreeRoamKind.CarMeet => "car meet",
        FreeRoamKind.RoadTrip => "road trip",
        _ => "free roam",
    };

    // the world a FREE ROAM ONLINE ask stays in, for the log
    private string Staying(string? why = null) => HostsOrOpensWorld || Volatile.Read(ref _mineSnapshot).Role == UiWorldRole.Hosting
        ? $"stay in {HostedWorldId ?? "its own world"} (hosted)"
        : $"stay in {Volatile.Read(ref _seatWorld) ?? "its seat"} (seated{SeatOrigin()}{(why is null ? string.Empty : ", " + why)})";

    private byte Decided(byte kind, byte result, string what)
    {
        _log($"worlds: {KindName(kind)}: {what}");
        if (result == FreeRoamResult.Ready)
        {
            Interlocked.Exchange(ref _lobbyReadyTicks, Now.UtcTicks); // the next leave of free roam is this lobby's
            ArmGiveUp(kind); // a seat that never played gets SeatGiveUp to start playing; hosting never
        }
        return result;
    }

    /// <summary>
    /// FREE ROAM ONLINE is about to ask for its world (the hook's <c>FreeRoamNeed</c>, CR-L40 §3.2): a hosted world, or a seat
    /// whose world runs free roam now, answers at once; else an official free roam world is quick joined (OD-190).
    /// Concurrent asks share one run.
    /// </summary>
    public Task<byte> EnsureFreeRoamWorldAsync(CancellationToken cancellation) => EnsureFreeRoamWorldAsync(FreeRoamKind.FreeRoam, cancellation);

    /// <summary>
    /// As <see cref="EnsureFreeRoamWorldAsync(CancellationToken)"/> for a car meet or an online Road Trip (<see cref="FreeRoamKind"/>).
    /// OD-190 (WORLDS_OFFICIAL §11.3): a world of its own stays (it changes activity with its host, OD-150); a seat stays while its
    /// world runs the asked activity now (a player world: its host's activity, or the car meet lobby; an official world: its one
    /// activity). Otherwise (the player started another activity, the host quit or closed, the guest could not follow the host, or
    /// no world at all) <c>POST /mm/v1/join {activity}</c> places it in an official world, the server ending the old seat, and the
    /// game's ticket lands there through the seat. <c>lobbies_full</c> is retried for <see cref="LobbiesFullBudget"/>. No world is
    /// ever opened for an ask. Asks of one kind share a run; another kind waits for it.
    /// </summary>
    public Task<byte> EnsureFreeRoamWorldAsync(byte kind, CancellationToken cancellation)
    {
        CancelFreeRoamLeave(); // the game left free roam for this ask: its world stays and is weighed here
        if (!OnlineOpen) return Task.FromResult(Decided(kind, FreeRoamResult.NotOnline, "no world (online is closed)"));
        if (StaysAtOnce(kind)) return Task.FromResult(Decided(kind, FreeRoamResult.Ready, Staying()));
        lock (_freeRoamState)
        {
            if (_freeRoamRun is { IsCompleted: false } running)
            {
                if (_freeRoamRunKind == kind) return running.WaitAsync(cancellation);
                _freeRoamRunKind = kind;
                return _freeRoamRun = AfterAsync(running, kind, cancellation);
            }
            _freeRoamRunKind = kind;
            return _freeRoamRun = FreeRoamCoreAsync(kind, cancellation);
        }

        async Task<byte> AfterAsync(Task<byte> previous, byte next, CancellationToken token)
        {
            try { await previous.WaitAsync(token).ConfigureAwait(false); }
            catch (Exception error) when (error is not OperationCanceledException || !token.IsCancellationRequested) { }
            return await FreeRoamCoreAsync(next, token).ConfigureAwait(false);
        }
    }

    private static string? LobbyOf(byte kind) => kind switch
    {
        FreeRoamKind.CarMeet => LobbyKinds.CarMeet,
        FreeRoamKind.RoadTrip => LobbyKinds.RoadTrip,
        _ => null,
    };

    // a seat (reserved or seated) or a world of its own, as this bridge last knew them; no request
    private bool HasFreeRoamWorld => _seat.GrantId is not null || HostsOrOpensWorld || Volatile.Read(ref _mineSnapshot).Role != UiWorldRole.None;

    // a world of its own (hosting or opening one): it runs whatever its host's game does (OD-150)
    private bool OwnWorld => HostsOrOpensWorld || Volatile.Read(ref _mineSnapshot).Role == UiWorldRole.Hosting;

    // OD-190: the activity a quick join asks for (WorldActivities.Placement)
    private static string PlacementOf(byte kind) => kind switch
    {
        FreeRoamKind.CarMeet => WorldActivities.CarMeet,
        FreeRoamKind.RoadTrip => WorldActivities.RoadTrip,
        _ => WorldActivities.FreeRoam,
    };

    /// <summary>
    /// OD-190 (§11.3 item 1) with no request: a world of its own, or a seat whose world runs this ask now as the last seat read,
    /// renewal or quick join said (a free roam ask in a player world not read yet stays too, as before). An unproven seat taken for
    /// another kind never does (test.11).
    /// </summary>
    private bool StaysAtOnce(byte kind) => UnprovenOtherSeat(kind) is null
        && (OwnWorld || HasFreeRoamWorld && !HostGone(Volatile.Read(ref _seatWorld)) && SeatRunsCached(kind) == true);

    // the seat's world as last read: its host is away (CR-L49: its game quit or left the session)
    private bool HostGone(string? world)
    {
        lock (_mineState) return world is not null && _seatHostAway is { } away && away.WorldId == world && away.Away;
    }

    /// <summary>Whether the seat's world runs this ask, from the seat reads alone: true, false (an official world), or null (read it).</summary>
    private bool? SeatRunsCached(byte kind)
    {
        if (Volatile.Read(ref _seatWorld) is not { } world) return true; // a seat whose world is not known here stays (as before)
        string? cached;
        lock (_mineState) cached = _seatActivity is { } seen && seen.WorldId == world ? seen.Activity : null;
        if (Volatile.Read(ref _seatOfficial) == world) return cached is null ? null : cached == PlacementOf(kind); // one activity for life
        if (cached is null) return kind == FreeRoamKind.FreeRoam ? true : null;
        // a player world: its host's activity now (a car meet runs on its lobby, read); a mismatch is read again before it moves the seat
        return kind != FreeRoamKind.CarMeet && cached == ActivityOf(kind) ? true : null;
    }

    /// <summary>
    /// OD-190 (§11.3): whether the seat's world runs this ask now, read from the server when the seat reads do not say: (stays, why for
    /// the log, the notice of a move). A player world that is gone or whose host is away is the host leaving; one whose host changed
    /// activity while this guest held it, a guest that could not follow. An official world has no host: its end says nothing. Anything
    /// unknown (a failed read) keeps the seat.
    /// </summary>
    private async Task<(bool Stays, string Why, string? Moved)> SeatRunsAsync(byte kind, CancellationToken cancellation)
    {
        var placement = PlacementOf(kind);
        if (Volatile.Read(ref _seatWorld) is not { } world) return (true, "its world is not known here", null);
        if (HostGone(world)) return (false, $"the host of {world} is away", "worlds.moved_host_left");
        switch (SeatRunsCached(kind))
        {
            case true: return (true, $"it runs {placement}", null);
            case false: return (false, $"official {world} does not run {placement}", null);
        }
        MmResult<WorldView>? view = null;
        try { view = await _mm.GetAsync(world, cancellation).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested) { }
        var official = Volatile.Read(ref _seatOfficial) == world || view?.Value?.Official == true;
        if (view is { Error.Kind: MmErrorKind.WorldNotFound }) return (false, $"{world} closed", official ? null : "worlds.moved_host_left");
        if (view is not { Ok: true, Value: { } seated }) return (true, "its world could not be read", null);
        if (seated.HostAway == true && !official) return (false, $"the host of {world} is away", "worlds.moved_host_left");
        if (official) Volatile.Write(ref _seatOfficial, world);
        // an official world by its one activity only (never its lobbies); a player world by its host's activity, a car meet by its lobby
        var runs = official ? seated.Activity == placement
            : kind == FreeRoamKind.CarMeet ? seated.Lobbies.Contains(LobbyKinds.CarMeet) : seated.Activity == ActivityOf(kind);
        if (runs)
        {
            if (!official) Volatile.Write(ref _hostMoved, null); // the guest is with its host again
            return (true, $"it runs {placement}", null);
        }
        return (false, $"{world} runs {(kind == FreeRoamKind.CarMeet && !official ? "no car meet" : seated.Activity)}, not {placement}",
            !official && Volatile.Read(ref _hostMoved)?.World == world ? "worlds.moved_cannot_follow" : null);
    }

    /// <summary>OD-190: the held seat ended because its world closed (its host quit or closed it); the next placement says so.</summary>
    private void ObserveHostLeft(SeatEnded? ended, string? heldGrant)
    {
        if (heldGrant is null || ended is not { Reason: SeatEnds.WorldClosed } || ended.GrantId != heldGrant) return;
        if (Volatile.Read(ref _seatOfficial) == ended.WorldId) return; // an official world closing is not a host leaving
        Volatile.Write(ref _hostLeft, new RecentWorld(ended.WorldId, Now));
    }

    /// <summary>
    /// OD-190/191: the seat's world, as a grant, a seat read or a renewal names it (a grant taken over from a read too: an OD-191
    /// group seat, an accepted request, a restart). A new world forgets the last one's host notices.
    /// </summary>
    private void SeatWorldIs(string world)
    {
        if (Interlocked.Exchange(ref _seatWorld, world) == world) return;
        Volatile.Write(ref _hostMoved, null);
        Volatile.Write(ref _hostLeft, null);
    }

    /// <summary>
    /// OD-191 (§11.4): before a seat moves (or a quick join takes one), the seat is read again (<c>GET /mm/v1/seat</c>, the overlay's
    /// path): a group world's seat issued moments ago for the car meet's session replaces the car meet seat, and is kept.
    /// </summary>
    private async Task RefreshSeatAsync(CancellationToken cancellation)
    {
        RequestReconcile();
        await MineAsync(background: false, cancellation).WaitAsync(cancellation).ConfigureAwait(false);
    }

    private async Task<byte> FreeRoamCoreAsync(byte kind, CancellationToken cancellation)
    {
        var lobby = LobbyOf(kind);
        var activity = ActivityOf(kind);
        var placement = PlacementOf(kind);
        if (StaysAtOnce(kind)) return Decided(kind, FreeRoamResult.Ready, Staying());
        // the whole ask, reads included, ends inside the hook's wait, so its notice is still shown
        var started = _clock.GetTimestamp();
        using var budget = new CancellationTokenSource(FreeRoamAskBudget, _clock);
        using var bounded = CancellationTokenSource.CreateLinkedTokenSource(cancellation, budget.Token);
        var token = bounded.Token;
        try
        {
            await RefreshSeatAsync(token).ConfigureAwait(false);
            if (StaysAtOnce(kind)) return Decided(kind, FreeRoamResult.Ready, Staying("its seat as read now"));
            if (await AccessAsync(token).ConfigureAwait(false) is { Allowed: false })
                return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, "no world (online lobbies are not allowed now)");
            if (WireMods().Refusal is { } refusal) return Decided(kind, FreeRoamResult.None, $"no world (mods: {refusal})");
            // test.11: a seat taken for another kind that never played is not this ask's world ("stay" left the game on FINDING
            // SESSION): it is let go (not on purpose) and this kind is placed as usual. No CancelMatchmaking: this ask is
            // the game's live matchmaking now.
            if (UnprovenOtherSeat(kind) is { } stale)
            {
                var staleWorld = Volatile.Read(ref _seatWorld) ?? "its seat";
                if (await ReleaseUnprovenSeatAsync(stale.Grant, $"an unproven {KindName(stale.Kind)} seat dropped for a {KindName(kind)} ask").ConfigureAwait(false))
                    _log($"worlds: {KindName(kind)}: dropped the unproven {KindName(stale.Kind)} seat in {staleWorld}; joining again");
            }
            if (OwnWorld) return Decided(kind, FreeRoamResult.Ready, Staying());
            string? moved = null;
            if (_seat.GrantId is not null)
            {
                // OD-190 (§11.3 item 1), OD-191: any held seat (reserved or seated) whose world runs this ask stays; otherwise the quick
                // join below moves it (the server ends the old seat in the same transaction; with no free official world it is kept)
                var seatWorld = Volatile.Read(ref _seatWorld) ?? "its seat";
                var (stays, why, notice) = await SeatRunsAsync(kind, token).ConfigureAwait(false);
                if (stays) return Decided(kind, FreeRoamResult.Ready, Staying(why));
                moved = notice;
                _log($"worlds: {KindName(kind)}: leaving {seatWorld}: {why}; quick join {placement}");
            }
            else if (Interlocked.Exchange(ref _hostLeft, null) is { } left && Now - left.LeftAt <= RejoinWindow) moved = "worlds.moved_host_left";
            // the player world the game left moments ago comes first (a failed car meet step retries at once), so a retry never
            // splits its players; an official one is placed again by activity
            if (Interlocked.Exchange(ref _recentlyLeft, null) is { } recent && Now - recent.LeftAt <= RejoinWindow && !HasFreeRoamWorld
                && lobby is null && !Isolated && moved is null && Volatile.Read(ref _seatOfficial) != recent.WorldId)
            {
                token.ThrowIfCancellationRequested();
                var back = await JoinAsync(new UiFrame.JoinWorld(recent.WorldId, string.Empty), activity, token, kind).ConfigureAwait(false);
                if (back.Result == UiJoin.Reserved)
                    return Decided(kind, FreeRoamResult.Ready, $"joined {recent.WorldId} (back in the world it left {(int)(Now - recent.LeftAt).TotalSeconds} s ago)");
                _log($"worlds: {KindName(kind)}: {recent.WorldId}, left {(int)(Now - recent.LeftAt).TotalSeconds} s ago, refused ({back.Result})");
            }
            // OD-149: an isolated player (or one the anti-cheat move is moving) plays in pineapple player worlds only, and an official
            // world never is one (§2.9)
            if (Isolated || Volatile.Read(ref _poolMoving) != 0)
                return await PoolJoinAsync(kind, lobby, activity, token).ConfigureAwait(false)
                    ?? Decided(kind, FreeRoamResult.None, "no world (isolated: no pineapple lobby to join)");
            return await QuickJoinAsync(kind, moved, started, token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (budget.IsCancellationRequested && !cancellation.IsCancellationRequested && OnlineOpen)
        {
            PlacementFailedToast();
            return Decided(kind, FreeRoamResult.None, $"no world (no lobby within {(int)FreeRoamAskBudget.TotalSeconds} s)");
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested || !OnlineOpen)
        {
            return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, OnlineOpen ? "no world (the game stopped waiting)" : "no world (online is closed)");
        }
    }

    // OD-190 (W-4 review): a placement that failed for anything but full lobbies says so
    private void PlacementFailedToast() =>
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text("worlds.placement_failed")));

    /// <summary>
    /// OD-190 (§11.2, §11.3): <c>POST /mm/v1/join {activity}</c> through the overlay's own join (its grant, socket proof context and
    /// relay leg, so the game's ticket lands in the new world). <c>lobbies_full</c> waits its <c>Retry-After</c> (1-5 s) and asks again
    /// until <see cref="LobbiesFullBudget"/> has passed on the clock since the first ask (each ask's own time counts), then says "All
    /// Infinity lobbies are full"; never past <see cref="FreeRoamAskBudget"/> from <paramref name="started"/>. Any other refusal says
    /// "Couldn't join an Infinity lobby". <paramref name="moved"/>: the notice of a move.
    /// </summary>
    private async Task<byte> QuickJoinAsync(byte kind, string? moved, long started, CancellationToken cancellation)
    {
        var placement = PlacementOf(kind);
        TimeSpan? firstAsk = null;
        while (true)
        {
            if (OwnWorld) return Decided(kind, FreeRoamResult.Ready, Staying()); // the player opened a world meanwhile
            if (Volatile.Read(ref _joinAttempt) != 0) return Decided(kind, FreeRoamResult.None, "no world (the player's own join is in flight)");
            if (!OnlineOpen) return Decided(kind, FreeRoamResult.NotOnline, "no world (online is closed)");
            cancellation.ThrowIfCancellationRequested(); // the hook's wait is bounded: no join past it
            firstAsk ??= _clock.GetElapsedTime(started);
            Volatile.Write(ref _lobbiesFullAfter, -1);
            var joined = await JoinAsync(new UiFrame.JoinWorld(string.Empty, string.Empty), placement, cancellation, kind).ConfigureAwait(false);
            if (joined.Result == UiJoin.Reserved)
            {
                var world = Volatile.Read(ref _seatWorld);
                if (world is not null)
                {
                    Volatile.Write(ref _seatOfficial, world);
                    lock (_mineState) _seatActivity = (world, placement);
                }
                if (moved is not null) _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60, _text(moved)));
                return Decided(kind, FreeRoamResult.Ready, $"joined {world ?? "an official world"} (quick join {placement}{(moved is null ? string.Empty : ", " + moved)})");
            }
            if (Interlocked.Exchange(ref _lobbiesFullAfter, -1) is var ticks and >= 0)
            {
                var after = TimeSpan.FromTicks(Math.Clamp(ticks, TimeSpan.TicksPerSecond, 5 * TimeSpan.TicksPerSecond));
                var now = _clock.GetElapsedTime(started);
                if (now + after - firstAsk.Value > LobbiesFullBudget || now + after > FreeRoamAskBudget)
                {
                    _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text("worlds.lobbies_full")));
                    return Decided(kind, FreeRoamResult.None, $"no world (every official {placement} world was full for {(int)(now - firstAsk.Value).TotalSeconds} s)");
                }
                _log($"worlds: {KindName(kind)}: every official {placement} world is full; asking again in {(int)after.TotalSeconds} s");
                await Task.Delay(after, _clock, cancellation).ConfigureAwait(false);
                continue;
            }
            if (joined.Result == UiJoin.SlowDown && Volatile.Read(ref _joinAttempt) != 0)
                return Decided(kind, FreeRoamResult.None, "no world (the player's own join is in flight)");
            if (OnlineOpen) PlacementFailedToast();
            return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, $"no world (quick join {placement} refused: {joined.Result})");
        }
    }

    /// <summary>
    /// OD-149: an isolated player's ask joins the closest pineapple player world from the list that runs it (at most
    /// <see cref="FreeRoamJoinAttempts"/>; full or closed answers move on). Null: none joined.
    /// </summary>
    private async Task<byte?> PoolJoinAsync(byte kind, string? lobby, string? activity, CancellationToken cancellation)
    {
        List<(WorldView World, LobbyRow Row)> candidates;
        try
        {
            var read = await ReadLobbiesAsync(WorldFilters.Every, notFull: true, cancellation).ConfigureAwait(false);
            if (read.FailureKey is { } failure) { _log($"worlds: {KindName(kind)}: no pineapple lobby read ({failure})"); return null; }
            candidates = OrderLobbies(read.Worlds.Select(w => (w.World, SessionRow(w.World, w.Locked))))
                .Where(r => Matching(r.Row) && !r.World.NeedsCode && r.World.Pool
                    && r.World.Audience is WorldAudiences.Everyone or WorldAudiences.Supporters
                    && (lobby is null || r.World.Lobbies.Contains(lobby))
                    // OD-150: a free roam or Road Trip ask joins a world running that now; a car meet ask any world that runs one
                    && (activity is null || r.World.Activity == activity))
                .Take(FreeRoamJoinAttempts).ToList();
        }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested)
        {
            _log($"worlds: {KindName(kind)}: no pineapple lobby read ({error.GetType().Name})");
            return null;
        }
        // a world the player left on purpose is not where its next lobby search sends it
        if (Volatile.Read(ref _leftOnPurpose) is { } avoided && Now - avoided.LeftAt <= RejoinWindow)
            candidates = [.. candidates.Where(c => c.World.Id != avoided.WorldId)];
        foreach (var (world, _) in candidates)
        {
            if (HasFreeRoamWorld) return Decided(kind, FreeRoamResult.Ready, Staying());
            cancellation.ThrowIfCancellationRequested(); // the hook's wait is bounded: no join past it
            var joined = await JoinAsync(new UiFrame.JoinWorld(world.Id, string.Empty), activity, cancellation, kind).ConfigureAwait(false);
            if (joined.Result == UiJoin.Reserved)
            {
                FreeRoamToast("worlds.free_roam_joined", world.Name);
                return Decided(kind, FreeRoamResult.Ready, $"joined {world.Id}");
            }
            if (joined.Result is not (UiJoin.Full or UiJoin.Closed))
                return HasFreeRoamWorld ? Decided(kind, FreeRoamResult.Ready, Staying($"join of {world.Id} refused ({joined.Result})"))
                    : Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, $"no world (join of {world.Id} refused: {joined.Result})");
            _log($"worlds: {KindName(kind)}: {world.Id} refused ({joined.Result}); trying the next lobby");
        }
        return HasFreeRoamWorld ? Decided(kind, FreeRoamResult.Ready, Staying()) : null;
    }

    /// <summary>The seat is let go to find a world that runs the asked lobby (not "on purpose": that world may come back later).</summary>
    private async Task LeaveForLobbyAsync(string why, CancellationToken cancellation)
    {
        InvalidateMine(reset: true);
        Volatile.Write(ref _seatWorld, null);
        _seat.Left();
        await ClearSeatEndAsync(why).ConfigureAwait(false); // no relay leg outlives the seat
        try { Observe(await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false)); }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested) { } // the seat lapses on its own
    }

    private void FreeRoamToast(string key, string lobby) =>
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60, _text(key).Replace("{lobby}", lobby, StringComparison.Ordinal)));

    // OD-151: the activity of the seat's world as last read (_mineState only); null: none read for this seat yet
    private (string WorldId, string Activity)? _seatActivity;

    /// <summary>
    /// OD-151: a seat's world changed activity (its host's game went into a Road Trip or back to free roam): the guest follows
    /// the host, so the seat stays and a toast says what the host runs now. The first read of a seat only records it; the
    /// host's own world never toasts. Called under <c>_mineState</c> with each seat read and renewal. OD-190: an official world's
    /// one activity is recorded (a later ask for another moves the seat) and never toasts.
    /// </summary>
    private void ObserveActivity(Seat seat)
    {
        ObserveHostAway(seat); // CR-L49: a guest is told once when its host goes away
        if (seat.State == SeatStates.Hosting || seat.World.Id != seat.WorldId) { _seatActivity = null; return; }
        var official = seat.World.Official == true;
        if (official) Volatile.Write(ref _seatOfficial, seat.WorldId);
        var activity = seat.World.Activity;
        if (!WorldActivities.Placement.Contains(activity)) return; // a value this launcher does not know
        if (_seatActivity is { } last && last.WorldId == seat.WorldId && last.Activity != activity && !official
            && WorldActivities.All.Contains(last.Activity) && WorldActivities.All.Contains(activity))
        {
            _log($"worlds: activity {last.Activity} -> {activity} in {seat.WorldId} (the host's)");
            // OD-190: a later ask for another activity could not follow the host; back in the activity it ran before, it can again
            Volatile.Write(ref _hostMoved, Volatile.Read(ref _hostMoved) is { } moved && moved.World == seat.WorldId
                ? activity == moved.From ? null : moved : new HostMoved(seat.WorldId, last.Activity));
            _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60,
                _text("worlds.host_activity").Replace("{mode}", ActivityName(activity), StringComparison.Ordinal)));
            // OD-151 follow-up: the guest's game starts its own matchmaking for the host's activity (the player confirms with (X))
            Volatile.Write(ref _followHost, new Session.Frame.FollowHost(
                activity == WorldActivities.RoadTrip ? Session.FollowHostLobby.RoadTrip : Session.FollowHostLobby.FreeRoam, Public: true));
        }
        _seatActivity = (seat.WorldId, activity);
    }

    // OD-151 follow-up: the FollowHost frame of the last activity change, not sent yet (the latest change wins)
    private Session.Frame.FollowHost? _followHost;

    /// <summary>Writes <see cref="Session.Frame.FollowHost"/> to the hook (the supervisor's control pipe).</summary>
    public Func<Session.Frame.FollowHost, CancellationToken, Task>? SendFollowHost { get; set; }

    /// <summary>
    /// OD-151 follow-up: sends the pending <see cref="Session.Frame.FollowHost"/> of a seat's activity change, once. Taken whether or not
    /// it can be written: a change is never replayed later (a closed or suspended session, or no hook writer, drops it with a log line).
    /// </summary>
    public async Task FollowHostAsync(CancellationToken cancellation)
    {
        if (Interlocked.Exchange(ref _followHost, null) is not { } follow) return;
        var name = follow.LobbyType == Session.FollowHostLobby.RoadTrip ? "Road Trip" : "free roam";
        if (!OnlineOpen || SendFollowHost is null) { _log($"worlds: the host is in {name}; online is closed here, the game is not asked to follow"); return; }
        try
        {
            await SendFollowHost(follow, cancellation).ConfigureAwait(false);
            _log($"worlds: the host is in {name}; the game is asked to look for it (FollowHost, the player confirms)");
        }
        catch (Exception error) when (error is IOException or InvalidOperationException or ProtocolException or ObjectDisposedException)
        { _log("worlds: FollowHost not sent (" + error.GetType().Name + ")"); }
    }

    /// <summary>The player-facing name of an activity: "Online Free Roam" or "Online Road Trip" (both languages).</summary>
    /// <summary>What a world ran when the lobby list last showed it (no extra read); null when it was not listed.</summary>
    private string? ListedActivity(string worldId) => _listedActivity.TryGetValue(worldId, out var activity) ? activity : null;

    private string ActivityName(string activity) =>
        _text(activity == WorldActivities.RoadTrip ? "worlds.activity_roadtrip" : "worlds.activity_freeroam");

    // OD-150: the activity a lobby ask joins by and opens with; a car meet does not change it, so it asks for none
    private static string? ActivityOf(byte kind) => kind switch
    {
        FreeRoamKind.FreeRoam => WorldActivities.FreeRoam,
        FreeRoamKind.RoadTrip => WorldActivities.RoadTrip,
        _ => null,
    };

    /// <summary>
    /// CR-L40 §4: <paramref name="template"/> (<c>{gamertag}'s lobby</c>, <c>Lobby de {gamertag}</c>) with the gamertag
    /// cut on a whole code point so the name stays within <see cref="MaxLobbyName"/> characters. No gamertag: "Infinity".
    /// </summary>
    public static string DefaultLobbyName(string? template, string? gamertag)
    {
        const string Placeholder = "{gamertag}";
        var tag = gamertag?.Trim();
        if (string.IsNullOrEmpty(tag)) return "Infinity";
        if (string.IsNullOrEmpty(template) || !template.Contains(Placeholder, StringComparison.Ordinal)) template = Placeholder + "'s lobby";
        var room = MaxLobbyName - (template.Length - Placeholder.Length);
        if (room <= 0) return tag.Length <= MaxLobbyName ? tag : Cut(tag, MaxLobbyName);
        return template.Replace(Placeholder, tag.Length <= room ? tag : Cut(tag, room), StringComparison.Ordinal);

        static string Cut(string text, int chars)
        {
            var builder = new StringBuilder();
            foreach (var rune in text.EnumerateRunes())
            {
                if (builder.Length + rune.Utf16SequenceLength > chars) break;
                builder.Append(rune.ToString());
            }
            return builder.ToString().TrimEnd();
        }
    }
}
