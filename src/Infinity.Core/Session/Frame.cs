// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Wire;
using System.Buffers.Binary;

namespace Infinity.Core.Session;

/// <summary><c>Capture.what</c> values (frame 0x13, ORCHESTRATOR §6).</summary>
public static class CaptureWhat
{
    public const byte OnlineSuspend = 6;
    public const byte RemotePaintReceiver = 5;
    /// <summary>The DXGI factory shims saw the swapchain and its queue.</summary>
    public const byte Dxgi = 1;

    public const byte WinRtGamepad = 2;

    public const byte XInput = 3;

    /// <summary>The overlay DLL load the <c>Overlay</c> frame asked for (ok = <c>fi_overlay_start</c> returned 0).</summary>
    public const byte OverlayLoad = 4;
}

/// <summary>Bounded terminal control reasons; never player text or an authorization token.</summary>
public enum OnlineSuspendReason : byte { Policy, UpdateRequired, Integrity, SessionEnded, ControlLost }

/// <summary>
/// One control-pipe frame between the launcher and the hook (types 0x01-0x0a, 0x13-0x15;
/// 0x0b-0x12 reserved). Layouts are fixed by the golden vectors in session.rs and
/// Infinity.Core.Tests; they change only together with the C hook and HOOK_MIGRATION.md.
/// </summary>
public abstract record Frame
{
    private Frame()
    {
    }

    public abstract byte Type { get; }
    public sealed record RelayClock(ulong UnixSeconds):Frame
    {
        public override byte Type=>0x19;
    }

    public sealed record RelayNetworkHint(byte[] Payload) : Frame
    {
        public override byte Type=>0x17;
        public override string ToString()=>"RelayNetworkHint { redacted }";
    }
    public sealed record RelayCandidates(byte[] Payload) : Frame
    {
        public override byte Type=>0x18;
        public override string ToString()=>"RelayCandidates { redacted }";
    }

    /// <summary>Grant-scoped secret over the authenticated control pipe only; never display the payload.</summary>
    public sealed record RelayContext(byte[] Payload) : Frame
    {
        public override byte Type=>0x16;
        public override string ToString()=>"RelayContext { redacted }";
        public bool Equals(RelayContext? other)=>other is not null && Payload.AsSpan().SequenceEqual(other.Payload);
        public override int GetHashCode()=>Payload.Length;
        internal static void Validate(ReadOnlySpan<byte> payload)
        {
            if(payload.Length==1 && payload[0]==0)return;
            if(payload.Length!=89 || payload[0]!=1 || new Guid(payload.Slice(1,16),bigEndian:true)==Guid.Empty
                || new Guid(payload.Slice(17,16),bigEndian:true)==Guid.Empty || BinaryPrimitives.ReadUInt64BigEndian(payload[33..])==0
                || BinaryPrimitives.ReadUInt64BigEndian(payload[41..])==0 || BinaryPrimitives.ReadUInt64BigEndian(payload[49..]) is 0 or > long.MaxValue)
                throw new ProtocolException("Invalid relay context");
        }
    }

    /// <summary>Authenticated launcher → hook: permanently stop online activity for this process/session.</summary>
    public sealed record OnlineSuspend(OnlineSuspendReason Reason) : Frame
    {
        public override byte Type => 0x15;
    }

    /// <summary>hook → launcher, first frame.</summary>
    public sealed record Hello(ushort Protocol, uint Pid, string HookVersion) : Frame
    {
        public override byte Type => 0x01;
    }

    /// <summary>launcher → hook: signed claims, their 64-byte signature, and the current session token with its expiry.</summary>
    public sealed record Ticket(byte[] Claims, byte[] Signature, Secret SessionToken, ulong TokenExpiresUnix) : Frame
    {
        public override byte Type => 0x02;

        public bool Equals(Ticket? other) => other is not null && Claims.AsSpan().SequenceEqual(other.Claims)
            && Signature.AsSpan().SequenceEqual(other.Signature) && SessionToken.Equals(other.SessionToken)
            && TokenExpiresUnix == other.TokenExpiresUnix;

        public override int GetHashCode() => HashCode.Combine(Claims.Length, TokenExpiresUnix);
    }

    /// <summary>hook → launcher: the session token is near expiry; answer with a heartbeat and a <see cref="Token"/>.</summary>
    public sealed record RefreshRequest : Frame
    {
        public override byte Type => 0x03;
    }

    /// <summary>launcher → hook: a fresh session token, after every heartbeat.</summary>
    public sealed record Token(Secret SessionToken, ulong ExpiresUnix) : Frame
    {
        public override byte Type => 0x04;
    }

    /// <summary>launcher → hook: text for the pause banner (level 0 info, 1 warning, 2 error).</summary>
    public sealed record Notice(byte Level, string Text) : Frame
    {
        public override byte Type => 0x05;
    }

    /// <summary>hook → launcher: restart the game through the launcher (reason code).</summary>
    public sealed record RelaunchRequest(byte Reason) : Frame
    {
        public override byte Type => 0x06;
    }

    /// <summary>Either side, last frame.</summary>
    public sealed record Bye(byte Reason) : Frame
    {
        public override byte Type => 0x07;
    }

    /// <summary>launcher → hook: no session this time; the hook plays offline and shows the reason.</summary>
    public sealed record Refused(string Reason) : Frame
    {
        public override byte Type => 0x08;
    }

    /// <summary>hook → launcher: something the launcher may act on (1 = a save revision was written).</summary>
    public sealed record Event(byte Kind, string Detail) : Frame
    {
        public override byte Type => 0x09;
    }

    /// <summary>Session file only: the ticket and a one-time code the hook redeems for a session token.</summary>
    public sealed record TicketCode(byte[] Claims, byte[] Signature, string RedeemCode) : Frame
    {
        public override byte Type => 0x0a;

        public bool Equals(TicketCode? other) => other is not null && Claims.AsSpan().SequenceEqual(other.Claims)
            && Signature.AsSpan().SequenceEqual(other.Signature) && RedeemCode == other.RedeemCode;

        public override int GetHashCode() => HashCode.Combine(Claims.Length, RedeemCode);
    }

    /// <summary>hook → launcher (0x13): what the hook captured or loaded (<see cref="CaptureWhat"/>), and whether it worked.</summary>
    public sealed record Capture(byte What, bool Ok) : Frame
    {
        public override byte Type => 0x13;
    }

    /// <summary>
    /// launcher → hook (0x14), right after <c>Ticket</c>: load <c>infinity-overlay.dll</c> at
    /// <paramref name="Path"/> (absolute, under data\bin) when enabled and its SHA-256 matches.
    /// </summary>
    public sealed record Overlay(bool Enabled, byte[] Sha256, string Path) : Frame
    {
        public override byte Type => 0x14;

        public bool Equals(Overlay? other) => other is not null && Enabled == other.Enabled
            && Sha256.AsSpan().SequenceEqual(other.Sha256) && Path == other.Path;

        public override int GetHashCode() => HashCode.Combine(Enabled, Path);
    }

    public byte[] Encode()
    {
        var w = new WireWriter(Type);
        switch (this)
        {
            case RelayClock clock:
                if(clock.UnixSeconds is 0 or >long.MaxValue)throw new ProtocolException("Invalid relay clock");
                var timestamp=new byte[8];BinaryPrimitives.WriteUInt64BigEndian(timestamp,clock.UnixSeconds);w.Fixed(timestamp,8,"relay clock");break;
            case RelayNetworkHint hint:
                Matchmaking.RelayNetworkFrames.Hint(hint.Payload);w.Fixed(hint.Payload,hint.Payload.Length,"relay network hint");break;
            case RelayCandidates candidates:
                Matchmaking.RelayNetworkFrames.ValidateCandidates(candidates.Payload);w.Fixed(candidates.Payload,candidates.Payload.Length,"relay candidates");break;
            case RelayContext context:
                RelayContext.Validate(context.Payload); w.Fixed(context.Payload,context.Payload.Length,"relay context");break;
            case OnlineSuspend suspend:
                if (!Enum.IsDefined(suspend.Reason)) throw new ProtocolException("unknown online suspension reason");
                w.U8((byte)suspend.Reason);
                break;
            case Hello h:
                w.U16(h.Protocol);
                w.U32(h.Pid);
                w.Str(h.HookVersion);
                break;
            case Ticket t:
                w.Bytes(t.Claims);
                w.Fixed(t.Signature, 64, "signature");
                w.Str(t.SessionToken.Expose());
                w.U64(t.TokenExpiresUnix);
                break;
            case TicketCode t:
                w.Bytes(t.Claims);
                w.Fixed(t.Signature, 64, "signature");
                w.Str(t.RedeemCode);
                break;
            case Refused r:
                w.Str(r.Reason);
                break;
            case RefreshRequest:
                break;
            case Token t:
                w.Str(t.SessionToken.Expose());
                w.U64(t.ExpiresUnix);
                break;
            case Notice n:
                w.U8(n.Level);
                w.Str(n.Text);
                break;
            case RelaunchRequest r:
                w.U8(r.Reason);
                break;
            case Bye b:
                w.U8(b.Reason);
                break;
            case Event e:
                w.U8(e.Kind);
                w.Str(e.Detail);
                break;
            case Capture c:
                w.U8(c.What);
                w.Flag(c.Ok);
                break;
            case Overlay o:
                w.Flag(o.Enabled);
                w.Fixed(o.Sha256, 32, "sha256");
                w.Str(o.Path);
                break;
        }
        return w.Finish("frame");
    }

    /// <summary>
    /// Decodes one frame from the start of <paramref name="buffer"/>: null when more bytes are
    /// needed, otherwise the frame and the bytes it used. Throws <see cref="ProtocolException"/>.
    /// </summary>
    public static Frame? Decode(ReadOnlySpan<byte> buffer, out int used)
    {
        used = 0;
        var length = Frames.BodyLength(buffer, "frame");
        if (length is null || buffer.Length < 4 + length.Value)
        {
            return null;
        }
        var type = buffer[4];
        var r = new WireReader(buffer[5..(4 + length.Value)], "frame");
        Frame frame = type switch
        {
            0x01 => new Hello(r.U16(), r.U32(), r.Str()),
            0x02 => new Ticket(r.Bytes(), r.Take(64).ToArray(), new Secret(r.Str()), r.U64()),
            0x03 => new RefreshRequest(),
            0x04 => new Token(new Secret(r.Str()), r.U64()),
            0x05 => new Notice(r.U8(), r.Str()),
            0x06 => new RelaunchRequest(r.U8()),
            0x07 => new Bye(r.U8()),
            0x08 => new Refused(r.Str()),
            0x09 => new Event(r.U8(), r.Str()),
            0x0a => new TicketCode(r.Bytes(), r.Take(64).ToArray(), r.Str()),
            0x13 => new Capture(r.U8(), r.Flag("flag byte")),
            0x14 => new Overlay(r.Flag("flag byte"), r.Take(32).ToArray(), r.Str()),
            0x15 => new OnlineSuspend((OnlineSuspendReason)r.U8()),
            0x16 => new RelayContext(r.Take(length.Value-1).ToArray()),
            0x17 => new RelayNetworkHint(r.Take(length.Value-1).ToArray()),
            0x18 => new RelayCandidates(r.Take(length.Value-1).ToArray()),
            0x19 => new RelayClock(BinaryPrimitives.ReadUInt64BigEndian(r.Take(8))),
            _ => throw new ProtocolException($"unknown frame type 0x{type:x2}"),
        };
        r.End();
        if(frame is RelayContext context)RelayContext.Validate(context.Payload);
        if(frame is RelayClock relayClock && relayClock.UnixSeconds is 0 or >long.MaxValue)throw new ProtocolException("Invalid relay clock");
        if(frame is RelayNetworkHint hint)Matchmaking.RelayNetworkFrames.Hint(hint.Payload);
        if(frame is RelayCandidates candidates)Matchmaking.RelayNetworkFrames.ValidateCandidates(candidates.Payload);
        if (frame is OnlineSuspend suspend && !Enum.IsDefined(suspend.Reason))
            throw new ProtocolException("unknown online suspension reason");
        used = 4 + length.Value;
        return frame;
    }
}
