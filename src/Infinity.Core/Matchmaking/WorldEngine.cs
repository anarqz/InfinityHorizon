// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>The world server a hosted world runs on.</summary>
public enum WorldEngineKind
{
    /// <summary>The in-process C# engine (<see cref="IntegratedWorldRelay"/>).</summary>
    Integrated,

    /// <summary>The Infinity Worlds child process, <c>worlds serve</c> (CR-L50, DEDICATED_SERVER.md; OD-188).</summary>
    Dedicated,
}

/// <summary>
/// Which engine hosts worlds (CR-L50 stage 2, OD-185). Never a player setting (OD-144) and never a fallback: a production build
/// always runs <see cref="ProductionEngine"/>; a development build may pick the other one for tests through
/// <see cref="Variable"/>. In dedicated mode a missing, unavailable or crashed engine makes hosting unavailable; it never
/// switches to the in-process engine.
/// </summary>
public static class WorldEngine
{
    /// <summary>
    /// The engine of every production build. D6 cutover: change this one line to <see cref="WorldEngineKind.Dedicated"/>, then
    /// delete the integrated path.
    /// </summary>
    public const WorldEngineKind ProductionEngine = WorldEngineKind.Integrated;

    /// <summary>Development builds only: <c>worlds</c> or <c>integrated</c>; anything else, or none, is <see cref="ProductionEngine"/>.</summary>
    public const string Variable = "FI_WORLD_ENGINE";

#if !INFINITY_PRODUCTION_BUILD
    /// <summary>Tests only (development builds): overrides <see cref="Variable"/> while set.</summary>
    internal static WorldEngineKind? TestOnlyOverride { get; set; }

    private static int _logged;
#endif

    /// <summary>The engine for the next world; said once to <paramref name="log"/> when it is not the release one.</summary>
    public static WorldEngineKind Selected(Action<string>? log = null)
    {
#if INFINITY_PRODUCTION_BUILD
        _ = log;
        return ProductionEngine;
#else
        var selected = TestOnlyOverride ?? Parse(Environment.GetEnvironmentVariable(Variable));
        if (selected != ProductionEngine && Interlocked.Exchange(ref _logged, 1) == 0)
            log?.Invoke($"worlds: {Variable} set (development build): worlds run on the {Name(selected)} engine");
        return selected;
#endif
    }

    /// <summary>Exactly <c>worlds</c> or <c>integrated</c> (case-insensitive, blanks trimmed); anything else is <see cref="ProductionEngine"/>.</summary>
    public static WorldEngineKind Parse(string? value) => value?.Trim().ToLowerInvariant() switch
    {
        "worlds" => WorldEngineKind.Dedicated,
        "integrated" => WorldEngineKind.Integrated,
        _ => ProductionEngine,
    };

    /// <summary>The <see cref="Variable"/> value that selects <paramref name="kind"/>.</summary>
    public static string Name(WorldEngineKind kind) => kind == WorldEngineKind.Dedicated ? "worlds" : "integrated";
}
