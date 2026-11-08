// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the right FRIENDS sidebar (OD-211, CR-S102, CORE_API §4.2a, W15), for every game.
//
// - A sidebar pinned to the right of the guide while it is open. All friends, online first; for an online friend what it does, per
//   game ("Racing on FH2", "Online free roam on FH2", "Private lobby in FM5"). Friendship is by account: one row per friend account
//   (the launcher sends the profile it plays, else its first), its other profiles are in the gamercard.
// - The top row "Add friend" opens an on-screen keyboard for a PROFILE name (the controller moves over the keys; a PC keyboard types
//   too). Incoming friend requests and requests to join this player's lobby (the leader's) are rows with Accept / Decline.
// - A friend row: A opens the gamercard, X asks to join its world, Y invites it to my lobby. The gamercard replaces the whole
//   sidebar: avatar, name, Discord (when linked), what it does, account age, its profiles, Ask to join, Invite to my game, the link
//   to its profile on the website (hidden when it is not listed), Unfriend (a second A confirms).
// - A friend the lobby's "Who can join" excludes shows greyed with the reason. Everything is the launcher's last FriendsState (0x2A);
//   actions are FriendsAction (0x2B); the launcher answers FriendsResult (0x2C), shown as a notice.
// - The controller enters with Right (Start from anywhere) and leaves with B at the list or Left; the mouse always works.
#include "overlay.h"

#include <shellapi.h>

#include <algorithm>
#include <cmath>
#include <cstring>
#include <deque>
#include <mutex>
#include <string>
#include <vector>

#include "imgui.h"
#include "style.h"

namespace fi {

using namespace ui;

namespace {

enum class View { List, Card, Keyboard };

std::mutex g_lock;
FriendsStateFrame g_state;               // under g_lock: the pipe thread sets it, the render thread copies it
bool g_have = false;
std::deque<FriendsResultFrame> g_results; // under g_lock
std::u16string g_typed;                   // under g_lock: characters typed on the game window's keyboard (WM_CHAR)

// render thread
bool g_in = false;       // the controller is in the sidebar
View g_view = View::List;
int g_focus = 0;         // the list's focused row (an index of Rows())
float g_scroll = 0;
std::string g_card;      // the account the gamercard shows
int g_card_focus = 0;
bool g_confirm_unfriend = false;
std::string g_text;      // the profile name being typed
int g_krow = 0, g_kcol = 0;
bool g_shift = false;
bool g_sending = false;  // an Add is with the launcher
bool g_was_open = false;
ULONGLONG g_refresh_at = 0;
FriendsStateFrame g_view_state;
bool g_view_have = false;

constexpr float kWidth = 400.0f;

bool Blocked() { return PaneModal() || FriendPickerOpen() || InvitePromptOpen(); }

// ---------------------------------------------------------------- words
std::string Fmt(const char *format, const std::string &game) {
    std::string out;
    for (const char *p = format; *p; ++p) {
        if (p[0] == '%' && p[1] == 's') { out += game; ++p; }
        else out += *p;
    }
    return out;
}

// What an online friend does, per game: "Racing on FH2" (OD-211).
std::string DoingText(const FriendRowFrame &r) {
    if (!r.Has(kFriendOnline)) return T("Offline", "Offline");
    const bool g = !r.game.empty();
    if (r.activity == kActRace) return g ? Fmt(T("Racing on %s", "Correndo no %s"), r.game) : T("Racing", "Correndo");
    if (r.Has(kFriendPrivate)) return g ? Fmt(T("Private lobby in %s", "Lobby privado no %s"), r.game) : T("Private lobby", "Lobby privado");
    switch (r.activity) {
    case kActFreeRoam: return g ? Fmt(T("Online free roam on %s", "Free roam online no %s"), r.game) : T("Online free roam", "Free roam online");
    case kActCarMeet: return g ? Fmt(T("Car meet on %s", "Encontro de carros no %s"), r.game) : T("Car meet", "Encontro de carros");
    case kActRoadTrip: return g ? Fmt(T("Road trip on %s", "Road trip no %s"), r.game) : T("Road trip", "Road trip");
    default: return g ? Fmt(T("Online on %s", "Online no %s"), r.game) : T("Online", "Online");
    }
}

const char *ExcludedText(uint8_t reason) {
    return reason == kExcludedMode ? T("Not allowed by Who can join", "Fora de Quem pode entrar") : T("Cannot join this lobby", "Não pode entrar neste lobby");
}

std::string AgeText(uint32_t days) {
    if (days == kFriendAgeUnknown) return {};
    const bool pt = Portuguese();
    auto plural = [&](uint32_t n, const char *en1, const char *enN, const char *pt1, const char *ptN) {
        return std::to_string(n) + " " + (n == 1 ? (pt ? pt1 : en1) : (pt ? ptN : enN));
    };
    if (days == 0) return T("New today", "Conta nova de hoje");
    if (days < 30) return plural(days, "day", "days", "dia", "dias");
    if (days < 365) return plural(days / 30, "month", "months", "mês", "meses");
    const uint32_t years = days / 365, months = (days % 365) / 30;
    std::string out = plural(years, "year", "years", "ano", "anos");
    if (months) out += " " + plural(months, "month", "months", "mês", "meses");
    return out;
}

// ---------------------------------------------------------------- the list's rows
struct Row {
    enum Kind { Add, Header, Join, Request, Friend } kind = Add;
    int index = 0;      // into the state's joins / requests / friends
    std::string header;
};

std::vector<Row> RowsOf(const FriendsStateFrame &st) {
    std::vector<Row> rows;
    rows.push_back({Row::Add, 0, {}});
    if (!st.joins.empty()) {
        rows.push_back({Row::Header, 0, T("WANT TO JOIN YOUR LOBBY", "QUEREM ENTRAR NO SEU LOBBY")});
        for (size_t i = 0; i < st.joins.size(); ++i) rows.push_back({Row::Join, static_cast<int>(i), {}});
    }
    if (!st.requests.empty()) {
        rows.push_back({Row::Header, 0, T("FRIEND REQUESTS", "PEDIDOS DE AMIZADE")});
        for (size_t i = 0; i < st.requests.size(); ++i) rows.push_back({Row::Request, static_cast<int>(i), {}});
    }
    size_t online = 0;
    for (const auto &f : st.friends) online += f.Has(kFriendOnline) ? 1 : 0;
    if (online) {
        rows.push_back({Row::Header, 0, std::string(T("ONLINE", "ONLINE")) + " · " + std::to_string(online)});
        for (size_t i = 0; i < st.friends.size(); ++i)
            if (st.friends[i].Has(kFriendOnline)) rows.push_back({Row::Friend, static_cast<int>(i), {}});
    }
    if (online < st.friends.size()) {
        rows.push_back({Row::Header, 0, std::string(T("OFFLINE", "OFFLINE")) + " · " + std::to_string(st.friends.size() - online)});
        for (size_t i = 0; i < st.friends.size(); ++i)
            if (!st.friends[i].Has(kFriendOnline)) rows.push_back({Row::Friend, static_cast<int>(i), {}});
    }
    return rows;
}

const FriendRowFrame *FindFriend(const FriendsStateFrame &st, const std::string &id) {
    for (const auto &f : st.friends)
        if (f.id == id) return &f;
    return nullptr;
}

void Act(uint8_t kind, const std::string &id = {}, const std::string &text = {}) {
    if (!FriendsActionValid(kind, id, text)) return;
    if (DevHost()) {
        Log("friends: dev host: action %u (no launcher)", static_cast<unsigned>(kind));
        return;
    }
    SendFriendsAction(kind, id, text);
}

// ---------------------------------------------------------------- the keyboard
enum class Key { Char, Shift, Backspace, Space, Add, Cancel };
struct KeyDef {
    const char *label;
    Key kind;
    float w; // in units of one character key
};

const std::vector<std::vector<KeyDef>> &Keys() {
    static const std::vector<std::vector<KeyDef>> keys = [] {
        std::vector<std::vector<KeyDef>> k;
        for (const char *line : {"1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm"}) {
            std::vector<KeyDef> row;
            static std::deque<std::string> labels; // stable storage for the one-character labels
            for (const char *c = line; *c; ++c) {
                labels.emplace_back(1, *c);
                row.push_back({labels.back().c_str(), Key::Char, 1.0f});
            }
            k.push_back(std::move(row));
        }
        k.push_back({{"Shift", Key::Shift, 2.0f}, {"-", Key::Char, 1.0f}, {"_", Key::Char, 1.0f}, {".", Key::Char, 1.0f}, {"Space", Key::Space, 3.0f},
                     {"Del", Key::Backspace, 2.0f}});
        k.push_back({{"Cancel", Key::Cancel, 5.0f}, {"Add", Key::Add, 5.0f}});
        return k;
    }();
    return keys;
}

float RowUnits(const std::vector<KeyDef> &row) {
    float u = 0;
    for (const auto &k : row) u += k.w;
    return u;
}

// The key of `row` whose centre is nearest to `x` (0..1 across the board).
int NearestKey(int row, float x) {
    const auto &r = Keys()[static_cast<size_t>(row)];
    const float total = RowUnits(r);
    float at = 0, best = 1e9f;
    int pick = 0;
    for (size_t i = 0; i < r.size(); ++i) {
        const float centre = (at + r[i].w / 2) / total;
        if (std::abs(centre - x) < best) { best = std::abs(centre - x); pick = static_cast<int>(i); }
        at += r[i].w;
    }
    return pick;
}

float KeyCentre(int row, int col) {
    const auto &r = Keys()[static_cast<size_t>(row)];
    float at = 0;
    for (int i = 0; i < col; ++i) at += r[static_cast<size_t>(i)].w;
    return (at + r[static_cast<size_t>(col)].w / 2) / RowUnits(r);
}

void AppendUtf8(std::string &out, char32_t c) {
    if (c < 0x80) out += static_cast<char>(c);
    else if (c < 0x800) { out += static_cast<char>(0xC0 | (c >> 6)); out += static_cast<char>(0x80 | (c & 0x3F)); }
    else { out += static_cast<char>(0xE0 | (c >> 12)); out += static_cast<char>(0x80 | ((c >> 6) & 0x3F)); out += static_cast<char>(0x80 | (c & 0x3F)); }
}

void TypeChar(char32_t c) {
    if (c < 0x20 || c == 0x7F || (c >= 0xD800 && c <= 0xDFFF)) return;
    std::string next = g_text;
    AppendUtf8(next, c);
    if (next.size() <= kMaxFriendName) g_text = next;
}

void Backspace() {
    while (!g_text.empty()) { // one code point
        const bool tail = (static_cast<unsigned char>(g_text.back()) & 0xC0) == 0x80;
        g_text.pop_back();
        if (!tail) break;
    }
}

void Submit() {
    if (g_sending || g_text.empty()) return;
    std::string name = g_text;
    while (!name.empty() && name.back() == ' ') name.pop_back();
    while (!name.empty() && name.front() == ' ') name.erase(name.begin());
    if (name.empty()) return;
    g_sending = true;
    Act(kFriendsAdd, {}, name);
}

void OpenKeyboard() {
    g_view = View::Keyboard;
    g_text.clear();
    g_krow = 1;
    g_kcol = 0;
    g_shift = false;
    g_sending = false;
}

void PressKey(const KeyDef &key) {
    switch (key.kind) {
    case Key::Char: {
        char c = key.label[0];
        if (g_shift && c >= 'a' && c <= 'z') c = static_cast<char>(c - 32);
        TypeChar(static_cast<char32_t>(c));
        if (g_shift) g_shift = false;
        break;
    }
    case Key::Shift: g_shift = !g_shift; break;
    case Key::Backspace: Backspace(); break;
    case Key::Space: TypeChar(U' '); break;
    case Key::Add: Submit(); break;
    case Key::Cancel: g_view = View::List; g_sending = false; break;
    }
}

// ---------------------------------------------------------------- drawing helpers
bool Inside(ImVec2 p, ImVec2 a, ImVec2 b) { return p.x >= a.x && p.x < b.x && p.y >= a.y && p.y < b.y; }

// A friend's disc: its Discord avatar (0x2D, protocol 11) once the launcher sent it, else its initials; a green dot when online.
// `id` names the friend row (null: a request row, initials only).
void Disc(ImDrawList *dl, ImVec2 c, float r, const std::string &name, bool online, bool supporter, const std::string *id = nullptr) {
    float u0 = 0, v0 = 0, u1 = 1, v1 = 1;
    if (const uint64_t texture = id ? FriendAvatarTexture(*id, u0, v0, u1, v1) : 0) {
        dl->AddImageRounded(static_cast<ImTextureID>(texture), ImVec2(c.x - r, c.y - r), ImVec2(c.x + r, c.y + r), ImVec2(u0, v0), ImVec2(u1, v1),
                            online ? IM_COL32_WHITE : IM_COL32(160, 160, 160, 255), r);
    } else {
        dl->AddCircleFilled(c, r, online ? (supporter ? kPinkU : IM_COL32(86, 86, 96, 255)) : IM_COL32(52, 52, 58, 255), 40);
        std::string initials = name.substr(0, std::min<size_t>(name.size() >= 2 && static_cast<unsigned char>(name[0]) < 0x80 ? 2 : 1, name.size()));
        for (auto &ch : initials) ch = static_cast<char>(ch >= 'a' && ch <= 'z' ? ch - 32 : ch);
        ImFont *font = r > 30 * s ? f.heading : f.tag;
        const ImVec2 size = Measure(font, initials.c_str());
        TextAt(dl, font, ImVec2(c.x - size.x / 2, c.y - size.y / 2), online ? kText : kTextDim, initials.c_str());
    }
    if (online) {
        dl->AddCircleFilled(ImVec2(c.x + r * 0.72f, c.y + r * 0.72f), r * 0.28f + 2 * s, kPanel, 20);
        dl->AddCircleFilled(ImVec2(c.x + r * 0.72f, c.y + r * 0.72f), r * 0.28f, kTalkGreen, 20);
    }
}

void Clipped(ImDrawList *dl, ImFont *font, ImVec2 a, float w, ImU32 color, const char *text, float h = 30.0f) {
    dl->PushClipRect(a, ImVec2(a.x + w, a.y + h * s), true);
    TextAt(dl, font, a, color, text);
    dl->PopClipRect();
}

struct Layout {
    float x, y, w, h; // the whole sidebar
    float pad, top, bottom;
};

// One tile of a list: the rounded tile, the focus ring (controller in the sidebar) or the selected bar.
void Tile(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, bool live) {
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus && live) FocusRing(dl, a, b, 6 * s);
    else if (focus) dl->AddRectFilled(ImVec2(a.x, a.y + 8 * s), ImVec2(a.x + 4 * s, b.y - 8 * s), kPinkU, 2 * s);
}

void HintLine(ImDrawList *dl, const Layout &l, float alpha, std::initializer_list<std::pair<const char *, const char *>> hints) {
    const float hy = l.y + l.h - 40 * s;
    dl->AddLine(ImVec2(l.x + l.pad, hy - 12 * s), ImVec2(l.x + l.w - l.pad, hy - 12 * s), Rgba(255, 255, 255, 0.08f * alpha));
    float hx = l.x + l.pad;
    for (const auto &h : hints) {
        const bool a = h.first[0] == 'A', b = h.first[0] == 'B', x = h.first[0] == 'X';
        FaceButton(dl, ImVec2(hx + 9 * s, hy + 10 * s), 9 * s, a ? kGreen : b ? kRed : x ? kBlue : kYellow, h.first);
        TextAt(dl, f.small, ImVec2(hx + 23 * s, hy + 1 * s), kTextDim, h.second, alpha);
        hx += 23 * s + Measure(f.small, h.second).x + 16 * s;
    }
}

// ---------------------------------------------------------------- the list
void MoveFocus(const std::vector<Row> &rows, int dir) {
    int i = g_focus;
    for (;;) {
        i += dir;
        if (i < 0 || i >= static_cast<int>(rows.size())) return;
        if (rows[static_cast<size_t>(i)].kind != Row::Header) { g_focus = i; return; }
    }
}

void OpenCard(const std::string &id) {
    g_view = View::Card;
    g_card = id;
    g_card_focus = 0;
    g_confirm_unfriend = false;
}

void ActivateRow(const FriendsStateFrame &st, const Row &row) {
    switch (row.kind) {
    case Row::Add: OpenKeyboard(); break;
    case Row::Join: Act(kFriendsAcceptJoin, st.joins[static_cast<size_t>(row.index)].id); break;
    case Row::Request: Act(kFriendsAccept, st.requests[static_cast<size_t>(row.index)].id); break;
    case Row::Friend: OpenCard(st.friends[static_cast<size_t>(row.index)].id); break;
    default: break;
    }
}

void ListX(const FriendsStateFrame &st, const Row &row) { // X: ask to join, or decline
    switch (row.kind) {
    case Row::Join: Act(kFriendsDeclineJoin, st.joins[static_cast<size_t>(row.index)].id); break;
    case Row::Request: Act(kFriendsDecline, st.requests[static_cast<size_t>(row.index)].id); break;
    case Row::Friend: {
        const auto &fr = st.friends[static_cast<size_t>(row.index)];
        if (fr.Has(kFriendCanAsk)) Act(kFriendsAsk, fr.id);
        else PushToast(ToastKind::Info, T("Friends", "Amigos"), T("That friend is not in a world you can join", "Esse amigo não está em um mundo em que você possa entrar"), 30, true);
        break;
    }
    default: break;
    }
}

void ListY(const FriendsStateFrame &st, const Row &row) { // Y: invite to my game
    if (row.kind != Row::Friend) return;
    const auto &fr = st.friends[static_cast<size_t>(row.index)];
    if (!st.have_lobby()) PushToast(ToastKind::Info, T("Friends", "Amigos"), T("Open a private lobby to invite friends", "Abra um lobby privado para convidar amigos"), 30, true);
    else if (fr.Has(kFriendExcluded)) PushToast(ToastKind::Info, T("Friends", "Amigos"), ExcludedText(fr.excluded), 30, true);
    else if (fr.Has(kFriendCanInvite)) Act(kFriendsInviteMine, fr.id);
}

void DrawList(ImDrawList *dl, const Layout &l, const FriendsStateFrame &st, bool have, float alpha) {
    const std::vector<Row> rows = RowsOf(st);
    g_focus = std::clamp(g_focus, 0, static_cast<int>(rows.size()) - 1);
    if (rows[static_cast<size_t>(g_focus)].kind == Row::Header) MoveFocus(rows, 1);
    const float th = 62 * s, hh = 34 * s, gap = 8 * s;
    float total = 0, focus_top = 0, focus_h = th;
    for (int i = 0; i < static_cast<int>(rows.size()); ++i) {
        const float h = rows[static_cast<size_t>(i)].kind == Row::Header ? hh : th;
        if (i == g_focus) { focus_top = total; focus_h = h; }
        total += h + gap;
    }
    const float view = l.bottom - l.top;
    g_scroll = std::clamp(g_scroll, focus_top + focus_h - view, focus_top);
    g_scroll = std::max(0.0f, total - gap <= view ? 0.0f : g_scroll);
    float y = l.top - g_scroll;
    dl->PushClipRect(ImVec2(l.x, l.top - 4 * s), ImVec2(l.x + l.w, l.bottom), true);
    const ImVec2 mouse = ImGui::GetIO().MousePos;
    for (int i = 0; i < static_cast<int>(rows.size()); ++i) {
        const Row &row = rows[static_cast<size_t>(i)];
        if (row.kind == Row::Header) {
            if (y + hh > l.top - 4 * s && y < l.bottom) TextAt(dl, f.tag, ImVec2(l.x + l.pad + 4 * s, y + 12 * s), kPinkU, row.header.c_str(), alpha);
            y += hh + gap;
            continue;
        }
        const ImVec2 a(l.x + l.pad, y), b(l.x + l.w - l.pad, y + th);
        y += th + gap;
        if (b.y < l.top - 4 * s || a.y > l.bottom) continue;
        const bool inside = a.y >= l.top - 1 && b.y <= l.bottom + 1;
        if (inside && !Blocked() && Inside(mouse, a, b) && ImGui::IsMouseHoveringRect(a, b)) {
            g_focus = i;
            if (ImGui::IsMouseClicked(0)) { g_in = true; ActivateRow(st, row); }
        }
        const bool focus = i == g_focus;
        Tile(dl, a, b, focus, g_in);
        const float tx = a.x + 64 * s, tw = b.x - tx - 12 * s;
        if (row.kind == Row::Add) {
            dl->AddText(f.icon, f.icon->FontSize, ImVec2(a.x + 18 * s, a.y + th / 2 - f.icon->FontSize / 2), focus ? kPinkU : kText, kIconAddFriend);
            TextAt(dl, f.body, ImVec2(a.x + 64 * s, a.y + th / 2 - 12 * s), kText, T("Add friend", "Adicionar amigo"));
            continue;
        }
        if (row.kind == Row::Join || row.kind == Row::Request) {
            const std::string &name = row.kind == Row::Join ? st.joins[static_cast<size_t>(row.index)].name : st.requests[static_cast<size_t>(row.index)].name;
            Disc(dl, ImVec2(a.x + 32 * s, a.y + th / 2), 20 * s, name, false, false);
            Clipped(dl, f.body, ImVec2(tx, a.y + 8 * s), tw, kText, name.c_str());
            Clipped(dl, f.small, ImVec2(tx, a.y + 35 * s), tw, kAmber,
                    row.kind == Row::Join ? T("A allow · X decline", "A permitir · X recusar") : T("A accept · X decline", "A aceitar · X recusar"), 22);
            continue;
        }
        const FriendRowFrame &fr = st.friends[static_cast<size_t>(row.index)];
        const bool excluded = fr.Has(kFriendExcluded);
        Disc(dl, ImVec2(a.x + 32 * s, a.y + th / 2), 20 * s, fr.name, fr.Has(kFriendOnline), fr.Has(kFriendSupporter), &fr.id);
        Clipped(dl, f.body, ImVec2(tx, a.y + 8 * s), tw, excluded || !fr.Has(kFriendOnline) ? kTextDim : kText, fr.name.c_str());
        if (!fr.discord_user.empty()) { // linked Discord, also in the list
            const float nw = Measure(f.body, fr.name.c_str()).x;
            if (nw + 120 * s < tw) TextAt(dl, f.tag, ImVec2(tx + nw + 10 * s, a.y + 14 * s), Rgba(120, 140, 255), (std::string("@") + fr.discord_user).c_str(), alpha);
        }
        const std::string sub = excluded ? std::string(ExcludedText(fr.excluded)) : DoingText(fr);
        Clipped(dl, f.small, ImVec2(tx, a.y + 35 * s), tw, excluded ? kAmber : kTextDim, sub.c_str(), 22);
    }
    dl->PopClipRect();
    (void)have;
}

// ---------------------------------------------------------------- the gamercard
struct CardButton {
    std::string label, note;
    bool enabled = true;
    enum Kind { Ask, Invite, Profile, Unfriend, Back } kind = Back;
};

std::vector<CardButton> ButtonsOf(const FriendsStateFrame &st, const FriendRowFrame &fr) {
    std::vector<CardButton> b;
    const bool online = fr.Has(kFriendOnline);
    b.push_back({T("Ask to join", "Pedir para entrar"),
                 fr.Has(kFriendCanAsk) ? "" : online ? T("Not in a world you can join", "Não está em um mundo em que você possa entrar") : T("Offline", "Offline"),
                 fr.Has(kFriendCanAsk), CardButton::Ask});
    if (st.have_lobby())
        b.push_back({T("Invite to my game", "Convidar para meu jogo"),
                     fr.Has(kFriendExcluded) ? ExcludedText(fr.excluded) : fr.Has(kFriendCanInvite) ? "" : T("Offline", "Offline"),
                     fr.Has(kFriendCanInvite), CardButton::Invite});
    if (!fr.profile_url.empty()) b.push_back({T("Open profile on the website", "Abrir perfil no site"), "", true, CardButton::Profile});
    b.push_back({g_confirm_unfriend ? T("Press A again to unfriend", "Aperte A de novo para desfazer a amizade") : T("Unfriend", "Desfazer amizade"), "", true, CardButton::Unfriend});
    b.push_back({T("Back", "Voltar"), "", true, CardButton::Back});
    return b;
}

void OpenProfile(const std::string &url) {
    // only the website's own pages; the game keeps running behind the browser
    static const char prefix[] = "https://play.infinity-horizon.com/";
    if (url.compare(0, sizeof(prefix) - 1, prefix) != 0) return;
    if (DevHost()) { Log("friends: dev host: profile link not opened"); return; }
    const int wide = MultiByteToWideChar(CP_UTF8, 0, url.c_str(), -1, nullptr, 0);
    if (wide <= 0) return;
    std::wstring w(static_cast<size_t>(wide), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, url.c_str(), -1, w.data(), wide);
    ShellExecuteW(nullptr, L"open", w.c_str(), nullptr, nullptr, SW_SHOWNORMAL);
    Log("friends: a friend's profile page opened in the browser");
}

void ActivateButton(const FriendRowFrame &fr, const CardButton &b) {
    if (b.kind == CardButton::Back) { g_view = View::List; return; }
    if (b.kind == CardButton::Unfriend) {
        if (!g_confirm_unfriend) { g_confirm_unfriend = true; return; }
        g_confirm_unfriend = false;
        Act(kFriendsUnfriend, fr.id);
        g_view = View::List;
        return;
    }
    g_confirm_unfriend = false;
    if (!b.enabled) {
        if (!b.note.empty()) PushToast(ToastKind::Info, T("Friends", "Amigos"), b.note, 30, true);
        return;
    }
    if (b.kind == CardButton::Ask) Act(kFriendsAsk, fr.id);
    else if (b.kind == CardButton::Invite) Act(kFriendsInviteMine, fr.id);
    else if (b.kind == CardButton::Profile) OpenProfile(fr.profile_url);
}

void DrawCard(ImDrawList *dl, const Layout &l, const FriendsStateFrame &st, float alpha) {
    const FriendRowFrame *fr = FindFriend(st, g_card);
    if (!fr) { g_view = View::List; return; }
    float y = l.top;
    const float cx = l.x + l.w / 2;
    Disc(dl, ImVec2(cx, y + 44 * s), 40 * s, fr->name, fr->Has(kFriendOnline), fr->Has(kFriendSupporter), &fr->id);
    y += 100 * s;
    const ImVec2 ns = Measure(f.heading, fr->name.c_str());
    dl->PushClipRect(ImVec2(l.x + l.pad, y), ImVec2(l.x + l.w - l.pad, y + 40 * s), true);
    TextAt(dl, f.heading, ImVec2(cx - std::min(ns.x, l.w - 2 * l.pad) / 2, y), kText, fr->name.c_str(), alpha);
    dl->PopClipRect();
    y += 40 * s;
    if (fr->Has(kFriendSupporter)) {
        const char *tag = "INFINITY SUPPORTER";
        const ImVec2 ts = Measure(f.tag, tag);
        dl->AddRectFilled(ImVec2(cx - ts.x / 2 - 8 * s, y), ImVec2(cx + ts.x / 2 + 8 * s, y + ts.y + 8 * s), kPinkU, (ts.y + 8 * s) / 2);
        TextAt(dl, f.tag, ImVec2(cx - ts.x / 2, y + 4 * s), kText, tag);
        y += ts.y + 14 * s;
    }
    auto line = [&](ImFont *font, ImU32 color, const std::string &text) {
        const ImVec2 ts = Measure(font, text.c_str());
        Clipped(dl, font, ImVec2(cx - std::min(ts.x, l.w - 2 * l.pad) / 2, y), l.w - 2 * l.pad, color, text.c_str(), 26);
        y += ts.y + 6 * s;
    };
    if (!fr->discord_user.empty()) { // the linked Discord's names (its avatar is the disc above, 0x2D)
        line(f.body, Rgba(120, 140, 255), fr->discord_name.empty() ? "@" + fr->discord_user : fr->discord_name + "  @" + fr->discord_user);
    }
    const bool excluded = fr->Has(kFriendExcluded);
    line(f.small, excluded ? kAmber : kTextDim, excluded ? std::string(ExcludedText(fr->excluded)) : DoingText(*fr));
    y += 8 * s;
    dl->AddLine(ImVec2(l.x + l.pad, y), ImVec2(l.x + l.w - l.pad, y), Rgba(255, 255, 255, 0.08f));
    y += 10 * s;
    const std::string age = AgeText(fr->account_days);
    auto field = [&](const char *name, const std::string &value, ImU32 color = kText) {
        TextAt(dl, f.small, ImVec2(l.x + l.pad, y), kTextDim, name);
        const ImVec2 vs = Measure(f.small, value.c_str());
        Clipped(dl, f.small, ImVec2(l.x + l.w - l.pad - std::min(vs.x, l.w * 0.62f), y), l.w * 0.62f, color, value.c_str(), 22);
        y += 24 * s;
    };
    if (!age.empty()) field(T("Account age", "Idade da conta"), age);
    for (const auto &p : fr->profiles) { // hardcore: {name} / noob: {name}
        std::string value = p.name;
        if (!p.game.empty()) value += " · " + p.game;
        field(p.mode == 0 ? "hardcore" : "noob", value);
    }
    y += 6 * s;
    const std::vector<CardButton> buttons = ButtonsOf(st, *fr);
    g_card_focus = std::clamp(g_card_focus, 0, static_cast<int>(buttons.size()) - 1);
    const float bh = 50 * s, gap = 8 * s;
    const ImVec2 mouse = ImGui::GetIO().MousePos;
    for (int i = 0; i < static_cast<int>(buttons.size()); ++i, y += bh + gap) {
        const CardButton &btn = buttons[static_cast<size_t>(i)];
        const ImVec2 a(l.x + l.pad, y), b(l.x + l.w - l.pad, y + bh);
        if (b.y > l.bottom) break;
        if (!Blocked() && Inside(mouse, a, b) && ImGui::IsMouseHoveringRect(a, b)) {
            g_card_focus = i;
            if (ImGui::IsMouseClicked(0)) { g_in = true; ActivateButton(*fr, btn); return; }
        }
        Tile(dl, a, b, i == g_card_focus, g_in);
        const bool two = !btn.note.empty();
        Clipped(dl, f.body, ImVec2(a.x + 18 * s, a.y + (two ? 5 * s : bh / 2 - 12 * s)), b.x - a.x - 36 * s, btn.enabled ? kText : kTextDim, btn.label.c_str());
        if (two) Clipped(dl, f.tag, ImVec2(a.x + 18 * s, a.y + 31 * s), b.x - a.x - 36 * s, kAmber, btn.note.c_str(), 16);
    }
    (void)alpha;
}

// ---------------------------------------------------------------- the keyboard view
void DrawKeyboard(ImDrawList *dl, const Layout &l, float alpha) {
    float y = l.top;
    TextAt(dl, f.tag, ImVec2(l.x + l.pad, y), kPinkU, T("ADD FRIEND", "ADICIONAR AMIGO"));
    y += 22 * s;
    dl->AddText(f.small, f.small->FontSize, ImVec2(l.x + l.pad, y), kTextDim,
                T("A profile name, Hardcore or Noob. You become friends with the account.", "O nome de um perfil, Hardcore ou Noob. Você fica amigo da conta."), nullptr,
                l.w - 2 * l.pad);
    y += 40 * s;
    const ImVec2 fa(l.x + l.pad, y), fb(l.x + l.w - l.pad, y + 50 * s);
    dl->AddRectFilled(fa, fb, IM_COL32(12, 12, 14, 255), 6 * s);
    dl->AddRect(fa, fb, g_sending ? kTextDim : kPinkU, 6 * s, 0, 2 * s);
    const std::string shown = g_text;
    const ImVec2 ts = Measure(f.body, shown.c_str());
    const bool blink = (GetTickCount64() / 500) % 2 == 0;
    dl->PushClipRect(ImVec2(fa.x + 10 * s, fa.y), ImVec2(fb.x - 10 * s, fb.y), true);
    const float tx = fa.x + 14 * s + std::min(0.0f, (fb.x - fa.x - 40 * s) - ts.x);
    if (shown.empty()) TextAt(dl, f.body, ImVec2(fa.x + 14 * s, fa.y + 12 * s), Rgba(255, 255, 255, 0.28f), T("Profile name", "Nome do perfil"));
    else TextAt(dl, f.body, ImVec2(tx, fa.y + 12 * s), kText, shown.c_str());
    if (blink && !g_sending) dl->AddRectFilled(ImVec2(tx + ts.x + 2 * s, fa.y + 11 * s), ImVec2(tx + ts.x + 4 * s, fa.y + 38 * s), kPinkU);
    dl->PopClipRect();
    y += 70 * s;
    const auto &keys = Keys();
    const float unit = (l.w - 2 * l.pad - 9 * 4 * s) / 10.0f, kh = 46 * s, kg = 4 * s;
    const ImVec2 mouse = ImGui::GetIO().MousePos;
    for (int r = 0; r < static_cast<int>(keys.size()); ++r, y += kh + kg) {
        const auto &row = keys[static_cast<size_t>(r)];
        const float total = RowUnits(row) * unit + (static_cast<float>(row.size()) - 1) * kg;
        float x = l.x + (l.w - total) / 2;
        for (int c = 0; c < static_cast<int>(row.size()); ++c) {
            const KeyDef &k = row[static_cast<size_t>(c)];
            const float w = k.w * unit + (k.w - 1) * kg;
            const ImVec2 a(x, y), b(x + w, y + kh);
            x += w + kg;
            if (!Blocked() && Inside(mouse, a, b) && ImGui::IsMouseHoveringRect(a, b)) {
                g_krow = r;
                g_kcol = c;
                if (ImGui::IsMouseClicked(0)) { g_in = true; PressKey(k); if (g_view != View::Keyboard) return; }
            }
            const bool focus = r == g_krow && c == g_kcol;
            const bool special = k.kind != Key::Char && k.kind != Key::Space;
            ImU32 fill = focus ? kTileFocus : kTile;
            if (k.kind == Key::Add) fill = focus ? kPinkU : kPinkDark;
            if (k.kind == Key::Shift && g_shift) fill = kBlue;
            dl->AddRectFilled(a, b, fill, 5 * s);
            if (focus && g_in) FocusRing(dl, a, b, 5 * s);
            std::string label = k.label;
            if (k.kind == Key::Char && g_shift && label[0] >= 'a' && label[0] <= 'z') label[0] = static_cast<char>(label[0] - 32);
            if (k.kind == Key::Shift) label = T("Shift", "Shift");
            if (k.kind == Key::Space) label = T("Space", "Espaço");
            if (k.kind == Key::Cancel) label = T("Cancel", "Cancelar");
            if (k.kind == Key::Add) label = g_sending ? T("Adding...", "Adicionando...") : T("Add", "Adicionar");
            if (k.kind == Key::Backspace) label = T("Delete", "Apagar");
            ImFont *font = special || label.size() > 1 ? f.small : f.body;
            const ImVec2 ls = Measure(font, label.c_str());
            TextAt(dl, font, ImVec2((a.x + b.x) / 2 - ls.x / 2, (a.y + b.y) / 2 - ls.y / 2), kText, label.c_str(), alpha);
        }
    }
}

// ---------------------------------------------------------------- the frame
void DrainInput() {
    std::u16string typed;
    std::deque<FriendsResultFrame> results;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        typed.swap(g_typed);
        results.swap(g_results);
        g_view_state = g_state;
        g_view_have = g_have;
    }
    if (g_view == View::Keyboard && !g_sending)
        for (char16_t c : typed) TypeChar(c);
    for (const auto &r : results) {
        const bool ok = r.result <= kResultNowFriends;
        PushToast(ok ? ToastKind::Friend : ToastKind::Warning, T("Friends", "Amigos"), r.message, 40, true);
        if (r.kind == kFriendsAdd) {
            g_sending = false;
            if (ok && g_view == View::Keyboard) { g_view = View::List; g_text.clear(); }
        }
    }
}

} // namespace

// ---------------------------------------------------------------- public
void SetFriendsState(FriendsStateFrame state) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_state = std::move(state);
    g_have = true;
}

void SetFriendsResult(FriendsResultFrame result) {
    std::lock_guard<std::mutex> lock(g_lock);
    if (g_results.size() >= 8) g_results.pop_front();
    g_results.push_back(std::move(result));
}

void FriendsDisconnected() {
    std::lock_guard<std::mutex> lock(g_lock);
    g_state = FriendsStateFrame{};
    g_have = false;
    g_results.clear();
}

float FriendsSidebarWidth() { return kWidth * s; }

bool FriendsFocused() { return g_in; }

bool FriendsTyping() { return g_in && g_view == View::Keyboard; }

void FriendsEnter() {
    g_in = true;
    if (g_view == View::Keyboard && g_sending) g_sending = false;
}

void FriendsLeave() { g_in = false; }

void FriendsChar(char16_t c) {
    std::lock_guard<std::mutex> lock(g_lock);
    if (g_typed.size() < 64) g_typed.push_back(c);
}

bool FriendsNavigate(Nav nav) {
    if (!g_in) return false;
    if (nav == Nav::TabLeft || nav == Nav::TabRight) { g_in = false; return false; } // LB / RB still change the guide's tab
    FriendsStateFrame st;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        st = g_state;
    }
    if (nav == Nav::Menu) { // Start: submit a name being typed, else back to the guide
        if (g_view == View::Keyboard) Submit();
        else g_in = false;
        return true;
    }
    switch (g_view) {
    case View::List: {
        const std::vector<Row> rows = RowsOf(st);
        g_focus = std::clamp(g_focus, 0, static_cast<int>(rows.size()) - 1);
        const Row &row = rows[static_cast<size_t>(g_focus)];
        switch (nav) {
        case Nav::Up: MoveFocus(rows, -1); break;
        case Nav::Down: MoveFocus(rows, 1); break;
        case Nav::Accept: ActivateRow(st, row); break;
        case Nav::X: ListX(st, row); break;
        case Nav::Y: ListY(st, row); break;
        case Nav::Back:
        case Nav::Left: g_in = false; break;
        default: break;
        }
        return true;
    }
    case View::Card: {
        const FriendRowFrame *fr = FindFriend(st, g_card);
        if (!fr) { g_view = View::List; return true; }
        const std::vector<CardButton> buttons = ButtonsOf(st, *fr);
        switch (nav) {
        case Nav::Up: g_card_focus = std::max(0, g_card_focus - 1); g_confirm_unfriend = false; break;
        case Nav::Down: g_card_focus = std::min(static_cast<int>(buttons.size()) - 1, g_card_focus + 1); g_confirm_unfriend = false; break;
        case Nav::Accept: ActivateButton(*fr, buttons[static_cast<size_t>(std::clamp(g_card_focus, 0, static_cast<int>(buttons.size()) - 1))]); break;
        case Nav::Back:
        case Nav::Left: g_view = View::List; g_confirm_unfriend = false; break;
        default: break;
        }
        return true;
    }
    case View::Keyboard: {
        const auto &keys = Keys();
        switch (nav) {
        case Nav::Up:
        case Nav::Down: {
            const float x = KeyCentre(g_krow, g_kcol);
            g_krow = std::clamp(g_krow + (nav == Nav::Up ? -1 : 1), 0, static_cast<int>(keys.size()) - 1);
            g_kcol = NearestKey(g_krow, x);
            break;
        }
        case Nav::Left: if (g_kcol > 0) --g_kcol; break;
        case Nav::Right:
            if (g_kcol + 1 < static_cast<int>(keys[static_cast<size_t>(g_krow)].size())) ++g_kcol;
            break;
        case Nav::Accept: PressKey(keys[static_cast<size_t>(g_krow)][static_cast<size_t>(g_kcol)]); break;
        case Nav::X: Backspace(); break;
        case Nav::Y: TypeChar(U' '); break;
        case Nav::Back: g_view = View::List; g_sending = false; break;
        default: break;
        }
        return true;
    }
    }
    return true;
}

// A key on the game window's keyboard while the sidebar is open (input.cpp): true when consumed. Typing is WM_CHAR (FriendsChar).
bool FriendsKeyDown(unsigned vk) {
    if (!FriendsTyping()) return false;
    switch (vk) {
    case VK_BACK: Backspace(); return true;
    case VK_RETURN: Submit(); return true;
    case VK_ESCAPE: g_view = View::List; g_sending = false; return true;
    case VK_SPACE: return true; // the character arrives as WM_CHAR
    default: return false;      // arrows still move over the keys
    }
}

// Every frame, drawn or not (ui.cpp DrawUi): what the launcher sent is applied, and the list is asked for while the guide is open.
void FriendsTick() {
    const bool open = SidebarOpen();
    if (open && !g_was_open) {
        g_refresh_at = 0;
        g_in = false;
        g_view = View::List;
        g_focus = 0;
        g_scroll = 0;
        g_sending = false;
    }
    if (!open && g_was_open) g_in = false;
    g_was_open = open;
    DrainInput();
    const ULONGLONG now = GetTickCount64();
    if (open && !OfflineSession() && now - g_refresh_at > 30000) { // the list stays current while the guide is open
        g_refresh_at = now;
        Act(kFriendsRefresh);
    }
}

void DrawFriendsSidebar(ImDrawList *dl, float width, float height, float alpha) {
    const bool open = SidebarOpen();
    (void)open;    Layout l;
    l.w = FriendsSidebarWidth();
    l.x = width - l.w * alpha;
    l.y = 0;
    l.h = height;
    l.pad = 22 * s;
    l.top = 112 * s;
    l.bottom = height - 64 * s;
    ColumnPanel(dl, l.x, l.w, height, alpha);
    const ImVec2 mouse = ImGui::GetIO().MousePos;
    if (ImGui::IsMouseClicked(0) && !Blocked()) { // a click outside gives the controller back to the guide
        if (mouse.x < l.x) g_in = false;
    }
    dl->AddRectFilledMultiColor(ImVec2(l.x, 0), ImVec2(l.x + l.w, 150 * s), Rgba(230, 0, 126, 0.16f * alpha), Rgba(230, 0, 126, 0.06f * alpha),
                                Rgba(20, 20, 22, 0), Rgba(20, 20, 22, 0));
    if (g_view == View::List || !g_view_have) {
        TextAt(dl, f.heading, ImVec2(l.x + l.pad, 30 * s), kText, T("FRIENDS", "AMIGOS"), alpha);
        size_t online = 0;
        for (const auto &fr : g_view_state.friends) online += fr.Has(kFriendOnline) ? 1 : 0;
        if (g_view_have && g_view_state.loaded())
            TextAt(dl, f.small, ImVec2(l.x + l.pad, 70 * s), kTextDim, (std::to_string(online) + " " + T("online", "online") + " · " + std::to_string(g_view_state.friends.size()) + " " +
                                                                        T("friends", "amigos")).c_str(), alpha);
    } else if (g_view == View::Card) {
        TextAt(dl, f.tag, ImVec2(l.x + l.pad, 36 * s), kPinkU, T("GAMERCARD", "GAMERCARD"), alpha);
    }
    if (OfflineSession()) {
        TextAt(dl, f.body, ImVec2(l.x + l.pad, l.top + 20 * s), kTextDim, T("Offline mode", "Modo offline"));
        TextAt(dl, f.small, ImVec2(l.x + l.pad, l.top + 52 * s), kTextDim, T("Sign in to the Infinity Launcher to see your friends.", "Entre no Infinity Launcher para ver seus amigos."), alpha);
        return;
    }
    if (!g_view_have) {
        TextAt(dl, f.body, ImVec2(l.x + l.pad, l.top + 20 * s), kTextDim, T("Loading friends...", "Carregando amigos..."), alpha);
        return;
    }
    if (!g_view_state.loaded() && g_view == View::List) {
        // the Add friend row still works; the list says it could not load
        DrawList(dl, l, g_view_state, true, alpha);
        TextAt(dl, f.small, ImVec2(l.x + l.pad, l.top + 90 * s), kAmber, T("Your friends list is unavailable right now", "Sua lista de amigos está indisponível agora"), alpha);
    } else if (g_view == View::List) {
        DrawList(dl, l, g_view_state, true, alpha);
        if (g_view_state.friends.empty() && g_view_state.requests.empty() && g_view_state.joins.empty())
            TextAt(dl, f.small, ImVec2(l.x + l.pad, l.top + 90 * s), kTextDim, T("No friends yet. Add one by their profile name.", "Nenhum amigo ainda. Adicione pelo nome do perfil."), alpha);
    } else if (g_view == View::Card) {
        DrawCard(dl, l, g_view_state, alpha);
    } else {
        DrawKeyboard(dl, l, alpha);
    }
    if (!g_in) {
        const char *enter = T("Right: Friends", "Direita: Amigos");
        TextAt(dl, f.small, ImVec2(l.x + l.pad, height - 34 * s), kTextDim, enter, alpha);
        return;
    }
    switch (g_view) {
    case View::List: HintLine(dl, l, alpha, {{"A", T("Open", "Abrir")}, {"X", T("Ask to join", "Pedir")}, {"Y", T("Invite", "Convidar")}, {"B", T("Back", "Voltar")}}); break;
    case View::Card: HintLine(dl, l, alpha, {{"A", T("Select", "Selecionar")}, {"B", T("Back", "Voltar")}}); break;
    case View::Keyboard: HintLine(dl, l, alpha, {{"A", T("Type", "Digitar")}, {"X", T("Delete", "Apagar")}, {"Y", T("Space", "Espaço")}, {"B", T("Cancel", "Cancelar")}}); break;
    }
}

} // namespace fi
