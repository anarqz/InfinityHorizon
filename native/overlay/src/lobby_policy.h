// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the lobby tab's decisions (OD-204, T-L3), free of ImGui and Windows so they are tested on their own.
//
// - The lobby tab is one roster pane (OD-204 item 4): the lobby this player is in and its players, never a list, a code or a
//   create form (OD-204 item 2: the game's own menus create, invite and join). Its kind names it: a private official lobby is the
//   Private Lobby, any other official one the Official Lobby, a player-hosted one (the launcher may still hold one until T-L8) a
//   user lobby.
// - Who may invite from the Private Lobby pane: ONE rule, the current lobby leader (A3, WORLDS_OFFICIAL §12.8b), read from
//   MyWorld.leader and this player's own roster row. An unknown leader (the field is not on the pipe yet, or not said) invites
//   nobody. Replace InviteGateOf, nothing else, when the rule changes.
// - The notice rule (T-L3; fixes T-L6's M1): a notice the launcher marks "always" (a private create's result and its hold, the
//   flagged Hardcore message with its time left) shows in every lobby; every other notice follows OD-192 (none in an official
//   lobby, private ones included). Invite prompts are not notices: they are their own modal frame and always show.
// - The invite prompts (0x23): one shown at a time, at most kMaxWaiting waiting, a repeated id refreshes its prompt, expires 0
//   withdraws it, and a prompt past its time goes away without an answer (the server's invite expires on its own).
#pragma once

#include <cstddef>
#include <cstdint>
#include <deque>
#include <string>

#include "ui_frames.h"

namespace fi {

enum class LobbyKind : uint8_t { None, User, Official, Private };

inline LobbyKind LobbyKindOf(const MyWorld &m) {
    if (m.role == 0) return LobbyKind::None;
    if (m.official && m.is_private) return LobbyKind::Private;
    if (m.official) return LobbyKind::Official;
    return LobbyKind::User;
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

// The choice after the current one, among the offered ones (Everyone, Hardcore only, Noob only, then Everyone again; FM5 skips
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

class InviteQueue {
public:
    static constexpr size_t kMaxWaiting = 4;
    static constexpr int kAccept = 0, kDecline = 1;
    struct Entry {
        InvitePromptFrame prompt;
        uint64_t deadline_ms = 0;
    };

    // A prompt from the launcher at `now_ms`: shown or queued; expires 0 withdraws its id. False: dropped (the queue is full).
    bool Offer(const InvitePromptFrame &p, uint64_t now_ms) {
        for (auto it = entries_.begin(); it != entries_.end(); ++it) {
            if (it->prompt.id != p.id) continue;
            if (p.expires == 0) {
                if (it == entries_.begin()) focus_ = kDecline;
                entries_.erase(it);
            } else {
                it->prompt = p;
                it->deadline_ms = now_ms + static_cast<uint64_t>(p.expires) * 1000;
            }
            return true;
        }
        if (p.expires == 0) return true; // nothing of that id: already gone
        if (entries_.size() >= kMaxWaiting) return false;
        entries_.push_back({p, now_ms + static_cast<uint64_t>(p.expires) * 1000});
        return true;
    }

    // Drops every prompt past its time (no answer is sent). True when the shown one went.
    bool Expire(uint64_t now_ms) {
        bool shown = false;
        for (auto it = entries_.begin(); it != entries_.end();) {
            if (now_ms >= it->deadline_ms) {
                if (it == entries_.begin()) { shown = true; focus_ = kDecline; }
                it = entries_.erase(it);
            } else {
                ++it;
            }
        }
        return shown;
    }

    const Entry *Current() const { return entries_.empty() ? nullptr : &entries_.front(); }
    size_t Waiting() const { return entries_.size(); }

    // Whole seconds left on the shown prompt (rounded up), 0 when none.
    uint32_t SecondsLeft(uint64_t now_ms) const {
        const Entry *e = Current();
        if (!e || now_ms >= e->deadline_ms) return 0;
        return static_cast<uint32_t>((e->deadline_ms - now_ms + 999) / 1000);
    }

    // The shown prompt's two buttons: 0 Accept, 1 Decline (focused first: an A meant for the guide never accepts).
    int Focus() const { return focus_; }
    void Move(int to) { focus_ = to <= 0 ? 0 : 1; }

    // The player answered the shown prompt: its id (to send with the answer) and the next one is shown. False: none shown.
    bool Answer(std::string *id) {
        if (entries_.empty()) return false;
        if (id) *id = entries_.front().prompt.id;
        entries_.pop_front();
        focus_ = kDecline;
        return true;
    }

    void Clear() {
        entries_.clear();
        focus_ = kDecline;
    }

private:
    std::deque<Entry> entries_;
    int focus_ = kDecline;
};

// The friend picker (T-L4, 0x25): what it lists and which row has the focus. Only the launcher opens it (flag open: the game's
// Invite friends or the pane's button answered with a picker); a picker without the flag only refreshes an open one. Only an
// available friend may be chosen; choosing marks that row invited at once (no double send) until the launcher's refresh.
class FriendPickerModel {
public:
    void Set(const FriendPickerFrame &frame) {
        if (!frame.open() && !shown_) return; // a refresh for a closed picker
        shown_ = true;
        status_ = frame.status;
        rows_ = frame.friends;
        if (frame.open()) focus_ = FirstAvailable();
        Clamp();
    }
    void Close() { shown_ = false; }
    void Clear() {
        shown_ = false;
        rows_.clear();
        focus_ = 0;
    }
    bool Shown() const { return shown_; }
    uint8_t Status() const { return status_; }
    const std::vector<PickerFriend> &Rows() const { return rows_; }
    int Focus() const { return focus_; }
    void Move(int delta) {
        focus_ += delta;
        Clamp();
    }
    void FocusRow(int row) {
        focus_ = row;
        Clamp();
    }
    // The focused row's XUID when it may be invited (the row is then marked invited); empty otherwise.
    std::string Choose() {
        if (!shown_ || focus_ < 0 || static_cast<size_t>(focus_) >= rows_.size()) return {};
        PickerFriend &row = rows_[static_cast<size_t>(focus_)];
        if (row.state != kPickerAvailable) return {};
        row.state = kPickerInvited;
        return row.xuid;
    }

private:
    int FirstAvailable() const {
        for (size_t i = 0; i < rows_.size(); ++i)
            if (rows_[i].state == kPickerAvailable) return static_cast<int>(i);
        return 0;
    }
    void Clamp() {
        if (focus_ >= static_cast<int>(rows_.size())) focus_ = static_cast<int>(rows_.size()) - 1;
        if (focus_ < 0) focus_ = 0;
    }
    bool shown_ = false;
    uint8_t status_ = kPickerList;
    std::vector<PickerFriend> rows_;
    int focus_ = 0;
};

} // namespace fi
