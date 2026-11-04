// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the game's PLAYER ACTIONS friend and report items (protocol 5, OD-193 F-2). The hook calls the
// fi_overlay_player_* exports from the game's UI thread: they answer from this cache at once and never wait for the pipe.
// What the launcher must answer goes out as 0x1F PlayerStateQuery / 0x21 PlayerActionRequest (pipe.cpp); its 0x20 / 0x22
// answers land here. Header only, no Windows: tests/player_actions_test.cpp drives it with its own clock and sender.
#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>
#include <mutex>
#include <string>

#include "ui_frames.h"

namespace fi {

class PlayerActionCache {
public:
    static constexpr size_t kStates = 32, kActions = 16;
    static constexpr uint64_t kAskEveryMs = 5000;     // a player's state is asked again at most this often
    static constexpr uint64_t kActionLifetimeMs = 60000; // an action the launcher never answered is forgotten

    // `send(type, body)` queues a frame and says whether it went (false: no launcher connected).
    using Send = std::function<bool(uint8_t, const std::vector<uint8_t> &)>;

    // The state the popup offers now (kPlayerUnknown until the launcher answered once); asks the launcher when unknown or stale.
    uint8_t State(uint64_t xuid, uint64_t now_ms, const Send &send) {
        if (xuid == 0) return kPlayerUnknown;
        bool ask = false;
        uint8_t state = kPlayerUnknown;
        {
            std::lock_guard<std::mutex> lock(lock_);
            Entry *e = Find(xuid);
            if (!e) {
                e = Oldest();
                *e = Entry{xuid, kPlayerUnknown, 0, 0, now_ms};
            }
            e->used_ms = now_ms;
            state = e->state;
            if (e->asked_ms == 0 || now_ms - e->asked_ms >= kAskEveryMs) {
                e->asked_ms = now_ms;
                ask = true;
            }
        }
        if (ask && !send(0x1F, EncodePlayerStateQuery(std::to_string(xuid)))) {
            std::lock_guard<std::mutex> lock(lock_);
            if (Entry *e = Find(xuid)) e->asked_ms = 0; // not sent: ask again on the next call
        }
        return state;
    }

    // A user action: its id (non-zero), or 0 when it cannot go (bad arguments, no launcher).
    uint32_t Act(uint64_t xuid, uint8_t kind, uint8_t reason, uint64_t now_ms, const Send &send) {
        if (xuid == 0 || !PlayerActionValid(kind, reason)) return 0;
        uint32_t id;
        {
            std::lock_guard<std::mutex> lock(lock_);
            id = ++next_id_;
            if (id == 0) id = ++next_id_;
            Action *slot = &actions_[0];
            for (auto &a : actions_) {
                if (a.id == 0 || now_ms - a.since_ms >= kActionLifetimeMs) { slot = &a; break; }
                if (a.since_ms < slot->since_ms) slot = &a;
            }
            *slot = Action{id, xuid, false, 0, kPlayerUnknown, now_ms, kind == kPlayerReport};
        }
        if (!send(0x21, EncodePlayerActionRequest(id, std::to_string(xuid), kind, reason))) {
            std::lock_guard<std::mutex> lock(lock_);
            for (auto &a : actions_) if (a.id == id) a = Action{};
            return 0;
        }
        return id;
    }

    // 1 with the result (the slot is freed), 0 while it waits, -1 for an id unknown here (never made, answered or forgotten).
    int32_t Result(uint32_t id, uint64_t now_ms, uint32_t *result, uint32_t *state) {
        std::lock_guard<std::mutex> lock(lock_);
        for (auto &a : actions_) {
            if (a.id != id || id == 0) continue;
            if (!a.done) {
                if (now_ms - a.since_ms >= kActionLifetimeMs) { a = Action{}; return -1; }
                return 0;
            }
            if (result) *result = a.result;
            if (state) *state = a.state;
            a = Action{};
            return 1;
        }
        return -1;
    }

    // 0x20 from the launcher.
    void OnState(const PlayerStateFrame &f) {
        const uint64_t xuid = Number(f.xuid);
        if (!xuid) return;
        std::lock_guard<std::mutex> lock(lock_);
        if (Entry *e = Find(xuid)) e->state = f.state;
    }

    // 0x22 from the launcher: the action's answer, and the player's state now.
    void OnDone(const PlayerActionDone &d) {
        const uint64_t xuid = Number(d.xuid);
        std::lock_guard<std::mutex> lock(lock_);
        for (auto &a : actions_) {
            if (a.id == d.id && a.xuid == xuid && !a.done) {
                a.done = true;
                a.result = d.result;
                a.state = d.state;
            }
        }
        if (d.state != kPlayerUnknown)
            if (Entry *e = Find(xuid)) e->state = d.state;
    }

    // The pipe closed: no answer comes for what waits, so it failed (2 a report, 10 anything else); states are asked again.
    void Disconnected() {
        std::lock_guard<std::mutex> lock(lock_);
        for (auto &e : states_) e.asked_ms = 0;
        for (auto &a : actions_) {
            if (a.id != 0 && !a.done) {
                a.done = true;
                a.result = a.report ? 2 : kPlayerResultMax;
                a.state = kPlayerUnknown;
            }
        }
    }

    static uint64_t Number(const std::string &xuid) {
        if (!XuidText(xuid)) return 0;
        uint64_t n = 0;
        for (char c : xuid) {
            const uint64_t digit = static_cast<uint64_t>(c - '0');
            if (n > (UINT64_MAX - digit) / 10) return 0;
            n = n * 10 + digit;
        }
        return n;
    }

private:
    struct Entry {
        uint64_t xuid = 0;
        uint8_t state = kPlayerUnknown;
        uint64_t asked_ms = 0;
        uint64_t used_ms = 0;
        uint64_t since_ms = 0;
    };
    struct Action {
        uint32_t id = 0;
        uint64_t xuid = 0;
        bool done = false;
        uint8_t result = 0, state = kPlayerUnknown;
        uint64_t since_ms = 0;
        bool report = false;
    };
    Entry *Find(uint64_t xuid) {
        for (auto &e : states_) if (e.xuid == xuid) return &e;
        return nullptr;
    }
    Entry *Oldest() {
        Entry *oldest = &states_[0];
        for (auto &e : states_) {
            if (e.xuid == 0) return &e;
            if (e.used_ms < oldest->used_ms) oldest = &e;
        }
        return oldest;
    }
    std::mutex lock_;
    Entry states_[kStates];
    Action actions_[kActions];
    uint32_t next_id_ = 0;
};

} // namespace fi
