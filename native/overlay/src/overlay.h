// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay (OD-38, OD-50): shared declarations. C++20, MSVC, x64.
#pragma once

#include <windows.h>

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

#include "ui_frames.h"

struct ImDrawList;

namespace fi {

// --- log and lifetime (main.cpp)
void Log(const char *format, ...);
// Turns the overlay off for good: hooks disabled, the game runs untouched (any fault).
void Passthrough(const char *why);
bool InPassthrough();
bool DevHost();
// OD-118: started by the launcher's Play offline: online-only tabs show "Offline mode".
bool OfflineSession();
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
std::string ProfileGamertag(); // the gamertag the guide shows ("Infinity" before the launcher's Status)
void SetLink(Link link, const std::string &detail);
// CR-L40 §1.3: the connected server's figures for the guide's bottom block (0x19 ServerStatus)
void SetServerStatus(const ServerStatus &status);
void SetConnectionDetails(uint8_t state, const std::string &text);
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

// --- worlds (worlds.cpp, OD-54/56/57/58, CR-L40): the guide's Lobbies tab; the launcher answers through the pipe
enum class WorldFilter : uint8_t { None, Friends, Near, Global, All };
// WorldRow is in ui_frames.h (protocol 2 adds modded, modsMatch, members and mods)
struct HostedWorld {
    uint8_t status = 0; // 0 open, else refused
    std::string world, code, message;
};
// status 0 ok, 1 loading, 2 error; `modded` echoes the query (protocol 1 answers are vanilla)
void SetWorlds(WorldFilter filter, bool modded, uint8_t status, const std::string &message, std::vector<WorldRow> rows);
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

// --- the right pane (pane.cpp, OD-112): one component for every detail view, drawn beside the guide
// The launcher's pane (DetailPane frame; any thread): a non-empty id opens it (and the guide), empty closes it.
void SetDetailPane(DetailPane pane);
// The game's player card (any thread): opens the guide on that player's pane at once (name, avatar
// placeholder, a disabled "Add friend") and asks the launcher for the rest.
void OpenPlayerCard(const std::string &xuid, const std::string &gamertag);
// Render thread, every frame: what the focused or hovered lobby row shows (null: nothing).
void SetHoverPane(const DetailPane *pane);
void PaneTick(float dt);      // render thread, before the guide: applies what arrived (may open the guide)
bool PaneModal();             // a launcher or player pane is open: it takes the controller
bool PaneNavigate(Nav nav);   // true: the pane used it
void ClosePane();
void DrawPane(ImDrawList *dl, float x, float y, float w, float height, float alpha); // render thread

// Development host only: a request the render thread runs at its next frame (1, 2 the keyboard; 3 the code).
void DevRequest(int request);

// --- input (input.cpp)
bool InstallWindowHook(HWND hwnd);
void PollController(); // on the render thread, once per frame
bool InstallGamepadHook(); // Windows.Gaming.Input: the game sees an idle pad while the sidebar is open

// --- D3D12 (dx12.cpp)
bool InstallRenderHooks();
// CR-L40 §1.3 (0x1A Avatar): any thread; the render thread uploads it at its next frame. Side 0 clears it.
void SetAvatar(AvatarImage avatar);
// Render thread: the avatar's texture for ImDrawList::AddImageRounded, or 0 (draw the initials).
uint64_t AvatarTexture();

// --- the UI pipe to the launcher (pipe.cpp)
void StartPipe(const wchar_t *name);
void SendEvent(uint8_t kind, const std::string &detail); // UiEventKind: 1 first frame, 5 passthrough, 7 quit
void SendPanel(bool open);
void SendWorldsQuery(WorldFilter filter, bool not_full); // 0x14 WorldsQueryV2 (modded 0: every lobby of the filter)
void SendPlayerDetailsQuery(const std::string &xuid, const std::string &gamertag);
void SendPaneAction(const std::string &pane, const std::string &action);
void SendJoinWorld(const std::string &world, const std::string &code);
void SendCreateWorld(const std::string &name, uint8_t audience, uint8_t max_players, uint8_t latency);
void SendMyWorldQuery();
// action: 1 close, 2 leave, 3 kick, 4 add friend, 5 report (reason 1 cheating, 2 abuse, 3 other)
void SendWorldAction(uint8_t action, const std::string &xuid, const std::string &gamertag, uint8_t reason);

} // namespace fi
