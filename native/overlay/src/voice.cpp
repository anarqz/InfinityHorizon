// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's state and its looks (OD-147, docs/launcher/VOICE_CHAT.md §5, §11). The launcher
// holds the truth (the lobby's switch, the mute history, the settings) and sends it as 0x1B VoiceState; the overlay
// shows it and asks for changes with 0x1C VoiceCommand. A change is applied here at once (the button follows the
// press) and the launcher's next VoiceState settles it. No audio here yet (VC-3).
//
// - The microphone is drawn with lines: the embedded icon font has none.
// - Over the game, while the guide is closed, each player talking now gets a chip above the minimap (bottom left):
//   a microphone and the gamertag. This is the fallback until the game's own HUD can show its talking icon.
#include "overlay.h"

#include <algorithm>
#include <cmath>
#include <mutex>
#include <unordered_map>

#include "style.h"

namespace fi {

using namespace ui;

namespace {
std::mutex g_lock;
VoiceState g_voice;
bool g_dev_seeded = false;

// How long a chip stays after its player stops (the game's GameChat/MinIsTalkingSeconds idea), and its fades.
constexpr float kTalkHold = 0.6f, kFadeIn = 0.12f, kFadeOut = 0.25f;
constexpr size_t kMaxChips = 4;

struct Chip {
    std::string gamertag;
    float shown = 0;  // 0..1
    float quiet = 0;  // seconds since its last talking state
    bool talking = false;
};
std::unordered_map<std::string, Chip> g_chips; // render thread
std::vector<std::string> g_chip_order;         // first talker first

// The development host: a lobby where the second player talks on and off, so every look can be seen.
void DevSeed(VoiceState &v) {
    if (g_dev_seeded) return;
    g_dev_seeded = true;
    v.flags = VoiceState::LobbyOn | VoiceState::LobbyKnown | VoiceState::MayChange | VoiceState::Enabled;
    v.mic = MicMode::Open;
    // the dev host's own lobby (worlds.cpp): "1" is this player, never in the list
    v.members = {{"2", "Synthetic#00230", VoiceMember::Talking}, {"3", "Synthetic#00042", 0}};
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
}
} // namespace

void SetVoiceState(VoiceState state) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_voice = std::move(state);
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

void DrawTalkers(ImDrawList *dl, float width, float height, float dt) {
    const VoiceState v = CurrentVoice();
    const bool hearing = v.lobby_on() && (v.flags & VoiceState::Enabled);
    for (auto &[xuid, chip] : g_chips) chip.talking = false;
    if (hearing) {
        for (const auto &m : v.members) {
            const bool heard = (m.state & VoiceMember::Talking) && !(m.state & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked));
            if (!heard) continue;
            auto [it, fresh] = g_chips.try_emplace(m.xuid);
            if (fresh) g_chip_order.push_back(m.xuid);
            it->second.gamertag = m.gamertag;
            it->second.talking = true;
            it->second.quiet = 0;
        }
    }
    for (auto it = g_chip_order.begin(); it != g_chip_order.end();) {
        Chip &chip = g_chips[*it];
        if (!chip.talking) chip.quiet += dt;
        const bool on = chip.talking || chip.quiet < kTalkHold;
        chip.shown = on ? std::min(1.0f, chip.shown + dt / kFadeIn) : std::max(0.0f, chip.shown - dt / kFadeOut);
        if (!on && chip.shown <= 0) {
            g_chips.erase(*it);
            it = g_chip_order.erase(it);
        } else {
            ++it;
        }
    }
    if (g_chip_order.empty()) return;

    // Above the game's minimap (bottom left at 16:9: the radar's top edge is about 72 % down): newest at the bottom.
    const float h = 34 * s, gap = 6 * s;
    const float x = std::round(width * 0.038f);
    float y = std::round(height * 0.715f) - h;
    const double t = ImGui::GetTime();
    size_t drawn = 0;
    for (auto it = g_chip_order.rbegin(); it != g_chip_order.rend() && drawn < kMaxChips; ++it, ++drawn) {
        const Chip &chip = g_chips[*it];
        const float a = EaseOutCubic(chip.shown);
        const ImVec2 ts = Measure(f.body, chip.gamertag.c_str());
        const float w = std::min(h + 10 * s + ts.x + 16 * s, width * 0.3f);
        const ImVec2 p0(x - (1 - a) * 18 * s, y), p1(p0.x + w, y + h);
        dl->AddRectFilled(p0, p1, Rgba(0, 0, 0, 0.55f * a), h / 2);
        // the disc behind the microphone pulses while the voice is live
        const float pulse = chip.talking ? 0.5f + 0.5f * static_cast<float>(std::sin(t * 9.0)) : 0.0f;
        const ImVec2 dc(p0.x + h / 2, y + h / 2);
        dl->AddCircleFilled(dc, h / 2 - 3 * s + pulse * 1.5f * s, Rgba(230, 0, 126, (0.85f + 0.15f * pulse) * a), 32);
        DrawMic(dl, dc, h * 0.56f, Rgba(255, 255, 255, a), false);
        dl->PushClipRect(ImVec2(dc.x + h / 2, y), ImVec2(p1.x - 8 * s, p1.y), true);
        TextAt(dl, f.body, ImVec2(dc.x + h / 2 + 8 * s, y + (h - ts.y) / 2), kText, chip.gamertag.c_str(), a);
        dl->PopClipRect();
        y -= h + gap;
    }
}

} // namespace fi
