// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the private lobby's friend picker (OD-204 T-L4, WORLDS_OFFICIAL §12.5). The launcher opens it (0x25 FriendPicker
// with the open flag) when the game's Invite friends or the Private Lobby pane's button asks, and only for the lobby's leader; it
// lists the player's friends only, each marked in this lobby, invited (a pending invite) or offline. Choosing an available friend
// sends 0x27 InviteSend; the launcher answers with one notice and a refreshed list.
//
// - The picker is modal inside the guide: it opens the guide, holds the controller (Up/Down choose a row, A invites, B closes the
//   picker and leaves the guide open) and the mouse (a click on a row invites it, on Close closes, anywhere else is eaten).
// - Closing the guide closes the picker; a launcher disconnect clears it. An invite prompt (invite.cpp) is drawn above it.
// - The list and its rules are lobby_policy.h FriendPickerModel; this file only draws and routes input.
#include "overlay.h"

#include <algorithm>
#include <atomic>
#include <mutex>
#include <string>

#include "lobby_policy.h"
#include "style.h"

namespace fi {

using namespace ui;

namespace {

std::mutex g_lock;
FriendPickerModel g_model;               // under g_lock: the pipe thread sets it, the render thread draws and answers
std::atomic<bool> g_open_guide{false};   // the launcher asked to open it: the render thread opens the guide

struct Layout {
    ImVec2 a, b;             // the card
    float list_y = 0, row_h = 0;
    int rows_visible = 0;
    ImVec2 close_a, close_b;
};

Layout LayoutOf(float width, float height, size_t rows) {
    Layout l;
    const float w = std::min(620 * s, width - 48 * s);
    l.row_h = 52 * s;
    const int fit = static_cast<int>(std::max(1.0f, (height - 260 * s) / l.row_h));
    l.rows_visible = std::max(1, std::min(fit, std::max(1, static_cast<int>(rows))));
    const float h = 150 * s + l.rows_visible * l.row_h;
    const float x = (width - w) / 2, y = (height - h) / 2;
    l.a = ImVec2(x, y);
    l.b = ImVec2(x + w, y + h);
    l.list_y = y + 84 * s;
    l.close_b = ImVec2(l.b.x - 24 * s, l.b.y - 18 * s);
    l.close_a = ImVec2(l.close_b.x - 160 * s, l.close_b.y - 44 * s);
    return l;
}

bool Inside(ImVec2 p, ImVec2 a, ImVec2 b) { return p.x >= a.x && p.x < b.x && p.y >= a.y && p.y < b.y; }

const char *StateName(uint8_t state) {
    switch (state) {
    case kPickerInLobby: return T("In this lobby", "Neste lobby");
    case kPickerInvited: return T("Invited", "Convidado");
    case kPickerOffline: return T("Offline", "Offline");
    default: return T("Invite", "Convidar");
    }
}

// The focused row was chosen (render thread): an available friend is invited.
void Choose() {
    std::string xuid;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        xuid = g_model.Choose();
    }
    if (xuid.empty()) return;
    if (DevHost()) {
        Log("invite: picker: dev host: a friend chosen (no launcher)");
        return;
    }
    SendInviteSend(xuid);
    Log("invite: picker: a friend chosen; the launcher sends the invite");
}

int g_scroll = 0; // render thread: the first row drawn

} // namespace

void SetFriendPicker(const FriendPickerFrame &picker) {
    bool shown;
    {
        std::lock_guard<std::mutex> lock(g_lock); // the open request and the model change together (review L4: FriendPickerTick)
        g_model.Set(picker);
        shown = g_model.Shown();
        if (picker.open()) g_open_guide = true;
    }
    if (picker.open()) {
        Log("invite: picker: opened by the launcher (%zu friends%s)", picker.friends.size(), picker.status == kPickerList ? "" : ", list unavailable");
    } else if (shown) {
        Log("invite: picker: refreshed");
    }
}

void FriendPickerDisconnected() {
    std::lock_guard<std::mutex> lock(g_lock);
    g_model.Clear();
    g_open_guide = false;
}

bool FriendPickerOpen() {
    std::lock_guard<std::mutex> lock(g_lock);
    return g_model.Shown();
}

void FriendPickerTick() {
    if (g_open_guide.exchange(false) && !SidebarOpen()) ToggleSidebar();
    if (!SidebarOpen()) {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_open_guide.load()) g_model.Close(); // the guide closed: so does the picker (never one the launcher just opened)
    }
}

bool PickerNavigate(Nav nav) {
    if (!SidebarOpen()) return false;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_model.Shown()) return false;
        switch (nav) {
        case Nav::Up: g_model.Move(-1); return true;
        case Nav::Down: g_model.Move(1); return true;
        case Nav::Back: g_model.Close(); return true; // the picker closes, the guide stays
        case Nav::Accept: break;
        default: return true; // modal: tabs and the rest do nothing under it
        }
    }
    Choose();
    return true;
}

void PickerMouse(float width, float height) {
    if (!SidebarOpen() || !ImGui::IsMouseClicked(0) || InvitePromptOpen()) return;
    size_t rows;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_model.Shown()) return;
        rows = g_model.Rows().size();
    }
    const Layout l = LayoutOf(width, height, rows);
    const ImVec2 at = ImGui::GetIO().MousePos;
    ImGui::GetIO().MouseDown[0] = false; // modal: nothing under the picker sees this click
    if (Inside(at, l.close_a, l.close_b)) {
        std::lock_guard<std::mutex> lock(g_lock);
        g_model.Close();
        return;
    }
    if (at.x < l.a.x || at.x >= l.b.x || at.y < l.list_y) return;
    const int row = g_scroll + static_cast<int>((at.y - l.list_y) / l.row_h);
    if (row < g_scroll || row >= g_scroll + l.rows_visible || row >= static_cast<int>(rows)) return;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        g_model.FocusRow(row);
    }
    Choose();
}

void DrawFriendPicker(float width, float height, float dt) {
    (void)dt;
    if (!SidebarOpen()) return;
    std::vector<PickerFriend> rows;
    uint8_t status;
    int focus;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_model.Shown()) return;
        rows = g_model.Rows();
        status = g_model.Status();
        focus = g_model.Focus();
    }
    const Layout l = LayoutOf(width, height, rows.size());
    if (focus < g_scroll) g_scroll = focus;
    if (focus >= g_scroll + l.rows_visible) g_scroll = focus - l.rows_visible + 1;
    g_scroll = std::max(0, std::min(g_scroll, std::max(0, static_cast<int>(rows.size()) - l.rows_visible)));
    ImDrawList *dl = ImGui::GetForegroundDrawList();
    dl->AddRectFilled(ImVec2(0, 0), ImVec2(width, height), Rgba(0, 0, 0, 0.45f));
    Shadow(dl, l.a, l.b, 10 * s, 1.0f);
    dl->AddRectFilled(l.a, l.b, kPanel, 10 * s);
    dl->AddRect(l.a, l.b, kPinkU, 10 * s, 0, 2 * s);
    const float pad = 24 * s, x = l.a.x + pad, w = l.b.x - l.a.x - 2 * pad;
    TextAt(dl, f.tag, ImVec2(x, l.a.y + 18 * s), kPinkU, T("PRIVATE LOBBY", "LOBBY PRIVADO"));
    TextAt(dl, f.title, ImVec2(x, l.a.y + 38 * s), kText, T("Invite friends", "Convidar amigos"));
    float y = l.list_y;
    if (status != kPickerList) {
        TextAt(dl, f.body, ImVec2(x, y + 12 * s), kTextDim, T("Your friends list is unavailable right now", "Sua lista de amigos está indisponível agora"));
    } else if (rows.empty()) {
        TextAt(dl, f.body, ImVec2(x, y + 12 * s), kTextDim, T("No friends to invite yet", "Nenhum amigo para convidar ainda"));
    }
    for (int i = g_scroll; i < static_cast<int>(rows.size()) && i < g_scroll + l.rows_visible; ++i, y += l.row_h) {
        const PickerFriend &row = rows[static_cast<size_t>(i)];
        const ImVec2 ra(x, y), rb(x + w, y + l.row_h - 6 * s);
        const bool available = row.state == kPickerAvailable;
        dl->AddRectFilled(ra, rb, kTile, 6 * s);
        if (i == focus) FocusRing(dl, ra, rb, 6 * s);
        dl->PushClipRect(ImVec2(ra.x + 14 * s, ra.y), ImVec2(rb.x - 170 * s, rb.y), true);
        const ImVec2 ns = Measure(f.body, row.gamertag.c_str());
        TextAt(dl, f.body, ImVec2(ra.x + 14 * s, (ra.y + rb.y - ns.y) / 2), available ? kText : kTextDim, row.gamertag.c_str());
        dl->PopClipRect();
        const char *state = StateName(row.state);
        const ImVec2 ss = Measure(f.small, state);
        TextAt(dl, f.small, ImVec2(rb.x - 14 * s - ss.x, (ra.y + rb.y - ss.y) / 2), available ? kPinkU : kTextDim, state);
    }
    dl->AddRectFilled(l.close_a, l.close_b, kTile, 6 * s);
    const char *close = T("Close", "Fechar");
    const ImVec2 cs = Measure(f.body, close);
    TextAt(dl, f.body, ImVec2((l.close_a.x + l.close_b.x - cs.x) / 2, (l.close_a.y + l.close_b.y - cs.y) / 2), kText, close);
    TextAt(dl, f.small, ImVec2(x, l.close_a.y + 12 * s), kTextDim, T("A invite  ·  B close", "A convidar  ·  B fechar"));
}

} // namespace fi
