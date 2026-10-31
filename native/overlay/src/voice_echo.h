// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's acoustic echo canceller (release 2.0.0; VOICE_CHAT.md §3.2). WebRTC's AEC3 with its
// 100 Hz high-pass, from the standalone WebRTC AudioProcessing module (BSD-3-Clause, see NOTICE), always on. The far
// end is what the speakers play: a WASAPI loopback of the output the voice plays to (the game and the voice mix), or,
// when that loopback is not available, the voice mix itself before it is queued for playback (voice_audio.cpp). Every
// 10 ms of the microphone goes through Capture before the gain, RNNoise and Opus. AEC3's own delay estimator aligns the
// two (the playback queue, the output and the input latencies), so no device latency is assumed.
//
// Threads: Render on the playback or the loopback thread (serialized here), Capture and DelayMs on the capture thread,
// at the same time (AEC3 hands the render audio across with its own queue). One Canceller per engine session.
#pragma once

#include <memory>

namespace fi::audio::echo {

class Canceller {
public:
    static constexpr int kRate = 48000, kBlock = 480; // 10 ms, mono

    Canceller();
    ~Canceller();
    Canceller(const Canceller &) = delete;
    Canceller &operator=(const Canceller &) = delete;

    // False: the library could not be set up (voice then runs without it and says so).
    bool Init();
    // 10 ms of the far end in [-1, 1], as it will be played.
    void Render(const float *far_end);
    // 10 ms of the microphone in [-1, 1], replaced by the echo-cancelled, high-passed signal.
    void Capture(float *mic);
    // The far end's delay in the microphone that AEC3 aligned to, in ms; < 0 until it has found one.
    int DelayMs() const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace fi::audio::echo
