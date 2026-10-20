// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>What the hosting launcher does next.</summary>
public enum HostStep
{
    /// <summary>Not hosting: nothing to send.</summary>
    Idle,

    /// <summary><c>PUT /mm/v1/worlds/{worldId}</c> with the settings (open, or reopen after a failed open).</summary>
    Open,

    /// <summary><c>POST /mm/v1/worlds/{worldId}/announce</c> now.</summary>
    Announce,

    /// <summary>Nothing until <see cref="HostDecision.At"/>.</summary>
    Wait,
}

/// <summary>One decision of <see cref="WorldHostScheduler"/>.</summary>
public readonly record struct HostDecision(HostStep Step, string? WorldId, DateTimeOffset? At);

/// <summary>How the scheduler read a refused open or announce.</summary>
public enum HostOutcome
{
    /// <summary>Retry later (<c>Retry-After</c>, a 5xx, or a code it does not act on): the world id is kept.</summary>
    Retry,

    /// <summary>
    /// 404 <c>MM.WORLD_NOT_FOUND</c> or 403 <c>MM.NOT_HOST</c>: the world is gone (lapsed, closed,
    /// or not ours). The scheduler is idle; the caller opens a new world with a new id
    /// (<see cref="WorldHostScheduler.NewWorldId"/>) if the player is still in free roam.
    /// </summary>
    WorldGone,

    /// <summary>
    /// 409 <c>MM.ALREADY_HOSTING {worldId}</c>: this profile already hosts
    /// <see cref="HostRefusal.ExistingWorldId"/>. The scheduler is idle; the caller either adopts it
    /// (<see cref="WorldHostScheduler.Hosting"/> with that id, then a <c>PUT</c> of its settings) or
    /// closes it (<c>DELETE</c>) and opens again.
    /// </summary>
    AlreadyHosting,

    /// <summary>A refusal the settings caused (<c>REQUEST.INVALID</c>, <c>PORTAL.VIP_REQUIRED</c>, <c>PORTAL.MODE_MISMATCH</c>, …): the scheduler is idle until the caller fixes them.</summary>
    Refused,

    /// <summary>The ticket was refused (401): refresh it and retry; the world id is kept.</summary>
    Unauthorized,
}

/// <summary>The result of <see cref="WorldHostScheduler.Failed"/>.</summary>
public readonly record struct HostRefusal(HostOutcome Outcome, string? ExistingWorldId, HostDecision Next);

/// <summary>
/// The host's heartbeat cadence (MATCHMAKING_API.md §2.5, §3.3): <c>PUT</c> once to open, then an
/// announce every 15 s (the world lapses 45 s after the last one). Pure: no timers, no I/O; every
/// call takes <c>now</c> (any monotonic-enough clock: only differences of this class's own
/// instants are compared).
/// </summary>
public sealed class WorldHostScheduler(TimeSpan? announceEvery = null)
{
    /// <summary>The contract's cadence.</summary>
    public static readonly TimeSpan DefaultAnnounceEvery = TimeSpan.FromSeconds(15);

    /// <summary>The wait after a failure the server named no wait for.</summary>
    public static readonly TimeSpan RetryDelay = TimeSpan.FromSeconds(5);

    private readonly TimeSpan _every = announceEvery ?? DefaultAnnounceEvery;
    private string? _worldId;
    private bool _open;
    private DateTimeOffset _due;

    /// <summary>The world being hosted (or about to be opened); <c>null</c> when idle.</summary>
    public string? WorldId => _worldId;

    /// <summary>The PUT succeeded: the world is open on the server.</summary>
    public bool IsOpen => _open;

    /// <summary>A client-minted world id: a UUID, stable for the world's life (the idempotency key, §2.5).</summary>
    public static string NewWorldId() => Guid.CreateVersion7().ToString();

    /// <summary>Starts hosting <paramref name="worldId"/>: the next step is its <c>PUT</c>, now.</summary>
    public HostDecision Hosting(string worldId, DateTimeOffset now)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(worldId);
        _worldId = worldId;
        _open = false;
        _due = now;
        return Next(now);
    }

    /// <summary>The <c>PUT</c> answered 201 or 200: the first announce is due 15 s from now.</summary>
    public HostDecision Opened(DateTimeOffset now) => Succeeded(now);

    /// <summary>An announce answered 200.</summary>
    public HostDecision Announced(DateTimeOffset now) => Succeeded(now);

    /// <summary>The host left free roam (<c>DELETE /mm/v1/worlds/{worldId}</c> is the caller's): idle.</summary>
    public void Stop()
    {
        _worldId = null;
        _open = false;
    }

    /// <summary>A transient failure without an answer (network, timeout): try again in 5 s.</summary>
    public HostDecision Transient(DateTimeOffset now)
    {
        _due = now + RetryDelay;
        return Next(now);
    }

    /// <summary>Reads a refused <c>PUT</c> or announce and decides what follows.</summary>
    public HostRefusal Failed(MmError error, DateTimeOffset now)
    {
        ArgumentNullException.ThrowIfNull(error);
        switch (error.Kind)
        {
            case MmErrorKind.WorldNotFound or MmErrorKind.NotHost:
                Stop();
                return new HostRefusal(HostOutcome.WorldGone, null, Next(now));
            case MmErrorKind.AlreadyHosting:
                Stop();
                return new HostRefusal(HostOutcome.AlreadyHosting, error.WorldId, Next(now));
            case MmErrorKind.RateLimited or MmErrorKind.Unavailable:
                _due = now + (error.RetryAfter ?? RetryDelay);
                return new HostRefusal(HostOutcome.Retry, null, Next(now));
            case MmErrorKind.Unauthorized:
                _due = now + RetryDelay;
                return new HostRefusal(HostOutcome.Unauthorized, null, Next(now));
            case MmErrorKind.Invalid or MmErrorKind.SupporterRequired or MmErrorKind.ModeMismatch or MmErrorKind.Conflict or MmErrorKind.WrongPool:
                Stop();
                return new HostRefusal(HostOutcome.Refused, null, Next(now));
            default:
                _due = now + RetryDelay;
                return new HostRefusal(HostOutcome.Retry, null, Next(now));
        }
    }

    /// <summary>What to do at <paramref name="now"/>.</summary>
    public HostDecision Next(DateTimeOffset now)
    {
        if (_worldId is null)
        {
            return new HostDecision(HostStep.Idle, null, null);
        }
        if (now < _due)
        {
            return new HostDecision(HostStep.Wait, _worldId, _due);
        }
        return new HostDecision(_open ? HostStep.Announce : HostStep.Open, _worldId, now);
    }

    private HostDecision Succeeded(DateTimeOffset now)
    {
        if (_worldId is null)
        {
            throw new InvalidOperationException("not hosting");
        }
        _open = true;
        _due = now + _every;
        return Next(now);
    }
}
