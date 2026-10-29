// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the UI pipe's protocol 2 frames (OD-112) and the shapes they carry. Header
// only, no Windows or ImGui, so the codec is tested on its own; pipe.cpp sends and receives them.
// The launcher's Infinity.Core/Overlay/UiFrame.cs is the reference codec: same envelope (u32 LE
// length of type + body, u8 type), little-endian fields, strings u16 LE length + UTF-8, flags 0/1.
//
// Overlay -> launcher:
//   0x14 WorldsQueryV2{u8 filter (1 friends, 2 close to me, 3 global, 4 all: every lobby, CR-L40),
//        u8 notFull, u8 modded (0: every lobby of the filter, modded or not; 1, an OD-112 overlay: modded only)}
//   0x17 PlayerDetailsQuery{str xuid (decimal), str gamertag (may be empty)}
//   0x18 PaneActionClick{str paneId, str actionId} (enabled actions only)
// Launcher -> overlay:
//   0x15 WorldsListV2{u8 filter, u8 modded, u8 status, str message, u8 count <= 50, count x LobbyRow},
//        the rows already ordered: matching ones (lock 0 and modsMatch) first, then the others
//        LobbyRow{str id, str name, str host, u8 players, u8 max, u8 kind, u8 audience, u8 lock,
//        u8 ping, u8 friendHost, u8 modded, u8 modsMatch, u8 anyMode (OD-141: open to Hardcore and Noob),
//        u8 members <= 12 x {str gamertag, u8 tier, u8 role}, u8 mods <= 32 x {str id, str version}}
//   0x16 DetailPane{str paneId (empty closes), str title, str subtitle, str avatarKey, u8 sections
//        <= 8 x {u8 kind (0 key/value rows, 1 items), str heading, u8 entries <= 32 x {str primary,
//        str secondary, u8 icon}}, u8 actions <= 4 x {str id, str label, u8 enabled}}
//   0x19 ServerStatus{str server (host, <= 128 B), u8 flags, u16 online, u16 capacity, u16 hardcore,
//        u16 noob, u16 lobbies, u32 profilesHardcore, u32 profilesNoob} (CR-L40 §1.3): flags bit 0
//        known (else "Status unavailable"), 1 capacity, 2 live mode and lobby figures, 3 profiles
//   0x1A Avatar{u16 width, u16 height, width * height * 4 bytes of straight-alpha RGBA}: square,
//        <= 128 (the launcher sends 96: 128 x 128 cannot fit the 64 KiB frame); {0, 0} clears it
// Protocol 1 frames with a CR-L43 (OD-141) field, also coded here so the golden vectors cover them:
//   0x0B CreateWorld{str name, u8 audience, u8 maxPlayers, u8 latency, u8 anyMode (0 the host's own mode, 1 open
//        to Hardcore and Noob)}; the launcher reads a frame without anyMode (an older overlay) as 0
//   0x0E MyWorld{u8 role, str worldId, str name, str host, str code, u8 audience, u8 players, u8 max, u8 named,
//        u8 count <= 16 x {str xuid, str gamertag, u8 tier, u8 role, u8 friend}, u8 anyMode}; an older launcher's
//        frame ends after the members (anyMode 0), and as before trailing bytes are ignored
// A malformed frame leaves `ok` false and is ignored whole.
#pragma once

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

namespace fi {

constexpr uint16_t kUiProtocol = 2;
// CR-L40: every lobby in one list; the launcher drops the last rows' players and mods to stay in one frame
constexpr size_t kMaxLobbyRows = 50, kMaxLobbyMembers = 12, kMaxLobbyMods = 32;
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

struct LobbyMember {
    std::string gamertag;
    uint8_t tier = 0; // 0 Infinity Racer, 1 Infinity Supporter (the badge only, CR-S69)
    uint8_t role = 0; // 0 host, 1 in the world, 2 seat reserved
};
struct LobbyMod {
    std::string id, version;
};

struct WorldRow {
    std::string id, name, host;
    uint8_t players = 0, max = 0;
    uint8_t kind = 0;      // 0 an Infinity Racer host, 1 an Infinity Supporter host (the badge)
    uint8_t audience = 0;  // 0 everyone, 1 supporters, 2 friends, 3 private
    uint8_t lock = 0;      // 0 joinable, 1 tier (an older server's; shown as 6), 2 friends, 3 code, 4 full, 5 latency, 6 other,
                           // 7 another mode (a Hardcore world), 8 another mode (a Noob world)
    uint8_t ping = 0;      // 0 unknown, 1 good, 2 fair, 3 poor
    bool friend_host = false;
    // protocol 2 (OD-112)
    bool modded = false;
    bool mods_match = true; // false: the join is blocked here
    bool any_mode = false;  // OD-141: open to Hardcore and Noob (the "Hardcore + Noob" badge; never locked for mode)
    std::vector<LobbyMember> members;
    std::vector<LobbyMod> mods;
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

// "My world" (OD-63): the world this profile hosts or holds a seat in, and its players. The XUID
// is the key of an action, never shown (OD-61).
struct MemberRow {
    std::string xuid, gamertag;
    uint8_t tier = 0;         // 0 Infinity Racer, 1 Infinity Supporter (the badge only, CR-S69)
    uint8_t role = 0;         // 0 host, 1 in the world, 2 on the way (seat reserved)
    uint8_t friend_state = 0; // 0 none, 1 friends, 2 request sent, 3 asked you, 4 you
};
struct MyWorld {
    uint8_t role = 0; // 0 none, 1 hosting, 2 seat reserved, 3 seated
    std::string world, name, host, code;
    uint8_t audience = 0, players = 0, max = 0;
    bool named = false; // false: the server gave no roster (only the host is known)
    std::vector<MemberRow> members;
    bool any_mode = false; // OD-141: open to Hardcore and Noob (the header's badge)
};
constexpr size_t kMaxMineMembers = 16;

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
inline std::vector<uint8_t> EncodeWorldsQueryV2(uint8_t filter, bool not_full, bool modded) {
    std::vector<uint8_t> b;
    wire::U8(b, filter);
    wire::Flag(b, not_full);
    wire::Flag(b, modded);
    return b;
}
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
// 0x0B (protocol 1; anyMode CR-L43)
inline std::vector<uint8_t> EncodeCreateWorld(const std::string &name, uint8_t audience, uint8_t max_players, uint8_t latency, bool any_mode) {
    std::vector<uint8_t> b;
    wire::Str(b, name);
    wire::U8(b, audience);
    wire::U8(b, max_players);
    wire::U8(b, latency);
    wire::Flag(b, any_mode);
    return b;
}

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
    return b;
}

// ---------------------------------------------------------------- launcher -> overlay bodies
struct WorldsListV2 {
    uint8_t filter = 0;
    bool modded = false;
    uint8_t status = 0; // 0 ok, 2 error
    std::string message;
    std::vector<WorldRow> rows;
};

inline bool DecodeWorldsListV2(const uint8_t *body, size_t size, WorldsListV2 &out) {
    wire::Reader r{body, size};
    WorldsListV2 l;
    l.filter = r.U8();
    l.modded = r.Flag();
    l.status = r.U8();
    l.message = r.Text();
    const size_t count = r.Count(kMaxLobbyRows);
    for (size_t i = 0; i < count && r.ok; ++i) {
        WorldRow w;
        w.id = r.Text();
        w.name = r.Text();
        w.host = r.Text();
        w.players = r.U8();
        w.max = r.U8();
        w.kind = r.U8();
        w.audience = r.U8();
        w.lock = r.U8();
        w.ping = r.U8();
        w.friend_host = r.Flag();
        w.modded = r.Flag();
        w.mods_match = r.Flag();
        w.any_mode = r.Flag();
        const size_t members = r.Count(kMaxLobbyMembers);
        for (size_t m = 0; m < members && r.ok; ++m) {
            LobbyMember p;
            p.gamertag = r.Text(kMaxGamertag);
            p.tier = r.U8();
            p.role = r.U8();
            if (p.tier > 1 || p.role > 2) r.ok = false;
            w.members.push_back(std::move(p));
        }
        const size_t mods = r.Count(kMaxLobbyMods);
        for (size_t m = 0; m < mods && r.ok; ++m) {
            LobbyMod mod;
            mod.id = r.Text(kMaxModId);
            mod.version = r.Text(kMaxModVersion);
            w.mods.push_back(std::move(mod));
        }
        if (!w.modded && !w.mods.empty()) r.ok = false;
        l.rows.push_back(std::move(w));
    }
    if (!r.Done()) return false;
    out = std::move(l);
    return true;
}

inline std::vector<uint8_t> EncodeWorldsListV2(const WorldsListV2 &l) {
    std::vector<uint8_t> b;
    wire::U8(b, l.filter);
    wire::Flag(b, l.modded);
    wire::U8(b, l.status);
    wire::Str(b, l.message);
    wire::U8(b, static_cast<uint8_t>(l.rows.size()));
    for (const auto &w : l.rows) {
        wire::Str(b, w.id);
        wire::Str(b, w.name);
        wire::Str(b, w.host);
        wire::U8(b, w.players);
        wire::U8(b, w.max);
        wire::U8(b, w.kind);
        wire::U8(b, w.audience);
        wire::U8(b, w.lock);
        wire::U8(b, w.ping);
        wire::Flag(b, w.friend_host);
        wire::Flag(b, w.modded);
        wire::Flag(b, w.mods_match);
        wire::Flag(b, w.any_mode);
        wire::U8(b, static_cast<uint8_t>(w.members.size()));
        for (const auto &m : w.members) {
            wire::Str(b, m.gamertag);
            wire::U8(b, m.tier);
            wire::U8(b, m.role);
        }
        wire::U8(b, static_cast<uint8_t>(w.mods.size()));
        for (const auto &m : w.mods) {
            wire::Str(b, m.id);
            wire::Str(b, m.version);
        }
    }
    return b;
}

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
