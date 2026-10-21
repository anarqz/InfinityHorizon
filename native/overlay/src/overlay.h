// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay (OD-38, OD-50): shared declarations. C++20, MSVC, x64.
#pragma once

#include <windows.h>

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

struct ImDrawList;

namespace fi {

// --- log and lifetime (main.cpp)
void Log(const char *format, ...);
// Turns the overlay off for good: hooks disabled, the game runs untouched (any fault).
void Passthrough(const char *why);
bool InPassthrough();
bool DevHost();
// The hook's pre-world paint upload (fi_overlay.h FI_LIVERY_*); FI_LIVERY_UNAVAILABLE without one. Blocks: not on the render thread.
int LiverySync(uint32_t timeout_ms);
void SetGameReadiness(bool ready);
bool GameReady();
void SetWorldAccess(bool allowed, const std::string &message);
void InitToastSound(HMODULE self);
void PlayToastSound();

// --- the model the UI shows (ui.cpp); every setter is thread-safe
enum class Tier : uint8_t { Unknown, Free, Supporter };
enum class Link : uint8_t { Connecting, Online, Offline, Reconnecting };
enum class ToastKind : uint8_t { Info, Success, Warning, Error, Achievement, Update, Friend };

void SetProfile(const std::string &gamertag, Tier tier);
void SetLink(Link link, const std::string &detail);
// ttl is the fully open hold time in tenths of a second; 0 uses the default, bounded to 1..30 s.
void PushToast(ToastKind kind, const std::string &title, const std::string &body, uint16_t ttl = 0);

// --- drawing (ui.cpp), called on the render thread inside an ImGui frame
void LoadFonts(float scale);
void DrawUi(float width, float height, float dt);
bool SidebarOpen();
void ToggleSidebar();
void CloseSidebar();

// Navigation from the controller or the keyboard, edge-triggered (input.cpp feeds ui.cpp).
enum class Nav : uint8_t { None, Up, Down, Left, Right, Accept, Back, X, Y, Menu, TabLeft, TabRight };
void Navigate(Nav nav);

// --- text entry (keyboard.cpp): the Xbox on-screen keyboard; a real keyboard types into it too
struct KeyboardRequest {
    std::string title;   // what is asked ("World password")
    std::string hint;    // shown while empty
    std::string initial; // the text it starts with
    bool secret = false; // dots instead of the text
    size_t max = 32;     // in characters
};
// Called on the render thread: ok and the text, or false and an empty string (cancelled).
using KeyboardDone = std::function<void(bool ok, const std::string &text)>;
void OpenKeyboard(const KeyboardRequest &request, KeyboardDone done); // render thread
bool KeyboardOpen();
void KeyboardNavigate(Nav nav);
bool KeyboardMessage(UINT msg, WPARAM wp); // the window procedure: true when the keyboard took it
void DrawKeyboard(float width, float height, float dt);

// --- the world code (code_entry.cpp, OD-57): XXXX-XXXX, large in the centre; A joins once complete
using CodeDone = std::function<void(bool join, const std::string &code)>; // render thread; code without the hyphen
void OpenCodeEntry(const std::string &title, const std::string &alphabet, CodeDone done); // render thread
bool CodeEntryOpen();
void CodeEntryNavigate(Nav nav);
void CodeEntryRefused(const std::string &why); // render thread
bool CodeEntryMessage(UINT msg, WPARAM wp);
void DrawCodeEntry(float width, float height, float dt);

// --- worlds (worlds.cpp, OD-54/56/57/58): the guide's Worlds tab; the launcher answers through the pipe
enum class WorldFilter : uint8_t { None, Friends, Near, Global };
struct WorldRow {
    std::string id, name, host;
    uint8_t players = 0, max = 0;
    uint8_t kind = 0;      // 0 everyone's, 1 Infinity Supporter
    uint8_t audience = 0;  // 0 everyone, 1 supporters, 2 friends, 3 private
    uint8_t lock = 0;      // 0 joinable, 1 tier, 2 friends, 3 code, 4 full, 5 latency
    uint8_t ping = 0;      // 0 unknown, 1 good, 2 fair, 3 poor
    bool friend_host = false;
};
struct HostedWorld {
    uint8_t status = 0; // 0 open, else refused
    std::string world, code, message;
};
void SetWorlds(WorldFilter filter, uint8_t status, const std::string &message, std::vector<WorldRow> rows); // status 0 ok, 1 loading, 2 error
void SetJoinResult(uint8_t status, const std::string &message); // 0 seat reserved, 3 wrong code, else refused
void SetHosted(const HostedWorld &hosted);

// "My world" (OD-63): the world this profile hosts or holds a seat in, and its players. The XUID
// is the key of an action, never shown (OD-61).
struct MemberRow {
    std::string xuid, gamertag;
    uint8_t tier = 0;         // 0 free, 1 Infinity Supporter
    uint8_t role = 0;         // 0 host, 1 in the world, 2 on the way (seat reserved)
    uint8_t friend_state = 0; // 0 none, 1 friends, 2 request sent, 3 asked you, 4 you
};
struct MyWorld {
    uint8_t role = 0; // 0 none, 1 hosting, 2 seat reserved, 3 seated
    std::string world, name, host, code;
    uint8_t audience = 0, players = 0, max = 0;
    bool named = false; // false: the server gave no roster (only the host is known)
    std::vector<MemberRow> members;
};
void SetMyWorld(MyWorld mine);
void SetActionResult(uint8_t action, uint8_t status, const std::string &message); // status 0 done
void WorldsReset();
bool WorldsNavigate(Nav nav); // false: the guide handles it (tabs, close)
bool WorldsCanRefresh();      // Y refreshes what the tab shows (the footer's hint)
void DrawWorldsTab(ImDrawList *dl, float x, float y, float w, float bottom, bool supporter, float dt);

// Development host only: a request the render thread runs at its next frame (1, 2 the keyboard; 3 the code).
void DevRequest(int request);

// --- input (input.cpp)
bool InstallWindowHook(HWND hwnd);
void PollController(); // on the render thread, once per frame
bool InstallGamepadHook(); // Windows.Gaming.Input: the game sees an idle pad while the sidebar is open

// --- D3D12 (dx12.cpp)
bool InstallRenderHooks();

// --- the UI pipe to the launcher (pipe.cpp)
void StartPipe(const wchar_t *name);
void SendEvent(uint8_t kind, const std::string &detail); // UiEventKind: 1 first frame, 5 passthrough, 7 quit
void SendPanel(bool open);
void SendWorldsQuery(WorldFilter filter, bool not_full);
void SendJoinWorld(const std::string &world, const std::string &code);
void SendCreateWorld(const std::string &name, uint8_t audience, uint8_t max_players, uint8_t latency);
void SendMyWorldQuery();
// action: 1 close, 2 leave, 3 kick, 4 add friend, 5 report (reason 1 cheating, 2 abuse, 3 other)
void SendWorldAction(uint8_t action, const std::string &xuid, const std::string &gamertag, uint8_t reason);

} // namespace fi
