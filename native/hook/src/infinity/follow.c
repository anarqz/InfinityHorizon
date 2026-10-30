/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Following the host's activity (OD-151). When a seated guest's host goes from online free roam to
   an online Road Trip (or back), the launcher sends FollowHost{lobby type, public} (launch.c). The
   guest's game then starts its own matchmaking for that lobby type, exactly as the pause menu's
   ONLINE ROAD TRIP / ONLINE FREE ROAM does: the game shows its FINDING SESSION banner and the player
   still confirms with (X). Nothing here moves a player by itself.

   EntityModel::NetworkMatchmakingModule (vftable game+0x2cec248) is updated on the game thread by
   slot 4, Update (game+0x808b60, void Update(module *, float)). Its start handler, the end of the
   menu's action, is OnStartMatchmaking (game+0x815880, void (module *, const
   StartMatchmakingEventArgument *)): busy, it queues the request (module+0x150, replacing a queued
   one); otherwise it starts at once. The argument is 12 bytes: u8 lobby type (1 Road Trip, 2 online
   free roam), u8 public, s32 location, s32 parent bucket (-1 and -1, as the menu's).

   Slot 4 points to a detour (only when Update's and the handler's first bytes and the slot are as
   expected). The pipe reader stores the asked lobby type; the detour, on the game thread, starts
   the matchmaking once, unless the game is already in that lobby type (a matchmaking entity at
   module+0xc0 and the last started lobby type at module+0x120, which FUN_140816640 writes) or has
   already queued it (the queued argument's first byte), then runs the game's own Update. A request
   older than FH_PENDING_MS, or one that arrives when the launcher session is no longer live, is
   dropped. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define FH_MODULE_VTABLE_RVA 0x2cec248u
#define FH_UPDATE_SLOT 4u
#define FH_UPDATE_RVA 0x808b60u
#define FH_START_RVA 0x815880u
#define FH_ENTITY_OFFSET 0xc0u /* the module's matchmaking entity (std::shared_ptr), null when none */
#define FH_LOBBY_OFFSET 0x120u /* u8: the lobby type of the last start */
#define FH_QUEUED_OFFSET 0x150u /* the queued StartMatchmakingEventArgument, null when none */
#define FH_PENDING_MS 30000u
#define FH_LOBBY_ROAD_TRIP 1u
#define FH_LOBBY_FREE_ROAM 2u

/* push rbp; push rbx; push rsi; push rdi; push r12; push r14; push r15; lea rbp,[rsp-27h] */
static const unsigned char fh_update_bytes[16] = {0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41,
                                                  0x56, 0x41, 0x57, 0x48, 0x8d, 0x6c, 0x24, 0xd9};
/* push r14; sub rsp,50h; mov qword [rsp+20h],-2; ... */
static const unsigned char fh_start_bytes[16] = {0x41, 0x56, 0x48, 0x83, 0xec, 0x50, 0x48, 0xc7,
                                                 0x44, 0x24, 0x20, 0xfe, 0xff, 0xff, 0xff, 0x48};

typedef struct {
    unsigned char lobby;
    unsigned char is_public;
    unsigned char reserved[2];
    int location;
    int parent_bucket;
} FhStartArgument;
typedef char fh_start_argument_is_12_bytes[sizeof(FhStartArgument) == 12 ? 1 : -1];

typedef void (*FhUpdateFn)(void *module, float dt);
typedef void (*FhStartFn)(void *module, const FhStartArgument *argument);

static unsigned char *fh_game;
static FhUpdateFn fh_update_original;
static volatile LONG fh_tried;
/* the asked follow: 0 none, else 0x10000 | public << 8 | lobby type; fh_pending_at its tick */
static volatile LONG fh_pending;
static volatile LONG64 fh_pending_at;


/* The game is already in `lobby` (its matchmaking entity is there and its last start was that
   lobby type), or has already queued a start of it. */
static int fh_already(unsigned char *module, unsigned lobby) {
    unsigned char *queued = 0, *entity = 0, current = 0;
    if (memory_read_pointer(module + FH_QUEUED_OFFSET, (void **)&queued)) {
        unsigned char queued_lobby = 0;
        /* another lobby type queued: a new start replaces it, as the menu's does */
        return memory_read(queued, &queued_lobby, 1) && queued_lobby == lobby;
    }
    return memory_read_pointer(module + FH_ENTITY_OFFSET, (void **)&entity) &&
           memory_read(module + FH_LOBBY_OFFSET, &current, 1) && current == lobby;
}

static void fh_update(void *module, float dt) {
    LONG pending = fh_pending ? InterlockedExchange(&fh_pending, 0) : 0;
    if (pending) {
        unsigned lobby = (unsigned)pending & 0xffu, is_public = ((unsigned)pending >> 8) & 1u;
        if (GetTickCount64() - (ULONGLONG)fh_pending_at > FH_PENDING_MS || !fi_launch_live() || fi_launch_suspended()) {
            log_append("FOLLOW: a host change came too late or online is closed; dropped.\r\n");
        } else if (fh_already(module, lobby)) {
            log_append(lobby == FH_LOBBY_ROAD_TRIP ? "FOLLOW: the host is in Online Road Trip; the game is already there.\r\n"
                                                   : "FOLLOW: the host is in Online Free Roam; the game is already there.\r\n");
        } else {
            FhStartArgument argument = {(unsigned char)lobby, (unsigned char)is_public, {0, 0}, -1, -1};
            ((FhStartFn)(uintptr_t)(fh_game + FH_START_RVA))(module, &argument);
            log_append(lobby == FH_LOBBY_ROAD_TRIP
                           ? "FOLLOW: the host is in Online Road Trip: matchmaking started (the player confirms).\r\n"
                           : "FOLLOW: the host is in Online Free Roam: matchmaking started (the player confirms).\r\n");
        }
    }
    fh_update_original(module, dt);
}

/* FollowHost from the launcher (launch.c's reader; lobby 1 or 2, already validated). */
void fi_follow_host(unsigned lobby, unsigned is_public) {
    if (lobby != FH_LOBBY_ROAD_TRIP && lobby != FH_LOBBY_FREE_ROAM) {
        return;
    }
    if (!fh_update_original) {
        log_append("FOLLOW: the host changed activity, but the matchmaking module is not patched here; ignored.\r\n");
        return;
    }
    InterlockedExchange64(&fh_pending_at, (LONG64)GetTickCount64());
    InterlockedExchange(&fh_pending, (LONG)(0x10000u | (is_public ? 0x100u : 0u) | lobby));
}

void fi_follow_apply(unsigned char *game_image) {
    if (InterlockedCompareExchange(&fh_tried, 1, 0) != 0) {
        return;
    }
    fh_game = game_image;
    unsigned char update[sizeof(fh_update_bytes)], start[sizeof(fh_start_bytes)];
    void **slot = (void **)(game_image + FH_MODULE_VTABLE_RVA + FH_UPDATE_SLOT * sizeof(void *));
    void *current = 0;
    DWORD old_protection, ignored;
    if (!memory_read(game_image + FH_UPDATE_RVA, update, sizeof(update)) ||
        !memory_equal(update, fh_update_bytes, sizeof(update)) ||
        !memory_read(game_image + FH_START_RVA, start, sizeof(start)) ||
        !memory_equal(start, fh_start_bytes, sizeof(start)) || !memory_read(slot, &current, sizeof(current)) ||
        current != game_image + FH_UPDATE_RVA ||
        !VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old_protection)) {
        fh_game = 0;
        log_append("FOLLOW: the matchmaking module is not as expected; left as is (a host's change of activity is "
                   "only shown).\r\n");
        return;
    }
    fh_update_original = (FhUpdateFn)(uintptr_t)current;
    *slot = (void *)(uintptr_t)fh_update;
    VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
    log_append("FOLLOW: a host's change of activity starts the game's own matchmaking (the player confirms).\r\n");
}
