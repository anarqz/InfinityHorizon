// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>The current local installation policy, independent of loaded-game readiness.</summary>
public sealed record WorldAccessDecision(bool Allowed, string MessageKey)
{
    public static WorldAccessDecision Compatible { get; } = new(true, string.Empty);
    public static WorldAccessDecision Inconclusive { get; } = new(false, "worlds.integrity_inconclusive");
}
