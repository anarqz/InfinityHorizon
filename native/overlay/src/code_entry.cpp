// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the world code (OD-57). A private or friends world is joined with the
// 8-character code its host shares, typed on a screen of its own: large, in the centre, as
// XXXX-XXXX, each character popping into its box as it is typed. Below, a pad with only the
// code's alphabet (a phone pad for digits). A types the focused key, X deletes, B goes back;
// once all 8 are in, the focus moves to JOIN and A joins. A real keyboard types too (Enter
// joins, Esc goes back, Backspace deletes, Ctrl+V pastes a shared code).
//
// Threads: as the keyboard (keyboard.cpp): input is queued, applied on the render thread.
#include "overlay.h"

#include <algorithm>
#include <cctype>
#include <cmath>
#include <mutex>
#include <vector>

#include "style.h"

namespace fi {

using namespace ui;

namespace {

constexpr size_t kLength = 8;

struct Event {
    enum Kind : uint8_t { Nav, Text, Back, Join, Cancel } kind;
    fi::Nav nav = fi::Nav::None;
    std::string text;
};

std::mutex g_events_lock;
std::vector<Event> g_events;

struct Cell {
    int row, col;
    char ch; // 0: the delete key
};

// render-thread state
bool g_active = false;
float g_anim = 0;
std::string g_title, g_alphabet, g_code;
CodeDone g_done;
std::vector<Cell> g_cells;
int g_rows = 0, g_cols = 0;
int g_row = 0, g_col = 0;           // g_row == g_rows: the JOIN button
float g_pop[kLength] = {};          // per box: time since its character arrived
std::string g_error;                 // the last refusal, shown under the boxes

void Layout() {
    g_cells.clear();
    if (g_alphabet == "0123456789") { // a phone pad
        g_cols = 3;
        g_rows = 4;
        for (int i = 0; i < 9; ++i) {
            g_cells.push_back({i / 3, i % 3, static_cast<char>('1' + i)});
        }
        g_cells.push_back({3, 1, '0'});
        g_cells.push_back({3, 2, 0});
    } else {
        g_cols = 8;
        int i = 0;
        for (char c : g_alphabet) {
            g_cells.push_back({i / g_cols, i % g_cols, c});
            ++i;
        }
        g_cells.push_back({i / g_cols, i % g_cols, 0});
        g_rows = i / g_cols + 1;
    }
}

const Cell *CellAt(int row, int col) {
    for (const auto &c : g_cells) {
        if (c.row == row && c.col == col) return &c;
    }
    return nullptr;
}

void Add(char c) {
    c = static_cast<char>(std::toupper(static_cast<unsigned char>(c)));
    if (g_code.size() >= kLength || g_alphabet.find(c) == std::string::npos) {
        return;
    }
    g_pop[g_code.size()] = 0;
    g_code.push_back(c);
    g_error.clear();
    if (g_code.size() == kLength) {
        g_row = g_rows; // the focus goes to JOIN
    }
}

void Delete() {
    if (!g_code.empty()) {
        g_code.pop_back();
        g_error.clear();
        if (g_row == g_rows) {
            g_row = 0;
        }
    }
}

void Finish(bool join) {
    if (join && g_code.size() != kLength) {
        return;
    }
    g_active = false;
    if (g_done) {
        auto done = std::move(g_done);
        done(join, join ? g_code : std::string());
    }
}

void Move(int dr, int dc) {
    if (g_row == g_rows) {
        if (dr < 0) g_row = g_rows - 1;
        return;
    }
    if (dr != 0) {
        const int row = g_row + dr;
        if (row >= g_rows) {
            if (g_code.size() == kLength) g_row = g_rows;
            return;
        }
        if (row < 0) return;
        // the nearest key in that row
        int best = -1;
        for (const auto &c : g_cells) {
            if (c.row == row && (best < 0 || std::abs(c.col - g_col) < std::abs(best - g_col))) best = c.col;
        }
        if (best >= 0) {
            g_row = row;
            g_col = best;
        }
        return;
    }
    for (int step = 1; step <= g_cols; ++step) {
        const int col = ((g_col + dc * step) % g_cols + g_cols) % g_cols;
        if (CellAt(g_row, col)) {
            g_col = col;
            return;
        }
    }
}

void Apply(const Event &e) {
    switch (e.kind) {
    case Event::Text:
        for (char c : e.text) {
            if (c != '-' && c != ' ') Add(c);
        }
        break;
    case Event::Back: Delete(); break;
    case Event::Join: Finish(true); break;
    case Event::Cancel: Finish(false); break;
    case Event::Nav:
        switch (e.nav) {
        case fi::Nav::Up: Move(-1, 0); break;
        case fi::Nav::Down: Move(1, 0); break;
        case fi::Nav::Left: Move(0, -1); break;
        case fi::Nav::Right: Move(0, 1); break;
        case fi::Nav::Accept:
            if (g_row == g_rows) {
                Finish(true);
            } else if (const Cell *c = CellAt(g_row, g_col)) {
                c->ch ? Add(c->ch) : Delete();
            }
            break;
        case fi::Nav::X: Delete(); break;
        case fi::Nav::Back: Finish(false); break;
        case fi::Nav::Menu: Finish(true); break;
        default: break;
        }
        break;
    }
}

void Queue(Event e) {
    std::lock_guard<std::mutex> lock(g_events_lock);
    if (g_events.size() < 64) {
        g_events.push_back(std::move(e));
    }
}

void Hint(ImDrawList *dl, float &x, float y, ImU32 color, const char *letter, const char *text, float alpha) {
    FaceButton(dl, ImVec2(x + 13 * s, y + 12 * s), 13 * s, color, letter);
    TextAt(dl, f.body, ImVec2(x + 34 * s, y), kTextDim, text, alpha);
    x += 34 * s + Measure(f.body, text).x + 34 * s;
}

} // namespace

void OpenCodeEntry(const std::string &title, const std::string &alphabet, CodeDone done) {
    g_title = title;
    g_alphabet = alphabet.empty() ? std::string("0123456789") : alphabet;
    std::transform(g_alphabet.begin(), g_alphabet.end(), g_alphabet.begin(), [](char c) { return static_cast<char>(std::toupper(static_cast<unsigned char>(c))); });
    g_done = std::move(done);
    g_code.clear();
    g_error.clear();
    Layout();
    g_row = 0;
    g_col = 0;
    g_active = true;
    std::lock_guard<std::mutex> lock(g_events_lock);
    g_events.clear();
}

bool CodeEntryOpen() { return g_active; }

void CodeEntryNavigate(Nav nav) { Queue({Event::Nav, nav, {}}); }

void CodeEntryRefused(const std::string &why) {
    // render thread: the launcher said no (wrong code, full...): the boxes stay, with the reason
    g_error = why;
}

bool CodeEntryMessage(UINT msg, WPARAM wp) {
    if (!g_active) {
        return false;
    }
    if (msg == WM_CHAR) {
        if (wp == 0x16) { // Ctrl+V: a code shared in a chat
            if (OpenClipboard(nullptr)) {
                if (HANDLE data = GetClipboardData(CF_TEXT)) {
                    if (const char *text = static_cast<const char *>(GlobalLock(data))) {
                        Queue({Event::Text, Nav::None, std::string(text).substr(0, 32)});
                        GlobalUnlock(data);
                    }
                }
                CloseClipboard();
            }
        } else if (wp < 0x80 && std::isalnum(static_cast<int>(wp))) {
            Queue({Event::Text, Nav::None, std::string(1, static_cast<char>(wp))});
        }
        return true;
    }
    if (msg == WM_KEYDOWN) {
        switch (wp) {
        case VK_BACK: Queue({Event::Back, Nav::None, {}}); return true;
        case VK_RETURN: Queue({Event::Join, Nav::None, {}}); return true;
        case VK_ESCAPE: Queue({Event::Cancel, Nav::None, {}}); return true;
        default: return false;
        }
    }
    return false;
}

void DrawCodeEntry(float width, float height, float dt) {
    {
        std::vector<Event> events;
        {
            std::lock_guard<std::mutex> lock(g_events_lock);
            events.swap(g_events);
        }
        for (const auto &e : events) {
            if (!g_active) break;
            Apply(e);
        }
    }
    const float target = g_active ? 1.0f : 0.0f;
    g_anim = g_anim < target ? std::min(target, g_anim + dt / 0.2f) : std::max(target, g_anim - dt / 0.16f);
    if (g_anim <= 0.001f) {
        return;
    }
    for (float &p : g_pop) {
        p += dt;
    }
    const float ease = EaseOutCubic(g_anim);
    const float alpha = g_anim;
    ImDrawList *dl = ImGui::GetForegroundDrawList();
    dl->AddRectFilled(ImVec2(0, 0), ImVec2(width, height), Rgba(8, 8, 10, 0.86f * alpha));
    const float cx = width / 2;
    float y = height * 0.18f + (1 - ease) * 40 * s;

    // title
    const ImVec2 tsz = Measure(f.heading, g_title.c_str());
    TextAt(dl, f.heading, ImVec2(cx - tsz.x / 2, y), kText, g_title.c_str(), alpha);
    y += 62 * s;

    // the eight boxes, XXXX-XXXX
    const float bw = 92 * s, bh = 118 * s, gap = 14 * s, dash = 54 * s;
    const float total = 8 * bw + 6 * gap + dash;
    float x = cx - total / 2;
    for (size_t i = 0; i < kLength; ++i) {
        const bool filled = i < g_code.size();
        const bool current = i == g_code.size();
        const ImVec2 a(x, y), b(x + bw, y + bh);
        dl->AddRectFilled(a, b, Rgba(32, 32, 36, alpha), 8 * s);
        if (current && g_active) {
            dl->AddRect(a, b, Rgba(230, 0, 126, alpha), 8 * s, 0, 3 * s);
        } else {
            dl->AddRect(a, b, Rgba(255, 255, 255, 0.14f * alpha), 8 * s, 0, 1.5f * s);
        }
        if (filled) {
            // the pop: from 1.35x to 1x with a little overshoot
            const float t = Clamp01(g_pop[i] / 0.22f);
            const float scale = 1.0f + 0.35f * (1 - EaseOutBack(t));
            const char text[2] = {g_code[i], 0};
            const float size = 64 * s * scale;
            const ImVec2 gs = f.heading->CalcTextSizeA(size, FLT_MAX, 0, text);
            dl->AddText(f.heading, size, ImVec2((a.x + b.x - gs.x) / 2, (a.y + b.y - gs.y) / 2), Rgba(255, 255, 255, alpha * Clamp01(t * 3)), text);
        } else if (current && g_active && std::fmod(ImGui::GetTime(), 1.0) < 0.55) {
            dl->AddRectFilled(ImVec2(a.x + bw * 0.3f, b.y - 24 * s), ImVec2(b.x - bw * 0.3f, b.y - 20 * s), Rgba(230, 0, 126, alpha));
        }
        x += bw + gap;
        if (i == 3) {
            const float dy = y + bh / 2;
            dl->AddRectFilled(ImVec2(x + dash * 0.2f - gap / 2, dy - 3 * s), ImVec2(x + dash * 0.8f - gap / 2, dy + 3 * s), Rgba(178, 178, 186, alpha), 2 * s);
            x += dash;
        }
    }
    y += bh + 20 * s;
    if (!g_error.empty()) {
        const ImVec2 es = Measure(f.body, g_error.c_str());
        TextAt(dl, f.body, ImVec2(cx - es.x / 2, y), kRed, g_error.c_str(), alpha);
    }
    y += 44 * s;

    // the pad
    const float kw = g_cols == 3 ? 104 * s : 78 * s, kh = 64 * s, kg = 10 * s;
    const float pad_w = g_cols * kw + (g_cols - 1) * kg;
    const float px = cx - pad_w / 2;
    for (const auto &c : g_cells) {
        const ImVec2 a(px + c.col * (kw + kg), y + c.row * (kh + kg)), b(a.x + kw, a.y + kh);
        const bool focus = g_active && g_row == c.row && g_col == c.col;
        if (ImGui::IsMouseHoveringRect(a, b) && ImGui::IsMouseClicked(0)) {
            g_row = c.row;
            g_col = c.col;
            Apply({Event::Nav, Nav::Accept, {}});
        }
        dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
        if (focus) FocusRing(dl, a, b, 6 * s);
        const char label[2] = {c.ch, 0};
        ImFont *font = c.ch ? f.heading : f.icon;
        const char *text = c.ch ? label : kIconBackspace;
        const ImVec2 ls = Measure(font, text);
        dl->AddText(font, font->FontSize, ImVec2((a.x + b.x - ls.x) / 2, (a.y + b.y - ls.y) / 2), kText, text);
    }
    y += g_rows * (kh + kg) + 14 * s;

    // JOIN, live once the code is complete
    const bool ready = g_code.size() == kLength;
    const float jw = std::max(pad_w, 320 * s), jh = 64 * s;
    const ImVec2 ja(cx - jw / 2, y), jb(cx + jw / 2, y + jh);
    if (ready && ImGui::IsMouseHoveringRect(ja, jb) && ImGui::IsMouseClicked(0)) {
        Finish(true);
    }
    dl->AddRectFilled(ja, jb, ready ? Rgba(230, 0, 126, alpha) : Rgba(52, 52, 58, alpha), 8 * s);
    if (ready && g_row == g_rows && g_active) FocusRing(dl, ja, jb, 8 * s);
    const char *join = T("JOIN", "ENTRAR");
    const ImVec2 js = Measure(f.title, join);
    TextAt(dl, f.title, ImVec2(cx - js.x / 2, y + (jh - js.y) / 2), ready ? kText : kTextDim, join, alpha);
    y += jh + 34 * s;

    // hints: A Join once complete (else Type), X Delete, B Back
    const char *a_text = ready && g_row == g_rows ? T("Join", "Entrar") : T("Type", "Digitar");
    const float hw = 34 * s * 3 + Measure(f.body, a_text).x + Measure(f.body, T("Delete", "Apagar")).x + Measure(f.body, T("Back", "Voltar")).x + 34 * s * 2;
    float hx = cx - hw / 2;
    Hint(dl, hx, y, kGreen, "A", a_text, alpha);
    Hint(dl, hx, y, kBlue, "X", T("Delete", "Apagar"), alpha);
    Hint(dl, hx, y, kRed, "B", T("Back", "Voltar"), alpha);
}

} // namespace fi
