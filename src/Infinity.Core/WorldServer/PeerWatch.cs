// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Numerics;
using Infinity.Core.Matchmaking;

namespace Infinity.Core.WorldServer;

/// <summary>
/// The rules one player measures the others' cars against (MATCHMAKING_API §3.9): <see cref="WatchRules"/> as read from the
/// platform, plus the test-only forced episode (<see cref="ForceSpeed"/>, AC-5; never set by a production launcher).
/// </summary>
public sealed record PeerWatchRules(uint Version, double SpeedMaxKmh, uint SpeedSustainMs, double ClimbMaxKmh, uint ClimbSustainMs,
    double JumpMaxMeters, bool ForceSpeed = false)
{
    public static PeerWatchRules From(WatchRules rules, bool forceSpeed = false)
    {
        ArgumentNullException.ThrowIfNull(rules);
        return new(rules.Version, rules.Speed.MaxKmh, rules.Speed.SustainMs, rules.Climb.MaxKmh, rules.Climb.SustainMs, rules.Jump.MaxMeters, forceSpeed);
    }
}

/// <summary>One ended episode of another player's car: what the launcher posts (<see cref="PeerObservation"/>, an id it mints).</summary>
public sealed record PeerWatchReport(ulong Xuid, string Kind, double Value, uint? SustainMs, uint RulesVersion, byte PeerIndex);

/// <summary>
/// AC-2 (OD-147, OD-158): measures other players' cars from their car predictions, never this player's own (the caller never
/// feeds it). Deterministic: every time comes from the caller. The same algorithm runs in the hook (native/hook watch.c) for a
/// joiner; docs/launcher/ANTICHEAT.md is the shared description.
/// <list type="bullet">
/// <item>A sample is a car's position (metres), its velocity (m/s, CarPredictionFull bits 96-143, SAURON V9; the scale is
/// provisional), its sender's prediction timestamp (ms on the sender's clock, to be confirmed live) and its universe, with the
/// local receive time. Y is up (to be confirmed live by the units line).</item>
/// <item>Speed (horizontal, x and z) and climb (upward, y) come from the velocity field, averaged over windows of at least
/// <see cref="WindowMs"/> of receive time, cross-checked by the position delta over the same window on both clocks: a window
/// counts as over a limit only when the velocity, the sender clock's and the receiver clock's speeds all say so (a lag burst, a
/// slowed sender clock or a forged velocity alone is not a speed). The episode's value is the smallest of the three.</item>
/// <item>Exclusions (the window restarts, the run resets): a car's first sample after (re)appearing (no sample for
/// <see cref="GapMs"/>), a universe change, a non-increasing timestamp (dropped), an event start, and any move past the jump
/// limit in one sample (fast travel, respawn, reset or a teleport), which is a <c>jump</c> episode instead.</item>
/// <item>One report per car and kind per episode, sent when it ends: <see cref="QuietMs"/> without being over the limit. A
/// speed or climb episode is reported only when one run over the limit lasted the rule's sustain; jumps always (log only
/// server-side); the clock ratio once per car per rules after <see cref="ClockMinMs"/> of measured windows (log only).</item>
/// </list>
/// Logs carry the observed peer index only: never an identity, a gamertag or an address.
/// </summary>
internal sealed class PeerWatch(PeerWatchRules rules, Action<PeerWatchReport> report, Action<string>? log = null)
{
    internal const long WindowMs = 250, GapMs = 1500, QuietMs = 2000, ClockMinMs = 30000;
    internal const int MaxCars = 32;

    /// <summary>CarPredictionFull: the velocity's three s16 start at body bit 96 (SAURON V9).</summary>
    internal const int VelocityOffset = 96;

    /// <summary>
    /// The velocity's scale: an s16 of ±32767 is ±200 m/s per axis (SAURON V9, inferred; saturates at 720 km/h per axis). A
    /// decoding constant, provisional until the AC-5 units line confirms it live; never a threshold.
    /// </summary>
    internal const double VelocityScale = 200.0 / 32767.0;

    /// <summary>The velocity (m/s) of three raw s16 axes.</summary>
    internal static Vector3 Velocity(short x, short y, short z) =>
        new((float)(x * VelocityScale), (float)(y * VelocityScale), (float)(z * VelocityScale));

    private sealed class Episode
    {
        internal bool Active;
        internal double Peak, PeakVelocity, PeakSender, PeakReceiver;
        internal long Run, MaxRun, LastOverAt, StartAt, SenderMs, ReceiverMs;
        internal int Disagreed;
    }

    private sealed class Car(byte index, ulong xuid)
    {
        internal byte Index { get; } = index;
        internal ulong Xuid { get; } = xuid;
        internal bool Has;
        internal Vector3 Position, AnchorPosition;
        internal uint Stamp, AnchorStamp;
        internal long At, AnchorAt;
        internal byte Universe;
        internal readonly Episode Speed = new(), Climb = new();
        internal bool JumpActive;
        internal double JumpPeak;
        internal long JumpLast;
        internal int Jumps;
        internal long ClockSender, ClockReceiver;
        internal bool ClockSent;
        internal double VelocityHorizontal, VelocityUp;
        internal int VelocitySamples;
        internal int First, Universes, Stale, Events;
    }

    private readonly Dictionary<byte, Car> _cars = [];
    private readonly Action<string> _log = log ?? (_ => { });
    private bool _forced, _unitsLogged;

    internal PeerWatchRules Rules => rules;

    private void Log(FormattableString line) => _log("watch: " + line.ToString(CultureInfo.InvariantCulture));

    /// <summary>A car prediction of the player at <paramref name="index"/> (<paramref name="xuid"/>), received at <paramref name="at"/> ms.</summary>
    internal void Sample(byte index, ulong xuid, Vector3 position, Vector3 velocity, uint stamp, byte universe, long at)
    {
        if (xuid == 0 || !float.IsFinite(position.X) || !float.IsFinite(position.Y) || !float.IsFinite(position.Z)
            || !float.IsFinite(velocity.X) || !float.IsFinite(velocity.Y) || !float.IsFinite(velocity.Z)) return;
        if (!_cars.TryGetValue(index, out var car) || car.Xuid != xuid)
        {
            if (car is not null) End(car, at, force: true);
            if (car is null && _cars.Count >= MaxCars) return;
            _cars[index] = car = new Car(index, xuid);
        }
        if (rules.ForceSpeed && !_forced)
        {
            // AC-5, test builds only: the next eligible peer gets one synthetic speed episode the server counts
            _forced = true;
            Log($"TEST: forced speed episode for peer {index} (FI_TEST_FORCE_OBSERVATION; never in a production build)");
            report(new(xuid, ObservationKinds.Speed, rules.SpeedMaxKmh + 50, rules.SpeedSustainMs + 1000, rules.Version, index));
        }
        EndQuiet(car, at);
        if (!car.Has || at - car.At > GapMs)
        {
            if (car.Has) Log($"peer {index}: first sample after {at - car.At} ms without one; excluded");
            car.First++;
            Restart(car, position, stamp, universe, at);
            return;
        }
        if (universe != car.Universe)
        {
            car.Universes++;
            Log($"peer {index}: universe {car.Universe} -> {universe}; excluded");
            Restart(car, position, stamp, universe, at);
            return;
        }
        var sent = unchecked((int)(stamp - car.Stamp));
        if (sent <= 0) { car.Stale++; return; }
        var moved = Vector3.Distance(position, car.Position);
        if (moved > rules.JumpMaxMeters)
        {
            Jump(car, moved, sent, at - car.At, at);
            Restart(car, position, stamp, universe, at);
            return;
        }
        (car.Position, car.Stamp, car.At) = (position, stamp, at);
        car.VelocityHorizontal += Math.Sqrt((double)velocity.X * velocity.X + (double)velocity.Z * velocity.Z);
        car.VelocityUp += Math.Max(0.0, velocity.Y); car.VelocitySamples++;
        var windowReceived = at - car.AnchorAt;
        var windowSent = unchecked((int)(stamp - car.AnchorStamp));
        if (windowReceived < WindowMs) return;
        if (windowSent <= 0) { Anchor(car); return; }
        var delta = position - car.AnchorPosition;
        var horizontal = Math.Sqrt((double)delta.X * delta.X + (double)delta.Z * delta.Z);
        var up = Math.Max(0.0, delta.Y);
        // metres per millisecond x 3600 = km/h
        double speedSent = horizontal / windowSent * 3600, speedReceived = horizontal / windowReceived * 3600;
        double climbSent = up / windowSent * 3600, climbReceived = up / windowReceived * 3600;
        // m/s x 3.6 = km/h
        var samples = Math.Max(1, car.VelocitySamples);
        double speedVelocity = car.VelocityHorizontal / samples * 3.6, climbVelocity = car.VelocityUp / samples * 3.6;
        if (!_unitsLogged)
        {
            // AC-5: one line per session confirms m/s and Y up (velocity vs position delta) and the stamp unit (sender vs receiver ms)
            _unitsLogged = true;
            Log($"peer {index}: units check: velocity {speedVelocity:0} km/h horizontal, {climbVelocity:0} km/h up; position delta {horizontal:0.0} m horizontal, {delta.Y:0.0} m vertical: {speedSent:0} km/h on {windowSent} stamp units (sender), {speedReceived:0} km/h on {windowReceived} ms (receiver)");
        }
        car.ClockSender += windowSent; car.ClockReceiver += windowReceived;
        Step(car, car.Speed, ObservationKinds.Speed, speedVelocity, speedSent, speedReceived, rules.SpeedMaxKmh, windowSent, windowReceived, at);
        Step(car, car.Climb, ObservationKinds.Climb, climbVelocity, climbSent, climbReceived, rules.ClimbMaxKmh, windowSent, windowReceived, at);
        Anchor(car);
        Clock(car);
    }

    /// <summary>An event starts (participants selected, a starting grid, a race start): every car restarts its measurement.</summary>
    internal void Exclude(string why)
    {
        foreach (var car in _cars.Values)
        {
            car.Has = false; car.Events++;
            car.Speed.Run = 0; car.Climb.Run = 0;
        }
        if (_cars.Count != 0) Log($"{why}: every car's measurement restarts; excluded");
    }

    /// <summary>The player at <paramref name="index"/> left: its episodes end now (a qualifying one is reported).</summary>
    internal void Forget(byte index, long at)
    {
        if (!_cars.Remove(index, out var car)) return;
        End(car, at, force: true);
    }

    /// <summary>Time passes without samples: episodes whose car stayed quiet for <see cref="QuietMs"/> end.</summary>
    internal void Tick(long at)
    {
        foreach (var car in _cars.Values) EndQuiet(car, at);
    }

    /// <summary>The rules end (another world, a clear, the world closed): every episode ends now.</summary>
    internal void Flush(long at)
    {
        foreach (var car in _cars.Values) End(car, at, force: true);
        _cars.Clear();
    }

    private static void Restart(Car car, Vector3 position, uint stamp, byte universe, long at)
    {
        (car.Has, car.Position, car.Stamp, car.At, car.Universe) = (true, position, stamp, at, universe);
        car.Speed.Run = 0; car.Climb.Run = 0;
        Anchor(car);
    }

    private static void Anchor(Car car)
    {
        (car.AnchorPosition, car.AnchorStamp, car.AnchorAt) = (car.Position, car.Stamp, car.At);
        (car.VelocityHorizontal, car.VelocityUp, car.VelocitySamples) = (0, 0, 0);
    }

    private void Step(Car car, Episode e, string kind, double velocity, double sent, double received, double maximum, int windowSent, long windowReceived, long at)
    {
        var overVelocity = velocity > maximum; var overSent = sent > maximum; var overReceived = received > maximum;
        if (overVelocity && overSent && overReceived)
        {
            if (!e.Active)
            {
                (e.Active, e.StartAt, e.Peak, e.PeakVelocity, e.PeakSender, e.PeakReceiver, e.Run, e.MaxRun, e.SenderMs, e.ReceiverMs, e.Disagreed) =
                    (true, at - windowReceived, 0, 0, 0, 0, 0, 0, 0, 0, 0);
                Log($"peer {car.Index}: {kind} episode starts: {velocity:0} km/h (velocity), {sent:0} km/h (sender clock), {received:0} km/h (receiver clock), over {maximum:0}");
            }
            e.Run += windowReceived; e.MaxRun = Math.Max(e.MaxRun, e.Run);
            e.Peak = Math.Max(e.Peak, Math.Min(velocity, Math.Min(sent, received)));
            e.PeakVelocity = Math.Max(e.PeakVelocity, velocity);
            e.PeakSender = Math.Max(e.PeakSender, sent); e.PeakReceiver = Math.Max(e.PeakReceiver, received);
            e.SenderMs += windowSent; e.ReceiverMs += windowReceived; e.LastOverAt = at;
            return;
        }
        e.Run = 0;
        if ((overVelocity || overSent || overReceived) && e.Active) e.Disagreed++;
    }

    private void Jump(Car car, double moved, int sent, long received, long at)
    {
        if (!car.JumpActive) { (car.JumpActive, car.JumpPeak, car.Jumps) = (true, moved, 0); }
        car.JumpPeak = Math.Max(car.JumpPeak, moved); car.Jumps++; car.JumpLast = at;
        Log($"peer {car.Index}: moved {moved:0} m in one sample ({sent} ms sender, {received} ms receiver): a jump, excluded from speed and climb");
    }

    private void Clock(Car car)
    {
        if (car.ClockSent || car.ClockReceiver < ClockMinMs || car.ClockSender <= 0) return;
        car.ClockSent = true;
        var ratio = (double)car.ClockSender / car.ClockReceiver;
        Log($"peer {car.Index}: clock ratio {ratio:0.000} (sender {car.ClockSender} ms / receiver {car.ClockReceiver} ms)");
        if (ratio is > 0 and <= 1000) report(new(car.Xuid, ObservationKinds.Clock, Math.Round(ratio, 3), null, rules.Version, car.Index));
    }

    private void EndQuiet(Car car, long at)
    {
        EndEpisode(car, car.Speed, ObservationKinds.Speed, rules.SpeedSustainMs, at, force: false);
        EndEpisode(car, car.Climb, ObservationKinds.Climb, rules.ClimbSustainMs, at, force: false);
        if (car.JumpActive && at - car.JumpLast >= QuietMs) EndJump(car);
    }

    private void End(Car car, long at, bool force)
    {
        EndEpisode(car, car.Speed, ObservationKinds.Speed, rules.SpeedSustainMs, at, force);
        EndEpisode(car, car.Climb, ObservationKinds.Climb, rules.ClimbSustainMs, at, force);
        if (car.JumpActive) EndJump(car);
    }

    private void EndJump(Car car)
    {
        car.JumpActive = false;
        Log($"peer {car.Index}: jump episode ends: {car.Jumps} jump(s), longest {car.JumpPeak:0} m; reported (log only)");
        report(new(car.Xuid, ObservationKinds.Jump, Math.Round(car.JumpPeak, 1), null, rules.Version, car.Index));
    }

    private void EndEpisode(Car car, Episode e, string kind, uint sustain, long at, bool force)
    {
        if (!e.Active || !force && at - e.LastOverAt < QuietMs) return;
        e.Active = false;
        var counted = e.MaxRun >= sustain;
        Log($"peer {car.Index}: {kind} episode ends: peak {e.Peak:0} km/h (velocity {e.PeakVelocity:0}, sender {e.PeakSender:0}, receiver {e.PeakReceiver:0}), longest run {e.MaxRun} ms of {sustain}, over for {e.SenderMs} ms sender / {e.ReceiverMs} ms receiver, measures disagreed {e.Disagreed}x, exclusions first {car.First} universe {car.Universes} stale {car.Stale} event {car.Events} jump {car.Jumps}: {(counted ? "reported" : "under the rule, not reported")}");
        // rounded up: a peak just over the limit never reads as the limit itself
        if (counted) report(new(car.Xuid, kind, Math.Ceiling(e.Peak * 10) / 10,(uint)Math.Min(e.MaxRun, 3_600_000), rules.Version, car.Index));
    }
}
