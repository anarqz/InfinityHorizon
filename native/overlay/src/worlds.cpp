// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: worlds (OD-54, OD-56, OD-57, OD-58). The guide's Worlds tab finds, joins
// and creates worlds; the launcher talks to matchmaking (the overlay holds no tokens) and this
// file only asks and shows.
//
// - The list: "Close to me" first, asked each time the guide opens (Friends, Close to me, Global;
//   "Hide full" on top of any; Y refreshes), at most 5 joinable worlds of the profile's mode. A
//   join answered "closed" or "full" refreshes it: the list was older than the world. A free player also sees
//   up to 3 Infinity Supporter worlds below, locked, under their own heading (OD-58): selecting
//   one explains the Supporter tier. No queue for worlds: a full world is just full.
// - Join reserves a seat only (matchmaking's handshake); the game's own Free Roam Online then
//   lands the player in it. Before the first join or create of the session, the not-rated
//   online-play warning (OD-54).
// - A private or friends world is joined with its code (code_entry.cpp); create gives the host
//   the code to share.
// - Create: name (the keyboard), audience (Everyone, Supporters, Friends, Private; a free host
//   gets Everyone or Private), players 2-12, latency limit.
// - My world (OD-63): while the profile hosts a world or holds a seat in one, the tab shows that
//   world instead of the list (updated by the launcher): the host closes it or kicks players,
//   a player leaves it; anyone adds a player as a friend or reports them with a reason. Player
//   names only, never their XUIDs (OD-61).
// - Syncing cars (OD-65 amendment): once a world is created or a seat reserved, the hook uploads
//   the current car's paint so world mates fetch it, behind a "Syncing cars" screen that takes no
//   input (the game already sees an idle pad while the guide is open), for at most 5 s; a timeout
//   or a failure is a warning, never a block. Then the prompt to go to Free Roam Online.
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

constexpr int kMaxJoinable = 5, kMaxLocked = 3;

// ---------------------------------------------------------------- what the launcher sent (any thread)
struct Shared {
    WorldAccessLease access;
    std::string access_message;
    WorldFilter filter = WorldFilter::None;
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
enum class Screen { List, Warning, Busy, Create, Hosted, Supporter, Member, Report, Confirm, Sync };
Screen g_screen = Screen::List;
WorldFilter g_filter = WorldFilter::Near; // the default filter (Alex, 2026-10-19)
bool g_queried = false;                    // the list was asked since the guide opened
bool g_hide_full = false;
bool g_warned = false; // the online-play warning, once per game session
int g_focus = 0;       // the list's focus: 0 the filter row, then the tiles
int g_chip = 1;        // in the filter row: 0 Friends, 1 Close to me, 2 Global, 3 Hide full
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

// syncing cars (OD-65 amendment)
constexpr uint32_t kSyncTimeoutMs = 5000;
constexpr float kSyncCap = 5.5f;       // the screen's own cap, should the hook not return
constexpr float kSyncMinShown = 0.8f;  // long enough to read, never a flash
float g_sync_time = 0;
bool g_sync_hosted = false;            // what follows: the Hosted screen, or the seat's prompt
std::atomic<int> g_sync_result{1000};  // 1000: still running
std::atomic<uint64_t> g_sync_serial{0};

void StartSync(bool hosted) {
    g_screen = Screen::Sync;
    g_sync_time = 0;
    g_sync_hosted = hosted;
    g_sync_result = 1000;
    const uint64_t serial = ++g_sync_serial;
    Log("livery: syncing the current car before the world");
    std::thread([serial] {
        const int result = LiverySync(kSyncTimeoutMs);
        if (g_sync_serial == serial) g_sync_result = result;
    }).detach();
}

void FinishSync(int result) {
    Log("livery: sync %s (%d)", result == FI_LIVERY_SYNCED ? "done" : result == FI_LIVERY_STOCK ? "not needed" : "not done", result);
    if (result != FI_LIVERY_SYNCED && result != FI_LIVERY_STOCK) {
        PushToast(ToastKind::Warning, T("Car paint not synced", "Pintura do carro não sincronizada"),
                  T("Other players may see your car in its standard colours.", "Outros jogadores podem ver seu carro nas cores de fábrica."));
    }
    if (g_sync_hosted) {
        g_screen = Screen::Hosted;
    } else {
        g_screen = Screen::List;
        PushToast(ToastKind::Success, T("Seat reserved", "Lugar reservado"),
                  T("Go to Free Roam Online to join the world", "Vá em Free Roam Online para entrar no mundo"));
        CloseSidebar();
    }
    g_mine_refresh = true;
}

struct Create {
    std::string name;
    int audience = 0; // WorldAudience
    int players = 8;
    int latency = 0;  // 0 any, 1 high (< 150 ms), 2 medium (< 100 ms), 3 low (< 60 ms)
    int field = 0;    // 0 name, 1 audience, 2 players, 3 latency, 4 create
} g_create;

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

const char *LockName(uint8_t lock) {
    switch (lock) {
    case 1: return T("Infinity Supporter", "Infinity Supporter");
    case 2: return T("Friends only", "Só amigos");
    case 3: return T("Needs a code", "Precisa de código");
    case 4: return T("Full", "Cheio");
    case 5: return T("Too far", "Longe demais");
    case 6: return T("Not available", "Indisponível");
    default: return "";
    }
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
        g_shared.list_status = 1; // loading
        g_shared.rows.clear();
        ++g_shared.list_serial;
    }
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
        OpenCodeEntry(T("Enter the world code", "Digite o código do mundo"), "0123456789", [](bool join, const std::string &code) {
            if (join) {
                g_warned = true; // already accepted on the way in
                Join(std::string(), code);
            }
        });
    });
}

void StartCreate(bool supporter) {
    if (!GameReady() || !WorldAccessAllowed()) return;
    WithWarning([supporter] {
        if (!GameReady() || !WorldAccessAllowed()) return;
        g_create = Create{};
        g_create.name = T("Sunday drive", "Passeio de domingo");
        g_create.audience = supporter ? 0 : 0;
        g_screen = Screen::Create;
    });
}

void SubmitCreate() {
    if (!GameReady() || !WorldAccessAllowed()) { g_screen = Screen::List; return; }
    g_world_action_busy = false;
    g_busy = T("Creating your world...", "Criando seu mundo...");
    g_screen = Screen::Busy;
    if (DevHost()) {
        g_dev_wait = 0.8f;
        g_dev_kind = 4;
    } else {
        SendCreateWorld(g_create.name, static_cast<uint8_t>(g_create.audience), static_cast<uint8_t>(g_create.players),
                        static_cast<uint8_t>(g_create.latency));
    }
}

// the dev host's fake launcher
void DevAnswer() {
    std::lock_guard<std::mutex> lock(g_lock);
    switch (g_dev_kind) {
    case 1: {
        g_shared.list_status = 0;
        g_shared.rows.clear();
        const bool free_player = true; // the demo plays a free player when the profile says so
        (void)free_player;
        g_shared.rows.push_back({"w1", "Sunset Cruise", "RallyFox98", 5, 8, 0, 0, 0, 1, true});
        g_shared.rows.push_back({"w2", "Nice coastline run", "Synthetic#00117", 3, 6, 0, 0, 0, 0, false});
        g_shared.rows.push_back({"w3", "Drift crew", "Synthetic#00230", 11, 12, 0, 0, 0, 2, false});
        if (!g_hide_full) {
            g_shared.rows.push_back({"w4", "Bucket list hunters", "Synthetic#00042", 12, 12, 0, 0, 4, 1, false});
        }
        g_shared.rows.push_back({"s1", "Supporters night race", "Synthetic#00301", 9, 12, 1, 1, 1, 0, false});
        g_shared.rows.push_back({"s2", "Italia full tour", "Synthetic#00302", 7, 10, 1, 1, 1, 1, false});
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
        g_shared.join_message = T("That code does not open any world", "Esse código não abre nenhum mundo");
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
        static const char *done[] = {"", "Your world is closed.", "You left the world.", "The player was removed from your world.", "Friend request sent.",
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

bool Clicked(ImVec2 a, ImVec2 b) { return ImGui::IsMouseHoveringRect(a, b) && ImGui::IsMouseClicked(0); }

struct Item {
    enum Kind { Chips, CreateTile, CodeTile, Row, LockedRow } kind;
    int row = -1;
};

std::vector<Item> g_items; // what the focus walks, rebuilt every frame
bool g_accept = false;     // A, applied at draw time on the focused item

std::vector<WorldRow> Joinable(const std::vector<WorldRow> &rows) {
    std::vector<WorldRow> out;
    for (const auto &r : rows) {
        if (r.lock != 1 && static_cast<int>(out.size()) < kMaxJoinable) out.push_back(r);
    }
    return out;
}

std::vector<WorldRow> Locked(const std::vector<WorldRow> &rows) {
    std::vector<WorldRow> out;
    for (const auto &r : rows) {
        if (r.lock == 1 && static_cast<int>(out.size()) < kMaxLocked) out.push_back(r);
    }
    return out;
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

void WorldTile(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const WorldRow &w) {
    const bool locked = w.lock != 0 || !GameReady();
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float lx = a.x + 18 * s;
    dl->PushClipRect(ImVec2(lx, a.y), ImVec2(b.x - 92 * s, b.y), true);
    TextAt(dl, f.body, ImVec2(lx, a.y + 10 * s), locked ? kTextDim : kText, w.name.c_str());
    std::string sub = w.host;
    if (w.friend_host) sub = std::string(T("Friend · ", "Amigo · ")) + sub;
    if (locked) sub += std::string(" · ") + LockName(w.lock);
    if (!GameReady()) sub = T("Load your game to access worlds", "Carregue sua partida para acessar os mundos");
    TextAt(dl, f.small, ImVec2(lx, a.y + 38 * s), w.lock == 1 ? kPinkU : kTextDim, sub.c_str());
    dl->PopClipRect();
    const std::string count = std::to_string(w.players) + "/" + std::to_string(w.max);
    const ImVec2 cs = Measure(f.body, count.c_str());
    TextAt(dl, f.body, ImVec2(b.x - 50 * s - cs.x, a.y + 10 * s), w.players >= w.max ? kRed : kText, count.c_str());
    PingBars(dl, ImVec2(b.x - 38 * s, a.y + 14 * s), w.ping);
    if (w.lock == 1) {
        dl->AddText(f.icon, f.icon->FontSize * 0.75f, ImVec2(b.x - 36 * s, a.y + 40 * s), kPinkU, kIconLock);
    }
}

// ---------------------------------------------------------------- screens
void DrawList(ImDrawList *dl, float x, float y, float w, float bottom, bool supporter, const Shared &sh) {
    g_items.clear();
    if (!DevHost() && !sh.access.Allowed(GetTickCount64())) {
        Wrapped(dl, f.body, ImVec2(x, y), w, kAmber, AccessMessage(sh));
        return;
    }
    const bool ready = GameReady();
    const char *load_hint = T("Load your game to access worlds", "Carregue sua partida para acessar os mundos");
    // filter chips
    const char *chips[4] = {FilterName(WorldFilter::Friends), FilterName(WorldFilter::Near), FilterName(WorldFilter::Global), T("Hide full", "Sem cheios")};
    float cx = x;
    const float ch = 36 * s;
    for (int i = 0; i < 4; ++i) {
        const ImVec2 ts = Measure(f.small, chips[i]);
        const float cw = ts.x + 26 * s;
        const ImVec2 a(cx, y), b(cx + cw, y + ch);
        const bool on = i < 3 ? g_filter == static_cast<WorldFilter>(i + 1) : g_hide_full;
        const bool focus = g_focus == 0 && g_chip == i;
        dl->AddRectFilled(a, b, on ? kPinkU : kTile, ch / 2);
        if (focus) FocusRing(dl, a, b, ch / 2);
        if (i == 3) {
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
        Tile(dl, a, b, focus_of(index), kIconAddFriend, T("Create a world", "Criar um mundo"), ready ? T("Pick who can join, players and latency", "Escolha quem entra, jogadores e latência") : load_hint, !ready);
        if (Clicked(a, b)) { g_focus = index; g_accept = true; }
        g_items.push_back({Item::CreateTile});
        y += th + gap;
    }
    {
        const int index = static_cast<int>(g_items.size());
        const ImVec2 a(x, y), b(x + w, y + th);
        Tile(dl, a, b, focus_of(index), kIconLock, T("Enter a world code", "Digitar o código de um mundo"), ready ? T("For private and friends worlds", "Para mundos privados e de amigos") : load_hint, !ready);
        if (Clicked(a, b)) { g_focus = index; g_accept = true; }
        g_items.push_back({Item::CodeTile});
        y += th + 16 * s;
    }

    if (g_filter == WorldFilter::None) {
        const char *hint = T("Pick a filter to find worlds.", "Escolha um filtro para encontrar mundos.");
        TextAt(dl, f.body, ImVec2(x, y), kTextDim, hint);
        return;
    }
    if (sh.list_status == 1) {
        const char *dots = (static_cast<int>(ImGui::GetTime() * 3) % 3 == 0) ? "·" : (static_cast<int>(ImGui::GetTime() * 3) % 3 == 1) ? "· ·" : "· · ·";
        TextAt(dl, f.body, ImVec2(x, y), kTextDim, (std::string(T("Looking for worlds ", "Procurando mundos ")) + dots).c_str());
        return;
    }
    if (sh.list_status == 2) {
        Wrapped(dl, f.body, ImVec2(x, y), w, kRed, sh.list_message.empty() ? T("Could not load the worlds.", "Não foi possível carregar os mundos.") : sh.list_message.c_str());
        return;
    }
    const auto joinable = Joinable(sh.rows);
    const auto locked = supporter ? std::vector<WorldRow>{} : Locked(sh.rows);
    const float wh = 64 * s;
    if (joinable.empty()) {
        TextAt(dl, f.body, ImVec2(x, y), kTextDim, T("No worlds here right now. Create your own!", "Nenhum mundo aqui agora. Crie o seu!"));
        y += 36 * s;
    }
    for (size_t i = 0; i < joinable.size() && y + wh < bottom; ++i) {
        const int index = static_cast<int>(g_items.size());
        const ImVec2 a(x, y), b(x + w, y + wh);
        WorldTile(dl, a, b, focus_of(index), joinable[i]);
        if (Clicked(a, b)) { g_focus = index; g_accept = true; }
        g_items.push_back({Item::Row, static_cast<int>(i)});
        y += wh + gap;
    }
    if (!locked.empty() && y + 40 * s < bottom) {
        y += 10 * s;
        TextAt(dl, f.tag, ImVec2(x, y), kPinkU, T("INFINITY SUPPORTER WORLDS", "MUNDOS INFINITY SUPPORTER"));
        y += 24 * s;
        for (size_t i = 0; i < locked.size() && y + wh < bottom; ++i) {
            const int index = static_cast<int>(g_items.size());
            const ImVec2 a(x, y), b(x + w, y + wh);
            WorldTile(dl, a, b, focus_of(index), locked[i]);
            if (Clicked(a, b)) { g_focus = index; g_accept = true; }
            g_items.push_back({Item::LockedRow, static_cast<int>(i)});
            y += wh + gap;
        }
    }
}

void AcceptList(bool supporter, const Shared &sh) {
    if (!WorldAccessAllowed()) return;
    if (g_focus < 0 || g_focus >= static_cast<int>(g_items.size())) return;
    const Item item = g_items[g_focus];
    switch (item.kind) {
    case Item::Chips:
        if (g_chip < 3) {
            g_filter = static_cast<WorldFilter>(g_chip + 1);
        } else {
            g_hide_full = !g_hide_full;
        }
        Query();
        break;
    case Item::CreateTile: StartCreate(supporter); break;
    case Item::CodeTile: JoinByCode(); break;
    case Item::Row: {
        const auto rows = Joinable(sh.rows);
        if (item.row < static_cast<int>(rows.size())) {
            const WorldRow &r = rows[item.row];
            if (r.lock == 4) {
                PushToast(ToastKind::Info, r.name, T("This world is full. Try another one.", "Esse mundo está cheio. Tente outro."));
            } else if (r.lock == 3) {
                JoinByCode();
            } else if (r.lock != 0) {
                PushToast(ToastKind::Info, r.name, LockName(r.lock));
            } else {
                Join(r.id, std::string());
            }
        }
        break;
    }
    case Item::LockedRow: g_screen = Screen::Supporter; break;
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
        "Online play exchanges data with other players. Bugs and crashes are expected in public worlds with unknown players. "
        "Infinity mitigates problems where possible, but cannot prevent every problem.\n\nPrefer playing with people you trust.",
        "Jogar online troca dados com outros jogadores. Bugs e travamentos são esperados em mundos públicos com jogadores desconhecidos. "
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

void DrawSync(ImDrawList *dl, float x, float y, float w) {
    y += 70 * s;
    const char *title = T("Syncing cars...", "Sincronizando carros...");
    const ImVec2 ts = Measure(f.title, title);
    TextAt(dl, f.title, ImVec2(x + (w - ts.x) / 2, y), kText, title);
    y += 64 * s;
    const float t = static_cast<float>(ImGui::GetTime());
    const ImVec2 c(x + w / 2, y + 28 * s);
    dl->PathArcTo(c, 26 * s, t * 5.0f, t * 5.0f + 4.2f, 32);
    dl->PathStroke(kPinkU, 0, 4 * s);
    y += 80 * s;
    const char *hint = T("Sending your car's paint so the other players see it.", "Enviando a pintura do seu carro para os outros jogadores verem.");
    Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, hint);
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

void DrawCreate(ImDrawList *dl, float x, float y, float w, bool supporter) {
    y = Panel(dl, x, y, T("Create a world", "Criar um mundo"));
    const float fh = 66 * s, gap = 8 * s;
    const std::string players = std::to_string(g_create.players);
    struct Field { const char *label; std::string value; bool choice; } fields[] = {
        {T("Name", "Nome"), g_create.name, false},
        {T("Who can join", "Quem pode entrar"), AudienceName(g_create.audience), true},
        {T("Players", "Jogadores"), players, true},
        {T("Latency limit", "Limite de latência"), LatencyName(g_create.latency), true},
    };
    for (int i = 0; i < 4; ++i) {
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
    if (!supporter) {
        Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, T("Infinity Supporters can also open worlds to Supporters only or to friends.",
                                                          "Infinity Supporters também podem abrir mundos só para Supporters ou para amigos."));
        y += 44 * s;
    } else {
        y += 8 * s;
    }
    const ImVec2 a(x, y), b(x + w, y + 56 * s);
    Button(dl, a, b, g_create.field == 4, true, T("Create world", "Criar mundo"));
    if (Clicked(a, b)) { g_create.field = 4; g_accept = true; }
}

void DrawHosted(ImDrawList *dl, float x, float y, float w, const Shared &sh) {
    y = Panel(dl, x, y, T("Your world is open", "Seu mundo está aberto"));
    TextAt(dl, f.body, ImVec2(x, y), kText, g_create.name.c_str());
    y += 30 * s;
    TextAt(dl, f.small, ImVec2(x, y), kTextDim, AudienceName(g_create.audience));
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
    Wrapped(dl, f.body, ImVec2(x, y), w, kText, T("Go to Free Roam Online in the game's menu to start your world.",
                                                  "Vá em Free Roam Online no menu do jogo para começar seu mundo."));
    y += 70 * s;
    const ImVec2 a(x, y), b(x + w, y + 56 * s);
    Button(dl, a, b, true, true, T("OK", "OK"));
    if (Clicked(a, b)) g_accept = true;
}

void DrawSupporter(ImDrawList *dl, float x, float y, float w) {
    y = Panel(dl, x, y, T("Infinity Supporter worlds", "Mundos Infinity Supporter"));
    dl->AddText(f.icon_big, f.icon_big->FontSize, ImVec2(x, y), kPinkU, kIconStar);
    y += 52 * s;
    const char *text = T("These worlds are open to Infinity Supporters. Supporters keep Infinity's servers running, and get worlds for "
                         "Supporters, friends and player profiles, cloud saves and leaderboards.\n\nBecome a Supporter in the Infinity launcher.",
                         "Esses mundos são abertos para Infinity Supporters. Os Supporters mantêm os servidores do Infinity no ar e ganham mundos "
                         "para Supporters, amigos e perfis de jogadores, saves na nuvem e placares.\n\nTorne-se um Supporter pelo launcher do Infinity.");
    Wrapped(dl, f.body, ImVec2(x, y), w, kText, text);
    y += WrappedHeight(f.body, w, text) + 28 * s;
    const ImVec2 a(x, y), b(x + w, y + 56 * s);
    Button(dl, a, b, true, true, T("OK", "OK"));
    if (Clicked(a, b)) g_accept = true;
}

// ---------------------------------------------------------------- my world (OD-63)
const char *MemberStatus(const MemberRow &m) {
    switch (m.role) {
    case 0: return T("Host", "Anfitrião");
    case 1: return T("In the world", "No mundo");
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
    const char *tag = m.role == 1 ? T("YOUR WORLD", "SEU MUNDO") : m.role == 2 ? T("SEAT RESERVED", "LUGAR RESERVADO") : T("YOU ARE IN A WORLD", "VOCÊ ESTÁ EM UM MUNDO");
    TextAt(dl, f.tag, ImVec2(x, y), kPinkU, tag);
    y += 24 * s;
    dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, y + 44 * s), true);
    TextAt(dl, f.title, ImVec2(x, y), kText, m.name.c_str());
    dl->PopClipRect();
    y += 42 * s;
    const std::string sub = (m.role == 1 ? std::string() : m.host + " · ") + AudienceName(m.audience) + " · " + std::to_string(m.players) + "/" +
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
    Button(dl, a, b, g_mine_focus == 0, false, m.role == 1 ? T("Close world", "Fechar mundo") : T("Leave world", "Sair do mundo"));
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
    if (g_mine_role == 1 && m.role != 0) out.push_back({3, T("Remove from my world", "Remover do meu mundo"), true});
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
    if (g_member.tier == 1) sub += " · Infinity Supporter";
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
    const char *title = g_confirm == 1 ? T("Close your world?", "Fechar seu mundo?") : g_confirm == 2 ? T("Leave this world?", "Sair deste mundo?")
                                                                                      : T("Remove this player?", "Remover este jogador?");
    const char *text = g_confirm == 1 ? T("Everyone in it goes back to their own world.", "Todos que estão nele voltam para o próprio mundo.")
                       : g_confirm == 2 ? T("Your seat is let go. Leave Free Roam Online in the game's menu too.", "Seu lugar é liberado. Saia também do Free Roam Online no menu do jogo.")
                                        : T("They leave your world now and cannot join it again while it is open.", "Ele sai do seu mundo agora e não pode voltar enquanto ele estiver aberto.");
    y = Panel(dl, x, y, title);
    if (g_confirm == 3) {
        TextAt(dl, f.body, ImVec2(x, y), kText, g_member.gamertag.c_str());
        y += 34 * s;
    }
    Wrapped(dl, f.body, ImVec2(x, y), w, kText, text);
    y += WrappedHeight(f.body, w, text) + 24 * s;
    const char *yes = g_confirm == 1 ? T("Close world", "Fechar mundo") : g_confirm == 2 ? T("Leave world", "Sair do mundo") : T("Remove", "Remover");
    Buttons(dl, x, y, w, {{1, yes, true}, {0, T("Back", "Voltar"), true}});
}
} // namespace

// ---------------------------------------------------------------- public
void SetWorldAccess(bool allowed, const std::string &message) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.access.Observe(allowed, GetTickCount64());
    g_shared.access_message = message;
}

void SetWorlds(WorldFilter filter, uint8_t status, const std::string &message, std::vector<WorldRow> rows) {
    std::lock_guard<std::mutex> lock(g_lock);
    if (filter != g_shared.filter) {
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
            if (g_focus == 0 && g_chip < 3) { ++g_chip; return true; }
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
    case Screen::Sync: return true;                     // no input until the car is synced
    case Screen::Create:
        switch (nav) {
        case Nav::Up: if (g_create.field > 0) --g_create.field; return true;
        case Nav::Down: if (g_create.field < 4) ++g_create.field; return true;
        case Nav::Left:
        case Nav::Right: {
            const int d = nav == Nav::Right ? 1 : -1;
            if (g_create.field == 1) {
                g_create.audience = (g_create.audience + d + 4) % 4;
            } else if (g_create.field == 2) {
                g_create.players = std::clamp(g_create.players + d, 2, 12);
            } else if (g_create.field == 3) {
                g_create.latency = (g_create.latency + d + 4) % 4;
            }
            return true;
        }
        case Nav::Accept: g_accept = true; return true;
        case Nav::Back: g_screen = Screen::List; return true;
        case Nav::Menu: SubmitCreate(); return true;
        default: return true;
        }
    case Screen::Hosted:
    case Screen::Supporter:
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

void DrawWorldsTab(ImDrawList *dl, float x, float y, float w, float bottom, bool supporter, float dt) {
    const char *reminder = T(
        "Remember: Livery sync between players only works if you have an active car on you right now, make sure to be in-game",
        "Lembrete: a sincronização de pinturas entre jogadores só funciona com um carro ativo no momento. Certifique-se de estar dentro da partida.");
    const float reminder_height = f.small->CalcTextSizeA(f.small->FontSize, FLT_MAX, w, reminder).y;
    const float reminder_y = bottom - reminder_height;
    Wrapped(dl, f.small, ImVec2(x, reminder_y), w, kTextDim, reminder);
    const char *warning = T(
        "Public worlds: bugs and crashes are expected with unknown players. Infinity mitigates problems where possible. Prefer people you trust.",
        "Mundos públicos: bugs e travamentos são esperados com jogadores desconhecidos. O Infinity mitiga os problemas quando possível. Prefira pessoas de confiança.");
    const float warning_height = WrappedHeight(f.small, w, warning);
    const float warning_y = reminder_y - 12 * s - warning_height;
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
        || g_screen == Screen::Supporter || g_screen == Screen::Sync || (g_screen == Screen::Busy && !g_world_action_busy))) {
        g_screen = Screen::List;
        g_after_warning = nullptr;
        ++g_sync_serial;
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
                StartSync(false); // the seat's prompt comes after the car's paint
            } else if (sh.join_status == 3) {
                // a wrong code: back on the code screen, with the reason
                g_screen = Screen::List;
                OpenCodeEntry(T("Enter the world code", "Digite o código do mundo"), "0123456789", [](bool join, const std::string &code) {
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
                StartSync(true); // then the Hosted screen
            } else {
                g_screen = Screen::Create;
                PushToast(ToastKind::Warning, T("Could not create the world", "Não foi possível criar o mundo"), sh.hosted.message);
            }
        }
    }
    if (sh.list_serial != g_seen_list) {
        g_seen_list = sh.list_serial;
        g_focus = std::min(g_focus, 2 + kMaxJoinable + kMaxLocked);
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
        DrawList(dl, x, y, w, bottom, supporter, sh);
        g_focus = std::clamp(g_focus, 0, std::max(0, static_cast<int>(g_items.size()) - 1));
        if (accept) AcceptList(supporter, sh);
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
    case Screen::Sync: {
        DrawSync(dl, x, y, w);
        g_sync_time += dt;
        const int result = g_sync_result.load();
        if (g_sync_time >= kSyncMinShown && result != 1000) {
            FinishSync(result);
        } else if (g_sync_time >= kSyncCap) {
            ++g_sync_serial; // a late answer is ignored
            FinishSync(FI_LIVERY_TIMEOUT);
        }
        break;
    }
    case Screen::Create:
        DrawCreate(dl, x, y, w, supporter);
        if (!supporter && (g_create.audience == 1 || g_create.audience == 2)) {
            g_create.audience = 3; // a free host: Everyone or Private (OD-57)
        }
        if (accept) {
            if (g_create.field == 0) {
                KeyboardRequest r;
                r.title = T("World name", "Nome do mundo");
                r.initial = g_create.name;
                r.max = 48;
                OpenKeyboard(r, [](bool ok, const std::string &text) {
                    if (ok && !text.empty()) g_create.name = text;
                });
            } else if (g_create.field == 4) {
                SubmitCreate();
            }
        }
        break;
    case Screen::Hosted:
        DrawHosted(dl, x, y, w, sh);
        if (accept) g_screen = Screen::List;
        break;
    case Screen::Supporter:
        DrawSupporter(dl, x, y, w);
        if (accept) g_screen = Screen::List;
        break;
    }
    dl->PopClipRect();
}

} // namespace fi
