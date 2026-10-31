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
// Worlds (OD-54/57/58; the launcher talks to matchmaking, the overlay only asks):
// Overlay -> launcher: 0x07 WorldsQuery{u8 filter (1 friends, 2 near, 3 global), u8 notFull};
// 0x09 JoinWorld{str worldId, str code (8 digits or empty)}; 0x0B CreateWorld{str name,
// u8 audience (0 everyone, 1 supporters, 2 friends, 3 private), u8 maxPlayers, u8 latency
// (0 any, 1 < 150 ms, 2 < 100 ms, 3 < 60 ms), u8 anyMode (OD-141: 0 the host's mode, 1 Hardcore and Noob)}.
// Launcher -> overlay: 0x08 WorldsList{u8 filter, u8 status (0 ok, 2 error), str message,
// u8 count, count x {str id, str name, str host, u8 players, u8 max, u8 kind, u8 audience,
// u8 lock (0 none, 1 tier, 2 friends, 3 code, 4 full, 5 latency), u8 ping (0-3), u8 friendHost}};
// 0x0A JoinResult{u8 status (0 seat reserved, 1 full, 2 not allowed, 3 wrong code, 4 closed,
// 5 slow down), str message}; 0x0C Hosted{u8 status (0 open), str worldId, str code, str message}.
//
// My world (OD-63): overlay -> launcher 0x0D MyWorldQuery{} (on explicit open/action/refresh while the Worlds tab is
// shown); 0x0F WorldAction{u8 action (1 close, 2 leave, 3 kick, 4 add friend, 5 report), str xuid,
// str gamertag, u8 reason (1 cheating, 2 abuse, 3 other)}. Launcher -> overlay 0x0E MyWorld{u8 role
// (0 none, 1 hosting, 2 reserved, 3 seated), str worldId, str name, str host, str code, u8 audience,
// u8 players, u8 max, u8 named, u8 count, count x {str xuid, str gamertag, u8 tier, u8 role (0 host,
// 1 seated, 2 reserved), u8 friend (0 none, 1 friends, 2 outgoing, 3 incoming, 4 self)}, u8 anyMode (OD-141)};
// 0x10 ActionResult{u8 action, u8 status (0 done, 1 refused), str message}.
//
// Protocol 2 (OD-112; layouts in ui_frames.h): this overlay asks with 0x14 WorldsQueryV2 (filter 4
// all by default, modded 0: every lobby, CR-L40) instead of 0x07 and still reads a 0x08 answer as
// vanilla; 0x15 WorldsListV2 rows (at most 50, ordered by the launcher) carry modded, modsMatch,
// players and mods; 0x16 DetailPane is the right pane; 0x17 PlayerDetailsQuery and 0x18
// PaneActionClick go to the launcher. 0x19 ServerStatus and 0x1A Avatar (CR-L40 §1.3) fill the
// guide's bottom block and its header avatar.
//
// Protocol 3 (OD-154; layouts in ui_frames.h): 0x1B VoiceState from the launcher (the lobby's voice chat, the mic
// mode, each player's mute and talking state); 0x1C VoiceCommand to it (the host's lobby switch, mutes, the mic
// mode, voice on/off, the volumes) and 0x1D VoiceDevice (the chosen microphone or output, VOICE_CHAT.md §15). 0x0B
// CreateWorld ends with u8 voice.
//
// Threads: the game's Present (the draw) never does I/O. Frames to the launcher go into a queue
// that this file's thread drains; the pipe is overlapped, so a pending read never holds a write
// (a synchronous handle serialises them, which froze the game inside Present on 2026-10-19).
// The overlay holds no tokens: everything it shows comes from here (OD-50). It reconnects every
// second for the life of the process; frames queued while disconnected are dropped.
#include "overlay.h"

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

void PutU16(std::vector<uint8_t> &b, uint16_t v) { wire::U16(b, v); }
void PutU32(std::vector<uint8_t> &b, uint32_t v) { wire::U32(b, v); }
void PutStr(std::vector<uint8_t> &b, const std::string &s) { wire::Str(b, s); }

// Any thread: queue a frame (never blocks on the pipe).
void Queue(uint8_t type, const std::vector<uint8_t> &body) {
    std::vector<uint8_t> frame = wire::Frame(type, body);
    {
        std::lock_guard<std::mutex> lock(g_out_lock);
        if (!g_connected) {
            return;
        }
        if (g_out.size() >= 64) {
            g_out.pop_front();
        }
        g_out.push_back(std::move(frame));
    }
    SetEvent(g_out_event);
}

using Reader = wire::Reader;

void OnFrame(uint8_t type, const uint8_t *body, size_t size) {
    Reader r{body, size};
    switch (type) {
    case 0x15: {
        WorldsListV2 list;
        if (DecodeWorldsListV2(body, size, list)) {
            SetWorlds(static_cast<WorldFilter>(list.filter), list.modded, list.status, list.message, std::move(list.rows));
        } else {
            Log("ui pipe: malformed WorldsListV2 ignored");
        }
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
        r.U8(); // level: the kind carries the look
        const uint8_t icon = r.U8();
        const uint16_t ttl = r.U16();
        const std::string text = r.Str();
        if (!r.ok) return;
        const size_t nl = text.find('\n');
        const ToastKind kind = icon <= static_cast<uint8_t>(ToastKind::Friend) ? static_cast<ToastKind>(icon) : ToastKind::Info;
        PushToast(kind, nl == std::string::npos ? text : text.substr(0, nl), nl == std::string::npos ? "" : text.substr(nl + 1), ttl);
        break;
    }
    case 0x08: {
        const auto filter = static_cast<WorldFilter>(r.U8());
        const uint8_t status = r.U8();
        const std::string message = r.Str();
        const uint8_t count = r.U8();
        std::vector<WorldRow> rows;
        for (uint8_t i = 0; i < count && r.ok && i < 16; ++i) {
            WorldRow w;
            w.id = r.Str();
            w.name = r.Str();
            w.host = r.Str();
            w.players = r.U8();
            w.max = r.U8();
            w.kind = r.U8();
            w.audience = r.U8();
            w.lock = r.U8();
            w.ping = r.U8();
            w.friend_host = r.U8() != 0;
            rows.push_back(std::move(w));
        }
        if (r.ok) SetWorlds(filter, false, status, message, std::move(rows)); // a protocol 1 answer: vanilla
        break;
    }
    case 0x0A: {
        const uint8_t status = r.U8();
        const std::string message = r.Str();
        if (r.ok) SetJoinResult(status, message);
        break;
    }
    case 0x0C: {
        HostedWorld h;
        h.status = r.U8();
        h.world = r.Str();
        h.code = r.Str();
        h.message = r.Str();
        if (r.ok) SetHosted(h);
        break;
    }
    case 0x0E: {
        MyWorld m;
        if (DecodeMyWorld(body, size, m)) SetMyWorld(std::move(m)); // ui_frames.h (anyMode, CR-L43)
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
        SetConnectionDetails(2, "");
        g_out.clear();
    }
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

void SendWorldsQuery(WorldFilter filter, bool not_full) {
    Queue(0x14, EncodeWorldsQueryV2(static_cast<uint8_t>(filter), not_full, false));
}

void SendPlayerDetailsQuery(const std::string &xuid, const std::string &gamertag) { Queue(0x17, EncodePlayerDetailsQuery(xuid, gamertag)); }

void SendPaneAction(const std::string &pane, const std::string &action) { Queue(0x18, EncodePaneActionClick(pane, action)); }

void SendJoinWorld(const std::string &world, const std::string &code) {
    std::vector<uint8_t> body;
    PutStr(body, world);
    PutStr(body, code);
    Queue(0x09, body);
}

void SendCreateWorld(const std::string &name, uint8_t audience, uint8_t max_players, uint8_t latency, bool any_mode, bool voice) {
    Queue(0x0B, EncodeCreateWorld(name, audience, max_players, latency, any_mode, voice));
}

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

} // namespace fi
