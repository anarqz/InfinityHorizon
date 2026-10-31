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

/// <summary>The lobby's voice mode (MATCHMAKING_API.md §5c "The mode", CR-L46): the host's, enforced by its relay.</summary>
internal enum VoiceMode : byte { Off = 0, Open = 1, Proximity = 2 }

/// <summary>
/// One side of a pose (§5c <c>0xC0F8</c>): a car the relay knows (<c>Car</c>), the receipt age of its sample in ms, its position
/// as the three f32 bit patterns it came with, its raw velocity, and the low bits of the player's jump counter. A side whose car
/// bit is 0 has every field zero (the jump counter rides in the flags whatever the car bit).
/// </summary>
internal readonly record struct VoicePoseSide(bool Car, ushort Age, uint X, uint Y, uint Z, short Vx, short Vy, short Vz, byte Jumps)
{
    internal static VoicePoseSide None(byte jumps) => new(false, 0, 0, 0, 0, 0, 0, 0, jumps);
}

/// <summary>The pose a proximity voice down carries (§5c, CR-L46): the talker's and the listener's cars at one relay instant,
/// and the listener's orientation (x, y, z of a unit quaternion, raw; zero when the listener has no car).</summary>
internal sealed record VoicePose(VoicePoseSide Talker, VoicePoseSide Listener, short Qx, short Qy, short Qz, bool SameSide = true)
{
    internal const byte TalkerCar = 1, ListenerCar = 2, SameSideFlag = 4;

    internal byte Flags => (byte)((Talker.Car ? TalkerCar : 0) | (Listener.Car ? ListenerCar : 0) | (SameSide ? SameSideFlag : 0)
        | (Talker.Jumps & 3) << 4 | (Listener.Jumps & 3) << 6);
}

internal sealed record VoiceUp(VoiceHeader Header, byte[] Opus);
internal sealed record VoiceDown(VoiceHeader Header, byte Seat, byte[] Opus, VoicePose? Pose = null);

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

/// <summary>State down's <c>0xC0F9</c> (§5c, CR-L46): the mode and, in proximity, the range R in metres (0 otherwise).</summary>
internal readonly record struct VoiceModeInfo(VoiceMode Mode, ushort Range);

/// <summary>
/// The relay's voice state for the lobby: <c>Lobby</c> bit 0 voice on; <c>Bundle</c> 3 or 6 frames per packet. Never who muted whom.
/// <c>Mode</c>: the <c>0xC0F9</c> attribute (a new relay always writes it; null when absent or ignored on receipt).
/// </summary>
internal sealed record VoiceStateDown(byte Lobby, byte Bundle, IReadOnlyList<VoiceSeat> Seats, VoiceModeInfo? Mode = null)
{
    internal const byte On = 1;

    /// <summary>What a receiver plays (normative): <c>0xC0F9</c>'s mode, else the mode from <c>lobby</c> (0 off, 1 open).</summary>
    internal VoiceMode EffectiveMode => Mode?.Mode ?? (Lobby == On ? VoiceMode.Open : VoiceMode.Off);
}

/// <summary>
/// The world relay's voice side channel (OD-154, D1: kinds 0x3E93 and 0x3E94; 0x3E92/0xC0F2, the retired livery
/// channel, stays reserved). STUN indications like car state, big-endian. Voice: the 12-byte transaction id is
/// {ver = 1, flags, seq u16, ts u32 (20 ms units), up: 4 random | down: seat u8, 3 random} and the attribute is one
/// Opus packet (1-400 bytes). Control up {ver, op, xuid u64, self u8, count u8 &lt;= 12, count x xuid u64}; state down
/// {ver, lobby u8, bundle u8, count u8 &lt;= 12, count x {seat u8, xuid u64, state u8}}. CR-L46: voice down may carry the
/// pose <c>0xC0F8</c> after <c>0xC0F4</c> (proximity), state down carries the mode <c>0xC0F9</c> after <c>0xC0F6</c>; a writer puts
/// the message's attribute first, a reader finds it anywhere, and a malformed optional attribute is ignored.
/// </summary>
internal static class VoiceCodec
{
    internal const ushort VoiceKind = 0x3e93, ControlKind = 0x3e94;
    internal const ushort VoiceUpAttribute = 0xc0f3, VoiceDownAttribute = 0xc0f4, ControlUpAttribute = 0xc0f5, StateDownAttribute = 0xc0f6;
    internal const ushort PoseAttribute = 0xc0f8, ModeAttribute = 0xc0f9;
    internal const byte Version = 1;
    internal const int MaxOpus = 400, MaxSeats = 12, PoseLength = 48, ModeLength = 4;
    /// <summary>The smallest range a receiver takes in <c>0xC0F9</c> (below: the attribute is ignored).</summary>
    internal const ushort MinRange = 20;

    internal static VoiceUp? ParseUp(ReadOnlySpan<byte> data)
    {
        if (!Shaped(data, VoiceKind, VoiceUpAttribute, out var opus) || opus.Length is 0 or > MaxOpus || data[8] != Version) return null;
        return new(Header(data), opus.ToArray());
    }

    internal static VoiceDown? ParseDown(ReadOnlySpan<byte> data)
    {
        if (!Shaped(data, VoiceKind, VoiceDownAttribute, out var opus) || opus.Length is 0 or > MaxOpus || data[8] != Version) return null;
        var pose = StunCodec.TryAttribute(data, VoiceKind, PoseAttribute, out var value) ? ParsePose(value) : null;
        return new(Header(data), data[16], opus.ToArray(), pose);
    }

    internal static byte[] WriteUp(VoiceUp message, ReadOnlySpan<byte> random = default) => Voice(VoiceUpAttribute, message.Header, null, message.Opus, null, random);

    internal static byte[] WriteDown(VoiceDown message, ReadOnlySpan<byte> random = default) =>
        Voice(VoiceDownAttribute, message.Header, message.Seat, message.Opus, message.Pose is { } pose ? PoseValue(pose) : null, random);

    internal static VoiceControlUp? ParseControl(ReadOnlySpan<byte> data)
    {
        if (!Shaped(data, ControlKind, ControlUpAttribute, out var v) || v.Length < 12 || v[0] != Version) return null;
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
        if (!Shaped(data, ControlKind, StateDownAttribute, out var v) || v.Length < 4 || v[0] != Version) return null;
        var count = v[3];
        if (v[1] > 1 || v[2] is not (3 or 6) || count > MaxSeats || v.Length != 4 + count * 10) return null;
        var seats = new VoiceSeat[count];
        for (var i = 0; i < count; i++)
        {
            var at = 4 + i * 10;
            if (v[at + 9] > 7) return null;
            seats[i] = new(v[at], BinaryPrimitives.ReadUInt64BigEndian(v[(at + 1)..]), v[at + 9]);
        }
        var mode = StunCodec.TryAttribute(data, ControlKind, ModeAttribute, out var m) ? ParseMode(m, v[1]) : null;
        return new(v[1], v[2], seats, mode);
    }

    internal static byte[] WriteState(VoiceStateDown message, ReadOnlySpan<byte> transaction = default)
    {
        if (message.Seats.Count > MaxSeats || message.Lobby > 1 || message.Bundle is not (3 or 6)) throw new WireFormatException(WireError.InvalidValue);
        if (message.Mode is { } mode && !ModeConsistent(mode, message.Lobby)) throw new WireFormatException(WireError.InvalidValue);
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
        if (message.Mode is not { } info) return StunCodec.Indication(ControlKind, StateDownAttribute, v, transaction);
        var modeValue = new byte[ModeLength];
        modeValue[0] = Version; modeValue[1] = (byte)info.Mode;
        BinaryPrimitives.WriteUInt16BigEndian(modeValue.AsSpan(2), info.Range);
        return StunCodec.Indications(ControlKind, transaction, (StateDownAttribute, v), (ModeAttribute, modeValue));
    }

    // receiver rules (§5c "The mode"): any inconsistency ignores the attribute; the state down itself is still taken
    private static VoiceModeInfo? ParseMode(ReadOnlySpan<byte> m, byte lobby)
    {
        if (m.Length != ModeLength || m[0] != Version || m[1] > 2) return null;
        var info = new VoiceModeInfo((VoiceMode)m[1], BinaryPrimitives.ReadUInt16BigEndian(m[2..]));
        return ModeConsistent(info, lobby) ? info : null;
    }

    private static bool ModeConsistent(VoiceModeInfo info, byte lobby) =>
        info.Mode is VoiceMode.Off or VoiceMode.Open or VoiceMode.Proximity
        && (info.Mode == VoiceMode.Proximity ? info.Range >= MinRange : info.Range == 0)
        && (lobby == 0) == (info.Mode == VoiceMode.Off);

    // the pose (§5c "The pose attribute"): ignored unless ver 1 and exactly 48 bytes; a set car bit with a non-finite f32 makes
    // that side "no car"; the reserved bit 3 is ignored
    private static VoicePose? ParsePose(ReadOnlySpan<byte> v)
    {
        if (v.Length != PoseLength || v[0] != Version) return null;
        var flags = v[1];
        var talker = Side(v, 2, (flags & VoicePose.TalkerCar) != 0, (byte)(flags >> 4 & 3));
        var listener = Side(v, 22, (flags & VoicePose.ListenerCar) != 0, (byte)(flags >> 6 & 3));
        var qx = (short)BinaryPrimitives.ReadUInt16BigEndian(v[42..]);
        var qy = (short)BinaryPrimitives.ReadUInt16BigEndian(v[44..]);
        var qz = (short)BinaryPrimitives.ReadUInt16BigEndian(v[46..]);
        if (!listener.Car) (qx, qy, qz) = ((short)0, (short)0, (short)0);
        return new(talker, listener, qx, qy, qz, (flags & VoicePose.SameSideFlag) != 0);
    }

    private static VoicePoseSide Side(ReadOnlySpan<byte> v, int at, bool car, byte jumps)
    {
        if (!car) return VoicePoseSide.None(jumps);
        var x = BinaryPrimitives.ReadUInt32BigEndian(v[(at + 2)..]);
        var y = BinaryPrimitives.ReadUInt32BigEndian(v[(at + 6)..]);
        var z = BinaryPrimitives.ReadUInt32BigEndian(v[(at + 10)..]);
        if (!float.IsFinite(BitConverter.UInt32BitsToSingle(x)) || !float.IsFinite(BitConverter.UInt32BitsToSingle(y))
            || !float.IsFinite(BitConverter.UInt32BitsToSingle(z))) return VoicePoseSide.None(jumps);
        return new(true, BinaryPrimitives.ReadUInt16BigEndian(v[at..]), x, y, z, (short)BinaryPrimitives.ReadUInt16BigEndian(v[(at + 14)..]),
            (short)BinaryPrimitives.ReadUInt16BigEndian(v[(at + 16)..]), (short)BinaryPrimitives.ReadUInt16BigEndian(v[(at + 18)..]), jumps);
    }

    private static byte[] PoseValue(VoicePose pose)
    {
        var v = new byte[PoseLength];
        v[0] = Version; v[1] = pose.Flags;
        WriteSide(v.AsSpan(2), pose.Talker);
        WriteSide(v.AsSpan(22), pose.Listener);
        if (pose.Listener.Car)
        {
            BinaryPrimitives.WriteInt16BigEndian(v.AsSpan(42), pose.Qx);
            BinaryPrimitives.WriteInt16BigEndian(v.AsSpan(44), pose.Qy);
            BinaryPrimitives.WriteInt16BigEndian(v.AsSpan(46), pose.Qz);
        }
        return v;
    }

    private static void WriteSide(Span<byte> v, VoicePoseSide side)
    {
        if (!side.Car) return; // all zero
        BinaryPrimitives.WriteUInt16BigEndian(v, side.Age);
        BinaryPrimitives.WriteUInt32BigEndian(v[2..], side.X);
        BinaryPrimitives.WriteUInt32BigEndian(v[6..], side.Y);
        BinaryPrimitives.WriteUInt32BigEndian(v[10..], side.Z);
        BinaryPrimitives.WriteInt16BigEndian(v[14..], side.Vx);
        BinaryPrimitives.WriteInt16BigEndian(v[16..], side.Vy);
        BinaryPrimitives.WriteInt16BigEndian(v[18..], side.Vz);
    }

    // §5c "Framing": the whole datagram must walk (every attribute within it), the message's attribute present
    private static bool Shaped(ReadOnlySpan<byte> data, ushort kind, ushort attribute, out ReadOnlySpan<byte> value) =>
        StunCodec.Walks(data) & StunCodec.TryAttribute(data, kind, attribute, out value);

    private static VoiceHeader Header(ReadOnlySpan<byte> data) =>
        new(data[9], BinaryPrimitives.ReadUInt16BigEndian(data[10..]), BinaryPrimitives.ReadUInt32BigEndian(data[12..]));

    // random: the transaction id's random tail (4 bytes up, 3 down); empty draws it (golden vectors pass zeros)
    private static byte[] Voice(ushort attribute, VoiceHeader header, byte? seat, byte[] opus, byte[]? pose, ReadOnlySpan<byte> random)
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
        return pose is null ? StunCodec.Indication(VoiceKind, attribute, opus, transaction)
            : StunCodec.Indications(VoiceKind, transaction, (attribute, opus), (PoseAttribute, pose));
    }
}
