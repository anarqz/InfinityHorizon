// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: which capture endpoint voice chat may use (VOICE_CHAT.md §14.2). Never a monitor or loopback:
// with no microphone, PulseAudio/PipeWire's only source is the output's monitor ("Monitor of ...", "*.monitor"), and
// Wine lists it as an ordinary input; capturing it would send the game's own sound to the lobby. The same goes for
// Windows' "Stereo Mix"-style inputs and a render endpoint used for capture (WASAPI loopback). No acceptable input:
// voice runs listen-only. The player may choose the microphone and the output (§15): the lists and the fallback to the
// system default are here too. Pure (no COM), so the private tests check it with a fake device list.
#pragma once

#include <cwctype>
#include <string>
#include <vector>

namespace fi::audio::devices {

struct Candidate {
    std::wstring id;    // IMMDevice::GetId
    std::wstring name;  // PKEY_Device_FriendlyName (Wine: the PulseAudio/PipeWire description)
    bool render = false; // the endpoint's data flow is render, or its id is a render endpoint's: loopback
};

inline std::wstring Lower(const std::wstring &s) {
    std::wstring out(s);
    for (wchar_t &c : out) c = static_cast<wchar_t>(std::towlower(c));
    return out;
}

// True: the endpoint carries what the PC plays, not a microphone.
inline bool IsMonitorOrLoopback(const Candidate &c) {
    if (c.render) return true;
    static const wchar_t *const kMarks[] = {L".monitor", L"monitor of", L"loopback", L"stereo mix", L"what u hear",
                                            L"wave out mix", L"what you hear"};
    const std::wstring name = Lower(c.name), id = Lower(c.id);
    for (const wchar_t *mark : kMarks)
        if (name.find(mark) != std::wstring::npos || id.find(mark) != std::wstring::npos) return true;
    return false;
}

// The endpoint to capture from: the default communications input if it is a microphone, else the first microphone in
// the system's order; -1 when there is none (listen-only). default_index < 0: there is no default.
inline int PickCapture(const std::vector<Candidate> &list, int default_index) {
    if (default_index >= 0 && default_index < static_cast<int>(list.size()) && !IsMonitorOrLoopback(list[default_index]))
        return default_index;
    for (size_t i = 0; i < list.size(); i++)
        if (!IsMonitorOrLoopback(list[i])) return static_cast<int>(i);
    return -1;
}

// ---------------------------------------------------------------- the player's choice (VOICE_CHAT.md §15)
// The settings list a microphone and an output by endpoint id; empty is "System default". The overlay lists the choices,
// the launcher keeps the pick (and its name, to say which one went missing), the engine opens it.

// One entry of the overlay's device lists. An empty id is "System default".
struct Choice {
    std::wstring id, name;
};

// What the player may pick: "System default" first, then every active endpoint in the system's order; for capture never
// a monitor or loopback (they are not listed at all, so they cannot be chosen). A nameless endpoint shows its id.
inline std::vector<Choice> Choices(const std::vector<Candidate> &list, bool capture) {
    std::vector<Choice> out{Choice{}};
    for (const Candidate &c : list)
        if (!capture || !IsMonitorOrLoopback(c)) out.push_back({c.id, c.name.empty() ? c.id : c.name});
    return out;
}

// The index of the chosen endpoint in `list`, or -1: none chosen (the default), or it is gone (unplugged, disabled), or
// for capture it is not a microphone.
inline int FindChosen(const std::vector<Candidate> &list, const std::wstring &chosen, bool capture) {
    if (chosen.empty()) return -1;
    for (size_t i = 0; i < list.size(); i++)
        if (list[i].id == chosen) return capture && IsMonitorOrLoopback(list[i]) ? -1 : static_cast<int>(i);
    return -1;
}

// A chosen endpoint that cannot be used now: the system default stands in (and the overlay says so once).
inline bool ChosenMissing(const std::vector<Candidate> &list, const std::wstring &chosen, bool capture) {
    return !chosen.empty() && FindChosen(list, chosen, capture) < 0;
}

// The capture endpoint with the player's choice: the chosen microphone while it is there, else PickCapture's.
inline int PickCapture(const std::vector<Candidate> &list, int default_index, const std::wstring &chosen) {
    const int i = FindChosen(list, chosen, true);
    return i >= 0 ? i : PickCapture(list, default_index);
}

} // namespace fi::audio::devices
