// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
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
    private RelayContentPolicy? _contentPolicy;
    internal Func<ulong, IPEndPoint, long, bool>? Authorize { get; set; }
    internal Action<ulong, string, int?, int?, int?>? ValidationRejected { get; set; }

    private void Rejected(RelayPeer peer, string kind, int? message = null, int? router = null, int? value = null)
        => ValidationRejected?.Invoke(peer.Xuid, kind, message, router, value);
    internal IReadOnlyList<ulong> PeerXuids => _peers.Where(p => p.Synced).Select(p => p.Xuid).ToArray();
    internal int PeerCount => _peers.Count;
    internal int SyncedCount => _peers.Count(p => p.Synced);

    internal RelayEngine(RelaySettings? settings, Action<RelayDatagram> send, RelayContentPolicy? contentPolicy = null)
    {
        _settings = settings ?? new();
        if (_settings.MaxPlayers is < 1 or > 12 || _settings.MaxPeersPerIp < 1 || _settings.MaxUnsynced is < 1 or > 254
            || _settings.RatePackets < 1 || _settings.RateMessages < 1 || _settings.RateBytes < 1
            || _settings.HistoryMaxBits < 1 || _settings.QueueMaxBits < 1 || _settings.QueueMaxMessages < 1
            || _settings.ReliableWindow is < 1 or > 256 || _settings.RacePrepareMs < 1 || _settings.RaceGridMs < 1
            || _settings.RaceFinishMs < 1 || _settings.GameOptionsGapMs < 1 || _settings.SyncCompleteMs < 1 || _settings.ResendAfterMs < 1)
            throw new ArgumentOutOfRangeException(nameof(settings));
        _send = send ?? throw new ArgumentNullException(nameof(send));
        _contentPolicy = contentPolicy;
    }

    internal void SetContentPolicy(RelayContentPolicy? policy)
    {
        _contentPolicy = policy;
        _liveries.Clear();
        foreach (var peer in _peers) peer.LiveryHook = null;
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
        foreach (var peer in _peers.Where(p => p.Xuid == xuid).ToArray()) Drop(peer);
    }

    internal void Clear()
    {
        _closed = true;
        _peers.Clear(); _handshakes.Clear(); _bans.Clear(); _hostVariables.Clear();
        _retiredAttempts.Clear(); _retiredOrder.Clear();
        ClearGameOptions();
        ClearSideChannels();
    }

    private uint NowWire => unchecked((uint)_now + 1);
    private static bool SharedXuid(ulong xuid) => xuid is 0 or 0x0009000000000000;
    private bool Allowed(ulong xuid, IPEndPoint endpoint) => Authorize?.Invoke(xuid, endpoint, _now) ?? true;
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

    private RelayPeer? Admit(IPEndPoint endpoint, NerdHeader header, NerdMessage.OpenConnection open, int bytes)
    {
        if (!HandshakeAllowed(endpoint, open.User)) return null;
        byte? reason = !Allowed(open.User, endpoint) ? (byte)4
            : _bans.GetValueOrDefault(endpoint.Address) > _now ? (byte)3
            : !open.Version.AsSpan().SequenceEqual(NerdCodec.Version) ? (byte)1
            : !open.Game.AsSpan().SequenceEqual(NerdCodec.Game) ? (byte)2
            : _peers.Count(p => !p.Synced && p.Endpoint.Address.Equals(endpoint.Address)) >= _settings.MaxPeersPerIp
                || _peers.Count(p => !p.Synced) >= _settings.MaxUnsynced
                || (SyncedCount >= _settings.MaxPlayers && (SharedXuid(open.User) || !_peers.Any(p => p.Synced && p.Xuid == open.User))) ? (byte)4 : null;
        if (reason is { } refusal)
        {
            var response = NerdCodec.Packet([NerdCodec.Write(new NerdMessage.OpenResponse(open.Flags, refusal, open.Attempt, 0))],
                NerdCodec.NoSequence, header.Id == NerdCodec.NoSequence ? NerdCodec.NoAck : header.Id, 0);
            if (response.Length <= bytes) Emit(endpoint, response);
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
        return peer;
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
            peer.Doomed = true;
            foreach (var key in _bans.Where(p => p.Value <= _now).Select(p => p.Key).ToArray()) _bans.Remove(key);
            if (_bans.Count >= 256) _bans.Remove(_bans.Keys.First());
            _bans[peer.Endpoint.Address] = _now + _settings.AbuseBanMs;
        }
        return false;
    }

    private void CompleteHandshake(RelayPeer peer)
    {
        if (!Allowed(peer.Xuid, peer.Endpoint)) { Drop(peer); return; }
        var old = !SharedXuid(peer.Xuid) ? _peers.FirstOrDefault(p => p != peer && p.Synced && p.Xuid == peer.Xuid) : null;
        if (old is not null)
        {
            if (_now - old.LastSeen < _settings.TakeoverQuietMs) { Drop(peer); return; }
            Drop(old);
        }
        if (SyncedCount >= _settings.MaxPlayers) { Drop(peer); return; }
        peer.Synced = true; peer.JoinedAt = _now;
        SendCompletion(peer);
        var payload = NerdPayload.SessionHost(_hostVariables.Select(p => (p.Key, p.Value.Value, p.Value.Width)).ToArray());
        if (_groupNode != 0) payload.Append(NerdPayload.GroupSatNav(_groupNode));
        var snapshot = new List<BitBuffer> { NerdCodec.Write(new NerdMessage.Object(true, 6, new(HandleKind.Own, 0), 0, _hostSequence, payload)) };
        AppendGameOptionsSnapshot(peer, snapshot);
        foreach (var owner in _peers.Where(p => p != peer && p.Synced))
            foreach (var (key, obj) in owner.Objects) snapshot.AddRange(obj.Snapshot(key.Router, new(HandleKind.Relay, key.Id, owner.Index)));
        foreach (var message in snapshot)
        {
            if (!QueueReliable(peer, message)) break;
        }
        Pace(peer);
    }

    private void SendCompletion(RelayPeer peer)
    {
        peer.LastCompletion = _now;
        Send(peer, [NerdCodec.Write(new NerdMessage.TimeSync(peer.SyncIndex++, NowWire)), NerdCodec.Write(new NerdMessage.SyncDone(peer.Attempt))], false);
    }

    private void Drop(RelayPeer peer)
    {
        if (!_peers.Remove(peer)) return;
        var deletes = peer.Objects.Select(p => NerdCodec.Write(new NerdMessage.Delete(p.Key.Router,
            new(HandleKind.Relay, p.Key.Id, peer.Index), AckWindow.SequenceAdd(AckWindow.SequenceNext(p.Value.Last), p.Value.Injected)))).ToArray();
        foreach (var other in _peers.Where(p => p.Synced)) SendSequenced(other, deletes);
        OnPeerLeft(peer);
        if (_peers.Count == 0) _groupNode = 0;
        RacePeerLeft(peer);
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
            Drop(peer);
        }
    }
    private void Emit(IPEndPoint endpoint, byte[] data) => _send(new(endpoint, data));

    private bool MaySend(RelayPeer peer)
    {
        if (!peer.Doomed && Allowed(peer.Xuid, peer.Endpoint)) return true;
        peer.Doomed = true;
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
        { peer.Doomed = true; return false; }
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
