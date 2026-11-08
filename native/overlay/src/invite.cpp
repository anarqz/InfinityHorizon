// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the invite toasts (owner 2026-11-08, docs/launcher/batch-1107/INVITE_UX.md; WORLDS_OFFICIAL §12.5). A friend
// invited this player to a private lobby: the launcher sends 0x23 InvitePrompt and a toast slides in at the TOP RIGHT with the inviter's
// avatar (the 0x2D cache of the FRIENDS sidebar; the initials until it arrives), "{name} invites you to play" and "Hold (Y) to accept".
//
// - Holding Y (a keyboard: F6, kHoldKeyName) fills a ring around the glyph in HoldRing::kHoldSeconds (1.2 s); released early, it drains.
//   When it completes the top toast's invite is accepted (0x24 InviteAnswer, accept) and the toast slides out; nothing else is shown
//   (the game shows its own join). There is no decline: a toast left alone slides out at the end of its answer window (the invite's own
//   time, the countdown) and nothing is sent (the server's invite lapses on its own).
// - Up to three toasts stack under each other, the oldest on top (the one a hold accepts); more wait their turn (lobby_policy.h
//   InviteQueue). A Y already held when a toast comes never accepts it: it must be released first (HoldRing).
// - While a toast is on screen the game's pad reading has Y taken out (input.cpp), and F6 never reaches the game, so the hold is no press
//   in the game (FH2's Y is the rewind); the Y that completed a hold stays held back until it is released.
// - With the guide open the toasts are hidden (Y is the guide's there) and keep counting down; they come back when it closes.
// - The launcher holds new invites back while this player races (InviteInbox.Holding); they come after the race with their time left.
#include "overlay.h"

#include <algorithm>
#include <atomic>
#include <cmath>
#include <map>
#include <mutex>
#include <string>

#include "lobby_policy.h"
#include "style.h"

namespace fi {

using namespace ui;

namespace {

std::mutex g_lock;
InviteQueue g_queue; // under g_lock: the pipe thread offers, the render thread shows and answers

// render thread
HoldRing g_ring;
bool g_held = false;                 // the pad's Y or the keyboard's F6, this frame (input.cpp)
bool g_dev_hold = false;             // the dev host's command 28
std::map<std::string, float> g_ys;   // each drawn toast's animated top
std::atomic<bool> g_on_screen{false}; // a live toast shows (the game's input thread reads it)

const char *ActivityName(const std::string &activity) {
    if (activity == "freeroam") return T("Free Roam", "Free Roam");
    if (activity == "roadtrip") return T("Road Trip", "Road Trip");
    if (activity == "race") return T("Race", "Corrida");
    return T("Private lobby", "Lobby privado");
}

// The inviter's disc: its avatar once the launcher sent it (0x2D, keyed by the FRIENDS row of that name), else its initials.
void Avatar(ImDrawList *dl, ImVec2 c, float r, const std::string &name, float alpha) {
    float u0 = 0, v0 = 0, u1 = 1, v1 = 1;
    const std::string id = FriendIdByName(name);
    const ImU32 white = Rgba(255, 255, 255, alpha);
    if (const uint64_t texture = id.empty() ? 0 : FriendAvatarTexture(id, u0, v0, u1, v1)) {
        dl->AddImageRounded(static_cast<ImTextureID>(texture), ImVec2(c.x - r, c.y - r), ImVec2(c.x + r, c.y + r), ImVec2(u0, v0), ImVec2(u1, v1), white, r);
        return;
    }
    dl->AddCircleFilled(c, r, Rgba(230, 0, 126, alpha), 40);
    std::string initials = name.substr(0, std::min<size_t>(name.size() >= 2 && static_cast<unsigned char>(name[0]) < 0x80 ? 2 : 1, name.size()));
    for (auto &ch : initials) ch = static_cast<char>(ch >= 'a' && ch <= 'z' ? ch - 32 : ch);
    const ImVec2 size = Measure(f.title, initials.c_str());
    TextAt(dl, f.title, ImVec2(c.x - size.x / 2, c.y - size.y / 2), kText, initials.c_str(), alpha);
}

// The glyph (the pad's yellow Y, or the F6 keycap) with the hold ring around it, `progress` 0..1 filled clockwise from the top.
void HoldGlyph(ImDrawList *dl, ImVec2 c, float r, float progress, bool pad, float alpha) {
    dl->AddCircle(c, r + 4 * s, Rgba(255, 255, 255, 0.22f * alpha), 40, 3 * s);
    if (progress > 0) {
        constexpr float kTop = -1.5707963f, kTurn = 6.2831853f;
        dl->PathArcTo(c, r + 4 * s, kTop, kTop + kTurn * std::min(1.0f, progress), 48);
        dl->PathStroke(Rgba(255, 185, 0, alpha), ImDrawFlags_None, 3.5f * s);
    }
    if (pad) {
        FaceButton(dl, c, r, kYellow, "Y");
        return;
    }
    const ImVec2 a(c.x - r, c.y - r * 0.8f), b(c.x + r, c.y + r * 0.8f);
    dl->AddRectFilled(a, b, Rgba(58, 58, 64, alpha), 4 * s);
    dl->AddRect(a, b, Rgba(255, 255, 255, 0.5f * alpha), 4 * s, 0, 1.2f * s);
    const ImVec2 ts = Measure(f.tag, kHoldKeyName);
    TextAt(dl, f.tag, ImVec2(c.x - ts.x / 2, c.y - ts.y / 2), kText, kHoldKeyName, alpha);
}

void Accept() {
    std::string id;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_queue.Answer(&id, GetTickCount64())) return;
    }
    if (DevHost()) {
        Log("invite: dev host: accepted by a hold (no launcher)");
        return;
    }
    SendInviteAnswer(id, true);
    Log("invite: accepted by a hold; the launcher joins and moves the game");
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

bool InviteToastsOnScreen() { return g_on_screen.load(); }

void SetInviteHold(bool held) { g_held = held; }

void DevHoldInvite() {
    if (DevHost()) g_dev_hold = true;
}

void DrawInviteToasts(float width, float height, float dt) {
    (void)height;
    const uint64_t now = GetTickCount64();
    std::vector<InviteQueue::Entry> drawn;

    std::string top_id;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (g_queue.Expire(now)) Log("invite: a prompt's time ran out without an answer");
        if (const auto *current = g_queue.Current()) top_id = current->prompt.id;
        if (!SidebarOpen()) drawn = g_queue.Drawn(now);
    }
    const bool live = !top_id.empty() && !SidebarOpen() && !InPassthrough();
    g_on_screen = live;
    if (!live) g_ring.Reset();
    else if (g_ring.Update(g_held || g_dev_hold, dt)) Accept();
    if (drawn.empty()) {
        g_ys.clear();
        return;
    }
    const float w = std::min(440 * s, width - 32 * s), h = 104 * s, margin = 24 * s, gap = 12 * s;
    const bool pad = PadPresent();
    ImDrawList *dl = ImGui::GetForegroundDrawList();
    std::map<std::string, float> ys;
    float slot = margin;
    for (const auto &e : drawn) {
        const bool leaving = !e.live();
        float &y = g_ys.try_emplace(e.prompt.id, slot).first->second;
        if (!leaving) y += (slot - y) * std::min(1.0f, dt * 14.0f); // a leaving toast keeps its place while it slides out
        ys[e.prompt.id] = y;
        const float in = e.shown_ms ? EaseOutCubic(static_cast<float>(now - e.shown_ms) / InviteQueue::kSlideMs) : 0.0f;
        const float out = leaving ? EaseInCubic(static_cast<float>(now - e.leaving_ms) / InviteQueue::kSlideMs) : 0.0f;
        const float shift = (1.0f - in + out) * (w + margin);
        const float alpha = std::clamp(in - out, 0.0f, 1.0f);
        const ImVec2 a(width - margin - w + shift, y), b(width - margin + shift, y + h);
        if (!leaving) slot += h + gap;
        if (alpha <= 0.01f) continue;
        const bool first = e.prompt.id == top_id;
        Shadow(dl, a, b, 12 * s, alpha);
        dl->AddRectFilled(a, b, Rgba(20, 20, 22, 0.96f * alpha), 12 * s);
        dl->AddRect(a, b, first ? Rgba(230, 0, 126, alpha) : Rgba(255, 255, 255, 0.14f * alpha), 12 * s, 0, (first ? 2.0f : 1.2f) * s);
        const float pad_x = 16 * s, disc = 30 * s;
        Avatar(dl, ImVec2(a.x + pad_x + disc, a.y + h / 2 - 6 * s), disc, e.prompt.from, alpha);
        const float tx = a.x + pad_x + 2 * disc + 14 * s;
        // the time left, top right; the line under it empties with the answer window
        const uint32_t left = InviteQueue::SecondsLeft(e, now);
        const std::string time = std::to_string(left) + " s";
        const ImVec2 tsz = Measure(f.small, time.c_str());
        TextAt(dl, f.small, ImVec2(b.x - pad_x - tsz.x, a.y + 12 * s), left <= 15 ? kAmber : kTextDim, time.c_str(), alpha);
        const std::string line = e.prompt.from + T(" invites you to play", " convida você para jogar");
        dl->PushClipRect(ImVec2(tx, a.y), ImVec2(b.x - pad_x - tsz.x - 8 * s, b.y), true);
        TextAt(dl, f.body, ImVec2(tx, a.y + 12 * s), kText, line.c_str(), alpha);
        dl->PopClipRect();
        TextAt(dl, f.small, ImVec2(tx, a.y + 36 * s), kTextDim, ActivityName(e.prompt.activity), alpha);
        const float hy = a.y + 74 * s;
        if (first && !leaving) { // "Hold (Y) to accept", the ring on the glyph
            const char *hold = T("Hold", "Segure"), *accept = T("to accept", "para aceitar");
            const ImVec2 hs = Measure(f.small, hold);
            TextAt(dl, f.small, ImVec2(tx, hy - hs.y / 2), kText, hold, alpha);
            const float r = 11 * s;
            const ImVec2 gc(tx + hs.x + 8 * s + r + 4 * s, hy);
            HoldGlyph(dl, gc, r, g_ring.Progress(), pad, alpha);
            TextAt(dl, f.small, ImVec2(gc.x + r + 12 * s, hy - hs.y / 2), kText, accept, alpha);
        } else if (!leaving) {
            TextAt(dl, f.small, ImVec2(tx, hy - Measure(f.small, "N").y / 2), kTextDim, T("Next in line", "Próximo da fila"), alpha);
        }
        const float total = e.prompt.expires ? static_cast<float>(e.prompt.expires) : 1.0f;
        const float frac = std::clamp((static_cast<float>(e.deadline_ms) - static_cast<float>(now)) / (total * 1000.0f), 0.0f, 1.0f);
        dl->AddRectFilled(ImVec2(a.x + 12 * s, b.y - 5 * s), ImVec2(a.x + 12 * s + (w - 24 * s) * frac, b.y - 3 * s), Rgba(230, 0, 126, 0.8f * alpha), 1 * s);
    }
    g_ys.swap(ys);
}

} // namespace fi
