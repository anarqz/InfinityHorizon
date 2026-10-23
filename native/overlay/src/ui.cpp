// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: what the player sees (OD-38, OD-50). 100% inspired by the Xbox UI:
//
// - Toasts: the Xbox achievement pill in the Horizon pink (#E6007E). It rises from the bottom as
//   a circle with the icon, opens into the full pill while the title and text fade in, holds,
//   then slides down. One at a time; the rest wait in order. Generic: connection status, news,
//   updates, achievements, friends.
// - Sidebar: the Xbox guide. Slides in from the left over a dimmed game: the profile header
//   (avatar, gamertag, tier chip, connection), tabs Home, Lobbies, Friends and Settings,
//   rounded tiles with the white focus ring, and controller hints. Friends is coming soon;
//   Settings opens the connection diagnostic report. Transient toasts remain available.
//
// Every text is UTF-8 (built with /utf-8); Portuguese when Windows' UI language is Portuguese.
#include "overlay.h"

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <deque>
#include <mutex>
#include <vector>

#include "imgui.h"
#include "imgui_internal.h"
#include "fonts.h"
#include "style.h"
#include "toast_schedule.h"

namespace fi {

namespace ui {

Fonts f;
float s = 1.0f; // 1.0 at 1080p

bool Portuguese() { return PRIMARYLANGID(GetUserDefaultUILanguage()) == LANG_PORTUGUESE; }
const char *T(const char *en, const char *pt) { return Portuguese() ? pt : en; }

// ---------------------------------------------------------------- easing
float Clamp01(float x) { return x < 0 ? 0 : x > 1 ? 1 : x; }
float EaseOutCubic(float t) { t = Clamp01(t); return 1 - std::pow(1 - t, 3.0f); }
float EaseInCubic(float t) { t = Clamp01(t); return t * t * t; }
float EaseOutBack(float t) {
    t = Clamp01(t);
    const float c1 = 1.4f, c3 = c1 + 1;
    return 1 + c3 * std::pow(t - 1, 3.0f) + c1 * std::pow(t - 1, 2.0f);
}

// ---------------------------------------------------------------- model
struct Toast {
    ToastKind kind;
    std::string title, body;
    float age = 0;
    float hold = 4.6f;
};
struct Model {
    std::string gamertag = "Infinity";
    Tier tier = Tier::Unknown;
    Link link = Link::Connecting;
    std::string detail;
    std::string connection_details;
    uint8_t connection_state = 2;
    ULONGLONG connection_at = 0;
};
std::mutex g_lock;
Model g_model;

// render-thread state
ToastSchedule<Toast> g_toasts;
bool g_open = false;
float g_open_anim = 0; // 0 closed .. 1 open
int g_tab = 0; // 0 Home, 1 Lobbies, 2 Friends, 3 Settings
constexpr int kTabs = 4, kTabWorlds = 1, kTabFriends = 2;
int g_focus = 0;
bool g_confirm_quit = false; // "Quit the game" asks for a second A
bool g_connection_open = false;
float g_connection_scroll = 0;

const char *IconOf(ToastKind kind) {
    switch (kind) {
    case ToastKind::Success: return kIconCheck;
    case ToastKind::Warning: return kIconWarning;
    case ToastKind::Error: return kIconError;
    case ToastKind::Achievement: return kIconStar;
    case ToastKind::Update: return kIconDownload;
    case ToastKind::Friend: return kIconAddFriend;
    default: return kIconInfo;
    }
}

// ---------------------------------------------------------------- drawing helpers
void Shadow(ImDrawList *dl, ImVec2 a, ImVec2 b, float rounding, float alpha) {
    for (int i = 1; i <= 6; ++i) {
        const float grow = i * 2.5f * s;
        dl->AddRectFilled(ImVec2(a.x - grow, a.y - grow + 3 * s), ImVec2(b.x + grow, b.y + grow + 3 * s),
                          Rgba(0, 0, 0, alpha * 0.07f * (7 - i) / 6.0f), rounding + grow);
    }
}

void TextAt(ImDrawList *dl, ImFont *font, ImVec2 pos, ImU32 color, const char *text, float alpha) {
    ImU32 c = (color & 0x00FFFFFF) | (static_cast<ImU32>(((color >> 24) & 0xFF) * alpha) << 24);
    dl->AddText(font, font->FontSize, pos, c, text);
}

ImVec2 Measure(ImFont *font, const char *text) { return font->CalcTextSizeA(font->FontSize, FLT_MAX, 0, text); }

// Xbox controller face button: a coloured disc with its letter.
void FaceButton(ImDrawList *dl, ImVec2 centre, float radius, ImU32 color, const char *letter) {
    dl->AddCircleFilled(centre, radius, color, 24);
    const ImVec2 size = Measure(f.tag, letter);
    dl->AddText(f.tag, f.tag->FontSize, ImVec2(centre.x - size.x / 2, centre.y - size.y / 2), kText, letter);
}

void FocusRing(ImDrawList *dl, ImVec2 a, ImVec2 b, float rounding) {
    dl->AddRect(ImVec2(a.x - 3 * s, a.y - 3 * s), ImVec2(b.x + 3 * s, b.y + 3 * s), Rgba(255, 255, 255, 0.25f), rounding + 3 * s, 0, 4 * s);
    dl->AddRect(a, b, kText, rounding, 0, 2.5f * s);
}

} // namespace ui

using namespace ui;

namespace {

// ---------------------------------------------------------------- toasts
constexpr float kRise = ToastSchedule<Toast>::Rise, kOpen = ToastSchedule<Toast>::Open, kLeave = ToastSchedule<Toast>::Leave;

// The pill: MultiColor fills ignore rounding, so draw the rounded shape and paint the gradient
// through a clip. Simpler and exact: a rounded fill plus a darker rounded fill on the lower half.
void DrawToastPill(ImDrawList *dl, float width, float height, const Toast &t) {
    const float icon_area = 88 * s;
    const float text_w = std::max(Measure(f.toast_title, t.title.c_str()).x, Measure(f.toast_body, t.body.c_str()).x);
    const float max_w = std::max(icon_area + 80 * s, std::min(720 * s, width - 32 * s));
    const float w = std::clamp(icon_area + 10 * s + text_w + icon_area * 0.55f, std::min(380 * s, max_w), max_w);
    const float wrap = w - icon_area - 12 * s - icon_area * 0.35f;
    const float body_h = f.toast_body->CalcTextSizeA(f.toast_body->FontSize, FLT_MAX, wrap, t.body.c_str()).y;
    const float h = std::max(icon_area, 46 * s + body_h + 16 * s);
    const float bottom = height - 64 * s;
    const float cx = width / 2;
    const float rise = EaseOutBack(t.age / kRise);
    const float open = EaseOutCubic((t.age - kRise) / kOpen);
    const float leave = EaseInCubic((t.age - (kRise + kOpen + t.hold)) / kLeave);
    const float y = bottom - h * rise + (h + 64 * s) * leave;
    const float alpha = 1.0f - leave;
    const float cur_w = icon_area + (w - icon_area) * open;
    const ImVec2 a(cx - cur_w / 2, y), b(cx + cur_w / 2, y + h);
    const bool error = t.kind == ToastKind::Error;
    Shadow(dl, a, b, icon_area / 2, alpha);
    // the pill with a smooth top-to-bottom depth (vertex shading keeps the rounded shape)
    const int first = dl->VtxBuffer.Size;
    dl->AddRectFilled(a, b, IM_COL32_WHITE, icon_area / 2);
    ImGui::ShadeVertsLinearColorGradientKeepAlpha(dl, first, dl->VtxBuffer.Size, ImVec2(a.x, a.y), ImVec2(a.x, b.y),
        error ? Rgba(214, 56, 40) : Rgba(240, 24, 140), error ? Rgba(160, 32, 22) : Rgba(184, 0, 100));
    for (int v = first; v < dl->VtxBuffer.Size; ++v) {
        ImU32 &c = dl->VtxBuffer[v].col;
        c = (c & 0x00FFFFFF) | (static_cast<ImU32>(255 * alpha) << 24);
    }
    dl->AddRect(a, b, Rgba(255, 255, 255, 0.16f * alpha), icon_area / 2, 0, 1.2f * s);
    const float disc = icon_area - 18 * s;
    const ImVec2 dc(a.x + icon_area / 2, y + h / 2);
    dl->AddCircleFilled(dc, disc / 2, Rgba(255, 255, 255, alpha), 48);
    const char *icon = IconOf(t.kind);
    const ImVec2 isz = Measure(f.icon_big, icon);
    dl->AddText(f.icon_big, f.icon_big->FontSize, ImVec2(dc.x - isz.x / 2, dc.y - isz.y / 2),
                error ? Rgba(196, 43, 28, alpha) : Rgba(230, 0, 126, alpha), icon);
    const float text_alpha = alpha * EaseOutCubic((t.age - kRise - kOpen * 0.45f) / (kOpen * 0.6f));
    if (text_alpha > 0.01f) {
        const float tx = a.x + icon_area + 4 * s;
        dl->PushClipRect(ImVec2(tx, a.y), ImVec2(b.x - icon_area * 0.35f, b.y), true);
        TextAt(dl, f.toast_title, ImVec2(tx + 4 * s, y + 13 * s), kText, t.title.c_str(), text_alpha);
        dl->AddText(f.toast_body, f.toast_body->FontSize, ImVec2(tx + 4 * s, y + 46 * s),
                    Rgba(255, 255, 255, text_alpha * 0.9f), t.body.c_str(), nullptr, wrap);
        dl->PopClipRect();
    }
}

void UpdateToasts(ImDrawList *dl, float width, float height, float dt) {
    if (g_toasts.Tick(dt)) PlayToastSound();
    if (const auto &current = g_toasts.Current()) DrawToastPill(dl, width, height, *current);
}

// ---------------------------------------------------------------- sidebar
struct Tile {
    const char *icon;
    std::string title, subtitle;
    bool locked;
    int action; // 0 none, 1 close, 2 coming soon, 3 supporter info
};

std::vector<Tile> TilesFor(int tab, const Model &m) {
    std::vector<Tile> tiles;
    switch (tab) {
    case 0: {
        const char *state = m.link == Link::Online ? T("Online", "Online")
                            : m.link == Link::Offline ? T("Offline", "Offline")
                            : m.link == Link::Reconnecting ? T("Reconnecting...", "Reconectando...")
                                                           : T("Connecting...", "Conectando...");
        tiles.push_back({kIconGlobe, T("Connection", "Conexão"), m.detail.empty() ? state : std::string(state) + " · " + m.detail, false, 0});
        tiles.push_back({kIconGame, T("Back to the game", "Voltar ao jogo"), T("Close the guide", "Fechar o guia"), false, 1});
        tiles.push_back({kIconPower, g_confirm_quit ? T("Press A again to quit", "Aperte A de novo para sair") : T("Quit the game", "Sair do jogo"),
                         g_confirm_quit ? T("Your progress is saved", "Seu progresso fica salvo") : T("Closes Forza Horizon 2", "Fecha o Forza Horizon 2"), false, 5});
        break;
    }
    default:
        tiles.push_back({kIconGlobe, T("CONNECTION DETAILS", "DETALHES DA CONEXÃO"), "", false, 4});
        break;
    }
    return tiles;
}

struct Pending {
    int action = -1;
} g_pending;

void Activate(const Tile &tile) {
    switch (tile.action) {
    case 1: CloseSidebar(); break;
    case 2: PushToast(ToastKind::Info, T("Coming soon", "Em breve"), tile.title); break;
    case 3: PushToast(ToastKind::Achievement, T("Infinity Supporter", "Infinity Supporter"),
                      T("Unlock friends and player profiles in the launcher.", "Libere amigos e perfis de jogadores pelo launcher.")); break;
    case 4: g_connection_open = true; SendEvent(8, ""); break;
    case 5:
        if (!g_confirm_quit) {
            g_confirm_quit = true;
        } else {
            g_confirm_quit = false;
            SendEvent(7, "player"); // UiEventKind.Quit: the launcher closes the game cleanly
            CloseSidebar();
        }
        break;
    default: break;
    }
}

void DrawTiles(ImDrawList *dl, float x0, float y, float pw, float pad, const Model &m) {
    const std::vector<Tile> tiles = TilesFor(g_tab, m);
    g_focus = std::clamp(g_focus, 0, std::max(0, static_cast<int>(tiles.size()) - 1));
    const float th = 78 * s;
    for (int i = 0; i < static_cast<int>(tiles.size()); ++i) {
        const Tile &tile = tiles[i];
        const ImVec2 ta(x0 + pad, y), tb(x0 + pw - pad, y + th);
        if (ImGui::IsMouseHoveringRect(ta, tb)) {
            g_focus = i;
            if (ImGui::IsMouseClicked(0)) {
                Activate(tile);
            }
        }
        const bool focus = i == g_focus;
        dl->AddRectFilled(ta, tb, focus ? kTileFocus : kTile, 6 * s);
        if (focus) {
            // the Xbox focus: a white ring with a soft outer glow
            dl->AddRect(ImVec2(ta.x - 3 * s, ta.y - 3 * s), ImVec2(tb.x + 3 * s, tb.y + 3 * s), Rgba(255, 255, 255, 0.25f), 9 * s, 0, 4 * s);
            dl->AddRect(ta, tb, kText, 6 * s, 0, 2.5f * s);
        }
        const ImU32 icon_color = tile.locked ? kTextDim : focus ? kPinkU : kText;
        dl->AddText(f.icon, f.icon->FontSize, ImVec2(ta.x + 20 * s, ta.y + th / 2 - f.icon->FontSize / 2), icon_color, tile.icon);
        const float lx = ta.x + 62 * s;
        dl->PushClipRect(ImVec2(lx, ta.y), ImVec2(tb.x - 44 * s, tb.y), true);
        TextAt(dl, f.body, ImVec2(lx, ta.y + (tile.subtitle.empty() ? th / 2 - 12 * s : 13 * s)), tile.locked ? kTextDim : kText, tile.title.c_str());
        if (!tile.subtitle.empty()) {
            TextAt(dl, f.small, ImVec2(lx, ta.y + 43 * s), kTextDim, tile.subtitle.c_str());
        }
        dl->PopClipRect();
        const char *right = tile.locked ? kIconLock : tile.action ? kIconChevron : "";
        if (*right) {
            dl->AddText(f.icon, f.icon->FontSize * 0.8f, ImVec2(tb.x - 36 * s, ta.y + th / 2 - f.icon->FontSize * 0.4f), kTextDim, right);
        }
        y += th + 10 * s;
    }
    if (g_pending.action >= 0) {
        if (g_focus < static_cast<int>(tiles.size())) {
            Activate(tiles[g_focus]);
        }
        g_pending.action = -1;
    }
}

void DrawSidebar(ImDrawList *dl, float width, float height, float dt, const Model &m) {
    const float target = g_open ? 1.0f : 0.0f;
    const float speed = dt / 0.22f;
    g_open_anim = g_open_anim < target ? std::min(target, g_open_anim + speed) : std::max(target, g_open_anim - speed);
    if (g_open_anim <= 0.001f) {
        return;
    }
    const float ease = g_open ? EaseOutCubic(g_open_anim) : g_open_anim * g_open_anim;
    dl->AddRectFilled(ImVec2(0, 0), ImVec2(width, height), Rgba(0, 0, 0, 0.5f * ease));

    const float pw = 460 * s;
    const float x0 = -pw + pw * ease;
    const ImVec2 a(x0, 0), b(x0 + pw, height);
    Shadow(dl, a, b, 0, ease);
    dl->AddRectFilled(a, b, kPanel);
    dl->AddLine(ImVec2(b.x, 0), ImVec2(b.x, height), Rgba(255, 255, 255, 0.06f));
    // a pink glow along the top, the Infinity accent
    dl->AddRectFilledMultiColor(a, ImVec2(b.x, 190 * s), Rgba(230, 0, 126, 0.30f), Rgba(230, 0, 126, 0.10f), Rgba(20, 20, 22, 0), Rgba(20, 20, 22, 0));

    const float pad = 28 * s;
    float y = 34 * s;
    // header: avatar, gamertag, tier, connection
    const float av = 72 * s;
    const ImVec2 ac(x0 + pad + av / 2, y + av / 2);
    dl->AddCircleFilled(ac, av / 2 + 3 * s, kText, 48);
    dl->AddCircleFilled(ac, av / 2, kPinkU, 48);
    std::string initials = m.gamertag.substr(0, std::min<size_t>(2, m.gamertag.size()));
    std::transform(initials.begin(), initials.end(), initials.begin(), [](char c) { return static_cast<char>(c >= 'a' && c <= 'z' ? c - 32 : c); });
    const ImVec2 isz = Measure(f.heading, initials.c_str());
    dl->AddText(f.heading, f.heading->FontSize, ImVec2(ac.x - isz.x / 2, ac.y - isz.y / 2), kText, initials.c_str());
    const float hx = x0 + pad + av + 18 * s;
    TextAt(dl, f.heading, ImVec2(hx, y + 4 * s), kText, m.gamertag.c_str());
    // tier chip
    const bool supporter = m.tier == Tier::Supporter;
    const char *tier = supporter ? "INFINITY SUPPORTER" : m.tier == Tier::Free ? "FREE" : "INFINITY";
    const ImVec2 tsz = Measure(f.tag, tier);
    const ImVec2 ca(hx, y + 40 * s), cb(hx + tsz.x + 16 * s, y + 40 * s + tsz.y + 8 * s);
    dl->AddRectFilled(ca, cb, supporter ? kPinkU : Rgba(70, 70, 78), (cb.y - ca.y) / 2);
    dl->AddText(f.tag, f.tag->FontSize, ImVec2(ca.x + 8 * s, ca.y + 4 * s), kText, tier);
    // connection
    const ImU32 dot = m.link == Link::Online ? IM_COL32(92, 214, 92, 255) : m.link == Link::Offline ? kRed : kAmber;
    const char *state = m.link == Link::Online ? "Online" : m.link == Link::Offline ? "Offline"
                        : m.link == Link::Reconnecting ? T("Reconnecting", "Reconectando") : T("Connecting", "Conectando");
    dl->AddCircleFilled(ImVec2(hx + 5 * s, cb.y + 17 * s), 5 * s, dot, 16);
    TextAt(dl, f.small, ImVec2(hx + 16 * s, cb.y + 9 * s), kTextDim, state);
    y += av + 34 * s;

    // tabs
    const char *icons[] = {kIconHome, kIconWorld, kIconPeople, kIconSettings};
    const char *names[] = {T("Home", "Início"), T("Lobbies", "Lobbies"), T("Friends", "Amigos"), T("Settings", "Ajustes")};
    const float tw = (pw - 2 * pad) / kTabs;
    for (int i = 0; i < kTabs; ++i) {
        const float tx = x0 + pad + i * tw;
        const bool active = i == g_tab;
        const ImVec2 isz2 = Measure(f.icon, icons[i]);
        dl->AddText(f.icon, f.icon->FontSize, ImVec2(tx + tw / 2 - isz2.x / 2, y), active ? kText : kTextDim, icons[i]);
        if (active) {
            dl->AddRectFilled(ImVec2(tx + tw * 0.25f, y + 34 * s), ImVec2(tx + tw * 0.75f, y + 37 * s), kPinkU, 2 * s);
        }
        if (ImGui::IsMouseHoveringRect(ImVec2(tx, y - 6 * s), ImVec2(tx + tw, y + 40 * s)) && ImGui::IsMouseClicked(0)) {
            g_tab = i;
            g_focus = 0;
        }
    }
    y += 48 * s;
    TextAt(dl, f.title, ImVec2(x0 + pad, y), kText, names[g_tab]);
    y += 38 * s;

    SetHoverPane(nullptr); // the worlds tab sets it again for its focused row
    if (g_tab == kTabWorlds) {
        DrawWorldsTab(dl, x0 + pad, y, pw - 2 * pad, height - 80 * s, supporter, dt);
    } else if (g_tab == kTabFriends) {
        g_pending.action = -1;
        const float center_x = x0 + pw / 2;
        const float center_y = (y + height - 80 * s) / 2;
        const float icon_size = 84 * s;
        const ImVec2 icon = f.icon_big->CalcTextSizeA(icon_size, FLT_MAX, 0, kIconContact);
        dl->AddText(f.icon_big, icon_size, ImVec2(center_x - icon.x / 2, center_y - 66 * s), Rgba(255, 255, 255, 0.22f), kIconContact);
        const char *soon = T("Coming soon", "Em breve");
        const ImVec2 text = Measure(f.title, soon);
        TextAt(dl, f.title, ImVec2(center_x - text.x / 2, center_y + 34 * s), kTextDim, soon);
    } else {
        DrawTiles(dl, x0, y, pw, pad, m);
    }

    // hints
    const float hy = height - 46 * s;
    dl->AddLine(ImVec2(x0 + pad, hy - 16 * s), ImVec2(x0 + pw - pad, hy - 16 * s), Rgba(255, 255, 255, 0.08f));
    float hx2 = x0 + pad;
    FaceButton(dl, ImVec2(hx2 + 11 * s, hy + 10 * s), 11 * s, kGreen, "A");
    TextAt(dl, f.small, ImVec2(hx2 + 28 * s, hy + 1 * s), kTextDim, T("Select", "Selecionar"));
    hx2 += 128 * s;
    FaceButton(dl, ImVec2(hx2 + 11 * s, hy + 10 * s), 11 * s, kRed, "B");
    TextAt(dl, f.small, ImVec2(hx2 + 28 * s, hy + 1 * s), kTextDim, T("Back", "Voltar"));
    if (g_tab == kTabWorlds && WorldsCanRefresh()) {
        hx2 += 112 * s;
        FaceButton(dl, ImVec2(hx2 + 11 * s, hy + 10 * s), 11 * s, kYellow, "Y");
        TextAt(dl, f.small, ImVec2(hx2 + 28 * s, hy + 1 * s), kTextDim, T("Refresh", "Atualizar"));
    }
    const char *brand = "INFINITY";
    const ImVec2 bsz = Measure(f.tag, brand);
    TextAt(dl, f.tag, ImVec2(x0 + pw - pad - bsz.x, hy + 4 * s), kPinkU, brand);

    // the right pane (OD-112), beside the guide and sliding with it
    DrawPane(dl, b.x, 0, std::min(420 * s, width - b.x), height, ease);
}

} // namespace

// ---------------------------------------------------------------- public
void SetProfile(const std::string &gamertag, Tier tier) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_model.gamertag = gamertag.empty() ? "Infinity" : gamertag;
    g_model.tier = tier;
}

void SetLink(Link link, const std::string &detail) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_model.link = link;
    g_model.detail = detail;
    if (link != Link::Online) { g_model.connection_details.clear(); g_model.connection_state = 2; }
}

void SetConnectionDetails(uint8_t state, const std::string &text) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_model.connection_state = state;
    g_model.connection_details = text;
    g_model.connection_at = GetTickCount64();
}

void PushToast(ToastKind kind, const std::string &title, const std::string &body, uint16_t ttl) {
    std::lock_guard<std::mutex> lock(g_lock);
    const float hold = ttl ? std::clamp(ttl / 10.0f, 1.0f, 30.0f) : 4.6f;
    g_toasts.Push({kind, title, body, 0, hold});
}

bool SidebarOpen() { return g_open; }
void ToggleSidebar() {
    g_open = !g_open;
    if (g_open) {
        g_tab = 0; // the guide always opens at Home, as the Xbox guide does
        g_focus = 0;
        WorldsReset();
    }
    g_confirm_quit = false;
    g_connection_open = false;
    if (!g_open) ClosePane();
    SendPanel(g_open);
}
void CloseSidebar() {
    if (g_open) {
        g_open = false;
        g_confirm_quit = false;
        g_connection_open = false;
        ClosePane();
        SendPanel(false);
    }
}

void Navigate(Nav nav) {
    if (g_connection_open) {
        if (nav == Nav::Back || nav == Nav::Accept) g_connection_open = false;
        if (nav == Nav::Up) g_connection_scroll -= 80 * s;
        if (nav == Nav::Down) g_connection_scroll += 80 * s;
        return;
    }
    if (CodeEntryOpen()) {
        CodeEntryNavigate(nav);
        return;
    }
    if (KeyboardOpen()) {
        KeyboardNavigate(nav);
        return;
    }
    if (!g_open) {
        return;
    }
    if (PaneNavigate(nav)) {
        return; // an open player or launcher pane holds the controller
    }
    if (g_tab == kTabWorlds && nav != Nav::TabLeft && nav != Nav::TabRight && WorldsNavigate(nav)) {
        return;
    }
    switch (nav) {
    case Nav::Up: --g_focus; break;
    case Nav::Down: ++g_focus; break; // clamped when drawn
    case Nav::Left:
    case Nav::TabLeft: g_tab = (g_tab + kTabs - 1) % kTabs; g_focus = 0; break;
    case Nav::Right:
    case Nav::TabRight: g_tab = (g_tab + 1) % kTabs; g_focus = 0; break;
    case Nav::Accept: g_pending.action = 1; break;
    case Nav::Back: CloseSidebar(); break;
    default: break;
    }
    if (g_focus < 0) {
        g_focus = 0;
    }
}

// The font data inside this DLL (fonts.rc), or null.
static ImFont *EmbeddedFont(int id, float size, const ImWchar *ranges, const char *name) {
    HMODULE self = nullptr;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                       reinterpret_cast<LPCWSTR>(&EmbeddedFont), &self);
    HRSRC info = FindResourceW(self, MAKEINTRESOURCEW(id), MAKEINTRESOURCEW(10)); // RT_RCDATA
    HGLOBAL res = info ? LoadResource(self, info) : nullptr;
    void *data = res ? LockResource(res) : nullptr;
    const DWORD bytes = info ? SizeofResource(self, info) : 0;
    ImGuiIO &io = ImGui::GetIO();
    if (!data || !bytes) {
        Log("fonts: %s missing from the DLL; ImGui's default font instead", name);
        ImFontConfig fallback;
        fallback.SizePixels = size;
        return io.Fonts->AddFontDefault(&fallback);
    }
    ImFontConfig cfg;
    cfg.OversampleH = 2;
    cfg.OversampleV = 2;
    cfg.FontDataOwnedByAtlas = false; // the DLL's own memory, for the life of the module
    return io.Fonts->AddFontFromMemoryTTF(data, static_cast<int>(bytes), size, &cfg, ranges);
}

void LoadFonts(float scale) {
    // Always the fonts embedded in the DLL (Selawik, Fluent UI System Icons): Proton prefixes and
    // the Steam Deck have no Segoe fonts, and Microsoft's are not ours to ship. The same look on
    // every platform.
    s = scale < 0.6f ? 0.6f : scale;
    // Latin-1 plus the general punctuation (… – — “ ” •)
    static const ImWchar text_ranges[] = {0x0020, 0x00FF, 0x2010, 0x2027, 0};
    static const ImWchar icon_ranges[] = {0xE000, 0xF8FF, 0};
    f.body = EmbeddedFont(FI_FONT_REGULAR, 19 * s, text_ranges, "Selawik");
    f.small = EmbeddedFont(FI_FONT_REGULAR, 15 * s, text_ranges, "Selawik");
    f.title = EmbeddedFont(FI_FONT_SEMIBOLD, 22 * s, text_ranges, "Selawik Semibold");
    f.heading = EmbeddedFont(FI_FONT_SEMIBOLD, 28 * s, text_ranges, "Selawik Semibold");
    f.tag = EmbeddedFont(FI_FONT_BOLD, 12 * s, text_ranges, "Selawik Bold");
    f.icon = EmbeddedFont(FI_FONT_ICONS, 24 * s, icon_ranges, "Fluent UI System Icons");
    f.icon_big = EmbeddedFont(FI_FONT_ICONS, 34 * s, icon_ranges, "Fluent UI System Icons");
    f.toast_title = EmbeddedFont(FI_FONT_SEMIBOLD, 23 * s, text_ranges, "Selawik Semibold");
    f.toast_body = EmbeddedFont(FI_FONT_REGULAR, 18 * s, text_ranges, "Selawik");
    ImGui::GetIO().FontDefault = f.body;
    Log("fonts: Selawik and Fluent UI System Icons, embedded, at scale %.2f", s);
}
std::atomic<int> g_dev_request{0};
void DevRequest(int request) { g_dev_request = request; }

void DrawUi(float width, float height, float dt) {
    const int request = g_dev_request.exchange(0);
    if (request == 1 || request == 2) {
        KeyboardRequest r;
        r.title = request == 1 ? T("Lobby password", "Senha do lobby") : T("Lobby name", "Nome do lobby");
        r.hint = request == 1 ? T("The password the host gave you", "A senha que o anfitrião passou") : T("Sunday drive", "Passeio de domingo");
        r.secret = request == 1;
        r.max = request == 1 ? 64 : 48;
        OpenKeyboard(r, [request](bool ok, const std::string &text) {
            Log("keyboard: %s, %zu bytes", ok ? "done" : "cancelled", text.size());
            if (ok && request == 2) {
                PushToast(ToastKind::Success, T("Lobby name", "Nome do lobby"), text);
            }
        });
    }
    if (request == 3) {
        OpenCodeEntry(T("Enter the lobby code", "Digite o código do lobby"), "0123456789", [](bool join, const std::string &code) {
            Log("code: %s, %zu characters", join ? "join" : "back", code.size());
        });
    }
    PaneTick(dt);
    Model snapshot;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        snapshot.gamertag = g_model.gamertag;
        snapshot.tier = g_model.tier;
        snapshot.link = g_model.link;
        snapshot.detail = g_model.detail;
        snapshot.connection_details = g_model.connection_details;
        snapshot.connection_state = g_model.connection_state;
        snapshot.connection_at = g_model.connection_at;
    }
    ImGui::GetIO().MouseDrawCursor = g_open;
    ImDrawList *dl = ImGui::GetForegroundDrawList();
    if (!g_connection_open) DrawSidebar(dl, width, height, dt, snapshot);
    if (g_connection_open) {
        ImGui::SetNextWindowPos(ImVec2(width / 2, height / 2), ImGuiCond_Always, ImVec2(0.5f, 0.5f));
        ImGui::SetNextWindowSize(ImVec2(std::min(width - 48 * s, 660 * s), std::min(height - 64 * s, 600 * s)), ImGuiCond_Always);
        if (ImGui::Begin(T("Connection details", "Detalhes da conexão"), &g_connection_open, ImGuiWindowFlags_NoCollapse)) {
            ImGui::BeginChild("connection-report", ImVec2(0, -48 * s));
            if (g_connection_scroll != 0) { ImGui::SetScrollY(ImGui::GetScrollY() + g_connection_scroll); g_connection_scroll = 0; }
            const bool expired = snapshot.connection_state == 0 && GetTickCount64() - snapshot.connection_at >= 10000;
            const char *body = expired || snapshot.connection_details.empty() ? T("Connection details unavailable.", "Detalhes da conexão indisponíveis.") : snapshot.connection_details.c_str();
            ImGui::TextWrapped("%s", body);
            ImGui::EndChild();
            if (ImGui::Button(T("Close", "Fechar"))) g_connection_open = false;
        }
        ImGui::End();
    }
    DrawKeyboard(width, height, dt);
    DrawCodeEntry(width, height, dt);
    UpdateToasts(dl, width, height, dt);
}

} // namespace fi
