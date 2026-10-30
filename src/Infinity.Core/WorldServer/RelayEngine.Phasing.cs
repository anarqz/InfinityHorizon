// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

/// <summary>
/// Event phasing. While an event exists (from participant selection until every participant is back in free roam) each player has a
/// private universe of its own (LobbyPlayer.PrivateUniverseId, router-1 var 40, 5 bits: 1-17 OnlinePrivate0-16). The game moves its car
/// there on the results, arrival and similar screens, so two finishers are never stacked in one shared universe. Cars seen across the
/// event boundary carry their owner's private universe, so racers and free-roam players do not collide. Outside events everybody is
/// back at 0, the shared default, which keeps free roam (parked bucket-list cars included) as it is.
/// </summary>
internal sealed partial class RelayEngine
{
    internal const byte PrivateUniverseVariable = 40;
    internal const int PrivateUniverseBits = 5;
    internal const byte FirstPrivateUniverse = 1, LastPrivateUniverse = 17;
    // CarPredictionFull: the car's universe (5 bits) and, last, its 32-bit prediction time
    internal const int CarUniverseOffset = 395;
    private const int CarStampOffset = NerdCodec.CarBodyBits - 32;
    // An event side that never empties (a game that never reports leaving its results) still ends
    private const long PhaseHoldMs = 10 * 60 * 1000;

    private readonly HashSet<byte> _eventSide = [];
    private bool _phasing;
    private long _phaseReleasedAt;

    private bool SameSide(RelayPeer a, RelayPeer b) => !_phasing || _eventSide.Contains(a.Index) == _eventSide.Contains(b.Index);

    /// <summary>Participants were selected: they form the event side, and every player gets a private universe.</summary>
    private void BeginPhase(IEnumerable<byte> participants)
    {
        _eventSide.Clear();
        foreach (var index in participants) _eventSide.Add(index);
        if (!_phasing)
        {
            _phasing = true;
            foreach (var peer in _peers.Where(p => p.Synced)) AssignUniverse(peer);
        }
        RefreshCars();
    }

    /// <summary>A player left the event side (back in free roam, dropped, or gone).</summary>
    private void LeaveEventSide(RelayPeer peer)
    {
        if (!_phasing || !_eventSide.Remove(peer.Index)) return;
        if (_peers.Contains(peer)) RefreshCars();
        EndPhaseWhenDone();
    }

    /// <summary>The event closed: the participants still in it (on their results screen) keep the side until they leave it.</summary>
    private void EventClosed(Race race)
    {
        if (!_phasing) return;
        _phaseReleasedAt = _now;
        foreach (var index in race.Participants.ToArray())
            if (_peers.FirstOrDefault(p => p.Index == index) is not { } peer || peer.ConsoleValues[3] == 0) _eventSide.Remove(index);
        RefreshCars();
        EndPhaseWhenDone();
    }

    private void EndPhaseWhenDone()
    {
        if (!_phasing || _race is not null || _eventSide.Count != 0) return;
        _phasing = false;
        Note("phasing: everybody is back in free roam, private universes released");
        foreach (var peer in _peers.Where(p => p.Universe != 0)) SetUniverse(peer, 0);
        RefreshCars();
    }

    private void TickPhasing()
    {
        if (_phasing && _race is null && _now - _phaseReleasedAt >= PhaseHoldMs && _eventSide.Count != 0)
        {
            Note($"phasing: {_eventSide.Count} player(s) never left the results, released");
            _eventSide.Clear();
            EndPhaseWhenDone();
        }
    }

    private void AssignUniverse(RelayPeer peer)
    {
        if (peer.Universe != 0) return;
        for (var universe = FirstPrivateUniverse; universe <= LastPrivateUniverse; universe++)
            if (_peers.All(p => p.Universe != universe)) { SetUniverse(peer, universe); return; }
    }

    /// <summary>The server's value of a player's PrivateUniverseId: to the owner, and to everybody else on its relayed object.</summary>
    private void SetUniverse(RelayPeer peer, byte universe)
    {
        peer.Universe = universe;
        if (!peer.Synced || !_peers.Contains(peer)) return;
        var player = peer.Objects.FirstOrDefault(p => p.Key.Router == 1);
        if (player.Value is not { } obj) return;
        var id = player.Key.Id;
        var payload = new BitBuffer().Push(PrivateUniverseVariable, 8).Push(universe, PrivateUniverseBits);
        var sequence = peer.PlayerSequence is { } previous ? AckWindow.SequenceNext(previous) : (byte)0;
        peer.PlayerSequence = sequence;
        SendSequenced(peer, [NerdCodec.Write(new NerdMessage.Object(false, 1, new(HandleKind.Server, id), 0, sequence, payload))]);
        obj.Injected = AckWindow.SequenceNext(obj.Injected);
        obj.Universe = universe == 0 ? null : universe;
        Forward(peer, [NerdCodec.Write(new NerdMessage.Object(false, 1, new(HandleKind.Relay, id, peer.Index), 0,
            AckWindow.SequenceAdd(obj.Last, obj.Injected), payload))], true);
    }

    /// <summary>A player's router-1 object was created: the universe assigned before it existed reaches it now.</summary>
    private void PlayerObjectCreated(RelayPeer peer)
    {
        peer.PlayerSequence = null;
        if (peer.Universe != 0) SetUniverse(peer, peer.Universe);
    }

    /// <summary>A car prediction for one receiver: across the event boundary it is in its owner's private universe.</summary>
    private BitBuffer CarFor(RelayPeer sender, RelayPeer receiver, NerdMessage.Car car) =>
        NerdCodec.Write(SameSide(sender, receiver) || sender.Universe == 0 ? car : car with { Body = WithBits(car.Body, CarUniverseOffset, PrivateUniverseBits, sender.Universe) });

    /// <summary>A packet's car prediction and car events: events (contacts) stay on their side of the event boundary.</summary>
    private void ForwardCars(RelayPeer sender, NerdMessage.Car? car, List<BitBuffer> events)
    {
        if (car is null && events.Count == 0) return;
        if (car is not null) sender.LastCar = car;
        foreach (var peer in _peers.Where(p => p != sender && p.Synced))
        {
            var same = SameSide(sender, peer);
            var messages = new List<BitBuffer>(events.Count + 1);
            if (car is not null) messages.Add(CarFor(sender, peer, car));
            if (same) messages.AddRange(events);
            if (messages.Count != 0) Send(peer, messages, false);
        }
    }

    /// <summary>
    /// The boundary moved: a parked car would keep its old universe until its next prediction, so its last one is sent again with a
    /// one-tick newer time (the game ignores a prediction that is not newer than the last it applied).
    /// </summary>
    private void RefreshCars()
    {
        foreach (var sender in _peers.Where(p => p.Synced && p.LastCar is not null))
        {
            var car = sender.LastCar!;
            var reader = new BitReader(car.Body.Bytes, car.Body.Length);
            reader.Skip(CarStampOffset);
            var stamp = unchecked((uint)reader.Read(32) + 1);
            sender.LastCar = car = car with { Body = WithBits(car.Body, CarStampOffset, 32, stamp) };
            foreach (var peer in _peers.Where(p => p != sender && p.Synced)) Send(peer, [CarFor(sender, peer, car)], false);
        }
    }

    private static BitBuffer WithBits(BitBuffer body, int offset, int width, ulong value)
    {
        var reader = new BitReader(body.Bytes, body.Length);
        var output = new BitBuffer();
        output.Append(reader.Take(offset));
        reader.Skip(width);
        output.Push(value, width);
        output.Append(reader.Take(reader.Left));
        return output;
    }
}
