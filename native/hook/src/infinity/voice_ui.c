/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Voice chat in the game's own UI (OD-147, VC-2b; docs/launcher/VOICE_CHAT.md in the monorepo,
   RE: deprecated/beta/notes/local-docs/re/voice-chat.md). FH2's player list (A_OnlinePlayerList,
   the car meet list) and its HUD mini leaderboard (HUDStatusMiniLeaderboard, top right) draw a
   microphone per player from three queries the game polls every frame. GameChat never runs on
   Azimuth, so they answer "no voice"; here they answer from the overlay's voice chat
   (fi_overlay_voice_state, an optional export), and the game's Mute/Unmute action on a player
   goes to the overlay (fi_overlay_voice_mute):

   - IsTalking(CLobbyPlayer *) (game+0xa2cb40): the whole function is replaced. The player's
     XUID comes from the table below (or its talker, +0x980, XUID at +0x10); with no overlay
     answer the game's own rule runs (PlayerType, vtable +0x90, zero and the talker's +0x79 set).
   - IsMuted(CLobbyPlayer *local, xuid) (game+0xa2c930): a detour; the original without an answer.
   - HasVoiceAbility: CLobbyPlayer's vtable slot +0x60 (vtable game+0x2d3c940), whose function
     (game+0x17fc680, HeadsetConnected, +0xc8) is shared by many classes: only this slot changes.
   - the talker attach (game+0xa2f930, CLobbyPlayer *, participant *), called for the local
     player when a session starts and for every remote one that joins: its participant's XUID
     (vtable +0x40) and whether it is the local player (+0x128) go into the table, then the
     original runs.
   - the user actions' Mute (game+0xa2d820) and Unmute (game+0xa32180) of the local player's
     PlayerChatStates (FUN_140e1e920, actions 2 and 4): the original runs, then the overlay.

   The local player is XUID 0 for the overlay (it does not know it). Off unless Infinity.cfg
   [hook] VoiceUi=1: nobody has seen this in the running game yet. Installed once, after the game
   build (TimeDateStamp, SizeOfImage) and every byte it replaces or relies on were checked; any
   mismatch leaves the whole game untouched. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define VU_IS_TALKING_RVA 0xa2cb40u
#define VU_IS_MUTED_RVA 0xa2c930u
#define VU_ATTACH_RVA 0xa2f930u
#define VU_MUTE_RVA 0xa2d820u
#define VU_UNMUTE_RVA 0xa32180u
#define VU_LOBBY_VTABLE_RVA 0x2d3c940u /* CLobbyPlayer::vftable */
#define VU_ABILITY_SLOT (0x60u / 8u)
#define VU_ABILITY_RVA 0x17fc680u
#define VU_CHAT_STATE_SLOT (0x1e0u / 8u)
#define VU_CHAT_STATE_RVA 0x180cd60u /* LobbyPlayer: this + 0x3b8 + i * 0x70 */
#define VU_PLAYER_TYPE_SLOT (0x90u / 8u)
#define VU_PARTICIPANT_XUID_SLOT (0x40u / 8u)
#define VU_PARTICIPANT_LOCAL_SLOT (0x128u / 8u)
#define VU_TALKER_OFFSET 0x980u
#define VU_TALKER_XUID_OFFSET 0x10u
#define VU_TALKER_TALKING_OFFSET 0x79u
#define VU_TABLE_SIZE 32u

/* IsTalking, all of it: push rbx; sub rsp,20h; mov rax,[rcx]; mov rbx,rcx; call [rax+90h];
   test al,al; jnz no; mov rcx,[rbx+980h]; test rcx,rcx; jz no; call talker_talking; test al,al;
   jz no; mov al,1; add rsp,20h; pop rbx; ret; no: xor al,al; add rsp,20h; pop rbx; ret */
static const unsigned char vu_is_talking_bytes[59] = {
    0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0x01, 0x48, 0x8b, 0xd9, 0xff, 0x90, 0x90, 0x00, 0x00, 0x00,
    0x84, 0xc0, 0x75, 0x1d, 0x48, 0x8b, 0x8b, 0x80, 0x09, 0x00, 0x00, 0x48, 0x85, 0xc9, 0x74, 0x11, 0xe8, 0x89,
    0x5d, 0x9d, 0xff, 0x84, 0xc0, 0x74, 0x08, 0xb0, 0x01, 0x48, 0x83, 0xc4, 0x20, 0x5b, 0xc3, 0x32, 0xc0, 0x48,
    0x83, 0xc4, 0x20, 0x5b, 0xc3};
/* The call above is a "byte at +0x79" getter the linker shares with other classes, and one that
   ExtraOptions patches (a treasure map property): it is neither checked nor called here; the
   fallback reads the talker's +0x79 itself. */
/* IsMuted up to its local-talker fallback: the 16-byte prologue (mov [rsp+8],rbx;
   mov [rsp+18h],rsi; mov [rsp+10h],rdx; push rdi) and the loop over the 12 PlayerChatStates
   (vtable +0x1e0, Xuid +0x10, Muted +0x20) */
static const unsigned char vu_is_muted_bytes[133] = {
    0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x18, 0x48, 0x89, 0x54, 0x24, 0x10, 0x57, 0x48, 0x83,
    0xec, 0x30, 0x48, 0x8b, 0xf1, 0x33, 0xdb, 0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00, 0x48, 0x8b, 0x06, 0x8b,
    0xd3, 0x48, 0x8b, 0xce, 0xff, 0x90, 0xe0, 0x01, 0x00, 0x00, 0x48, 0x8b, 0xc8, 0x48, 0x8b, 0xf8, 0x48, 0x8b,
    0x10, 0xff, 0x52, 0x10, 0x48, 0x3b, 0x44, 0x24, 0x48, 0x74, 0x19, 0xff, 0xc3, 0x83, 0xfb, 0x0c, 0x72, 0xd8,
    0x32, 0xc0, 0x48, 0x8b, 0x5c, 0x24, 0x40, 0x48, 0x8b, 0x74, 0x24, 0x50, 0x48, 0x83, 0xc4, 0x30, 0x5f, 0xc3,
    0x48, 0x8b, 0x07, 0x48, 0x8b, 0xcf, 0xff, 0x50, 0x20, 0x84, 0xc0, 0x74, 0x12, 0xb0, 0x01, 0x48, 0x8b, 0x5c,
    0x24, 0x40, 0x48, 0x8b, 0x74, 0x24, 0x50, 0x48, 0x83, 0xc4, 0x30, 0x5f, 0xc3, 0x48, 0x8b, 0x8e, 0x80, 0x09,
    0x00, 0x00, 0x48, 0x85, 0xc9, 0x74, 0xc3};
/* the talker attach up to its talker test: mov [rsp+10h],rbx; push rdi; sub rsp,40h;
   mov rax,[rcx]; mov rbx,rdx (16, the prologue); mov rdi,rcx; call [rax+90h]; test al,al;
   jnz; cmp qword [rdi+980h],0 */
static const unsigned char vu_attach_bytes[41] = {
    0x48, 0x89, 0x5c, 0x24, 0x10, 0x57, 0x48, 0x83, 0xec, 0x40, 0x48, 0x8b, 0x01, 0x48, 0x8b, 0xda,
    0x48, 0x8b, 0xf9, 0xff, 0x90, 0x90, 0x00, 0x00, 0x00, 0x84, 0xc0, 0x0f, 0x85, 0xdc, 0x00, 0x00,
    0x00, 0x48, 0x83, 0xbf, 0x80, 0x09, 0x00, 0x00, 0x00};
/* Mute and Unmute up to the setter call: the 16-byte prologue (mov [rsp+8],rbx; mov [rsp+10h],rbp;
   mov [rsp+18h],rsi; push rdi), the loop over the 12 PlayerChatStates, then Muted (vtable +0x28)
   set to 1 (mov dl,1) or 0 (xor edx,edx) */
#define VU_MUTE_SIZE 84u
#define VU_MUTE_VALUE_AT 76u
static const unsigned char vu_mute_bytes[VU_MUTE_SIZE] = {
    0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57, 0x48,
    0x83, 0xec, 0x20, 0x48, 0x8b, 0xea, 0x48, 0x8b, 0xf1, 0x33, 0xdb, 0x0f, 0x1f, 0x40, 0x00, 0x48, 0x8b,
    0x06, 0x8b, 0xd3, 0x48, 0x8b, 0xce, 0xff, 0x90, 0xe8, 0x01, 0x00, 0x00, 0x48, 0x8b, 0xc8, 0x48, 0x8b,
    0xf8, 0x4c, 0x8b, 0x00, 0x41, 0xff, 0x50, 0x10, 0x48, 0x3b, 0xc5, 0x74, 0x09, 0xff, 0xc3, 0x83, 0xfb,
    0x0c, 0x72, 0xd9, 0xeb, 0x0b, 0x48, 0x8b, 0x07, 0xb2, 0x01, 0x48, 0x8b, 0xcf, 0xff, 0x50, 0x28};
static const unsigned char vu_unmute_value[2] = {0x33, 0xd2}; /* xor edx,edx where Mute has mov dl,1 */
/* HeadsetConnected: movzx eax,byte [rcx+0c8h]; ret */
static const unsigned char vu_ability_bytes[8] = {0x0f, 0xb6, 0x81, 0xc8, 0x00, 0x00, 0x00, 0xc3};

typedef unsigned char (*VuBoolFn)(void *object);
typedef uint64_t (*VuXuidFn)(void *object);
typedef unsigned char (*VuIsMutedFn)(void *local, uint64_t xuid);
typedef void (*VuAttachFn)(void *player, void *participant);
typedef void (*VuMuteFn)(void *local, uint64_t xuid);
typedef uint32_t (*VuStateFn)(uint64_t xuid);
typedef void (*VuOverlayMuteFn)(uint64_t xuid, int32_t muted);

static VuIsMutedFn vu_is_muted_original;
static VuAttachFn vu_attach_original;
static VuMuteFn vu_mute_original, vu_unmute_original;
static VuBoolFn vu_ability_original;

/* CLobbyPlayer -> XUID, filled by the talker attach. A player is written XUID first, then its
   pointer; read pointer, XUID, pointer again (the game's UI thread reads, its session code
   writes). Slots are reused round robin; an old CLobbyPlayer address comes back only through
   another attach, which rewrites it. */
typedef struct {
    void *volatile player;
    volatile uint64_t xuid;
    volatile LONG local;
} VuPlayer;
static VuPlayer vu_players[VU_TABLE_SIZE];
static volatile LONG vu_next;

static void vu_remember(void *player, uint64_t xuid, int local) {
    if (!player || (!xuid && !local)) return;
    unsigned slot = VU_TABLE_SIZE;
    for (unsigned i = 0; i < VU_TABLE_SIZE; ++i) {
        if (vu_players[i].player == player) {
            slot = i;
            break;
        }
    }
    if (slot == VU_TABLE_SIZE) {
        for (unsigned i = 0; i < VU_TABLE_SIZE && slot == VU_TABLE_SIZE; ++i) {
            if (!vu_players[i].player) slot = i;
        }
    }
    if (slot == VU_TABLE_SIZE) slot = (unsigned)InterlockedIncrement(&vu_next) % VU_TABLE_SIZE;
    InterlockedExchangePointer((void *volatile *)&vu_players[slot].player, 0);
    vu_players[slot].xuid = xuid;
    InterlockedExchange(&vu_players[slot].local, local ? 1 : 0);
    InterlockedExchangePointer((void *volatile *)&vu_players[slot].player, player);
}

/* The overlay's key for a CLobbyPlayer: 1 with *xuid (0 for the local player), 0 unknown. */
static int vu_player_xuid(void *player, uint64_t *xuid) {
    if (!player) return 0;
    for (unsigned i = 0; i < VU_TABLE_SIZE; ++i) {
        if (vu_players[i].player != player) continue;
        uint64_t value = vu_players[i].xuid;
        LONG local = vu_players[i].local;
        if (vu_players[i].player != player) break; /* rewritten meanwhile: ask the talker */
        *xuid = local ? 0 : value;
        return 1;
    }
    void *talker = 0;
    uint64_t value = 0;
    if (memory_read_pointer((unsigned char *)player + VU_TALKER_OFFSET, &talker) &&
        memory_read((unsigned char *)talker + VU_TALKER_XUID_OFFSET, &value, sizeof(value)) && value) {
        *xuid = value;
        return 1;
    }
    return 0;
}

/* The overlay's exports, once its DLL is started (it is never unloaded). */
static VuStateFn volatile vu_state_fn;
static VuOverlayMuteFn volatile vu_mute_fn;
static volatile LONG vu_resolved;

static void vu_resolve(void) {
    if (vu_resolved) return;
    if (!fi_launch_overlay_export("fi_overlay_start")) return; /* not started (yet) */
    vu_state_fn = (VuStateFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_voice_state");
    vu_mute_fn = (VuOverlayMuteFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_voice_mute");
    InterlockedExchange(&vu_resolved, 1);
}

/* FI_VOICE_* bits from the overlay (fi_overlay.h), 0 when there is no answer. */
#define VU_ABLE 0x1u
#define VU_TALKING 0x2u
#define VU_MUTED 0x4u
static uint32_t vu_state(uint64_t xuid) {
    vu_resolve();
    VuStateFn state = vu_state_fn;
    return state ? state(xuid) : 0;
}

static uint32_t vu_player_state(void *player) {
    uint64_t xuid = 0;
    return vu_player_xuid(player, &xuid) ? vu_state(xuid) : 0;
}

static void *vu_virtual(void *object, unsigned slot) {
    void **vtable = 0, *function = 0;
    return object && memory_read_pointer(object, (void **)&vtable) && memory_read_pointer(vtable + slot, &function)
               ? function
               : 0;
}

/* IsTalking (UI thread). */
static unsigned char vu_is_talking(void *player) {
    VuBoolFn player_type = (VuBoolFn)(uintptr_t)vu_virtual(player, VU_PLAYER_TYPE_SLOT);
    if (!player_type || player_type(player)) return 0; /* not a human player (or no object) */
    uint32_t state = vu_player_state(player);
    if (state) return (state & VU_TALKING) && !(state & VU_MUTED);
    void *talker = 0;
    unsigned char talking = 0;
    return memory_read_pointer((unsigned char *)player + VU_TALKER_OFFSET, &talker) &&
           memory_read((unsigned char *)talker + VU_TALKER_TALKING_OFFSET, &talking, 1) && talking;
}

/* IsMuted(local player, xuid). */
static unsigned char vu_is_muted(void *local, uint64_t xuid) {
    uint32_t state = xuid ? vu_state(xuid) : 0;
    if (state) return (state & VU_MUTED) != 0;
    return vu_is_muted_original(local, xuid);
}

/* CLobbyPlayer slot +0x60, HasVoiceAbility. */
static unsigned char vu_has_voice(void *player) {
    uint32_t state = vu_player_state(player);
    if (state) return (state & VU_ABLE) != 0;
    return vu_ability_original(player);
}

/* The talker attach: who this CLobbyPlayer is, then the game's own attach. */
static void vu_attach(void *player, void *participant) {
    VuXuidFn xuid = (VuXuidFn)(uintptr_t)vu_virtual(participant, VU_PARTICIPANT_XUID_SLOT);
    VuBoolFn local = (VuBoolFn)(uintptr_t)vu_virtual(participant, VU_PARTICIPANT_LOCAL_SLOT);
    if (player && xuid && local) {
        vu_remember(player, xuid(participant), local(participant) != 0);
    }
    vu_attach_original(player, participant);
}

static void vu_mute_to_overlay(uint64_t xuid, int muted) {
    vu_resolve();
    VuOverlayMuteFn mute = vu_mute_fn;
    if (mute && xuid) mute(xuid, muted);
}

/* The user actions' Mute and Unmute (local player, target XUID). */
static void vu_mute(void *local, uint64_t xuid) {
    vu_mute_original(local, xuid);
    vu_mute_to_overlay(xuid, 1);
}
static void vu_unmute(void *local, uint64_t xuid) {
    vu_unmute_original(local, xuid);
    vu_mute_to_overlay(xuid, 0);
}

static int vu_bytes(unsigned char *image, unsigned rva, const unsigned char *expected, size_t size) {
    unsigned char current[sizeof(vu_is_muted_bytes)];
    return size <= sizeof(current) && memory_read(image + rva, current, size) && memory_equal(current, expected, size);
}

/* The game build and every byte this file replaces or relies on: 1 when all may be taken. */
static int vu_image_ok(unsigned char *image) {
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    void *ability = 0, *chat_state = 0;
    unsigned char unmute[VU_MUTE_SIZE];
    if (!image || !memory_read(image, &dos, sizeof(dos)) || dos.e_magic != IMAGE_DOS_SIGNATURE ||
        dos.e_lfanew < 64 || dos.e_lfanew > 4096 || !memory_read(image + dos.e_lfanew, &nt, sizeof(nt)) ||
        nt.Signature != IMAGE_NT_SIGNATURE || nt.FileHeader.Machine != IMAGE_FILE_MACHINE_AMD64 ||
        nt.FileHeader.TimeDateStamp != GAME_TIMESTAMP || nt.OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC ||
        nt.OptionalHeader.SizeOfImage != GAME_IMAGE_SIZE) {
        return 0;
    }
    if (!memory_read(image + VU_UNMUTE_RVA, unmute, sizeof(unmute))) return 0;
    memory_copy(unmute + VU_MUTE_VALUE_AT, vu_mute_bytes + VU_MUTE_VALUE_AT, sizeof(vu_unmute_value));
    unsigned char value[sizeof(vu_unmute_value)];
    return vu_bytes(image, VU_IS_TALKING_RVA, vu_is_talking_bytes, sizeof(vu_is_talking_bytes)) &&
           vu_bytes(image, VU_IS_MUTED_RVA, vu_is_muted_bytes, sizeof(vu_is_muted_bytes)) &&
           vu_bytes(image, VU_ATTACH_RVA, vu_attach_bytes, sizeof(vu_attach_bytes)) &&
           vu_bytes(image, VU_MUTE_RVA, vu_mute_bytes, sizeof(vu_mute_bytes)) &&
           memory_equal(unmute, vu_mute_bytes, sizeof(unmute)) &&
           memory_read(image + VU_UNMUTE_RVA + VU_MUTE_VALUE_AT, value, sizeof(value)) &&
           memory_equal(value, vu_unmute_value, sizeof(value)) &&
           vu_bytes(image, VU_ABILITY_RVA, vu_ability_bytes, sizeof(vu_ability_bytes)) &&
           memory_read(image + VU_LOBBY_VTABLE_RVA + 8u * VU_ABILITY_SLOT, &ability, sizeof(ability)) &&
           ability == (void *)(image + VU_ABILITY_RVA) &&
           memory_read(image + VU_LOBBY_VTABLE_RVA + 8u * VU_CHAT_STATE_SLOT, &chat_state, sizeof(chat_state)) &&
           chat_state == (void *)(image + VU_CHAT_STATE_RVA);
}

/* 1 when everything is in place; 0 when nothing was changed (or, on a failed VirtualProtect
   after the checks, only what is in place: each part falls back to the game on its own). */
static int vu_install(unsigned char *image) {
    if (!vu_image_ok(image)) return 0;
    /* 4 trampolines of 16 + 14 bytes, and one never called (IsTalking is replaced whole) */
    unsigned char *code = VirtualAlloc(0, 5 * 32, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if (!code) return 0;
    vu_ability_original = (VuBoolFn)(uintptr_t)(image + VU_ABILITY_RVA);
    void *muted = memory_detour14(image + VU_IS_MUTED_RVA, vu_is_muted_bytes, 16, (const void *)(uintptr_t)vu_is_muted,
                                  code);
    if (!muted) return 0;
    vu_is_muted_original = (VuIsMutedFn)(uintptr_t)muted;
    void *attach = memory_detour14(image + VU_ATTACH_RVA, vu_attach_bytes, 16, (const void *)(uintptr_t)vu_attach,
                                   code + 32);
    if (!attach) return 0;
    vu_attach_original = (VuAttachFn)(uintptr_t)attach;
    void *mute = memory_detour14(image + VU_MUTE_RVA, vu_mute_bytes, 16, (const void *)(uintptr_t)vu_mute, code + 64);
    if (!mute) return 0;
    vu_mute_original = (VuMuteFn)(uintptr_t)mute;
    void *unmute = memory_detour14(image + VU_UNMUTE_RVA, vu_mute_bytes, 16, (const void *)(uintptr_t)vu_unmute,
                                   code + 96);
    if (!unmute) return 0;
    vu_unmute_original = (VuMuteFn)(uintptr_t)unmute;
    if (!memory_detour14(image + VU_IS_TALKING_RVA, vu_is_talking_bytes, 14, (const void *)(uintptr_t)vu_is_talking,
                         code + 128)) {
        return 0;
    }
    void **slot = (void **)(image + VU_LOBBY_VTABLE_RVA + 8u * VU_ABILITY_SLOT);
    DWORD old_protection, ignored;
    if (!VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old_protection)) return 0;
    *slot = (void *)(uintptr_t)vu_has_voice;
    VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
    return 1;
}

void fi_voice_ui_apply(unsigned char *game_image) {
    static long installed;
    if (!fi_cfg()->voice_ui) {
        log_append("VOICE UI: off (Infinity.cfg [hook] VoiceUi=0): the game's player list and HUD show no voice chat.\r\n");
        return;
    }
    if (InterlockedCompareExchange(&installed, 1, 0) != 0) return;
    if (!vu_image_ok(game_image)) {
        log_append("VOICE UI: the game's voice queries are not the supported build: left as they are.\r\n");
        return;
    }
    log_append(vu_install(game_image)
                   ? "VOICE UI: the game's player list and HUD show the overlay's voice chat (VoiceUi=1).\r\n"
                   : "VOICE UI: installing failed part way (VirtualAlloc or VirtualProtect); each part left falls back to the game.\r\n");
}
