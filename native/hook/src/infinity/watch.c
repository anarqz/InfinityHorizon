/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Watching the other players' cars in a Hardcore world (AC-2; the platform's MATCHMAKING_API 3.9). A
   seated joiner's hook measures every other player's car from what reaches it over the network,
   never its own (the world server never sends a player its own car), and reports what it saw to the
   launcher. It punishes nothing: the server decides on a quorum of the world's players. The host
   measures in its launcher's world server instead (the same algorithm, Infinity.Core PeerWatch).

   The rules come only from the launcher (Watch, 0x25: launch.c), which reads them from the platform
   for the Hardcore world this player is seated in; nothing here is a threshold. Without rules, or
   after a clear, nothing is measured.

   Input (proxy.c): every datagram the world sends the game. When its first NerdCore message is a car
   prediction (id 15, the world server's relay handle naming the car's owner by peer index, then the
   710-bit body: x, y, z as f32 first, the velocity as three s16 at body bit 96 (SAURON V9: s16 / 32767
   x 200 m/s per axis, a provisional scale), the universe in 5 bits at body bit 395, the sender's
   prediction timestamp in the last 32 bits), that is one sample of the owner's car with the local
   receive time. Y is up; the session's first window logs one units line (the velocity against the
   position delta, the stamp delta against the receive delta) so a live capture confirms m/s, Y up
   and the stamp unit.
   A starting grid (39) or a race start (40) first restarts every car (an event start). The world's
   watch roster (fi_watch_roster: a STUN indication 0x3E95 from the world server) names each peer
   index's XUID; a car whose peer index is missing from it is not measured.

   Measurement, per car: speed (horizontal, x and z) and climb (upward, y) from the velocity field,
   averaged over windows of at least WT_WINDOW_MS of receive time and cross-checked by the position
   delta over the same window on both clocks (sender's timestamps and local receive time); a window is
   over a limit only when the velocity and both clocks say so, and the episode's value is the
   smallest of the three. Excluded (the window restarts and the run resets): a
   car's first sample after WT_GAP_MS without one, a universe change, a timestamp that did not
   advance (dropped), an event start, and any move past the jump limit in one sample (fast travel, a
   respawn, a reset or a teleport), which is a jump instead. One report per car and kind per episode,
   when it ends (WT_QUIET_MS without being over): speed and climb only when one run over the limit
   lasted the rule's sustain; jumps always (stored, never counted); the clock ratio (sender ms per
   receiver ms) once per car per rules after WT_CLOCK_MIN_MS of windows (stored, never counted).
   Logs name the observed peer index only: never an identity, a gamertag or an address. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define WT_CARS 16
#define WT_ROSTER 12
#define WT_WINDOW_MS 250u
#define WT_GAP_MS 1500u
#define WT_QUIET_MS 2000u
#define WT_CLOCK_MIN_MS 30000
#define WT_CAR_BITS 710u
#define WT_UNIVERSE_BIT 395u
#define WT_STAMP_BIT (WT_CAR_BITS - 32u)
#define WT_VELOCITY_BIT 96u
#define WT_VELOCITY_SCALE (200.0 / 32767.0) /* m/s per s16 unit (SAURON V9, provisional); a decoding constant */
#define WT_POSITION_MAX 100000.0
#define WT_KIND_SPEED 1u
#define WT_KIND_CLIMB 2u
#define WT_KIND_JUMP 3u
#define WT_KIND_CLOCK 4u
#define WT_ROSTER_KIND 0x3E95u
#define WT_ROSTER_ATTR 0xC0F7u /* 0xC0F5/0xC0F6 are the voice control */
#define WT_LINE 320u

typedef struct {
    int active;
    double peak, peak_velocity, peak_sent, peak_received;
    long long run, max_run, sent_ms, received_ms;
    unsigned long long last_over;
    unsigned disagreed;
} WtEpisode;

typedef struct {
    int used, has;
    unsigned index;
    double x, y, z, ax, ay, az;
    unsigned stamp, anchor_stamp, universe;
    unsigned long long at, anchor_at;
    WtEpisode speed, climb;
    int jump_active;
    double jump_peak;
    unsigned long long jump_last;
    unsigned jumps;
    long long clock_sent, clock_received;
    int clock_done;
    double velocity_horizontal, velocity_up;
    unsigned velocity_samples;
    unsigned first, universes, stale, events;
} WtCar;

static SRWLOCK wt_lock = SRWLOCK_INIT;
static volatile LONG wt_on;
static int wt_force;
static unsigned wt_version, wt_speed_sustain, wt_climb_sustain;
static double wt_speed_max, wt_climb_max, wt_jump_max;
static WtCar wt_cars[WT_CARS];
static const WtCar wt_empty;
static unsigned char wt_roster_index[WT_ROSTER];
static unsigned long long wt_roster_xuid[WT_ROSTER];
static unsigned wt_roster_count;
static int wt_units_logged;

/* ------------------------------------------------------------------ log lines */

static void wt_put(char *line, size_t *n, const char *text) {
    while (*text && *n + 1 < WT_LINE) line[(*n)++] = *text++;
    line[*n] = 0;
}
static void wt_num(char *line, size_t *n, unsigned long long value) {
    char digits[20];
    int d = 0;
    do {
        digits[d++] = (char)('0' + value % 10u);
        value /= 10u;
    } while (value && d < 20);
    while (d && *n + 1 < WT_LINE) line[(*n)++] = digits[--d];
    line[*n] = 0;
}
/* A non-negative double, rounded to a whole number. */
static void wt_whole(char *line, size_t *n, double value) {
    wt_num(line, n, value > 0.0 ? (unsigned long long)(long long)(value + 0.5) : 0u);
}
static void wt_begin(char *line, size_t *n, const WtCar *car) {
    *n = 0;
    line[0] = 0;
    wt_put(line, n, "WATCH: peer ");
    wt_num(line, n, car->index);
    wt_put(line, n, ": ");
}
static void wt_end(char *line, size_t *n) {
    wt_put(line, n, "\r\n");
    log_append(line);
}

/* ------------------------------------------------------------------ arithmetic (no CRT) */

static double wt_sqrt(double value) {
    if (!(value > 0.0)) return 0.0;
    unsigned long long bits;
    double guess;
    memory_copy(&bits, &value, sizeof(bits));
    bits = (bits >> 1) + 0x1FF8000000000000ull; /* halves the exponent: within a few percent */
    memory_copy(&guess, &bits, sizeof(guess));
    for (int i = 0; i < 6; ++i) guess = 0.5 * (guess + value / guess);
    return guess;
}
static double wt_max(double a, double b) { return a > b ? a : b; }
static double wt_min(double a, double b) { return a < b ? a : b; }
/* value x 1000, rounded up (a peak just over a limit never reads as the limit), at most `cap` */
static unsigned wt_milli(double value, unsigned cap) {
    double scaled = value * 1000.0;
    if (!(scaled > 0.0)) return 0;
    if (scaled >= (double)cap) return cap;
    long long whole = (long long)scaled;
    if ((double)whole < scaled) ++whole;
    return (unsigned)whole;
}

/* ------------------------------------------------------------------ reports */

static unsigned long long wt_xuid_of(unsigned index) {
    for (unsigned i = 0; i < wt_roster_count; ++i) {
        if (wt_roster_index[i] == index) return wt_roster_xuid[i];
    }
    return 0;
}

static void wt_report(const WtCar *car, unsigned kind, unsigned value, unsigned sustain) {
    char line[WT_LINE];
    size_t n;
    unsigned long long xuid = wt_xuid_of(car->index);
    wt_begin(line, &n, car);
    if (!xuid) {
        wt_put(line, &n, "no XUID in the world's roster yet; the observation is not reported");
    } else if (fi_launch_peer_observation(xuid, kind, value, sustain, wt_version, car->index)) {
        wt_put(line, &n, "observation queued for the launcher");
    } else {
        wt_put(line, &n, "the observation could not be queued (no live launcher session)");
    }
    wt_end(line, &n);
}

static void wt_end_episode(WtCar *car, WtEpisode *e, unsigned kind, unsigned sustain, unsigned long long now, int force) {
    if (!e->active || (!force && now - e->last_over < WT_QUIET_MS)) return;
    e->active = 0;
    int counted = e->max_run >= (long long)sustain;
    char line[WT_LINE];
    size_t n;
    wt_begin(line, &n, car);
    wt_put(line, &n, kind == WT_KIND_SPEED ? "speed" : "climb");
    wt_put(line, &n, " episode ends: peak ");
    wt_whole(line, &n, e->peak);
    wt_put(line, &n, " km/h (velocity ");
    wt_whole(line, &n, e->peak_velocity);
    wt_put(line, &n, ", sender ");
    wt_whole(line, &n, e->peak_sent);
    wt_put(line, &n, ", receiver ");
    wt_whole(line, &n, e->peak_received);
    wt_put(line, &n, "), longest run ");
    wt_num(line, &n, (unsigned long long)e->max_run);
    wt_put(line, &n, " ms of ");
    wt_num(line, &n, sustain);
    wt_put(line, &n, ", over for ");
    wt_num(line, &n, (unsigned long long)e->sent_ms);
    wt_put(line, &n, " ms sender / ");
    wt_num(line, &n, (unsigned long long)e->received_ms);
    wt_put(line, &n, " ms receiver, measures disagreed ");
    wt_num(line, &n, e->disagreed);
    wt_put(line, &n, "x, exclusions first ");
    wt_num(line, &n, car->first);
    wt_put(line, &n, " universe ");
    wt_num(line, &n, car->universes);
    wt_put(line, &n, " stale ");
    wt_num(line, &n, car->stale);
    wt_put(line, &n, " event ");
    wt_num(line, &n, car->events);
    wt_put(line, &n, " jump ");
    wt_num(line, &n, car->jumps);
    wt_put(line, &n, counted ? ": reported" : ": under the rule, not reported");
    wt_end(line, &n);
    if (counted) {
        wt_report(car, kind, wt_milli(e->peak, 5000000u), e->max_run > 3600000 ? 3600000u : (unsigned)e->max_run);
    }
}

static void wt_end_jump(WtCar *car) {
    if (!car->jump_active) return;
    car->jump_active = 0;
    char line[WT_LINE];
    size_t n;
    wt_begin(line, &n, car);
    wt_put(line, &n, "jump episode ends: ");
    wt_num(line, &n, car->jumps);
    wt_put(line, &n, " jump(s), longest ");
    wt_whole(line, &n, car->jump_peak);
    wt_put(line, &n, " m; reported (log only)");
    wt_end(line, &n);
    wt_report(car, WT_KIND_JUMP, wt_milli(car->jump_peak, 1000000000u), 0);
}

static void wt_end_quiet(WtCar *car, unsigned long long now) {
    wt_end_episode(car, &car->speed, WT_KIND_SPEED, wt_speed_sustain, now, 0);
    wt_end_episode(car, &car->climb, WT_KIND_CLIMB, wt_climb_sustain, now, 0);
    if (car->jump_active && now - car->jump_last >= WT_QUIET_MS) wt_end_jump(car);
}

static void wt_end_all(WtCar *car, unsigned long long now) {
    wt_end_episode(car, &car->speed, WT_KIND_SPEED, wt_speed_sustain, now, 1);
    wt_end_episode(car, &car->climb, WT_KIND_CLIMB, wt_climb_sustain, now, 1);
    wt_end_jump(car);
}

/* Every car's episodes end now (a qualifying one is reported) and every car is forgotten. */
static void wt_flush(unsigned long long now) {
    for (unsigned i = 0; i < WT_CARS; ++i) {
        if (wt_cars[i].used) wt_end_all(&wt_cars[i], now);
        memory_copy(&wt_cars[i], &wt_empty, sizeof(wt_empty));
    }
}

/* ------------------------------------------------------------------ samples */

static void wt_anchor(WtCar *car) {
    car->ax = car->x;
    car->ay = car->y;
    car->az = car->z;
    car->anchor_stamp = car->stamp;
    car->anchor_at = car->at;
    car->velocity_horizontal = 0.0;
    car->velocity_up = 0.0;
    car->velocity_samples = 0;
}

static void wt_restart(WtCar *car, double x, double y, double z, unsigned stamp, unsigned universe, unsigned long long now) {
    car->has = 1;
    car->x = x;
    car->y = y;
    car->z = z;
    car->stamp = stamp;
    car->universe = universe;
    car->at = now;
    car->speed.run = 0;
    car->climb.run = 0;
    wt_anchor(car);
}

static void wt_step(WtCar *car, WtEpisode *e, unsigned kind, double velocity, double sent, double received, double maximum,
                    long long window_sent, long long window_received, unsigned long long now) {
    int over_velocity = velocity > maximum, over_sent = sent > maximum, over_received = received > maximum;
    if (over_velocity && over_sent && over_received) {
        if (!e->active) {
            e->active = 1;
            e->peak = e->peak_velocity = e->peak_sent = e->peak_received = 0.0;
            e->run = e->max_run = e->sent_ms = e->received_ms = 0;
            e->disagreed = 0;
            char line[WT_LINE];
            size_t n;
            wt_begin(line, &n, car);
            wt_put(line, &n, kind == WT_KIND_SPEED ? "speed" : "climb");
            wt_put(line, &n, " episode starts: ");
            wt_whole(line, &n, velocity);
            wt_put(line, &n, " km/h (velocity), ");
            wt_whole(line, &n, sent);
            wt_put(line, &n, " km/h (sender clock), ");
            wt_whole(line, &n, received);
            wt_put(line, &n, " km/h (receiver clock), over ");
            wt_whole(line, &n, maximum);
            wt_end(line, &n);
        }
        e->run += window_received;
        if (e->run > e->max_run) e->max_run = e->run;
        e->peak = wt_max(e->peak, wt_min(velocity, wt_min(sent, received)));
        e->peak_velocity = wt_max(e->peak_velocity, velocity);
        e->peak_sent = wt_max(e->peak_sent, sent);
        e->peak_received = wt_max(e->peak_received, received);
        e->sent_ms += window_sent;
        e->received_ms += window_received;
        e->last_over = now;
        return;
    }
    e->run = 0;
    if ((over_velocity || over_sent || over_received) && e->active) ++e->disagreed;
}

static void wt_jump(WtCar *car, double moved, int sent, unsigned long long received, unsigned long long now) {
    if (!car->jump_active) {
        car->jump_active = 1;
        car->jump_peak = moved;
        car->jumps = 0;
    }
    car->jump_peak = wt_max(car->jump_peak, moved);
    ++car->jumps;
    car->jump_last = now;
    char line[WT_LINE];
    size_t n;
    wt_begin(line, &n, car);
    wt_put(line, &n, "moved ");
    wt_whole(line, &n, moved);
    wt_put(line, &n, " m in one sample (");
    wt_num(line, &n, (unsigned)sent);
    wt_put(line, &n, " ms sender, ");
    wt_num(line, &n, received);
    wt_put(line, &n, " ms receiver): a jump, excluded from speed and climb");
    wt_end(line, &n);
}

static void wt_clock(WtCar *car) {
    if (car->clock_done || car->clock_received < WT_CLOCK_MIN_MS || car->clock_sent <= 0) return;
    car->clock_done = 1;
    double ratio = (double)car->clock_sent / (double)car->clock_received;
    char line[WT_LINE];
    size_t n;
    wt_begin(line, &n, car);
    wt_put(line, &n, "clock ratio x1000 ");
    wt_whole(line, &n, ratio * 1000.0);
    wt_put(line, &n, " (sender ");
    wt_num(line, &n, (unsigned long long)car->clock_sent);
    wt_put(line, &n, " ms / receiver ");
    wt_num(line, &n, (unsigned long long)car->clock_received);
    wt_put(line, &n, " ms)");
    wt_end(line, &n);
    unsigned milli = wt_milli(ratio, 1000000u);
    if (milli) wt_report(car, WT_KIND_CLOCK, milli, 0);
}

static void wt_note(WtCar *car, const char *text, unsigned long long a, const char *middle, unsigned long long b, const char *tail) {
    char line[WT_LINE];
    size_t n;
    wt_begin(line, &n, car);
    wt_put(line, &n, text);
    wt_num(line, &n, a);
    if (middle) {
        wt_put(line, &n, middle);
        wt_num(line, &n, b);
    }
    wt_put(line, &n, tail);
    wt_end(line, &n);
}

static WtCar *wt_car(unsigned index) {
    WtCar *free_slot = 0, *oldest = 0;
    for (unsigned i = 0; i < WT_CARS; ++i) {
        WtCar *c = &wt_cars[i];
        if (c->used && c->index == index) return c;
        if (!c->used && !free_slot) free_slot = c;
        if (c->used && (!oldest || c->at < oldest->at)) oldest = c;
    }
    WtCar *car = free_slot ? free_slot : oldest;
    if (car->used) wt_end_all(car, car->at);
    memory_copy(car, &wt_empty, sizeof(wt_empty));
    car->used = 1;
    car->index = index;
    return car;
}

/* One sample of the car of peer `index`, under wt_lock. */
static void wt_sample(unsigned index, double x, double y, double z, double vx, double vy, double vz, unsigned stamp, unsigned universe,
                      unsigned long long now) {
    WtCar *car = wt_car(index);
    for (unsigned i = 0; i < WT_CARS; ++i) {
        if (wt_cars[i].used) wt_end_quiet(&wt_cars[i], now);
    }
    if (wt_force && wt_xuid_of(index)) {
        /* AC-5, test builds only (the launcher's FI_TEST_FORCE_OBSERVATION): one synthetic speed episode */
        wt_force = 0;
        wt_note(car, "TEST: forced speed episode (rules version ", wt_version, 0, 0, "; never in a production launcher)");
        wt_report(car, WT_KIND_SPEED, wt_milli(wt_speed_max + 50.0, 5000000u), wt_speed_sustain + 1000u);
    }
    if (!car->has || now - car->at > WT_GAP_MS) {
        if (car->has) wt_note(car, "first sample after ", now - car->at, 0, 0, " ms without one; excluded");
        ++car->first;
        wt_restart(car, x, y, z, stamp, universe, now);
        return;
    }
    if (universe != car->universe) {
        ++car->universes;
        wt_note(car, "universe ", car->universe, " -> ", universe, "; excluded");
        wt_restart(car, x, y, z, stamp, universe, now);
        return;
    }
    int sent = (int)(stamp - car->stamp);
    if (sent <= 0) {
        ++car->stale;
        return;
    }
    double dx = x - car->x, dy = y - car->y, dz = z - car->z;
    double moved = wt_sqrt(dx * dx + dy * dy + dz * dz);
    if (moved > wt_jump_max) {
        wt_jump(car, moved, sent, now - car->at, now);
        wt_restart(car, x, y, z, stamp, universe, now);
        car->speed.run = 0;
        car->climb.run = 0;
        return;
    }
    car->x = x;
    car->y = y;
    car->z = z;
    car->stamp = stamp;
    car->at = now;
    car->velocity_horizontal += wt_sqrt(vx * vx + vz * vz);
    car->velocity_up += vy > 0.0 ? vy : 0.0;
    ++car->velocity_samples;
    long long window_received = (long long)(now - car->anchor_at);
    int window_sent = (int)(stamp - car->anchor_stamp);
    if (window_received < (long long)WT_WINDOW_MS) return;
    if (window_sent <= 0) {
        wt_anchor(car);
        return;
    }
    double wx = x - car->ax, wy = y - car->ay, wz = z - car->az;
    double horizontal = wt_sqrt(wx * wx + wz * wz), up = wy > 0.0 ? wy : 0.0;
    /* metres per millisecond x 3600 = km/h */
    double speed_sent = horizontal / (double)window_sent * 3600.0, speed_received = horizontal / (double)window_received * 3600.0;
    double climb_sent = up / (double)window_sent * 3600.0, climb_received = up / (double)window_received * 3600.0;
    /* m/s x 3.6 = km/h */
    double samples = car->velocity_samples ? (double)car->velocity_samples : 1.0;
    double speed_velocity = car->velocity_horizontal / samples * 3.6, climb_velocity = car->velocity_up / samples * 3.6;
    if (!wt_units_logged) {
        /* AC-5: once per session, so a live capture confirms m/s and Y up, and the stamp unit */
        wt_units_logged = 1;
        char line[WT_LINE];
        size_t n;
        wt_begin(line, &n, car);
        wt_put(line, &n, "units check: velocity ");
        wt_whole(line, &n, speed_velocity);
        wt_put(line, &n, " km/h horizontal, ");
        wt_whole(line, &n, climb_velocity);
        wt_put(line, &n, " km/h up; position delta ");
        wt_whole(line, &n, horizontal);
        wt_put(line, &n, " m horizontal, ");
        wt_whole(line, &n, up);
        wt_put(line, &n, " m up: ");
        wt_whole(line, &n, speed_sent);
        wt_put(line, &n, " km/h on ");
        wt_num(line, &n, (unsigned)window_sent);
        wt_put(line, &n, " stamp units (sender), ");
        wt_whole(line, &n, speed_received);
        wt_put(line, &n, " km/h on ");
        wt_num(line, &n, (unsigned long long)window_received);
        wt_put(line, &n, " ms (receiver)");
        wt_end(line, &n);
    }
    car->clock_sent += window_sent;
    car->clock_received += window_received;
    wt_step(car, &car->speed, WT_KIND_SPEED, speed_velocity, speed_sent, speed_received, wt_speed_max, window_sent, window_received, now);
    wt_step(car, &car->climb, WT_KIND_CLIMB, climb_velocity, climb_sent, climb_received, wt_climb_max, window_sent, window_received, now);
    wt_anchor(car);
    wt_clock(car);
}

/* ------------------------------------------------------------------ the world's datagrams */

/* LSB-first bits of a NerdCore body (as follow.c and Infinity.Core's BitReader). */
static unsigned wt_bits(const unsigned char *data, size_t bits_end, size_t *at, unsigned width, int *ok) {
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

static double wt_float(unsigned bits, int *ok) {
    float f;
    memory_copy(&f, &bits, sizeof(f));
    if (!float_is_finite(f) || f > (float)WT_POSITION_MAX || f < -(float)WT_POSITION_MAX) *ok = 0;
    return (double)f;
}

void fi_watch_datagram(const unsigned char *data, size_t size) {
    if (!wt_on || !data || size < 12u || size > 0xffffu || !data[8] || (((size_t)data[10] << 8) | data[9]) != size) return;
    const unsigned char *body = data + 11;
    size_t end = (size - 11u) * 8u, at = 0;
    int ok = 1;
    unsigned id = wt_bits(body, end, &at, 7, &ok);
    if (!ok) return;
    unsigned long long now = GetTickCount64();
    if (id == 39u || id == 40u) {
        /* a starting grid or a race start: an event starts, every car's measurement restarts */
        AcquireSRWLockExclusive(&wt_lock);
        int any = 0;
        for (unsigned i = 0; i < WT_CARS; ++i) {
            if (!wt_cars[i].used) continue;
            wt_cars[i].has = 0;
            wt_cars[i].speed.run = 0;
            wt_cars[i].climb.run = 0;
            ++wt_cars[i].events;
            any = 1;
        }
        ReleaseSRWLockExclusive(&wt_lock);
        if (any) log_append(id == 39u ? "WATCH: a starting grid: every car's measurement restarts; excluded.\r\n"
                                      : "WATCH: a race start: every car's measurement restarts; excluded.\r\n");
        return;
    }
    if (id != 15u) return;
    unsigned a = wt_bits(body, end, &at, 1, &ok), b = wt_bits(body, end, &at, 1, &ok);
    wt_bits(body, end, &at, 7, &ok); /* the owner's car id */
    if (!ok || a || !b) return;      /* only a relayed car names its owner */
    unsigned owner = wt_bits(body, end, &at, 8, &ok);
    if (!ok || !owner || owner == 255u || at + WT_CAR_BITS > end) return;
    size_t start = at;
    double x = wt_float(wt_bits(body, end, &at, 32, &ok), &ok);
    double y = wt_float(wt_bits(body, end, &at, 32, &ok), &ok);
    double z = wt_float(wt_bits(body, end, &at, 32, &ok), &ok);
    at = start + WT_VELOCITY_BIT;
    double vx = (double)(short)wt_bits(body, end, &at, 16, &ok) * WT_VELOCITY_SCALE;
    double vy = (double)(short)wt_bits(body, end, &at, 16, &ok) * WT_VELOCITY_SCALE;
    double vz = (double)(short)wt_bits(body, end, &at, 16, &ok) * WT_VELOCITY_SCALE;
    at = start + WT_UNIVERSE_BIT;
    unsigned universe = wt_bits(body, end, &at, 5, &ok);
    at = start + WT_STAMP_BIT;
    unsigned stamp = wt_bits(body, end, &at, 32, &ok);
    if (!ok) return;
    AcquireSRWLockExclusive(&wt_lock);
    /* a peer missing from the roster is not measured (5e): no identity to report it under */
    if (wt_on && wt_xuid_of(owner)) wt_sample(owner, x, y, z, vx, vy, vz, stamp, universe, now);
    ReleaseSRWLockExclusive(&wt_lock);
}

/* The world server's watch roster (the platform's MATCHMAKING_API 5e): {u8 1, u8 count 0-12, count x {u8
   index, u64 BE xuid}}, padded to 4 bytes, in attribute 0xC0F7 of a STUN indication 0x3E95 (proxy.c checks
   it came from the world). A wrong version or length is ignored (the roster in hand stays). */
void fi_watch_roster(const unsigned char *d, int n) {
    if (n < 20 || (d[0] & 0xc0) || ((unsigned)d[0] << 8 | d[1]) != WT_ROSTER_KIND || d[4] != 0x21 || d[5] != 0x12 ||
        d[6] != 0xa4 || d[7] != 0x42 || ((int)((unsigned)d[2] << 8 | d[3])) + 20 != n) {
        return;
    }
    const unsigned char *v = 0;
    unsigned len = 0;
    for (int at = 20; at + 4 <= n;) {
        unsigned type = (unsigned)d[at] << 8 | d[at + 1];
        len = (unsigned)d[at + 2] << 8 | d[at + 3];
        if (at + 4 + (int)len > n) return;
        if (type == WT_ROSTER_ATTR) {
            v = d + at + 4;
            break;
        }
        at += 4 + (int)((len + 3u) & ~3u);
    }
    if (!v || len < 2u || v[0] != 1 || v[1] > WT_ROSTER || len != 2u + 9u * v[1]) return;
    unsigned long long own = azimuth_account_xuid_value();
    AcquireSRWLockExclusive(&wt_lock);
    int changed = v[1] != wt_roster_count;
    unsigned count = 0;
    for (unsigned i = 0; i < v[1]; ++i) {
        const unsigned char *e = v + 2 + 9 * i;
        unsigned long long xuid = 0;
        for (int k = 0; k < 8; ++k) xuid = xuid << 8 | e[1 + k];
        if (!xuid || xuid == own || !e[0] || e[0] == 255) continue;
        if (count >= wt_roster_count || wt_roster_index[count] != e[0] || wt_roster_xuid[count] != xuid) changed = 1;
        wt_roster_index[count] = e[0];
        wt_roster_xuid[count] = xuid;
        ++count;
    }
    if (count != wt_roster_count) changed = 1;
    wt_roster_count = count;
    ReleaseSRWLockExclusive(&wt_lock);
    if (changed) {
        char line[WT_LINE];
        size_t k = 0;
        line[0] = 0;
        wt_put(line, &k, "WATCH: the world's roster names ");
        wt_num(line, &k, count);
        wt_put(line, &k, " other player(s).");
        wt_end(line, &k);
    }
}

/* ------------------------------------------------------------------ the launcher's rules */

static unsigned wt_u32(const unsigned char *p) {
    return (unsigned)p[0] | (unsigned)p[1] << 8 | (unsigned)p[2] << 16 | (unsigned)p[3] << 24;
}

/* Watch (0x25) from the launcher: {u8 1, u8 flags, u32 version, u32 speed milli-km/h, u32 speed sustain
   ms, u32 climb milli-km/h, u32 climb sustain ms, u32 jump mm}, all LE. Version 0 with every value 0
   clears. 0 when it does not parse (a protocol error: launch.c refuses the frame), else 1. */
int fi_watch_rules(const unsigned char *p, size_t n) {
    if (!p || n != 26u || p[0] != 1 || (p[1] & ~1u)) return 0;
    unsigned version = wt_u32(p + 2), speed = wt_u32(p + 6), speed_sustain = wt_u32(p + 10), climb = wt_u32(p + 14),
             climb_sustain = wt_u32(p + 18), jump = wt_u32(p + 22);
    if (!version) {
        if (p[1] || speed || speed_sustain || climb || climb_sustain || jump) return 0;
    } else if (!speed || speed > 5000000u || !climb || climb > 5000000u || !speed_sustain || speed_sustain > 3600000u ||
               !climb_sustain || climb_sustain > 3600000u || !jump || jump > 1000000000u) {
        return 0;
    }
    unsigned long long now = GetTickCount64();
    AcquireSRWLockExclusive(&wt_lock);
    wt_flush(now);
    wt_version = version;
    wt_speed_max = (double)speed / 1000.0;
    wt_speed_sustain = speed_sustain;
    wt_climb_max = (double)climb / 1000.0;
    wt_climb_sustain = climb_sustain;
    wt_jump_max = (double)jump / 1000.0;
    wt_force = version && (p[1] & 1u);
    wt_units_logged = 0;
    InterlockedExchange(&wt_on, version ? 1 : 0);
    ReleaseSRWLockExclusive(&wt_lock);
    char line[WT_LINE];
    size_t k = 0;
    line[0] = 0;
    if (!version) {
        wt_put(line, &k, "WATCH: off (no Hardcore world); nothing is measured.");
    } else {
        wt_put(line, &k, "WATCH: on, rules version ");
        wt_num(line, &k, version);
        wt_put(line, &k, ": speed over ");
        wt_num(line, &k, speed / 1000u);
        wt_put(line, &k, " km/h for ");
        wt_num(line, &k, speed_sustain);
        wt_put(line, &k, " ms, climb over ");
        wt_num(line, &k, climb / 1000u);
        wt_put(line, &k, " km/h for ");
        wt_num(line, &k, climb_sustain);
        wt_put(line, &k, " ms, jump ");
        wt_num(line, &k, jump / 1000u);
        wt_put(line, &k, " m (log only)");
        if (p[1] & 1u) wt_put(line, &k, "; TEST: one forced speed episode");
        wt_put(line, &k, ".");
    }
    wt_end(line, &k);
    return 1;
}

/* The launcher session ended or went offline: episodes end, nothing more is measured. */
void fi_watch_stop(void) {
    if (!wt_on) return;
    AcquireSRWLockExclusive(&wt_lock);
    wt_flush(GetTickCount64());
    InterlockedExchange(&wt_on, 0);
    wt_force = 0;
    ReleaseSRWLockExclusive(&wt_lock);
    log_append("WATCH: off (the launcher session ended).\r\n");
}
