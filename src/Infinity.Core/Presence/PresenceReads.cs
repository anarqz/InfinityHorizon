// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Presence;

/// <summary>
/// When the launcher reads <c>GET /launcher/v1/profiles/{xuid}/presence</c> (CR-S72, CR-L44; DISCORD_PRESENCE.md §2, §8) and which
/// livery image the card may show. Thread-safe; the clock is the caller's.
/// <list type="bullet">
/// <item>A regular read at game start and every <see cref="Every"/> (any read restarts that wait, a relaunch included).</item>
/// <item>A car change drops the previous car's image at once (a wrong livery is never shown) and asks for a read with the new
/// <c>?car=</c>: at most one such read per <see cref="CarEvery"/>, the latest car winning; only while fewer than
/// <see cref="HourlyBudget"/> − <see cref="RegularReserve"/> reads were made in the last hour, so the regular reads always fit.
/// Otherwise the read waits (the card shows the <c>infinity</c> art) and a regular read carries the car.</item>
/// <item>No read at all past <see cref="HourlyBudget"/> in the last hour or before a 429's <c>Retry-After</c>.</item>
/// <item>"Show my livery" off: no <c>?car=</c> and no car-change reads.</item>
/// </list>
/// </summary>
public sealed class PresenceReads
{
    /// <summary>The server's limit: 60 reads per account per clock hour (<c>launcher.presence</c>).</summary>
    public const int HourlyBudget = 60;

    /// <summary>Kept for the regular reads: an hour holds 12 of them.</summary>
    public const int RegularReserve = 12;

    public static readonly TimeSpan Every = TimeSpan.FromMinutes(5);
    public static readonly TimeSpan CarEvery = TimeSpan.FromSeconds(60);
    private static readonly TimeSpan Hour = TimeSpan.FromHours(1);

    private readonly object _lock = new();
    private readonly Queue<DateTimeOffset> _reads = new();
    private DateTimeOffset _lastRead = DateTimeOffset.MinValue;
    private DateTimeOffset _lastCarRead = DateTimeOffset.MinValue;
    private DateTimeOffset _notBefore = DateTimeOffset.MinValue;
    private int? _car;
    private bool _carPending;
    private (int Car, string Url)? _image;

    /// <summary>The current car's <c>Data_Car.Id</c>, or null when unknown.</summary>
    public int? Car { get { lock (_lock) return _car; } }

    /// <summary>Reads made in the hour before <paramref name="now"/>.</summary>
    public int ReadsInLastHour(DateTimeOffset now) { lock (_lock) { Trim(now); return _reads.Count; } }

    /// <summary>
    /// The hook's car changed (<c>Data_Car.Id</c>; null when unknown or the game exited): the previous car's image is dropped now and a
    /// read for the new car is wanted. False when it is the same car.
    /// </summary>
    public bool CarChanged(int? car)
    {
        if (car is <= 0) car = null;
        lock (_lock)
        {
            if (car == _car) return false;
            _car = car;
            _image = null;
            _carPending = car is not null;
            return true;
        }
    }

    /// <summary>"Show my livery" was turned on: the current car's image is read again unless it is already known.</summary>
    public void LiveryWanted()
    {
        lock (_lock) if (_car is not null && _image is null) _carPending = true;
    }

    /// <summary>
    /// Whether a read starts now and the car it sends (<paramref name="car"/>, null for none). A started read is counted at once.
    /// <paramref name="livery"/> is the "Show my livery" toggle.
    /// </summary>
    public bool TryStart(DateTimeOffset now, bool livery, out int? car)
    {
        car = null;
        lock (_lock)
        {
            Trim(now);
            if (now < _notBefore || _reads.Count >= HourlyBudget) return false;
            var regular = now - _lastRead >= Every;
            var carRead = livery && _carPending && _car is not null && now - _lastCarRead >= CarEvery
                && _reads.Count < HourlyBudget - RegularReserve;
            if (!regular && !carRead) return false;
            car = livery ? _car : null;
            _reads.Enqueue(now);
            _lastRead = now;
            if (carRead) _lastCarRead = now;
            if (car is not null) _carPending = false;
            return true;
        }
    }

    /// <summary>A read for <paramref name="car"/> answered: its image is kept only while that car is still the current one.</summary>
    public void Answered(int? car, string? image)
    {
        if (car is null) return;
        lock (_lock)
        {
            if (car != _car) return; // the car changed while the read was out: its answer is not this car's
            _image = string.IsNullOrEmpty(image) ? null : (car.Value, image);
        }
    }

    /// <summary>A read for <paramref name="car"/> failed: the current car's read is wanted again (after <see cref="CarEvery"/>).</summary>
    public void Failed(int? car)
    {
        if (car is null) return;
        lock (_lock) if (car == _car && _image is null) _carPending = true;
    }

    /// <summary>429 <c>REQUEST.RATE_LIMITED</c>: no read before <paramref name="until"/>.</summary>
    public void Throttled(DateTimeOffset until)
    {
        lock (_lock) if (until > _notBefore) _notBefore = until;
    }

    /// <summary>The image the card may show: the current car's answer, only with "Show my livery" on.</summary>
    public string? Image(bool livery)
    {
        lock (_lock) return livery && _image is { } known && known.Car == _car ? known.Url : null;
    }

    private void Trim(DateTimeOffset now)
    {
        while (_reads.Count > 0 && now - _reads.Peek() >= Hour) _reads.Dequeue();
    }
}
