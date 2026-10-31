// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// OD-149 (MATCHMAKING_API.md §3.9): the pineapple pool's words and link. A Hardcore profile the anti-cheat system isolated
/// plays only in pineapple worlds for a while; the launcher says so in neutral words with a link to dispute it.
/// </summary>
public static class AntiCheatPool
{
    /// <summary>Where an isolation is disputed: the launcher's community Discord (the sidebar's Discord button).</summary>
    public const string DisputeLink = "https://discord.gg/yMrC9hD99t";

    /// <summary>"1 h 23 min", "2 h" or "23 min": the time left rounded up to the minute (the same words in English and pt-BR).</summary>
    public static string TimeLeft(TimeSpan left)
    {
        var minutes = (long)Math.Ceiling(Math.Max(0, left.TotalMinutes));
        if (minutes < 1) minutes = 1;
        var (hours, rest) = (minutes / 60, minutes % 60);
        return hours == 0 ? FormattableString.Invariant($"{rest} min")
            : rest == 0 ? FormattableString.Invariant($"{hours} h")
            : FormattableString.Invariant($"{hours} h {rest} min");
    }
}

// OD-149 (§3.9): the pineapple pool. Every list marks pineapple worlds (Row: kind bit 1, lock 9 for a clean player); a clean
// player's join of one is refused with a clear text; an isolated player sees the red notice with the time left and, when its
// seat in a clean world ends or its clean world closes for the pool, is moved to the first pineapple world it can join, or
// hosts one (a world an isolated host opens is a pineapple world by the server's rule). Hardcore only: a Noob session never
// reads or acts on a pool.
public sealed partial class WorldsBridge
{
    /// <summary>How long the automatic move to a pineapple world looks for one (the free roam ask's own budget).</summary>
    public static readonly TimeSpan PoolMoveBudget = TimeSpan.FromSeconds(18);

    // the caller's isolation end (UTC ticks of the server's instant); 0: not isolated, as the last list or seat read said
    private long _poolUntilTicks;
    // the seat whose pool end was already handled (a seat read repeats `ended` until the next seat)
    private string? _poolHandledGrant;
    // the hosted world is a pineapple world (its PUT's answer): its end is never a pool move
    private bool _hostedPool;
    private int _poolMoving;
    private Task _poolMove = Task.CompletedTask;

    private bool PoolApplies => string.Equals(_mode, WorldModes.Hardcore, StringComparison.Ordinal);

    /// <summary>OD-149: the end of this Hardcore profile's isolation, while it lasts (null: not isolated, unknown, or Noob).</summary>
    public DateTimeOffset? PoolUntil
    {
        get
        {
            var ticks = Interlocked.Read(ref _poolUntilTicks);
            if (ticks == 0 || !PoolApplies) return null;
            var until = new DateTimeOffset(ticks, TimeSpan.Zero);
            return until > Now ? until : null;
        }
    }

    private bool Isolated => PoolUntil is not null;

    /// <summary>The automatic move of the last pool end (tests wait for it).</summary>
    public Task PoolMove => Volatile.Read(ref _poolMove);

    /// <summary>A list or seat read's <c>pool</c>: present sets the isolation's end, absent clears it.</summary>
    private void ObservePool(CallerPool? pool)
    {
        if (!PoolApplies) return;
        long ticks = 0;
        if (pool?.Until is { } until && DateTimeOffset.TryParse(until, CultureInfo.InvariantCulture,
            DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out var at)) ticks = at.UtcTicks;
        var before = Interlocked.Exchange(ref _poolUntilTicks, ticks);
        if (before == ticks) return;
        if (ticks == 0) _log("worlds: the anti-cheat isolation ended");
        else if (before == 0) _log("worlds: isolated by the anti-cheat system until " + new DateTimeOffset(ticks, TimeSpan.Zero).ToString("O", CultureInfo.InvariantCulture));
    }

    /// <summary>The red notice's text for the launcher's window ("Isolated by the anti-cheat system for 1 h 23 min..."), or null.</summary>
    public string? PoolNotice() => PoolUntil is { } until
        ? _text("worlds.pool_notice").Replace("{time}", AntiCheatPool.TimeLeft(until - Now), StringComparison.Ordinal)
        : null;

    /// <summary>The overlay list's status and message: <see cref="UiLobby.ListIsolated"/> with "&lt;seconds&gt;\n&lt;link&gt;" while isolated.</summary>
    private (byte Status, string Message) PoolListStatus() => PoolUntil is { } until
        ? (UiLobby.ListIsolated, ((long)Math.Ceiling((until - Now).TotalSeconds)).ToString(CultureInfo.InvariantCulture) + "\n" + AntiCheatPool.DisputeLink)
        : ((byte)0, string.Empty);

    /// <summary>
    /// A join answered 409 <c>PORTAL.CONFLICT {reason: "pool"}</c>: a clean player's join of a pineapple world, or an isolated
    /// player's join of a clean one. Answered as <see cref="UiJoin.Closed"/> (the overlay refreshes its list and shows the text;
    /// the free roam ask tries its next world). Null for any other answer.
    /// </summary>
    private UiFrame.JoinResult? PoolRefusal(MmError? error)
    {
        if (error is not { Kind: MmErrorKind.Conflict, Reason: ConflictReasons.Pool }) return null;
        _log("worlds: join refused: pool (anti-cheat isolation)");
        return new(UiJoin.Closed, _text(Isolated ? "worlds.pool_only" : "worlds.not_allowed_pool"));
    }

    /// <summary>The toast of a move to a pineapple world.</summary>
    private UiFrame.Toast PoolToast() => new(UiLevel.Warn, 2, 80, _text("worlds.pool_moved"));

    /// <summary>
    /// The seat this launcher held (<paramref name="heldGrant"/>) ended with <c>reason: "pool"</c>: the move starts (once per
    /// seat). True when it did; an older seat's end, or any other reason, is not this launcher's move.
    /// </summary>
    private bool SeatEndedForPool(SeatEnded? ended, string? heldGrant)
    {
        if (!PoolApplies || ended is not { Reason: SeatEnds.Pool } || heldGrant is null || ended.GrantId != heldGrant) return false;
        if (Interlocked.Exchange(ref _poolHandledGrant, heldGrant) == heldGrant) return false;
        StartPoolMove($"seat {heldGrant} in {ended.WorldId} ended");
        return true;
    }

    /// <summary>
    /// The server ended the hosted world for the pool: its answer names <c>pool</c>, or the host is isolated now and its world
    /// was a clean one (§3.9: a clean world an isolated host hosts closes). A pineapple world's end never is.
    /// </summary>
    private bool HostedWorldClosedForPool(MmError error) =>
        PoolApplies && !_hostedPool && (string.Equals(error.Reason, ConflictReasons.Pool, StringComparison.Ordinal) || Isolated);

    /// <summary>
    /// The move (§3.9 "on the flag"): the free roam ask's own path joins the first pineapple world this player may join (the
    /// candidates of an isolated player are pineapple worlds only) or opens one, then asks the game to look for free roam
    /// (<see cref="Session.Frame.FollowHost"/>, which lands it in the new seat or world). One move at a time.
    /// </summary>
    private void StartPoolMove(string why)
    {
        if (Interlocked.CompareExchange(ref _poolMoving, 1, 0) != 0) return;
        _log($"worlds: anti-cheat isolation ({why}); moving to a pineapple lobby");
        Volatile.Write(ref _poolMove, Task.Run(MoveToPoolAsync));
    }

    private async Task MoveToPoolAsync()
    {
        try
        {
            // the caller's own change (a seat read or renew under _mineState) is published before the ask weighs it
            lock (_mineState) Interlocked.MemoryBarrier();
            // and a host operation that started it (the announce that read the close) has ended: the world is no longer this one's
            Task hostQueue;
            lock (_hostState) hostQueue = _hostOperation;
            await hostQueue.ConfigureAwait(false);
            using var deadline = new CancellationTokenSource(PoolMoveBudget, _clock);
            using var budget = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token, deadline.Token);
            var result = await EnsureFreeRoamWorldAsync(FreeRoamKind.FreeRoam, budget.Token).ConfigureAwait(false);
            if (result == FreeRoamResult.Ready)
                Volatile.Write(ref _followHost, new Session.Frame.FollowHost(Session.FollowHostLobby.FreeRoam, Public: true));
            else _log($"worlds: no pineapple lobby joined or opened ({result})");
        }
        catch (Exception error) when (HostFailure(error) || error is ObjectDisposedException)
        {
            _log($"worlds: the move to a pineapple lobby failed ({error.GetType().Name})");
        }
        finally { Volatile.Write(ref _poolMoving, 0); }
    }
}
