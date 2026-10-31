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
// - Pineapple lobbies (OD-149, 2.0.1): a world hosted by a Hardcore player the anti-cheat system isolated carries the
//   pineapple mark and its name in the list; focused, the right pane opens with the notice that explains it. A clean player
//   sees it dimmed and cannot join it (lock 9). An isolated player gets the red notice with the time left counting down above
//   the list (and above its own lobby), with the Discord link to dispute it. Neutral words only (ui_frames.h, OD-149).
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
#include "voice_audio.h"
#include "voice_volume.h"
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
    // OD-149: this player is isolated by the anti-cheat system until this tick (GetTickCount64; 0 not isolated), from the list
    uint64_t pool_deadline = 0;
    std::string pool_link;
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
int g_mine_focus = 0;       // 0 the close or leave button, [1 the host's voice switch], then the players
int g_mine_count = 0;       // what the focus walks: 1 [+ 1] + the players
int g_mine_col = 0;         // on a player: 0 the name, 1 its voice bar below it (OD-154; §15: the mic button + volume)
int g_mine_scroll = 0;      // §15: the first player row drawn (the rows follow the focus)
bool g_mine_voice_row = false; // the host's "Voice chat" switch is item 1
bool g_mine_x = false;      // X on a player: mute or unmute them, applied at draw time
MemberRow g_member;         // the player the card is about
uint8_t g_mine_role = 0;    // of the world last drawn
int g_confirm = 0;          // the action to confirm: 1 close, 2 leave, 3 kick
int g_buttons = 0;          // how many buttons the current card has
uint64_t g_seen_mine = 0, g_seen_action = 0;
int g_dev_mine = 0;         // the dev host's own world: 0 none, 1 hosting, 3 seated

// a world created (the Hosted screen) or a seat reserved (where the host is, from the launcher's answer)
void Admitted(bool hosted, const std::string &message = std::string()) {
    if (hosted) {
        g_screen = Screen::Hosted;
    } else {
        g_screen = Screen::List;
        PushToast(ToastKind::Success, T("Seat reserved", "Lugar reservado"),
                  message.empty() ? T("Pick the host's online mode in the game's menu", "Escolha no menu do jogo o modo online do host")
                                  : message.c_str());
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
    bool voice = true;     // OD-154: the lobby's voice chat, on by default; the host changes it any time
    int field = 0;    // 0 name, 1 audience, 2 players, 3 latency, 4 modes, 5 voice chat, 6 create
} g_create;
constexpr int kCreateModes = 4, kCreateVoice = 5, kCreateButton = 6;
bool g_last_voice = true; // the host's last choice is the next lobby's default (this game session)

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

// OD-154: the lobby's voice chat switch
const char *VoiceName(bool on) { return on ? T("On", "Ligado") : T("Off", "Desligado"); }
const char *VoiceLabel(bool on) { return on ? T("Voice chat on", "Chat de voz ligado") : T("Voice chat off", "Chat de voz desligado"); }

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
    case kLockPool: return T("Isolated players only", "Só jogadores isolados");
    default: return "";
    }
}

// OD-149: a pineapple world (hosted by a player the anti-cheat system isolated); the owner's "abacaxi" lobbies
bool Pineapple(const WorldRow &w) { return (w.kind & kKindPineapple) != 0; }
const char *PineappleName() { return T("Pineapple lobby", "Lobby abacaxi"); }
const char *PineappleExplained() {
    return T("Isolated by the anti-cheat system. Players it isolates play only in pineapple lobbies until their time ends; only they can join them.",
             "Isolado pelo sistema anti-trapaça. Os jogadores que ele isola jogam só em lobbies abacaxi até o tempo acabar; só eles podem entrar.");
}

// Why a row is not "matching" (CR-L40 §2.2), the first that applies; None: the player joins it as they are.
enum class Why { None, Mode, Mods, Friends, Code, Full, Far, Pool, Other };

Why WhyOf(const WorldRow &w) {
    if ((w.lock == 7 || w.lock == 8) && !w.any_mode) return Why::Mode; // a lobby open to both modes is never another mode's
    if (w.lock == 7 || w.lock == 8) return Why::Other;
    if (w.lock == kLockPool) return Why::Pool; // OD-149: a pineapple world and a clean player
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
        g_create.voice = g_last_voice;
        g_screen = Screen::Create;
    });
}

void SubmitCreate() {
    if (!GameReady() || !WorldAccessAllowed()) { g_screen = Screen::List; return; }
    g_world_action_busy = false;
    g_last_voice = g_create.voice;
    g_busy = T("Creating your lobby...", "Criando seu lobby...");
    g_screen = Screen::Busy;
    if (DevHost()) {
        g_dev_wait = 0.8f;
        g_dev_kind = 4;
    } else {
        SendCreateWorld(g_create.name, static_cast<uint8_t>(g_create.audience), static_cast<uint8_t>(g_create.players),
                        static_cast<uint8_t>(g_create.latency), g_create.any_mode, g_create.voice);
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
        g_shared.rows.push_back({"p1", "Night cruise", "Synthetic#00512", 3, 8, kKindPineapple, 0, kLockPool, 1, false}); // OD-149
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

// OD-149: the pineapple mark, drawn (the icon font has no pineapple): a golden fruit with three leaves, centred on `c`.
void PineappleMark(ImDrawList *dl, ImVec2 c, float size) {
    const ImU32 fruit = IM_COL32(255, 196, 0, 255), leaf = IM_COL32(72, 180, 72, 255), seed = IM_COL32(176, 112, 0, 255);
    const float rx = size * 0.30f, ry = size * 0.36f;
    const ImVec2 body(c.x, c.y + size * 0.14f);
    const float top = body.y - ry * 0.85f;
    dl->AddTriangleFilled(ImVec2(c.x, c.y - size * 0.50f), ImVec2(c.x - size * 0.12f, top), ImVec2(c.x + size * 0.12f, top), leaf);
    dl->AddTriangleFilled(ImVec2(c.x - size * 0.30f, c.y - size * 0.36f), ImVec2(c.x - size * 0.14f, top + size * 0.04f), ImVec2(c.x + size * 0.02f, top), leaf);
    dl->AddTriangleFilled(ImVec2(c.x + size * 0.30f, c.y - size * 0.36f), ImVec2(c.x - size * 0.02f, top), ImVec2(c.x + size * 0.14f, top + size * 0.04f), leaf);
    dl->AddEllipseFilled(body, ImVec2(rx, ry), fruit);
    for (int row = -1; row <= 1; ++row) {     // the fruit's eyes, a small diamond grid inside the ellipse
        for (int col = -1; col <= 1; ++col) {
            if ((row + col) % 2 != 0) continue;
            dl->AddCircleFilled(ImVec2(body.x + col * rx * 0.5f, body.y + row * ry * 0.5f), size * 0.045f, seed);
        }
    }
}

// The red notice of a player the anti-cheat system isolated (OD-149), with the time left counting down every frame and the
// dispute link (the overlay opens nothing: the link is shown to type or click in the launcher). Returns the y below it.
float PoolBanner(ImDrawList *dl, float x, float y, float w, const Shared &sh) {
    const uint64_t now = GetTickCount64();
    if (sh.pool_deadline <= now) return y;
    const uint64_t left = (sh.pool_deadline - now + 999) / 1000;
    const std::string title = std::string(T("Isolated by the anti-cheat system for ", "Isolado pelo sistema anti-trapaça por ")) + PoolTimeLeft(left);
    std::string body = T("Until then you play only in pineapple lobbies.", "Até lá, você só joga em lobbies abacaxi.");
    if (!sh.pool_link.empty())
        body += std::string(T(" Think this is a mistake? Dispute it on Discord: ", " Acha que é um engano? Conteste no Discord: ")) + sh.pool_link;
    const float pad = 12 * s, icon = 30 * s, tw = w - 2 * pad - icon;
    const float th = WrappedHeight(f.body, tw, title.c_str()), bh = WrappedHeight(f.small, tw, body.c_str());
    const ImVec2 a(x, y), b(x + w, y + pad + th + 6 * s + bh + pad);
    dl->AddRectFilled(a, b, Rgba(232, 17, 35, 0.22f), 6 * s);
    dl->AddRect(a, b, kRed, 6 * s, 0, 2 * s);
    dl->AddText(f.icon, f.icon->FontSize * 0.8f, ImVec2(x + pad, y + pad), kRed, kIconWarning);
    Wrapped(dl, f.body, ImVec2(x + pad + icon, y + pad), tw, kText, title.c_str());
    Wrapped(dl, f.small, ImVec2(x + pad + icon, y + pad + th + 6 * s), tw, kText, body.c_str());
    return b.y + 14 * s;
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
    // OD-149: a pineapple world carries the pineapple mark before its name
    const float nx = Pineapple(w) ? lx + 24 * s : lx;
    if (Pineapple(w)) PineappleMark(dl, ImVec2(lx + 9 * s, a.y + 22 * s), 22 * s);
    dl->PushClipRect(ImVec2(lx, a.y), ImVec2(b.x - 92 * s, b.y), true);
    TextAt(dl, f.body, ImVec2(nx, a.y + 10 * s), ready ? kText : kTextDim, w.name.c_str());
    std::string sub = w.host;
    if (w.kind & kKindSupporter) sub += " · Supporter"; // CR-S69: the host's badge, nothing more
    if (Pineapple(w)) sub += std::string(" · ") + PineappleName();
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
    if (Pineapple(w)) {
        // OD-149: the large notice of a focused pineapple world, first in the pane
        p.subtitle = std::string(PineappleName()) + " · " + p.subtitle;
        p.sections.push_back({1, T("PINEAPPLE LOBBY", "LOBBY ABACAXI"), {{T("Isolated by the anti-cheat system", "Isolado pelo sistema anti-trapaça"),
                                                                          PineappleExplained(), PaneIcon::Warning}}});
        if (WhyOf(w) == Why::Pool)
            p.sections.back().entries.push_back({T("You cannot join it", "Você não pode entrar"),
                                                 T("Only players isolated by the anti-cheat system can join this lobby.",
                                                   "Só jogadores isolados pelo sistema anti-trapaça podem entrar neste lobby."),
                                                 PaneIcon::None});
    }
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
    y = PoolBanner(dl, x, y, w, sh); // OD-149: the list's header while this player is isolated
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
        case Why::Pool: // OD-149: a clean player and a pineapple world
            PushToast(ToastKind::Info, PineappleName(), T("Only players isolated by the anti-cheat system can join this lobby.",
                                                          "Só jogadores isolados pelo sistema anti-trapaça podem entrar neste lobby."));
            break;
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
        {T("Voice chat · you can change it later", "Chat de voz · dá para mudar depois"), VoiceName(g_create.voice), true}, // OD-154
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
    const std::string kind = std::string(AudienceName(g_create.audience)) + (g_create.any_mode ? std::string(" · ") + kAnyModeBadge : std::string()) +
                             " · " + VoiceLabel(g_create.voice);
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

// ---------------------------------------------------------------- voice chat in my world (OD-154)
int FirstPlayerItem() { return g_mine_voice_row ? 2 : 1; }

// Why the microphone squares do nothing now, or null when they work.
const char *VoiceUnavailable(const VoiceState &v) {
    if (!(v.flags & VoiceState::LobbyKnown)) return T("The host's voice chat setting has not arrived yet.", "A configuração de voz do anfitrião ainda não chegou.");
    if (!(v.flags & VoiceState::LobbyOn))
        return v.flags & VoiceState::MayChange ? T("Voice chat is off in your lobby. Turn it on above.", "O chat de voz está desligado no seu lobby. Ligue acima.")
                                               : T("The host turned voice chat off in this lobby.", "O anfitrião desligou o chat de voz neste lobby.");
    if (!(v.flags & VoiceState::Enabled)) return T("Voice chat is off in your settings (Settings > Voice chat).", "O chat de voz está desligado nos seus ajustes (Ajustes > Chat de voz).");
    return nullptr;
}

// A: the player's own square switches the microphone between open and muted; anyone else's mutes or unmutes them.
void ToggleVoice(const MemberRow &m, const VoiceState &v) {
    if (const char *why = VoiceUnavailable(v)) {
        PushToast(ToastKind::Info, T("Voice chat", "Chat de voz"), why);
        return;
    }
    if (m.friend_state == 4) {
        const MicMode next = v.mic == MicMode::Muted ? MicMode::Open : MicMode::Muted;
        VoiceCommand(VoiceOp::Mic, std::string(), static_cast<uint8_t>(next));
        PushToast(ToastKind::Info, T("Microphone", "Microfone"), MicModeName(next));
        return;
    }
    const bool muted = v.StateOf(m.xuid) & VoiceMember::MutedByMe;
    VoiceCommand(VoiceOp::Mute, m.xuid, muted ? 0 : 1);
    PushToast(ToastKind::Info, muted ? T("Unmuted", "Som liberado") : T("Muted", "Silenciado"), m.gamertag);
}

void ToggleLobbyVoice(const VoiceState &v) {
    const bool on = !v.lobby_on();
    VoiceCommand(VoiceOp::LobbyVoice, std::string(), on ? 1 : 0);
    PushToast(ToastKind::Info, VoiceLabel(on), on ? T("Everyone in your lobby can talk.", "Todos no seu lobby podem falar.")
                                                  : T("Nobody's voice is carried in your lobby.", "Nenhuma voz é transmitida no seu lobby."));
}

// Heard talking now (the green border around the name, §15): another player's talking state, or this player's own voice
// leaving the PC; never while voice is unusable or the player is silenced.
bool RowTalking(const MemberRow &m, const VoiceState &v) {
    if (VoiceUnavailable(v)) return false;
    if (m.friend_state == 4) return v.mic != MicMode::Muted && audio::Sending();
    const uint8_t st = v.StateOf(m.xuid);
    return (st & VoiceMember::Talking) && !(st & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked));
}

// §15: the row's volume: another player's (this player's level for them) or, on this player's own row, the microphone's.
uint8_t RowVolume(const MemberRow &m, const VoiceState &v) { return m.friend_state == 4 ? v.mic_volume : v.VolumeOf(m.xuid); }
void SetRowVolume(const MemberRow &m, uint8_t to) {
    const VoiceState v = CurrentVoice();
    if (!VolumeValue(to) || to == RowVolume(m, v)) return;
    if (m.friend_state == 4) VoiceCommand(VoiceOp::MicVolume, std::string(), to);
    else VoiceCommand(VoiceOp::PlayerVolume, m.xuid, to);
}

// The microphone button inside a player's voice bar: their voice at a glance, and A to mute them.
void MicSquare(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const MemberRow &m, const VoiceState &v) {
    const bool self = m.friend_state == 4, usable = VoiceUnavailable(v) == nullptr;
    const uint8_t st = v.StateOf(m.xuid);
    const bool talking = RowTalking(m, v);
    dl->AddRectFilled(a, b, focus ? Rgba(70, 70, 78) : Rgba(52, 52, 58), 5 * s);
    bool slash = false;
    ImU32 color = kText;
    if (!usable) {
        slash = true;
        color = Rgba(255, 255, 255, 0.28f);
    } else if (self) {
        slash = v.mic == MicMode::Muted;
        color = slash ? kRed : v.mic == MicMode::PushToTalk ? kTextDim : kText;
    } else if (st & (VoiceMember::Blocked | VoiceMember::MutedForLobby)) {
        slash = true;
        color = kAmber;
    } else if (st & VoiceMember::MutedByMe) {
        slash = true;
        color = kRed;
    } else if (talking) {
        color = kTalkGreen;
    }
    const ImVec2 c((a.x + b.x) / 2, (a.y + b.y) / 2);
    DrawMic(dl, c, (b.y - a.y) * 0.62f, color, slash, focus ? Rgba(70, 70, 78) : Rgba(52, 52, 58));
    if (usable && self && v.mic == MicMode::PushToTalk) {
        const ImVec2 ts = Measure(f.tag, "R3");
        TextAt(dl, f.tag, ImVec2(b.x - ts.x - 3 * s, b.y - ts.y - 1 * s), kTextDim, "R3");
    }
}

// §15: the shorter rectangle under a player's name: the microphone button (A mutes them; on this player's own row it
// opens or mutes the microphone) and beside it that player's volume (Left/Right; on the own row, "My microphone
// volume"), its value at the right. One focus stop. `mic_clicked`/`volume_clicked` report the mouse.
void VoiceBar(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const MemberRow &m, const VoiceState &v, bool *mic_clicked, uint8_t *volume_clicked) {
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float h = b.y - a.y, pad = 4 * s;
    const ImVec2 ma(a.x + pad, a.y + pad), mb(a.x + h - pad, b.y - pad);
    MicSquare(dl, ma, mb, focus, m, v);
    if (Clicked(ma, mb)) *mic_clicked = true;
    const uint8_t volume = RowVolume(m, v);
    const std::string value = std::to_string(volume);
    const ImVec2 vs = Measure(f.small, value.c_str());
    const ImVec2 sa(mb.x + 14 * s, a.y + h / 2 - 8 * s), sb(b.x - 22 * s - Measure(f.small, "100").x, a.y + h / 2 + 8 * s);
    const uint8_t st = v.StateOf(m.xuid);
    const bool heard = !VoiceUnavailable(v) && (m.friend_state == 4 ? v.mic != MicMode::Muted : !(st & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked)));
    VolumeSlider(dl, sa, sb, volume, focus && heard, volume_clicked);
    TextAt(dl, f.small, ImVec2(b.x - 12 * s - vs.x, a.y + h / 2 - vs.y / 2), focus ? kText : kTextDim, value.c_str());
}

// The host's switch, under the close button: on or off for everyone, at any time while the lobby is open.
void VoiceSwitch(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const VoiceState &v) {
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float h = b.y - a.y;
    DrawMic(dl, ImVec2(a.x + 30 * s, a.y + h / 2), 26 * s, v.lobby_on() ? kText : kTextDim, !v.lobby_on(), focus ? kTileFocus : kTile);
    TextAt(dl, f.small, ImVec2(a.x + 58 * s, a.y + 7 * s), kTextDim, T("Voice chat · for everyone in your lobby", "Chat de voz · para todos no seu lobby"));
    TextAt(dl, f.body, ImVec2(a.x + 58 * s, a.y + 27 * s), kText, VoiceName(v.lobby_on()));
    // the switch itself, on the right
    const ImVec2 sa(b.x - 64 * s, a.y + h / 2 - 12 * s), sb(b.x - 18 * s, a.y + h / 2 + 12 * s);
    dl->AddRectFilled(sa, sb, v.lobby_on() ? kPinkU : Rgba(255, 255, 255, 0.18f), 12 * s);
    const float knob = v.lobby_on() ? sb.x - 12 * s : sa.x + 12 * s;
    dl->AddCircleFilled(ImVec2(knob, a.y + h / 2), 9 * s, kText, 24);
}

void DrawMine(ImDrawList *dl, float x, float y, float w, float bottom, const MyWorld &m) {
    g_mine_role = m.role;
    const VoiceState voice = CurrentVoice();
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
    if (m.role != 1) { // the host changes it below; everyone else reads it here (OD-154)
        y += 20 * s;
        const char *said = voice.flags & VoiceState::LobbyKnown ? VoiceLabel(voice.lobby_on()) : T("Voice chat: set by the host", "Chat de voz: definido pelo anfitrião");
        TextAt(dl, f.small, ImVec2(x, y), voice.lobby_on() ? kText : kTextDim, said);
    }
    if (voice.lobby_on() && (voice.flags & VoiceState::Enabled) && audio::ListenOnly()) { // only monitors, or no input
        y += 20 * s;
        TextAt(dl, f.small, ImVec2(x, y), kAmber, T("No microphone: you can listen", "Sem microfone: você pode ouvir"));
    }
    y += 28 * s;
    if (m.role == 1 && !m.code.empty()) {
        const std::string code = std::string(T("Code ", "Código ")) + FormatCode(m.code);
        TextAt(dl, f.body, ImVec2(x, y), kText, code.c_str());
        y += 32 * s;
    }
    if (m.role == 2) {
        Wrapped(dl, f.small, ImVec2(x, y), w, kAmber, T("Pick the host's online mode in the game's menu to join it.", "Escolha no menu do jogo o modo online do host para entrar nele."));
        y += 30 * s;
    }
    y += 6 * s;
    const float bh = 52 * s;
    const ImVec2 a(x, y), b(x + w, y + bh);
    Button(dl, a, b, g_mine_focus == 0, false, m.role == 1 ? T("Close lobby", "Fechar lobby") : T("Leave lobby", "Sair do lobby"));
    if (Clicked(a, b)) { g_mine_focus = 0; g_accept = true; }
    y += bh + 10 * s;
    g_mine_voice_row = m.role == 1;
    if (g_mine_voice_row) {
        const ImVec2 va(x, y), vb(x + w, y + 58 * s);
        VoiceSwitch(dl, va, vb, g_mine_focus == 1, voice);
        if (Clicked(va, vb)) { g_mine_focus = 1; g_accept = true; }
        y += 58 * s + 10 * s;
    }
    y += 10 * s;

    TextAt(dl, f.tag, ImVec2(x, y), kTextDim, T("PLAYERS", "JOGADORES"));
    y += 24 * s;
    if (!m.named) {
        Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, T("Player names arrive with the next server update.", "Os nomes dos jogadores chegam na próxima atualização do servidor."));
        y += 30 * s;
    }
    // OD-154, §15: each player is the name's rectangle (a green border while they talk) and, below it, a shorter one
    // with their microphone button and their volume. The rows scroll with the focus.
    const float nh = 56 * s, vh = 40 * s, inner = 4 * s, gap = 12 * s, indent = 60 * s;
    const float row = nh + inner + vh + gap;
    const int first = FirstPlayerItem();
    const int count = static_cast<int>(m.members.size());
    const int fit = std::max(1, static_cast<int>((bottom - y + gap) / row));
    const int focused = g_mine_focus - first;
    if (focused >= 0) g_mine_scroll = std::clamp(g_mine_scroll, focused - fit + 1, focused);
    g_mine_scroll = std::clamp(g_mine_scroll, 0, std::max(0, count - fit));
    if (g_mine_scroll > 0) { // more above
        const ImVec2 ts = Measure(f.tag, "...");
        TextAt(dl, f.tag, ImVec2(x + w - ts.x, y - 22 * s), kTextDim, "...");
    }
    for (int i = g_mine_scroll; i < count && i < g_mine_scroll + fit; ++i) {
        const MemberRow &member = m.members[static_cast<size_t>(i)];
        const int item = i + first;
        const bool focus = g_mine_focus == item;
        const ImVec2 ta(x, y), tb(x + w, y + nh), ba(x + indent, tb.y + inner), bb(x + w, tb.y + inner + vh);
        MemberTile(dl, ta, tb, focus && g_mine_col == 0, member);
        if (RowTalking(member, voice)) { // the whole name area, inside the focus ring when it has one
            const float in = focus && g_mine_col == 0 ? 3.5f * s : 0.0f;
            dl->AddRect(ImVec2(ta.x + in, ta.y + in), ImVec2(tb.x - in, tb.y - in), kTalkGreen, 6 * s, 0, 2.5f * s);
        }
        bool mic_clicked = false;
        uint8_t volume_clicked = 0;
        VoiceBar(dl, ba, bb, focus && g_mine_col == 1, member, voice, &mic_clicked, &volume_clicked);
        if (Clicked(ta, tb)) { g_mine_focus = item; g_mine_col = 0; g_accept = true; }
        if (mic_clicked) { g_mine_focus = item; g_mine_col = 1; g_accept = true; }
        if (volume_clicked) { g_mine_focus = item; g_mine_col = 1; SetRowVolume(member, volume_clicked); }
        y += row;
    }
    if (g_mine_scroll + fit < count) { // more below
        const ImVec2 ts = Measure(f.tag, "...");
        TextAt(dl, f.tag, ImVec2(x + w - ts.x, y - gap + 1 * s), kTextDim, "...");
    }
    g_mine_count = first + count;
}

// The card's choices: an action number (3 kick, 4 friend, 5 report, 0 back) and its label.
struct Choice {
    int action;
    const char *label;
    bool enabled;
};

std::vector<Choice> MemberChoices(const MemberRow &m) {
    std::vector<Choice> out;
    // OD-154: voice first, the most frequent reason to open a card in a loud lobby
    const VoiceState v = CurrentVoice();
    const uint8_t st = v.StateOf(m.xuid);
    out.push_back({6, st & VoiceMember::MutedByMe ? T("Unmute voice", "Liberar voz") : T("Mute voice", "Silenciar voz"), true});
    if (g_mine_role == 1 && m.role != 0)
        out.push_back({7, st & VoiceMember::MutedForLobby ? T("Unmute for the lobby", "Liberar para o lobby") : T("Mute for the lobby", "Silenciar para o lobby"), true});
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
                       : g_confirm == 2 ? T("Your seat is let go. Leave the online mode in the game's menu too.", "Seu lugar é liberado. Saia também do modo online no menu do jogo.")
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
    // OD-149: status 3 is a good list for an isolated player (its message the countdown and the dispute link); a failed list keeps
    // the last notice, any other good one clears it
    const PoolNotice pool = ParsePoolNotice(status, message);
    if (pool.isolated) {
        g_shared.pool_deadline = GetTickCount64() + static_cast<uint64_t>(pool.seconds) * 1000;
        g_shared.pool_link = pool.link;
        status = 0;
    } else if (status != 1 && status != 2) {
        g_shared.pool_deadline = 0;
        g_shared.pool_link.clear();
    }
    g_shared.list_status = status;
    g_shared.list_message = pool.isolated ? std::string() : message;
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

bool WorldsMuteHint() { return g_screen == Screen::List && g_showing_mine && g_mine_focus >= FirstPlayerItem(); }

bool WorldsNavigate(Nav nav) {
    switch (g_screen) {
    case Screen::List:
        if (g_showing_mine) {
            const bool on_player = g_mine_focus >= FirstPlayerItem(), on_switch = g_mine_voice_row && g_mine_focus == 1;
            // §15: a player is two stops, the name and below it the voice bar (A the microphone button, Left/Right the
            // volume); Up/Down walk name, bar, next name. Right on a name goes down to its bar.
            const auto step_volume = [](int direction) {
                Shared sh;
                { std::lock_guard<std::mutex> lock(g_lock); sh = g_shared; }
                const int player = g_mine_focus - FirstPlayerItem();
                if (player < 0 || player >= static_cast<int>(sh.mine.members.size())) return;
                const MemberRow &m = sh.mine.members[static_cast<size_t>(player)];
                SetRowVolume(m, audio::StepVolume(RowVolume(m, CurrentVoice()), direction));
            };
            switch (nav) {
            case Nav::Up:
                if (on_player && g_mine_col == 1) { g_mine_col = 0; return true; }
                if (g_mine_focus > 0) --g_mine_focus;
                g_mine_col = g_mine_focus >= FirstPlayerItem() ? 1 : 0;
                return true;
            case Nav::Down:
                if (on_player && g_mine_col == 0) { g_mine_col = 1; return true; }
                if (g_mine_focus + 1 < g_mine_count) { ++g_mine_focus; g_mine_col = 0; }
                return true;
            // OD-154: on the host's switch Left/Right flip it
            case Nav::Right:
                if (on_player && g_mine_col == 0) { g_mine_col = 1; return true; }
                if (on_player) { step_volume(1); return true; }
                if (on_switch) { ToggleLobbyVoice(CurrentVoice()); return true; }
                return false; // the tabs
            case Nav::Left:
                if (on_player && g_mine_col == 1) { step_volume(-1); return true; }
                if (on_switch) { ToggleLobbyVoice(CurrentVoice()); return true; }
                return false;
            case Nav::X: if (on_player) { g_mine_x = true; return true; } return false;
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
            } else if (g_create.field == kCreateVoice) {
                g_create.voice = !g_create.voice;
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
                Admitted(false, sh.join_message); // the seat's prompt: where the host is
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
            DrawMine(dl, x, PoolBanner(dl, x, y, w, sh), w, bottom, sh.mine); // OD-149: the notice stays above its own lobby
            g_mine_focus = std::clamp(g_mine_focus, 0, std::max(0, g_mine_count - 1));
            const int player = g_mine_focus - FirstPlayerItem();
            const bool x_press = g_mine_x;
            g_mine_x = false;
            if (x_press && player >= 0 && player < static_cast<int>(sh.mine.members.size())) {
                ToggleVoice(sh.mine.members[player], CurrentVoice()); // X: mute from the row, without the card
            }
            if (accept) {
                if (g_mine_focus == 0) {
                    g_confirm = sh.mine.role == 1 ? 1 : 2;
                    g_button = 0;
                    g_screen = Screen::Confirm;
                } else if (g_mine_voice_row && g_mine_focus == 1) {
                    ToggleLobbyVoice(CurrentVoice());
                } else if (player >= 0 && player < static_cast<int>(sh.mine.members.size()) && g_mine_col == 1) {
                    ToggleVoice(sh.mine.members[player], CurrentVoice());
                } else if (player >= 0 && player < static_cast<int>(sh.mine.members.size())) {
                    const MemberRow &m = sh.mine.members[player];
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
                case 6: ToggleVoice(g_member, CurrentVoice()); break; // the card stays: its label follows
                case 7: {
                    const bool muted = CurrentVoice().StateOf(g_member.xuid) & VoiceMember::MutedForLobby;
                    VoiceCommand(VoiceOp::MuteForLobby, g_member.xuid, muted ? 0 : 1);
                    PushToast(ToastKind::Info, muted ? T("Unmuted for the lobby", "Liberado para o lobby") : T("Muted for the lobby", "Silenciado para o lobby"),
                              g_member.gamertag);
                    break;
                }
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
            } else if (g_create.field == kCreateVoice) {
                g_create.voice = !g_create.voice;
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
