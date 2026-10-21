// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>A single in-flight operation and a monotonic retry circuit; never renews a server lease.</summary>
internal sealed class WorldOperationRetry(TimeProvider clock, Func<double>? jitter = null)
{
    private readonly object _sync = new();
    private readonly DateTimeOffset _anchor = clock.GetUtcNow();
    private readonly long _stamp = clock.GetTimestamp();
    private DateTimeOffset _observed;
    private DateTimeOffset _due;
    private long _generation;
    private bool _running;
    private int _failures;

    internal bool Running { get { lock (_sync) return _running; } }

    internal bool TryBegin(out long generation)
    {
        lock (_sync)
        {
            generation = _generation;
            if (_running || Now() < _due) return false;
            _running = true;
            generation = ++_generation;
            return true;
        }
    }

    internal void Complete(long generation, bool transient, TimeSpan? retryAfter = null)
    {
        lock (_sync)
        {
            if (generation != _generation) return;
            _running = false;
            if (!transient) { _failures = 0; _due = default; return; }
            _failures = Math.Min(_failures + 1, 5);
            var sample = jitter?.Invoke() ?? Random.Shared.NextDouble();
            if (!double.IsFinite(sample)) sample = 0.5;
            var seconds = Math.Min(30, Math.Pow(2, _failures) * (0.8 + Math.Clamp(sample, 0, 1) * 0.4));
            var delay = TimeSpan.FromSeconds(seconds);
            if (retryAfter is { } server && server > delay) delay = server;
            var now = Now();
            _due = delay.Ticks > DateTimeOffset.MaxValue.UtcTicks - now.UtcTicks ? DateTimeOffset.MaxValue : now + delay;
        }
    }

    internal void Cancel(long generation)
    {
        lock (_sync) if (generation == _generation) _running = false;
    }

    private DateTimeOffset Now()
    {
        var monotonic = _anchor + clock.GetElapsedTime(_stamp);
        var wall = clock.GetUtcNow();
        if (monotonic > _observed) _observed = monotonic;
        if (wall > _observed) _observed = wall;
        return _observed;
    }
}
