// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
#pragma once

#include <cstdint>
#include <limits>

namespace fi {

class WorldAccessLease {
    uint64_t until_ = 0;

public:
    void Observe(bool allowed, uint64_t now) {
        until_ = allowed && now <= std::numeric_limits<uint64_t>::max() - 10000 ? now + 10000 : 0;
    }

    bool Allowed(uint64_t now) const { return until_ != 0 && now < until_; }
};

} // namespace fi
