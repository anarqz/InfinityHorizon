// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

/// <summary>An online Road Trip world (lobby type 1): the game's events from the host's database and how the vote is drawn.</summary>
internal sealed record RoadTripSettings(RoadTripCatalog Catalog, bool StormIsland, int Seed);

/// <summary>
/// Online Road Trip. The server owns RoadTripSynced (router 9, type 0, id 0; 4-bit var ids) and runs its state: a destination vote,
/// car select, the drive, then the destination's four events one after another, then a new vote. The vote itself is the players'
/// RoadTripEventSelection transfer object (router 9, id 0), filled by the server. Players report where they are on their own
/// RoadTripPlayerSynced (router 9, type 2), which the server relays and reads. Drives are not timed by guessing distances: the first
/// player to arrive gives the others a short time to follow, and everybody arriving ends the drive at once.
/// </summary>
internal sealed partial class RelayEngine
{
    internal enum RoadTripState : byte
    {
        Initialising = 0, DestinationVote = 1, CarSelect = 2, DriveToEvent = 4, ArrivedAtDestination = 5,
        PrepareForEvent = 6, InEvent = 7, FinaliseEvent = 8,
    }

    internal const long RoadTripInitialisingMs = 10000, RoadTripVoteMs = 60000, RoadTripAllVotedMs = 10000;
    /// <summary>
    /// Each player's RoadTripSynced is created in Initialising and changes state this long after: the game leaves its "wait for the
    /// server" step on a state CHANGE, which a late joiner created straight into the current state would never see.
    /// </summary>
    internal const long RoadTripPlayerStartMs = 3000;
    internal const long RoadTripCarSelectMs = 30000, RoadTripAllPickedMs = 3000, RoadTripConfigureMs = 12000;
    internal const long RoadTripDriveCapMs = 300000, RoadTripFirstArrivalMs = 30000, RoadTripDriveHoldMs = 180000;
    internal const long RoadTripPrepareMs = 120000;
    /// <summary>A router-1 update this large during car select is a player's car choice.</summary>
    internal const int RoadTripCarChoiceBits = 1000;
    /// <summary>
    /// LobbyPlayer.PlayerHost, the game's "local player leads" test, on the player object (8-bit var ids): the player object's own
    /// 12 variables come first, so LobbyPlayer's index 1 is 13 (as PrivateUniverseId, its index 28, is 40).
    /// </summary>
    internal const byte RoadTripLeaderVariable = 13;
    /// <summary>The player object's own IsLeader (var 5); var 1 there is HasLivery, never written.</summary>
    internal const byte RoadTripIsLeaderVariable = 5;
    /// <summary>OpenConnection's A for an online Road Trip.</summary>
    internal const uint RoadTripLobby = 1;
    /// <summary>The largest RoadTripPlayerSynced payload relayed (its four variables fit in 64 bits).</summary>
    internal const int RoadTripPlayerMaxBits = 256;

    private sealed class RoadTrip(RoadTripSettings settings)
    {
        internal RoadTripSettings Settings { get; } = settings;
        internal Random Random { get; } = new(settings.Seed);
        internal RoadTripState State { get; set; }
        internal long Until { get; set; } = long.MaxValue;
        internal long FirstSeenAt { get; set; } = -1;
        internal IReadOnlyList<RoadTripDestination>? Plan { get; set; }
        internal Dictionary<byte, int> Votes { get; } = [];
        internal HashSet<byte> Picked { get; } = [];
        internal Dictionary<byte, int> PlayerStates { get; } = [];
        internal int? Winner { get; set; }
        internal int EventIndex { get; set; }
        internal int? LastRace { get; set; }
        internal bool Configured { get; set; }
        internal long ConfigureUntil { get; set; }
        internal bool HoldDrive { get; set; }
        internal long HoldUntil { get; set; }
        internal byte? Leader { get; set; }
        internal int CurrentRace => Plan![Winner!.Value].Events[EventIndex];
    }

    private RoadTrip? _roadTrip;
    private byte _roadTripSequence;
    private bool IsRoadTrip => _roadTrip is not null;

    private void StartRoadTrip()
    {
        if (_settings.RoadTrip is { } settings) _roadTrip = new(settings);
    }

    /// <summary>RoadTripSynced variables: id (4 bits) and value, at their widths.</summary>
    private static BitBuffer RoadTripVariables(params (byte Variable, long Value)[] variables)
    {
        var output = new BitBuffer();
        foreach (var (variable, value) in variables)
        {
            var width = variable switch { 1 => 3, 4 or 5 => 14, 7 => 1, 8 => 4, _ => 32 };
            output.Push(variable, 4).Push(unchecked((ulong)value) & (width == 64 ? ulong.MaxValue : (1UL << width) - 1), width);
        }
        return output;
    }

    /// <summary>The whole RoadTripSynced as it is now (a joiner's create, and the state every update repeats).</summary>
    private BitBuffer RoadTripSnapshotPayload()
    {
        var trip = _roadTrip!;
        var values = new List<(byte, long)> { (8, WireState(trip.State)), (9, trip.Until == long.MaxValue ? 0 : WireAt(trip.Until)) };
        if (trip.Winner is { } winner)
        {
            values.Add((0, trip.Plan![winner].Location));
            values.Add((1, trip.EventIndex));
            values.Add((3, trip.CurrentRace));
            values.Add((2, trip.LastRace ?? 0));
        }
        return RoadTripVariables([.. values]);
    }

    /// <summary>RoadTripSynced.State on the wire for a step (the values a released relay drives the game with; one table to change).</summary>
    internal static long WireState(RoadTripState state) => (long)state;

    private static uint WireAt(long engineMs) => unchecked((uint)engineMs + 1);

    private long? _roadTripStateSent; // the last RoadTripSynced State an update carried, for its one log line per change

    private void RoadTripUpdate(params (byte Variable, long Value)[] variables)
    {
        foreach (var (variable, value) in variables)
        {
            if (variable != 8 || _roadTripStateSent == value) continue;
            _roadTripStateSent = value;
            Note(string.Create(System.Globalization.CultureInfo.InvariantCulture, $"road trip: state {(RoadTripState)(byte)value} ({value}) sent"));
        }
        RoadTripBroadcast(RoadTripVariables(variables));
    }

    // one sequence for the world's RoadTripSynced: every update goes to everybody (a repeat changes nothing for those already there)
    private void RoadTripBroadcast(BitBuffer variables)
    {
        _roadTripSequence = AckWindow.SequenceNext(_roadTripSequence);
        var update = NerdCodec.Write(new NerdMessage.Object(false, 9, new(HandleKind.Own, 0), 0, _roadTripSequence, variables));
        foreach (var peer in _peers.Where(p => p.Synced)) SendSequenced(peer, [update]);
    }

    /// <summary>The players whose own Initialising is over: they are shown the vote.</summary>
    private IEnumerable<RelayPeer> RoadTripShown => RoadTripPlayers.Where(p => p.RoadTripStartAt is null);

    /// <summary>A player's own Initialising ends: the trip's current state reaches it as a change, with the vote when one is drawn.</summary>
    private void StartRoadTripPlayers()
    {
        var trip = _roadTrip!;
        foreach (var peer in RoadTripPlayers.Where(p => p.RoadTripStartAt is { } at && _now >= at).ToArray())
        {
            peer.RoadTripStartAt = null;
            if (trip.State == RoadTripState.Initialising) { Note($"road trip: peer {peer.Index} waits for the vote (still initialising)"); continue; }
            if (trip.Plan is not null) SendSequenced(peer, [EventSelection(peer, false)]);
            RoadTripBroadcast(RoadTripSnapshotPayload());
            Note($"road trip: peer {peer.Index} starts: RoadTripSynced State {WireState(trip.State)}");
        }
    }

    /// <summary>The vote's transfer object for one player (7-bit var ids: the locations, then per destination its events and class).</summary>
    private BitBuffer EventSelection(RelayPeer peer, bool votesOnly)
    {
        var trip = _roadTrip!;
        var payload = new BitBuffer();
        var counts = VoteCounts();
        if (!votesOnly)
        {
            payload.Push(0, 7);
            foreach (var destination in trip.Plan!) payload.Push(unchecked((uint)destination.Location), 32);
        }
        // the vote's blocks (from var 1) and their twin set (from var 31)
        foreach (var first in new[] { 1, 31 })
            for (var b = 0; b < RoadTripCatalog.Destinations; b++)
            {
                var block = first + 10 * b;
                if (!votesOnly)
                {
                    var destination = trip.Plan![b];
                    for (var k = 0; k < RoadTripCatalog.EventsPerDestination; k++)
                    {
                        payload.Push((ulong)(block + 2 * k), 7).Push(unchecked((uint)destination.Events[k]), 32);
                        payload.Push((ulong)(block + 2 * k + 1), 7).Push((ulong)Math.Min(12, destination.MinPlayers[k]), 4);
                    }
                    payload.Push((ulong)(block + 8), 7).Push((ulong)destination.CarClass, 3);
                }
                payload.Push((ulong)(block + 9), 7).Push((ulong)Math.Min(31, counts[b]), 5);
            }
        peer.RoadTripSelectionSequence = peer.RoadTripSelectionSequence is { } previous ? AckWindow.SequenceNext(previous) : (byte)0;
        return NerdCodec.Write(new NerdMessage.Object(false, 9, new(HandleKind.Transfer, 0), 0, peer.RoadTripSelectionSequence.Value, payload));
    }

    private int[] VoteCounts()
    {
        var counts = new int[RoadTripCatalog.Destinations];
        foreach (var (index, choice) in _roadTrip!.Votes)
            if (choice is >= 0 and < RoadTripCatalog.Destinations && _peers.Any(p => p.Index == index && p.Synced)) counts[choice]++;
        return counts;
    }

    private IEnumerable<RelayPeer> RoadTripPlayers => _peers.Where(p => p.Synced);

    /// <summary>A joiner's snapshot: RoadTripSynced as it is, and the vote when one is drawn.</summary>
    private void AppendRoadTripSnapshot(RelayPeer peer, List<BitBuffer> snapshot)
    {
        if (_roadTrip is not { } trip) return;
        if (trip.FirstSeenAt < 0) { trip.FirstSeenAt = _now; trip.Until = _now + RoadTripInitialisingMs; }
        // created in Initialising for every player; its own change follows (StartRoadTripPlayers)
        snapshot.Add(NerdCodec.Write(new NerdMessage.Object(true, 9, new(HandleKind.Own, 0), 0, _roadTripSequence,
            RoadTripVariables((8, WireState(RoadTripState.Initialising)), (9, 0)))));
        peer.RoadTripStartAt = _now + RoadTripPlayerStartMs;
        Note($"road trip: create RoadTripSynced (router 9, type 0, id 0) State 0 for peer {peer.Index}; the trip is in {trip.State}");
    }

    private void RoadTripJoined(RelayPeer peer)
    {
        if (_roadTrip is not { } trip) return;
        ChooseRoadTripLeader();
        // a second player before the vote is drawn: draw it now, for both (it is drawn once, never under an open vote screen)
        if (trip.State == RoadTripState.Initialising && RoadTripPlayers.Count() >= 2) BeginVote();
    }

    private void ChooseRoadTripLeader()
    {
        var trip = _roadTrip!;
        if (trip.Leader is { } leader && _peers.Any(p => p.Index == leader && p.Synced)) return;
        if (RoadTripPlayers.OrderBy(p => p.JoinedAt).FirstOrDefault() is not { } next) { trip.Leader = null; return; }
        trip.Leader = next.Index;
        if (next.Objects.FirstOrDefault(p => p.Key.Router == 1) is { Value: not null } player)
        {
            var sequence = next.PlayerSequence is { } previous ? AckWindow.SequenceNext(previous) : (byte)0;
            next.PlayerSequence = sequence;
            SendSequenced(next, [NerdCodec.Write(new NerdMessage.Object(false, 1, new(HandleKind.Server, player.Key.Id), 0, sequence,
                new BitBuffer().Push(RoadTripIsLeaderVariable, 8).Push(1, 1).Push(RoadTripLeaderVariable, 8).Push(1, 1)))]);
        }
        Note($"road trip: peer {next.Index} leads");
    }

    private void BeginVote()
    {
        var trip = _roadTrip!;
        var players = RoadTripPlayers.Count();
        trip.Plan = trip.Settings.Catalog.Draw(players, trip.Settings.StormIsland, trip.Random);
        if (trip.Plan is null) { Note("road trip: the game database cannot fill a vote"); trip.Until = long.MaxValue; return; }
        trip.Votes.Clear(); trip.Winner = null; trip.EventIndex = 0;
        trip.State = RoadTripState.DestinationVote;
        trip.Until = _now + RoadTripVoteMs;
        foreach (var peer in RoadTripShown) SendSequenced(peer, [EventSelection(peer, false)]);
        RoadTripUpdate((1, 0), (8, WireState(trip.State)), (9, WireAt(trip.Until)));
        Note($"road trip: vote of locations {string.Join(",", trip.Plan.Select(d => d.Location))} for {players} player(s)");
    }

    private void RoadTripVote(RelayPeer peer, BitBuffer body)
    {
        if (_roadTrip is not { State: RoadTripState.DestinationVote } trip) return;
        var reader = new BitReader(body.Bytes, body.Length);
        ObjectHandle.Read(ref reader, 2);
        var choice = (int)reader.Read(32);
        if (choice is < 0 or >= RoadTripCatalog.Destinations) return;
        trip.Votes[peer.Index] = choice;
        foreach (var other in RoadTripShown) SendSequenced(other, [EventSelection(other, true)]);
        if (RoadTripPlayers.All(p => trip.Votes.ContainsKey(p.Index)) && trip.Until > _now + RoadTripAllVotedMs)
        {
            trip.Until = _now + RoadTripAllVotedMs;
            RoadTripUpdate((9, WireAt(trip.Until)));
        }
    }

    private void FinishVote()
    {
        var trip = _roadTrip!;
        var counts = VoteCounts();
        trip.Winner = Array.IndexOf(counts, counts.Max()); // a tie goes to the first of the tied destinations
        trip.State = RoadTripState.CarSelect;
        trip.Until = _now + RoadTripCarSelectMs;
        trip.Picked.Clear();
        // CurrentLocationId must be one of the voted locations: the game looks it up in the vote and would index -1 otherwise
        RoadTripUpdate((0, trip.Plan![trip.Winner.Value].Location), (3, trip.CurrentRace), (8, WireState(trip.State)), (9, WireAt(trip.Until)));
        Note($"road trip: location {trip.Plan[trip.Winner.Value].Location} won ({string.Join("/", counts)}), car select");
        ConfigureNextEvent();
    }

    /// <summary>The leader builds the event's GameOptions (ConfigureGameOptionsForRoadTripEvent 50): the game finds the event by them.</summary>
    private void ConfigureNextEvent()
    {
        var trip = _roadTrip!;
        trip.Configured = false;
        trip.ConfigureUntil = _now + RoadTripConfigureMs + (trip.State == RoadTripState.CarSelect ? RoadTripCarSelectMs : 0);
        ChooseRoadTripLeader();
        if (trip.Leader is not { } leader || _peers.FirstOrDefault(p => p.Index == leader) is not { } peer) return;
        _eventOwner = leader; // its GameOptions are the event's
        SendSequenced(peer, [new BitBuffer().Push(50, 7).Push(unchecked((uint)trip.CurrentRace), 32).Push(1, 32)]);
        Note($"road trip: peer {leader} configures event {trip.CurrentRace}");
    }

    private void RoadTripConfigured(RelayPeer peer)
    {
        if (_roadTrip is { } trip && trip.Leader == peer.Index) trip.Configured = true;
    }

    private void RoadTripCarPicked(RelayPeer peer)
    {
        if (_roadTrip is not { State: RoadTripState.CarSelect } trip || !trip.Picked.Add(peer.Index)) return;
        if (RoadTripPlayers.All(p => trip.Picked.Contains(p.Index)) && trip.Until > _now + RoadTripAllPickedMs)
        {
            trip.Until = _now + RoadTripAllPickedMs;
            RoadTripUpdate((9, WireAt(trip.Until)));
        }
    }

    private void StartDrive()
    {
        var trip = _roadTrip!;
        trip.State = RoadTripState.DriveToEvent;
        foreach (var peer in RoadTripPlayers) trip.PlayerStates[peer.Index] = 0;
        // EventRaceId is the race just finished (0 before the first): after a race the game fast-travels to its finish; the first drive starts\r
        // where the players are. The game starts its route only when StateEndTime goes 0 -> non-zero
        SetHostVariables((4, 1, 1));
        RoadTripUpdate((0, trip.Plan![trip.Winner!.Value].Location), (1, trip.EventIndex), (2, trip.LastRace ?? 0),
            (3, trip.CurrentRace), (8, WireState(trip.State)), (9, 0));
        if (trip.EventIndex == 0) BeginCountdown();
        else
        {
            // after an event the players are still on their results: the countdown waits until they are back on the road
            trip.HoldDrive = true; trip.HoldUntil = _now + RoadTripDriveHoldMs; trip.Until = long.MaxValue;
        }
        Note($"road trip: drive to event {trip.CurrentRace} ({trip.EventIndex + 1}/{RoadTripCatalog.EventsPerDestination})");
    }

    private void BeginCountdown()
    {
        var trip = _roadTrip!;
        trip.HoldDrive = false;
        trip.Until = _now + RoadTripDriveCapMs;
        RoadTripUpdate((9, WireAt(trip.Until)));
    }

    /// <summary>A player's RoadTripPlayerSynced (3-bit var ids: 0 State, 4 bits; 1 CompletedEventCount, 3 bits; 2 XPOnDestinationJoin, s32).</summary>
    private void RoadTripPlayerUpdate(RelayPeer peer, BitBuffer payload)
    {
        if (_roadTrip is not { } trip) return;
        var reader = new BitReader(payload.Bytes, payload.Length);
        try
        {
            while (reader.Left >= 3)
            {
                var variable = reader.Read(3);
                if (variable == 0) trip.PlayerStates[peer.Index] = (int)reader.Read(4);
                else if (variable == 1) reader.Skip(3);
                else if (variable == 2) reader.Skip(32);
                else break; // a layout this server does not read: the object is still relayed as it came
            }
        }
        catch (WireFormatException) { return; }
        RoadTripProgress();
    }

    private int PlayerState(RelayPeer peer) => _roadTrip!.PlayerStates.GetValueOrDefault(peer.Index);

    private void RoadTripProgress()
    {
        var trip = _roadTrip!;
        if (trip.State != RoadTripState.DriveToEvent) return;
        var players = RoadTripPlayers.ToList();
        if (trip.HoldDrive)
        {
            if (players.Count != 0 && players.All(p => PlayerState(p) == (int)RoadTripState.DriveToEvent)) BeginCountdown();
            return;
        }
        var arrived = players.Count(p => PlayerState(p) >= (int)RoadTripState.ArrivedAtDestination);
        if (arrived == 0) return;
        if (arrived == players.Count) { trip.Until = _now; return; }
        if (trip.Until > _now + RoadTripFirstArrivalMs)
        {
            trip.Until = _now + RoadTripFirstArrivalMs;
            RoadTripUpdate((9, WireAt(trip.Until)));
            Note("road trip: first arrival, the others have 30 s");
        }
    }

    private void BeginPrepare()
    {
        var trip = _roadTrip!;
        trip.State = RoadTripState.PrepareForEvent;
        trip.Until = _now + RoadTripPrepareMs;
        RoadTripUpdate((8, WireState(trip.State)), (9, WireAt(trip.Until)));
        RoadTripReady(force: false);
    }

    /// <summary>Everybody ready (Console.Ready), or the time is up with somebody ready: the event starts with those ready.</summary>
    private void RoadTripReady(bool force)
    {
        if (_roadTrip is not { State: RoadTripState.PrepareForEvent } trip || _race is not null) return;
        var players = RoadTripPlayers.ToList();
        var ready = players.Where(p => p.ConsoleValues[4] != 0).ToList();
        if (ready.Count == 0 || !force && ready.Count != players.Count) return;
        var leader = ready.FirstOrDefault(p => p.Index == trip.Leader) ?? ready[0];
        _eventOwner = leader.Index;
        trip.Until = long.MaxValue;
        // the state stays PrepareForEvent until the race begins: a game that saw InEvent while still waiting would leave for free roam
        RoadTripUpdate((9, 0));
        _race = new(leader, _now);
        foreach (var peer in ready.Where(p => p != leader)) _race.Participants.Add(peer.Index);
        SetHostVariables((4, 1, 1), (6, 0, 1));
        Note($"road trip: event {trip.CurrentRace} starts with {ready.Count}/{players.Count}");
        SelectParticipants();
    }

    /// <summary>The event closed (EndRace): the destination's next event, or a new vote after its fourth.</summary>
    private void RoadTripEventClosed()
    {
        if (_roadTrip is not { Winner: not null } trip) return;
        trip.LastRace = trip.CurrentRace;
        if (trip.EventIndex + 1 < RoadTripCatalog.EventsPerDestination)
        {
            trip.EventIndex++;
            trip.State = RoadTripState.FinaliseEvent;
            trip.Until = _now + 6000; // the players finish returning to free roam
            ConfigureNextEvent();
        }
        else
        {
            trip.LastRace = trip.CurrentRace;
            BeginVote();
        }
    }

    private void TickRoadTrip()
    {
        if (_roadTrip is not { } trip) return;
        StartRoadTripPlayers();
        if (trip.State == RoadTripState.DriveToEvent && trip.HoldDrive && _now >= trip.HoldUntil) { BeginCountdown(); return; }
        if (_now < trip.Until) return;
        switch (trip.State)
        {
            case RoadTripState.Initialising when RoadTripPlayers.Any(): BeginVote(); break;
            case RoadTripState.DestinationVote: FinishVote(); break;
            case RoadTripState.CarSelect when trip.Configured || _now >= trip.ConfigureUntil: StartDrive(); break;
            case RoadTripState.FinaliseEvent when trip.Configured || _now >= trip.ConfigureUntil: StartDrive(); break;
            case RoadTripState.DriveToEvent: BeginPrepare(); break;
            case RoadTripState.PrepareForEvent: RoadTripReady(force: true); break;
        }
    }

    private void RoadTripPeerLeft(RelayPeer peer)
    {
        if (_roadTrip is not { } trip) return;
        trip.Votes.Remove(peer.Index); trip.Picked.Remove(peer.Index); trip.PlayerStates.Remove(peer.Index);
        if (trip.Leader == peer.Index) { trip.Leader = null; ChooseRoadTripLeader(); }
        RoadTripProgress();
    }
}
