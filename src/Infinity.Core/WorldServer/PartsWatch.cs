// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Matchmaking;

namespace Infinity.Core.WorldServer;

/// <summary>
/// NR-6: the car a player object (router 1) describes, as far as its writes can be read. SAURON V9: var 103 ModelId (u32, the car's
/// ordinal), var 107 the installed upgrades (<see cref="PartsCatalog.Slots"/> x u32 part rows; "no part" is 0xFFFFFFFF or 0). A write
/// is a run of {u8 index, value}; it is read only while every index on the way has a known width, and stops at the first that has not
/// (never a guessed boundary). The same table is in native/hook watch.c (wt_player_width); docs/launcher/ANTICHEAT.md "parts".
/// </summary>
internal static class PlayerParts
{
    internal const int ModelVariable = 103, PartsVariable = 107;
    internal const uint NoPart = uint.MaxValue;

    /// <summary>
    /// The widths (bits) of player-object variables, SAURON's map: 0-11 PlayerData (3 is the name: a 5-bit count of UTF-16 units, -2
    /// here); 12-80 LobbyPlayer, not mapped (0: a write reaching them stops); 81-161 PlayerChoices and its nested setup (103 ModelId,
    /// 107 the 43 part rows, 121 GearRatios 9 x f32). The same table is wt_player_width in native/hook watch.c.
    /// </summary>
    internal static int Width(int index) => index switch
    {
        0 or 4 => 64,
        1 or 2 or 5 or 7 => 1,
        3 => -2,
        6 or (>= 8 and <= 11) => 32,
        81 or 82 or (>= 83 and <= 87) or 88 => 32,              // CarClass, PerfIndex, ratings, AIRank
        89 or 90 => 1,                                          // AIIsFriend, TeamId
        91 => 3,                                                // PlayerBucketId
        92 => 32,                                               // PittingProfile
        93 or 94 => 64,                                         // LiveryId, TuneId
        95 or 96 or 97 => 2,                                    // assists: suggested line, braking, steering
        98 or 99 => 1,                                          // assists: stability, traction control
        100 => 2,                                               // assist: shifting
        101 => 1,                                               // assist: rewind
        102 => 3,                                               // AIDifficulty
        ModelVariable or 104 or 105 or 106 => 32,               // ModelId, SharedId, TireBrand, WheelStyle
        PartsVariable => 32 * PartsCatalog.Slots,
        (>= 108 and <= 120) or (>= 122 and <= 143) => 32,       // tune values (f32), 120 FuelLoad
        121 => 9 * 32,                                          // GearRatios
        144 => 1,                                               // UseStockSetup
        >= 145 and <= 148 => 32,
        >= 149 and <= 153 => 1,
        >= 154 and <= 157 => 2,
        >= 158 and <= 161 => 32,
        _ => 0,
    };

    /// <summary>The model and the part rows <paramref name="payload"/> writes (each null when absent or past an unknown variable).</summary>
    internal static (uint? Model, uint[]? Parts) Read(BitBuffer payload) => Read(payload, out _);

    /// <summary>As <see cref="Read(BitBuffer)"/>; <paramref name="complete"/>: the whole write was read (no unknown variable, no truncation).</summary>
    internal static (uint? Model, uint[]? Parts) Read(BitBuffer payload, out bool complete)
    {
        uint? model = null; uint[]? parts = null;
        complete = false;
        var reader = new BitReader(payload.Bytes, payload.Length);
        try
        {
            while (true)
            {
                if (reader.Left == 0) { complete = true; break; }
                if (reader.Left < 8) break;
                var index = (int)reader.Read(8);
                var width = Width(index);
                if (width == 0) break;
                if (index == ModelVariable) { model = (uint)reader.Read(32); continue; }
                if (index == PartsVariable)
                {
                    var read = new uint[PartsCatalog.Slots];
                    for (var i = 0; i < read.Length; i++) read[i] = (uint)reader.Read(32);
                    parts = read;
                    continue;
                }
                reader.Skip(width == -2 ? 16 * (int)reader.Read(5) : width);
            }
        }
        catch (WireFormatException) { /* a truncated write: what was read before it stands */ }
        return (model, parts);
    }
}

/// <summary>
/// NR-6 (MATCHMAKING_API §3.9 <c>parts</c>: "a part or tune the game could not have produced"; OD-163): checks another player's
/// installed upgrades against this player's own game database. The host runs it in its world server from each peer's player object;
/// a seated joiner's hook reads the relayed player objects and the launcher runs it on what the hook sends (Frame.PlayerParts).
/// <list type="bullet">
/// <item>Measured only when the model is in this database (a car-pack car or a placeholder is never judged) and the part rows look
/// like this model's (at least one of its engine or motor rows, one of its drivetrain rows and one of its body rows are installed):
/// otherwise the layout is not what V9 says and nothing is judged.</item>
/// <item>A row that belongs to none of the model's tables (a part of another car, or a row the game does not have) is one
/// <c>parts</c> observation whose value is that row (the first in slot order); one per episode per player: the episode lasts while
/// the car stays impossible, and the same (model, row) is never reported twice under one rules version.</item>
/// <item>Logged only (SAURON confirms the slot order live): rows outside their own slot's table. Tune values (V9 vars 108-143) are
/// not read: their widths and units are not mapped.</item>
/// </list>
/// Logs name the peer index, the model and counts only: never an identity, a gamertag or an address.
/// </summary>
internal sealed class PartsWatch(PartsCatalog catalog, uint rulesVersion, Action<PeerWatchReport> report, Action<string>? log = null)
{
    internal const int MaxCars = 32;

    private sealed class Car(ulong xuid)
    {
        internal ulong Xuid { get; } = xuid;
        internal uint? Model;
        internal uint[]? Parts;
        internal (uint Model, uint Part)? Episode;
        internal uint? UnknownModelLogged;
        internal string? Checked;
    }

    private readonly Dictionary<byte, Car> _cars = [];
    private readonly HashSet<(ulong Xuid, uint Model, uint Part)> _reported = [];
    private readonly Action<string> _log = log ?? (_ => { });

    private void Log(FormattableString line) => _log("parts: " + line.ToString(CultureInfo.InvariantCulture));

    /// <summary>
    /// A write of the player object of the player at <paramref name="index"/> (<paramref name="xuid"/>): what it set of the model and
    /// the part rows (null: unchanged). <paramref name="fresh"/>: a new object (a create), whose earlier values are gone.
    /// </summary>
    internal void Observe(byte index, ulong xuid, uint? model, uint[]? parts, bool fresh = false)
    {
        if (xuid == 0 || parts is { Length: not PartsCatalog.Slots }) return;
        if (!_cars.TryGetValue(index, out var car) || car.Xuid != xuid || fresh)
        {
            if (car is not null) End(index, car, "a new player object");
            if (car is null && _cars.Count >= MaxCars) return;
            _cars[index] = car = new Car(xuid);
        }
        if (model is { } m) car.Model = m;
        if (parts is not null) car.Parts = (uint[])parts.Clone();
        if (car.Model is not { } current || car.Parts is not { } installed) return;
        Check(index, car, current, installed);
    }

    private void Check(byte index, Car car, uint model, uint[] installed)
    {
        var key = model.ToString(CultureInfo.InvariantCulture) + ":" + string.Join(',', installed);
        if (key == car.Checked) return; // the same car again: nothing new to judge
        car.Checked = key;
        // SAURON's live check: once per change, var 103 and a summary of var 107 (peer index only)
        var firsts = installed.Select((part, slot) => (part, slot)).Where(p => p.part is not (PlayerParts.NoPart or 0)).Take(6)
            .Select(p => string.Create(CultureInfo.InvariantCulture, $"{p.slot}:{p.part}"));
        Log($"peer {index}: var 103 = {model}; var 107: {installed.Count(p => p == PlayerParts.NoPart)} x 0xFFFFFFFF, {installed.Count(p => p == 0)} x 0, first rows (slot:row) {string.Join(' ', firsts)}");
        if (catalog.Model(model) is not { } rows)
        {
            if (car.UnknownModelLogged != model) Log($"peer {index}: model {model} is not in this game database (a car pack or a placeholder); not judged");
            car.UnknownModelLogged = model;
            End(index, car, "a model this database does not know");
            return;
        }
        var present = installed.Where(p => p is not (PlayerParts.NoPart or 0)).ToArray();
        var known = present.Count(rows.All.Contains);
        bool Has(int slot) => present.Any(rows.BySlot[slot].Contains);
        if (!(Has(PartsCatalog.EngineSlot) || Has(PartsCatalog.MotorSlot)) || !Has(PartsCatalog.DrivetrainSlot) || !Has(PartsCatalog.BodySlot))
        {
            Log($"peer {index}: model {model}: {present.Length} part rows, {known} of this car, without its engine, drivetrain and body rows: not the layout V9 maps; not judged");
            End(index, car, "an unrecognised layout");
            return;
        }
        var outside = 0; uint? offending = null;
        var misplaced = 0;
        for (var slot = 0; slot < installed.Length; slot++)
        {
            var part = installed[slot];
            if (part is PlayerParts.NoPart or 0) continue;
            if (!rows.All.Contains(part)) { outside++; offending ??= part; }
            else if (!rows.BySlot[slot].Contains(part)) misplaced++;
        }
        Log($"peer {index}: model {model}: {present.Length} part rows, {known} of this car, {outside} not; {misplaced} outside their slot's table (log only)");
        if (offending is not { } first)
        {
            End(index, car, "every part row belongs to the car");
            return;
        }
        if (car.Episode == (model, first)) return;
        if (car.Episode is not null) End(index, car, "another impossible part");
        car.Episode = (model, first);
        if (!_reported.Add((car.Xuid, model, first)))
        {
            Log($"peer {index}: model {model}: part row {first} already reported under rules version {rulesVersion}; not again");
            return;
        }
        Log($"peer {index}: model {model}: part row {first} belongs to none of this car's tables: reported");
        report(new(car.Xuid, ObservationKinds.Parts, first, null, rulesVersion, index));
    }

    private void End(byte index, Car car, string why)
    {
        if (car.Episode is not { } episode) return;
        car.Episode = null;
        Log($"peer {index}: the impossible-parts episode (model {episode.Model}) ends: {why}");
    }

    /// <summary>The player at <paramref name="index"/> left: its episode ends.</summary>
    internal void Forget(byte index)
    {
        if (_cars.Remove(index, out var car)) End(index, car, "the player left");
    }

    /// <summary>The rules end: every episode ends.</summary>
    internal void Flush()
    {
        foreach (var (index, car) in _cars) End(index, car, "the rules ended");
        _cars.Clear();
    }
}
