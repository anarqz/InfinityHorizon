// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the look shared by every screen (ui.cpp defines it): the Xbox palette in the
// Horizon pink, Segoe fonts, icons, easing and the drawing helpers. Render thread only.
#pragma once

#include <cstdint>
#include <string>

#include "imgui.h"

namespace fi::ui {

// ---------------------------------------------------------------- palette
constexpr ImU32 Rgba(int r, int g, int b, float a = 1.0f) { return IM_COL32(r, g, b, static_cast<int>(a * 255.0f)); }
inline constexpr ImU32 kPinkU = IM_COL32(230, 0, 126, 255);
inline constexpr ImU32 kPinkDark = IM_COL32(176, 0, 96, 255);
inline constexpr ImU32 kPanel = IM_COL32(20, 20, 22, 250);
inline constexpr ImU32 kTile = IM_COL32(38, 38, 42, 255);
inline constexpr ImU32 kTileFocus = IM_COL32(52, 52, 58, 255);
inline constexpr ImU32 kText = IM_COL32(255, 255, 255, 255);
inline constexpr ImU32 kTextDim = IM_COL32(178, 178, 186, 255);
inline constexpr ImU32 kGreen = IM_COL32(16, 124, 16, 255);
inline constexpr ImU32 kBlue = IM_COL32(0, 120, 215, 255);
inline constexpr ImU32 kYellow = IM_COL32(255, 185, 0, 255);
inline constexpr ImU32 kAmber = IM_COL32(255, 185, 0, 255);
inline constexpr ImU32 kRed = IM_COL32(232, 17, 35, 255);
inline constexpr ImU32 kTalkGreen = IM_COL32(92, 214, 92, 255); // the "Online" dot's green: a player talking now (§15)

// Fluent UI System Icons (regular, 24) code points, the font embedded in the DLL (fonts.rc)
inline constexpr const char *kIconInfo = "\xEF\x92\xA4";       // F4A4 Fluent
inline constexpr const char *kIconCheck = "\xEF\x8A\x95";      // F295 Fluent
inline constexpr const char *kIconWarning = "\xEF\xA1\xAA";    // F86A Fluent
inline constexpr const char *kIconError = "\xEF\x8F\xB2";      // F3F2 Fluent
inline constexpr const char *kIconStar = "\xEF\x9C\x90";       // F710 Fluent
inline constexpr const char *kIconDownload = "\xEF\x85\x91";   // F151 Fluent
inline constexpr const char *kIconAddFriend = "\xEF\x97\x83";  // F5C3 Fluent
inline constexpr const char *kIconHome = "\xEF\x92\x81";       // F481 Fluent
inline constexpr const char *kIconPeople = "\xEF\x96\xA9";     // F5A9 Fluent
inline constexpr const char *kIconMessage = "\xEF\x8A\x87";    // F287 Fluent
inline constexpr const char *kIconSettings = "\xEF\x9A\xAA";   // F6AA Fluent
inline constexpr const char *kIconGlobe = "\xEF\x91\x9B";      // F45B Fluent
inline constexpr const char *kIconContact = "\xEF\x96\xBE";    // F5BE Fluent
inline constexpr const char *kIconLock = "\xEE\x9E\x90";       // E790 Fluent
inline constexpr const char *kIconBack = "\xEF\x8D\xAA";       // F36A Fluent
inline constexpr const char *kIconGame = "\xEF\x91\x91";       // F451 Fluent
inline constexpr const char *kIconChevron = "\xEF\x8A\xB1";    // F2B1 Fluent
inline constexpr const char *kIconPower = "\xEF\x98\x8F";      // F60F Fluent
inline constexpr const char *kIconBackspace = "\xEF\x86\xB2";  // F1B2 Fluent
inline constexpr const char *kIconShift = "\xEF\x92\xBF";      // F4BF Fluent (KeyboardShift)
inline constexpr const char *kIconEnter = "\xEE\x83\x81";      // E0C1 Fluent (Enter)
inline constexpr const char *kIconKeyboard = "\xEF\x92\xB9";   // F4B9 Fluent
inline constexpr const char *kIconWorld = "\xEF\x8F\x9B";      // F3DB Fluent

struct Fonts {
    ImFont *body = nullptr;        // Segoe UI 19
    ImFont *small = nullptr;       // Segoe UI 15
    ImFont *title = nullptr;       // Segoe UI Semibold 22
    ImFont *heading = nullptr;     // Segoe UI Semibold 28
    ImFont *tag = nullptr;         // Segoe UI Bold 12
    ImFont *icon = nullptr;        // icons 24
    ImFont *icon_big = nullptr;    // icons 34
    ImFont *toast_title = nullptr; // Segoe UI Semibold 23
    ImFont *toast_body = nullptr;  // Segoe UI 18
};
extern Fonts f;
extern float s; // 1.0 at 1080p

bool Portuguese();
const char *T(const char *en, const char *pt);

float Clamp01(float x);
float EaseOutCubic(float t);
float EaseInCubic(float t);
float EaseOutBack(float t);

void Shadow(ImDrawList *dl, ImVec2 a, ImVec2 b, float rounding, float alpha);
void TextAt(ImDrawList *dl, ImFont *font, ImVec2 pos, ImU32 color, const char *text, float alpha = 1.0f);
ImVec2 Measure(ImFont *font, const char *text);
// Xbox controller face button: a coloured disc with its letter.
void FaceButton(ImDrawList *dl, ImVec2 centre, float radius, ImU32 color, const char *letter);
// The Xbox focus: a white ring with a soft outer glow.
void FocusRing(ImDrawList *dl, ImVec2 a, ImVec2 b, float rounding);
// A volume slider (VOICE_CHAT.md §15) on the track a..b (its height is the knob's): the filled part in pink while
// `active` (else dim), the knob at `volume` (1..100). A mouse click on the track (a little taller than drawn) sets
// `clicked` to the value under the mouse; it stays untouched otherwise.
void VolumeSlider(ImDrawList *dl, ImVec2 a, ImVec2 b, int volume, bool active, uint8_t *clicked = nullptr);

} // namespace fi::ui
