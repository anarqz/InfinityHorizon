// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the lobby tab (OD-63, OD-192, OD-196, OD-204). The launcher talks to matchmaking (the overlay holds no
// tokens); this file only asks and shows.
//
// - OD-204 (T-L3): one roster pane, the lobby this player is in and its players (lobby_policy.h LobbyKind). No lobby list, no
//   lobby code and no create form: the game's own menus create, invite and join (OD-204 item 2). Out of a lobby the tab says so
//   and where lobbies come from.
// - A private official lobby (MyWorld.is_private) is the Private Lobby: its players with Report, Friends, Mute and the voice
//   volume, Leave lobby (A12, OD-196) and Invite friends, enabled for the current lobby leader only (lobby_policy.h InviteGateOf;
//   it asks the launcher for the friend picker, friend_picker.cpp, T-L4). Any other official lobby is the Official Lobby (OD-192): the same pane without Invite.
//   A lobby the launcher itself hosts (the player-hosted code stays until T-L8) shows its players; it is closed from the launcher.
// - No overlay notice in an official lobby, a private one included (OD-192), except what the launcher marks always (ui.cpp
//   PushToast, T-L3); this tab's own action results follow the same rule.
// - Each player is their card (the name; a green border while they talk) with the big mute button beside it; A on the card opens
//   their details in the second column beside the guide: their volume (1-100), mute, friend and report. Player names only, never
//   their XUIDs (OD-61).
// - Modded players (OD-181): a gear beside a player's name says they use mods (0x1E RosterMods), and their details list them.
// - Hardcore + Noob (OD-141): a lobby open to both modes carries the "Hardcore + Noob" badge in its header.
#include "overlay.h"

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <mutex>
#include <vector>

#include "fi_overlay.h"
#include "lobby_policy.h"
#include "style.h"
#include "ui_layout.h"
#include "voice_audio.h"
#include "voice_volume.h"
#include "world_access.h"

namespace fi {

using namespace ui;

namespace {

// ---------------------------------------------------------------- what the launcher sent (any thread)
struct Shared {
    WorldAccessLease access;
    std::string access_message;
    MyWorld mine;
    uint64_t mine_serial = 0;
    RosterMods roster_mods; // OD-181 (0x1E): each player's declared mods, for mine.world only
    uint8_t action = 0, action_status = 0;
    std::string action_message;
    uint64_t action_serial = 0;
};
std::mutex g_lock;
Shared g_shared;
std::atomic<bool> g_official{false}; // OD-192: g_shared.mine is an official world's (read without the lock: PushToast, the tab name)
std::atomic<bool> g_private{false};  // OD-204: and a private one (the tab name)

bool WorldAccessAllowed() {
    if (DevHost()) return true;
    std::lock_guard<std::mutex> lock(g_lock);
    return g_shared.access.Allowed(GetTickCount64());
}

const char *AccessMessage(const Shared &sh) {
    return !sh.access.Allowed(GetTickCount64()) && !sh.access_message.empty() ? sh.access_message.c_str() : T(
        "Online installation verification is unavailable. Return to the launcher to review your installation or get help.",
        "A verificação da instalação para jogar online está indisponível. Volte ao launcher para verificar sua instalação ou obter ajuda.");
}

// ---------------------------------------------------------------- render-thread state
enum class Screen { List, Busy, Member, Report, Confirm };
Screen g_screen = Screen::List;
std::string g_busy; // what Busy says

// the lobby (OD-63)
bool g_mine_refresh = true;  // one explicit reconcile on open/action/manual refresh
bool g_showing_mine = false; // the List screen shows the lobby this frame
int g_mine_focus = 0;        // the buttons above the players (g_mine_buttons), then the players
int g_mine_count = 0;        // what the focus walks: the buttons + the players
int g_mine_col = 0;          // on a player: layout::kStopCard the card, kStopMute the mute button beside it (OD-154)
int g_mine_scroll = 0;       // §15: the first player row drawn (the rows follow the focus)
bool g_mine_x = false;       // X on a player: mute or unmute them, applied at draw time
MemberRow g_member;          // the player the card is about
std::string g_mine_world;    // of the world last drawn (OD-181: whose players' mods g_roster holds)
RosterMods g_roster;         // OD-181: the render thread's copy of Shared::roster_mods
int g_button = 0;            // a details or confirm screen's focus
int g_buttons = 0;           // how many buttons the current card has
uint64_t g_seen_mine = 0, g_seen_action = 0;

// The buttons above the players, in focus order (T-L3): Leave lobby (every lobby but one the launcher hosts, OD-196, A12), then
// Invite friends in a private lobby. Rebuilt by DrawMine every frame.
enum class MineButton : uint8_t { Leave, Invite };
std::vector<MineButton> g_mine_buttons;

// the dev host answers by itself, as a launcher would: a private lobby this player leads
float g_dev_wait = -1;
int g_dev_kind = 0;
int g_dev_mine = 3; // the dev host's own world: 0 none, 3 seated

// OD-154, CR-L46: the lobby's voice mode (off, open, proximity)
const char *VoiceLabel(uint8_t mode) {
    return mode == kLobbyVoiceProximity ? T("Voice chat: proximity", "Chat de voz: proximidade")
           : mode == kLobbyVoiceOpen    ? T("Voice chat: open", "Chat de voz: aberto")
                                        : T("Voice chat off", "Chat de voz desligado");
}

// OD-141: the badge of a lobby open to both modes (the same words in both languages)
constexpr const char *kAnyModeBadge = "Hardcore + Noob";

// ---------------------------------------------------------------- requests
void Act(uint8_t action, const MemberRow &member, uint8_t reason) {
    g_busy = T("One moment...", "Um momento...");
    g_screen = Screen::Busy;
    if (DevHost()) {
        g_dev_wait = 0.6f;
        g_dev_kind = 10 + action;
    } else {
        SendWorldAction(action, member.xuid, member.gamertag, reason);
    }
}

// Invite friends (T-L4): the launcher answers 0x26 with the friend picker (0x25, open) or a notice; the dev host opens a synthetic one.
void InviteFriends(const MyWorld &m) {
    if (InviteGateOf(m) != InviteGate::Allowed) return;
    if (DevHost()) {
        FriendPickerFrame p;
        p.flags = kPickerOpen;
        p.friends = {{"2", "Synthetic#00230", kPickerInLobby}, {"4", "Synthetic#00311", kPickerAvailable},
                     {"5", "Synthetic#00412", kPickerInvited}, {"6", "Synthetic#00519", kPickerOffline}};
        SetFriendPicker(p);
        return;
    }
    SendFriendPickerAsk();
    Log("invite: Invite friends chosen in the Private Lobby pane; the launcher answers with its friend picker");
}

// the dev host's fake launcher
void DevAnswer() {
    std::lock_guard<std::mutex> lock(g_lock);
    switch (g_dev_kind) {
    case 6: { // the lobby, as the launcher would answer: a private one, this player ("1") its leader
        MyWorld m;
        if (g_dev_mine != 0) {
            m.role = static_cast<uint8_t>(g_dev_mine);
            m.world = "h1";
            m.name = "Synthetic Private Free Roam";
            m.host = "";
            m.players = 3;
            m.max = 12;
            m.named = true;
            m.official = true;
            m.is_private = true;
            m.transport = kTransportDirect;
            m.leader = "1";
            m.members.push_back({"1", "Synthetic#00117", 1, 1, 4});
            m.members.push_back({"2", "Synthetic#00230", 0, 1, 0});
            m.members.push_back({"3", "Synthetic#00042", 1, 2, 3});
        }
        // OD-181: the dev roster mixes mods (the launcher's 0x1E): the first and the third player declare some, the second none
        g_shared.roster_mods = RosterMods{};
        if (g_dev_mine != 0) {
            g_shared.roster_mods.world = m.world;
            g_shared.roster_mods.members.push_back({"1", {{"Synthetic Handling Pack", "3"}, {"extraoptions", "2"}}});
            g_shared.roster_mods.members.push_back({"3", {{"Synthetic Night Lights", "1"}}});
        }
        g_official.store(m.role != 0 && m.official, std::memory_order_relaxed);
        g_private.store(m.role != 0 && m.official && m.is_private, std::memory_order_relaxed);
        g_shared.mine = m;
        ++g_shared.mine_serial;
        break;
    }
    case 12: case 14: case 15: {
        const int action = g_dev_kind - 10;
        g_shared.action = static_cast<uint8_t>(action);
        g_shared.action_status = 0;
        g_shared.action_message = action == 2 ? "You left the lobby." : action == 4 ? "Friend request sent." : "Thanks. The report was sent to the moderators.";
        ++g_shared.action_serial;
        if (action == 2) g_dev_mine = 0;
        break;
    }
    default: break;
    }
}

// ---------------------------------------------------------------- drawing helpers
void Wrapped(ImDrawList *dl, ImFont *font, ImVec2 pos, float width, ImU32 color, const char *text) {
    dl->AddText(font, font->FontSize, pos, color, text, nullptr, width);
}

float WrappedHeight(ImFont *font, float width, const char *text) {
    return font->CalcTextSizeA(font->FontSize, FLT_MAX, width, text).y;
}

// a modal right pane, invite prompt or friend picker holds the input: the tab underneath takes no clicks
bool Clicked(ImVec2 a, ImVec2 b) {
    return !PaneModal() && !InvitePromptOpen() && !FriendPickerOpen() && ImGui::IsMouseHoveringRect(a, b) && ImGui::IsMouseClicked(0);
}

bool g_accept = false; // A, applied at draw time on the focused item

float Panel(ImDrawList *dl, float x, float y, const char *title) {
    TextAt(dl, f.title, ImVec2(x, y), kText, title);
    return y + 42 * s;
}

void Button(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, bool primary, const char *text) {
    dl->AddRectFilled(a, b, primary ? kPinkU : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const ImVec2 ts = Measure(f.body, text);
    TextAt(dl, f.body, ImVec2((a.x + b.x - ts.x) / 2, (a.y + b.y - ts.y) / 2), kText, text);
}

void DrawBusy(ImDrawList *dl, float x, float y, float w) {
    y += 60 * s;
    const float t = static_cast<float>(ImGui::GetTime());
    const ImVec2 c(x + w / 2, y + 30 * s);
    dl->PathArcTo(c, 26 * s, t * 5.0f, t * 5.0f + 4.2f, 32);
    dl->PathStroke(kPinkU, 0, 4 * s);
    const ImVec2 ts = Measure(f.body, g_busy.c_str());
    TextAt(dl, f.body, ImVec2(x + (w - ts.x) / 2, y + 80 * s), kText, g_busy.c_str());
}

// Out of any lobby (T-L3): where lobbies come from now.
void DrawNoLobby(ImDrawList *dl, float x, float y, float w, const Shared &sh) {
    if (!DevHost() && !sh.access.Allowed(GetTickCount64())) {
        Wrapped(dl, f.body, ImVec2(x, y), w, kAmber, AccessMessage(sh));
        return;
    }
    TextAt(dl, f.title, ImVec2(x, y), kText, T("You are not in a lobby", "Você não está em um lobby"));
    y += 40 * s;
    Wrapped(dl, f.body, ImVec2(x, y), w, kTextDim, T(
        "Lobbies are joined and created from the game's own menus: Free Roam Online places you in an official lobby, and a private "
        "Free Roam or Road Trip is a lobby only your friends can join. This tab then shows the lobby you are in and its players.",
        "Os lobbies são acessados e criados pelos menus do próprio jogo: o Free Roam Online coloca você em um lobby oficial, e um "
        "Free Roam ou Road Trip privado é um lobby em que só seus amigos podem entrar. Esta aba mostra então o lobby em que você "
        "está e os jogadores dele."));
}

// ---------------------------------------------------------------- my world (OD-63)
const char *MemberStatus(const MemberRow &m) {
    switch (m.role) {
    case 0: return T("Host", "Anfitrião");
    case 1: return T("In the lobby", "No lobby");
    default: return T("On the way", "Chegando");
    }
}

const char *FriendStatus(uint8_t state) {
    switch (state) {
    case 1: return T("Friend", "Amigo");
    case 2: return T("Friend request sent", "Pedido de amizade enviado");
    case 3: return T("Wants to be your friend", "Quer ser seu amigo");
    case 4: return T("You", "Você");
    default: return "";
    }
}

void MemberTile(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const MemberRow &m, bool leader) {
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float th = b.y - a.y;
    dl->AddText(f.icon, f.icon->FontSize, ImVec2(a.x + 18 * s, a.y + th / 2 - f.icon->FontSize / 2), m.friend_state == 4 ? kPinkU : kText, kIconContact);
    const float lx = a.x + 60 * s;
    dl->PushClipRect(ImVec2(lx, a.y), ImVec2(b.x - 44 * s, b.y), true);
    TextAt(dl, f.body, ImVec2(lx, a.y + 9 * s), kText, m.gamertag.c_str());
    if (const auto *mods = g_roster.Of(g_mine_world, m.xuid); mods && !mods->empty()) {
        // OD-181: the small gear right after the name: this player uses mods (A on the card lists them)
        const float gx = lx + Measure(f.body, m.gamertag.c_str()).x + 8 * s;
        dl->AddText(f.icon, f.icon->FontSize * 0.6f, ImVec2(gx, a.y + 12 * s), kTextDim, kIconSettings);
    }
    std::string sub = leader ? T("Lobby leader", "Líder do lobby") : MemberStatus(m); // T-L3: the private lobby's leader
    if (const char *fs = FriendStatus(m.friend_state); *fs) sub += std::string(" · ") + fs;
    TextAt(dl, f.small, ImVec2(lx, a.y + 36 * s), m.friend_state == 3 ? kPinkU : kTextDim, sub.c_str());
    dl->PopClipRect();
    if (m.tier == 1) {
        dl->AddText(f.icon, f.icon->FontSize * 0.8f, ImVec2(b.x - 36 * s, a.y + th / 2 - f.icon->FontSize * 0.4f), kPinkU, kIconStar);
    }
}

// ---------------------------------------------------------------- voice chat in my world (OD-154)
int FirstPlayerItem() { return static_cast<int>(g_mine_buttons.size()); }

// Why the microphone squares do nothing now, or null when they work.
const char *VoiceUnavailable(const VoiceState &v) {
    if (!(v.flags & VoiceState::LobbyKnown)) return T("The lobby's voice chat setting has not arrived yet.", "A configuração de voz do lobby ainda não chegou.");
    if (!(v.flags & VoiceState::LobbyOn)) return T("Voice chat is off in this lobby.", "O chat de voz está desligado neste lobby.");
    if (!(v.flags & VoiceState::Enabled)) return T("Voice chat is off in your settings (Settings > Voice chat).", "O chat de voz está desligado nos seus ajustes (Ajustes > Chat de voz).");
    return nullptr;
}

// A: the player's own square switches the microphone between open and muted; anyone else's mutes or unmutes them.
void ToggleVoice(const MemberRow &m, const VoiceState &v) {
    if (const char *why = VoiceUnavailable(v)) {
        PushToast(ToastKind::Info, T("Voice chat", "Chat de voz"), why);
        return;
    }
    if (m.friend_state == 4) {
        const MicMode next = v.mic == MicMode::Muted ? MicMode::Open : MicMode::Muted;
        VoiceCommand(VoiceOp::Mic, std::string(), static_cast<uint8_t>(next));
        PushToast(ToastKind::Info, T("Microphone", "Microfone"), MicModeName(next));
        return;
    }
    const bool muted = v.StateOf(m.xuid) & VoiceMember::MutedByMe;
    VoiceCommand(VoiceOp::Mute, m.xuid, muted ? 0 : 1);
    PushToast(ToastKind::Info, muted ? T("Unmuted", "Som liberado") : T("Muted", "Silenciado"), m.gamertag);
}

// Heard talking now (the green border around the name, §15): another player's talking state, or this player's own voice
// leaving the PC; never while voice is unusable or the player is silenced.
bool RowTalking(const MemberRow &m, const VoiceState &v) {
    if (VoiceUnavailable(v)) return false;
    if (m.friend_state == 4) return v.mic != MicMode::Muted && audio::Sending();
    const uint8_t st = v.StateOf(m.xuid);
    return (st & VoiceMember::Talking) && !(st & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked));
}

// §15: the row's volume: another player's (this player's level for them) or, on this player's own row, the microphone's.
uint8_t RowVolume(const MemberRow &m, const VoiceState &v) { return m.friend_state == 4 ? v.mic_volume : v.VolumeOf(m.xuid); }
void SetRowVolume(const MemberRow &m, uint8_t to) {
    const VoiceState v = CurrentVoice();
    if (!VolumeValue(to) || to == RowVolume(m, v)) return;
    if (m.friend_state == 4) VoiceCommand(VoiceOp::MicVolume, std::string(), to);
    else VoiceCommand(VoiceOp::PlayerVolume, m.xuid, to);
}

// The microphone square inside a player's mute button: their voice at a glance, and A to mute them.
void MicSquare(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const MemberRow &m, const VoiceState &v) {
    const bool self = m.friend_state == 4, usable = VoiceUnavailable(v) == nullptr;
    const uint8_t st = v.StateOf(m.xuid);
    const bool talking = RowTalking(m, v);
    dl->AddRectFilled(a, b, focus ? Rgba(70, 70, 78) : Rgba(52, 52, 58), 5 * s);
    bool slash = false;
    ImU32 color = kText;
    if (!usable) {
        slash = true;
        color = Rgba(255, 255, 255, 0.28f);
    } else if (self && (v.flags & VoiceState::SelfBlocked)) { // CR-S67/S68: the server blocks this player's voice
        slash = true;
        color = kAmber;
    } else if (self) {
        slash = v.mic == MicMode::Muted;
        color = slash ? kRed : v.mic == MicMode::PushToTalk ? kTextDim : kText;
    } else if (st & (VoiceMember::Blocked | VoiceMember::MutedForLobby)) {
        slash = true;
        color = kAmber;
    } else if (st & VoiceMember::MutedByMe) {
        slash = true;
        color = kRed;
    } else if (talking) {
        color = kTalkGreen;
    }
    const ImVec2 c((a.x + b.x) / 2, (a.y + b.y) / 2);
    DrawMic(dl, c, (b.y - a.y) * 0.62f, color, slash, focus ? Rgba(70, 70, 78) : Rgba(52, 52, 58));
    if (usable && self && v.mic == MicMode::PushToTalk) {
        const ImVec2 ts = Measure(f.tag, "R3");
        TextAt(dl, f.tag, ImVec2(b.x - ts.x - 3 * s, b.y - ts.y - 1 * s), kTextDim, "R3");
    }
}

// The big mute button beside a player's card (the card's height, square): their voice at a glance (talking green,
// muted red, blocked or muted for the lobby amber) and A to mute or unmute them; on this player's own row it opens or
// mutes the microphone. The player's volume is in their details (the second column).
void MuteButton(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const MemberRow &m, const VoiceState &v) {
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float in = 7 * s;
    MicSquare(dl, ImVec2(a.x + in, a.y + in), ImVec2(b.x - in, b.y - in), focus, m, v);
}

// The volume row in a player's details: "Voice volume", the slider (Left/Right, or the mouse) and the value; the same
// 1..100 kept per XUID and the same gain law as before (VoiceOp::PlayerVolume).
void VolumeRow(ImDrawList *dl, ImVec2 a, ImVec2 b, bool focus, const MemberRow &m, uint8_t *picked) {
    const VoiceState v = CurrentVoice();
    dl->AddRectFilled(a, b, focus ? kTileFocus : kTile, 6 * s);
    if (focus) FocusRing(dl, a, b, 6 * s);
    const float h = b.y - a.y;
    const char *label = T("Voice volume", "Volume da voz");
    const ImVec2 ls = Measure(f.small, label);
    TextAt(dl, f.small, ImVec2(a.x + 16 * s, a.y + h / 2 - ls.y / 2), focus ? kText : kTextDim, label);
    const uint8_t volume = RowVolume(m, v);
    const std::string value = std::to_string(volume);
    const ImVec2 vs = Measure(f.body, value.c_str());
    const float wide = Measure(f.body, "100").x;
    const ImVec2 sa(a.x + 16 * s + ls.x + 16 * s, a.y + h / 2 - 8 * s), sb(b.x - 28 * s - wide, a.y + h / 2 + 8 * s);
    const uint8_t st = v.StateOf(m.xuid);
    const bool heard = !VoiceUnavailable(v) && !(st & (VoiceMember::MutedByMe | VoiceMember::MutedForLobby | VoiceMember::Blocked));
    VolumeSlider(dl, sa, sb, volume, focus && heard, picked);
    TextAt(dl, f.body, ImVec2(b.x - 14 * s - vs.x, a.y + h / 2 - vs.y / 2), focus ? kText : kTextDim, value.c_str());
}

// What the mouse clicked in my world this frame (item -1: nothing); the caller decides what it does.
struct MineClick {
    int item = -1;
    int col = layout::kStopCard;
};

const char *LobbyTag(LobbyKind kind) {
    switch (kind) {
    case LobbyKind::Private: return T("PRIVATE LOBBY", "LOBBY PRIVADO");
    case LobbyKind::Official: return T("OFFICIAL LOBBY", "LOBBY OFICIAL");
    default: return T("PLAYER-HOSTED LOBBY", "LOBBY HOSPEDADO POR JOGADOR");
    }
}

MineClick DrawMine(ImDrawList *dl, float x, float y, float w, float bottom, const MyWorld &m) {
    MineClick click;
    g_mine_world = m.world;
    const VoiceState voice = CurrentVoice();
    // OD-192, OD-204: the lobby and its players, never a lobby option (the server fixes them)
    const LobbyKind kind = LobbyKindOf(m);
    TextAt(dl, f.tag, ImVec2(x, y), kPinkU, LobbyTag(kind));
    y += 24 * s;
    dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, y + 44 * s), true);
    TextAt(dl, f.title, ImVec2(x, y), kText, m.name.c_str());
    dl->PopClipRect();
    y += 42 * s;
    // OD-141: the lobby's modes are fixed while it is open, so they are shown, never offered
    const std::string sub = (kind == LobbyKind::User && m.role != 1 && !m.host.empty() ? m.host + " · " : std::string()) +
                            (m.any_mode ? std::string(kAnyModeBadge) + " · " : std::string()) + std::to_string(m.players) + "/" +
                            std::to_string(m.max) + T(" players", " jogadores");
    TextAt(dl, f.small, ImVec2(x, y), kTextDim, sub.c_str());
    if (m.transport != kTransportUnknown && !m.notice.empty()) { // OD-160: the official relay or a direct connection, as the launcher says
        y += 20 * s;
        dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, y + 20 * s), true);
        TextAt(dl, f.small, ImVec2(x, y), m.transport == kTransportRelay ? kTalkGreen : kAmber, m.notice.c_str());
        dl->PopClipRect();
    }
    { // the lobby's voice chat as the launcher says it (OD-154, CR-S67; official lobbies: proximity, OD-192, A9)
        y += 20 * s;
        const char *said = voice.flags & VoiceState::LobbyKnown ? VoiceLabel(voice.lobby_mode()) : T("Voice chat: not known yet", "Chat de voz: ainda desconhecido");
        TextAt(dl, f.small, ImVec2(x, y), voice.lobby_on() ? kText : kTextDim, said);
    }
    uint8_t team = 0;
    if (VoiceTeamChat(&team)) { // OD-165: the relay says a team event runs for this player: its voice is its team's only
        y += 22 * s;
        const char *chip = T("Team chat", "Chat da equipe");
        const ImVec2 cs = Measure(f.tag, chip);
        const ImU32 tint = team == 1 ? Rgba(70, 130, 230) : Rgba(220, 70, 70);
        dl->AddRectFilled(ImVec2(x, y), ImVec2(x + cs.x + 16 * s, y + cs.y + 6 * s), tint, 4 * s);
        TextAt(dl, f.tag, ImVec2(x + 8 * s, y + 3 * s), kText, chip);
        y += 6 * s;
    }
    if (voice.flags & VoiceState::SelfBlocked) { // CR-S67/S68: kept small; the launcher's toast says until when
        y += 20 * s;
        TextAt(dl, f.small, ImVec2(x, y), kAmber, T("Your voice chat is blocked for now", "Seu chat de voz está bloqueado por enquanto"));
    }
    if (voice.lobby_on() && (voice.flags & VoiceState::Enabled) && audio::ListenOnly()) { // only monitors, or no input
        y += 20 * s;
        TextAt(dl, f.small, ImVec2(x, y), kAmber, T("No microphone: you can listen", "Sem microfone: você pode ouvir"));
    }
    y += 28 * s;
    if (m.role == 2 && kind == LobbyKind::User) {
        Wrapped(dl, f.small, ImVec2(x, y), w, kAmber, T("Pick the host's online mode in the game's menu to join it.", "Escolha no menu do jogo o modo online do host para entrar nele."));
        y += 30 * s;
    }
    y += 6 * s;
    g_mine_buttons.clear();
    if (m.role == 1) {
        // a lobby the launcher itself hosts (player-hosted, until T-L8): the overlay offers no host action any more (T-L3)
        Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, T("This lobby is hosted from the Infinity Launcher: close it there.", "Este lobby é hospedado pelo Infinity Launcher: feche-o por lá."));
        y += 30 * s;
    }
    // OD-196 (W-11, reverses W-9), A12 (OD-204): every lobby this player holds a seat in keeps Leave lobby, an official or a private
    // one included, so a player whose game shows no Horizon Solo option can still leave. Item 0 whenever the player is seated.
    if (m.role != 1) {
        const float bh = 52 * s;
        const ImVec2 a(x, y), b(x + w, y + bh);
        Button(dl, a, b, g_mine_focus == 0, false, T("Leave lobby", "Sair do lobby"));
        if (Clicked(a, b)) click = {0, layout::kStopCard};
        g_mine_buttons.push_back(MineButton::Leave);
        y += bh + 10 * s;
    }
    // OD-204 (T-L3): Invite friends in a private lobby, enabled for the current leader only (lobby_policy.h InviteGateOf, A3)
    if (kind == LobbyKind::Private) {
        const int item = static_cast<int>(g_mine_buttons.size());
        const bool allowed = InviteGateOf(m) == InviteGate::Allowed;
        const float bh = 52 * s;
        const ImVec2 a(x, y), b(x + w, y + bh);
        Button(dl, a, b, g_mine_focus == item, allowed, T("Invite friends", "Convidar amigos"));
        if (!allowed) dl->AddRectFilled(a, b, Rgba(0, 0, 0, 0.45f), 6 * s);
        if (Clicked(a, b)) click = {item, layout::kStopCard};
        g_mine_buttons.push_back(MineButton::Invite);
        y += bh + 6 * s;
        if (!allowed) {
            TextAt(dl, f.small, ImVec2(x, y), kTextDim, T("Only the lobby leader can invite", "Só o líder do lobby pode convidar"));
            y += 20 * s;
        }
        y += 4 * s;
    }
    y += 10 * s;
    // the buttons changed (private to official, a lobby the launcher hosts): the focus goes back to the top instead of
    // silently landing on another control
    static size_t last_buttons = 0;
    if (g_mine_buttons.size() != last_buttons) {
        last_buttons = g_mine_buttons.size();
        g_mine_focus = 0;
        g_mine_col = layout::kStopCard;
    }

    TextAt(dl, f.tag, ImVec2(x, y), kTextDim, T("PLAYERS", "JOGADORES"));
    y += 24 * s;
    if (!m.named) {
        Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, T("Player names arrive with the next server update.", "Os nomes dos jogadores chegam na próxima atualização do servidor."));
        y += 30 * s;
    }
    // OD-154: each player is their card (a green border while they talk; A opens their details) and beside it the big
    // mute button (ui_layout.h LayoutPlayerRow). The rows scroll with the focus.
    const float nh = 56 * s, gap = 12 * s, side_gap = 10 * s;
    const float row = nh + gap;
    const bool details_open = g_screen != Screen::List; // a player's details are open: their card stays marked
    const int first = FirstPlayerItem();
    const int count = static_cast<int>(m.members.size());
    const int fit = std::max(1, static_cast<int>((bottom - y + gap) / row));
    const int focused = g_mine_focus - first;
    if (focused >= 0) g_mine_scroll = std::clamp(g_mine_scroll, focused - fit + 1, focused);
    g_mine_scroll = std::clamp(g_mine_scroll, 0, std::max(0, count - fit));
    if (g_mine_scroll > 0) { // more above
        const ImVec2 ts = Measure(f.tag, "...");
        TextAt(dl, f.tag, ImVec2(x + w - ts.x, y - 22 * s), kTextDim, "...");
    }
    for (int i = g_mine_scroll; i < count && i < g_mine_scroll + fit; ++i) {
        const MemberRow &member = m.members[static_cast<size_t>(i)];
        const int item = i + first;
        const bool focus = g_mine_focus == item;
        const layout::PlayerRow r = layout::LayoutPlayerRow(x, y, w, nh, side_gap);
        const ImVec2 ta(r.card.x0, r.card.y0), tb(r.card.x1, r.card.y1), ma(r.mute.x0, r.mute.y0), mb(r.mute.x1, r.mute.y1);
        const bool card_focus = focus && g_mine_col == layout::kStopCard;
        MemberTile(dl, ta, tb, card_focus, member, kind == LobbyKind::Private && !m.leader.empty() && member.xuid == m.leader);
        if (details_open && member.xuid == g_member.xuid) { // its details are in the column: a pink bar at its left
            dl->AddRectFilled(ImVec2(ta.x, ta.y + 10 * s), ImVec2(ta.x + 4 * s, tb.y - 10 * s), kPinkU, 2 * s);
        }
        if (RowTalking(member, voice)) { // the whole card, inside the focus ring when it has one
            const float in = card_focus ? 3.5f * s : 0.0f;
            dl->AddRect(ImVec2(ta.x + in, ta.y + in), ImVec2(tb.x - in, tb.y - in), kTalkGreen, 6 * s, 0, 2.5f * s);
        }
        MuteButton(dl, ma, mb, focus && g_mine_col == layout::kStopMute, member, voice);
        if (Clicked(ta, tb)) click = {item, layout::kStopCard};
        if (Clicked(ma, mb)) click = {item, layout::kStopMute};
        y += row;
    }
    if (g_mine_scroll + fit < count) { // more below
        const ImVec2 ts = Measure(f.tag, "...");
        TextAt(dl, f.tag, ImVec2(x + w - ts.x, y - gap + 1 * s), kTextDim, "...");
    }
    g_mine_count = first + count;
    return click;
}

// The card's choices: an action number (4 friend, 5 report, 6 mute, 0 back) and its label.
struct Choice {
    int action;
    const char *label;
    bool enabled;
};

constexpr int kChoiceVolume = 8; // the volume row: Left/Right or the mouse, A does nothing

// The details' controls in ui_layout.h's order (OD-154: voice first, the most frequent reason to open a card in a loud
// lobby: the volume, then the mute toggle). OD-204 item 4: Report, Friends, Mute and the voice volume; no host action (T-L3).
std::vector<Choice> MemberChoices(const MemberRow &m) {
    std::vector<Choice> out;
    const VoiceState v = CurrentVoice();
    const uint8_t st = v.StateOf(m.xuid);
    for (layout::MemberItem item : layout::MemberItems(m.friend_state)) {
        switch (item) {
        case layout::MemberItem::Volume: out.push_back({kChoiceVolume, T("Voice volume", "Volume da voz"), true}); break;
        case layout::MemberItem::Mute:
            out.push_back({6, st & VoiceMember::MutedByMe ? T("Unmute voice", "Liberar voz") : T("Mute voice", "Silenciar voz"), true});
            break;
        case layout::MemberItem::Friend:
            if (m.friend_state == 0) out.push_back({4, T("Add friend", "Adicionar amigo"), true});
            else if (m.friend_state == 2) out.push_back({4, T("Friend request sent", "Pedido de amizade enviado"), false});
            else out.push_back({4, T("Accept friend request", "Aceitar pedido de amizade"), true});
            break;
        case layout::MemberItem::Report: out.push_back({5, T("Report", "Denunciar"), true}); break;
        case layout::MemberItem::Back: out.push_back({0, T("Back", "Voltar"), true}); break;
        }
    }
    return out;
}

void Buttons(ImDrawList *dl, float x, float y, float w, const std::vector<Choice> &choices) {
    const float bh = 52 * s;
    for (size_t i = 0; i < choices.size(); ++i) {
        const ImVec2 a(x, y), b(x + w, y + bh);
        const bool focus = g_button == static_cast<int>(i);
        if (choices[i].action == kChoiceVolume) {
            uint8_t picked = 0;
            VolumeRow(dl, a, b, focus, g_member, &picked);
            if (picked) { g_button = static_cast<int>(i); SetRowVolume(g_member, picked); }
            else if (Clicked(a, b)) g_button = static_cast<int>(i);
            y += bh + 8 * s;
            continue;
        }
        Button(dl, a, b, focus, false, choices[i].label);
        if (!choices[i].enabled) dl->AddRectFilled(a, b, Rgba(0, 0, 0, 0.45f), 6 * s);
        if (Clicked(a, b)) { g_button = static_cast<int>(i); g_accept = true; }
        y += bh + 8 * s;
    }
    g_buttons = static_cast<int>(choices.size());
}

// Left/Right on the details' volume row move it; false: the focused control is not the volume.
bool StepMemberVolume(int direction) {
    const auto choices = MemberChoices(g_member);
    if (g_button < 0 || g_button >= static_cast<int>(choices.size()) || choices[g_button].action != kChoiceVolume) return false;
    SetRowVolume(g_member, audio::StepVolume(RowVolume(g_member, CurrentVoice()), direction));
    return true;
}

void DrawMember(ImDrawList *dl, float x, float y, float w) {
    TextAt(dl, f.tag, ImVec2(x, y), kTextDim, T("PLAYER", "JOGADOR"));
    y += 24 * s;
    dl->AddText(f.icon_big, f.icon_big->FontSize, ImVec2(x, y), g_member.tier == 1 ? kPinkU : kText, kIconContact);
    const float lx = x + f.icon_big->FontSize + 16 * s;
    TextAt(dl, f.title, ImVec2(lx, y), kText, g_member.gamertag.c_str());
    std::string sub = MemberStatus(g_member);
    if (g_member.tier == 1) sub += " · Supporter"; // CR-S69: the badge
    if (const char *fs = FriendStatus(g_member.friend_state); *fs) sub += std::string(" · ") + fs;
    TextAt(dl, f.small, ImVec2(lx, y + 40 * s), kTextDim, sub.c_str());
    y += 70 * s;
    // the player's voice now, under the name (talking, muted, blocked)
    const VoiceState v = CurrentVoice();
    const uint8_t st = v.StateOf(g_member.xuid);
    const char *voice = RowTalking(g_member, v)                 ? T("Talking", "Falando")
                        : st & VoiceMember::Blocked              ? T("Voice blocked", "Voz bloqueada")
                        : st & VoiceMember::MutedForLobby        ? T("Muted for the lobby", "Silenciado para o lobby")
                        : st & VoiceMember::MutedByMe            ? T("Muted by you", "Silenciado por você")
                                                                 : "";
    if (*voice) TextAt(dl, f.small, ImVec2(x, y), RowTalking(g_member, v) ? kTalkGreen : st & VoiceMember::MutedByMe ? kRed : kAmber, voice);
    y += 28 * s;
    const std::vector<Choice> choices = MemberChoices(g_member);
    Buttons(dl, x, y, w, choices);
    // OD-181: the mods this player declared, under the actions (shown, never a reason to leave): the catalog's name, else the id
    y += static_cast<float>(choices.size()) * 60 * s + 10 * s;
    TextAt(dl, f.tag, ImVec2(x, y), kTextDim, T("MODS", "MODS"));
    y += 24 * s;
    const auto *mods = g_roster.Of(g_mine_world, g_member.xuid);
    if (!mods || mods->empty()) {
        TextAt(dl, f.small, ImVec2(x, y), kTextDim, T("None", "Nenhum"));
        return;
    }
    std::string list;
    for (const auto &mod : *mods) {
        if (!list.empty()) list += "  ·  ";
        list += mod.id + " v" + mod.version;
    }
    Wrapped(dl, f.small, ImVec2(x, y), w, kText, list.c_str());
}

std::vector<Choice> ReportChoices() {
    return {{1, T("Cheating", "Trapaça"), true}, {2, T("Abuse or harassment", "Abuso ou assédio"), true}, {3, T("Something else", "Outro motivo"), true},
            {0, T("Back", "Voltar"), true}};
}

void DrawReport(ImDrawList *dl, float x, float y, float w) {
    y = Panel(dl, x, y, T("Report a player", "Denunciar um jogador"));
    TextAt(dl, f.body, ImVec2(x, y), kText, g_member.gamertag.c_str());
    y += 34 * s;
    const char *text = T("Why are you reporting this player? Moderators review every report.", "Por que você está denunciando este jogador? Os moderadores analisam cada denúncia.");
    Wrapped(dl, f.small, ImVec2(x, y), w, kTextDim, text);
    y += WrappedHeight(f.small, w, text) + 18 * s;
    Buttons(dl, x, y, w, ReportChoices());
}

// Leave lobby's confirmation (the only one left, T-L3).
void DrawConfirm(ImDrawList *dl, float x, float y, float w) {
    const char *text = T("Your seat is let go. Leave the online mode in the game's menu too.", "Seu lugar é liberado. Saia também do modo online no menu do jogo.");
    y = Panel(dl, x, y, T("Leave this lobby?", "Sair deste lobby?"));
    Wrapped(dl, f.body, ImVec2(x, y), w, kText, text);
    y += WrappedHeight(f.body, w, text) + 24 * s;
    Buttons(dl, x, y, w, {{1, T("Leave lobby", "Sair do lobby"), true}, {0, T("Back", "Voltar"), true}});
}

// The lobby stays in the guide while a player's details are in the column. The controller is in the column; the mouse
// still works here: another player's card shows their details instead, a mute button mutes, anything else goes back.
void DrawMineBehind(ImDrawList *dl, float x, float y, float w, float bottom, const Shared &sh) {
    const MineClick click = DrawMine(dl, x, y, w, bottom, sh.mine);
    if (click.item < 0) return;
    const int player = click.item - FirstPlayerItem();
    if (player < 0 || player >= static_cast<int>(sh.mine.members.size())) {
        g_screen = Screen::List;
        g_mine_focus = click.item;
        g_mine_col = layout::kStopCard;
        g_button = 0;
        g_accept = true;
        return;
    }
    const MemberRow &m = sh.mine.members[static_cast<size_t>(player)];
    g_mine_focus = click.item;
    g_mine_col = click.col;
    if (click.col == layout::kStopMute) {
        ToggleVoice(m, CurrentVoice());
    } else if (m.friend_state != 4 && m.xuid != g_member.xuid) {
        g_member = m;
        g_button = 0;
        g_screen = Screen::Member;
    }
}
} // namespace

bool WorldsColumnOpen() { return g_screen == Screen::Member || g_screen == Screen::Report; }

void DrawWorldsColumn(ImDrawList *dl, float x, float w, float height, float alpha) {
    const float pad = 26 * s, cw = w - 2 * pad;
    if (cw < 120 * s) return;
    ColumnPanel(dl, x, w, height, alpha);
    dl->PushClipRect(ImVec2(x, 0), ImVec2(x + w, height - 70 * s), true);
    const float y = 34 * s;
    if (g_screen == Screen::Member) DrawMember(dl, x + pad, y, cw);
    else DrawReport(dl, x + pad, y, cw);
    dl->PopClipRect();
    const float hy = height - 46 * s;
    dl->AddLine(ImVec2(x + pad, hy - 16 * s), ImVec2(x + w - pad, hy - 16 * s), Rgba(255, 255, 255, 0.08f * alpha));
    FaceButton(dl, ImVec2(x + pad + 11 * s, hy + 10 * s), 11 * s, kGreen, "A");
    TextAt(dl, f.small, ImVec2(x + pad + 28 * s, hy + 1 * s), kTextDim, T("Select", "Selecionar"), alpha);
    FaceButton(dl, ImVec2(x + pad + 139 * s, hy + 10 * s), 11 * s, kRed, "B");
    TextAt(dl, f.small, ImVec2(x + pad + 156 * s, hy + 1 * s), kTextDim, T("Back", "Voltar"), alpha);
}

// ---------------------------------------------------------------- public
void SetWorldAccess(bool allowed, const std::string &message) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.access.Observe(allowed, GetTickCount64());
    g_shared.access_message = message;
}

void SetMyWorld(MyWorld mine) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_official.store(mine.role != 0 && mine.official, std::memory_order_relaxed); // OD-192
    g_private.store(mine.role != 0 && mine.official && mine.is_private, std::memory_order_relaxed); // OD-204
    g_shared.mine = std::move(mine);
    ++g_shared.mine_serial;
}

void SetRosterMods(RosterMods mods) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.roster_mods = std::move(mods);
}

void SetActionResult(uint8_t action, uint8_t status, const std::string &message) {
    std::lock_guard<std::mutex> lock(g_lock);
    g_shared.action = action;
    g_shared.action_status = status;
    g_shared.action_message = message;
    ++g_shared.action_serial;
}

void WorldsReset() {
    // the guide opened again: back to the lobby; the lobby asked at once
    if (g_screen != Screen::Busy) g_screen = Screen::List;
    g_mine_refresh = true;
}

bool WorldsCanRefresh() { return g_screen == Screen::List && WorldAccessAllowed(); }

bool WorldsOfficial() { return g_official.load(std::memory_order_relaxed); }

bool WorldsPrivate() { return g_private.load(std::memory_order_relaxed); }

bool WorldsMuteHint() { return g_screen == Screen::List && g_showing_mine && g_mine_focus >= FirstPlayerItem(); }

bool WorldsNavigate(Nav nav) {
    switch (g_screen) {
    case Screen::List:
        if (g_showing_mine) {
            const bool on_player = g_mine_focus >= FirstPlayerItem();
            // A player is two stops side by side (ui_layout.h): the card (A opens the details) and the mute button
            // (A mutes). Up/Down keep the column between players; Right on a card goes to its mute button.
            layout::LobbyFocus lf{g_mine_focus, g_mine_col};
            const auto move = [&](layout::Move m) {
                const bool used = layout::MoveLobbyFocus(lf, m, FirstPlayerItem(), g_mine_count);
                g_mine_focus = lf.item;
                g_mine_col = lf.col;
                return used;
            };
            switch (nav) {
            case Nav::Up: return move(layout::Move::Up);
            case Nav::Down: return move(layout::Move::Down);
            case Nav::Right: return move(layout::Move::Right); // false: the tabs
            case Nav::Left: return move(layout::Move::Left);
            case Nav::X: if (on_player) { g_mine_x = true; return true; } return false;
            case Nav::Accept: g_accept = true; return true;
            case Nav::Y: g_mine_refresh = true; return true; // refresh
            default: return false; // the tabs, close
            }
        }
        if (nav == Nav::Y) { g_mine_refresh = true; return true; } // out of a lobby: Y asks again
        return false;
    case Screen::Busy: return true; // waits for the launcher
    case Screen::Member:
    case Screen::Report:
    case Screen::Confirm:
        switch (nav) {
        case Nav::Up: if (g_button > 0) --g_button; return true;
        case Nav::Down: if (g_button + 1 < g_buttons) ++g_button; return true;
        case Nav::Left:
        case Nav::Right: // the details' volume row
            if (g_screen == Screen::Member) StepMemberVolume(nav == Nav::Right ? 1 : -1);
            return true;
        case Nav::Accept: g_accept = true; return true;
        case Nav::Back:
            g_screen = g_screen == Screen::Report ? Screen::Member : Screen::List;
            g_button = 0;
            return true;
        default: return true;
        }
    }
    return false;
}

void DrawWorldsTab(ImDrawList *dl, float x, float y, float w, float bottom, float dt) {
    dl->PushClipRect(ImVec2(x, y), ImVec2(x + w, bottom), true);
    Shared sh;
    {
        std::lock_guard<std::mutex> lock(g_lock);
        sh = g_shared;
    }
    g_roster = sh.roster_mods; // OD-181: the cards and the details column read it this frame
    const bool world_access = DevHost() || sh.access.Allowed(GetTickCount64());
    if (g_dev_wait >= 0) {
        g_dev_wait -= dt;
        if (g_dev_wait < 0) DevAnswer();
    }
    // The launcher owns active/pending reconciliation. An open tab must not create a second poll loop.
    if (g_mine_refresh) {
        g_mine_refresh = false;
        if (DevHost()) {
            if (g_dev_wait < 0) { g_dev_wait = 0.05f; g_dev_kind = 6; }
        } else {
            SendMyWorldQuery();
        }
    }
    if (sh.mine_serial != g_seen_mine) {
        g_seen_mine = sh.mine_serial;
        if (sh.mine.role == 0 && (g_screen == Screen::Member || g_screen == Screen::Report || g_screen == Screen::Confirm)) {
            g_screen = Screen::List; // the world ended under the card
        }
        if (g_screen == Screen::Member) {
            // the card follows the player (friend state, role); gone: back to the world
            const auto it = std::find_if(sh.mine.members.begin(), sh.mine.members.end(), [](const MemberRow &m) { return m.xuid == g_member.xuid; });
            if (it == sh.mine.members.end()) g_screen = Screen::List; else g_member = *it;
        }
    }
    if (sh.action_serial != g_seen_action) {
        g_seen_action = sh.action_serial;
        if (g_screen == Screen::Busy) {
            g_screen = Screen::List;
            PushToast(sh.action_status == 0 ? ToastKind::Success : ToastKind::Warning,
                      sh.action_status == 0 ? T("Done", "Pronto") : T("Not done", "Não foi feito"), sh.action_message);
            g_mine_refresh = true;
        }
    }

    const bool accept = g_accept;
    g_accept = false;
    if (!world_access && sh.mine.role != 0) {
        const char *message = AccessMessage(sh);
        Wrapped(dl, f.body, ImVec2(x, y), w, kAmber, message);
        y += WrappedHeight(f.body, w, message) + 16 * s;
    }
    switch (g_screen) {
    case Screen::List:
        g_showing_mine = sh.mine.role != 0;
        if (!g_showing_mine) {
            g_mine_buttons.clear();
            DrawNoLobby(dl, x, y, w, sh);
            break;
        }
        {
            const MineClick click = DrawMine(dl, x, y, w, bottom, sh.mine);
            if (click.item >= 0) { g_mine_focus = click.item; g_mine_col = click.col; g_accept = true; } // A next frame
            g_mine_focus = std::clamp(g_mine_focus, 0, std::max(0, g_mine_count - 1)); // OD-196: item 0, Leave, in every seated lobby
            const int player = g_mine_focus - FirstPlayerItem();
            const bool x_press = g_mine_x;
            g_mine_x = false;
            if (x_press && player >= 0 && player < static_cast<int>(sh.mine.members.size())) {
                ToggleVoice(sh.mine.members[player], CurrentVoice()); // X: mute from the row, without the card
            }
            if (accept) {
                if (g_mine_focus < FirstPlayerItem()) {
                    switch (g_mine_buttons[static_cast<size_t>(g_mine_focus)]) {
                    case MineButton::Leave: // OD-196, A12: the Leave confirmation in every seated lobby
                        g_button = 0;
                        g_screen = Screen::Confirm;
                        break;
                    case MineButton::Invite: InviteFriends(sh.mine); break; // enabled for the leader only (A3)
                    }
                } else if (player >= 0 && player < static_cast<int>(sh.mine.members.size()) && g_mine_col == layout::kStopMute) {
                    ToggleVoice(sh.mine.members[player], CurrentVoice());
                } else if (player >= 0 && player < static_cast<int>(sh.mine.members.size())) {
                    const MemberRow &m = sh.mine.members[player];
                    if (m.friend_state != 4) {
                        g_member = m;
                        g_button = 0;
                        g_screen = Screen::Member;
                    }
                }
            }
        }
        break;
    case Screen::Member: {
        DrawMineBehind(dl, x, y, w, bottom, sh); // the details are in the column (DrawWorldsColumn)
        if (accept && g_screen == Screen::Member) {
            const auto choices = MemberChoices(g_member);
            if (g_button < static_cast<int>(choices.size()) && choices[g_button].enabled) {
                switch (choices[g_button].action) {
                case 4: Act(4, g_member, 0); break;
                case 6: ToggleVoice(g_member, CurrentVoice()); break; // the card stays: its label follows
                case 5: g_button = 0; g_screen = Screen::Report; break;
                case kChoiceVolume: break; // Left/Right move it
                default: g_screen = Screen::List; break;
                }
            }
        }
        break;
    }
    case Screen::Report: {
        DrawMineBehind(dl, x, y, w, bottom, sh);
        if (accept && g_screen == Screen::Report) {
            const auto choices = ReportChoices();
            const int reason = g_button < static_cast<int>(choices.size()) ? choices[g_button].action : 0;
            if (reason == 0) { g_button = 0; g_screen = Screen::Member; } else { Act(5, g_member, static_cast<uint8_t>(reason)); }
        }
        break;
    }
    case Screen::Confirm:
        DrawConfirm(dl, x, y, w);
        if (accept && g_screen == Screen::Confirm) {
            if (g_button == 0) Act(2, MemberRow{}, 0); // leave
            else g_screen = Screen::List;
            g_button = 0;
        }
        break;
    case Screen::Busy: DrawBusy(dl, x, y, w); break;
    }
    dl->PopClipRect();
}

} // namespace fi
