// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;

namespace Infinity.Core.WorldServer;

/// <summary>FIUP v1 fixed frames. Credentials never enter the returned packet or its display.</summary>
internal static class UdpProofCodec
{
    internal sealed record Packet(byte Type, byte Kind, Guid Grant, Guid World, ulong Host, ulong Peer, byte[] Raw,
        IPEndPoint? Observed, ulong Expires)
    {
        public override string ToString() => "FIUP packet";
    }
    private static readonly byte[][] Domains = [[], "fi-udp-proof-v1/hello\n"u8.ToArray(), "fi-udp-proof-v1/challenge\n"u8.ToArray(),
        "fi-udp-proof-v1/response\n"u8.ToArray(), "fi-udp-proof-v1/ack\n"u8.ToArray()];

    internal static bool Unicast(IPAddress address)
    {
        if (address.IsIPv4MappedToIPv6) address = address.MapToIPv4();
        return address.AddressFamily == AddressFamily.InterNetwork ? address.GetAddressBytes()[0] is > 0 and < 224
            : address.AddressFamily == AddressFamily.InterNetworkV6 && !address.Equals(IPAddress.IPv6Any) && !address.IsIPv6Multicast;
    }

    internal static Packet? Parse(ReadOnlySpan<byte> data)
    {
        if (data.Length is not (104 or 147) || !data[..4].SequenceEqual("FIUP"u8) || data[4] != 1 || data[5] is < 1 or > 4
            || data[6] > 1 || data[7] != 0 || (data[5] == 1 ? data.Length != 104 : data.Length != 147)) return null;
        var grant = new Guid(data.Slice(8, 16), bigEndian: true); var world = new Guid(data.Slice(24, 16), bigEndian: true);
        var host = BinaryPrimitives.ReadUInt64BigEndian(data[40..]); var peer = BinaryPrimitives.ReadUInt64BigEndian(data[48..]);
        if (grant == Guid.Empty || world == Guid.Empty || host == 0 || peer == 0 || data.Slice(56,16).IndexOfAnyExcept((byte)0) < 0) return null;
        IPEndPoint? observed = null; ulong expires = 0;
        if (data[5] != 1)
        {
            if (data.Slice(72,16).IndexOfAnyExcept((byte)0) < 0 || data[88] is not (4 or 6)) return null;
            if (data[88] == 4 && data.Slice(93,12).IndexOfAnyExcept((byte)0) >= 0) return null;
            var address = new IPAddress(data.Slice(89, data[88] == 4 ? 4 : 16));
            if (!Unicast(address)) return null;
            var port = BinaryPrimitives.ReadUInt16BigEndian(data[105..]); if (port == 0) return null;
            observed = StunCodec.Canonical(new(address, port)); expires = BinaryPrimitives.ReadUInt64BigEndian(data[107..]);
            if (expires == 0 || expires > long.MaxValue) return null;
        }
        return new(data[5],data[6],grant,world,host,peer,data.ToArray(),observed,expires);
    }

    internal static void Sign(Span<byte> frame, ReadOnlySpan<byte> key)
    {
        var payload = frame.Length - 32; var domain = Domains[frame[5]];
        Span<byte> input = stackalloc byte[domain.Length + payload]; domain.CopyTo(input); frame[..payload].CopyTo(input[domain.Length..]);
        HMACSHA256.HashData(key,input,frame[payload..]);
    }
    internal static bool Verify(Packet packet, ReadOnlySpan<byte> key)
    {
        if (key.Length != 32) return false;
        Span<byte> copy = stackalloc byte[packet.Raw.Length]; packet.Raw.CopyTo(copy); Sign(copy,key);
        return CryptographicOperations.FixedTimeEquals(copy[^32..],packet.Raw.AsSpan(packet.Raw.Length-32));
    }
    internal static byte[] Header(Guid grant, Guid world, ulong host, ulong peer)
    {
        var raw = new byte[56]; "FIUP"u8.CopyTo(raw); raw[4]=1;
        grant.TryWriteBytes(raw.AsSpan(8,16),bigEndian:true,out _); world.TryWriteBytes(raw.AsSpan(24,16),bigEndian:true,out _);
        BinaryPrimitives.WriteUInt64BigEndian(raw.AsSpan(40),host); BinaryPrimitives.WriteUInt64BigEndian(raw.AsSpan(48),peer); return raw;
    }
    internal static byte[] Challenge(Packet hello, IPEndPoint endpoint, ulong expiry, ReadOnlySpan<byte> key)
    {
        endpoint=StunCodec.Canonical(endpoint); var raw=new byte[147]; hello.Raw.AsSpan(0,72).CopyTo(raw); raw[5]=2;
        RandomNumberGenerator.Fill(raw.AsSpan(72,16)); raw[88]=(byte)(endpoint.AddressFamily==AddressFamily.InterNetwork ? 4 : 6);
        endpoint.Address.GetAddressBytes().CopyTo(raw,89); BinaryPrimitives.WriteUInt16BigEndian(raw.AsSpan(105),(ushort)endpoint.Port);
        BinaryPrimitives.WriteUInt64BigEndian(raw.AsSpan(107),expiry); Sign(raw,key); return raw;
    }
    internal static byte[] ChangeType(ReadOnlySpan<byte> frame, byte type, ReadOnlySpan<byte> key)
    { var raw=frame.ToArray(); raw[5]=type; Sign(raw,key); return raw; }
    internal static bool SameContext(Packet packet, ReadOnlySpan<byte> header) => packet.Raw.AsSpan(8,48).SequenceEqual(header[8..]);
}
