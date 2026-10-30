// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;

namespace Infinity.Core.WorldServer;

internal sealed record RelaySettings
{
    public int MaxPlayers { get; init; } = 8;
    public bool Races { get; init; }
    /// <summary>A car meet world (lobby type 3, or 6 on the island): SessionHost's LobbyType and a server-owned CarMeetSynced. Null: free roam.</summary>
    public uint? CarMeetLobby { get; init; }
    /// <summary>An online Road Trip world (lobby type 1). Null: not one.</summary>
    public RoadTripSettings? RoadTrip { get; init; }
    /// <summary>The world's weather sequence; null picks a random seed.</summary>
    public int? WeatherSeed { get; init; }
    /// <summary>A TimeSync re-bases the game's network clock (remote cars are timed against it), so it is refreshed only rarely.</summary>
    public long TimeSyncMs { get; init; } = 30000;
    /// <summary>When everybody invited has accepted, the invitation closes this soon instead of waiting out its window.</summary>
    public long InviteCloseMs { get; init; } = 1500;
    public long QuietDropMs { get; init; } = 15000;
    public long KeepaliveMs { get; init; } = 500;
    public long ResendAfterMs { get; init; } = 500;
    public long RaceInviteMs { get; init; } = 15000;
    public long RacePrepareMs { get; init; } = 45000;
    public long RaceGridMs { get; init; } = 45000;
    public long RaceFinishMs { get; init; } = 30000;
    public long GameOptionsGapMs { get; init; } = 15000;
    public long SyncCompleteMs { get; init; } = 10000;
    public int ReliableWindow { get; init; } = 128;
    public int QueueMaxMessages { get; init; } = 4096;
    public long UnsyncedTimeoutMs { get; init; } = 3000;
    // Limits half-open connections; verified/synchronized seats use world capacity instead.
    public int MaxPeersPerIp { get; init; } = 2;
    public int MaxUnsynced { get; init; } = 32;
    public int HandshakesPerSecond { get; init; } = 4;
    public long TakeoverQuietMs { get; init; } = 3000;
    public int RatePackets { get; init; } = 300;
    public int RateMessages { get; init; } = 1000;
    public int RateBytes { get; init; } = 256 * 1024;
    public long AbuseMs { get; init; } = 5000;
    public long AbuseBanMs { get; init; } = 60000;
    public int HistoryMaxBits { get; init; } = 2 * 1024 * 1024;
    public int QueueMaxBits { get; init; } = 8 * 1024 * 1024;
}

internal sealed record RelayDatagram(IPEndPoint Destination, byte[] Data);
internal sealed record ReliablePacket(ushort Id, BitBuffer[] Messages, long SentAt, int Tries);

internal sealed class RelayObject(byte type, byte sequence, BitBuffer payload)
{
    internal byte Type { get; } = type;
    internal byte BaseSequence { get; private set; } = sequence;
    internal BitBuffer Base { get; } = BitBuffer.From(payload.Bytes, payload.Length);
    internal byte Last { get; private set; } = sequence;
    internal byte Injected { get; set; }
    internal Dictionary<byte, bool> ServerVariables { get; } = [];
    /// <summary>A player object's server-set PrivateUniverseId (router 1, var 40), replayed to late joiners; null when 0.</summary>
    internal byte? Universe { get; set; }
    internal List<(byte Sequence, BitBuffer Payload)> Tail { get; } = [];
    internal List<(byte Sequence, BitBuffer Payload)> Pending { get; } = [];
    internal int HistoryBits => Base.Length + Tail.Sum(p => p.Payload.Length) + Pending.Sum(p => p.Payload.Length);

    internal bool Update(byte next, BitBuffer update)
    {
        if (next == AckWindow.SequenceNext(Last))
        {
            Apply(next, update);
            while (Pending.FindIndex(p => p.Sequence == AckWindow.SequenceNext(Last)) is var at && at >= 0)
            {
                var item = Pending[at]; Pending.RemoveAt(at); Apply(item.Sequence, item.Payload);
            }
            return true;
        }
        if (!AckWindow.SequenceNewer(next, Last) || Pending.Any(p => p.Sequence == next)) return false;
        if (Pending.Count >= 16)
        {
            var far = Pending.Select((p, i) => (Ahead: (p.Sequence + 255 - Last) % 255, Index: i)).MaxBy(p => p.Ahead);
            Pending.RemoveAt(far.Index);
        }
        Pending.Add((next, update));
        return true;
    }

    private void Apply(byte next, BitBuffer update)
    {
        Last = next;
        if (Tail.Count == 0 && Base.Length + update.Length <= NerdCodec.MaxPayloadBits)
        { Base.Append(update); BaseSequence = next; return; }
        Tail.Add((next, update));
        var size = Tail.Sum(p => p.Payload.Length);
        while (Tail.Count > 32 || size > 64 * 1024)
        {
            var oldest = Tail[0]; Tail.RemoveAt(0); size -= oldest.Payload.Length;
            if (Base.Length + oldest.Payload.Length <= NerdCodec.MaxPayloadBits) Base.Append(oldest.Payload);
            BaseSequence = oldest.Sequence;
        }
    }

    internal IEnumerable<BitBuffer> Snapshot(byte router, ObjectHandle handle)
    {
        var payload = BitBuffer.From(Base.Bytes, Base.Length);
        if (router == 3 && payload.Length + ServerVariables.Count * 5 > NerdCodec.MaxPayloadBits)
        {
            // A console's history may repeat known variables thousands of times. Its last
            // values are sufficient for a snapshot and leave room for server-owned flags.
            var latest = new SortedDictionary<byte, ulong>();
            foreach (var (index, value) in NerdPayload.ConsoleVariablesOf(payload) ?? []) latest[index] = value;
            payload = new BitBuffer();
            foreach (var (index, value) in latest) payload.Push(index, 4).Push(value, index >= 7 ? 2 : 1);
        }
        foreach (var (index, value) in ServerVariables) payload.Push(index, 4).Boolean(value);
        if (router == 1 && Universe is { } universe)
            payload.Push(RelayEngine.PrivateUniverseVariable, 8).Push(universe, RelayEngine.PrivateUniverseBits);
        yield return NerdCodec.Write(new NerdMessage.Object(true, router, handle, Type, AckWindow.SequenceAdd(BaseSequence, Injected), payload));
        foreach (var (seq, update) in Tail.Concat(Pending))
            yield return NerdCodec.Write(new NerdMessage.Object(false, router, handle, 0, AckWindow.SequenceAdd(seq, Injected), update));
    }
}

internal sealed class RelayPeer(IPEndPoint endpoint, byte index, ulong xuid, uint attempt, long now)
{
    internal IPEndPoint Endpoint { get; } = endpoint;
    internal byte Index { get; } = index;
    internal ulong Xuid { get; } = xuid;
    internal uint Attempt { get; } = attempt;
    internal uint ClientAttempt { get; init; }
    internal bool CompletionConfirmed { get; set; }
    internal long LastCompletion { get; set; }
    internal AckWindow Acks { get; } = new();
    internal ushort NextPacket { get; set; } = 1;
    internal List<ReliablePacket> InFlight { get; } = [];
    internal bool Synced { get; set; }
    internal ushort SyncIndex { get; set; }
    internal long ReceivedBytes { get; set; }
    internal long SentBytes { get; set; }
    internal uint? ConsoleId { get; set; }
    internal byte? ConsoleSequence { get; set; }
    internal byte? ConsoleLatestSequence { get; set; }
    internal bool FinaliseAsked { get; set; }
    internal bool Finalised { get; set; }
    internal Dictionary<uint, (byte Sequence, long At)> LobbySequences { get; } = [];
    internal ulong[] ConsoleValues { get; } = new ulong[9];
    internal Dictionary<byte, byte> ConsoleVariableSequences { get; } = [];
    internal Dictionary<(byte Router, uint Id), RelayObject> Objects { get; } = [];
    internal Queue<NerdMessage.Object> Orphans { get; } = new();
    internal long LastSeen { get; set; } = now;
    internal long JoinedAt { get; set; } = now;
    internal uint? Rtt { get; set; }
    internal double PacketTokens { get; set; }
    internal double MessageTokens { get; set; }
    internal double ByteTokens { get; set; }
    internal long BucketAt { get; set; } = now;
    internal long? OverSince { get; set; }
    internal long? OverLast { get; set; }
    internal Queue<BitBuffer> Queue { get; } = new();
    internal int QueueBits { get; set; }
    internal bool Doomed { get; set; }
    internal CarState? CarState { get; set; }
    internal System.Numerics.Vector3? CarPosition { get; set; }
    /// <summary>The last car prediction, sent again with a newer time when the event boundary moves.</summary>
    internal NerdMessage.Car? LastCar { get; set; }
    /// <summary>This player's private universe while an event exists (0 outside, the shared default).</summary>
    internal byte Universe { get; set; }
    internal byte? PlayerSequence { get; set; }
    internal long LastTimeSync { get; set; }
    internal byte? RoadTripSelectionSequence { get; set; }
    internal bool UniverseDue { get; set; }
    internal IPEndPoint? CarHook { get; set; }
    internal long? CarStateAt { get; set; }
    internal int HistoryBits => Objects.Values.Sum(p => p.HistoryBits) + Orphans.Sum(p => p.Payload.Length);

    internal ushort TakePacketId()
    {
        var result = NextPacket;
        NextPacket = result >= 0xfffd ? (ushort)0 : (ushort)(result + 1);
        return result;
    }
}
