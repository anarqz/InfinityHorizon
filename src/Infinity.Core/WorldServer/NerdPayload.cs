// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Numerics;

namespace Infinity.Core.WorldServer;

internal enum PayloadFault { Router, Index, Duplicate, NotFinite, NameTooLong, Truncated, TooBig, OutOfRange }
internal readonly record struct PayloadFailure(PayloadFault Fault, int Value = 0);
internal readonly record struct OwnershipRequest(byte Router, uint Type, ObjectHandle Handle);
/// <summary>A MsgLapScore's fields in wire order (<see cref="NerdPayload.LapScore"/>): three doubles, a u32, two flags.</summary>
internal readonly record struct LapScoreFields(double Value68, double Value78, double Value80, uint Count8C, bool Flag89, bool Flag8B);

/// <summary>Read and rewrite forwarded payloads before they reach a game's trusting deserializers.</summary>
internal static class NerdPayload
{
    internal const int NetworkNameMax = 16;
    internal const int PlayerDataMaxBits = 8192;
    internal const float FloatMax = 1e6f;
    internal const float PositionMax = 1e5f;

    // Positive = bits, -1 = finite f32, -2 = bounded UTF-16 NetworkName.
    private static readonly int[] PlayerVariables = [64, 1, 1, -2, 64, 1, 32, 1, 32, 32, 32, 32];
    private static readonly int[] CarVariables = [.. Enumerable.Repeat(-1, 21), 5];
    private static readonly int[] ConsoleVariables = [1, 1, 1, 1, 1, 1, 1, 2, 2];
    /// <summary>
    /// BucketListPlayerStateSynced (router 12, type 0), as the game's constructor declares it (the 6 variables give 3-bit var ids, the
    /// bit length of the count): 0 Score (SyncedInt&lt;float, Range -2^31..2^31-1, 32 bits&gt;: an integer on the wire, kept opaque),
    /// 1 CurrentSlotId and 2 CurrentChallengeId (SyncedInt&lt;long, 32 bits&gt;), 3 RequestingPartner (SyncedBool), 4 State
    /// (SyncedEnum&lt;BucketListPlayerStateEnum, default 0, 4 bits&gt;: 1 Loading, 2 Ready, 3 Active, 4 Finished, 5 Retry), 5
    /// ArrivedAtDestination (SyncedBool). Pinned from the game binary, T22 (2026-11-02).
    /// </summary>
    private static readonly int[] BucketListPlayerVariables = [32, 32, 32, 1, 4, 1];
    internal const byte BucketListSlotVariable = 1, BucketListChallengeVariable = 2, BucketListRequestingVariable = 3, BucketListStateVariable = 4;
    /// <summary>Every BucketListPlayerStateSynced variable once (120 bits), rounded up; a repeated id is refused anyway.</summary>
    internal const int BucketListPlayerMaxBits = 128;
    internal static int? MaxBits(byte router) => router switch
    {
        1 => PlayerDataMaxBits, 2 => 787, 3 => 47, 12 => BucketListPlayerMaxBits, _ => null,
    };
    private static int IndexWidth(byte router) => router switch { 1 => 8, 2 => 5, 3 => 4, 12 => 3, _ => 0 };

    /// <summary>
    /// Player object (router 1) variables past the mapped twelve whose width is known, so that an update carrying exactly one of
    /// them, and nothing else, is superseded in a relayed object's history by a later such update of the same variable. Widths
    /// from live captures, plus the ones this server writes itself. Ids 0-11 use <see cref="PlayerVariables"/> (fixed widths only:
    /// 5 IsLeader is 1 bit there; the name's width varies, so a name update is never superseded). Anything absent is unknown.
    /// </summary>
    private static readonly Dictionary<int, int> PlayerSingleWidths = new()
    {
        [12] = 1,                                            // Ready
        [RelayEngine.RoadTripLeaderVariable] = 1,            // 13 PlayerHost (this server writes it)
        [16] = 1,                                            // InitialCarLoading
        [19] = 4,                                            // PlayerStatus
        [27] = 32,                                           // XP (a float)
        [28] = 32,
        [32] = 1,                                            // HasInitialCarBeenChosen
        [RelayEngine.PrivateUniverseVariable] = RelayEngine.PrivateUniverseBits, // 40 PrivateUniverseId (this server writes it)
        [41] = 64,                                           // an id
        [RelayEngine.TeamVariable] = RelayEngine.TeamBits,   // 90 TeamId (this server writes it, NR-10)
    };

    /// <summary>
    /// The variable an update carries when its whole payload is exactly one variable of known width (id, then the value, nothing
    /// else); null otherwise. Routers 2 and 3 use the sanitizer's own tables, which every stored payload was re-encoded with.
    /// </summary>
    internal static int? SingleVariable(byte router, BitBuffer payload)
    {
        var indexWidth = IndexWidth(router);
        if (indexWidth == 0 || payload.Length <= indexWidth) return null;
        var index = (int)new BitReader(payload.Bytes, payload.Length).Read(indexWidth);
        var width = router switch
        {
            1 when index < PlayerVariables.Length => PlayerVariables[index],
            1 => PlayerSingleWidths.GetValueOrDefault(index),
            2 when index < CarVariables.Length => CarVariables[index] == -1 ? 32 : CarVariables[index],
            3 when index < ConsoleVariables.Length => ConsoleVariables[index],
            12 when index < BucketListPlayerVariables.Length => BucketListPlayerVariables[index],
            _ => 0,
        };
        return width > 0 && payload.Length == indexWidth + width ? index : null;
    }

    internal static bool TrySanitize(byte router, BitBuffer payload, out BitBuffer? clean, out PayloadFailure? failure)
    {
        clean = null;
        failure = null;
        var variables = router switch { 1 => PlayerVariables, 2 => CarVariables, 3 => ConsoleVariables, 12 => BucketListPlayerVariables, _ => null };
        if (variables is null) { failure = new(PayloadFault.Router, router); return false; }
        if (payload.Length > MaxBits(router)) { failure = new(PayloadFault.TooBig, payload.Length); return false; }
        var indexWidth = IndexWidth(router);
        var room = router == 1 ? 163 : variables.Length;
        var reader = new BitReader(payload.Bytes, payload.Length);
        var result = new BitBuffer();
        Span<bool> seen = stackalloc bool[32];
        seen.Clear();
        try
        {
            while (reader.Left > 0)
            {
                var start = reader;
                var index = (int)reader.Read(indexWidth);
                if (index >= variables.Length)
                {
                    if (index >= room) { failure = new(PayloadFault.Index, index); return false; }
                    // Unmapped PlayerData tail is retained within its existing size limit, but for TeamId (var 90), which only this
                    // server writes (NR-10, OD-165): walked while its variables' widths are known, var 90 dropped
                    PlayerTail(start, result);
                    clean = result;
                    return true;
                }
                if (seen[index]) { failure = new(PayloadFault.Duplicate, index); return false; }
                seen[index] = true;
                var entry = new BitBuffer().Push((ulong)index, indexWidth);
                switch (variables[index])
                {
                    case -1:
                        var bits = (uint)reader.Read(32);
                        if (!FloatOk(bits, FloatMax)) { failure = new(PayloadFault.NotFinite, index); return false; }
                        entry.Push(bits, 32);
                        break;
                    case -2:
                        var count = (int)reader.Read(5);
                        if (count > NetworkNameMax) { failure = new(PayloadFault.NameTooLong, count); return false; }
                        entry.Push((ulong)count, 5);
                        for (var i = 0; i < count; i++) entry.Push(reader.Read(16), 16);
                        break;
                    default:
                        var value = reader.Read(variables[index]);
                        // a bucket-list State past the game's enum (EnumCount 8) is no state the game writes
                        if (router == 12 && index == BucketListStateVariable && value > BucketListStateMax)
                        { failure = new(PayloadFault.OutOfRange, (int)value); return false; }
                        entry.Push(value, variables[index]);
                        break;
                }
                if (router != 3 || index is not (1 or 2 or 4)) result.Append(entry);
            }
            clean = result;
            return true;
        }
        catch (WireFormatException) { failure = new(PayloadFault.Truncated); return false; }
    }

    /// <summary>
    /// The player object's unmapped tail (ids from 12): copied, but for every TeamId (var 90) entry, while each variable's width is
    /// known (<see cref="PlayerSingleWidths"/>); from the first variable of unknown width (or a cut entry) the rest is copied as it
    /// came, unread. True when the whole tail was read (a TeamId further on, if any, could not be seen).
    /// </summary>
    internal static bool PlayerTail(BitReader tail, BitBuffer result)
    {
        while (tail.Left > 0)
        {
            var start = tail;
            if (tail.Left < 8) { result.Append(start.Take(start.Left)); return false; }
            var index = (int)tail.Read(8);
            if (index < PlayerVariables.Length || PlayerSingleWidths.GetValueOrDefault(index) is not (> 0 and var width) || tail.Left < width)
            {
                result.Append(start.Take(start.Left));
                return false;
            }
            var value = tail.Read(width);
            if (index == RelayEngine.TeamVariable) continue; // a client never writes its team
            result.Push((ulong)index, 8).Push(value, width);
        }
        return true;
    }

    /// <summary>Whether a sanitized player payload may still carry a client TeamId: its unmapped tail was not fully read.</summary>
    internal static bool PlayerTailUnread(BitBuffer payload)
    {
        var reader = new BitReader(payload.Bytes, payload.Length);
        try
        {
            while (reader.Left > 0)
            {
                var start = reader;
                var index = (int)reader.Read(8);
                if (index >= PlayerVariables.Length) return !PlayerTail(start, new BitBuffer());
                switch (PlayerVariables[index])
                {
                    case -2: var count = (int)reader.Read(5); reader.Skip(16 * count); break;
                    default: reader.Skip(PlayerVariables[index]); break;
                }
            }
            return false;
        }
        catch (WireFormatException) { return true; }
    }

    internal static bool FloatOk(uint bits, float maximum)
    {
        var number = BitConverter.UInt32BitsToSingle(bits);
        return float.IsFinite(number) && Math.Abs(number) <= maximum;
    }

    internal static Vector3? CarPosition(BitBuffer body)
    {
        if (body.Length != NerdCodec.CarBodyBits) return null;
        var reader = new BitReader(body.Bytes, body.Length);
        var x = (uint)reader.Read(32); var y = (uint)reader.Read(32); var z = (uint)reader.Read(32);
        return FloatOk(x, PositionMax) && FloatOk(y, PositionMax) && FloatOk(z, PositionMax)
            ? new Vector3(BitConverter.UInt32BitsToSingle(x), BitConverter.UInt32BitsToSingle(y), BitConverter.UInt32BitsToSingle(z)) : null;
    }

    internal static bool CarEventOk(byte id, BitBuffer body)
    {
        if (NerdCodec.CarEventBits(id) != body.Length) return false;
        var reader = new BitReader(body.Bytes, body.Length);
        if (id == 20) reader.Skip(32);
        var count = id == 20 ? 9 : id == 21 ? 6 : 0;
        for (var i = 0; i < count; i++) if (!FloatOk((uint)reader.Read(32), FloatMax)) return false;
        return true;
    }

    internal static BitBuffer SessionHost(params (byte Variable, ulong Value, int Width)[] entries)
    {
        var output = new BitBuffer();
        foreach (var (variable, value, width) in entries) output.Push(variable, 4).Push(value, width);
        return output;
    }

    internal static BitBuffer GroupSatNav(int node) => SessionHost((12, unchecked((uint)node), 32));

    internal const int CarMeetNameMax = 64;

    /// <summary>
    /// CarMeetSynced variables (5-bit ids): 1 InviteCountDownTime (u32, its default 10000), and when given 16 GameModeSessionName /
    /// 17 GameModeSessionTemplateName (a 7-bit count, at most 64, then that many UTF-16 units). JoinTypes and the invite stay at 0.
    /// </summary>
    internal static BitBuffer CarMeet(string? sessionName = null, string? templateName = null, long countdownMs = 10000)
    {
        var output = new BitBuffer().Push(1, 5).Push((ulong)countdownMs, 32);
        foreach (var (variable, text) in new[] { (16, sessionName), (17, templateName) })
        {
            if (text is null) continue;
            var count = Math.Min(text.Length, CarMeetNameMax);
            output.Push((ulong)variable, 5).Push((ulong)count, 7);
            for (var i = 0; i < count; i++) output.Push(text[i], 16);
        }
        return output;
    }

    internal static IReadOnlyList<(byte Variable, ulong Value)>? ConsoleVariablesOf(BitBuffer payload)
    {
        var result = new List<(byte, ulong)>();
        var reader = new BitReader(payload.Bytes, payload.Length);
        try
        {
            while (reader.Left > 0)
            {
                var index = (byte)reader.Read(4);
                if (index >= ConsoleVariables.Length) return null;
                result.Add((index, reader.Read(ConsoleVariables[index])));
            }
            return result;
        }
        catch (WireFormatException) { return null; }
    }

    internal static OwnershipRequest? Ownership(BitBuffer body)
    {
        var reader = new BitReader(body.Bytes, body.Length);
        try
        {
            var router = (byte)reader.Read(4);
            var widths = NerdCodec.RouterWidths(router);
            var type = (uint)reader.Read(widths.Type);
            return new(router, type, ObjectHandle.Read(ref reader, widths.Id));
        }
        catch (WireFormatException) { return null; }
    }

    internal static BitBuffer GrantOwnership(byte router, ObjectHandle handle, byte sequence, BitBuffer payload)
    {
        if (payload.Length > NerdCodec.MaxPayloadBits) throw new WireFormatException(WireError.LengthLimit);
        var output = new BitBuffer().Push(11, 7).Push(router, 4);
        handle.Write(output, NerdCodec.RouterWidths(router).Id);
        return output.Push(sequence, 8).Push((ulong)payload.Length, 14).Append(payload);
    }

    internal static BitBuffer TransferOwnership(byte router, ObjectHandle handle, byte sequence)
    {
        var output = new BitBuffer().Push(12, 7).Push(router, 4);
        handle.Write(output, NerdCodec.RouterWidths(router).Id);
        return output.Push(sequence, 8);
    }

    internal static bool? ReadyRequest(BitBuffer body)
    {
        var reader = new BitReader(body.Bytes, body.Length);
        try { ObjectHandle.Read(ref reader, 2); return reader.Boolean(); }
        catch (WireFormatException) { return null; }
    }

    internal static BitBuffer? ScoreForOthers(BitBuffer body, byte sender)
    {
        var reader = new BitReader(body.Bytes, body.Length);
        try
        {
            var own = ObjectHandle.Read(ref reader, 7);
            if (own.Kind != HandleKind.Own) return null;
            var result = new BitBuffer();
            new ObjectHandle(HandleKind.Relay, own.Id, sender).Write(result, 7);
            return result.Append(reader.Take(reader.Left));
        }
        catch (WireFormatException) { return null; }
    }

    internal static BitBuffer StartingGrid(IReadOnlyList<ObjectHandle> handles)
    {
        var output = new BitBuffer().Push(39, 7);
        new ObjectHandle(HandleKind.Own, 0).Write(output, 2);
        output.Push((ulong)Math.Min(12, handles.Count), 4);
        foreach (var handle in handles.Take(12)) handle.Write(output, 7);
        return output;
    }

    /// <summary>
    /// MsgCarMeetNotifyPending (29): the 2-bit pending action. <see cref="CarMeetPendingWait"/>: wait for the session the server
    /// makes, then join it (the game shows WAITING FOR SESSION). <see cref="CarMeetPendingNone"/>: no pending action; a game that
    /// was waiting for the server's session ends that matchmaking as aborted by the server and leaves the wait.
    /// </summary>
    internal static BitBuffer CarMeetNotifyPending(byte action) => new BitBuffer().Push(29, 7).Push(action, 2);

    internal const byte CarMeetPendingNone = 0, CarMeetPendingWait = 1;

    /// <summary>MsgCarMeetSessionReady (32): the session's name and template, each a compact-length Latin-1 string.</summary>
    internal static BitBuffer CarMeetSessionReady(string name, string template) =>
        new BitBuffer().Push(32, 7).ByteString(System.Text.Encoding.Latin1.GetBytes(name)).ByteString(System.Text.Encoding.Latin1.GetBytes(template));

    /// <summary>MsgReadyRequestResponse (42): the lobby's event key, then the 2-bit answer (0 accepted).</summary>
    internal static BitBuffer ReadyResponse(byte code)
    {
        var output = new BitBuffer().Push(42, 7);
        new ObjectHandle(HandleKind.Own, 0).Write(output, 2);
        return output.Push(code, 2);
    }

    internal static BitBuffer StartRace(uint time)
    {
        var output = new BitBuffer().Push(40, 7);
        new ObjectHandle(HandleKind.Own, 0).Write(output, 2);
        return output.Push(time, 32);
    }

    internal static NerdMessage.Object ConsoleUpdate(uint id, byte sequence, params (byte Variable, bool Value)[] entries)
    {
        var payload = new BitBuffer();
        foreach (var (variable, value) in entries) payload.Push(variable, 4).Boolean(value);
        return new(false, 3, new(HandleKind.Server, id), 0, sequence, payload);
    }

    /// <summary>The highest BucketListPlayerStateEnum value (EnumCount is 8): a larger State only comes from a misread payload.</summary>
    internal const int BucketListStateMax = 7;

    /// <summary>
    /// A BucketListPlayerStateSynced payload's variables, in order. A payload that does not read fully gives its clean prefix: the
    /// variables before the first unknown id, repeated id, cut value or impossible State.
    /// </summary>
    internal static IReadOnlyList<(byte Variable, ulong Value)> BucketListPlayerOf(BitBuffer payload)
    {
        var result = new List<(byte, ulong)>();
        var reader = new BitReader(payload.Bytes, payload.Length);
        var seen = 0;
        try
        {
            while (reader.Left > 0)
            {
                var index = (byte)reader.Read(3);
                if (index >= BucketListPlayerVariables.Length || (seen & (1 << index)) != 0) break;
                var value = reader.Read(BucketListPlayerVariables[index]);
                if (index == BucketListStateVariable && value > BucketListStateMax) break;
                seen |= 1 << index;
                result.Add((index, value));
            }
        }
        catch (WireFormatException) { /* cut: the variables before it */ }
        return result;
    }

    /// <summary>BucketListPlayerStateSynced variables re-encoded in the game's layout (3-bit ids, the pinned widths).</summary>
    internal static BitBuffer BucketListPlayer(IEnumerable<(byte Variable, ulong Value)> variables)
    {
        var output = new BitBuffer();
        foreach (var (variable, value) in variables) output.Push(variable, 3).Push(value, BucketListPlayerVariables[variable]);
        return output;
    }

    /// <summary>
    /// The player object's NetworkName (var 3) in one relayed payload, the last one it carries: read from the payload's start while the
    /// variables' widths are known (<see cref="PlayerVariables"/>, <see cref="PlayerSingleWidths"/>). A name after a variable of unknown
    /// width in the same payload is not seen; each payload is read on its own, so a later update carrying the name still is. Null: none.
    /// </summary>
    internal static string? PlayerName(BitBuffer payload)
    {
        var reader = new BitReader(payload.Bytes, payload.Length);
        string? name = null;
        try
        {
            while (reader.Left >= 8)
            {
                var index = (int)reader.Read(8);
                if (index >= PlayerVariables.Length)
                {
                    if (PlayerSingleWidths.GetValueOrDefault(index) is not (> 0 and var width)) return name;
                    reader.Skip(width);
                    continue;
                }
                if (PlayerVariables[index] != -2) { reader.Skip(PlayerVariables[index]); continue; }
                var units = new char[(int)reader.Read(5)];
                for (var i = 0; i < units.Length; i++) units[i] = (char)reader.Read(16);
                name = new string(units);
            }
        }
        catch (WireFormatException) { }
        return name;
    }

    /// <summary>BucketListSlotArraySynced variables (6-bit ids, s32 values), in the order given.</summary>
    internal static BitBuffer BucketListSlots(IEnumerable<(byte Variable, int Value)> variables)
    {
        var output = new BitBuffer();
        foreach (var (variable, value) in variables) output.Push(variable, 6).Push(unchecked((uint)value), 32);
        return output;
    }

    /// <summary>
    /// The co-op partner (65), server to client: the partner's BucketListPlayerStateSynced key as the receiver knows it (7-bit id), the
    /// pair's 5-bit bucket-list universe (the game's ToBucketListUniverse override for the challenge; 0 is free roam), found, then a
    /// compact-length Latin-1 string of at most <paramref name="nameMax"/> bytes.
    /// </summary>
    internal static BitBuffer BucketListPartner(ObjectHandle partner, byte universe, bool found, string name, int nameMax)
    {
        var output = new BitBuffer().Push(65, 7);
        partner.Write(output, 7);
        var text = System.Text.Encoding.Latin1.GetBytes(name.Select(c => c is >= ' ' and <= '~' or >= '\u00a0' and <= '\u00ff' ? c : '?').ToArray());
        return output.Push(universe, 5).Boolean(found).ByteString(text.AsSpan(0, Math.Min(text.Length, nameMax)));
    }

    /// <summary>The co-op challenge's start time (61) on the network clock, server to client.</summary>
    internal static BitBuffer BucketListStart(uint time) => new BitBuffer().Push(61, 7).Push(time, 32);

    /// <summary>A co-op challenge waiting (66), started (67) or ended (68), server to client: one s32.</summary>
    internal static BitBuffer BucketListStatus(byte kind, int value) => new BitBuffer().Push(kind, 7).Push(unchecked((uint)value), 32);

    /// <summary>
    /// MsgInProgressScoreUpdate (43): the sender's own handle (7-bit id), then 33 bits: the f32 score first, then one flag bit. The
    /// game's send writes them in that order (the scoring metric's double narrowed to a float, then a car flag). This returns the
    /// raw f32, unvalidated, and the flag; null when the body does not read.
    /// </summary>
    internal static (float Value, bool Flag)? InProgressScore(BitBuffer body)
    {
        var reader = new BitReader(body.Bytes, body.Length);
        try
        {
            ObjectHandle.Read(ref reader, 7);
            if (reader.Left != 33) return null;
            var value = BitConverter.UInt32BitsToSingle((uint)reader.Read(32));
            return (value, reader.Boolean());
        }
        catch (WireFormatException) { return null; }
    }

    /// <summary>
    /// Whether a MsgLapScore (44) body has the game's layout: the sender's own handle (7-bit id), three compact-length strings of
    /// exactly 8 bytes (the send writes three doubles), then exactly 34 bits (a u32 and two flags).
    /// </summary>
    internal static bool LapScoreOk(BitBuffer body) => LapScore(body) is not null;

    /// <summary>
    /// A MsgLapScore (44) read field by field (<see cref="LapScoreOk"/>'s layout), null when it does not have it. The game's send
    /// (FUN_141804270, filled by FUN_140f924d0 from its scoring object) writes the object's doubles at +0x68, +0x78 and +0x80, its
    /// u32 at +0x8c and its flags at +0x89 and +0x8b, in that order. The ranking comparator (FUN_140fa1440) ranks a non-zero u32
    /// first, then the +0x8b flag, then the +0x80 double; what each one means in a lap is not pinned (the server only logs them).
    /// </summary>
    internal static LapScoreFields? LapScore(BitBuffer body)
    {
        var reader = new BitReader(body.Bytes, body.Length);
        try
        {
            if (ObjectHandle.Read(ref reader, 7).Kind != HandleKind.Own) return null;
            Span<double> values = stackalloc double[3];
            for (var i = 0; i < 3; i++)
            {
                var bytes = reader.ByteString(8);
                if (bytes.Length != 8) return null;
                values[i] = BitConverter.ToDouble(bytes);
            }
            if (reader.Left != 34) return null;
            return new(values[0], values[1], values[2], (uint)reader.Read(32), reader.Boolean(), reader.Boolean());
        }
        catch (WireFormatException) { return null; }
    }

    /// <summary>A SessionHostSynced f32 value: its 32 bits as written by <see cref="SessionHost"/> at width 32.</summary>
    internal static ulong HostFloat(float value) => BitConverter.SingleToUInt32Bits(value);
}
