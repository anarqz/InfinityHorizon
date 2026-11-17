/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* FH2Online free roam: what ExtraOptions and its mods change on a player's own car and the game
   never sends, shown on that car in the other players' games (docs/contracts/MATCHMAKING_API.md
   §5f, CR-L53, OD-248):
   - 0xC0F1, layout 1 (unchanged byte for byte): its headlights forced on or off, its roof;
   - 0xC0FC: FH2 ANNA's parts (doors, hood, trunk...) and signals (hazards, left, right): sent only
     while FH2ANNA.dll is loaded in this process, and applied only while it is loaded here;
   - 0xC0FD, 0..2 per datagram: an opaque blob of a mod registered through the hook's API
     (include/infinity_carsync.h; Underglow): kept only for a mod registered in this process.
   All of it only while the supported ExtraOptions (FI_EO_NAME: the player's 0.2.98, or the 0.6.1
   inside ExtraOptions.Infinity) is active in this process (fi_eo_sync_enabled, eo_detect.c):
   without it nothing is sent, and what the relay passes is dropped. The relay forwards these
   indications (each mod's only between two players who both have it), so hooks with and without
   ExtraOptions (and older hooks, which read 0xC0F1 alone) share a world unchanged.

   Each hook tells the world's relay its car's state with a private STUN indication (the relay's
   carstate.rs has the format): from its proxy's socket (azimuth_proxy.c, the address the relay
   knows it by; OD-208: this PC never hosts a world). The state goes when it changes (at most once
   per CS_CHANGE_MS, ten a second) and every second (the relay passes the others' states only to a
   hook that speaks). The relay passes each other player's state with that player's XUID and the
   position of its car, taken from its last CarPredictionFull as it is sent.

   The game's car objects do not say whose they are, so a remote car (ForzaCar kind 2 or 5,
   carstate_hook.c) is matched by position: the first car within CS_MATCH_METERS of a player's
   position heard less than CS_FRESH_MS ago is that player's, and stays so until, with a fresh
   position, it is CS_UNMATCH_METERS away. carstate_hook.c asks for each such car what to show
   (azimuth_carstate_wanted) and shows it as ExtraOptions shows the local car's. */
#define INFINITY_CARSYNC_EXPORTS 1
#include "infinity.h"
#include "../../include/infinity_carsync.h"
#include "../core/logging.h"
#include "../core/support.h"

#define CS_KIND 0x3E91u
#define CS_ATTR 0xC0F1u
#define CS_PARTS_ATTR 0xC0FCu
#define CS_MOD_ATTR 0xC0FDu
#define CS_VERSION 1
#define CS_PARTS_VERSION 1
#define CS_UP_LEN 16
#define CS_DOWN_LEN 28
#define CS_PARTS_LEN 8
#define CS_UP_MAX (20 + 4 + CS_UP_LEN + 4 + CS_PARTS_LEN + FI_CS_BLOBS_PER_DATAGRAM * (4 + 4 + FI_CS_BLOB_MAX))
#define CS_MODS ((int)INFINITY_CARSYNC_MODS_MAX)
#define CS_MOD_CANDIDATES 48 /* 0xC0FD attributes looked at in one datagram (548 bytes hold at most 44) */
#define CS_REMOTES 16
#define CS_RESEND_MS 1000u
#define CS_CHANGE_MS 100u
#define CS_FRESH_MS 300u
#define CS_FORGET_MS 5000u
#define CS_ANNA_CHECK_MS 1000u
#define CS_CAR_GONE_MS 1000u
#define CS_MATCH_METERS 15.0f
#define CS_UNMATCH_METERS 30.0f

_Static_assert(FI_CS_BLOB_MAX == INFINITY_CARSYNC_BLOB_MAX, "one blob size");
_Static_assert(CS_UP_MAX <= 548, "an up datagram fits the relay's STUN MAX_LEN");

/* the clock and the module lookup (the unit tests give their own) */
#ifndef CS_NOW
#define CS_NOW() GetTickCount64()
#endif
#ifndef CS_MODULE_LOADED
#define CS_MODULE_LOADED(name) (GetModuleHandleW(name) != 0)
#endif

typedef struct {
    unsigned char id, version, length;
    unsigned char bytes[FI_CS_BLOB_MAX];
    unsigned long long heard_at;
} CsBlob;

typedef struct {
    int used;
    unsigned char lights, roof;
    int has_parts;
    unsigned char signals;
    unsigned short parts;
    unsigned long long xuid;
    float position[3];
    int has_position;
    unsigned long long heard_at, position_at;
    void *car; /* the car it is shown on, once matched */
    unsigned long long car_seen_at; /* that car's last update: a car gone for CS_CAR_GONE_MS frees the player */
    CsBlob blobs[CS_MODS];
} CsRemote;

/* a mod registered in this process, and the local car's blob it published */
typedef struct {
    unsigned char id, version, length;
    int dirty; /* changed since it last went out */
    unsigned char bytes[FI_CS_BLOB_MAX];
} CsMod;

static CsRemote cs_remotes[CS_REMOTES];
static CsMod cs_mods[CS_MODS];
static int cs_mod_count, cs_mod_cursor;
static SRWLOCK cs_lock = SRWLOCK_INIT;
static volatile LONG cs_remote_count;
static int cs_lights = -1, cs_roof = -1, cs_has_parts = -1, cs_signals = -1, cs_parts = -1;
static unsigned long long cs_sent_at;
static unsigned short cs_seq;

/* ------------------------------------------------------------------- the wire */

static void cs_put16(unsigned char *p, unsigned v) {
    p[0] = (unsigned char)(v >> 8);
    p[1] = (unsigned char)v;
}

static void cs_put64(unsigned char *p, unsigned long long v) {
    for (int i = 0; i < 8; ++i) {
        p[i] = (unsigned char)(v >> (56 - 8 * i));
    }
}

static unsigned cs_get16(const unsigned char *p) {
    return (unsigned)(p[0] << 8 | p[1]);
}

static float cs_float(const unsigned char *p) {
    unsigned bits = (unsigned)p[0] << 24 | (unsigned)p[1] << 16 | (unsigned)p[2] << 8 | p[3];
    float f;
    memory_copy(&f, &bits, sizeof(f));
    return f;
}

/* zeroed byte by byte (volatile: the freestanding DLL has no memset to be turned into) */
static void cs_zero(void *at, size_t size) {
    volatile unsigned char *p = at;
    while (size--) {
        *p++ = 0;
    }
}

/* A sync id that may carry a blob: 1..255 but 2 (FH2 ANNA's 0xC0FC, no blob). */
static int cs_blob_id(unsigned id) {
    return id >= 1 && id <= 255 && id != INFINITY_CARSYNC_ID_ANNA;
}

int fi_carstate_encode_up(unsigned char *out, int capacity, const FiCarState *s) {
    if (s->lights > 2 || s->roof > 1 || (s->has_parts && (s->signals > 3 || (s->parts & ~FI_CS_PARTS_MASK) ||
                                                          (s->parts & 1u) != s->roof)) ||
        s->blob_count < 0 || s->blob_count > FI_CS_BLOBS_PER_DATAGRAM) {
        return 0;
    }
    int n = 20 + 4 + CS_UP_LEN + (s->has_parts ? 4 + CS_PARTS_LEN : 0);
    for (int i = 0; i < s->blob_count; ++i) {
        const FiCarBlob *b = &s->blobs[i];
        if (!cs_blob_id(b->id) || b->length < 1 || b->length > FI_CS_BLOB_MAX) {
            return 0;
        }
        for (int j = 0; j < i; ++j) {
            if (s->blobs[j].id == b->id) {
                return 0;
            }
        }
        n += 4 + ((4 + b->length + 3) & ~3);
    }
    if (n > capacity) {
        return 0;
    }
    cs_zero(out, (size_t)n);
    cs_put16(out, CS_KIND);
    cs_put16(out + 2, (unsigned)(n - 20));
    out[4] = 0x21, out[5] = 0x12, out[6] = 0xa4, out[7] = 0x42;
    unsigned long long noise = CS_NOW() * 0x9e3779b97f4a7c15ull ^ (unsigned long long)(uintptr_t)&noise;
    for (int i = 0; i < 12; ++i) {
        out[8 + i] = (unsigned char)(noise >> (8 * (i % 8))) ^ (unsigned char)i;
    }
    unsigned char *a = out + 20;
    cs_put16(a, CS_ATTR);
    cs_put16(a + 2, CS_UP_LEN);
    a[4] = CS_VERSION;
    a[5] = s->lights;
    a[6] = s->roof;
    cs_put16(a + 8, s->sequence);
    cs_put64(a + 12, s->xuid);
    a += 4 + CS_UP_LEN;
    if (s->has_parts) {
        cs_put16(a, CS_PARTS_ATTR);
        cs_put16(a + 2, CS_PARTS_LEN);
        a[4] = CS_PARTS_VERSION;
        a[5] = s->signals;
        cs_put16(a + 6, s->parts);
        a += 4 + CS_PARTS_LEN;
    }
    for (int i = 0; i < s->blob_count; ++i) {
        const FiCarBlob *b = &s->blobs[i];
        cs_put16(a, CS_MOD_ATTR);
        cs_put16(a + 2, 4u + b->length);
        a[4] = b->id;
        a[5] = b->version;
        a[6] = b->length;
        memory_copy(a + 8, b->bytes, b->length);
        a += 4 + ((4 + b->length + 3) & ~3);
    }
    return n;
}

int fi_carstate_decode_down(const unsigned char *d, int n, FiCarState *s) {
    if (n < 20 || (d[0] & 0xc0) || cs_get16(d) != CS_KIND || d[4] != 0x21 || d[5] != 0x12 || d[6] != 0xa4 ||
        d[7] != 0x42 || (int)cs_get16(d + 2) + 20 != n) {
        return 0;
    }
    const unsigned char *core = 0, *parts = 0;
    const unsigned char *mods[FI_CS_BLOBS_PER_DATAGRAM], *seen[CS_MOD_CANDIDATES];
    int mod_count = 0, seen_count = 0;
    for (int at = 20; at + 4 <= n;) {
        unsigned type = cs_get16(d + at), len = cs_get16(d + at + 2);
        if (at + 4 + (int)len > n) {
            return 0;
        }
        const unsigned char *v = d + at + 4;
        if (type == CS_ATTR && len == CS_DOWN_LEN && !core) {
            core = v;
        } else if (type == CS_PARTS_ATTR && len == CS_PARTS_LEN && !parts) {
            parts = v;
        } else if (type == CS_MOD_ATTR && len >= 5 && v[2] >= 1 && v[2] <= FI_CS_BLOB_MAX && len == 4u + v[2] &&
                   !v[3] && cs_blob_id(v[0])) {
            if (seen_count < CS_MOD_CANDIDATES) {
                seen[seen_count++] = v;
            }
        }
        at += 4 + (int)((len + 3) & ~3u);
    }
    /* a sync id repeated in one datagram drops every blob of it (§5f); then the first two */
    for (int i = 0; i < seen_count && mod_count < FI_CS_BLOBS_PER_DATAGRAM; ++i) {
        int repeats = 0;
        for (int j = 0; j < seen_count; ++j) {
            repeats += seen[j][0] == seen[i][0];
        }
        if (repeats == 1) {
            mods[mod_count++] = seen[i];
        }
    }
    if (!core || core[0] != CS_VERSION || core[1] > 2 || core[2] > 1) {
        return 0;
    }
    cs_zero(s, sizeof(*s));
    s->lights = core[1];
    s->roof = core[2];
    s->peer = core[3];
    s->sequence = (unsigned short)cs_get16(core + 4);
    s->flags = core[6];
    for (int i = 0; i < 8; ++i) {
        s->xuid = s->xuid << 8 | core[8 + i];
    }
    float position[3] = {cs_float(core + 16), cs_float(core + 20), cs_float(core + 24)};
    s->has_position = (core[6] & 1) && float_is_finite(position[0]) && float_is_finite(position[1]) &&
                      float_is_finite(position[2]);
    memory_copy(s->position, position, sizeof(position));
    if (parts) {
        unsigned bits = cs_get16(parts + 2);
        if (parts[0] == CS_PARTS_VERSION && parts[1] <= 3 && !(bits & ~FI_CS_PARTS_MASK) && (bits & 1u) == s->roof) {
            s->has_parts = 1;
            s->signals = parts[1];
            s->parts = (unsigned short)bits;
        }
    }
    for (int i = 0; i < mod_count; ++i) {
        FiCarBlob *b = &s->blobs[s->blob_count++];
        b->id = mods[i][0];
        b->version = mods[i][1];
        b->length = mods[i][2];
        memory_copy(b->bytes, mods[i] + 4, b->length);
    }
    return 1;
}

/* ------------------------------------------------------------------- FH2 ANNA */

static volatile LONG cs_anna;
static unsigned long long cs_anna_at;

int fi_anna_loaded(void) {
    unsigned long long now = CS_NOW();
    if (!cs_anna_at || now - cs_anna_at >= CS_ANNA_CHECK_MS) {
        InterlockedExchange(&cs_anna, CS_MODULE_LOADED(L"FH2ANNA.dll") ? 1 : 0);
        cs_anna_at = now ? now : 1;
    }
    return cs_anna != 0;
}

/* ------------------------------------------------------------------- local state */

/* Up to two published blobs for this datagram, changed ones first, then in turn (only when more
   than two mods publish does a blob wait for the next datagram). Under cs_lock. */
static int cs_pick_blobs(FiCarState *s, int *picked) {
    int changed = 0;
    s->blob_count = 0;
    for (int pass = 0; pass < 2; ++pass) {
        for (int k = 0; k < cs_mod_count && s->blob_count < FI_CS_BLOBS_PER_DATAGRAM; ++k) {
            int i = (cs_mod_cursor + k) % cs_mod_count;
            CsMod *m = &cs_mods[i];
            int taken = 0;
            for (int j = 0; j < s->blob_count; ++j) {
                taken |= picked[j] == i;
            }
            if (!m->length || taken || (pass == 0 && !m->dirty)) {
                continue;
            }
            changed |= m->dirty;
            picked[s->blob_count] = i;
            FiCarBlob *b = &s->blobs[s->blob_count++];
            b->id = m->id;
            b->version = m->version;
            b->length = m->length;
            memory_copy(b->bytes, m->bytes, m->length);
        }
    }
    return changed;
}

void azimuth_carstate_local(int lights, int roof, int signals, unsigned parts) {
    if (!fi_eo_sync_enabled()) {
        return;
    }
    unsigned long long xuid = azimuth_account_xuid_value(), now = CS_NOW();
    if (!xuid) {
        return;
    }
    FiCarState s;
    cs_zero(&s, sizeof(s));
    s.lights = (unsigned char)lights;
    s.roof = (unsigned char)(roof ? 1 : 0);
    s.has_parts = fi_anna_loaded();
    s.signals = (unsigned char)(s.has_parts && signals >= 0 && signals <= 3 ? signals : 0);
    s.parts = (unsigned short)(s.has_parts ? (parts & FI_CS_PARTS_MASK & ~1u) | s.roof : 0);
    s.xuid = xuid;
    int picked[FI_CS_BLOBS_PER_DATAGRAM] = {0};
    AcquireSRWLockExclusive(&cs_lock);
    int changed = cs_pick_blobs(&s, picked);
    ReleaseSRWLockExclusive(&cs_lock);
    changed |= lights != cs_lights || s.roof != cs_roof || s.has_parts != cs_has_parts || s.signals != cs_signals ||
               s.parts != cs_parts;
    if ((!changed && now - cs_sent_at < CS_RESEND_MS) || (changed && cs_sent_at && now - cs_sent_at < CS_CHANGE_MS)) {
        return;
    }
    s.sequence = ++cs_seq;
    unsigned char m[CS_UP_MAX];
    int n = fi_carstate_encode_up(m, (int)sizeof(m), &s);
    /* from the proxy's socket, or, for a world on this network the game reached directly (no proxy),
       from the world's own LAN socket (livery_world.c). OD-208: this PC never hosts a world. */
    int sent = n && (azimuth_proxy_send_world(m, n) || azimuth_world_lan_send(m, n));
    if (sent) {
        cs_lights = lights;
        cs_roof = s.roof;
        cs_has_parts = s.has_parts;
        cs_signals = s.signals;
        cs_parts = s.parts;
        cs_sent_at = now;
        AcquireSRWLockExclusive(&cs_lock);
        for (int j = 0; j < s.blob_count; ++j) {
            for (int i = 0; i < cs_mod_count; ++i) {
                if (cs_mods[i].id == s.blobs[j].id && cs_mods[i].length == s.blobs[j].length &&
                    memory_equal(cs_mods[i].bytes, s.blobs[j].bytes, s.blobs[j].length)) {
                    cs_mods[i].dirty = 0;
                }
            }
        }
        if (cs_mod_count) {
            cs_mod_cursor = (cs_mod_cursor + s.blob_count) % cs_mod_count;
        }
        ReleaseSRWLockExclusive(&cs_lock);
    }
}

/* ----------------------------------------------------------------- remote states */

static CsMod *cs_registered(unsigned id) {
    for (int i = 0; i < cs_mod_count; ++i) {
        if (cs_mods[i].id == id) {
            return &cs_mods[i];
        }
    }
    return 0;
}

void azimuth_carstate_received(const unsigned char *d, int n) {
    FiCarState s;
    if (!fi_eo_sync_enabled() || !fi_carstate_decode_down(d, n, &s)) {
        return;
    }
    if (!s.xuid || s.xuid == azimuth_account_xuid_value()) {
        return;
    }
    /* 0xC0FC is kept only while FH2 ANNA is loaded here (§5f layer 3): dropped unread otherwise */
    int anna = s.has_parts && fi_anna_loaded();
    unsigned long long now = CS_NOW();
    AcquireSRWLockExclusive(&cs_lock);
    CsRemote *r = 0, *free_slot = 0, *oldest = 0;
    for (int i = 0; i < CS_REMOTES; ++i) {
        CsRemote *c = &cs_remotes[i];
        if (c->used && c->xuid == s.xuid) {
            r = c;
            break;
        }
        if (!c->used && !free_slot) {
            free_slot = c;
        }
        if (c->used && (!oldest || c->heard_at < oldest->heard_at)) {
            oldest = c;
        }
    }
    int fresh = !r;
    if (!r) {
        r = free_slot ? free_slot : oldest;
        cs_zero(r, sizeof(*r));
        r->used = 1;
        r->xuid = s.xuid;
    }
    r->lights = s.lights;
    r->roof = s.roof;
    r->has_parts = anna;
    r->signals = anna ? s.signals : 0;
    r->parts = anna ? s.parts : 0;
    r->heard_at = now;
    if (s.has_position) {
        memory_copy(r->position, s.position, sizeof(s.position));
        r->has_position = 1;
        r->position_at = now;
    }
    /* a blob only for a mod registered in this process (§5f layer 3); the others are dropped unread */
    for (int j = 0; j < s.blob_count; ++j) {
        const FiCarBlob *b = &s.blobs[j];
        if (!cs_registered(b->id)) {
            continue;
        }
        CsBlob *slot = 0;
        for (int i = 0; i < CS_MODS && !slot; ++i) {
            if (r->blobs[i].length && r->blobs[i].id == b->id) {
                slot = &r->blobs[i];
            }
        }
        for (int i = 0; i < CS_MODS && !slot; ++i) {
            if (!r->blobs[i].length || now - r->blobs[i].heard_at >= CS_FORGET_MS || !cs_registered(r->blobs[i].id)) {
                slot = &r->blobs[i];
            }
        }
        if (slot) {
            slot->id = b->id;
            slot->version = b->version;
            slot->length = b->length;
            memory_copy(slot->bytes, b->bytes, b->length);
            slot->heard_at = now;
        }
    }
    LONG count = 0;
    for (int i = 0; i < CS_REMOTES; ++i) {
        count += cs_remotes[i].used;
    }
    InterlockedExchange(&cs_remote_count, count);
    ReleaseSRWLockExclusive(&cs_lock);
    if (fresh) {
        log_append("CARSTATE: a player's car state is heard from the relay.\r\n");
    }
}

void azimuth_carstate_unmatch(void *car) {
    if (!car || !cs_remote_count) {
        return;
    }
    AcquireSRWLockExclusive(&cs_lock);
    for (int i = 0; i < CS_REMOTES; ++i) {
        if (cs_remotes[i].car == car) {
            cs_remotes[i].car = 0;
        }
    }
    ReleaseSRWLockExclusive(&cs_lock);
}

int azimuth_carstate_any(void) {
    return cs_remote_count != 0;
}

/* A position is used only just after the relay sent one: to match a car not yet matched, or to
   check the one matched. Otherwise the car's own position is not read at all. */
int azimuth_carstate_needs_position(void *car) {
    unsigned long long now = CS_NOW();
    int needed = 0;
    AcquireSRWLockExclusive(&cs_lock);
    int matched = 0;
    for (int i = 0; i < CS_REMOTES && !matched; ++i) {
        matched = cs_remotes[i].used && cs_remotes[i].car == car;
    }
    for (int i = 0; i < CS_REMOTES && !needed; ++i) {
        const CsRemote *r = &cs_remotes[i];
        needed = r->used && r->has_position && now - r->position_at < CS_FRESH_MS &&
                 (matched ? r->car == car : !r->car || now - r->car_seen_at >= CS_CAR_GONE_MS);
    }
    ReleaseSRWLockExclusive(&cs_lock);
    return needed;
}

static float cs_distance2(const float *a, const float *b) {
    float d = 0;
    for (int i = 0; i < 3; ++i) {
        d += (a[i] - b[i]) * (a[i] - b[i]);
    }
    return d;
}

static void cs_fill(const CsRemote *r, FiCarWanted *w) {
    w->lights = r->lights;
    w->roof = r->roof;
    w->has_parts = r->has_parts;
    w->signals = r->signals;
    w->parts = r->has_parts ? r->parts : r->roof;
}

int azimuth_carstate_wanted(void *car, const float position[3], FiCarWanted *wanted) {
    unsigned long long now = CS_NOW();
    int found = 0, matched = 0;
    AcquireSRWLockExclusive(&cs_lock);
    CsRemote *best = 0;
    float best_d2 = CS_MATCH_METERS * CS_MATCH_METERS;
    LONG count = 0;
    for (int i = 0; i < CS_REMOTES; ++i) {
        CsRemote *r = &cs_remotes[i];
        if (r->used && now - r->heard_at >= CS_FORGET_MS) {
            r->used = 0; /* left the world */
        }
        if (r->used && r->car && r->car != car && now - r->car_seen_at >= CS_CAR_GONE_MS) {
            r->car = 0; /* its car is gone (the player changed car, or it left the area) */
        }
        count += r->used;
    }
    for (int i = 0; i < CS_REMOTES; ++i) {
        CsRemote *r = &cs_remotes[i];
        if (!r->used) {
            continue;
        }
        int recent = r->has_position && now - r->position_at < CS_FRESH_MS;
        if (r->car == car) {
            if (position && recent && cs_distance2(position, r->position) > CS_UNMATCH_METERS * CS_UNMATCH_METERS) {
                r->car = 0; /* not that player's car after all (or it changed car) */
                continue;
            }
            r->car_seen_at = now;
            cs_fill(r, wanted);
            found = 1;
            break;
        }
        if (!r->car && position && recent) {
            float d2 = cs_distance2(position, r->position);
            if (d2 < best_d2) {
                best_d2 = d2;
                best = r;
            }
        }
    }
    if (!found && best) {
        best->car = car;
        best->car_seen_at = now;
        cs_fill(best, wanted);
        found = matched = 1;
    }
    InterlockedExchange(&cs_remote_count, count);
    ReleaseSRWLockExclusive(&cs_lock);
    if (matched) {
        log_append("CARSTATE: a player's car found by its position; its car state is shown on it.\r\n");
    }
    return found;
}

/* ------------------------------------------------- the mods' API (infinity_carsync.h) */

uint32_t InfinityCarSync_Version(void) {
    return INFINITY_CARSYNC_VERSION;
}

int32_t InfinityCarSync_Info(InfinityCarSyncInfo *info) {
    if (!info || info->size < 8) {
        return INFINITY_CARSYNC_E_ARGUMENT;
    }
    InfinityCarSyncInfo mine = {sizeof(mine), INFINITY_CARSYNC_VERSION, INFINITY_CARSYNC_BLOB_MAX,
                                INFINITY_CARSYNC_MODS_MAX, fi_eo_sync_enabled() ? 1u : 0u};
    uint32_t size = info->size < sizeof(mine) ? info->size : (uint32_t)sizeof(mine);
    mine.size = size;
    memory_copy(info, &mine, size);
    return INFINITY_CARSYNC_OK;
}

int32_t InfinityCarSync_Register(uint32_t abi_version, uint8_t sync_id, uint8_t blob_version) {
    if (abi_version != INFINITY_CARSYNC_VERSION) {
        return INFINITY_CARSYNC_E_VERSION;
    }
    if (!cs_blob_id(sync_id)) {
        return INFINITY_CARSYNC_E_ID;
    }
    int32_t result = INFINITY_CARSYNC_OK, fresh = 0;
    AcquireSRWLockExclusive(&cs_lock);
    CsMod *m = cs_registered(sync_id);
    if (m) {
        m->dirty |= m->version != blob_version && m->length;
        m->version = blob_version;
    } else if (cs_mod_count >= CS_MODS) {
        result = INFINITY_CARSYNC_E_FULL;
    } else {
        m = &cs_mods[cs_mod_count++];
        cs_zero(m, sizeof(*m));
        m->id = sync_id;
        m->version = blob_version;
        fresh = 1;
    }
    ReleaseSRWLockExclusive(&cs_lock);
    if (fresh) {
        log_append_value("CARSTATE: a mod registered for car-state sync, sync id ", sync_id);
    }
    return result;
}

int32_t InfinityCarSync_Publish(uint8_t sync_id, const void *bytes, uint32_t length) {
    if (!length || length > INFINITY_CARSYNC_BLOB_MAX || !bytes) {
        return INFINITY_CARSYNC_E_LENGTH; /* "off" is a blob of the mod's own (1..64 bytes), never nothing */
    }
    unsigned char copy[FI_CS_BLOB_MAX];
    if (!memory_read(bytes, copy, length)) {
        return INFINITY_CARSYNC_E_ARGUMENT;
    }
    int32_t result = INFINITY_CARSYNC_OK;
    AcquireSRWLockExclusive(&cs_lock);
    CsMod *m = cs_registered(sync_id);
    if (!m) {
        result = INFINITY_CARSYNC_E_UNREGISTERED;
    } else if (m->length != length || !memory_equal(m->bytes, copy, length)) {
        m->length = (unsigned char)length;
        memory_copy(m->bytes, copy, length);
        m->dirty = 1;
    }
    ReleaseSRWLockExclusive(&cs_lock);
    return result;
}

int32_t InfinityCarSync_Get(const void *car, uint8_t sync_id, void *out, uint32_t capacity, uint8_t *blob_version) {
    if (!car || (!out && capacity)) {
        return INFINITY_CARSYNC_E_ARGUMENT;
    }
    unsigned kind = 0;
    if (!memory_read((const unsigned char *)car + 0x4d08, &kind, sizeof(kind)) || (kind != 2 && kind != 5)) {
        return INFINITY_CARSYNC_NONE; /* only a network car is another player's (a reused address never is) */
    }
    unsigned long long now = CS_NOW();
    int32_t result = INFINITY_CARSYNC_NONE;
    unsigned char bytes[FI_CS_BLOB_MAX], version = 0;
    AcquireSRWLockShared(&cs_lock);
    if (!cs_registered(sync_id)) {
        result = INFINITY_CARSYNC_E_UNREGISTERED;
    } else {
        for (int i = 0; i < CS_REMOTES && !result; ++i) {
            const CsRemote *r = &cs_remotes[i];
            if (!r->used || r->car != car || now - r->heard_at >= CS_FORGET_MS) {
                continue;
            }
            for (int k = 0; k < CS_MODS; ++k) {
                const CsBlob *b = &r->blobs[k];
                if (b->length && b->id == sync_id && now - b->heard_at < CS_FORGET_MS) {
                    result = b->length;
                    version = b->version;
                    memory_copy(bytes, b->bytes, b->length);
                    break;
                }
            }
            break;
        }
    }
    ReleaseSRWLockShared(&cs_lock);
    if (result > 0) {
        if ((uint32_t)result > capacity) {
            return INFINITY_CARSYNC_E_CAPACITY;
        }
        memory_copy(out, bytes, (size_t)result);
        if (blob_version) {
            *blob_version = version;
        }
    }
    return result;
}
