// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Numerics;

namespace Infinity.Core.WorldServer;

internal readonly record struct CarState(byte Lights, byte Roof, ushort Sequence);
internal readonly record struct CarStateUp(CarState State, ulong Xuid);
internal readonly record struct CarStateDown(CarState State, byte Index, ulong Xuid, Vector3? Position);

internal static class CarStateCodec
{
    internal const ushort Kind = 0x3e91;
    internal const ushort Attribute = 0xc0f1;

    private static CarState? ReadState(ReadOnlySpan<byte> value) => value[0] == 1 && value[1] <= 2 && value[2] <= 1
        ? new(value[1], value[2], BinaryPrimitives.ReadUInt16BigEndian(value[4..])) : null;

    internal static CarStateUp? ParseUp(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, Kind, Attribute, out var value) || value.Length != 16 || ReadState(value) is not { } state) return null;
        return new(state, BinaryPrimitives.ReadUInt64BigEndian(value[8..]));
    }

    internal static CarStateDown? ParseDown(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, Kind, Attribute, out var value) || value.Length != 28 || ReadState(value) is not { } state) return null;
        Vector3? position = (value[6] & 1) != 0 ? new Vector3(
            BinaryPrimitives.ReadSingleBigEndian(value[16..]), BinaryPrimitives.ReadSingleBigEndian(value[20..]), BinaryPrimitives.ReadSingleBigEndian(value[24..])) : null;
        return new(state, value[3], BinaryPrimitives.ReadUInt64BigEndian(value[8..]), position);
    }

    private static byte[] Value(CarState state, ulong xuid, int length)
    {
        if (state.Lights > 2 || state.Roof > 1) throw new WireFormatException(WireError.InvalidValue);
        var value = new byte[length]; value[0] = 1; value[1] = state.Lights; value[2] = state.Roof;
        BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(4), state.Sequence);
        BinaryPrimitives.WriteUInt64BigEndian(value.AsSpan(8), xuid);
        return value;
    }

    internal static byte[] WriteUp(CarStateUp message, ReadOnlySpan<byte> transaction = default) =>
        StunCodec.Indication(Kind, Attribute, Value(message.State, message.Xuid, 16), transaction);

    internal static byte[] WriteDown(CarStateDown message, ReadOnlySpan<byte> transaction = default)
    {
        var value = Value(message.State, message.Xuid, 28); value[3] = message.Index;
        if (message.Position is { } position)
        {
            value[6] = 1;
            BinaryPrimitives.WriteSingleBigEndian(value.AsSpan(16), position.X);
            BinaryPrimitives.WriteSingleBigEndian(value.AsSpan(20), position.Y);
            BinaryPrimitives.WriteSingleBigEndian(value.AsSpan(24), position.Z);
        }
        return StunCodec.Indication(Kind, Attribute, value, transaction);
    }
}
