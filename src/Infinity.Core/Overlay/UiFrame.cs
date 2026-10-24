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
/// Same envelope as the control pipe, its own type space 0x01-0x18 (0x07-0x10: worlds, OD-54, OD-63;
/// 0x14-0x18: modded lobbies and the detail pane, OD-112, protocol 2; native/overlay/src/ui_frames.h
/// and pipe.cpp are the other end). The hook never sees it.
/// </summary>
public abstract record UiFrame
{
    /// <summary>What this launcher speaks; <see cref="UiSession"/> also accepts <see cref="MinProtocolVersion"/>.</summary>
    public const ushort ProtocolVersion = 2;

    /// <summary>Protocol 1 overlays know only 0x01-0x13: never send them a protocol 2 frame.</summary>
    public const ushort MinProtocolVersion = 1;

    public const int MaxLobbyRows = 16, MaxLobbyMembers = 12, MaxLobbyMods = 32;
    public const int MaxPaneSections = 8, MaxPaneEntries = 32, MaxPaneActions = 4, MaxPaneText = 512, MaxPaneId = 64;

    /// <summary>The lowest type that needs protocol 2.</summary>
    public const byte FirstProtocol2Type = 0x14;

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

    /// <summary>0x14, overlay → launcher (protocol 2): <see cref="WorldsQuery"/> plus the modded filter (false: vanilla worlds only; true: modded only).</summary>
    public sealed record WorldsQueryV2(byte Filter, bool NotFull, bool Modded) : UiFrame
    {
        public override byte Type => 0x14;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>
    /// 0x15, launcher → overlay (protocol 2): the answer to <see cref="WorldsQueryV2"/>, echoing its filter
    /// and modded toggle; each row embeds its players and mods so the overlay's hover pane needs no request.
    /// </summary>
    public sealed record WorldsListV2(byte Filter, bool Modded, byte Result, string Message, IReadOnlyList<LobbyRow> Rows) : UiFrame
    {
        public override byte Type => 0x15;

        public override UiDirection Direction => UiDirection.ToOverlay;

        /// <summary>A protocol 1 answer as a protocol 2 one: every row vanilla and matching, no details; null (the modded filter) is an empty list.</summary>
        public static WorldsListV2 FromVanilla(WorldsQueryV2 query, WorldsList? list)
        {
            ArgumentNullException.ThrowIfNull(query);
            if (list is null) return new(query.Filter, query.Modded, 0, "", []);
            return new(list.Filter, query.Modded, list.Result, list.Message,
                list.Rows.Take(MaxLobbyRows).Select(row => new LobbyRow(row, false, true, [], [])).ToList());
        }
    }

    /// <summary>
    /// 0x16, launcher → overlay (protocol 2): the generic right pane (player details today; any future
    /// kind uses the same frame). A non-empty <c>PaneId</c> opens or replaces the pane; an empty one closes it.
    /// <c>AvatarKey</c> empty: initials of the title.
    /// </summary>
    public sealed record DetailPane(string PaneId, string Title, string Subtitle, string AvatarKey, IReadOnlyList<PaneSection> Sections,
        IReadOnlyList<PaneAction> Actions) : UiFrame
    {
        public override byte Type => 0x16;

        public override UiDirection Direction => UiDirection.ToOverlay;

        internal void Validate()
        {
            if (PaneId.Length != 0) PaneKey(PaneId, "pane id");
            Limit(Title, MaxPaneText, "pane title");
            Limit(Subtitle, MaxPaneText, "pane subtitle");
            Limit(AvatarKey, MaxPaneId, "avatar key");
            Bound(Sections.Count, MaxPaneSections, "pane sections");
            Bound(Actions.Count, MaxPaneActions, "pane actions");
            foreach (var section in Sections)
            {
                if (section.Kind > UiPaneSection.Items) throw new ProtocolException("invalid pane section kind");
                Limit(section.Heading, MaxPaneText, "section heading");
                Bound(section.Entries.Count, MaxPaneEntries, "pane entries");
                foreach (var entry in section.Entries)
                {
                    Limit(entry.Primary, MaxPaneText, "pane entry");
                    Limit(entry.Secondary, MaxPaneText, "pane entry");
                    if (entry.Icon > UiPaneIcon.Max) throw new ProtocolException("invalid pane icon");
                }
            }
            foreach (var action in Actions)
            {
                PaneKey(action.Id, "action id");
                Limit(action.Label, MaxPaneText, "action label");
            }
        }
    }

    /// <summary>0x17, overlay → launcher (protocol 2): the game's player card asked for this player (decimal XUID; gamertag when known).</summary>
    public sealed record PlayerDetailsQuery(string Xuid, string Gamertag) : UiFrame
    {
        public override byte Type => 0x17;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>0x18, overlay → launcher (protocol 2): the player chose an enabled action of a pane (disabled ones are never sent).</summary>
    public sealed record PaneActionClick(string PaneId, string ActionId) : UiFrame
    {
        public override byte Type => 0x18;

        public override UiDirection Direction => UiDirection.ToLauncher;
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
            case WorldsQueryV2 q:
                w.U8(q.Filter);
                w.Flag(q.NotFull);
                w.Flag(q.Modded);
                break;
            case WorldsListV2 l:
                Bound(l.Rows.Count, MaxLobbyRows, "lobby rows");
                w.U8(l.Filter);
                w.Flag(l.Modded);
                w.U8(l.Result);
                w.Str(l.Message);
                w.U8((byte)l.Rows.Count);
                foreach (var row in l.Rows)
                {
                    row.Validate();
                    var world = row.World;
                    w.Str(world.Id);
                    w.Str(world.Name);
                    w.Str(world.Host);
                    w.U8(world.Players);
                    w.U8(world.Max);
                    w.U8(world.Kind);
                    w.U8(world.Audience);
                    w.U8(world.Lock);
                    w.U8(world.Ping);
                    w.Flag(world.FriendHost);
                    w.Flag(row.Modded);
                    w.Flag(row.ModsMatch);
                    w.U8((byte)row.Members.Count);
                    foreach (var member in row.Members)
                    {
                        w.Str(member.Gamertag);
                        w.U8(member.Tier);
                        w.U8(member.Role);
                    }
                    w.U8((byte)row.Mods.Count);
                    foreach (var mod in row.Mods)
                    {
                        w.Str(mod.Id);
                        w.Str(mod.Version);
                    }
                }
                break;
            case DetailPane p:
                p.Validate();
                w.Str(p.PaneId);
                w.Str(p.Title);
                w.Str(p.Subtitle);
                w.Str(p.AvatarKey);
                w.U8((byte)p.Sections.Count);
                foreach (var section in p.Sections)
                {
                    w.U8(section.Kind);
                    w.Str(section.Heading);
                    w.U8((byte)section.Entries.Count);
                    foreach (var entry in section.Entries)
                    {
                        w.Str(entry.Primary);
                        w.Str(entry.Secondary);
                        w.U8(entry.Icon);
                    }
                }
                w.U8((byte)p.Actions.Count);
                foreach (var action in p.Actions)
                {
                    w.Str(action.Id);
                    w.Str(action.Label);
                    w.Flag(action.Enabled);
                }
                break;
            case PlayerDetailsQuery q:
                if (q.Xuid.Length is 0 or > 20 || !q.Xuid.All(char.IsAsciiDigit)) throw new ProtocolException("Invalid player details xuid");
                Limit(q.Gamertag, MaxPaneText, "gamertag");
                w.Str(q.Xuid);
                w.Str(q.Gamertag);
                break;
            case PaneActionClick c:
                PaneKey(c.PaneId, "pane id");
                PaneKey(c.ActionId, "action id");
                w.Str(c.PaneId);
                w.Str(c.ActionId);
                break;
        }
        return w.Finish("ui frame");
    }

    internal static void Bound(int count, int max, string what)
    {
        if (count > max) throw new ProtocolException($"too many {what} ({count} > {max})");
    }

    internal static void Limit(string text, int maxBytes, string what)
    {
        if (System.Text.Encoding.UTF8.GetByteCount(text) > maxBytes) throw new ProtocolException($"{what} is longer than {maxBytes} bytes");
    }

    /// <summary>A pane or action id: 1-64 ASCII letters, digits and <c>:._-</c> (a player pane is <c>player:&lt;xuid&gt;</c>).</summary>
    internal static void PaneKey(string id, string what)
    {
        if (id.Length is 0 or > MaxPaneId || !id.All(c => char.IsAsciiLetterOrDigit(c) || c is ':' or '.' or '_' or '-'))
            throw new ProtocolException($"invalid {what}");
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
        if (buffer.Length >= 5 && buffer[4] is < 0x01 or > 0x18)
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
            0x13 => new ConnectionDetails(r.U8(), r.Str()),
            0x14 => new WorldsQueryV2(r.U8(), r.Flag("WorldsQueryV2.notFull"), r.Flag("WorldsQueryV2.modded")),
            0x15 => DecodeListV2(ref r),
            0x16 => DecodePane(ref r),
            0x17 => new PlayerDetailsQuery(r.Str(), r.Str()),
            _ => new PaneActionClick(r.Str(), r.Str()),
        };
        r.End();
        switch (frame)
        {
            case PlayerDetailsQuery or PaneActionClick:
                _ = frame.Encode(); // the same rules as the sender's
                break;
        }
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

    private static WorldsListV2 DecodeListV2(ref WireReader r)
    {
        var filter = r.U8();
        var modded = r.Flag("WorldsListV2.modded");
        var status = r.U8();
        var message = r.Str();
        var count = r.U8();
        Bound(count, MaxLobbyRows, "lobby rows");
        var rows = new List<LobbyRow>(count);
        for (var i = 0; i < count; ++i)
        {
            var world = new WorldRow(r.Str(), r.Str(), r.Str(), r.U8(), r.U8(), r.U8(), r.U8(), r.U8(), r.U8(), r.Flag("LobbyRow.friendHost"));
            var rowModded = r.Flag("LobbyRow.modded");
            var match = r.Flag("LobbyRow.modsMatch");
            var memberCount = r.U8();
            Bound(memberCount, MaxLobbyMembers, "lobby members");
            var members = new List<LobbyMember>(memberCount);
            for (var m = 0; m < memberCount; ++m) members.Add(new LobbyMember(r.Str(), r.U8(), r.U8()));
            var modCount = r.U8();
            Bound(modCount, MaxLobbyMods, "lobby mods");
            var mods = new List<LobbyMod>(modCount);
            for (var m = 0; m < modCount; ++m) mods.Add(new LobbyMod(r.Str(), r.Str()));
            var row = new LobbyRow(world, rowModded, match, members, mods);
            row.Validate();
            rows.Add(row);
        }
        return new WorldsListV2(filter, modded, status, message, rows);
    }

    private static DetailPane DecodePane(ref WireReader r)
    {
        var id = r.Str();
        var title = r.Str();
        var subtitle = r.Str();
        var avatar = r.Str();
        var sectionCount = r.U8();
        Bound(sectionCount, MaxPaneSections, "pane sections");
        var sections = new List<PaneSection>(sectionCount);
        for (var i = 0; i < sectionCount; ++i)
        {
            var kind = r.U8();
            var heading = r.Str();
            var entryCount = r.U8();
            Bound(entryCount, MaxPaneEntries, "pane entries");
            var entries = new List<PaneEntry>(entryCount);
            for (var e = 0; e < entryCount; ++e) entries.Add(new PaneEntry(r.Str(), r.Str(), r.U8()));
            sections.Add(new PaneSection(kind, heading, entries));
        }
        var actionCount = r.U8();
        Bound(actionCount, MaxPaneActions, "pane actions");
        var actions = new List<PaneAction>(actionCount);
        for (var i = 0; i < actionCount; ++i) actions.Add(new PaneAction(r.Str(), r.Str(), r.Flag("PaneAction.enabled")));
        var pane = new DetailPane(id, title, subtitle, avatar, sections, actions);
        pane.Validate();
        return pane;
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

    /// <summary>Protocol 2 list request (modded filter, OD-112): answer with <see cref="UiFrame.WorldsListV2"/>.</summary>
    public sealed record WorldsV2(UiFrame.WorldsQueryV2 Query) : UiIncoming;

    /// <summary>The game's player card: answer with a <see cref="UiFrame.DetailPane"/> <c>player:&lt;xuid&gt;</c>.</summary>
    public sealed record PlayerDetails(UiFrame.PlayerDetailsQuery Request) : UiIncoming;

    public sealed record PaneClick(UiFrame.PaneActionClick Request) : UiIncoming;
}

/// <summary>
/// One row of <see cref="UiFrame.WorldsListV2"/> (OD-112): the protocol 1 <see cref="WorldRow"/>, whether
/// the world is modded, whether its mods equal the local set (a false <c>ModsMatch</c> blocks the join in
/// the overlay), its players (host first unless dedicated, at most 12) and its mods (sorted by id, at most
/// 32; empty for a vanilla world). Members carry no XUID (OD-61).
/// </summary>
public sealed record LobbyRow(WorldRow World, bool Modded, bool ModsMatch, IReadOnlyList<LobbyMember> Members, IReadOnlyList<LobbyMod> Mods)
{
    internal void Validate()
    {
        UiFrame.Bound(Members.Count, UiFrame.MaxLobbyMembers, "lobby members");
        UiFrame.Bound(Mods.Count, UiFrame.MaxLobbyMods, "lobby mods");
        if (!Modded && Mods.Count != 0) throw new ProtocolException("a vanilla lobby lists no mods");
        foreach (var member in Members)
        {
            UiFrame.Limit(member.Gamertag, 64, "member gamertag");
            if (member.Tier > UiTier.Supporter || member.Role > 2) throw new ProtocolException("invalid lobby member");
        }
        foreach (var mod in Mods)
        {
            UiFrame.Limit(mod.Id, 64, "mod id");
            UiFrame.Limit(mod.Version, 16, "mod version");
        }
    }
}

/// <summary><c>Tier</c> <see cref="UiTier"/>; <c>Role</c> 0 host, 1 in the world, 2 seat reserved.</summary>
public sealed record LobbyMember(string Gamertag, byte Tier, byte Role);

/// <summary>A mod of a lobby: the manifest's multiplayer <c>compatibilityId</c> and its revision (display only; MODDED_LOBBIES.md §2).</summary>
public sealed record LobbyMod(string Id, string Version);

/// <summary>One ordered section of a <see cref="UiFrame.DetailPane"/>: <see cref="UiPaneSection"/>, a heading (may be empty) and up to 32 entries.</summary>
public sealed record PaneSection(byte Kind, string Heading, IReadOnlyList<PaneEntry> Entries);

/// <summary>
/// A key/value row (<see cref="UiPaneSection.Rows"/>: key, value) or a list item (<see cref="UiPaneSection.Items"/>:
/// text, detail); <c>Icon</c> <see cref="UiPaneIcon"/>.
/// </summary>
public sealed record PaneEntry(string Primary, string Secondary, byte Icon = UiPaneIcon.None);

/// <summary>A pane button; a disabled one is drawn but never sent back.</summary>
public sealed record PaneAction(string Id, string Label, bool Enabled);

/// <summary><c>PaneSection.kind</c>.</summary>
public static class UiPaneSection
{
    public const byte Rows = 0;
    public const byte Items = 1;
}

/// <summary><c>PaneEntry.icon</c>: the overlay's own glyphs (unknown values are refused).</summary>
public static class UiPaneIcon
{
    public const byte None = 0;
    public const byte Player = 1;
    public const byte Host = 2;
    public const byte Supporter = 3;
    public const byte Mod = 4;
    public const byte Warning = 5;
    public const byte Check = 6;
    public const byte Max = Check;
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

    /// <summary>The overlay's protocol (from its hello); 0 before. Send 0x14-0x18 only when it is 2 or more.</summary>
    public ushort Protocol { get; private set; }

    /// <summary>Whether this overlay understands <paramref name="frame"/> (false before the hello).</summary>
    public bool Accepts(UiFrame frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        return Ready && (frame.Type < UiFrame.FirstProtocol2Type || Protocol >= 2);
    }

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
            if (hello.Protocol is < UiFrame.MinProtocolVersion or > UiFrame.ProtocolVersion)
            {
                throw new ProtocolException($"ui protocol {hello.Protocol}");
            }
            if (_expectedPid is { } expected && expected != hello.Pid)
            {
                throw new ProtocolException($"UiHello pid {hello.Pid} is not the child {expected}");
            }
            Ready = true;
            Protocol = hello.Protocol;
            return new UiIncoming.Hello(hello.OverlayVersion, [_status]);
        }
        if (frame.Type >= UiFrame.FirstProtocol2Type && Protocol < 2)
        {
            throw new ProtocolException($"protocol {Protocol} overlay sent type 0x{frame.Type:x2}");
        }
        return frame switch
        {
            UiFrame.UiHello => throw new ProtocolException("second UiHello"),
            UiFrame.WorldsQueryV2 q => new UiIncoming.WorldsV2(q),
            UiFrame.PlayerDetailsQuery p => new UiIncoming.PlayerDetails(p),
            UiFrame.PaneActionClick c => new UiIncoming.PaneClick(c),
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
