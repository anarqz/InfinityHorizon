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

/// <summary><c>Status.tier</c>: only a badge (CR-S69, OD-129: nothing depends on it).</summary>
public static class UiTier
{
    /// <summary>Infinity Racer (internal tier id <c>free</c>): every account.</summary>
    public const byte Free = 0;

    /// <summary>Infinity Supporter (internal tier id <c>vip</c>; never shown as "VIP"): the Supporter badge.</summary>
    public const byte Supporter = 1;

    /// <summary>The badge of a wire tier: <c>vip</c> is a Supporter, anything else (<c>free</c>, absent, unknown) a Racer.</summary>
    public static byte Of(string? tier) => tier == "vip" ? Supporter : Free;
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
/// Same envelope as the control pipe, its own type space 0x01-0x1A (0x0D-0x10: the lobby the player is in, OD-63;
/// 0x16-0x18: the detail pane, OD-112, protocol 2; 0x19-0x1A: the guide's server
/// status and avatar, CR-L40; 0x1B-0x1D: voice chat, protocol 3; 0x1E: each lobby player's mods, OD-181, protocol 4;
/// 0x1F-0x22: the game's PLAYER ACTIONS friend and report items, OD-193 F-2, protocol 5; 0x23-0x24: the private lobby's invite
/// prompt and its answer, OD-204 T-L3, protocol 6; 0x25-0x27: the private lobby's friend picker and its invite, OD-204 T-L4,
/// protocol 7; 0x28-0x29: the private lobby's "Who can join", OD-210 CR-S101 W14, protocol 8; native/overlay/src/ui_frames.h and
/// pipe.cpp are the other end). The hook
/// never sees it. Protocol 9 (OD-208): players host no world, so the lobby list, join by code, create and hosted frames
/// (0x07-0x0C, 0x14, 0x15) and WorldAction close (1) and kick (3) are gone from the pipe; either end refuses them as unknown.
/// </summary>
public abstract record UiFrame
{
    /// <summary>What this launcher speaks; <see cref="UiSession"/> also accepts <see cref="MinProtocolVersion"/>.</summary>
    public const ushort ProtocolVersion = 10;

    /// <summary>Protocol 1 overlays know only 0x01-0x13: never send them a protocol 2 frame.</summary>
    public const ushort MinProtocolVersion = 1;

    /// <summary>OD-181: a roster player's declared mods, at most this many.</summary>
    public const int MaxLobbyMods = 32;
    public const int MaxPaneSections = 8, MaxPaneEntries = 32, MaxPaneActions = 4, MaxPaneText = 512, MaxPaneId = 64;

    /// <summary>CR-L40 §1.3: the server's host in <see cref="ServerStatus"/>, in UTF-8 bytes.</summary>
    public const int MaxServerHost = 128;

    /// <summary>
    /// CR-L40 §1.3: an avatar's side. A 128 x 128 image (65 536 bytes) cannot fit the 64 KiB frame with its
    /// header, so the launcher sends <see cref="AvatarSide"/>; 127 is the largest square that fits.
    /// </summary>
    public const int MaxAvatarSide = 128, AvatarSide = 96;

    /// <summary>The lowest type that needs protocol 2.</summary>
    public const byte FirstProtocol2Type = 0x14;

    /// <summary>The lowest type that needs protocol 3 (voice chat, OD-154).</summary>
    public const byte FirstProtocol3Type = 0x1B;

    /// <summary>The lowest type that needs protocol 4 (each lobby player's declared mods, OD-181).</summary>
    public const byte FirstProtocol4Type = 0x1E;

    /// <summary>The lowest type that needs protocol 5 (the game's PLAYER ACTIONS friend and report items, OD-193 F-2).</summary>
    public const byte FirstProtocol5Type = 0x1F;

    /// <summary>The lowest type that needs protocol 6 (the private lobby's invite prompt, OD-204 T-L3).</summary>
    public const byte FirstProtocol6Type = 0x23;

    /// <summary>The lowest type that needs protocol 7 (the private lobby's friend picker, OD-204 T-L4).</summary>
    public const byte FirstProtocol7Type = 0x25;

    /// <summary>The lowest type that needs protocol 8 (the private lobby's "Who can join", OD-210, CR-S101, W14).</summary>
    public const byte FirstProtocol8Type = 0x28;

    /// <summary>The lowest type that needs protocol 10 (the friends sidebar, OD-211, CR-S102, W15).</summary>
    public const byte FirstProtocol10Type = 0x2A;

    /// <summary>W15: the most friend rows, incoming friend requests, join requests, profiles per friend one <see cref="FriendsState"/> carries.</summary>
    public const int MaxFriendRows = 64, MaxFriendRequests = 16, MaxJoinRequests = 8, MaxFriendProfiles = 4;

    /// <summary>W15 text limits in UTF-8 bytes: a name, a game's short label, a profile link, a result message, a typed profile name.</summary>
    public const int MaxFriendName = 64, MaxFriendGame = 8, MaxFriendUrl = 160, MaxFriendMessage = 160;

    /// <summary>The last type of the UI pipe.</summary>
    public const byte LastType = 0x2C;

    /// <summary>T-L4: the most friends one <see cref="FriendPicker"/> lists (the launcher keeps the first ones, available ones first).</summary>
    public const int MaxPickerFriends = 100;

    /// <summary>T-L3: an invite id on the pipe (<see cref="InvitePrompt"/>, <see cref="InviteAnswer"/>), in bytes of [A-Za-z0-9:._-].</summary>
    public const int MaxInviteId = 64;

    /// <summary>T-L3: <see cref="InvitePrompt"/>'s activity, in UTF-8 bytes, and the longest time it may stay (seconds).</summary>
    public const int MaxInviteActivity = 16, MaxInviteSeconds = 3600;

    /// <summary>OD-181: the players a <see cref="RosterMods"/> names (the "my world" roster's own bound).</summary>
    public const int MaxRosterMembers = 16;

    /// <summary>OD-154: the other players a <see cref="VoiceState"/> lists.</summary>
    public const int MaxVoiceMembers = 16;

    /// <summary>VOICE_CHAT.md §15: a device id or name, in UTF-8 bytes.</summary>
    public const int MaxDeviceText = 256;

    /// <summary>VOICE_CHAT.md §15: every volume slider is 1..100, 100 by default.</summary>
    public const byte MinVolume = 1, MaxVolume = 100;

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

    /// <summary>
    /// 0x03, launcher → overlay: <see cref="UiLevel"/>, icon, lifetime in tenths of a second, localised text. <c>Flags</c> (T-L3, an
    /// additive byte after the text, written only when not 0; an older overlay ignores it): <see cref="UiToastFlags"/>.
    /// </summary>
    public sealed record Toast(byte Level, byte Icon, ushort TtlDs, string Text, byte Flags = 0) : UiFrame
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

    /// <summary>The bridge's join of an official world by id (an empty id: a quick join). Not a pipe frame: 0x09 is retired (OD-208).</summary>
    public sealed record JoinWorld(string WorldId);

    /// <summary>The bridge's join result: <see cref="UiJoin"/> and a localised message. Not a pipe frame: 0x0A is retired (OD-208).</summary>
    public sealed record JoinResult(byte Result, string Message);

    /// <summary>0x0D, overlay → launcher: "my world" (the Worlds tab is shown; every 5 s while it is).</summary>
    public sealed record MyWorldQuery : UiFrame
    {
        public override byte Type => 0x0D;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>
    /// 0x0E, launcher → overlay: the world this profile hosts or holds a seat in (<see cref="UiWorldRole"/>;
    /// <c>None</c>: no world, the other fields empty), its code (host only) and its players. <c>Named</c>
    /// false: the server gave no roster (only the host is known). <c>AnyMode</c> (OD-141, after the members): the world is
    /// open to Hardcore and Noob (the header's "Hardcore + Noob" badge). <c>Transport</c> and <c>Notice</c> (OD-160, after
    /// <c>AnyMode</c>, written only when <c>Transport</c> is not <see cref="UiTransport.Unknown"/>): the connection in use (u8
    /// <see cref="UiTransport"/>) and the lobby notice the overlay shows as given (the launcher's own localized text; an older overlay
    /// ignores trailing bytes of this frame). <c>Official</c> (OD-192, after <c>Notice</c>, written only when true; then
    /// <c>Transport</c> and <c>Notice</c> are written even when unknown): the world is an official one (the overlay shows its
    /// Official Lobby pane, offers no lobby option and shows no notice). <c>Private</c> (OD-204, T-L6, after <c>Official</c>, written
    /// only when true and only with <c>Official</c>): a private official world, created in the game's own menu (the overlay's
    /// Private Lobby pane, T-L3; an older overlay ignores the byte). <c>Leader</c> (OD-204, T-L3, after <c>Private</c>, written only
    /// when private and known): the lobby leader's decimal XUID (WORLDS_OFFICIAL §12.8b); the overlay enables Invite friends for the
    /// leader only, so empty (unknown) lets nobody invite.
    /// </summary>
    public sealed record MyWorld(byte Role, string WorldId, string Name, string Host, string Code, byte Audience, byte Players, byte Max,
        bool Named, IReadOnlyList<MemberRow> Members, bool AnyMode = false, byte Transport = UiTransport.Unknown, string Notice = "",
        bool Official = false, bool Private = false, string Leader = "") : UiFrame
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

    /// <summary>
    /// 0x19, launcher → overlay (protocol 2, CR-L40 §1.3): the connected server's host and figures for the
    /// guide's bottom block. <c>Flags</c> <see cref="UiServerStatus"/>: without <c>Known</c> every figure is
    /// ignored ("Status unavailable"); a figure without its flag is never shown (not as zero).
    /// </summary>
    public sealed record ServerStatus(string Server, byte Flags, ushort Online, ushort Capacity, ushort Hardcore, ushort Noob, ushort Lobbies,
        uint ProfilesHardcore, uint ProfilesNoob) : UiFrame
    {
        public override byte Type => 0x19;

        public override UiDirection Direction => UiDirection.ToOverlay;

        /// <summary>The frame for <paramref name="status"/> (null: unreachable), each figure saturated at its field's maximum.</summary>
        public static ServerStatus From(string server, Api.ServerStatus? status)
        {
            var host = Matchmaking.WorldsBridge.Clip(server, MaxServerHost);
            if (status is null) return new(host, 0, 0, 0, 0, 0, 0, 0, 0);
            static ushort U16(uint? value) => (ushort)Math.Min(value ?? 0, ushort.MaxValue);
            static uint U32(ulong? value) => (uint)Math.Min(value ?? 0, uint.MaxValue);
            var flags = UiServerStatus.Known;
            if (status.Capacity is not null) flags |= UiServerStatus.Capacity;
            if (status.HasLive) flags |= UiServerStatus.Live;
            if (status.HasProfiles) flags |= UiServerStatus.Profiles;
            return new(host, flags, U16(status.Online), U16(status.Capacity), U16(status.Hardcore), U16(status.Noob), U16(status.Lobbies),
                U32(status.ProfilesHardcore), U32(status.ProfilesNoob));
        }

        internal void Validate()
        {
            Limit(Server, MaxServerHost, "server host");
            if (Flags > UiServerStatus.All) throw new ProtocolException("invalid server status flags");
        }
    }

    /// <summary>
    /// 0x1A, launcher → overlay (protocol 2, CR-L40 §1.3): the player's avatar, square, straight-alpha RGBA,
    /// exactly <c>Width * Height * 4</c> bytes after the header; <c>0x1A{0,0}</c> clears it (the initials).
    /// <c>Rgba</c> compares by reference: compare encodings.
    /// </summary>
    public sealed record Avatar(ushort Width, ushort Height, byte[] Rgba) : UiFrame
    {
        public override byte Type => 0x1A;

        public override UiDirection Direction => UiDirection.ToOverlay;

        public static Avatar None { get; } = new(0, 0, []);

        internal void Validate()
        {
            if (Width != Height || Width > MaxAvatarSide || Rgba is null || Rgba.Length != Width * Height * 4)
                throw new ProtocolException("invalid avatar");
        }
    }

    /// <summary>
    /// 0x1B, launcher → overlay (protocol 3, OD-154): the voice chat the overlay shows. <c>Flags</c>
    /// <see cref="UiVoice"/>; <c>Mic</c> <see cref="UiMicMode"/>; <c>Members</c> the other players of the lobby (at most
    /// 16; never this player, whose voice is <c>Mic</c>), each with its <see cref="UiVoiceMember"/> state and the volume
    /// this player hears them at. VOICE_CHAT.md §15: the player's own preferences follow: the voice chat volume, the
    /// microphone volume (1..100) and the chosen microphone and output (an empty id is the system default, with an
    /// empty name; ids and names at most 256 UTF-8 bytes).
    /// </summary>
    public sealed record VoiceState(byte Flags, byte Mic, IReadOnlyList<VoiceMember> Members, byte VoiceVolume = MaxVolume, byte MicVolume = MaxVolume,
        string MicId = "", string MicName = "", string OutputId = "", string OutputName = "") : UiFrame
    {
        public override byte Type => 0x1B;

        public override UiDirection Direction => UiDirection.ToOverlay;

        internal void Validate()
        {
            if (Flags > UiVoice.All || Mic > UiMicMode.Max) throw new ProtocolException("invalid voice state");
            Bound(Members.Count, MaxVoiceMembers, "voice members");
            foreach (var member in Members)
            {
                CheckXuid(member.Xuid, "voice member xuid");
                Limit(member.Gamertag, 64, "voice member gamertag");
                if (member.State > UiVoiceMember.All) throw new ProtocolException("invalid voice member state");
                CheckVolume(member.Volume);
            }
            CheckVolume(VoiceVolume);
            CheckVolume(MicVolume);
            CheckDevice(MicId, MicName);
            CheckDevice(OutputId, OutputName);
        }
    }

    /// <summary>
    /// 0x1C, overlay → launcher (protocol 3, OD-154): <see cref="UiVoiceOp"/> with its <c>Value</c>; <c>Xuid</c> names
    /// the player of ops 2, 6 and 9 and is empty for the others. Ops 7-9 (§15) carry a volume, 1..100.
    /// </summary>
    public sealed record VoiceCommand(byte Op, string Xuid, byte Value) : UiFrame
    {
        public override byte Type => 0x1C;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate()
        {
            if (Op is < UiVoiceOp.LobbyVoice or > UiVoiceOp.PlayerVolume) throw new ProtocolException("invalid voice op");
            if (Op is UiVoiceOp.Mute or UiVoiceOp.MuteForLobby or UiVoiceOp.PlayerVolume) CheckXuid(Xuid, "voice command xuid");
            else if (Xuid.Length != 0) throw new ProtocolException("this voice op names no player");
            if (Op is UiVoiceOp.VoiceVolume or UiVoiceOp.MicVolume or UiVoiceOp.PlayerVolume) CheckVolume(Value);
            else if (Op switch { UiVoiceOp.Mic => Value > UiMicMode.Max, UiVoiceOp.LobbyVoice => Value > UiLobbyVoice.Max, _ => Value > 1 })
                throw new ProtocolException("invalid voice value");
        }
    }

    /// <summary>
    /// 0x1D, overlay → launcher (protocol 3, VOICE_CHAT.md §15): the microphone (<see cref="UiVoiceFlow.Microphone"/>)
    /// or output the player picked. An empty <c>Id</c> is the system default and has an empty <c>Name</c>; both at most
    /// 256 UTF-8 bytes. The name is kept only to say which device went missing.
    /// </summary>
    public sealed record VoiceDevice(byte Flow, string Id, string Name) : UiFrame
    {
        public override byte Type => 0x1D;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate()
        {
            if (Flow > UiVoiceFlow.Max) throw new ProtocolException("invalid voice device flow");
            CheckDevice(Id, Name);
        }
    }

    /// <summary>
    /// 0x1E, launcher → overlay (protocol 4, OD-181 / CR-L48): the mods each player of "my world" declared, sent after every
    /// <see cref="MyWorld"/>. <c>WorldId</c> the world of that <see cref="MyWorld"/> (empty: no world, nobody listed);
    /// <c>Members</c> only the players with at least one mod (at most 16; a player not listed uses none), each by XUID (the
    /// roster's action key, never shown) with its mods as shown: the catalog's name when known, else the mod's id, and its
    /// version (sorted by id, at most 32). The overlay marks each listed player's card and lists the mods in their details.
    /// </summary>
    public sealed record RosterMods(string WorldId, IReadOnlyList<RosterModsMember> Members) : UiFrame
    {
        public override byte Type => 0x1E;

        public override UiDirection Direction => UiDirection.ToOverlay;

        /// <summary>No world: the overlay forgets every player's mods.</summary>
        public static RosterMods None { get; } = new(string.Empty, []);

        internal void Validate()
        {
            Limit(WorldId, MaxPaneId, "roster world id");
            if (WorldId.Length == 0 && Members.Count != 0) throw new ProtocolException("no world lists no players");
            Bound(Members.Count, MaxRosterMembers, "roster members");
            foreach (var member in Members)
            {
                CheckXuid(member.Xuid, "roster member xuid");
                if (member.Mods.Count == 0) throw new ProtocolException("a roster member without mods is not listed");
                Bound(member.Mods.Count, MaxLobbyMods, "roster member mods");
                foreach (var mod in member.Mods)
                {
                    if (mod.Id.Length == 0) throw new ProtocolException("empty roster mod");
                    Limit(mod.Id, 64, "roster mod name");
                    Limit(mod.Version, 16, "roster mod version");
                }
            }
        }
    }

    /// <summary>
    /// 0x1F, overlay → launcher (protocol 5, OD-193 F-2): the game's PLAYER ACTIONS popup opened on this player (decimal XUID); answer
    /// with <see cref="PlayerState"/>. The hook asks through the overlay, which asks at most every few seconds per player.
    /// </summary>
    public sealed record PlayerStateQuery(string Xuid) : UiFrame
    {
        public override byte Type => 0x1F;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate() => CheckXuid(Xuid, "player state xuid");
    }

    /// <summary>0x20, launcher → overlay (protocol 5): which friend item the player's popup offers, <see cref="UiPlayerState"/>.</summary>
    public sealed record PlayerState(string Xuid, byte State) : UiFrame
    {
        public override byte Type => 0x20;

        public override UiDirection Direction => UiDirection.ToOverlay;

        internal void Validate()
        {
            CheckXuid(Xuid, "player state xuid");
            if (State > UiPlayerState.Max) throw new ProtocolException("invalid player state");
        }
    }

    /// <summary>
    /// 0x21, overlay → launcher (protocol 5): the player chose one of our PLAYER ACTIONS items. <c>Id</c> (non-zero) names this user
    /// action (its answer echoes it); <c>Kind</c> <see cref="UiPlayerAction"/>; <c>Reason</c> <see cref="UiReport"/> for a report, else 0.
    /// </summary>
    public sealed record PlayerActionRequest(uint Id, string Xuid, byte Kind, byte Reason) : UiFrame
    {
        public override byte Type => 0x21;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate()
        {
            if (Id == 0) throw new ProtocolException("player action id 0");
            CheckXuid(Xuid, "player action xuid");
            if (Kind is < UiPlayerAction.Add or > UiPlayerAction.Report) throw new ProtocolException("invalid player action");
            if (Kind == UiPlayerAction.Report ? Reason is < UiReport.Cheating or > UiReport.Other : Reason != 0)
                throw new ProtocolException("invalid player action reason");
        }
    }

    /// <summary>
    /// 0x22, launcher → overlay (protocol 5): how <see cref="PlayerActionRequest"/> <c>Id</c> went: <c>Result</c> the text the game shows
    /// (<see cref="UiPlayerResult"/>; the hook holds the words, in the game's language), <c>State</c> the player's state now.
    /// </summary>
    public sealed record PlayerActionDone(uint Id, string Xuid, byte Result, byte State) : UiFrame
    {
        public override byte Type => 0x22;

        public override UiDirection Direction => UiDirection.ToOverlay;

        internal void Validate()
        {
            if (Id == 0) throw new ProtocolException("player action id 0");
            CheckXuid(Xuid, "player action xuid");
            if (Result > UiPlayerResult.Max) throw new ProtocolException("invalid player action result");
            if (State > UiPlayerState.Max) throw new ProtocolException("invalid player state");
        }
    }

    /// <summary>
    /// 0x23, launcher → overlay (protocol 6, OD-204 T-L3, WORLDS_OFFICIAL §12.5): a friend invited this player to a private lobby; the
    /// overlay shows a modal Accept / Decline (never a toast: toasts follow OD-192's quiet rule). <c>InviteId</c> 1-64 bytes of
    /// [A-Za-z0-9:._-], echoed by <see cref="InviteAnswer"/>; <c>FromGamertag</c> 1-64 bytes (shown; never a XUID); <c>Activity</c>
    /// at most 16 bytes (<c>freeroam</c>, <c>roadtrip</c>, anything else shown as a lobby); <c>ExpiresInSec</c> 1-3600, or 0 to
    /// withdraw the prompt of that id (then the gamertag and activity may be empty). A prompt that runs out goes without an answer.
    /// </summary>
    public sealed record InvitePrompt(string InviteId, string FromGamertag, string Activity, ushort ExpiresInSec) : UiFrame
    {
        public override byte Type => 0x23;

        public override UiDirection Direction => UiDirection.ToOverlay;

        /// <summary>The prompt of <paramref name="inviteId"/> goes (answered elsewhere, expired or cancelled).</summary>
        public static InvitePrompt Withdraw(string inviteId) => new(inviteId, "", "", 0);

        internal void Validate()
        {
            PaneKey(InviteId, "invite id");
            Limit(FromGamertag, 64, "invite gamertag");
            Limit(Activity, MaxInviteActivity, "invite activity");
            if (ExpiresInSec > MaxInviteSeconds) throw new ProtocolException("invite expiry past an hour");
            if (ExpiresInSec != 0 && FromGamertag.Length == 0) throw new ProtocolException("an invite names its inviter");
        }
    }

    /// <summary>0x24, overlay → launcher (protocol 6, T-L3): the player's answer to <see cref="InvitePrompt"/> <c>InviteId</c>.</summary>
    public sealed record InviteAnswer(string InviteId, bool Accept) : UiFrame
    {
        public override byte Type => 0x24;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate() => PaneKey(InviteId, "invite id");
    }

    /// <summary>
    /// 0x25, launcher → overlay (protocol 7, OD-204 T-L4, WORLDS_OFFICIAL §12.5): the private lobby's friend picker. <c>Flags</c>
    /// <see cref="UiPickerFlags"/> (bit 0: open the picker now, and the guide with it; without it the list only refreshes a picker
    /// already open); <c>ListStatus</c> <see cref="UiPickerStatus"/> (1: the friend list could not be read, no rows); at most
    /// <see cref="MaxPickerFriends"/> rows of the player's friends only, each <see cref="PickerFriend"/>.
    /// </summary>
    public sealed record FriendPicker(byte Flags, byte ListStatus, IReadOnlyList<PickerFriend> Friends) : UiFrame
    {
        public override byte Type => 0x25;

        public override UiDirection Direction => UiDirection.ToOverlay;

        internal void Validate()
        {
            if (Flags > UiPickerFlags.All) throw new ProtocolException("invalid picker flags");
            if (ListStatus > UiPickerStatus.Max) throw new ProtocolException("invalid picker status");
            Bound(Friends.Count, MaxPickerFriends, "picker friends");
            if (ListStatus != UiPickerStatus.List && Friends.Count != 0) throw new ProtocolException("an unavailable list has no rows");
            foreach (var friend in Friends)
            {
                CheckXuid(friend.Xuid, "picker xuid");
                if (friend.Gamertag.Length == 0) throw new ProtocolException("a picker row names its friend");
                Limit(friend.Gamertag, 64, "picker gamertag");
                if (friend.State > UiPickerState.Max) throw new ProtocolException("invalid picker state");
            }
        }
    }

    /// <summary>0x26, overlay → launcher (protocol 7, T-L4): the Private Lobby pane's Invite friends: answer with <see cref="FriendPicker"/>.</summary>
    public sealed record FriendPickerAsk : UiFrame
    {
        public override byte Type => 0x26;

        public override UiDirection Direction => UiDirection.ToLauncher;
    }

    /// <summary>
    /// 0x27, overlay → launcher (protocol 7, T-L4): the player chose a friend in the picker: invite that profile's decimal XUID
    /// (<c>POST /mm/v1/worlds/{worldId}/invites</c>); the launcher answers with one notice and a refreshed <see cref="FriendPicker"/>.
    /// </summary>
    public sealed record InviteSend(string Xuid) : UiFrame
    {
        public override byte Type => 0x27;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate() => CheckXuid(Xuid, "invite xuid");
    }

    /// <summary>
    /// 0x28, launcher → overlay (protocol 8, OD-210, CR-S101 §2.11, W14): the private lobby's "Who can join", sent after every
    /// <see cref="MyWorld"/>. <c>WorldId</c> the private official world it belongs to (empty: none, nothing shown); <c>Modes</c>
    /// <see cref="UiJoinModes"/> as <c>WorldView.modes</c> says; <c>Offered</c> the choices the world's game has
    /// (<see cref="UiJoinModes.OfferedOf"/>: FM5 has no Hardcore). The overlay lets only the current leader change it (the invite rule).
    /// </summary>
    public sealed record LobbyJoinModes(string WorldId, byte Modes, byte Offered) : UiFrame
    {
        public override byte Type => 0x28;

        public override UiDirection Direction => UiDirection.ToOverlay;

        /// <summary>No private lobby: the overlay shows no control.</summary>
        public static LobbyJoinModes None { get; } = new(string.Empty, UiJoinModes.Everyone, 0);

        internal void Validate()
        {
            Limit(WorldId, MaxPaneId, "join modes world");
            if (Modes > UiJoinModes.Max || Offered > UiJoinModes.OfferAll) throw new ProtocolException("invalid join modes");
            if (WorldId.Length == 0 && (Modes != UiJoinModes.Everyone || Offered != 0)) throw new ProtocolException("no world has no join modes");
            if (WorldId.Length != 0 && (Offered & UiJoinModes.OfferEveryone) == 0) throw new ProtocolException("Everyone is always offered");
        }
    }

    /// <summary>
    /// 0x29, overlay → launcher (protocol 8, OD-210, CR-S101, W14): the private lobby's leader chose who can join from now on
    /// (<see cref="UiJoinModes"/>): <c>PUT /mm/v1/worlds/{worldId}/modes</c>; the launcher answers one notice, a fresh <see cref="MyWorld"/>
    /// and <see cref="LobbyJoinModes"/>. Players already in the lobby stay.
    /// </summary>
    public sealed record JoinModesSet(byte Modes) : UiFrame
    {
        public override byte Type => 0x29;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate()
        {
            if (Modes > UiJoinModes.Max) throw new ProtocolException("invalid join modes");
        }
    }

    /// <summary>
    /// 0x2A, launcher → overlay (protocol 10, OD-211, CR-S102 CORE_API §4.2a, W15): the right FRIENDS sidebar. <c>Flags</c>
    /// <see cref="UiFriendsFlags"/>; up to <see cref="MaxFriendRows"/> friend accounts (online first), up to
    /// <see cref="MaxFriendRequests"/> incoming friend requests and up to <see cref="MaxJoinRequests"/> requests to join this player's
    /// lobby (the leader's). Sent whole after every change; the overlay shows what it last got.
    /// </summary>
    public sealed record FriendsState(byte Flags, IReadOnlyList<FriendRow> Friends, IReadOnlyList<FriendRequestRow> Requests,
        IReadOnlyList<FriendRequestRow> JoinRequests) : UiFrame
    {
        public override byte Type => 0x2A;

        public override UiDirection Direction => UiDirection.ToOverlay;

        /// <summary>No list yet (not loaded): the sidebar says so.</summary>
        public static FriendsState Unloaded { get; } = new(0, [], [], []);

        internal void Validate()
        {
            if (Flags > UiFriendsFlags.All) throw new ProtocolException("invalid friends flags");
            Bound(Friends.Count, MaxFriendRows, "friend rows");
            Bound(Requests.Count, MaxFriendRequests, "friend requests");
            Bound(JoinRequests.Count, MaxJoinRequests, "join requests");
            if ((Flags & UiFriendsFlags.Loaded) == 0 && (Friends.Count != 0 || Requests.Count != 0)) throw new ProtocolException("an unloaded list has no rows");
            foreach (var row in Friends) row.Validate();
            foreach (var request in Requests.Concat(JoinRequests)) request.Validate();
        }
    }

    /// <summary>0x2B, overlay → launcher (protocol 10, W15): a friends action; <see cref="UiFriendsAction"/> names the kinds and what each carries.</summary>
    public sealed record FriendsAction(byte Kind, string Id, string Text) : UiFrame
    {
        public override byte Type => 0x2B;

        public override UiDirection Direction => UiDirection.ToLauncher;

        internal void Validate()
        {
            if (Kind is < UiFriendsAction.Refresh or > UiFriendsAction.Max) throw new ProtocolException("invalid friends action");
            Limit(Text, MaxFriendName, "friends action text");
            if (Kind == UiFriendsAction.Add)
            {
                if (Id.Length != 0 || Text.Length == 0) throw new ProtocolException("Add names a profile, not an id");
            }
            else if (Kind == UiFriendsAction.Refresh)
            {
                if (Id.Length != 0 || Text.Length != 0) throw new ProtocolException("Refresh carries nothing");
            }
            else
            {
                PaneKey(Id, "friends action id");
                if (Text.Length != 0) throw new ProtocolException("this friends action carries no text");
            }
        }
    }

    /// <summary>
    /// 0x2C, launcher → overlay (protocol 10, W15): the answer to a <see cref="FriendsAction"/> (<c>Kind</c> echoed, <c>Result</c>
    /// <see cref="UiFriendsResult"/>) with the words to show, in the player's language (a notice that always shows: the player asked).
    /// </summary>
    public sealed record FriendsResult(byte Kind, byte Result, string Message) : UiFrame
    {
        public override byte Type => 0x2C;

        public override UiDirection Direction => UiDirection.ToOverlay;

        internal void Validate()
        {
            if (Kind is < UiFriendsAction.Refresh or > UiFriendsAction.Max) throw new ProtocolException("invalid friends result kind");
            if (Result > UiFriendsResult.Max) throw new ProtocolException("invalid friends result");
            Limit(Message, MaxFriendMessage, "friends result message");
        }
    }

    internal static void CheckVolume(byte volume)
    {
        if (volume is < MinVolume or > MaxVolume) throw new ProtocolException("invalid volume");
    }

    internal static void CheckDevice(string id, string name)
    {
        Limit(id, MaxDeviceText, "device id");
        Limit(name, MaxDeviceText, "device name");
        if (id.Length == 0 && name.Length != 0) throw new ProtocolException("the system default has no name");
    }

    /// <summary>A decimal XUID: 1-20 ASCII digits.</summary>
    internal static void CheckXuid(string xuid, string what)
    {
        if (xuid.Length is 0 or > 20 || !xuid.All(char.IsAsciiDigit)) throw new ProtocolException($"invalid {what}");
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
                if (t.Flags > UiToastFlags.All) throw new ProtocolException("invalid toast flags");
                w.U8(t.Level);
                w.U8(t.Icon);
                w.U16(t.TtlDs);
                w.Str(t.Text);
                if (t.Flags != 0) w.U8(t.Flags); // T-L3: only when set (the bytes of every older toast stand)
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
                w.Flag(m.AnyMode);
                if (m.Private && !m.Official) throw new ProtocolException("a private MyWorld is an official one");
                if (m.Transport != UiTransport.Unknown || m.Official)
                {
                    if (m.Transport > UiTransport.Direct) throw new ProtocolException("invalid MyWorld.transport");
                    w.U8(m.Transport);
                    w.Str(m.Notice);
                    if (m.Official) w.Flag(true); // OD-192
                    if (m.Private) w.Flag(true); // OD-204 (T-L6)
                    if (m.Private && m.Leader.Length != 0) // OD-204 (T-L3): the leader, only when known
                    {
                        CheckXuid(m.Leader, "MyWorld.leader");
                        w.Str(m.Leader);
                    }
                }
                break;
            case WorldAction a:
                if (a.Action is not (UiWorldAction.Leave or UiWorldAction.AddFriend or UiWorldAction.Report)) throw new ProtocolException("invalid WorldAction.action");
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
            case ServerStatus s:
                s.Validate();
                w.Str(s.Server);
                w.U8(s.Flags);
                w.U16(s.Online);
                w.U16(s.Capacity);
                w.U16(s.Hardcore);
                w.U16(s.Noob);
                w.U16(s.Lobbies);
                w.U32(s.ProfilesHardcore);
                w.U32(s.ProfilesNoob);
                break;
            case Avatar a:
                a.Validate();
                w.U16(a.Width);
                w.U16(a.Height);
                w.Fixed(a.Rgba, a.Rgba.Length, "avatar");
                break;
            case VoiceState v:
                v.Validate();
                w.U8(v.Flags);
                w.U8(v.Mic);
                w.U8((byte)v.Members.Count);
                foreach (var member in v.Members)
                {
                    w.Str(member.Xuid);
                    w.Str(member.Gamertag);
                    w.U8(member.State);
                    w.U8(member.Volume);
                }
                w.U8(v.VoiceVolume);
                w.U8(v.MicVolume);
                w.Str(v.MicId);
                w.Str(v.MicName);
                w.Str(v.OutputId);
                w.Str(v.OutputName);
                break;
            case VoiceCommand c:
                c.Validate();
                w.U8(c.Op);
                w.Str(c.Xuid);
                w.U8(c.Value);
                break;
            case VoiceDevice d:
                d.Validate();
                w.U8(d.Flow);
                w.Str(d.Id);
                w.Str(d.Name);
                break;
            case RosterMods m:
                m.Validate();
                w.Str(m.WorldId);
                w.U8((byte)m.Members.Count);
                foreach (var member in m.Members)
                {
                    w.Str(member.Xuid);
                    w.U8((byte)member.Mods.Count);
                    foreach (var mod in member.Mods)
                    {
                        w.Str(mod.Id);
                        w.Str(mod.Version);
                    }
                }
                break;
            case PlayerStateQuery q:
                q.Validate();
                w.Str(q.Xuid);
                break;
            case PlayerState s:
                s.Validate();
                w.Str(s.Xuid);
                w.U8(s.State);
                break;
            case PlayerActionRequest a:
                a.Validate();
                w.U32(a.Id);
                w.Str(a.Xuid);
                w.U8(a.Kind);
                w.U8(a.Reason);
                break;
            case PlayerActionDone d:
                d.Validate();
                w.U32(d.Id);
                w.Str(d.Xuid);
                w.U8(d.Result);
                w.U8(d.State);
                break;
            case InvitePrompt p:
                p.Validate();
                w.Str(p.InviteId);
                w.Str(p.FromGamertag);
                w.Str(p.Activity);
                w.U16(p.ExpiresInSec);
                break;
            case InviteAnswer a:
                a.Validate();
                w.Str(a.InviteId);
                w.Flag(a.Accept);
                break;
            case FriendPicker p:
                p.Validate();
                w.U8(p.Flags);
                w.U8(p.ListStatus);
                w.U8((byte)p.Friends.Count);
                foreach (var friend in p.Friends)
                {
                    w.Str(friend.Xuid);
                    w.Str(friend.Gamertag);
                    w.U8(friend.State);
                }
                break;
            case FriendPickerAsk:
                break;
            case InviteSend s:
                s.Validate();
                w.Str(s.Xuid);
                break;
            case LobbyJoinModes j: // OD-210 (W14)
                j.Validate();
                w.Str(j.WorldId);
                w.U8(j.Modes);
                w.U8(j.Offered);
                break;
            case JoinModesSet s:
                s.Validate();
                w.U8(s.Modes);
                break;
            case FriendsState f: // W15 (protocol 10)
                f.Validate();
                w.U8(f.Flags);
                w.U8((byte)f.Friends.Count);
                foreach (var row in f.Friends)
                {
                    w.Str(row.Id);
                    w.Str(row.Name);
                    w.U16(row.Flags);
                    w.Str(row.Game);
                    w.U8(row.Activity);
                    w.U8(row.Excluded);
                    w.Str(row.DiscordUser);
                    w.Str(row.DiscordName);
                    w.U32(row.AccountDays);
                    w.Str(row.ProfileUrl);
                    w.U8((byte)row.Profiles.Count);
                    foreach (var profile in row.Profiles)
                    {
                        w.Str(profile.Name);
                        w.U8(profile.Mode);
                        w.Str(profile.Game);
                    }
                }
                w.U8((byte)f.Requests.Count);
                foreach (var request in f.Requests)
                {
                    w.Str(request.Id);
                    w.Str(request.Name);
                }
                w.U8((byte)f.JoinRequests.Count);
                foreach (var request in f.JoinRequests)
                {
                    w.Str(request.Id);
                    w.Str(request.Name);
                }
                break;
            case FriendsAction a:
                a.Validate();
                w.U8(a.Kind);
                w.Str(a.Id);
                w.Str(a.Text);
                break;
            case FriendsResult r:
                r.Validate();
                w.U8(r.Kind);
                w.U8(r.Result);
                w.Str(r.Message);
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
        // OD-208: the lobby list, join by code, create and hosted frames (0x07-0x0C, 0x14, 0x15) are gone: unknown, as any other
        if (buffer.Length >= 5 && buffer[4] is < 0x01 or > LastType or (>= 0x07 and <= 0x0C) or 0x14 or 0x15)
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
            0x03 => new Toast(r.U8(), r.U8(), r.U16(), r.Str(), r.IsEmpty ? (byte)0 : r.U8()),
            0x04 => new Panel(r.Flag("Panel.open")),
            0x05 => new UiEvent(r.U8(), r.Str()),
            0x06 => new Bye(r.U8()),
            0x0D => new MyWorldQuery(),
            0x0E => DecodeMine(ref r),
            0x0F => r.U8() is var action and (UiWorldAction.Leave or UiWorldAction.AddFriend or UiWorldAction.Report)
                ? new WorldAction(action, r.Str(), r.Str(), r.U8()) : throw new ProtocolException("invalid WorldAction.action"),
            0x10 => new ActionResult(r.U8(), r.U8(), r.Str()),
            0x11 => new GameReadiness(r.Flag("GameReadiness.ready")),
            0x12 => new WorldAccess(r.Flag("WorldAccess.allowed"), r.Str()),
            0x13 => new ConnectionDetails(r.U8(), r.Str()),
            0x16 => DecodePane(ref r),
            0x17 => new PlayerDetailsQuery(r.Str(), r.Str()),
            0x18 => new PaneActionClick(r.Str(), r.Str()),
            0x19 => new ServerStatus(r.Str(), r.U8(), r.U16(), r.U16(), r.U16(), r.U16(), r.U16(), r.U32(), r.U32()),
            0x1A => DecodeAvatar(ref r),
            0x1B => DecodeVoiceState(ref r),
            0x1C => new VoiceCommand(r.U8(), r.Str(), r.U8()),
            0x1D => new VoiceDevice(r.U8(), r.Str(), r.Str()),
            0x1E => DecodeRosterMods(ref r),
            0x1F => new PlayerStateQuery(r.Str()),
            0x20 => new PlayerState(r.Str(), r.U8()),
            0x21 => new PlayerActionRequest(r.U32(), r.Str(), r.U8(), r.U8()),
            0x22 => new PlayerActionDone(r.U32(), r.Str(), r.U8(), r.U8()),
            0x23 => new InvitePrompt(r.Str(), r.Str(), r.Str(), r.U16()),
            0x24 => new InviteAnswer(r.Str(), r.Flag("InviteAnswer.accept")),
            0x25 => DecodePicker(ref r),
            0x26 => new FriendPickerAsk(),
            0x27 => new InviteSend(r.Str()),
            0x28 => new LobbyJoinModes(r.Str(), r.U8(), r.U8()),
            0x29 => new JoinModesSet(r.U8()),
            0x2A => DecodeFriendsState(ref r),
            0x2B => new FriendsAction(r.U8(), r.Str(), r.Str()),
            _ => new FriendsResult(r.U8(), r.U8(), r.Str()),
        };
        r.End();
        switch (frame)
        {
            case PlayerDetailsQuery or PaneActionClick or ServerStatus or VoiceState or VoiceCommand or VoiceDevice or RosterMods
                or PlayerStateQuery or PlayerState or PlayerActionRequest or PlayerActionDone or InvitePrompt or InviteAnswer or Toast
                or FriendPicker or InviteSend or LobbyJoinModes or JoinModesSet or FriendsState or FriendsAction or FriendsResult:
                _ = frame.Encode(); // the same rules as the sender's
                break;
        }
        if (frame is ConnectionDetails details && (details.State > 2 || System.Text.Encoding.UTF8.GetByteCount(details.Text) > 4096)) throw new ProtocolException("Invalid connection details");
        if (frame is UiEvent { Kind: UiEventKind.ConnectionDetails } query && query.Detail.Length != 0) throw new ProtocolException("Connection query detail must be empty");
        used = 4 + length.Value;
        return frame;
    }

    private static FriendsState DecodeFriendsState(ref WireReader r)
    {
        var flags = r.U8();
        var count = r.U8();
        Bound(count, MaxFriendRows, "friend rows");
        var friends = new List<FriendRow>(count);
        for (var i = 0; i < count; ++i)
        {
            var id = r.Str();
            var name = r.Str();
            var rowFlags = r.U16();
            var game = r.Str();
            var activity = r.U8();
            var excluded = r.U8();
            var discordUser = r.Str();
            var discordName = r.Str();
            var days = r.U32();
            var url = r.Str();
            var profileCount = r.U8();
            Bound(profileCount, MaxFriendProfiles, "friend profiles");
            var profiles = new List<FriendProfileRow>(profileCount);
            for (var p = 0; p < profileCount; ++p) profiles.Add(new FriendProfileRow(r.Str(), r.U8(), r.Str()));
            friends.Add(new FriendRow(id, name, rowFlags, game, activity, excluded, discordUser, discordName, days, url, profiles));
        }
        var requests = DecodeFriendRequests(ref r, MaxFriendRequests, "friend requests");
        var joins = DecodeFriendRequests(ref r, MaxJoinRequests, "join requests");
        return new FriendsState(flags, friends, requests, joins);
    }

    private static List<FriendRequestRow> DecodeFriendRequests(ref WireReader r, int max, string what)
    {
        var count = r.U8();
        Bound(count, max, what);
        var rows = new List<FriendRequestRow>(count);
        for (var i = 0; i < count; ++i) rows.Add(new FriendRequestRow(r.Str(), r.Str()));
        return rows;
    }

    private static FriendPicker DecodePicker(ref WireReader r)
    {
        var flags = r.U8();
        var status = r.U8();
        var count = r.U8();
        Bound(count, MaxPickerFriends, "picker friends");
        var friends = new List<PickerFriend>(count);
        for (var i = 0; i < count; ++i) friends.Add(new PickerFriend(r.Str(), r.Str(), r.U8()));
        return new FriendPicker(flags, status, friends);
    }

    private static VoiceState DecodeVoiceState(ref WireReader r)
    {
        var flags = r.U8();
        var mic = r.U8();
        var count = r.U8();
        Bound(count, MaxVoiceMembers, "voice members");
        var members = new List<VoiceMember>(count);
        for (var i = 0; i < count; ++i) members.Add(new VoiceMember(r.Str(), r.Str(), r.U8(), r.U8()));
        return new VoiceState(flags, mic, members, r.U8(), r.U8(), r.Str(), r.Str(), r.Str(), r.Str());
    }

    private static RosterMods DecodeRosterMods(ref WireReader r)
    {
        var world = r.Str();
        var count = r.U8();
        Bound(count, MaxRosterMembers, "roster members");
        var members = new List<RosterModsMember>(count);
        for (var i = 0; i < count; ++i)
        {
            var xuid = r.Str();
            var modCount = r.U8();
            Bound(modCount, MaxLobbyMods, "roster member mods");
            var mods = new List<LobbyMod>(modCount);
            for (var m = 0; m < modCount; ++m) mods.Add(new LobbyMod(r.Str(), r.Str()));
            members.Add(new RosterModsMember(xuid, mods));
        }
        return new RosterMods(world, members);
    }

    private static Avatar DecodeAvatar(ref WireReader r)
    {
        var width = r.U16();
        var height = r.U16();
        if (width != height || width > MaxAvatarSide) throw new ProtocolException("invalid avatar");
        return new Avatar(width, height, r.Take(width * height * 4).ToArray());
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
        var anyMode = r.Flag("MyWorld.anyMode");
        if (r.IsEmpty) return new MyWorld(role, id, name, host, code, audience, players, max, named, members, anyMode);
        // OD-160: the connection in use and its notice (a launcher before it ends after anyMode)
        var transport = r.U8();
        var notice = r.Str();
        var official = !r.IsEmpty && r.Flag("MyWorld.official"); // OD-192: an unknown transport only ahead of it
        if (transport > UiTransport.Direct || (transport == UiTransport.Unknown && !official)) throw new ProtocolException("invalid MyWorld.transport");
        var isPrivate = !r.IsEmpty && r.Flag("MyWorld.private"); // OD-204 (T-L6): only after an official flag of 1
        if (isPrivate && !official) throw new ProtocolException("a private MyWorld is an official one");
        var leader = ""; // OD-204 (T-L3): only after a private flag, and only when known (an empty one is never written)
        if (isPrivate && !r.IsEmpty)
        {
            leader = r.Str();
            CheckXuid(leader, "MyWorld.leader");
        }
        return new MyWorld(role, id, name, host, code, audience, players, max, named, members, anyMode, transport, notice, official, isPrivate, leader);
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

    public sealed record Mine : UiIncoming;

    /// <summary>Leave (2), friend (4) or report (5).</summary>
    public sealed record Act(UiFrame.WorldAction Request) : UiIncoming;
    /// <summary>The game's player card: answer with a <see cref="UiFrame.DetailPane"/> <c>player:&lt;xuid&gt;</c>.</summary>
    public sealed record PlayerDetails(UiFrame.PlayerDetailsQuery Request) : UiIncoming;

    public sealed record PaneClick(UiFrame.PaneActionClick Request) : UiIncoming;

    /// <summary>Protocol 3 (OD-154): a voice chat change; answer with the new <see cref="UiFrame.VoiceState"/>.</summary>
    public sealed record Voice(UiFrame.VoiceCommand Request) : UiIncoming;

    /// <summary>VOICE_CHAT.md §15: the player picked a microphone or an output; answer with the new <see cref="UiFrame.VoiceState"/>.</summary>
    public sealed record VoiceDevice(UiFrame.VoiceDevice Request) : UiIncoming;

    /// <summary>Protocol 5 (OD-193 F-2): the game's PLAYER ACTIONS asks a player's friend state; answer with <see cref="UiFrame.PlayerState"/>.</summary>
    public sealed record PlayerStateAsk(UiFrame.PlayerStateQuery Request) : UiIncoming;

    /// <summary>Protocol 5 (OD-193 F-2): one of our PLAYER ACTIONS items was chosen; answer with <see cref="UiFrame.PlayerActionDone"/>.</summary>
    public sealed record PlayerAct(UiFrame.PlayerActionRequest Request) : UiIncoming;

    /// <summary>Protocol 6 (OD-204 T-L3): the player answered an invite prompt (the launcher's accept and decline are T-L4/T-L5's).</summary>
    public sealed record InviteAnswer(UiFrame.InviteAnswer Answer) : UiIncoming;

    /// <summary>Protocol 7 (OD-204 T-L4): the Private Lobby pane's Invite friends; answer with <see cref="UiFrame.FriendPicker"/> or a notice.</summary>
    public sealed record PickerAsk : UiIncoming;

    /// <summary>Protocol 7 (OD-204 T-L4): a friend chosen in the picker: invite it, then one notice and the refreshed picker.</summary>
    public sealed record InviteSend(UiFrame.InviteSend Request) : UiIncoming;

    /// <summary>Protocol 8 (OD-210, CR-S101, W14): the private lobby's leader chose who can join; one notice, then the lobby again.</summary>
    public sealed record JoinModesSet(UiFrame.JoinModesSet Request) : UiIncoming;

    /// <summary>Protocol 10 (OD-211, CR-S102, W15): an action of the friends sidebar; answer with <see cref="UiFrame.FriendsResult"/> and a fresh <see cref="UiFrame.FriendsState"/>.</summary>
    public sealed record FriendsAct(UiFrame.FriendsAction Request) : UiIncoming;
}

/// <summary><see cref="UiFrame.FriendsState"/>'s flags (W15).</summary>
public static class UiFriendsFlags
{
    /// <summary>The friend list was read; without it the sidebar says it could not.</summary>
    public const byte Loaded = 1;

    /// <summary>This player leads a private lobby: "Invite to my game" is offered.</summary>
    public const byte HaveLobby = 2;

    public const byte All = Loaded | HaveLobby;
}

/// <summary><see cref="FriendRow.Flags"/> (W15).</summary>
public static class UiFriendFlag
{
    public const ushort Online = 1, Favorite = 2, Supporter = 4, CanAsk = 8, CanInvite = 0x10, Excluded = 0x20, Racing = 0x40, Private = 0x80;

    public const ushort All = 0xFF;
}

/// <summary><see cref="FriendRow.Activity"/> (W15): what an online friend does; the overlay words it with the game ("Racing on FH2").</summary>
public static class UiFriendActivity
{
    public const byte Offline = 0, Menus = 1, FreeRoam = 2, CarMeet = 3, RoadTrip = 4, Race = 5, Max = Race;

    /// <summary>The wire value of a presence: a <c>doing.activity</c> when the server sends one, else what <c>officialActivity</c> / <c>state</c> say.</summary>
    public static byte Of(Api.FriendPlaying? playing, bool online)
    {
        if (!online || playing is null || playing.State == "offline") return Offline;
        if (playing.Doing is { } doing)
            return doing.Racing == true || doing.Activity == "race" ? Race
                : doing.Activity == "freeroam" ? FreeRoam : doing.Activity == "carmeet" ? CarMeet : doing.Activity == "roadtrip" ? RoadTrip : Menus;
        return playing.OfficialActivity switch { "freeroam" => FreeRoam, "carmeet" => CarMeet, "roadtrip" => RoadTrip, _ => Menus };
    }
}

/// <summary><see cref="FriendRow.Excluded"/> (W15): why "Ask to join" / "Invite to my game" is off for a friend.</summary>
public static class UiFriendExcluded
{
    public const byte None = 0, Mode = 1, Other = 2, Max = Other;
}

/// <summary><see cref="UiFrame.FriendsAction.Kind"/> (W15) and what each carries (<c>Id</c> the friend account, request or join request).</summary>
public static class UiFriendsAction
{
    /// <summary>Read the lists again (the sidebar opened). Carries nothing.</summary>
    public const byte Refresh = 1;

    /// <summary>"Add friend": <c>Text</c> a profile name (any profile of the account), no id.</summary>
    public const byte Add = 2;

    /// <summary>Accept an incoming friend request (<c>Id</c> its id).</summary>
    public const byte Accept = 3;

    /// <summary>Decline an incoming friend request (<c>DELETE /launcher/v1/friends/{id}</c>).</summary>
    public const byte Decline = 4;

    /// <summary>Unfriend (<c>Id</c> the friend account).</summary>
    public const byte Unfriend = 5;

    /// <summary>"Ask to join" the friend's world (<c>POST /mm/v1/worlds/{worldId}/requests</c>).</summary>
    public const byte AskToJoin = 6;

    /// <summary>"Invite to my game" (the private lobby's invite).</summary>
    public const byte InviteToMine = 7;

    /// <summary>The leader accepts a request to join its lobby (<c>Id</c> the request).</summary>
    public const byte AcceptJoin = 8;

    /// <summary>The leader declines a request to join its lobby.</summary>
    public const byte DeclineJoin = 9;

    public const byte Max = DeclineJoin;
}

/// <summary><see cref="UiFrame.FriendsResult.Result"/> (W15).</summary>
public static class UiFriendsResult
{
    public const byte Done = 0, Requested = 1, NowFriends = 2, NotFound = 3, Refused = 4, Failed = 5, Max = Failed;
}

/// <summary>A profile of a friend as the gamercard lists it: its name, mode (0 hardcore, 1 noob) and the game's short label.</summary>
public sealed record FriendProfileRow(string Name, byte Mode, string Game)
{
    internal void Validate()
    {
        if (Name.Length == 0) throw new ProtocolException("a friend profile has a name");
        UiFrame.Limit(Name, UiFrame.MaxFriendName, "friend profile name");
        UiFrame.Limit(Game, UiFrame.MaxFriendGame, "friend profile game");
        if (Mode > 1) throw new ProtocolException("invalid friend profile mode");
    }
}

/// <summary>An incoming friend request or a request to join this player's lobby: its id (the action key) and the asker's profile name.</summary>
public sealed record FriendRequestRow(string Id, string Name)
{
    internal void Validate()
    {
        UiFrame.PaneKey(Id, "friend request id");
        if (Name.Length == 0) throw new ProtocolException("a request names its asker");
        UiFrame.Limit(Name, UiFrame.MaxFriendName, "friend request name");
    }
}

/// <summary>
/// One friend account of <see cref="UiFrame.FriendsState"/> (W15): <c>Id</c> the account handle (the action key), <c>Name</c> the profile
/// shown (the one online, else the first), <c>Flags</c> <see cref="UiFriendFlag"/>, <c>Game</c> the short label of the game it plays,
/// <c>Activity</c> <see cref="UiFriendActivity"/>, <c>Excluded</c> <see cref="UiFriendExcluded"/>, the linked Discord (empty: none),
/// <c>AccountDays</c> the account's age in days (<see cref="Unknown"/> when the server did not say), <c>ProfileUrl</c> an https link
/// (empty: not listed on the website) and every <c>Profiles</c> name with its mode.
/// </summary>
public sealed record FriendRow(string Id, string Name, ushort Flags, string Game, byte Activity, byte Excluded, string DiscordUser,
    string DiscordName, uint AccountDays, string ProfileUrl, IReadOnlyList<FriendProfileRow> Profiles)
{
    /// <summary>No age: the server sent no <c>accountSince</c>.</summary>
    public const uint Unknown = uint.MaxValue;

    internal void Validate()
    {
        UiFrame.PaneKey(Id, "friend id");
        if (Name.Length == 0) throw new ProtocolException("a friend row has a name");
        UiFrame.Limit(Name, UiFrame.MaxFriendName, "friend name");
        UiFrame.Limit(Game, UiFrame.MaxFriendGame, "friend game");
        UiFrame.Limit(DiscordUser, UiFrame.MaxFriendName, "friend discord user");
        UiFrame.Limit(DiscordName, UiFrame.MaxFriendName, "friend discord name");
        UiFrame.Limit(ProfileUrl, UiFrame.MaxFriendUrl, "friend profile url");
        if (ProfileUrl.Length != 0 && !ProfileUrl.StartsWith("https://", StringComparison.Ordinal)) throw new ProtocolException("a profile link is https");
        if (Flags > UiFriendFlag.All || Activity > UiFriendActivity.Max || Excluded > UiFriendExcluded.Max) throw new ProtocolException("invalid friend row");
        if (((Flags & UiFriendFlag.Excluded) != 0) != (Excluded != UiFriendExcluded.None)) throw new ProtocolException("excluded flag and reason agree");
        UiFrame.Bound(Profiles.Count, UiFrame.MaxFriendProfiles, "friend profiles");
        foreach (var profile in Profiles) profile.Validate();
    }
}

/// <summary>
/// OD-210 (CR-S101 §2.11, W14): the private lobby's "Who can join" on the pipe (<see cref="UiFrame.LobbyJoinModes"/>,
/// <see cref="UiFrame.JoinModesSet"/>) and its <c>WorldView.modes</c> / <c>WorldJoinModes.modes</c> values.
/// </summary>
public static class UiJoinModes
{
    /// <summary>Everyone (<c>any</c>), the default of every private lobby.</summary>
    public const byte Everyone = 0;

    /// <summary>Hardcore only (<c>hardcore</c>).</summary>
    public const byte HardcoreOnly = 1;

    /// <summary>Noob only (<c>noob</c>).</summary>
    public const byte NoobOnly = 2;

    public const byte Max = NoobOnly;

    /// <summary><see cref="UiFrame.LobbyJoinModes.Offered"/>: one bit per choice (bit n is choice n).</summary>
    public const byte OfferEveryone = 1, OfferHardcore = 2, OfferNoob = 4, OfferAll = OfferEveryone | OfferHardcore | OfferNoob;

    /// <summary>A <c>WorldView.modes</c> (or, from an older server, its <c>mode</c>) as a choice; anything else reads as Everyone.</summary>
    public static byte Of(string? modes) => modes switch
    {
        Matchmaking.WorldModes.Hardcore => HardcoreOnly,
        Matchmaking.WorldModes.Noob => NoobOnly,
        _ => Everyone,
    };

    /// <summary>The <c>WorldJoinModes.modes</c> the route takes for a choice.</summary>
    public static string ToServer(byte choice) => choice switch
    {
        HardcoreOnly => Matchmaking.WorldModes.Hardcore,
        NoobOnly => Matchmaking.WorldModes.Noob,
        _ => Matchmaking.WorldModes.Any,
    };

    /// <summary>The choices a game's world has (CR-S101): FM5 has no Hardcore (the server answers 400 <c>modes</c>), FH2 all three.</summary>
    public static byte OfferedOf(string? game) => game == "fm5" ? (byte)(OfferEveryone | OfferNoob) : OfferAll;

    /// <summary>Whether <paramref name="choice"/> is offered in <paramref name="offered"/>.</summary>
    public static bool Offers(byte offered, byte choice) => choice <= Max && (offered & (1 << choice)) != 0;
}

/// <summary><see cref="UiFrame.FriendPicker"/>'s flags (T-L4).</summary>
public static class UiPickerFlags
{
    /// <summary>Open the picker now (and the guide with it); without it the list only refreshes a picker already open.</summary>
    public const byte Open = 1;

    public const byte All = Open;
}

/// <summary><see cref="UiFrame.FriendPicker"/>'s status (T-L4).</summary>
public static class UiPickerStatus
{
    /// <summary>The rows are the friend list (possibly empty: no friends to invite).</summary>
    public const byte List = 0;

    /// <summary>The friend list could not be read: the picker says so, with no rows.</summary>
    public const byte Unavailable = 1;

    public const byte Max = Unavailable;
}

/// <summary>A <see cref="PickerFriend"/>'s state (T-L4): only <see cref="Available"/> may be chosen.</summary>
public static class UiPickerState
{
    public const byte Available = 0;

    /// <summary>Already in this lobby (its roster, or the friend's presence names this world).</summary>
    public const byte InLobby = 1;

    /// <summary>This launcher's invite to that friend is pending (it holds a seat for 2 minutes).</summary>
    public const byte Invited = 2;

    /// <summary>Not online now.</summary>
    public const byte Offline = 3;

    public const byte Max = Offline;
}

/// <summary>
/// One row of <see cref="UiFrame.FriendPicker"/> (T-L4): a friend account's profile as the invite names it (the decimal XUID is the
/// action key, never shown), its gamertag and <see cref="UiPickerState"/>.
/// </summary>
public sealed record PickerFriend(string Xuid, string Gamertag, byte State);

/// <summary><c>Toast.flags</c> (T-L3), an additive byte after the text.</summary>
public static class UiToastFlags
{
    /// <summary>
    /// The notice shows even in an official lobby, a private one included: a private create's result and its hold, and the flagged
    /// Hardcore message with its time left (<see cref="Matchmaking.PrivateLobbyNotices"/>). Every other notice follows OD-192's quiet
    /// rule (<see cref="Supervisor.OverlayChannel.Quiet"/>, the overlay's PushToast). Invite prompts are their own frame.
    /// </summary>
    public const byte Always = 1;

    public const byte All = Always;
}

/// <summary>
/// One other player of <see cref="UiFrame.VoiceState"/>: <c>State</c> <see cref="UiVoiceMember"/>; <c>Volume</c> (§15)
/// the level this player hears them at, 1..100.
/// </summary>
public sealed record VoiceMember(string Xuid, string Gamertag, byte State, byte Volume = UiFrame.MaxVolume);

/// <summary><see cref="UiFrame.VoiceDevice"/>'s flow (§15).</summary>
public static class UiVoiceFlow
{
    public const byte Microphone = 0, Output = 1, Max = Output;
}

/// <summary><c>VoiceState.flags</c> (OD-154).</summary>
public static class UiVoice
{
    /// <summary>The lobby's voice chat is on (meaningful with <see cref="LobbyKnown"/>).</summary>
    public const byte LobbyOn = 1;

    /// <summary>The lobby's switch is known (the host's own, or the host's relay said it).</summary>
    public const byte LobbyKnown = 2;

    /// <summary>Retired with player hosting (OD-208): the launcher never sets it; the bit stays the layout's.</summary>
    public const byte MayChange = 4;

    /// <summary>Voice chat is on in this player's settings.</summary>
    public const byte Enabled = 8;

    /// <summary>CR-L46: the lobby's voice mode is proximity (with <see cref="LobbyOn"/>; without it, on is open).</summary>
    public const byte Proximity = 16;

    /// <summary>CR-S67/S68: this player's own voice is blocked (a moderator voice ban or a community mute in force).</summary>
    public const byte Blocked = 32;

    public const byte All = 63;
}

/// <summary>The lobby's voice mode on the UI pipe: <see cref="UiVoiceOp.LobbyVoice"/>'s value and <c>CreateWorld.voice</c> (CR-L46).</summary>
public static class UiLobbyVoice
{
    public const byte Off = 0, Open = 1, Proximity = 2, Max = Proximity;
}

/// <summary><c>VoiceState.mic</c> and the value of <see cref="UiVoiceOp.Mic"/>.</summary>
public static class UiMicMode
{
    public const byte Open = 0, Muted = 1, PushToTalk = 2, Max = PushToTalk;
}

/// <summary><c>VoiceMember.state</c> bits.</summary>
public static class UiVoiceMember
{
    public const byte MutedByMe = 1, Talking = 2, MutedForLobby = 4, Blocked = 8, All = 15;
}

/// <summary><c>VoiceCommand.op</c>.</summary>
public static class UiVoiceOp
{
    /// <summary>The host sets the lobby's voice mode, <see cref="UiLobbyVoice"/> (0 off, 1 open, 2 proximity), at any time (OD-154, CR-L46).</summary>
    public const byte LobbyVoice = 1;

    public const byte Mute = 2;
    public const byte MuteEveryone = 3;
    public const byte Mic = 4;
    public const byte Enabled = 5;

    /// <summary>The host mutes a player for everyone in the lobby (1) or lifts it (0).</summary>
    public const byte MuteForLobby = 6;

    /// <summary>§15: the voice chat volume (the mix of every other player), 1..100.</summary>
    public const byte VoiceVolume = 7;

    /// <summary>§15: this player's microphone volume, 1..100.</summary>
    public const byte MicVolume = 8;

    /// <summary>§15: the volume this player hears the named player at, 1..100 (kept per XUID).</summary>
    public const byte PlayerVolume = 9;
}

/// <summary>
/// A mod of a lobby or a player, display only (MODDED_LOBBIES.md §2): <c>Id</c> the text shown, the catalog's name when the
/// launcher knows the mod (OD-181), else the manifest's multiplayer <c>compatibilityId</c>; <c>Version</c> its revision.
/// </summary>
public sealed record LobbyMod(string Id, string Version);

/// <summary>One player of <see cref="UiFrame.RosterMods"/>: the roster's XUID (the action key, never shown) and the mods it declared.</summary>
public sealed record RosterModsMember(string Xuid, IReadOnlyList<LobbyMod> Mods);

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
/// One player of <see cref="UiFrame.MyWorld"/>: <c>Tier</c> <see cref="UiTier"/>, <c>Role</c> 0 host /
/// 1 seated / 2 reserved, <c>Friend</c> <see cref="UiFriend"/>. The XUID is the action key, never shown (OD-61).
/// </summary>
public sealed record MemberRow(string Xuid, string Gamertag, byte Tier, byte Role, byte Friend);

/// <summary><c>MyWorld.role</c>.</summary>
public static class UiWorldRole
{
    public const byte None = 0;
    // 1 (a host's own world) is retired: players host no world (OD-208)
    public const byte Reserved = 2;
    public const byte Seated = 3;
}

/// <summary>
/// <c>MyWorld.transport</c> (OD-160): how this player's datagrams travel now, from the server's transport and the hook's reports
/// (<see cref="Unknown"/>: not said, and nothing more is written on the wire).
/// </summary>
public static class UiTransport
{
    public const byte Unknown = 0;
    public const byte Relay = 1;
    public const byte Direct = 2;
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
    // 1 (the host closes its world) and 3 (the host kicks a player) are retired: players host no world (OD-208).

    /// <summary>A seat holder leaves its seat.</summary>
    public const byte Leave = 2;

    /// <summary>A friend request (or the acceptance of theirs) by gamertag.</summary>
    public const byte AddFriend = 4;

    public const byte Report = 5;
}

/// <summary>
/// <see cref="UiFrame.PlayerState"/> (OD-193 F-2): the friend item the game's PLAYER ACTIONS offers for a player, from the account's
/// friend list (<c>GET /launcher/v1/friends</c>). REPORT PLAYER is offered in every state but <see cref="Own"/>.
/// </summary>
public static class UiPlayerState
{
    /// <summary>Not known (the list could not be read): only REPORT PLAYER.</summary>
    public const byte Unknown = 0;

    /// <summary>ADD FRIEND.</summary>
    public const byte None = 1;

    /// <summary>REMOVE FRIEND (a profile of a friend account).</summary>
    public const byte Friend = 2;

    /// <summary>ACCEPT FRIEND REQUEST (that account asked this one).</summary>
    public const byte Incoming = 3;

    /// <summary>CANCEL FRIEND REQUEST (this account asked that profile).</summary>
    public const byte Outgoing = 4;

    /// <summary>A profile of this account: no item.</summary>
    public const byte Own = 5;

    public const byte Max = Own;
}

/// <summary><see cref="UiFrame.PlayerActionRequest"/> kinds (OD-193 F-2).</summary>
public static class UiPlayerAction
{
    public const byte Add = 1, Remove = 2, Accept = 3, Cancel = 4, Report = 5;
}

/// <summary>
/// <see cref="UiFrame.PlayerActionDone"/> results: the text the game's own message popup shows (the hook holds it in English and
/// pt-BR; <see cref="None"/> shows nothing).
/// </summary>
public static class UiPlayerResult
{
    public const byte None = 0;

    /// <summary>202: the game's own "Your report was submitted." (Storefront 0xa139).</summary>
    public const byte ReportSubmitted = 1;

    /// <summary>The report was not taken (404 and every other failure): the game's "Your report failed to submit." (Storefront 0xf33d).</summary>
    public const byte ReportFailed = 2;

    /// <summary>429 <c>REQUEST.RATE_LIMITED</c>.</summary>
    public const byte TryLater = 3;

    public const byte FriendRequestSent = 4;

    /// <summary>Friends now (an ADD that met their request, or an ACCEPT).</summary>
    public const byte FriendAdded = 5;

    public const byte FriendRemoved = 6;
    public const byte RequestCancelled = 7;

    /// <summary>404 <c>PORTAL.NOT_FOUND</c> on a friend action.</summary>
    public const byte PlayerNotFound = 8;

    /// <summary>409 <c>SOCIAL.LIMIT_REACHED</c>.</summary>
    public const byte FriendLimit = 9;

    public const byte ActionFailed = 10;
    public const byte Max = ActionFailed;
}

/// <summary><c>WorldAction.reason</c> for a report.</summary>
public static class UiReport
{
    public const byte Cheating = 1;
    public const byte Abuse = 2;
    public const byte Other = 3;
}

/// <summary><c>ServerStatus.flags</c> (CR-L40 §1.3).</summary>
public static class UiServerStatus
{
    /// <summary>The figures were read; without it the overlay shows "Status unavailable".</summary>
    public const byte Known = 1;

    public const byte Capacity = 2;

    /// <summary>Hardcore, Noob and Lobbies came from the server's <c>live</c>.</summary>
    public const byte Live = 4;

    public const byte Profiles = 8;
    public const byte All = Known | Capacity | Live | Profiles;
}

/// <summary><see cref="UiFrame.JoinResult"/>'s result (0 reserved; the rest is a refusal).</summary>
public static class UiJoin
{
    public const byte Reserved = 0;
    public const byte Full = 1;
    public const byte NotAllowed = 2;
    // 3 (a lobby code needed) is retired with lobby codes (OD-208)
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
        return Ready && (frame.Type < UiFrame.FirstProtocol2Type || Protocol >= 2) && (frame.Type < UiFrame.FirstProtocol3Type || Protocol >= 3)
            && (frame.Type < UiFrame.FirstProtocol4Type || Protocol >= 4) && (frame.Type < UiFrame.FirstProtocol5Type || Protocol >= 5)
            && (frame.Type < UiFrame.FirstProtocol6Type || Protocol >= 6) && (frame.Type < UiFrame.FirstProtocol7Type || Protocol >= 7)
            && (frame.Type < UiFrame.FirstProtocol8Type || Protocol >= 8) && (frame.Type < UiFrame.FirstProtocol10Type || Protocol >= 10);
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
        if (frame.Type >= UiFrame.FirstProtocol2Type && Protocol < 2 || frame.Type >= UiFrame.FirstProtocol3Type && Protocol < 3
            || frame.Type >= UiFrame.FirstProtocol4Type && Protocol < 4 || frame.Type >= UiFrame.FirstProtocol5Type && Protocol < 5
            || frame.Type >= UiFrame.FirstProtocol6Type && Protocol < 6 || frame.Type >= UiFrame.FirstProtocol7Type && Protocol < 7
            || frame.Type >= UiFrame.FirstProtocol8Type && Protocol < 8 || frame.Type >= UiFrame.FirstProtocol10Type && Protocol < 10)
        {
            throw new ProtocolException($"protocol {Protocol} overlay sent type 0x{frame.Type:x2}");
        }
        return frame switch
        {
            UiFrame.UiHello => throw new ProtocolException("second UiHello"),
            UiFrame.FriendPickerAsk => new UiIncoming.PickerAsk(),
            UiFrame.InviteSend s => new UiIncoming.InviteSend(s),
            UiFrame.JoinModesSet m => new UiIncoming.JoinModesSet(m),
            UiFrame.FriendsAction f => new UiIncoming.FriendsAct(f),
            UiFrame.InviteAnswer a => new UiIncoming.InviteAnswer(a),
            UiFrame.PlayerStateQuery q => new UiIncoming.PlayerStateAsk(q),
            UiFrame.PlayerActionRequest a => new UiIncoming.PlayerAct(a),
            UiFrame.VoiceCommand v => new UiIncoming.Voice(v),
            UiFrame.VoiceDevice d => new UiIncoming.VoiceDevice(d),
            UiFrame.PlayerDetailsQuery p => new UiIncoming.PlayerDetails(p),
            UiFrame.PaneActionClick c => new UiIncoming.PaneClick(c),
            UiFrame.UiEvent e => new UiIncoming.Event(e.Kind, e.Detail),
            UiFrame.Panel p => new UiIncoming.Panel(p.Open),
            UiFrame.Bye b => new UiIncoming.Bye(b.Reason),
            UiFrame.MyWorldQuery => new UiIncoming.Mine(),
            UiFrame.WorldAction a => new UiIncoming.Act(a),
            _ => throw new ProtocolException("direction checked above"),
        };
    }
}
