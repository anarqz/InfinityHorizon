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
#include "voice_wire.h"

namespace fi {

using namespace ui;

namespace {
std::mutex g_lock;
VoiceState g_voice; // the launcher's (0x1B), with this overlay's commands applied
bool g_dev_seeded = false;

// The world relay's last state (0x3E94 down), under g_lock; stale after 5 s without one.
struct RelayView {
    bool known = false;
    bool lobby_on = false;
    uint8_t bundle = 3;
    std::vector<voice_wire::Seat> seats;
    ULONGLONG at = 0;
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
            if ((seat.state & voice_wire::kSeatTalking) || audio::SeatTalking(seat.seat)) m.state |= VoiceMember::Talking;
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
    }
    Publish(v);
}
} // namespace

void SetVoiceState(VoiceState state) {
    std::lock_guard<std::mutex> lock(g_lock);
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
    ULONGLONG next_start = 0;
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
            const bool ptt = v.mic == MicMode::PushToTalk;
            const bool allowed = v.mic != MicMode::Muted && lobby;
            audio::SetTransmit(ptt ? allowed && PushToTalkHeld() : allowed, ptt);
            for (const auto &seat : relay.seats) {
                const std::string xuid = std::to_string(seat.xuid);
                const uint8_t st = v.StateOf(xuid);
                const bool silenced = seat.xuid == g_self_xuid.load() || (st & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked))
                                      || (seat.state & (voice_wire::kSeatMutedForLobby | voice_wire::kSeatBlocked));
                audio::SetSilenced(seat.seat, silenced);
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

// A datagram from the relay (fi_overlay_voice): voice to the engine, state to the view.
void VoiceArrived(const uint8_t *data, size_t size) {
    voice_wire::VoiceDown down;
    if (voice_wire::DecodeVoiceDown(data, size, down)) {
        if (audio::Running()) audio::Receive(down.seat, down.seq, down.ts, down.flags, down.opus, down.size);
        return;
    }
    voice_wire::StateDown state;
    if (voice_wire::DecodeStateDown(data, size, state)) {
        std::lock_guard<std::mutex> lock(g_lock);
        g_relay = {true, state.lobby == 1, state.bundle, std::move(state.seats), GetTickCount64()};
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
