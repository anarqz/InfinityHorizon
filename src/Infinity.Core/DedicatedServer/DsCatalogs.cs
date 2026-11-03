// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using Infinity.Core.WorldServer;

namespace Infinity.Core.DedicatedServer;

/// <summary>
/// CR-L51 (DEDICATED_SERVER.md §9): the game data the engine uses, read by the launcher from the player's own installed game
/// database with the reference's queries (<see cref="RoadTripCatalog"/>, <see cref="PartsCatalog"/>, called as they are) and
/// sent over the control channel; the engine reads no game file. Logs carry counts only, never a row.
/// </summary>
public sealed class DsCatalogs(string? gameDatabase, Action<string>? log = null)
{
    /// <summary>The most Road Trip events in one <c>catalog.set</c> (§4.2).</summary>
    public const int MaxEvents = 1024;

    /// <summary>The most classes per event (§9: CarClassId 0-7).</summary>
    public const int MaxClasses = 8;

    /// <summary>The most row ids per slot table, and in all, in one <c>catalog.parts</c> (§4.2).</summary>
    public const int MaxIdsPerTable = 1024, MaxIds = 4096;

    private readonly Action<string> _log = log ?? (_ => { });
    private readonly Lock _gate = new();
    private PartsCatalog? _parts;
    private bool _partsRead;

    /// <summary>The <c>catalog.set</c> of this database (events at the reference's Road Trip locations, in its order), or null with none.</summary>
    public DsCatalogSet? RoadTrip()
    {
        if (gameDatabase is not { Length: > 0 } database || !File.Exists(database) || RoadTripCatalog.Load(database) is not { } catalog)
        {
            _log("ds catalog: no Road Trip events (no readable game database)");
            return null;
        }
        return RoadTrip(catalog.Events.Select(e => (e.Id, e.Location, e.Mode, e.MinPlayers, (IEnumerable<int>)e.Classes)));
    }

    /// <summary>The <c>catalog.set</c> of these rows: bounded per §4.2, versioned by their content (the same rows, the same version).</summary>
    internal DsCatalogSet RoadTrip(IEnumerable<(int Id, int Location, int Mode, int MinPlayers, IEnumerable<int> Classes)> rows)
    {
        var events = rows.Take(MaxEvents).Select(r => new DsRoadTripEvent
        {
            Id = r.Id,
            Location = r.Location,
            Mode = r.Mode,
            MinPlayers = r.MinPlayers,
            Classes = [.. r.Classes.Where(c => c is >= 0 and < MaxClasses).Distinct().Take(MaxClasses)],
        }).ToArray();
        var text = new StringBuilder();
        foreach (var e in events)
            text.Append(CultureInfo.InvariantCulture, $"{e.Id}:{e.Location}:{e.Mode}:{e.MinPlayers}:{string.Join('.', e.Classes)};");
        var version = "rt-" + Convert.ToHexStringLower(SHA256.HashData(Encoding.ASCII.GetBytes(text.ToString())))[..16];
        _log(string.Create(CultureInfo.InvariantCulture, $"ds catalog: {events.Length} Road Trip event(s), version {version}"));
        return new DsCatalogSet { Version = version, RoadTrip = new DsRoadTrip { Events = events } };
    }

    /// <summary>
    /// The <c>catalog.parts</c> answer for <paramref name="model"/>: every slot table with the model's rows (bounded), or no
    /// entry at all when the database does not have the model or cannot be read (§9: not judged).
    /// </summary>
    public DsCatalogParts Parts(uint model)
    {
        PartsCatalog? catalog;
        lock (_gate)
        {
            if (!_partsRead)
            {
                _partsRead = true;
                _parts = PartsCatalog.Load(gameDatabase, line => _log("ds catalog: " + line));
            }
            catalog = _parts;
        }
        if (catalog?.Model(model) is not { } parts)
        {
            _log("ds catalog: parts of a model this game database does not have");
            return new DsCatalogParts { Model = model, Slots = new Dictionary<string, IReadOnlyList<uint>>(StringComparer.Ordinal) };
        }
        var (slots, omitted) = Bound(PartsCatalog.SlotTables.Take(parts.BySlot.Length).Select((slot, index) => (slot.Table, (IEnumerable<uint>)parts.BySlot[index])));
        _log(string.Create(CultureInfo.InvariantCulture,
            $"ds catalog: parts of a model, {slots.Count} table(s), {slots.Values.Sum(v => v.Count)} row(s){(omitted > 0 ? $", {omitted} table(s) omitted past the bounds (not judged)" : string.Empty)}"));
        return new DsCatalogParts { Model = model, Slots = slots };
    }

    /// <summary>
    /// §4.2's bounds without ever truncating a list (a partial list would flag legitimate parts): a table past
    /// <see cref="MaxIdsPerTable"/> is left out, and once <see cref="MaxIds"/> would be passed every remaining table is left
    /// out. A table left out is "not judged", which is safe.
    /// </summary>
    internal static (Dictionary<string, IReadOnlyList<uint>> Slots, int Omitted) Bound(IEnumerable<(string Table, IEnumerable<uint> Ids)> tables)
    {
        var slots = new Dictionary<string, IReadOnlyList<uint>>(StringComparer.Ordinal);
        var (total, omitted, full) = (0, 0, false);
        foreach (var (table, rows) in tables)
        {
            var ids = rows.Order().ToArray();
            if (full || ids.Length > MaxIdsPerTable) { omitted++; continue; }
            if (total + ids.Length > MaxIds) { full = true; omitted++; continue; }
            total += ids.Length;
            slots[table] = ids;
        }
        return (slots, omitted);
    }
}
