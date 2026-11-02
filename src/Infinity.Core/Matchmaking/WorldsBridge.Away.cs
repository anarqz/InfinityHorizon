// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// OD-180.2 and its follow-up (CR-L49, MATCHMAKING_API.md §2.5 hostAway, §3.3 item 1, item 6 and "An away host"): a hosted world
// outlives its host's game while another player holds a reserved or seated place. The host's heartbeat says hostAway: true while
// its game is not in the world (it left free roam past OD-148's grace, or it exited or crashed); a seated guest is told once.
public sealed partial class WorldsBridge
{
    // 1 while the hosted world's heartbeats say hostAway: true
    private int _hostAway;
    private long _hostAwaySince, _hostAwayHours;
    // the other players' reserved or seated places as the last PUT or heartbeat answered (MaxPlayers − 1 − OpenSeats)
    private int _hostedOtherSeats;
    // a guest's seat: the world it last read and whether its host was away there
    private (string WorldId, bool Away)? _seatHostAway;

    /// <summary>The hosted world's heartbeats say <c>hostAway: true</c> (the host's game is not in it).</summary>
    public bool HostAway => Volatile.Read(ref _hostAway) != 0 && HostedWorldId is not null;

    // the heartbeat's field: true while away, else left out (absent means false, §2.5)
    private bool? HostAwayField => HostAway ? true : null;

    private void SetHostAway(bool away, string why)
    {
        var was = Interlocked.Exchange(ref _hostAway, away ? 1 : 0) != 0;
        if (away && !was) (_hostAwaySince, _hostAwayHours) = (_clock.GetTimestamp(), 0);
        if (was == away || HostedWorldId is not { } id) return;
        _log(away ? $"worlds: the host is away from {id} ({why}); its heartbeat says hostAway"
            : $"worlds: the host is back in {id} ({why}); hostAway dropped");
    }

    // every end or new start of a hosted world
    private void ResetHostAway()
    {
        Volatile.Write(ref _hostAway, 0);
        Volatile.Write(ref _hostedOtherSeats, 0);
    }

    private void ObserveOtherSeats(WorldView world) =>
        Volatile.Write(ref _hostedOtherSeats, (int)Math.Max(0L, (long)world.MaxPlayers - 1 - world.OpenSeats));

    /// <summary>
    /// The host's game exited or crashed (the play session's game process ended). True when the hosted world stays for the other
    /// players (OD-180.2 follow-up): it is then away until <see cref="AwayCheckAsync"/> or its heartbeat ends it, and the caller keeps
    /// the play session live (no gate leave) and the host ticking. False: no world, or nobody else in it (the caller closes as before).
    /// </summary>
    public async Task<bool> GameExitedAsync(CancellationToken cancellation)
    {
        if (HostedWorldId is not { } id || !OnlineOpen) return false;
        var others = await OtherPlayersAsync(cancellation).ConfigureAwait(false);
        if (others <= 0) return false;
        CancelFreeRoamLeave(); // the away loop weighs it from now on
        SetHostAway(true, "its game exited");
        _log($"worlds: the host's game exited; {id} stays open while {others} other player(s) remain");
        return true;
    }

    /// <summary>
    /// While the host's game is gone: true while the hosted world still has another player; else it is closed (relay stopped, one
    /// DELETE) and false. A world kept for hours says so once an hour.
    /// </summary>
    public async Task<bool> AwayCheckAsync(CancellationToken cancellation)
    {
        if (HostedWorldId is not { } id) return false;
        var others = await OtherPlayersAsync(cancellation).ConfigureAwait(false);
        if (others > 0)
        {
            if ((long)(_clock.GetElapsedTime(Interlocked.Read(ref _hostAwaySince)) / HeldWorldLogEvery) is var hours && hours > _hostAwayHours)
            {
                _hostAwayHours = hours;
                _log($"worlds: {id} kept {hours} h with no host game; {others} other player(s) remain");
            }
            return true;
        }
        _log($"worlds: the host is away and no other player is left in {id}; closing it");
        await CloseHostAsync(cancellation).ConfigureAwait(false);
        return false;
    }

    /// <summary>
    /// A guest's seat read or renewal (<c>seat.world.hostAway</c>): once per change from not away to away, a toast says the host
    /// left the session. The seat is not ended (the host's launcher still serves the world).
    /// </summary>
    private void ObserveHostAway(Seat seat)
    {
        if (seat.State == SeatStates.Hosting || seat.World.Id != seat.WorldId) { _seatHostAway = null; return; }
        var away = seat.World.HostAway == true;
        var before = _seatHostAway is { } last && last.WorldId == seat.WorldId && last.Away;
        _seatHostAway = (seat.WorldId, away);
        if (!away || before) return;
        _log($"worlds: the host of {seat.WorldId} is away; the seat stays");
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60, _text("worlds.host_away")));
    }
}
