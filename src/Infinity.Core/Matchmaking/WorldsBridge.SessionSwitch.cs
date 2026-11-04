// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

// W-12 (OD-196's report, F-7): a car meet invitation's session takes every member, the host included, from the car meet to free
// roam in the host's own world (MATCHMAKING_API §3.8). The games leave the car meet connection (the hook's Event 4) the moment
// the session is ready and join the session's world seconds later. FreeRoamLeftGrace's 30 s rule read that move as leaving: on
// 2026-11-04 the joiner's launcher let its seat go 30 s after its game left the car meet (17:37:33Z), the host's launcher said
// hostAway at the same second, and the server closed the world (§3.3 item 6). Now a session switch holds the world (host) or the
// seat (joiner) through the move, bounded by SessionSwitchBudget: it starts at the host's game-sessions answer (201) or at the
// hook's Event 8 (the game joins the invitation's session), and ends when the game is in the world again (Event 3), when the
// budget runs out (the 30 s rule then applies as before), or when the world or the seat ends. A game left moving toward a world
// or seat that is gone is released (CancelMatchmaking), so JOINING SESSION never waits forever.
public sealed partial class WorldsBridge
{
    /// <summary>How long the world (host) or the seat (joiner) is held for a game moving into a car meet invitation's session.</summary>
    public static readonly TimeSpan SessionSwitchBudget = TimeSpan.FromSeconds(90);

    // World: the hosted world or the seat's world the session lands in; Since: its start (timestamp); Why: for the log;
    // Moving: the game left the car meet or joins the session (Event 4 or 8): only then does an end release its matchmaking
    private sealed record SessionSwitch(string World, long Since, string Why, bool Host, bool Moving);
    private SessionSwitch? _switch;

    /// <summary>The world the pending session switch lands in, if any (tests and the log).</summary>
    internal string? SessionSwitchWorld => Volatile.Read(ref _switch)?.World;

    /// <summary>
    /// The host's launcher created the car meet invitation's session for its world (201): the host's game, when it is one of the
    /// session's members, is about to move from the car meet to free roam in this same world.
    /// </summary>
    private void HostSessionCreated(string worldId, IReadOnlyList<string> xuids)
    {
        if (_ownXuid is not null && !xuids.Contains(_ownXuid, StringComparer.Ordinal)) return; // the host's game stays in the car meet
        ArmSessionSwitch(worldId, host: true, moving: false, "the car meet session was created");
    }

    /// <summary>
    /// A joiner's game left its seat's world while that player world ran its car meet (the seat's last read): it may be moving
    /// into an invitation's session in the same world (a joiner's launcher hears of no session), so the seat is held as a switch.
    /// A player who left the car meet for solo keeps the seat at most <see cref="SessionSwitchBudget"/>; it is then let go as before.
    /// </summary>
    private void JoinerLeftCarMeet()
    {
        if (OwnWorld || _seat.GrantId is null || Volatile.Read(ref _seatWorld) is not { } world) return;
        if (Volatile.Read(ref _seatOfficial) == world) return; // an official car meet's sessions go to group worlds (§2.9, OD-191)
        // the world connection the game just closed was a car meet's: it was joined after a car meet ask answered Ready (review
        // M1: taken once, so a later leave of free roam or of the session's world never counts as a car meet's)
        if (Interlocked.Exchange(ref _joinedKind, 0) == FreeRoamKind.CarMeet + 1)
            ArmSessionSwitch(world, host: false, moving: true, "its game left the car meet");
    }

    // the kind + 1 of the last lobby ask answered Ready, until the game joins a world (0: none)
    private int _readyKind;
    // the kind + 1 of the lobby ask the game's current world connection was joined for (0: none or not known), until it leaves
    private int _joinedKind;

    /// <summary>The game joined a world (Event 3 join): its connection is the lobby the last Ready ask was for.</summary>
    private void JoinedFor() => Volatile.Write(ref _joinedKind, Interlocked.Exchange(ref _readyKind, 0));

    /// <summary>
    /// A lobby ask (FreeRoamNeed): the game is matchmaking on its own, not moving into an invitation's session, so a pending
    /// switch ends here, never later releasing that ask's matchmaking (review M2).
    /// </summary>
    private void SessionSwitchAsked()
    {
        var pending = Interlocked.Exchange(ref _switch, null);
        var expired = Interlocked.Exchange(ref _switchExpired, null);
        if ((pending ?? expired) is { } ended)
            _log($"worlds: the game asks for a lobby; the car meet session hold for {ended.World} ends");
    }

    private void ArmSessionSwitch(string world, bool host, bool moving, string why)
    {
        while (true)
        {
            var before = Volatile.Read(ref _switch);
            // the same move keeps its start (the 201 and the game's leave of one session arrive milliseconds apart); review M3: a
            // switch whose budget ran out, or a second 201 before any move, starts afresh
            var next = before is not null && before.World == world && before.Host == host
                && _clock.GetElapsedTime(before.Since) < SessionSwitchBudget && (moving || before.Moving)
                ? before with { Moving = before.Moving || moving }
                : new SessionSwitch(world, _clock.GetTimestamp(), why, host, moving);
            if (Interlocked.CompareExchange(ref _switch, next, before) != before) continue;
            if (before is null || next.Since != before.Since)
                _log($"worlds: {world} is held up to {(int)SessionSwitchBudget.TotalSeconds} s while the game moves into the car meet's session ({why})");
            return;
        }
    }

    /// <summary>The game left its world (Event 4): a pending switch for that world is now a move.</summary>
    private void SessionSwitchLeft(string? world)
    {
        while (Volatile.Read(ref _switch) is { Moving: false } pending && pending.World == world)
            if (Interlocked.CompareExchange(ref _switch, pending with { Moving = true }, pending) == pending) return;
    }

    /// <summary>
    /// How long the switch still holds <paramref name="world"/>; null: no switch for it, or its budget ran out (said once, and
    /// the switch ends: the 30 s rule applies again, and a game still moving is released when its world or seat ends).
    /// </summary>
    private TimeSpan? SessionSwitchHolds(string? world)
    {
        if (world is null || Volatile.Read(ref _switch) is not { } pending || pending.World != world) return null;
        var left = SessionSwitchBudget - _clock.GetElapsedTime(pending.Since);
        if (left > TimeSpan.Zero) return left;
        if (Interlocked.CompareExchange(ref _switch, null, pending) == pending)
        {
            _log($"worlds: the game did not reach {world} within {(int)SessionSwitchBudget.TotalSeconds} s of the car meet session "
                + $"({pending.Why}); the hold ends");
            if (pending.Moving) Volatile.Write(ref _switchExpired, pending);
        }
        return null;
    }

    // a switch whose budget ran out while its game was moving: the close or leave that follows releases its matchmaking first
    private SessionSwitch? _switchExpired;

    /// <summary>The game is in a world again (Event 3): the switch ends with how long the move took.</summary>
    private void SessionSwitchJoined()
    {
        Volatile.Write(ref _switchExpired, null);
        if (Interlocked.Exchange(ref _switch, null) is not { } done) return;
        _log($"worlds: the game reached {done.World} {(int)_clock.GetElapsedTime(done.Since).TotalSeconds} s after the car meet session ({done.Why})");
    }

    /// <summary>
    /// The world or the seat a moving game was headed for ends (<paramref name="world"/>; null: whichever): the switch ends, and a
    /// game still moving toward it (JOINING SESSION) gets the terminal release (CancelMatchmaking), never a wait on a world that is
    /// gone. True when it released.
    /// </summary>
    private async Task<bool> ReleaseSessionSwitchAsync(string? world, string why)
    {
        SessionSwitch? moving = null;
        if (Volatile.Read(ref _switch) is { } pending && (world is null || pending.World == world)
            && Interlocked.CompareExchange(ref _switch, null, pending) == pending && pending.Moving) moving = pending;
        if (Volatile.Read(ref _switchExpired) is { } expired && (world is null || expired.World == world)
            && Interlocked.CompareExchange(ref _switchExpired, null, expired) == expired) moving ??= expired;
        if (moving is null) return false;
        _log($"worlds: {why} while the game moves into its car meet session ({(int)_clock.GetElapsedTime(moving.Since).TotalSeconds} s after "
            + $"{moving.Why}); the game's matchmaking is released first");
        await CancelMatchmakingAsync().ConfigureAwait(false);
        return true;
    }

    /// <summary>A seat read or renewal says the held seat ended: a game moving into that seat's world is released (W-12 item c).</summary>
    private void SessionSwitchSeatEnded(SeatEnded? ended, string? heldGrant)
    {
        if (heldGrant is null || Volatile.Read(ref _seatWorld) is not { } world) return;
        var reason = ended is { } end && end.GrantId == heldGrant ? end.Reason : "no seat";
        if (Volatile.Read(ref _switch) is not { } pending || pending.World != world || pending.Host) return;
        // called under _mineState: the release (a pipe write) runs outside it
        _ = Task.Run(() => ReleaseSessionSwitchAsync(world, $"the seat in {world} ended ({reason})"));
    }
}
