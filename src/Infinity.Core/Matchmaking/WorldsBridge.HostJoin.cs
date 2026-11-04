// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

// W-10: a private lobby's host must reach its own world. The world server admits the host's XUID from loopback only (no grant,
// no FIUP hello), so nothing "answers" for the host: its game has to start FREE ROAM ONLINE, whose lobby ask (FreeRoamNeed)
// this bridge answers Ready for a world of its own (StaysAtOnce), and the server's ticket then lands the game on its relay.
// Nothing started that matchmaking before: a host that created a lobby from the overlay while its game was in solo, or stuck in
// another search, never got in. Now the open world asks the game to start it (FollowHost, the guest-follow path of OD-151; the
// player confirms), waits a bounded HostJoinBudget for the game to join, and says how it ended. A world closed before the host's
// game ever joined it first releases the game's matchmaking (CancelMatchmaking), so no search is left waiting on a world that is
// gone.
public sealed partial class WorldsBridge
{
    /// <summary>How long a new hosted world waits for its host's game to join it before it says so (it stays open either way).</summary>
    public static readonly TimeSpan HostJoinBudget = TimeSpan.FromSeconds(60);

    // Said: the budget's line was written (the world still waits: a close later still releases the game's matchmaking)
    private sealed record HostJoinWait(string WorldId, long Since, bool Said = false);

    // the hosted world whose host's game was asked in and has not joined it yet; null: none (joined, closed, or the game was
    // not asked: no game in this session, or it plays in a seat's world)
    private HostJoinWait? _hostJoin;
    // the world the pending FollowHost was queued for (its own log line); null: a guest's follow of its host
    private string? _followOwnWorld;

    /// <summary>
    /// A world just opened for a session whose game is in no world: the game is asked to start FREE ROAM ONLINE (sent by the next
    /// <see cref="FollowHostAsync"/>) and its join is awaited (said after <see cref="HostJoinBudget"/>). A game that plays in a
    /// seat's world is left alone: the player moves it, and nothing is ever released for it.
    /// </summary>
    private void ArmHostJoin(string worldId)
    {
        if (SendFollowHost is null) return; // no game here (OD-109's dedicated host): nobody to admit
        if (Volatile.Read(ref _gameInWorld) != 0)
        {
            _log($"worlds: {worldId} is open; the host's game plays in another world, so it is not asked in");
            return;
        }
        Volatile.Write(ref _hostJoin, new HostJoinWait(worldId, _clock.GetTimestamp()));
        _log($"worlds: {worldId} waits for the host's own game (admitted from loopback, no grant; said after {(int)HostJoinBudget.TotalSeconds} s)");
        Volatile.Write(ref _followOwnWorld, worldId);
        Volatile.Write(ref _followHost, new Session.Frame.FollowHost(Session.FollowHostLobby.FreeRoam, Public: true));
    }

    /// <summary>The host's game joined its world (the hook's <c>host:&lt;port&gt;</c>): the wait ends with its result.</summary>
    private void HostJoined()
    {
        if (Interlocked.Exchange(ref _hostJoin, null) is not { } wait) return;
        _log($"worlds: the host's game joined its world {wait.WorldId} after {(int)_clock.GetElapsedTime(wait.Since).TotalSeconds} s (admitted from loopback)");
    }

    /// <summary>
    /// The seat loop's check: past <see cref="HostJoinBudget"/> without the host's game, it is said once. The world stays, and so
    /// does the wait: a later close still releases the game's matchmaking (F-5 closed 4 min after the open).
    /// </summary>
    public void HostJoinTick()
    {
        if (Volatile.Read(ref _hostJoin) is not { Said: false } wait || _clock.GetElapsedTime(wait.Since) < HostJoinBudget) return;
        if (Interlocked.CompareExchange(ref _hostJoin, wait with { Said = true }, wait) != wait) return;
        _log($"worlds: the host's game is not in its world {wait.WorldId} {(int)HostJoinBudget.TotalSeconds} s after it opened "
            + "(no game connection from loopback); the world stays open, FREE ROAM ONLINE takes the game there");
    }

    /// <summary>
    /// W-10 Q4: the hosted world <paramref name="worldId"/> closes. When its host's game was asked in and never joined it (and plays
    /// in no other world), the game may still be matchmaking toward it: that matchmaking is released first (CancelMatchmaking),
    /// before the relay stops and the DELETE.
    /// </summary>
    private async Task ReleaseHostJoinAsync(string? worldId)
    {
        if (Interlocked.Exchange(ref _hostJoin, null) is not { } wait) return;
        if (Interlocked.CompareExchange(ref _followOwnWorld, null, wait.WorldId) == wait.WorldId)
            Volatile.Write(ref _followHost, null); // its FollowHost, not sent yet, never takes the game to a world that is gone
        if (worldId is not null && wait.WorldId != worldId) return;
        if (Volatile.Read(ref _gameInWorld) != 0) return; // the game plays in a seat's world now: never pulled out of it
        bool asking;
        lock (_freeRoamState) asking = _freeRoamRun is { IsCompleted: false };
        _log($"worlds: closing {wait.WorldId} while the host's game is not in it ({(int)_clock.GetElapsedTime(wait.Since).TotalSeconds} s after it opened"
            + (asking ? ", a lobby ask in flight" : string.Empty) + "); the game's matchmaking is released first");
        await CancelMatchmakingAsync().ConfigureAwait(false);
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
}
