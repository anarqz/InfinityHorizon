// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>The current local installation policy, independent of loaded-game readiness.</summary>
public sealed record WorldAccessDecision(bool Allowed, string MessageKey)
{
    public static WorldAccessDecision Compatible { get; } = new(true, string.Empty);
    public static WorldAccessDecision Inconclusive { get; } = new(false, "worlds.integrity_inconclusive");

    /// <summary>OD-109: this launcher hosts a dedicated lobby, so its in-game overlay offers no lobby actions.</summary>
    public static WorldAccessDecision DedicatedRunning { get; } = new(false, "worlds.dedicated_running");
}
