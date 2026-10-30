// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

/// <summary>
/// King and Infected, the tag events. The event options say which one runs (GameTypePreset) and its rules (TagOptions). Each game
/// works a hit out from those rules and reports it (MsgTagCollision 36: both cars' buckets before and after); the server owns every
/// car's bucket: it draws the first It car(s) while the starting grid is up and tells every participant (MsgTagBuckets 37: the
/// receiver's own car by its own key, the others by relay keys). 37 travels unreliably, so each change is sent again a few times.
/// </summary>
internal sealed partial class RelayEngine
{
    private sealed class TagGame(TagRules rules, long limitMs, long assignAt)
    {
        internal TagRules Rules { get; } = rules;
        internal long LimitMs { get; } = limitMs;
        internal long AssignAt { get; } = assignAt;
        internal bool Assigned { get; set; }
        /// <summary>Each car's bucket by its player's peer index.</summary>
        internal Dictionary<byte, byte> Buckets { get; } = [];
        internal List<long> Repeats { get; } = [];
        /// <summary>"GO" on the server clock, once the start was sent.</summary>
        internal long? GoAt { get; set; }
        internal long Marked { get; set; }
        /// <summary>How long each car was It since "GO" (King: crown time).</summary>
        internal Dictionary<byte, long> HeldMs { get; } = [];
        /// <summary>King: crowns taken; Infected: infections made.</summary>
        internal Dictionary<byte, int> Hits { get; } = [];
        internal byte NotIt => (byte)(Rules.ItBucket == TagRules.NotIt ? TagRules.It : TagRules.NotIt);
        internal bool IsIt(byte index) => Buckets.GetValueOrDefault(index, NotIt) == Rules.ItBucket;
    }

    /// <summary>The first It car(s) are drawn this long after the starting grid, so the grid screen already shows them.</summary>
    private const long TagAssignDelayMs = 1500;
    /// <summary>A change is sent again this long after its first send.</summary>
    private static readonly long[] TagRepeatMs = [400, 1200, 3000];
    private const long TagDefaultLimitMs = 240000;
    private const long TagLongestLimitMs = 3600000;
    /// <summary>The games end a tag event on their own clock and send their final scores then: an earlier one is not a finish.</summary>
    private const long TagEarliestFinishMs = 20000;
    private TagGame? _tag;
    private Random? _tagRandom;
    private Random TagRandom => _tagRandom ??= new(_settings.TagSeed ?? RandomNumberGenerator.GetInt32(int.MaxValue));

    private static string Describe(TagMode mode) => mode == TagMode.King ? "King" : "Infected";

    /// <summary>The starting grid was sent: a King or Infected event (by its decoded options) gets its tag game.</summary>
    private void BeginTag()
    {
        _tag = null;
        if (!GameOptionsFields.TryRead(_gameOptions.Records, out var fields) || fields.Tag is not { } rules) return;
        var limit = fields.EndCondition == 2 && fields.TimedRaceLength is { } seconds && seconds > 0
            ? Math.Min(TagLongestLimitMs, (long)(seconds * 1000)) : TagDefaultLimitMs;
        _tag = new(rules, limit, _now + TagAssignDelayMs);
        Note(string.Create(CultureInfo.InvariantCulture,
            $"tag: {Describe(rules.Mode)} event, {limit / 1000} s, It bucket {rules.ItBucket}, players per It {rules.PlayersPerIt?.ToString(CultureInfo.InvariantCulture) ?? "unset"}"));
    }

    /// <summary>The start was sent: crown time counts from "GO", and the buckets go out again for the start.</summary>
    private void TagStarted(long goAt)
    {
        if (_tag is not { } tag) return;
        tag.GoAt = goAt;
        tag.Marked = _now;
        if (tag.Assigned) SendTagBuckets(tag, true);
    }

    /// <summary>The participants whose car is known, in grid order (at most one MsgTagBuckets' slots).</summary>
    private List<RelayPeer> TagCars() => _race is not { } race ? [] : race.Participants
        .Select(index => _peers.FirstOrDefault(p => p.Index == index)).OfType<RelayPeer>()
        .Where(p => p.Synced && p.CarId is not null).Take(NerdMessage.TagBuckets.Slots).ToList();

    private void TickTag()
    {
        if (_tag is not { } tag || _race is not { } race) return;
        if (!tag.Assigned)
        {
            if (_now >= tag.AssignAt) AssignTag(tag);
            return;
        }
        if (!EnsureIt(tag) && tag.Repeats.Count != 0 && _now >= tag.Repeats[0])
        {
            tag.Repeats.RemoveAt(0);
            SendTagBuckets(tag, false);
        }
        if (race.Stage != RaceStage.Started) return;
        var cars = TagCars();
        if (tag.Rules.Mode == TagMode.Infected && cars.Count > 1 && cars.All(p => tag.IsIt(p.Index)))
        {
            Note("tag: everyone infected");
            EndRace("everyone infected");
        }
        else if (tag.GoAt is { } go && _now >= go + tag.LimitMs)
        {
            Note("tag: time up");
            EndRace("tag time up");
        }
    }

    private void AssignTag(TagGame tag)
    {
        var cars = TagCars();
        if (cars.Count == 0) return;
        AccountTag(tag);
        tag.Buckets.Clear();
        foreach (var car in cars) tag.Buckets[car.Index] = tag.NotIt;
        // OD-153: Infected starts with exactly one patient zero whatever the player count (its TagOptions have no
        // NumPlayersPerIt); King keeps the event's NumPlayersPerIt: max(1, ceil(players / NumPlayersPerIt)) crowns.
        var count = tag.Rules.Mode == TagMode.King && tag.Rules.PlayersPerIt is { } per and > 0
            ? Math.Max(1, (cars.Count + per - 1) / per) : 1;
        count = Math.Min(count, cars.Count);
        var pool = cars.ToList();
        for (var i = 0; i < count; i++)
        {
            var pick = pool[TagRandom.Next(pool.Count)];
            pool.Remove(pick);
            tag.Buckets[pick.Index] = (byte)tag.Rules.ItBucket;
        }
        tag.Assigned = true;
        Note(string.Create(CultureInfo.InvariantCulture,
            $"tag: {(tag.Rules.Mode == TagMode.Infected ? "infected" : "crowned")} ({count} it of {cars.Count})"));
        SendTagBuckets(tag, true);
    }

    /// <summary>With nobody It any more (the It car left), a new one is drawn. True when it was.</summary>
    private bool EnsureIt(TagGame tag)
    {
        var cars = TagCars();
        if (cars.Count == 0 || cars.Any(p => tag.IsIt(p.Index))) return false;
        var pick = cars[TagRandom.Next(cars.Count)];
        AccountTag(tag);
        tag.Buckets[pick.Index] = (byte)tag.Rules.ItBucket;
        Note(string.Create(CultureInfo.InvariantCulture, $"tag: no It car left, car {pick.Index} is It now"));
        SendTagBuckets(tag, true);
        return true;
    }

    /// <summary>MsgTagBuckets to every participant; <paramref name="repeat"/> schedules its repeats.</summary>
    private void SendTagBuckets(TagGame tag, bool repeat)
    {
        if (_race is not { } race) return;
        var cars = TagCars();
        foreach (var receiver in _peers.Where(p => p.Synced && race.Participants.Contains(p.Index)))
        {
            var keys = new ObjectHandle[NerdMessage.TagBuckets.Slots];
            var buckets = new byte[NerdMessage.TagBuckets.Slots];
            for (var i = 0; i < keys.Length; i++)
            {
                if (i >= cars.Count) { keys[i] = NerdMessage.TagBuckets.Unused; continue; }
                var car = cars[i];
                keys[i] = car == receiver ? new(HandleKind.Own, car.CarId!.Value) : new(HandleKind.Relay, car.CarId!.Value, car.Index);
                buckets[i] = tag.Buckets.GetValueOrDefault(car.Index, tag.NotIt);
            }
            Send(receiver, [NerdCodec.Write(new NerdMessage.TagBuckets(keys, buckets))], false);
        }
        if (!repeat) return;
        tag.Repeats.Clear();
        tag.Repeats.AddRange(TagRepeatMs.Select(delay => _now + delay));
    }

    /// <summary>A game's hit report: applied once, when the buckets it saw are the server's and exactly one car is It.</summary>
    private void TagCollision(RelayPeer peer, NerdMessage.TagCollision hit)
    {
        if (_tag is not { Assigned: true } tag || _race is not { } race || !race.Participants.Contains(peer.Index)) return;
        var a = TagCar(peer, hit.A);
        var b = TagCar(peer, hit.B);
        // a game reports the hits of its own car; both cars must be in the event
        if (a is null || b is null || a == b || (a != peer && b != peer)
            || !tag.Buckets.TryGetValue(a.Index, out var aBucket) || !tag.Buckets.TryGetValue(b.Index, out var bBucket))
        {
            Note(string.Create(CultureInfo.InvariantCulture, $"tag: hit from peer {peer.Index} ignored (cars not in the event)"));
            return;
        }
        // the other car's game reports the same hit too: once applied, its "before" no longer matches (nor does a stale one)
        if (aBucket != hit.Values[0] || bBucket != hit.Values[2]) return;
        if (tag.IsIt(a.Index) == tag.IsIt(b.Index))
        {
            Note(string.Create(CultureInfo.InvariantCulture, $"tag: hit from peer {peer.Index} ignored (not one It car and one not)"));
            return;
        }
        var (it, other) = tag.IsIt(a.Index) ? (a, b) : (b, a);
        AccountTag(tag);
        tag.Buckets[it.Index] = (byte)tag.Rules.ItBecomes;
        tag.Buckets[other.Index] = (byte)tag.Rules.NotItBecomes;
        // King: the crown is taken; Infected: the It car infects
        var scorer = tag.Rules.Mode == TagMode.King ? other : it;
        tag.Hits[scorer.Index] = tag.Hits.GetValueOrDefault(scorer.Index) + 1;
        var reported = (it == a ? hit.Values[1] : hit.Values[3], other == a ? hit.Values[1] : hit.Values[3]);
        Note(string.Create(CultureInfo.InvariantCulture, $"tag: car {it.Index} tagged car {other.Index}")
            + (reported == (tag.Buckets[it.Index], tag.Buckets[other.Index]) ? string.Empty
                : string.Create(CultureInfo.InvariantCulture, $" (the game reported {reported.Item1}/{reported.Item2}, the event's rules apply)")));
        SendTagBuckets(tag, true);
    }

    /// <summary>The car a key names, as the reporting game knows it: its own car, or another player's by its relay key.</summary>
    private RelayPeer? TagCar(RelayPeer reporter, ObjectHandle key) => key.Kind switch
    {
        HandleKind.Own => reporter,
        HandleKind.Relay => _peers.FirstOrDefault(p => p.Index == key.Peer),
        _ => null,
    };

    /// <summary>A participant left the event: its car leaves the buckets, and a new It car is drawn when it was the last one.</summary>
    private void TagLeft(RelayPeer peer)
    {
        if (_tag is not { } tag) return;
        AccountTag(tag);
        tag.Buckets.Remove(peer.Index);
        if (tag.Assigned) EnsureIt(tag);
    }

    /// <summary>A final score this early in a tag event is not its end.</summary>
    private bool TagTooEarlyToFinish() => _tag is { } tag && (tag.GoAt is not { } go || _now - go < TagEarliestFinishMs);

    /// <summary>Adds the time since the last bucket change to every It car's time (from "GO" on). Called before buckets change.</summary>
    private void AccountTag(TagGame tag)
    {
        if (tag.GoAt is { } go && _now > Math.Max(go, tag.Marked))
        {
            var from = Math.Max(go, tag.Marked);
            foreach (var (index, _) in tag.Buckets.Where(pair => pair.Value == tag.Rules.ItBucket))
                tag.HeldMs[index] = tag.HeldMs.GetValueOrDefault(index) + _now - from;
        }
        tag.Marked = _now;
    }

    /// <summary>The event closed: its tag results (peer indexes only) go to the event log.</summary>
    private void TagEnded()
    {
        if (_tag is not { } tag) return;
        _tag = null;
        if (!tag.Assigned) return;
        AccountTag(tag);
        var winner = tag.HeldMs.OrderByDescending(pair => pair.Value).ThenBy(pair => pair.Key).Select(pair => (byte?)pair.Key).FirstOrDefault();
        var survivors = tag.Buckets.Where(pair => pair.Value != tag.Rules.ItBucket).Select(pair => pair.Key).Order().ToArray();
        var hits = tag.Hits.Values.Sum();
        LastTagResult = new(tag.Rules.Mode, tag.Rules.Mode == TagMode.King ? winner : null, new Dictionary<byte, long>(tag.HeldMs), hits, survivors);
        if (tag.Rules.Mode == TagMode.King)
        {
            var longest = winner is { } w
                ? string.Create(CultureInfo.InvariantCulture, $"car {w} ({tag.HeldMs[w] / 1000.0:0.0} s)") : "nobody";
            Note(string.Create(CultureInfo.InvariantCulture, $"tag: results: King, longest crown {longest}, crowns taken {hits}"));
        }
        else
        {
            var left = survivors.Length != 0 ? "cars " + string.Join(",", survivors) : "none";
            Note(string.Create(CultureInfo.InvariantCulture, $"tag: results: Infected, {hits} infection(s), survivors: {left}"));
        }
    }

    /// <summary>The last tag event's result (peer indexes only); null before one ended.</summary>
    internal TagResult? LastTagResult { get; private set; }
}

/// <summary>
/// A tag event's result: King's winner is the car that held the crown longest (<see cref="HeldMs"/>, from "GO"); Hits are crowns
/// taken (King) or infections made (Infected); Survivors are the cars never infected (Infected) or not holding a crown at the end.
/// </summary>
internal sealed record TagResult(TagMode Mode, byte? Winner, IReadOnlyDictionary<byte, long> HeldMs, int Hits, IReadOnlyList<byte> Survivors);
