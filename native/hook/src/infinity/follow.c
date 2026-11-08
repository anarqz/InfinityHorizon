/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The launcher's FollowHost move (OD-151's frame; OD-208: no player host is followed any more: an accepted
   invite, a private create and the anti-cheat move use it). The launcher sends FollowHost{lobby type,
   public} (launch.c); the game then starts its own matchmaking for that lobby type, exactly as the pause menu's
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
   dropped.

   Moving into an accepted invite's lobby (T-L5, OD-204; FollowHost's move flag, Capture 13). The launcher has already taken the
   seat in the private lobby; the game must leave its current session by its own matchmaking. fh_already would skip a game already
   in that lobby type (an invitee in public free roam: HANDOFF 11.8 M2), so a move skips only a queued start of the same type; it
   starts anyway when the game is connected in online free roam, or in nothing; it never starts when the game is connected with any
   other lobby type (a Road Trip, 1, or a car meet, 3 or 6: an event the player is in), which is logged and left to the launcher's
   bound (its seat is let go there). Nothing else changes: the player still confirms with (X), and a stale or closed
   request is dropped as any other. T-1108-J: a move's start that the game queued behind its own running search (module+0x150
   while +0xc0 lives) gets the game's disconnect for that search, so the queued start runs (fh_move_unpark).

   Giving up (CancelMatchmaking, 0x24). When the launcher gave up on a lobby ask's seat (it did not
   start playing within 15 s), the same detour, on the module's next Update, sends the game's own
   cancel the way Update itself does (game+0x80905e): the no-data queued send FUN_141903d50 (game+
   0x1903d50, void (module *, const event descriptor *)) with NetworkMatchmakingRequestDisconnect's
   descriptor (game+0x34bdc28, a static in .data's uninitialized part). A start queued meanwhile
   (module+0x150, a heap StartMatchmakingEventArgument) is freed first with the game's operator
   delete (game+0x31c7b0) and the slot zeroed, so it cannot fire later. The disconnect is sent only
   while the module has a matchmaking entity (module+0xc0 non-null), as the game's own senders check.
   Only when the send's and the delete's first 16 bytes are as expected; otherwise the request is only
   logged.

   Diagnostics: a queued start (module+0x150) that stays for more than FH_QUEUED_LOG_MS is logged
   once with what keeps it parked (the entity at +0xc0, bytes +0x15c, +0x15d, +0x170, dwords +0x134,
   +0x130, +0x124). Read only.

   T-1108-J diagnostics (static RE of the FH2 Durango project, read only). Exactly three writers set the entity at +0xc0:
   FUN_140816640 (a start: from the lobby map at +0xf0, +0x120 = its lobby type; called by OnStartMatchmaking and the network
   update FUN_1407d4790), FUN_1408183f0 (a session join: from +0xe0, +0x120 = 7; called by Update's +0x134/+0x130 branches and
   FUN_140818340, which joins the Party's game session) and Update's +0x170 branch (a restart: from +0xd0). The senders of
   NetworkStartMatchmaking (descriptor game+0x34bdaf8) are menus (PauseMenuTiled's callback FUN_140dcaf80, FUN_1408bc120,
   FUN_140d883b0, FUN_140715eb0) and FUN_1407d4790; the Party statics slot 6 caller (FUN_141791000) only sets the Party's own
   state. So a search that starts outside the menus is a session join that follows the Party manager (DAT_1436e2958: +0x2e8 in a
   party, +0x30 its game session, +0x48 and +0x50 the gates Update reads). The detour logs, read only and at most once a second,
   each change of the entity (started, with its source told by which field it was copied from, or ended) or of the queued start,
   the whole state at FollowHost, at the move's unpark disconnect and 1 s after it. */
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
#define FH_SEND_RVA 0x1903d50u       /* the module's no-data queued send of an event */
#define FH_DISCONNECT_RVA 0x34bdc28u /* NetworkMatchmakingRequestDisconnect's event descriptor */
#define FH_DELETE_RVA 0x31c7b0u      /* the game's operator delete */
#define FH_QUEUED_LOG_MS 5000u
#define FH_RESTART_OFFSET 0xd0u /* the entity Update's +0x170 branch restarts (shared_ptr) */
#define FH_JOIN_OFFSET 0xe0u    /* the entity FUN_1408183f0 joins a session with (shared_ptr) */
#define FH_TARGET_OFFSET 0x140u /* the session FUN_140817030 stores to join, null when none */
#define FH_PARTY_RVA 0x36e2958u /* the Party manager pointer Update's gates read */
#define FH_WATCH_MS 1000u       /* at most one state-change line a second */
#define FH_UNPARK_CHECK_MS 1000u

/* push rbp; push rbx; push rsi; push rdi; push r12; push r14; push r15; lea rbp,[rsp-27h] */
static const unsigned char fh_update_bytes[16] = {0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41,
                                                  0x56, 0x41, 0x57, 0x48, 0x8d, 0x6c, 0x24, 0xd9};
/* push r14; sub rsp,50h; mov qword [rsp+20h],-2; ... */
static const unsigned char fh_start_bytes[16] = {0x41, 0x56, 0x48, 0x83, 0xec, 0x50, 0x48, 0xc7,
                                                 0x44, 0x24, 0x20, 0xfe, 0xff, 0xff, 0xff, 0x48};
/* mov [rsp+8],rbx; push rdi; sub rsp,40h; mov rdi,rdx; lea rdx,[rsp+20h]... */
static const unsigned char fh_send_bytes[16] = {0x48, 0x89, 0x5c, 0x24, 0x08, 0x57, 0x48, 0x83,
                                                0xec, 0x40, 0x48, 0x8b, 0xfa, 0x48, 0x8d, 0x54};
/* test rcx,rcx; je +1d; push rbx; sub rsp,20h; mov rbx,rcx; call ... */
static const unsigned char fh_delete_bytes[16] = {0x48, 0x85, 0xc9, 0x74, 0x1d, 0x53, 0x48, 0x83,
                                                  0xec, 0x20, 0x48, 0x8b, 0xd9, 0xe8, 0xbe, 0x2b};

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
typedef void (*FhSendFn)(void *module, const void *event);
typedef void (*FhDeleteFn)(void *block);

static unsigned char *fh_game;
static FhUpdateFn fh_update_original;
static volatile LONG fh_tried;
/* the asked follow: 0 none, else 0x10000 | move << 17 | public << 8 | lobby type; fh_pending_at its tick */
#define FH_PENDING_MOVE 0x20000u
static volatile LONG fh_pending;
static volatile LONG64 fh_pending_at;
/* CancelMatchmaking: fh_cancel_ready once the send's and the delete's bytes were as expected;
   fh_cancel asked (fh_cancel_at its tick) */
static int fh_cancel_ready, fh_delete_ready;
static volatile LONG fh_cancel;
/* the lobby type (OpenConnection's A) the game last connected its world with, 0 none (its session
   left); set by the joiner's proxy thread, read on the game thread */
static volatile LONG fh_connected;
/* the tick of the game's last OpenConnection: an association destroyed within FH_LEFT_MS of one is
   the old connection going away after the new one opened (live, test.9-11), never the game leaving */
static volatile LONG64 fh_connected_at;
#define FH_LEFT_MS 3000u
static volatile LONG64 fh_cancel_at;
/* W-10 Q4: when the game thread ran the release (0 none) and which of its after-checks (3 s, 10 s)
   were logged: a game that freezes after a release never writes them. Game thread only. */
static ULONGLONG fh_released_at;
static int fh_released_logged;
/* the queued start last seen at module+0x150, since when, and whether it was logged */
static void *fh_queued_seen;
static ULONGLONG fh_queued_since;
static int fh_queued_logged;
/* T-1108-J diagnostics, game thread only: the entity and queued start last logged, when, whether the hook's own start ran since,
   and when the move's unpark disconnect was sent (0 none) */
static void *fh_watch_entity, *fh_watch_queued;
static ULONGLONG fh_watch_at;
static int fh_own_start;
static ULONGLONG fh_unpark_at;

/* A bounded log line: text and unsigned decimals appended to `line` (capacity FH_LINE). */
#define FH_LINE 400u
static void fh_put(char *line, size_t *n, const char *text) {
    while (*text && *n + 1 < FH_LINE) line[(*n)++] = *text++;
    line[*n] = 0;
}
static void fh_put_number(char *line, size_t *n, unsigned value) {
    char digits[10];
    int d = 0;
    do {
        digits[d++] = (char)('0' + value % 10u);
        value /= 10u;
    } while (value && d < 10);
    while (d && *n + 1 < FH_LINE) line[(*n)++] = digits[--d];
    line[*n] = 0;
}

/* T-1108-J: the module's matchmaking state, read only (see the header). source: 0 no entity, 1 a start (the lobby map), 2 a
   session join (+0xe0), 3 a restart (+0xd0). party: 1 when the Party manager pointer was readable and set. */
enum { FH_SOURCE_NONE = 0, FH_SOURCE_START = 1, FH_SOURCE_JOIN = 2, FH_SOURCE_RESTART = 3 };
typedef struct {
    void *entity, *queued;
    unsigned source, lobby, queued_lobby, target;
    unsigned b15c, b15d, b170, d134, d130, d124;
    unsigned party, p2e8, p30, p48, p50;
} FhState;

static int fh_state_read(unsigned char *module, FhState *state) {
    unsigned char block[0x178u - FH_ENTITY_OFFSET];
    state->entity = state->queued = 0;
    state->source = state->lobby = state->queued_lobby = state->target = 0;
    state->b15c = state->b15d = state->b170 = state->d134 = state->d130 = state->d124 = 0;
    state->party = state->p2e8 = state->p30 = state->p48 = state->p50 = 0;
    if (!memory_read(module + FH_ENTITY_OFFSET, block, sizeof(block))) return 0;
#define FH_AT(offset) (block + (offset) - FH_ENTITY_OFFSET)
    void *restart = 0, *join = 0, *target = 0;
    memory_copy(&state->entity, FH_AT(FH_ENTITY_OFFSET), sizeof(void *));
    memory_copy(&restart, FH_AT(FH_RESTART_OFFSET), sizeof(void *));
    memory_copy(&join, FH_AT(FH_JOIN_OFFSET), sizeof(void *));
    memory_copy(&target, FH_AT(FH_TARGET_OFFSET), sizeof(void *));
    memory_copy(&state->queued, FH_AT(FH_QUEUED_OFFSET), sizeof(void *));
    memory_copy(&state->d134, FH_AT(0x134u), 4);
    memory_copy(&state->d130, FH_AT(0x130u), 4);
    memory_copy(&state->d124, FH_AT(0x124u), 4);
    state->lobby = *FH_AT(FH_LOBBY_OFFSET);
    state->b15c = *FH_AT(0x15cu);
    state->b15d = *FH_AT(0x15du);
    state->b170 = *FH_AT(0x170u);
#undef FH_AT
    state->target = target != 0;
    state->source = !state->entity          ? FH_SOURCE_NONE
                    : state->entity == join    ? FH_SOURCE_JOIN
                    : state->entity == restart ? FH_SOURCE_RESTART
                                               : FH_SOURCE_START;
    if (state->queued) {
        unsigned char lobby = 0;
        if (memory_read(state->queued, &lobby, 1)) state->queued_lobby = lobby;
    }
    unsigned char *party = 0;
    if (fh_game && memory_read_pointer(fh_game + FH_PARTY_RVA, (void **)&party)) {
        unsigned char b2e8 = 0, b50 = 0;
        void *session = 0;
        state->party = memory_read(party + 0x2e8, &b2e8, 1) && memory_read(party + 0x30, &session, sizeof(session)) &&
                       memory_read(party + 0x48, &state->p48, 4) && memory_read(party + 0x50, &b50, 1);
        state->p2e8 = b2e8;
        state->p30 = session != 0;
        state->p50 = b50;
    }
    return 1;
}

static void fh_put_source(char *line, size_t *n, const FhState *state) {
    if (state->source == FH_SOURCE_JOIN) {
        fh_put(line, n, "a session join: an invite, the Party's game session or a rejoin");
    } else if (state->source == FH_SOURCE_RESTART) {
        fh_put(line, n, "the module's restart (+170)");
    } else {
        fh_put(line, n, "a start for lobby type ");
        fh_put_number(line, n, state->lobby);
    }
}

/* " entity=1 (<source>) +120=N queued=1 (lobby type N) +15c=.. +140=N party +2e8=N +30=N +48=N +50=N" */
static void fh_put_state(char *line, size_t *n, const FhState *state) {
    fh_put(line, n, " entity=");
    fh_put_number(line, n, state->entity != 0);
    if (state->entity) {
        fh_put(line, n, " (");
        fh_put_source(line, n, state);
        fh_put(line, n, ")");
    }
    fh_put(line, n, " +120=");
    fh_put_number(line, n, state->lobby);
    fh_put(line, n, " queued=");
    fh_put_number(line, n, state->queued != 0);
    if (state->queued) {
        fh_put(line, n, " (lobby type ");
        fh_put_number(line, n, state->queued_lobby);
        fh_put(line, n, ")");
    }
    fh_put(line, n, " +15c=");
    fh_put_number(line, n, state->b15c);
    fh_put(line, n, " +15d=");
    fh_put_number(line, n, state->b15d);
    fh_put(line, n, " +170=");
    fh_put_number(line, n, state->b170);
    fh_put(line, n, " +134=");
    fh_put_number(line, n, state->d134);
    fh_put(line, n, " +130=");
    fh_put_number(line, n, state->d130);
    fh_put(line, n, " +124=");
    fh_put_number(line, n, state->d124);
    fh_put(line, n, " +140=");
    fh_put_number(line, n, state->target);
    if (!state->party) {
        fh_put(line, n, " party=none");
        return;
    }
    fh_put(line, n, " party +2e8=");
    fh_put_number(line, n, state->p2e8);
    fh_put(line, n, " +30=");
    fh_put_number(line, n, state->p30);
    fh_put(line, n, " +48=");
    fh_put_number(line, n, state->p48);
    fh_put(line, n, " +50=");
    fh_put_number(line, n, state->p50);
}

/* "FOLLOW: the game's matchmaking <when>:" and the state. */
static void fh_state_log(unsigned char *module, const char *when) {
    FhState state;
    char line[FH_LINE];
    size_t n = 0;
    fh_put(line, &n, "FOLLOW: the game's matchmaking ");
    fh_put(line, &n, when);
    fh_put(line, &n, ":");
    if (fh_state_read(module, &state)) {
        fh_put_state(line, &n, &state);
    } else {
        fh_put(line, &n, " unreadable");
    }
    fh_put(line, &n, ".\r\n");
    log_append(line);
}

/* After each Update: a change of the entity (the game's search started or ended) or of the queued start, at most once every
   FH_WATCH_MS (changes in between are folded into the next line); change-only, read only. */
static void fh_search_watch(unsigned char *module) {
    void *entity = 0, *queued = 0;
    if (!memory_read(module + FH_ENTITY_OFFSET, &entity, sizeof(entity)) ||
        !memory_read(module + FH_QUEUED_OFFSET, &queued, sizeof(queued))) {
        return;
    }
    if (entity == fh_watch_entity && queued == fh_watch_queued) {
        fh_own_start = 0; /* the hook's start was only queued, or changed nothing */
        return;
    }
    ULONGLONG now = GetTickCount64();
    FhState state;
    if ((fh_watch_at && now - fh_watch_at < FH_WATCH_MS) || !fh_state_read(module, &state)) return;
    char line[FH_LINE];
    size_t n = 0;
    if (state.entity && state.entity != fh_watch_entity) {
        fh_put(line, &n, "FOLLOW: the game's matchmaking search started (");
        fh_put_source(line, &n, &state);
        fh_put(line, &n, fh_own_start ? "; by the hook's FollowHost start):" : "; not the hook's):");
        fh_own_start = 0;
    } else if (!state.entity && fh_watch_entity) {
        fh_put(line, &n, "FOLLOW: the game's matchmaking search ended:");
    } else {
        fh_put(line, &n, "FOLLOW: the game's queued matchmaking start changed:");
    }
    fh_put_state(line, &n, &state);
    fh_put(line, &n, ".\r\n");
    log_append(line);
    fh_watch_entity = state.entity;
    fh_watch_queued = state.queued;
    fh_watch_at = now;
}

/* A queued start that waits longer than FH_QUEUED_LOG_MS: what parks it, once per queued start. */
static void fh_queued_watch(unsigned char *module) {
    void *queued = 0;
    if (!memory_read_pointer(module + FH_QUEUED_OFFSET, &queued)) {
        fh_queued_seen = 0;
        fh_queued_logged = 0;
        return;
    }
    ULONGLONG now = GetTickCount64();
    if (queued != fh_queued_seen) {
        fh_queued_seen = queued;
        fh_queued_since = now;
        fh_queued_logged = 0;
        return;
    }
    if (fh_queued_logged || now - fh_queued_since <= FH_QUEUED_LOG_MS) return;
    fh_queued_logged = 1;
    void *entity = 0;
    unsigned char b15c = 0, b15d = 0, b170 = 0;
    unsigned d134 = 0, d130 = 0, d124 = 0;
    int has_entity = memory_read_pointer(module + FH_ENTITY_OFFSET, &entity);
    memory_read(module + 0x15c, &b15c, 1);
    memory_read(module + 0x15d, &b15d, 1);
    memory_read(module + 0x170, &b170, 1);
    memory_read(module + 0x134, &d134, 4);
    memory_read(module + 0x130, &d130, 4);
    memory_read(module + 0x124, &d124, 4);
    char line[FH_LINE];
    size_t n = 0;
    fh_put(line, &n, "FOLLOW: a queued start waits over 5 s: queued=1 entity=");
    fh_put_number(line, &n, has_entity ? 1u : 0u);
    fh_put(line, &n, " +15c=");
    fh_put_number(line, &n, b15c);
    fh_put(line, &n, " +15d=");
    fh_put_number(line, &n, b15d);
    fh_put(line, &n, " +170=");
    fh_put_number(line, &n, b170);
    fh_put(line, &n, " +134=");
    fh_put_number(line, &n, d134);
    fh_put(line, &n, " +130=");
    fh_put_number(line, &n, d130);
    fh_put(line, &n, " +124=");
    fh_put_number(line, &n, d124);
    /* T-1108-J: what the entity is (its source and lobby type), the session target, and the Party gates Update reads */
    FhState state;
    if (fh_state_read(module, &state)) {
        fh_put(line, &n, " | the game's search:");
        if (state.entity) {
            fh_put(line, &n, " ");
            fh_put_source(line, &n, &state);
        } else {
            fh_put(line, &n, " none");
        }
        fh_put(line, &n, " +120=");
        fh_put_number(line, &n, state.lobby);
        fh_put(line, &n, " queued lobby type ");
        fh_put_number(line, &n, state.queued_lobby);
        fh_put(line, &n, " +140=");
        fh_put_number(line, &n, state.target);
        if (state.party) {
            fh_put(line, &n, " party +2e8=");
            fh_put_number(line, &n, state.p2e8);
            fh_put(line, &n, " +30=");
            fh_put_number(line, &n, state.p30);
            fh_put(line, &n, " +48=");
            fh_put_number(line, &n, state.p48);
            fh_put(line, &n, " +50=");
            fh_put_number(line, &n, state.p50);
        } else {
            fh_put(line, &n, " party=none");
        }
    }
    fh_put(line, &n, "\r\n");
    log_append(line);
}

/* The queued start at module+0x150 (its first byte the lobby type) is freed the game's way and the
   slot zeroed; 0 when the delete's bytes were not as expected (it stays). */
static int fh_queued_free(unsigned char *module, void *queued) {
    if (!fh_delete_ready) return 0;
    ((FhDeleteFn)(uintptr_t)(fh_game + FH_DELETE_RVA))(queued);
    *(void **)(module + FH_QUEUED_OFFSET) = 0;
    fh_queued_seen = 0;
    fh_queued_logged = 0;
    return 1;
}

/* The launcher gave up: a queued start is freed the game's way, then the game's own disconnect, only
   while a matchmaking entity exists (module+0xc0 non-null), as the game's own senders check first. */
static void fh_cancel_now(unsigned char *module) {
    void *queued = 0, *entity = 0;
    if (memory_read_pointer(module + FH_QUEUED_OFFSET, &queued)) fh_queued_free(module, queued);
    fh_queued_seen = 0;
    fh_queued_logged = 0;
    if (!memory_read_pointer(module + FH_ENTITY_OFFSET, &entity)) {
        log_append("FOLLOW: the launcher gave up, but the game has no matchmaking to cancel; no disconnect sent.\r\n");
        return;
    }
    ((FhSendFn)(uintptr_t)(fh_game + FH_SEND_RVA))(module, fh_game + FH_DISCONNECT_RVA);
    log_append("FOLLOW: matchmaking cancelled (the launcher gave up).\r\n");
}

static void fh_log_lobby(const char *text, unsigned lobby, const char *tail) {
    char line[FH_LINE];
    size_t n = 0;
    fh_put(line, &n, text);
    fh_put_number(line, &n, lobby);
    fh_put(line, &n, tail);
    log_append(line);
}

/* The host's lobby type `lobby` asked. A queued start of another lobby type is stale: it is freed,
   so it cannot fire later (live, test.11: a stale Road Trip start pulled a guest out of free roam).
   1 when no start is needed: one of `lobby` is queued already, or the game is in `lobby` (the lobby
   type it last connected its world with; else, as a second guard, its matchmaking entity at +0xc0
   with the last started lobby type at +0x120). */
static int fh_already(unsigned char *module, unsigned lobby) {
    unsigned char *queued = 0, *entity = 0, current = 0;
    if (memory_read_pointer(module + FH_QUEUED_OFFSET, (void **)&queued)) {
        unsigned char queued_lobby = 0;
        if (!memory_read(queued, &queued_lobby, 1)) return 0; /* unreadable: the start replaces it */
        if (queued_lobby == lobby) {
            fh_log_lobby("FOLLOW: already queued (lobby type ", lobby, ").\r\n");
            return 1;
        }
        fh_log_lobby("FOLLOW: a stale queued start (lobby type ", queued_lobby,
                     fh_queued_free(module, queued) ? ") was dropped.\r\n" : ") stays (the game's delete is not as expected).\r\n");
    }
    if ((unsigned)fh_connected == lobby) return 1;
    return memory_read_pointer(module + FH_ENTITY_OFFSET, (void **)&entity) &&
           memory_read(module + FH_LOBBY_OFFSET, &current, 1) && current == lobby;
}

/* T-L5: a move into an accepted invite's lobby of type `lobby`. 1 when it must not start: a start of `lobby` is queued already
   (a stale one of another type is freed, as fh_already does), or the game is connected with any lobby type but online free roam
   (a Road Trip, a car meet: an event, never pulled out of it, whatever the invite's type). Connected in free roam, or in nothing:
   0, it starts (fh_already's skip of free roam is the case a move exists for). */
static int fh_move_blocked(unsigned char *module, unsigned lobby) {
    unsigned char *queued = 0;
    if (memory_read_pointer(module + FH_QUEUED_OFFSET, (void **)&queued)) {
        unsigned char queued_lobby = 0;
        if (memory_read(queued, &queued_lobby, 1) && queued_lobby == lobby) {
            fh_log_lobby("FOLLOW: an invite's move: already queued (lobby type ", lobby, ").\r\n");
            return 1;
        }
        fh_log_lobby("FOLLOW: an invite's move: a stale queued start (lobby type ", queued_lobby,
                     fh_queued_free(module, queued) ? ") was dropped.\r\n" : ") stays (the game's delete is not as expected).\r\n");
    }
    unsigned connected = (unsigned)fh_connected;
    if (connected && connected != FH_LOBBY_FREE_ROAM) {
        fh_log_lobby("FOLLOW: an invite's move is refused: the game is connected with lobby type ", connected,
                     " (an event); it is not pulled out (the launcher lets the seat go).\r\n");
        return 1;
    }
    return 0;
}

/* T-1108-J (live, games-6): a move's start parked behind the game's own matchmaking. OnStartMatchmaking queues the start
   (module+0x150) while a live matchmaking entity exists (+0xc0), and Update runs the queued start only once the entity is gone
   (game+0x808b60: the +0x15c branch needs +0xc0 null); a game sitting in its own FINDING SESSION search never drops it, so the
   move waited forever (queued=1 entity=1 +15c=1). As Update itself does when its entity's session is not the one asked
   (game+0x80905e), the game's own disconnect is sent so that search ends; the queued start is kept and the game runs it on a
   following Update. Only for a move (the player chose the private lobby), only with a queued start of `lobby` and an entity, and
   only on the send's verified bytes; a game connected in a session has no entity (live, test.11), so it is never disconnected. */
static void fh_move_unpark(unsigned char *module, unsigned lobby) {
    unsigned char *queued = 0, queued_lobby = 0;
    void *entity = 0;
    unsigned connected = (unsigned)fh_connected;
    if ((connected && connected != FH_LOBBY_FREE_ROAM) || !memory_read_pointer(module + FH_QUEUED_OFFSET, (void **)&queued) || !memory_read(queued, &queued_lobby, 1) ||
        queued_lobby != lobby || !memory_read_pointer(module + FH_ENTITY_OFFSET, &entity)) {
        return;
    }
    if (!fh_cancel_ready) {
        log_append("FOLLOW: an invite's move waits behind the game's own matchmaking, and its disconnect is not as expected "
                   "here; it stays queued.\r\n");
        return;
    }
    ((FhSendFn)(uintptr_t)(fh_game + FH_SEND_RVA))(module, fh_game + FH_DISCONNECT_RVA);
    fh_log_lobby("FOLLOW: an invite's move waits behind the game's own matchmaking: that search is disconnected; the queued "
                 "start (lobby type ", lobby, ") runs once it ends.\r\n");
    fh_state_log(module, "at the unpark disconnect");
    fh_unpark_at = GetTickCount64();
}

static void fh_update(void *module, float dt) {
    if (fh_cancel && InterlockedExchange(&fh_cancel, 0)) {
        if (GetTickCount64() - (ULONGLONG)fh_cancel_at > FH_PENDING_MS || !fi_launch_live() || fi_launch_suspended()) {
            log_append("FOLLOW: a matchmaking cancel came too late or online is closed; dropped.\r\n");
        } else {
            fh_cancel_now(module);
            fh_released_at = GetTickCount64();
            fh_released_logged = 0;
        }
    }
    if (fh_released_at) {
        ULONGLONG since = GetTickCount64() - fh_released_at;
        if (fh_released_logged == 0 && since >= 3000u) {
            fh_released_logged = 1;
            log_append("FOLLOW: the game thread still runs 3 s after the matchmaking release.\r\n");
            azimuth_freeroam_pending_log("3 s after the release");
        } else if (fh_released_logged == 1 && since >= 10000u) {
            fh_released_at = 0;
            log_append("FOLLOW: the game thread still runs 10 s after the matchmaking release.\r\n");
        }
    }
    LONG pending = fh_pending ? InterlockedExchange(&fh_pending, 0) : 0;
    if (pending) {
        unsigned lobby = (unsigned)pending & 0xffu, is_public = ((unsigned)pending >> 8) & 1u;
        int move = ((unsigned)pending & FH_PENDING_MOVE) != 0;
        fh_state_log(module, move ? "at an invite's FollowHost" : "at FollowHost");
        if (GetTickCount64() - (ULONGLONG)fh_pending_at > FH_PENDING_MS || !fi_launch_live() || fi_launch_suspended()) {
            log_append(move ? "FOLLOW: an invite's move came too late or online is closed; dropped.\r\n"
                            : "FOLLOW: a host change came too late or online is closed; dropped.\r\n");
        } else if (move) {
            if (!fh_move_blocked(module, lobby)) {
                FhStartArgument argument = {(unsigned char)lobby, (unsigned char)is_public, {0, 0}, -1, -1};
                ((FhStartFn)(uintptr_t)(fh_game + FH_START_RVA))(module, &argument);
                fh_own_start = 1;
                fh_log_lobby("FOLLOW: an invite's move: matchmaking started for lobby type ", lobby,
                             " (the player confirms; the game leaves its current session itself).\r\n");
            }
            fh_move_unpark(module, lobby);
        } else if (fh_already(module, lobby)) {
            log_append(lobby == FH_LOBBY_ROAD_TRIP ? "FOLLOW: the host is in Online Road Trip; the game is already there.\r\n"
                                                   : "FOLLOW: the host is in Online Free Roam; the game is already there.\r\n");
        } else {
            FhStartArgument argument = {(unsigned char)lobby, (unsigned char)is_public, {0, 0}, -1, -1};
            ((FhStartFn)(uintptr_t)(fh_game + FH_START_RVA))(module, &argument);
            fh_own_start = 1;
            log_append(lobby == FH_LOBBY_ROAD_TRIP
                           ? "FOLLOW: the host is in Online Road Trip: matchmaking started (the player confirms).\r\n"
                           : "FOLLOW: the host is in Online Free Roam: matchmaking started (the player confirms).\r\n");
        }
    }
    fh_queued_watch(module);
    if (fh_unpark_at && GetTickCount64() - fh_unpark_at >= FH_UNPARK_CHECK_MS) {
        fh_unpark_at = 0;
        fh_state_log(module, "1 s after the unpark disconnect");
    }
    fh_update_original(module, dt);
    fh_search_watch(module);
}

/* One datagram the game sends its world through the joiner's proxy: when its first NerdCore message
   is an OpenConnection, its lobby type A (read as Infinity.Core's RelayEngine.LobbyTypeOf: an 11-byte
   header with a message count > 0, then LSB-first bits: id 7 = 0, two compact byte strings of at
   most 38 and 16 bytes, flags 2, user 64, A 32). Not a STUN message. 0 otherwise. */
static unsigned fh_bits(const unsigned char *data, size_t bits_end, size_t *at, unsigned width, int *ok) {
    unsigned long long value = 0;
    if (!*ok || width > 32u || *at + width > bits_end) {
        *ok = 0;
        return 0;
    }
    for (unsigned got = 0; got < width; ++got, ++*at) {
        value |= (unsigned long long)((data[*at >> 3] >> (*at & 7u)) & 1u) << got;
    }
    return (unsigned)value;
}
static void fh_skip_string(const unsigned char *data, size_t bits_end, size_t *at, unsigned maximum, int *ok) {
    unsigned width = 8u * fh_bits(data, bits_end, at, 2, ok) + 6u;
    unsigned count = fh_bits(data, bits_end, at, width, ok);
    if (!*ok || count > maximum || *at + 8u * (size_t)count > bits_end) {
        *ok = 0;
        return;
    }
    *at += 8u * (size_t)count;
}
static unsigned fh_open_lobby(const unsigned char *data, size_t size) {
    if (!data || size < 11u || size > 0xffffu || !data[8]) return 0;
    if (size >= 20u && !(data[0] & 0xc0u) && data[4] == 0x21 && data[5] == 0x12 && data[6] == 0xa4 && data[7] == 0x42 &&
        (((size_t)data[2] << 8) | data[3]) + 20u == size && !(size % 4u)) {
        return 0; /* STUN */
    }
    const unsigned char *body = data + 11;
    size_t end = (size - 11u) * 8u, at = 0;
    int ok = 1;
    if (fh_bits(body, end, &at, 7, &ok) != 0 || !ok) return 0;
    fh_skip_string(body, end, &at, 38, &ok);
    fh_skip_string(body, end, &at, 16, &ok);
    fh_bits(body, end, &at, 2, &ok);
    fh_bits(body, end, &at, 32, &ok);
    fh_bits(body, end, &at, 32, &ok);
    unsigned lobby = fh_bits(body, end, &at, 32, &ok);
    fh_bits(body, end, &at, 32, &ok); /* B */
    fh_bits(body, end, &at, 32, &ok); /* the attempt: the whole message is there */
    return ok ? lobby : 0;
}

/* The joiner's proxy (proxy.c), for each datagram the game sends its world. */
void fi_follow_datagram(const unsigned char *data, size_t size) {
    unsigned lobby = fh_open_lobby(data, size);
    if (lobby && lobby < 0x100u) InterlockedExchange64(&fh_connected_at, (LONG64)GetTickCount64());
    if (lobby && lobby < 0x100u && (unsigned)fh_connected != lobby) {
        InterlockedExchange(&fh_connected, (LONG)lobby);
        fh_log_lobby("FOLLOW: the game connects its world with lobby type ", lobby, ".\r\n");
    }
}

/* An association destroyed (freeroam.c). The game opens its next lobby's connection before it
   destroys the old association, so only an OpenConnection FH_LEFT_MS old or more means the game left
   its session (no lobby type now); a recent one is the new connection's and stays. */
void fi_follow_left(void) {
    if (GetTickCount64() - (ULONGLONG)fh_connected_at >= FH_LEFT_MS) {
        InterlockedExchange(&fh_connected, 0);
    } else if (fh_connected) {
        fh_log_lobby("FOLLOW: an old connection closed after the game connected with lobby type ", (unsigned)fh_connected,
                     "; kept.\r\n");
    }
}
/* FollowHost from the launcher (launch.c's reader; lobby 1 or 2, already validated; move: T-L5's flag). */
void fi_follow_host(unsigned lobby, unsigned is_public, int move) {
    if (lobby != FH_LOBBY_ROAD_TRIP && lobby != FH_LOBBY_FREE_ROAM) {
        return;
    }
    if (!fh_update_original) {
        log_append(move ? "FOLLOW: an invite's move, but the matchmaking module is not patched here; ignored.\r\n"
                        : "FOLLOW: the host changed activity, but the matchmaking module is not patched here; ignored.\r\n");
        return;
    }
    InterlockedExchange64(&fh_pending_at, (LONG64)GetTickCount64());
    InterlockedExchange(&fh_pending, (LONG)(0x10000u | (move ? FH_PENDING_MOVE : 0u) | (is_public ? 0x100u : 0u) | lobby));
}

/* CancelMatchmaking from the launcher (launch.c's reader; its empty payload already checked). */
void fi_follow_cancel(void) {
    if (!fh_update_original || !fh_cancel_ready) {
        log_append("FOLLOW: the launcher gave up on its seat, but the game's matchmaking cancel is not available here; "
                   "ignored.\r\n");
        return;
    }
    InterlockedExchange64(&fh_cancel_at, (LONG64)GetTickCount64());
    InterlockedExchange(&fh_cancel, 1);
    /* W-10 Q4: what the game still waits on when its matchmaking is released (read only) */
    azimuth_freeroam_pending_log("at the matchmaking release");
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
        fh_cancel_ready = 0;
        fh_delete_ready = 0;
        log_append("FOLLOW: the matchmaking module is not as expected; left as is (a host's change of activity is "
                   "only shown).\r\n");
        return;
    }
    unsigned char send[sizeof(fh_send_bytes)], release[sizeof(fh_delete_bytes)];
    fh_delete_ready = memory_read(game_image + FH_DELETE_RVA, release, sizeof(release)) &&
                      memory_equal(release, fh_delete_bytes, sizeof(release));
    fh_cancel_ready = fh_delete_ready && memory_read(game_image + FH_SEND_RVA, send, sizeof(send)) &&
                      memory_equal(send, fh_send_bytes, sizeof(send));
    fh_update_original = (FhUpdateFn)(uintptr_t)current;
    *slot = (void *)(uintptr_t)fh_update;
    VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
    log_append("FOLLOW: a host's change of activity starts the game's own matchmaking (the player confirms).\r\n");
    log_append(fh_cancel_ready ? "FOLLOW: the launcher can cancel the game's matchmaking when it gives up on a seat.\r\n"
                               : "FOLLOW: the game's matchmaking cancel is not as expected; a give-up only releases the "
                                 "seat.\r\n");
}
