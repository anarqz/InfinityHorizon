// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// FIFO lifetime for the render thread; producers only enqueue. No sound on receipt.
#pragma once
#include <algorithm>
#include <deque>
#include <mutex>
#include <optional>
#include <utility>

namespace fi {
template<class T> class ToastSchedule {
    std::mutex lock_;
    std::deque<T> pending_;
    std::optional<T> current_;
    float gap_ = 0;
public:
    static constexpr float Rise = 0.28f, Open = 0.32f, Leave = 0.32f, Gap = 0.35f;
    void Push(T toast) {
        std::lock_guard<std::mutex> lock(lock_);
        pending_.push_back(std::move(toast));
    }
    // True exactly when a new toast becomes visible; the caller plays its sound once.
    bool Tick(float dt) {
        dt = std::clamp(dt, 0.0f, 0.1f); // a stalled frame must not skip queued notifications
        if (current_) {
            current_->age += dt;
            if (current_->age >= Rise + Open + current_->hold + Leave) {
                current_.reset();
                gap_ = Gap;
            }
            return false;
        }
        if (gap_ > 0) { gap_ = std::max(0.0f, gap_ - dt); return false; }
        std::lock_guard<std::mutex> lock(lock_);
        if (pending_.empty()) return false;
        current_ = std::move(pending_.front());
        pending_.pop_front();
        current_->age = 0;
        return true;
    }
    const std::optional<T>& Current() const { return current_; }
};
}
