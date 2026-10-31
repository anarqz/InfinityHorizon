// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's volume sliders (VOICE_CHAT.md §15). Every slider is 1..100, 100 by default: the voice
// chat volume (the mix of every other player), this player's microphone volume and each other player's volume. A
// slider only lowers (100 is unity gain, never a boost), so the levels the engine was tuned for stay the ceiling.
// Pure, so the private tests check the curve.
#pragma once

#include <algorithm>
#include <cstdint>

namespace fi::audio {

constexpr int kMinVolume = 1, kMaxVolume = 100, kDefaultVolume = 100;

// A stored or received volume made valid: 0 (unset, or a damaged value) and anything past 100 read as the default.
constexpr uint8_t ValidVolume(int volume) {
    return static_cast<uint8_t>(volume < kMinVolume || volume > kMaxVolume ? kDefaultVolume : volume);
}

// One Left/Right step on a slider: to the next multiple of 5 in that direction, kept on the slider (100 up stays 100;
// 5 down is 1, and 1 up is 5). A value set by the mouse (say 37) steps to 40 or 35.
constexpr int kVolumeStep = 5;
constexpr uint8_t StepVolume(int volume, int direction) {
    const int v = std::clamp(volume, kMinVolume, kMaxVolume);
    const int next = direction > 0 ? (v / kVolumeStep + 1) * kVolumeStep : direction < 0 ? (v - 1) / kVolumeStep * kVolumeStep : v;
    return static_cast<uint8_t>(std::clamp(next, kMinVolume, kMaxVolume));
}

// The slider's value at a fraction 0..1 of its track (the mouse): 0 is 1, 1 is 100.
constexpr uint8_t VolumeAtFraction(float fraction) {
    const float f = !(fraction > 0.0f) ? 0.0f : fraction > 1.0f ? 1.0f : fraction; // NaN reads as 0
    return static_cast<uint8_t>(kMinVolume + static_cast<int>(f * (kMaxVolume - kMinVolume) + 0.5f));
}

// The linear gain of a slider position: the square law, (v / 100)^2. Loudness follows the ear: 50 is a quarter of the
// amplitude (-12 dB), 25 is -24 dB, 10 is -40 dB, 1 is -80 dB (just above silence; muting is its own control).
// Out-of-range input is clamped, so the gain is always in [1e-4, 1].
constexpr float VolumeGain(int volume) {
    const float v = static_cast<float>(std::clamp(volume, kMinVolume, kMaxVolume)) / kMaxVolume;
    return v * v;
}

} // namespace fi::audio
