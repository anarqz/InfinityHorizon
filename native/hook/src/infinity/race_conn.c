/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The game's race connectivity, observed (T38b). After an online race's results the game returns to
   network free roam only while its lobby's race connectivity is Online; any other value takes it to
   the offline free roam, which ends its world session. The value is an int at +0x38 of the lobby
   object whose pointer sits at game+0x34d6348 (its getter game+0xa2c920: cmp [rcx+38h],1; sete al;
   its setter game+0xa31620; 0 Undefined, 1 Online, 2 as the race-ending grid and the stateflow's
   set_race_connectivity_offline leave it).

   Read only: the module's install checks that the game's own NetworkIsOnlineRace (game+0x8c8670)
   loads that pointer (mov rcx,[rip+disp] at game+0x8c867e naming game+0x34d6348) and that the getter
   is those bytes; any difference leaves the watch off and says so once. Then every main-update
   sample (game_ready.c, 250 ms) reads the pointer and the value through memory_read, and a change is
   one line "RACE: connectivity <old> -> <new> (<name>)": the first RC_FIRST changes, then a count at
   every power of two. An unreadable lobby is one note, never a crash. The association's DestroyAsync
   logs the value once more (bounded too). No game write, no game call. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define RC_LOAD_RVA 0x8c867eu   /* NetworkIsOnlineRace: mov rcx,[rip+disp32] */
#define RC_LOBBY_RVA 0x34d6348u /* the lobby object's pointer */
#define RC_GETTER_RVA 0xa2c920u
#define RC_VALUE_AT 0x38u
#define RC_FIRST 32
#define RC_UNSEEN (-1000) /* not read yet */

static const unsigned char *rc_image;
static volatile LONG rc_installed, rc_changes, rc_teardowns, rc_noted;
static int rc_last = RC_UNSEEN;

static void rc_name(char *line, size_t *n, size_t cap, int value) {
    const char *name = value == 0 ? "Undefined" : value == 1 ? "Online" : 0;
    char digits[16];
    unsigned d = 0;
    if (!name) {
        unsigned v = value < 0 ? (unsigned)-(long long)value : (unsigned)value;
        do digits[d++] = (char)('0' + v % 10); while ((v /= 10) && d < 12);
        if (value < 0) digits[d++] = '-';
    }
    if (name) {
        while (*name && *n + 1 < cap) line[(*n)++] = *name++;
    } else {
        while (d && *n + 1 < cap) line[(*n)++] = digits[--d];
    }
    line[*n] = 0;
}

static void rc_text(char *line, size_t *n, size_t cap, const char *text) {
    while (*text && *n + 1 < cap) line[(*n)++] = *text++;
    line[*n] = 0;
}

static void rc_number(char *line, size_t *n, size_t cap, unsigned long long value) {
    char digits[24];
    unsigned d = 0;
    do digits[d++] = (char)('0' + value % 10); while ((value /= 10) && d < sizeof(digits));
    while (d && *n + 1 < cap) line[(*n)++] = digits[--d];
    line[*n] = 0;
}

int fi_race_conn_check(const unsigned char *image) {
    static const unsigned char load[3] = {0x48, 0x8b, 0x0d};
    static const unsigned char getter[8] = {0x83, 0x79, 0x38, 0x01, 0x0f, 0x94, 0xc0, 0xc3};
    unsigned char current[8];
    int disp = 0;
    if (!image || !memory_read(image + RC_LOAD_RVA, current, 3) || !memory_equal(current, load, 3) ||
        !memory_read(image + RC_LOAD_RVA + 3, &disp, 4) ||
        image + RC_LOAD_RVA + 7 + disp != image + RC_LOBBY_RVA) {
        return 0;
    }
    return memory_read(image + RC_GETTER_RVA, current, 8) && memory_equal(current, getter, 8);
}

void fi_race_conn_apply(unsigned char *image) {
    if (InterlockedCompareExchange(&rc_installed, 1, 0)) return;
    if (!fi_race_conn_check(image)) {
        log_append("RACE: the race connectivity code is not as expected; not watched.\r\n");
        return;
    }
    rc_image = image;
    log_append("RACE: race connectivity watched (read only).\r\n");
}

int fi_race_conn_read(int *value) {
    void *lobby = 0;
    if (!rc_image || !memory_read_pointer(rc_image + RC_LOBBY_RVA, &lobby) || !lobby) return 0;
    return memory_read((const unsigned char *)lobby + RC_VALUE_AT, value, 4);
}

void fi_race_conn_tick(void) {
    if (!rc_image) return;
    int value = 0;
    if (!fi_race_conn_read(&value)) {
        if (!InterlockedExchange(&rc_noted, 1)) {
            log_append("RACE: the race connectivity is not readable now (no lobby object); watched again when it is.\r\n");
        }
        return;
    }
    int old = rc_last;
    if (value == old) return;
    rc_last = value;
    LONG number = InterlockedIncrement(&rc_changes);
    if (number > RC_FIRST && (number & (number - 1))) return;
    char line[160];
    size_t n = 0;
    line[0] = 0;
    rc_text(line, &n, sizeof(line), "RACE: connectivity ");
    if (old == RC_UNSEEN) rc_text(line, &n, sizeof(line), "-");
    else rc_name(line, &n, sizeof(line), old);
    rc_text(line, &n, sizeof(line), " -> ");
    rc_name(line, &n, sizeof(line), value);
    if (number > RC_FIRST) {
        rc_text(line, &n, sizeof(line), " (");
        rc_number(line, &n, sizeof(line), (unsigned long long)number);
        rc_text(line, &n, sizeof(line), " changes)");
    }
    rc_text(line, &n, sizeof(line), ".\r\n");
    log_append(line);
}

void fi_race_conn_teardown(void) {
    if (!rc_image) return;
    LONG number = InterlockedIncrement(&rc_teardowns);
    if (number > RC_FIRST && (number & (number - 1))) return;
    int value = 0;
    char line[160];
    size_t n = 0;
    line[0] = 0;
    rc_text(line, &n, sizeof(line), "RACE: connectivity at association teardown = ");
    if (fi_race_conn_read(&value)) rc_name(line, &n, sizeof(line), value);
    else rc_text(line, &n, sizeof(line), "unreadable");
    rc_text(line, &n, sizeof(line), ".\r\n");
    log_append(line);
}
