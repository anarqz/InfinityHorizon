/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* FH2Online free roam over the Internet: the joining player's side.

   The game talks NerdCore from its socket ([::]:5100) to the address the association gives it.
   For a world on another PC that address becomes this proxy's, [::1]:<port>, and the proxy's own
   dual-stack socket reaches the world:
   1. STUN (RFC 5389) to two public servers: this socket's public address and the NAT's kind.
   2. POST /infinity/v1/worlds/punch with those addresses: the server answers the world's
      addresses (its LAN on the same network, the host's address, its STUN and UPnP ones) and
      asks the host's relay to punch towards ours, so that the host's NAT lets us in.
   3. Every candidate is probed (STUN Binding requests, which the relay answers, and the game's
      own packets) until one answers; that one is the world. Packets from any other address
      never reach the game (the source filter), and only the game's packets (from loopback) go
      to the world.
   Nothing answering for PX_GIVE_UP_MS is logged; the game's packets keep going to the session's
   address, as before F3. [FreeRoam] Direct=1 turns the proxy off (the game then sends to the
   session's address itself). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define PX_AF_INET 2
#define PX_AF_INET6 23
#define PX_SOCK_DGRAM 2
#define PX_IPPROTO_UDP 17
#define PX_IPPROTO_IPV6 41
#define PX_IPV6_V6ONLY 27
#define PX_SOL_SOCKET 0xffff
#define PX_SO_RCVTIMEO 0x1006
#define PX_INVALID ((UINT_PTR)~(UINT_PTR)0)
#define PX_MAGIC 0x2112A442u
#define PX_CANDIDATES 8
#define PX_STUN_SERVERS 4
#define PX_PACKET 2048
#define PX_PROBE_MS 250u
#define PX_GIVE_UP_MS 15000u
/* Before a world answers, the game's packets go to every candidate for this long at most, then
   only to the session's address (a host naming someone else's address cannot aim the players'
   traffic at it for longer). */
#define PX_SPRAY_MS 30000u
#define PX_STUN_WAIT_MS 1500u
#define PX_KEEPALIVE_MS 15000u

typedef struct {
    unsigned short family, port; /* port in network order */
    unsigned flow;
    unsigned char address[16];
    unsigned scope;
} PxAddr;

typedef struct {
    int flags, family, socktype, protocol;
    size_t addrlen;
    wchar_t *canonname;
    void *addr;
    void *next;
} PxAddrInfo;

typedef UINT_PTR(WINAPI *PxSocketFn)(int, int, int);
typedef int(WINAPI *PxBindFn)(UINT_PTR, const void *, int);
typedef int(WINAPI *PxSetOptionFn)(UINT_PTR, int, int, const char *, int);
typedef int(WINAPI *PxRecvFromFn)(UINT_PTR, char *, int, int, void *, int *);
typedef int(WINAPI *PxSendToFn)(UINT_PTR, const char *, int, int, const void *, int);
typedef int(WINAPI *PxCloseFn)(UINT_PTR);
typedef int(WINAPI *PxNameFn)(UINT_PTR, void *, int *);
typedef int(WINAPI *PxStartupFn)(WORD, void *);
typedef int(WINAPI *PxGetAddrInfoFn)(const wchar_t *, const wchar_t *, const void *, PxAddrInfo **);
typedef void(WINAPI *PxFreeAddrInfoFn)(PxAddrInfo *);
typedef int(WINAPI *PxPtonFn)(int, const wchar_t *, void *);
typedef const wchar_t *(WINAPI *PxNtopFn)(int, const void *, wchar_t *, size_t);

static struct {
    PxSocketFn socket;
    PxBindFn bind;
    PxSetOptionFn setsockopt;
    PxRecvFromFn recvfrom;
    PxSendToFn sendto;
    PxCloseFn closesocket;
    PxNameFn getsockname;
    PxStartupFn startup;
    PxGetAddrInfoFn getaddrinfo;
    PxFreeAddrInfoFn freeaddrinfo;
    PxPtonFn pton;
    PxNtopFn ntop;
} px;

static int px_direct;
static SRWLOCK px_lock = SRWLOCK_INIT;
/* px_generation: each proxy thread stops once it changes. */
static volatile LONG px_generation, px_loaded;
static UINT_PTR px_socket = PX_INVALID;
static PxAddr px_candidates[PX_CANDIDATES];
static volatile LONG px_candidate_count;
static PxAddr px_world;
static volatile LONG px_have_world;
static PxAddr px_stun[PX_STUN_SERVERS];
static unsigned px_stun_count;
static PxAddr px_mapped[PX_STUN_SERVERS];
static unsigned char px_mapped_seen[PX_STUN_SERVERS];
static unsigned char px_txid[PX_STUN_SERVERS + 1][12]; /* the servers', then the probes' */
static unsigned short px_local_port;

/* ------------------------------------------------------------------ helpers */

typedef struct {
    char text[256];
    size_t size;
} PxLine;

static void px_text(PxLine *l, const char *t) {
    while (t && *t && l->size + 1 < sizeof(l->text)) {
        l->text[l->size++] = *t++;
    }
    l->text[l->size] = 0;
}

static void px_number(PxLine *l, unsigned long long v) {
    char d[24];
    unsigned n = 0;
    do {
        d[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v && n < sizeof(d));
    char t[24];
    for (unsigned i = 0; i < n; ++i) {
        t[i] = d[n - 1 - i];
    }
    t[n] = 0;
    px_text(l, t);
}

/* `a` as text: a.b.c.d:port or [v6]:port (out: at least 64 bytes). */
static void px_format(const PxAddr *a, char *out, size_t capacity) {
    wchar_t wide[64];
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    unsigned port = (unsigned)((a->port >> 8) | ((a->port & 0xff) << 8));
    int v4 = 1;
    for (int i = 0; i < 10; ++i) {
        v4 &= a->address[i] == 0;
    }
    v4 &= a->address[10] == 0xff && a->address[11] == 0xff;
    wide[0] = 0;
    if (v4) {
        px.ntop(PX_AF_INET, a->address + 12, wide, ARRAY_COUNT(wide));
    } else {
        px_text(&l, "[");
        px.ntop(PX_AF_INET6, a->address, wide, ARRAY_COUNT(wide));
    }
    for (size_t i = 0; wide[i] && i < ARRAY_COUNT(wide); ++i) {
        char c[2] = {(char)wide[i], 0};
        px_text(&l, c);
    }
    px_text(&l, v4 ? ":" : "]:");
    px_number(&l, port);
    size_t n = l.size < capacity - 1 ? l.size : capacity - 1;
    memory_copy(out, l.text, n);
    out[n] = 0;
}

/* "a.b.c.d:port" or "[v6]:port" (n characters) -> `out` (IPv4 as ::ffff:a.b.c.d). */
static int px_parse(const char *text, size_t n, PxAddr *out) {
    wchar_t host[64];
    size_t start = 0, end = 0, colon = 0;
    if (n < 4 || n >= ARRAY_COUNT(host)) {
        return 0;
    }
    if (text[0] == '[') {
        start = 1;
        for (end = 1; end < n && text[end] != ']'; ++end) {
        }
        colon = end + 1;
    } else {
        for (end = 0; end < n && text[end] != ':'; ++end) {
        }
        colon = end;
    }
    if (colon >= n || text[colon] != ':') {
        return 0;
    }
    unsigned port = 0;
    for (size_t i = colon + 1; i < n; ++i) {
        if (text[i] < '0' || text[i] > '9' || port > 65535) {
            return 0;
        }
        port = port * 10 + (unsigned)(text[i] - '0');
    }
    if (!port || port > 65535) {
        return 0;
    }
    for (size_t i = start; i < end; ++i) {
        host[i - start] = (wchar_t)(unsigned char)text[i];
    }
    host[end - start] = 0;
    PxAddr a;
    a.family = PX_AF_INET6;
    a.port = (unsigned short)(((port & 0xff) << 8) | (port >> 8));
    a.flow = 0;
    a.scope = 0;
    for (int i = 0; i < 16; ++i) {
        ((volatile unsigned char *)a.address)[i] = 0;
    }
    unsigned char v4[4];
    if (text[0] != '[' && px.pton(PX_AF_INET, host, v4) == 1) {
        a.address[10] = a.address[11] = 0xff;
        memory_copy(a.address + 12, v4, 4);
    } else if (text[0] != '[' || px.pton(PX_AF_INET6, host, a.address) != 1) {
        return 0;
    }
    *out = a;
    return 1;
}

/* A sockaddr from recvfrom (IPv6, or IPv4 on a dual-stack socket's IPv4 path) as PxAddr. */
static void px_from(const unsigned char *raw, PxAddr *out) {
    memory_copy(out, raw, sizeof(*out));
    if (out->family == PX_AF_INET) {
        unsigned char v4[4];
        memory_copy(v4, raw + 4, 4);
        out->family = PX_AF_INET6;
        out->flow = 0;
        out->scope = 0;
        for (int i = 0; i < 16; ++i) {
            ((volatile unsigned char *)out->address)[i] = 0;
        }
        out->address[10] = out->address[11] = 0xff;
        memory_copy(out->address + 12, v4, 4);
    }
    out->flow = 0;
    out->scope = 0;
}

static int px_same(const PxAddr *a, const PxAddr *b) {
    return a->port == b->port && memory_equal(a->address, b->address, 16);
}

static int px_loopback(const PxAddr *a) {
    static const unsigned char v6[16] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1};
    return memory_equal(a->address, v6, 16) ||
           (a->address[10] == 0xff && a->address[11] == 0xff && a->address[12] == 127);
}

static void px_new_txid(unsigned char *t) {
    static volatile LONG counter;
    unsigned long long x = GetTickCount64() * 0x9e3779b97f4a7c15ull ^
                           ((unsigned long long)InterlockedIncrement(&counter) << 40) ^ (unsigned long long)(uintptr_t)t ^
                           (unsigned long long)(uintptr_t)&x;
    for (int i = 0; i < 12; ++i) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        t[i] = (unsigned char)(x >> 24);
    }
}

static void px_send(const void *data, int size, const PxAddr *to) {
    if (!fi_launch_network_allowed()) return;
    px.sendto(px_socket, (const char *)data, size, 0, to, (int)sizeof(*to));
}

static void px_binding_request(const unsigned char *txid, const PxAddr *to) {
    unsigned char m[20];
    m[0] = 0, m[1] = 1, m[2] = 0, m[3] = 0;
    m[4] = 0x21, m[5] = 0x12, m[6] = 0xa4, m[7] = 0x42;
    memory_copy(m + 8, txid, 12);
    px_send(m, 20, to);
}

static void px_binding_success(const unsigned char *txid, const PxAddr *seen) {
    unsigned char m[44];
    int v4 = seen->address[10] == 0xff && seen->address[11] == 0xff;
    int value = v4 ? 8 : 20;
    m[0] = 1, m[1] = 1, m[2] = 0, m[3] = (unsigned char)(4 + value);
    m[4] = 0x21, m[5] = 0x12, m[6] = 0xa4, m[7] = 0x42;
    memory_copy(m + 8, txid, 12);
    m[20] = 0, m[21] = 0x20, m[22] = 0, m[23] = (unsigned char)value;
    m[24] = 0, m[25] = v4 ? 1 : 2;
    unsigned char *port = (unsigned char *)&seen->port; /* network order */
    m[26] = (unsigned char)(port[0] ^ 0x21), m[27] = (unsigned char)(port[1] ^ 0x12);
    if (v4) {
        for (int i = 0; i < 4; ++i) m[28 + i] = (unsigned char)(seen->address[12 + i] ^ m[4 + i]);
    } else {
        for (int i = 0; i < 16; ++i) m[28 + i] = (unsigned char)(seen->address[i] ^ m[4 + i]);
    }
    px_send(m, 24 + value, seen);
}

/* A STUN message (RFC 5389): kind, txid and its (XOR-)MAPPED-ADDRESS. 0: not STUN. */
static int px_stun_parse(const unsigned char *d, int n, unsigned *kind, const unsigned char **txid, PxAddr *mapped,
                         int *has_mapped) {
    if (n < 20 || (d[0] & 0xc0) || d[4] != 0x21 || d[5] != 0x12 || d[6] != 0xa4 || d[7] != 0x42 ||
        ((d[2] << 8) | d[3]) + 20 != n || (n & 3)) {
        return 0;
    }
    *kind = (unsigned)((d[0] << 8) | d[1]);
    *txid = d + 8;
    *has_mapped = 0;
    for (int at = 20; at + 4 <= n;) {
        unsigned type = (unsigned)((d[at] << 8) | d[at + 1]);
        int len = (d[at + 2] << 8) | d[at + 3];
        if (at + 4 + len > n) {
            break;
        }
        const unsigned char *v = d + at + 4;
        int x = type == 0x0020 || type == 0x8020;
        if ((x || (type == 0x0001 && !*has_mapped)) && len >= 8) {
            PxAddr a;
            a.family = PX_AF_INET6;
            a.flow = 0;
            a.scope = 0;
            for (int i = 0; i < 16; ++i) ((volatile unsigned char *)a.address)[i] = 0;
            unsigned char p0 = v[2], p1 = v[3];
            if (x) p0 ^= 0x21, p1 ^= 0x12;
            ((unsigned char *)&a.port)[0] = p0;
            ((unsigned char *)&a.port)[1] = p1;
            if (v[1] == 1) {
                a.address[10] = a.address[11] = 0xff;
                for (int i = 0; i < 4; ++i) a.address[12 + i] = (unsigned char)(v[4 + i] ^ (x ? d[4 + i] : 0));
                *mapped = a, *has_mapped = 1;
            } else if (v[1] == 2 && len >= 20) {
                for (int i = 0; i < 16; ++i) a.address[i] = (unsigned char)(v[4 + i] ^ (x ? d[4 + i] : 0));
                *mapped = a, *has_mapped = 1;
            }
        }
        at += 4 + ((len + 3) & ~3);
    }
    return 1;
}

/* --------------------------------------------------------------- candidates */

static void px_add_candidate(const PxAddr *a) {
    AcquireSRWLockExclusive(&px_lock);
    int known = 0;
    for (LONG i = 0; i < px_candidate_count; ++i) {
        known |= px_same(&px_candidates[i], a);
    }
    if (!known && px_candidate_count < PX_CANDIDATES) {
        px_candidates[px_candidate_count] = *a;
        InterlockedIncrement(&px_candidate_count);
    }
    ReleaseSRWLockExclusive(&px_lock);
}

static int px_is_candidate(const PxAddr *a) {
    int found = 0;
    AcquireSRWLockExclusive(&px_lock);
    for (LONG i = 0; i < px_candidate_count; ++i) {
        found |= px_same(&px_candidates[i], a);
    }
    ReleaseSRWLockExclusive(&px_lock);
    return found;
}

/* The server's {"world":["ip:port",...]} -> candidates. */
static void px_take_world(const char *reply) {
    const char *at = text_find(reply, text_length(reply), "\"world\":[");
    if (!at) {
        return;
    }
    for (at += 9; *at && *at != ']';) {
        if (*at != '"') {
            ++at;
            continue;
        }
        const char *start = ++at;
        while (*at && *at != '"') ++at;
        PxAddr a;
        if (px_parse(start, (size_t)(at - start), &a)) {
            px_add_candidate(&a);
        }
        if (*at == '"') ++at;
    }
}

/* POST worlds/punch with this socket's public addresses (a thread: the proxy keeps relaying). */
static DWORD WINAPI px_signal(void *argument) {
    LONG generation = (LONG)(intptr_t)argument;
    if (!fi_launch_network_allowed() || px_generation != generation) return 0;
    static char body[512], reply[2048];
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    px_text(&l, "{\"candidates\":[");
    unsigned listed = 0;
    for (unsigned i = 0; i < px_stun_count && listed < 3; ++i) {
        if (!px_mapped_seen[i]) continue;
        int dup = 0;
        for (unsigned j = 0; j < i; ++j) dup |= px_mapped_seen[j] && px_same(&px_mapped[j], &px_mapped[i]);
        if (dup) continue;
        char a[72];
        px_format(&px_mapped[i], a, sizeof(a));
        px_text(&l, listed++ ? ",\"" : "\"");
        px_text(&l, a);
        px_text(&l, "\"");
    }
    px_text(&l, "],\"port\":");
    px_number(&l, px_local_port);
    px_text(&l, "}");
    memory_copy(body, l.text, l.size + 1);
    static wchar_t header[1200];
    azimuth_account_auth_header(header, ARRAY_COUNT(header));
    if (!fi_launch_network_allowed() || px_generation != generation) return 0;
    DWORD status = azimuth_net_call(L"POST", L"/infinity/v1/worlds/punch", header, body, reply, sizeof(reply));
    if (px_generation != generation || !fi_launch_network_allowed()) {
        return 0;
    }
    PxLine m;
    m.size = 0;
    m.text[0] = 0;
    px_text(&m, "PROXY: punch exchange answered ");
    px_number(&m, status);
    px_text(&m, status == 200 ? "; probing the world's addresses.\r\n" : "; probing the session's address only.\r\n");
    log_append(m.text);
    if (status == 200) {
        px_take_world(reply);
    }
    return 0;
}

/* ----------------------------------------------------------------- the loop */

static void px_log_world(const PxAddr *a, const char *how, DWORD after) {
    char text[72];
    px_format(a, text, sizeof(text));
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    px_text(&l, "PROXY: the world answered at ");
    px_text(&l, text);
    px_text(&l, " (");
    px_text(&l, how);
    px_text(&l, ", ");
    px_number(&l, after);
    px_text(&l, " ms).\r\n");
    log_append(l.text);
}

static void px_pick(const PxAddr *a, const char *how, DWORD started) {
    if (InterlockedCompareExchange(&px_have_world, 1, 0) == 0) {
        px_world = *a;
        px_log_world(a, how, (DWORD)GetTickCount64() - started);
    }
}

static DWORD WINAPI px_thread(void *argument) {
    LONG generation = (LONG)(intptr_t)argument;
    /* this thread's own socket: a restart (azimuth_proxy_stop, then a new one) may set
       px_socket to the next one before this thread is done, and closing that one too would
       close it twice (on Wine a socket's number can be a file of the game's by then) */
    UINT_PTR own = px_socket;
    static unsigned char packet[PX_PACKET], from_raw[64];
    PxAddr game, from;
    memory_copy(&game, &px_candidates[0], sizeof(game));
    int have_game = 0, signalled = 0, gave_up = 0;
    DWORD started = (DWORD)GetTickCount64(), last_probe = 0, last_keepalive = (DWORD)GetTickCount64();
    for (unsigned i = 0; i < px_stun_count; ++i) {
        px_new_txid(px_txid[i]);
        px_binding_request(px_txid[i], &px_stun[i]);
    }
    px_new_txid(px_txid[PX_STUN_SERVERS]);
    while (px_generation == generation && fi_launch_network_allowed()) {
        DWORD now = (DWORD)GetTickCount64();
        /* the punch exchange once STUN answered (or gave up waiting) */
        if (!signalled) {
            unsigned answered = 0;
            for (unsigned i = 0; i < px_stun_count; ++i) answered += px_mapped_seen[i];
            if (answered == px_stun_count || now - started >= PX_STUN_WAIT_MS) {
                signalled = 1;
                HANDLE t = CreateThread(0, 0, px_signal, (void *)(intptr_t)generation, 0, 0);
                if (t) CloseHandle(t);
            }
        }
        if (!px_have_world && now - last_probe >= PX_PROBE_MS && now - started < PX_SPRAY_MS) {
            last_probe = now;
            for (LONG i = 0; i < px_candidate_count && i < PX_CANDIDATES; ++i) {
                px_binding_request(px_txid[PX_STUN_SERVERS], &px_candidates[i]);
            }
            if (!gave_up && now - started >= PX_GIVE_UP_MS) {
                gave_up = 1;
                log_append("PROXY: no answer from the world's addresses yet; the host's network may block it "
                           "(INFINITY > TEST CONNECTION). Still trying.\r\n");
            }
        }
        if (px_have_world && now - last_keepalive >= PX_KEEPALIVE_MS) {
            last_keepalive = now;
            px_binding_request(px_txid[PX_STUN_SERVERS], &px_world);
        }
        int from_size = (int)sizeof(from_raw);
        int n = px.recvfrom(own, (char *)packet, sizeof(packet), 0, from_raw, &from_size);
        if (px_generation != generation || !fi_launch_network_allowed()) break;
        if (n <= 0) {
            continue;
        }
        px_from(from_raw, &from);
        unsigned kind = 0;
        const unsigned char *txid = 0;
        PxAddr mapped;
        int has_mapped = 0;
        if (px_stun_parse(packet, n, &kind, &txid, &mapped, &has_mapped)) {
            if (kind == 0x3E91) {
                /* the relay passing another player's headlights and roof (azimuth_carstate.c) */
                if (px_have_world && px_same(&from, &px_world)) {
                    azimuth_carstate_received(packet, n);
                }
            } else if (kind == 0x3E92) {
                /* the relay's livery channel (livery_world.c, OD-66): world mates' paints */
                if (px_have_world && px_same(&from, &px_world)) {
                    azimuth_livery_received(packet, n);
                }
            } else if (kind == 0x0101) {
                for (unsigned i = 0; i < px_stun_count; ++i) {
                    if (memory_equal(txid, px_txid[i], 12) && px_same(&from, &px_stun[i]) && has_mapped) {
                        px_mapped[i] = mapped;
                        px_mapped_seen[i] = 1;
                    }
                }
                if (memory_equal(txid, px_txid[PX_STUN_SERVERS], 12) && px_is_candidate(&from)) {
                    px_pick(&from, "STUN", started);
                }
            } else if (kind == 0x0001 && px_is_candidate(&from)) {
                /* the host's relay punching towards us */
                px_binding_success(txid, &from);
            }
            continue;
        }
        if (px_loopback(&from)) {
            /* the game (the first loopback sender is it: nobody else knows this port) */
            if (!have_game) {
                game = from;
                have_game = 1;
            } else if (!px_same(&from, &game)) {
                continue;
            }
            if (px_have_world) {
                px_send(packet, n, &px_world);
            } else {
                LONG targets = now - started < PX_SPRAY_MS ? px_candidate_count : 1;
                for (LONG i = 0; i < targets && i < PX_CANDIDATES; ++i) {
                    px_send(packet, n, &px_candidates[i]);
                }
            }
            continue;
        }
        /* from the Internet: only the world reaches the game */
        if (!px_have_world && px_is_candidate(&from)) {
            px_pick(&from, "NerdCore", started);
        }
        if (px_have_world && px_same(&from, &px_world) && have_game) {
            px_send(packet, n, &game);
        }
    }
    InterlockedCompareExchangePointer((void *volatile *)&px_socket, (void *)PX_INVALID, (void *)own);
    px.closesocket(own);
    return 0;
}

int azimuth_proxy_send_world(const void *data, int size) {
    UINT_PTR s = px_socket;
    if (!fi_launch_network_allowed() || s == PX_INVALID || !px_have_world || px_direct) {
        return 0;
    }
    PxAddr world = px_world;
    return px.sendto(s, (const char *)data, size, 0, &world, (int)sizeof(world)) == size;
}

/* --------------------------------------------------------------------- setup */

static int px_load(void) {
    if (px_loaded) {
        return px_loaded > 0;
    }
    HMODULE ws2 = LoadLibraryExW(L"ws2_32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (ws2) {
        px.socket = (PxSocketFn)(uintptr_t)GetProcAddress(ws2, "socket");
        px.bind = (PxBindFn)(uintptr_t)GetProcAddress(ws2, "bind");
        px.setsockopt = (PxSetOptionFn)(uintptr_t)GetProcAddress(ws2, "setsockopt");
        px.recvfrom = (PxRecvFromFn)(uintptr_t)GetProcAddress(ws2, "recvfrom");
        px.sendto = (PxSendToFn)(uintptr_t)GetProcAddress(ws2, "sendto");
        px.closesocket = (PxCloseFn)(uintptr_t)GetProcAddress(ws2, "closesocket");
        px.getsockname = (PxNameFn)(uintptr_t)GetProcAddress(ws2, "getsockname");
        px.startup = (PxStartupFn)(uintptr_t)GetProcAddress(ws2, "WSAStartup");
        px.getaddrinfo = (PxGetAddrInfoFn)(uintptr_t)GetProcAddress(ws2, "GetAddrInfoW");
        px.freeaddrinfo = (PxFreeAddrInfoFn)(uintptr_t)GetProcAddress(ws2, "FreeAddrInfoW");
        px.pton = (PxPtonFn)(uintptr_t)GetProcAddress(ws2, "InetPtonW");
        px.ntop = (PxNtopFn)(uintptr_t)GetProcAddress(ws2, "InetNtopW");
    }
    int ok = ws2 && px.socket && px.bind && px.setsockopt && px.recvfrom && px.sendto && px.closesocket &&
             px.getsockname && px.startup && px.getaddrinfo && px.freeaddrinfo && px.pton && px.ntop;
    if (ok) {
        static unsigned char data[512]; /* WSADATA */
        ok = px.startup(0x0202, data) == 0;
    }
    px_loaded = ok ? 1 : -1;
    return ok;
}

/* The public STUN servers (resolved once; IPv4 and IPv6 of each). */
static void px_resolve_stun(void) {
    static const wchar_t *const names[2][2] = {{L"stun.l.google.com", L"19302"}, {L"stun.cloudflare.com", L"3478"}};
    if (px_stun_count) {
        return;
    }
    for (int k = 0; k < 2; ++k) {
        PxAddrInfo hints, *list = 0;
        unsigned char *h = (unsigned char *)&hints;
        for (size_t i = 0; i < sizeof(hints); ++i) h[i] = 0;
        hints.socktype = PX_SOCK_DGRAM;
        hints.protocol = PX_IPPROTO_UDP;
        if (px.getaddrinfo(names[k][0], names[k][1], &hints, &list) != 0) {
            continue;
        }
        int have4 = 0, have6 = 0;
        for (PxAddrInfo *i = list; i && px_stun_count < PX_STUN_SERVERS; i = (PxAddrInfo *)i->next) {
            if ((i->family == PX_AF_INET && !have4) || (i->family == PX_AF_INET6 && !have6)) {
                px_from((const unsigned char *)i->addr, &px_stun[px_stun_count++]);
                have4 |= i->family == PX_AF_INET;
                have6 |= i->family == PX_AF_INET6;
            }
        }
        px.freeaddrinfo(list);
    }
}

/* A world on this network: an IPv4-mapped RFC 1918 address (10/8, 172.16/12, 192.168/16). The
   server hands a joiner the host's LAN address when both share a public address
   (MATCHMAKING_API.md 4a): no NAT between them, so the game goes there directly. */
static int px_private_v4(const PxAddr *a) {
    const unsigned char *b = a->address;
    for (int i = 0; i < 10; ++i) {
        if (b[i]) {
            return 0;
        }
    }
    return b[10] == 0xff && b[11] == 0xff &&
           (b[12] == 10 || (b[12] == 172 && b[13] >= 16 && b[13] <= 31) || (b[12] == 192 && b[13] == 168));
}

void azimuth_proxy_configure(int direct) {
    px_direct = direct;
}

/* Called with the world the session named (CreateAssociationAsync). For a world on another PC
   it starts the proxy and returns 1 with `local` = the address the game should use. */
int azimuth_proxy_start(const void *world, void *local) {
    if (!fi_launch_network_allowed()) return 0;
    PxAddr target, here;
    memory_copy(&target, world, sizeof(target));
    if (px_direct && fi_tier_free()) {
        /* [hook] FreeRoamDirect is a diagnostic for Supporters: a free profile always goes
           through the proxy, to the world the server named */
        px_direct = 0;
        log_append("PROXY: free profile: [hook] FreeRoamDirect ignored; the world is reached through the proxy.\r\n");
    }
    if (px_direct || px_loopback(&target) || !px_load()) {
        return 0;
    }
    if (fi_launch_protocol() && px_private_v4(&target)) {
        /* OD-54, local network first: the launcher's host relay on this LAN, reached directly */
        log_append("PROXY: the world is on this network; the game goes to it directly.\r\n");
        return 0;
    }
    azimuth_proxy_stop();
    px_resolve_stun();
    if (!fi_launch_network_allowed()) return 0;
    UINT_PTR s = px.socket(PX_AF_INET6, PX_SOCK_DGRAM, PX_IPPROTO_UDP);
    if (s == PX_INVALID) {
        log_append("PROXY: no socket; the game goes to the world directly.\r\n");
        return 0;
    }
    DWORD off = 0, timeout = 50;
    px.setsockopt(s, PX_IPPROTO_IPV6, PX_IPV6_V6ONLY, (const char *)&off, sizeof(off));
    px.setsockopt(s, PX_SOL_SOCKET, PX_SO_RCVTIMEO, (const char *)&timeout, sizeof(timeout));
    PxAddr any;
    unsigned char *z = (unsigned char *)&any;
    for (size_t i = 0; i < sizeof(any); ++i) z[i] = 0;
    any.family = PX_AF_INET6;
    int size = (int)sizeof(here);
    if (px.bind(s, &any, (int)sizeof(any)) != 0 || px.getsockname(s, &here, &size) != 0) {
        px.closesocket(s);
        log_append("PROXY: could not open its port; the game goes to the world directly.\r\n");
        return 0;
    }
    px_local_port = (unsigned short)((here.port >> 8) | ((here.port & 0xff) << 8));
    px_socket = s;
    px_candidate_count = 0;
    px_have_world = 0;
    for (unsigned i = 0; i < PX_STUN_SERVERS; ++i) px_mapped_seen[i] = 0;
    px_add_candidate(&target);
    if (!fi_launch_network_allowed()) {
        px_socket = PX_INVALID;
        px.closesocket(s);
        return 0;
    }
    LONG generation = InterlockedIncrement(&px_generation);
    HANDLE t = CreateThread(0, 0, px_thread, (void *)(intptr_t)generation, 0, 0);
    if (!t) {
        px.closesocket(s);
        px_socket = PX_INVALID;
        return 0;
    }
    CloseHandle(t);
    /* the game sends to [::1]:<our port> */
    PxAddr out;
    for (size_t i = 0; i < sizeof(out); ++i) ((unsigned char *)&out)[i] = 0;
    out.family = PX_AF_INET6;
    out.port = here.port;
    out.address[15] = 1;
    memory_copy(local, &out, sizeof(out));
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    px_text(&l, "PROXY: on, port ");
    px_number(&l, px_local_port);
    px_text(&l, "; ");
    px_number(&l, px_stun_count);
    px_text(&l, " STUN server address(es).\r\n");
    log_append(l.text);
    return 1;
}

/* The association ended: the proxy's thread closes its socket and ends. */
void azimuth_proxy_stop(void) {
    if (px_socket != PX_INVALID) {
        InterlockedIncrement(&px_generation);
        for (int i = 0; i < 40 && px_socket != PX_INVALID; ++i) {
            Sleep(10);
        }
    }
}

/* ------------------------------------------------------- TEST CONNECTION */

static void px_report(wchar_t *out, size_t cap, const wchar_t *text) {
    size_t n = 0;
    while (out[n] && n + 1 < cap) ++n;
    text = azimuth_tr(text);
    while (text && *text && n + 1 < cap) out[n++] = *text++;
    out[n] = 0;
}

static void px_report_raw(wchar_t *out, size_t cap, const char *text) {
    size_t n = 0;
    while (out[n] && n + 1 < cap) ++n;
    while (text && *text && n + 1 < cap) out[n++] = (wchar_t)(unsigned char)*text++;
    out[n] = 0;
}

/* The line of dedicated_server.net that starts with `key ` ("" when there is none). */
static int px_net_line(const char *text, const char *key, char *out, size_t cap) {
    size_t k = text_length(key);
    for (const char *line = text; *line;) {
        const char *end = line;
        while (*end && *end != '\n' && *end != '\r') ++end;
        if ((size_t)(end - line) > k && memory_equal(line, key, k) && line[k] == ' ') {
            size_t n = (size_t)(end - line) - k - 1;
            n = n < cap - 1 ? n : cap - 1;
            memory_copy(out, line + k + 1, n);
            out[n] = 0;
            return 1;
        }
        line = *end ? end + 1 : end;
    }
    out[0] = 0;
    return 0;
}

/* Settings > Infinity > TEST CONNECTION, free roam part: STUN from a socket of its own (this
   network's public address, IPv6, the NAT's kind) and, while hosting, what the relay found
   (dedicated_server.net). Takes about 1.5 s. */
void azimuth_proxy_nat_test(wchar_t *out, size_t cap) {
    if (!fi_launch_network_allowed()) return;
    px_report(out, cap, L"\n\nFREE ROAM (worlds over the Internet)");
    if (!px_load()) {
        return;
    }
    px_resolve_stun();
    UINT_PTR s = px.socket(PX_AF_INET6, PX_SOCK_DGRAM, PX_IPPROTO_UDP);
    PxAddr seen[PX_STUN_SERVERS];
    int got[PX_STUN_SERVERS] = {0};
    static unsigned char tx[PX_STUN_SERVERS][12], packet[PX_PACKET], from_raw[64];
    if (s != PX_INVALID) {
        DWORD off = 0, timeout = 100;
        px.setsockopt(s, PX_IPPROTO_IPV6, PX_IPV6_V6ONLY, (const char *)&off, sizeof(off));
        px.setsockopt(s, PX_SOL_SOCKET, PX_SO_RCVTIMEO, (const char *)&timeout, sizeof(timeout));
        PxAddr any;
        for (size_t i = 0; i < sizeof(any); ++i) ((unsigned char *)&any)[i] = 0;
        any.family = PX_AF_INET6;
        if (px.bind(s, &any, (int)sizeof(any)) == 0) {
            for (unsigned i = 0; i < px_stun_count; ++i) {
                if (!fi_launch_network_allowed()) break;
                unsigned char m[20] = {0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42};
                px_new_txid(tx[i]);
                memory_copy(m + 8, tx[i], 12);
                px.sendto(s, (const char *)m, 20, 0, &px_stun[i], (int)sizeof(px_stun[i]));
            }
            DWORD started = (DWORD)GetTickCount64();
            unsigned answered = 0;
            while (fi_launch_network_allowed() && answered < px_stun_count &&
                   (DWORD)GetTickCount64() - started < PX_STUN_WAIT_MS) {
                int from_size = (int)sizeof(from_raw);
                int n = px.recvfrom(s, (char *)packet, sizeof(packet), 0, from_raw, &from_size);
                if (n <= 0) continue;
                PxAddr from, mapped;
                px_from(from_raw, &from);
                unsigned kind = 0;
                const unsigned char *txid = 0;
                int has = 0;
                if (!px_stun_parse(packet, n, &kind, &txid, &mapped, &has) || kind != 0x0101 || !has) continue;
                for (unsigned i = 0; i < px_stun_count; ++i) {
                    if (!got[i] && memory_equal(txid, tx[i], 12) && px_same(&from, &px_stun[i])) {
                        got[i] = 1;
                        seen[i] = mapped;
                        ++answered;
                    }
                }
            }
        }
        px.closesocket(s);
    }
    /* IPv4 answers decide the NAT; any IPv6 answer means IPv6 works */
    int v4 = 0, v6 = 0, same = 1;
    PxAddr first4;
    for (unsigned i = 0; i < px_stun_count; ++i) {
        int server_v4 = px_stun[i].address[10] == 0xff && px_stun[i].address[11] == 0xff;
        if (!got[i]) continue;
        if (server_v4) {
            if (!v4) first4 = seen[i];
            else same &= px_same(&first4, &seen[i]);
            ++v4;
        } else {
            ++v6;
        }
    }
    if (!v4 && !v6) {
        px_report(out, cap, L"\nNo answer from the STUN servers: this network seems to block UDP. Playing with players outside "
                            L"your network will not work until it allows it (a firewall, or a school or work network).");
    } else {
        if (v4) {
            char text[72];
            px_format(&first4, text, sizeof(text));
            /* the port is this test's own: only the address means something to the player */
            for (size_t i = 0; text[i]; ++i) {
                if (text[i] == ':') {
                    text[i] = 0;
                    break;
                }
            }
            px_report(out, cap, L"\nYour address on the Internet: ");
            px_report_raw(out, cap, text);
        }
        px_report(out, cap, v6 ? L"\nIPv6: yes." : L"\nIPv6: no.");
        if (v4 >= 2 && same) {
            px_report(out, cap, L"\nNAT: moderate (cone). You can join most players' worlds and most can join yours, with no "
                                L"port to open.");
        } else if (v4 >= 2) {
            px_report(out, cap, L"\nNAT: strict (symmetric). You can join worlds whose host has a moderate or open NAT, but two "
                                L"strict NATs cannot meet. Fixes: turn UPnP on in the router, ask your operator for a public IP "
                                L"(CGNAT), or forward UDP 5101 to this PC when hosting.");
        } else {
            px_report(out, cap, L"\nNAT: not measured (only one STUN server answered).");
        }
    }
    /* the world this PC hosts */
    static char net[1024], value[160];
    if (azimuth_world_net_text(net, sizeof(net))) {
        px_report(out, cap, L"\n\nYOUR WORLD");
        if (px_net_line(net, "public", value, sizeof(value))) {
            px_report(out, cap, L"\nPlayers reach it at ");
            px_report_raw(out, cap, value);
            px_report(out, cap, L".");
        }
        px_net_line(net, "upnp", value, sizeof(value));
        if (!value[0] || memory_equal(value, "pending", 7)) {
            px_report(out, cap, L"\nUPnP: still asking the router.");
        } else if (memory_equal(value, "none", 4) || memory_equal(value, "off", 3)) {
            px_report(out, cap, L"\nUPnP: not available on this router (players still get in by hole punching).");
        } else if (memory_equal(value, "failed", 6)) {
            px_report(out, cap, L"\nUPnP: the router refused (");
            px_report_raw(out, cap, value + (text_length(value) > 7 ? 7 : text_length(value)));
            px_report(out, cap, L"); players still get in by hole punching.");
        } else {
            px_report(out, cap, L"\nUPnP: the router forwarded the world's port.");
        }
        if (px_net_line(net, "othernat", value, sizeof(value))) {
            px_report(out, cap, L"\nYour router is behind another router or the operator's CGNAT: players outside may not get "
                                L"in. Put the operator's modem in bridge mode, or ask for a public IP.");
        }
    }
    px_report(out, cap, L"\nGuide: github.com/anarqz/ForzaInfinity/blob/main/docs/FREEROAM.md");
}
