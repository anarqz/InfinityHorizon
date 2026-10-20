// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

internal enum HandleKind : byte { Own, Relay, Server, Transfer }

internal readonly record struct ObjectHandle(HandleKind Kind, uint Id, byte Peer = 0)
{
    internal static ObjectHandle Read(ref BitReader reader, int width)
    {
        var a = reader.Boolean();
        var b = reader.Boolean();
        var id = (uint)reader.Read(width);
        return new(a ? (b ? HandleKind.Server : HandleKind.Transfer) : (b ? HandleKind.Relay : HandleKind.Own),
            id, !a && b ? (byte)reader.Read(8) : (byte)0);
    }

    internal void Write(BitBuffer writer, int width)
    {
        writer.Boolean(Kind is HandleKind.Server or HandleKind.Transfer)
            .Boolean(Kind is HandleKind.Server or HandleKind.Relay).Push(Id, width);
        if (Kind == HandleKind.Relay) writer.Push(Peer, 8);
    }
}

/// <summary>Internal game-wire values, not platform HTTP DTOs. Layouts match deprecated/native-relay/src/nerd.rs.</summary>
internal abstract record NerdMessage
{
    internal abstract byte Id { get; }
    internal sealed record OpenConnection(byte[] Version, byte[] Game, byte Flags, ulong User, uint A, uint B, uint Attempt) : NerdMessage { internal override byte Id => 0; }
    internal sealed record OpenResponse(byte Flags, byte Reason, uint Attempt, uint OwnAttempt) : NerdMessage { internal override byte Id => 1; }
    internal sealed record Close : NerdMessage { internal override byte Id => 2; }
    internal sealed record Ping(uint Time) : NerdMessage { internal override byte Id => 3; }
    internal sealed record Pong(uint Echo, uint Time) : NerdMessage { internal override byte Id => 4; }
    internal sealed record TimeSync(ushort Index, uint Time) : NerdMessage { internal override byte Id => 5; }
    internal sealed record SyncDone(uint Attempt) : NerdMessage { internal override byte Id => 6; }
    internal sealed record Object(bool Create, byte Router, ObjectHandle Handle, byte Type, byte Sequence, BitBuffer Payload) : NerdMessage { internal override byte Id => Create ? (byte)7 : (byte)8; }
    internal sealed record Delete(byte Router, ObjectHandle Handle, byte Sequence) : NerdMessage { internal override byte Id => 9; }
    internal sealed record Car(ObjectHandle Handle, BitBuffer Body) : NerdMessage { internal override byte Id => 15; }
    internal sealed record CarEvent(byte Kind, ObjectHandle Handle, BitBuffer Body) : NerdMessage { internal override byte Id => Kind; }
    internal sealed record TagCollision(ObjectHandle A, ObjectHandle B, byte[] Values) : NerdMessage { internal override byte Id => 36; }
    internal sealed record Traffic(byte Kind, BitBuffer Body) : NerdMessage { internal override byte Id => Kind; }
    internal sealed record DetachPart(byte Kind, byte[] Data) : NerdMessage { internal override byte Id => 24; }
    internal sealed record Chat(ulong Xuid, byte[] Data) : NerdMessage { internal override byte Id => 33; }
    internal sealed record FinaliseSessionEntry : NerdMessage { internal override byte Id => 34; }
    internal sealed record InactivePlayers(ObjectHandle Handle) : NerdMessage { internal override byte Id => 70; }
    internal sealed record GroupSatNav(ObjectHandle Handle, int Node) : NerdMessage { internal override byte Id => 52; }
    internal sealed record Other(byte Kind, BitBuffer Body) : NerdMessage { internal override byte Id => Kind; }
}
