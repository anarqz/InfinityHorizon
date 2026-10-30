// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

/// <summary>Single-owner, deterministic game state. All time and datagram output come from its caller.</summary>
internal sealed partial class RelayEngine
{
    private readonly RelaySettings _settings;
    private readonly Action<RelayDatagram> _send;
    private readonly List<RelayPeer> _peers = [];
    private readonly Dictionary<(IPAddress Address, ulong Identity), (long At, int Count)> _handshakes = [];
    private readonly Dictionary<IPAddress, long> _bans = [];
    private readonly Dictionary<(IPEndPoint Endpoint, uint Attempt), long> _retiredAttempts = [];
    private readonly Queue<((IPEndPoint Endpoint, uint Attempt) Key, long At)> _retiredOrder = [];
    private const int MaxRetiredAttempts = 1024;
    private const long RetiredAttemptMs = 60000;
    private readonly Dictionary<byte, (ulong Value, int Width)> _hostVariables = [];
    private byte _nextIndex = 1;
    private byte _hostSequence;
    private int _groupNode;
    private long _now, _lastKeepalive, _lastResend, _lastPace, _lastCarStates;
    private bool _closed;
    internal Func<ulong, IPEndPoint, long, bool>? Authorize { get; set; }
    internal Func<ulong, IPEndPoint, long, bool>? AuthorizeHook { get; set; }
    internal Action<ulong, string, int?, int?, int?>? ValidationRejected { get; set; }
    /// <summary>An OpenConnection past the handshake rate: peer index (0 when refused), A (the lobby type), B, refusal reason or null.</summary>
    internal Action<byte, uint, uint, byte?>? ConnectionOpened { get; set; }
    /// <summary>Event lifecycle lines (ownership, race requests, stages, readiness), peer indexes only: no identity or address.</summary>
    internal Action<string>? WorldLog { get; set; }
    private const int NotesPerWindow = 40;
    private const long NoteWindowMs = 10000;
    private long _noteWindow = long.MinValue;
    private int _notes, _notesDropped;

    // Bounded: a client repeating a request cannot flood the launcher log.
    private void Note(string line)
    {
        if (WorldLog is not { } log) return;
        if (_noteWindow == long.MinValue || _now - _noteWindow >= NoteWindowMs)
        {
            if (_notesDropped != 0) log(string.Create(CultureInfo.InvariantCulture, $"{_notesDropped} event lines suppressed"));
            _noteWindow = _now; _notes = 0; _notesDropped = 0;
        }
        if (++_notes > NotesPerWindow) { _notesDropped++; return; }
        log(line);
    }

    private void Rejected(RelayPeer peer, string kind, int? message = null, int? router = null, int? value = null)
        => ValidationRejected?.Invoke(peer.Xuid, kind, message, router, value);
    internal IReadOnlyList<ulong> PeerXuids => _peers.Where(p => p.Synced).Select(p => p.Xuid).ToArray();
    internal int PeerCount => _peers.Count;
    internal int SyncedCount => _peers.Count(p => p.Synced);

    internal RelayEngine(RelaySettings? settings, Action<RelayDatagram> send)
    {
        _settings = settings ?? new();
        if (_settings.MaxPlayers is < 1 or > 12 || _settings.MaxPeersPerIp < 1 || _settings.MaxUnsynced is < 1 or > 254
            || _settings.RatePackets < 1 || _settings.RateMessages < 1 || _settings.RateBytes < 1
            || _settings.HistoryMaxBits < 1 || _settings.QueueMaxBits < 1 || _settings.QueueMaxMessages < 1
            || _settings.ReliableWindow is < 1 or > 256 || _settings.RacePrepareMs < 1 || _settings.RaceGridMs < 1
            || _settings.RaceFinishMs < 1 || _settings.GameOptionsGapMs < 1 || _settings.SyncCompleteMs < 1 || _settings.ResendAfterMs < 1
            || _settings.CarMeetLobby is { } lobby && !IsCarMeetLobby(lobby))
            throw new ArgumentOutOfRangeException(nameof(settings));
        _send = send ?? throw new ArgumentNullException(nameof(send));
        // SessionHostSynced var 0 LobbyType (3 bits); its constructor default is 2, free roam. The game loads the session's
        // entities for this value, so a Road Trip world must say 1, or the client never runs its Road Trip states
        if (_settings.CarMeetLobby is { } carMeet) _hostVariables[0] = (carMeet, 3);
        else if (_settings.RoadTrip is not null) _hostVariables[0] = (RoadTripLobby, 3);
        if (_settings.TimeSyncMs < 1 || _settings.InviteCloseMs < 0) throw new ArgumentOutOfRangeException(nameof(settings));
        _weather = new(_settings.WeatherSeed ?? RandomNumberGenerator.GetInt32(int.MaxValue), _now, NowWire);
        StartRoadTrip();
    }

    private readonly RelayWeather _weather;
    private byte _weatherSequence;

    private void TickWeather()
    {
        if (!_peers.Any(p => p.Synced) || !_weather.Tick(_now, NowWire)) return;
        _weatherSequence = AckWindow.SequenceNext(_weatherSequence);
        var update = NerdCodec.Write(new NerdMessage.Object(false, 10, new(HandleKind.Own, 0), 0, _weatherSequence, _weather.Payload()));
        foreach (var peer in _peers.Where(p => p.Synced)) SendSequenced(peer, [update]);
        Note($"weather: level {_weather.Level}, ramp to {_weather.AtmosphereDesired:0.000}, rain to {_weather.RainDesired:0}");
    }

    /// <summary>OpenConnection's A: 3 is a car meet, 6 the island's car meet (the same world kind).</summary>
    internal static bool IsCarMeetLobby(uint lobby) => lobby is 3 or 6;

    /// <summary>The lobby type (A) a datagram's first message asks for, when it is an OpenConnection.</summary>
    internal static uint? LobbyTypeOf(ReadOnlySpan<byte> data)
    {
        if (StunCodec.IsStun(data) || NerdHeader.Parse(data) is not { Count: > 0 }) return null;
        var reader = new BitReader(data[NerdHeader.Length..]);
        return NerdCodec.TryRead(ref reader, out var first, out _) && first is NerdMessage.OpenConnection open ? open.A : null;
    }

    /// <summary>
    /// When the newest synced player reaching this world over loopback synced (the host's own game; OD-150, its activity),
    /// on the caller's clock; null: none.
    /// </summary>
    internal long? LoopbackSyncedAt => _peers.Where(p => p.Synced && IsLoopback(p.Endpoint.Address)).Select(p => (long?)p.JoinedAt).Max();

    private static bool IsLoopback(IPAddress address) => IPAddress.IsLoopback(address.IsIPv4MappedToIPv6 ? address.MapToIPv4() : address);

    internal bool HasPeer(IPEndPoint endpoint)
    {
        endpoint = StunCodec.Canonical(endpoint);
        return _peers.Any(p => p.Endpoint.Equals(endpoint));
    }

    /// <summary>The endpoint's game moved to another world on this host: its seat here goes, without a Close to the game.</summary>
    internal void Forget(IPEndPoint endpoint, long nowMilliseconds)
    {
        _now = Math.Max(_now, nowMilliseconds);
        endpoint = StunCodec.Canonical(endpoint);
        _heldOpens.Remove(endpoint);
        foreach (var peer in _peers.Where(p => p.Endpoint.Equals(endpoint)).ToArray()) Drop(peer, "moved to another world on this host");
    }

    internal void Receive(ReadOnlySpan<byte> data, IPEndPoint endpoint, long nowMilliseconds)
    {
        if (_closed) return;
        _now = Math.Max(_now, nowMilliseconds);
        endpoint = StunCodec.Canonical(endpoint);
        if (StunCodec.IsStun(data)) { ReceiveSideChannel(data, endpoint); return; }
        try { ReceivePacket(data, endpoint); }
        catch (WireFormatException) { /* malformed input ends this datagram only */ }
        DropDoomed();
    }

    internal void Kick(ulong xuid, long nowMilliseconds)
    {
        _now = Math.Max(_now, nowMilliseconds);
        foreach (var peer in _peers.Where(p => p.Xuid == xuid).ToArray()) Drop(peer, "kicked");
    }

    internal void Clear()
    {
        _closed = true;
        _peers.Clear(); _handshakes.Clear(); _bans.Clear(); _hostVariables.Clear();
        _retiredAttempts.Clear(); _retiredOrder.Clear(); _heldOpens.Clear();
        ClearGameOptions();
        ClearSideChannels();
        _eventSide.Clear(); _phasing = false;
    }

    private uint NowWire => unchecked((uint)_now + 1);
    private static bool SharedXuid(ulong xuid) => xuid is 0 or 0x0009000000000000;
    private bool Allowed(ulong xuid, IPEndPoint endpoint) => Authorize?.Invoke(xuid, endpoint, _now) ?? true;
    private bool AllowedHook(ulong xuid,IPEndPoint endpoint) => (AuthorizeHook ?? Authorize)?.Invoke(xuid,endpoint,_now) ?? true;
    private bool HookSource(RelayPeer peer,IPEndPoint endpoint) => AuthorizeHook is not null
        ? AllowedHook(peer.Xuid,endpoint) : MaySpeakFor(peer.Endpoint,endpoint);
    private bool RetiredAttempt(IPEndPoint endpoint, uint attempt)
    {
        while (_retiredOrder.TryPeek(out var item) && _now - item.At >= RetiredAttemptMs)
        { _retiredOrder.Dequeue(); _retiredAttempts.Remove(item.Key); }
        return _retiredAttempts.ContainsKey((endpoint, attempt));
    }

    private void RetireAttempt(RelayPeer peer)
    {
        if (RetiredAttempt(peer.Endpoint, peer.ClientAttempt)) return;
        while (_retiredOrder.Count >= MaxRetiredAttempts) _retiredAttempts.Remove(_retiredOrder.Dequeue().Key);
        var key = (peer.Endpoint, peer.ClientAttempt);
        _retiredAttempts.Add(key, _now); _retiredOrder.Enqueue((key, _now));
    }
    private bool HandshakeAllowed(IPEndPoint endpoint, ulong xuid)
    {
        // An installed policy has already verified this exact identity/endpoint. Sharing a NAT
        // must not collapse all authorized people into one four-attempt bucket.
        var key = (endpoint.Address, Authorize is not null && Allowed(xuid, endpoint) ? xuid : 0);
        if (_handshakes.TryGetValue(key, out var bucket))
        {
            bucket = _now - bucket.At >= 1000 ? (_now, 1) : (bucket.At, bucket.Count + 1);
            _handshakes[key] = bucket;
            return bucket.Count <= _settings.HandshakesPerSecond;
        }
        foreach (var expired in _handshakes.Where(p => _now - p.Value.At >= 1000).Select(p => p.Key).ToArray()) _handshakes.Remove(expired);
        if (_handshakes.Count >= 1024) _handshakes.Remove(_handshakes.Keys.First());
        _handshakes[key] = (_now, 1);
        return _settings.HandshakesPerSecond >= 1;
    }

    private RelayPeer? Admit(IPEndPoint endpoint, NerdHeader header, NerdMessage.OpenConnection open, ReadOnlySpan<byte> data)
    {
        var bytes = data.Length;
        if (!HandshakeAllowed(endpoint, open.User)) return null;
        // a seat whose endpoint proof may still come (a game moving to a new socket): no final refusal, the open waits for it
        if (!Allowed(open.User, endpoint) && Hold(endpoint, header, open, data)) return null;
        byte? reason = !Allowed(open.User, endpoint) ? (byte)4
            : _bans.GetValueOrDefault(endpoint.Address) > _now ? (byte)3
            : !open.Version.AsSpan().SequenceEqual(NerdCodec.Version) ? (byte)1
            : !open.Game.AsSpan().SequenceEqual(NerdCodec.Game) ? (byte)2
            : _peers.Count(p => !p.Synced && p.Endpoint.Address.Equals(endpoint.Address)) >= _settings.MaxPeersPerIp
                || _peers.Count(p => !p.Synced) >= _settings.MaxUnsynced
                || (SyncedCount >= _settings.MaxPlayers && (SharedXuid(open.User) || !_peers.Any(p => p.Synced && p.Xuid == open.User))) ? (byte)4 : null;
        if (reason is { } refusal)
        {
            Refuse(endpoint, header, open, bytes, refusal);
            return null;
        }
        byte index = 0;
        for (var i = 0; i < 254; i++)
        {
            var candidate = (byte)((_nextIndex - 1 + i) % 254 + 1);
            if (_peers.All(p => p.Index != candidate)) { index = candidate; _nextIndex = (byte)(candidate % 254 + 1); break; }
        }
        if (index == 0) return null;
        var attempt = BitConverter.ToUInt32(RandomNumberGenerator.GetBytes(4));
        var peer = new RelayPeer(endpoint, index, open.User, attempt, _now)
        { ClientAttempt = open.Attempt, PacketTokens = _settings.RatePackets * 2.0, MessageTokens = _settings.RateMessages * 2.0, ByteTokens = _settings.RateBytes * 2.0 };
        _peers.Add(peer);
        ConnectionOpened?.Invoke(index, open.A, open.B, null);
        return peer;
    }

    private void Refuse(IPEndPoint endpoint, NerdHeader header, NerdMessage.OpenConnection open, int bytes, byte refusal)
    {
        ConnectionOpened?.Invoke(0, open.A, open.B, refusal);
        var response = NerdCodec.Packet([NerdCodec.Write(new NerdMessage.OpenResponse(open.Flags, refusal, open.Attempt, 0))],
            NerdCodec.NoSequence, header.Id == NerdCodec.NoSequence ? NerdCodec.NoAck : header.Id, 0);
        if (response.Length <= bytes) Emit(endpoint, response);
    }

    // An OpenConnection from an endpoint not proven yet, for an identity whose proof may still come (AwaitsProof): kept
    // unanswered, the game's own retries included, and taken in again once that endpoint is allowed (TickHeldOpens). The
    // game takes reason 4 as final (live, test.9: a joiner moving from its Road Trip to free roam on a new socket was
    // refused before its proof completed and never connected again). Admission still needs Allowed: nothing is let in early.
    private sealed record HeldOpen(byte[] Data, NerdHeader Header, NerdMessage.OpenConnection Open, long At);
    private readonly Dictionary<IPEndPoint, HeldOpen> _heldOpens = [];
    internal const long HeldOpenMs = 20000;
    private const int MaxHeldOpens = 32;

    /// <summary>Whether this identity holds a seat whose endpoint proof may still arrive; its unproven OpenConnection waits for it (null: none waits).</summary>
    internal Func<ulong, bool>? AwaitsProof { get; set; }

    /// <summary>A held OpenConnection was admitted after its endpoint proof (not from a datagram): its world now holds the endpoint.</summary>
    internal Action<IPEndPoint>? AdmittedLate { get; set; }

    private bool Hold(IPEndPoint endpoint, NerdHeader header, NerdMessage.OpenConnection open, ReadOnlySpan<byte> data)
    {
        if (SharedXuid(open.User) || AwaitsProof?.Invoke(open.User) != true) return false;
        if (_heldOpens.TryGetValue(endpoint, out var held))
        {
            if (held.Open.User != open.User) return false;
            _heldOpens[endpoint] = new(data.ToArray(), header, open, held.At); // the latest attempt; the wait is not extended
            return true;
        }
        if (_heldOpens.Count >= MaxHeldOpens) return false;
        _heldOpens[endpoint] = new(data.ToArray(), header, open, _now);
        Note(string.Create(CultureInfo.InvariantCulture, $"OpenConnection lobby type {open.A} waits for its endpoint proof"));
        return true;
    }

    private void TickHeldOpens()
    {
        if (_heldOpens.Count == 0) return;
        foreach (var (endpoint, held) in _heldOpens.ToArray())
        {
            if (_peers.Any(p => p.Endpoint.Equals(endpoint))) { _heldOpens.Remove(endpoint); continue; } // a retry got in
            if (Allowed(held.Open.User, endpoint))
            {
                _heldOpens.Remove(endpoint);
                try { ReceivePacket(held.Data, endpoint); }
                catch (WireFormatException) { continue; }
                if (_peers.Any(p => p.Endpoint.Equals(endpoint)))
                {
                    Note(string.Create(CultureInfo.InvariantCulture, $"OpenConnection lobby type {held.Open.A} admitted after {_now - held.At} ms waiting for its proof"));
                    AdmittedLate?.Invoke(endpoint);
                }
                continue;
            }
            if (_now - held.At < HeldOpenMs && AwaitsProof?.Invoke(held.Open.User) == true) continue;
            _heldOpens.Remove(endpoint); // no proof came: the refusal it was spared
            Refuse(endpoint, held.Header, held.Open, held.Data.Length, 4);
        }
    }

    private bool TakeRate(RelayPeer peer, NerdHeader header, int length)
    {
        var elapsed = Math.Max(0, _now - peer.BucketAt) / 1000.0;
        peer.PacketTokens = Math.Min(_settings.RatePackets * 2.0, peer.PacketTokens + elapsed * _settings.RatePackets);
        peer.MessageTokens = Math.Min(_settings.RateMessages * 2.0, peer.MessageTokens + elapsed * _settings.RateMessages);
        peer.ByteTokens = Math.Min(_settings.RateBytes * 2.0, peer.ByteTokens + elapsed * _settings.RateBytes);
        peer.BucketAt = _now;
        if (peer.PacketTokens >= 1 && peer.MessageTokens >= header.Count && peer.ByteTokens >= length)
        { peer.PacketTokens--; peer.MessageTokens -= header.Count; peer.ByteTokens -= length; return true; }
        if (peer.OverLast is not { } last || _now - last >= 1500) peer.OverSince = _now;
        peer.OverLast = _now;
        if (_now - peer.OverSince >= _settings.AbuseMs)
        {
            Doom(peer, "over the rate limit");
            foreach (var key in _bans.Where(p => p.Value <= _now).Select(p => p.Key).ToArray()) _bans.Remove(key);
            if (_bans.Count >= 256) _bans.Remove(_bans.Keys.First());
            _bans[peer.Endpoint.Address] = _now + _settings.AbuseBanMs;
        }
        return false;
    }

    private void CompleteHandshake(RelayPeer peer)
    {
        if (!Allowed(peer.Xuid, peer.Endpoint)) { Drop(peer, "not allowed at sync"); return; }
        var old = !SharedXuid(peer.Xuid) ? _peers.FirstOrDefault(p => p != peer && p.Synced && p.Xuid == peer.Xuid) : null;
        if (old is not null)
        {
            if (_now - old.LastSeen < _settings.TakeoverQuietMs) { Drop(peer, $"its player is still active as peer {old.Index}"); return; }
            Drop(old, $"taken over by peer {peer.Index}");
        }
        if (SyncedCount >= _settings.MaxPlayers) { Drop(peer, "world full at sync"); return; }
        peer.Synced = true; peer.JoinedAt = _now;
        SendCompletion(peer);
        var payload = NerdPayload.SessionHost(_hostVariables.Select(p => (p.Key, p.Value.Value, p.Value.Width)).ToArray());
        if (_groupNode != 0) payload.Append(NerdPayload.GroupSatNav(_groupNode));
        var snapshot = new List<BitBuffer>
        {
            NerdCodec.Write(new NerdMessage.Object(true, 6, new(HandleKind.Own, 0), 0, _hostSequence, payload)),
            // the world's shared weather (server-owned WeatherSynced, router 10, id 0) at its current sequence
            NerdCodec.Write(new NerdMessage.Object(true, 10, new(HandleKind.Own, 0), 0, _weatherSequence, _weather.Payload())),
        };
        // The car meet's one server-owned CarMeetSynced (router 11, id 0); the variables it omits keep their constructor defaults
        if (_settings.CarMeetLobby is not null)
            snapshot.Add(NerdCodec.Write(new NerdMessage.Object(true, 11, new(HandleKind.Own, 0), 0, _carMeetSequence, CarMeetState())));
        AppendGameOptionsSnapshot(peer, snapshot);
        AppendRoadTripSnapshot(peer, snapshot);
        foreach (var owner in _peers.Where(p => p != peer && p.Synced))
            foreach (var (key, obj) in owner.Objects) snapshot.AddRange(obj.Snapshot(key.Router, new(HandleKind.Relay, key.Id, owner.Index)));
        foreach (var message in snapshot)
        {
            if (!QueueReliable(peer, message)) break;
        }
        // During an event a newcomer needs a private universe of its own too (it is on the free-roam side)
        if (_phasing) AssignUniverse(peer);
        RoadTripJoined(peer);
        Pace(peer);
    }

    private void SendCompletion(RelayPeer peer)
    {
        peer.LastCompletion = _now; peer.LastTimeSync = _now;
        Send(peer, [NerdCodec.Write(new NerdMessage.TimeSync(peer.SyncIndex++, NowWire)), NerdCodec.Write(new NerdMessage.SyncDone(peer.Attempt))], false);
    }

    /// <summary>The peer leaves this world; <paramref name="reason"/> goes to the world log with its index (never an identity or address).</summary>
    private void Drop(RelayPeer peer, string reason)
    {
        if (!_peers.Remove(peer)) return;
        Note(string.Create(CultureInfo.InvariantCulture, $"peer {peer.Index} dropped: {reason}{(peer.Synced ? string.Empty : " (not synced)")}"));
        var deletes = peer.Objects.Select(p => NerdCodec.Write(new NerdMessage.Delete(p.Key.Router,
            new(HandleKind.Relay, p.Key.Id, peer.Index), AckWindow.SequenceAdd(AckWindow.SequenceNext(p.Value.Last), p.Value.Injected)))).ToArray();
        foreach (var other in _peers.Where(p => p.Synced)) SendSequenced(other, deletes);
        OnPeerLeft(peer);
        if (_peers.Count == 0) _groupNode = 0;
        RacePeerLeft(peer);
        LeaveEventSide(peer);
        CarMeetPeerLeft(peer);
        RoadTripPeerLeft(peer);
    }

    private void DropDoomed()
    {
        foreach (var peer in _peers.Where(p => p.Doomed).ToArray())
        {
            if (peer.Synced && Allowed(peer.Xuid, peer.Endpoint))
            {
                var ack = peer.Acks.Header;
                Emit(peer.Endpoint, NerdCodec.Packet([NerdCodec.Write(new NerdMessage.Close())], NerdCodec.NoSequence, ack.Ack, ack.Bits));
            }
            Drop(peer, peer.DoomedBy ?? "doomed");
        }
    }

    /// <summary>The peer is dropped at the end of this receive or tick; the first reason is kept for the log.</summary>
    private static void Doom(RelayPeer peer, string reason)
    {
        peer.DoomedBy ??= reason;
        peer.Doomed = true;
    }
    private void Emit(IPEndPoint endpoint, byte[] data) => _send(new(endpoint, data));

    private bool MaySend(RelayPeer peer)
    {
        if (!peer.Doomed && Allowed(peer.Xuid, peer.Endpoint)) return true;
        Doom(peer, "not allowed any more");
        return false;
    }

    private void Send(RelayPeer peer, IReadOnlyList<BitBuffer> messages, bool sequenced, int tries = 0)
    {
        if (!MaySend(peer)) return;
        var chunks = new List<BitBuffer>(); var bits = 0;
        void Flush()
        {
            if (chunks.Count == 0) return;
            if (sequenced && peer.InFlight.Count >= _settings.ReliableWindow)
            {
                foreach (var message in chunks) if (!QueueReliable(peer, message)) break;
                chunks.Clear(); bits = 0;
                return;
            }
            var ack = peer.Acks.Header;
            var id = sequenced ? peer.TakePacketId() : NerdCodec.NoSequence;
            var packet = NerdCodec.Packet(chunks, id, ack.Ack, ack.Bits);
            if (peer.Synced || peer.SentBytes + packet.Length <= peer.ReceivedBytes)
            {
                if (!peer.Synced) peer.SentBytes += packet.Length;
                Emit(peer.Endpoint, packet);
                if (sequenced)
                {
                    peer.InFlight.Add(new(id, chunks.ToArray(), _now, tries));
                }
            }
            chunks.Clear(); bits = 0;
        }
        foreach (var message in messages)
        {
            if (chunks.Count != 0 && (chunks.Count >= 8 || (bits + message.Length + 7) / 8 > 1189)) Flush();
            chunks.Add(message); bits += message.Length;
        }
        Flush();
    }

    private void SendAck(RelayPeer peer, List<BitBuffer> messages)
    {
        if (!MaySend(peer)) return;
        if (messages.Count != 0) { Send(peer, messages, false); return; }
        var ack = peer.Acks.Header;
        var packet = NerdCodec.Packet([], NerdCodec.NoSequence, ack.Ack, ack.Bits);
        if (!peer.Synced && peer.SentBytes + packet.Length > peer.ReceivedBytes) return;
        if (!peer.Synced) peer.SentBytes += packet.Length;
        Emit(peer.Endpoint, packet);
    }

    private void SendSequenced(RelayPeer peer, IReadOnlyList<BitBuffer> messages)
    {
        if (!MaySend(peer)) return;
        if (peer.Queue.Count == 0) { Send(peer, messages, true); return; }
        foreach (var message in messages)
        {
            if (!QueueReliable(peer, message)) return;
        }
    }

    private bool QueueReliable(RelayPeer peer, BitBuffer message)
    {
        if (peer.Queue.Count >= _settings.QueueMaxMessages || peer.QueueBits + message.Length > _settings.QueueMaxBits)
        { Doom(peer, "reliable queue full"); return false; }
        peer.Queue.Enqueue(message); peer.QueueBits += message.Length;
        return true;
    }

    private void Pace(RelayPeer peer)
    {
        var budget = 8 * 1024;
        while (peer.InFlight.Count < _settings.ReliableWindow && peer.Queue.Count != 0 && budget > 0)
        {
            var batch = new List<BitBuffer>(); var bits = 0;
            while (batch.Count < 8 && peer.Queue.TryPeek(out var message))
            {
                if (batch.Count != 0 && (bits + message.Length + 7) / 8 > 1189) break;
                bits += message.Length; peer.QueueBits -= message.Length; batch.Add(peer.Queue.Dequeue());
            }
            budget -= (bits + 7) / 8;
            Send(peer, batch, true);
            if (peer.Doomed) return;
        }
    }

    private void Forward(RelayPeer from, List<BitBuffer> messages, bool sequenced)
    {
        if (messages.Count == 0) return;
        foreach (var peer in _peers.Where(p => p != from && p.Synced))
            if (sequenced) SendSequenced(peer, messages); else Send(peer, messages, false);
    }
}
