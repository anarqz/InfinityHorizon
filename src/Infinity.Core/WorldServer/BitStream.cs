// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

internal enum WireError { Truncated, LengthLimit, UnknownMessage, UnknownRouter, InvalidValue }

internal sealed class WireFormatException(WireError error, int value = 0) : Exception(error.ToString())
{
    public WireError Error { get; } = error;
    public int Value { get; } = value;
}

/// <summary>NerdCore LSB-first bits. Padding belongs to the byte transport, never the payload.</summary>
internal sealed class BitBuffer
{
    // A packet's size field is u16. Bound writers as well as readers before allocating.
    internal const int MaxBits = ushort.MaxValue * 8;
    private byte[] _bytes = new byte[32];
    public int Length { get; private set; }
    internal ReadOnlySpan<byte> Bytes => _bytes.AsSpan(0, (Length + 7) / 8);
    internal byte[] ToArray() => Bytes.ToArray();

    internal BitBuffer Push(ulong value, int width)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(width);
        ArgumentOutOfRangeException.ThrowIfGreaterThan(width, 64);
        if (width > MaxBits - Length) throw new WireFormatException(WireError.LengthLimit);
        var needed = (Length + width + 7) / 8;
        if (needed > _bytes.Length) Array.Resize(ref _bytes, Math.Min(ushort.MaxValue, Math.Max(needed, _bytes.Length * 2)));
        while (width > 0)
        {
            var offset = Length & 7;
            var count = Math.Min(8 - offset, width);
            _bytes[Length >> 3] |= (byte)((value & ((1UL << count) - 1)) << offset);
            value >>= count;
            Length += count;
            width -= count;
        }
        return this;
    }

    internal BitBuffer Boolean(bool value) => Push(value ? 1UL : 0, 1);

    internal BitBuffer CompactUInt(uint value)
    {
        var size = (value & 0xffc00000) != 0 ? 3 : (value & 0x00ffc000) != 0 ? 2 : (value & 0xffc0) != 0 ? 1 : 0;
        return Push((ulong)size, 2).Push(value, 8 * size + 6);
    }

    internal BitBuffer ByteString(ReadOnlySpan<byte> value)
    {
        if (value.Length > (MaxBits - Length - 32) / 8) throw new WireFormatException(WireError.LengthLimit);
        CompactUInt((uint)value.Length);
        foreach (var b in value) Push(b, 8);
        return this;
    }

    internal BitBuffer Append(BitBuffer other)
    {
        ArgumentNullException.ThrowIfNull(other);
        if (other.Length > MaxBits - Length) throw new WireFormatException(WireError.LengthLimit);
        // An independent reader also makes self-append well-defined.
        var reader = new BitReader(other.Bytes.ToArray(), other.Length);
        while (reader.Left > 0)
        {
            var count = Math.Min(64, reader.Left);
            Push(reader.Read(count), count);
        }
        return this;
    }

    internal static BitBuffer From(ReadOnlySpan<byte> bytes, int bitLength)
    {
        var reader = new BitReader(bytes, bitLength);
        return reader.Take(bitLength);
    }
}

internal ref struct BitReader
{
    private readonly ReadOnlySpan<byte> _bytes;
    private readonly int _end;
    internal int Position { get; private set; }
    internal readonly int Left => _end - Position;

    internal BitReader(ReadOnlySpan<byte> bytes, int? bitLength = null)
    {
        if (bytes.Length > ushort.MaxValue) throw new WireFormatException(WireError.LengthLimit);
        _bytes = bytes;
        _end = bitLength ?? bytes.Length * 8;
        if (_end < 0 || _end > bytes.Length * 8) throw new WireFormatException(WireError.Truncated);
        Position = 0;
    }

    internal ulong Read(int width)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(width);
        ArgumentOutOfRangeException.ThrowIfGreaterThan(width, 64);
        if (width > Left) throw new WireFormatException(WireError.Truncated);
        ulong value = 0;
        for (var got = 0; got < width;)
        {
            var at = Position + got;
            var offset = at & 7;
            var count = Math.Min(8 - offset, width - got);
            value |= (ulong)((_bytes[at >> 3] >> offset) & ((1 << count) - 1)) << got;
            got += count;
        }
        Position += width;
        return value;
    }

    internal bool Boolean() => Read(1) != 0;
    internal uint CompactUInt() => (uint)Read(8 * (int)Read(2) + 6);

    internal byte[] ByteString(int maximum)
    {
        var count = CompactUInt();
        if (count > maximum) throw new WireFormatException(WireError.LengthLimit);
        return ReadBytes((int)count);
    }

    internal byte[] ReadBytes(int count)
    {
        if (count < 0 || count > Left / 8) throw new WireFormatException(WireError.Truncated);
        var result = new byte[count];
        for (var i = 0; i < count; i++) result[i] = (byte)Read(8);
        return result;
    }

    internal void Skip(int width)
    {
        if (width < 0 || width > Left) throw new WireFormatException(WireError.Truncated);
        Position += width;
    }

    internal BitBuffer Take(int width)
    {
        if (width < 0 || width > Left) throw new WireFormatException(WireError.Truncated);
        var result = new BitBuffer();
        while (width > 0)
        {
            var count = Math.Min(64, width);
            result.Push(Read(count), count);
            width -= count;
        }
        return result;
    }
}
