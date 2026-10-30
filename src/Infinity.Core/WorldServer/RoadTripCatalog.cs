// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Microsoft.Data.Sqlite;

namespace Infinity.Core.WorldServer;

/// <summary>One of the game's online events (AnthemOnlineEvents), with the car classes it allows and the players its mode needs.</summary>
internal sealed record RoadTripEvent(int Id, int Location, int Mode, int MinPlayers, IReadOnlySet<int> Classes);

/// <summary>A destination on the vote: its location, its four events (and the players each needs) and the one car class they share.</summary>
internal sealed record RoadTripDestination(int Location, IReadOnlyList<int> Events, IReadOnlyList<int> MinPlayers, int CarClass);

/// <summary>
/// The game's own Road Trip events, read from the host's installed game database (read-only, nothing copied): AnthemOnlineEvents
/// (location, mode), AnthemOnlineEventPermissions (the classes each allows) and RaceModes (MinimumPlayers). The vote offers three
/// destinations of four events each, and every destination one car class that all four of its events allow.
/// </summary>
internal sealed class RoadTripCatalog
{
    /// <summary>The game's Road Trip destinations (RoadTripLocationId); 9 is Storm Island, an expansion.</summary>
    internal const int FirstLocation = 3, LastLocation = 9, StormIsland = 9;
    /// <summary>CarClasses 0..5 (D, C, B, A, S1, S2); 6 X and 7 R are rental or event-specific.</summary>
    internal const int Classes = 6;
    internal const int EventsPerDestination = 4, Destinations = 3;

    internal IReadOnlyList<RoadTripEvent> Events { get; }

    internal RoadTripCatalog(IEnumerable<RoadTripEvent> events) =>
        Events = [.. events.Where(e => e.Location is >= FirstLocation and <= LastLocation)];

    /// <summary>The catalog of <paramref name="gameDatabase"/> (gamedbRC.slt), or null when it cannot be read.</summary>
    internal static RoadTripCatalog? Load(string gameDatabase)
    {
        try
        {
            using var db = new SqliteConnection(new SqliteConnectionStringBuilder
            { DataSource = gameDatabase, Mode = SqliteOpenMode.ReadOnly, Pooling = false }.ToString());
            db.Open();
            var classes = new Dictionary<int, HashSet<int>>();
            using (var command = db.CreateCommand())
            {
                command.CommandText = "SELECT AnthemOnlineEventId, CarClassId FROM AnthemOnlineEventPermissions";
                using var rows = command.ExecuteReader();
                while (rows.Read())
                {
                    var id = rows.GetInt32(0);
                    if (!classes.TryGetValue(id, out var set)) classes[id] = set = [];
                    set.Add(rows.GetInt32(1));
                }
            }
            var events = new List<RoadTripEvent>();
            using (var command = db.CreateCommand())
            {
                command.CommandText = "SELECT e.AnthemOnlineEventId, e.RoadTripLocationId, e.RaceModeId, m.MinimumPlayers " +
                    "FROM AnthemOnlineEvents e JOIN RaceModes m ON m.id = e.RaceModeId WHERE e.RoadTripLocationId > 0";
                using var rows = command.ExecuteReader();
                while (rows.Read())
                {
                    var id = rows.GetInt32(0);
                    events.Add(new(id, rows.GetInt32(1), rows.GetInt32(2), Math.Max(1, rows.GetInt32(3)),
                        classes.TryGetValue(id, out var set) ? set : new HashSet<int>()));
                }
            }
            return new(events);
        }
        catch (Exception error) when (error is SqliteException or IOException or UnauthorizedAccessException or InvalidOperationException)
        {
            return null;
        }
    }

    /// <summary>
    /// Three destinations for <paramref name="players"/> players: only events their mode can run with them, each destination four
    /// distinct events that share one allowed class (their modes mixed), the three locations and, where possible, the three classes
    /// different. Storm Island only when the host has it. Null when the catalog cannot fill three destinations.
    /// </summary>
    internal IReadOnlyList<RoadTripDestination>? Draw(int players, bool stormIsland, Random random)
    {
        ArgumentNullException.ThrowIfNull(random);
        var eligible = Events.Where(e => e.MinPlayers <= Math.Max(1, players) && (stormIsland || e.Location != StormIsland)).ToList();
        var locations = eligible.Select(e => e.Location).Distinct().OrderBy(_ => random.Next()).ToList();
        var chosen = new List<RoadTripDestination>();
        var usedClasses = new HashSet<int>();
        foreach (var location in locations)
        {
            if (chosen.Count == Destinations) break;
            var here = eligible.Where(e => e.Location == location).ToList();
            // a class not used yet comes first, then any class; each tried in a random order
            var order = Enumerable.Range(0, Classes).OrderBy(c => (usedClasses.Contains(c), random.Next()));
            foreach (var carClass in order)
            {
                var fits = here.Where(e => e.Classes.Contains(carClass)).ToList();
                if (fits.Count < EventsPerDestination) continue;
                var picked = Mix(fits, random);
                chosen.Add(new(location, [.. picked.Select(e => e.Id)], [.. picked.Select(e => e.MinPlayers)], carClass));
                usedClasses.Add(carClass);
                break;
            }
        }
        return chosen.Count == Destinations ? chosen : null;
    }

    // four events, round robin over their race modes in a random order, so a destination is not four of one kind
    private static List<RoadTripEvent> Mix(List<RoadTripEvent> events, Random random)
    {
        var pools = events.GroupBy(e => e.Mode).Select(g => new Queue<RoadTripEvent>(g.OrderBy(_ => random.Next()))).OrderBy(_ => random.Next()).ToList();
        var picked = new List<RoadTripEvent>(EventsPerDestination);
        while (picked.Count < EventsPerDestination)
            foreach (var pool in pools.Where(p => p.Count != 0))
                if (picked.Count < EventsPerDestination) picked.Add(pool.Dequeue());
        return picked;
    }
}
