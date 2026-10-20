// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: text entry (OD-54: world names and passwords). The Xbox on-screen keyboard:
// a panel rising from the bottom with the field and a key grid navigated by the D-pad / stick.
// A types the focused key, X deletes, Y adds a space, Menu confirms, B cancels. A real keyboard
// types straight into the field (Enter confirms, Esc cancels, Backspace deletes, Ctrl+V pastes).
//
// Threads: the window procedure (typed characters) and the render thread (controller) only queue
// events; the render thread applies them in DrawKeyboard and calls the request's callback there.
#include "overlay.h"

#include <algorithm>
#include <cmath>
#include <mutex>
#include <vector>

#include "style.h"

namespace fi {

using namespace ui;

namespace {

struct Event {
    enum Kind : uint8_t { Nav, Char, Back, Done, Cancel } kind;
    fi::Nav nav = fi::Nav::None;
    std::string text; // Char: UTF-8
};

std::mutex g_events_lock;
std::vector<Event> g_events;

// render-thread state
struct Request {
    KeyboardRequest req;
    KeyboardDone done;
    std::string text;
} g_kb;
bool g_active = false;
float g_anim = 0;
int g_row = 1, g_col = 0;
bool g_shift = false, g_symbols = false;
float g_blink = 0;

// The grid: four rows of ten character keys, then the special row (in tenths of the width).
const char *const kLetters[4] = {"1234567890", "qwertyuiop", "asdfghjkl@", "zxcvbnm-_."};
const char *const kSymbols[4] = {"1234567890", "!#$%&*()+=", "[]{};:'\"/?", "<>\\|~`^,.-"};
enum Special { kShift, kSym, kSpace, kDelete, kDone, kSpecials };
const float kSpecialUnits[kSpecials] = {1.5f, 1.5f, 4.0f, 1.5f, 1.5f};

size_t Codepoints(const std::string &str) {
    return static_cast<size_t>(std::count_if(str.begin(), str.end(), [](char c) { return (static_cast<unsigned char>(c) & 0xC0) != 0x80; }));
}

void PopCodepoint(std::string &str) {
    while (!str.empty() && (static_cast<unsigned char>(str.back()) & 0xC0) == 0x80) {
        str.pop_back();
    }
    if (!str.empty()) {
        str.pop_back();
    }
}

void Insert(const std::string &utf8) {
    for (size_t i = 0; i < utf8.size();) {
        const unsigned char c = static_cast<unsigned char>(utf8[i]);
        const size_t n = c < 0x80 ? 1 : (c >> 5) == 6 ? 2 : (c >> 4) == 14 ? 3 : 4;
        if (Codepoints(g_kb.text) >= g_kb.req.max) {
            return;
        }
        if (c >= 0x20 && c != 0x7F) {
            g_kb.text.append(utf8, i, n);
        }
        i += n;
    }
}

void Finish(bool ok) {
    g_active = false;
    if (g_kb.done) {
        auto done = std::move(g_kb.done);
        done(ok, ok ? g_kb.text : std::string());
    }
    std::fill(g_kb.text.begin(), g_kb.text.end(), '\0'); // a password does not linger
    g_kb.text.clear();
}

// Key centres in units of the row (0..10), for moving between rows of different shapes.
float CentreOf(int row, int col) {
    if (row < 4) {
        return col + 0.5f;
    }
    float x = 0;
    for (int i = 0; i < col; ++i) {
        x += kSpecialUnits[i];
    }
    return x + kSpecialUnits[col] / 2;
}

int ColumnsOf(int row) { return row < 4 ? 10 : kSpecials; }

int Nearest(int row, float centre) {
    int best = 0;
    float distance = 1e9f;
    for (int c = 0; c < ColumnsOf(row); ++c) {
        const float d = std::fabs(CentreOf(row, c) - centre);
        if (d < distance) {
            distance = d;
            best = c;
        }
    }
    return best;
}

void Press() {
    if (g_row < 4) {
        char key = (g_symbols ? kSymbols : kLetters)[g_row][g_col];
        if (g_shift && key >= 'a' && key <= 'z') {
            key = static_cast<char>(key - 32);
            g_shift = false; // one capital, as the Xbox keyboard does
        }
        Insert(std::string(1, key));
        return;
    }
    switch (g_col) {
    case kShift: g_shift = !g_shift; break;
    case kSym: g_symbols = !g_symbols; break;
    case kSpace: Insert(" "); break;
    case kDelete: PopCodepoint(g_kb.text); break;
    case kDone: Finish(true); break;
    default: break;
    }
}

void Apply(const Event &e) {
    switch (e.kind) {
    case Event::Char: Insert(e.text); break;
    case Event::Back: PopCodepoint(g_kb.text); break;
    case Event::Done: Finish(true); break;
    case Event::Cancel: Finish(false); break;
    case Event::Nav:
        switch (e.nav) {
        case fi::Nav::Up:
            if (g_row > 0) {
                const float c = CentreOf(g_row, g_col);
                --g_row;
                g_col = Nearest(g_row, c);
            }
            break;
        case fi::Nav::Down:
            if (g_row < 4) {
                const float c = CentreOf(g_row, g_col);
                ++g_row;
                g_col = Nearest(g_row, c);
            }
            break;
        case fi::Nav::Left: g_col = (g_col + ColumnsOf(g_row) - 1) % ColumnsOf(g_row); break;
        case fi::Nav::Right: g_col = (g_col + 1) % ColumnsOf(g_row); break;
        case fi::Nav::Accept: Press(); break;
        case fi::Nav::Back: Finish(false); break;
        case fi::Nav::X: PopCodepoint(g_kb.text); break;
        case fi::Nav::Y: Insert(" "); break;
        case fi::Nav::Menu: Finish(true); break;
        default: break;
        }
        break;
    }
}

void Queue(Event e) {
    std::lock_guard<std::mutex> lock(g_events_lock);
    if (g_events.size() < 256) {
        g_events.push_back(std::move(e));
    }
}

std::string Utf8(const wchar_t *w, int n) {
    const int bytes = WideCharToMultiByte(CP_UTF8, 0, w, n, nullptr, 0, nullptr, nullptr);
    std::string out(bytes > 0 ? bytes : 0, '\0');
    if (bytes > 0) {
        WideCharToMultiByte(CP_UTF8, 0, w, n, out.data(), bytes, nullptr, nullptr);
    }
    return out;
}

void DrawKey(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, bool lit, const char *label, ImFont *font) {
    const float r = 6 * s;
    dl->AddRectFilled(a, b, lit ? kPinkU : focus ? kTileFocus : kTile, r);
    if (focus) {
        FocusRing(dl, a, b, r);
    }
    const ImVec2 size = Measure(font, label);
    dl->AddText(font, font->FontSize, ImVec2((a.x + b.x - size.x) / 2, (a.y + b.y - size.y) / 2), kText, label);
}

void Hint(ImDrawList *dl, float &x, float y, ImU32 color, const char *letter, const char *text) {
    FaceButton(dl, ImVec2(x + 11 * s, y + 10 * s), 11 * s, color, letter);
    TextAt(dl, f.small, ImVec2(x + 28 * s, y + 1 * s), kTextDim, text);
    x += 28 * s + Measure(f.small, text).x + 26 * s;
}

} // namespace

void OpenKeyboard(const KeyboardRequest &request, KeyboardDone done) {
    // render thread (called from a screen's activation)
    if (g_active) {
        Finish(false);
    }
    g_kb.req = request;
    g_kb.done = std::move(done);
    g_kb.text.clear();
    Insert(request.initial);
    g_active = true;
    g_row = 1;
    g_col = 0;
    g_shift = false;
    g_symbols = false;
    std::lock_guard<std::mutex> lock(g_events_lock);
    g_events.clear();
}

bool KeyboardOpen() { return g_active; }

void KeyboardNavigate(Nav nav) { Queue({Event::Nav, nav, {}}); }

bool KeyboardMessage(UINT msg, WPARAM wp) {
    if (!g_active) {
        return false;
    }
    if (msg == WM_CHAR) {
        if (wp == 0x16) { // Ctrl+V
            if (OpenClipboard(nullptr)) {
                if (HANDLE data = GetClipboardData(CF_UNICODETEXT)) {
                    if (const auto *w = static_cast<const wchar_t *>(GlobalLock(data))) {
                        std::wstring line(w);
                        line = line.substr(0, line.find_first_of(L"\r\n"));
                        Queue({Event::Char, Nav::None, Utf8(line.c_str(), static_cast<int>(line.size()))});
                        GlobalUnlock(data);
                    }
                }
                CloseClipboard();
            }
            return true;
        }
        if (wp >= 0x20 && wp != 0x7F) {
            const wchar_t w = static_cast<wchar_t>(wp);
            if (w >= 0xD800 && w <= 0xDFFF) {
                return true; // outside the overlay's fonts anyway
            }
            Queue({Event::Char, Nav::None, Utf8(&w, 1)});
        }
        return true;
    }
    if (msg == WM_KEYDOWN) {
        switch (wp) {
        case VK_BACK: Queue({Event::Back, Nav::None, {}}); return true;
        case VK_RETURN: Queue({Event::Done, Nav::None, {}}); return true;
        case VK_ESCAPE: Queue({Event::Cancel, Nav::None, {}}); return true;
        default: return false; // arrows and the rest: the usual navigation
        }
    }
    return false;
}

void DrawKeyboard(float width, float height, float dt) {
    {
        std::vector<Event> events;
        {
            std::lock_guard<std::mutex> lock(g_events_lock);
            events.swap(g_events);
        }
        for (const auto &e : events) {
            if (!g_active) {
                break;
            }
            Apply(e);
        }
    }
    const float target = g_active ? 1.0f : 0.0f;
    g_anim = g_anim < target ? std::min(target, g_anim + dt / 0.2f) : std::max(target, g_anim - dt / 0.16f);
    if (g_anim <= 0.001f) {
        return;
    }
    g_blink += dt;
    ImDrawList *dl = ImGui::GetForegroundDrawList();
    const float ease = EaseOutCubic(g_anim);
    dl->AddRectFilled(ImVec2(0, 0), ImVec2(width, height), Rgba(0, 0, 0, 0.45f * ease));

    const float pw = std::min(1080 * s, width - 80 * s), ph = 470 * s;
    const float x0 = (width - pw) / 2, y0 = height - (ph + 40 * s) * ease;
    const ImVec2 a(x0, y0), b(x0 + pw, y0 + ph);
    Shadow(dl, a, b, 10 * s, ease);
    dl->AddRectFilled(a, b, kPanel, 10 * s);
    dl->AddRectFilledMultiColor(a, ImVec2(b.x, a.y + 6 * s), kPinkU, kPinkU, kPinkDark, kPinkDark);
    const float pad = 28 * s;
    float y = y0 + 22 * s;
    TextAt(dl, f.title, ImVec2(x0 + pad, y), kText, g_kb.req.title.c_str());
    const std::string count = std::to_string(Codepoints(g_kb.text)) + "/" + std::to_string(g_kb.req.max);
    TextAt(dl, f.small, ImVec2(x0 + pw - pad - Measure(f.small, count.c_str()).x, y + 6 * s), kTextDim, count.c_str());
    y += 40 * s;

    // the field
    const ImVec2 fa(x0 + pad, y), fb(x0 + pw - pad, y + 52 * s);
    dl->AddRectFilled(fa, fb, IM_COL32(8, 8, 10, 255), 6 * s);
    dl->AddRect(fa, fb, kPinkU, 6 * s, 0, 2 * s);
    float cx = fa.x + 16 * s;
    const float cy = (fa.y + fb.y) / 2;
    dl->PushClipRect(fa, fb, true);
    if (g_kb.text.empty() && !g_kb.req.hint.empty()) {
        TextAt(dl, f.body, ImVec2(cx, cy - f.body->FontSize / 2), kTextDim, g_kb.req.hint.c_str());
    } else if (g_kb.req.secret) {
        const size_t n = Codepoints(g_kb.text);
        for (size_t i = 0; i < n; ++i) {
            dl->AddCircleFilled(ImVec2(cx + 6 * s, cy), 5 * s, kText, 16);
            cx += 16 * s;
        }
    } else {
        // the end of the text stays visible: draw it right-aligned once it overflows
        const float tw = Measure(f.body, g_kb.text.c_str()).x;
        const float room = fb.x - fa.x - 40 * s;
        const float tx = tw > room ? fa.x + 16 * s + room - tw : cx;
        TextAt(dl, f.body, ImVec2(tx, cy - f.body->FontSize / 2), kText, g_kb.text.c_str());
        cx = tx + tw;
    }
    if (std::fmod(g_blink, 1.0f) < 0.55f && !(g_kb.text.empty() && !g_kb.req.hint.empty())) {
        dl->AddRectFilled(ImVec2(cx + 2 * s, cy - 13 * s), ImVec2(cx + 4 * s, cy + 13 * s), kPinkU);
    }
    dl->PopClipRect();
    y += 52 * s + 22 * s;

    // the grid
    const float gap = 8 * s, kh = 50 * s;
    const float unit = (pw - 2 * pad + gap) / 10;
    for (int row = 0; row < 4; ++row) {
        const char *keys = (g_symbols ? kSymbols : kLetters)[row];
        for (int col = 0; col < 10; ++col) {
            char label[2] = {keys[col], 0};
            if (g_shift && label[0] >= 'a' && label[0] <= 'z') {
                label[0] = static_cast<char>(label[0] - 32);
            }
            const ImVec2 ka(x0 + pad + col * unit, y), kb(ka.x + unit - gap, y + kh);
            if (ImGui::IsMouseHoveringRect(ka, kb) && ImGui::IsMouseClicked(0)) {
                g_row = row;
                g_col = col;
                Press();
            }
            DrawKey(dl, ka, kb, g_active && row == g_row && col == g_col, false, label, f.title);
        }
        y += kh + gap;
    }
    const char *labels[kSpecials] = {kIconShift, g_symbols ? "abc" : "&123", T("space", "espaço"), kIconBackspace, T("Done", "Concluir")};
    float kx = x0 + pad;
    for (int col = 0; col < kSpecials; ++col) {
        const ImVec2 ka(kx, y), kb(kx + kSpecialUnits[col] * unit - gap, y + kh);
        if (ImGui::IsMouseHoveringRect(ka, kb) && ImGui::IsMouseClicked(0)) {
            g_row = 4;
            g_col = col;
            Press();
        }
        const bool icon = col == kShift || col == kDelete;
        DrawKey(dl, ka, kb, g_active && g_row == 4 && g_col == col, (col == kShift && g_shift) || col == kDone, labels[col], icon ? f.icon : f.body);
        kx += kSpecialUnits[col] * unit;
    }
    y += kh + 18 * s;

    // the controller hints
    float hx = x0 + pad;
    Hint(dl, hx, y, kGreen, "A", T("Type", "Digitar"));
    Hint(dl, hx, y, kBlue, "X", T("Delete", "Apagar"));
    Hint(dl, hx, y, kYellow, "Y", T("Space", "Espaço"));
    Hint(dl, hx, y, kRed, "B", T("Cancel", "Cancelar"));
    const char *menu = T("Menu  Done", "Menu  Concluir");
    TextAt(dl, f.small, ImVec2(x0 + pw - pad - Measure(f.small, menu).x, y + 1 * s), kTextDim, menu);
}

} // namespace fi
