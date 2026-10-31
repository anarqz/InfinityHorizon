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
    /// <summary>NR-6: this player's own game database's upgrade tables (the <c>parts</c> rule); null: that rule measures nothing.</summary>
    internal PartsCatalog? Parts { get; init; }

    /// <summary>
    /// NR-5: <c>speed.perCarFloorKmh</c> (null: a server older than NR-5). With it, speed is measured against the floor and each
    /// speed report carries <see cref="PeerWatchReport.HeldKmh"/> and, when known, <see cref="PeerWatchReport.CarId"/>.
    /// </summary>
    public double? SpeedFloorKmh { get; init; }

    /// <summary>NR-5: speed episodes are measured against the per-car floor and carry heldKmh and carId.</summary>
    public bool PerCar => SpeedFloorKmh is not null;

    /// <summary>What speed is measured against: the floor (NR-5), else the flat limit.</summary>
    public double SpeedMeasuredKmh => SpeedFloorKmh ?? SpeedMaxKmh;

    public static PeerWatchRules From(WatchRules rules, bool forceSpeed = false)
    {
        ArgumentNullException.ThrowIfNull(rules);
        return new(rules.Version, rules.Speed.MaxKmh, rules.Speed.SustainMs, rules.Climb.MaxKmh, rules.Climb.SustainMs, rules.Jump.MaxMeters, forceSpeed)
        {
            SpeedFloorKmh = rules.Speed.PerCarFloorKmh,
        };
    }
}

/// <summary>
/// One ended episode of another player's car: what the launcher posts (<see cref="PeerObservation"/>, an id it mints). Speed under
/// NR-5 also carries <see cref="HeldKmh"/> (in (0, <see cref="Value"/>]) and <see cref="CarId"/> (the car's ordinal, when read).
/// </summary>
public sealed record PeerWatchReport(ulong Xuid, string Kind, double Value, uint? SustainMs, uint RulesVersion, byte PeerIndex,
    uint? CarId = null, double? HeldKmh = null);

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
/// <item>NR-5 (<see cref="PeerWatchRules.PerCar"/>): speed is measured against <c>perCarFloorKmh</c>; a speed report also carries
/// the held speed (the maximum, over every window of the speed sustain inside one run, of that window's lowest measured value; see
/// <see cref="HeldSegments"/>) and the car's ordinal (var 103 of the observed player's own player object, <see cref="Model"/>).</item>
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

    /// <summary>
    /// NR-5: the held speed keeps at most this many windows of the current run (each at least <see cref="WindowMs"/>, so 16 s at
    /// least); a full buffer merges its two oldest into one (their lowest value, their summed time), which can only lower the held
    /// speed, never raise it. watch.c's <c>WT_HELD</c> is the same.
    /// </summary>
    internal const int HeldSegments = 64;

    private sealed class Episode
    {
        internal bool Active;
        internal double Peak, PeakVelocity, PeakSender, PeakReceiver, Held;
        internal long Run, MaxRun, LastOverAt, StartAt, SenderMs, ReceiverMs;
        internal int Disagreed;
        // NR-5: the current run's windows, oldest first: (the window's value, its receive time)
        internal readonly List<(double Value, long Ms)> Windows = new(HeldSegments);
        internal long WindowsMs;

        internal void ResetRun()
        {
            Run = 0;
            Windows.Clear(); WindowsMs = 0;
        }

        /// <summary>
        /// One window over the limit in the current run: the held speed becomes the larger of itself and the lowest value of the
        /// shortest suffix of the run lasting at least <paramref name="sustain"/>. That is every window of the sustain ending at a
        /// window's end; one ending elsewhere covers at least the same windows, so it never holds more.
        /// </summary>
        internal void Hold(double value, long ms, uint sustain)
        {
            if (Windows.Count >= HeldSegments)
            {
                Windows[1] = (Math.Min(Windows[0].Value, Windows[1].Value), Windows[0].Ms + Windows[1].Ms);
                Windows.RemoveAt(0);
            }
            Windows.Add((value, ms)); WindowsMs += ms;
            while (Windows.Count > 1 && WindowsMs - Windows[0].Ms >= sustain) { WindowsMs -= Windows[0].Ms; Windows.RemoveAt(0); }
            if (WindowsMs < sustain) return;
            var lowest = double.MaxValue;
            foreach (var (v, _) in Windows) lowest = Math.Min(lowest, v);
            Held = Math.Max(Held, lowest);
        }
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
    // NR-5: each peer's car ordinal (var 103) from its own player object, by peer index; null: not read yet
    private readonly Dictionary<byte, (ulong Xuid, uint? Model)> _models = [];
    private readonly Action<string> _log = log ?? (_ => { });
    private bool _forced, _unitsLogged;

    /// <summary>The largest ordinal a <c>carId</c> may carry (§3.9: a whole ordinal in [0, 1e6]); a larger one is omitted.</summary>
    internal const uint MaxCarId = 1_000_000;

    internal PeerWatchRules Rules => rules;

    private void Log(FormattableString line) => _log("watch: " + line.ToString(CultureInfo.InvariantCulture));

    /// <summary>
    /// NR-5: a write of the player object of the player at <paramref name="index"/> (<paramref name="xuid"/>) as it arrived on that
    /// player's own leg (never a copy another player forwarded): its var 103 <c>ModelId</c> when the write set it (null: unchanged).
    /// <paramref name="fresh"/>: a new object, whose earlier model is gone.
    /// </summary>
    internal void Model(byte index, ulong xuid, uint? model, bool fresh = false)
    {
        if (xuid == 0) return;
        var known = _models.TryGetValue(index, out var had) && had.Xuid == xuid && !fresh;
        if (!known && !_models.ContainsKey(index) && _models.Count >= MaxCars) return;
        var next = model ?? (known ? had.Model : null);
        if (known && next == had.Model) return;
        _models[index] = (xuid, next);
        if (next is { } car && (!known || had.Model != car)) Log($"peer {index}: car ordinal {car} (var 103)");
    }

    private uint? CarId(Car car) =>
        _models.TryGetValue(car.Index, out var m) && m.Xuid == car.Xuid && m.Model is { } model && model <= MaxCarId ? model : null;

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
            // over the flat limit with no car named, so the server counts it whatever the car (NR-5: held for the whole sustain)
            var value = rules.SpeedMaxKmh + 50;
            report(new(xuid, ObservationKinds.Speed, value, rules.SpeedSustainMs + 1000, rules.Version, index, null, rules.PerCar ? value : null));
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
        Step(car, car.Speed, ObservationKinds.Speed, speedVelocity, speedSent, speedReceived, rules.SpeedMeasuredKmh, windowSent, windowReceived, at);
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
            car.Speed.ResetRun(); car.Climb.ResetRun();
        }
        if (_cars.Count != 0) Log($"{why}: every car's measurement restarts; excluded");
    }

    /// <summary>The player at <paramref name="index"/> left: its episodes end now (a qualifying one is reported).</summary>
    internal void Forget(byte index, long at)
    {
        _models.Remove(index);
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
        _models.Clear();
    }

    private static void Restart(Car car, Vector3 position, uint stamp, byte universe, long at)
    {
        (car.Has, car.Position, car.Stamp, car.At, car.Universe) = (true, position, stamp, at, universe);
        car.Speed.ResetRun(); car.Climb.ResetRun();
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
                (e.Active, e.StartAt, e.Peak, e.PeakVelocity, e.PeakSender, e.PeakReceiver, e.Held, e.Run, e.MaxRun, e.SenderMs, e.ReceiverMs, e.Disagreed) =
                    (true, at - windowReceived, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
                e.ResetRun();
                Log($"peer {car.Index}: {kind} episode starts: {velocity:0} km/h (velocity), {sent:0} km/h (sender clock), {received:0} km/h (receiver clock), over {maximum:0}");
            }
            e.Run += windowReceived; e.MaxRun = Math.Max(e.MaxRun, e.Run);
            var measured = Math.Min(velocity, Math.Min(sent, received));
            e.Peak = Math.Max(e.Peak, measured);
            if (kind == ObservationKinds.Speed && rules.PerCar) e.Hold(measured, windowReceived, rules.SpeedSustainMs);
            e.PeakVelocity = Math.Max(e.PeakVelocity, velocity);
            e.PeakSender = Math.Max(e.PeakSender, sent); e.PeakReceiver = Math.Max(e.PeakReceiver, received);
            e.SenderMs += windowSent; e.ReceiverMs += windowReceived; e.LastOverAt = at;
            return;
        }
        e.ResetRun();
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
        // NR-5: speed carries the held speed and the car's ordinal (when read)
        var perCar = kind == ObservationKinds.Speed && rules.PerCar;
        var held = perCar && counted ? HeldValue(e) : null;
        var carId = perCar ? CarId(car) : null;
        Log($"peer {car.Index}: {kind} episode ends: peak {e.Peak:0} km/h (velocity {e.PeakVelocity:0}, sender {e.PeakSender:0}, receiver {e.PeakReceiver:0}){(perCar ? $", held {e.Held.ToString("0", CultureInfo.InvariantCulture)} km/h, car {(carId is { } id ? id.ToString(CultureInfo.InvariantCulture) : "unknown")}" : string.Empty)}, longest run {e.MaxRun} ms of {sustain}, over for {e.SenderMs} ms sender / {e.ReceiverMs} ms receiver, measures disagreed {e.Disagreed}x, exclusions first {car.First} universe {car.Universes} stale {car.Stale} event {car.Events} jump {car.Jumps}: {(counted ? "reported" : "under the rule, not reported")}");
        // rounded up: a peak just over the limit never reads as the limit itself
        if (counted) report(new(car.Xuid, kind, Math.Ceiling(e.Peak * 10) / 10, (uint)Math.Min(e.MaxRun, 3_600_000), rules.Version, car.Index, carId, held));
    }

    /// <summary>The held speed for the platform: rounded down to 0.1 km/h (never above what was measured, so never above the peak).</summary>
    private static double? HeldValue(Episode e)
    {
        var held = Math.Floor(e.Held * 10) / 10;
        return held > 0 ? held : null;
    }
}
