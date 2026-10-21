// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Integrity;

/// <summary>Monotonic local receiver capability, refreshed independently of droppable diagnostic events.</summary>
public sealed class ReceiverReadinessLease(TimeProvider? clock = null)
{
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly object _sync = new();
    private bool _connected, _ready;
    private long _observed, _connectionStamp;
    public void Connected() { lock (_sync) { _connected = true; _ready = false; _connectionStamp = _clock.GetTimestamp(); } }
    public void Disconnected() { lock (_sync) { _connected = _ready = false; } }
    public void Observe(bool ready) { lock (_sync) { if (_connected) { _ready = ready; _observed = _clock.GetTimestamp(); } } }
    public void Observe(bool ready, long receivedTimestamp)
    {
        lock (_sync)
        {
            var elapsed = _clock.GetElapsedTime(receivedTimestamp);
            if (!_connected || receivedTimestamp < _connectionStamp || elapsed < TimeSpan.Zero || elapsed >= TimeSpan.FromSeconds(10)) { _ready = false; return; }
            _ready = ready; _observed = receivedTimestamp;
        }
    }
    public bool Ready
    {
        get { lock (_sync) { var elapsed = _clock.GetElapsedTime(_observed); return _connected && _ready && elapsed >= TimeSpan.Zero && elapsed < TimeSpan.FromSeconds(10); } }
    }
}
