// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// T-L5: one accepted invite as the inbox hands it to the bridge: its id, the private world it names, the world's activity as the
/// invite showed it (null: not said), the time the invite has left on the inbox's clock, and the private world's game session
/// (<c>session</c>, CORE_API §4.2). <see cref="Session"/> is carried for <c>gameSessionReady</c> only: T-R2 proved no consumer of it
/// (WORLDS_OFFICIAL §12.5, §12.8a, §12.9), so nothing sends it yet.
/// </summary>
public sealed record InviteAccept(string InviteId, string WorldId, string? Activity, TimeSpan Left, GameSessionReference? Session = null);

/// <summary>T-L5: what an accept did.</summary>
public enum InviteAcceptOutcome
{
    /// <summary>The seat in the private lobby is held and the game was asked in (FollowHost): the bridge watches the move.</summary>
    Moving,

    /// <summary>The server refused the join, or it failed: one notice; the invite stays answered.</summary>
    Refused,

    /// <summary>The launcher could not serve it now (an event, another lobby request, no game loaded): nothing changed; its prompt shows again.</summary>
    NotNow,

    /// <summary>The seat is already in that lobby: nothing to do.</summary>
    Already,
}

/// <summary>T-L5: an accept's outcome and its one notice (marked Always), or none.</summary>
public sealed record InviteAcceptAnswer(InviteAcceptOutcome Outcome, UiFrame.Toast? Notice);

// T-L5 (OD-204, WORLDS_OFFICIAL §12.5, §12.6, §12.8b; MATCHMAKING_API §2.10, §3.1): getting the invitee into the private lobby.
//
// Order (justified in docs/HANDOFF_RUST_ENGINE.md §11.17): accept → join → proof → FollowHost{move} → the game's ask answered Ready → the game
// connects. The join comes first: it is the only check of the friendship, the seat and the region (refusals cost nothing: the player
// stays where it is), and the server allows one live seat, so a public seat is left in that join (its locked answer) and never earlier.
// The game then moves by its own matchmaking (FollowHost, the W-10 frame), which leaves its current session itself; the hook's
// fh_already would skip a game already in online free roam, so the frame carries the move flag (Capture 13) and the hook starts it
// anyway, but never out of a car meet or a Road Trip. The launcher never releases the game's session first (CancelMatchmaking is a
// search's cancel; out of an established session it is unproven), and never yanks a player out of an event: such an accept is
// answered "Leave the event first" and its prompt shows again 30 s later.
//
// Bounded, never stranded: the game must ask for its lobby within InviteMoveBudget (60 s: the player confirms FINDING SESSION with
// (X)); then its seat has PrivateSeatGiveUp (60 s, T-L6's private connect window) to play, else the give-up releases it and cancels
// the search (CancelMatchmaking: the game is searching then). The private lobby closing before the game connects ends the move the
// same way: the search is cancelled if the game had asked, and one notice says the lobby closed. A move that never asked releases the
// seat with one notice and no cancel (no search of ours runs; a late start of the game's own lands it in a public lobby by its ask).
public sealed partial class WorldsBridge
{
    /// <summary>T-L5: how long the game has to ask for its lobby (FreeRoamNeed) after an accepted invite's FollowHost.</summary>
    public static readonly TimeSpan InviteMoveBudget = TimeSpan.FromSeconds(60);

    /// <summary>T-L5: how long the accept's join may take (the overlay's prompt is already answered meanwhile).</summary>
    public static readonly TimeSpan InviteJoinBudget = TimeSpan.FromSeconds(15);

    /// <summary>T-L5: how often a move is checked (the seat played, ended, or the game's ask is late).</summary>
    public static readonly TimeSpan InviteMoveTick = TimeSpan.FromSeconds(1);

    /// <summary>
    /// T-L5: the hook takes FollowHost's move flag (<see cref="Session.CaptureWhat.FollowMove"/>, reported ok in this session). Without
    /// it, a game already playing in a lobby is never moved: the accept says "Leave your current lobby first".
    /// </summary>
    public bool FollowMoveCapable { get; set; }

    /// <summary>
    /// W7b (OD-206, CR-S99 §4, lobby spec §11.3-11.4, route 1): the hook fires an accepted invite's session as the game's Party
    /// <c>GameSessionReady</c> (<see cref="Session.CaptureWhat.SessionReady"/>, reported ok in this session: FM5). Then an accept joins
    /// the private race first and hands the game the host's session (<see cref="SendSessionReady"/>) instead of FollowHost; the game
    /// shows its own Join popup and the player accepts it there. Off (FH2): FollowHost, unchanged.
    /// </summary>
    public bool SessionReadyCapable { get; set; }

    /// <summary>
    /// W7b: sends <see cref="Session.Frame.SessionReady"/> with the host's session and returns the hook's
    /// <see cref="Session.Frame.SessionReadyResult"/> result (<see cref="Session.SessionReadyResults"/>). Throws when it cannot be sent;
    /// cancelled at <see cref="SessionReadyBudget"/>.
    /// </summary>
    public Func<GameSessionReference, CancellationToken, Task<byte>>? SendSessionReady { get; set; }

    /// <summary>W7b: how long the hook has to answer SessionReady (it only builds the reference and invokes the game's delegate).</summary>
    public static readonly TimeSpan SessionReadyBudget = TimeSpan.FromSeconds(10);

    private int _inviteAccepting;

    // the accepted invite's move: the private world, its grant, the ask kind, when the game was asked in (timestamp), when the game asked
    // for its lobby (0 not yet), and what asked it in (FollowHost; W7b: SessionReady)
    private sealed record InviteMove(string WorldId, string Grant, byte Kind, long Since, long AskedAt = 0, string Via = "FollowHost");
    private InviteMove? _inviteMove;
    private Task _inviteWatch = Task.CompletedTask;

    /// <summary>The private world an accepted invite is moving the game into, until it plays there or the move ends (tests and the log).</summary>
    internal string? InviteMoveWorld => Volatile.Read(ref _inviteMove)?.WorldId;

    /// <summary>The running move's watch (tests wait for it).</summary>
    internal Task InviteWatch => Volatile.Read(ref _inviteWatch);

    /// <summary>
    /// T-L5: the player accepted <paramref name="invite"/> in the overlay. One accept at a time (a second one meanwhile is
    /// <see cref="InviteAcceptOutcome.NotNow"/>). Never logs the inviter, a XUID or the invite id.
    /// </summary>
    public async Task<InviteAcceptAnswer> AcceptInviteAsync(InviteAccept invite, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(invite);
        if (Interlocked.CompareExchange(ref _inviteAccepting, 1, 0) != 0) return NotNow(PrivateLobbyNotices.InviteBusy, "another accept is being answered");
        try { return await AcceptInviteCoreAsync(invite, cancellation).ConfigureAwait(false); }
        finally { Volatile.Write(ref _inviteAccepting, 0); }
    }

    private async Task<InviteAcceptAnswer> AcceptInviteCoreAsync(InviteAccept invite, CancellationToken cancellation)
    {
        var accepted = _clock.GetTimestamp();
        if (string.IsNullOrEmpty(invite.WorldId)) return Refused(PrivateLobbyNotices.InviteJoinFailed, "the invite names no world");
        // W7b (CR-S99 §4): a game that takes the invite by Party GameSessionReady (FM5) runs private races; FH2's path is unchanged
        var bySession = SessionReadyCapable;
        // a private lobby runs free roam or a Road Trip, never a car meet (MATCHMAKING_API §2.10); FM5's runs a race (CR-S99 §1)
        if (invite.Activity is { } named && (bySession ? named != WorldActivities.Race : named != WorldActivities.FreeRoam && named != WorldActivities.RoadTrip))
            return Refused(PrivateLobbyNotices.InviteJoinFailed, $"the invite's lobby runs {named}");
        var kind = invite.Activity == WorldActivities.RoadTrip ? FreeRoamKind.RoadTrip : FreeRoamKind.FreeRoam;
        if (!OnlineOpen) return Refused(PrivateLobbyNotices.InviteJoinFailed, "online is closed");
        if (invite.Left <= TimeSpan.Zero) return Refused(PrivateLobbyNotices.InviteExpired, "the invite expired before it was accepted");
        if (bySession ? SendSessionReady is null : SendFollowHost is null) return Refused(PrivateLobbyNotices.InviteJoinFailed, "no game pipe to move the game with");
        if (bySession && invite.Session is null) return Refused(PrivateLobbyNotices.InviteJoinFailed, "the invite names no game session (route 1 needs it)");
        if (OwnWorld) return NotNowText(_text("worlds.close_before_join"), "this player hosts a lobby");
        if (GameReadiness.CurrentGeneration is not { } generation) return NotNowText(_text("worlds.load_game"), "the game is not loaded");
        if (_seat.GrantId is not null && Volatile.Read(ref _seatWorld) == invite.WorldId)
        {
            _log($"worlds: invite: accepted; the seat is already in {invite.WorldId}");
            return new(InviteAcceptOutcome.Already, null);
        }
        if (Volatile.Read(ref _inviteMove) is { } moving && _seat.GrantId == moving.Grant && !Played(moving.Grant))
            return NotNow(PrivateLobbyNotices.InviteBusy, $"the game is still moving into {moving.WorldId}");
        bool asking;
        lock (_freeRoamState) asking = _freeRoamRun is { IsCompleted: false };
        if (asking || Volatile.Read(ref _joinAttempt) != 0)
            return NotNow(PrivateLobbyNotices.InviteBusy, asking ? "a lobby ask (or the game's private create) is in flight" : "a join is in flight");
        if (GameBetweenLobbies() is { } between) return NotNow(PrivateLobbyNotices.InviteBusy, between);
        if (GameInEvent() is { } inEvent) return NotNow(PrivateLobbyNotices.InviteLeaveEvent, inEvent);
        var inLobby = Volatile.Read(ref _gameInWorld) != 0;
        if (inLobby && !FollowMoveCapable && !bySession) return NotNow(PrivateLobbyNotices.InviteLeaveLobby, "the game plays in a lobby and this hook cannot move it (no Capture 13)");
        // lobby spec §11.4 "State needed": the game's Join popup shows on its main menu or hopper screen; in a lobby it waits (INFERRED)
        if (inLobby && bySession) return NotNow(PrivateLobbyNotices.InviteLeaveLobby, "the game plays in a lobby (its Join popup shows on the main menu or the hopper screen)");

        _log($"worlds: invite: accepted; joining private lobby {invite.WorldId} ({invite.Activity ?? "activity not said"}; the game {(inLobby ? "plays in a lobby" : "is in no lobby")})");
        UiFrame.JoinResult joined;
        using (var budget = new CancellationTokenSource(InviteJoinBudget, _clock))
        using (var bounded = CancellationTokenSource.CreateLinkedTokenSource(cancellation, budget.Token))
        {
            try
            {
                joined = await JoinAsync(new UiFrame.JoinWorld(invite.WorldId, string.Empty), invite.Activity, bounded.Token, kind).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (budget.IsCancellationRequested && !cancellation.IsCancellationRequested)
            {
                return Refused(PrivateLobbyNotices.InviteJoinFailed, $"no join answer within {(int)InviteJoinBudget.TotalSeconds} s");
            }
        }
        if (joined.Result == UiJoin.Reserved && Volatile.Read(ref _seatWorld) is { } world && world == invite.WorldId && _seat.GrantId is { } grant)
            return bySession
                ? await SessionReadyIntoAsync(world, grant, kind, invite.Activity ?? WorldActivities.Race, invite.Session!, generation, cancellation).ConfigureAwait(false)
                : await MoveIntoAsync(world, grant, kind, invite.Activity ?? ActivityOf(kind)!, inLobby, cancellation).ConfigureAwait(false);
        if (joined.Result == UiJoin.Reserved)
        {
            // never expected (JoinCoreAsync releases a grant of another world): the seat is not this invite's, so it is let go
            if (_seat.GrantId is { } other) await ReleaseUnprovenSeatAsync(other, "an accepted invite's join answered another world").ConfigureAwait(false);
            return Refused(PrivateLobbyNotices.InviteJoinFailed, "the join answered another world");
        }
        var refusal = Volatile.Read(ref _lastJoinRefusal);
        var expired = _clock.GetElapsedTime(accepted) >= invite.Left;
        var reason = PrivateLobbyNotices.InviteJoinRefusal(joined.Result, refusal, expired);
        // a refusal the table does not know keeps the join's own words (mods, mode, a game not loaded, a lobby of its own)
        var text = reason == PrivateLobbyNotices.InviteJoinFailed && !string.IsNullOrWhiteSpace(joined.Message) && refusal is not null
            ? joined.Message
            : PrivateLobbyNotices.Text(_text, reason, reason == ConflictReasons.Pool ? PoolTimeLeft() : null);
        // fm5-9: the join's own result and words too, so a refusal made before any request (T3: a local check) is never silent
        _log($"worlds: invite: join of {invite.WorldId} refused ({reason}{(refusal is null || refusal == reason ? string.Empty : ", " + refusal)}; " +
             $"result {joined.Result}{(string.IsNullOrWhiteSpace(joined.Message) ? string.Empty : ": " + joined.Message)})");
        return new(InviteAcceptOutcome.Refused, PrivateLobbyNotices.Toast(text));
    }

    // the seat is held: the private seat class, the move record, FollowHost{move}, and the move's watch
    private async Task<InviteAcceptAnswer> MoveIntoAsync(string world, string grant, byte kind, string activity, bool inLobby, CancellationToken cancellation)
    {
        Volatile.Write(ref _seatPrivate, world); // T-L6's private class: kept past 30 s, exempt from test.11, never quick joined out of
        Volatile.Write(ref _seatOfficial, world); // transport stays official (direct, FIUP)
        lock (_mineState) _seatActivity = (world, activity);
        Volatile.Write(ref _recentlyLeft, null);
        Volatile.Write(ref _privateLeft, null); // the player chose it (again)
        var move = new InviteMove(world, grant, kind, _clock.GetTimestamp());
        Volatile.Write(ref _inviteMove, move);
        var follow = new Session.Frame.FollowHost(kind == FreeRoamKind.RoadTrip ? Session.FollowHostLobby.RoadTrip : Session.FollowHostLobby.FreeRoam,
            Public: true, Move: FollowMoveCapable);
        try
        {
            await SendFollowHost!(follow, cancellation).ConfigureAwait(false);
        }
        catch (Exception error) when (error is IOException or InvalidOperationException or ProtocolException or ObjectDisposedException or OperationCanceledException)
        {
            Interlocked.CompareExchange(ref _inviteMove, null, move);
            await ReleaseUnprovenSeatAsync(grant, "an accepted invite's FollowHost could not be sent").ConfigureAwait(false);
            return Refused(PrivateLobbyNotices.InviteJoinFailed, $"FollowHost not sent ({error.GetType().Name}); the seat in {world} is let go");
        }
        _log($"worlds: invite: seated in private lobby {world}; the game is asked to start {(kind == FreeRoamKind.RoadTrip ? "ONLINE ROAD TRIP" : "FREE ROAM ONLINE")} there "
            + $"(FollowHost{(follow.Move ? " with the move flag" : string.Empty)}, the player confirms; {(inLobby ? "it leaves its current lobby" : "from no lobby")}; "
            + $"its ask within {(int)InviteMoveBudget.TotalSeconds} s)");
        // the watch outlives the accept's own token (the overlay's answer): only online's end stops it (InviteMoveWatchAsync)
        Volatile.Write(ref _inviteWatch, Task.Run(() => InviteMoveWatchAsync(move), CancellationToken.None));
        return new(InviteAcceptOutcome.Moving, PrivateLobbyNotices.Done(PrivateLobbyNotices.Text(_text, PrivateLobbyNotices.InviteJoining)));
    }

    // W7b (OD-206, CR-S99 §2 item 4 and §4, lobby spec §11.3-11.4, route 1): the seat is held (the join came first: the game reads
    // the session at once, 0x140be4ee0, and must find the real hostName). The readiness generation taken before the join is checked
    // again here, as JoinAsync did during and after it; then the hook fires the game's Party GameSessionReady with the host's session
    // ({scid, templateName, name} of GET /launcher/v1/invites). The game shows its own Join popup and the player accepts it there (no
    // automation); the move's watch then bounds the connect as FollowHost's does (InviteMoveBudget, no lobby ask in this route).
    private async Task<InviteAcceptAnswer> SessionReadyIntoAsync(string world, string grant, byte kind, string activity, GameSessionReference session,
        uint generation, CancellationToken cancellation)
    {
        if (!GameReadiness.Matches(generation))
        {
            await ReleaseUnprovenSeatAsync(grant, "an accepted invite's game changed during the join").ConfigureAwait(false);
            return Refused(PrivateLobbyNotices.InviteJoinFailed, $"the game's readiness changed during the join; the seat in {world} is let go");
        }
        Volatile.Write(ref _seatPrivate, world); // T-L6's private class, as MoveIntoAsync
        Volatile.Write(ref _seatOfficial, world);
        lock (_mineState) _seatActivity = (world, activity);
        Volatile.Write(ref _recentlyLeft, null);
        Volatile.Write(ref _privateLeft, null);
        var move = new InviteMove(world, grant, kind, _clock.GetTimestamp(), Via: "SessionReady");
        Volatile.Write(ref _inviteMove, move);
        string? failure = null;
        try
        {
            using var budget = new CancellationTokenSource(SessionReadyBudget, _clock);
            using var bounded = CancellationTokenSource.CreateLinkedTokenSource(cancellation, budget.Token);
            var result = await SendSessionReady!(session, bounded.Token).ConfigureAwait(false);
            if (result != Session.SessionReadyResults.Invoked) failure = $"the hook answered SessionReady with result {result}" + result switch
            {
                Session.SessionReadyResults.NoDelegate => " (the game never added its GameSessionReady delegate)",
                Session.SessionReadyResults.NoReference => " (no MultiplayerSessionReference)",
                _ => " (the game refused it)",
            };
        }
        catch (Exception error) when (error is IOException or InvalidOperationException or ProtocolException or ObjectDisposedException or OperationCanceledException)
        {
            failure = $"SessionReady not answered ({error.GetType().Name})";
        }
        if (failure is null && !GameReadiness.Matches(generation)) failure = "the game's readiness changed while SessionReady was answered";
        if (failure is not null)
        {
            Interlocked.CompareExchange(ref _inviteMove, null, move);
            await ReleaseUnprovenSeatAsync(grant, "an accepted invite's SessionReady failed").ConfigureAwait(false);
            return Refused(PrivateLobbyNotices.InviteJoinFailed, $"{failure}; the seat in {world} is let go");
        }
        _log($"worlds: invite: seated in {(activity == WorldActivities.Race ? "private race" : "private lobby")} {world}; the game was handed the host's session {session.Name} "
            + $"(SessionReady, Party GameSessionReady); the player accepts the game's own Join popup; it has {(int)InviteMoveBudget.TotalSeconds} s to connect");
        Volatile.Write(ref _inviteWatch, Task.Run(() => InviteMoveWatchAsync(move), CancellationToken.None));
        return new(InviteAcceptOutcome.Moving, PrivateLobbyNotices.Done(PrivateLobbyNotices.Text(_text, PrivateLobbyNotices.InviteJoining)));
    }

    /// <summary>
    /// T-L5: the game is in a car meet or an online Road Trip (or moving into a car meet's session): it is never pulled out of it. Null:
    /// it is in free roam, in no lobby, or its lobby is not known as an event.
    /// </summary>
    private string? GameInEvent()
    {
        if (Volatile.Read(ref _switch) is { } switching) return $"it is moving into the car meet's session in {switching.World}";
        if (Volatile.Read(ref _gameInWorld) == 0) return null;
        switch (Volatile.Read(ref _joinedKind))
        {
            case FreeRoamKind.CarMeet + 1: return "it is in a car meet";
            case FreeRoamKind.RoadTrip + 1: return "it is in an online Road Trip";
        }
        string? activity;
        lock (_mineState) activity = _seatActivity is { } seen && seen.WorldId == Volatile.Read(ref _seatWorld) ? seen.Activity : null;
        return activity is WorldActivities.CarMeet or WorldActivities.RoadTrip ? $"its lobby runs {activity}" : null;
    }

    // the game left its lobby moments ago (the 30 s wait for its next ask runs): it may be on its way into a car meet or a Road Trip
    private string? GameBetweenLobbies() => Volatile.Read(ref _freeRoamLeave) is null ? null
        : "the game left its lobby moments ago (its next lobby ask may follow)";

    /// <summary>
    /// T-L5: called with every lobby ask answered Ready: an accepted invite's move whose seat this is now has its game's ask. The give-up
    /// (<see cref="ArmGiveUp"/>) then bounds the connect at <see cref="PrivateSeatGiveUp"/>.
    /// </summary>
    private void InviteMoveAsked(byte kind)
    {
        while (Volatile.Read(ref _inviteMove) is { AskedAt: 0 } move && _seat.GrantId == move.Grant)
        {
            if (Interlocked.CompareExchange(ref _inviteMove, move with { AskedAt = _clock.GetTimestamp() }, move) != move) continue;
            _log($"worlds: invite: the game asks for its {KindName(kind)} {(int)_clock.GetElapsedTime(move.Since).TotalSeconds} s after {move.Via}; "
                + $"{move.WorldId} has {(int)PrivateSeatGiveUp.TotalSeconds} s to play");
            return;
        }
    }

    /// <summary>T-L5: the give-up's bound for <paramref name="grant"/>: an accepted invite's seat has T-L6's private connect window.</summary>
    private TimeSpan GiveUpAfter(string grant, byte kind) =>
        Volatile.Read(ref _inviteMove) is { } move && move.Grant == grant ? PrivateSeatGiveUp : GiveUpAfter(kind);

    /// <summary>
    /// T-L5: how long the free-roam-left wait still holds <paramref name="world"/> for an accepted invite's move (the game leaves its old
    /// lobby on its way in); null: no move into it, or its bounds are over (the 30 s rule applies again).
    /// </summary>
    private TimeSpan? InviteMoveHolds(string? world)
    {
        if (world is null || Volatile.Read(ref _inviteMove) is not { } move || move.WorldId != world || _seat.GrantId != move.Grant || Played(move.Grant)) return null;
        var left = InviteMoveBudget + PrivateSeatGiveUp - _clock.GetElapsedTime(move.Since);
        return left > TimeSpan.Zero ? left : null;
    }

    private async Task InviteMoveWatchAsync(InviteMove started)
    {
        try
        {
            while (true)
            {
                await Task.Delay(InviteMoveTick, _clock, _onlineStop.Token).ConfigureAwait(false);
                if (Volatile.Read(ref _inviteMove) is not { } move || move.Grant != started.Grant) return; // another accept, or ended
                if (Played(move.Grant))
                {
                    End(move, $"the game reached private lobby {move.WorldId} {(int)_clock.GetElapsedTime(move.Since).TotalSeconds} s after {move.Via}");
                    return;
                }
                if (_seat.GrantId is { } other && other != move.Grant)
                {
                    End(move, $"another seat replaced the move into {move.WorldId}"); // the player's own choice (a create, a join): nothing to cancel
                    return;
                }
                if (_seat.GrantId is null)
                {
                    await SeatGoneAsync(move).ConfigureAwait(false);
                    return;
                }
                if (move.AskedAt == 0 && _clock.GetElapsedTime(move.Since) >= InviteMoveBudget)
                {
                    if (!End(move, $"the game did not ask for its lobby within {(int)InviteMoveBudget.TotalSeconds} s of {move.Via}; the seat in {move.WorldId} is let go")) return;
                    // no CancelMatchmaking: no search of ours runs (a start the player never confirmed lands in a public lobby by its own ask)
                    await ReleaseUnprovenSeatAsync(move.Grant, "an accepted invite's game never asked for its lobby").ConfigureAwait(false);
                    _freeRoamToasts.Enqueue(PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.InviteMoveFailed));
                    return;
                }
                // the give-up owns an asked seat (PrivateSeatGiveUp, then release and CancelMatchmaking); this watch only outlives it
                if (move.AskedAt != 0 && _clock.GetElapsedTime(move.AskedAt) >= PrivateSeatGiveUp + InviteMoveBudget)
                {
                    End(move, $"the move into {move.WorldId} ends past its bounds");
                    return;
                }
            }
        }
        catch (OperationCanceledException)
        {
            Interlocked.CompareExchange(ref _inviteMove, null, started); // online closed: its own cleanup releases the seat
        }
    }

    // the move's seat ended before the game played: given up (the give-up released and cancelled), left by the player, or closed/released
    private async Task SeatGoneAsync(InviteMove move)
    {
        if (Volatile.Read(ref _givenUpGrant) == move.Grant)
        {
            if (End(move, $"the game did not connect to {move.WorldId} within {(int)PrivateSeatGiveUp.TotalSeconds} s of its ask (given up)"))
                _freeRoamToasts.Enqueue(PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.InviteMoveFailed));
            return;
        }
        if (Volatile.Read(ref _leftOnPurpose) is { } left && left.WorldId == move.WorldId)
        {
            End(move, $"the player left {move.WorldId} before the game reached it"); // its Leave released the game's search first (W-10 Q4)
            return;
        }
        if (!End(move, $"the seat in {move.WorldId} ended before the game reached it (the lobby closed or released it)")) return;
        // a game searching toward a lobby that is gone gets the terminal release, never JOINING SESSION on a world that is gone
        if (move.AskedAt != 0) await CancelMatchmakingAsync().ConfigureAwait(false);
        _freeRoamToasts.Enqueue(PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.InviteClosed));
    }

    private bool End(InviteMove move, string why)
    {
        if (Interlocked.CompareExchange(ref _inviteMove, null, move) != move) return false;
        _log("worlds: invite: " + why);
        return true;
    }

    private InviteAcceptAnswer Refused(string reason, string why)
    {
        _log($"worlds: invite: not joined ({why})");
        return new(InviteAcceptOutcome.Refused, PrivateLobbyNotices.Toast(_text, reason));
    }

    private InviteAcceptAnswer NotNow(string reason, string why) => NotNowText(PrivateLobbyNotices.Text(_text, reason), why);

    private InviteAcceptAnswer NotNowText(string text, string why)
    {
        _log($"worlds: invite: not now ({why}); nothing changed");
        return new(InviteAcceptOutcome.NotNow, PrivateLobbyNotices.Toast(text));
    }
}
