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
    bool monitor = false; // named after an active output, in any language (NamesAnOutput)
};

inline std::wstring Lower(const std::wstring &s) {
    std::wstring out(s);
    for (wchar_t &c : out) c = static_cast<wchar_t>(std::towlower(c));
    return out;
}

// The forms a friendly name is compared in, lower case: the whole name, and the device description inside the last
// top-level "(...)" when the name is "<endpoint> (<description>)", as Windows and Wine wrap it ("Microphone (X)",
// "Speakers (X)"). A capture and a render endpoint of one device share that description.
inline std::vector<std::wstring> NameForms(const std::wstring &name) {
    const std::wstring lower = Lower(name);
    std::vector<std::wstring> forms{lower};
    if (lower.size() < 4 || lower.back() != L')') return forms;
    int depth = 0;
    for (size_t i = lower.size(); i-- > 0;) {
        if (lower[i] == L')') depth++;
        else if (lower[i] == L'(' && --depth == 0) {
            if (i >= 2 && lower[i - 1] == L' ' && i + 2 < lower.size()) forms.push_back(lower.substr(i + 1, lower.size() - i - 2));
            break;
        }
    }
    return forms;
}

// True: `capture` is "<words> <an output's name>", the way PulseAudio/PipeWire name an output's monitor in every
// language ("Monitor of X", "Monitor de X", "Monitor von X", "Moniteur de X", "Монитор X"), wrapped by Wine or not.
// The same description on both sides ("Headset Microphone (AirPods)" beside "Headphones (AirPods)") is one device's
// microphone and speaker, not a monitor. Ignored: output names under 4 characters or generic ("Speakers"), and a
// Windows duplicate prefix ("2- USB Audio" beside "USB Audio"). Only the prefix form: a suffix ("X Hands-Free" beside
// "X") is how Windows names a Bluetooth headset's microphone.
inline bool NamesAnOutput(const std::wstring &capture, const std::vector<std::wstring> &render_names) {
    static const wchar_t *const kGeneric[] = {L"speakers", L"speaker", L"headphones", L"headset", L"earphones", L"output",
                                              L"audio", L"sound", L"default", L"digital output", L"line out", L"microphone"};
    const std::vector<std::wstring> mine = NameForms(capture);
    for (const std::wstring &render : render_names)
        for (const std::wstring &out : NameForms(render)) {
            if (out.size() < 4) continue;
            bool generic = false;
            for (const wchar_t *g : kGeneric) generic = generic || out == g;
            if (generic) continue;
            for (const std::wstring &in : mine) {
                if (in.size() < out.size() + 2 || in.compare(in.size() - out.size(), out.size(), out) != 0 || in[in.size() - out.size() - 1] != L' ')
                    continue;
                std::wstring affix = in.substr(0, in.size() - out.size() - 1);
                while (!affix.empty() && affix.back() == L' ') affix.pop_back();
                bool duplicate = affix.size() >= 2 && affix.back() == L'-';
                for (size_t i = 0; duplicate && i + 1 < affix.size(); i++) duplicate = affix[i] >= L'0' && affix[i] <= L'9';
                if (!affix.empty() && !duplicate) return true;
            }
        }
    return false;
}

// True: the endpoint is a stand-in for "the system default" rather than one device, as Wine lists PulseAudio's or
// PipeWire's default source (named after the sound server, not a device). What it captures cannot be checked: on a PC
// with no microphone (SAURON, 2026-10-31) the server's default source is the output's monitor, so opening it sends the
// game and the voice mix to the lobby. Never opened, never listed; a real microphone behind it is listed on its own.
inline bool IsDefaultAlias(const Candidate &c) {
    static const wchar_t *const kMarks[] = {L"pulseaudio", L"pipewire", L"{default}"};
    const std::wstring name = Lower(c.name), id = Lower(c.id);
    for (const wchar_t *mark : kMarks)
        if (name.find(mark) != std::wstring::npos || id.find(mark) != std::wstring::npos) return true;
    return name == L"default" || name == L"default device" || name == L"microphone (default)";
}

// True: the endpoint carries what the PC plays (or cannot be shown not to), not a microphone. The marks include the
// localized names of Windows drivers' "Stereo Mix" (de/nl, pt, es, fr, it, pl, ru, ja, zh, ko).
inline bool IsMonitorOrLoopback(const Candidate &c) {
    if (c.render || c.monitor || IsDefaultAlias(c)) return true;
    static const wchar_t *const kMarks[] = {
        L".monitor", L"monitor of", L"loopback", L"stereo mix", L"what u hear", L"wave out mix", L"what you hear",
        L"stereomix", L"stereo-mix", L"mixagem estéreo", L"mixagem estereo", L"mistura estéreo", L"mistura estereo",
        L"mezcla estéreo", L"mezcla estereo", L"mixage stéréo", L"mixage stereo", L"missaggio stereo",
        L"miks stereo", L"стерео микшер",
        L"Стерео микшер", // towlower may leave Cyrillic as is
        L"ステレオ ミキサー", L"立体声混音", L"스테레오 믹스"};
    const std::wstring name = Lower(c.name), id = Lower(c.id);
    for (const wchar_t *mark : kMarks)
        if (name.find(mark) != std::wstring::npos || id.find(mark) != std::wstring::npos) return true;
    return false;
}

// The endpoint to capture from: the default communications input if it is a microphone, else the first microphone in
// the system's order; -1 when there is none (listen-only). default_index < 0: there is no default. The default is
// always resolved to its entry in `list` and checked like any other: an unchecked default is never opened (with every
// input a monitor, a loopback or a default stand-in, the answer is -1, never "the default anyway").
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
