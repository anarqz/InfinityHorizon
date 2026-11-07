// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the guide's layout decisions, free of ImGui and Windows so they are tested on their own.
//
// - Settings categories: the Settings tab lists parent categories; the focused one fills the second column (beside the
//   guide, where the lobby and player details show). Each category is an id, its English and Portuguese labels and the
//   settings it holds, in order. A new category is one more entry here (ui.cpp draws any category's settings as tiles;
//   a page that needs more registers its own draw function there by id).
// - A player row in "my lobby": the player's card (the name, A opens their details) and beside it the big mute button;
//   two focus stops, the card first. No volume slider in the row: the per-player volume lives in the details.
// - A player's details: the volume first, then the mute toggle, then the rest.
#pragma once

#include <cstddef>
#include <cstdint>
#include <vector>

namespace fi::layout {

// ---------------------------------------------------------------- settings
enum class SettingsItem : uint8_t {
    ConnectionStatus,  // the link to the Infinity server (Online, Reconnecting...) and its detail
    ConnectionDetails, // the session's connection report (the launcher's text)
    VoiceEnabled,      // voice chat on or off
    MicMode,           // open, push-to-talk, muted
    MicDevice,         // the microphone (the device list)
    OutputDevice,      // where the lobby is heard (the device list)
    VoiceVolume,       // everyone's voices, 1..100
    MicVolume,         // this player's microphone, 1..100
    MuteEveryone,      // mute or unmute the lobby's players
};

struct SettingsCategory {
    const char *id;
    const char *en, *pt;
    std::vector<SettingsItem> items;
};

inline const std::vector<SettingsCategory> &SettingsCategories() {
    static const std::vector<SettingsCategory> categories = {
        {"connection", "Connection", "Conexão", {SettingsItem::ConnectionStatus, SettingsItem::ConnectionDetails}},
        {"voice", "Voice Chat", "Chat de voz",
         {SettingsItem::VoiceEnabled, SettingsItem::MicMode, SettingsItem::MicDevice, SettingsItem::OutputDevice, SettingsItem::VoiceVolume,
          SettingsItem::MicVolume, SettingsItem::MuteEveryone}},
    };
    return categories;
}

// The category index holding `item`, or -1.
inline int SettingsCategoryOf(SettingsItem item) {
    const auto &all = SettingsCategories();
    for (size_t c = 0; c < all.size(); ++c)
        for (SettingsItem i : all[c].items)
            if (i == item) return static_cast<int>(c);
    return -1;
}

// The category index of `id`, or -1.
inline int SettingsCategoryIndex(const char *id) {
    const auto &all = SettingsCategories();
    for (size_t c = 0; c < all.size(); ++c) {
        const char *a = all[c].id, *b = id;
        while (*a && *a == *b) ++a, ++b;
        if (*a == *b) return static_cast<int>(c);
    }
    return -1;
}

// A focus index kept inside a list of `count` (0 when empty).
inline int ClampFocus(int focus, int count) { return count <= 0 ? 0 : focus < 0 ? 0 : focus >= count ? count - 1 : focus; }

// ---------------------------------------------------------------- a player row in my lobby
struct Box {
    float x0, y0, x1, y1;
};
struct PlayerRow {
    Box card; // the name; A (or a click) opens the player's details
    Box mute; // the big mute button beside it, as tall as the card and square
};
inline constexpr int kPlayerRowStops = 2; // the card, then the mute button
inline constexpr int kStopCard = 0, kStopMute = 1;

inline PlayerRow LayoutPlayerRow(float x, float y, float w, float h, float gap) {
    const float side = h;
    return {{x, y, x + w - side - gap, y + h}, {x + w - side, y, x + w, y + h}};
}

// Where the controller goes in the lobby: items 0..first_player-1 are the buttons above the players (one stop each),
// then each player is two stops side by side (card, mute). Up/Down keep the column between players; Right on a card
// goes to its mute button, Left on a mute button back to the card.
enum class Move : uint8_t { Up, Down, Left, Right };
struct LobbyFocus {
    int item = 0; // the buttons above the players (Leave lobby, then Invite friends in a private lobby), then the players
    int col = 0;  // on a player: kStopCard or kStopMute
};
// True: used. False: the guide may use it (Left/Right change the tab off a player's card or a button).
inline bool MoveLobbyFocus(LobbyFocus &f, Move move, int first_player, int count) {
    const bool on_player = f.item >= first_player;
    switch (move) {
    case Move::Up:
    case Move::Down: {
        const int next = move == Move::Up ? f.item - 1 : f.item + 1;
        if (next < 0 || next >= count) return true;
        f.col = on_player && next >= first_player ? f.col : kStopCard;
        f.item = next;
        return true;
    }
    case Move::Right:
        if (!on_player) return false;
        f.col = kStopMute;
        return true;
    case Move::Left:
        if (!on_player || f.col == kStopCard) return false;
        f.col = kStopCard;
        return true;
    }
    return false;
}

// ---------------------------------------------------------------- a player's details
enum class MemberItem : uint8_t { Volume, Mute, Friend, Report, Back };

// The details' controls in focus order (OD-204 item 4: Report, Friends, Mute and the voice volume; T-L3 removed the host's mute
// for the lobby and remove, which only served player-hosted lobbies). friend_state (MemberRow): 0 none, 1 friends, 2 request
// sent, 3 request received, 4 this player.
inline std::vector<MemberItem> MemberItems(uint8_t friend_state) {
    std::vector<MemberItem> out{MemberItem::Volume, MemberItem::Mute};
    if (friend_state == 0 || friend_state == 2 || friend_state == 3) out.push_back(MemberItem::Friend);
    out.push_back(MemberItem::Report);
    out.push_back(MemberItem::Back);
    return out;
}

} // namespace fi::layout
