/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* FH2Online free roam: what ExtraOptions changes on a player's own car and the game never sends
   (its headlights forced on or off, its roof open or closed), shown on that car in the other
   players' games. Only while the player's own ExtraOptions 0.2.98 is active in this process
   (fi_eo_sync_enabled, eo_detect.c): without it nothing is sent, and what the relay passes is
   dropped. The relay forwards these indications blindly, so hooks with and without
   ExtraOptions (and older hooks) share a world unchanged.

   Each hook tells the world's relay its car's state with a private STUN indication (the relay's
   carstate.rs has the format): a joining player from its proxy's socket (azimuth_proxy.c, the
   address the relay knows it by), the host from a loopback socket of its own (its game talks to
   the relay over loopback, with no proxy). The state goes when it changes and every second (the
   relay passes the others' states only to a hook that speaks). The relay passes each other
   player's state with that player's XUID and the position of its car, taken from its last
   CarPredictionFull as it is sent.

   The game's car objects do not say whose they are, so a remote car is matched by position:
   the first car within CS_MATCH_METERS of a player's position heard less than CS_FRESH_MS ago
   is that player's, and stays so until, with a fresh position, it is CS_UNMATCH_METERS away.
   carstate_hook.c asks for each car that is not the local one what to show
   (azimuth_carstate_wanted) and shows it as ExtraOptions shows the local car's. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define CS_KIND 0x3E91u
#define CS_ATTR 0xC0F1u
#define CS_VERSION 1
#define CS_UP_LEN 16
#define CS_DOWN_LEN 28
#define CS_REMOTES 16
#define CS_RESEND_MS 1000u
#define CS_FRESH_MS 300u
#define CS_FORGET_MS 5000u
#define CS_MATCH_METERS 15.0f
#define CS_UNMATCH_METERS 30.0f

#define CS_AF_INET 2
#define CS_SOCK_DGRAM 2
#define CS_IPPROTO_UDP 17
#define CS_SOL_SOCKET 0xffff
#define CS_SO_RCVTIMEO 0x1006
#define CS_INVALID ((UINT_PTR)~(UINT_PTR)0)

typedef struct {
    int used;
    unsigned char lights, roof;
    unsigned long long xuid;
    float position[3];
    int has_position;
    unsigned long long heard_at, position_at;
    void *car; /* the car it is shown on, once matched */
} CsRemote;

static CsRemote cs_remotes[CS_REMOTES];
static SRWLOCK cs_lock = SRWLOCK_INIT;
static volatile LONG cs_remote_count;
static int cs_lights = -1, cs_roof = -1;
static unsigned long long cs_sent_at;
static unsigned short cs_seq;

/* ------------------------------------------------------------- the host's socket */

typedef struct {
    unsigned short family, port; /* port in network order */
    unsigned char address[4];
    unsigned char zero[8];
} CsAddr4;

typedef UINT_PTR(WINAPI *CsSocketFn)(int, int, int);
typedef int(WINAPI *CsSetOptionFn)(UINT_PTR, int, int, const char *, int);
typedef int(WINAPI *CsRecvFromFn)(UINT_PTR, char *, int, int, void *, int *);
typedef int(WINAPI *CsSendToFn)(UINT_PTR, const char *, int, int, const void *, int);
typedef int(WINAPI *CsCloseFn)(UINT_PTR);
typedef int(WINAPI *CsStartupFn)(WORD, void *);

static struct {
    CsSocketFn socket;
    CsSetOptionFn setsockopt;
    CsRecvFromFn recvfrom;
    CsSendToFn sendto;
    CsCloseFn closesocket;
    CsStartupFn startup;
} cs;
static volatile LONG cs_loaded;
static UINT_PTR cs_host_socket = CS_INVALID;
static unsigned cs_host_port;

static int cs_load(void) {
    if (cs_loaded) {
        return cs_loaded > 0;
    }
    HMODULE ws2 = LoadLibraryExW(L"ws2_32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (ws2) {
        cs.socket = (CsSocketFn)(uintptr_t)GetProcAddress(ws2, "socket");
        cs.setsockopt = (CsSetOptionFn)(uintptr_t)GetProcAddress(ws2, "setsockopt");
        cs.recvfrom = (CsRecvFromFn)(uintptr_t)GetProcAddress(ws2, "recvfrom");
        cs.sendto = (CsSendToFn)(uintptr_t)GetProcAddress(ws2, "sendto");
        cs.closesocket = (CsCloseFn)(uintptr_t)GetProcAddress(ws2, "closesocket");
        cs.startup = (CsStartupFn)(uintptr_t)GetProcAddress(ws2, "WSAStartup");
    }
    int ok = ws2 && cs.socket && cs.setsockopt && cs.recvfrom && cs.sendto && cs.closesocket && cs.startup;
    if (ok) {
        static unsigned char data[512]; /* WSADATA */
        ok = cs.startup(0x0202, data) == 0;
    }
    cs_loaded = ok ? 1 : -1;
    return ok;
}

static void cs_relay_address(CsAddr4 *a, unsigned port) {
    memory_copy(a, &(CsAddr4){0}, sizeof(*a));
    a->family = CS_AF_INET;
    a->port = (unsigned short)((port >> 8 & 0xff) | (port & 0xff) << 8);
    a->address[0] = 127;
    a->address[3] = 1;
}

/* The relay's answers to the host's hook (the others' states), while this PC hosts. */
static DWORD WINAPI cs_host_listen(void *argument) {
    UINT_PTR s = (UINT_PTR)argument;
    static unsigned char packet[256];
    unsigned char from[64];
    while (cs_host_socket == s) {
        int from_size = (int)sizeof(from);
        int n = cs.recvfrom(s, (char *)packet, sizeof(packet), 0, from, &from_size);
        if (n > 0) {
            azimuth_carstate_received(packet, n);
        }
        if (!azimuth_world_hosting()) {
            break;
        }
    }
    if (cs_host_socket == s) {
        cs_host_socket = CS_INVALID;
    }
    cs.closesocket(s);
    return 0;
}

/* Sends to the relay from the host's loopback socket (made the first time). */
static int cs_host_send(const unsigned char *data, int size) {
    unsigned port = azimuth_world_port();
    if (!port || !cs_load()) {
        return 0;
    }
    if (cs_host_socket == CS_INVALID || cs_host_port != port) {
        UINT_PTR s = cs.socket(CS_AF_INET, CS_SOCK_DGRAM, CS_IPPROTO_UDP);
        if (s == CS_INVALID) {
            return 0;
        }
        DWORD timeout = 250;
        cs.setsockopt(s, CS_SOL_SOCKET, CS_SO_RCVTIMEO, (const char *)&timeout, sizeof(timeout));
        CsAddr4 relay;
        cs_relay_address(&relay, port);
        /* the first send binds it (an ephemeral loopback port), then it can receive */
        if (cs.sendto(s, (const char *)data, size, 0, &relay, (int)sizeof(relay)) != size) {
            cs.closesocket(s);
            return 0;
        }
        cs_host_socket = s;
        cs_host_port = port;
        HANDLE t = CreateThread(0, 0, cs_host_listen, (void *)s, 0, 0);
        if (t) {
            CloseHandle(t);
        }
        log_append("CARSTATE: this PC hosts; headlights and roof go to the relay from a loopback socket.\r\n");
        return 1;
    }
    CsAddr4 relay;
    cs_relay_address(&relay, port);
    return cs.sendto(cs_host_socket, (const char *)data, size, 0, &relay, (int)sizeof(relay)) == size;
}

/* ------------------------------------------------------------------- local state */

static void cs_put16(unsigned char *p, unsigned v) {
    p[0] = (unsigned char)(v >> 8);
    p[1] = (unsigned char)v;
}

static void cs_put64(unsigned char *p, unsigned long long v) {
    for (int i = 0; i < 8; ++i) {
        p[i] = (unsigned char)(v >> (56 - 8 * i));
    }
}

void azimuth_carstate_local(int lights, int roof) {
    if (!fi_eo_sync_enabled()) {
        return;
    }
    unsigned long long xuid = azimuth_account_xuid_value(), now = GetTickCount64();
    if (!xuid || (lights == cs_lights && roof == cs_roof && now - cs_sent_at < CS_RESEND_MS)) {
        return;
    }
    unsigned char m[20 + 4 + CS_UP_LEN] = {0};
    cs_put16(m, CS_KIND);
    cs_put16(m + 2, 4 + CS_UP_LEN);
    m[4] = 0x21, m[5] = 0x12, m[6] = 0xa4, m[7] = 0x42;
    unsigned long long noise = now * 0x9e3779b97f4a7c15ull ^ (unsigned long long)(uintptr_t)&noise;
    for (int i = 0; i < 12; ++i) {
        m[8 + i] = (unsigned char)(noise >> (8 * (i % 8))) ^ (unsigned char)i;
    }
    cs_put16(m + 20, CS_ATTR);
    cs_put16(m + 22, CS_UP_LEN);
    unsigned char *v = m + 24;
    v[0] = CS_VERSION;
    v[1] = (unsigned char)lights;
    v[2] = (unsigned char)roof;
    cs_put16(v + 4, ++cs_seq);
    cs_put64(v + 8, xuid);
    /* the host from its loopback socket; a joiner from its proxy's socket, or, for a world on this
       network the game reached directly (no proxy), from the livery channel's own socket */
    int sent = azimuth_world_hosting() ? cs_host_send(m, (int)sizeof(m))
               : azimuth_proxy_send_world(m, (int)sizeof(m)) || azimuth_world_lan_send(m, (int)sizeof(m));
    if (sent) {
        cs_lights = lights;
        cs_roof = roof;
        cs_sent_at = now;
    }
}

/* ----------------------------------------------------------------- remote states */

static unsigned cs_get16(const unsigned char *p) {
    return (unsigned)(p[0] << 8 | p[1]);
}

static float cs_float(const unsigned char *p) {
    unsigned bits = (unsigned)p[0] << 24 | (unsigned)p[1] << 16 | (unsigned)p[2] << 8 | p[3];
    float f;
    memory_copy(&f, &bits, sizeof(f));
    return f;
}

void azimuth_carstate_received(const unsigned char *d, int n) {
    if (!fi_eo_sync_enabled() || n < 20 || (d[0] & 0xc0) || cs_get16(d) != CS_KIND || d[4] != 0x21 || d[5] != 0x12 || d[6] != 0xa4 ||
        d[7] != 0x42 || (int)cs_get16(d + 2) + 20 != n) {
        return;
    }
    const unsigned char *v = 0;
    for (int at = 20; at + 4 <= n;) {
        unsigned type = cs_get16(d + at), len = cs_get16(d + at + 2);
        if (at + 4 + (int)len > n) {
            return;
        }
        if (type == CS_ATTR && len == CS_DOWN_LEN) {
            v = d + at + 4;
            break;
        }
        at += 4 + (int)((len + 3) & ~3u);
    }
    if (!v || v[0] != CS_VERSION || v[1] > 2 || v[2] > 1) {
        return;
    }
    unsigned long long xuid = 0;
    for (int i = 0; i < 8; ++i) {
        xuid = xuid << 8 | v[8 + i];
    }
    if (!xuid || xuid == azimuth_account_xuid_value()) {
        return;
    }
    float position[3] = {cs_float(v + 16), cs_float(v + 20), cs_float(v + 24)};
    int has_position = (v[6] & 1) && float_is_finite(position[0]) && float_is_finite(position[1]) &&
                       float_is_finite(position[2]);
    unsigned long long now = GetTickCount64();
    AcquireSRWLockExclusive(&cs_lock);
    CsRemote *r = 0, *free_slot = 0, *oldest = 0;
    for (int i = 0; i < CS_REMOTES; ++i) {
        CsRemote *c = &cs_remotes[i];
        if (c->used && c->xuid == xuid) {
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
        memory_copy(r, &(CsRemote){0}, sizeof(*r));
        r->used = 1;
        r->xuid = xuid;
    }
    r->lights = v[1];
    r->roof = v[2];
    r->heard_at = now;
    if (has_position) {
        memory_copy(r->position, position, sizeof(position));
        r->has_position = 1;
        r->position_at = now;
    }
    LONG count = 0;
    for (int i = 0; i < CS_REMOTES; ++i) {
        count += cs_remotes[i].used;
    }
    InterlockedExchange(&cs_remote_count, count);
    ReleaseSRWLockExclusive(&cs_lock);
    if (fresh) {
        log_append("CARSTATE: a player's headlights and roof are heard from the relay.\r\n");
    }
}

int azimuth_carstate_any(void) {
    return cs_remote_count != 0;
}

/* A position is used only just after the relay sent one: to match a car not yet matched, or to
   check the one matched. Otherwise the car's own position is not read at all. */
int azimuth_carstate_needs_position(void *car) {
    unsigned long long now = GetTickCount64();
    int needed = 0;
    AcquireSRWLockExclusive(&cs_lock);
    int matched = 0;
    for (int i = 0; i < CS_REMOTES && !matched; ++i) {
        matched = cs_remotes[i].used && cs_remotes[i].car == car;
    }
    for (int i = 0; i < CS_REMOTES && !needed; ++i) {
        const CsRemote *r = &cs_remotes[i];
        needed = r->used && r->has_position && now - r->position_at < CS_FRESH_MS &&
                 (matched ? r->car == car : !r->car);
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

int azimuth_carstate_wanted(void *car, const float position[3], int *lights, int *roof) {
    unsigned long long now = GetTickCount64();
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
            *lights = r->lights;
            *roof = r->roof;
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
        *lights = best->lights;
        *roof = best->roof;
        found = matched = 1;
    }
    InterlockedExchange(&cs_remote_count, count);
    ReleaseSRWLockExclusive(&cs_lock);
    if (matched) {
        log_append("CARSTATE: a player's car found by its position; its headlights and roof are shown on it.\r\n");
    }
    return found;
}
