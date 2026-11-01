// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's small signal pieces (OD-154, VC-3): the capture format converter (any channel count,
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

enum class SampleKind : uint8_t { Float32, Int16, Int24, Int32, UInt8, Float64 };

struct DeviceFormat {
    uint32_t rate = 48000;
    uint16_t channels = 1;
    SampleKind kind = SampleKind::Float32;
    uint16_t container = 4; // bytes per sample in the buffer (Int24 may sit in 3 or 4)
    bool operator==(const DeviceFormat &) const = default;
};

inline const char *KindName(SampleKind k) {
    switch (k) {
    case SampleKind::Float32: return "float32";
    case SampleKind::Int16: return "int16";
    case SampleKind::Int24: return "int24";
    case SampleKind::Int32: return "int32";
    case SampleKind::UInt8: return "uint8";
    case SampleKind::Float64: return "float64";
    }
    return "?";
}

// Windowed-sinc resampler between any two rates, 48 kHz out by default (Blackman window, 32 taps, 1024-phase table
// with linear interpolation). The cut-off follows the lower of the two rates, so 96/192 kHz microphones do not alias.
class Resampler {
public:
    void Reset(uint32_t in_rate, uint32_t out_rate = 48000) {
        in_rate_ = in_rate;
        out_rate_ = out_rate;
        step_ = static_cast<double>(in_rate) / static_cast<double>(out_rate);
        history_.assign(kHalf, 0.0f); // leading zeros: the first output sits at history_[kHalf]
        pos_ = kHalf;
        const double cutoff = 0.5 * std::min(1.0, static_cast<double>(out_rate) / in_rate) * 0.92; // of the input rate
        table_.resize(2 * kHalf * kPhases + 1);
        for (size_t i = 0; i < table_.size(); i++) {
            const double t = static_cast<double>(i) / kPhases - kHalf; // -kHalf..kHalf input samples
            const double x = 2.0 * cutoff * t;
            const double sinc = std::abs(x) < 1e-9 ? 1.0 : std::sin(kPi * x) / (kPi * x);
            const double w = 0.42 + 0.5 * std::cos(kPi * t / kHalf) + 0.08 * std::cos(2.0 * kPi * t / kHalf);
            table_[i] = static_cast<float>(2.0 * cutoff * sinc * w);
        }
    }
    bool Passthrough() const { return in_rate_ == out_rate_; }
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
    uint32_t in_rate_ = 48000, out_rate_ = 48000;
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
            case SampleKind::UInt8: v = (static_cast<int>(*p) - 128) / 128.0; break;
            case SampleKind::Float64: { double x; std::memcpy(&x, p, 8); v = std::isfinite(x) ? x : 0.0; break; }
            }
            acc += v;
        }
        out[i] = static_cast<float>(std::clamp(acc / f.channels, -1.0, 1.0));
    }
}

// One sample in -1..1 written in the device's format (the playback side's converter).
inline void WriteSample(const DeviceFormat &f, uint8_t *p, float v) {
    const double x = std::clamp(static_cast<double>(std::isfinite(v) ? v : 0.0f), -1.0, 1.0);
    switch (f.kind) {
    case SampleKind::Float32: { const float y = static_cast<float>(x); std::memcpy(p, &y, 4); break; }
    case SampleKind::Float64: std::memcpy(p, &x, 8); break;
    case SampleKind::Int16: { const int16_t y = static_cast<int16_t>(std::lround(std::clamp(x * 32768.0, -32768.0, 32767.0))); std::memcpy(p, &y, 2); break; }
    case SampleKind::Int24: {
        const int32_t y = static_cast<int32_t>(std::lround(std::clamp(x * 8388608.0, -8388608.0, 8388607.0)));
        std::memset(p, 0, f.container);
        uint8_t *q = p + (f.container - 3);
        q[0] = static_cast<uint8_t>(y); q[1] = static_cast<uint8_t>(y >> 8); q[2] = static_cast<uint8_t>(y >> 16);
        break;
    }
    case SampleKind::Int32: { const int32_t y = static_cast<int32_t>(std::llround(std::clamp(x * 2147483648.0, -2147483648.0, 2147483647.0))); std::memcpy(p, &y, 4); break; }
    case SampleKind::UInt8: *p = static_cast<uint8_t>(std::clamp(std::lround(x * 128.0) + 128, 0L, 255L)); break;
    }
}

// The standard rate nearest to a measured one (by ratio).
inline uint32_t NearestStandardRate(double measured) {
    static const uint32_t kRates[] = {8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000, 88200, 96000, 176400, 192000};
    uint32_t best = kRates[0];
    double best_d = 1e9;
    for (const uint32_t r : kRates) {
        const double d = std::abs(std::log(measured / r));
        if (d < best_d) { best_d = d; best = r; }
    }
    return best;
}

// Watches the rate a capture stream really delivers against the rate its format says (a Bluetooth headset that
// switched profile under an open stream, or a driver that relabels). After 1 s of warm-up, 2 s windows; two windows
// in a row more than 20 % off: Add returns the measured rate snapped to a standard one (else 0). Frames the device may
// have dropped are not a rate: a buffer marked discontinuous, or one read more than kMaxGapMs after the previous (the
// worker was held up long enough for the device buffer to overflow), restarts the current window.
class RateWatch {
public:
    static constexpr int64_t kMaxGapMs = 80; // under the capture's 100 ms device buffer
    explicit RateWatch(uint32_t nominal = 48000) : nominal_(nominal) {}
    uint32_t Add(size_t frames, int64_t now_ms, bool discontinuity = false) {
        const bool gap = discontinuity || (last_ms_ >= 0 && now_ms - last_ms_ > kMaxGapMs);
        last_ms_ = now_ms;
        if (start_ms_ < 0) { start_ms_ = now_ms; return 0; }
        if (window_ms_ < 0) {
            if (now_ms - start_ms_ >= 1000) { window_ms_ = now_ms; count_ = 0; } // this buffer was filled before the window
            return 0;
        }
        if (gap) { window_ms_ = now_ms; count_ = 0; return 0; } // the strikes so far stand
        count_ += frames;
        const int64_t span = now_ms - window_ms_;
        if (span < 2000) return 0;
        const double measured = static_cast<double>(count_) * 1000.0 / static_cast<double>(span);
        window_ms_ = now_ms;
        count_ = 0;
        last_ = measured;
        strikes_ = std::abs(measured / nominal_ - 1.0) > 0.2 ? strikes_ + 1 : 0;
        return strikes_ >= 2 ? NearestStandardRate(measured) : 0;
    }
    double LastMeasured() const { return last_; }

private:
    uint32_t nominal_;
    int64_t start_ms_ = -1, window_ms_ = -1, last_ms_ = -1;
    uint64_t count_ = 0;
    int strikes_ = 0;
    double last_ = 0.0;
};

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
