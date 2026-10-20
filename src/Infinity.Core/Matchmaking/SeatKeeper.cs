// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.Matchmaking;

/// <summary>What the launcher does next with its seat.</summary>
public enum SeatStep
{
    /// <summary>Nothing until <see cref="SeatDecision.At"/>.</summary>
    Wait,

    /// <summary><c>POST /mm/v1/seat/renew {grantId}</c> now.</summary>
    Renew,

    /// <summary>The seat is seated or hosting: the host's announce keeps it, no renew.</summary>
    Seated,

    /// <summary>No seat to keep (<see cref="SeatDecision.Why"/>): stop renewing.</summary>
    Stop,
}

/// <summary>One decision of <see cref="SeatKeeper"/>.</summary>
public readonly record struct SeatDecision(SeatStep Step, string? GrantId, DateTimeOffset? At, string? Why)
{
    /// <summary>No live seat.</summary>
    public const string NoSeat = "none";

    /// <summary>The reserved seat reached its <c>holdUntil</c>: a renew can no longer extend it.</summary>
    public const string HoldReached = "hold";

    /// <summary>The reserved seat passed its <c>expiresAt</c> without a renew.</summary>
    public const string Lapsed = "lapsed";

    /// <summary>The player left (<see cref="SeatKeeper.Left"/>).</summary>
    public const string LeftSeat = "left";
}

/// <summary>
/// When to renew a reserved seat (MATCHMAKING_API.md §3.2): every 20 s while it is
/// <c>reserved</c>, never past its <c>holdUntil</c>, and not at all once it is seated, hosting,
/// ended or gone. Pure: no timers, no I/O; every call takes <c>now</c>, which must be on the
/// server's clock (<c>ApiClient.ServerNow</c>), since the instants it compares are the server's.
/// </summary>
public sealed class SeatKeeper(TimeSpan? renewEvery = null)
{
    /// <summary>The contract's cadence while the player walks to FREE ROAM ONLINE.</summary>
    public static readonly TimeSpan DefaultRenewEvery = TimeSpan.FromSeconds(20);

    private readonly TimeSpan _every = renewEvery ?? DefaultRenewEvery;
    private string? _grantId;
    private string _state = SeatStates.Reserved;
    private DateTimeOffset _expiresAt;
    private DateTimeOffset _holdUntil;
    private DateTimeOffset _due;
    private string? _stopped = SeatDecision.NoSeat;

    /// <summary>The kept grant, while there is one.</summary>
    public string? GrantId => _stopped is null ? _grantId : null;

    /// <summary>After a join answered <c>granted</c>: the seat is reserved, the first renew 20 s from now.</summary>
    public SeatDecision Granted(Grant grant, DateTimeOffset now)
    {
        ArgumentNullException.ThrowIfNull(grant);
        Keep(grant.Id, SeatStates.Reserved, Instant(grant.ExpiresAt), Instant(grant.HoldUntil), now);
        return Next(now);
    }

    /// <summary>After any seat read or renew (<c>GET /mm/v1/seat</c>, <c>POST /mm/v1/seat/renew</c>).</summary>
    public SeatDecision Observe(SeatAnswer answer, DateTimeOffset now)
    {
        ArgumentNullException.ThrowIfNull(answer);
        if (answer.Seat is not { } seat)
        {
            _stopped = answer.Ended?.Reason ?? SeatDecision.NoSeat;
            return Next(now);
        }
        if (seat.State != SeatStates.Reserved)
        {
            _grantId = seat.GrantId;
            _state = seat.State;
            _stopped = null;
            return Next(now);
        }
        if (seat.GrantId is not { } grant || seat.ExpiresAt is not { } expires || seat.HoldUntil is not { } hold)
        {
            // A reserved seat always carries them (§3.2); without them there is nothing to renew.
            _stopped = SeatDecision.NoSeat;
            return Next(now);
        }
        Keep(grant, SeatStates.Reserved, Instant(expires), Instant(hold), now);
        return Next(now);
    }

    /// <summary>
    /// A renew that got no answer (a network failure, a 5xx) or a 429: try again after
    /// <paramref name="retryAfter"/> (default 5 s), still bounded by the seat's expiry and hold.
    /// </summary>
    public SeatDecision RenewFailed(DateTimeOffset now, TimeSpan? retryAfter = null)
    {
        _due = now + (retryAfter ?? RetryDelay);
        return Next(now);
    }

    /// <summary>The wait after a failed renew when the server named none.</summary>
    public static readonly TimeSpan RetryDelay = TimeSpan.FromSeconds(5);

    /// <summary>The player left (<c>DELETE /mm/v1/seat</c>): nothing more to renew.</summary>
    public void Left() => _stopped = SeatDecision.LeftSeat;

    /// <summary>What to do at <paramref name="now"/>.</summary>
    public SeatDecision Next(DateTimeOffset now)
    {
        if (_stopped is { } why)
        {
            return new SeatDecision(SeatStep.Stop, null, null, why);
        }
        if (_state != SeatStates.Reserved)
        {
            return new SeatDecision(SeatStep.Seated, _grantId, null, _state);
        }
        if (now >= _holdUntil)
        {
            return new SeatDecision(SeatStep.Stop, null, null, SeatDecision.HoldReached);
        }
        if (now >= _expiresAt)
        {
            return new SeatDecision(SeatStep.Stop, null, null, SeatDecision.Lapsed);
        }
        return now >= _due
            ? new SeatDecision(SeatStep.Renew, _grantId, now, null)
            : new SeatDecision(SeatStep.Wait, _grantId, _due, null);
    }

    private void Keep(string grantId, string state, DateTimeOffset expiresAt, DateTimeOffset holdUntil, DateTimeOffset now)
    {
        _grantId = grantId;
        _state = state;
        _expiresAt = expiresAt;
        _holdUntil = holdUntil;
        // Every 20 s, and at the latest 20 s before the expiry: a plain seat read late in the
        // seat's minute does not extend it, so it must not push the renew past it.
        var latest = expiresAt - _every;
        _due = now + _every < latest ? now + _every : (latest > now ? latest : now);
        _stopped = null;
    }

    internal static DateTimeOffset Instant(string wire) =>
        DateTimeOffset.Parse(wire, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal);
}
