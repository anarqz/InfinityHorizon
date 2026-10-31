// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Numerics;

namespace Infinity.Core.WorldServer;

internal enum PayloadFault { Router, Index, Duplicate, NotFinite, NameTooLong, Truncated, TooBig }
internal readonly record struct PayloadFailure(PayloadFault Fault, int Value = 0);
internal readonly record struct OwnershipRequest(byte Router, uint Type, ObjectHandle Handle);

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
    internal static int? MaxBits(byte router) => router switch { 1 => PlayerDataMaxBits, 2 => 787, 3 => 47, _ => null };

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
        var indexWidth = router switch { 1 => 8, 2 => 5, 3 => 4, _ => 0 };
        if (indexWidth == 0 || payload.Length <= indexWidth) return null;
        var index = (int)new BitReader(payload.Bytes, payload.Length).Read(indexWidth);
        var width = router switch
        {
            1 when index < PlayerVariables.Length => PlayerVariables[index],
            1 => PlayerSingleWidths.GetValueOrDefault(index),
            2 when index < CarVariables.Length => CarVariables[index] == -1 ? 32 : CarVariables[index],
            3 when index < ConsoleVariables.Length => ConsoleVariables[index],
            _ => 0,
        };
        return width > 0 && payload.Length == indexWidth + width ? index : null;
    }

    internal static bool TrySanitize(byte router, BitBuffer payload, out BitBuffer? clean, out PayloadFailure? failure)
    {
        clean = null;
        failure = null;
        var variables = router switch { 1 => PlayerVariables, 2 => CarVariables, 3 => ConsoleVariables, _ => null };
        if (variables is null) { failure = new(PayloadFault.Router, router); return false; }
        if (payload.Length > MaxBits(router)) { failure = new(PayloadFault.TooBig, payload.Length); return false; }
        var indexWidth = router == 1 ? 8 : router == 2 ? 5 : 4;
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
                    // Unmapped PlayerData tail is retained within its existing size limit.
                    result.Append(start.Take(start.Left));
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
                    default: entry.Push(reader.Read(variables[index]), variables[index]); break;
                }
                if (router != 3 || index is not (1 or 2 or 4)) result.Append(entry);
            }
            clean = result;
            return true;
        }
        catch (WireFormatException) { failure = new(PayloadFault.Truncated); return false; }
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

    /// <summary>MsgCarMeetNotifyPending (29): the 2-bit pending action (1: wait for the session the server makes, then join it).</summary>
    internal static BitBuffer CarMeetNotifyPending(byte action) => new BitBuffer().Push(29, 7).Push(action, 2);

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
}
