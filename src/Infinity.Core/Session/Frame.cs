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

    /// <summary>
    /// CR-S57: the hook can fall back to a grant's relay (it takes <see cref="Frame.RelayLeg"/> and sends
    /// <see cref="Frame.RelayFallback"/>). Only when ok in this session does a join send <c>nat</c> and <c>relays</c>.
    /// </summary>
    public const byte RelayJoiner = 7;

    /// <summary>
    /// CR-L40: the hook asks for a lobby (<see cref="Frame.FreeRoamNeed"/>) before FREE ROAM ONLINE's matchmaking
    /// request and waits for <see cref="Frame.FreeRoamReady"/>; sent with ok at every handshake.
    /// </summary>
    public const byte FreeRoam = 8;
}

/// <summary><see cref="Frame.FollowHost"/> lobby types: the game's own <c>StartMatchmakingEventArgument</c> lobby type.</summary>
public static class FollowHostLobby
{
    public const byte RoadTrip = 1;
    public const byte FreeRoam = 2;
}

/// <summary><see cref="Frame.RelayFallback"/> states (hook → launcher, CR-S57).</summary>
public static class RelayFallbackState
{
    /// <summary>The direct punch failed: renew the seat with <c>transport: "relay"</c> now and push the leg with transport relay.</summary>
    public const byte DirectFailed = 1;
    /// <summary>The pair's datagrams flow through the grant's relay.</summary>
    public const byte RelayOk = 2;
    /// <summary>The relay stayed unavailable (10 s of retries): the host could not be reached.</summary>
    public const byte RelayUnavailable = 3;
    /// <summary>The pair is direct.</summary>
    public const byte DirectOk = 4;
}

/// <summary><c>ModsReport.phase</c>.</summary>
public static class ModsReportPhase
{
    public const byte Loading = 0;
    public const byte Ready = 1;
    public const byte Failed = 2;
}

/// <summary><c>ModsReportEntry.state</c>.</summary>
public static class ModsReportState
{
    public const byte Pending = 0;
    public const byte Initialized = 1;
    public const byte Failed = 2;
}

/// <summary>One mod of a <see cref="Frame.ModsReport"/>: the plan's id and version, the hashes the runtime verified, the echoed compatibility fingerprint.</summary>
public sealed record ModsReportEntry(string Id, string Version, byte[] BinarySha256, byte[] SettingsSha256, byte[] CompatibilitySha256, byte State)
{
    public bool Equals(ModsReportEntry? other) => other is not null && Id == other.Id && Version == other.Version && State == other.State
        && BinarySha256.AsSpan().SequenceEqual(other.BinarySha256) && SettingsSha256.AsSpan().SequenceEqual(other.SettingsSha256)
        && CompatibilitySha256.AsSpan().SequenceEqual(other.CompatibilitySha256);

    public override int GetHashCode() => HashCode.Combine(Id, State);
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

    /// <summary>
    /// hook → launcher (0x1b, N-H2, OD-112): what the native mod runtime actually initialized, bound to
    /// this launch by the plan's <paramref name="Nonce"/> and <paramref name="PlanSha256"/>
    /// (<see cref="ModsReportPhase"/>). The hashes are the files the runtime verified before loading them;
    /// the compatibility hash is the launcher's own <c>infinity-mod-compatibility-v1</c> fingerprint, echoed.
    /// Compatibility evidence, not anti-cheat: accept it only when the nonce and plan digest are this
    /// launch's and every binary and settings hash equals what the launcher prepared.
    /// </summary>
    public sealed record ModsReport(byte[] Nonce, byte[] PlanSha256, byte Phase, IReadOnlyList<ModsReportEntry> Mods) : Frame
    {
        public const byte Version = 1;
        public const int MaxMods = 32;

        public override byte Type => 0x1b;

        public bool Equals(ModsReport? other) => other is not null && Nonce.AsSpan().SequenceEqual(other.Nonce)
            && PlanSha256.AsSpan().SequenceEqual(other.PlanSha256) && Phase == other.Phase && Mods.SequenceEqual(other.Mods);

        public override int GetHashCode() => HashCode.Combine(Phase, Mods.Count);

        internal void Validate()
        {
            if (Nonce.Length != 16 || Nonce.All(b => b == 0) || PlanSha256.Length != 32 || Phase > ModsReportPhase.Failed || Mods.Count > MaxMods)
                throw new ProtocolException("Invalid mods report");
            foreach (var mod in Mods)
            {
                if (mod.Id.Length is 0 or > 64 || mod.Version.Length > 64 || !mod.Id.All(c => c is > ' ' and < (char)0x7f) || !mod.Version.All(c => c is >= ' ' and < (char)0x7f)
                    || mod.BinarySha256.Length != 32 || mod.SettingsSha256.Length != 32 || mod.CompatibilitySha256.Length != 32 || mod.State > ModsReportState.Failed)
                    throw new ProtocolException("Invalid mods report entry");
            }
            if (Mods.Select(m => m.Id).Distinct(StringComparer.Ordinal).Count() != Mods.Count) throw new ProtocolException("Duplicate mods report entry");
            if (Phase == ModsReportPhase.Ready && Mods.Any(m => m.State != ModsReportState.Initialized))
                throw new ProtocolException("A ready mods report lists a mod that is not initialized");
        }
    }

    /// <summary>
    /// launcher → hook (0x1c, CR-L39), right before <see cref="Ticket"/> and only for a custom server: the launch-ticket keys its
    /// <c>/info</c> published and the realm's pin matched. With it the hook verifies the ticket only against these keys and takes
    /// a game server on any host (plain http too), never an official one. <c>{u8 1, u8 count 1-8, count x {str kid, [32] key}}</c>.
    /// </summary>
    public sealed record CustomTrust(IReadOnlyList<(string Kid, byte[] Key)> Keys) : Frame
    {
        public const byte Version = 1;
        public const int MaxKeys = 8;

        public override byte Type => 0x1c;

        public bool Equals(CustomTrust? other) => other is not null && Keys.Count == other.Keys.Count
            && Keys.Zip(other.Keys).All(pair => pair.First.Kid == pair.Second.Kid && pair.First.Key.AsSpan().SequenceEqual(pair.Second.Key));

        public override int GetHashCode() => Keys.Count;

        internal void Validate()
        {
            if (Keys.Count is 0 or > MaxKeys) throw new ProtocolException("Invalid custom trust");
            foreach (var (kid, key) in Keys)
                if (kid is not { Length: >= 1 and <= 32 } || !kid.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-') || key is not { Length: 32 })
                    throw new ProtocolException("Invalid custom trust key");
        }
    }

    public sealed record UdpDiagnostic(byte[] Payload):Frame
    {
        public override byte Type=>0x1a;
        public override string ToString()=>"UdpDiagnostic { bounded }";
        public static bool Valid(ReadOnlySpan<byte> p)=>p.Length==32 && p[0]==1 && p[1] is >=1 and <=28 && p[2]<=2 && p[3]<=4;
    }
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

    /// <summary>
    /// launcher → hook (0x1d, CR-S57): the relay a verified grant names, for the joiner's fallback.
    /// <c>{u8 1, grant[16], u8 transport (0 direct, 1 relay), relayIPv4[4], u16 BE rendezvous port, u16 BE claimsLen,
    /// claims[1-2048], sig[64]}</c>; clear = <c>{u8 1, grant[16] zero}</c>. The claims and signature are the grant's raw
    /// signed bytes (the hook presents them to the relay). Never display the payload.
    /// </summary>
    public sealed record RelayLeg(byte[] Payload) : Frame
    {
        public const byte Version = 1;
        public const int MaxClaims = 2048;
        public const int ClearLength = 17;

        public override byte Type => 0x1d;
        public override string ToString() => "RelayLeg { redacted }";
        public bool Equals(RelayLeg? other) => other is not null && Payload.AsSpan().SequenceEqual(other.Payload);
        public override int GetHashCode() => Payload.Length;

        public static RelayLeg Clear() => new([Version, .. new byte[16]]);

        /// <summary>The leg of <paramref name="grant"/> at <paramref name="relay"/> (an IPv4 address) and its rendezvous port.</summary>
        public static RelayLeg Build(Guid grant, bool viaRelay, System.Net.IPAddress relay, ushort port, ReadOnlySpan<byte> claims, ReadOnlySpan<byte> signature)
        {
            ArgumentNullException.ThrowIfNull(relay);
            if (relay.AddressFamily != System.Net.Sockets.AddressFamily.InterNetwork || claims.Length is 0 or > MaxClaims || signature.Length != 64)
                throw new ProtocolException("Invalid relay leg");
            var payload = new byte[90 + claims.Length];
            payload[0] = Version;
            grant.TryWriteBytes(payload.AsSpan(1, 16), bigEndian: true, out _);
            payload[17] = viaRelay ? (byte)1 : (byte)0;
            relay.TryWriteBytes(payload.AsSpan(18, 4), out _);
            BinaryPrimitives.WriteUInt16BigEndian(payload.AsSpan(22), port);
            BinaryPrimitives.WriteUInt16BigEndian(payload.AsSpan(24), (ushort)claims.Length);
            claims.CopyTo(payload.AsSpan(26));
            signature.CopyTo(payload.AsSpan(26 + claims.Length));
            Validate(payload);
            return new(payload);
        }

        internal static void Validate(ReadOnlySpan<byte> payload)
        {
            if (payload.Length == ClearLength && payload[0] == Version && !payload[1..].ContainsAnyExcept((byte)0)) return;
            // the hook refuses an IPv4 whose first byte is 0 or 224+ (this network, multicast, reserved)
            if (payload.Length < 91 || payload[0] != Version || !payload.Slice(1, 16).ContainsAnyExcept((byte)0) || payload[17] > 1
                || payload[18] is 0 or >= 224 || BinaryPrimitives.ReadUInt16BigEndian(payload[22..]) == 0
                || BinaryPrimitives.ReadUInt16BigEndian(payload[24..]) is var claims && (claims is 0 or > MaxClaims || payload.Length != 90 + claims))
                throw new ProtocolException("Invalid relay leg");
        }
    }

    /// <summary>hook → launcher (0x1e, CR-S57): <c>{grant[16], u8 state}</c>, one of <see cref="RelayFallbackState"/>.</summary>
    public sealed record RelayFallback(Guid Grant, byte State) : Frame
    {
        public override byte Type => 0x1e;

        internal static void Validate(Guid grant, byte state)
        {
            if (grant == Guid.Empty || state is < RelayFallbackState.DirectFailed or > RelayFallbackState.DirectOk)
                throw new ProtocolException("Invalid relay fallback");
        }
    }

    /// <summary>
    /// hook → launcher (0x1f, CR-L40): <c>{u32 LE nonce, u8 kind}</c>, the kind a <see cref="Matchmaking.FreeRoamKind"/> (an
    /// older hook's <c>{u32 nonce}</c> reads as free roam). FREE ROAM ONLINE's, a car meet's or an online Road Trip's matchmaking
    /// request waits (at most 20 s) until the launcher answers <see cref="FreeRoamReady"/> with the same nonce.
    /// </summary>
    public sealed record FreeRoamNeed(uint Nonce, byte Kind = Matchmaking.FreeRoamKind.FreeRoam) : Frame
    {
        public override byte Type => 0x1f;

        internal static void Validate(byte kind)
        {
            if (kind > Matchmaking.FreeRoamKind.RoadTrip) throw new ProtocolException("Invalid free roam kind");
        }
    }

    /// <summary>
    /// launcher → hook (0x20, CR-L40): <c>{u32 LE nonce, u8 result}</c>, the result one of
    /// <see cref="Matchmaking.FreeRoamResult"/> (0 a world is ready, 1 none, 2 not online).
    /// </summary>
    public sealed record FreeRoamReady(uint Nonce, byte Result) : Frame
    {
        public override byte Type => 0x20;

        internal static void Validate(byte result)
        {
            if (result > Matchmaking.FreeRoamResult.NotOnline) throw new ProtocolException("Invalid free roam result");
        }
    }

    /// <summary>
    /// hook → launcher (0x21, CR-L41.3): <c>{u32 LE level, u64 LE credits}</c>, the XP level
    /// (<c>playerRaceHistory[0].level</c>) and credits (<c>playerHistory[0].credits</c>) of an <c>UpdateAll</c> the hook let
    /// through, sent only when they changed. For the Discord presence; never an authority on progress.
    /// </summary>
    public sealed record Progress(uint Level, ulong Credits) : Frame
    {
        public override byte Type => 0x21;
    }

    /// <summary>
    /// hook → launcher (0x22, CR-L41.3): <c>{i32 LE carKey}</c>, the local player's current car when it changed: the raw i32 at
    /// current+0x40, always &gt; 0, <c>Data_Car.Id × 1000</c> (<see cref="Infinity.Core.Process.CarNames"/> maps it to the name).
    /// For the Discord presence.
    /// </summary>
    public sealed record Car(int CarKey) : Frame
    {
        public override byte Type => 0x22;

        internal static void Validate(int key)
        {
            if (key <= 0) throw new ProtocolException("Invalid car key");
        }
    }

    /// <summary>
    /// launcher → hook (0x23, OD-151): <c>{u8 lobbyType, u8 public}</c>. The seated guest's host now hosts another activity:
    /// the hook starts the game's own matchmaking for that lobby type (<see cref="FollowHostLobby"/>: 1 Road Trip, 2 free roam),
    /// as the pause menu's ONLINE ROAD TRIP / ONLINE FREE ROAM does, so the game's FINDING SESSION banner asks the player to
    /// confirm with (X). Nothing happens when the game is already in, or already looking for, that lobby type.
    /// </summary>
    public sealed record FollowHost(byte LobbyType, bool Public) : Frame
    {
        public override byte Type => 0x23;

        internal static void Validate(byte lobby)
        {
            if (lobby is not (FollowHostLobby.RoadTrip or FollowHostLobby.FreeRoam)) throw new ProtocolException("Invalid follow-host lobby type");
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
            case ModsReport report:
                report.Validate();
                w.U8(ModsReport.Version);
                w.Fixed(report.Nonce, 16, "mods nonce");
                w.Fixed(report.PlanSha256, 32, "mods plan");
                w.U8(report.Phase);
                w.U8((byte)report.Mods.Count);
                foreach (var mod in report.Mods)
                {
                    w.Str(mod.Id);
                    w.Str(mod.Version);
                    w.Fixed(mod.BinarySha256, 32, "mod binary");
                    w.Fixed(mod.SettingsSha256, 32, "mod settings");
                    w.Fixed(mod.CompatibilitySha256, 32, "mod compatibility");
                    w.U8(mod.State);
                }
                break;
            case CustomTrust trust:
                trust.Validate();
                w.U8(CustomTrust.Version);
                w.U8((byte)trust.Keys.Count);
                foreach (var (kid, key) in trust.Keys)
                {
                    w.Str(kid);
                    w.Fixed(key, 32, "custom trust key");
                }
                break;
            case UdpDiagnostic diagnostic:
                if(!UdpDiagnostic.Valid(diagnostic.Payload))throw new ProtocolException("Invalid UDP diagnostic");
                w.Fixed(diagnostic.Payload,32,"UDP diagnostic");break;
            case RelayClock clock:
                if(clock.UnixSeconds is 0 or >long.MaxValue)throw new ProtocolException("Invalid relay clock");
                var timestamp=new byte[8];BinaryPrimitives.WriteUInt64BigEndian(timestamp,clock.UnixSeconds);w.Fixed(timestamp,8,"relay clock");break;
            case RelayNetworkHint hint:
                Matchmaking.RelayNetworkFrames.Hint(hint.Payload);w.Fixed(hint.Payload,hint.Payload.Length,"relay network hint");break;
            case RelayCandidates candidates:
                Matchmaking.RelayNetworkFrames.ValidateCandidates(candidates.Payload);w.Fixed(candidates.Payload,candidates.Payload.Length,"relay candidates");break;
            case RelayContext context:
                RelayContext.Validate(context.Payload); w.Fixed(context.Payload,context.Payload.Length,"relay context");break;
            case RelayLeg leg:
                RelayLeg.Validate(leg.Payload); w.Fixed(leg.Payload, leg.Payload.Length, "relay leg"); break;
            case RelayFallback fallback:
                RelayFallback.Validate(fallback.Grant, fallback.State);
                var grant = new byte[16]; fallback.Grant.TryWriteBytes(grant, bigEndian: true, out _);
                w.Fixed(grant, 16, "relay fallback grant"); w.U8(fallback.State); break;
            case FreeRoamNeed need:
                FreeRoamNeed.Validate(need.Kind);
                w.U32(need.Nonce); w.U8(need.Kind); break;
            case FreeRoamReady ready:
                FreeRoamReady.Validate(ready.Result);
                w.U32(ready.Nonce); w.U8(ready.Result); break;
            case Progress progress:
                w.U32(progress.Level); w.U64(progress.Credits); break;
            case Car car:
                Car.Validate(car.CarKey);
                w.U32(unchecked((uint)car.CarKey)); break;
            case FollowHost follow:
                FollowHost.Validate(follow.LobbyType);
                w.U8(follow.LobbyType); w.Flag(follow.Public); break;
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
            0x1a => new UdpDiagnostic(r.Take(length.Value-1).ToArray()),
            0x1b => DecodeModsReport(ref r),
            0x1c => DecodeCustomTrust(ref r),
            0x1d => new RelayLeg(r.Take(length.Value - 1).ToArray()),
            0x1e => new RelayFallback(new Guid(r.Take(16), bigEndian: true), r.U8()),
            0x1f => length.Value - 1 == 4 ? new FreeRoamNeed(r.U32()) : new FreeRoamNeed(r.U32(), r.U8()),
            0x20 => new FreeRoamReady(r.U32(), r.U8()),
            0x21 => new Progress(r.U32(), r.U64()),
            0x22 => new Car(unchecked((int)r.U32())),
            0x23 => new FollowHost(r.U8(), r.Flag("follow-host public")),
            _ => throw new ProtocolException($"unknown frame type 0x{type:x2}"),
        };
        r.End();
        if (frame is ModsReport mods) mods.Validate();
        if (frame is CustomTrust trust) trust.Validate();
        if(frame is RelayContext context)RelayContext.Validate(context.Payload);
        if (frame is RelayLeg leg) RelayLeg.Validate(leg.Payload);
        if (frame is RelayFallback fallback) RelayFallback.Validate(fallback.Grant, fallback.State);
        if (frame is FreeRoamReady ready) FreeRoamReady.Validate(ready.Result);
        if (frame is FreeRoamNeed need) FreeRoamNeed.Validate(need.Kind);
        if (frame is Car car) Car.Validate(car.CarKey);
        if (frame is FollowHost follow) FollowHost.Validate(follow.LobbyType);
        if(frame is RelayClock relayClock && relayClock.UnixSeconds is 0 or >long.MaxValue)throw new ProtocolException("Invalid relay clock");
        if(frame is RelayNetworkHint hint)Matchmaking.RelayNetworkFrames.Hint(hint.Payload);
        if(frame is RelayCandidates candidates)Matchmaking.RelayNetworkFrames.ValidateCandidates(candidates.Payload);
        if (frame is OnlineSuspend suspend && !Enum.IsDefined(suspend.Reason))
            throw new ProtocolException("unknown online suspension reason");
        used = 4 + length.Value;
        return frame;
    }

    private static CustomTrust DecodeCustomTrust(ref WireReader r)
    {
        if (r.U8() != CustomTrust.Version) throw new ProtocolException("Unknown custom trust version");
        var count = r.U8();
        if (count is 0 or > CustomTrust.MaxKeys) throw new ProtocolException("Invalid custom trust");
        var keys = new List<(string Kid, byte[] Key)>(count);
        for (var i = 0; i < count; i++) keys.Add((r.Str(), r.Take(32).ToArray()));
        return new CustomTrust(keys);
    }

    private static ModsReport DecodeModsReport(ref WireReader r)
    {
        if (r.U8() != ModsReport.Version) throw new ProtocolException("Unknown mods report version");
        var nonce = r.Take(16).ToArray();
        var plan = r.Take(32).ToArray();
        var phase = r.U8();
        var count = r.U8();
        if (count > ModsReport.MaxMods) throw new ProtocolException("Too many mods in a report");
        var mods = new List<ModsReportEntry>(count);
        for (var i = 0; i < count; ++i)
            mods.Add(new ModsReportEntry(r.Str(), r.Str(), r.Take(32).ToArray(), r.Take(32).ToArray(), r.Take(32).ToArray(), r.U8()));
        return new ModsReport(nonce, plan, phase, mods);
    }
}
