// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.Matchmaking;

/// <summary>Successful roster snapshots only; an unavailable read is never a departure.</summary>
public sealed class LobbyRoster(string? ownXuid)
{
    private readonly object _sync = new();
    private string? _world;
    private DateTimeOffset? _generated;
    private Dictionary<string, WorldMember> _members = new(StringComparer.Ordinal);
    private readonly Queue<Change> _changes = new();

    public sealed record Change(bool Joined, string Gamertag);

    public void Reset()
    {
        lock (_sync) { _world = null; _generated = null; _members.Clear(); _changes.Clear(); }
    }

    public void Observe(string world, WorldMembers snapshot)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        if (snapshot.WorldId is not null && snapshot.WorldId != world) return;
        var stamp = DateTimeOffset.TryParse(snapshot.GeneratedAt, CultureInfo.InvariantCulture,
            DateTimeStyles.AssumeUniversal, out var generated) ? generated : (DateTimeOffset?)null;
        lock (_sync)
        {
            if (_world == world && stamp is { } time && _generated is { } last && time <= last) return;
            var members = snapshot.Items.Where(m => m.Xuid != ownXuid && m.Friend != "self")
                .DistinctBy(m => m.Xuid).ToDictionary(m => m.Xuid, StringComparer.Ordinal);
            if (_world == world)
            {
                foreach (var (id, member) in members)
                    if (!_members.ContainsKey(id)) Add(new(true, member.Gamertag));
                foreach (var (id, member) in _members)
                    if (!members.ContainsKey(id)) Add(new(false, member.Gamertag));
            }
            else _changes.Clear();
            _world = world;
            _generated = stamp;
            _members = members;
        }
    }

    public IReadOnlyList<Change> Drain()
    {
        lock (_sync) { var result = _changes.ToArray(); _changes.Clear(); return result; }
    }

    private void Add(Change change)
    {
        if (_changes.Count == 32) _changes.Dequeue();
        _changes.Enqueue(change);
    }
}
