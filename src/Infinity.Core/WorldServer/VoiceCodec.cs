// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

/// <summary>A voice datagram's header, carried in the STUN transaction id (VOICE_CHAT.md §3.1, OD-154).</summary>
internal readonly record struct VoiceHeader(byte Flags, ushort Sequence, uint Timestamp)
{
    /// <summary><c>Flags</c> bit 0: the last packet of a talk spurt.</summary>
    internal const byte EndOfSpurt = 1;

    /// <summary><c>Flags</c> bit 1: sent while push-to-talk is held.</summary>
    internal const byte PushToTalk = 2;
}

internal sealed record VoiceUp(VoiceHeader Header, byte[] Opus);
internal sealed record VoiceDown(VoiceHeader Header, byte Seat, byte[] Opus);

/// <summary>A hook's voice control: <c>Op</c> 1 subscribe, 2 replace the mute list, 3 leave; <c>Self</c> bit 0 mic muted, bit 1 deafened.</summary>
internal sealed record VoiceControlUp(byte Op, ulong Xuid, byte Self, IReadOnlyList<ulong> Mutes)
{
    internal const byte Subscribe = 1, ReplaceMutes = 2, Leave = 3;
    internal const byte MicMuted = 1, Deafened = 2;
}

/// <summary>One seat of a <see cref="VoiceStateDown"/>: <c>State</c> bit 0 talking, 1 muted for the lobby, 2 voice blocked.</summary>
internal readonly record struct VoiceSeat(byte Seat, ulong Xuid, byte State)
{
    internal const byte Talking = 1, MutedForLobby = 2, Blocked = 4;
}

/// <summary>The relay's voice state for the lobby: <c>Lobby</c> bit 0 voice on; <c>Bundle</c> 3 or 6 frames per packet. Never who muted whom.</summary>
internal sealed record VoiceStateDown(byte Lobby, byte Bundle, IReadOnlyList<VoiceSeat> Seats)
{
    internal const byte On = 1;
}

/// <summary>
/// The world relay's voice side channel (OD-154, D1: kinds 0x3E93 and 0x3E94; 0x3E92/0xC0F2, the retired livery
/// channel, stays reserved). STUN indications like car state, big-endian. Voice: the 12-byte transaction id is
/// {ver = 1, flags, seq u16, ts u32 (20 ms units), up: 4 random | down: seat u8, 3 random} and the attribute is one
/// Opus packet (1-400 bytes). Control up {ver, op, xuid u64, self u8, count u8 &lt;= 12, count x xuid u64}; state down
/// {ver, lobby u8, bundle u8, count u8 &lt;= 12, count x {seat u8, xuid u64, state u8}}.
/// </summary>
internal static class VoiceCodec
{
    internal const ushort VoiceKind = 0x3e93, ControlKind = 0x3e94;
    internal const ushort VoiceUpAttribute = 0xc0f3, VoiceDownAttribute = 0xc0f4, ControlUpAttribute = 0xc0f5, StateDownAttribute = 0xc0f6;
    internal const byte Version = 1;
    internal const int MaxOpus = 400, MaxSeats = 12;

    internal static VoiceUp? ParseUp(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, VoiceKind, VoiceUpAttribute, out var opus) || opus.Length is 0 or > MaxOpus || data[8] != Version) return null;
        return new(Header(data), opus.ToArray());
    }

    internal static VoiceDown? ParseDown(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, VoiceKind, VoiceDownAttribute, out var opus) || opus.Length is 0 or > MaxOpus || data[8] != Version) return null;
        return new(Header(data), data[16], opus.ToArray());
    }

    internal static byte[] WriteUp(VoiceUp message, ReadOnlySpan<byte> random = default) => Voice(VoiceUpAttribute, message.Header, null, message.Opus, random);

    internal static byte[] WriteDown(VoiceDown message, ReadOnlySpan<byte> random = default) => Voice(VoiceDownAttribute, message.Header, message.Seat, message.Opus, random);

    internal static VoiceControlUp? ParseControl(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, ControlKind, ControlUpAttribute, out var v) || v.Length < 12 || v[0] != Version) return null;
        var op = v[1];
        var count = v[11];
        if (op is < VoiceControlUp.Subscribe or > VoiceControlUp.Leave || v[10] > 3 || count > MaxSeats || v.Length != 12 + count * 8) return null;
        var mutes = new ulong[count];
        for (var i = 0; i < count; i++) mutes[i] = BinaryPrimitives.ReadUInt64BigEndian(v[(12 + i * 8)..]);
        return new(op, BinaryPrimitives.ReadUInt64BigEndian(v[2..]), v[10], mutes);
    }

    internal static byte[] WriteControl(VoiceControlUp message, ReadOnlySpan<byte> transaction = default)
    {
        if (message.Mutes.Count > MaxSeats || message.Op is < VoiceControlUp.Subscribe or > VoiceControlUp.Leave || message.Self > 3)
            throw new WireFormatException(WireError.InvalidValue);
        var v = new byte[12 + message.Mutes.Count * 8];
        v[0] = Version; v[1] = message.Op;
        BinaryPrimitives.WriteUInt64BigEndian(v.AsSpan(2), message.Xuid);
        v[10] = message.Self; v[11] = (byte)message.Mutes.Count;
        for (var i = 0; i < message.Mutes.Count; i++) BinaryPrimitives.WriteUInt64BigEndian(v.AsSpan(12 + i * 8), message.Mutes[i]);
        return StunCodec.Indication(ControlKind, ControlUpAttribute, v, transaction);
    }

    internal static VoiceStateDown? ParseState(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, ControlKind, StateDownAttribute, out var v) || v.Length < 4 || v[0] != Version) return null;
        var count = v[3];
        if (v[1] > 1 || v[2] is not (3 or 6) || count > MaxSeats || v.Length != 4 + count * 10) return null;
        var seats = new VoiceSeat[count];
        for (var i = 0; i < count; i++)
        {
            var at = 4 + i * 10;
            if (v[at + 9] > 7) return null;
            seats[i] = new(v[at], BinaryPrimitives.ReadUInt64BigEndian(v[(at + 1)..]), v[at + 9]);
        }
        return new(v[1], v[2], seats);
    }

    internal static byte[] WriteState(VoiceStateDown message, ReadOnlySpan<byte> transaction = default)
    {
        if (message.Seats.Count > MaxSeats || message.Lobby > 1 || message.Bundle is not (3 or 6)) throw new WireFormatException(WireError.InvalidValue);
        var v = new byte[4 + message.Seats.Count * 10];
        v[0] = Version; v[1] = message.Lobby; v[2] = message.Bundle; v[3] = (byte)message.Seats.Count;
        for (var i = 0; i < message.Seats.Count; i++)
        {
            var seat = message.Seats[i];
            if (seat.State > 7) throw new WireFormatException(WireError.InvalidValue);
            var at = 4 + i * 10;
            v[at] = seat.Seat;
            BinaryPrimitives.WriteUInt64BigEndian(v.AsSpan(at + 1), seat.Xuid);
            v[at + 9] = seat.State;
        }
        return StunCodec.Indication(ControlKind, StateDownAttribute, v, transaction);
    }

    private static VoiceHeader Header(ReadOnlySpan<byte> data) =>
        new(data[9], BinaryPrimitives.ReadUInt16BigEndian(data[10..]), BinaryPrimitives.ReadUInt32BigEndian(data[12..]));

    // random: the transaction id's random tail (4 bytes up, 3 down); empty draws it (golden vectors pass zeros)
    private static byte[] Voice(ushort attribute, VoiceHeader header, byte? seat, byte[] opus, ReadOnlySpan<byte> random)
    {
        if (opus.Length is 0 or > MaxOpus) throw new WireFormatException(WireError.LengthLimit);
        Span<byte> transaction = stackalloc byte[12];
        if (random.IsEmpty) RandomNumberGenerator.Fill(transaction[8..]);
        else random.CopyTo(transaction[(seat is null ? 8 : 9)..]);
        transaction[0] = Version;
        transaction[1] = header.Flags;
        BinaryPrimitives.WriteUInt16BigEndian(transaction[2..], header.Sequence);
        BinaryPrimitives.WriteUInt32BigEndian(transaction[4..], header.Timestamp);
        if (seat is { } s) transaction[8] = s;
        return StunCodec.Indication(VoiceKind, attribute, opus, transaction);
    }
}
