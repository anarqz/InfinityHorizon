/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* The profile sync (Forza.WebServices.OnlineProfile.UpdateAll) checked here before it goes:
   progress arrives in steps, so a sync whose credits, XP or level grew beyond what play can earn
   since the last one sent is held back (the game is answered as the server answers it, and the
   next sync is measured from the same point). The server checks the same numbers again
   (server/src/forza/gates.rs): this only keeps a broken or edited profile from being sent.

   The limits are the server's: twice the most production saw between two accepted syncs
   (2026-10-16), a base plus an allowance per hour since the last sync sent.

   UpdateAll's body (bin/xtsw, big-endian, positional; server/schema.json) is 17 arrays of
   structs whose fields are all fixed-size: u32 count, then count elements. The totals are in
   playerHistory[0] (credits) and playerRaceHistory[0] (level, xp, totalWinnings). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define PG_CREDITS_BASE 540000.0
#define PG_CREDITS_PER_HOUR 620000.0
#define PG_XP_BASE 384000.0
#define PG_XP_PER_HOUR 503000.0
#define PG_LEVELS_BASE 12.0
#define PG_LEVELS_PER_HOUR 16.0

/* The wire size of each array's element, in UpdateAll's order (server/schema.json). */
static const unsigned pg_element_size[] = {26, 18, 28, 48, 28, 48, 28, 48, 64, 37, 67, 148, 128, 24, 37, 192, 160};
#define PG_PLAYER_HISTORY 10
#define PG_RACE_HISTORY 11
#define PG_CREDITS_AT 5
#define PG_LEVEL_AT 0
#define PG_XP_AT 4
#define PG_WINNINGS_AT 8
#define PG_ARRAY_MAX 0x2000u

typedef struct {
    unsigned credits, level, xp, winnings;
} ProgressTotals;

static unsigned pg_u32(const unsigned char *p) {
    return (unsigned)p[0] << 24 | (unsigned)p[1] << 16 | (unsigned)p[2] << 8 | p[3];
}

/* The totals in an UpdateAll body; 0 when it does not read (or carries no history). */
int azimuth_progress_read(const unsigned char *body, size_t size, unsigned out[4]) {
    size_t at = 0;
    int have_player = 0, have_race = 0;
    ProgressTotals t = {0, 0, 0, 0};
    for (unsigned i = 0; i < ARRAY_COUNT(pg_element_size); ++i) {
        if (at > size || size - at < 4) {
            return 0;
        }
        unsigned count = pg_u32(body + at);
        at += 4;
        if (count > PG_ARRAY_MAX || (size - at) / pg_element_size[i] < count) {
            return 0;
        }
        if (count && i == PG_PLAYER_HISTORY) {
            t.credits = pg_u32(body + at + PG_CREDITS_AT);
            have_player = 1;
        }
        if (count && i == PG_RACE_HISTORY) {
            t.level = pg_u32(body + at + PG_LEVEL_AT);
            t.xp = pg_u32(body + at + PG_XP_AT);
            t.winnings = pg_u32(body + at + PG_WINNINGS_AT);
            have_race = 1;
        }
        at += (size_t)count * pg_element_size[i];
    }
    if (at != size || !have_player || !have_race) {
        return 0;
    }
    out[0] = t.credits;
    out[1] = t.level;
    out[2] = t.xp;
    out[3] = t.winnings;
    return 1;
}

/* Whether `now` may follow `base` after `hours`: 0 when it may, else the first number over
   (1 credits, 2 winnings, 3 XP, 4 level). Spending is never a problem; winnings measured from
   0 (a new profile, a career reset) are taken as they are, as the server does. */
int azimuth_progress_over(const unsigned base[4], const unsigned now[4], double hours) {
    double grew_credits = (double)now[0] - (double)base[0], grew_level = (double)now[1] - (double)base[1];
    double grew_xp = (double)now[2] - (double)base[2], grew_winnings = (double)now[3] - (double)base[3];
    if (hours < 0) {
        hours = 0;
    }
    if (grew_credits > PG_CREDITS_BASE + PG_CREDITS_PER_HOUR * hours) {
        return 1;
    }
    if (base[3] > 0 && grew_winnings > PG_CREDITS_BASE + PG_CREDITS_PER_HOUR * hours) {
        return 2;
    }
    if (grew_xp > PG_XP_BASE + PG_XP_PER_HOUR * hours) {
        return 3;
    }
    if (grew_level > PG_LEVELS_BASE + PG_LEVELS_PER_HOUR * hours) {
        return 4;
    }
    return 0;
}

#ifndef FH2_PROGRESS_TEST
static unsigned pg_sent[4];
static ULONGLONG pg_sent_at;
static int pg_have_sent;
static SRWLOCK pg_lock = SRWLOCK_INIT;

/* Whether a header line names it (case-insensitive, within the headers' text). */
static int pg_headers_have(const wchar_t *headers, const wchar_t *text) {
    for (const wchar_t *p = headers; p && *p; ++p) {
        size_t i = 0;
        while (text[i] && (p[i] | 32) == (text[i] | 32)) {
            ++i;
        }
        if (!text[i]) {
            return 1;
        }
    }
    return 0;
}

int azimuth_progress_hold(const wchar_t *headers, const void *body, size_t size) {
    unsigned now[4];
    if (!pg_headers_have(headers, L"X-Method: UpdateAll") || !pg_headers_have(headers, L"OnlineProfile")) {
        return 0;
    }
    /* sync stopped (azimuth_cloud_stop): no profile sync either until the game restarts */
    if (azimuth_cloud_stopped()) {
        return 1;
    }
    /* a Noob profile has no progress gates (its own save, as it is) */
    if (azimuth_account_noob()) {
        return 0;
    }
    if (!azimuth_progress_read(body, size, now)) {
        return 0;
    }
    AcquireSRWLockExclusive(&pg_lock);
    int over = 0;
    ULONGLONG tick = GetTickCount64();
    if (pg_have_sent) {
        over = azimuth_progress_over(pg_sent, now, (double)(tick - pg_sent_at) / 3600000.0);
    }
    if (!over) {
        memory_copy(pg_sent, now, sizeof(pg_sent));
        pg_sent_at = tick;
        pg_have_sent = 1;
    }
    ReleaseSRWLockExclusive(&pg_lock);
    if (over) {
        static const char *const what[] = {"", "credits", "total winnings", "XP", "level"};
        log_append("PROGRESS: a profile sync was held back (");
        log_append(what[over]);
        log_append(" grew beyond what play earns since the last one sent).\r\n");
        /* fh2online.58: only this sync is held back; the session and its save go on (the server
           measures the save's own progress), and no restore is asked at the next start */
    }
    return over != 0;
}
#endif
