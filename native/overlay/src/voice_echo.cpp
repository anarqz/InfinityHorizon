// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: AEC3 + high-pass around the WebRTC AudioProcessing module's own classes (voice_echo.h). The same
// order as its AudioProcessing does for one capture and one render channel at 48 kHz: render split into three bands →
// AEC3 analysis; capture full-band high-pass → AEC3 analysis → split → AEC3 → merge. Only these classes are linked
// (no noise suppressor, AGC or AECM: the engine has its own gain and RNNoise after this).
#include "voice_echo.h"

#include <atomic>
#include <cmath>
#include <mutex>
#include <optional>

#include "api/audio/echo_canceller3_config.h"
#include "modules/audio_processing/aec3/echo_canceller3.h"
#include "modules/audio_processing/audio_buffer.h"
#include "modules/audio_processing/high_pass_filter.h"
#include "modules/audio_processing/include/audio_processing.h"

namespace fi::audio::echo {

namespace {
constexpr int kDelayAfterRenderBlocks = 300; // 3 s of far-end speech: AEC3 has aligned by then
}

struct Canceller::Impl {
    webrtc::StreamConfig config{kRate, 1};
    webrtc::AudioBuffer render{kRate, 1, kRate, 1, kRate, 1};
    webrtc::AudioBuffer capture{kRate, 1, kRate, 1, kRate, 1};
    webrtc::HighPassFilter high_pass{kRate, 1};
    webrtc::EchoCanceller3 aec{webrtc::EchoCanceller3Config(), std::nullopt, kRate, 1, 1};
    std::atomic<int> render_active{0}; // far-end blocks with sound in them, capped
    std::mutex render_lock;            // the mix and the loopback may overlap for a block when the source switches
};

Canceller::Canceller() = default;
Canceller::~Canceller() = default;

bool Canceller::Init() {
    try {
        impl_ = std::make_unique<Impl>();
        return true;
    } catch (...) {
        impl_.reset();
        return false;
    }
}

void Canceller::Render(const float *far_end) {
    if (!impl_) return;
    std::lock_guard lock(impl_->render_lock);
    float energy = 0.0f;
    for (int i = 0; i < kBlock; i++) energy += far_end[i] * far_end[i];
    if (energy > kBlock * 1e-6f && impl_->render_active.load(std::memory_order_relaxed) < kDelayAfterRenderBlocks) // > -60 dBFS
        impl_->render_active.fetch_add(1, std::memory_order_relaxed);
    const float *channels[1] = {far_end};
    impl_->render.CopyFrom(channels, impl_->config);
    impl_->render.SplitIntoFrequencyBands();
    impl_->aec.AnalyzeRender(&impl_->render);
}

void Canceller::Capture(float *mic) {
    if (!impl_) return;
    webrtc::AudioBuffer &b = impl_->capture;
    const float *in[1] = {mic};
    b.CopyFrom(in, impl_->config);
    impl_->high_pass.Process(&b, /*use_split_band_data=*/false);
    impl_->aec.AnalyzeCapture(&b);
    b.SplitIntoFrequencyBands();
    impl_->aec.ProcessCapture(&b, /*level_change=*/false);
    b.MergeFrequencyBands();
    float *out[1] = {mic};
    b.CopyTo(impl_->config, out);
}

int Canceller::DelayMs() const {
    if (!impl_ || impl_->render_active.load(std::memory_order_relaxed) < kDelayAfterRenderBlocks) return -1;
    return impl_->aec.GetMetrics().delay_ms;
}

} // namespace fi::audio::echo
