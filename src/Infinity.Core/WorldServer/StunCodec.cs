// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Net;
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

internal sealed record StunMessage(ushort Kind, byte[] Transaction, IPEndPoint? Mapped);

/// <summary>Bounded STUN framing and addresses. No network I/O, discovery or authentication policy.</summary>
internal static class StunCodec
{
    internal const uint Magic = 0x2112a442;
    internal const ushort BindingRequestKind = 1;
    internal const ushort BindingSuccessKind = 0x101;
    internal const int MaxLength = 548;

    internal static bool IsStun(ReadOnlySpan<byte> data) => data.Length is >= 20 and <= MaxLength
        && (data[0] & 0xc0) == 0 && BinaryPrimitives.ReadUInt32BigEndian(data[4..]) == Magic
        && BinaryPrimitives.ReadUInt16BigEndian(data[2..]) + 20 == data.Length && data.Length % 4 == 0;

    internal static StunMessage? Parse(ReadOnlySpan<byte> data)
    {
        if (!IsStun(data)) return null;
        IPEndPoint? mapped = null;
        var transaction = data.Slice(8, 12);
        for (var at = 20; at + 4 <= data.Length;)
        {
            var kind = BinaryPrimitives.ReadUInt16BigEndian(data[at..]);
            var count = BinaryPrimitives.ReadUInt16BigEndian(data[(at + 2)..]);
            if (count > data.Length - at - 4) return null;
            var value = data.Slice(at + 4, count);
            if (kind is 0x20 or 0x8020) mapped = Address(value, transaction) ?? mapped;
            else if (kind == 1 && mapped is null) mapped = Address(value, []);
            at += 4 + ((count + 3) / 4) * 4;
        }
        return new(BinaryPrimitives.ReadUInt16BigEndian(data), transaction.ToArray(), mapped);
    }

    private static IPEndPoint? Address(ReadOnlySpan<byte> value, ReadOnlySpan<byte> xor)
    {
        if (value.Length < 8) return null;
        var count = value[1] == 1 ? 4 : value[1] == 2 && value.Length >= 20 ? 16 : 0;
        if (count == 0) return null;
        var address = value.Slice(4, count).ToArray();
        var port = BinaryPrimitives.ReadUInt16BigEndian(value[2..]);
        if (!xor.IsEmpty)
        {
            port ^= (ushort)(Magic >> 16);
            Span<byte> key = stackalloc byte[16];
            BinaryPrimitives.WriteUInt32BigEndian(key, Magic);
            xor.CopyTo(key[4..]);
            for (var i = 0; i < address.Length; i++) address[i] ^= key[i];
        }
        return new(new IPAddress(address), port);
    }

    internal static byte[] BindingRequest(ReadOnlySpan<byte> transaction)
    {
        var output = new byte[20];
        Header(output, BindingRequestKind, transaction);
        return output;
    }

    internal static byte[] BindingSuccess(ReadOnlySpan<byte> transaction, IPEndPoint seen)
    {
        var endpoint = Canonical(seen);
        var address = endpoint.Address.GetAddressBytes();
        var value = new byte[4 + address.Length];
        value[1] = address.Length == 4 ? (byte)1 : (byte)2;
        BinaryPrimitives.WriteUInt16BigEndian(value.AsSpan(2), (ushort)(endpoint.Port ^ (Magic >> 16)));
        Span<byte> key = stackalloc byte[16];
        BinaryPrimitives.WriteUInt32BigEndian(key, Magic);
        if (transaction.Length != 12) throw new ArgumentException("STUN transaction must be 12 bytes", nameof(transaction));
        transaction.CopyTo(key[4..]);
        for (var i = 0; i < address.Length; i++) value[4 + i] = (byte)(address[i] ^ key[i]);
        return Indication(BindingSuccessKind, 0x20, value, transaction);
    }

    internal static byte[] Indication(ushort kind, ushort attribute, ReadOnlySpan<byte> value, ReadOnlySpan<byte> transaction = default)
    {
        var length = 24 + ((value.Length + 3) / 4) * 4;
        if (length > MaxLength) throw new WireFormatException(WireError.LengthLimit);
        var output = new byte[length];
        Header(output, kind, transaction);
        BinaryPrimitives.WriteUInt16BigEndian(output.AsSpan(20), attribute);
        BinaryPrimitives.WriteUInt16BigEndian(output.AsSpan(22), (ushort)value.Length);
        value.CopyTo(output.AsSpan(24));
        return output;
    }

    /// <summary>An indication with several attributes, in this order, each padded to 4 bytes (§5c: the message's attribute first).</summary>
    internal static byte[] Indications(ushort kind, ReadOnlySpan<byte> transaction, params (ushort Attribute, byte[] Value)[] attributes)
    {
        var length = 20 + attributes.Sum(a => 4 + ((a.Value.Length + 3) / 4) * 4);
        if (length > MaxLength || attributes.Any(a => a.Value.Length > ushort.MaxValue)) throw new WireFormatException(WireError.LengthLimit);
        var output = new byte[length];
        Header(output, kind, transaction);
        var at = 20;
        foreach (var (attribute, value) in attributes)
        {
            BinaryPrimitives.WriteUInt16BigEndian(output.AsSpan(at), attribute);
            BinaryPrimitives.WriteUInt16BigEndian(output.AsSpan(at + 2), (ushort)value.Length);
            value.CopyTo(output.AsSpan(at + 4));
            at += 4 + ((value.Length + 3) / 4) * 4;
        }
        return output;
    }

    /// <summary>A well-formed STUN datagram whose every attribute lies within it (an attribute past its end fails the shape).</summary>
    internal static bool Walks(ReadOnlySpan<byte> data)
    {
        if (!IsStun(data)) return false;
        for (var at = 20; at + 4 <= data.Length;)
        {
            var count = BinaryPrimitives.ReadUInt16BigEndian(data[(at + 2)..]);
            if (count > data.Length - at - 4) return false;
            at += 4 + ((count + 3) / 4) * 4;
        }
        return true;
    }

    private static void Header(Span<byte> output, ushort kind, ReadOnlySpan<byte> transaction)
    {
        BinaryPrimitives.WriteUInt16BigEndian(output, kind);
        BinaryPrimitives.WriteUInt16BigEndian(output[2..], (ushort)(output.Length - 20));
        BinaryPrimitives.WriteUInt32BigEndian(output[4..], Magic);
        if (transaction.IsEmpty) RandomNumberGenerator.Fill(output.Slice(8, 12));
        else
        {
            if (transaction.Length != 12) throw new ArgumentException("STUN transaction must be 12 bytes", nameof(transaction));
            transaction.CopyTo(output.Slice(8, 12));
        }
    }

    internal static bool TryAttribute(ReadOnlySpan<byte> data, ushort kind, ushort attribute, out ReadOnlySpan<byte> value)
    {
        value = default;
        if (!IsStun(data) || BinaryPrimitives.ReadUInt16BigEndian(data) != kind) return false;
        for (var at = 20; at + 4 <= data.Length;)
        {
            var tag = BinaryPrimitives.ReadUInt16BigEndian(data[at..]);
            var count = BinaryPrimitives.ReadUInt16BigEndian(data[(at + 2)..]);
            if (count > data.Length - at - 4) return false;
            if (tag == attribute) { value = data.Slice(at + 4, count); return true; }
            at += 4 + ((count + 3) / 4) * 4;
        }
        return false;
    }

    internal static IPEndPoint Canonical(IPEndPoint endpoint) => endpoint.Address.IsIPv4MappedToIPv6
        ? new(endpoint.Address.MapToIPv4(), endpoint.Port) : endpoint;
    internal static IPEndPoint ForSocket(IPEndPoint endpoint, bool ipv6) => ipv6 && endpoint.Address.AddressFamily == System.Net.Sockets.AddressFamily.InterNetwork
        ? new(endpoint.Address.MapToIPv6(), endpoint.Port) : endpoint;
}
