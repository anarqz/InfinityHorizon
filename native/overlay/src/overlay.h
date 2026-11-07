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
struct ImVec2;

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
enum class Tier : uint8_t { Unknown, Free, Supporter }; // Free: an Infinity Racer; the tier is a badge only (CR-S69)
enum class Link : uint8_t { Connecting, Online, Offline, Reconnecting };
enum class ToastKind : uint8_t { Info, Success, Warning, Error, Achievement, Update, Friend };

void SetProfile(const std::string &gamertag, Tier tier);
std::string ProfileGamertag(); // the gamertag the guide shows ("Infinity" before the launcher's Status)
void SetLink(Link link, const std::string &detail);
// CR-L40 §1.3: the connected server's figures for the guide's bottom block (0x19 ServerStatus)
void SetServerStatus(const ServerStatus &status);
void SetConnectionDetails(uint8_t state, const std::string &text);
// ttl is the fully open hold time in tenths of a second; 0 uses the default, bounded to 1..30 s. `always` (T-L3, the launcher's
// toast flag kToastAlways): shown even in an official lobby; any other notice follows OD-192 (lobby_policy.h ToastShows).
void PushToast(ToastKind kind, const std::string &title, const std::string &body, uint16_t ttl = 0, bool always = false);

// --- drawing (ui.cpp), called on the render thread inside an ImGui frame
void LoadFonts(float scale);
void DrawUi(float width, float height, float dt);
bool SidebarOpen();
void ToggleSidebar();
void CloseSidebar();

// Navigation from the controller or the keyboard, edge-triggered (input.cpp feeds ui.cpp).
enum class Nav : uint8_t { None, Up, Down, Left, Right, Accept, Back, X, Y, Menu, TabLeft, TabRight };
void Navigate(Nav nav);

// --- the lobby tab (worlds.cpp, OD-63, OD-192, OD-204 T-L3): one roster pane of the lobby this player is in; the launcher talks
// to matchmaking and answers through the pipe. No list, code or create (OD-204 item 2: the game's own menus do that).
// "My world": MemberRow and MyWorld are in ui_frames.h (CR-L43 adds any_mode, T-L6 is_private, T-L3 leader).
void SetMyWorld(MyWorld mine);
// OD-181 (protocol 4, 0x1E): the mods each player of my world declared (RosterMods in ui_frames.h).
void SetRosterMods(RosterMods mods);
void SetActionResult(uint8_t action, uint8_t status, const std::string &message); // status 0 done
void WorldsReset();
bool WorldsNavigate(Nav nav); // false: the guide handles it (tabs, close)
bool WorldsCanRefresh();      // Y refreshes what the tab shows (the footer's hint)
bool WorldsMuteHint();        // X mutes the focused player (OD-154; the footer's hint)
bool WorldsOfficial();        // OD-192: the player is in an official lobby, a private one included (any thread): no notices
bool WorldsPrivate();         // OD-204: the player is in a private official lobby (any thread): the tab is the Private Lobby

// --- the invite prompt (invite.cpp, OD-204 T-L3, WORLDS_OFFICIAL §12.5): a modal Accept / Decline, never a toast
void SetInvitePrompt(const InvitePromptFrame &prompt); // any thread (0x23); expires 0 withdraws that id
void InvitesDisconnected();                             // any thread: the launcher went away, every prompt goes
bool InvitePromptOpen();                                // render thread: a prompt is shown
// Render thread, while the guide is open: the prompt holds the controller (true: used). With the guide closed the prompt only
// shows, with how to open the guide, so a prompt never takes the pad from the game while driving.
bool InviteNavigate(Nav nav);
void InviteMouse(float width, float height);                // render thread, every frame, before the guide: a modal's clicks
void DrawInvitePrompt(float width, float height, float dt); // render thread, every frame, over the guide
void DrawWorldsTab(ImDrawList *dl, float x, float y, float w, float bottom, float dt);
// A player's details in my lobby (their volume, mute, moderation, friend, report) fill the second column beside the
// guide while open; the tab keeps showing the lobby. Drawn after DrawWorldsTab, x..x + w.
bool WorldsColumnOpen();
void DrawWorldsColumn(ImDrawList *dl, float x, float w, float height, float alpha);

// --- voice chat (voice.cpp, OD-154, VOICE_CHAT.md §11): the launcher holds the state, the overlay shows it and asks
// VoiceState and VoiceMember are in ui_frames.h (0x1B).
void SetVoiceState(VoiceState state); // any thread
VoiceState CurrentVoice();            // a copy, any thread
// OD-165: a team event runs for this player (the relay's 0xC0FA, fresh): its team (0 red, 1 blue); false otherwise
bool VoiceTeamChat(uint8_t *team);
// Asks the launcher (0x1C) and applies the change at once, so the screen follows the button before the answer.
void VoiceCommand(VoiceOp op, const std::string &xuid, uint8_t value);
// §15: the player picked a microphone or an output (empty id: the system default). Applied at once, kept by the
// launcher (0x1D). Any thread.
void VoiceDeviceCommand(VoiceFlow flow, const std::string &id, const std::string &name);
// §15: what the settings list, refreshed by the voice thread every few seconds (and soon after a request): "System
// default" (empty id) first; only ids that fit the pipe. Any thread.
struct VoiceDeviceEntry {
    std::string id, name;
};
std::vector<VoiceDeviceEntry> VoiceDevices(VoiceFlow flow);
void RequestVoiceDevices(); // the settings are showing: refresh the lists soon
// A microphone drawn with lines (the icon font has none), centred on `c`, `size` tall; `muted` adds the slash,
// cut in `behind` (the surface's colour). Colours are ImU32.
void DrawMic(ImDrawList *dl, ImVec2 c, float size, uint32_t color, bool muted, uint32_t behind = 0xFF2A2626);
const char *MicModeName(MicMode mode);
// The hook's voice transport (FiOverlayHost.voice_send, size >= 64) and this player's XUID: starts voice chat's
// thread (subscription to the world relay, the audio engine). Called once from fi_overlay_start.
void SetVoiceHost(int32_t (*send)(const uint8_t *datagram, uint32_t size), uint64_t xuid);
// R3 push-to-talk is held now (input.cpp sets it; the audio engine, VC-3, reads it).
void SetPushToTalk(bool held);
bool PushToTalkHeld();

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
void SendPlayerDetailsQuery(const std::string &xuid, const std::string &gamertag);
void SendPaneAction(const std::string &pane, const std::string &action);
void SendVoiceCommand(VoiceOp op, const std::string &xuid, uint8_t value); // 0x1C
void SendVoiceDevice(VoiceFlow flow, const std::string &id, const std::string &name); // 0x1D (§15)
void SendMyWorldQuery();
// action: 2 leave, 4 add friend, 5 report (reason 1 cheating, 2 abuse, 3 other). 1 close and 3 kick stay reserved (T-L3).
void SendWorldAction(uint8_t action, const std::string &xuid, const std::string &gamertag, uint8_t reason);
void SendInviteAnswer(const std::string &invite, bool accept); // 0x24 (protocol 6, T-L3)

} // namespace fi
