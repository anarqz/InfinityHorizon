// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the invite prompt (OD-204 T-L3, WORLDS_OFFICIAL §12.5). A friend invited this player to a private lobby: the
// launcher sends 0x23 InvitePrompt and this modal asks Accept or Decline; the answer goes back as 0x24 InviteAnswer. It is never a
// toast: toasts follow OD-192 and are dropped in official lobbies, and an invitee may well be in one.
//
// - With the guide closed the prompt shows as a card at the top of the screen with how to open the guide; it never takes the pad
//   from the game (an invite can arrive mid-race). With the guide open it is modal: it holds the controller (Left/Right or Up/Down
//   choose, A answers, B closes the guide and leaves the prompt waiting) and the mouse (a click outside it does nothing). Decline
//   is focused first, so an A meant for the guide never accepts an invite by accident.
// - One prompt at a time; others wait (lobby_policy.h InviteQueue). The time left counts down; at 0 the prompt goes without an
//   answer (the server's invite expires on its own, MATCHMAKING_API §3.4: 2 minutes).
// - What happens after Accept (the join, the proof, moving the game) is the launcher's (T-L5); this file only asks.
#include "overlay.h"

#include <algorithm>
#include <mutex>
#include <string>

#include "lobby_policy.h"
#include "style.h"

namespace fi {

using namespace ui;

namespace {

std::mutex g_lock;
InviteQueue g_queue; // under g_lock: the pipe thread offers, the render thread shows and answers

const char *ActivityName(const std::string &activity) {
    if (activity == "freeroam") return T("Free Roam", "Free Roam");
    if (activity == "roadtrip") return T("Road Trip", "Road Trip");
    return T("Lobby", "Lobby");
}

struct Layout {
    ImVec2 a, b;           // the card
    ImVec2 accept_a, accept_b, decline_a, decline_b;
    bool modal = false;    // the guide is open: centred, with buttons
};

Layout LayoutOf(float width, float height, bool modal) {
    Layout l;
    l.modal = modal;
    const float w = std::min(560 * s, width - 48 * s), h = modal ? 250 * s : 118 * s;
    const float x = (width - w) / 2, y = modal ? (height - h) / 2 : 40 * s;
    l.a = ImVec2(x, y);
    l.b = ImVec2(x + w, y + h);
    const float bw = (w - 3 * 24 * s) / 2, bh = 52 * s, by = y + h - 24 * s - bh;
    l.accept_a = ImVec2(x + 24 * s, by);
    l.accept_b = ImVec2(x + 24 * s + bw, by + bh);
    l.decline_a = ImVec2(l.accept_b.x + 24 * s, by);
    l.decline_b = ImVec2(l.decline_a.x + bw, by + bh);
    return l;
}

bool Inside(ImVec2 p, ImVec2 a, ImVec2 b) { return p.x >= a.x && p.x < b.x && p.y >= a.y && p.y < b.y; }

// The player answered the shown prompt (render thread).
void Answer(bool accept) {
    std::string id;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_queue.Answer(&id)) return;
    }
    if (DevHost()) {
        Log("invite: dev host: %s (no launcher)", accept ? "accepted" : "declined");
        return;
    }
    SendInviteAnswer(id, accept);
    Log("invite: %s; the launcher acts on it", accept ? "accepted" : "declined");
}

void Button(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, bool primary, const char *text) {
    dl->AddRectFilled(a, b, primary ? kPinkU : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const ImVec2 ts = Measure(f.body, text);
    TextAt(dl, f.body, ImVec2((a.x + b.x - ts.x) / 2, (a.y + b.y - ts.y) / 2), kText, text);
}

} // namespace

void SetInvitePrompt(const InvitePromptFrame &prompt) {
    bool offered;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        offered = g_queue.Offer(prompt, GetTickCount64());
    }
    if (!offered) Log("invite: a prompt was dropped (%zu already waiting)", InviteQueue::kMaxWaiting);
    else if (prompt.expires == 0) Log("invite: a prompt was withdrawn by the launcher");
    else Log("invite: a prompt arrived (%u s to answer)", static_cast<unsigned>(prompt.expires));
}

void InvitesDisconnected() {
    std::lock_guard<std::mutex> lock(g_lock);
    g_queue.Clear();
}

bool InvitePromptOpen() {
    std::lock_guard<std::mutex> lock(g_lock);
    return g_queue.Current() != nullptr;
}

bool InviteNavigate(Nav nav) {
    if (!SidebarOpen()) return false;
    int focus = 0;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_queue.Current()) return false;
        switch (nav) {
        case Nav::Left:
        case Nav::Up: g_queue.Move(0); return true;
        case Nav::Right:
        case Nav::Down: g_queue.Move(1); return true;
        case Nav::Back: break; // the guide closes; the prompt keeps waiting (below, outside the lock)
        case Nav::Accept: focus = g_queue.Focus(); break;
        default: return true; // modal: tabs, X, Y and Menu do nothing under it
        }
    }
    if (nav == Nav::Back) { // B closes the guide, whatever screen is under the prompt; nothing is answered
        CloseSidebar();
        return true;
    }
    Answer(focus == 0);
    return true;
}

void InviteMouse(float width, float height) {
    if (!SidebarOpen() || !ImGui::IsMouseClicked(0) || !InvitePromptOpen()) return;
    // the guide is open and a prompt shows: a click on a button answers; any other click is eaten (modal), before anything
    // under the prompt sees it
    const Layout l = LayoutOf(width, height, true);
    const ImVec2 at = ImGui::GetIO().MousePos;
    ImGui::GetIO().MouseDown[0] = false;
    if (Inside(at, l.accept_a, l.accept_b)) Answer(true);
    else if (Inside(at, l.decline_a, l.decline_b)) Answer(false);
}

void DrawInvitePrompt(float width, float height, float dt) {
    (void)dt;
    const uint64_t now = GetTickCount64();
    InvitePromptFrame prompt;
    uint32_t left = 0;
    size_t waiting = 0;
    int focus = 0;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (g_queue.Expire(now)) Log("invite: a prompt expired without an answer");
        const auto *current = g_queue.Current();
        if (!current) return;
        prompt = current->prompt;
        left = g_queue.SecondsLeft(now);
        waiting = g_queue.Waiting();
        focus = g_queue.Focus();
    }
    const bool modal = SidebarOpen();
    const Layout l = LayoutOf(width, height, modal);
    ImDrawList *dl = ImGui::GetForegroundDrawList();
    if (modal) dl->AddRectFilled(ImVec2(0, 0), ImVec2(width, height), Rgba(0, 0, 0, 0.45f));
    Shadow(dl, l.a, l.b, 10 * s, 1.0f);
    dl->AddRectFilled(l.a, l.b, kPanel, 10 * s);
    dl->AddRect(l.a, l.b, kPinkU, 10 * s, 0, 2 * s);
    const float pad = 24 * s, x = l.a.x + pad, w = l.b.x - l.a.x - 2 * pad;
    float y = l.a.y + 18 * s;
    TextAt(dl, f.tag, ImVec2(x, y), kPinkU, T("PRIVATE LOBBY INVITE", "CONVITE PARA LOBBY PRIVADO"));
    const std::string time = std::to_string(left) + " s";
    const ImVec2 ts = Measure(f.small, time.c_str());
    TextAt(dl, f.small, ImVec2(l.b.x - pad - ts.x, y - 2 * s), left <= 15 ? kAmber : kTextDim, time.c_str());
    y += 22 * s;
    const std::string line = prompt.from + T(" invites you to a private lobby", " convida você para um lobby privado");
    dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, y + 34 * s), true);
    TextAt(dl, f.title, ImVec2(x, y), kText, line.c_str());
    dl->PopClipRect();
    y += 32 * s;
    std::string sub = ActivityName(prompt.activity);
    if (waiting > 1) sub += std::string(" · +") + std::to_string(waiting - 1) + T(" more waiting", " na fila");
    TextAt(dl, f.small, ImVec2(x, y), kTextDim, sub.c_str());
    if (!modal) {
        y += 24 * s;
        TextAt(dl, f.small, ImVec2(x, y), kText,
               T("Open the guide to answer: hold LB + RB, or press Shift+Tab", "Abra o guia para responder: segure LB + RB, ou pressione Shift+Tab"));
        return;
    }
    Button(dl, l.accept_a, l.accept_b, focus == 0, true, T("Accept", "Aceitar"));
    Button(dl, l.decline_a, l.decline_b, focus == 1, false, T("Decline", "Recusar"));
}

} // namespace fi
