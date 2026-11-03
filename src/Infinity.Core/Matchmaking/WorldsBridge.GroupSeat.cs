// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

// OD-191 (WORLDS_OFFICIAL §11.4, W-5): a car meet session moves its players' seats to a new official group world. The game
// leaves the car meet (the hook's Event 4) and connects to the group world seconds later; that connection must start under the
// group seat's proof context, since a context changed under a live connection restarts the hook's proof and route and the engine
// drops the stream (live 2026-11-03 17:23: admitted at :04, re-proved at :09 after the next seat read, quiet at :24). So the seat
// is read on a short schedule after the car meet is left, the game's join waits for that read, and only then is it bound.
public sealed partial class WorldsBridge
{
    /// <summary>The longest the seat is read for the group world's seat after the game leaves an official car meet.</summary>
    public static readonly TimeSpan GroupSeatWatchLimit = TimeSpan.FromSeconds(10);

    /// <summary>
    /// The waits between those reads (7 in 10 s): the <c>mm.seat</c> bucket is 30 a minute (MATCHMAKING_API §1) and the seat's own
    /// reads (every 15 s in a world) and renewals (every 20 s) share it, so 2 a second for 10 s would not fit.
    /// </summary>
    internal static readonly TimeSpan[] GroupSeatPolls =
        [TimeSpan.FromMilliseconds(500), TimeSpan.FromMilliseconds(500), TimeSpan.FromSeconds(1), TimeSpan.FromSeconds(1),
         TimeSpan.FromSeconds(2), TimeSpan.FromSeconds(2), TimeSpan.FromSeconds(3)];

    /// <summary>How long a reserved group seat a car meet ticket never releases (the server's moving seats last as long).</summary>
    public static readonly TimeSpan GroupSeatHold = TimeSpan.FromSeconds(60);

    // the official car meet world the watch reads away from; null: no watch
    private string? _groupWatchFrom;
    private Task _groupWatch = Task.CompletedTask;
    // a join seen during the watch: the next read happens at once, and the seat is bound once the watch ends
    private TaskCompletionSource _groupNudge = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private int _joinDeferred;
    // the group seat a read took over from an official car meet seat, and when
    private sealed record GroupSeatTaken(string Grant, DateTimeOffset At);
    private GroupSeatTaken? _groupSeat;

    /// <summary>The current watch (tests wait for it).</summary>
    internal Task GroupSeatWatch => Volatile.Read(ref _groupWatch);

    /// <summary>The grant the game's join was bound to (the seat that plays).</summary>
    internal string? PlayedGrant => Volatile.Read(ref _seatPlayedGrant);

    /// <summary>The hook's proof generation: it moves only when the context's grant, world, host, peer or key changes.</summary>
    internal long ProofGeneration => Interlocked.Read(ref _proofGeneration);

    // the held seat is in an official car meet world, as the seat reads say
    private string? OfficialCarMeetSeat()
    {
        if (_seat.GrantId is null || Volatile.Read(ref _seatWorld) is not { } world || Volatile.Read(ref _seatOfficial) != world) return null;
        lock (_mineState) return _seatActivity is { } seen && seen.WorldId == world && seen.Activity == WorldActivities.CarMeet ? world : null;
    }

    /// <summary>The game left free roam (Event 4): in an official car meet, its seat is read until the group world's seat arrives.</summary>
    private void StartGroupSeatWatch(CancellationToken cancellation)
    {
        if (!OnlineOpen || OfficialCarMeetSeat() is not { } carMeet) return;
        if (Interlocked.CompareExchange(ref _groupWatchFrom, carMeet, null) is not null) return; // one watch at a time
        Volatile.Write(ref _groupNudge, new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously));
        _log($"worlds: the game left the official car meet {carMeet}; its seat is read until a group world's seat arrives (at most {(int)GroupSeatWatchLimit.TotalSeconds} s)");
        Volatile.Write(ref _groupWatch, WatchGroupSeatAsync(carMeet, cancellation));
    }

    private async Task WatchGroupSeatAsync(string carMeet, CancellationToken cancellation)
    {
        await Task.Yield();
        string? reached = null, why = "its 10 s ended";
        var reads = 0;
        var started = _clock.GetTimestamp();
        try
        {
            using var stop = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _onlineStop.Token);
            foreach (var wait in GroupSeatPolls)
            {
                var nudge = Volatile.Read(ref _groupNudge);
                using (var tick = CancellationTokenSource.CreateLinkedTokenSource(stop.Token))
                {
                    await Task.WhenAny(Task.Delay(wait, _clock, tick.Token), nudge.Task).ConfigureAwait(false);
                    tick.Cancel(); // a join's nudge leaves no timer behind
                }
                stop.Token.ThrowIfCancellationRequested();
                if (nudge.Task.IsCompleted) Interlocked.CompareExchange(ref _groupNudge, new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously), nudge);
                if (!OnlineOpen) { why = "online closed"; break; }
                reads++;
                await RefreshSeatAsync(stop.Token).ConfigureAwait(false); // installs a taken-over grant's context (ReadMineAsync)
                if (_seat.GrantId is null) { why = "the seat ended"; break; }
                if (Volatile.Read(ref _seatWorld) is { } now && now != carMeet) { reached = now; break; }
                if (_clock.GetElapsedTime(started) >= GroupSeatWatchLimit) break;
            }
        }
        catch (OperationCanceledException) { why = OnlineOpen ? "the session stopped" : "online closed"; }
        catch (Exception error) when (HostFailure(error)) { why = "a seat read failed (" + error.GetType().Name + ")"; }
        finally
        {
            Volatile.Write(ref _groupWatchFrom, null);
            _log(reached is null ? $"worlds: no group world's seat after {reads} read(s) of the car meet seat: {why}"
                : $"worlds: the car meet seat moved to {reached} after {reads} read(s); its context is installed");
            // the game's join, held while the seat moved, is bound to the seat there is now
            if (Interlocked.Exchange(ref _joinDeferred, 0) != 0)
                SeatPlays(reached is null ? "the game joined" : "the game joined its group world");
        }
    }

    /// <summary>
    /// The game joined a world (Event 3 join) while the car meet seat moves: the join is not bound to the car meet seat; the seat is
    /// read at once and bound when the watch ends. True when it was held.
    /// </summary>
    private bool HoldJoinForGroupSeat()
    {
        if (Volatile.Read(ref _groupWatchFrom) is not { } carMeet) return false;
        Volatile.Write(ref _joinDeferred, 1);
        _log($"worlds: the game joined while its seat in the car meet {carMeet} moves; its seat is read now and bound once it arrives");
        Volatile.Read(ref _groupNudge).TrySetResult();
        return true;
    }

    /// <summary>
    /// A seat read took over <paramref name="seat"/> from the seat in <paramref name="previous"/>: from an official car meet into
    /// another official world, reserved, it is that car meet's group seat (OD-191). Called under <c>_mineState</c>.
    /// </summary>
    private void ObserveGroupSeat(string? previous, Seat seat)
    {
        if (previous is null || previous == seat.WorldId || seat.GrantId is not { } grant || seat.State != SeatStates.Reserved
            || seat.World.Official != true || Volatile.Read(ref _seatOfficial) != previous
            || _seatActivity is not { Activity: WorldActivities.CarMeet } seen || seen.WorldId != previous) return;
        Volatile.Write(ref _groupSeat, new GroupSeatTaken(grant, Now));
    }

    /// <summary>
    /// W-5 hardening: a reserved group seat younger than <see cref="GroupSeatHold"/> that has not played yet is never released by a
    /// car meet ticket (its quick join would end it before the game reaches the group world).
    /// </summary>
    private bool FreshGroupSeat() => Volatile.Read(ref _groupSeat) is { } group && _seat.GrantId == group.Grant && !Played(group.Grant)
        && Volatile.Read(ref _mineSnapshot).Role == Overlay.UiWorldRole.Reserved && Now - group.At < GroupSeatHold;
}
