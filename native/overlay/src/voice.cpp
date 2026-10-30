// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's state and its looks (OD-147, docs/launcher/VOICE_CHAT.md §5, §11, §12). The launcher
// holds the truth (the lobby's switch, the mute history, the settings) and sends it as 0x1B VoiceState; the overlay
// shows it and asks for changes with 0x1C VoiceCommand. A change is applied here at once (the button follows the
// press) and the launcher's next VoiceState settles it. No audio here yet (VC-3).
//
// - The microphone is drawn with lines: the embedded icon font has none.
// - Who talks is shown by the game itself (OD-147: the top-right mini leaderboard and the session player list): the
//   hook answers the game's queries from fi_overlay_voice_state, read lock-free from a snapshot of the state.
#include "overlay.h"

#include <algorithm>
#include <atomic>
#include <charconv>
#include <cmath>
#include <mutex>

#include "fi_overlay.h"
#include "style.h"

namespace fi {

using namespace ui;

namespace {
std::mutex g_lock;
VoiceState g_voice;
bool g_dev_seeded = false;

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

// Called with g_lock held, after every change.
void Publish(const VoiceState &v) {
    g_self_bits.store(GameVoiceBits(v, std::string()), std::memory_order_release);
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
    return g_voice;
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

// ---------------------------------------------------------------- the hook's view (fi_overlay.h, OD-147)
extern "C" __declspec(dllexport) uint32_t fi_overlay_voice_state(uint64_t xuid) {
    if (xuid == 0) return fi::g_self_bits.load(std::memory_order_acquire);
    for (const auto &slot : fi::g_slots) {
        if (slot.xuid.load(std::memory_order_acquire) != xuid) continue;
        const uint32_t bits = slot.bits.load(std::memory_order_acquire);
        return slot.xuid.load(std::memory_order_acquire) == xuid ? bits : 0; // rewritten meanwhile: unknown this frame
    }
    return 0;
}

extern "C" __declspec(dllexport) void fi_overlay_voice_mute(uint64_t xuid, int32_t muted) {
    if (xuid == 0 || fi::InPassthrough()) return;
    fi::VoiceCommand(fi::VoiceOp::Mute, std::to_string(xuid), muted ? 1 : 0);
}
