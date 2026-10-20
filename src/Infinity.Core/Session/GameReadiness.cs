// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.Session;

/// <summary>Game-thread readiness received on the authenticated hook pipe; absence/staleness denies world entry.</summary>
public sealed class GameReadiness(TimeProvider? clock = null)
{
    public static readonly TimeSpan MaxAge = TimeSpan.FromMilliseconds(1500);
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly object _sync = new();
    private uint _generation;
    private bool _ready;
    private long _received;

    public uint? CurrentGeneration
    {
        get
        {
            lock (_sync)
                return _ready && _clock.GetElapsedTime(_received) <= MaxAge ? _generation : null;
        }
    }

    public bool Matches(uint generation) => CurrentGeneration == generation;

    public void Invalidate()
    {
        lock (_sync) _ready = false;
    }

    /// <summary>Event 5 detail: canonical nonzero decimal u32, colon, 0/1. Older generations cannot revive a load.</summary>
    public bool Observe(string detail, long? receivedTimestamp = null)
    {
        ArgumentNullException.ThrowIfNull(detail);
        var colon = detail.IndexOf(':', StringComparison.Ordinal);
        if (colon <= 0 || colon != detail.Length - 2 || detail[^1] is not ('0' or '1')
            || !uint.TryParse(detail.AsSpan(0, colon), NumberStyles.None, CultureInfo.InvariantCulture, out var generation)
            || generation == 0 || detail[0] == '0') return false;
        lock (_sync)
        {
            if (generation < _generation) return false;
            _generation = generation;
            _ready = detail[^1] == '1';
            _received = receivedTimestamp ?? _clock.GetTimestamp();
            return true;
        }
    }
}
