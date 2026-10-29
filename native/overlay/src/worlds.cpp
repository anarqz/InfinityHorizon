// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: worlds (OD-54, OD-56, OD-57, OD-58, CR-L40). The guide's Lobbies tab finds,
// joins and creates worlds; the launcher talks to matchmaking (the overlay holds no tokens) and
// this file only asks and shows.
//
// - The list (CR-L40 §2.2): every lobby of the game, asked each time the guide opens with the "all"
//   filter and no chip selected. Friends, Close to me and Global narrow it, Hide full drops full
//   ones; selecting the active filter again returns to all; Y refreshes. Up to 50 rows, already
//   ordered by the launcher: the matching ones (the profile's mode or open to both modes, the same
//   mods, open to the player) first, then the others drawn dimmed with their reason under the name (another mode,
//   mods, friends only, a code, full, too far, not available). A code row opens the code entry, the
//   rest say their reason. CR-S69 (OD-129): no lobby is shut by the tier any more; an older server's
//   tier lock (1) reads "Not available". A Supporter host carries the "Supporter" badge. The list
//   scrolls with the focus and the mouse wheel. A join answered "closed" or "full" refreshes it: the
//   list was older than the world. No queue for worlds: a full world is just full.
// - Join reserves a seat only (matchmaking's handshake); the game's own Free Roam Online then
//   lands the player in it. Before the first join or create of the session, the not-rated
//   online-play warning (OD-54).
// - A private or friends world is joined with its code (code_entry.cpp); create gives the host
//   the code to share.
// - Create: name (the keyboard; "{gamertag}'s lobby" by default, CR-L40 §4), audience (Everyone,
//   Supporters, Friends, Private; every host gets all four, OD-129), players 2-12, latency limit, and
//   the modes (OD-141, CR-L43): the host's own mode (the default) or open to Hardcore and Noob, sent
//   with the create and fixed while the lobby is open (a new lobby is needed to change it).
// - Hardcore + Noob (OD-141): a lobby open to both modes carries the "Hardcore + Noob" badge in the
//   list, the right pane and my world's header. The overlay never filters or dims by mode itself: a
//   row is another mode's (lock 7/8) only when the launcher says so, from the server's answer.
// - My world (OD-63): while the profile hosts a world or holds a seat in one, the tab shows that
//   world instead of the list (updated by the launcher): the host closes it or kicks players,
//   a player leaves it; anyone adds a player as a friend or reports them with a reason. Player
//   names only, never their XUIDs (OD-61).
// - Once a world is created the Hosted screen follows; once a seat is reserved, the prompt to go
//   to Free Roam Online. (The OD-65 "Syncing cars" hold is retired: the server keeps the liveries
//   and world mates' games fetch them by the game's own calls, CR-S59.)
// - Modded lobbies (OD-112): modded and vanilla worlds share the list (mods are a reason, not a
//   filter, CR-L40). A world whose mods differ from the player's carries a warning icon and is never
//   joined from here: the player is told which mods it uses. The focused or hovered world fills the
//   right pane (pane.cpp) with its players and mods, from the list's own rows.
#include "overlay.h"

#include <algorithm>
#include <atomic>
#include <chrono>
#include <thread>
#include <cmath>
#include <mutex>
#include <vector>

#include "fi_overlay.h"
#include "style.h"
#include "world_access.h"

namespace fi {

using namespace ui;

namespace {

// ---------------------------------------------------------------- what the launcher sent (any thread)
struct Shared {
    WorldAccessLease access;
    std::string access_message;
    WorldFilter filter = WorldFilter::None;
    bool modded = false;     // the modded toggle the list was asked with
    uint8_t list_status = 0; // WorldsStatus
    std::string list_message;
    std::vector<WorldRow> rows;
    uint64_t list_serial = 0;
    uint8_t join_status = 0;
    std::string join_message;
    uint64_t join_serial = 0;
    HostedWorld hosted;
    uint64_t hosted_serial = 0;
    MyWorld mine;
    uint64_t mine_serial = 0;
    uint8_t action = 0, action_status = 0;
    std::string action_message;
    uint64_t action_serial = 0;
};
std::mutex g_lock;
Shared g_shared;

bool WorldAccessAllowed() {
    if (DevHost()) return true;
    std::lock_guard<std::mutex> lock(g_lock);
    return g_shared.access.Allowed(GetTickCount64());
}

const char *AccessMessage(const Shared &sh) {
    return !sh.access.Allowed(GetTickCount64()) && !sh.access_message.empty() ? sh.access_message.c_str() : T(
        "Online installation verification is unavailable. Return to the launcher to review your installation or get help.",
        "A verificação da instalação para jogar online está indisponível. Volte ao launcher para verificar sua instalação ou obter ajuda.");
}

// ---------------------------------------------------------------- render-thread state
enum class Screen { List, Warning, Busy, Create, Hosted, Member, Report, Confirm };
Screen g_screen = Screen::List;
WorldFilter g_filter = WorldFilter::All; // CR-L40: every lobby, no chip selected
bool g_queried = false;                   // the list was asked since the guide opened
bool g_hide_full = false;
bool g_warned = false; // the online-play warning, once per game session
int g_focus = 0;       // the list's focus: 0 the filter row, then the tiles
int g_chip = 0;        // in the filter row: 0 Friends, 1 Close to me, 2 Global, 3 Hide full
constexpr int kLastChip = 3;
float g_scroll = 0;    // the rows' scroll, in pixels from the first row
DetailPane g_lobby_pane; // the focused or hovered row's details, rebuilt every frame
uint64_t g_seen_list = 0, g_seen_join = 0, g_seen_hosted = 0;
std::string g_busy;    // what Busy says
bool g_world_action_busy = false;
std::function<void()> g_after_warning;

// my world (OD-63)
bool g_mine_refresh = true; // one explicit reconcile on open/action/manual refresh
bool g_showing_mine = false; // the List screen shows my world this frame
int g_mine_focus = 0;       // 0 the close or leave button, then the players
int g_mine_count = 0;       // what the focus walks: 1 + the players
MemberRow g_member;         // the player the card is about
uint8_t g_mine_role = 0;    // of the world last drawn
int g_confirm = 0;          // the action to confirm: 1 close, 2 leave, 3 kick
int g_buttons = 0;          // how many buttons the current card has
uint64_t g_seen_mine = 0, g_seen_action = 0;
int g_dev_mine = 0;         // the dev host's own world: 0 none, 1 hosting, 3 seated

// a world created (the Hosted screen) or a seat reserved (the prompt to go to Free Roam Online)
void Admitted(bool hosted) {
    if (hosted) {
        g_screen = Screen::Hosted;
    } else {
        g_screen = Screen::List;
        PushToast(ToastKind::Success, T("Seat reserved", "Lugar reservado"),
                  T("Go to Free Roam Online to join the lobby", "Vá em Free Roam Online para entrar no lobby"));
        CloseSidebar();
    }
    g_mine_refresh = true;
}

struct Create {
    std::string name;
    int audience = 0; // WorldAudience
    int players = 8;
    int latency = 0;  // 0 any, 1 high (< 150 ms), 2 medium (< 100 ms), 3 low (< 60 ms)
    bool any_mode = false; // OD-141: open to Hardcore and Noob; false: the host's own mode
    int field = 0;    // 0 name, 1 audience, 2 players, 3 latency, 4 modes, 5 create
} g_create;
constexpr int kCreateModes = 4, kCreateButton = 5;

// the dev host answers by itself, as a launcher would
float g_dev_wait = -1;
int g_dev_kind = 0;

const char *FilterName(WorldFilter filter) {
    switch (filter) {
    case WorldFilter::Friends: return T("Friends", "Amigos");
    case WorldFilter::Near: return T("Close to me", "Perto de mim");
    case WorldFilter::Global: return T("Global", "Global");
    default: return "";
    }
}

const char *AudienceName(int audience) {
    switch (audience) {
    case 1: return T("Infinity Supporters", "Infinity Supporters");
    case 2: return T("Friends", "Amigos");
    case 3: return T("Private · with a code", "Privado · com código");
    default: return T("Everyone", "Todos");
    }
}

const char *LatencyName(int latency) {
    switch (latency) {
    case 1: return T("Under 150 ms", "Até 150 ms");
    case 2: return T("Under 100 ms", "Até 100 ms");
    case 3: return T("Under 60 ms", "Até 60 ms");
    default: return T("Any", "Qualquer");
    }
}

const char *ModesName(bool any_mode) {
    return any_mode ? T("Open to Hardcore and Noob", "Aberto a Hardcore e Noob") : T("My mode only", "Só o meu modo");
}

// OD-141: the badge of a lobby open to both modes (the same words in both languages)
constexpr const char *kAnyModeBadge = "Hardcore + Noob";

const char *LockName(uint8_t lock) {
    switch (lock) {
    case 1: // an older server's tier lock (CR-S69): no upsell, just not joinable here
    case 6: return T("Not available", "Indisponível");
    case 2: return T("Friends only", "Só amigos");
    case 3: return T("Needs a code", "Precisa de código");
    case 4: return T("Full", "Cheio");
    case 5: return T("Too far", "Longe demais");
    case 7: return T("Hardcore lobby", "Lobby Hardcore");
    case 8: return T("Noob lobby", "Lobby Noob");
    default: return "";
    }
}

// Why a row is not "matching" (CR-L40 §2.2), the first that applies; None: the player joins it as they are.
enum class Why { None, Mode, Mods, Friends, Code, Full, Far, Other };

Why WhyOf(const WorldRow &w) {
    if ((w.lock == 7 || w.lock == 8) && !w.any_mode) return Why::Mode; // a lobby open to both modes is never another mode's
    if (w.lock == 7 || w.lock == 8) return Why::Other;
    if (!w.mods_match) return Why::Mods;
    switch (w.lock) {
    case 0: return Why::None;
    case 2: return Why::Friends;
    case 3: return Why::Code;
    case 4: return Why::Full;
    case 5: return Why::Far;
    default: return Why::Other;
    }
}

// A vanilla row matches only a session without mods: one in the list tells whether the player has mods.
// 1 no mods, 0 mods, -1 unknown.
int LocalVanilla(const std::vector<WorldRow> &rows) {
    for (const auto &r : rows) {
        if (!r.modded && r.lock != 7 && r.lock != 8) return r.mods_match ? 1 : 0;
    }
    return -1;
}

// The reason in a row's subtitle.
std::string WhyText(const WorldRow &w, int local_vanilla) {
    if (WhyOf(w) != Why::Mods) return LockName(w.lock);
    if (!w.modded) return T("No mods", "Sem mods");
    return local_vanilla == 1 ? T("Modded lobby", "Lobby com mods") : T("Mods differ", "Mods diferentes");
}

std::string FormatCode(const std::string &code) {
    return code.size() == 8 ? code.substr(0, 4) + "-" + code.substr(4) : code;
}

// ---------------------------------------------------------------- requests
void Act(uint8_t action, const MemberRow &member, uint8_t reason) {
    g_world_action_busy = true;
    g_busy = T("One moment...", "Um momento...");
    g_screen = Screen::Busy;
    if (DevHost()) {
        g_dev_wait = 0.6f;
        g_dev_kind = 10 + action;
    } else {
        SendWorldAction(action, member.xuid, member.gamertag, reason);
    }
}

void Query() {
    if (!WorldAccessAllowed()) return;
    if (g_filter == WorldFilter::None) {
        return;
    }
    {
        std::lock_guard<std::mutex> lock(g_lock);
        g_shared.filter = g_filter;
        g_shared.modded = false; // CR-L40: modded and vanilla lobbies in one list
        g_shared.list_status = 1; // loading
        g_shared.rows.clear();
        ++g_shared.list_serial;
    }
    g_scroll = 0;
    if (DevHost()) {
        g_dev_wait = 0.5f;
        g_dev_kind = 1;
    } else {
        SendWorldsQuery(g_filter, g_hide_full);
    }
}

void WithWarning(std::function<void()> then) {
    if (g_warned) {
        then();
        return;
    }
    g_after_warning = std::move(then);
    g_screen = Screen::Warning;
}

void Join(const std::string &world, const std::string &code) {
    if (!GameReady() || !WorldAccessAllowed()) return;
    WithWarning([world, code] {
        if (!GameReady() || !WorldAccessAllowed()) return;
        g_world_action_busy = false;
        g_busy = T("Reserving your seat...", "Reservando seu lugar...");
        g_screen = Screen::Busy;
        if (DevHost()) {
            g_dev_wait = 0.8f;
            g_dev_kind = code == "00000000" ? 3 : 2;
        } else {
            SendJoinWorld(world, code);
        }
    });
}

void JoinByCode() {
    if (!GameReady() || !WorldAccessAllowed()) return;
    WithWarning([] {
        OpenCodeEntry(T("Enter the lobby code", "Digite o código do lobby"), "0123456789", [](bool join, const std::string &code) {
            if (join) {
                g_warned = true; // already accepted on the way in
                Join(std::string(), code);
            }
        });
    });
}

constexpr size_t kMaxLobbyName = 48; // characters, as the launcher and the server cut it

// CR-L40 §4: "{gamertag}'s lobby" / "Lobby de {gamertag}", at most 48 characters: the gamertag is cut
// on a whole code point so the whole name fits. No profile yet: "Infinity".
std::string DefaultLobbyName(const std::string &gamertag) {
    const std::string tag = Utf8Cut(gamertag, gamertag.size()); // empty when not UTF-8
    if (tag.empty()) return "Infinity";
    const bool pt = Portuguese();
    const std::string before = pt ? "Lobby de " : "", after = pt ? "" : "'s lobby"; // ASCII: a byte per character
    const size_t room = kMaxLobbyName - before.size() - after.size();
    size_t end = 0;
    for (size_t chars = 0; end < tag.size() && chars < room; ++chars) {
        const unsigned c = static_cast<unsigned char>(tag[end]);
        end += c < 0x80 ? 1 : (c >> 5) == 0x6 ? 2 : (c >> 4) == 0xE ? 3 : 4;
    }
    return before + tag.substr(0, end) + after;
}

void StartCreate() {
    if (!GameReady() || !WorldAccessAllowed()) return;
    WithWarning([] {
        if (!GameReady() || !WorldAccessAllowed()) return;
        g_create = Create{}; // Everyone; any of the four audiences for every host (OD-129)
        g_create.name = DefaultLobbyName(ProfileGamertag());
        g_screen = Screen::Create;
    });
}

void SubmitCreate() {
    if (!GameReady() || !WorldAccessAllowed()) { g_screen = Screen::List; return; }
    g_world_action_busy = false;
    g_busy = T("Creating your lobby...", "Criando seu lobby...");
    g_screen = Screen::Busy;
    if (DevHost()) {
        g_dev_wait = 0.8f;
        g_dev_kind = 4;
    } else {
        SendCreateWorld(g_create.name, static_cast<uint8_t>(g_create.audience), static_cast<uint8_t>(g_create.players),
                        static_cast<uint8_t>(g_create.latency), g_create.any_mode);
    }
}

// the dev host's fake launcher
void DevAnswer() {
    std::lock_guard<std::mutex> lock(g_lock);
    switch (g_dev_kind) {
    case 1: {
        g_shared.list_status = 0;
        g_shared.rows.clear();
        const auto members = [](std::initializer_list<LobbyMember> list) { return std::vector<LobbyMember>(list); };
        // in the launcher's order (CR-L40): the matching rows, then the others with their reason
        g_shared.rows.push_back({"w1", "Sunset Cruise", "RallyFox98", 5, 8, 0, 0, 0, 1, true});
        g_shared.rows.back().members = members({{"RallyFox98", 0, 0}, {"Synthetic#00117", 1, 1}, {"Synthetic#00230", 0, 1}, {"Synthetic#00042", 0, 1}, {"Synthetic#00301", 1, 2}});
        g_shared.rows.push_back({"w2", "Nice coastline run", "Synthetic#00117", 3, 6, 0, 0, 0, 0, false});
        g_shared.rows.push_back({"w3", "Drift crew", "Synthetic#00230", 11, 12, 0, 0, 0, 2, false});
        WorldRow modded{"m1", "Nitro drift night", "Synthetic#00117", 4, 8, 0, 0, 0, 1, false};
        modded.modded = true;
        modded.mods_match = false;
        modded.members = members({{"Synthetic#00117", 1, 0}, {"Synthetic#00230", 0, 1}, {"RallyFox98", 0, 1}, {"Synthetic#00042", 1, 2}});
        modded.mods = {{"extraoptions", "2"}, {"nitrosystem", "3"}};
        g_shared.rows.push_back(modded);
        WorldRow mixed{"a1", "Everyone's coast tour", "Synthetic#00412", 4, 10, 0, 0, 0, 1, false};
        mixed.any_mode = true; // OD-141: open to Hardcore and Noob, matching for either mode
        g_shared.rows.insert(g_shared.rows.begin() + 2, mixed);
        g_shared.rows.push_back({"h1", "Hardcore endurance", "Synthetic#00303", 6, 12, 0, 0, 7, 1, false});
        g_shared.rows.push_back({"s1", "Supporters night race", "Synthetic#00301", 9, 12, 1, 1, 0, 0, false});
        g_shared.rows.push_back({"c1", "Friday crew", "Synthetic#00230", 3, 8, 0, 3, 3, 1, false});
        if (!g_hide_full) {
            g_shared.rows.push_back({"w4", "Bucket list hunters", "Synthetic#00042", 12, 12, 0, 0, 4, 1, false});
        }
        for (int i = 0; i < 6; ++i) { // enough rows to scroll
            g_shared.rows.push_back({"n" + std::to_string(i), "Far away cruise " + std::to_string(i + 1), "Synthetic#0040" + std::to_string(i), 2, 8, 0, 0, 5, 3, false});
        }
        ++g_shared.list_serial;
        break;
    }
    case 2:
        g_shared.join_status = 0;
        g_shared.join_message.clear();
        ++g_shared.join_serial;
        g_dev_mine = 3;
        break;
    case 3:
        g_shared.join_status = 3;
        g_shared.join_message = T("That code does not open any lobby", "Esse código não abre nenhum lobby");
        ++g_shared.join_serial;
        break;
    case 4:
        g_shared.hosted = {0, "h1", g_create.audience >= 2 ? "48201573" : "", ""};
        ++g_shared.hosted_serial;
        g_dev_mine = 1;
        break;
    case 6: { // my world, as the launcher would answer
        MyWorld m;
        if (g_dev_mine != 0) {
            m.role = static_cast<uint8_t>(g_dev_mine);
            m.world = "h1";
            m.name = g_dev_mine == 1 ? g_create.name : "Sunset Cruise";
            m.host = g_dev_mine == 1 ? "Synthetic#00117" : "RallyFox98";
            m.code = g_dev_mine == 1 && g_create.audience >= 2 ? "48201573" : "";
            m.audience = static_cast<uint8_t>(g_dev_mine == 1 ? g_create.audience : 0);
            m.any_mode = g_dev_mine == 1 && g_create.any_mode;
            m.players = 3;
            m.max = 8;
            m.named = true;
            m.members.push_back({"1", m.host, 1, 0, static_cast<uint8_t>(g_dev_mine == 1 ? 4 : 0)});
            m.members.push_back({"2", g_dev_mine == 1 ? "Synthetic#00230" : "Synthetic#00117", 0, 1, static_cast<uint8_t>(g_dev_mine == 1 ? 0 : 4)});
            m.members.push_back({"3", "Synthetic#00042", 1, 2, 3});
        }
        g_shared.mine = m;
        ++g_shared.mine_serial;
        break;
    }
    case 11: case 12: case 13: case 14: case 15: {
        const int action = g_dev_kind - 10;
        static const char *done[] = {"", "Your lobby is closed.", "You left the lobby.", "The player was removed from your lobby.", "Friend request sent.",
                                     "Thanks. The report was sent to the moderators."};
        g_shared.action = static_cast<uint8_t>(action);
        g_shared.action_status = 0;
        g_shared.action_message = done[action];
        ++g_shared.action_serial;
        if (action <= 2) g_dev_mine = 0;
        break;
    }
    default: break;
    }
}

// ---------------------------------------------------------------- drawing helpers
void PingBars(ImDrawList *dl, ImVec2 at, uint8_t ping) {
    // 0 unknown, 1 good, 2 fair, 3 poor: bars lit from the left
    const int lit = ping == 1 ? 3 : ping == 2 ? 2 : ping == 3 ? 1 : 0;
    const ImU32 on = ping == 3 ? kAmber : IM_COL32(92, 214, 92, 255);
    for (int i = 0; i < 3; ++i) {
        const float h = (6 + i * 5) * s;
        dl->AddRectFilled(ImVec2(at.x + i * 7 * s, at.y + 16 * s - h), ImVec2(at.x + i * 7 * s + 4 * s, at.y + 16 * s), i < lit ? on : Rgba(255, 255, 255, 0.18f), 1 * s);
    }
}

void Wrapped(ImDrawList *dl, ImFont *font, ImVec2 pos, float width, ImU32 color, const char *text) {
    dl->AddText(font, font->FontSize, pos, color, text, nullptr, width);
}

float WrappedHeight(ImFont *font, float width, const char *text) {
    return font->CalcTextSizeA(font->FontSize, FLT_MAX, width, text).y;
}

// a modal right pane holds the input: the tab underneath takes no clicks
bool Clicked(ImVec2 a, ImVec2 b) { return !PaneModal() && ImGui::IsMouseHoveringRect(a, b) && ImGui::IsMouseClicked(0); }

struct Item {
    enum Kind { Chips, CreateTile, CodeTile, Row } kind;
    int row = -1; // a Row: its index in the launcher's rows
};

std::vector<Item> g_items; // what the focus walks, rebuilt every frame
bool g_accept = false;     // A, applied at draw time on the focused item

// The vertices drawn since `first` at `alpha` of their own (a dimmed row keeps its look).
void FadeSince(ImDrawList *dl, int first, float alpha) {
    for (int v = first; v < dl->VtxBuffer.Size; ++v) {
        ImU32 &c = dl->VtxBuffer[v].col;
        c = (c & 0x00FFFFFF) | (static_cast<ImU32>(((c >> 24) & 0xFF) * alpha) << 24);
    }
}

void Tile(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const char *icon, const char *title, const char *subtitle, bool dim = false) {
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float th = b.y - a.y;
    dl->AddText(f.icon, f.icon->FontSize, ImVec2(a.x + 20 * s, a.y + th / 2 - f.icon->FontSize / 2), dim ? kTextDim : focus ? kPinkU : kText, icon);
    const float lx = a.x + 62 * s;
    dl->PushClipRect(ImVec2(lx, a.y), ImVec2(b.x - 12 * s, b.y), true);
    TextAt(dl, f.body, ImVec2(lx, a.y + (subtitle && *subtitle ? 11 * s : th / 2 - 12 * s)), dim ? kTextDim : kText, title);
    if (subtitle && *subtitle) TextAt(dl, f.small, ImVec2(lx, a.y + 39 * s), kTextDim, subtitle);
    dl->PopClipRect();
}

// One lobby row. A matching row is drawn as is; any other at about 45% with its reason after the
// host (CR-L40 §2.2). The focus ring stays at full strength on either.
void WorldTile(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const WorldRow &w, int local_vanilla) {
    const Why why = WhyOf(w);
    const bool other = why != Why::None;
    const bool ready = GameReady();
    const int first = dl->VtxBuffer.Size;
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    const float lx = a.x + 18 * s;
    dl->PushClipRect(ImVec2(lx, a.y), ImVec2(b.x - 92 * s, b.y), true);
    TextAt(dl, f.body, ImVec2(lx, a.y + 10 * s), ready ? kText : kTextDim, w.name.c_str());
    std::string sub = w.host;
    if (w.kind == 1) sub += " · Supporter"; // CR-S69: the host's badge, nothing more
    if (w.any_mode) sub += std::string(" · ") + kAnyModeBadge; // OD-141
    if (w.friend_host) sub = std::string(T("Friend · ", "Amigo · ")) + sub;
    if (other) sub += " · " + WhyText(w, local_vanilla);
    if (!ready) sub = T("Load your game to access lobbies", "Carregue sua partida para acessar os lobbies");
    const ImU32 sub_color = !ready ? kTextDim : why == Why::Mods ? kAmber : kTextDim;
    TextAt(dl, f.small, ImVec2(lx, a.y + 38 * s), sub_color, sub.c_str());
    dl->PopClipRect();
    const std::string count = std::to_string(w.players) + "/" + std::to_string(w.max);
    const ImVec2 cs = Measure(f.body, count.c_str());
    TextAt(dl, f.body, ImVec2(b.x - 50 * s - cs.x, a.y + 10 * s), w.players >= w.max ? kRed : kText, count.c_str());
    PingBars(dl, ImVec2(b.x - 38 * s, a.y + 14 * s), w.ping);
    if (why == Why::Code) {
        dl->AddText(f.icon, f.icon->FontSize * 0.75f, ImVec2(b.x - 36 * s, a.y + 40 * s), kText, kIconLock);
    } else if (why == Why::Mods) {
        // OD-112: the small "your mods don't match" mark
        dl->AddText(f.icon, f.icon->FontSize * 0.75f, ImVec2(b.x - 36 * s, a.y + 38 * s), kAmber, kIconWarning);
    }
    if (other) FadeSince(dl, first, 0.45f);
    if (focus) FocusRing(dl, a, b, 6 * s);
}

std::string ModList(const WorldRow &w) {
    std::string out;
    for (const auto &m : w.mods) {
        if (!out.empty()) out += ", ";
        out += m.id + " " + m.version;
    }
    return out;
}

// Why a row whose mods differ from the player's cannot be joined (modsMatch false; vanilla rows too).
std::string MismatchText(const WorldRow &w) {
    if (!w.modded) return T("This lobby has no mods; turn your mods off in the launcher to join.",
                            "Este lobby não tem mods; desligue seus mods no launcher para entrar.");
    const std::string list = ModList(w);
    return (list.empty() ? std::string(T("This lobby uses other mods.", "Este lobby usa outros mods."))
                         : std::string(T("This lobby uses: ", "Este lobby usa: ")) + list + ".") +
           T(" Use the same mods in the launcher to join.", " Use os mesmos mods no launcher para entrar.");
}

// The right pane for a lobby (OD-112): built here from the row, so hovering costs no request.
const DetailPane &LobbyPane(const WorldRow &w) {
    DetailPane &p = g_lobby_pane;
    p = DetailPane{};
    p.id = "lobby:" + w.id;
    p.title = w.name;
    p.subtitle = w.host + " · " + AudienceName(w.audience) + (w.any_mode ? std::string(" · ") + kAnyModeBadge : std::string()) + " · " +
                 std::to_string(w.players) + "/" + std::to_string(w.max) + T(" players", " jogadores");
    p.avatar = "icon:world";
    if (!w.mods_match) {
        p.sections.push_back({1, "", {{T("Your mods don't match this lobby", "Seus mods não batem com os deste lobby"),
                                       w.modded ? T("Use the same mods in the launcher to join", "Use os mesmos mods no launcher para entrar")
                                                : T("This lobby has no mods; turn your mods off to join", "Este lobby não tem mods; desligue seus mods para entrar"),
                                       PaneIcon::Warning}}});
    }
    PaneSection players{1, T("PLAYERS", "JOGADORES"), {}};
    for (const auto &m : w.members) {
        std::string role = m.role == 0 ? T("Host", "Anfitrião") : m.role == 1 ? T("In the lobby", "No lobby") : T("On the way", "Chegando");
        if (m.tier == 1) role += " · Supporter"; // CR-S69: the badge
        players.entries.push_back({m.gamertag, role, m.role == 0 ? PaneIcon::Host : m.tier == 1 ? PaneIcon::Supporter : PaneIcon::Player});
    }
    if (players.entries.empty()) players.entries.push_back({w.host, T("Host", "Anfitrião"), PaneIcon::Host});
    p.sections.push_back(std::move(players));
    if (w.modded) {
        PaneSection mods{1, T("MODS", "MODS"), {}};
        for (const auto &m : w.mods) mods.entries.push_back({m.id, std::string(T("Version ", "Versão ")) + m.version, PaneIcon::Mod});
        p.sections.push_back(std::move(mods));
    } else {
        p.sections.push_back({0, T("MODS", "MODS"), {{T("Mods", "Mods"), T("None · vanilla", "Nenhum · vanilla"), PaneIcon::None}}});
    }
    return p;
}

// ---------------------------------------------------------------- screens
void DrawList(ImDrawList *dl, float x, float y, float w, float bottom, const Shared &sh) {
    g_items.clear();
    if (!DevHost() && !sh.access.Allowed(GetTickCount64())) {
        Wrapped(dl, f.body, ImVec2(x, y), w, kAmber, AccessMessage(sh));
        return;
    }
    const bool ready = GameReady();
    const char *load_hint = T("Load your game to access lobbies", "Carregue sua partida para acessar os lobbies");
    // filter chips
    // none selected: every lobby (CR-L40); Friends, Close to me and Global narrow it
    const char *chips[kLastChip + 1] = {FilterName(WorldFilter::Friends), FilterName(WorldFilter::Near), FilterName(WorldFilter::Global),
                                        T("Hide full", "Sem cheios")};
    float cx = x;
    const float ch = 36 * s;
    for (int i = 0; i <= kLastChip; ++i) {
        const ImVec2 ts = Measure(f.small, chips[i]);
        const float cw = ts.x + 26 * s;
        const bool wrapped = i > 0 && cx + cw > x + w;
        if (wrapped) {
            cx = x;
            y += ch + 8 * s;
        }
        const ImVec2 a(cx, y), b(cx + cw, y + ch);
        const bool on = i < 3 ? g_filter == static_cast<WorldFilter>(i + 1) : g_hide_full;
        const bool focus = g_focus == 0 && g_chip == i;
        dl->AddRectFilled(a, b, on ? kPinkU : kTile, ch / 2);
        if (focus) FocusRing(dl, a, b, ch / 2);
        if (i == 3 && !wrapped) {
            dl->AddLine(ImVec2(a.x - 7 * s, y + 6 * s), ImVec2(a.x - 7 * s, b.y - 6 * s), Rgba(255, 255, 255, 0.12f));
        }
        TextAt(dl, f.small, ImVec2(a.x + 13 * s, y + (ch - ts.y) / 2), kText, chips[i]);
        if (Clicked(a, b)) {
            g_focus = 0;
            g_chip = i;
            g_accept = true;
        }
        cx = b.x + (i == 2 ? 16 * s : 8 * s);
    }
    g_items.push_back({Item::Chips});
    y += ch + 18 * s;

    const float th = 66 * s, gap = 8 * s;
    auto focus_of = [&](int index) { return g_focus == index; };
    // create and code tiles
    {
        const int index = static_cast<int>(g_items.size());
        const ImVec2 a(x, y), b(x + w, y + th);
        Tile(dl, a, b, focus_of(index), kIconAddFriend, T("Create a lobby", "Criar um lobby"), ready ? T("Pick who can join, players and latency", "Escolha quem entra, jogadores e latência") : load_hint, !ready);
        if (Clicked(a, b)) { g_focus = index; g_accept = true; }
        g_items.push_back({Item::CreateTile});
        y += th + gap;
    }
    {
        const int index = static_cast<int>(g_items.size());
        const ImVec2 a(x, y), b(x + w, y + th);
        Tile(dl, a, b, focus_of(index), kIconLock, T("Enter a lobby code", "Digitar o código de um lobby"), ready ? T("For private and friends lobbies", "Para lobbies privados e de amigos") : load_hint, !ready);
        if (Clicked(a, b)) { g_focus = index; g_accept = true; }
        g_items.push_back({Item::CodeTile});
        y += th + 16 * s;
    }

    if (g_filter == WorldFilter::None) {
        const char *hint = T("Pick a filter to find lobbies.", "Escolha um filtro para encontrar lobbies.");
        TextAt(dl, f.body, ImVec2(x, y), kTextDim, hint);
        return;
    }
    if (sh.list_status == 1) {
        const char *dots = (static_cast<int>(ImGui::GetTime() * 3) % 3 == 0) ? "·" : (static_cast<int>(ImGui::GetTime() * 3) % 3 == 1) ? "· ·" : "· · ·";
        TextAt(dl, f.body, ImVec2(x, y), kTextDim, (std::string(T("Looking for lobbies ", "Procurando lobbies ")) + dots).c_str());
        return;
    }
    if (sh.list_status == 2) {
        Wrapped(dl, f.body, ImVec2(x, y), w, kRed, sh.list_message.empty() ? T("Could not load the lobbies.", "Não foi possível carregar os lobbies.") : sh.list_message.c_str());
        return;
    }
    // CR-L40: every row the launcher sent, in its order; the list scrolls inside [top, bottom)
    const auto &rows = sh.rows;
    const float wh = 64 * s, step = wh + gap;
    const bool any_matching = std::any_of(rows.begin(), rows.end(), [](const WorldRow &r) { return WhyOf(r) == Why::None; });
    if (!any_matching) {
        TextAt(dl, f.body, ImVec2(x, y), kTextDim, rows.empty() ? T("No lobbies here right now. Create your own!", "Nenhum lobby aqui agora. Crie o seu!")
                                                                : T("No lobby you can join right now. Create your own!", "Nenhum lobby em que você possa entrar agora. Crie o seu!"));
        y += 36 * s;
    }
    const float top = y, view = std::max(0.0f, bottom - top);
    const float content = rows.empty() ? 0 : rows.size() * step - gap;
    const int first_row = static_cast<int>(g_items.size());
    if (!PaneModal() && ImGui::IsMouseHoveringRect(ImVec2(x, top), ImVec2(x + w, bottom))) g_scroll -= ImGui::GetIO().MouseWheel * step;
    if (g_focus >= first_row && g_focus < first_row + static_cast<int>(rows.size())) {
        // the focused row is always in view
        const float row_top = (g_focus - first_row) * step;
        if (row_top < g_scroll) g_scroll = row_top;
        if (row_top + wh > g_scroll + view) g_scroll = row_top + wh - view;
    }
    g_scroll = std::clamp(g_scroll, 0.0f, std::max(0.0f, content - view));
    const int local_vanilla = LocalVanilla(rows);
    const WorldRow *hovered = nullptr, *focused = nullptr;
    dl->PushClipRect(ImVec2(x - 4 * s, top), ImVec2(x + w + 4 * s, bottom), true);
    for (size_t i = 0; i < rows.size(); ++i) {
        const int index = static_cast<int>(g_items.size());
        g_items.push_back({Item::Row, static_cast<int>(i)});
        const float ry = top + i * step - g_scroll;
        if (focus_of(index)) focused = &rows[i];
        if (ry + wh <= top || ry >= bottom) continue; // out of view: still walked by the focus
        const ImVec2 a(x, ry), b(x + w, ry + wh);
        WorldTile(dl, a, b, focus_of(index), rows[i], local_vanilla);
        const bool inside = ImGui::GetIO().MousePos.y >= top && ImGui::GetIO().MousePos.y < bottom;
        if (inside && Clicked(a, b)) { g_focus = index; g_accept = true; }
        if (inside && ImGui::IsMouseHoveringRect(a, b)) hovered = &rows[i];
    }
    dl->PopClipRect();
    if (content > view && view > 0) {
        // the scroll's position, a thin bar at the right edge
        const float bar = std::max(24 * s, view * view / content);
        const float by = top + (view - bar) * (g_scroll / (content - view));
        dl->AddRectFilled(ImVec2(x + w + 6 * s, by), ImVec2(x + w + 9 * s, by + bar), Rgba(255, 255, 255, 0.25f), 2 * s);
    }
    // the right pane: the row under the mouse, else the focused one (before A joins it)
    if (const WorldRow *shown = hovered ? hovered : focused) SetHoverPane(&LobbyPane(*shown));
}

void AcceptList(const Shared &sh) {
    if (!WorldAccessAllowed()) return;
    if (g_focus < 0 || g_focus >= static_cast<int>(g_items.size())) return;
    const Item item = g_items[g_focus];
    switch (item.kind) {
    case Item::Chips:
        if (g_chip < 3) {
            // the active filter again: back to every lobby (CR-L40)
            const auto chosen = static_cast<WorldFilter>(g_chip + 1);
            g_filter = g_filter == chosen ? WorldFilter::All : chosen;
        } else {
            g_hide_full = !g_hide_full;
        }
        Query();
        break;
    case Item::CreateTile: StartCreate(); break;
    case Item::CodeTile: JoinByCode(); break;
    case Item::Row: {
        if (item.row < 0 || item.row >= static_cast<int>(sh.rows.size())) break;
        const WorldRow &r = sh.rows[item.row];
        switch (WhyOf(r)) {
        case Why::None: Join(r.id, std::string()); break;
        case Why::Code: JoinByCode(); break;            // a code row opens the code entry
        case Why::Mods:
            // OD-112: only the exact same mods join (a vanilla lobby: none); the server refuses it too
            PushToast(ToastKind::Warning, T("Your mods don't match", "Seus mods não batem"), MismatchText(r));
            break;
        case Why::Mode:
            PushToast(ToastKind::Info, r.name, r.lock == 7 ? T("This lobby is for Hardcore profiles.", "Este lobby é para perfis Hardcore.")
                                                           : T("This lobby is for Noob profiles.", "Este lobby é para perfis Noob."));
            break;
        case Why::Full: PushToast(ToastKind::Info, r.name, T("This lobby is full. Try another one.", "Esse lobby está cheio. Tente outro.")); break;
        default: PushToast(ToastKind::Info, r.name, LockName(r.lock)); break;
        }
        break;
    }
    }
}

float Panel(ImDrawList *dl, float x, float y, const char *title) {
    TextAt(dl, f.title, ImVec2(x, y), kText, title);
    return y + 42 * s;
}

void Button(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, bool primary, const char *text) {
    dl->AddRectFilled(a, b, primary ? kPinkU : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const ImVec2 ts = Measure(f.body, text);
    TextAt(dl, f.body, ImVec2((a.x + b.x - ts.x) / 2, (a.y + b.y - ts.y) / 2), kText, text);
}

int g_button = 0; // a two-button screen's focus: 0 the primary

void DrawWarning(ImDrawList *dl, float x, float y, float w) {
    y = Panel(dl, x, y, T("Before you play online", "Antes de jogar online"));
    dl->AddText(f.icon_big, f.icon_big->FontSize, ImVec2(x, y), kAmber, kIconWarning);
    y += 52 * s;
    const char *text = T(
        "Online play exchanges data with other players. Bugs and crashes are expected in public lobbies with unknown players. "
        "Infinity mitigates problems where possible, but cannot prevent every problem.\n\nPrefer playing with people you trust.",
        "Jogar online troca dados com outros jogadores. Bugs e travamentos são esperados em lobbies públicos com jogadores desconhecidos. "
        "O Infinity mitiga os problemas quando possível, mas não consegue evitar todos eles.\n\nPrefira jogar com pessoas de confiança.");
    Wrapped(dl, f.body, ImVec2(x, y), w, kText, text);
    y += WrappedHeight(f.body, w, text) + 28 * s;
    const float bh = 56 * s;
    const ImVec2 a1(x, y), b1(x + w, y + bh);
    Button(dl, a1, b1, g_button == 0, true, T("I understand, continue", "Entendi, continuar"));
    if (Clicked(a1, b1)) { g_button = 0; g_accept = true; }
    y += bh + 10 * s;
    const ImVec2 a2(x, y), b2(x + w, y + bh);
    Button(dl, a2, b2, g_button == 1, false, T("Back", "Voltar"));
    if (Clicked(a2, b2)) { g_button = 1; g_accept = true; }
}

void DrawBusy(ImDrawList *dl, float x, float y, float w) {
    y += 60 * s;
    const float t = static_cast<float>(ImGui::GetTime());
    const ImVec2 c(x + w / 2, y + 30 * s);
    dl->PathArcTo(c, 26 * s, t * 5.0f, t * 5.0f + 4.2f, 32);
    dl->PathStroke(kPinkU, 0, 4 * s);
    const ImVec2 ts = Measure(f.body, g_busy.c_str());
    TextAt(dl, f.body, ImVec2(x + (w - ts.x) / 2, y + 80 * s), kText, g_busy.c_str());
}

void DrawCreate(ImDrawList *dl, float x, float y, float w) {
    y = Panel(dl, x, y, T("Create a lobby", "Criar um lobby"));
    const float fh = 66 * s, gap = 8 * s;
    const std::string players = std::to_string(g_create.players);
    struct Field { const char *label; std::string value; bool choice; } fields[] = {
        {T("Name", "Nome"), g_create.name, false},
        {T("Who can join", "Quem pode entrar"), AudienceName(g_create.audience), true},
        {T("Players", "Jogadores"), players, true},
        {T("Latency limit", "Limite de latência"), LatencyName(g_create.latency), true},
        {T("Modes · set when it opens", "Modos · definidos ao abrir"), ModesName(g_create.any_mode), true}, // OD-141
    };
    static_assert(sizeof(fields) / sizeof(fields[0]) == kCreateButton, "one field per row before the button");
    for (int i = 0; i < kCreateButton; ++i) {
        const ImVec2 a(x, y), b(x + w, y + fh);
        const bool focus = g_create.field == i;
        dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
        if (focus) FocusRing(dl, a, b, 6 * s);
        TextAt(dl, f.small, ImVec2(a.x + 18 * s, a.y + 9 * s), kTextDim, fields[i].label);
        dl->PushClipRect(a, ImVec2(b.x - 40 * s, b.y), true);
        TextAt(dl, f.body, ImVec2(a.x + 18 * s, a.y + 32 * s), kText, fields[i].value.c_str());
        dl->PopClipRect();
        if (fields[i].choice) {
            // ‹ › arrows: left and right change it
            dl->AddTriangleFilled(ImVec2(b.x - 34 * s, a.y + fh / 2), ImVec2(b.x - 26 * s, a.y + fh / 2 - 7 * s), ImVec2(b.x - 26 * s, a.y + fh / 2 + 7 * s), focus ? kText : kTextDim);
            dl->AddTriangleFilled(ImVec2(b.x - 10 * s, a.y + fh / 2), ImVec2(b.x - 18 * s, a.y + fh / 2 - 7 * s), ImVec2(b.x - 18 * s, a.y + fh / 2 + 7 * s), focus ? kText : kTextDim);
        } else {
            dl->AddText(f.icon, f.icon->FontSize * 0.8f, ImVec2(b.x - 34 * s, a.y + fh / 2 - f.icon->FontSize * 0.4f), kTextDim, kIconKeyboard);
        }
        if (Clicked(a, b)) { g_create.field = i; g_accept = true; }
        y += fh + gap;
    }
    y += 8 * s;
    const ImVec2 a(x, y), b(x + w, y + 56 * s);
    Button(dl, a, b, g_create.field == kCreateButton, true, T("Create lobby", "Criar lobby"));
    if (Clicked(a, b)) { g_create.field = kCreateButton; g_accept = true; }
}

void DrawHosted(ImDrawList *dl, float x, float y, float w, const Shared &sh) {
    y = Panel(dl, x, y, T("Your lobby is open", "Seu lobby está aberto"));
    TextAt(dl, f.body, ImVec2(x, y), kText, g_create.name.c_str());
    y += 30 * s;
    const std::string kind = std::string(AudienceName(g_create.audience)) + (g_create.any_mode ? std::string(" · ") + kAnyModeBadge : std::string());
    TextAt(dl, f.small, ImVec2(x, y), kTextDim, kind.c_str());
    y += 40 * s;
    if (!sh.hosted.code.empty()) {
        TextAt(dl, f.small, ImVec2(x, y), kTextDim, T("Share this code to let players in:", "Compartilhe este código para os jogadores entrarem:"));
        y += 28 * s;
        const std::string code = FormatCode(sh.hosted.code);
        const float size = 52 * s;
        const ImVec2 cs = f.heading->CalcTextSizeA(size, FLT_MAX, 0, code.c_str());
        const ImVec2 a(x, y), b(x + w, y + cs.y + 28 * s);
        dl->AddRectFilled(a, b, IM_COL32(8, 8, 10, 255), 8 * s);
        dl->AddRect(a, b, kPinkU, 8 * s, 0, 2 * s);
        dl->AddText(f.heading, size, ImVec2(x + (w - cs.x) / 2, y + 14 * s), kText, code.c_str());
        y = b.y + 24 * s;
    }
    Wrapped(dl, f.body, ImVec2(x, y), w, kText, T("Go to Free Roam Online in the game's menu to start your lobby.",
                                                  "Vá em Free Roam Online no menu do jogo para começar seu lobby."));
    y += 70 * s;
    const ImVec2 a(x, y), b(x + w, y + 56 * s);
    Button(dl, a, b, true, true, T("OK", "OK"));
    if (Clicked(a, b)) g_accept = true;
}

// ---------------------------------------------------------------- my world (OD-63)
const char *MemberStatus(const MemberRow &m) {
    switch (m.role) {
    case 0: return T("Host", "Anfitrião");
    case 1: return T("In the lobby", "No lobby");
    default: return T("On the way", "Chegando");
    }
}

const char *FriendStatus(uint8_t state) {
    switch (state) {
    case 1: return T("Friend", "Amigo");
    case 2: return T("Friend request sent", "Pedido de amizade enviado");
    case 3: return T("Wants to be your friend", "Quer ser seu amigo");
    case 4: return T("You", "Você");
    default: return "";
    }
}

void MemberTile(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const MemberRow &m) {
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float th = b.y - a.y;
    dl->AddText(f.icon, f.icon->FontSize, ImVec2(a.x + 18 * s, a.y + th / 2 - f.icon->FontSize / 2), m.friend_state == 4 ? kPinkU : kText, kIconContact);
    const float lx = a.x + 60 * s;
    dl->PushClipRect(ImVec2(lx, a.y), ImVec2(b.x - 44 * s, b.y), true);
    TextAt(dl, f.body, ImVec2(lx, a.y + 9 * s), kText, m.gamertag.c_str());
    std::string sub = MemberStatus(m);
    if (const char *fs = FriendStatus(m.friend_state); *fs) sub += std::string(" · ") + fs;
    TextAt(dl, f.small, ImVec2(lx, a.y + 36 * s), m.friend_state == 3 ? kPinkU : kTextDim, sub.c_str());
    dl->PopClipRect();
    if (m.tier == 1) {
        dl->AddText(f.icon, f.icon->FontSize * 0.8f, ImVec2(b.x - 36 * s, a.y + th / 2 - f.icon->FontSize * 0.4f), kPinkU, kIconStar);
    }
}

void DrawMine(ImDrawList *dl, float x, float y, float w, float bottom, const MyWorld &m) {
    g_mine_role = m.role;
    const char *tag = m.role == 1 ? T("YOUR LOBBY", "SEU LOBBY") : m.role == 2 ? T("SEAT RESERVED", "LUGAR RESERVADO") : T("YOU ARE IN A LOBBY", "VOCÊ ESTÁ EM UM LOBBY");
    TextAt(dl, f.tag, ImVec2(x, y), kPinkU, tag);
    y += 24 * s;
    dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, y + 44 * s), true);
    TextAt(dl, f.title, ImVec2(x, y), kText, m.name.c_str());
    dl->PopClipRect();
    y += 42 * s;
    // OD-141: the lobby's modes are fixed while it is open, so they are shown, never offered
    const std::string sub = (m.role == 1 ? std::string() : m.host + " · ") + AudienceName(m.audience) +
                            (m.any_mode ? std::string(" · ") + kAnyModeBadge : std::string()) + " · " + std::to_string(m.players) + "/" +
                            std::to_string(m.max) + T(" players", " jogadores");
    TextAt(dl, f.small, ImVec2(x, y), kTextDim, sub.c_str());
    y += 28 * s;
    if (m.role == 1 && !m.code.empty()) {
        const std::string code = std::string(T("Code ", "Código ")) + FormatCode(m.code);
        TextAt(dl, f.body, ImVec2(x, y), kText, code.c_str());
        y += 32 * s;
    }
    if (m.role == 2) {
        Wrapped(dl, f.small, ImVec2(x, y), w, kAmber, T("Go to Free Roam Online in the game's menu to join it.", "Vá em Free Roam Online no menu do jogo para entrar nele."));
        y += 30 * s;
    }
    y += 6 * s;
    const float bh = 52 * s;
    const ImVec2 a(x, y), b(x + w, y + bh);
    Button(dl, a, b, g_mine_focus == 0, false, m.role == 1 ? T("Close lobby", "Fechar lobby") : T("Leave lobby", "Sair do lobby"));
    if (Clicked(a, b)) { g_mine_focus = 0; g_accept = true; }
    y += bh + 20 * s;

    TextAt(dl, f.tag, ImVec2(x, y), kTextDim, T("PLAYERS", "JOGADORES"));
    y += 24 * s;
    if (!m.named) {
        Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, T("Player names arrive with the next server update.", "Os nomes dos jogadores chegam na próxima atualização do servidor."));
        y += 30 * s;
    }
    const float th = 62 * s, gap = 8 * s;
    int shown = 0;
    for (size_t i = 0; i < m.members.size() && y + th < bottom; ++i, ++shown) {
        const ImVec2 ta(x, y), tb(x + w, y + th);
        MemberTile(dl, ta, tb, g_mine_focus == static_cast<int>(i) + 1, m.members[i]);
        if (Clicked(ta, tb)) { g_mine_focus = static_cast<int>(i) + 1; g_accept = true; }
        y += th + gap;
    }
    g_mine_count = 1 + shown;
}

// The card's choices: an action number (3 kick, 4 friend, 5 report, 0 back) and its label.
struct Choice {
    int action;
    const char *label;
    bool enabled;
};

std::vector<Choice> MemberChoices(const MemberRow &m) {
    std::vector<Choice> out;
    if (g_mine_role == 1 && m.role != 0) out.push_back({3, T("Remove from my lobby", "Remover do meu lobby"), true});
    switch (m.friend_state) {
    case 0: out.push_back({4, T("Add friend", "Adicionar amigo"), true}); break;
    case 2: out.push_back({4, T("Friend request sent", "Pedido de amizade enviado"), false}); break;
    case 3: out.push_back({4, T("Accept friend request", "Aceitar pedido de amizade"), true}); break;
    default: break;
    }
    out.push_back({5, T("Report", "Denunciar"), true});
    out.push_back({0, T("Back", "Voltar"), true});
    return out;
}

void Buttons(ImDrawList *dl, float x, float y, float w, const std::vector<Choice> &choices) {
    const float bh = 52 * s;
    for (size_t i = 0; i < choices.size(); ++i) {
        const ImVec2 a(x, y), b(x + w, y + bh);
        Button(dl, a, b, g_button == static_cast<int>(i), false, choices[i].label);
        if (!choices[i].enabled) dl->AddRectFilled(a, b, Rgba(0, 0, 0, 0.45f), 6 * s);
        if (Clicked(a, b)) { g_button = static_cast<int>(i); g_accept = true; }
        y += bh + 8 * s;
    }
    g_buttons = static_cast<int>(choices.size());
}

void DrawMember(ImDrawList *dl, float x, float y, float w) {
    TextAt(dl, f.tag, ImVec2(x, y), kTextDim, T("PLAYER", "JOGADOR"));
    y += 24 * s;
    dl->AddText(f.icon_big, f.icon_big->FontSize, ImVec2(x, y), g_member.tier == 1 ? kPinkU : kText, kIconContact);
    const float lx = x + f.icon_big->FontSize + 16 * s;
    TextAt(dl, f.title, ImVec2(lx, y), kText, g_member.gamertag.c_str());
    std::string sub = MemberStatus(g_member);
    if (g_member.tier == 1) sub += " · Supporter"; // CR-S69: the badge
    if (const char *fs = FriendStatus(g_member.friend_state); *fs) sub += std::string(" · ") + fs;
    TextAt(dl, f.small, ImVec2(lx, y + 40 * s), kTextDim, sub.c_str());
    y += 84 * s;
    Buttons(dl, x, y, w, MemberChoices(g_member));
}

std::vector<Choice> ReportChoices() {
    return {{1, T("Cheating", "Trapaça"), true}, {2, T("Abuse or harassment", "Abuso ou assédio"), true}, {3, T("Something else", "Outro motivo"), true},
            {0, T("Back", "Voltar"), true}};
}

void DrawReport(ImDrawList *dl, float x, float y, float w) {
    y = Panel(dl, x, y, T("Report a player", "Denunciar um jogador"));
    TextAt(dl, f.body, ImVec2(x, y), kText, g_member.gamertag.c_str());
    y += 34 * s;
    const char *text = T("Why are you reporting this player? Moderators review every report.", "Por que você está denunciando este jogador? Os moderadores analisam cada denúncia.");
    Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, text);
    y += WrappedHeight(f.small, w, text) + 18 * s;
    Buttons(dl, x, y, w, ReportChoices());
}

void DrawConfirm(ImDrawList *dl, float x, float y, float w) {
    const char *title = g_confirm == 1 ? T("Close your lobby?", "Fechar seu lobby?") : g_confirm == 2 ? T("Leave this lobby?", "Sair deste lobby?")
                                                                                      : T("Remove this player?", "Remover este jogador?");
    const char *text = g_confirm == 1 ? T("Everyone in it goes back to their own lobby.", "Todos que estão nele voltam para o próprio lobby.")
                       : g_confirm == 2 ? T("Your seat is let go. Leave Free Roam Online in the game's menu too.", "Seu lugar é liberado. Saia também do Free Roam Online no menu do jogo.")
                                        : T("They leave your lobby now and cannot join it again while it is open.", "Ele sai do seu lobby agora e não pode voltar enquanto ele estiver aberto.");
    y = Panel(dl, x, y, title);
    if (g_confirm == 3) {
        TextAt(dl, f.body, ImVec2(x, y), kText, g_member.gamertag.c_str());
        y += 34 * s;
    }
    Wrapped(dl, f.body, ImVec2(x, y), w, kText, text);
    y += WrappedHeight(f.body, w, text) + 24 * s;
    const char *yes = g_confirm == 1 ? T("Close lobby", "Fechar lobby") : g_confirm == 2 ? T("Leave lobby", "Sair do lobby") : T("Remove", "Remover");
    Buttons(dl, x, y, w, {{1, yes, true}, {0, T("Back", "Voltar"), true}});
}
} // namespace

// ---------------------------------------------------------------- public
void SetWorldAccess(bool allowed, const std::string &message) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.access.Observe(allowed, GetTickCount64());
    g_shared.access_message = message;
}

void SetWorlds(WorldFilter filter, bool modded, uint8_t status, const std::string &message, std::vector<WorldRow> rows) {
    std::lock_guard<std::mutex> lock(g_lock);
    if (filter != g_shared.filter || modded != g_shared.modded) {
        return; // an answer to a filter the player already left
    }
    g_shared.list_status = status;
    g_shared.list_message = message;
    g_shared.rows = std::move(rows);
    ++g_shared.list_serial;
}

void SetJoinResult(uint8_t status, const std::string &message) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.join_status = status;
    g_shared.join_message = message;
    ++g_shared.join_serial;
}

void SetHosted(const HostedWorld &hosted) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.hosted = hosted;
    ++g_shared.hosted_serial;
}

void SetMyWorld(MyWorld mine) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.mine = std::move(mine);
    ++g_shared.mine_serial;
}

void SetActionResult(uint8_t action, uint8_t status, const std::string &message) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.action = action;
    g_shared.action_status = status;
    g_shared.action_message = message;
    ++g_shared.action_serial;
}

void WorldsReset() {
    // the guide opened again: back to the list (the filter and its results stay); my world asked at once
    if (g_screen != Screen::Busy && g_screen != Screen::Hosted) {
        g_screen = Screen::List;
    }
    g_mine_refresh = true;
    g_queried = false; // a fresh list
}

bool WorldsCanRefresh() { return g_screen == Screen::List && WorldAccessAllowed(); }

bool WorldsNavigate(Nav nav) {
    switch (g_screen) {
    case Screen::List:
        if (g_showing_mine) {
            switch (nav) {
            case Nav::Up: if (g_mine_focus > 0) --g_mine_focus; return true;
            case Nav::Down: if (g_mine_focus + 1 < g_mine_count) ++g_mine_focus; return true;
            case Nav::Accept: g_accept = true; return true;
            case Nav::Y: g_mine_refresh = true; return true; // refresh
            default: return false; // the tabs, close
            }
        }
        switch (nav) {
        case Nav::Up: if (g_focus > 0) --g_focus; return true;
        case Nav::Down: if (g_focus + 1 < static_cast<int>(g_items.size())) ++g_focus; return true;
        case Nav::Left:
            if (g_focus == 0 && g_chip > 0) { --g_chip; return true; }
            return false; // the tabs
        case Nav::Right:
            if (g_focus == 0 && g_chip < kLastChip) { ++g_chip; return true; }
            return false;
        case Nav::Accept: g_accept = true; return true;
        case Nav::Y: Query(); return true; // refresh
        default: return false;
        }
    case Screen::Warning:
        switch (nav) {
        case Nav::Up: g_button = 0; return true;
        case Nav::Down: g_button = 1; return true;
        case Nav::Accept: g_accept = true; return true;
        case Nav::Back: g_screen = Screen::List; g_after_warning = nullptr; return true;
        default: return true;
        }
    case Screen::Busy: return nav != Nav::Back || true; // waits for the launcher
    case Screen::Create:
        switch (nav) {
        case Nav::Up: if (g_create.field > 0) --g_create.field; return true;
        case Nav::Down: if (g_create.field < kCreateButton) ++g_create.field; return true;
        case Nav::Left:
        case Nav::Right: {
            const int d = nav == Nav::Right ? 1 : -1;
            if (g_create.field == 1) {
                g_create.audience = (g_create.audience + d + 4) % 4;
            } else if (g_create.field == 2) {
                g_create.players = std::clamp(g_create.players + d, 2, 12);
            } else if (g_create.field == 3) {
                g_create.latency = (g_create.latency + d + 4) % 4;
            } else if (g_create.field == kCreateModes) {
                g_create.any_mode = !g_create.any_mode; // two choices: either arrow switches
            }
            return true;
        }
        case Nav::Accept: g_accept = true; return true;
        case Nav::Back: g_screen = Screen::List; return true;
        case Nav::Menu: SubmitCreate(); return true;
        default: return true;
        }
    case Screen::Hosted:
        if (nav == Nav::Accept || nav == Nav::Back) g_accept = true;
        return true;
    case Screen::Member:
    case Screen::Report:
    case Screen::Confirm:
        switch (nav) {
        case Nav::Up: if (g_button > 0) --g_button; return true;
        case Nav::Down: if (g_button + 1 < g_buttons) ++g_button; return true;
        case Nav::Accept: g_accept = true; return true;
        case Nav::Back:
            g_screen = g_screen == Screen::Report || (g_screen == Screen::Confirm && g_confirm == 3) ? Screen::Member : Screen::List;
            g_button = 0;
            return true;
        default: return true;
        }
    }
    return false;
}

void DrawWorldsTab(ImDrawList *dl, float x, float y, float w, float bottom, float dt) {
    const char *warning = T(
        "Public lobbies: bugs and crashes are expected with unknown players. Infinity mitigates problems where possible. Prefer people you trust.",
        "Lobbies públicos: bugs e travamentos são esperados com jogadores desconhecidos. O Infinity mitiga os problemas quando possível. Prefira pessoas de confiança.");
    const float warning_height = WrappedHeight(f.small, w, warning);
    const float warning_y = bottom - warning_height;
    Wrapped(dl, f.small, ImVec2(x, warning_y), w, kAmber, warning);
    bottom = warning_y - 16 * s;
    dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, bottom), true);
    Shared sh;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        sh = g_shared;
    }
    const bool world_access = DevHost() || sh.access.Allowed(GetTickCount64());
    if (!world_access && (g_screen == Screen::Warning || g_screen == Screen::Create || g_screen == Screen::Hosted
        || (g_screen == Screen::Busy && !g_world_action_busy))) {
        g_screen = Screen::List;
        g_after_warning = nullptr;
        g_accept = false;
    }
    if (g_dev_wait >= 0) {
        g_dev_wait -= dt;
        if (g_dev_wait < 0) DevAnswer();
    }
    // answers from the launcher
    if (sh.join_serial != g_seen_join) {
        g_seen_join = sh.join_serial;
        if (g_screen == Screen::Busy) {
            if (sh.join_status == 0 && world_access) {
                Admitted(false); // the seat's prompt
            } else if (sh.join_status == 3) {
                // a wrong code: back on the code screen, with the reason
                g_screen = Screen::List;
                OpenCodeEntry(T("Enter the lobby code", "Digite o código do lobby"), "0123456789", [](bool join, const std::string &code) {
                    if (join) Join(std::string(), code);
                });
                CodeEntryRefused(sh.join_message);
            } else {
                g_screen = Screen::List;
                PushToast(ToastKind::Warning, T("Could not join", "Não foi possível entrar"), sh.join_message);
                if (sh.join_status == 1 || sh.join_status == 4) {
                    Query(); // full or closed: the list was older than the world
                }
            }
        }
    }
    if (sh.hosted_serial != g_seen_hosted) {
        g_seen_hosted = sh.hosted_serial;
        if (g_screen == Screen::Busy) {
            if (sh.hosted.status == 0 && world_access) {
                Admitted(true); // the Hosted screen
            } else {
                g_screen = Screen::Create;
                PushToast(ToastKind::Warning, T("Could not create the lobby", "Não foi possível criar o lobby"), sh.hosted.message);
            }
        }
    }
    if (sh.list_serial != g_seen_list) {
        g_seen_list = sh.list_serial;
        g_focus = std::min(g_focus, 2 + static_cast<int>(kMaxLobbyRows));
    }
    // The launcher owns active/pending reconciliation. An open tab must not create a second poll loop.
    if (g_mine_refresh) {
        g_mine_refresh = false;
        if (DevHost()) {
            if (g_dev_wait < 0) { g_dev_wait = 0.05f; g_dev_kind = 6; }
        } else {
            SendMyWorldQuery();
        }
    }
    if (sh.mine_serial != g_seen_mine) {
        g_seen_mine = sh.mine_serial;
        if (sh.mine.role == 0 && (g_screen == Screen::Member || g_screen == Screen::Report || g_screen == Screen::Confirm)) {
            g_screen = Screen::List; // the world ended under the card
        }
        if (g_screen == Screen::Member) {
            // the card follows the player (friend state, role); gone: back to the world
            const auto it = std::find_if(sh.mine.members.begin(), sh.mine.members.end(), [](const MemberRow &m) { return m.xuid == g_member.xuid; });
            if (it == sh.mine.members.end()) g_screen = Screen::List; else g_member = *it;
        }
    }
    if (sh.action_serial != g_seen_action) {
        g_seen_action = sh.action_serial;
        if (g_screen == Screen::Busy) {
            g_screen = Screen::List;
            PushToast(sh.action_status == 0 ? ToastKind::Success : ToastKind::Warning,
                      sh.action_status == 0 ? T("Done", "Pronto") : T("Not done", "Não foi feito"), sh.action_message);
            g_mine_refresh = true;
        }
    }

    const bool accept = g_accept;
    g_accept = false;
    if (!world_access && (g_screen != Screen::List || sh.mine.role != 0)) {
        const char *message = AccessMessage(sh);
        Wrapped(dl, f.body, ImVec2(x, y), w, kAmber, message);
        y += WrappedHeight(f.body, w, message) + 16 * s;
    }
    switch (g_screen) {
    case Screen::List:
        g_showing_mine = sh.mine.role != 0;
        if (g_showing_mine) {
            DrawMine(dl, x, y, w, bottom, sh.mine);
            g_mine_focus = std::clamp(g_mine_focus, 0, std::max(0, g_mine_count - 1));
            if (accept) {
                if (g_mine_focus == 0) {
                    g_confirm = sh.mine.role == 1 ? 1 : 2;
                    g_button = 0;
                    g_screen = Screen::Confirm;
                } else if (g_mine_focus - 1 < static_cast<int>(sh.mine.members.size())) {
                    const MemberRow &m = sh.mine.members[g_mine_focus - 1];
                    if (m.friend_state != 4) {
                        g_member = m;
                        g_button = 0;
                        g_screen = Screen::Member;
                    }
                }
            }
            break;
        }
        if (!g_queried) {
            if (world_access) {
                g_queried = true;
                Query();
            }
        }
        DrawList(dl, x, y, w, bottom, sh);
        g_focus = std::clamp(g_focus, 0, std::max(0, static_cast<int>(g_items.size()) - 1));
        if (accept) AcceptList(sh);
        break;
    case Screen::Member: {
        DrawMember(dl, x, y, w);
        if (accept) {
            const auto choices = MemberChoices(g_member);
            if (g_button < static_cast<int>(choices.size()) && choices[g_button].enabled) {
                switch (choices[g_button].action) {
                case 3: g_confirm = 3; g_button = 0; g_screen = Screen::Confirm; break;
                case 4: Act(4, g_member, 0); break;
                case 5: g_button = 0; g_screen = Screen::Report; break;
                default: g_screen = Screen::List; break;
                }
            }
        }
        break;
    }
    case Screen::Report: {
        DrawReport(dl, x, y, w);
        if (accept) {
            const auto choices = ReportChoices();
            const int reason = g_button < static_cast<int>(choices.size()) ? choices[g_button].action : 0;
            if (reason == 0) { g_button = 0; g_screen = Screen::Member; } else { Act(5, g_member, static_cast<uint8_t>(reason)); }
        }
        break;
    }
    case Screen::Confirm:
        DrawConfirm(dl, x, y, w);
        if (accept) {
            if (g_button == 0) {
                Act(static_cast<uint8_t>(g_confirm), g_confirm == 3 ? g_member : MemberRow{}, 0);
            } else {
                g_screen = g_confirm == 3 ? Screen::Member : Screen::List;
            }
            g_button = 0;
        }
        break;
    case Screen::Warning:
        DrawWarning(dl, x, y, w);
        if (accept) {
            if (g_button == 0) {
                g_warned = true;
                g_screen = Screen::List;
                if (auto then = std::move(g_after_warning)) then();
            } else {
                g_screen = Screen::List;
            }
            g_button = 0;
        }
        break;
    case Screen::Busy: DrawBusy(dl, x, y, w); break;
    case Screen::Create:
        DrawCreate(dl, x, y, w);
        if (accept) {
            if (g_create.field == 0) {
                KeyboardRequest r;
                r.title = T("Lobby name", "Nome do lobby");
                r.initial = g_create.name;
                r.max = 48;
                OpenKeyboard(r, [](bool ok, const std::string &text) {
                    if (ok && !text.empty()) g_create.name = text;
                });
            } else if (g_create.field == kCreateModes) {
                g_create.any_mode = !g_create.any_mode; // A (or a click) switches it too
            } else if (g_create.field == kCreateButton) {
                SubmitCreate();
            }
        }
        break;
    case Screen::Hosted:
        DrawHosted(dl, x, y, w, sh);
        if (accept) g_screen = Screen::List;
        break;
    }
    dl->PopClipRect();
}

} // namespace fi
