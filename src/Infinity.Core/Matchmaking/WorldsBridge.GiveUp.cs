// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// A lobby ask answered Ready with a seat (joined, or kept while it never played) must lead the game into that world quickly:
// it connects within seconds when there is something to connect to. A seat that has not started playing after SeatGiveUp
// is given up: released (never "left on purpose") and the game's own matchmaking is cancelled (CancelMatchmaking, 0x24),
// so FINDING SESSION never waits forever (owner: "Put a 15 s limit and give up").
public sealed partial class WorldsBridge
{
    /// <summary>How long a seat a lobby ask answered Ready has to start playing before it is given up.</summary>
    public static readonly TimeSpan SeatGiveUp = TimeSpan.FromSeconds(15);

    /// <summary>
    /// T-L6: the bound for a seat a private create (FreeRoamNeed kinds 3/4) took. After Ready the game still writes its session
    /// (host, active) and connects, a path T-R2 has not timed (it saw about 50 s before UNABLE TO JOIN SESSION with no server), and
    /// it is no matchmaking search: the seat is let go after this, with no CancelMatchmaking.
    /// </summary>
    public static readonly TimeSpan PrivateSeatGiveUp = TimeSpan.FromSeconds(60);

    /// <summary>
    /// T-1109-C: the bound for a car meet ask's seat. A car meet's ticket is never found at its POST (MATCHMAKING_API §4a
    /// <c>CarMeet_qos</c>): the game learns the match from its own ticket poll, about every 5 s, then leaves free roam, writes its
    /// target session and only then connects (live 3.0.0, 2026-11-09 00:15:36: Ready to the game's OpenConnection took 9 s). Before
    /// OD-208 the hook's proven-route report (RelayFallback DirectOk, about 1 s after Ready) marked the seat played, so 15 s never
    /// cut a car meet; that report is gone and the join is the only proof, so a car meet gets two more polls of room. Still bounded:
    /// FINDING SESSION never waits forever (the owner's rule), and the same release and CancelMatchmaking follow.
    /// </summary>
    public static readonly TimeSpan CarMeetSeatGiveUp = TimeSpan.FromSeconds(30);

    private static TimeSpan GiveUpAfter(byte kind) => kind switch
    {
        _ when FreeRoamKind.IsPrivateCreate(kind) => PrivateSeatGiveUp,
        FreeRoamKind.CarMeet => CarMeetSeatGiveUp,
        _ => SeatGiveUp,
    };

    /// <summary>Writes <see cref="Session.Frame.CancelMatchmaking"/> to the hook (the supervisor's control pipe).</summary>
    public Func<CancellationToken, Task>? SendCancelMatchmaking { get; set; }

    private sealed record SeatTaken(string Grant, byte Kind);

    // the lobby ask kind the current grant was taken for (an overlay join: free roam)
    private SeatTaken? _seatTaken;
    // the grant that started playing (the hook's join event, or its direct/relay report); a seat that played is never given up
    private string? _seatPlayedGrant;
    // the grant given up or dropped unproven: an accepted invite's move into it ends (InviteAccept)
    private string? _givenUpGrant;
    private CancellationTokenSource? _giveUp;

    private void SeatTakenFor(string grant, byte kind) => Volatile.Write(ref _seatTaken, new SeatTaken(grant, kind));

    /// <summary>
    /// T35: the seat's lobby origin for the log of a lobby ask kept in it (", from its car meet seat, played"): a car meet → free
    /// roam move in the same world says which seat it keeps; empty without a seat.
    /// </summary>
    private string SeatOrigin()
    {
        if (_seat.GrantId is not { } grant) return string.Empty;
        var kind = Volatile.Read(ref _seatTaken) is { } taken && taken.Grant == grant ? taken.Kind : FreeRoamKind.FreeRoam;
        return $", from its {KindName(kind)} seat, {(Played(grant) ? "played" : "not played yet")}";
    }

    private bool Played(string grant) => string.Equals(Volatile.Read(ref _seatPlayedGrant), grant, StringComparison.Ordinal);


    /// <summary>The seat started playing: its give-up bound is called off (for good: a seat that played is never given up).</summary>
    private void SeatPlays(string how)
    {
        if (_seat.GrantId is not { } grant || Interlocked.Exchange(ref _seatPlayedGrant, grant) == grant) return;
        if (Interlocked.Exchange(ref _giveUp, null) is { } bound)
        {
            CancelFreeRoamLeave(bound);
            _log($"worlds: the seat in {Volatile.Read(ref _seatWorld) ?? "its world"} plays ({how}); no give-up");
        }
    }

    /// <summary>
    /// T-1109-C: the game joined a world while its join is held (a group seat watch or a staged install decides which grant it binds
    /// to): the game is connecting, so the give-up is called off now; the bind marks the seat played when the hold ends. Since OD-208
    /// the hook no longer reports a proven route (RelayFallback DirectOk), so the join is the only proof and must not be lost here.
    /// </summary>
    private void JoinHeldCallsOffGiveUp()
    {
        if (Interlocked.Exchange(ref _giveUp, null) is not { } bound) return;
        CancelFreeRoamLeave(bound);
        _log($"worlds: the game joined {Volatile.Read(ref _seatWorld) ?? "its world"} while its seat is settled; no give-up");
    }

    /// <summary>
    /// test.11: a seat that never played and was taken for another lobby ask than <paramref name="kind"/> (its grant and
    /// kind), or null: no seat, a seat that played, one taken for this kind, or one this launcher did not take.
    /// </summary>
    private SeatTaken? UnprovenOtherSeat(byte kind)
    {
        if (_seat.GrantId is not { } grant || Played(grant)) return null;
        if (SeatPrivate(Volatile.Read(ref _seatWorld))) return null; // T-L6: a private lobby's seat is exempt (WORLDS_OFFICIAL §12.2 item 11)
        // OD-191: a grant this launcher did not take (a seat read's: an OD-191 group seat, an accepted request) was taken for no ask
        // of its own; its world's activity decides
        if (Volatile.Read(ref _seatTaken) is not { } seat || seat.Grant != grant) return null;
        return seat.Kind == kind ? null : seat;
    }

    /// <summary>
    /// A lobby ask was answered Ready: with a seat that never played, <see cref="SeatGiveUp"/> starts again
    /// from now. A later ask, the seat playing, its end or online's end call it off.
    /// </summary>
    private void ArmGiveUp(byte kind)
    {
        if (_seat.GrantId is not { } grant || Played(grant) || !OnlineOpen) return;
        var world = Volatile.Read(ref _seatWorld) ?? "its seat";
        CancellationTokenSource bound;
        try { bound = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token); }
        catch (ObjectDisposedException) { return; }
        CancelFreeRoamLeave(Interlocked.Exchange(ref _giveUp, bound));
        _ = GiveUpAsync(bound, grant, world, kind, GiveUpAfter(grant, kind)); // T-L5: an accepted invite's seat has the private window
    }

    private async Task GiveUpAsync(CancellationTokenSource bound, string grant, string world, byte kind, TimeSpan giveUpAfter)
    {
        var due = false;
        try
        {
            await Task.Delay(giveUpAfter, _clock, bound.Token).ConfigureAwait(false);
            due = Interlocked.CompareExchange(ref _giveUp, null, bound) == bound;
        }
        catch (OperationCanceledException) { }
        finally
        {
            Interlocked.CompareExchange(ref _giveUp, null, bound);
            bound.Dispose();
        }
        if (!due || !OnlineOpen || _seat.GrantId != grant || Played(grant)) return;
        // a new lobby ask or the player's own join is the game's flow now: it decides (and re-arms) on its own
        if (Volatile.Read(ref _joinAttempt) != 0) return;
        if (Volatile.Read(ref _joinDeferred) != 0) return; // T-1109-C: the game joined; its bind waits for the seat to settle
        lock (_freeRoamState) if (_freeRoamRun is { IsCompleted: false }) return;
        var reason = GiveUpReason(grant);
        var after = (int)giveUpAfter.TotalSeconds;
        if (!await ReleaseUnprovenSeatAsync(grant, $"the seat was given up after {after} s: {reason}").ConfigureAwait(false)) return;
        _log($"worlds: {KindName(kind)}: gave up on {world} after {after} s: {reason}");
        if (FreeRoamKind.IsPrivateCreate(kind)) return; // T-L6: the game is writing its own session, not searching
        await CancelMatchmakingAsync().ConfigureAwait(false);
    }

    private string GiveUpReason(string grant)
    {
        if (NeedsProof && !ProofInstalled) return "no proof";
        return "the game never connected";
    }

    /// <summary>
    /// Lets go of the seat <paramref name="grant"/> when it is still this bridge's: not on purpose (its world may be picked
    /// again), not a recent world to go back to; its proof context and relay leg are cleared. False: another seat by now.
    /// </summary>
    private async Task<bool> ReleaseUnprovenSeatAsync(string grant, string why)
    {
        if (_seat.GrantId != grant) return false;
        Volatile.Write(ref _givenUpGrant, grant);
        CancelFreeRoamLeave(Interlocked.Exchange(ref _giveUp, null));
        await LeaveForLobbyAsync(why, CancellationToken.None).ConfigureAwait(false);
        return true;
    }

    /// <summary>
    /// W-10 Q4: the player leaves a seat from the overlay before its game ever played in it: the game's matchmaking toward that
    /// world is released first (CancelMatchmaking), as a give-up does.
    /// </summary>
    private async Task ReleaseUnplayedSeatAsync()
    {
        if (_seat.GrantId is not { } grant || Played(grant) || Volatile.Read(ref _gameInWorld) != 0) return;
        _log($"worlds: leaving {Volatile.Read(ref _seatWorld) ?? "its seat"} before the game played in it; the game's matchmaking is released first");
        await CancelMatchmakingAsync().ConfigureAwait(false);
    }

    /// <summary>The hook cancels the game's matchmaking (CancelMatchmaking); a closed session or a failed write is only logged.</summary>
    private async Task CancelMatchmakingAsync()
    {
        if (!OnlineOpen || SendCancelMatchmaking is null) { _log("worlds: CancelMatchmaking not sent (online is closed here)"); return; }
        try
        {
            await SendCancelMatchmaking(_onlineStop.Token).ConfigureAwait(false);
            _log("worlds: the game's matchmaking is cancelled (CancelMatchmaking)");
        }
        catch (Exception error) when (error is IOException or InvalidOperationException or ProtocolException or ObjectDisposedException or OperationCanceledException)
        { _log("worlds: CancelMatchmaking not sent (" + error.GetType().Name + ")"); }
    }
}
