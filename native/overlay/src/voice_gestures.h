// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's controller gestures while driving (OD-154, VOICE_CHAT.md §5). Header only, no
// Windows, so it is tested on its own; input.cpp feeds it once per frame with the guide closed.
//
// - R3 held = push-to-talk, only in push-to-talk mode. R3 still reaches the game (the owner chose it knowing that).
// - RB held first, then R3 = the next mic mode (Open -> Muted -> Push-to-talk -> Open). That R3 press is spent on the
//   change: it never talks, even in push-to-talk mode, until R3 is let go. R3 alone never changes the mode, and the
//   LB + RB guide chord is not touched (it needs both shoulders, this needs RB and R3).
#pragma once

namespace fi {

struct VoiceGestures {
    struct Result {
        bool talk = false;  // push-to-talk is held now (meaningful in push-to-talk mode)
        bool cycle = false; // this frame: go to the next mic mode
    };

    // rb, r3: held now; ptt: the mic mode is push-to-talk.
    Result Update(bool rb, bool r3, bool ptt) {
        Result out;
        if (r3 && !r3_was) {
            spent = rb_was && rb; // RB was already down before R3: a mode change
            out.cycle = spent;
        }
        if (!r3) spent = false;
        out.talk = ptt && r3 && !spent;
        rb_was = rb;
        r3_was = r3;
        return out;
    }

    void Reset() { *this = VoiceGestures{}; }

    bool rb_was = false, r3_was = false, spent = false;
};

} // namespace fi
