// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

/// <summary>The primary race bucket's car restriction: a class (255 any) and a minimum and maximum PI (0 none).</summary>
internal readonly record struct CarRule(int CarClass, int MinimumPi, int MaximumPi)
{
    internal static CarRule Any => new(AnyClass, 0, 0);
    internal const int AnyClass = 255;
    internal bool Unrestricted => CarClass == AnyClass && MinimumPi == 0 && MaximumPi == 0;

    /// <summary>Whether a car of <paramref name="carClass"/> and performance index <paramref name="pi"/> (×1000 shows its PI) may enter.</summary>
    internal bool Accepts(int? carClass, float pi)
    {
        if (Unrestricted) return true;
        if (carClass is not { } have || !float.IsFinite(pi)) return false;
        var shown = (int)(pi * 1000 + 0.5f);
        return (CarClass == AnyClass || have == CarClass) && (MinimumPi == 0 || shown >= MinimumPi) && (MaximumPi == 0 || shown <= MaximumPi);
    }
}

/// <summary>
/// Reads the event options the leader writes (GameOptionsSynced): a stream of [10-bit variable id][value], ids ascending, each write a
/// delta (an omitted variable keeps its value). Every variable's width is known, in constructor order, so a write is read from its
/// start, never searched: an unknown, unordered or truncated write is refused whole rather than guessed, since one wrong width would
/// misread every later variable (a car restriction or a track included). The options stay opaque on the wire; only this reads them.
/// </summary>
internal sealed class GameOptionsFields
{
    internal const int IdBits = 10;
    internal const int CarClassId = 30, MinimumPiId = 31, MaximumPiId = 32;
    internal const int EndConditionId = 418, NumLapsId = 419, NumTeamsId = 428, TeamScoringId = 429;
    /// <summary>A bucket name: a 5-bit count of up to 24 UTF-16 units.</summary>
    private const int Name = -24;

    // One race bucket: its name, ids and flags, the class and PI limits (8, 16, 16 at its offsets 8-10), two 50 × u16 restriction lists.
    private static readonly int[] Bucket =
    [
        Name, 32, 1, 8, 8, 32, 32, 32, 8, 16, 16, 32, 32, 32, 32, 32, 32,
        2, 8, 2, 8, 2, 8, 2, 8, 2, 16, 2, 16, 2, 16, 2, 8, 2, 8, 2, 8,
        2, 8, 2, 8, 2, 8, 1, 32, 800, 800,
    ];

    /// <summary>Every variable's width in bits (negative: a string of at most that many units), ids 0..492.</summary>
    internal static readonly int[] Widths =
    [
        8, 8, 3, 4, 32, 32, 8, 32, 32, 8, 32, 32, 4, 4, 8, 3, 2, 3, 1, 3, 8, 8,
        .. Bucket, .. Bucket, .. Bucket, .. Bucket, .. Bucket, .. Bucket, .. Bucket, .. Bucket,
        32, 32, 2, 3, 1, 1, 1, 1, 8, 8, 32, 32, 32, 1, 8, 8, 8, 32, 32, 32, 3, 32,
        // time, destination, tag, scoring, assists, hopper, weather and the top-level flags (436 is three floats)
        32, 8, 32, 32, 4, 1, 1, 1, 2, 3, 8, 1, 1, 1, 1, 8, 96, 1, 1,
        32, 32, 8, 8, 8, 8, 8, 1, 1, 1, 1, 1, 2, 2, 2, 3, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 3, 32, 16, 16, 32, 32, 32, 32, 1, 1, 1,
        1, 32, 4, 1, 1, 32, 32, 32, 32, 1, 1, 4, 1, 1, 1, 1, 1, 1,
    ];

    private readonly SortedDictionary<int, BitBuffer> _values = [];

    /// <summary>The variables set so far, each its raw bits (a string with its count).</summary>
    internal IReadOnlyDictionary<int, BitBuffer> Values => _values;

    /// <summary>Applies one write (a delta) on top of the earlier ones; a write that does not read cleanly changes nothing.</summary>
    internal bool Apply(BitBuffer write)
    {
        ArgumentNullException.ThrowIfNull(write);
        var read = new List<(int Id, BitBuffer Raw)>();
        var reader = new BitReader(write.Bytes, write.Length);
        var previous = -1;
        try
        {
            while (reader.Left > 0)
            {
                if (reader.Left < IdBits) return false;
                var id = (int)reader.Read(IdBits);
                if (id <= previous || id >= Widths.Length) return false;
                var width = Widths[id];
                var start = reader;
                if (width < 0)
                {
                    var count = (int)reader.Read(5);
                    if (count > -width) return false;
                    reader = start;
                    width = 5 + 16 * count;
                }
                if (reader.Left < width) return false;
                read.Add((id, reader.Take(width)));
                previous = id;
            }
        }
        catch (WireFormatException) { return false; }
        foreach (var (id, raw) in read) _values[id] = raw;
        return true;
    }

    /// <summary>A fresh reading of a whole journal (its records in order); false when one of them does not read.</summary>
    internal static bool TryRead(IEnumerable<BitBuffer> records, out GameOptionsFields fields)
    {
        ArgumentNullException.ThrowIfNull(records);
        fields = new();
        foreach (var record in records) if (!fields.Apply(record)) return false;
        return true;
    }

    private ulong? Number(int id)
    {
        if (!_values.TryGetValue(id, out var raw) || raw.Length > 64) return null;
        var reader = new BitReader(raw.Bytes, raw.Length);
        return reader.Read(raw.Length);
    }

    /// <summary>The primary race bucket's restriction (nothing written: any car).</summary>
    internal CarRule CarRule => new((int)(Number(CarClassId) ?? CarRule.AnyClass), (int)(Number(MinimumPiId) ?? 0), (int)(Number(MaximumPiId) ?? 0));

    /// <summary>NumLaps, when written.</summary>
    internal uint? NumLaps => Number(NumLapsId) is { } laps ? (uint)laps : null;

    /// <summary>EndConditionType (0 none, 1 laps, 2 time, 3..6 other modes), when written.</summary>
    internal int? EndCondition => Number(EndConditionId) is { } end ? (int)end : null;

    /// <summary>ScoringOptions.NumTeams (0..2; 2 is "Teams: On"), when written.</summary>
    internal int? NumTeams => Number(NumTeamsId) is { } teams ? (int)teams : null;
}
