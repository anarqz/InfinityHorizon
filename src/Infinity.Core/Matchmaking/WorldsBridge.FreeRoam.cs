// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using Infinity.Core.Api;
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

    /// <summary>
    /// T-L6 (OD-204, WORLDS_OFFICIAL §12.2): the game created a private free roam in its own menu; the hook holds its session
    /// write until the launcher holds the player's private official lobby (<c>POST /mm/v1/join {activity, private: true}</c>).
    /// Sent only to a launcher that answered <see cref="Session.CaptureWhat.PrivateCreate"/>.
    /// </summary>
    public const byte PrivateFreeRoam = 3;

    /// <summary>T-L6: as <see cref="PrivateFreeRoam"/> for a private Road Trip (the hook derives it once T-R2 finds its marker).</summary>
    public const byte PrivateRoadTrip = 4;

    /// <summary>Kinds 3 and 4: a private create, never a matchmaking ask.</summary>
    public static bool IsPrivateCreate(byte kind) => kind is PrivateFreeRoam or PrivateRoadTrip;
}

// CR-L40 §3.2, OD-190 (WORLDS_OFFICIAL §11.3): before the game's FreeRoam, car meet or Road Trip hopper request the hook asks;
// a seat in an official world that runs that activity stays (rule 1, the held seat); otherwise the launcher quick joins an
// official world of the activity (the overlay's own join, with its access, game-readiness and mods checks). It never opens a
// world for a ticket (OD-208: players host no world; a private official lobby is created only by the game's own menu).
public sealed partial class WorldsBridge
{
    /// <summary>OD-190 (§11.3 item 3): how long a quick join answered <c>lobbies_full</c> keeps retrying before it says so.</summary>
    public static readonly TimeSpan LobbiesFullBudget = TimeSpan.FromSeconds(10);

    /// <summary>The longest one ask may take, reads included: inside the hook's 18 s (PlaySupervisor.FreeRoamBudget), so its notice shows.</summary>
    public static readonly TimeSpan FreeRoamAskBudget = TimeSpan.FromSeconds(15);

    /// <summary>The most joins one ask sends (the <c>mm.join</c> bucket is 6 a minute, MATCHMAKING_API §1): one is left for the player.</summary>
    public const int MaxJoinsPerAsk = 5;

    // OD-190: the Retry-After of the last quick join answered lobbies_full, as ticks; -1: the last join was not one
    private long _lobbiesFullAfter = -1;
    // OD-190: the seat's world when it is an official one (a quick join's, or a seat read or world read said so)
    private string? _seatOfficial;

    /// <summary>
    /// OD-192: the seat's world is an official one. Official play stays vanilla: no overlay notice is sent while it holds
    /// (<see cref="Supervisor.OverlayChannel.Quiet"/>); notices show in user lobbies only. It holds after that seat ends too (its
    /// "seat ended" and "left" notices stay quiet) until the player takes a seat in another world.
    /// </summary>
    public bool InOfficialLobby => Volatile.Read(ref _seatOfficial) is { } official
        && (Volatile.Read(ref _seatWorld) is not { } world || world == official);

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
        Volatile.Write(ref _privateRejoinArmed, 0); // T-L4 (a): the player's own Leave is never undone by privateWorldId
        if (Interlocked.Exchange(ref _seatWorld, null) is { } world)
        {
            Volatile.Write(ref _leftOnPurpose, new RecentWorld(world, Now));
            if (SeatPrivate(world)) Volatile.Write(ref _privateLeft, world);
        }
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
    /// OD-148 (refines OD-57): the game left free roam. After <see cref="FreeRoamLeftGrace"/> with no lobby ask an official world's
    /// seat is let go (<see cref="LeaveAsync"/>; a private lobby's played seat stays, the server's CR-S86 release bounds it); a
    /// lobby ask, the game joining a world again (<see cref="GameJoined"/>), the overlay's own leave or join, the session's end or
    /// another leave of free roam ends this wait first. A lobby ask answered Ready within the grace BEFORE this leave counts too
    /// (live, test.9: back from a Road Trip the game asks for free roam, opens it, and only then closes its Road Trip connection):
    /// no wait starts. Only the leave waits: the seat's renewals go on meanwhile.
    /// </summary>
    public async Task FreeRoamLeftAsync(CancellationToken cancellation)
    {
        GameInWorld(false);
        // W-6: a seat staged while the game played is installed at once (no read); OD-191: else an official car meet's seat is watched
        if (Volatile.Read(ref _staged) is not null) await InstallStagedAsync("the game left its world").ConfigureAwait(false);
        StartGroupSeatWatch(cancellation);
        var asked = Interlocked.Exchange(ref _lobbyReadyTicks, 0); // one ask covers one leave
        if (asked != 0 && Now - new DateTimeOffset(asked, TimeSpan.Zero) is var ago && ago >= TimeSpan.Zero && ago <= FreeRoamLeftGrace)
        {
            CancelFreeRoamLeave();
            if (Volatile.Read(ref _seatWorld) is { } kept)
                _log($"worlds: free roam left; {kept} stays: the game asked for its next lobby {(int)ago.TotalSeconds} s ago");
            return;
        }
        var pending = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _onlineStop.Token);
        CancelFreeRoamLeave(Interlocked.Exchange(ref _freeRoamLeave, pending)); // a new leave starts the wait again
        var world = Volatile.Read(ref _seatWorld);
        var due = false;
        try
        {
            if (InviteMoveHolds(world) is not null)
                _log($"worlds: free roam left; {world} is kept: an accepted invite moves the game there");
            else if (world is not null)
                _log(KeepsPrivateSeat() is not null
                    ? $"worlds: free roam left; {world} is kept for the game's next lobby ({KeptSeat})"
                    : $"worlds: free roam left; {world} is kept {(int)FreeRoamLeftGrace.TotalSeconds} s for the game's next lobby");
            var wait = FreeRoamLeftGrace;
            while (true)
            {
                await Task.Delay(wait, _clock, pending.Token).ConfigureAwait(false);
                wait = FreeRoamLeftGrace;
                if (Volatile.Read(ref _freeRoamLeave) != pending) break; // called off meanwhile
                // T-L5: an accepted invite moves the game into this seat's private lobby: the move's own bounds end it (InviteMoveWatchAsync)
                if (InviteMoveHolds(Volatile.Read(ref _seatWorld)) is { } moving)
                {
                    wait = moving < FreeRoamLeftGrace ? moving : FreeRoamLeftGrace;
                    _log($"worlds: {Volatile.Read(ref _seatWorld)} stays: an accepted invite moves the game there ({(int)Math.Ceiling(moving.TotalSeconds)} s of its bounds left)");
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
        // §11.3 rule 1 (W-14, live, worlds9 17:14:39 local): a played seat in a private official lobby is never let go by this rule.
        // The game may take longer than the grace between leaving free roam and its next lobby ask; the seat ends only on the player's
        // own leave, the world's close, or the server's release (CR-S86), and an ask after that rejoins the world first (RejoinWindow).
        // A public official world's seat goes as before.
        if (KeepsPrivateSeat() is { } player)
        {
            Volatile.Write(ref _keptPastGrace, player); // review 1: the next ask reads the seat before it says "stay"
            _log($"worlds: free roam left {(int)FreeRoamLeftGrace.TotalSeconds} s ago with no lobby asked; {player} stays ({KeptSeat} "
                + "is kept for the game's next lobby; the player's leave, the world's close or the server's release ends it)");
            return;
        }
        if (Volatile.Read(ref _seatWorld) is { } gone)
            _log($"worlds: free roam left {(int)FreeRoamLeftGrace.TotalSeconds} s ago with no lobby asked; leaving {gone}");
        await LeaveAsync(cancellation).ConfigureAwait(false);
    }

    /// <summary>
    /// §11.3 rule 1 (W-14, T-L6, WORLDS_OFFICIAL §12.2 item 11): the seat's world when it is a private official lobby whose seat the
    /// game played (a used seat, which CR-S86 bounds); null otherwise. A reserved seat the game never played keeps the 30 s rule.
    /// </summary>
    private string? KeepsPrivateSeat() => _seat.GrantId is { } grant && Played(grant) && Volatile.Read(ref _seatWorld) is { } world
        && SeatPrivate(world) ? world : null;

    // W-14 review 1: the private lobby whose seat was kept past the grace with the game out of it (the server may release it any
    // moment, CR-S86): every ask reads the seat first instead of trusting the cached activity; cleared when the game joins again
    private string? _keptPastGrace;

    /// <summary>
    /// The game joined a world again (the hook's Event 3): a pending leave after free roam is called off, the game is in the world
    /// (live, test.9: a joiner's Road Trip connection ended just before its free roam one opened). OD-208: every join is a guest's
    /// (players host no world), whatever detail the hook sent.
    /// </summary>
    public void GameJoined()
    {
        var pending = Interlocked.Exchange(ref _freeRoamLeave, null);
        Volatile.Write(ref _keptPastGrace, null); // W-14: the game is in a world again
        // the join is the seat's proof: no give-up; OD-191: never bound to a car meet seat that is moving
        // W-6: with a seat staged it is installed first and the join bound to it
        JoinedFor(); // the lobby this connection is (a car meet's, after a car meet ask)
        if (!JoinStaged() && !HoldJoinForGroupSeat()) { SeatPlays("the game joined"); GameInWorld(true); }
        if (pending is null) return;
        CancelFreeRoamLeave(pending);
        if (Volatile.Read(ref _seatWorld) is { } world) _log($"worlds: the game joined again; {world} stays");
    }

    // the kind + 1 of the last lobby ask answered Ready, until the game joins a world (0: none)
    private int _readyKind;
    // the kind + 1 of the lobby ask the game's current world connection was joined for (0: none or not known), until it leaves
    private int _joinedKind;

    /// <summary>The game joined a world (Event 3): its connection is the lobby the last Ready ask was for.</summary>
    private void JoinedFor() => Volatile.Write(ref _joinedKind, Interlocked.Exchange(ref _readyKind, 0));

    /// <summary>A pending leave after free roam is called off (the seat, or the player's own choice, stays).</summary>
    private void CancelFreeRoamLeave() => CancelFreeRoamLeave(Interlocked.Exchange(ref _freeRoamLeave, null));

    private static void CancelFreeRoamLeave(CancellationTokenSource? pending)
    {
        try { pending?.Cancel(); }
        catch (ObjectDisposedException) { } // its wait already ended
        catch (AggregateException) { }
    }

    private string KindName(byte kind) => kind switch
    {
        FreeRoamKind.CarMeet => "car meet",
        FreeRoamKind.RoadTrip => "road trip",
        // CR-S99 (W7b): the game's private create runs the activity its game says (FM5: a race); FH2's wording is unchanged
        FreeRoamKind.PrivateFreeRoam => PrivateCreateActivity == WorldActivities.Race ? "private race" : "private free roam",
        FreeRoamKind.PrivateRoadTrip => "private road trip",
        _ => "free roam",
    };

    // the world a FREE ROAM ONLINE ask stays in, for the log
    private string Staying(string? why = null) =>
        $"stay in {Volatile.Read(ref _seatWorld) ?? "its seat"} (seated{SeatOrigin()}{(why is null ? string.Empty : ", " + why)})";

    private byte Decided(byte kind, byte result, string what)
    {
        _log($"worlds: {KindName(kind)}: {what}");
        if (result == FreeRoamResult.Ready)
        {
            Interlocked.Exchange(ref _lobbyReadyTicks, Now.UtcTicks); // the next leave of free roam is this lobby's
            Volatile.Write(ref _readyKind, kind + 1); // the lobby the game's next world connection is (T-L5: GameInEvent)
            InviteMoveAsked(kind); // T-L5: an accepted invite's game asked for its lobby
            ArmGiveUp(kind); // a seat that never played gets SeatGiveUp to start playing
        }
        return result;
    }

    /// <summary>
    /// FREE ROAM ONLINE is about to ask for its world (the hook's <c>FreeRoamNeed</c>, CR-L40 §3.2): a seat whose world runs free roam
    /// now answers at once; else an official free roam world is quick joined (OD-190).
    /// Concurrent asks share one run.
    /// </summary>
    public Task<byte> EnsureFreeRoamWorldAsync(CancellationToken cancellation) => EnsureFreeRoamWorldAsync(FreeRoamKind.FreeRoam, cancellation);

    /// <summary>
    /// As <see cref="EnsureFreeRoamWorldAsync(CancellationToken)"/> for a car meet or an online Road Trip (<see cref="FreeRoamKind"/>).
    /// OD-190 (WORLDS_OFFICIAL §11.3 rule 1): a seat stays while its official world runs the asked activity (its one activity for
    /// life; a private lobby by PrivateLobbyPolicy). Otherwise (the player started another activity, the world closed, or no world
    /// at all) <c>POST /mm/v1/join {activity}</c> places it in an official world, the server ending the old seat, and the
    /// game's ticket lands there through the seat. <c>lobbies_full</c> is retried for <see cref="LobbiesFullBudget"/>. No world is
    /// ever opened for an ask. Asks of one kind share a run; another kind waits for it.
    /// </summary>
    public Task<byte> EnsureFreeRoamWorldAsync(byte kind, CancellationToken cancellation)
    {
        CancelFreeRoamLeave(); // the game left free roam for this ask: its world stays and is weighed here
        if (!OnlineOpen) return Task.FromResult(Decided(kind, FreeRoamResult.NotOnline, "no world (online is closed)"));
        // T-L6: a private create (kinds 3/4) always calls the server: the hook holds the game's session write for it
        if (!FreeRoamKind.IsPrivateCreate(kind) && StaysAtOnce(kind)) return Task.FromResult(Decided(kind, FreeRoamResult.Ready, Staying()));
        lock (_freeRoamState)
        {
            if (_freeRoamRun is { IsCompleted: false } running)
            {
                if (_freeRoamRunKind == kind) return running.WaitAsync(cancellation);
                _freeRoamRunKind = kind;
                return _freeRoamRun = AfterAsync(running, kind, cancellation);
            }
            _freeRoamRunKind = kind;
            return _freeRoamRun = CoreFor(kind, cancellation);
        }

        async Task<byte> AfterAsync(Task<byte> previous, byte next, CancellationToken token)
        {
            try { await previous.WaitAsync(token).ConfigureAwait(false); }
            catch (Exception error) when (error is not OperationCanceledException || !token.IsCancellationRequested) { }
            return await CoreFor(next, token).ConfigureAwait(false);
        }

        Task<byte> CoreFor(byte next, CancellationToken token) =>
            FreeRoamKind.IsPrivateCreate(next) ? PrivateCreateCoreAsync(next, token) : FreeRoamCoreAsync(next, token);
    }

    // a seat (reserved or seated), as this bridge last knew it; no request
    private bool HasFreeRoamWorld => _seat.GrantId is not null || Volatile.Read(ref _mineSnapshot).Role != UiWorldRole.None;

    // OD-190: the activity a quick join asks for (WorldActivities.Placement)
    private static string PlacementOf(byte kind) => kind switch
    {
        FreeRoamKind.CarMeet => WorldActivities.CarMeet,
        FreeRoamKind.RoadTrip or FreeRoamKind.PrivateRoadTrip => WorldActivities.RoadTrip,
        _ => WorldActivities.FreeRoam,
    };

    /// <summary>
    /// OD-190 (§11.3 item 1) with no request: a seat whose world runs this ask now as the last seat read,
    /// renewal or quick join said (a free roam ask in a world not read yet stays too, as before). An unproven seat taken for
    /// another kind never does (test.11).
    /// </summary>
    private bool StaysAtOnce(byte kind) => UnprovenOtherSeat(kind) is null
        && (HasFreeRoamWorld && SeatRunsCached(kind) == true
            && (Volatile.Read(ref _keptPastGrace) is not { } kept || kept != Volatile.Read(ref _seatWorld)) // W-14 review 1
            || kind == FreeRoamKind.CarMeet && FreshGroupSeat()); // W-5: a car meet ticket never releases a fresh group seat


    /// <summary>
    /// Whether the seat's official world runs this ask, from the seat reads alone: true, false, or null (read it). An official world
    /// runs one activity for life; a seat whose activity is not read yet stays on a free roam ask (as before) and is read for the others.
    /// </summary>
    private bool? SeatRunsCached(byte kind)
    {
        if (Volatile.Read(ref _seatWorld) is not { } world) return true; // a seat whose world is not known here stays (as before)
        string? cached;
        lock (_mineState) cached = _seatActivity is { } seen && seen.WorldId == world ? seen.Activity : null;
        if (cached is null) return Volatile.Read(ref _seatOfficial) != world && kind == FreeRoamKind.FreeRoam ? true : null;
        return cached == PlacementOf(kind);
    }

    /// <summary>
    /// OD-190 (§11.3): whether the seat's official world runs this ask now, read from the server when the seat reads do not say:
    /// (stays, why for the log). An official world runs its one activity; its end says nothing. Anything unknown (a failed read)
    /// keeps the seat.
    /// </summary>
    private async Task<(bool Stays, string Why)> SeatRunsAsync(byte kind, CancellationToken cancellation)
    {
        var placement = PlacementOf(kind);
        if (Volatile.Read(ref _seatWorld) is not { } world) return (true, "its world is not known here");
        switch (SeatRunsCached(kind))
        {
            case true: return (true, $"it runs {placement}");
            case false: return (false, $"official {world} does not run {placement}");
        }
        MmResult<WorldView>? view = null;
        try { view = await _mm.GetAsync(world, cancellation).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested) { }
        if (view is { Error.Kind: MmErrorKind.WorldNotFound }) return (false, $"{world} closed");
        if (view is not { Ok: true, Value: { } seated }) return (true, "its world could not be read");
        Volatile.Write(ref _seatOfficial, world);
        // an official world by its one activity only (never its lobbies)
        return seated.Activity == placement ? (true, $"it runs {placement}") : (false, $"{world} runs {seated.Activity}, not {placement}");
    }

    /// <summary>
    /// T-L6: the held seat ended. A private lobby's seat the server released (missing from the announce, a crash or a slow move) is
    /// rejoined by id; any other end of an official seat says nothing (the next ask places the game again).
    /// </summary>
    private void ObserveSeatEnded(SeatEnded? ended, string? heldGrant)
    {
        if (heldGrant is null || ended is null || ended.GrantId != heldGrant) return;
        if (SeatPrivate(ended.WorldId) && ended.Reason == SeatEnds.Released)
        {
            Volatile.Write(ref _recentlyLeft, new RecentWorld(ended.WorldId, Now));
            Volatile.Write(ref _privateRejoinArmed, 1); // T-L4 (a): the server's privateWorldId, when it names one, comes first
        }
    }

    /// <summary>
    /// OD-190/191: the seat's world, as a grant, a seat read or a renewal names it (a grant taken over from a read too: an OD-191
    /// group seat, an accepted request, a restart).
    /// </summary>
    private void SeatWorldIs(string world)
    {
        Volatile.Write(ref _privateRejoinArmed, 0); // T-L4 (a): a seat is held: privateWorldId is no rejoin target now
        if (Interlocked.Exchange(ref _seatWorld, world) == world) return;
        Volatile.Write(ref _keptPastGrace, null);
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
            // T-L6 (WORLDS_OFFICIAL §12.2 item 11): a private lobby's seat (known from the seat just read) is never dropped by test.11
            // nor moved by a quick join: PrivateLobbyPolicy says whether this ask stays there or holds
            if (PrivateSeatDecision(kind) is { } inPrivate) return inPrivate;
            // test.11: a seat taken for another kind that never played is not this ask's world ("stay" left the game on FINDING
            // SESSION): it is let go (not on purpose) and this kind is placed as usual. No CancelMatchmaking: this ask is
            // the game's live matchmaking now.
            if (UnprovenOtherSeat(kind) is { } stale)
            {
                var staleWorld = Volatile.Read(ref _seatWorld);
                if (await ReleaseUnprovenSeatAsync(stale.Grant, $"an unproven {KindName(stale.Kind)} seat dropped for a {KindName(kind)} ask").ConfigureAwait(false))
                    _log($"worlds: {KindName(kind)}: dropped the unproven {KindName(stale.Kind)} seat in {staleWorld ?? "its seat"}; joining again");
            }
            var joins = 0; // every join of this ask counts toward MaxJoinsPerAsk
            if (_seat.GrantId is not null)
            {
                // OD-190 (§11.3 item 1), OD-191: any held seat (reserved or seated) whose world runs this ask stays; otherwise the quick
                // join below moves it (the server ends the old seat in the same transaction; with no free official world it is kept)
                var seatWorld = Volatile.Read(ref _seatWorld) ?? "its seat";
                var (stays, why) = await SeatRunsAsync(kind, token).ConfigureAwait(false);
                if (stays) return Decided(kind, FreeRoamResult.Ready, Staying(why));
                _log($"worlds: {KindName(kind)}: leaving {seatWorld}: {why}; quick join {placement}");
            }
            // T-L6: the private lobby the game left moments ago comes first (a failed step retries at once), so a retry never splits
            // its players; a public official world is placed again by activity
            var recent = Volatile.Read(ref _recentlyLeft);
            var rejoin = recent is not null && Now - recent.LeftAt <= RejoinWindow && !HasFreeRoamWorld && !Isolated && SeatPrivate(recent.WorldId);
            if (recent is not null && (!rejoin || await RecentWorldRunsAsync(recent, kind, token).ConfigureAwait(false)))
                rejoin &= Interlocked.CompareExchange(ref _recentlyLeft, null, recent) == recent; // one ask takes it
            else rejoin = false;
            if (rejoin && recent is not null)
            {
                token.ThrowIfCancellationRequested();
                joins++;
                var back = await JoinAsync(new UiFrame.JoinWorld(recent.WorldId), activity, token, kind).ConfigureAwait(false);
                if (back.Result == UiJoin.Reserved)
                    return Decided(kind, FreeRoamResult.Ready, $"joined {recent.WorldId} (back in the world it left {(int)(Now - recent.LeftAt).TotalSeconds} s ago)");
                _log($"worlds: {KindName(kind)}: {recent.WorldId}, left {(int)(Now - recent.LeftAt).TotalSeconds} s ago, refused ({back.Result})");
                if (SeatPrivate(recent.WorldId)) Volatile.Write(ref _privateRejoinArmed, 0); // T-L4 (a): never asked again from privateWorldId
            }
            // OD-149 (OD-208): an isolated player quick joins too; the server places a flagged caller (or refuses with 409 pool)
            return await QuickJoinAsync(kind, started, joins, token).ConfigureAwait(false);
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

    /// <summary>
    /// T-L6: whether the private lobby the game left moments ago runs this ask, so the ask rejoins it before any quick join. A free
    /// roam ask always does (as before); a private lobby runs its one activity for life (a car meet ask never goes back to one, A7);
    /// one gone never. A failed read tries the world anyway (its join answers for it).
    /// </summary>
    private async Task<bool> RecentWorldRunsAsync(RecentWorld recent, byte kind, CancellationToken cancellation)
    {
        if (kind == FreeRoamKind.FreeRoam) return true;
        MmResult<WorldView>? view = null;
        try { view = await _mm.GetAsync(recent.WorldId, cancellation).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested) { }
        if (view is { Error.Kind: MmErrorKind.WorldNotFound }) return false;
        if (view is not { Ok: true, Value: { } seen }) return true;
        return kind != FreeRoamKind.CarMeet && seen.Activity == ActivityOf(kind);
    }

    // OD-190 (W-4 review): a placement that failed for anything but full lobbies says so
    private void PlacementFailedToast() =>
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text("worlds.placement_failed")));

    /// <summary>
    /// OD-190 (§11.2, §11.3): <c>POST /mm/v1/join {activity}</c> through the overlay's own join (its grant, socket proof context and
    /// relay leg, so the game's ticket lands in the new world). <c>lobbies_full</c> waits its <c>Retry-After</c> (2-5 s) and asks again
    /// until <see cref="LobbiesFullBudget"/> has passed on the clock since the first ask (each ask's own time counts), at most
    /// <see cref="MaxJoinsPerAsk"/> joins in all (<paramref name="joins"/> already sent; the <c>mm.join</c> bucket is 6 a minute), then
    /// says "All Infinity lobbies are full"; never past <see cref="FreeRoamAskBudget"/> from <paramref name="started"/>. A rate-limited
    /// answer stops at once. 409 <c>pool</c> (a flagged caller) starts OD-149's anti-cheat move. Any other refusal says "Couldn't join
    /// an Infinity lobby".
    /// </summary>
    private async Task<byte> QuickJoinAsync(byte kind, long started, int joins, CancellationToken cancellation)
    {
        var placement = PlacementOf(kind);
        TimeSpan? firstAsk = null;
        var full = false;
        var unavailableRetried = false;
        while (true)
        {
            if (Volatile.Read(ref _joinAttempt) != 0) return Decided(kind, FreeRoamResult.None, "no world (the player's own join is in flight)");
            if (!OnlineOpen) return Decided(kind, FreeRoamResult.NotOnline, "no world (online is closed)");
            cancellation.ThrowIfCancellationRequested(); // the hook's wait is bounded: no join past it
            firstAsk ??= _clock.GetElapsedTime(started);
            Volatile.Write(ref _lobbiesFullAfter, -1);
            Volatile.Write(ref _unavailableAfter, -1);
            joins++;
            var joined = await JoinAsync(new UiFrame.JoinWorld(string.Empty), placement, cancellation, kind).ConfigureAwait(false);
            if (joined.Result == UiJoin.Reserved)
            {
                var world = Volatile.Read(ref _seatWorld);
                if (world is not null)
                {
                    Volatile.Write(ref _seatOfficial, world);
                    lock (_mineState) _seatActivity = (world, placement);
                }
                return Decided(kind, FreeRoamResult.Ready, $"joined {world ?? "an official world"} (quick join {placement})");
            }
            if (Interlocked.Exchange(ref _lobbiesFullAfter, -1) is var ticks and >= 0)
            {
                full = true;
                var after = TimeSpan.FromTicks(Math.Clamp(ticks, 2 * TimeSpan.TicksPerSecond, 5 * TimeSpan.TicksPerSecond));
                var now = _clock.GetElapsedTime(started);
                if (joins >= MaxJoinsPerAsk || now + after - firstAsk.Value > LobbiesFullBudget || now + after > FreeRoamAskBudget)
                {
                    _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text("worlds.lobbies_full")));
                    return Decided(kind, FreeRoamResult.None, $"no world (every official {placement} world was full for {(int)(now - firstAsk.Value).TotalSeconds} s)");
                }
                _log($"worlds: {KindName(kind)}: every official {placement} world is full; asking again in {(int)after.TotalSeconds} s");
                await Task.Delay(after, _clock, cancellation).ConfigureAwait(false);
                continue;
            }
            if (Interlocked.Exchange(ref _unavailableAfter, -1) is var unavailable and >= 0)
            {
                // CR-S100: no spare after the server's own wait (503, Retry-After 15). One more ask after Retry-After, only when it still
                // fits the hook's wait (the hook gives up at 18 s, so it never does for a 15 s Retry-After); else the message
                var wait = TimeSpan.FromTicks(Math.Clamp(unavailable, TimeSpan.TicksPerSecond, 30 * TimeSpan.TicksPerSecond));
                if (!unavailableRetried && _clock.GetElapsedTime(started) + wait + TimeSpan.FromSeconds(10) <= FreeRoamAskBudget)
                {
                    unavailableRetried = true;
                    _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 2, 60, _text("worlds.waiting_free_world")));
                    await Task.Delay(wait, _clock, cancellation).ConfigureAwait(false);
                    continue;
                }
                _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text("worlds.no_free_world")));
                return Decided(kind, FreeRoamResult.None, $"no world (no free official {placement} world after the server's wait)");
            }
            if (joined.Result == UiJoin.SlowDown && Volatile.Read(ref _joinAttempt) != 0)
                return Decided(kind, FreeRoamResult.None, "no world (the player's own join is in flight)");
            if (LastJoinRefusal == ConflictReasons.Pool && PoolApplies)
            {
                // OD-149: the server flagged this caller (409 pool): the anti-cheat move asks again (the server places it)
                _freeRoamToasts.Enqueue(PoolToast());
                StartPoolMove($"a quick join of {placement} answered pool");
                return Decided(kind, FreeRoamResult.None, $"no world (quick join {placement}: pool; the anti-cheat move takes over)");
            }
            // a rate-limited answer (429, or the join circuit's wait) stops at once: never spin against the mm.join bucket
            if (joined.Result == UiJoin.SlowDown && full && OnlineOpen)
            {
                _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text("worlds.lobbies_full")));
                return Decided(kind, FreeRoamResult.None, $"no world (quick join {placement} rate limited after full lobbies)");
            }
            if (OnlineOpen) PlacementFailedToast();
            return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, $"no world (quick join {placement} refused: {joined.Result})");
        }
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

    // the activity of the seat's official world as last read (_mineState only); null: none read for this seat yet
    private (string WorldId, string Activity)? _seatActivity;

    // CR-S100 follow-up: the game session the last seat read carried (Seat.session) and the world it belongs to
    private sealed record SeatSessionRef(string WorldId, GameSessionReference Session);
    private SeatSessionRef? _seatSession;

    /// <summary>The game session of the seat in <paramref name="worldId"/> as the server last said it (<c>Seat.session</c>); null: none.</summary>
    internal GameSessionReference? SeatSessionOf(string worldId) =>
        Volatile.Read(ref _seatSession) is { } held && held.WorldId == worldId ? held.Session : null;

    /// <summary>
    /// OD-190: records the activity of the seat's official world (its one activity for life: a later ask for another moves the
    /// seat) and its private class. Called under <c>_mineState</c> with each seat read and renewal.
    /// </summary>
    private void ObserveActivity(Seat seat)
    {
        if (seat.World.Id != seat.WorldId) { _seatActivity = null; return; }
        if (seat.Session is { } session) Volatile.Write(ref _seatSession, new SeatSessionRef(seat.WorldId, session));
        ObserveInviteHolds(seat.WorldId, seat.World); // R-1108-I2: no seat held for an invite: its "Invited" mark goes
        ObservePrivate(seat.WorldId, seat.World); // T-L6: the private official seat class
        if (seat.World.Official == true) Volatile.Write(ref _seatOfficial, seat.WorldId);
        var activity = seat.World.Activity;
        if (!WorldActivities.Placement.Contains(activity)) return; // a value this launcher does not know
        _seatActivity = (seat.WorldId, activity);
    }

    // the FollowHost (0x23) move queued for the hook, not sent yet (OD-149: the anti-cheat move's free roam; the latest wins)
    private Session.Frame.FollowHost? _followHost;

    /// <summary>Writes <see cref="Session.Frame.FollowHost"/> to the hook (the supervisor's control pipe).</summary>
    public Func<Session.Frame.FollowHost, CancellationToken, Task>? SendFollowHost { get; set; }

    /// <summary>
    /// Sends the queued <see cref="Session.Frame.FollowHost"/> move (the game starts its own matchmaking for that lobby, which lands
    /// it in the new seat), once. Taken whether or not it can be written: a move is never replayed later (a closed or suspended
    /// session, or no hook writer, drops it with a log line).
    /// </summary>
    public async Task FollowHostAsync(CancellationToken cancellation)
    {
        if (Interlocked.Exchange(ref _followHost, null) is not { } follow) return;
        var name = follow.LobbyType == Session.FollowHostLobby.RoadTrip ? "Road Trip" : "free roam";
        if (!OnlineOpen || SendFollowHost is null) { _log($"worlds: a {name} move is queued; online is closed here, the game is not asked to move"); return; }
        try
        {
            await SendFollowHost(follow, cancellation).ConfigureAwait(false);
            _log($"worlds: the game is asked to look for {name} (FollowHost, the player confirms)");
        }
        catch (Exception error) when (error is IOException or InvalidOperationException or ProtocolException or ObjectDisposedException)
        { _log("worlds: FollowHost not sent (" + error.GetType().Name + ")"); }
    }

    // OD-150: the activity a lobby ask joins by and opens with; a car meet does not change it, so it asks for none
    private static string? ActivityOf(byte kind) => kind switch
    {
        FreeRoamKind.FreeRoam or FreeRoamKind.PrivateFreeRoam => WorldActivities.FreeRoam,
        FreeRoamKind.RoadTrip or FreeRoamKind.PrivateRoadTrip => WorldActivities.RoadTrip,
        _ => null,
    };
}
