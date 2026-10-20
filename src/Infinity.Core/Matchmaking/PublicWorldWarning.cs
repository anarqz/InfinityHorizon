// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>One warning per visit to a public world; Supporter worlds can also contain strangers.</summary>
public sealed class PublicWorldWarning
{
    private readonly object _sync = new();
    private string? _world;
    private bool _warned, _pending;

    /// <summary>Observe an accepted host or a confirmed seated player, never a reservation alone.</summary>
    public void Observe(string world, string audience)
    {
        if (string.IsNullOrWhiteSpace(world)) return;
        lock (_sync)
        {
            if (_world != world)
            {
                _world = world;
                _warned = false;
                _pending = false;
            }
            if (audience is not (WorldAudiences.Everyone or WorldAudiences.Supporters))
            {
                _pending = false;
                return;
            }
            if (_warned) return;
            _warned = true;
            _pending = true;
        }
    }

    public bool Take()
    {
        lock (_sync) { var pending = _pending; _pending = false; return pending; }
    }

    public void Reset()
    {
        lock (_sync) { _world = null; _warned = false; _pending = false; }
    }
}
