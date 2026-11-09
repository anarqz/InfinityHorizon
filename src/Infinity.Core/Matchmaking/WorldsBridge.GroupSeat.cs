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
        // T-1109-C: only a car meet the game was in can be left. A car meet seat the quick join reserved moments ago, which the game
        // never joined (not played here, not seated by the server), is the game LEAVING FREE ROAM on its way into it: no watch, or the
        // game's join into the car meet would be held for 10 s while the seat's 15 s give-up runs
        if (_seat.GrantId is not { } held || !Played(held) && !SeatedIn(carMeet))
        {
            _log($"worlds: the game left free roam on its way into the car meet {carMeet} (its seat never played): no group seat watch");
            return;
        }
        if (Interlocked.CompareExchange(ref _groupWatchFrom, carMeet, null) is not null) return; // one watch at a time
        Volatile.Write(ref _groupNudge, new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously));
        _log($"worlds: the game left the official car meet {carMeet}; its seat is read until a group world's seat arrives (at most {(int)GroupSeatWatchLimit.TotalSeconds} s)");
        Volatile.Write(ref _groupWatch, WatchGroupSeatAsync(carMeet, cancellation));
    }

    // the last seat read says the server seated this player in `world` (its announce lists the game: it was there)
    private bool SeatedIn(string world) => Volatile.Read(ref _mineSnapshot) is { Role: Overlay.UiWorldRole.Seated } mine && mine.WorldId == world;

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
            { SeatPlays(reached is null ? "the game joined" : "the game joined its group world"); GameInWorld(true); }
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

    // ---- W-6: a new seat in another world while the game still plays in the held seat's world ----
    //
    // Live 2026-11-03 18:54:51: the server reserved the group seat around SessionReady while the game still played in the car meet;
    // the regular seat read took it over and pushed its context, the hook's live car meet leg failed at once (direct_failed) and the
    // game never got the invitation. Such a seat is staged instead: the hook keeps its context and leg, the staged seat is kept alive
    // (renewed every 20 s), and it is installed when the game leaves its world (Event 4) or joins another (Event 3 join), however long
    // the game plays (W-7, MACBOOK's ruling: never by the server's 60 s moving window alone). The old seat is never left or renewed on purpose meanwhile (it is seated: no renewal runs).

    /// <summary>How often a staged seat is renewed (MATCHMAKING_API §3.2's cadence for a reserved seat).</summary>
    public static readonly TimeSpan StagedRenewEvery = TimeSpan.FromSeconds(20);

    private sealed record StagedSeat(SeatAnswer Answer, string Grant, string World, DateTimeOffset Since, DateTimeOffset RenewAt);
    private StagedSeat? _staged;
    // 1 from the game's bound join (Event 3 join) until it leaves its world (Event 4)
    private int _gameInWorld;
    private Task _stagedInstall = Task.CompletedTask;

    /// <summary>The staged seat's grant, if any.</summary>
    internal string? StagedGrant => Volatile.Read(ref _staged)?.Grant;

    /// <summary>The install a join started (tests wait for it).</summary>
    internal Task StagedInstall => Volatile.Read(ref _stagedInstall);

    private void GameInWorld(bool playing) => Volatile.Write(ref _gameInWorld, playing ? 1 : 0);

    /// <summary>
    /// A seat read whose seat is a new grant in another world while the game plays in the held seat's world: staged, true (the read
    /// changes nothing else), for as long as the game plays there (W-7: the server's 60 s moving window never installs it).
    /// </summary>
    private bool StageSeat(SeatAnswer value)
    {
        if (value.Seat is not { GrantId: { } grant } seat || seat.World.Id != seat.WorldId) return false;
        if (_seat.GrantId is not { } held || held == grant || Volatile.Read(ref _gameInWorld) == 0 || !Played(held)) return false;
        if (Volatile.Read(ref _seatWorld) is not { } playing || playing == seat.WorldId) return false;
        var before = Volatile.Read(ref _staged);
        var same = before is not null && before.Grant == grant;
        Volatile.Write(ref _staged, new StagedSeat(value, grant, seat.WorldId, same ? before!.Since : Now, same ? before!.RenewAt : Now + StagedRenewEvery));
        if (!same) _log($"worlds: a seat in {seat.WorldId} arrived while the game plays in {playing}; staged (the hook keeps its context and leg)");
        lock (_mineState) { _mineReconciled = true; _mineUncertain = false; }
        return true;
    }

    /// <summary>A read that does not stage (another seat, none, or the moving window ended): the staged seat is dropped, said why.</summary>
    private void DropStaged(SeatAnswer value)
    {
        if (Interlocked.Exchange(ref _staged, null) is not { } dropped) return;
        _log(value.Seat?.GrantId == dropped.Grant
            ? $"worlds: the staged seat in {dropped.World} is taken over by this read (the old seat's moving window ended, or the game left)"
            : $"worlds: the staged seat in {dropped.World} is gone ({(value.Ended is { } ended && ended.GrantId == dropped.Grant ? ended.Reason : "another seat")})");
    }

    /// <summary>
    /// Installs the staged seat now, with no read: its proof context first, then the grant, its world and activity (as a read's
    /// takeover). False with none staged, or when its context could not be written (the next read decides).
    /// </summary>
    private async Task<bool> InstallStagedAsync(string why)
    {
        if (Interlocked.Exchange(ref _staged, null) is not { Answer.Seat: { } seat } staged) return false;
        long epoch;
        lock (_mineState) epoch = Interlocked.Increment(ref _mineEpoch);
        bool sent;
        try { sent = await SendSeatProofAsync(seat, epoch, CancellationToken.None).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error)) { sent = false; }
        if (!sent) { _log($"worlds: the staged seat in {staged.World} could not be installed ({why}); the next seat read decides"); RequestReconcile(); return false; }
        lock (_mineState)
        {
            if (!OnlineOpen) return false;
            _seat.Observe(staged.Answer, Now);
            ObserveGroupSeat(Volatile.Read(ref _seatWorld), seat);
            SeatWorldIs(seat.WorldId);
            ObserveActivity(seat);
        }
        RequestReconcile(); // the overlay's world follows on the next read
        _log($"worlds: the staged seat in {staged.World} is installed ({why})");
        return true;
    }

    /// <summary>The game joined (Event 3 join) with a seat staged: it is installed first, then the join is bound to it.</summary>
    private bool JoinStaged()
    {
        if (Volatile.Read(ref _staged) is null) return false;
        Volatile.Write(ref _joinDeferred, 1);
        Volatile.Write(ref _stagedInstall, Task.Run(async () =>
        {
            await InstallStagedAsync("the game joined another world").ConfigureAwait(false);
            if (Interlocked.Exchange(ref _joinDeferred, 0) != 0) { SeatPlays("the game joined its new world"); GameInWorld(true); }
        }));
        return true;
    }

    /// <summary>The staged seat's renewal, every <see cref="StagedRenewEvery"/> (the held seat is seated: it has none).</summary>
    private async Task RenewStagedAsync(CancellationToken cancellation)
    {
        if (Volatile.Read(ref _staged) is not { } staged || Now < staged.RenewAt) return;
        try
        {
            var renewed = await OperationAsync(_renewRetry, token => _mm.RenewSeatAsync(staged.Grant, token), cancellation).ConfigureAwait(false);
            if (renewed is not { Ok: true, Value: { } answer }) return;
            if (answer.Seat?.GrantId == staged.Grant)
                Interlocked.CompareExchange(ref _staged, staged with { Answer = answer, RenewAt = Now + StagedRenewEvery }, staged);
            else if (Interlocked.CompareExchange(ref _staged, null, staged) == staged)
                _log($"worlds: the staged seat in {staged.World} ended before it was installed");
        }
        catch (Exception error) when (HostFailure(error)) { }
    }

    /// <summary>
    /// W-5 hardening: a reserved group seat younger than <see cref="GroupSeatHold"/> that has not played yet is never released by a
    /// car meet ticket (its quick join would end it before the game reaches the group world).
    /// </summary>
    private bool FreshGroupSeat() => Volatile.Read(ref _groupSeat) is { } group && _seat.GrantId == group.Grant && !Played(group.Grant)
        && Volatile.Read(ref _mineSnapshot).Role == Overlay.UiWorldRole.Reserved && Now - group.At < GroupSeatHold;
}
