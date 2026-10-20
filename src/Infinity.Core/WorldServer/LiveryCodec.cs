// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;

namespace Infinity.Core.WorldServer;

internal readonly record struct LiveryMeta(uint Car, ushort Layers = 0, ulong Creator = 0);
internal sealed record LiveryChunk(ushort Sequence, ushort Index, ushort Count, uint Total, LiveryMeta Meta, ulong Owner, byte[] Data);
internal abstract record LiveryMessage
{
    internal sealed record Put(LiveryChunk Chunk) : LiveryMessage;
    internal sealed record Stored(ushort Sequence) : LiveryMessage;
    internal sealed record Want(ulong Xuid) : LiveryMessage;
    internal sealed record Data(LiveryChunk Chunk) : LiveryMessage;
    internal sealed record Ack(ulong Owner, ushort Sequence) : LiveryMessage;
    internal sealed record Done(byte Liveries) : LiveryMessage;
    internal sealed record Gone(ulong Owner) : LiveryMessage;
}

/// <summary>OD-66 wire format; LiveryBoard owns storage and synchronization.</summary>
internal static class LiveryCodec
{
    internal const ushort Kind = 0x3e92;
    internal const ushort Attribute = 0xc0f2;
    internal const int ChunkBytes = 480;
    internal const int MaxLiveryBytes = 512 * 1024;
    private const int ChunkHeaderBytes = 34;

    internal static LiveryMessage? Parse(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, Kind, Attribute, out var value) || value.Length < 2 || value[0] != 1) return null;
        switch (value[1])
        {
            case 1 or 4:
                var chunk = ParseChunk(value);
                return chunk is null ? null : value[1] == 1 ? new LiveryMessage.Put(chunk) : new LiveryMessage.Data(chunk);
            case 2 when value.Length == 4: return new LiveryMessage.Stored(BinaryPrimitives.ReadUInt16BigEndian(value[2..]));
            case 3 when value.Length == 10: return new LiveryMessage.Want(BinaryPrimitives.ReadUInt64BigEndian(value[2..]));
            case 5 when value.Length == 12: return new LiveryMessage.Ack(BinaryPrimitives.ReadUInt64BigEndian(value[2..]), BinaryPrimitives.ReadUInt16BigEndian(value[10..]));
            case 6 when value.Length == 3: return new LiveryMessage.Done(value[2]);
            case 7 when value.Length == 10: return new LiveryMessage.Gone(BinaryPrimitives.ReadUInt64BigEndian(value[2..]));
            default: return null;
        }
    }

    private static LiveryChunk? ParseChunk(ReadOnlySpan<byte> value)
    {
        if (value.Length < ChunkHeaderBytes) return null;
        var sequence = BinaryPrimitives.ReadUInt16BigEndian(value[2..]);
        var index = BinaryPrimitives.ReadUInt16BigEndian(value[4..]);
        var count = BinaryPrimitives.ReadUInt16BigEndian(value[6..]);
        var total = BinaryPrimitives.ReadUInt32BigEndian(value[8..]);
        if (total > MaxLiveryBytes) return null;
        var expectedCount = Math.Max(1, ((int)total + ChunkBytes - 1) / ChunkBytes);
        if (count != expectedCount || index >= count) return null;
        var expectedBytes = index + 1 < count ? ChunkBytes : (int)total - (count - 1) * ChunkBytes;
        if (value.Length - ChunkHeaderBytes != expectedBytes) return null;
        return new(sequence, index, count, total,
            new(BinaryPrimitives.ReadUInt32BigEndian(value[12..]), BinaryPrimitives.ReadUInt16BigEndian(value[24..]), BinaryPrimitives.ReadUInt64BigEndian(value[26..])),
            BinaryPrimitives.ReadUInt64BigEndian(value[16..]), value[ChunkHeaderBytes..].ToArray());
    }

    internal static byte[] Write(LiveryMessage message, ReadOnlySpan<byte> transaction = default)
    {
        byte[] value;
        switch (message)
        {
            case LiveryMessage.Put m: value = ChunkValue(1, m.Chunk); break;
            case LiveryMessage.Data m: value = ChunkValue(4, m.Chunk); break;
            case LiveryMessage.Stored m:
                value = [1, 2, 0, 0]; BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(2), m.Sequence); break;
            case LiveryMessage.Want m:
                value = new byte[10]; value[1] = 3; BinaryPrimitives.WriteUInt64BigEndian(value.AsSpan(2), m.Xuid); break;
            case LiveryMessage.Ack m:
                value = new byte[12]; value[1] = 5; BinaryPrimitives.WriteUInt64BigEndian(value.AsSpan(2), m.Owner);
                BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(10), m.Sequence); break;
            case LiveryMessage.Done m: value = [1, 6, m.Liveries]; break;
            case LiveryMessage.Gone m:
                value = new byte[10]; value[1] = 7; BinaryPrimitives.WriteUInt64BigEndian(value.AsSpan(2), m.Owner); break;
            default: throw new WireFormatException(WireError.InvalidValue);
        }
        value[0] = 1;
        return StunCodec.Indication(Kind, Attribute, value, transaction);
    }

    private static byte[] ChunkValue(byte operation, LiveryChunk chunk)
    {
        if (chunk.Data.Length > ChunkBytes) throw new WireFormatException(WireError.LengthLimit);
        var value = new byte[ChunkHeaderBytes + chunk.Data.Length];
        value[0] = 1; value[1] = operation;
        BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(2), chunk.Sequence);
        BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(4), chunk.Index);
        BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(6), chunk.Count);
        BinaryPrimitives.WriteUInt32BigEndian(value.AsSpan(8), chunk.Total);
        BinaryPrimitives.WriteUInt32BigEndian(value.AsSpan(12), chunk.Meta.Car);
        BinaryPrimitives.WriteUInt64BigEndian(value.AsSpan(16), chunk.Owner);
        BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(24), chunk.Meta.Layers);
        BinaryPrimitives.WriteUInt64BigEndian(value.AsSpan(26), chunk.Meta.Creator);
        chunk.Data.CopyTo(value, ChunkHeaderBytes);
        if (ParseChunk(value) is null) throw new WireFormatException(WireError.InvalidValue);
        return value;
    }

    internal static IReadOnlyList<LiveryChunk> Chunks(ushort sequence, LiveryMeta meta, ulong owner, ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length > MaxLiveryBytes) throw new WireFormatException(WireError.LengthLimit);
        var count = Math.Max(1, (bytes.Length + ChunkBytes - 1) / ChunkBytes);
        var result = new LiveryChunk[count];
        for (var i = 0; i < count; i++)
        {
            var start = i * ChunkBytes;
            result[i] = new(sequence, (ushort)i, (ushort)count, (uint)bytes.Length, meta, owner,
                bytes.Slice(start, Math.Min(ChunkBytes, bytes.Length - start)).ToArray());
        }
        return result;
    }
}
