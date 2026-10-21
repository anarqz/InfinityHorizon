// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Session;
using Infinity.Core.Wire;

namespace Infinity.Core.Overlay;

/// <summary><c>UiEvent</c> kinds (ORCHESTRATOR §4.3, OVERLAY_V1.md).</summary>
public static class UiEventKind
{
    /// <summary>The overlay drew its first frame (detail <c>WxH</c>).</summary>
    public const byte FirstFrame = 1;

    /// <summary>The swapchain size changed (detail <c>WxH</c>).</summary>
    public const byte Resize = 2;

    public const byte PanelOpenedByHotkey = 3;

    public const byte PanelClosed = 4;

    /// <summary>The overlay stopped drawing (panic, device removed); the game keeps presenting.</summary>
    public const byte Passthrough = 5;

    /// <summary>The hook's captured swapchain is not the one the overlay hooked.</summary>
    public const byte CaptureMismatch = 6;

    /// <summary>The player confirmed "Quit game" in the panel: the supervisor closes the game.</summary>
    public const byte Quit = 7;
    public const byte ConnectionDetails = 8;
}

/// <summary><c>Status.tier</c>.</summary>
public static class UiTier
{
    public const byte Free = 0;

    /// <summary>Infinity Supporter (internal tier id <c>vip</c>; never shown as "VIP").</summary>
    public const byte Supporter = 1;
}

/// <summary><c>Status.state</c>.</summary>
public static class UiState
{
    public const byte Queued = 0;
    public const byte Running = 1;
    public const byte Offline = 2;
    public const byte Exiting = 3;
}

/// <summary><c>Toast.level</c>.</summary>
public static class UiLevel
{
    public const byte Info = 0;
    public const byte Warn = 1;
    public const byte Error = 2;
}

public enum UiDirection
{
    ToLauncher,
    ToOverlay,
    Both,
}

/// <summary>
/// One UI-pipe frame between the launcher and the overlay (<c>&lt;control pipe&gt;-ui</c>; overlay_ui.rs).
/// Same envelope as the control pipe, its own type space 0x01-0x13 (0x07-0x10: worlds, OD-54, OD-63;
/// native/overlay/src/pipe.cpp is the other end). The hook never sees it.
/// </summary>
public abstract record UiFrame
{
    public const ushort ProtocolVersion = 1;

    private UiFrame()
    {
    }

    public abstract byte Type { get; }

    public abstract UiDirection Direction { get; }

    public sealed record ConnectionDetails(byte State, string Text) : UiFrame
    {
        public override byte Type => 0x13;
        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x11, launcher → overlay; expires after 1500 ms without an update.</summary>
    public sealed record GameReadiness(bool Ready) : UiFrame
    {
        public override byte Type => 0x11;
        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x12, launcher → overlay; an allowed installation decision expires after ten seconds.</summary>
    public sealed record WorldAccess(bool Allowed, string Message) : UiFrame
    {
        public override byte Type => 0x12;
        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x01, overlay → launcher, first frame.</summary>
    public sealed record UiHello(ushort Protocol, uint Pid, string OverlayVersion) : UiFrame
    {
        public override byte Type => 0x01;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>0x02, launcher → overlay: account (<c>Tag#12345</c>), <see cref="UiTier"/>, <see cref="UiState"/>, detail.</summary>
    public sealed record Status(string Account, byte Tier, byte State, string Detail) : UiFrame
    {
        public override byte Type => 0x02;

        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x03, launcher → overlay: <see cref="UiLevel"/>, icon, lifetime in tenths of a second, localised text.</summary>
    public sealed record Toast(byte Level, byte Icon, ushort TtlDs, string Text) : UiFrame
    {
        public override byte Type => 0x03;

        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x04, both: the launcher asks; the overlay reports the result.</summary>
    public sealed record Panel(bool Open) : UiFrame
    {
        public override byte Type => 0x04;

        public override UiDirection Direction => UiDirection.Both;
    }

    /// <summary>0x05, overlay → launcher: a <see cref="UiEventKind"/> and free text (never a secret).</summary>
    public sealed record UiEvent(byte Kind, string Detail) : UiFrame
    {
        public override byte Type => 0x05;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>0x06, either side, last frame.</summary>
    public sealed record Bye(byte Reason) : UiFrame
    {
        public override byte Type => 0x06;

        public override UiDirection Direction => UiDirection.Both;
    }

    /// <summary>0x07, overlay → launcher: the list for a filter (1 friends, 2 near, 3 global; OD-58), full ones left out or not.</summary>
    public sealed record WorldsQuery(byte Filter, bool NotFull) : UiFrame
    {
        public override byte Type => 0x07;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>0x08, launcher → overlay: the list (status 0 ok, 2 error with a localised message), at most 16 rows.</summary>
    public sealed record WorldsList(byte Filter, byte Result, string Message, IReadOnlyList<WorldRow> Rows) : UiFrame
    {
        public override byte Type => 0x08;

        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x09, overlay → launcher: join a world by id, or by its 8-digit code (one of them empty).</summary>
    public sealed record JoinWorld(string WorldId, string Code) : UiFrame
    {
        public override byte Type => 0x09;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>0x0A, launcher → overlay: <see cref="UiJoin"/> and a localised message.</summary>
    public sealed record JoinResult(byte Result, string Message) : UiFrame
    {
        public override byte Type => 0x0A;

        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x0B, overlay → launcher: open a world (audience 0 everyone, 1 supporters, 2 friends, 3 private; latency 0 any, 1 &lt; 150, 2 &lt; 100, 3 &lt; 60 ms).</summary>
    public sealed record CreateWorld(string Name, byte Audience, byte MaxPlayers, byte Latency) : UiFrame
    {
        public override byte Type => 0x0B;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>0x0C, launcher → overlay: the world opened (status 0) with its code, or why not.</summary>
    public sealed record Hosted(byte Result, string WorldId, string Code, string Message) : UiFrame
    {
        public override byte Type => 0x0C;

        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x0D, overlay → launcher: "my world" (the Worlds tab is shown; every 5 s while it is).</summary>
    public sealed record MyWorldQuery : UiFrame
    {
        public override byte Type => 0x0D;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>
    /// 0x0E, launcher → overlay: the world this profile hosts or holds a seat in (<see cref="UiWorldRole"/>;
    /// <c>None</c>: no world, the other fields empty), its code (host only) and its players. <c>Named</c>
    /// false: the server gave no roster (only the host is known).
    /// </summary>
    public sealed record MyWorld(byte Role, string WorldId, string Name, string Host, string Code, byte Audience, byte Players, byte Max,
        bool Named, IReadOnlyList<MemberRow> Members) : UiFrame
    {
        public override byte Type => 0x0E;

        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    /// <summary>0x0F, overlay → launcher: a <see cref="UiWorldAction"/> on the world or one of its players (<c>Reason</c>: <see cref="UiReport"/>).</summary>
    public sealed record WorldAction(byte Action, string Xuid, string Gamertag, byte Reason) : UiFrame
    {
        public override byte Type => 0x0F;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>0x10, launcher → overlay: how a <see cref="WorldAction"/> went (0 done, 1 refused) and a localised message.</summary>
    public sealed record ActionResult(byte Action, byte Result, string Message) : UiFrame
    {
        public override byte Type => 0x10;

        public override UiDirection Direction => UiDirection.ToOverlay;
    }

    public static string PipeName(Handoff handoff)
    {
        ArgumentNullException.ThrowIfNull(handoff);
        return handoff.UiPipeName;
    }

    public byte[] Encode()
    {
        var w = new WireWriter(Type);
        switch (this)
        {
            case ConnectionDetails details:
                if (details.State > 2 || System.Text.Encoding.UTF8.GetByteCount(details.Text) > 4096) throw new ProtocolException("Invalid connection details");
                w.U8(details.State); w.Str(details.Text);
                break;
            case GameReadiness ready:
                w.Flag(ready.Ready);
                break;
            case WorldAccess access:
                w.Flag(access.Allowed);
                w.Str(access.Message);
                break;
            case UiHello h:
                w.U16(h.Protocol);
                w.U32(h.Pid);
                w.Str(h.OverlayVersion);
                break;
            case Status s:
                w.Str(s.Account);
                w.U8(s.Tier);
                w.U8(s.State);
                w.Str(s.Detail);
                break;
            case Toast t:
                w.U8(t.Level);
                w.U8(t.Icon);
                w.U16(t.TtlDs);
                w.Str(t.Text);
                break;
            case Panel p:
                w.Flag(p.Open);
                break;
            case UiEvent e:
                if (e.Kind == UiEventKind.ConnectionDetails && e.Detail.Length != 0) throw new ProtocolException("Connection query detail must be empty");
                w.U8(e.Kind);
                w.Str(e.Detail);
                break;
            case Bye b:
                w.U8(b.Reason);
                break;
            case WorldsQuery q:
                w.U8(q.Filter);
                w.Flag(q.NotFull);
                break;
            case WorldsList l:
                w.U8(l.Filter);
                w.U8(l.Result);
                w.Str(l.Message);
                var rows = l.Rows.Take(16).ToList();
                w.U8((byte)rows.Count);
                foreach (var row in rows)
                {
                    w.Str(row.Id);
                    w.Str(row.Name);
                    w.Str(row.Host);
                    w.U8(row.Players);
                    w.U8(row.Max);
                    w.U8(row.Kind);
                    w.U8(row.Audience);
                    w.U8(row.Lock);
                    w.U8(row.Ping);
                    w.U8(row.FriendHost ? (byte)1 : (byte)0);
                }
                break;
            case JoinWorld j:
                w.Str(j.WorldId);
                w.Str(j.Code);
                break;
            case JoinResult r:
                w.U8(r.Result);
                w.Str(r.Message);
                break;
            case CreateWorld c:
                w.Str(c.Name);
                w.U8(c.Audience);
                w.U8(c.MaxPlayers);
                w.U8(c.Latency);
                break;
            case Hosted h:
                w.U8(h.Result);
                w.Str(h.WorldId);
                w.Str(h.Code);
                w.Str(h.Message);
                break;
            case MyWorldQuery:
                break;
            case MyWorld m:
                w.U8(m.Role);
                w.Str(m.WorldId);
                w.Str(m.Name);
                w.Str(m.Host);
                w.Str(m.Code);
                w.U8(m.Audience);
                w.U8(m.Players);
                w.U8(m.Max);
                w.Flag(m.Named);
                var members = m.Members.Take(16).ToList();
                w.U8((byte)members.Count);
                foreach (var member in members)
                {
                    w.Str(member.Xuid);
                    w.Str(member.Gamertag);
                    w.U8(member.Tier);
                    w.U8(member.Role);
                    w.U8(member.Friend);
                }
                break;
            case WorldAction a:
                w.U8(a.Action);
                w.Str(a.Xuid);
                w.Str(a.Gamertag);
                w.U8(a.Reason);
                break;
            case ActionResult r:
                w.U8(r.Action);
                w.U8(r.Result);
                w.Str(r.Message);
                break;
        }
        return w.Finish("ui frame");
    }

    /// <summary>
    /// Decodes one frame: null when more bytes are needed. An unknown type is refused as soon as
    /// its byte has arrived.
    /// </summary>
    public static UiFrame? Decode(ReadOnlySpan<byte> buffer, out int used)
    {
        used = 0;
        var length = Frames.BodyLength(buffer, "ui frame");
        if (length is null)
        {
            return null;
        }
        if (buffer.Length >= 5 && buffer[4] is < 0x01 or > 0x13)
        {
            throw new ProtocolException($"unknown ui frame type 0x{buffer[4]:x2}");
        }
        if (buffer.Length < 4 + length.Value)
        {
            return null;
        }
        var r = new WireReader(buffer[5..(4 + length.Value)], "ui frame");
        UiFrame frame = buffer[4] switch
        {
            0x01 => new UiHello(r.U16(), r.U32(), r.Str()),
            0x02 => new Status(r.Str(), r.U8(), r.U8(), r.Str()),
            0x03 => new Toast(r.U8(), r.U8(), r.U16(), r.Str()),
            0x04 => new Panel(r.Flag("Panel.open")),
            0x05 => new UiEvent(r.U8(), r.Str()),
            0x06 => new Bye(r.U8()),
            0x07 => new WorldsQuery(r.U8(), r.Flag("WorldsQuery.notFull")),
            0x08 => DecodeList(ref r),
            0x09 => new JoinWorld(r.Str(), r.Str()),
            0x0A => new JoinResult(r.U8(), r.Str()),
            0x0B => new CreateWorld(r.Str(), r.U8(), r.U8(), r.U8()),
            0x0C => new Hosted(r.U8(), r.Str(), r.Str(), r.Str()),
            0x0D => new MyWorldQuery(),
            0x0E => DecodeMine(ref r),
            0x0F => new WorldAction(r.U8(), r.Str(), r.Str(), r.U8()),
            0x10 => new ActionResult(r.U8(), r.U8(), r.Str()),
            0x11 => new GameReadiness(r.Flag("GameReadiness.ready")),
            0x12 => new WorldAccess(r.Flag("WorldAccess.allowed"), r.Str()),
            _ => new ConnectionDetails(r.U8(), r.Str()),
        };
        r.End();
        if (frame is ConnectionDetails details && (details.State > 2 || System.Text.Encoding.UTF8.GetByteCount(details.Text) > 4096)) throw new ProtocolException("Invalid connection details");
        if (frame is UiEvent { Kind: UiEventKind.ConnectionDetails } query && query.Detail.Length != 0) throw new ProtocolException("Connection query detail must be empty");
        used = 4 + length.Value;
        return frame;
    }

    private static MyWorld DecodeMine(ref WireReader r)
    {
        var role = r.U8();
        var id = r.Str();
        var name = r.Str();
        var host = r.Str();
        var code = r.Str();
        var audience = r.U8();
        var players = r.U8();
        var max = r.U8();
        var named = r.Flag("MyWorld.named");
        var count = r.U8();
        var members = new List<MemberRow>(count);
        for (var i = 0; i < count; ++i)
        {
            members.Add(new MemberRow(r.Str(), r.Str(), r.U8(), r.U8(), r.U8()));
        }
        return new MyWorld(role, id, name, host, code, audience, players, max, named, members);
    }

    private static WorldsList DecodeList(ref WireReader r)
    {
        var filter = r.U8();
        var status = r.U8();
        var message = r.Str();
        var count = r.U8();
        var rows = new List<WorldRow>(count);
        for (var i = 0; i < count; ++i)
        {
            rows.Add(new WorldRow(r.Str(), r.Str(), r.Str(), r.U8(), r.U8(), r.U8(), r.U8(), r.U8(), r.U8(), r.U8() != 0));
        }
        return new WorldsList(filter, status, message, rows);
    }
}

/// <summary>What the launcher learns from one overlay frame.</summary>
public abstract record UiIncoming
{
    private UiIncoming()
    {
    }

    /// <summary>The overlay said hello: send <paramref name="Reply"/> (the current status).</summary>
    public sealed record Hello(string OverlayVersion, IReadOnlyList<UiFrame> Reply) : UiIncoming;

    public sealed record Event(byte Kind, string Detail) : UiIncoming;

    public sealed record Panel(bool Open) : UiIncoming;

    public sealed record Bye(byte Reason) : UiIncoming;

    public sealed record Worlds(UiFrame.WorldsQuery Query) : UiIncoming;

    public sealed record Join(UiFrame.JoinWorld Request) : UiIncoming;

    public sealed record Create(UiFrame.CreateWorld Request) : UiIncoming;

    public sealed record Mine : UiIncoming;

    public sealed record Act(UiFrame.WorldAction Request) : UiIncoming;
}

/// <summary>
/// One row of the overlay's world list: <c>Kind</c> 0 everyone's / 1 Infinity Supporter host,
/// <c>Audience</c> 0-3, <c>Lock</c> 0 joinable / 1 tier / 2 friends / 3 code / 4 full / 5 latency /
/// 6 other, <c>Ping</c> 0 unknown / 1 good / 2 fair / 3 poor.
/// </summary>
public sealed record WorldRow(string Id, string Name, string Host, byte Players, byte Max, byte Kind, byte Audience, byte Lock, byte Ping, bool FriendHost);

/// <summary>
/// One player of <see cref="UiFrame.MyWorld"/>: <c>Tier</c> <see cref="UiTier"/>, <c>Role</c> 0 host /
/// 1 seated / 2 reserved, <c>Friend</c> <see cref="UiFriend"/>. The XUID is the action key, never shown (OD-61).
/// </summary>
public sealed record MemberRow(string Xuid, string Gamertag, byte Tier, byte Role, byte Friend);

/// <summary><c>MyWorld.role</c>.</summary>
public static class UiWorldRole
{
    public const byte None = 0;
    public const byte Hosting = 1;
    public const byte Reserved = 2;
    public const byte Seated = 3;
}

/// <summary><c>MemberRow.friend</c> (to the caller's account, OD-55).</summary>
public static class UiFriend
{
    public const byte None = 0;
    public const byte Friends = 1;
    public const byte Outgoing = 2;
    public const byte Incoming = 3;
    public const byte Self = 4;
}

/// <summary><c>WorldAction.action</c>.</summary>
public static class UiWorldAction
{
    /// <summary>The host closes its world.</summary>
    public const byte Close = 1;

    /// <summary>A seat holder leaves its seat.</summary>
    public const byte Leave = 2;

    /// <summary>The host kicks a player.</summary>
    public const byte Kick = 3;

    /// <summary>A friend request (or the acceptance of theirs) by gamertag.</summary>
    public const byte AddFriend = 4;

    public const byte Report = 5;
}

/// <summary><c>WorldAction.reason</c> for a report.</summary>
public static class UiReport
{
    public const byte Cheating = 1;
    public const byte Abuse = 2;
    public const byte Other = 3;
}

/// <summary><c>JoinResult.status</c> (the overlay's worlds.cpp reads 0 and 3; the rest is a refusal).</summary>
public static class UiJoin
{
    public const byte Reserved = 0;
    public const byte Full = 1;
    public const byte NotAllowed = 2;
    public const byte Code = 3;
    public const byte Closed = 4;
    public const byte SlowDown = 5;
}

/// <summary>
/// The launcher's side of one UI-pipe connection (overlay_ui.rs <c>UiSession</c>): the first frame
/// must be <c>UiHello</c> from the child, frames in the wrong direction are errors (close the pipe),
/// and it keeps the status line the overlay is shown.
/// </summary>
public sealed class UiSession
{
    private readonly uint? _expectedPid;
    private UiFrame.Status _status;

    public UiSession(uint? expectedPid, UiFrame.Status status)
    {
        _expectedPid = expectedPid;
        _status = status ?? throw new ArgumentNullException(nameof(status));
    }

    public bool Ready { get; private set; }

    /// <summary>Replaces the status line; returns the frame to send when the overlay is ready.</summary>
    public UiFrame.Status? SetStatus(UiFrame.Status status)
    {
        _status = status ?? throw new ArgumentNullException(nameof(status));
        return Ready ? status : null;
    }

    /// <summary>One frame from the overlay. A <see cref="ProtocolException"/> means: close the pipe.</summary>
    public UiIncoming OnFrame(UiFrame frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        if (frame.Direction == UiDirection.ToOverlay)
        {
            throw new ProtocolException($"overlay sent a launcher frame (type 0x{frame.Type:x2})");
        }
        if (!Ready)
        {
            if (frame is not UiFrame.UiHello hello)
            {
                throw new ProtocolException($"first ui frame is type 0x{frame.Type:x2}, not UiHello");
            }
            if (hello.Protocol != UiFrame.ProtocolVersion)
            {
                throw new ProtocolException($"ui protocol {hello.Protocol}");
            }
            if (_expectedPid is { } expected && expected != hello.Pid)
            {
                throw new ProtocolException($"UiHello pid {hello.Pid} is not the child {expected}");
            }
            Ready = true;
            return new UiIncoming.Hello(hello.OverlayVersion, [_status]);
        }
        return frame switch
        {
            UiFrame.UiHello => throw new ProtocolException("second UiHello"),
            UiFrame.UiEvent e => new UiIncoming.Event(e.Kind, e.Detail),
            UiFrame.Panel p => new UiIncoming.Panel(p.Open),
            UiFrame.Bye b => new UiIncoming.Bye(b.Reason),
            UiFrame.WorldsQuery q => new UiIncoming.Worlds(q),
            UiFrame.JoinWorld j => new UiIncoming.Join(j),
            UiFrame.CreateWorld c => new UiIncoming.Create(c),
            UiFrame.MyWorldQuery => new UiIncoming.Mine(),
            UiFrame.WorldAction a => new UiIncoming.Act(a),
            _ => throw new ProtocolException("direction checked above"),
        };
    }
}
