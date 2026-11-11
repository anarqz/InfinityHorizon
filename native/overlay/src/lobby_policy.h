// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the lobby tab's decisions (OD-204, T-L3), free of ImGui and Windows so they are tested on their own.
//
// - The lobby tab is one roster pane (OD-204 item 4): the lobby this player is in and its players, never a list, a code or a
//   create form (OD-204 item 2: the game's own menus create, invite and join). Its kind names it: a private official lobby is the
//   Private Lobby, any other one the Official Lobby: players host no lobby (OD-208), so every seat is in an official one.
// - Who may invite from the Private Lobby pane: ONE rule, the current lobby leader (A3, WORLDS_OFFICIAL §12.8b), read from
//   MyWorld.leader and this player's own roster row. An unknown leader (the field is not on the pipe yet, or not said) invites
//   nobody. Replace InviteGateOf, nothing else, when the rule changes.
// - The notice rule (T-L3; fixes T-L6's M1): a notice the launcher marks "always" (a private create's result and its hold, the
//   flagged Hardcore message with its time left) shows in every lobby; every other notice follows OD-192 (none in an official
//   lobby, private ones included). Invite prompts are not notices: they are their own toasts and always show (the launcher holds
//   them back while this player races, INVITE_UX).
// - The invite toasts (0x23): up to three on screen, at most kMaxWaiting in all, a repeated id refreshes its toast, expires 0
//   withdraws it, and a toast past its time slides away without an answer (the server's invite expires on its own). Accepting is
//   a hold of Y (HoldRing); the FRIENDS sidebar's invite mode words each friend's Invite (InviteStateOf, InviteSends).
#pragma once

#include <cstddef>
#include <cstdint>
#include <deque>
#include <string>

#include "ui_frames.h"

namespace fi {

enum class LobbyKind : uint8_t { None, Official, Private };

inline LobbyKind LobbyKindOf(const MyWorld &m) {
    if (m.role == 0) return LobbyKind::None;
    return m.official && m.is_private ? LobbyKind::Private : LobbyKind::Official;
}

// This player's own roster row (friend state 4, "you"), or null.
inline const MemberRow *SelfRow(const MyWorld &m) {
    for (const auto &p : m.members)
        if (p.friend_state == 4) return &p;
    return nullptr;
}

enum class InviteGate : uint8_t {
    Allowed,       // this player is the lobby's current leader
    NotPrivate,    // no Invite friends at all (not in a private lobby)
    LeaderUnknown, // a private lobby whose leader is not known yet: disabled, "Only the lobby leader can invite"
    NotLeader,     // another player leads: disabled, the same words
};

// The one invite rule (A3): the current leader may invite.
inline InviteGate InviteGateOf(const MyWorld &m) {
    if (LobbyKindOf(m) != LobbyKind::Private) return InviteGate::NotPrivate;
    if (m.leader.empty()) return InviteGate::LeaderUnknown;
    const MemberRow *self = SelfRow(m);
    return self && !self->xuid.empty() && self->xuid == m.leader ? InviteGate::Allowed : InviteGate::NotLeader;
}

// OD-210 (CR-S101 §2.11, W14): the Private Lobby pane's "Who can join" (0x28 LobbyJoinModes). Shown in a private lobby once the
// launcher said its value for this very world; only the current leader may change it (the invite rule above, InviteGateOf), everyone
// else sees it read-only. A change applies to new joins only: nobody already in the lobby is removed (kicking stays the leader's).
enum class JoinModesGate : uint8_t {
    Allowed,    // this player leads: A moves to the next offered choice (0x29)
    Hidden,     // not a private lobby, or its value not known yet for this world: no control
    ReadOnly,   // another player leads, or the leader is not known: shown, disabled
};

inline JoinModesGate JoinModesGateOf(const MyWorld &m, const LobbyJoinModesFrame &j) {
    if (LobbyKindOf(m) != LobbyKind::Private || j.world.empty() || j.world != m.world) return JoinModesGate::Hidden;
    return InviteGateOf(m) == InviteGate::Allowed ? JoinModesGate::Allowed : JoinModesGate::ReadOnly;
}

// The choice after the current one, among the offered ones (Everyone, Hardcore only, Azimuth only, then Everyone again; FM5 skips
// Hardcore). The current one when nothing else is offered.
inline uint8_t NextJoinModes(const LobbyJoinModesFrame &j) {
    for (uint8_t step = 1; step <= kJoinModesMax + 1; ++step) {
        const uint8_t c = static_cast<uint8_t>((j.modes + step) % (kJoinModesMax + 1));
        if (j.Offers(c)) return c;
    }
    return j.modes;
}

// The notice rule: true when a toast with these flags shows now.
inline bool ToastShows(bool in_official_lobby, uint8_t flags) { return !in_official_lobby || (flags & kToastAlways) != 0; }

// The invite toasts (0x23, INVITE_UX 2026-11-08): at most kMaxShown on screen (top right, the oldest on top), the rest queued, at
// most kMaxWaiting in all. Each lives for the time the launcher gave it (the invite's answer window), counted from its arrival; a
// toast that goes (answered, withdrawn, past its time) stays kSlideMs as "leaving" so it can slide out, then it is purged.
// OD-223 revised (owner 2026-11-08): the second line of an invite notice (0x23 with notice set: the game is off the screens that take it).
constexpr const char *kInviteNoticeEn = "Go into Multiplayer to accept it";
constexpr const char *kInviteNoticePtBr = "Entre no Multiplayer para aceitar";

class InviteQueue {
public:
    static constexpr size_t kMaxShown = 3, kMaxWaiting = 8;
    static constexpr uint64_t kSlideMs = 320;
    struct Entry {
        InvitePromptFrame prompt;
        uint64_t deadline_ms = 0;
        uint64_t shown_ms = 0;   // when it first came on screen (0: still queued): its slide in
        uint64_t leaving_ms = 0; // when it went (0: live): its slide out
        bool live() const { return leaving_ms == 0; }
    };

    // A prompt from the launcher at `now_ms`: shown or queued; expires 0 withdraws its id. False: dropped (the queue is full).
    bool Offer(const InvitePromptFrame &p, uint64_t now_ms) {
        for (auto &e : entries_) {
            if (!e.live() || e.prompt.id != p.id) continue;
            if (p.expires == 0) Leave(e, now_ms);
            else {
                e.prompt = p;
                e.deadline_ms = now_ms + static_cast<uint64_t>(p.expires) * 1000;
            }
            return true;
        }
        if (p.expires == 0) return true; // nothing of that id: already gone
        if (Waiting() >= kMaxWaiting) return false;
        entries_.push_back({p, now_ms + static_cast<uint64_t>(p.expires) * 1000, 0, 0});
        return true;
    }

    // Every live toast past its time leaves (no answer is sent); a toast that left kSlideMs ago is purged. True when one left.
    bool Expire(uint64_t now_ms) {
        bool left = false;
        for (auto &e : entries_) {
            if (e.live() && now_ms >= e.deadline_ms) {
                Leave(e, now_ms);
                left = true;
            }
        }
        for (auto it = entries_.begin(); it != entries_.end();) {
            if (!it->live() && now_ms >= it->leaving_ms + kSlideMs) it = entries_.erase(it);
            else ++it;
        }
        return left;
    }

    // The first live toast (the one a hold answers), or null.
    const Entry *Current() const {
        for (const auto &e : entries_)
            if (e.live()) return &e;
        return nullptr;
    }
    // Live toasts, shown and queued.
    size_t Waiting() const {
        size_t n = 0;
        for (const auto &e : entries_) n += e.live() ? 1 : 0;
        return n;
    }

    // What to draw at `now_ms`, in order: the first kMaxShown live toasts and every leaving one (the ones that just came on screen
    // get their shown_ms).
    std::vector<Entry> Drawn(uint64_t now_ms) {
        std::vector<Entry> out;
        size_t live = 0;
        for (auto &e : entries_) {
            if (e.live()) {
                if (live++ >= kMaxShown) continue;
                if (e.shown_ms == 0) e.shown_ms = now_ms ? now_ms : 1;
            }
            out.push_back(e);
        }
        return out;
    }

    // Whole seconds left on a toast (rounded up), 0 when past.
    static uint32_t SecondsLeft(const Entry &e, uint64_t now_ms) {
        if (now_ms >= e.deadline_ms) return 0;
        return static_cast<uint32_t>((e.deadline_ms - now_ms + 999) / 1000);
    }
    uint32_t SecondsLeft(uint64_t now_ms) const {
        const Entry *e = Current();
        return e ? SecondsLeft(*e, now_ms) : 0;
    }

    // Whether a hold may answer now: the first live toast is an accept toast (OD-223 revised: a notice takes no hold and no Y).
    bool Holdable() const {
        const Entry *e = Current();
        return e != nullptr && !e->prompt.notice;
    }

    // The player accepted the first live toast: its id (to send with the answer); it leaves and the next one moves up. False: none, or
    // the first one is a notice (it is never answered here).
    bool Answer(std::string *id, uint64_t now_ms = 0) {
        for (auto &e : entries_) {
            if (!e.live()) continue;
            if (e.prompt.notice) return false;
            if (id) *id = e.prompt.id;
            Leave(e, now_ms);
            return true;
        }
        return false;
    }

    void Clear() { entries_.clear(); }

private:
    static void Leave(Entry &e, uint64_t now_ms) { e.leaving_ms = now_ms ? now_ms : 1; }
    std::deque<Entry> entries_;
};

// "Hold (Y) to accept" (INVITE_UX): a ring fills while the button is held, kHoldSeconds to full; released early it drains (kDrainSeconds
// from full to empty). Update returns true exactly once, on the frame the ring completes; after that, and when the ring starts (a toast
// came while Y was already down, as a rewind), the button must be released before it can fill again. A long frame counts as at most
// kMaxStep, so a hitch never completes a hold on its own.
class HoldRing {
public:
    static constexpr float kHoldSeconds = 1.2f, kDrainSeconds = 0.4f, kMaxStep = 0.1f;
    bool Update(bool held, float dt) {
        dt = dt < 0 ? 0 : dt > kMaxStep ? kMaxStep : dt;
        if (!held) {
            armed_ = true;
            progress_ -= dt / kDrainSeconds;
            if (progress_ < 0) progress_ = 0;
            return false;
        }
        if (!armed_) return false;
        progress_ += dt / kHoldSeconds;
        if (progress_ < 1) return false;
        progress_ = 0;
        armed_ = false; // the next toast needs a new press
        return true;
    }
    float Progress() const { return progress_; }
    bool Armed() const { return armed_; }
    // The toasts went (none to accept): empty, and a new press is needed.
    void Reset() {
        progress_ = 0;
        armed_ = false;
    }

private:
    float progress_ = 0;
    bool armed_ = false;
};

// The FRIENDS sidebar's invite mode (INVITE_UX, protocol 12): what one friend row's Invite says. Only Available invites; every other
// state is the row's reason, shown greyed. `sending`: this overlay asked for that friend and the launcher has not answered yet.
enum class InviteRowState : uint8_t { Available, Sending, Invited, InLobby, Offline, Excluded, NoLobby };
inline InviteRowState InviteStateOf(const FriendRowFrame &r, bool have_lobby, bool sending) {
    if (!have_lobby) return InviteRowState::NoLobby;
    if (r.Has(kFriendInLobby)) return InviteRowState::InLobby;
    if (r.Has(kFriendInvited)) return InviteRowState::Invited;
    if (sending) return InviteRowState::Sending;
    if (!r.Has(kFriendOnline)) return InviteRowState::Offline;
    if (r.Has(kFriendExcluded) || !r.Has(kFriendCanInvite)) return InviteRowState::Excluded;
    return InviteRowState::Available;
}

// The invites this overlay asked for (0x2B kind 7) and the launcher has not answered (0x2C kind 7, in order): a second press on the
// same friend sends nothing until then, or until kForgetMs (a lost answer never locks a row).
class InviteSends {
public:
    static constexpr uint64_t kForgetMs = 15000;
    bool Begin(const std::string &id, uint64_t now_ms) {
        Forget(now_ms);
        if (Sending(id, now_ms)) return false;
        sent_.push_back({id, now_ms});
        return true;
    }
    bool Sending(const std::string &id, uint64_t now_ms) const {
        for (const auto &s : sent_)
            if (s.id == id && now_ms < s.at + kForgetMs) return true;
        return false;
    }
    void Answered() {
        if (!sent_.empty()) sent_.pop_front();
    }
    void Clear() { sent_.clear(); }
    size_t Pending() const { return sent_.size(); }

private:
    struct Sent {
        std::string id;
        uint64_t at = 0;
    };
    void Forget(uint64_t now_ms) {
        while (!sent_.empty() && now_ms >= sent_.front().at + kForgetMs) sent_.pop_front();
    }
    std::deque<Sent> sent_;
};

} // namespace fi
