// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the UI pipe's protocol 2 frames (OD-112) and the shapes they carry. Header
// only, no Windows or ImGui, so the codec is tested on its own; pipe.cpp sends and receives them.
// The launcher's Infinity.Core/Overlay/UiFrame.cs is the reference codec: same envelope (u32 LE
// length of type + body, u8 type), little-endian fields, strings u16 LE length + UTF-8, flags 0/1.
//
// Overlay -> launcher:
//   0x17 PlayerDetailsQuery{str xuid (decimal), str gamertag (may be empty)}
//   0x18 PaneActionClick{str paneId, str actionId} (enabled actions only)
// Launcher -> overlay:
//   0x16 DetailPane{str paneId (empty closes), str title, str subtitle, str avatarKey, u8 sections
//        <= 8 x {u8 kind (0 key/value rows, 1 items), str heading, u8 entries <= 32 x {str primary,
//        str secondary, u8 icon}}, u8 actions <= 4 x {str id, str label, u8 enabled}}
//   0x19 ServerStatus{str server (host, <= 128 B), u8 flags, u16 online, u16 capacity, u16 hardcore,
//        u16 noob, u16 lobbies, u32 profilesHardcore, u32 profilesNoob} (CR-L40 §1.3): flags bit 0
//        known (else "Status unavailable"), 1 capacity, 2 live mode and lobby figures, 3 profiles
//   0x1A Avatar{u16 width, u16 height, width * height * 4 bytes of straight-alpha RGBA}: square,
//        <= 128 (the launcher sends 96: 128 x 128 cannot fit the 64 KiB frame); {0, 0} clears it
// Protocol 1 frames with a CR-L43 (OD-141) field, also coded here so the golden vectors cover them:
//   0x0E MyWorld{u8 role, str worldId, str name, str host, str code, u8 audience, u8 players, u8 max, u8 named,
//        u8 count <= 16 x {str xuid, str gamertag, u8 tier, u8 role, u8 friend}, u8 anyMode}; an older launcher's
//        frame ends after the members (anyMode 0), and as before trailing bytes are ignored
// Protocol 3 (OD-154, docs/launcher/VOICE_CHAT.md §11): voice chat
//   0x1B VoiceState, launcher -> overlay {u8 flags, u8 micMode (0 open, 1 muted, 2 push-to-talk),
//        u8 count <= 16 x {str xuid (decimal), str gamertag (<= 64 B), u8 state, u8 volume (1..100)}, u8 voiceVolume
//        (1..100), u8 micVolume (1..100), str microphoneId, str microphoneName, str outputId, str outputName (each
//        <= 256 B; an empty id is the system default, and its name is empty too)}, the other players of the lobby
//        (never this one: its own voice is micMode and flag 3): flags bit 0 the lobby's voice chat is on, 1 it is known
//        (else "set by the host"), 2 this player may change it (the host), 3 voice is on in this player's settings,
//        4 the lobby's mode is proximity (with bit 0; CR-L46), 5 this player's own voice is blocked (CR-S67/S68);
//        state bit 0 muted by this player, 1 talking now, 2 muted for the lobby by the host, 3 voice blocked
//   0x1C VoiceCommand, overlay -> launcher {u8 op, str xuid (decimal, empty for ops 1, 3, 4, 5, 7, 8), u8 value}:
//        op 1 the lobby's voice mode (the host; value 0 off, 1 open, 2 proximity), 2 mute a player (1) or unmute (0), 3 mute (1) or unmute
//        (0) everyone, 4 the mic mode (value as micMode), 5 voice on/off in the settings, 6 mute a player for the
//        whole lobby (ops 1 and 6 are retired: players host no lobby, OD-208), 7 the voice chat volume, 8 this player's microphone volume, 9 a player's volume
//        (ops 7-9: value 1..100, VOICE_CHAT.md §15)
//   0x1D VoiceDevice, overlay -> launcher {u8 flow (0 microphone, 1 output), str id, str name} (each <= 256 B; an empty
//        id is the system default and comes with an empty name): the device the player picked (§15)
// Protocol 4 (OD-181, CR-L48): players may mix mods; every player shows the mods it declared
//   0x1E RosterMods, launcher -> overlay {str worldId (<= 64 B; empty: no world, nobody listed), u8 count <= 16 x {str xuid
//        (decimal), u8 mods 1..32 x {str name (<= 64 B: the catalog's name, else the mod's id), str version (<= 16 B)}}}, sent
//        after every 0x0E MyWorld: only the players with mods (anyone else uses none).
// Protocol 5 (OD-193 F-2): the game's own PLAYER ACTIONS popup gets ADD / REMOVE / ACCEPT / CANCEL FRIEND and REPORT PLAYER (the
// hook adds the items and asks through this overlay's fi_overlay_player_* exports; the launcher holds the friend list and the calls)
//   0x1F PlayerStateQuery, overlay -> launcher {str xuid (decimal)}
//   0x20 PlayerState, launcher -> overlay {str xuid, u8 state (0 unknown, 1 none: ADD, 2 friend: REMOVE, 3 incoming: ACCEPT,
//        4 outgoing: CANCEL, 5 own profile: no item)}
//   0x21 PlayerActionRequest, overlay -> launcher {u32 id (non-zero), str xuid, u8 kind (1 add, 2 remove, 3 accept, 4 cancel,
//        5 report), u8 reason (report: 1 cheating, 2 abuse, 3 other; else 0)}
//   0x22 PlayerActionDone, launcher -> overlay {u32 id, str xuid, u8 result (0 none, 1 report submitted, 2 report failed, 3 try
//        later, 4 friend request sent, 5 friends now, 6 friend removed, 7 request cancelled, 8 player not found, 9 friend limit,
//        10 failed), u8 state (as 0x20)}
// Protocol 6 (OD-204, T-L3, WORLDS_OFFICIAL §12.5): the private lobby's invite prompt, a modal (never a toast: toasts follow OD-192)
//   0x23 InvitePrompt, launcher -> overlay {str inviteId (1..64 B of [A-Za-z0-9:._-]), str fromGamertag (1..64 B), str activity
//        (<= 16 B: "freeroam", "roadtrip", or anything else shown as a lobby), u16 expiresInSec (1..3600; 0 withdraws the prompt of
//        that id: answered elsewhere, expired or cancelled)}
//   0x24 InviteAnswer, overlay -> launcher {str inviteId, u8 accept (0 decline, 1 accept)}; nothing is sent when a prompt expires
// Protocol 7 (OD-204, T-L4, WORLDS_OFFICIAL §12.5): the private lobby's friend picker
//   0x25 FriendPicker, launcher -> overlay {u8 flags (bit 0 open: show the picker now, with the guide; without it only refresh an
//        open one), u8 status (0 the list, 1 the friend list could not be read: no rows), u8 count <= 100 x {str xuid (decimal),
//        str gamertag (1..64 B), u8 state (0 available, 1 in this lobby, 2 invited: pending, 3 offline)}}; friends only
//   0x26 FriendPickerAsk, overlay -> launcher {}: the Private Lobby pane's Invite friends (the launcher answers 0x25 or a notice)
//   0x27 InviteSend, overlay -> launcher {str xuid}: an available friend chosen; the launcher answers one notice and a fresh 0x25
// Protocol 8 (OD-210, CR-S101 MATCHMAKING_API §2.11, W14): the private lobby's "Who can join"
//   0x28 LobbyJoinModes, launcher -> overlay {str worldId (<= 64 B; empty: no private lobby, modes 0 and offered 0), u8 modes (0 Everyone,
//        1 Hardcore only, 2 Noob only: WorldView.modes), u8 offered (bit n: choice n is offered; bit 0 always with a world; FM5: no
//        Hardcore)}, sent after every 0x0E MyWorld
//   0x29 JoinModesSet, overlay -> launcher {u8 modes}: the current leader's choice (PUT /mm/v1/worlds/{id}/modes); the launcher answers
//        one notice and a fresh 0x28. New joins only: players already in the lobby stay
// Protocol 10 (OD-211, CR-S102 CORE_API §4.2a, W15): the right FRIENDS sidebar
//   0x2A FriendsState, launcher -> overlay {u8 flags (bit 0 loaded: the list was read, else "could not load"; bit 1 have lobby: this
//        player leads a private lobby, so Invite to my game is offered), u8 count <= 64 x {str id (PaneKey, the account), str name (1..64 B),
//        u16 flags (bit 0 online, 1 favourite, 2 supporter, 3 can ask, 4 can invite, 5 excluded, 6 racing, 7 private), str game (<= 8 B,
//        "FH2"), u8 activity (0 offline, 1 menus, 2 free roam, 3 car meet, 4 road trip, 5 race), u8 excluded (0 none, 1 mode, 2 other; set
//        exactly when flag 5), str discordUser, str discordName (each <= 64 B; empty: none linked), u32 accountDays (0xFFFFFFFF unknown),
//        str profileUrl (<= 160 B, https or empty), u8 profiles <= 4 x {str name, u8 mode (0 hardcore, 1 noob), str game}},
//        u8 requests <= 16 x {str id, str name} (incoming friend requests), u8 joins <= 8 x {str id, str name} (requests to join this
//        player's lobby)}
//   0x2B FriendsAction, overlay -> launcher {u8 kind (1 refresh, 2 add, 3 accept, 4 decline, 5 unfriend, 6 ask to join, 7 invite to my
//        game, 8 accept join request, 9 decline join request), str id (PaneKey; empty for 1 and 2), str text (<= 64 B; the profile name
//        for 2, else empty)}
//   0x2C FriendsResult, launcher -> overlay {u8 kind (echoed), u8 result (0 done, 1 requested, 2 now friends, 3 not found, 4 refused,
//        5 failed), str message (<= 160 B, in the player's language)}
// Online toasts ("{gamertag} is online · {game}") are ordinary 0x03 Toasts with the friend icon (6).
// Protocol 11 (OD-211, CR-S102): a friend's Discord avatar
//   0x2D FriendAvatar, launcher -> overlay {str id (PaneKey: a 0x2A friend row's id), u16 side (96, or 0: none), side * side * 4
//        bytes of straight-alpha RGBA}: the launcher fetched it from cdn.discordapp.com only and decoded it; the overlay draws it in
//        the friend's disc (the initials until one arrives) and never fetches anything
// Protocol 12 (owner 2026-11-08, INVITE_UX): inviting is the FRIENDS sidebar, never a centred picker
//   0x2E FriendsOpen, launcher -> overlay {u8 mode (0 the list, 1 invite: each friend row gets an Invite action)}: opens the guide with
//        the FRIENDS sidebar shown and focused. The launcher sends it for the game's Invite friends and the Private Lobby pane's 0x26,
//        right after a fresh 0x2A; to an older overlay it still sends 0x25 (and this overlay takes a 0x25 with the open flag as mode 1).
//   0x2A FriendsState's row flags gain bit 8 in this lobby and bit 9 invited (this launcher's invite into it is pending); can invite
//        (bit 4) is never set with either. A protocol 10-11 overlay never gets them (the launcher masks them off).
// Additive fields of older frames (an older reader ignores them):
//   0x03 Toast ends with u8 flags (bit 0 always: shown even in an official lobby, the T-L3 notice rule), written only when not 0
//   0x0E MyWorld ends with str leader (the lobby leader's decimal XUID) after the private flag, written only when private and known
// Protocol 9 (OD-208): players host no world. The lobby list, join by code, create and hosted frames (0x07-0x0C, 0x14, 0x15),
// WorldAction 1/3 and VoiceOp 1/6 are gone from the pipe (either end ignores or refuses them as unknown); MyWorld's role 1
// (hosting) is retired, and its host, code and audience fields stay in the layout (the code always empty).
// A malformed frame leaves `ok` false and is ignored whole.
#pragma once

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

namespace fi {

constexpr uint16_t kUiProtocol = 12;
constexpr size_t kMaxLobbyMods = 32; // OD-181: a roster player's declared mods
constexpr size_t kMaxServerHost = 128, kMaxAvatarSide = 128;
constexpr size_t kMaxPaneSections = 8, kMaxPaneEntries = 32, kMaxPaneActions = 4;
// UiFrame.cs' text limits in UTF-8 bytes: the launcher refuses anything longer, so neither side sends it
constexpr size_t kMaxGamertag = 64, kMaxModId = 64, kMaxModVersion = 16, kMaxPaneText = 512, kMaxPaneId = 64;

// Strict UTF-8 (no overlong forms, surrogates or code points past U+10FFFF), as the launcher decodes.
inline bool Utf8(const std::string &s) {
    const auto *p = reinterpret_cast<const unsigned char *>(s.data());
    for (size_t i = 0, n = s.size(); i < n;) {
        const unsigned c = p[i];
        size_t len = c < 0x80 ? 1 : (c >> 5) == 0x6 ? 2 : (c >> 4) == 0xE ? 3 : (c >> 3) == 0x1E ? 4 : 0;
        if (!len || i + len > n) return false;
        unsigned cp = len == 1 ? c : len == 2 ? c & 0x1F : len == 3 ? c & 0x0F : c & 0x07;
        for (size_t k = 1; k < len; ++k) {
            if ((p[i + k] & 0xC0) != 0x80) return false;
            cp = (cp << 6) | (p[i + k] & 0x3F);
        }
        if ((len == 2 && cp < 0x80) || (len == 3 && cp < 0x800) || (len == 4 && cp < 0x10000) || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF))
            return false;
        i += len;
    }
    return true;
}

// At most `max` bytes of valid UTF-8, cut on a code point; empty when `s` is not valid UTF-8.
inline std::string Utf8Cut(const std::string &s, size_t max) {
    if (!Utf8(s)) return {};
    if (s.size() <= max) return s;
    size_t n = max;
    while (n > 0 && (static_cast<unsigned char>(s[n]) & 0xC0) == 0x80) --n;
    return s.substr(0, n);
}

// A pane or action id: [A-Za-z0-9:._-]{1,64} (UiFrame.PaneKey).
inline bool PaneKey(const std::string &id) {
    if (id.empty() || id.size() > kMaxPaneId) return false;
    for (char c : id)
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == ':' || c == '.' || c == '_' || c == '-')) return false;
    return true;
}

enum class PaneIcon : uint8_t { None, Player, Host, Supporter, Mod, Warning, Check };
constexpr uint8_t kPaneIconMax = static_cast<uint8_t>(PaneIcon::Check);

struct LobbyMod {
    std::string id, version;
};

struct PaneEntry {
    std::string primary, secondary;
    PaneIcon icon = PaneIcon::None;
};
struct PaneSection {
    uint8_t kind = 0; // 0 key/value rows, 1 items
    std::string heading;
    std::vector<PaneEntry> entries;
};
struct PaneAction {
    std::string id, label;
    bool enabled = false;
};
// The reusable right pane: lobby details, player details, and whatever comes next.
struct DetailPane {
    std::string id; // "lobby:<world>", "player:<xuid>"; empty: none
    std::string title, subtitle;
    std::string avatar; // empty: the title's initials; "icon:world" a built-in glyph
    std::vector<PaneSection> sections;
    std::vector<PaneAction> actions;
};

// "My world" (OD-63): the world this profile holds a seat in, and its players. The XUID
// is the key of an action, never shown (OD-61).
struct MemberRow {
    std::string xuid, gamertag;
    uint8_t tier = 0;         // 0 Infinity Racer, 1 Infinity Supporter (the badge only, CR-S69)
    uint8_t role = 0;         // 0 host, 1 in the world, 2 on the way (seat reserved)
    uint8_t friend_state = 0; // 0 none, 1 friends, 2 request sent, 3 asked you, 4 you
};
struct MyWorld {
    uint8_t role = 0; // 0 none, 2 seat reserved, 3 seated (1, hosting, is retired: OD-208)
    std::string world, name, host, code;
    uint8_t audience = 0, players = 0, max = 0;
    bool named = false; // false: the server gave no roster (only the host is known)
    std::vector<MemberRow> members;
    bool any_mode = false; // OD-141: open to Hardcore and Noob (the header's badge)
    // OD-160 (after any_mode, only when known): the connection in use (0 unknown, 1 the official relay, 2 direct to the host) and
    // the lobby notice, the launcher's own localized text shown as given. An older launcher ends after any_mode.
    uint8_t transport = 0;
    std::string notice;
    // OD-192 (after notice, written only when true; then the transport is written even when unknown): the world is an official
    // one. The guide's lobby tab becomes the Official Lobby pane, with no lobby options, and no overlay notice shows.
    bool official = false;
    // OD-204 (T-L6, after official, written only when true and only with official): a private official world, created in the
    // game's own menu: the guide's Private Lobby pane (T-L3).
    bool is_private = false;
    // OD-204 (T-L3, after the private flag, written only when private and known): the lobby leader's decimal XUID (WORLDS_OFFICIAL
    // §12.8b; the server reports it, the launcher passes it on). Empty: unknown, so nobody may invite from this overlay.
    std::string leader;
};
constexpr uint8_t kTransportUnknown = 0, kTransportRelay = 1, kTransportDirect = 2;
constexpr size_t kMaxMineMembers = 16;

// 0x1E (protocol 4, OD-181): the mods each player of "my world" declared, by XUID (the action key, never shown).
struct RosterModsMember {
    std::string xuid;
    std::vector<LobbyMod> mods; // 1..32, as shown: name (or id) and version
};
struct RosterMods {
    std::string world; // the MyWorld it belongs to; empty: none
    std::vector<RosterModsMember> members;
    const std::vector<LobbyMod> *Of(const std::string &world_id, const std::string &xuid) const {
        if (world.empty() || world != world_id) return nullptr;
        for (const auto &m : members)
            if (m.xuid == xuid) return &m.mods;
        return nullptr;
    }
};
constexpr size_t kMaxRosterMembers = 16;

namespace wire {

inline void U8(std::vector<uint8_t> &b, uint8_t v) { b.push_back(v); }
inline void U16(std::vector<uint8_t> &b, uint16_t v) { b.push_back(v & 0xFF); b.push_back(v >> 8); }
inline void U32(std::vector<uint8_t> &b, uint32_t v) { for (int i = 0; i < 4; ++i) b.push_back((v >> (8 * i)) & 0xFF); }
inline void Flag(std::vector<uint8_t> &b, bool v) { b.push_back(v ? 1 : 0); }
inline void Str(std::vector<uint8_t> &b, const std::string &s) {
    const size_t n = s.size() > 0xFFFF ? 0xFFFF : s.size();
    U16(b, static_cast<uint16_t>(n));
    b.insert(b.end(), s.begin(), s.begin() + n);
}
// The whole frame: length, type, body.
inline std::vector<uint8_t> Frame(uint8_t type, const std::vector<uint8_t> &body) {
    std::vector<uint8_t> frame;
    U32(frame, static_cast<uint32_t>(1 + body.size()));
    frame.push_back(type);
    frame.insert(frame.end(), body.begin(), body.end());
    return frame;
}

struct Reader {
    const uint8_t *p;
    size_t left;
    bool ok = true;
    uint8_t U8() { if (left < 1) { ok = false; return 0; } --left; return *p++; }
    uint16_t U16() { const uint16_t lo = U8(); return static_cast<uint16_t>(lo | (U8() << 8)); }
    uint32_t U32() { const uint32_t lo = U16(); return lo | (static_cast<uint32_t>(U16()) << 16); }
    bool Flag() { const uint8_t v = U8(); if (v > 1) ok = false; return v == 1; }
    size_t Count(size_t max) { const uint8_t n = U8(); if (n > max) ok = false; return ok ? n : 0; }
    std::string Str() {
        const uint16_t n = U16();
        if (!ok || left < n) { ok = false; return {}; }
        std::string s(reinterpret_cast<const char *>(p), n);
        p += n;
        left -= n;
        return s;
    }
    // A protocol 2 string: strict UTF-8 and at most `max` bytes (the launcher's own rules)
    std::string Text(size_t max = 0xFFFF) {
        std::string s = Str();
        if (ok && (s.size() > max || !Utf8(s))) ok = false;
        return s;
    }
    bool Done() const { return ok && left == 0; }
};

} // namespace wire

// ---------------------------------------------------------------- overlay -> launcher bodies
// The game's gamertag is untrusted text: cut to the launcher's 512 bytes on a code point, or dropped
// when it is not UTF-8, so it can never be the frame that closes the pipe.
inline std::vector<uint8_t> EncodePlayerDetailsQuery(const std::string &xuid, const std::string &gamertag) {
    std::vector<uint8_t> b;
    wire::Str(b, xuid);
    wire::Str(b, Utf8Cut(gamertag, kMaxPaneText));
    return b;
}
inline std::vector<uint8_t> EncodePaneActionClick(const std::string &pane, const std::string &action) {
    std::vector<uint8_t> b;
    wire::Str(b, pane);
    wire::Str(b, action);
    return b;
}
// The lobby's voice mode on the pipe (CR-L46): VoiceState's lobby mode.
enum : uint8_t { kLobbyVoiceOff = 0, kLobbyVoiceOpen = 1, kLobbyVoiceProximity = 2, kLobbyVoiceMax = kLobbyVoiceProximity };

// ---------------------------------------------------------------- voice chat (protocol 3, OD-154)
enum class MicMode : uint8_t { Open, Muted, PushToTalk };
constexpr uint8_t kMicModeMax = static_cast<uint8_t>(MicMode::PushToTalk);
constexpr size_t kMaxVoiceMembers = 16, kMaxXuid = 20;
// VOICE_CHAT.md §15: the sliders (1..100, 100 the default) and a device's id and name (UTF-8 bytes)
constexpr uint8_t kVolumeMin = 1, kVolumeMax = 100;
constexpr size_t kMaxDeviceText = 256;
inline bool VolumeValue(uint8_t v) { return v >= kVolumeMin && v <= kVolumeMax; }

struct VoiceMember {
    enum : uint8_t { MutedByMe = 1, Talking = 2, MutedForLobby = 4, Blocked = 8, All = 15 };
    std::string xuid, gamertag; // the gamertag names a talker over the game (the HUD needs no roster)
    uint8_t state = 0;
    uint8_t volume = kVolumeMax; // this player's volume for that player (§15), kept by the launcher per XUID
};
struct VoiceState {
    enum : uint8_t { LobbyOn = 1, LobbyKnown = 2, MayChange = 4, Enabled = 8, Proximity = 16, SelfBlocked = 32, AllFlags = 63 };
    uint8_t flags = 0;
    MicMode mic = MicMode::Open;
    std::vector<VoiceMember> members;
    // §15: the player's own preferences, kept by the launcher per profile
    uint8_t voice_volume = kVolumeMax; // the mix of every other player
    uint8_t mic_volume = kVolumeMax;   // this player's captured voice
    std::string mic_id, mic_name;      // the chosen microphone; empty id: the system default
    std::string output_id, output_name;
    bool lobby_on() const { return (flags & LobbyKnown) && (flags & LobbyOn); }
    // the lobby's mode as the pipe says it (CR-L46): kLobbyVoiceOff, Open or Proximity (off while unknown)
    uint8_t lobby_mode() const { return !lobby_on() ? kLobbyVoiceOff : (flags & Proximity) ? kLobbyVoiceProximity : kLobbyVoiceOpen; }
    uint8_t StateOf(const std::string &xuid) const {
        for (const auto &m : members)
            if (m.xuid == xuid) return m.state;
        return 0;
    }
    uint8_t VolumeOf(const std::string &xuid) const {
        for (const auto &m : members)
            if (m.xuid == xuid) return m.volume;
        return kVolumeMax;
    }
};

// 0x1D's flow
enum class VoiceFlow : uint8_t { Microphone, Output };
constexpr uint8_t kVoiceFlowMax = static_cast<uint8_t>(VoiceFlow::Output);

// fi_overlay_voice_state's answer (fi_overlay.h FI_VOICE_*) for the game's own UI: `xuid` empty is this player.
constexpr uint32_t kGameVoiceAble = 1, kGameVoiceTalking = 2, kGameVoiceMuted = 4;
inline uint32_t GameVoiceBits(const VoiceState &v, const std::string &xuid) {
    if (!v.lobby_on() || !(v.flags & VoiceState::Enabled)) return 0; // no microphones in the game's UI at all
    if (xuid.empty()) return v.mic == MicMode::Muted ? 0 : kGameVoiceAble;
    for (const auto &m : v.members) {
        if (m.xuid != xuid) continue;
        const bool muted = m.state & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked);
        return kGameVoiceAble | (muted ? kGameVoiceMuted : 0) | (!muted && (m.state & VoiceMember::Talking) ? kGameVoiceTalking : 0);
    }
    return 0;
}

enum class VoiceOp : uint8_t {
    LobbyVoice = 1, Mute = 2, MuteEveryone = 3, Mic = 4, Enabled = 5, MuteForLobby = 6, VoiceVolume = 7, MicVolume = 8, PlayerVolume = 9
};
constexpr uint8_t kVoiceOpMax = static_cast<uint8_t>(VoiceOp::PlayerVolume);

// A decimal XUID, 1 to 20 ASCII digits (the launcher's rule).
inline bool XuidText(const std::string &xuid) {
    if (xuid.empty() || xuid.size() > kMaxXuid) return false;
    for (char c : xuid)
        if (c < '0' || c > '9') return false;
    return true;
}

// Ops 2, 6 and 9 name a player; the others never do.
inline bool VoiceOpNamesPlayer(VoiceOp op) { return op == VoiceOp::Mute || op == VoiceOp::MuteForLobby || op == VoiceOp::PlayerVolume; }

inline std::vector<uint8_t> EncodeVoiceCommand(VoiceOp op, const std::string &xuid, uint8_t value) {
    std::vector<uint8_t> b;
    wire::U8(b, static_cast<uint8_t>(op));
    wire::Str(b, VoiceOpNamesPlayer(op) ? xuid : std::string());
    wire::U8(b, value);
    return b;
}

inline bool DecodeVoiceState(const uint8_t *body, size_t size, VoiceState &out) {
    wire::Reader r{body, size};
    VoiceState v;
    v.flags = r.U8();
    const uint8_t mic = r.U8();
    if (v.flags > VoiceState::AllFlags || mic > kMicModeMax) r.ok = false;
    v.mic = static_cast<MicMode>(mic);
    const size_t count = r.Count(kMaxVoiceMembers);
    for (size_t i = 0; i < count && r.ok; ++i) {
        VoiceMember m;
        m.xuid = r.Text(kMaxXuid);
        m.gamertag = r.Text(kMaxGamertag);
        m.state = r.U8();
        m.volume = r.U8();
        if (!XuidText(m.xuid) || m.state > VoiceMember::All || !VolumeValue(m.volume)) r.ok = false;
        v.members.push_back(std::move(m));
    }
    v.voice_volume = r.U8();
    v.mic_volume = r.U8();
    v.mic_id = r.Text(kMaxDeviceText);
    v.mic_name = r.Text(kMaxDeviceText);
    v.output_id = r.Text(kMaxDeviceText);
    v.output_name = r.Text(kMaxDeviceText);
    if (!VolumeValue(v.voice_volume) || !VolumeValue(v.mic_volume) || (v.mic_id.empty() && !v.mic_name.empty())
        || (v.output_id.empty() && !v.output_name.empty()))
        r.ok = false;
    if (!r.Done()) return false;
    out = std::move(v);
    return true;
}

inline std::vector<uint8_t> EncodeVoiceState(const VoiceState &v) {
    std::vector<uint8_t> b;
    wire::U8(b, v.flags);
    wire::U8(b, static_cast<uint8_t>(v.mic));
    wire::U8(b, static_cast<uint8_t>(v.members.size()));
    for (const auto &m : v.members) {
        wire::Str(b, m.xuid);
        wire::Str(b, m.gamertag);
        wire::U8(b, m.state);
        wire::U8(b, m.volume);
    }
    wire::U8(b, v.voice_volume);
    wire::U8(b, v.mic_volume);
    wire::Str(b, v.mic_id);
    wire::Str(b, v.mic_name);
    wire::Str(b, v.output_id);
    wire::Str(b, v.output_name);
    return b;
}

// 0x1D (§15): the device the player picked. The name is cut to 256 bytes on a code point; an id past 256 bytes (or not
// UTF-8) cannot be kept, so the caller never offers it. An empty id (the system default) sends no name.
inline std::vector<uint8_t> EncodeVoiceDevice(VoiceFlow flow, const std::string &id, const std::string &name) {
    std::vector<uint8_t> b;
    wire::U8(b, static_cast<uint8_t>(flow));
    wire::Str(b, id);
    wire::Str(b, id.empty() ? std::string() : Utf8Cut(name, kMaxDeviceText));
    return b;
}
inline bool DeviceIdSendable(const std::string &id) { return id.size() <= kMaxDeviceText && Utf8(id); }

// 0x0E (protocol 1, read as leniently as before: trailing bytes ignored; anyMode CR-L43, absent from an older launcher)
inline bool DecodeMyWorld(const uint8_t *body, size_t size, MyWorld &out) {
    wire::Reader r{body, size};
    MyWorld m;
    m.role = r.U8();
    m.world = r.Str();
    m.name = r.Str();
    m.host = r.Str();
    m.code = r.Str();
    m.audience = r.U8();
    m.players = r.U8();
    m.max = r.U8();
    m.named = r.U8() != 0;
    const uint8_t count = r.U8();
    for (uint8_t i = 0; i < count && r.ok; ++i) {
        MemberRow p;
        p.xuid = r.Str();
        p.gamertag = r.Str();
        p.tier = r.U8();
        p.role = r.U8();
        p.friend_state = r.U8();
        if (m.members.size() < kMaxMineMembers) m.members.push_back(std::move(p));
    }
    if (r.ok && r.left > 0) m.any_mode = r.Flag();
    if (r.ok && r.left > 0) { // OD-160
        m.transport = r.U8();
        m.notice = r.Text(kMaxPaneText);
        if (r.ok && r.left > 0) m.official = r.Flag(); // OD-192
        if (r.ok && r.left > 0) m.is_private = r.Flag(); // OD-204 (T-L6)
        if (m.is_private && !m.official) r.ok = false; // a private world is an official one
        if (r.ok && m.is_private && r.left > 0) { // OD-204 (T-L3): the leader, only when known
            m.leader = r.Text(kMaxXuid);
            if (r.ok && !XuidText(m.leader)) r.ok = false;
        }
        // an unknown transport is written only ahead of the official flag
        if ((m.transport == kTransportUnknown && !m.official) || m.transport > kTransportDirect) r.ok = false;
    }
    if (!r.ok) return false;
    out = std::move(m);
    return true;
}

inline std::vector<uint8_t> EncodeMyWorld(const MyWorld &m) {
    std::vector<uint8_t> b;
    wire::U8(b, m.role);
    wire::Str(b, m.world);
    wire::Str(b, m.name);
    wire::Str(b, m.host);
    wire::Str(b, m.code);
    wire::U8(b, m.audience);
    wire::U8(b, m.players);
    wire::U8(b, m.max);
    wire::Flag(b, m.named);
    wire::U8(b, static_cast<uint8_t>(m.members.size()));
    for (const auto &p : m.members) {
        wire::Str(b, p.xuid);
        wire::Str(b, p.gamertag);
        wire::U8(b, p.tier);
        wire::U8(b, p.role);
        wire::U8(b, p.friend_state);
    }
    wire::Flag(b, m.any_mode);
    if (m.transport != kTransportUnknown || m.official) { // OD-160: only when known (the 2.0.0 layout otherwise)
        wire::U8(b, m.transport);
        wire::Str(b, m.notice);
        if (m.official) wire::Flag(b, true); // OD-192
        if (m.official && m.is_private) wire::Flag(b, true); // OD-204 (T-L6)
        if (m.official && m.is_private && !m.leader.empty()) wire::Str(b, m.leader); // OD-204 (T-L3)
    }
    return b;
}

// ---------------------------------------------------------------- 0x03 Toast and its T-L3 flags
// The toast's look is its kind (`icon`, ToastKind's order); `level` is read and not used. Bit 0 of the additive flags: the notice
// always shows, even in an official lobby (a private create's result, A10's flagged Hardcore message); every other notice follows
// OD-192's quiet rule. A launcher before T-L3 writes no flags byte (0).
constexpr uint8_t kToastAlways = 1, kToastFlagsAll = kToastAlways;
struct ToastFrame {
    uint8_t level = 0, icon = 0;
    uint16_t ttl = 0; // tenths of a second; 0 the overlay's own default
    std::string text; // "Title\nBody"
    uint8_t flags = 0;
    bool always() const { return (flags & kToastAlways) != 0; }
};
// As leniently as before (trailing bytes past the flags ignored); unknown flag bits are ignored too (a newer launcher's).
inline bool DecodeToast(const uint8_t *body, size_t size, ToastFrame &out) {
    wire::Reader r{body, size};
    ToastFrame t;
    t.level = r.U8();
    t.icon = r.U8();
    t.ttl = r.U16();
    t.text = r.Str();
    if (r.ok && r.left > 0) t.flags = static_cast<uint8_t>(r.U8() & kToastFlagsAll);
    if (!r.ok) return false;
    out = std::move(t);
    return true;
}
inline std::vector<uint8_t> EncodeToast(const ToastFrame &t) {
    std::vector<uint8_t> b;
    wire::U8(b, t.level);
    wire::U8(b, t.icon);
    wire::U16(b, t.ttl);
    wire::Str(b, t.text);
    if (t.flags != 0) wire::U8(b, t.flags);
    return b;
}

// ---------------------------------------------------------------- protocol 6: the invite prompt (OD-204, T-L3)
constexpr size_t kMaxInviteId = 64, kMaxInviteActivity = 16;
constexpr uint16_t kMaxInviteSeconds = 3600;
struct InvitePromptFrame {
    std::string id;       // the invite's id, echoed by the answer
    std::string from;     // the inviting player's gamertag (shown; the XUID never is)
    std::string activity; // "freeroam", "roadtrip", or anything else (shown as a lobby)
    uint16_t expires = 0; // seconds left; 0 withdraws the prompt of `id`
};
// Strict, as the launcher writes it (UiFrame.InvitePrompt.Validate).
inline bool DecodeInvitePrompt(const uint8_t *body, size_t size, InvitePromptFrame &out) {
    wire::Reader r{body, size};
    InvitePromptFrame p;
    p.id = r.Text(kMaxInviteId);
    p.from = r.Text(kMaxGamertag);
    p.activity = r.Text(kMaxInviteActivity);
    p.expires = r.U16();
    if (r.ok && (!PaneKey(p.id) || (p.expires != 0 && p.from.empty()) || p.expires > kMaxInviteSeconds)) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(p);
    return true;
}
inline std::vector<uint8_t> EncodeInvitePrompt(const InvitePromptFrame &p) {
    std::vector<uint8_t> b;
    wire::Str(b, p.id);
    wire::Str(b, p.from);
    wire::Str(b, p.activity);
    wire::U16(b, p.expires);
    return b;
}
inline std::vector<uint8_t> EncodeInviteAnswer(const std::string &id, bool accept) {
    std::vector<uint8_t> b;
    wire::Str(b, id);
    wire::Flag(b, accept);
    return b;
}
struct InviteAnswerFrame {
    std::string id;
    bool accept = false;
};
inline bool DecodeInviteAnswer(const uint8_t *body, size_t size, InviteAnswerFrame &out) {
    wire::Reader r{body, size};
    InviteAnswerFrame a;
    a.id = r.Text(kMaxInviteId);
    a.accept = r.Flag();
    if (r.ok && !PaneKey(a.id)) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(a);
    return true;
}

// ---------------------------------------------------------------- protocol 7: the friend picker (OD-204, T-L4)
constexpr size_t kMaxPickerFriends = 100;
constexpr uint8_t kPickerOpen = 1, kPickerFlagsAll = kPickerOpen;
constexpr uint8_t kPickerList = 0, kPickerUnavailable = 1;
enum : uint8_t { kPickerAvailable = 0, kPickerInLobby = 1, kPickerInvited = 2, kPickerOffline = 3, kPickerStateMax = kPickerOffline };
struct PickerFriend {
    std::string xuid;     // the action key (decimal), never shown
    std::string gamertag; // shown
    uint8_t state = kPickerAvailable;
};
struct FriendPickerFrame {
    uint8_t flags = 0, status = kPickerList;
    std::vector<PickerFriend> friends;
    bool open() const { return (flags & kPickerOpen) != 0; }
};
inline bool PickerXuid(const std::string &xuid) {
    if (xuid.empty() || xuid.size() > 20) return false;
    for (char c : xuid)
        if (c < '0' || c > '9') return false;
    return true;
}
// Strict, as the launcher writes it (UiFrame.FriendPicker.Validate).
inline bool DecodeFriendPicker(const uint8_t *body, size_t size, FriendPickerFrame &out) {
    wire::Reader r{body, size};
    FriendPickerFrame p;
    p.flags = r.U8();
    p.status = r.U8();
    const size_t count = r.Count(kMaxPickerFriends);
    if (r.ok && (p.flags > kPickerFlagsAll || p.status > kPickerUnavailable || (p.status != kPickerList && count != 0))) r.ok = false;
    for (size_t i = 0; i < count && r.ok; ++i) {
        PickerFriend f;
        f.xuid = r.Text(20);
        f.gamertag = r.Text(kMaxGamertag);
        f.state = r.U8();
        if (r.ok && (!PickerXuid(f.xuid) || f.gamertag.empty() || f.state > kPickerStateMax)) r.ok = false;
        p.friends.push_back(std::move(f));
    }
    if (!r.Done()) return false;
    out = std::move(p);
    return true;
}
inline std::vector<uint8_t> EncodeFriendPicker(const FriendPickerFrame &p) {
    std::vector<uint8_t> b;
    wire::U8(b, p.flags);
    wire::U8(b, p.status);
    wire::U8(b, static_cast<uint8_t>(p.friends.size()));
    for (const auto &f : p.friends) {
        wire::Str(b, f.xuid);
        wire::Str(b, f.gamertag);
        wire::U8(b, f.state);
    }
    return b;
}
inline std::vector<uint8_t> EncodeInviteSend(const std::string &xuid) {
    std::vector<uint8_t> b;
    wire::Str(b, xuid);
    return b;
}
inline bool DecodeInviteSend(const uint8_t *body, size_t size, std::string &xuid) {
    wire::Reader r{body, size};
    std::string x = r.Text(20);
    if (r.ok && !PickerXuid(x)) r.ok = false;
    if (!r.Done()) return false;
    xuid = std::move(x);
    return true;
}

// ---------------------------------------------------------------- protocol 8: "Who can join" (OD-210, CR-S101, W14)
enum : uint8_t { kJoinEveryone = 0, kJoinHardcoreOnly = 1, kJoinNoobOnly = 2, kJoinModesMax = kJoinNoobOnly };
constexpr uint8_t kOfferEveryone = 1, kOfferHardcore = 2, kOfferNoob = 4, kOfferAll = kOfferEveryone | kOfferHardcore | kOfferNoob;
struct LobbyJoinModesFrame {
    std::string world; // the private lobby it belongs to; empty: none
    uint8_t modes = kJoinEveryone;
    uint8_t offered = 0;
    bool Offers(uint8_t choice) const { return choice <= kJoinModesMax && (offered & (1u << choice)) != 0; }
};
// Strict, as the launcher writes it (UiFrame.LobbyJoinModes.Validate).
inline bool DecodeLobbyJoinModes(const uint8_t *body, size_t size, LobbyJoinModesFrame &out) {
    wire::Reader r{body, size};
    LobbyJoinModesFrame f;
    f.world = r.Text(kMaxPaneId);
    f.modes = r.U8();
    f.offered = r.U8();
    if (r.ok && (f.modes > kJoinModesMax || f.offered > kOfferAll)) r.ok = false;
    if (r.ok && f.world.empty() && (f.modes != kJoinEveryone || f.offered != 0)) r.ok = false;
    if (r.ok && !f.world.empty() && (f.offered & kOfferEveryone) == 0) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(f);
    return true;
}
inline std::vector<uint8_t> EncodeLobbyJoinModes(const LobbyJoinModesFrame &f) {
    std::vector<uint8_t> b;
    wire::Str(b, f.world);
    wire::U8(b, f.modes);
    wire::U8(b, f.offered);
    return b;
}
inline std::vector<uint8_t> EncodeJoinModesSet(uint8_t modes) {
    std::vector<uint8_t> b;
    wire::U8(b, modes > kJoinModesMax ? kJoinEveryone : modes);
    return b;
}
inline bool DecodeJoinModesSet(const uint8_t *body, size_t size, uint8_t &modes) {
    wire::Reader r{body, size};
    const uint8_t m = r.U8();
    if (r.ok && m > kJoinModesMax) r.ok = false;
    if (!r.Done()) return false;
    modes = m;
    return true;
}

// ---------------------------------------------------------------- protocol 10: the friends sidebar (OD-211, CR-S102, W15)
constexpr size_t kMaxFriendRows = 64, kMaxFriendRequests = 16, kMaxJoinRequests = 8, kMaxFriendProfiles = 4;
constexpr size_t kMaxFriendName = 64, kMaxFriendGame = 8, kMaxFriendUrl = 160, kMaxFriendMessage = 160;
constexpr uint32_t kFriendAgeUnknown = 0xFFFFFFFFu;
enum : uint8_t { kFriendsLoaded = 1, kFriendsHaveLobby = 2, kFriendsFlagsAll = 3 };
enum : uint16_t { kFriendOnline = 1, kFriendFavorite = 2, kFriendSupporter = 4, kFriendCanAsk = 8, kFriendCanInvite = 0x10, kFriendExcluded = 0x20,
                  kFriendRacing = 0x40, kFriendPrivate = 0x80, kFriendInLobby = 0x100, kFriendInvited = 0x200, kFriendFlagsAll = 0x3FF };
enum : uint8_t { kActOffline = 0, kActMenus = 1, kActFreeRoam = 2, kActCarMeet = 3, kActRoadTrip = 4, kActRace = 5, kActMax = kActRace };
enum : uint8_t { kExcludedNone = 0, kExcludedMode = 1, kExcludedOther = 2, kExcludedMax = kExcludedOther };
enum : uint8_t { kFriendsRefresh = 1, kFriendsAdd = 2, kFriendsAccept = 3, kFriendsDecline = 4, kFriendsUnfriend = 5, kFriendsAsk = 6,
                 kFriendsInviteMine = 7, kFriendsAcceptJoin = 8, kFriendsDeclineJoin = 9, kFriendsActionMax = kFriendsDeclineJoin };
enum : uint8_t { kResultDone = 0, kResultRequested = 1, kResultNowFriends = 2, kResultNotFound = 3, kResultRefused = 4, kResultFailed = 5,
                 kResultMax = kResultFailed };
struct FriendProfileRow {
    std::string name;
    uint8_t mode = 1; // 0 hardcore, 1 noob
    std::string game;
};
struct FriendRowFrame {
    std::string id, name;
    uint16_t flags = 0;
    std::string game;
    uint8_t activity = kActOffline, excluded = kExcludedNone;
    std::string discord_user, discord_name;
    uint32_t account_days = kFriendAgeUnknown;
    std::string profile_url;
    std::vector<FriendProfileRow> profiles;
    bool Has(uint16_t flag) const { return (flags & flag) != 0; }
};
struct FriendRequestRow {
    std::string id, name;
};
struct FriendsStateFrame {
    uint8_t flags = 0;
    std::vector<FriendRowFrame> friends;
    std::vector<FriendRequestRow> requests, joins;
    bool loaded() const { return (flags & kFriendsLoaded) != 0; }
    bool have_lobby() const { return (flags & kFriendsHaveLobby) != 0; }
};
struct FriendsResultFrame {
    uint8_t kind = 0, result = kResultDone;
    std::string message;
};
inline bool DecodeFriendRequests(wire::Reader &r, size_t max, std::vector<FriendRequestRow> &out) {
    const size_t count = r.Count(max);
    for (size_t i = 0; i < count && r.ok; ++i) {
        FriendRequestRow q;
        q.id = r.Text(kMaxPaneId);
        q.name = r.Text(kMaxFriendName);
        if (r.ok && (!PaneKey(q.id) || q.name.empty())) r.ok = false;
        out.push_back(std::move(q));
    }
    return r.ok;
}
// Strict, as the launcher writes it (UiFrame.FriendsState.Validate)
inline bool DecodeFriendsState(const uint8_t *body, size_t size, FriendsStateFrame &out) {
    wire::Reader r{body, size};
    FriendsStateFrame f;
    f.flags = r.U8();
    const size_t count = r.Count(kMaxFriendRows);
    if (r.ok && f.flags > kFriendsFlagsAll) r.ok = false;
    for (size_t i = 0; i < count && r.ok; ++i) {
        FriendRowFrame row;
        row.id = r.Text(kMaxPaneId);
        row.name = r.Text(kMaxFriendName);
        row.flags = r.U16();
        row.game = r.Text(kMaxFriendGame);
        row.activity = r.U8();
        row.excluded = r.U8();
        row.discord_user = r.Text(kMaxFriendName);
        row.discord_name = r.Text(kMaxFriendName);
        row.account_days = r.U32();
        row.profile_url = r.Text(kMaxFriendUrl);
        if (r.ok && (!PaneKey(row.id) || row.name.empty() || row.flags > kFriendFlagsAll || row.activity > kActMax || row.excluded > kExcludedMax ||
                     (row.Has(kFriendExcluded) != (row.excluded != kExcludedNone)) ||
                     (row.Has(kFriendCanInvite) && (row.flags & (kFriendInLobby | kFriendInvited)) != 0) ||
                     (!row.profile_url.empty() && row.profile_url.rfind("https://", 0) != 0)))
            r.ok = false;
        const size_t profiles = r.Count(kMaxFriendProfiles);
        for (size_t p = 0; p < profiles && r.ok; ++p) {
            FriendProfileRow pr;
            pr.name = r.Text(kMaxFriendName);
            pr.mode = r.U8();
            pr.game = r.Text(kMaxFriendGame);
            if (r.ok && (pr.name.empty() || pr.mode > 1)) r.ok = false;
            row.profiles.push_back(std::move(pr));
        }
        f.friends.push_back(std::move(row));
    }
    DecodeFriendRequests(r, kMaxFriendRequests, f.requests);
    DecodeFriendRequests(r, kMaxJoinRequests, f.joins);
    if (r.ok && !f.loaded() && (!f.friends.empty() || !f.requests.empty())) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(f);
    return true;
}
inline std::vector<uint8_t> EncodeFriendsState(const FriendsStateFrame &f) {
    std::vector<uint8_t> b;
    wire::U8(b, f.flags);
    wire::U8(b, static_cast<uint8_t>(f.friends.size()));
    for (const auto &row : f.friends) {
        wire::Str(b, row.id);
        wire::Str(b, row.name);
        wire::U16(b, row.flags);
        wire::Str(b, row.game);
        wire::U8(b, row.activity);
        wire::U8(b, row.excluded);
        wire::Str(b, row.discord_user);
        wire::Str(b, row.discord_name);
        wire::U32(b, row.account_days);
        wire::Str(b, row.profile_url);
        wire::U8(b, static_cast<uint8_t>(row.profiles.size()));
        for (const auto &p : row.profiles) {
            wire::Str(b, p.name);
            wire::U8(b, p.mode);
            wire::Str(b, p.game);
        }
    }
    for (const auto *list : {&f.requests, &f.joins}) {
        wire::U8(b, static_cast<uint8_t>(list->size()));
        for (const auto &q : *list) {
            wire::Str(b, q.id);
            wire::Str(b, q.name);
        }
    }
    return b;
}
inline bool DecodeFriendsResult(const uint8_t *body, size_t size, FriendsResultFrame &out) {
    wire::Reader r{body, size};
    FriendsResultFrame f;
    f.kind = r.U8();
    f.result = r.U8();
    f.message = r.Text(kMaxFriendMessage);
    if (r.ok && (f.kind < kFriendsRefresh || f.kind > kFriendsActionMax || f.result > kResultMax)) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(f);
    return true;
}
inline std::vector<uint8_t> EncodeFriendsResult(const FriendsResultFrame &f) {
    std::vector<uint8_t> b;
    wire::U8(b, f.kind);
    wire::U8(b, f.result);
    wire::Str(b, f.message);
    return b;
}
// Overlay -> launcher; the same rule the launcher checks (UiFrame.FriendsAction.Validate): Refresh carries nothing, Add a name, the rest an id.
inline bool FriendsActionValid(uint8_t kind, const std::string &id, const std::string &text) {
    if (kind < kFriendsRefresh || kind > kFriendsActionMax || text.size() > kMaxFriendName || !Utf8(text)) return false;
    if (kind == kFriendsAdd) return id.empty() && !text.empty();
    if (kind == kFriendsRefresh) return id.empty() && text.empty();
    return PaneKey(id) && text.empty();
}
inline std::vector<uint8_t> EncodeFriendsAction(uint8_t kind, const std::string &id, const std::string &text) {
    std::vector<uint8_t> b;
    wire::U8(b, kind);
    wire::Str(b, id);
    wire::Str(b, text);
    return b;
}
inline bool DecodeFriendsAction(const uint8_t *body, size_t size, uint8_t &kind, std::string &id, std::string &text) {
    wire::Reader r{body, size};
    const uint8_t k = r.U8();
    std::string i = r.Text(kMaxPaneId), t = r.Text(kMaxFriendName);
    if (r.ok && !FriendsActionValid(k, i, t)) r.ok = false;
    if (!r.Done()) return false;
    kind = k;
    id = std::move(i);
    text = std::move(t);
    return true;
}

// ---------------------------------------------------------------- protocol 11: a friend's Discord avatar (OD-211, CR-S102)
constexpr uint16_t kFriendAvatarSide = 96;

struct FriendAvatarFrame {
    std::string id;            // the friend row's id (PaneKey)
    uint16_t side = 0;         // 96, or 0: none (the initials)
    std::vector<uint8_t> rgba; // side * side * 4, straight alpha
};

// Strict, as the launcher writes it (UiFrame.FriendAvatar.Validate)
inline bool DecodeFriendAvatar(const uint8_t *body, size_t size, FriendAvatarFrame &out) {
    wire::Reader r{body, size};
    FriendAvatarFrame a;
    a.id = r.Text(kMaxPaneId);
    a.side = r.U16();
    if (!r.ok || !PaneKey(a.id) || (a.side != 0 && a.side != kFriendAvatarSide)) return false;
    const size_t bytes = static_cast<size_t>(a.side) * a.side * 4;
    if (r.left != bytes) return false;
    a.rgba.assign(r.p, r.p + bytes);
    out = std::move(a);
    return true;
}

inline std::vector<uint8_t> EncodeFriendAvatar(const FriendAvatarFrame &a) {
    std::vector<uint8_t> b;
    wire::Str(b, a.id);
    wire::U16(b, a.side);
    b.insert(b.end(), a.rgba.begin(), a.rgba.end());
    return b;
}

// ---------------------------------------------------------------- protocol 12: open the FRIENDS sidebar (INVITE_UX, 2026-11-08)
enum : uint8_t { kFriendsOpenList = 0, kFriendsOpenInvite = 1, kFriendsOpenMax = kFriendsOpenInvite };

// Strict, as the launcher writes it (UiFrame.FriendsOpen.Validate)
inline bool DecodeFriendsOpen(const uint8_t *body, size_t size, uint8_t &mode) {
    wire::Reader r{body, size};
    const uint8_t m = r.U8();
    if (r.ok && m > kFriendsOpenMax) r.ok = false;
    if (!r.Done()) return false;
    mode = m;
    return true;
}
inline std::vector<uint8_t> EncodeFriendsOpen(uint8_t mode) {
    std::vector<uint8_t> b;
    wire::U8(b, mode);
    return b;
}

// 0x1E (protocol 4): strict, as the launcher writes it (UiFrame.RosterMods.Validate)
inline bool DecodeRosterMods(const uint8_t *body, size_t size, RosterMods &out) {
    wire::Reader r{body, size};
    RosterMods m;
    m.world = r.Text(kMaxPaneId);
    const size_t count = r.Count(kMaxRosterMembers);
    if (r.ok && m.world.empty() && count != 0) r.ok = false;
    for (size_t i = 0; i < count && r.ok; ++i) {
        RosterModsMember p;
        p.xuid = r.Text(kMaxXuid);
        const size_t mods = r.Count(kMaxLobbyMods);
        if (!XuidText(p.xuid) || mods == 0) r.ok = false;
        for (size_t k = 0; k < mods && r.ok; ++k) {
            LobbyMod mod;
            mod.id = r.Text(kMaxModId);
            mod.version = r.Text(kMaxModVersion);
            if (mod.id.empty()) r.ok = false;
            p.mods.push_back(std::move(mod));
        }
        m.members.push_back(std::move(p));
    }
    if (!r.Done()) return false;
    out = std::move(m);
    return true;
}

inline std::vector<uint8_t> EncodeRosterMods(const RosterMods &m) {
    std::vector<uint8_t> b;
    wire::Str(b, m.world);
    wire::U8(b, static_cast<uint8_t>(m.members.size()));
    for (const auto &p : m.members) {
        wire::Str(b, p.xuid);
        wire::U8(b, static_cast<uint8_t>(p.mods.size()));
        for (const auto &mod : p.mods) {
            wire::Str(b, mod.id);
            wire::Str(b, mod.version);
        }
    }
    return b;
}

// ---------------------------------------------------------------- PLAYER ACTIONS (protocol 5, OD-193 F-2)
enum : uint8_t { kPlayerUnknown = 0, kPlayerNone = 1, kPlayerFriend = 2, kPlayerIncoming = 3, kPlayerOutgoing = 4, kPlayerOwn = 5,
                 kPlayerStateMax = kPlayerOwn };
enum : uint8_t { kPlayerAdd = 1, kPlayerRemove = 2, kPlayerAccept = 3, kPlayerCancel = 4, kPlayerReport = 5 };
enum : uint8_t { kReportCheating = 1, kReportAbuse = 2, kReportOther = 3 };
constexpr uint8_t kPlayerResultMax = 10;

// The kind and reason rule both ends check (UiFrame.PlayerActionRequest.Validate).
inline bool PlayerActionValid(uint8_t kind, uint8_t reason) {
    if (kind < kPlayerAdd || kind > kPlayerReport) return false;
    return kind == kPlayerReport ? reason >= kReportCheating && reason <= kReportOther : reason == 0;
}

inline std::vector<uint8_t> EncodePlayerStateQuery(const std::string &xuid) {
    std::vector<uint8_t> b;
    wire::Str(b, xuid);
    return b;
}
inline std::vector<uint8_t> EncodePlayerActionRequest(uint32_t id, const std::string &xuid, uint8_t kind, uint8_t reason) {
    std::vector<uint8_t> b;
    wire::U32(b, id);
    wire::Str(b, xuid);
    wire::U8(b, kind);
    wire::U8(b, reason);
    return b;
}

struct PlayerStateFrame {
    std::string xuid;
    uint8_t state = kPlayerUnknown;
};
inline bool DecodePlayerState(const uint8_t *body, size_t size, PlayerStateFrame &out) {
    wire::Reader r{body, size};
    PlayerStateFrame f;
    f.xuid = r.Text(kMaxXuid);
    f.state = r.U8();
    if (!XuidText(f.xuid) || f.state > kPlayerStateMax) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(f);
    return true;
}
inline std::vector<uint8_t> EncodePlayerState(const PlayerStateFrame &f) {
    std::vector<uint8_t> b;
    wire::Str(b, f.xuid);
    wire::U8(b, f.state);
    return b;
}

struct PlayerActionDone {
    uint32_t id = 0;
    std::string xuid;
    uint8_t result = 0, state = kPlayerUnknown;
};
inline bool DecodePlayerActionDone(const uint8_t *body, size_t size, PlayerActionDone &out) {
    wire::Reader r{body, size};
    PlayerActionDone d;
    d.id = r.U32();
    d.xuid = r.Text(kMaxXuid);
    d.result = r.U8();
    d.state = r.U8();
    if (d.id == 0 || !XuidText(d.xuid) || d.result > kPlayerResultMax || d.state > kPlayerStateMax) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(d);
    return true;
}
inline std::vector<uint8_t> EncodePlayerActionDone(const PlayerActionDone &d) {
    std::vector<uint8_t> b;
    wire::U32(b, d.id);
    wire::Str(b, d.xuid);
    wire::U8(b, d.result);
    wire::U8(b, d.state);
    return b;
}

// ---------------------------------------------------------------- launcher -> overlay bodies
inline bool DecodeDetailPane(const uint8_t *body, size_t size, DetailPane &out) {
    wire::Reader r{body, size};
    DetailPane p;
    p.id = r.Text(kMaxPaneId);
    p.title = r.Text(kMaxPaneText);
    p.subtitle = r.Text(kMaxPaneText);
    p.avatar = r.Text(kMaxPaneId);
    if (!p.id.empty() && !PaneKey(p.id)) r.ok = false;
    const size_t sections = r.Count(kMaxPaneSections);
    for (size_t i = 0; i < sections && r.ok; ++i) {
        PaneSection s;
        s.kind = r.U8();
        if (s.kind > 1) r.ok = false;
        s.heading = r.Text(kMaxPaneText);
        const size_t entries = r.Count(kMaxPaneEntries);
        for (size_t e = 0; e < entries && r.ok; ++e) {
            PaneEntry entry;
            entry.primary = r.Text(kMaxPaneText);
            entry.secondary = r.Text(kMaxPaneText);
            const uint8_t icon = r.U8();
            if (icon > kPaneIconMax) r.ok = false;
            entry.icon = static_cast<PaneIcon>(icon);
            s.entries.push_back(std::move(entry));
        }
        p.sections.push_back(std::move(s));
    }
    const size_t actions = r.Count(kMaxPaneActions);
    for (size_t i = 0; i < actions && r.ok; ++i) {
        PaneAction a;
        a.id = r.Text(kMaxPaneId);
        a.label = r.Text(kMaxPaneText);
        a.enabled = r.Flag();
        if (!PaneKey(a.id)) r.ok = false;
        p.actions.push_back(std::move(a));
    }
    if (!r.Done()) return false;
    out = std::move(p);
    return true;
}

// CR-L40 §1.3: the connected server's figures for the guide's bottom block.
struct ServerStatus {
    enum : uint8_t { Known = 1, Capacity = 2, Live = 4, Profiles = 8, All = 15 };
    std::string server; // the host shown under the status
    uint8_t flags = 0;  // 0: nothing read yet, "Status unavailable"
    uint16_t online = 0, capacity = 0, hardcore = 0, noob = 0, lobbies = 0;
    uint32_t profiles_hardcore = 0, profiles_noob = 0;
};

inline bool DecodeServerStatus(const uint8_t *body, size_t size, ServerStatus &out) {
    wire::Reader r{body, size};
    ServerStatus s;
    s.server = r.Text(kMaxServerHost);
    s.flags = r.U8();
    s.online = r.U16();
    s.capacity = r.U16();
    s.hardcore = r.U16();
    s.noob = r.U16();
    s.lobbies = r.U16();
    s.profiles_hardcore = r.U32();
    s.profiles_noob = r.U32();
    if (s.flags > ServerStatus::All) r.ok = false;
    if (!r.Done()) return false;
    out = std::move(s);
    return true;
}

inline std::vector<uint8_t> EncodeServerStatus(const ServerStatus &s) {
    std::vector<uint8_t> b;
    wire::Str(b, s.server);
    wire::U8(b, s.flags);
    wire::U16(b, s.online);
    wire::U16(b, s.capacity);
    wire::U16(b, s.hardcore);
    wire::U16(b, s.noob);
    wire::U16(b, s.lobbies);
    wire::U32(b, s.profiles_hardcore);
    wire::U32(b, s.profiles_noob);
    return b;
}

// CR-L40 §1.3: the player's avatar, square straight-alpha RGBA; side 0 clears it (the initials).
struct AvatarImage {
    uint16_t side = 0;
    std::vector<uint8_t> rgba; // side * side * 4
};

inline bool DecodeAvatar(const uint8_t *body, size_t size, AvatarImage &out) {
    wire::Reader r{body, size};
    const uint16_t width = r.U16(), height = r.U16();
    if (!r.ok || width != height || width > kMaxAvatarSide) return false;
    const size_t bytes = static_cast<size_t>(width) * height * 4;
    if (r.left != bytes) return false;
    out.side = width;
    out.rgba.assign(r.p, r.p + bytes);
    return true;
}

inline std::vector<uint8_t> EncodeAvatar(const AvatarImage &a) {
    std::vector<uint8_t> b;
    wire::U16(b, a.side);
    wire::U16(b, a.side);
    b.insert(b.end(), a.rgba.begin(), a.rgba.end());
    return b;
}

inline std::vector<uint8_t> EncodeDetailPane(const DetailPane &p) {
    std::vector<uint8_t> b;
    wire::Str(b, p.id);
    wire::Str(b, p.title);
    wire::Str(b, p.subtitle);
    wire::Str(b, p.avatar);
    wire::U8(b, static_cast<uint8_t>(p.sections.size()));
    for (const auto &s : p.sections) {
        wire::U8(b, s.kind);
        wire::Str(b, s.heading);
        wire::U8(b, static_cast<uint8_t>(s.entries.size()));
        for (const auto &e : s.entries) {
            wire::Str(b, e.primary);
            wire::Str(b, e.secondary);
            wire::U8(b, static_cast<uint8_t>(e.icon));
        }
    }
    wire::U8(b, static_cast<uint8_t>(p.actions.size()));
    for (const auto &a : p.actions) {
        wire::Str(b, a.id);
        wire::Str(b, a.label);
        wire::Flag(b, a.enabled);
    }
    return b;
}

} // namespace fi
