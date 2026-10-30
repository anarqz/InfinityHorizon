// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;

namespace Infinity.Core.WorldServer;

internal sealed partial class RelayEngine
{
    private void ReceivePacket(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        if (NerdHeader.Parse(data) is not { } header) return;
        var reader = new BitReader(data[NerdHeader.Length..]);
        var probe = reader;
        NerdMessage? first = null;
        var hasFirst = header.Count > 0 && NerdCodec.TryRead(ref probe, out first, out _);
        if (first is NerdMessage.OpenConnection original && RetiredAttempt(endpoint, original.Attempt)) return;
        var peer = _peers.FirstOrDefault(p => p.Endpoint.Equals(endpoint));
        if (peer is not null && header.Id == 0 && first is NerdMessage.OpenConnection restart)
        {
            if (restart.User != peer.Xuid || !restart.Version.AsSpan().SequenceEqual(NerdCodec.Version)
                || !restart.Game.AsSpan().SequenceEqual(NerdCodec.Game)) return;
            if (restart.Attempt != peer.ClientAttempt) { RetireAttempt(peer); Drop(peer, "the game reconnected (new attempt)"); peer = null; }
        }
        var admitted = false;
        if (peer is null)
        {
            if (!hasFirst || first is not NerdMessage.OpenConnection open) return;
            peer = Admit(endpoint, header, open, data);
            if (peer is null) return;
            admitted = true;
        }
        if (!Allowed(peer.Xuid, endpoint)) { Drop(peer, "not allowed"); return; }
        if (!TakeRate(peer, header, data.Length)) return;
        peer.LastSeen = _now;
        if (!peer.Synced) peer.ReceivedBytes += data.Length;
        peer.InFlight.RemoveAll(p => AckWindow.Acknowledges(header.Ack, header.AckBits, p.Id));
        if (!admitted && header.Id == 0 && first is NerdMessage.OpenConnection repeated && repeated.Attempt == peer.ClientAttempt)
        {
            if (HandshakeAllowed(endpoint, peer.Xuid))
                Send(peer, [NerdCodec.Write(new NerdMessage.OpenResponse(repeated.Flags, 0, repeated.Attempt, peer.Attempt))], false);
            return;
        }
        var fresh = peer.Acks.Accept(header.Id);
        if (!fresh)
        {
            if (peer.Synced && !peer.CompletionConfirmed && first is NerdMessage.OpenResponse { Reason: 6 } proof
                && proof.Attempt == peer.Attempt && _now - peer.LastCompletion >= _settings.ResendAfterMs) SendCompletion(peer);
            SendAck(peer, []);
            return;
        }
        var answers = new List<BitBuffer>(); var reliable = new List<BitBuffer>();
        var forward = new List<BitBuffer>(); var cars = new List<BitBuffer>(); var syncedNow = false;
        NerdMessage.Car? carNow = null;
        var synced = peer.Synced;
        for (var i = 0; i < header.Count && reader.Left >= 7; i++)
        {
            if (!NerdCodec.TryRead(ref reader, out var message, out _)) { Rejected(peer, "truncated_packet"); break; }
            switch (message)
            {
                case NerdMessage.OpenConnection open:
                    if (open.User != peer.Xuid) break;
                    if (!admitted && !HandshakeAllowed(endpoint, peer.Xuid)) break;
                    admitted = false;
                    answers.Add(NerdCodec.Write(new NerdMessage.OpenResponse(open.Flags, 0, open.Attempt, peer.Attempt)));
                    break;
                case NerdMessage.OpenResponse response:
                    if (!synced && response.Reason == 6 && response.Attempt == peer.Attempt) syncedNow = true;
                    else if (synced && !peer.CompletionConfirmed && response.Reason == 6 && response.Attempt == peer.Attempt
                        && _now - peer.LastCompletion >= _settings.ResendAfterMs) SendCompletion(peer);
                    break;
                case NerdMessage.Close: Drop(peer, "the game closed its connection"); return;
                case NerdMessage.Ping ping: answers.Add(NerdCodec.Write(new NerdMessage.Pong(ping.Time, NowWire))); break;
                case NerdMessage.Pong pong:
                    if (pong.Echo == 0 && pong.Time <= NowWire && NowWire - pong.Time < 60000) peer.Rtt = NowWire - pong.Time;
                    break;
                case var _ when !synced: break;
                case NerdMessage.Object obj:
                    if (OnObject(peer, obj) is { } relayed) { peer.CompletionConfirmed = true; forward.Add(relayed); }
                    break;
                case NerdMessage.Delete deleted when deleted.Handle.Kind == HandleKind.Own && deleted.Router is >= 1 and <= 3:
                    peer.Objects.Remove((deleted.Router, deleted.Handle.Id), out var deletedObject);
                    var kept = peer.Orphans.Where(p => p.Router != deleted.Router || p.Handle.Id != deleted.Handle.Id).ToArray();
                    peer.Orphans.Clear(); foreach (var item in kept) peer.Orphans.Enqueue(item);
                    forward.Add(NerdCodec.Write(deleted with { Handle = new(HandleKind.Relay, deleted.Handle.Id, peer.Index),
                        Sequence = AckWindow.SequenceAdd(deleted.Sequence, deletedObject?.Injected ?? 0) }));
                    break;
                case NerdMessage.Car car when car.Handle.Kind == HandleKind.Own && NerdPayload.CarPosition(car.Body) is { } position:
                    peer.CompletionConfirmed = true;
                    peer.CarPosition = position;
                    peer.CarId = car.Handle.Id;
                    carNow = car with { Handle = new(HandleKind.Relay, car.Handle.Id, peer.Index) };
                    break;
                case NerdMessage.Car car when car.Handle.Kind == HandleKind.Own:
                    Rejected(peer, "invalid_car", 15); break;
                case NerdMessage.CarEvent carEvent when carEvent.Handle.Kind == HandleKind.Own && NerdPayload.CarEventOk(carEvent.Kind, carEvent.Body):
                    var change = NerdCodec.Write(carEvent with { Handle = new(HandleKind.Relay, carEvent.Handle.Id, peer.Index) });
                    if (carEvent.Kind == 35) forward.Add(change); else cars.Add(change);
                    break;
                case NerdMessage.CarEvent carEvent when carEvent.Handle.Kind == HandleKind.Own:
                    Rejected(peer, "invalid_car_event", carEvent.Kind); break;
                case NerdMessage.GroupSatNav nav when nav.Node is >= 0 and <= NerdCodec.SatNavNodeMax:
                    peer.CompletionConfirmed = true; SetGroupNode(nav.Node); break;
                case NerdMessage.Other { Kind: 26 } invite: InviteRequest(peer, invite.Body); break;
                case NerdMessage.Other { Kind: 27 } answer: JoinOrCancel(peer, answer.Body); break;
                case NerdMessage.Other { Kind: 47 } vote when IsRoadTrip: RoadTripVote(peer, vote.Body); break;
                case NerdMessage.Other { Kind: 51 } when IsRoadTrip: RoadTripConfigured(peer); break;
                case NerdMessage.Other other when _settings.Races:
                    switch (other.Kind)
                    {
                        case 10: reliable.AddRange(Ownership(peer, other.Body)); break;
                        case 38: RequestRace(peer); break;
                        case 41: if (NerdPayload.ReadyRequest(other.Body) is { } ready) Ready(peer, ready); break;
                        case 43 or 44 or 45:
                            if (PassScore(peer, other.Kind, other.Body) && other.Kind == 45) Finished(peer);
                            break;
                    }
                    break;
                case NerdMessage.TagCollision hit when _settings.Races: TagCollision(peer, hit); break;
                case NerdMessage.FinaliseSessionEntry: peer.CompletionConfirmed = true; peer.FinaliseAsked = true; break;
                // Server-only, traffic, collidable, chat and other layouts are parsed but never relayed.
            }
        }
        if (peer.HistoryBits > _settings.HistoryMaxBits) { Doom(peer, "object history over its limit"); return; }
        Forward(peer, forward, true); ForwardCars(peer, carNow, cars);
        if (peer.UniverseDue) { peer.UniverseDue = false; PlayerObjectCreated(peer); }
        if (Finalise(peer) is { } finalised) reliable.Add(finalised);
        if (reliable.Count != 0) SendSequenced(peer, reliable);
        RaceStep();
        if (answers.Count != 0 || header.Id != NerdCodec.NoSequence) SendAck(peer, answers);
        if (syncedNow) CompleteHandshake(peer);
    }

    private BitBuffer? OnObject(RelayPeer peer, NerdMessage.Object obj)
    {
        if (_settings.Races && obj.Router == 6 && obj.Handle.Kind == HandleKind.Transfer)
        { OnGameOptions(peer, obj); return null; }
        // the game makes its RoadTripPlayerSynced in every session type (free roam and car meets too): relayed in every world
        var roadTripPlayer = obj.Router == 9 && obj.Handle.Kind == HandleKind.Own && obj.Payload.Length <= RoadTripPlayerMaxBits
            && (obj.Create ? obj.Type == 2 : peer.Objects.ContainsKey((9, obj.Handle.Id)));
        if (!roadTripPlayer)
        {
            if (obj.Handle.Kind != HandleKind.Own || obj.Router is < 1 or > 3 || obj.Type != 0)
            {
                if (obj.Router == 9) Note($"road trip: peer {peer.Index} router-9 object refused (create {obj.Create}, type {obj.Type}, {obj.Handle.Kind} id {obj.Handle.Id}, {obj.Payload.Length} bits)");
                return null;
            }
            if (!NerdPayload.TrySanitize(obj.Router, obj.Payload, out var payload, out var fault))
            { Rejected(peer, "invalid_payload", obj.Id, obj.Router, fault?.Value); return null; }
            obj = obj with { Payload = payload! };
            if (obj.Router == 1 && !obj.Create && obj.Payload.Length >= RoadTripCarChoiceBits) RoadTripCarPicked(peer);
            if (obj.Router == 1) ObserveCarChoice(peer, obj.Payload);
        }
        else
        {
            if (obj.Create && IsRoadTrip) Note($"road trip: peer {peer.Index} created its RoadTripPlayerSynced (router 9, type 2, id {obj.Handle.Id}): accepted");
            RoadTripPlayerUpdate(peer, obj.Payload);
        }
        var key = (obj.Router, obj.Handle.Id);
        var previous = peer.Objects.GetValueOrDefault(key);
        if (obj.Create)
        {
            if (previous is not null && !AckWindow.SequenceNewer(obj.Sequence, previous.Last)) return null;
            peer.Objects.Remove(key);
            if (peer.Objects.Count < 256)
            {
                var (router, id, index) = (obj.Router, obj.Handle.Id, peer.Index);
                var history = new RelayObject(obj.Type, obj.Sequence, obj.Payload, router,
                    line => Note(string.Create(CultureInfo.InvariantCulture, $"peer {index} router {router} object {id}: {line}")));
                var orphans = peer.Orphans.ToArray(); peer.Orphans.Clear();
                foreach (var orphan in orphans)
                    if ((orphan.Router, orphan.Handle.Id) == key) history.Update(orphan.Sequence, orphan.Payload);
                    else peer.Orphans.Enqueue(orphan);
                peer.Objects[key] = history;
            }
            // the server's PrivateUniverseId follows the create, once it has reached the others
            if (obj.Router == 1) peer.UniverseDue = true;
            if (obj.Router == 3 && peer.ConsoleId != obj.Handle.Id) { peer.ConsoleId = obj.Handle.Id; peer.ConsoleSequence = null; }
            if (obj.Router == 3) { peer.ConsoleVariableSequences.Clear(); peer.ConsoleLatestSequence = obj.Sequence; }
        }
        else if (previous is not null)
        { if (!previous.Update(obj.Sequence, obj.Payload)) return null; }
        else
        {
            if (peer.Orphans.Any(p => p.Router == obj.Router && p.Handle.Id == obj.Handle.Id && p.Sequence == obj.Sequence)) return null;
            if (peer.Orphans.Count >= 32) peer.Orphans.Dequeue();
            peer.Orphans.Enqueue(obj);
        }
        if (obj.Router == 3 && peer.ConsoleId == obj.Handle.Id)
        {
            if (peer.ConsoleLatestSequence is not { } latest || AckWindow.SequenceNewer(obj.Sequence, latest))
                peer.ConsoleLatestSequence = obj.Sequence;
            var observations = new List<(byte Variable, ulong Value)>();
            foreach (var (index, value) in NerdPayload.ConsoleVariablesOf(obj.Payload) ?? [])
                if (!peer.ConsoleVariableSequences.TryGetValue(index, out var last) || AckWindow.SequenceNewer(obj.Sequence, last))
                {
                    peer.ConsoleVariableSequences[index] = obj.Sequence;
                    observations.Add((index, value));
                }
            ObserveConsole(peer, observations);
        }
        var injected = peer.Objects.GetValueOrDefault(key)?.Injected ?? 0;
        return NerdCodec.Write(obj with { Handle = new(HandleKind.Relay, obj.Handle.Id, peer.Index), Sequence = AckWindow.SequenceAdd(obj.Sequence, injected) });
    }

    private static BitBuffer? Finalise(RelayPeer peer)
    {
        if (!peer.FinaliseAsked || peer.Finalised || peer.ConsoleId is not { } id) return null;
        peer.Finalised = true;
        var sequence = peer.ConsoleSequence is { } previous ? AckWindow.SequenceNext(previous) : (byte)0;
        peer.ConsoleSequence = sequence;
        return NerdCodec.Write(NerdPayload.ConsoleUpdate(id, sequence, (1, true)));
    }

    private void SetGroupNode(int node)
    {
        if (node == _groupNode) return;
        _groupNode = node; _hostSequence = AckWindow.SequenceNext(_hostSequence);
        var update = NerdCodec.Write(new NerdMessage.Object(false, 6, new(HandleKind.Own, 0), 0, _hostSequence, NerdPayload.GroupSatNav(node)));
        foreach (var peer in _peers.Where(p => p.Synced)) SendSequenced(peer, [update]);
    }

    internal void Tick(long nowMilliseconds)
    {
        if (_closed) return;
        _now = Math.Max(_now, nowMilliseconds);
        TickHeldOpens();
        TickSideChannels(); TickRace(); TickGameOptions(); TickPhasing(); TickWeather(); TickCarMeet(); TickRoadTrip();
        if (_now - _lastResend >= 50)
        {
            _lastResend = _now;
            foreach (var peer in _peers.ToArray())
            {
                if (peer.Synced && !peer.CompletionConfirmed)
                {
                    if (_now - peer.JoinedAt >= _settings.SyncCompleteMs) { Doom(peer, "sync never confirmed"); continue; }
                    if (_now - peer.LastCompletion >= _settings.ResendAfterMs) SendCompletion(peer);
                }
                var wait = Math.Max(_settings.ResendAfterMs, 2L * (peer.Rtt ?? 0));
                var due = peer.InFlight.Where(p => _now - p.SentAt >= wait).ToArray();
                peer.InFlight.RemoveAll(p => _now - p.SentAt >= wait);
                foreach (var packet in due)
                    if (packet.Tries >= 20) Doom(peer, "reliable packet unacknowledged after 20 tries");
                    else Send(peer, packet.Messages, true, packet.Tries + 1);
            }
        }
        if (_now - _lastPace >= 20) { _lastPace = _now; foreach (var peer in _peers) Pace(peer); }
        if (_now - _lastKeepalive >= _settings.KeepaliveMs)
        {
            _lastKeepalive = _now;
            foreach (var peer in _peers.ToArray())
                if (!Allowed(peer.Xuid, peer.Endpoint)) Drop(peer, "not allowed any more");
                else if (_now - peer.LastSeen > _settings.QuietDropMs) Drop(peer, string.Create(System.Globalization.CultureInfo.InvariantCulture, $"quiet for {_now - peer.LastSeen} ms"));
                else if (!peer.Synced && _now - peer.JoinedAt > _settings.UnsyncedTimeoutMs) Drop(peer, "never synced");
                else if (peer.Synced)
                {
                    if (_now - peer.LastTimeSync >= _settings.TimeSyncMs)
                    {
                        peer.LastTimeSync = _now;
                        Send(peer, [NerdCodec.Write(new NerdMessage.Ping(NowWire)), NerdCodec.Write(new NerdMessage.TimeSync(peer.SyncIndex++, NowWire))], false);
                    }
                    else Send(peer, [NerdCodec.Write(new NerdMessage.Ping(NowWire))], false);
                }
        }
        DropDoomed();
    }
}
