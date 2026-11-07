/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* The game's car database (OD-208: what is left of world.c once players host no world): its
   SHA-256 goes with every request to the server, and a database changed mid-session cuts the
   game off the online world and tells the launcher (Event 8, world left), which leaves the seat
   (DELETE /mm/v1/seat; OD-208: the Beta's POST /infinity/v1/worlds/left is gone). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/sha256.h"
#include "../core/support.h"

typedef struct {
    char *data;
    size_t size, capacity;
} WorldText;

static void world_add(WorldText *t, const char *text) {
    while (text && *text && t->size + 1 < t->capacity) {
        t->data[t->size++] = *text++;
    }
    t->data[t->size] = 0;
}

/* The car database changed during the session (world_car_db_tampered). */
static volatile LONG world_car_db_changed;

/* ------------------------------------------------------------ the car database */
/* Players only meet with the same cars installed (imported FM5 cars are rows in it): the
   SHA-256 of the game's car database (FiGameProfile.car_db; FH2's media\stripped\gamedbRC.slt) goes with every request to the server
   (X-Fh2-CarDb, azimuth_net.c) and the server keeps worlds apart by it. Hashed once, on a
   thread of its own, when the game starts. */

#define WORLD_HASH_CHUNK 65536u

static wchar_t world_car_db_path[PATH_CAPACITY];
static wchar_t world_car_db_header[96];
static volatile LONG world_car_db_ready;
/* 0 not started, 1 hashing, 2 done (ready or unreadable): requests wait only while 1. */
static volatile LONG world_car_db_state;
static unsigned char world_car_db_digest[32];

#define WORLD_REHASH_MS 30000u

static void world_car_db_tampered(void);

/* SHA-256 of the car database; 0 when it cannot be read. */
static int world_digest_car_db(unsigned char digest[32]) {
    unsigned char *chunk = HeapAlloc(GetProcessHeap(), 0, WORLD_HASH_CHUNK);
    HANDLE file = CreateFileW(world_car_db_path, GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING,
                              FILE_ATTRIBUTE_NORMAL, 0);
    if (!chunk || file == INVALID_HANDLE_VALUE) {
        if (chunk) HeapFree(GetProcessHeap(), 0, chunk);
        if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
        return 0;
    }
    Sha256Context hash;
    DWORD got = 0;
    sha256_initialize(&hash);
    while (ReadFile(file, chunk, WORLD_HASH_CHUNK, &got, 0) && got) {
        sha256_update(&hash, chunk, got);
    }
    CloseHandle(file);
    HeapFree(GetProcessHeap(), 0, chunk);
    sha256_finalize(&hash, digest);
    return 1;
}

/* Hashed when the game starts, then again every 30 s: a database changed during the session
   (cars swapped for an advantage) cuts the player off the world and tells the server
   (world_car_db_tampered). A file that cannot be read for a moment is not a change. A
   modified client can lie about all of this: it only stops honest clients from switching
   cars mid-session, and the world's own checks are the relay's. */
static DWORD WINAPI world_hash_car_db(void *unused) {
    (void)unused;
    unsigned char digest[32];
    if (!world_digest_car_db(digest)) {
        InterlockedExchange(&world_car_db_state, 2);
        log_append("WORLD: the car database could not be read; the server will refuse online play.\r\n");
        return 0;
    }
    memory_copy(world_car_db_digest, digest, sizeof(digest));
    static const wchar_t prefix[] = L"X-Fh2-CarDb: ";
    static const char hex[] = "0123456789abcdef";
    size_t n = 0;
    for (size_t i = 0; prefix[i]; ++i) world_car_db_header[n++] = prefix[i];
    for (int i = 0; i < 32; ++i) {
        world_car_db_header[n++] = (wchar_t)hex[digest[i] >> 4];
        world_car_db_header[n++] = (wchar_t)hex[digest[i] & 15];
    }
    world_car_db_header[n] = 0;
    InterlockedExchange(&world_car_db_ready, 1);
    InterlockedExchange(&world_car_db_state, 2);
    char line[96];
    WorldText t = {line, 0, sizeof(line)};
    line[0] = 0;
    world_add(&t, "WORLD: car database ");
    for (int i = 0; i < 8; ++i) {
        char pair[3] = {hex[digest[i] >> 4], hex[digest[i] & 15], 0};
        world_add(&t, pair);
    }
    world_add(&t, "... (sent to the server: only the game's own cars play online).\r\n");
    log_append(line);
    for (;;) {
        Sleep(WORLD_REHASH_MS);
        if (world_digest_car_db(digest) && !memory_equal(digest, world_car_db_digest, sizeof(digest))) {
            world_car_db_tampered();
            return 0;
        }
    }
}

/* `executable` is the game's forza_durango_release_final.exe in its Mount folder. */
void azimuth_world_hash_car_db(const wchar_t *executable) {
    size_t n = 0, slash = 0;
    if (InterlockedCompareExchange(&world_car_db_state, 1, 0) != 0) {
        return;
    }
    while (executable && executable[n] && n + 1 < ARRAY_COUNT(world_car_db_path)) {
        world_car_db_path[n] = executable[n];
        if (executable[n] == L'\\' || executable[n] == L'/') slash = n + 1;
        ++n;
    }
    world_car_db_path[slash] = 0;
    const wchar_t *rest = fi_game_profile()->car_db;
    size_t rest_length = 0;
    while (rest[rest_length]) ++rest_length;
    if (slash + rest_length + 1 >= ARRAY_COUNT(world_car_db_path)) {
        InterlockedExchange(&world_car_db_state, 2);
        return;
    }
    memory_copy(world_car_db_path + slash, rest, (rest_length + 1) * sizeof(wchar_t));
    HANDLE thread = CreateThread(0, 0, world_hash_car_db, 0, 0, 0);
    if (thread) {
        CloseHandle(thread);
    } else {
        InterlockedExchange(&world_car_db_state, 2);
    }
}

/* Waits (at most `ms`) while the car database is being hashed: the server refuses an account's
   requests without the hash, so the game's first ones wait for it. */
void azimuth_world_car_db_wait(unsigned ms) {
    for (unsigned waited = 0; world_car_db_state == 1 && waited < ms; waited += 20) {
        Sleep(20);
    }
}

/* "X-Fh2-CarDb: <sha-256>", or 0 until it is known. */
const wchar_t *azimuth_world_car_db_header(void) {
    return world_car_db_ready ? world_car_db_header : 0;
}

/* The car database changed during the session: off the world, and the launcher is told (it
   leaves this player's seat, DELETE /mm/v1/seat). The
   X-Fh2-CarDb header now says "changed", so the server keeps this game out of every world
   until it restarts. */
static void world_car_db_tampered(void) {
    if (InterlockedExchange(&world_car_db_changed, 1)) {
        return;
    }
    log_append("WORLD: the car database changed during the session; this game leaves the online world.\r\n");
    static const wchar_t changed[] = L"X-Fh2-CarDb: changed";
    memory_copy(world_car_db_header, changed, sizeof(changed));
    azimuth_freeroam_cut();
    fi_launch_event(FI_EVENT_WORLD_LEFT, "cars");
}
