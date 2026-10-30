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
   dropped.

   Giving up (CancelMatchmaking, 0x24). When the launcher gave up on a lobby ask's seat (it did not
   start playing within 15 s), the same detour, on the module's next Update, sends the game's own
   cancel the way Update itself does (game+0x80905e): the no-data queued send FUN_141903d50 (game+
   0x1903d50, void (module *, const event descriptor *)) with NetworkMatchmakingRequestDisconnect's
   descriptor (game+0x34bdc28, a static in .data's uninitialized part). A start queued meanwhile
   (module+0x150, a heap StartMatchmakingEventArgument) is freed first with the game's operator
   delete (game+0x31c7b0) and the slot zeroed, so it cannot fire later. Only when the send's and the
   delete's first 16 bytes are as expected; otherwise the request is only logged.

   Diagnostics: a queued start (module+0x150) that stays for more than FH_QUEUED_LOG_MS is logged
   once with what keeps it parked (the entity at +0xc0, bytes +0x15c, +0x15d, +0x170, dwords +0x134,
   +0x130, +0x124). Read only. */
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
/* the asked follow: 0 none, else 0x10000 | public << 8 | lobby type; fh_pending_at its tick */
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
/* the queued start last seen at module+0x150, since when, and whether it was logged */
static void *fh_queued_seen;
static ULONGLONG fh_queued_since;
static int fh_queued_logged;

/* A bounded log line: text and unsigned decimals appended to `line` (capacity FH_LINE). */
#define FH_LINE 256u
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

/* The launcher gave up: a queued start is freed the game's way, then the game's own disconnect. */
static void fh_cancel_now(unsigned char *module) {
    void *queued = 0;
    if (memory_read_pointer(module + FH_QUEUED_OFFSET, &queued)) fh_queued_free(module, queued);
    fh_queued_seen = 0;
    fh_queued_logged = 0;
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

static void fh_update(void *module, float dt) {
    if (fh_cancel && InterlockedExchange(&fh_cancel, 0)) {
        if (GetTickCount64() - (ULONGLONG)fh_cancel_at > FH_PENDING_MS || !fi_launch_live() || fi_launch_suspended()) {
            log_append("FOLLOW: a matchmaking cancel came too late or online is closed; dropped.\r\n");
        } else {
            fh_cancel_now(module);
        }
    }
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
    fh_queued_watch(module);
    fh_update_original(module, dt);
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

/* CancelMatchmaking from the launcher (launch.c's reader; its empty payload already checked). */
void fi_follow_cancel(void) {
    if (!fh_update_original || !fh_cancel_ready) {
        log_append("FOLLOW: the launcher gave up on its seat, but the game's matchmaking cancel is not available here; "
                   "ignored.\r\n");
        return;
    }
    InterlockedExchange64(&fh_cancel_at, (LONG64)GetTickCount64());
    InterlockedExchange(&fh_cancel, 1);
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
