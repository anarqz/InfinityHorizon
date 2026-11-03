// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;

namespace Infinity.Core.WorldServer;

internal readonly record struct NerdHeader(ushort Id, ushort Ack, uint AckBits, byte Count, ushort Size)
{
    internal const int Length = 11;
    internal static NerdHeader? Parse(ReadOnlySpan<byte> data) => data.Length < Length ? null :
        new(BinaryPrimitives.ReadUInt16LittleEndian(data), BinaryPrimitives.ReadUInt16LittleEndian(data[2..]),
            BinaryPrimitives.ReadUInt32LittleEndian(data[4..]), data[8], BinaryPrimitives.ReadUInt16LittleEndian(data[9..]));

    internal void Write(Span<byte> data)
    {
        if (data.Length < Length) throw new WireFormatException(WireError.Truncated);
        BinaryPrimitives.WriteUInt16LittleEndian(data, Id);
        BinaryPrimitives.WriteUInt16LittleEndian(data[2..], Ack);
        BinaryPrimitives.WriteUInt32LittleEndian(data[4..], AckBits);
        data[8] = Count;
        BinaryPrimitives.WriteUInt16LittleEndian(data[9..], Size);
    }
}

internal static class NerdCodec
{
    internal const ushort NoSequence = 0xffff;
    /// <summary>
    /// The <c>ack</c> sent while nothing sequenced has been received (an empty <see cref="AckWindow.Header"/>, a refusal to an
    /// unsequenced open), with <c>ackBits</c> 0. 0xfffe is also a real packet id (ids run 0..0xfffe, NERDCORE_WIRE.md §2.2): a peer
    /// reading it before it has sent that many packets finds nothing to acknowledge (its own ids from 0 are ahead of it, §3.3), and an
    /// ack of 0xfffe received from a peer does acknowledge our packet 0xfffe.
    /// </summary>
    internal const ushort NoAck = 0xfffe;
    internal const int MaxPayloadBits = (1 << 14) - 1;
    /// <summary>
    /// The largest datagram the game accepts, header included: its packet-header reader closes the connection on a size over
    /// 0x55c (1372 bytes) with 0x80600007 "Disconnect Invalid Msg Size" (NERDCORE_WIRE.md §2.3, §3.5). NerdCore has no message
    /// fragmentation (§2.4: messages carry no length and never span packets), so every message fits one datagram whole.
    /// </summary>
    internal const int MaxDatagramBytes = 1372;
    /// <summary>The message bits one datagram carries after its header.</summary>
    internal const int MaxMessageBits = (MaxDatagramBytes - NerdHeader.Length) * 8;
    /// <summary>The widest object-message prefix: 7 id + 4 router + 2 kind + 7 id + 8 peer (a relayed key) + 2 type + 8 sequence + 14 length.</summary>
    internal const int ObjectPrefixMaxBits = 52;
    /// <summary>An object payload this long or shorter always fits one datagram, alone, under any key.</summary>
    internal const int MaxDatagramPayloadBits = MaxMessageBits - ObjectPrefixMaxBits;
    internal const int CarBodyBits = 710;
    internal const int DetachPartSafeBytes = 1024;
    internal const int ChatSafeBytes = 64;
    internal const int SatNavNodeMax = 0x00ffffff;
    internal static ReadOnlySpan<byte> Version => "6CC152FD"u8;
    internal static ReadOnlySpan<byte> Game => "Flagship"u8;

    internal static (int Id, int Type) RouterWidths(byte router) => router switch
    {
        >= 1 and <= 3 => (7, 0),
        6 => (2, 2),
        9 or 12 => (4, 2),
        // weather (10) and car meet (11): one server-owned object each, so 1 id bit and no type bits
        10 or 11 => (1, 0),
        _ => throw new WireFormatException(WireError.UnknownRouter, router),
    };

    internal static int? CarEventBits(byte id) => id switch { 20 => 352, 21 => 192, 35 => 0, _ => null };
    internal static int? TrafficBits(byte id) => id switch { 16 => 314, 17 => 200, 71 or 72 => 72, 73 => 330, 74 => 185, _ => null };

    internal static byte[] Packet(IReadOnlyList<BitBuffer> messages, ushort id, ushort ack, uint ackBits)
    {
        if (messages.Count > byte.MaxValue) throw new WireFormatException(WireError.LengthLimit);
        var body = new BitBuffer();
        foreach (var message in messages) body.Append(message);
        var length = NerdHeader.Length + body.Bytes.Length;
        if (length > ushort.MaxValue) throw new WireFormatException(WireError.LengthLimit);
        var output = new byte[length];
        new NerdHeader(id, ack, ackBits, (byte)messages.Count, (ushort)length).Write(output);
        body.Bytes.CopyTo(output.AsSpan(NerdHeader.Length));
        return output;
    }

    // Read one message only. Unknown layouts stop the packet; never guess the next boundary.
    internal static bool TryRead(ref BitReader reader, out NerdMessage? message, out WireError? error)
    {
        try { message = Read(ref reader); error = null; return true; }
        catch (WireFormatException e) { message = null; error = e.Error; return false; }
    }

    internal static NerdMessage Read(ref BitReader reader)
    {
        var id = (byte)reader.Read(7);
        switch (id)
        {
            case 0: return new NerdMessage.OpenConnection(reader.ByteString(38), reader.ByteString(16),
                (byte)reader.Read(2), reader.Read(64), (uint)reader.Read(32), (uint)reader.Read(32), (uint)reader.Read(32));
            case 1: return new NerdMessage.OpenResponse((byte)reader.Read(2), (byte)reader.Read(3), (uint)reader.Read(32), (uint)reader.Read(32));
            case 2: return new NerdMessage.Close();
            case 3: return new NerdMessage.Ping((uint)reader.Read(32));
            case 4: return new NerdMessage.Pong((uint)reader.Read(32), (uint)reader.Read(32));
            case 5: return new NerdMessage.TimeSync((ushort)reader.Read(16), (uint)reader.Read(32));
            case 6: return new NerdMessage.SyncDone((uint)reader.Read(32));
            case 7 or 8:
            {
                var router = (byte)reader.Read(4);
                var widths = RouterWidths(router);
                var handle = ObjectHandle.Read(ref reader, widths.Id);
                var type = id == 7 && widths.Type != 0 ? (byte)reader.Read(widths.Type) : (byte)0;
                var sequence = (byte)reader.Read(8);
                var length = (int)reader.Read(14);
                return new NerdMessage.Object(id == 7, router, handle, type, sequence, reader.Take(length));
            }
            case 9:
            {
                var router = (byte)reader.Read(4);
                var handle = ObjectHandle.Read(ref reader, RouterWidths(router).Id);
                return new NerdMessage.Delete(router, handle, (byte)reader.Read(8));
            }
            case 15: return new NerdMessage.Car(ObjectHandle.Read(ref reader, 7), reader.Take(CarBodyBits));
            case 20 or 21 or 35: return new NerdMessage.CarEvent(id, ObjectHandle.Read(ref reader, 7), reader.Take(CarEventBits(id)!.Value));
            case 36: return new NerdMessage.TagCollision(ObjectHandle.Read(ref reader, 7), ObjectHandle.Read(ref reader, 7),
                [(byte)reader.Read(3), (byte)reader.Read(3), (byte)reader.Read(3), (byte)reader.Read(3)]);
            case 37:
            {
                var cars = new ObjectHandle[NerdMessage.TagBuckets.Slots];
                for (var i = 0; i < cars.Length; i++) cars[i] = ObjectHandle.Read(ref reader, 7);
                var buckets = new byte[NerdMessage.TagBuckets.Slots];
                for (var i = 0; i < buckets.Length; i++) buckets[i] = (byte)reader.Read(8);
                return new NerdMessage.TagBuckets(cars, buckets);
            }
            case 16 or 17 or 71 or 72 or 73 or 74: return new NerdMessage.Traffic(id, reader.Take(TrafficBits(id)!.Value));
            case 24:
            {
                var kind = (byte)reader.Read(4);
                return new NerdMessage.DetachPart(kind, reader.ReadBytes((int)reader.Read(11)));
            }
            case 33:
            {
                var xuid = reader.Read(64);
                return new NerdMessage.Chat(xuid, reader.ReadBytes((int)reader.Read(7)));
            }
            case 34: return new NerdMessage.FinaliseSessionEntry();
            case 70: return new NerdMessage.InactivePlayers(ObjectHandle.Read(ref reader, 2));
            case 52: return new NerdMessage.GroupSatNav(ObjectHandle.Read(ref reader, 2), unchecked((int)reader.Read(32)));
            default:
            {
                var probe = reader;
                SkipOther(ref probe, id);
                return new NerdMessage.Other(id, reader.Take(probe.Position - reader.Position));
            }
        }
    }

    internal static BitBuffer Write(NerdMessage message)
    {
        var writer = new BitBuffer().Push(message.Id, 7);
        switch (message)
        {
            case NerdMessage.OpenConnection m:
                if (m.Version.Length > 38 || m.Game.Length > 16) throw new WireFormatException(WireError.LengthLimit);
                writer.ByteString(m.Version).ByteString(m.Game).Push(m.Flags, 2).Push(m.User, 64)
                    .Push(m.A, 32).Push(m.B, 32).Push(m.Attempt, 32);
                break;
            case NerdMessage.OpenResponse m: writer.Push(m.Flags, 2).Push(m.Reason, 3).Push(m.Attempt, 32).Push(m.OwnAttempt, 32); break;
            case NerdMessage.Close or NerdMessage.FinaliseSessionEntry: break;
            case NerdMessage.Ping m: writer.Push(m.Time, 32); break;
            case NerdMessage.Pong m: writer.Push(m.Echo, 32).Push(m.Time, 32); break;
            case NerdMessage.TimeSync m: writer.Push(m.Index, 16).Push(m.Time, 32); break;
            case NerdMessage.SyncDone m: writer.Push(m.Attempt, 32); break;
            case NerdMessage.Object m:
            {
                var widths = RouterWidths(m.Router);
                if (m.Payload.Length > MaxPayloadBits) throw new WireFormatException(WireError.LengthLimit);
                writer.Push(m.Router, 4);
                m.Handle.Write(writer, widths.Id);
                if (m.Create && widths.Type != 0) writer.Push(m.Type, widths.Type);
                writer.Push(m.Sequence, 8).Push((ulong)m.Payload.Length, 14).Append(m.Payload);
                break;
            }
            case NerdMessage.Delete m:
                writer.Push(m.Router, 4); m.Handle.Write(writer, RouterWidths(m.Router).Id); writer.Push(m.Sequence, 8); break;
            case NerdMessage.Car m:
                RequireLength(m.Body, CarBodyBits); m.Handle.Write(writer, 7); writer.Append(m.Body); break;
            case NerdMessage.CarEvent m:
                RequireLength(m.Body, CarEventBits(m.Kind)); m.Handle.Write(writer, 7); writer.Append(m.Body); break;
            case NerdMessage.TagCollision m:
                if (m.Values.Length != 4 || m.Values.Any(v => v > 7)) throw new WireFormatException(WireError.InvalidValue);
                m.A.Write(writer, 7); m.B.Write(writer, 7);
                foreach (var v in m.Values) writer.Push(v, 3);
                break;
            case NerdMessage.TagBuckets m:
                if (m.Cars.Length != NerdMessage.TagBuckets.Slots || m.Buckets.Length != NerdMessage.TagBuckets.Slots
                    || m.Cars.Any(c => c.Id > 127)) throw new WireFormatException(WireError.InvalidValue);
                foreach (var car in m.Cars) car.Write(writer, 7);
                foreach (var bucket in m.Buckets) writer.Push(bucket, 8);
                break;
            case NerdMessage.Traffic m: RequireLength(m.Body, TrafficBits(m.Kind)); writer.Append(m.Body); break;
            case NerdMessage.DetachPart m:
                if (m.Data.Length >= 1 << 11) throw new WireFormatException(WireError.LengthLimit);
                writer.Push(m.Kind, 4).Push((ulong)m.Data.Length, 11);
                foreach (var b in m.Data) writer.Push(b, 8);
                break;
            case NerdMessage.Chat m:
                if (m.Data.Length >= 1 << 7) throw new WireFormatException(WireError.LengthLimit);
                writer.Push(m.Xuid, 64).Push((ulong)m.Data.Length, 7);
                foreach (var b in m.Data) writer.Push(b, 8);
                break;
            case NerdMessage.InactivePlayers m: m.Handle.Write(writer, 2); break;
            case NerdMessage.GroupSatNav m: m.Handle.Write(writer, 2); writer.Push(unchecked((uint)m.Node), 32); break;
            case NerdMessage.Other m:
                var probe = new BitReader(m.Body.Bytes, m.Body.Length);
                SkipOther(ref probe, m.Kind);
                if (probe.Left != 0) throw new WireFormatException(WireError.InvalidValue);
                writer.Append(m.Body);
                break;
            default: throw new WireFormatException(WireError.UnknownMessage, message.Id);
        }
        return writer;
    }

    private static void RequireLength(BitBuffer body, int? length)
    {
        if (length is null || body.Length != length) throw new WireFormatException(WireError.InvalidValue);
    }

    // Positive = fixed bits; -1/-2 = handle widths 7/2; -3/-4 = compact byte string max 64/8.
    internal static int[]? OtherLayout(byte id) => id switch
    {
        14 => [-1, 44], 26 => [34], 27 => [35], 31 => [32], 32 => [-3, -3],
        38 or 60 or 64 => [], 40 => [-2, 32], 41 => [-2, 1], 43 => [-1, 33],
        44 => [-1, -4, -4, -4, 34], 45 => [-1, -4, -4, -4, -4, -4, 59],
        46 => [-2, 64], 47 => [-2, 32], 49 or 62 => [1], 51 => [48],
        54 => [384], 56 => [224], 58 => [96], 59 => [-1], 63 => [64], _ => null,
    };

    private static void SkipOther(ref BitReader reader, byte id)
    {
        switch (id)
        {
            case 42: // MsgReadyRequestResponse, server to client: the lobby's event key and a 2-bit answer
                ObjectHandle.Read(ref reader, 2);
                reader.Skip(2);
                return;
            case 29: // MsgCarMeetNotifyPending, server to client: a 2-bit pending action
                reader.Skip(2);
                return;
            case 50: // ConfigureGameOptionsForRoadTripEvent, server to the leader: the event, then u32
                reader.Skip(64);
                return;
            case 61: // bucket-list co-op start time, server to client (u32)
                reader.Skip(32);
                return;
            case 66 or 67 or 68: // bucket-list co-op status, server to client (§6.3): key(7) [key(7)] s32 [bool]
                ObjectHandle.Read(ref reader, 7);
                if (id != 66) ObjectHandle.Read(ref reader, 7);
                reader.Skip(id == 68 ? 33 : 32);
                return;
            case 65: // bucket-list co-op partner, server to client: PlayerState key, the pair's 5-bit universe, found, a name of at most 36 bytes
                ObjectHandle.Read(ref reader, 7);
                reader.Skip(6);
                reader.ByteString(RelayEngine.BucketListNameMax);
                return;
            case 39:
                ObjectHandle.Read(ref reader, 2);
                var count = (int)reader.Read(4);
                for (var i = 0; i < count; i++) ObjectHandle.Read(ref reader, 7);
                return;
            case 10 or 11 or 12 or 13:
                var router = (byte)reader.Read(4);
                var widths = RouterWidths(router);
                if (id == 10) reader.Skip(widths.Type);
                if (id == 13 && router != 3) throw new WireFormatException(WireError.UnknownMessage, id);
                ObjectHandle.Read(ref reader, widths.Id);
                if (id is 11 or 12) reader.Skip(8);
                if (id == 11) reader.Skip((int)reader.Read(14));
                if (id == 13) reader.Skip((int)reader.Read(16) * 6);
                return;
            default:
                foreach (var field in OtherLayout(id) ?? throw new WireFormatException(WireError.UnknownMessage, id))
                {
                    switch (field)
                    {
                        case -1: ObjectHandle.Read(ref reader, 7); break;
                        case -2: ObjectHandle.Read(ref reader, 2); break;
                        case -3: reader.ByteString(64); break;
                        case -4: reader.ByteString(8); break;
                        default: reader.Skip(field); break;
                    }
                }
                return;
        }
    }
}

/// <summary>
/// Packet ids and their acknowledgement, as NerdCore counts them (NERDCORE_WIRE.md §2.2, §2.3, §3.2, §3.3): sequenced ids run
/// <c>0..0xfffe</c> (the next is <c>(id + 1) mod 0xffff</c>; 0xffff is "unsequenced"), and every distance is taken modulo 0xffff.
/// </summary>
internal sealed class AckWindow
{
    /// <summary>The count of sequenced packet ids, 0..0xfffe.</summary>
    internal const int IdModulus = 0xffff;
    /// <summary>A sequenced id this far from the newest, the short way round, or farther, is dropped (§2.3), as the game drops it.</summary>
    internal const int DropDistance = 0x3fff;

    private ushort? _top;
    private uint _bits;
    internal (ushort Ack, uint Bits) Header => (_top ?? NerdCodec.NoAck, _bits);

    /// <summary>How far <paramref name="to"/> is ahead of <paramref name="from"/>, modulo 0xffff (0..0xfffe).</summary>
    internal static int Ahead(ushort from, ushort to) => ((to - from) % IdModulus + IdModulus) % IdModulus;

    /// <summary>The id after <paramref name="id"/>: <c>(id + 1) mod 0xffff</c>, never 0xffff.</summary>
    internal static ushort NextId(ushort id) => (ushort)((id + 1) % IdModulus);

    /// <summary>
    /// Whether a sequenced packet is new: the game's receiver checks first (§2.3: equal to the newest, or 0x3fff or more away the short
    /// way round, is dropped), then duplicates within the 32 acknowledged behind the newest, and anything older than that, are stale.
    /// A dropped or stale packet is not recorded, so it is never acknowledged as new and its mutations never repeat.
    /// </summary>
    internal bool Accept(ushort packet)
    {
        if (packet == NerdCodec.NoSequence) return true;
        if (_top is { } top)
        {
            var ahead = Ahead(top, packet);
            if (ahead == 0 || Math.Min(ahead, IdModulus - ahead) >= DropDistance) return false;
            if (ahead > DropDistance)
            {
                var back = IdModulus - ahead;
                if (back > 32 || (_bits & (1u << (back - 1))) != 0) return false;
            }
        }
        Add(packet);
        return true;
    }

    internal void Add(ushort packet)
    {
        if (packet == NerdCodec.NoSequence) return;
        if (_top is not { } top) { _top = packet; _bits = 0; return; }
        var ahead = Ahead(top, packet);
        if (ahead == 0) return;
        // the newest moves only for an id ahead within half the id space (§3.2)
        if (ahead < IdModulus / 2)
        {
            _bits = ahead > 32 ? 0 : (ahead == 32 ? 0 : _bits << ahead) | (1u << (ahead - 1));
            _top = packet;
        }
        else
        {
            var back = IdModulus - ahead;
            if (back is >= 1 and <= 32) _bits |= 1u << (back - 1);
        }
    }

    /// <summary>Whether an incoming <c>ack</c>/<c>ackBits</c> acknowledges our sent <paramref name="packet"/> (§3.3, distances mod 0xffff).</summary>
    internal static bool Acknowledges(ushort ack, uint bits, ushort packet)
    {
        if (packet == NerdCodec.NoSequence || ack == NerdCodec.NoSequence) return false;
        if (ack == packet) return true;
        var back = Ahead(packet, ack);
        return back is >= 1 and <= 32 && (bits & (1u << (back - 1))) != 0;
    }

    internal static byte SequenceNext(byte sequence) => SequenceAdd(sequence, 1);
    internal static byte SequenceAdd(byte sequence, byte count) => (byte)((sequence + count) % 255);
    internal static bool SequenceNewer(byte a, byte b) => (b < a && a - b < 128) || (a < b && b - a > 128);
}
