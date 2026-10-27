/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   World mates' liveries (OD-66, MATCHMAKING_API.md 5a, register CR-L26). In a launcher session
   the paints of the players in one world travel through that world's relay, in memory, and the
   hook answers the game's own content-versioning calls for them; no service and no database.

   - The game's repaint (and, later, the hook's own run of the game's car save before a world)
     calls OnlineProfile.UpdateLivery {name, carId, numberOfLayers, isLocked, creatorXuid, data}
     and then ContentVersioning.CreateVersionedLivery {name, carId, LiveryThumbnail}. The first is
     read here (and still goes to the server, a Supporter Hardcore's permanent paint, CR-S25); the
     second is answered here with the paint's content-addressed id
     0x4000... | (SHA-256(le32(carId) || data)[0..8] & 0x3FFF...), so one id never names two
     designs and the game's MultiplayerLivery_<id> cache is never stale. The game keeps the id on
     the car and replicates it; the paint goes to the relay (PUT until STORED).
   - A world mate's game fetches a paint by that id (NetworkLiveryManager ->
     RetrieveVersionedLiveryById, SAURON's RE): an id in the 01 space is answered here from the
     paints the relay delivered (WANT on entering the world, then DATA, each ACKed), waiting up
     to 5 s for one still on its way; an unknown one is the server's "no such livery" (the car
     shows its stock paint). Any other id goes to the server.
   - The relay is reached from a socket of the hook's own when the world is on this PC or this
     network (the relay binds a private peer's other ports, 5a), else through the joining
     player's proxy socket (proxy.c hands the relay's answers here).

   Nothing here logs the paint's bytes or a player's XUID. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/sha256.h"
#include "../core/support.h"

#define LW_KIND 0x3E92u
#define LW_ATTR 0xC0F2u
#define LW_MAGIC 0x2112A442u
#define LW_VERSION 1
#define LW_OP_PUT 1
#define LW_OP_STORED 2
#define LW_OP_WANT 3
#define LW_OP_DATA 4
#define LW_OP_ACK 5
#define LW_OP_DONE 6
#define LW_OP_GONE 7
#define LW_CHUNK 480u
#define LW_MAX_LIVERY (512u * 1024u)
#define LW_CHUNK_HEAD 32u /* seq, index, count, total, car, who, layers, creator */
#define LW_PEERS 16
#define LW_PUT_EVERY_MS 500u
#define LW_PUT_TRIES 10u
#define LW_WANT_EVERY_MS 1100u
#define LW_WANT_TRIES 6u
#define LW_RETRIEVE_WAIT_MS 5000u
#define LW_NAME_MAX 256u

typedef struct {
    int used, complete;
    unsigned long long owner, id, creator;
    unsigned short seq, count, got, layers;
    unsigned car, total;
    unsigned char *bytes, *have; /* have: one byte per chunk while assembling */
    int refused; /* a conflicting sequence cannot be restarted until a newer sequence */
} LwPaint;

static SRWLOCK lw_lock = SRWLOCK_INIT;
static LwPaint lw_peers[LW_PEERS];
/* A replacement is assembled separately, so an incomplete/refused transfer never destroys
   the last complete paint. Both arrays are bounded to LW_PEERS * LW_MAX_LIVERY bytes each. */
static LwPaint lw_incoming[LW_PEERS];

/* Policy v1 permits no remote serialized paint. Only the authenticated launcher's local
   control channel may select a supported mode; no packet can enable a game parser here. */
static int lw_remote_stock_only(void) { return fi_launch_protocol(); }

/* the player's own paint (kept across worlds in a session) */
static struct {
    int have, stored;
    unsigned long long id, creator;
    unsigned short seq, layers;
    unsigned car, size, tries;
    unsigned char *bytes;
    unsigned long long sent_at;
} lw_own;

/* the last UpdateLivery, until its CreateVersionedLivery */
static struct {
    int have;
    unsigned char name[LW_NAME_MAX];
    unsigned name_size, car, size;
    unsigned short layers;
    unsigned long long creator;
    unsigned char *bytes;
} lw_update;

/* ------------------------------------------------------------------- small helpers */

static void *lw_alloc(size_t size) { return size ? HeapAlloc(GetProcessHeap(), 0, size) : 0; }
static void lw_free(void *p) {
    if (p) HeapFree(GetProcessHeap(), 0, p);
}

static unsigned lw_be16(const unsigned char *p) { return (unsigned)p[0] << 8 | p[1]; }
static unsigned lw_be32(const unsigned char *p) {
    return (unsigned)p[0] << 24 | (unsigned)p[1] << 16 | (unsigned)p[2] << 8 | p[3];
}
static unsigned long long lw_be64(const unsigned char *p) {
    return (unsigned long long)lw_be32(p) << 32 | lw_be32(p + 4);
}
static void lw_put16(unsigned char *p, unsigned v) {
    p[0] = (unsigned char)(v >> 8);
    p[1] = (unsigned char)v;
}
static void lw_put32(unsigned char *p, unsigned v) {
    lw_put16(p, v >> 16);
    lw_put16(p + 2, v & 0xffffu);
}
static void lw_put64(unsigned char *p, unsigned long long v) {
    lw_put32(p, (unsigned)(v >> 32));
    lw_put32(p + 4, (unsigned)v);
}

static void lw_log_number(const char *head, unsigned long long value, const char *tail) {
    char line[160], digits[24];
    size_t n = 0, d = 0;
    for (const char *p = head; *p && n + 30 < sizeof(line); ++p) line[n++] = *p;
    do {
        digits[d++] = (char)('0' + value % 10);
        value /= 10;
    } while (value && d < sizeof(digits));
    while (d) line[n++] = digits[--d];
    for (const char *p = tail; *p && n + 3 < sizeof(line); ++p) line[n++] = *p;
    line[n++] = '\r';
    line[n++] = '\n';
    line[n] = 0;
    log_append(line);
}

/* The world livery id of a paint (5a): SHA-256 over le32(carId) then the design bytes, its first
   8 bytes big-endian, the top two bits 01. */
unsigned long long fi_livery_content_id(unsigned car, const unsigned char *data, size_t size) {
    unsigned char le[4] = {(unsigned char)car, (unsigned char)(car >> 8), (unsigned char)(car >> 16),
                           (unsigned char)(car >> 24)};
    Sha256Context context;
    unsigned char digest[32];
    sha256_initialize(&context);
    sha256_update(&context, le, sizeof(le));
    sha256_update(&context, data, size);
    sha256_finalize(&context, digest);
    return 0x4000000000000000ull | (lw_be64(digest) & 0x3FFFFFFFFFFFFFFFull);
}

static int lw_world_id(unsigned long long id) { return (id >> 62) == 1; }

/* ------------------------------------------------------------- the game's requests */

/* A header's value in the request's header block (CRLF-separated, case-insensitive name). */
static int lw_header(const wchar_t *headers, const wchar_t *name, char *out, size_t cap) {
    size_t name_len = 0;
    while (name[name_len]) ++name_len;
    for (const wchar_t *line = headers; line && *line;) {
        size_t i = 0;
        while (i < name_len && line[i]) {
            wchar_t a = line[i], b = name[i];
            if (a >= L'A' && a <= L'Z') a = (wchar_t)(a + 32);
            if (b >= L'A' && b <= L'Z') b = (wchar_t)(b + 32);
            if (a != b) break;
            ++i;
        }
        if (i == name_len && line[i] == L':') {
            const wchar_t *v = line + i + 1;
            while (*v == L' ') ++v;
            size_t n = 0;
            while (v[n] && v[n] != L'\r' && v[n] != L'\n' && v[n] != L',' && n + 1 < cap) {
                out[n] = v[n] < 128 ? (char)v[n] : '?';
                ++n;
            }
            out[n] = 0;
            return 1;
        }
        while (*line && *line != L'\n') ++line;
        if (*line == L'\n') ++line;
    }
    return 0;
}

static int lw_text_is(const char *a, const char *b) {
    while (*a && *a == *b) {
        ++a;
        ++b;
    }
    return *a == 0 && *b == 0;
}

/* UpdateLivery's parameters (bin/xtsw: big-endian, strings and blobs u32-length-prefixed):
   name, carId i32, numberOfLayers i16, isLocked u8, creatorXuid u64, data. Kept until the
   CreateVersionedLivery that names the same (name, carId). */
static void lw_read_update(const unsigned char *body, size_t size) {
    if (size < 4) return;
    unsigned name_size = lw_be32(body);
    if (name_size > LW_NAME_MAX || size < 4ull + name_size + 4 + 2 + 1 + 8 + 4) return;
    const unsigned char *p = body + 4 + name_size;
    unsigned car = lw_be32(p);
    unsigned short layers = (unsigned short)lw_be16(p + 4);
    unsigned long long creator = lw_be64(p + 7);
    unsigned data_size = lw_be32(p + 15);
    if (data_size == 0 || data_size > LW_MAX_LIVERY || size != 4ull + name_size + 19 + data_size) return;
    unsigned char *copy = lw_alloc(data_size);
    if (!copy) return;
    memory_copy(copy, p + 19, data_size);
    AcquireSRWLockExclusive(&lw_lock);
    lw_free(lw_update.bytes);
    memory_copy(lw_update.name, body + 4, name_size);
    lw_update.name_size = name_size;
    lw_update.car = car;
    lw_update.layers = layers;
    lw_update.creator = creator;
    lw_update.bytes = copy;
    lw_update.size = data_size;
    lw_update.have = 1;
    ReleaseSRWLockExclusive(&lw_lock);
}

/* 12 zero bytes (status 0) then the results: CreateVersionedLivery's versionedLiveryId i64. */
static unsigned char *lw_reply_id(unsigned long long id, size_t *size) {
    unsigned char *r = lw_alloc(20);
    if (!r) return 0;
    for (int i = 0; i < 12; ++i) r[i] = 0;
    lw_put64(r + 12, id);
    *size = 20;
    return r;
}

/* RetrieveVersionedLiveryById's VersionedLiveryData: profileId 0, carId, numberOfLayers,
   creatorXuid, data, two empty thumbnails. */
static unsigned char *lw_reply_paint(const LwPaint *p, size_t *size) {
    size_t n = 42u + p->total;
    unsigned char *r = lw_alloc(n);
    if (!r) return 0;
    for (int i = 0; i < 16; ++i) r[i] = 0;
    lw_put32(r + 16, p->car);
    lw_put16(r + 20, p->layers);
    lw_put64(r + 22, p->creator);
    lw_put32(r + 30, p->total);
    memory_copy(r + 34, p->bytes, p->total);
    lw_put32(r + 34 + p->total, 0);
    lw_put32(r + 38 + p->total, 0);
    *size = n;
    return r;
}

/* The server's "no such livery" (Xls error 1): the game shows the car's stock paint. */
static unsigned char *lw_reply_none(size_t *size) {
    static const unsigned char none[26] = {0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 14, 'n', 'o', ' ',
                                           's', 'u', 'c', 'h', ' ', 'l', 'i', 'v', 'e', 'r', 'y'};
    unsigned char *r = lw_alloc(sizeof(none));
    if (!r) return 0;
    memory_copy(r, none, sizeof(none));
    *size = sizeof(none);
    return r;
}

/* A paint by its world id (own or a world mate's), copied into `out` (owner of bytes). */
static int lw_find(unsigned long long id, LwPaint *out) {
    int found = 0;
    AcquireSRWLockExclusive(&lw_lock);
    if (lw_own.have && lw_own.id == id) {
        unsigned char *copy = lw_alloc(lw_own.size);
        if (copy) {
            memory_copy(copy, lw_own.bytes, lw_own.size);
            *out = (LwPaint){1, 1, 0, id, lw_own.creator, lw_own.seq, 0, 0, lw_own.layers, lw_own.car, lw_own.size,
                             copy, 0, 0};
            found = 1;
        }
    }
    for (int i = 0; !found && i < LW_PEERS; ++i) {
        const LwPaint *p = &lw_peers[i];
        if (p->used && p->complete && p->id == id) {
            unsigned char *copy = lw_alloc(p->total);
            if (copy) {
                memory_copy(copy, p->bytes, p->total);
                *out = *p;
                out->bytes = copy;
                out->have = 0;
                found = 1;
            }
        }
    }
    ReleaseSRWLockExclusive(&lw_lock);
    return found;
}

static void lw_own_changed(void); /* the relay client: PUT the new paint */

/* One line per livery call, passed to the server or answered here, with its id and size (never
   the bytes or a player): either PC's log shows both the painter's and the viewer's side. */
static void lw_log_call(const char *method, const char *how, unsigned long long id, unsigned long long bytes) {
    char line[200], digits[24];
    size_t n = 0;
    const char *parts[] = {"LIVERY: ", method, " ", how};
    for (int k = 0; k < 4; ++k) {
        for (const char *p = parts[k]; *p && n + 60 < sizeof(line); ++p) line[n++] = *p;
    }
    const unsigned long long values[2] = {id, bytes};
    const char *labels[2] = {" id ", ", bytes "};
    for (int k = 0; k < 2; ++k) {
        if (k == 0 && !id) continue;
        for (const char *p = labels[k]; *p; ++p) line[n++] = *p;
        unsigned long long v = values[k];
        size_t d = 0;
        do {
            digits[d++] = (char)('0' + v % 10);
            v /= 10;
        } while (v);
        while (d) line[n++] = digits[--d];
    }
    line[n++] = '.';
    line[n++] = '\r';
    line[n++] = '\n';
    line[n] = 0;
    log_append(line);
}

/* net.c, in a launcher session: 1 the request is answered here (`*reply`, freed with
   fi_livery_free), 0 it goes on (to the server, or the other answers). */
int fi_livery_route(const wchar_t *headers, const unsigned char *body, size_t size, unsigned char **reply,
                    size_t *reply_size) {
    char cls[96], method[64];
    *reply = 0;
    *reply_size = 0;
    if (!headers || !lw_header(headers, L"X-ClassName", cls, sizeof(cls)) ||
        !lw_header(headers, L"X-Method", method, sizeof(method))) {
        return 0;
    }
    if (lw_text_is(cls, "Forza.WebServices.OnlineProfile") && lw_text_is(method, "UpdateLivery")) {
        lw_read_update(body, size);
        lw_log_call("OnlineProfile.UpdateLivery", "passed to the server (read here)", 0, size);
        return 0; /* still the server's (CR-S25: a Supporter Hardcore's permanent paint) */
    }
    if (!lw_text_is(cls, "Forza.WebServices.ContentVersioning")) {
        return 0;
    }
    if (lw_text_is(method, "RemoveReferenceToVersionedLivery")) {
        unsigned long long id = size >= 8 ? lw_be64(body) : 0;
        if (size >= 8 && (lw_world_id(id) || id == ~0ull)) {
            /* a world id (the server never knew it) or -1 (no previous paint): nothing to remove;
               the method has no results, so the reply is the 12-byte status 0 */
            unsigned char *r = lw_alloc(12);
            if (!r) return 0;
            for (int i = 0; i < 12; ++i) r[i] = 0;
            *reply = r;
            *reply_size = 12;
            lw_log_call("ContentVersioning.RemoveReferenceToVersionedLivery", "answered here (nothing to remove)",
                        lw_world_id(id) ? id : 0, size);
            return 1;
        }
        lw_log_call("ContentVersioning.RemoveReferenceToVersionedLivery", "passed to the server", id, size);
        return 0;
    }
    if (lw_text_is(method, "CreateVersionedLivery")) {
        if (size < 4) return 0;
        unsigned name_size = lw_be32(body);
        if (name_size > LW_NAME_MAX || size < 4ull + name_size + 4) return 0;
        unsigned car = lw_be32(body + 4 + name_size);
        unsigned long long id = 0;
        unsigned paint_size = 0;
        AcquireSRWLockExclusive(&lw_lock);
        if (lw_update.have && lw_update.car == car && lw_update.name_size == name_size &&
            memory_equal(lw_update.name, body + 4, name_size)) {
            id = fi_livery_content_id(car, lw_update.bytes, lw_update.size);
            lw_free(lw_own.bytes);
            lw_own.bytes = lw_update.bytes;
            lw_own.size = paint_size = lw_update.size;
            lw_own.car = car;
            lw_own.layers = lw_update.layers;
            lw_own.creator = lw_update.creator;
            lw_own.id = id;
            lw_own.seq = (unsigned short)(lw_own.seq + 1);
            lw_own.have = 1;
            lw_own.stored = 0;
            lw_own.tries = 0;
            lw_own.sent_at = 0;
            lw_update.bytes = 0;
            lw_update.have = 0;
        }
        ReleaseSRWLockExclusive(&lw_lock);
        if (!id) {
            lw_log_call("ContentVersioning.CreateVersionedLivery", "passed to the server (no paint read for it)", 0, size);
            return 0; /* no paint read for it: the server's, as before */
        }
        lw_log_call("ContentVersioning.CreateVersionedLivery", "answered here", id, paint_size);
        lw_log_number("LIVERY: this car's paint has its world id here (", paint_size, " bytes); it goes to the "
                                                                                   "world's relay.");
        lw_own_changed();
        *reply = lw_reply_id(id, reply_size);
        return *reply != 0;
    }
    if (lw_text_is(method, "RetrieveVersionedLiveryById")) {
        if (size < 8) return 0;
        unsigned long long id = lw_be64(body);
        if (!lw_world_id(id)) {
            lw_log_call("ContentVersioning.RetrieveVersionedLiveryById", "passed to the server", id, size);
            return 0; /* a server id (a permanent paint): the server's */
        }
        if (lw_remote_stock_only()) {
            *reply = lw_reply_none(reply_size);
            lw_log_call("ContentVersioning.RetrieveVersionedLiveryById", "refused by stock-only receiver policy", id, 0);
            return *reply != 0;
        }
        LwPaint paint;
        unsigned long long until = GetTickCount64() + LW_RETRIEVE_WAIT_MS;
        int found = lw_find(id, &paint);
        while (!found && GetTickCount64() < until) {
            Sleep(50); /* a world mate's paint still on its way through the relay */
            found = lw_find(id, &paint);
        }
        if (found) {
            *reply = lw_reply_paint(&paint, reply_size);
            lw_log_call("ContentVersioning.RetrieveVersionedLiveryById", "answered here from the relay", id, paint.total);
            lw_free(paint.bytes);
        } else {
            *reply = lw_reply_none(reply_size);
            lw_log_call("ContentVersioning.RetrieveVersionedLiveryById",
                        "not here within 5 s (no such livery: stock paint)", id, 0);
        }
        return *reply != 0;
    }
    return 0;
}

void fi_livery_free(unsigned char *reply) { lw_free(reply); }

/* ------------------------------------------------------------- the relay channel */

typedef UINT_PTR(WINAPI *LwSocketFn)(int, int, int);
typedef int(WINAPI *LwSetOptionFn)(UINT_PTR, int, int, const char *, int);
typedef int(WINAPI *LwRecvFromFn)(UINT_PTR, char *, int, int, void *, int *);
typedef int(WINAPI *LwSendToFn)(UINT_PTR, const char *, int, int, const void *, int);
typedef int(WINAPI *LwCloseFn)(UINT_PTR);
typedef int(WINAPI *LwStartupFn)(WORD, void *);
#define LW_INVALID ((UINT_PTR)~(UINT_PTR)0)

static struct {
    LwSocketFn socket;
    LwSetOptionFn setsockopt;
    LwRecvFromFn recvfrom;
    LwSendToFn sendto;
    LwCloseFn closesocket;
    LwStartupFn startup;
} lw_ws;
static volatile LONG lw_ws_loaded;

static int lw_ws_load(void) {
    if (lw_ws_loaded) return lw_ws_loaded > 0;
    HMODULE ws2 = LoadLibraryExW(L"ws2_32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (ws2) {
        lw_ws.socket = (LwSocketFn)(uintptr_t)GetProcAddress(ws2, "socket");
        lw_ws.setsockopt = (LwSetOptionFn)(uintptr_t)GetProcAddress(ws2, "setsockopt");
        lw_ws.recvfrom = (LwRecvFromFn)(uintptr_t)GetProcAddress(ws2, "recvfrom");
        lw_ws.sendto = (LwSendToFn)(uintptr_t)GetProcAddress(ws2, "sendto");
        lw_ws.closesocket = (LwCloseFn)(uintptr_t)GetProcAddress(ws2, "closesocket");
        lw_ws.startup = (LwStartupFn)(uintptr_t)GetProcAddress(ws2, "WSAStartup");
    }
    int ok = ws2 && lw_ws.socket && lw_ws.setsockopt && lw_ws.recvfrom && lw_ws.sendto && lw_ws.closesocket &&
             lw_ws.startup;
    if (ok) {
        static unsigned char data[512]; /* WSADATA */
        ok = lw_ws.startup(0x0202, data) == 0;
    }
    lw_ws_loaded = ok ? 1 : -1;
    return ok;
}

/* the world (a sockaddr_in6, as freeroam.c has it) and how the relay is reached */
static unsigned char lw_world[28];
static volatile LONG lw_serial;   /* a new world, or none: the worker of an older one stops */
static int lw_mode;               /* 0 none, 1 own socket, 2 the proxy's socket */
static UINT_PTR lw_socket = LW_INVALID;
/* Leave and terminal control may race. Transfer ownership before closing so they cannot
   close the same descriptor twice (the second close could hit an unrelated reused handle). */
static void lw_close_socket(void) {
    UINT_PTR owned = (UINT_PTR)InterlockedExchangePointer((void *volatile *)&lw_socket, (void *)LW_INVALID);
    if (owned != LW_INVALID) lw_ws.closesocket(owned);
}
static int lw_done;
static unsigned lw_want_tries;
static unsigned long long lw_want_at;

static int lw_private(const unsigned char *a /* 16 bytes */) {
    int mapped = 1;
    for (int i = 0; i < 10 && mapped; ++i) mapped = a[i] == 0;
    mapped = mapped && a[10] == 0xff && a[11] == 0xff;
    if (mapped) {
        const unsigned char *v4 = a + 12;
        return v4[0] == 10 || v4[0] == 127 || (v4[0] == 172 && (v4[1] & 0xf0) == 16) ||
               (v4[0] == 192 && v4[1] == 168) || (v4[0] == 169 && v4[1] == 254);
    }
    int loopback = 1;
    for (int i = 0; i < 15 && loopback; ++i) loopback = a[i] == 0;
    return (loopback && a[15] == 1) || (a[0] & 0xfe) == 0xfc || (a[0] == 0xfe && (a[1] & 0xc0) == 0x80);
}

static void lw_send(const unsigned char *data, int size) {
    if (!fi_launch_network_allowed()) return;
    if (lw_mode == 1 && lw_socket != LW_INVALID) {
        lw_ws.sendto(lw_socket, (const char *)data, size, 0, lw_world, (int)sizeof(lw_world));
    } else if (lw_mode == 2) {
        azimuth_proxy_send_world(data, size);
    }
}

/* One message: the STUN header (type, length, magic cookie, transaction id), then the attribute
   (type, length, value padded to 4). */
static int lw_message(unsigned char *m, const unsigned char *value, unsigned size) {
    unsigned padded = (size + 3u) & ~3u;
    lw_put16(m, LW_KIND);
    lw_put16(m + 2, 4 + padded);
    lw_put32(m + 4, LW_MAGIC);
    unsigned long long t = GetTickCount64() ^ ((unsigned long long)(uintptr_t)m << 7);
    for (int i = 0; i < 12; ++i) {
        t = t * 6364136223846793005ull + 1442695040888963407ull;
        m[8 + i] = (unsigned char)(t >> 33);
    }
    lw_put16(m + 20, LW_ATTR);
    lw_put16(m + 22, size);
    memory_copy(m + 24, value, size);
    for (unsigned i = size; i < padded; ++i) m[24 + i] = 0;
    return (int)(24 + padded);
}

static void lw_send_want(void) {
    if (lw_remote_stock_only()) return;
    unsigned char v[10], m[40];
    v[0] = LW_VERSION;
    v[1] = LW_OP_WANT;
    lw_put64(v + 2, azimuth_account_xuid_value());
    lw_send(m, lw_message(m, v, sizeof(v)));
}

static void lw_send_ack(unsigned long long owner, unsigned seq) {
    unsigned char v[12], m[40];
    v[0] = LW_VERSION;
    v[1] = LW_OP_ACK;
    lw_put64(v + 2, owner);
    lw_put16(v + 10, seq);
    lw_send(m, lw_message(m, v, sizeof(v)));
}

/* Every chunk of the player's own paint (called with lw_lock held). */
static void lw_send_own(void) {
    if (lw_remote_stock_only()) return;
    static unsigned char v[2 + LW_CHUNK_HEAD + LW_CHUNK], m[24 + sizeof(v) + 4];
    unsigned count = (lw_own.size + LW_CHUNK - 1) / LW_CHUNK;
    unsigned long long xuid = azimuth_account_xuid_value();
    for (unsigned i = 0; i < count; ++i) {
        unsigned at = i * LW_CHUNK, n = lw_own.size - at < LW_CHUNK ? lw_own.size - at : LW_CHUNK;
        v[0] = LW_VERSION;
        v[1] = LW_OP_PUT;
        lw_put16(v + 2, lw_own.seq);
        lw_put16(v + 4, i);
        lw_put16(v + 6, count);
        lw_put32(v + 8, lw_own.size);
        lw_put32(v + 12, lw_own.car);
        lw_put64(v + 16, xuid);
        lw_put16(v + 24, lw_own.layers);
        lw_put64(v + 26, lw_own.creator);
        memory_copy(v + 2 + LW_CHUNK_HEAD, lw_own.bytes + at, n);
        lw_send(m, lw_message(m, v, 2 + LW_CHUNK_HEAD + n));
    }
}

static void lw_drop_peer(LwPaint *p) {
    lw_free(p->bytes);
    lw_free(p->have);
    *p = (LwPaint){0};
}

static int lw_sequence_after(unsigned next, unsigned previous) {
    unsigned distance = (next - previous) & 0xffffu;
    return distance != 0 && distance < 0x8000u;
}

static int lw_chunk_matches(const LwPaint *p, unsigned car, unsigned layers,
                            unsigned long long creator, unsigned total, unsigned count) {
    return p->car == car && p->layers == layers && p->creator == creator &&
           p->total == total && p->count == count;
}

static void lw_refuse_incoming(LwPaint *p) {
    lw_free(p->bytes);
    lw_free(p->have);
    p->bytes = p->have = 0;
    p->refused = 1;
}

/* A world mate's chunk (DATA): assembled, then kept with its world id. */
static void lw_on_data(const unsigned char *v, unsigned size) {
    if (lw_remote_stock_only()) return;
    if (size < 2 + LW_CHUNK_HEAD) return;
    unsigned seq = lw_be16(v + 2), index = lw_be16(v + 4), count = lw_be16(v + 6), total = lw_be32(v + 8);
    unsigned car = lw_be32(v + 12);
    unsigned long long owner = lw_be64(v + 16);
    unsigned layers = lw_be16(v + 24);
    unsigned long long creator = lw_be64(v + 26);
    unsigned n = size - 2 - LW_CHUNK_HEAD;
    if (total > LW_MAX_LIVERY || !owner) return;
    unsigned want_count = total ? (total + LW_CHUNK - 1) / LW_CHUNK : 1;
    unsigned expected = index + 1 < want_count ? LW_CHUNK : total - (want_count - 1) * LW_CHUNK;
    if (count != want_count || index >= count || n != expected) return;
    int ack = 0, done_now = 0;
    AcquireSRWLockExclusive(&lw_lock);
    int at = -1;
    for (int i = 0; i < LW_PEERS && at < 0; ++i)
        if (lw_peers[i].used && lw_peers[i].owner == owner) at = i;
    if (at < 0)
        for (int i = 0; i < LW_PEERS && at < 0; ++i)
            if (!lw_peers[i].used) at = i;
    if (at >= 0) {
        LwPaint *current = &lw_peers[at], *slot = &lw_incoming[at];
        if (!current->used) { current->used = 1; current->owner = owner; }
        const LwPaint *floor = slot->used ? slot : current->complete ? current : 0;
        if (floor && (seq != floor->seq ? !lw_sequence_after(seq, floor->seq) : floor->refused)) goto finished;
        if (current->complete && current->seq == seq) {
            /* A duplicate is acknowledged only if it describes exactly the stored content. */
            ack = lw_chunk_matches(current, car, layers, creator, total, count) &&
                  memory_equal(current->bytes + index * LW_CHUNK, v + 2 + LW_CHUNK_HEAD, n);
            if (!ack) {
                lw_drop_peer(slot);
                slot->used = slot->refused = 1;
                slot->owner = owner;
                slot->seq = (unsigned short)seq;
            }
            goto finished;
        }
        if (!slot->used || slot->seq != seq) {
            lw_drop_peer(slot);
            slot->used = 1;
            slot->owner = owner;
            slot->seq = (unsigned short)seq;
            slot->count = (unsigned short)count;
            slot->total = total;
            slot->car = car;
            slot->layers = (unsigned short)layers;
            slot->creator = creator;
            slot->bytes = lw_alloc(total ? total : 1);
            slot->have = lw_alloc(count);
            if (!slot->bytes || !slot->have) { lw_refuse_incoming(slot); goto finished; }
            for (unsigned i = 0; i < count; ++i) slot->have[i] = 0;
        }
        if (!lw_chunk_matches(slot, car, layers, creator, total, count)) {
            lw_refuse_incoming(slot);
            goto finished;
        }
        if (slot->have[index]) {
            if (!memory_equal(slot->bytes + index * LW_CHUNK, v + 2 + LW_CHUNK_HEAD, n)) lw_refuse_incoming(slot);
            goto finished;
        }
        memory_copy(slot->bytes + index * LW_CHUNK, v + 2 + LW_CHUNK_HEAD, n);
        slot->have[index] = 1;
        if (++slot->got == slot->count) {
            slot->complete = 1;
            slot->id = fi_livery_content_id(car, slot->bytes, total);
            lw_free(slot->have);
            slot->have = 0;
            lw_drop_peer(current);
            *current = *slot;
            *slot = (LwPaint){0};
            ack = done_now = 1;
        }
    }
finished:
    ReleaseSRWLockExclusive(&lw_lock);
    if (ack) lw_send_ack(owner, seq);
    if (done_now) {
        static volatile LONG logged;
        if (!InterlockedExchange(&logged, 1)) log_append("LIVERY: a world mate's paint came through the relay.\r\n");
    }
}

/* A message from the relay (the own socket's thread, or proxy.c). */
void azimuth_livery_received(const unsigned char *packet, int size) {
    if (!fi_launch_network_allowed()) return;
    if (size < 28 || lw_be16(packet) != LW_KIND || lw_be32(packet + 4) != LW_MAGIC ||
        lw_be16(packet + 2) + 20u != (unsigned)size) {
        return;
    }
    for (int at = 20; at + 4 <= size;) {
        unsigned type = lw_be16(packet + at), n = lw_be16(packet + at + 2);
        if (at + 4 + (int)n > size) return;
        const unsigned char *v = packet + at + 4;
        if (type == LW_ATTR && n >= 2 && v[0] == LW_VERSION) {
            switch (v[1]) {
            case LW_OP_STORED:
                if (n == 4) {
                    AcquireSRWLockExclusive(&lw_lock);
                    int first = lw_own.have && !lw_own.stored && lw_own.seq == lw_be16(v + 2);
                    if (first) lw_own.stored = 1;
                    ReleaseSRWLockExclusive(&lw_lock);
                    if (first) log_append("LIVERY: the world's relay keeps this car's paint for the world mates.\r\n");
                }
                break;
            case LW_OP_DATA: lw_on_data(v, n); break;
            case LW_OP_DONE:
                if (n == 3) {
                    lw_done = 1;
                    lw_log_number("LIVERY: the world mates' paints are here (", v[2], ").");
                }
                break;
            case LW_OP_GONE:
                if (n == 10) {
                    unsigned long long owner = lw_be64(v + 2);
                    AcquireSRWLockExclusive(&lw_lock);
                    for (int i = 0; i < LW_PEERS; ++i) {
                        if (lw_peers[i].used && lw_peers[i].owner == owner) {
                            lw_drop_peer(&lw_peers[i]);
                            lw_drop_peer(&lw_incoming[i]);
                        }
                    }
                    ReleaseSRWLockExclusive(&lw_lock);
                }
                break;
            default: break;
            }
            return;
        }
        at += 4 + (int)((n + 3u) & ~3u);
    }
}

/* The world's worker: the relay's answers on the own socket, WANT until DONE, PUT until STORED. */
static DWORD WINAPI lw_worker(void *argument) {
    LONG serial = (LONG)(intptr_t)argument;
    static unsigned char packet[1024];
    unsigned char from[64];
    while (lw_serial == serial && fi_launch_network_allowed()) {
        if (lw_mode == 1 && lw_socket != LW_INVALID) {
            int from_size = (int)sizeof(from);
            int n = lw_ws.recvfrom(lw_socket, (char *)packet, sizeof(packet), 0, from, &from_size);
            if (lw_serial != serial || !fi_launch_network_allowed()) break;
            if (n >= 2 && lw_be16(packet) == 0x3E91u) {
                azimuth_carstate_received(packet, n); /* the relay passing another car's headlights and roof */
            } else if (n > 0) {
                azimuth_livery_received(packet, n);
            }
        } else {
            Sleep(100);
        }
        unsigned long long now = GetTickCount64();
        if (!lw_done && lw_want_tries < LW_WANT_TRIES && now - lw_want_at >= LW_WANT_EVERY_MS) {
            lw_want_at = now;
            ++lw_want_tries;
            lw_send_want();
        }
        AcquireSRWLockExclusive(&lw_lock);
        if (lw_own.have && !lw_own.stored && lw_own.tries < LW_PUT_TRIES && now - lw_own.sent_at >= LW_PUT_EVERY_MS) {
            lw_own.sent_at = now;
            ++lw_own.tries;
            lw_send_own();
        }
        ReleaseSRWLockExclusive(&lw_lock);
    }
    return 0;
}

static void lw_own_changed(void) {
    /* the worker sends it at its next turn (it is the only one that sends PUTs) */
}

/* freeroam.c: the game joined a world (`remote`: its sockaddr_in6). */
void azimuth_livery_world_entered(const void *remote) {
    if (!fi_launch_protocol() || !remote || !fi_launch_network_allowed()) return;
    const unsigned char *r = remote;
    LONG serial = InterlockedIncrement(&lw_serial);
    lw_close_socket();
    memory_copy(lw_world, r, sizeof(lw_world));
    lw_done = 0;
    lw_want_tries = 0;
    lw_want_at = 0;
    AcquireSRWLockExclusive(&lw_lock);
    for (int i = 0; i < LW_PEERS; ++i) { lw_drop_peer(&lw_peers[i]); lw_drop_peer(&lw_incoming[i]); }
    lw_own.stored = 0; /* a new world: the paint goes to its relay too */
    lw_own.tries = 0;
    lw_own.sent_at = 0;
    ReleaseSRWLockExclusive(&lw_lock);
    lw_mode = 0;
    if (azimuth_proxy_proof_mode()) {
        lw_mode=2; /* FIUP proves game and hook kinds separately on the forwarding socket. */
    } else if (lw_private(r + 8)) {
        if (lw_ws_load()) {
            UINT_PTR s = lw_ws.socket(23 /* AF_INET6 */, 2 /* SOCK_DGRAM */, 17 /* IPPROTO_UDP */);
            if (s != LW_INVALID) {
                DWORD off = 0, timeout = 100;
                lw_ws.setsockopt(s, 41 /* IPPROTO_IPV6 */, 27 /* IPV6_V6ONLY */, (const char *)&off, sizeof(off));
                lw_ws.setsockopt(s, 0xffff /* SOL_SOCKET */, 0x1006 /* SO_RCVTIMEO */, (const char *)&timeout,
                                 sizeof(timeout));
                lw_socket = s;
                lw_mode = 1;
            }
        }
    } else {
        lw_mode = 2; /* the joining player's proxy socket: the address the relay knows this peer by */
    }
    if (!lw_mode) {
        log_append("LIVERY: no socket for the world's relay; world mates' paints stay stock.\r\n");
        return;
    }
    if (!fi_launch_network_allowed()) {
        azimuth_livery_world_left();
        return;
    }
    HANDLE t = CreateThread(0, 0, lw_worker, (void *)(intptr_t)serial, 0, 0);
    if (t) CloseHandle(t);
    log_append(lw_mode == 1 ? "LIVERY: world mates' paints through the relay (this PC's own socket).\r\n"
                            : "LIVERY: world mates' paints through the relay (the proxy's socket).\r\n");
}

/* carstate.c: a message to the world's relay from this hook's own socket, for a world on this
   network that the game reached directly (no proxy socket to speak from; the relay binds a
   private peer's other ports, MATCHMAKING_API.md 5a). 1 sent. The relay's answers come back on
   the same socket (lw_worker hands carstate's on). */
int azimuth_world_lan_send(const void *data, int size) {
    if (!fi_launch_network_allowed() || lw_mode != 1 || lw_socket == LW_INVALID) return 0;
    return lw_ws.sendto(lw_socket, (const char *)data, size, 0, lw_world, (int)sizeof(lw_world)) == size;
}

/* freeroam.c: the game left the world; its paints go (the own one stays for the next world). */
void azimuth_livery_world_left(void) {
    InterlockedIncrement(&lw_serial);
    lw_mode = 0;
    lw_close_socket();
    AcquireSRWLockExclusive(&lw_lock);
    for (int i = 0; i < LW_PEERS; ++i) { lw_drop_peer(&lw_peers[i]); lw_drop_peer(&lw_incoming[i]); }
    ReleaseSRWLockExclusive(&lw_lock);
}

/* --------------------------------------------- the current paint before a world (OD-66) */

/* The overlay's "Syncing cars" (FiOverlayHost.livery_sync, livery.c) asks, once a world is
   created or a seat reserved, for the active car's current paint to get its world id before the
   player enters: the game's own ApplyLivery on that car (SAURON's RE, livery-presync.md 1,
   verified in disassembly), the same path as a manual apply in the paint shop. It builds the car
   save chain; the hook submits it to the game's background saver; the game then serializes the
   paint and calls UpdateLivery and CreateVersionedLivery, which the hook answers above (the world
   id, then the PUT once the world is joined); the game writes the id on the car and replicates it.
   The run happens on the game thread (fi_livery_game_tick, from carstate_hook.c's car update),
   at most once per car per session; the overlay waits for the Create answer (<= its timeout). */
#define LW_GAME_RVA 0x327270u           /* the game singleton accessor */
#define LW_GARAGE_CAR_RVA 0x2dadd0u     /* (garage, &carRef, garage id, 0): a referenced garage car */
#define LW_APPLY_LIVERY_RVA 0x2f3520u   /* the car's ApplyLivery(car, &out, &liveryRef, 0) */
#define LW_NEW_RVA 0x3229c0u            /* the game's operator new(size) */
#define LW_RUNNER_VTABLE_RVA 0x2d3bbb8u /* Forza::CBackgroundSaveTask */
#define LW_RUNNER_INIT_RVA 0x33b660u    /* its queue's constructor (this) */
#define LW_RUNNER_ADD_RVA 0xa46ac0u     /* (runner, &taskRef): consumes the ref passed */
#define LW_SCHEDULE_RVA 0x126c170u      /* (scheduler, runner): takes the runner */
#define LW_SCHEDULER_RVA 0x367a6d0u     /* the global async scheduler pointer */
#define LW_FREE_RVA 0x31c7b0u           /* the game's free, for out's buffer */
#define LW_CODE_END_RVA 0x1dcc000u

typedef void *(*LwObjectFn)(void *);
typedef void *(*LwNoArgFn)(void);
typedef void (*LwGarageCarFn)(void *, void **, unsigned, int);
typedef void *(*LwApplyFn)(void *, void *, void **, unsigned long long);
typedef void *(*LwNewFn)(size_t);
typedef void (*LwAddFn)(void *, void **);
typedef void (*LwScheduleFn)(void *, void *);
typedef void (*LwFreeFn)(void *);

static unsigned char *lw_game;
static volatile LONG lw_pre_state; /* 0 idle, 1 asked, 2 building, 3 scheduled, 4 stock, 5 failed, 7 confirmed */
static volatile LONG lw_pre_busy;
static unsigned lw_pre_generation, lw_pre_garage_id;
static unsigned short lw_pre_seq;
static int lw_pending_for(unsigned generation) {
    return (lw_pre_state == 2 || lw_pre_state == 3) && lw_pre_generation == generation;
}

/* An object's virtual method at `offset`, if it points into the game's code. */
static uintptr_t lw_method(void *object, unsigned offset) {
    void *vtable;
    uintptr_t address;
    if (!object || !memory_read_pointer(object, &vtable) || !memory_read((unsigned char *)vtable + offset, &address, 8) ||
        address < (uintptr_t)lw_game + 0x1000 || address >= (uintptr_t)lw_game + LW_CODE_END_RVA) {
        return 0;
    }
    return address;
}

static int lw_add_ref(void *object) {
    uintptr_t f = lw_method(object, 0x10);
    if (f) ((LwObjectFn)f)(object);
    return f != 0;
}

static void lw_release(void *object) {
    uintptr_t f = lw_method(object, 0x18);
    if (f) ((LwObjectFn)f)(object);
}

/* A garage row id the game can load (game+0x2dadd0): never 0, nor the loaner's 0xFFFFFFFF, nor anything past INT_MAX. */
static int lw_garage_row_id(unsigned garage_id) {
    return garage_id != 0 && garage_id < 0x7fffffffu;
}

/* The run itself, on the game thread: 3 ran, 4 the car has no paint, 5 not possible. */
static LONG lw_apply_current(void) {
    unsigned generation = 0;
    if (!fi_game_ready(&generation) || generation != lw_pre_generation) return 5;
    void *game = ((LwNoArgFn)(uintptr_t)(lw_game + LW_GAME_RVA))();
    uintptr_t get_player = lw_method(game, 0x48);
    void *player = get_player ? ((LwObjectFn)get_player)(game) : 0;
    uintptr_t get_garage = lw_method(player, 0xd8), get_current = lw_method(player, 0x100);
    void *garage = get_garage ? ((LwObjectFn)get_garage)(player) : 0;
    void *current = get_current ? ((LwObjectFn)get_current)(player) : 0;
    unsigned garage_id = 0;
    /* a loaner (a Bucket List car) has no garage row: its id is 0xFFFFFFFF, and loading it by id makes the game's own
       stats code crash on an unset record (2026-10-27): only a plausible row id is ever loaded */
    if (!garage || !current || !memory_read((unsigned char *)current + 0x260, &garage_id, 4) || !lw_garage_row_id(garage_id)) {
        return 5;
    }
    lw_pre_garage_id = garage_id;
    void *car = 0;
    ((LwGarageCarFn)(uintptr_t)(lw_game + LW_GARAGE_CAR_RVA))(garage, &car, garage_id, 0);
    if (!car) return 5;
    /* the car's livery holder (car+0x1e8) has its paint at +0x28: none is the stock paint */
    void *paint = 0;
    if (!memory_read((unsigned char *)car + 0x1e8 + 0x28, &paint, sizeof(paint))) {
        lw_release(car);
        return 5;
    }
    if (!paint) {
        lw_release(car);
        return 4; /* stock paint: nothing to send */
    }
    void *scheduler = 0;
    if (!memory_read_pointer(lw_game + LW_SCHEDULER_RVA, &scheduler)) {
        lw_release(car);
        return 5;
    }
    /* out: a std::vector of task refs (begin, end, capacity) */
    struct {
        void **begin, **end, **capacity;
    } out = {0, 0, 0};
    /* ApplyLivery's third argument is a ref to the paint-shop's livery object (0x230 bytes, its
       own holder at +0xb8), not the car holder's paint: the base task (game+0x2df330) copies
       that holder. Null makes the base task build that object from the car's current holder
       (game+0x2c0f50 -> 0x2df070(car)): exactly "save the paint it has". (Passing the holder's
       paint made the base task lock garbage at +0xb8: the 2026-10-19 crash, Ghidra here.) */
    void *paint_ref = 0;
    ((LwApplyFn)(uintptr_t)(lw_game + LW_APPLY_LIVERY_RVA))(car, &out, &paint_ref, 0);
    unsigned char *runner = out.begin != out.end ? ((LwNewFn)(uintptr_t)(lw_game + LW_NEW_RVA))(0x98) : 0;
    LONG result = 5;
    if (runner) {
        for (unsigned i = 0; i < 0x98; ++i) runner[i] = 0;
        uintptr_t vtable = (uintptr_t)(lw_game + LW_RUNNER_VTABLE_RVA);
        memory_copy(runner, &vtable, 8);
        ((LwObjectFn)(uintptr_t)(lw_game + LW_RUNNER_INIT_RVA))(runner + 8);
        for (void **task = out.begin; task < out.end; ++task) {
            void *task_ref = *task;
            if (task_ref && lw_add_ref(task_ref)) {
                ((LwAddFn)(uintptr_t)(lw_game + LW_RUNNER_ADD_RVA))(runner, &task_ref); /* consumes it */
            }
        }
        ((LwScheduleFn)(uintptr_t)(lw_game + LW_SCHEDULE_RVA))(scheduler, runner); /* the scheduler owns it */
        result = 3;
    }
    for (void **task = out.begin; task < out.end; ++task) lw_release(*task);
    if (out.begin) ((LwFreeFn)(uintptr_t)(lw_game + LW_FREE_RVA))(out.begin);
    lw_release(car);
    return result;
}

/* Completion must name this request's active garage car, not merely any paint callback. */
static void lw_check_completion(unsigned garage_id, long long car_id) {
    if (lw_pre_state != 3 || garage_id != lw_pre_garage_id || car_id <= 0) return;
    AcquireSRWLockShared(&lw_lock);
    int confirmed = lw_own.have && lw_own.seq != lw_pre_seq && lw_own.id == (unsigned long long)car_id;
    ReleaseSRWLockShared(&lw_lock);
    if (confirmed) InterlockedCompareExchange(&lw_pre_state, 7, 3);
}

/* Diagnostic (log only, SAURON's ask): the paint shop's apply (game+0xd25460) also sets the
   player's replicated livery field player+0x1a8 (game+0x2a6120(player, x)), which the run above
   does not; to learn what x is, every second the game thread reads player+0x1a8 and the active
   car's VersionedLiveryId (car+0x258) and logs any change. */
#define LW_PLAYER_RECORD_RVA 0xced1e0u
#define LW_SYNC_FIELD_RVA 0x2a6120u /* (player, id): player+0x1a8 = id, marked for replication */
typedef void (*LwSyncFieldFn)(void *, long long);
static unsigned long long lw_watch_at;
static long long lw_seen_field = 0x7fffffffffffffffll, lw_seen_car = 0x7fffffffffffffffll;

static void lw_watch(void) {
    unsigned long long now = GetTickCount64();
    if (now - lw_watch_at < 1000) return;
    lw_watch_at = now;
    void *record = ((LwNoArgFn)(uintptr_t)(lw_game + LW_PLAYER_RECORD_RVA))();
    long long field = 0, car_id = -2;
    if (!record || !memory_read((unsigned char *)record - 0xae8 + 0x1a8, &field, sizeof(field))) return;
    void *game = ((LwNoArgFn)(uintptr_t)(lw_game + LW_GAME_RVA))();
    uintptr_t get_player = lw_method(game, 0x48);
    void *player = get_player ? ((LwObjectFn)get_player)(game) : 0;
    uintptr_t get_garage = lw_method(player, 0xd8), get_current = lw_method(player, 0x100);
    void *garage = get_garage ? ((LwObjectFn)get_garage)(player) : 0;
    void *current = get_current ? ((LwObjectFn)get_current)(player) : 0;
    unsigned garage_id = 0;
    if (garage && current && memory_read((unsigned char *)current + 0x260, &garage_id, 4) && lw_garage_row_id(garage_id)) {
        void *car = 0;
        ((LwGarageCarFn)(uintptr_t)(lw_game + LW_GARAGE_CAR_RVA))(garage, &car, garage_id, 0);
        if (car) {
            memory_read((unsigned char *)car + 0x258, &car_id, sizeof(car_id));
            lw_release(car);
        }
    }
    /* The player's replicated livery field is the current car's VersionedLiveryId (SAURON's .69
       log: the game sets it from car+0x258 on load; world mates fetch by it). The run of the
       current paint sets car+0x258 but not the field (the paint shop's apply does both,
       game+0xd25460): set it the paint shop's way, game+0x2a6120(player, id), which returns at
       once when unchanged and otherwise marks the field for replication. */
    if (car_id > 0 && field != car_id) {
        ((LwSyncFieldFn)(uintptr_t)(lw_game + LW_SYNC_FIELD_RVA))((unsigned char *)record - 0xae8, car_id);
        log_append("LIVERY: the player's replicated paint id now names the car's paint (world mates fetch it).\r\n");
        memory_read((unsigned char *)record - 0xae8 + 0x1a8, &field, sizeof(field));
    }
    if (field == car_id) lw_check_completion(garage_id, car_id);
    if (field == lw_seen_field && car_id == lw_seen_car) return;
    lw_seen_field = field;
    lw_seen_car = car_id;
    char line[160];
    size_t n = 0;
    const char *head = "LIVERY: diag player+0x1a8=0x";
    while (*head) line[n++] = *head++;
    static const char hex[] = "0123456789abcdef";
    for (int s = 60; s >= 0; s -= 4) line[n++] = hex[((unsigned long long)field >> s) & 15];
    const char *mid = " car+0x258=0x";
    while (*mid) line[n++] = *mid++;
    for (int s = 60; s >= 0; s -= 4) line[n++] = hex[((unsigned long long)car_id >> s) & 15];
    const char *tail = " garage id ";
    while (*tail) line[n++] = *tail++;
    char digits[12];
    int d = 0;
    unsigned g = garage_id;
    do {
        digits[d++] = (char)('0' + g % 10);
        g /= 10;
    } while (g && d < 11);
    while (d) line[n++] = digits[--d];
    line[n++] = '\r';
    line[n++] = '\n';
    line[n] = 0;
    log_append(line);
}

/* carstate_hook.c's car update (the game thread, every frame): the asked run, once. */
void fi_livery_game_tick(void) {
    unsigned generation = 0;
    if (!fi_game_ready(&generation)) {
        if (lw_pre_state == 1 || lw_pre_state == 3) InterlockedExchange(&lw_pre_state, 5);
        return;
    }
    if ((lw_pre_state == 1 || lw_pre_state == 3) && generation != lw_pre_generation) {
        InterlockedExchange(&lw_pre_state, 5);
    }
    /* The watch reloads the current car as a garage car by its id (game+0x2dadd0), which the game itself
       cannot survive when the current car is not a garage car: a Bucket List loaner has no garage row, the
       load fills a record with the "unset" sentinels and the game's own stats code runs off the stack
       (2026-10-27, SAURON's RE of the minidump). So it runs only while a paint run this player asked is
       waiting for its confirmation (state 3), never every second for the whole session. */
    if (lw_game && fi_launch_protocol() && lw_pre_state == 3) {
        lw_watch();
    }
    if (lw_pre_state != 1 || !lw_game || InterlockedCompareExchange(&lw_pre_state, 2, 1) != 1) {
        return;
    }
    LONG result = lw_apply_current();
    InterlockedExchange(&lw_pre_state, result);
}

/* livery.c's fi_livery_sync (the overlay's thread): ask the run, then wait for the game's
   CreateVersionedLivery answered here (a FI_LIVERY_* code). */
static int lw_run_current(unsigned timeout_ms) {
    if (!lw_game || !fi_launch_protocol()) {
        return FI_LIVERY_UNAVAILABLE;
    }
    unsigned generation = 0;
    if (!fi_game_ready(&generation)) return FI_LIVERY_FAILED;
    /* A timed-out scheduled job remains pending: a retry waits for it instead of claiming it
       succeeded or scheduling a second copy. Completed requests are never a per-car cache. */
    if (!lw_pending_for(generation)) {
        lw_pre_generation = generation;
        lw_pre_garage_id = 0;
        AcquireSRWLockShared(&lw_lock);
        lw_pre_seq = lw_own.seq;
        ReleaseSRWLockShared(&lw_lock);
        InterlockedExchange(&lw_pre_state, 1);
    }
    unsigned long long until = GetTickCount64() + timeout_ms;
    log_append("LIVERY: the current paint before the world: asked (the game thread runs it).\r\n");
    LONG state;
    while ((state = lw_pre_state) == 1 || state == 2 || state == 3) {
        unsigned current_generation = 0;
        if (!fi_game_ready(&current_generation) || current_generation != generation) return FI_LIVERY_FAILED;
        if (GetTickCount64() >= until) {
            InterlockedCompareExchange(&lw_pre_state, 0, 1); /* not started: drop the ask */
            log_append("LIVERY: the current paint before the world: the game thread did not run it in time.\r\n");
            return FI_LIVERY_TIMEOUT;
        }
        Sleep(20);
    }
    InterlockedExchange(&lw_pre_state, 0);
    if (state == 7) {
        log_append("LIVERY: the current paint before the world: this car's world id confirmed.\r\n");
        return FI_LIVERY_SYNCED;
    }
    if (state == 4) {
        log_append("LIVERY: the current paint before the world: the car has its stock paint.\r\n");
        return FI_LIVERY_STOCK;
    }
    log_append("LIVERY: the current paint before the world: not possible here (no player, car or saver).\r\n");
    return FI_LIVERY_FAILED;
}

int fi_livery_run_current(unsigned timeout_ms) {
    if (InterlockedCompareExchange(&lw_pre_busy, 1, 0) != 0) return FI_LIVERY_FAILED;
    int result = lw_run_current(timeout_ms);
    InterlockedExchange(&lw_pre_busy, 0);
    return result;
}

/* ------------------------------------------------------- remote paint load boundary */

#define LW_QUEUE_REMOTE_RVA 0x9ee3d0u
#define LW_NOTIFY_VTABLE_RVA 0x2c62ff0u
#define LW_NOTIFY_FAILURE_RVA 0x1db440u
#define LW_NOTIFY_RELEASE_RVA 0x3224acu
#define LW_CAPTURE_RECEIVER 5u

/* Win64: manager, versioned id, pointer to one owned notify reference, opaque fourth register.
   The game's id=-1 path does no queue allocation but still releases that owned reference. */
typedef void (*LwQueueRemote)(void *, unsigned long long, void **, uintptr_t);
typedef void (*LwNotifyFailure)(void *);
static LwQueueRemote lw_queue_original;
static unsigned char *lw_receiver_image;
static volatile LONG lw_receiver_ready, lw_receiver_faulted;

static const unsigned char lw_queue_bytes[16] = {
    0x48, 0x8b, 0xc4, 0x4c, 0x89, 0x40, 0x18, 0x48,
    0x89, 0x50, 0x10, 0x57, 0x48, 0x83, 0xec, 0x50
};
static const unsigned char lw_failure_bytes[3] = {0xc2, 0x00, 0x00};
static const unsigned char lw_release_bytes[12] = {
    0x48, 0x81, 0xe9, 0xd8, 0x02, 0x00, 0x00, 0xe9, 0x78, 0xff, 0xff, 0xff
};

int fi_livery_remote_ready(void) {
    return lw_receiver_ready != 0 && lw_receiver_faulted == 0;
}

int fi_livery_remote_policy(unsigned mode) {
    /* No permissive mode exists in this version. Unsupported policy always fails closed. */
    return mode == 0 && fi_livery_remote_ready();
}

static void lw_receiver_fault(void) {
    InterlockedExchange(&lw_receiver_ready, 0);
    if (InterlockedExchange(&lw_receiver_faulted, 1) == 0) fi_launch_capture(LW_CAPTURE_RECEIVER, 0);
}

static void lw_queue_remote(void *manager, unsigned long long id, void **notify, uintptr_t opaque) {
    if (!lw_remote_stock_only()) {
        lw_queue_original(manager, id, notify, opaque);
        return;
    }
    void *listener = 0, *vtable = 0;
    if (!notify || !memory_read(notify, &listener, sizeof(listener))) {
        lw_receiver_fault();
        return; /* no valid owned reference is available to release */
    }
    if (listener) {
        if (!memory_read_pointer(listener, &vtable)) {
            lw_receiver_fault();
            return;
        }
        if (vtable == lw_game + LW_NOTIFY_VTABLE_RVA) {
            /* The supported interface's failure method is a checked no-op. The original
               id=-1 path below, rather than this wrapper, consumes exactly the owned ref. */
            ((LwNotifyFailure)(uintptr_t)(lw_game + LW_NOTIFY_FAILURE_RVA))(listener);
        } else {
            uintptr_t release = 0;
            lw_receiver_fault();
            /* The original function owns this internal game reference. Only its usual
               cleanup may consume an unexpected readable game interface; an unreadable
               release slot cannot be repaired by guessing an object or an address. */
            if (!memory_read((unsigned char *)vtable + 0x18, &release, sizeof(release)) ||
                release < (uintptr_t)lw_game + 0x1000 || release >= (uintptr_t)lw_game + 0x1dcc000u) return;
        }
    }
    /* This precedes both the existing disk-cache hit and the HTTP completion/parser path.
       Do not pass an empty successful livery or synthesize another car/model pointer. */
    lw_queue_original(manager, ~0ull, notify, opaque);
}

static int lw_receiver_image_ok(unsigned char *image) {
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    unsigned char queue[sizeof(lw_queue_bytes)], failure[sizeof(lw_failure_bytes)], release[sizeof(lw_release_bytes)];
    uintptr_t on_failure = 0, on_release = 0;
    return image && memory_read(image, &dos, sizeof(dos)) && dos.e_magic == IMAGE_DOS_SIGNATURE &&
           dos.e_lfanew >= 64 && dos.e_lfanew <= 4096 && memory_read(image + dos.e_lfanew, &nt, sizeof(nt)) &&
           nt.Signature == IMAGE_NT_SIGNATURE && nt.FileHeader.Machine == IMAGE_FILE_MACHINE_AMD64 &&
           !(nt.FileHeader.Characteristics & IMAGE_FILE_DLL) && nt.FileHeader.TimeDateStamp == GAME_TIMESTAMP &&
           nt.OptionalHeader.Magic == IMAGE_NT_OPTIONAL_HDR64_MAGIC && nt.OptionalHeader.SizeOfImage == GAME_IMAGE_SIZE &&
           memory_read(image + LW_QUEUE_REMOTE_RVA, queue, sizeof(queue)) && memory_equal(queue, lw_queue_bytes, sizeof(queue)) &&
           memory_read(image + LW_NOTIFY_FAILURE_RVA, failure, sizeof(failure)) && memory_equal(failure, lw_failure_bytes, sizeof(failure)) &&
           memory_read(image + LW_NOTIFY_RELEASE_RVA, release, sizeof(release)) && memory_equal(release, lw_release_bytes, sizeof(release)) &&
           memory_read(image + LW_NOTIFY_VTABLE_RVA + 0x40, &on_failure, sizeof(on_failure)) &&
           on_failure == (uintptr_t)(image + LW_NOTIFY_FAILURE_RVA) &&
           memory_read(image + LW_NOTIFY_VTABLE_RVA + 0x18, &on_release, sizeof(on_release)) &&
           on_release == (uintptr_t)(image + LW_NOTIFY_RELEASE_RVA);
}

static int lw_receiver_install(unsigned char *game_image) {
    if (lw_receiver_faulted) return 0;
    if (lw_queue_original) {
        if (lw_receiver_image != game_image) {
            InterlockedExchange(&lw_receiver_faulted, 1);
            InterlockedExchange(&lw_receiver_ready, 0);
            return 0;
        }
        return fi_livery_remote_ready();
    }
    if (!lw_receiver_image_ok(game_image)) return 0;
    unsigned char *code = VirtualAlloc(0, 32, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!code) return 0;
    memory_copy(code, lw_queue_bytes, sizeof(lw_queue_bytes));
    uintptr_t back = (uintptr_t)(game_image + LW_QUEUE_REMOTE_RVA + sizeof(lw_queue_bytes));
    uintptr_t target = (uintptr_t)lw_queue_remote;
    unsigned char jump[14] = {0xff, 0x25, 0, 0, 0, 0};
    memory_copy(jump + 6, &back, sizeof(back));
    memory_copy(code + sizeof(lw_queue_bytes), jump, sizeof(jump));
    DWORD old_protection, ignored;
    if (!VirtualProtect(code, 32, PAGE_EXECUTE_READ, &old_protection)) {
        VirtualFree(code, 0, MEM_RELEASE); return 0;
    }
    FlushInstructionCache(GetCurrentProcess(), code, 32);
    unsigned char *at = game_image + LW_QUEUE_REMOTE_RVA;
    if (!VirtualProtect(at, sizeof(jump), PAGE_EXECUTE_READWRITE, &old_protection)) {
        VirtualFree(code, 0, MEM_RELEASE); return 0;
    }
    lw_game = game_image;
    lw_receiver_image = game_image;
    lw_queue_original = (LwQueueRemote)(uintptr_t)code;
    memory_copy(jump + 6, &target, sizeof(target));
    memory_copy(at, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), at, sizeof(jump));
    if (!VirtualProtect(at, sizeof(jump), old_protection, &ignored)) {
        /* The jump remains conservative, but an incomplete installation must never authorize
           worlds or be retried as a healthy installation in this process. */
        InterlockedExchange(&lw_receiver_faulted, 1);
        return 0;
    }
    InterlockedExchange(&lw_receiver_ready, 1);
    return 1;
}

void fi_livery_apply(unsigned char *game_image) {
    if (!lw_queue_original) lw_game = game_image;
    int ready = lw_receiver_install(game_image);
    fi_launch_capture(LW_CAPTURE_RECEIVER, ready);
    log_append(ready ? "LIVERY: stock-only remote load boundary installed before cache and parser.\r\n"
                     : "LIVERY: remote load boundary unavailable; online worlds must remain disabled.\r\n");
}
