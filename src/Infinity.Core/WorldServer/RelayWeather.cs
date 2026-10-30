// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

/// <summary>
/// A world's shared weather: the server-owned WeatherSynced (router 10, id 0; 4-bit var ids). The atmosphere moves between four
/// severity levels (clear, mostly sunny, cloudy, rain), ramping one whole unit of intensity in 165 s, then holds 90-180 s before the
/// next target. It rains only while the atmosphere is above 0.603. Each change gives both the current and desired values with their
/// network-clock ticks, so every game interpolates the same ramp itself. Random choices come from a seed, so a world is reproducible.
/// </summary>
internal sealed class RelayWeather
{
    internal static readonly double[] Levels = [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0];
    internal const long RampMsPerUnit = 165000;
    internal const double RainStart = 0.603;
    internal const long HoldMinMs = 90000, HoldMaxMs = 180000;
    // odds of the next level (the current one is excluded and the rest renormalised); clear is only a starting state
    private static readonly (int Level, double Weight)[] Next = [(1, 0.45), (2, 0.35), (3, 0.20)];

    private readonly Random _random;
    internal int Level { get; private set; }
    internal float AtmosphereCurrent { get; private set; }
    internal uint AtmosphereCurrentTick { get; private set; }
    internal float AtmosphereDesired { get; private set; }
    internal uint AtmosphereDesiredTick { get; private set; }
    internal float RainCurrent { get; private set; }
    internal uint RainCurrentTick { get; private set; }
    internal float RainDesired { get; private set; }
    internal uint RainDesiredTick { get; private set; }
    internal uint[] Presets { get; private set; } = [];
    /// <summary>The engine time (ms) of the next change.</summary>
    internal long NextChangeAt { get; private set; }

    internal RelayWeather(int seed, long now, uint tick)
    {
        _random = new Random(seed);
        Level = Pick(-1);
        AtmosphereCurrent = AtmosphereDesired = (float)Levels[Level];
        RainCurrent = RainDesired = Level == 3 ? 1f : 0f;
        AtmosphereCurrentTick = AtmosphereDesiredTick = RainCurrentTick = RainDesiredTick = tick;
        Presets = PresetsFor();
        NextChangeAt = now + Hold();
    }

    private int Pick(int current)
    {
        var choices = Next.Where(c => c.Level != current).ToArray();
        var roll = _random.NextDouble() * choices.Sum(c => c.Weight);
        foreach (var (level, weight) in choices) { if (roll < weight) return level; roll -= weight; }
        return choices[^1].Level;
    }

    // preset index per severity level: clear 0, mostly sunny 1, cloudy 3 or 5, rain 7
    private uint[] PresetsFor() => [0, 1, _random.Next(2) == 0 ? 3u : 5u, 7];

    private long Hold() => HoldMinMs + (long)(_random.NextDouble() * (HoldMaxMs - HoldMinMs));

    /// <summary>Where a ramp from (from, fromTick) to (to, toTick) is at <paramref name="tick"/>.</summary>
    private static float At(float from, uint fromTick, float to, uint toTick, uint tick)
    {
        var span = unchecked(toTick - fromTick);
        var done = unchecked(tick - fromTick);
        return span == 0 || done >= span ? to : from + (to - from) * done / span;
    }

    /// <summary>A new target when its hold is over: true when the weather changed (the caller sends the update).</summary>
    internal bool Tick(long now, uint tick)
    {
        if (now < NextChangeAt) return false;
        var current = At(AtmosphereCurrent, AtmosphereCurrentTick, AtmosphereDesired, AtmosphereDesiredTick, tick);
        var rain = At(RainCurrent, RainCurrentTick, RainDesired, RainDesiredTick, tick);
        Level = Pick(Level);
        var target = (float)Levels[Level];
        var rampMs = (long)(Math.Abs(target - current) * RampMsPerUnit);
        var atmosphereEnd = unchecked(tick + (uint)rampMs);
        var rainTarget = Level == 3 ? 1f : 0f;
        uint rainStart = tick, rainEnd = tick;
        if (rainTarget != rain)
        {
            if (rainTarget > rain)
            {
                // rain begins once the atmosphere passes the threshold and is full when it arrives
                rainStart = unchecked(tick + (uint)Math.Max(0, (RainStart - current) * RampMsPerUnit));
                rainEnd = atmosphereEnd;
            }
            else rainEnd = unchecked(tick + (uint)Math.Max(1000, (current - RainStart) * RampMsPerUnit));
        }
        AtmosphereCurrent = current; AtmosphereCurrentTick = tick; AtmosphereDesired = target; AtmosphereDesiredTick = atmosphereEnd;
        RainCurrent = rain; RainCurrentTick = rainStart; RainDesired = rainTarget; RainDesiredTick = rainEnd;
        Presets = PresetsFor();
        NextChangeAt = now + rampMs + Hold();
        return true;
    }

    /// <summary>All WeatherSynced variables: 0-3 atmosphere (current, tick, desired, tick), 4 the four preset indexes, 5-8 rain.</summary>
    internal BitBuffer Payload()
    {
        var output = new BitBuffer();
        void Float(byte variable, float value) => output.Push(variable, 4).Push(BitConverter.SingleToUInt32Bits(value), 32);
        void Tick(byte variable, uint value) => output.Push(variable, 4).Push(value, 32);
        Float(0, AtmosphereCurrent); Tick(1, AtmosphereCurrentTick); Float(2, AtmosphereDesired); Tick(3, AtmosphereDesiredTick);
        output.Push(4, 4);
        foreach (var preset in Presets) output.Push(preset, 32);
        Float(5, RainCurrent); Tick(6, RainCurrentTick); Float(7, RainDesired); Tick(8, RainDesiredTick);
        return output;
    }
}
