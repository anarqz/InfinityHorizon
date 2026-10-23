// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the UI pipe's protocol 2 frames (OD-112) and the shapes they carry. Header
// only, no Windows or ImGui, so the codec is tested on its own; pipe.cpp sends and receives them.
// The launcher's Infinity.Core/Overlay/UiFrame.cs is the reference codec: same envelope (u32 LE
// length of type + body, u8 type), little-endian fields, strings u16 LE length + UTF-8, flags 0/1.
//
// Overlay -> launcher:
//   0x14 WorldsQueryV2{u8 filter, u8 notFull, u8 modded (0 vanilla worlds only, 1 modded only)}
//   0x17 PlayerDetailsQuery{str xuid (decimal), str gamertag (may be empty)}
//   0x18 PaneActionClick{str paneId, str actionId} (enabled actions only)
// Launcher -> overlay:
//   0x15 WorldsListV2{u8 filter, u8 modded, u8 status, str message, u8 count <= 16, count x LobbyRow}
//        LobbyRow{str id, str name, str host, u8 players, u8 max, u8 kind, u8 audience, u8 lock,
//        u8 ping, u8 friendHost, u8 modded, u8 modsMatch, u8 members <= 12 x {str gamertag, u8 tier,
//        u8 role}, u8 mods <= 32 x {str id, str version}}
//   0x16 DetailPane{str paneId (empty closes), str title, str subtitle, str avatarKey, u8 sections
//        <= 8 x {u8 kind (0 key/value rows, 1 items), str heading, u8 entries <= 32 x {str primary,
//        str secondary, u8 icon}}, u8 actions <= 4 x {str id, str label, u8 enabled}}
// A malformed frame leaves `ok` false and is ignored whole.
#pragma once

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

namespace fi {

constexpr uint16_t kUiProtocol = 2;
constexpr size_t kMaxLobbyRows = 16, kMaxLobbyMembers = 12, kMaxLobbyMods = 32;
constexpr size_t kMaxPaneSections = 8, kMaxPaneEntries = 32, kMaxPaneActions = 4;

enum class PaneIcon : uint8_t { None, Player, Host, Supporter, Mod, Warning, Check };
constexpr uint8_t kPaneIconMax = static_cast<uint8_t>(PaneIcon::Check);

struct LobbyMember {
    std::string gamertag;
    uint8_t tier = 0; // 0 free, 1 Infinity Supporter
    uint8_t role = 0; // 0 host, 1 in the world, 2 seat reserved
};
struct LobbyMod {
    std::string id, version;
};

struct WorldRow {
    std::string id, name, host;
    uint8_t players = 0, max = 0;
    uint8_t kind = 0;      // 0 everyone's, 1 Infinity Supporter
    uint8_t audience = 0;  // 0 everyone, 1 supporters, 2 friends, 3 private
    uint8_t lock = 0;      // 0 joinable, 1 tier, 2 friends, 3 code, 4 full, 5 latency
    uint8_t ping = 0;      // 0 unknown, 1 good, 2 fair, 3 poor
    bool friend_host = false;
    // protocol 2 (OD-112)
    bool modded = false;
    bool mods_match = true; // false: the join is blocked here
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
inline std::vector<uint8_t> EncodePlayerDetailsQuery(const std::string &xuid, const std::string &gamertag) {
    std::vector<uint8_t> b;
    wire::Str(b, xuid);
    wire::Str(b, gamertag);
    return b;
}
inline std::vector<uint8_t> EncodePaneActionClick(const std::string &pane, const std::string &action) {
    std::vector<uint8_t> b;
    wire::Str(b, pane);
    wire::Str(b, action);
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
    l.message = r.Str();
    const size_t count = r.Count(kMaxLobbyRows);
    for (size_t i = 0; i < count && r.ok; ++i) {
        WorldRow w;
        w.id = r.Str();
        w.name = r.Str();
        w.host = r.Str();
        w.players = r.U8();
        w.max = r.U8();
        w.kind = r.U8();
        w.audience = r.U8();
        w.lock = r.U8();
        w.ping = r.U8();
        w.friend_host = r.Flag();
        w.modded = r.Flag();
        w.mods_match = r.Flag();
        const size_t members = r.Count(kMaxLobbyMembers);
        for (size_t m = 0; m < members && r.ok; ++m) {
            LobbyMember p;
            p.gamertag = r.Str();
            p.tier = r.U8();
            p.role = r.U8();
            if (p.tier > 1 || p.role > 2) r.ok = false;
            w.members.push_back(std::move(p));
        }
        const size_t mods = r.Count(kMaxLobbyMods);
        for (size_t m = 0; m < mods && r.ok; ++m) {
            LobbyMod mod;
            mod.id = r.Str();
            mod.version = r.Str();
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
    p.id = r.Str();
    p.title = r.Str();
    p.subtitle = r.Str();
    p.avatar = r.Str();
    const size_t sections = r.Count(kMaxPaneSections);
    for (size_t i = 0; i < sections && r.ok; ++i) {
        PaneSection s;
        s.kind = r.U8();
        if (s.kind > 1) r.ok = false;
        s.heading = r.Str();
        const size_t entries = r.Count(kMaxPaneEntries);
        for (size_t e = 0; e < entries && r.ok; ++e) {
            PaneEntry entry;
            entry.primary = r.Str();
            entry.secondary = r.Str();
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
        a.id = r.Str();
        a.label = r.Str();
        a.enabled = r.Flag();
        if (a.id.empty()) r.ok = false;
        p.actions.push_back(std::move(a));
    }
    if (!r.Done()) return false;
    out = std::move(p);
    return true;
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
