// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the UI pipe client (ORCHESTRATOR §4.3; the launcher's Overlay/UiFrame.cs is
// the reference codec, with the same envelope as the control pipe: u32 LE length of type + body,
// u8 type, little-endian fields, strings as u16 LE length + UTF-8, at most 64 KiB).
//
// Overlay -> launcher: 0x01 UiHello{u16 protocol = 2, u32 pid, str version} first; 0x04
// Panel{u8 open} when the guide opens or closes; 0x05 UiEvent{u8 kind, str detail}; 0x06 Bye.
// Launcher -> overlay: 0x02 Status{str account, u8 tier (0 free, 1 Infinity Supporter), u8 state
// (0 queued, 1 running, 2 offline, 3 exiting), str detail}; 0x03 Toast{u8 level, u8 icon, u16 ttl
// in tenths, str text}: `icon` is the toast kind (ToastKind order) and `text` is "Title\nBody";
// 0x04 Panel{u8 open} opens or closes the guide.
// 0x11 GameReadiness{u8 ready}, lease 1.5 s; 0x12 WorldAccess{u8 allowed, str message},
// an independent 10 s lease. Missing/malformed/expired installation access stays denied.
//
// Worlds (OD-54/57/58): the lobby list, join by code, create and hosted frames (0x07-0x0C, 0x14, 0x15) are gone (OD-208: players
// host no world; the game's own menus create, invite and join, OD-204 item 2). Any of them from an older launcher is an unknown
// frame here, ignored.
//
// My world (OD-63): overlay -> launcher 0x0D MyWorldQuery{} (on explicit open/action/refresh while the lobby tab is
// shown); 0x0F WorldAction{u8 action (2 leave, 4 add friend, 5 report; 1 close and 3 kick are retired, OD-208), str xuid,
// str gamertag, u8 reason (1 cheating, 2 abuse, 3 other)}. Launcher -> overlay 0x0E MyWorld{u8 role
// (0 none, 2 reserved, 3 seated; 1 hosting is retired), str worldId, str name, str host, str code (empty), u8 audience,
// u8 players, u8 max, u8 named, u8 count, count x {str xuid, str gamertag, u8 tier, u8 role (0 host,
// 1 seated, 2 reserved), u8 friend (0 none, 1 friends, 2 outgoing, 3 incoming, 4 self)}, u8 anyMode (OD-141)};
// 0x10 ActionResult{u8 action, u8 status (0 done, 1 refused), str message}.
//
// Protocol 2 (OD-112; layouts in ui_frames.h): 0x16 DetailPane is the right pane; 0x17 PlayerDetailsQuery and 0x18
// PaneActionClick go to the launcher. 0x19 ServerStatus and 0x1A Avatar (CR-L40 §1.3) fill the
// guide's bottom block and its header avatar.
//
// Protocol 3 (OD-154; layouts in ui_frames.h): 0x1B VoiceState from the launcher (the lobby's voice chat, the mic
// mode, each player's mute and talking state); 0x1C VoiceCommand to it (mutes, the mic mode, voice on/off, the
// volumes) and 0x1D VoiceDevice (the chosen microphone or output, VOICE_CHAT.md §15).
//
// Protocol 4 (OD-181): 0x1E RosterMods from the launcher after each 0x0E, the mods each player of my world declared
// (players may mix mods; modsMatch is always 1 and is ignored).
//
// Protocol 5 (OD-193 F-2; layouts in ui_frames.h, the cache in player_actions.h): the game's own PLAYER ACTIONS popup asks a
// player's friend state (0x1F PlayerStateQuery -> 0x20 PlayerState) and sends our items' actions (0x21 PlayerActionRequest ->
// 0x22 PlayerActionDone) through the fi_overlay_player_* exports the hook calls; nothing of it is drawn here.
//
// Protocol 6 (OD-204 T-L3; layouts in ui_frames.h, the queue in lobby_policy.h): 0x23 InvitePrompt from the launcher is an invite
// toast (invite.cpp, INVITE_UX: hold Y to accept); 0x24 InviteAnswer carries the accept back. The 0x03 toast's additive flags byte
// marks the notices that show even in an official lobby; 0x0E MyWorld's additive leader names who may invite.
//
// Protocol 7 (OD-204 T-L4): 0x26 FriendPickerAsk is the Private Lobby pane's Invite friends. The centred picker is gone (INVITE_UX): an
// older launcher's 0x25 with the open flag opens the FRIENDS sidebar's invite mode, as protocol 12's 0x2E FriendsOpen does; its rows
// and a refresh without the flag are ignored (the sidebar shows 0x2A), and 0x27 InviteSend is never sent.
//
// Protocol 8 (OD-210, CR-S101, W14; layouts in ui_frames.h, the rule in lobby_policy.h): 0x28 LobbyJoinModes from the launcher is the
// private lobby's "Who can join" (after every 0x0E); 0x29 JoinModesSet carries the leader's choice.
//
// Threads: the game's Present (the draw) never does I/O. Frames to the launcher go into a queue
// that this file's thread drains; the pipe is overlapped, so a pending read never holds a write
// (a synchronous handle serialises them, which froze the game inside Present on 2026-10-19).
// The overlay holds no tokens: everything it shows comes from here (OD-50). It reconnects every
// second for the life of the process; frames queued while disconnected are dropped.
#include "overlay.h"
#include "player_actions.h"
#include "fi_overlay.h"

#include <deque>
#include <mutex>
#include <string>
#include <vector>

namespace fi {

namespace {
std::wstring g_name;
std::mutex g_out_lock;
std::deque<std::vector<uint8_t>> g_out; // whole frames, waiting for the pipe thread
HANDLE g_out_event = nullptr;           // set when g_out has something
bool g_connected = false;
PlayerActionCache g_players; // OD-193 F-2

void PutU16(std::vector<uint8_t> &b, uint16_t v) { wire::U16(b, v); }
void PutU32(std::vector<uint8_t> &b, uint32_t v) { wire::U32(b, v); }
void PutStr(std::vector<uint8_t> &b, const std::string &s) { wire::Str(b, s); }

// Any thread: queue a frame (never blocks on the pipe); false when no launcher is connected (dropped).
bool Queue(uint8_t type, const std::vector<uint8_t> &body) {
    std::vector<uint8_t> frame = wire::Frame(type, body);
    {
        std::lock_guard<std::mutex> lock(g_out_lock);
        if (!g_connected) {
            return false;
        }
        if (g_out.size() >= 64) {
            g_out.pop_front();
        }
        g_out.push_back(std::move(frame));
    }
    SetEvent(g_out_event);
    return true;
}

using Reader = wire::Reader;

void OnFrame(uint8_t type, const uint8_t *body, size_t size) {
    Reader r{body, size};
    switch (type) {
    case 0x23: { // protocol 6 (T-L3): the invite prompt
        InvitePromptFrame prompt;
        if (DecodeInvitePrompt(body, size, prompt)) SetInvitePrompt(prompt);
        else Log("ui pipe: malformed InvitePrompt ignored");
        break;
    }
    case 0x2A: { // protocol 10 (OD-211, W15): the friends sidebar
        FriendsStateFrame state;
        if (DecodeFriendsState(body, size, state)) SetFriendsState(std::move(state));
        else Log("ui pipe: malformed FriendsState ignored");
        break;
    }
    case 0x2C: {
        FriendsResultFrame result;
        if (DecodeFriendsResult(body, size, result)) SetFriendsResult(std::move(result));
        else Log("ui pipe: malformed FriendsResult ignored");
        break;
    }
    case 0x2D: { // protocol 11 (OD-211, CR-S102): a friend's Discord avatar
        FriendAvatarFrame avatar;
        if (DecodeFriendAvatar(body, size, avatar)) SetFriendAvatar(std::move(avatar));
        else Log("ui pipe: malformed FriendAvatar ignored");
        break;
    }
    case 0x28: { // protocol 8 (OD-210, W14): the private lobby's "Who can join"
        LobbyJoinModesFrame modes;
        if (DecodeLobbyJoinModes(body, size, modes)) SetLobbyJoinModes(std::move(modes));
        else Log("ui pipe: malformed LobbyJoinModes ignored");
        break;
    }
    case 0x25: { // protocol 7 (T-L4): an older launcher's friend picker: its open flag opens the sidebar's invite mode
        FriendPickerFrame picker;
        if (!DecodeFriendPicker(body, size, picker)) Log("ui pipe: malformed FriendPicker ignored");
        else if (picker.open()) FriendsOpen(true);
        break;
    }
    case 0x2E: { // protocol 12 (INVITE_UX): open the guide on the FRIENDS sidebar, focused
        uint8_t mode = 0;
        if (DecodeFriendsOpen(body, size, mode)) FriendsOpen(mode == kFriendsOpenInvite);
        else Log("ui pipe: malformed FriendsOpen ignored");
        break;
    }
    case 0x16: {
        DetailPane pane;
        if (DecodeDetailPane(body, size, pane)) SetDetailPane(std::move(pane));
        else Log("ui pipe: malformed DetailPane ignored");
        break;
    }
    case 0x19: {
        ServerStatus status;
        if (DecodeServerStatus(body, size, status)) SetServerStatus(status);
        else Log("ui pipe: malformed ServerStatus ignored");
        break;
    }
    case 0x1A: {
        AvatarImage avatar;
        if (DecodeAvatar(body, size, avatar)) SetAvatar(std::move(avatar));
        else Log("ui pipe: malformed Avatar ignored");
        break;
    }
    case 0x1B: {
        VoiceState voice;
        if (DecodeVoiceState(body, size, voice)) SetVoiceState(std::move(voice));
        else Log("ui pipe: malformed VoiceState ignored");
        break;
    }
    case 0x1E: { // OD-181 (protocol 4): each lobby player's declared mods
        RosterMods mods;
        if (DecodeRosterMods(body, size, mods)) SetRosterMods(std::move(mods));
        else Log("ui pipe: malformed RosterMods ignored");
        break;
    }
    case 0x20: { // OD-193 F-2 (protocol 5)
        PlayerStateFrame state;
        if (DecodePlayerState(body, size, state)) g_players.OnState(state);
        else Log("ui pipe: malformed PlayerState ignored");
        break;
    }
    case 0x22: {
        PlayerActionDone done;
        if (DecodePlayerActionDone(body, size, done)) g_players.OnDone(done);
        else Log("ui pipe: malformed PlayerActionDone ignored");
        break;
    }
    case 0x13: {
        const uint8_t state = r.U8();
        const std::string text = r.Str();
        if (r.ok && r.left == 0 && state <= 2 && text.size() <= 4096) SetConnectionDetails(state, text);
        break;
    }
    case 0x12: {
        const uint8_t allowed = r.U8();
        const std::string message = r.Str();
        if (r.ok && r.left == 0 && allowed <= 1 && message.size() <= 512) SetWorldAccess(allowed == 1, message);
        else SetWorldAccess(false, {});
        break;
    }
    case 0x11: {
        const uint8_t ready = r.U8();
        if (r.ok && size == 1 && ready <= 1) SetGameReadiness(ready == 1);
        break;
    }
    case 0x02: {
        const std::string account = r.Str();
        const uint8_t tier = r.U8(), state = r.U8();
        const std::string detail = r.Str();
        if (!r.ok) return;
        SetProfile(account, tier == 1 ? Tier::Supporter : Tier::Free);
        SetLink(state == 1 ? Link::Online : state == 0 ? Link::Connecting : state == 2 ? Link::Offline : Link::Reconnecting, detail);
        break;
    }
    case 0x03: {
        ToastFrame t; // the level is read and not used: the kind carries the look
        if (!DecodeToast(body, size, t)) return;
        const size_t nl = t.text.find('\n');
        const ToastKind kind = t.icon <= static_cast<uint8_t>(ToastKind::Friend) ? static_cast<ToastKind>(t.icon) : ToastKind::Info;
        PushToast(kind, nl == std::string::npos ? t.text : t.text.substr(0, nl), nl == std::string::npos ? "" : t.text.substr(nl + 1), t.ttl, t.always());
        break;
    }
    case 0x0E: {
        MyWorld m;
        if (DecodeMyWorld(body, size, m)) SetMyWorld(std::move(m)); // ui_frames.h (anyMode CR-L43, private T-L6, leader T-L3)
        break;
    }
    case 0x10: {
        const uint8_t action = r.U8(), status = r.U8();
        const std::string message = r.Str();
        if (r.ok) SetActionResult(action, status, message);
        break;
    }
    case 0x04: {
        const bool open = r.U8() != 0;
        if (r.ok && open != SidebarOpen()) {
            ToggleSidebar();
        }
        break;
    }
    default:
        break; // Bye or an overlay-bound frame we do not use
    }
}

// One overlapped write, bounded (2 s, as the launcher bounds its own).
bool WriteAll(HANDLE pipe, const std::vector<uint8_t> &frame, HANDLE event) {
    OVERLAPPED ov{};
    ov.hEvent = event;
    DWORD written = 0;
    if (!WriteFile(pipe, frame.data(), static_cast<DWORD>(frame.size()), nullptr, &ov) && GetLastError() != ERROR_IO_PENDING) {
        return false;
    }
    if (WaitForSingleObject(event, 2000) != WAIT_OBJECT_0) {
        CancelIoEx(pipe, &ov);
        GetOverlappedResult(pipe, &ov, &written, TRUE);
        return false;
    }
    return GetOverlappedResult(pipe, &ov, &written, FALSE) && written == frame.size();
}

void Session(HANDLE pipe) {
    HANDLE read_event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    HANDLE write_event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    {
        std::lock_guard<std::mutex> lock(g_out_lock);
        g_out.clear();
        g_connected = true;
    }
    std::vector<uint8_t> hello;
    PutU16(hello, kUiProtocol);
    PutU32(hello, GetCurrentProcessId());
    PutStr(hello, "0.2.0");
    Queue(0x01, hello);
    Log("ui pipe: connected");

    std::vector<uint8_t> buffer;
    uint8_t chunk[4096];
    bool reading = false;
    OVERLAPPED rov{};
    for (bool alive = true; alive && !InPassthrough();) {
        if (!reading) {
            rov = OVERLAPPED{};
            rov.hEvent = read_event;
            ResetEvent(read_event);
            if (!ReadFile(pipe, chunk, sizeof(chunk), nullptr, &rov) && GetLastError() != ERROR_IO_PENDING) {
                break;
            }
            reading = true;
        }
        HANDLE waits[] = {read_event, g_out_event};
        const DWORD which = WaitForMultipleObjects(2, waits, FALSE, 1000);
        if (which == WAIT_OBJECT_0) {
            reading = false;
            DWORD got = 0;
            if (!GetOverlappedResult(pipe, &rov, &got, FALSE) || got == 0) {
                break;
            }
            buffer.insert(buffer.end(), chunk, chunk + got);
            while (buffer.size() >= 5) {
                const uint32_t length = buffer[0] | (buffer[1] << 8) | (buffer[2] << 16) | (static_cast<uint32_t>(buffer[3]) << 24);
                if (length == 0 || length > 64 * 1024) {
                    alive = false; // not our protocol: close
                    break;
                }
                if (buffer.size() < 4 + length) {
                    break;
                }
                OnFrame(buffer[4], buffer.data() + 5, length - 1);
                buffer.erase(buffer.begin(), buffer.begin() + 4 + length);
            }
        } else if (which == WAIT_OBJECT_0 + 1) {
            std::deque<std::vector<uint8_t>> out;
            {
                std::lock_guard<std::mutex> lock(g_out_lock);
                out.swap(g_out);
                ResetEvent(g_out_event);
            }
            for (const auto &frame : out) {
                ResetEvent(write_event);
                if (!WriteAll(pipe, frame, write_event)) {
                    alive = false;
                    break;
                }
            }
        }
    }
    if (reading) {
        CancelIoEx(pipe, &rov);
        DWORD ignored = 0;
        GetOverlappedResult(pipe, &rov, &ignored, TRUE);
    }
    {
        std::lock_guard<std::mutex> lock(g_out_lock);
        g_connected = false;
        g_players.Disconnected();
        SetConnectionDetails(2, "");
        g_out.clear();
    }
    InvitesDisconnected(); // T-L3: no launcher to answer to: its prompts go
    FriendsDisconnected(); // W15: and the friends sidebar
    CloseHandle(read_event);
    CloseHandle(write_event);
}

DWORD WINAPI PipeThread(void *) {
    while (!InPassthrough()) {
        HANDLE pipe = CreateFileW(g_name.c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING,
                                  FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, nullptr);
        if (pipe == INVALID_HANDLE_VALUE) {
            Sleep(1000);
            continue;
        }
        Session(pipe);
        CloseHandle(pipe);
        Log("ui pipe: disconnected");
        Sleep(1000);
    }
    return 0;
}
} // namespace

void StartPipe(const wchar_t *name) {
    g_name = name;
    g_out_event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (HANDLE thread = CreateThread(nullptr, 0, PipeThread, nullptr, 0, nullptr)) {
        CloseHandle(thread);
    }
}

void SendEvent(uint8_t kind, const std::string &detail) {
    std::vector<uint8_t> body;
    body.push_back(kind);
    PutStr(body, detail);
    Queue(0x05, body);
}

void SendPanel(bool open) { Queue(0x04, {static_cast<uint8_t>(open ? 1 : 0)}); }

void SendPlayerDetailsQuery(const std::string &xuid, const std::string &gamertag) { Queue(0x17, EncodePlayerDetailsQuery(xuid, gamertag)); }

void SendPaneAction(const std::string &pane, const std::string &action) { Queue(0x18, EncodePaneActionClick(pane, action)); }

void SendVoiceCommand(VoiceOp op, const std::string &xuid, uint8_t value) { Queue(0x1C, EncodeVoiceCommand(op, xuid, value)); }

void SendVoiceDevice(VoiceFlow flow, const std::string &id, const std::string &name) { Queue(0x1D, EncodeVoiceDevice(flow, id, name)); }

void SendMyWorldQuery() { Queue(0x0D, {}); }

void SendWorldAction(uint8_t action, const std::string &xuid, const std::string &gamertag, uint8_t reason) {
    std::vector<uint8_t> body;
    body.push_back(action);
    PutStr(body, xuid);
    PutStr(body, gamertag);
    body.push_back(reason);
    Queue(0x0F, body);
}

void SendInviteAnswer(const std::string &invite, bool accept) { Queue(0x24, EncodeInviteAnswer(invite, accept)); }

void SendFriendPickerAsk() { Queue(0x26, {}); }


void SendJoinModesSet(uint8_t modes) { Queue(0x29, EncodeJoinModesSet(modes)); }
void SendFriendsAction(uint8_t kind, const std::string &id, const std::string &text) {
    if (FriendsActionValid(kind, id, text)) Queue(0x2B, EncodeFriendsAction(kind, id, text));
}

} // namespace fi

// OD-193 F-2 (fi_overlay.h): the game's PLAYER ACTIONS, for the hook. Any thread; they never wait for the pipe.
extern "C" __declspec(dllexport) uint32_t fi_overlay_player_state(uint64_t xuid) {
    if (fi::InPassthrough()) return FI_PLAYER_OWN; // no launcher behind it: no item at all
    return fi::g_players.State(xuid, GetTickCount64(), fi::Queue);
}

extern "C" __declspec(dllexport) uint32_t fi_overlay_player_action(uint64_t xuid, uint32_t kind, uint32_t reason) {
    if (fi::InPassthrough() || kind > 0xFF || reason > 0xFF) return 0;
    return fi::g_players.Act(xuid, static_cast<uint8_t>(kind), static_cast<uint8_t>(reason), GetTickCount64(), fi::Queue);
}

extern "C" __declspec(dllexport) int32_t fi_overlay_player_result(uint32_t id, uint32_t *result, uint32_t *state) {
    return fi::g_players.Result(id, GetTickCount64(), result, state);
}
