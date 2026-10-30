// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the voice decoder's adaptive complexity (OD-147, VC-3, Windows/Deck parity). One rule on every
// platform, with no platform input at all: every decoder starts at 7 (NoLACE). The playback thread reports, for each
// 20 ms mix frame with decoding in it, the decode cost per 20 ms of audio times the talkers (PLC included; a packet
// decodes 3 or 6 frames at once, so the raw time per mix frame is bursty); only while the p95 of the last ~3 s of such frames is
// over the budget does the level step down to 6 (LACE), then 5 (no OSCE, deep PLC kept). It steps back up after
// 30 s of projected headroom: the cost at the level above is estimated from the ratio measured when it stepped down,
// and must fit in 80 % of the budget. A step down within 60 s of a step up doubles the next wait (up to 10 min), so
// it never flaps. Pure logic, no clock and no Windows calls: unit-tested with injected timings.
#pragma once

#include <algorithm>
#include <array>
#include <cstddef>

namespace fi::audio {

class ComplexityController {
public:
    static constexpr int kTop = 7, kBottom = 5;
    static constexpr size_t kWindow = 150;          // 3 s of 20 ms frames with decoding in them
    static constexpr int kUpFrames = 1500;          // 30 s of headroom
    static constexpr int kUpFramesMax = 30000;      // 10 min
    static constexpr int kFlapFrames = 3000;        // a step down this soon after a step up is a flap

    struct Change {
        int from = 0, to = 0;
        double p95_us = 0, projected_us = 0;
    };

    // 1000 us of decoding per 20 ms of audio = 5 % of one core. NoLACE for the relay's 4 talkers measured ~340 us here
    // (voice_audio_test, Core 7 240H) and is estimated ~740 us on an efficiency core (§12.4): the Deck should stay at 7.
    explicit ComplexityController(double budget_us = 1000.0) : budget_(budget_us) { ratio_.fill(2.0); }

    // One mix frame in which at least one talker decoded or concealed; returns true when the level changed (Last()).
    bool OnFrame(double cost_us) {
        window_[next_] = cost_us;
        next_ = (next_ + 1) % kWindow;
        if (filled_ < kWindow) filled_++;
        since_up_++;
        if (filled_ < kWindow) return false;
        const double p95 = P95();
        if (level_ > kBottom && p95 > budget_) {
            if (++over_ >= static_cast<int>(kWindow)) return Step(-1, p95, p95);
        } else {
            over_ = 0;
        }
        if (level_ < kTop) {
            if (measure_ratio_) { // the first full window at the new level: how much the step saved
                ratio_[level_ + 1] = std::max(1.2, ratio_pre_ / std::max(p95, 1.0));
                measure_ratio_ = false;
            }
            const double projected = p95 * ratio_[level_ + 1];
            if (projected < 0.8 * budget_) {
                if (++headroom_ >= up_frames_) return Step(+1, p95, projected);
            } else {
                headroom_ = 0;
            }
        }
        return false;
    }
    int Level() const { return level_; }
    const Change &Last() const { return last_; }
    int UpWaitFrames() const { return up_frames_; }
    double Budget() const { return budget_; }
    // The p95 of the current window (0 until it is full).
    double CurrentP95() const { return filled_ < kWindow ? 0.0 : P95(); }

private:
    double P95() const {
        std::array<double, kWindow> copy = window_;
        const size_t k = static_cast<size_t>(0.95 * (kWindow - 1));
        std::nth_element(copy.begin(), copy.begin() + static_cast<std::ptrdiff_t>(k), copy.end());
        return copy[k];
    }
    bool Step(int dir, double p95, double projected) {
        last_ = {level_, level_ + dir, p95, projected};
        if (dir < 0) {
            if (since_up_ < kFlapFrames) up_frames_ = std::min(up_frames_ * 2, kUpFramesMax);
            ratio_pre_ = p95;
            measure_ratio_ = true;
        } else {
            since_up_ = 0;
            measure_ratio_ = false;
        }
        level_ += dir;
        filled_ = next_ = 0; // decide again only on frames measured at the new level
        over_ = headroom_ = 0;
        return true;
    }

    double budget_;
    std::array<double, kWindow> window_{};
    std::array<double, kTop + 1> ratio_{}; // ratio_[l]: cost at l over cost at l - 1
    size_t next_ = 0, filled_ = 0;
    int level_ = kTop, over_ = 0, headroom_ = 0, up_frames_ = kUpFrames, since_up_ = kFlapFrames;
    double ratio_pre_ = 0;
    bool measure_ratio_ = false;
    Change last_;
};

} // namespace fi::audio
