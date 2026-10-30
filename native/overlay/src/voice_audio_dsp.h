// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's small signal pieces (OD-147, VC-3): the capture format converter (any channel count,
// rate and sample type to 48 kHz mono float), the 80 Hz high-pass and the automatic gain (VOICE_CHAT.md §3.2). No
// allocation after construction except the converter's history, no Windows calls: unit-tested on their own.
#pragma once

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <vector>

namespace fi::audio::dsp {

constexpr double kPi = 3.14159265358979323846;

enum class SampleKind : uint8_t { Float32, Int16, Int24, Int32 };

struct DeviceFormat {
    uint32_t rate = 48000;
    uint16_t channels = 1;
    SampleKind kind = SampleKind::Float32;
    uint16_t container = 4; // bytes per sample in the buffer (Int24 may sit in 3 or 4)
    bool operator==(const DeviceFormat &) const = default;
};

// Windowed-sinc resampler from any rate to 48 kHz (Blackman window, 32 taps, 1024-phase table with linear
// interpolation). The cut-off follows the lower of the two rates, so 96/192 kHz microphones do not alias.
class Resampler {
public:
    void Reset(uint32_t in_rate) {
        in_rate_ = in_rate;
        step_ = static_cast<double>(in_rate) / 48000.0;
        history_.assign(kHalf, 0.0f); // leading zeros: the first output sits at history_[kHalf]
        pos_ = kHalf;
        const double cutoff = 0.5 * std::min(1.0, 48000.0 / in_rate) * 0.92; // of the input rate
        table_.resize(2 * kHalf * kPhases + 1);
        for (size_t i = 0; i < table_.size(); i++) {
            const double t = static_cast<double>(i) / kPhases - kHalf; // -kHalf..kHalf input samples
            const double x = 2.0 * cutoff * t;
            const double sinc = std::abs(x) < 1e-9 ? 1.0 : std::sin(kPi * x) / (kPi * x);
            const double w = 0.42 + 0.5 * std::cos(kPi * t / kHalf) + 0.08 * std::cos(2.0 * kPi * t / kHalf);
            table_[i] = static_cast<float>(2.0 * cutoff * sinc * w);
        }
    }
    bool Passthrough() const { return in_rate_ == 48000; }
    // Appends `n` input samples and writes the outputs they complete to `out` (cleared first).
    void Process(const float *in, size_t n, std::vector<float> &out) {
        out.clear();
        if (Passthrough()) { out.assign(in, in + n); return; }
        history_.insert(history_.end(), in, in + n);
        while (pos_ + kHalf < static_cast<double>(history_.size())) {
            const long i0 = static_cast<long>(pos_);
            double acc = 0.0;
            for (long k = i0 - kHalf + 1; k <= i0 + kHalf; k++) {
                const double d = (pos_ - k + kHalf) * kPhases; // 0..2*kHalf*kPhases
                const size_t j = static_cast<size_t>(d);
                const double f = d - j;
                const float h = j + 1 < table_.size() ? static_cast<float>(table_[j] * (1.0 - f) + table_[j + 1] * f) : table_[j];
                acc += h * history_[static_cast<size_t>(k)];
            }
            out.push_back(static_cast<float>(acc));
            pos_ += step_;
        }
        const long keep_from = static_cast<long>(pos_) - kHalf; // samples before this are never read again
        if (keep_from > 0) {
            history_.erase(history_.begin(), history_.begin() + keep_from);
            pos_ -= keep_from;
        }
    }

private:
    static constexpr long kHalf = 16;
    static constexpr long kPhases = 1024;
    uint32_t in_rate_ = 48000;
    double step_ = 1.0, pos_ = 0.0;
    std::vector<float> history_, table_;
};

// One device buffer (interleaved) to mono float in -1..1: channels are averaged.
inline void ToMono(const DeviceFormat &f, const uint8_t *data, size_t frames, std::vector<float> &out) {
    out.resize(frames);
    const size_t stride = static_cast<size_t>(f.container) * f.channels;
    for (size_t i = 0; i < frames; i++) {
        double acc = 0.0;
        for (uint16_t c = 0; c < f.channels; c++) {
            const uint8_t *p = data + i * stride + static_cast<size_t>(c) * f.container;
            double v = 0.0;
            switch (f.kind) {
            case SampleKind::Float32: { float x; std::memcpy(&x, p, 4); v = std::isfinite(x) ? x : 0.0f; break; }
            case SampleKind::Int16: { int16_t x; std::memcpy(&x, p, 2); v = x / 32768.0; break; }
            case SampleKind::Int24: { // the top three bytes of the container hold the sample
                const uint8_t *q = p + (f.container - 3);
                const int32_t x = static_cast<int32_t>((static_cast<uint32_t>(q[0]) << 8) | (static_cast<uint32_t>(q[1]) << 16) | (static_cast<uint32_t>(q[2]) << 24)) >> 8;
                v = x / 8388608.0;
                break;
            }
            case SampleKind::Int32: { int32_t x; std::memcpy(&x, p, 4); v = x / 2147483648.0; break; }
            }
            acc += v;
        }
        out[i] = static_cast<float>(std::clamp(acc / f.channels, -1.0, 1.0));
    }
}

// 2nd-order Butterworth high-pass (RBJ), 80 Hz at 48 kHz.
class HighPass {
public:
    HighPass() {
        const double w0 = 2.0 * kPi * 80.0 / 48000.0, alpha = std::sin(w0) / (2.0 * 0.70710678118654752);
        const double a0 = 1.0 + alpha;
        b0_ = (1.0 + std::cos(w0)) / 2.0 / a0;
        b1_ = -(1.0 + std::cos(w0)) / a0;
        b2_ = b0_;
        a1_ = -2.0 * std::cos(w0) / a0;
        a2_ = (1.0 - alpha) / a0;
    }
    float Process(float x) {
        const double y = b0_ * x + b1_ * x1_ + b2_ * x2_ - a1_ * y1_ - a2_ * y2_;
        x2_ = x1_; x1_ = x; y2_ = y1_; y1_ = y;
        if (std::abs(y1_) < 1e-15) y1_ = 0.0; // keep the state out of denormals in digital silence
        if (std::abs(y2_) < 1e-15) y2_ = 0.0;
        return static_cast<float>(y);
    }

private:
    double b0_, b1_, b2_, a1_, a2_, x1_ = 0, x2_ = 0, y1_ = 0, y2_ = 0;
};

// Automatic gain to a fixed speech level (§3.2): the level follows only 10 ms blocks the noise suppressor called
// voice (the previous block's decision), ~200 ms; the gain moves 0.5 dB per block within -12..+24 dB; instant peak
// limit. Samples are in int16 scale (what RNNoise expects).
class Agc {
public:
    explicit Agc(double target_dbfs = -20.0) : target_(target_dbfs), level_(target_dbfs) {}
    // Applies the gain to `block` (n samples) in place; `prev_voice` is the noise suppressor's last probability.
    void Process(float *block, size_t n, float prev_voice, float threshold) {
        double e = 0.0, peak = 0.0;
        for (size_t i = 0; i < n; i++) { e += static_cast<double>(block[i]) * block[i]; peak = std::max(peak, static_cast<double>(std::abs(block[i]))); }
        const double rms_db = 10.0 * std::log10(e / n + 1e-9) - 20.0 * std::log10(32767.0);
        if (prev_voice >= threshold && rms_db > -60.0) level_ = 0.95 * level_ + 0.05 * rms_db;
        const double want = std::clamp(target_ - level_, -12.0, 24.0);
        gain_db_ += std::clamp(want - gain_db_, -0.5, 0.5);
        double g = std::pow(10.0, gain_db_ / 20.0);
        if (peak * g > 30000.0) g = 30000.0 / peak;
        for (size_t i = 0; i < n; i++) block[i] = static_cast<float>(block[i] * g);
    }
    double GainDb() const { return gain_db_; }

private:
    double target_, level_, gain_db_ = 0.0;
};

} // namespace fi::audio::dsp
