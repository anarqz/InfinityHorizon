// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// A lobby ask answered Ready with a seat (joined, or kept while it never played) must lead the game into that world quickly:
// it connects within seconds when there is something to connect to. A seat that has not started playing after SeatGiveUp
// is given up: released (never "left on purpose") and the game's own matchmaking is cancelled (CancelMatchmaking, 0x24),
// so FINDING SESSION never waits forever (owner: "Put a 15 s limit and give up"). Hosting is never given up by this.
public sealed partial class WorldsBridge
{
    /// <summary>How long a seat a lobby ask answered Ready has to start playing before it is given up.</summary>
    public static readonly TimeSpan SeatGiveUp = TimeSpan.FromSeconds(15);

    /// <summary>Writes <see cref="Session.Frame.CancelMatchmaking"/> to the hook (the supervisor's control pipe).</summary>
    public Func<CancellationToken, Task>? SendCancelMatchmaking { get; set; }

    private sealed record SeatTaken(string Grant, byte Kind);

    // the lobby ask kind the current grant was taken for (an overlay join: free roam)
    private SeatTaken? _seatTaken;
    // the grant that started playing (the hook's join event, or its direct/relay report); a seat that played is never given up
    private string? _seatPlayedGrant;
    // the grant whose relay the hook reported unreachable (the give-up's reason)
    private string? _unreachableGrant;
    // the grant given up or dropped unproven: no relay leg is pushed for it again
    private string? _givenUpGrant;
    private CancellationTokenSource? _giveUp;

    private void SeatTakenFor(string grant, byte kind) => Volatile.Write(ref _seatTaken, new SeatTaken(grant, kind));

    private bool Played(string grant) => string.Equals(Volatile.Read(ref _seatPlayedGrant), grant, StringComparison.Ordinal);

    private bool Hosting => HostsOrOpensWorld || Volatile.Read(ref _mineSnapshot).Role == UiWorldRole.Hosting;

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
    /// test.11: a seat that never played and was taken for another lobby ask than <paramref name="kind"/> (its grant and
    /// kind), or null: no seat, hosting, a seat that played, or one taken for this kind.
    /// </summary>
    private SeatTaken? UnprovenOtherSeat(byte kind)
    {
        if (Hosting || _seat.GrantId is not { } grant || Played(grant)) return null;
        var taken = Volatile.Read(ref _seatTaken) is { } seat && seat.Grant == grant ? seat.Kind : FreeRoamKind.FreeRoam;
        return taken == kind ? null : new SeatTaken(grant, taken);
    }

    /// <summary>
    /// A lobby ask was answered Ready: with a seat that never played (not hosting), <see cref="SeatGiveUp"/> starts again
    /// from now. A later ask, the seat playing, its end or online's end call it off.
    /// </summary>
    private void ArmGiveUp(byte kind)
    {
        if (Hosting || _seat.GrantId is not { } grant || Played(grant) || !OnlineOpen) return;
        var world = Volatile.Read(ref _seatWorld) ?? "its seat";
        CancellationTokenSource bound;
        try { bound = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token); }
        catch (ObjectDisposedException) { return; }
        CancelFreeRoamLeave(Interlocked.Exchange(ref _giveUp, bound));
        _ = GiveUpAsync(bound, grant, world, kind);
    }

    private async Task GiveUpAsync(CancellationTokenSource bound, string grant, string world, byte kind)
    {
        var due = false;
        try
        {
            await Task.Delay(SeatGiveUp, _clock, bound.Token).ConfigureAwait(false);
            due = Interlocked.CompareExchange(ref _giveUp, null, bound) == bound;
        }
        catch (OperationCanceledException) { }
        finally
        {
            Interlocked.CompareExchange(ref _giveUp, null, bound);
            bound.Dispose();
        }
        if (!due || !OnlineOpen || _seat.GrantId != grant || Played(grant) || Hosting) return;
        // a new lobby ask or the player's own join is the game's flow now: it decides (and re-arms) on its own
        if (Volatile.Read(ref _joinAttempt) != 0) return;
        lock (_freeRoamState) if (_freeRoamRun is { IsCompleted: false }) return;
        var reason = GiveUpReason(grant);
        if (!await ReleaseUnprovenSeatAsync(grant).ConfigureAwait(false)) return;
        _log($"worlds: {KindName(kind)}: gave up on {world} after {(int)SeatGiveUp.TotalSeconds} s: {reason}");
        await CancelMatchmakingAsync().ConfigureAwait(false);
    }

    private string GiveUpReason(string grant)
    {
        if (NeedsProof && !ProofInstalled) return "no proof";
        if (Volatile.Read(ref _unreachableGrant) == grant || Volatile.Read(ref _refusedGrant) == grant || Volatile.Read(ref _fallbackGrant) == grant)
            return "no route";
        return "the game never connected";
    }

    /// <summary>
    /// Lets go of the seat <paramref name="grant"/> when it is still this bridge's: not on purpose (its world may be picked
    /// again), not a recent world to go back to; its proof context and relay leg are cleared. False: another seat by now.
    /// </summary>
    private async Task<bool> ReleaseUnprovenSeatAsync(string grant)
    {
        if (_seat.GrantId != grant) return false;
        Volatile.Write(ref _givenUpGrant, grant);
        CancelFreeRoamLeave(Interlocked.Exchange(ref _giveUp, null));
        await LeaveForLobbyAsync(CancellationToken.None).ConfigureAwait(false);
        return true;
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
