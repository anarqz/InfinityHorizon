// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the right pane (OD-112). One component for every detail view, beside the
// guide: a header (avatar, title, subtitle), ordered sections (key/value rows or items with an
// icon) and up to four actions. A new kind of pane is new data (ui_frames.h DetailPane), never a
// new screen.
//
// - Hover: the worlds tab hands the focused or hovered lobby row each frame; no input of its own.
// - Modal: a pane from the launcher (0x16) or the game's player card. It takes the controller
//   (up/down the actions, A an enabled action, B closes) until closed or the guide closes. The
//   player card opens at once with what the game gave (the gamertag, an avatar placeholder and a
//   disabled "Add friend") and asks the launcher (0x17); its answer for the same player replaces
//   it, an answer for a player card already closed is dropped.
#include "overlay.h"

#include <algorithm>
#include <cctype>
#include <mutex>

#include "imgui.h"
#include "style.h"

namespace fi {

using namespace ui;

namespace {

std::mutex g_lock;
DetailPane g_pending;      // from the launcher or the player card, for the render thread
bool g_pending_set = false;
bool g_pending_local = false; // the player card's placeholder (not the launcher's answer)

// render thread
DetailPane g_modal;
bool g_modal_open = false;
DetailPane g_hover;
bool g_hover_set = false;
int g_focus = 0;
bool g_accept = false;
float g_dev_reply = -1; // the dev host's launcher answers the player card
std::string g_dev_xuid, g_dev_gamertag;

DetailPane PlayerPlaceholder(const std::string &xuid, const std::string &gamertag) {
    DetailPane p;
    p.id = "player:" + xuid;
    p.title = gamertag.empty() ? T("Player", "Jogador") : gamertag;
    p.subtitle = T("Loading profile...", "Carregando perfil...");
    p.actions.push_back({"friend.add", T("Add friend", "Adicionar amigo"), false});
    return p;
}

DetailPane DevPlayer(const std::string &xuid, const std::string &gamertag) {
    DetailPane p;
    p.id = "player:" + xuid;
    p.title = gamertag;
    p.subtitle = "Infinity Supporter";
    p.sections.push_back({0, T("PROFILE", "PERFIL"), {{T("Mode", "Modo"), "Hardcore", PaneIcon::None},
                                                        {T("Lobby", "Lobby"), "Sunset Cruise", PaneIcon::None}}});
    p.sections.push_back({1, T("FRIENDS", "AMIGOS"), {{T("Friends are coming soon", "Amigos chegam em breve"), "", PaneIcon::Player}}});
    p.actions.push_back({"friend.add", T("Add friend", "Adicionar amigo"), false});
    return p;
}

const char *IconGlyph(PaneIcon icon) {
    switch (icon) {
    case PaneIcon::Player: return kIconContact;
    case PaneIcon::Host: return kIconHome;
    case PaneIcon::Supporter: return kIconStar;
    case PaneIcon::Mod: return kIconSettings;
    case PaneIcon::Warning: return kIconWarning;
    case PaneIcon::Check: return kIconCheck;
    default: return "";
    }
}

ImU32 IconColor(PaneIcon icon) {
    switch (icon) {
    case PaneIcon::Supporter: return kPinkU;
    case PaneIcon::Warning: return kAmber;
    case PaneIcon::Check: return IM_COL32(92, 214, 92, 255);
    default: return kTextDim;
    }
}

ImU32 Fade(ImU32 color, float alpha) { return (color & 0x00FFFFFF) | (static_cast<ImU32>(((color >> 24) & 0xFF) * alpha) << 24); }

void Avatar(ImDrawList *dl, ImVec2 centre, float radius, const DetailPane &p, float alpha) {
    dl->AddCircleFilled(centre, radius + 3 * s, Fade(kText, alpha), 48);
    dl->AddCircleFilled(centre, radius, Fade(kPinkU, alpha), 48);
    // "icon:<name>": a built-in glyph; anything else (custom avatars, soon): the title's initials for now
    const char *glyph = p.avatar == "icon:world" ? kIconWorld : p.avatar == "icon:player" ? kIconContact : nullptr;
    if (glyph) {
        const ImVec2 g = Measure(f.icon_big, glyph);
        dl->AddText(f.icon_big, f.icon_big->FontSize, ImVec2(centre.x - g.x / 2, centre.y - g.y / 2), Fade(kText, alpha), glyph);
        return;
    }
    std::string initials;
    for (char c : p.title) {
        if (std::isalnum(static_cast<unsigned char>(c))) initials += static_cast<char>(std::toupper(static_cast<unsigned char>(c)));
        if (initials.size() == 2) break;
    }
    if (initials.empty()) initials = "?";
    const ImVec2 t = Measure(f.heading, initials.c_str());
    dl->AddText(f.heading, f.heading->FontSize, ImVec2(centre.x - t.x / 2, centre.y - t.y / 2), Fade(kText, alpha), initials.c_str());
}

void Activate(const DetailPane &p, const PaneAction &a) {
    if (!a.enabled) return; // a disabled action is shown, never sent
    if (DevHost()) {
        PushToast(ToastKind::Info, a.label, p.title);
    } else {
        SendPaneAction(p.id, a.id);
    }
}

} // namespace

void SetDetailPane(DetailPane pane) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_pending = std::move(pane);
    g_pending_set = true;
    g_pending_local = false;
}

void OpenPlayerCard(const std::string &xuid, const std::string &gamertag) {
    if (xuid.empty() || xuid.size() > 20 || !std::all_of(xuid.begin(), xuid.end(), [](char c) { return c >= '0' && c <= '9'; })) return;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        g_pending = PlayerPlaceholder(xuid, gamertag);
        g_pending_set = true;
        g_pending_local = true;
    }
    if (!DevHost()) SendPlayerDetailsQuery(xuid, gamertag); // the dev host answers in PaneTick
}

void SetHoverPane(const DetailPane *pane) {
    g_hover_set = pane != nullptr;
    if (pane) g_hover = *pane;
}

bool PaneModal() { return g_modal_open; }

void ClosePane() {
    g_modal_open = false;
    g_modal = DetailPane{};
    g_focus = 0;
}

// Render thread, once a frame before the guide: applies what arrived, opens the guide for it.
void PaneTick(float dt) {
    if (g_dev_reply >= 0) {
        g_dev_reply -= dt;
        if (g_dev_reply < 0) SetDetailPane(DevPlayer(g_dev_xuid, g_dev_gamertag));
    }
    DetailPane pane;
    bool local = false;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (!g_pending_set) return;
        pane = std::move(g_pending);
        local = g_pending_local;
        g_pending_set = false;
    }
    if (pane.id.empty()) {
        ClosePane();
        return;
    }
    // the launcher's player pane only updates the card that asked for it
    if (!local && pane.id.rfind("player:", 0) == 0 && !(g_modal_open && g_modal.id == pane.id)) return;
    if (!g_modal_open || g_modal.id != pane.id) g_focus = 0;
    if (local && DevHost()) {
        g_dev_xuid = pane.id.substr(7);
        g_dev_gamertag = pane.title;
        g_dev_reply = 0.4f;
    }
    g_modal = std::move(pane);
    g_modal_open = true;
    if (!SidebarOpen()) ToggleSidebar();
}

bool PaneNavigate(Nav nav) {
    if (!g_modal_open) return false;
    const int count = static_cast<int>(g_modal.actions.size());
    switch (nav) {
    case Nav::Up: if (g_focus > 0) --g_focus; return true;
    case Nav::Down: if (g_focus + 1 < count) ++g_focus; return true;
    case Nav::Accept: g_accept = true; return true;
    case Nav::Back: ClosePane(); return true;
    default: return true; // the pane holds the controller while it is open
    }
}

void DrawPane(ImDrawList *dl, float x, float y0, float w, float height, float alpha) {
    const bool modal = g_modal_open;
    if (!modal && !g_hover_set) {
        g_accept = false;
        return;
    }
    const DetailPane &p = modal ? g_modal : g_hover;
    const ImVec2 a(x, y0), b(x + w, height);
    dl->AddRectFilled(a, b, Fade(kPanel, alpha));
    dl->AddLine(ImVec2(a.x, 0), ImVec2(a.x, height), Rgba(255, 255, 255, 0.08f * alpha));
    const float pad = 26 * s;
    const float cw = w - 2 * pad;
    float y = y0 + 34 * s;
    const float bottom = height - (modal ? 80 * s : 40 * s);

    // header
    const float av = 64 * s;
    Avatar(dl, ImVec2(x + pad + av / 2, y + av / 2), av / 2, p, alpha);
    const float hx = x + pad + av + 18 * s;
    dl->PushClipRect(ImVec2(hx, y), ImVec2(x + w - pad, y + av + 8 * s), true);
    TextAt(dl, f.heading, ImVec2(hx, y + 2 * s), kText, p.title.c_str(), alpha);
    dl->AddText(f.small, f.small->FontSize, ImVec2(hx, y + 40 * s), Fade(kTextDim, alpha), p.subtitle.c_str(), nullptr, x + w - pad - hx);
    dl->PopClipRect();
    y += av + 28 * s;

    // the actions sit at the bottom; the sections fill what is left above them
    const float bh = 50 * s, bgap = 8 * s;
    const float actions_h = p.actions.empty() ? 0 : p.actions.size() * (bh + bgap);
    const float sections_bottom = bottom - actions_h - 8 * s;
    dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, sections_bottom), true);
    for (const auto &section : p.sections) {
        if (y + 30 * s > sections_bottom) break;
        if (!section.heading.empty()) {
            TextAt(dl, f.tag, ImVec2(x + pad, y), kTextDim, section.heading.c_str(), alpha);
            y += 22 * s;
        }
        size_t shown = 0;
        for (const auto &e : section.entries) {
            const float eh = section.kind == 0 ? 30 * s : (e.secondary.empty() ? 40 * s : 52 * s);
            if (y + eh > sections_bottom) break;
            if (section.kind == 0) {
                TextAt(dl, f.small, ImVec2(x + pad, y + 4 * s), kTextDim, e.primary.c_str(), alpha);
                const ImVec2 vs = Measure(f.body, e.secondary.c_str());
                TextAt(dl, f.body, ImVec2(std::max(x + pad + cw * 0.4f, x + w - pad - vs.x), y), kText, e.secondary.c_str(), alpha);
            } else {
                dl->AddRectFilled(ImVec2(x + pad, y), ImVec2(x + w - pad, y + eh - 6 * s), Fade(kTile, alpha), 6 * s);
                float tx = x + pad + 14 * s;
                if (const char *glyph = IconGlyph(e.icon); *glyph) {
                    dl->AddText(f.icon, f.icon->FontSize * 0.85f, ImVec2(tx, y + (eh - 6 * s) / 2 - f.icon->FontSize * 0.43f), Fade(IconColor(e.icon), alpha), glyph);
                    tx += 36 * s;
                }
                TextAt(dl, f.body, ImVec2(tx, y + (e.secondary.empty() ? 8 * s : 5 * s)), e.icon == PaneIcon::Warning ? kAmber : kText, e.primary.c_str(), alpha);
                if (!e.secondary.empty()) TextAt(dl, f.small, ImVec2(tx, y + 28 * s), kTextDim, e.secondary.c_str(), alpha);
            }
            y += eh;
            ++shown;
        }
        if (shown < section.entries.size()) {
            const std::string more = "+" + std::to_string(section.entries.size() - shown) + T(" more", " a mais");
            TextAt(dl, f.small, ImVec2(x + pad, y), kTextDim, more.c_str(), alpha);
            y += 24 * s;
        }
        y += 16 * s;
    }
    dl->PopClipRect();

    // actions
    float ay = bottom - actions_h;
    g_focus = std::clamp(g_focus, 0, std::max(0, static_cast<int>(p.actions.size()) - 1));
    for (size_t i = 0; i < p.actions.size(); ++i) {
        const PaneAction &act = p.actions[i];
        const ImVec2 ba(x + pad, ay), bb(x + w - pad, ay + bh);
        const bool focus = modal && g_focus == static_cast<int>(i);
        dl->AddRectFilled(ba, bb, Fade(act.enabled ? (i == 0 ? kPinkU : kTile) : kTile, alpha), 6 * s);
        if (focus) FocusRing(dl, ba, bb, 6 * s);
        const ImVec2 ts = Measure(f.body, act.label.c_str());
        TextAt(dl, f.body, ImVec2((ba.x + bb.x - ts.x) / 2, (ba.y + bb.y - ts.y) / 2), act.enabled ? kText : kTextDim, act.label.c_str(), alpha);
        if (!act.enabled) dl->AddRectFilled(ba, bb, Rgba(0, 0, 0, 0.35f * alpha), 6 * s);
        if (modal && ImGui::IsMouseHoveringRect(ba, bb) && ImGui::IsMouseClicked(0)) {
            g_focus = static_cast<int>(i);
            g_accept = true;
        }
        ay += bh + bgap;
    }
    if (modal) {
        const float hy = height - 46 * s;
        dl->AddLine(ImVec2(x + pad, hy - 16 * s), ImVec2(x + w - pad, hy - 16 * s), Rgba(255, 255, 255, 0.08f * alpha));
        FaceButton(dl, ImVec2(x + pad + 11 * s, hy + 10 * s), 11 * s, kRed, "B");
        TextAt(dl, f.small, ImVec2(x + pad + 28 * s, hy + 1 * s), kTextDim, T("Close", "Fechar"), alpha);
    }
    if (g_accept) {
        g_accept = false;
        if (modal && g_focus < static_cast<int>(p.actions.size())) Activate(p, p.actions[g_focus]);
    }
}

} // namespace fi
