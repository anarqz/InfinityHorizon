// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: input.
//
// - Keyboard and mouse: the game window is subclassed. Shift+Tab toggles the sidebar (Steam's
//   chord); while it is open, arrows / Enter / Esc navigate and every keyboard and mouse message
//   goes to ImGui instead of the game.
// - Controller: Azimuth reads pads through Windows.Gaming.Input; IGamepad::GetCurrentReading is
//   hooked once a pad exists, and the overlay sees the same reading the game does (whatever Steam
//   Input or another remapper made of the pad).
//   - LB + RB held together for 1 s toggle the sidebar (OD-125 follow-up, 2026-10-27: Azimuth 1.0
//     opens its own menu with View held about 3 s, so View is the game's and Azimuth's again; OD-50
//     had View held 1 s). Both are driving buttons, so the game keeps seeing them: only the chord
//     held that long toggles, once per hold. Steam and the Xbox Game Bar take the Xbox button, and
//     Steam's desktop layout takes View + Menu, before any game sees them.
//   - While the sidebar is open the game reads an idle pad ("connected, neutral", never
//     "disconnected"); after it closes, until every button is up, so the B that closed it never
//     reaches the game. The D-pad / left stick / A / B of that reading navigate, with repeat.
//   The hook only records; the render thread (PollController, once per frame) acts on it.
// - The Xbox (Guide) button also toggles, through XInputGetStateEx (xinput1_4 ordinal 100), for
//   players without Steam or the Game Bar on it; WGI never reports it. XInput navigates only
//   until the WGI hook is in.
// - Voice chat (OD-154, voice_gestures.h), with the guide closed: R3 held is push-to-talk, RB then R3 the next mic
//   mode with a notice on screen. Both still reach the game: they only read the pad.
#include "overlay.h"
#include "style.h"
#include "voice_gestures.h"

#include <MinHook.h>
#include <roapi.h>
#include <windows.gaming.input.h>
#include <wrl/client.h>
#include <wrl/wrappers/corewrappers.h>

#include <atomic>
#include <chrono>
#include <cstdlib>

#include "imgui.h"

#pragma comment(lib, "runtimeobject.lib")

extern IMGUI_IMPL_API LRESULT ImGui_ImplWin32_WndProcHandler(HWND, UINT, WPARAM, LPARAM);

namespace fi {

namespace {

WNDPROC g_game_proc = nullptr;
HWND g_hwnd = nullptr;

LRESULT CALLBACK OverlayProc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp) {
    if (!InPassthrough()) {
        if (msg == WM_KEYDOWN && wp == VK_TAB && (GetKeyState(VK_SHIFT) & 0x8000)) {
            ToggleSidebar();
            Log("guide: %s by Shift+Tab", SidebarOpen() ? "opened" : "closed");
            return 0;
        }
        if (SidebarOpen()) {
            if (CodeEntryMessage(msg, wp) || KeyboardMessage(msg, wp)) {
                return 0;
            }
            if (msg == WM_KEYDOWN) {
                switch (wp) {
                case VK_UP: Navigate(Nav::Up); break;
                case VK_DOWN: Navigate(Nav::Down); break;
                case VK_LEFT: Navigate(Nav::Left); break;
                case VK_RIGHT: Navigate(Nav::Right); break;
                case VK_RETURN:
                case VK_SPACE: Navigate(Nav::Accept); break;
                case VK_ESCAPE:
                case VK_BACK: Navigate(Nav::Back); break;
                default: break;
                }
            }
            const bool keyboard = msg >= WM_KEYFIRST && msg <= WM_KEYLAST;
            const bool mouse = (msg >= WM_MOUSEFIRST && msg <= WM_MOUSELAST) || msg == WM_INPUT;
            if (keyboard || mouse) {
                ImGui_ImplWin32_WndProcHandler(hwnd, msg, wp, lp);
                return msg == WM_INPUT ? DefWindowProcW(hwnd, msg, wp, lp) : 0;
            }
        }
    }
    return CallWindowProcW(g_game_proc, hwnd, msg, wp, lp);
}

// --- XInput (ours, not the game's)
// XINPUT_STATE plus the DWORD XInputGetStateEx (ordinal 100) also writes: 20 bytes, not 16.
struct XState {
    DWORD packet;
    WORD buttons;
    BYTE lt, rt;
    SHORT lx, ly, rx, ry;
    DWORD reserved;
};
static_assert(sizeof(XState) == 20, "XInputGetStateEx writes 20 bytes");
std::atomic<bool> g_hold_until_release{false}; // after the guide closes: the game sees an idle pad until every button is up
using GetStateExFn = DWORD(WINAPI *)(DWORD, XState *);
GetStateExFn g_get_state = nullptr;
WORD g_previous = 0;
Nav g_held = Nav::None;
std::chrono::steady_clock::time_point g_next_repeat;

constexpr WORD kUp = 0x0001, kDown = 0x0002, kLeft = 0x0004, kRight = 0x0008, kStart = 0x0010, kBack = 0x0020, kRThumb = 0x0080, kLB = 0x0100, kRB = 0x0200,
               kGuide = 0x0400, kA = 0x1000, kB = 0x2000, kX = 0x4000, kY = 0x8000;
VoiceGestures g_voice_gestures; // the render thread's

// OD-154: R3 and RB + R3 while driving (the guide closed).
void VoiceGesture(WORD buttons) {
    const VoiceState v = CurrentVoice();
    const auto result = g_voice_gestures.Update(buttons & kRB, buttons & kRThumb, v.mic == MicMode::PushToTalk);
    SetPushToTalk(result.talk && (v.flags & VoiceState::Enabled));
    if (!result.cycle) return;
    if (!(v.flags & VoiceState::Enabled)) {
        PushToast(ToastKind::Info, ui::T("Voice chat is off", "Chat de voz desligado"),
                  ui::T("Turn it on in the guide: Settings > Voice chat", "Ligue no guia: Ajustes > Chat de voz"), 25);
        return;
    }
    const auto next = static_cast<MicMode>((static_cast<uint8_t>(v.mic) + 1) % (kMicModeMax + 1));
    VoiceCommand(VoiceOp::Mic, std::string(), static_cast<uint8_t>(next));
    PushToast(ToastKind::Info, ui::T("Microphone", "Microfone"), MicModeName(next), 20);
    Log("voice: mic mode %u by RB + R3", static_cast<unsigned>(next));
}

Nav Direction(const XState &s) {
    constexpr SHORT dead = 16000;
    if ((s.buttons & kUp) || s.ly > dead) return Nav::Up;
    if ((s.buttons & kDown) || s.ly < -dead) return Nav::Down;
    if ((s.buttons & kLeft) || s.lx < -dead) return Nav::Left;
    if ((s.buttons & kRight) || s.lx > dead) return Nav::Right;
    return Nav::None;
}

// --- Windows.Gaming.Input
using ABI::Windows::Gaming::Input::GamepadReading;
using ReadingFn = HRESULT(STDMETHODCALLTYPE *)(void *, GamepadReading *);
ReadingFn g_reading = nullptr;
std::atomic<bool> g_pad_hooked{false};
std::atomic<bool> g_open_seen{false}; // the render thread's SidebarOpen(), for the game's input thread

// GamepadButtons
constexpr UINT32 kWMenu = 0x1, kWView = 0x2, kWA = 0x4, kWB = 0x8, kWX = 0x10, kWY = 0x20, kWLB = 0x400, kWRB = 0x800, kWUp = 0x40, kWDown = 0x80, kWLeft = 0x100, kWRight = 0x200,
                 kWRThumb = 0x2000;
constexpr auto kTriggerHold = std::chrono::milliseconds(1000); // LB + RB held this long toggles the guide

// Each pad's latest raw reading, for the render thread's navigation: merged, since an idle second
// pad (a virtual one, a wheel) is read too and must not hide the one in the player's hands.
std::atomic<UINT32> g_w_buttons[4]{};
std::atomic<int> g_w_lx[4]{}, g_w_ly[4]{}; // thumbstick x 32767
std::atomic<bool> g_trigger_toggle{false};

// A chord held for kTriggerHold: true once per hold, when it gets there.
struct Chord {
    bool held = false, fired = false;
    std::chrono::steady_clock::time_point since{};
    bool Update(bool down, std::chrono::steady_clock::time_point now) {
        if (!down) { held = fired = false; return false; }
        if (!held) { held = true; fired = false; since = now; return false; }
        if (fired || now - since < kTriggerHold) return false;
        fired = true;
        return true;
    }
};
Chord g_xinput_shoulders; // the render thread's, from XInput, until the WGI hook is in

// The LB + RB chord's state, on the game's input thread only. Several pads share one vtable slot: one state per pad.
struct Trigger {
    void *pad = nullptr;
    Chord shoulders{};
};
Trigger g_triggers[4];

Trigger &TriggerOf(void *pad) {
    for (auto &t : g_triggers) {
        if (t.pad == pad) return t;
    }
    for (auto &t : g_triggers) {
        if (!t.pad) { t.pad = pad; return t; }
    }
    return g_triggers[0];
}

HRESULT STDMETHODCALLTYPE HookReading(void *self, GamepadReading *value) {
    const HRESULT hr = g_reading(self, value);
    if (FAILED(hr) || !value || InPassthrough()) {
        return hr;
    }
    const auto raw = static_cast<UINT32>(value->Buttons);
    Trigger &trigger = TriggerOf(self);
    const size_t slot = static_cast<size_t>(&trigger - g_triggers);
    g_w_buttons[slot] = raw;
    g_w_lx[slot] = static_cast<int>(value->LeftThumbstickX * 32767);
    g_w_ly[slot] = static_cast<int>(value->LeftThumbstickY * 32767);
    const auto now = std::chrono::steady_clock::now();
    if (trigger.shoulders.Update((raw & (kWLB | kWRB)) == (kWLB | kWRB), now)) g_trigger_toggle = true;
    if (g_open_seen.load() || g_hold_until_release.load()) {
        const UINT64 stamp = value->Timestamp;
        *value = GamepadReading{};
        value->Timestamp = stamp;
        return hr;
    }
    // View is never held back: Azimuth 1.0's own menu opens with View held (OD-125 follow-up)
    return hr;
}

bool TryHookGamepad() {
    using namespace ABI::Windows::Gaming::Input;
    using namespace ABI::Windows::Foundation::Collections;
    Microsoft::WRL::ComPtr<IGamepadStatics> statics;
    Microsoft::WRL::Wrappers::HStringReference name(RuntimeClass_Windows_Gaming_Input_Gamepad);
    if (FAILED(RoGetActivationFactory(name.Get(), IID_PPV_ARGS(&statics)))) {
        return false;
    }
    Microsoft::WRL::ComPtr<IVectorView<Gamepad *>> pads;
    unsigned count = 0;
    if (FAILED(statics->get_Gamepads(&pads)) || FAILED(pads->get_Size(&count)) || count == 0) {
        return false;
    }
    Microsoft::WRL::ComPtr<IGamepad> pad;
    if (FAILED(pads->GetAt(0, &pad))) {
        return false;
    }
    // IInspectable (6 slots), then IGamepad: get_Vibration, put_Vibration, GetCurrentReading
    void **vtable = *reinterpret_cast<void ***>(pad.Get());
    return MH_CreateHook(vtable[8], reinterpret_cast<void *>(&HookReading), reinterpret_cast<void **>(&g_reading)) == MH_OK &&
           MH_EnableHook(vtable[8]) == MH_OK;
}

DWORD WINAPI GamepadThread(void *) {
    RoInitialize(RO_INIT_MULTITHREADED);
    // a pad may be plugged in later: look every few seconds until one can be hooked
    for (int i = 0; i < 2400 && !InPassthrough(); ++i) {
        if (TryHookGamepad()) {
            g_pad_hooked = true;
            Log("controller: the game's reading is gated while the sidebar is open");
            break;
        }
        Sleep(3000);
    }
    RoUninitialize();
    return 0;
}

} // namespace

bool InstallWindowHook(HWND hwnd) {
    if (g_game_proc || !hwnd) {
        return g_game_proc != nullptr;
    }
    g_hwnd = hwnd;
    g_game_proc = reinterpret_cast<WNDPROC>(SetWindowLongPtrW(hwnd, GWLP_WNDPROC, reinterpret_cast<LONG_PTR>(&OverlayProc)));
    if (HMODULE xinput = LoadLibraryW(L"xinput1_4.dll")) {
        g_get_state = reinterpret_cast<GetStateExFn>(GetProcAddress(xinput, reinterpret_cast<LPCSTR>(100)));
    }
    Log("input: window subclassed; Xbox button %s", g_get_state ? "available" : "unavailable (xinput1_4 missing)");
    return g_game_proc != nullptr;
}

bool InstallGamepadHook() {
    HANDLE thread = CreateThread(nullptr, 0, GamepadThread, nullptr, 0, nullptr);
    if (thread) {
        CloseHandle(thread);
    }
    return thread != nullptr;
}

void PollController() {
    g_open_seen = SidebarOpen();
    if (g_trigger_toggle.exchange(false)) {
        ToggleSidebar();
        Log("guide: %s by holding View or LB + RB", SidebarOpen() ? "opened" : "closed");
        g_hold_until_release = true;
        g_open_seen = SidebarOpen();
    }
    // the reading to navigate with: the game's own (WGI) once hooked, else ours (XInput)
    XState state{};
    bool any = false;
    if (g_get_state) {
        for (DWORD user = 0; user < 4 && !any; ++user) {
            any = g_get_state(user, &state) == ERROR_SUCCESS;
        }
    }
    const bool guide = any && (state.buttons & kGuide) && !(g_previous & kGuide);
    if (!g_pad_hooked.load() && g_xinput_shoulders.Update(any && (state.buttons & (kLB | kRB)) == (kLB | kRB), std::chrono::steady_clock::now())) {
        ToggleSidebar();
        Log("guide: %s by holding LB + RB", SidebarOpen() ? "opened" : "closed");
        g_hold_until_release = true;
        g_open_seen = SidebarOpen();
        return;
    }
    if (g_pad_hooked.load()) {
        UINT32 w = 0;
        int lx = 0, ly = 0;
        for (size_t i = 0; i < 4; ++i) {
            w |= g_w_buttons[i].load();
            const int x = g_w_lx[i].load(), y = g_w_ly[i].load();
            if (std::abs(x) > std::abs(lx)) lx = x;
            if (std::abs(y) > std::abs(ly)) ly = y;
        }
        state.buttons = static_cast<WORD>((state.buttons & kGuide) | ((w & kWUp) ? kUp : 0) | ((w & kWDown) ? kDown : 0) |
                                          ((w & kWLeft) ? kLeft : 0) | ((w & kWRight) ? kRight : 0) | ((w & kWA) ? kA : 0) |
                                          ((w & kWB) ? kB : 0) | ((w & kWX) ? kX : 0) | ((w & kWY) ? kY : 0) |
                                          ((w & kWMenu) ? kStart : 0) | ((w & kWView) ? kBack : 0) |
                                          ((w & kWLB) ? kLB : 0) | ((w & kWRB) ? kRB : 0) | ((w & kWRThumb) ? kRThumb : 0));
        state.lx = static_cast<SHORT>(lx);
        state.ly = static_cast<SHORT>(ly);
        state.lt = state.rt = 0;
        any = true;
    }
    if (!any) {
        g_previous = 0;
        g_voice_gestures.Reset();
        SetPushToTalk(false);
        return;
    }
    const WORD pressed = state.buttons & static_cast<WORD>(~g_previous);
    g_previous = state.buttons;
    const bool idle = state.buttons == 0 && std::abs(state.lx) < 8000 && std::abs(state.ly) < 8000 && state.lt < 30 && state.rt < 30;
    if (idle) {
        g_hold_until_release = false;
    }
    if (guide) {
        ToggleSidebar();
        Log("guide: %s by the Xbox button", SidebarOpen() ? "opened" : "closed");
        g_hold_until_release = true;
        g_open_seen = SidebarOpen();
        return;
    }
    if (!SidebarOpen()) {
        g_held = Nav::None;
        VoiceGesture(state.buttons);
        return;
    }
    g_voice_gestures.Reset(); // in the guide RB is a tab, and nobody talks by holding R3 in a menu
    SetPushToTalk(false);
    if (pressed & kA) Navigate(Nav::Accept);
    if (pressed & kX) Navigate(Nav::X);
    if (pressed & kY) Navigate(Nav::Y);
    if (pressed & kStart) Navigate(Nav::Menu);
    if (pressed & kLB) Navigate(Nav::TabLeft);
    if (pressed & kRB) Navigate(Nav::TabRight);
    if (pressed & kB) {
        Navigate(Nav::Back);
        g_hold_until_release = true; // the B that closed the guide never reaches the game
    }
    const auto now = std::chrono::steady_clock::now();
    const Nav dir = Direction(state);
    if (dir != g_held) {
        g_held = dir;
        if (dir != Nav::None) {
            Navigate(dir);
            g_next_repeat = now + std::chrono::milliseconds(380);
        }
    } else if (dir != Nav::None && now >= g_next_repeat) {
        Navigate(dir);
        g_next_repeat = now + std::chrono::milliseconds(110);
    }
}

} // namespace fi
