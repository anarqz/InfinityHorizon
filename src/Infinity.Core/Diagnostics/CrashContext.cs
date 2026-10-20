// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Diagnostics;

using System.Collections.Immutable;

public sealed record CrashParticipant(string Xuid, string Gamertag, string Role);
public sealed record CrashObservation(DateTimeOffset At, string SenderXuid, string Kind, int? Message, int? Router, int? Value);
public sealed record CrashWorld(string WorldId, string Audience, DateTimeOffset ObservedAt, bool RosterComplete,
    IReadOnlyList<CrashParticipant> Participants);
public sealed record CrashContextSnapshot(CrashWorld? World, IReadOnlyList<CrashObservation> Observations);

/// <summary>Bounded local observations; neither membership nor a rejected packet establishes crash causality.</summary>
public sealed class CrashContext(TimeProvider? clock = null)
{
    public const int MaxObservations = 128;
    private readonly object _sync = new();
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly Queue<CrashObservation> _observations = new();
    private CrashWorld? _world;
    private CrashContextSnapshot? _frozen;
    private DateTimeOffset? _frozenAt;
    public DateTimeOffset? FrozenAt { get { lock (_sync) return _frozenAt; } }

    public void ObserveWorld(CrashWorld? world)
    {
        lock (_sync)
        {
            if (_frozen is not null) return;
            if (world is null) { _world = null; _observations.Clear(); return; }
            if (!Guid.TryParse(world.WorldId, out var worldId) || worldId == Guid.Empty || world.Audience is not ("everyone" or "supporters" or "friends" or "private")) return;
            if (_world?.WorldId != world.WorldId) _observations.Clear();
            _world = world with
            {
                Participants = world.Participants.Take(12)
                .Where(p => p.Xuid.Length is >= 1 and <= 20 && p.Xuid.All(char.IsAsciiDigit) && ulong.TryParse(p.Xuid, out var id) && id != 0)
                .DistinctBy(p => p.Xuid)
                .Select(p => p with { Gamertag = Clean(p.Gamertag, 64), Role = p.Role is "host" or "seated" or "reserved" ? p.Role : "unknown" }).ToImmutableArray()
            };
        }
    }

    public void Rejected(ulong sender, string kind, int? message = null, int? router = null, int? value = null)
    {
        if (sender == 0 || kind is not ("truncated_packet" or "invalid_payload" or "invalid_car" or "invalid_car_event" or "invalid_livery" or "game_options_overflow")) return;
        lock (_sync)
        {
            if (_frozen is not null) return;
            var now = _clock.GetUtcNow();
            var xuid = sender.ToString(System.Globalization.CultureInfo.InvariantCulture);
            // Keep work and retained metadata bounded even when a peer floods one invalid field.
            if (_observations.LastOrDefault() is { } last && last.SenderXuid == xuid && last.Kind == kind
                && last.Message == message && last.Router == router && last.Value == value && now - last.At < TimeSpan.FromSeconds(1)) return;
            while (_observations.TryPeek(out var old) && now - old.At > TimeSpan.FromSeconds(30)) _observations.Dequeue();
            if (_observations.Count == MaxObservations) _observations.Dequeue();
            _observations.Enqueue(new(now, xuid, kind, message, router, value));
        }
    }

    public CrashContextSnapshot Freeze(DateTimeOffset? occurredAt = null)
    {
        lock (_sync)
        {
            var now = occurredAt ?? _clock.GetUtcNow();
            _frozenAt ??= now;
            return _frozen ??= new(_world, _observations.Where(o => o.At <= now && now - o.At <= TimeSpan.FromSeconds(30)).ToImmutableArray());
        }
    }

    internal static string Clean(string text, int limit) => new(text.Where(c => !char.IsControl(c)).Take(limit).ToArray());
}
