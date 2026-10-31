// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's state and its looks (OD-154, docs/launcher/VOICE_CHAT.md §5, §11, §12). The launcher
// holds the truth (the lobby's switch, the mute history, the settings) and sends it as 0x1B VoiceState; the overlay
// shows it and asks for changes with 0x1C VoiceCommand. A change is applied here at once (the button follows the
// press) and the launcher's next VoiceState settles it.
//
// - The world relay's view (VC-1/VC-2: 0x3E94 state down through the hook) joins the launcher's: a joiner learns the
//   lobby's switch from it, and who talks comes from it and from the audio engine (voice_audio.h). Merged() is what the
//   guide and the game show.
// - The voice thread subscribes this player to the relay (its mic flags and its mute list, again every 5 s and on any
//   change; leave when voice turns off), runs the engine only while the lobby has voice, opens the microphone as the
//   mic mode says (open, muted, or R3 held) and silences the seats this player must not hear.
//
// - The microphone is drawn with lines: the embedded icon font has none.
// - Who talks is shown by the game itself (OD-154: the top-right mini leaderboard and the session player list): the
//   hook answers the game's queries from fi_overlay_voice_state, read lock-free from a snapshot of the state.
#include "overlay.h"

#include <algorithm>
#include <atomic>
#include <charconv>
#include <cmath>
#include <mutex>
#include <thread>
#include <vector>

#include "fi_overlay.h"
#include "style.h"
#include "voice_audio.h"
#include "voice_volume.h"
#include "voice_wire.h"

namespace fi {

using namespace ui;

namespace {
std::mutex g_lock;
VoiceState g_voice; // the launcher's (0x1B), with this overlay's commands applied
bool g_dev_seeded = false;
// §15: a slider moved or a device picked here in the last second wins over the launcher's answer to an earlier step,
// so a held Left/Right never snaps back; the launcher's state settles it after that.
constexpr ULONGLONG kLocalHoldMs = 1000;
ULONGLONG g_levels_at = 0, g_devices_at = 0;

// The world relay's last state (0x3E94 down), under g_lock; stale after 5 s without one.
struct RelayView {
    bool known = false;
    bool lobby_on = false;
    uint8_t bundle = 3;
    std::vector<voice_wire::Seat> seats;
    ULONGLONG at = 0;
    // CR-L46: what to play follows this (0xC0F9, else the lobby byte); WorldView.voice is display only
    uint8_t mode = voice_wire::kModeOff;
    uint16_t range = 0;
} g_relay;
constexpr ULONGLONG kRelayStaleMs = 5000;

bool RelayFresh(const RelayView &r) { return r.known && GetTickCount64() - r.at < kRelayStaleMs; }

// The game's view (fi_overlay_voice_state): a fixed table of atomics, rewritten whenever the state changes. A reader
// may see a slot mid-rewrite for one frame; it never blocks the game's UI thread.
struct Slot {
    std::atomic<uint64_t> xuid{0};
    std::atomic<uint32_t> bits{0};
};
Slot g_slots[kMaxVoiceMembers];
std::atomic<uint32_t> g_self_bits{0};

uint64_t XuidNumber(const std::string &xuid) {
    uint64_t n = 0;
    const auto [end, error] = std::from_chars(xuid.data(), xuid.data() + xuid.size(), n);
    return error == std::errc() && end == xuid.data() + xuid.size() ? n : 0;
}

// The launcher's state with the relay's view and the engine's talkers (g_lock held).
VoiceState Merged(const VoiceState &launcher) {
    VoiceState v = launcher;
    if (!RelayFresh(g_relay)) return v;
    if (!(v.flags & VoiceState::LobbyKnown)) // a joiner: the host's relay says the lobby's switch
        v.flags = static_cast<uint8_t>(v.flags | VoiceState::LobbyKnown | (g_relay.lobby_on ? VoiceState::LobbyOn : 0));
    for (auto &m : v.members) {
        const uint64_t xuid = XuidNumber(m.xuid);
        for (const auto &seat : g_relay.seats) {
            if (seat.xuid != xuid) continue;
            // in proximity a talker beyond the audible range is talking but not heard (the relay's bits are per listener there)
            const bool engine = g_relay.mode == voice_wire::kModeProximity ? audio::SeatHeard(seat.seat) : audio::SeatTalking(seat.seat);
            if ((seat.state & voice_wire::kSeatTalking) || engine) m.state |= VoiceMember::Talking;
            if (seat.state & voice_wire::kSeatMutedForLobby) m.state |= VoiceMember::MutedForLobby;
            if (seat.state & voice_wire::kSeatBlocked) m.state |= VoiceMember::Blocked;
        }
    }
    return v;
}

// Called with g_lock held, after every change (and by the voice thread, for who talks).
void Publish(const VoiceState &launcher) {
    const VoiceState v = Merged(launcher);
    uint32_t self = GameVoiceBits(v, std::string());
    if (self && audio::Sending()) self |= kGameVoiceTalking;
    g_self_bits.store(self, std::memory_order_release);
    for (size_t i = 0; i < kMaxVoiceMembers; ++i) {
        const bool used = i < v.members.size();
        g_slots[i].xuid.store(0, std::memory_order_release); // invalidate first: a reader never pairs a new xuid with old bits
        g_slots[i].bits.store(used ? GameVoiceBits(v, v.members[i].xuid) : 0, std::memory_order_release);
        g_slots[i].xuid.store(used ? XuidNumber(v.members[i].xuid) : 0, std::memory_order_release);
    }
}

// The development host: a lobby where the second player talks, so every look can be seen.
void DevSeed(VoiceState &v) {
    if (g_dev_seeded) return;
    g_dev_seeded = true;
    v.flags = VoiceState::LobbyOn | VoiceState::LobbyKnown | VoiceState::MayChange | VoiceState::Enabled;
    v.mic = MicMode::Open;
    // the dev host's own lobby (worlds.cpp): "1" is this player, never in the list
    v.members = {{"2", "Synthetic#00230", VoiceMember::Talking}, {"3", "Synthetic#00042", 0}};
    Publish(v);
}

void Apply(VoiceState &v, VoiceOp op, const std::string &xuid, uint8_t value) {
    const auto set = [](uint8_t &bits, uint8_t bit, bool on) { bits = static_cast<uint8_t>(on ? bits | bit : bits & ~bit); };
    switch (op) {
    case VoiceOp::LobbyVoice:
        if (v.flags & VoiceState::MayChange) {
            set(v.flags, VoiceState::LobbyOn, value != 0);
            set(v.flags, VoiceState::LobbyKnown, true);
        }
        break;
    case VoiceOp::Mute:
        for (auto &m : v.members)
            if (m.xuid == xuid) set(m.state, VoiceMember::MutedByMe, value != 0);
        break;
    case VoiceOp::MuteEveryone:
        for (auto &m : v.members) set(m.state, VoiceMember::MutedByMe, value != 0);
        break;
    case VoiceOp::Mic:
        if (value <= kMicModeMax) v.mic = static_cast<MicMode>(value);
        break;
    case VoiceOp::Enabled:
        set(v.flags, VoiceState::Enabled, value != 0);
        break;
    case VoiceOp::MuteForLobby:
        if (v.flags & VoiceState::MayChange)
            for (auto &m : v.members)
                if (m.xuid == xuid) set(m.state, VoiceMember::MutedForLobby, value != 0);
        break;
    case VoiceOp::VoiceVolume: // §15
        if (VolumeValue(value)) { v.voice_volume = value; g_levels_at = GetTickCount64(); }
        break;
    case VoiceOp::MicVolume:
        if (VolumeValue(value)) { v.mic_volume = value; g_levels_at = GetTickCount64(); }
        break;
    case VoiceOp::PlayerVolume:
        if (VolumeValue(value)) {
            for (auto &m : v.members)
                if (m.xuid == xuid) m.volume = value;
            g_levels_at = GetTickCount64();
        }
        break;
    }
    Publish(v);
}
} // namespace

void SetVoiceState(VoiceState state) {
    std::lock_guard<std::mutex> lock(g_lock);
    const ULONGLONG now = GetTickCount64();
    if (now - g_levels_at < kLocalHoldMs) { // §15: the sliders being moved keep their place
        state.voice_volume = g_voice.voice_volume;
        state.mic_volume = g_voice.mic_volume;
        for (auto &m : state.members)
            for (const auto &old : g_voice.members)
                if (old.xuid == m.xuid) m.volume = old.volume;
    }
    if (now - g_devices_at < kLocalHoldMs) {
        state.mic_id = g_voice.mic_id;
        state.mic_name = g_voice.mic_name;
        state.output_id = g_voice.output_id;
        state.output_name = g_voice.output_name;
    }
    g_voice = std::move(state);
    Publish(g_voice);
}

VoiceState CurrentVoice() {
    std::lock_guard<std::mutex> lock(g_lock);
    if (DevHost()) DevSeed(g_voice);
    return Merged(g_voice);
}

void VoiceCommand(VoiceOp op, const std::string &xuid, uint8_t value) {
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (DevHost()) DevSeed(g_voice);
        Apply(g_voice, op, xuid, value);
    }
    if (!DevHost()) SendVoiceCommand(op, xuid, value);
}

void VoiceDeviceCommand(VoiceFlow flow, const std::string &id, const std::string &name) {
    if (!DeviceIdSendable(id)) return; // never listed (VoiceDevices drops them)
    const std::string shown = id.empty() ? std::string() : Utf8Cut(name, kMaxDeviceText);
    {
        std::lock_guard<std::mutex> lock(g_lock);
        if (DevHost()) DevSeed(g_voice);
        (flow == VoiceFlow::Microphone ? g_voice.mic_id : g_voice.output_id) = id;
        (flow == VoiceFlow::Microphone ? g_voice.mic_name : g_voice.output_name) = shown;
        g_devices_at = GetTickCount64();
    }
    if (!DevHost()) SendVoiceDevice(flow, id, shown);
}

// ---------------------------------------------------------------- the settings' device lists (§15)
namespace {
std::mutex g_devices_lock;
std::vector<VoiceDeviceEntry> g_device_lists[2]; // [VoiceFlow]: "System default" first
bool g_devices_known = false;                     // a refresh listed at least one endpoint
std::atomic<bool> g_devices_wanted{true};

// The voice thread: both lists again (COM on this thread, never the game's).
void RefreshDevices() {
    std::vector<VoiceDeviceEntry> lists[2];
    bool known = false;
    for (int flow = 0; flow < 2; flow++) {
        for (const audio::Device &d : audio::ListDevices(flow == 0)) {
            if (!DeviceIdSendable(d.id) || !Utf8(d.name)) continue; // the launcher could not keep it
            lists[flow].push_back({d.id, d.name});
            if (!d.id.empty()) known = true;
        }
        if (lists[flow].empty() || !lists[flow][0].id.empty()) lists[flow].insert(lists[flow].begin(), VoiceDeviceEntry{});
    }
    std::lock_guard<std::mutex> lock(g_devices_lock);
    g_device_lists[0] = std::move(lists[0]);
    g_device_lists[1] = std::move(lists[1]);
    g_devices_known = known;
}

// A chosen device that is not in the list now: said once per device for the game session; the engine already uses the
// system default meanwhile (voice_audio_devices.h).
void TellMissing(const VoiceState &v) {
    static std::string told[2]; // voice thread only
    std::lock_guard<std::mutex> lock(g_devices_lock);
    if (!g_devices_known) return; // no list (COM failed, or nothing at all): nothing reliable to say
    for (int flow = 0; flow < 2; flow++) {
        const std::string &id = flow == 0 ? v.mic_id : v.output_id;
        const std::string &name = flow == 0 ? v.mic_name : v.output_name;
        if (id.empty() || told[flow] == id) continue;
        const auto &list = g_device_lists[flow];
        if (std::any_of(list.begin(), list.end(), [&](const VoiceDeviceEntry &e) { return e.id == id; })) continue;
        told[flow] = id;
        Log("voice: the chosen %s is not connected; using the system default", flow == 0 ? "microphone" : "output");
        PushToast(ToastKind::Warning, flow == 0 ? T("Microphone not found", "Microfone não encontrado") : T("Audio output not found", "Saída de áudio não encontrada"),
                  (name.empty() ? std::string(flow == 0 ? T("Your microphone", "Seu microfone") : T("Your audio output", "Sua saída de áudio")) : name)
                      + T(" is not connected. Voice chat uses the system default.", " não está conectado. O chat de voz usa o padrão do sistema."));
    }
}
} // namespace

std::vector<VoiceDeviceEntry> VoiceDevices(VoiceFlow flow) {
    if (DevHost()) { // the development host: a fake list, so every look can be seen
        if (flow == VoiceFlow::Microphone) return {{}, {"dev-mic-1", "Microphone (Synthetic USB Audio)"}, {"dev-mic-2", "Headset Microphone (Synthetic)"}};
        return {{}, {"dev-out-1", "Speakers (Synthetic Audio)"}, {"dev-out-2", "Headphones (Synthetic USB Audio)"}};
    }
    std::lock_guard<std::mutex> lock(g_devices_lock);
    const auto &list = g_device_lists[flow == VoiceFlow::Microphone ? 0 : 1];
    return list.empty() ? std::vector<VoiceDeviceEntry>{VoiceDeviceEntry{}} : list;
}

void RequestVoiceDevices() { g_devices_wanted.store(true, std::memory_order_relaxed); }

namespace {
std::atomic<bool> g_push_to_talk{false};
}

void SetPushToTalk(bool held) {
    if (g_push_to_talk.exchange(held) != held) Log("voice: push-to-talk %s", held ? "held" : "released");
}

bool PushToTalkHeld() { return g_push_to_talk.load(); }

// ---------------------------------------------------------------- the relay link and the engine (VC-1/2/3)
namespace {
std::atomic<int32_t (*)(const uint8_t *, uint32_t)> g_send{nullptr};
std::atomic<uint64_t> g_self_xuid{0};
std::atomic<uint32_t> g_noise{0x9E3779B9u}; // the transaction ids' random tails (not secret: STUN-shaped framing only)

void Noise(uint8_t *out, size_t n) {
    uint32_t x = g_noise.load(std::memory_order_relaxed) ^ static_cast<uint32_t>(GetTickCount64());
    for (size_t i = 0; i < n; ++i) {
        x ^= x << 13; x ^= x >> 17; x ^= x << 5;
        out[i] = static_cast<uint8_t>(x);
    }
    g_noise.store(x, std::memory_order_relaxed);
}

int32_t SendDatagram(const std::vector<uint8_t> &datagram) {
    const auto send = g_send.load();
    return send && !datagram.empty() ? send(datagram.data(), static_cast<uint32_t>(datagram.size())) : FI_VOICE_E_SHAPE;
}

// The engine's packets: framed here, sent by the hook.
void SendVoice(const uint8_t *opus, size_t size, uint8_t flags, uint16_t seq, uint32_t ts) {
    uint8_t random[4];
    Noise(random, sizeof(random));
    SendDatagram(voice_wire::EncodeVoiceUp(flags, seq, ts, opus, size, random));
}

struct Subscription {
    bool active = false;
    uint8_t self = 0;
    std::vector<uint64_t> mutes;
    ULONGLONG at = 0;
};

void SendControl(uint8_t op, uint8_t self, const std::vector<uint64_t> &mutes) {
    uint8_t transaction[12];
    Noise(transaction, sizeof(transaction));
    const int32_t result = SendDatagram(voice_wire::EncodeControlUp(op, g_self_xuid.load(), self, mutes, transaction));
    static int32_t last = 1;
    if (result != last) {
        last = result;
        Log("voice: control op %u to the relay: %d", static_cast<unsigned>(op), static_cast<int>(result));
    }
}

DWORD WINAPI VoiceThread(void *) {
    constexpr ULONGLONG kResubscribeMs = 5000, kStartRetryMs = 10000;
    Subscription sub;
    ULONGLONG next_start = 0, devices_at = 0;
    while (!InPassthrough()) {
        Sleep(50);
        const ULONGLONG now = GetTickCount64();
        VoiceState launcher;
        RelayView relay;
        {
            std::lock_guard<std::mutex> lock(g_lock);
            launcher = g_voice;
            relay = g_relay;
        }
        const VoiceState v = [&] { std::lock_guard<std::mutex> lock(g_lock); return Merged(launcher); }();
        const bool enabled = (v.flags & VoiceState::Enabled) && !OfflineSession();
        // subscribe while voice is on (the hook answers "no world" until there is one), leave when it turns off
        if (enabled) {
            const uint8_t self = v.mic == MicMode::Muted ? voice_wire::kSelfMicMuted : 0;
            std::vector<uint64_t> mutes;
            for (const auto &m : v.members)
                if ((m.state & VoiceMember::MutedByMe) && mutes.size() < voice_wire::kMaxSeats) mutes.push_back(XuidNumber(m.xuid));
            if (!sub.active || sub.self != self || sub.mutes != mutes || now - sub.at >= kResubscribeMs) {
                SendControl(voice_wire::kSubscribe, self, mutes);
                sub = {true, self, mutes, now};
            }
        } else if (sub.active) {
            SendControl(voice_wire::kLeave, 0, {});
            sub = {};
        }
        // §15: the settings' device lists (soon after the settings ask, else every 10 s) and the player's choices,
        // which the engine follows even between sessions
        const bool wanted = g_devices_wanted.load(std::memory_order_relaxed);
        if ((wanted && now - devices_at >= 1000) || now - devices_at >= 10000) {
            g_devices_wanted = false;
            devices_at = now;
            RefreshDevices();
            if (enabled) TellMissing(v);
        }
        audio::SetDevices(v.mic_id, v.output_id);
        audio::SetMicGain(audio::VolumeGain(v.mic_volume));
        // the engine runs while the lobby has voice and this player wants it
        const bool lobby = RelayFresh(relay) && relay.lobby_on;
        if (enabled && lobby) {
            if (!audio::Running() && now >= next_start) {
                if (!audio::Start(&SendVoice)) Log("voice: the audio engine did not start; trying again in 10 s");
                next_start = now + kStartRetryMs;
            }
        } else if (audio::Running()) {
            audio::Stop();
        }
        if (audio::Running()) {
            audio::SetBundle(relay.bundle);
            audio::SetVolume(audio::VolumeGain(v.voice_volume)); // §15: the mix of every other player
            const bool ptt = v.mic == MicMode::PushToTalk;
            const bool allowed = v.mic != MicMode::Muted && lobby;
            audio::SetTransmit(ptt ? allowed && PushToTalkHeld() : allowed, ptt);
            for (const auto &seat : relay.seats) {
                const std::string xuid = std::to_string(seat.xuid);
                const uint8_t st = v.StateOf(xuid);
                const bool silenced = seat.xuid == g_self_xuid.load() || (st & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked))
                                      || (seat.state & (voice_wire::kSeatMutedForLobby | voice_wire::kSeatBlocked));
                audio::SetSilenced(seat.seat, silenced);
                audio::SetSeatGain(seat.seat, audio::VolumeGain(v.VolumeOf(xuid))); // §15: this player's volume for them
            }
        }
        {
            std::lock_guard<std::mutex> lock(g_lock);
            Publish(g_voice); // who talks, for the game's icons
        }
    }
    if (audio::Running()) audio::Stop();
    return 0;
}
} // namespace

void SetVoiceHost(int32_t (*send)(const uint8_t *, uint32_t), uint64_t xuid) {
    if (!send || g_send.exchange(send)) return;
    g_self_xuid = xuid;
    if (xuid == 0) Log("voice: the hook gave no XUID; the relay will refuse this player's voice");
    if (HANDLE thread = CreateThread(nullptr, 0, VoiceThread, nullptr, 0, nullptr)) CloseHandle(thread);
    Log("voice: transport ready");
}

// ---------------------------------------------------------------- proximity (CR-L46, MATCHMAKING_API.md §5c)
namespace {
// Velocity on the wire is raw: ±32767 = ±200 m/s per axis (provisional until SAURON's live check; only this changes then).
constexpr float kVelocityScale = 200.0f / 32767.0f;
// The jump counters last seen (2 bits each; a change is a jump): per voice seat for talkers, and this player's own.
// Only fi_overlay_voice's thread touches them.
int g_talker_jumps[256];
int g_self_jumps = -1;
bool g_jumps_init = false;

audio::spatial::Pose SpatialPose(const voice_wire::PoseSide &side, bool jumped, const int16_t *orientation) {
    audio::spatial::Pose p;
    p.has_car = side.car;
    p.jumped = jumped;
    if (!side.car) return p;
    p.position = {side.pos[0], side.pos[1], side.pos[2]};
    p.velocity = {side.vel[0] * kVelocityScale, side.vel[1] * kVelocityScale, side.vel[2] * kVelocityScale};
    p.age_ms = side.age;
    if (orientation) {
        audio::spatial::OrientationFromWire(orientation[0], orientation[1], orientation[2], p.orientation);
        p.has_orientation = true;
    }
    return p;
}

// The engine's side of a proximity voice down (the adapter between the wire and voice_audio.h): the two poses, before
// Receive of the same packet. Without a usable pose both sides are carless, which the engine plays as open.
void ApplyPose(const voice_wire::VoiceDown &down) {
    if (!g_jumps_init) {
        for (int &j : g_talker_jumps) j = -1;
        g_jumps_init = true;
    }
    const voice_wire::Pose &pose = down.pose;
    if (!pose.present) {
        audio::SetPose(down.seat, audio::spatial::Pose{}, audio::spatial::Pose{});
        return;
    }
    const bool talker_jumped = g_talker_jumps[down.seat] >= 0 && g_talker_jumps[down.seat] != pose.talker.jumps;
    const bool self_jumped = g_self_jumps >= 0 && g_self_jumps != pose.listener.jumps;
    g_talker_jumps[down.seat] = pose.talker.jumps;
    g_self_jumps = pose.listener.jumps;
    audio::SetPose(down.seat, SpatialPose(pose.talker, talker_jumped, nullptr), SpatialPose(pose.listener, self_jumped, pose.orient));
}

// The engine follows state down's mode (and, in proximity, its R: clients never hard-code it).
void ApplyMode(uint8_t mode, uint16_t range) {
    static uint8_t last_mode = 0xFF;
    static uint16_t last_range = 0;
    if (mode != last_mode) {
        last_mode = mode;
        audio::SetVoiceMode(static_cast<audio::VoiceMode>(mode));
        Log("voice: the relay's mode is %s", mode == voice_wire::kModeProximity ? "proximity" : mode == voice_wire::kModeOpen ? "open" : "off");
    }
    if (mode == voice_wire::kModeProximity && range != last_range) {
        last_range = range;
        audio::spatial::Params params; // the defaults (OD-164), with the relay's R
        params.range_m = static_cast<float>(range);
        audio::SetSpatialParams(params);
    }
}
} // namespace

// A datagram from the relay (fi_overlay_voice): voice to the engine, state to the view.
void VoiceArrived(const uint8_t *data, size_t size) {
    voice_wire::VoiceDown down;
    if (voice_wire::DecodeVoiceDown(data, size, down)) {
        if (!audio::Running()) return;
        uint8_t mode;
        {
            std::lock_guard<std::mutex> lock(g_lock);
            mode = g_relay.mode;
        }
        if (mode == voice_wire::kModeProximity) ApplyPose(down); // open: any pose ignored
        audio::Receive(down.seat, down.seq, down.ts, down.flags, down.opus, down.size);
        return;
    }
    voice_wire::StateDown state;
    if (voice_wire::DecodeStateDown(data, size, state)) {
        ApplyMode(state.mode, state.range);
        std::lock_guard<std::mutex> lock(g_lock);
        g_relay = {true, state.mode != voice_wire::kModeOff, state.bundle, std::move(state.seats), GetTickCount64(), state.mode, state.range};
        Publish(g_voice);
    }
}

const char *MicModeName(MicMode mode) {
    switch (mode) {
    case MicMode::Muted: return T("Muted", "Mudo");
    case MicMode::PushToTalk: return T("Push to talk (R3)", "Apertar para falar (R3)");
    default: return T("Open", "Aberto");
    }
}

void DrawMic(ImDrawList *dl, ImVec2 c, float size, uint32_t color, bool muted, uint32_t behind) {
    // a 24-unit grid: the capsule 8 wide and 12 tall at the top, the cradle around its lower half, the stem and foot
    const float u = size / 24.0f, line = std::max(1.5f, 1.9f * u);
    const ImVec2 o(c.x - 12 * u, c.y - 12 * u);
    const auto P = [&](float x, float y) { return ImVec2(o.x + x * u, o.y + y * u); };
    dl->AddRectFilled(P(8, 2), P(16, 14.5f), color, 4 * u);
    dl->PathArcTo(P(12, 10.5f), 6.5f * u, 0.0f, 3.14159265f, 16);
    dl->PathStroke(color, 0, line);
    dl->AddLine(P(12, 17), P(12, 21), color, line);
    dl->AddLine(P(8.5f, 21.5f), P(15.5f, 21.5f), color, line);
    if (muted) {
        // the slash cuts the capsule in the colour behind it, so it reads at any size
        dl->AddLine(P(4, 3), P(20, 21), behind, line * 2.6f);
        dl->AddLine(P(4, 3), P(20, 21), color, line);
    }
}

} // namespace fi

// ---------------------------------------------------------------- the hook's view (fi_overlay.h, OD-154)
extern "C" __declspec(dllexport) uint32_t fi_overlay_voice_state(uint64_t xuid) {
    if (xuid == 0) return fi::g_self_bits.load(std::memory_order_acquire);
    for (const auto &slot : fi::g_slots) {
        if (slot.xuid.load(std::memory_order_acquire) != xuid) continue;
        const uint32_t bits = slot.bits.load(std::memory_order_acquire);
        return slot.xuid.load(std::memory_order_acquire) == xuid ? bits : 0; // rewritten meanwhile: unknown this frame
    }
    return 0;
}

extern "C" __declspec(dllexport) void fi_overlay_voice(const uint8_t *datagram, uint32_t size) {
    if (!datagram || size > fi::voice_wire::kMaxDatagram || fi::InPassthrough()) return;
    fi::VoiceArrived(datagram, size);
}

extern "C" __declspec(dllexport) void fi_overlay_voice_mute(uint64_t xuid, int32_t muted) {
    if (xuid == 0 || fi::InPassthrough()) return;
    fi::VoiceCommand(fi::VoiceOp::Mute, std::to_string(xuid), muted ? 1 : 0);
}
