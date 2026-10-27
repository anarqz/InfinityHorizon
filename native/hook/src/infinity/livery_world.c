/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Liveries in a launcher session (CR-S59, GAME_PLATFORM.md 3a "Liveries"). The server keeps each
   profile's liveries on every ticket and answers the game's own livery calls itself; the hook
   answers none of them. OD-66's live livery channel through the world's relay (MATCHMAKING_API.md
   5a: the hook's PUT/WANT/ACK, its content-addressed 01... ids, its answers from the relay's
   deliveries), the pre-world run of the current car's paint and its per-second watcher are
   retired. What stays here:

   - fi_livery_route: one log line per livery call the game sends (its id and size, never the
     bytes or a player); the call goes on to the server (tier.c passes the six livery calls on
     every ticket).
   - The world's LAN socket: for a world on this PC or this network that the game reached
     directly (no proxy socket to speak from), carstate.c speaks to the relay from a socket of the
     hook's own (the relay binds a private peer's other ports), and the relay's carstate answers
     come back on it. Anything else the relay might still send there is dropped unread.
   - The remote-paint receiver boundary (CR-L29, ONLINE_INTEGRITY.md "Receiver readiness
     boundary"): the hook still checks the game image and sits on the game's queued load of another
     player's livery, and Capture.what 5 reports it installed. The owner lifted its stock-only policy
     with CR-S59 (owner-decisions 2026-10-27 "The stock-only receiver guard lifts with CR-S59"): the
     load goes on to the game, which fetches the livery from the server by id. The server serves
     others only layerless designs while FI_LIVERY_LAYERED is off.

   Nothing here logs the paint's bytes or a player's XUID. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

static unsigned lw_be16(const unsigned char *p) { return (unsigned)p[0] << 8 | p[1]; }
static unsigned lw_be32(const unsigned char *p) {
    return (unsigned)p[0] << 24 | (unsigned)p[1] << 16 | (unsigned)p[2] << 8 | p[3];
}
static unsigned long long lw_be64(const unsigned char *p) {
    return (unsigned long long)lw_be32(p) << 32 | lw_be32(p + 4);
}

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

/* One line per livery call, with its id and size (never the bytes or a player): either PC's log
   shows both the painter's and the viewer's side. */
static void lw_log_call(const char *what, unsigned long long id, unsigned long long bytes) {
    char line[200], digits[24];
    size_t n = 0;
    const char *parts[] = {"LIVERY: ", what, " passed to the server"};
    for (int k = 0; k < 3; ++k) {
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

/* The six livery calls the server answers (CR-S59): class, method, whether the body starts with a
   versioned livery id, whether it is logged (the Storefront lists are asked in a loop). */
static const struct {
    const char *cls, *method, *what;
    int by_id, logged;
} lw_calls[] = {
    {"Forza.WebServices.OnlineProfile", "UpdateLivery", "OnlineProfile.UpdateLivery", 0, 1},
    {"Forza.WebServices.ContentVersioning", "CreateVersionedLivery", "ContentVersioning.CreateVersionedLivery", 0, 1},
    {"Forza.WebServices.ContentVersioning", "RemoveReferenceToVersionedLivery",
     "ContentVersioning.RemoveReferenceToVersionedLivery", 1, 1},
    {"Forza.WebServices.ContentVersioning", "RetrieveVersionedLiveryById", "ContentVersioning.RetrieveVersionedLiveryById",
     1, 1},
    {"Forza.WebServices.Storefront", "GetLiveriesForDrivatar", "Storefront.GetLiveriesForDrivatar", 0, 0},
    {"Forza.WebServices.Storefront", "GetFeaturedLiveriesForRandomCars", "Storefront.GetFeaturedLiveriesForRandomCars",
     0, 0},
};

/* Which of lw_calls the request is (its X-ClassName and X-Method), or -1. */
static int lw_call_of(const wchar_t *headers) {
    char cls[96], method[64];
    if (!headers || !lw_header(headers, L"X-ClassName", cls, sizeof(cls)) ||
        !lw_header(headers, L"X-Method", method, sizeof(method))) {
        return -1;
    }
    for (size_t i = 0; i < sizeof(lw_calls) / sizeof(lw_calls[0]); ++i) {
        if (lw_text_is(cls, lw_calls[i].cls) && lw_text_is(method, lw_calls[i].method)) return (int)i;
    }
    return -1;
}

/* net.c, in a launcher session: 1 the request is one of the livery calls (noted), 0 it is not.
   Either way nothing is answered here: the request goes on to the server. */
int fi_livery_route(const wchar_t *headers, const unsigned char *body, size_t size) {
    int i = lw_call_of(headers);
    if (i < 0) return 0;
    if (lw_calls[i].logged) {
        lw_log_call(lw_calls[i].what, lw_calls[i].by_id && body && size >= 8 ? lw_be64(body) : 0, size);
    }
    return 1;
}

static size_t lw_put_number(char *line, size_t n, unsigned long long v) {
    char digits[24];
    size_t d = 0;
    do {
        digits[d++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    while (d) line[n++] = digits[--d];
    return n;
}

static size_t lw_put_text(char *line, size_t n, const char *text) {
    while (*text) line[n++] = *text++;
    return n;
}

/* net.c, after the server answered a livery call: its HTTP status, the bin/xtsw reply's first two
   words (0 0 when served; the Beta's Xls error 1 is "no such livery") and its size, beside the
   request's line, so either side's log matches the server's (never the paint's bytes). */
void fi_livery_reply(const wchar_t *headers, const unsigned char *request_body, size_t request_size,
                     unsigned status, const unsigned char *reply, size_t reply_size) {
    int i = lw_call_of(headers);
    if (i < 0 || !lw_calls[i].logged) return;
    char line[240];
    size_t n = lw_put_text(line, 0, "LIVERY: ");
    n = lw_put_text(line, n, lw_calls[i].what);
    if (lw_calls[i].by_id && request_body && request_size >= 8) {
        n = lw_put_text(line, n, " id ");
        n = lw_put_number(line, n, lw_be64(request_body));
    }
    n = lw_put_text(line, n, " answered HTTP ");
    n = lw_put_number(line, n, status);
    if (reply && reply_size >= 8) {
        n = lw_put_text(line, n, ", xtsw ");
        n = lw_put_number(line, n, lw_be32(reply));
        n = lw_put_text(line, n, "/");
        n = lw_put_number(line, n, lw_be32(reply + 4));
    }
    n = lw_put_text(line, n, ", bytes ");
    n = lw_put_number(line, n, reply_size);
    n = lw_put_text(line, n, ".\r\n");
    line[n] = 0;
    log_append(line);
}

/* ------------------------------------------------------------- the world's LAN socket */

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

/* the world (a sockaddr_in6, as freeroam.c has it) and whether the own socket speaks to it */
static unsigned char lw_world[28];
static volatile LONG lw_serial; /* a new world, or none: the worker of an older one stops */
static int lw_lan;              /* 1 the own socket is open for this world */
static UINT_PTR lw_socket = LW_INVALID;
/* Leave and terminal control may race. Transfer ownership before closing so they cannot
   close the same descriptor twice (the second close could hit an unrelated reused handle). */
static void lw_close_socket(void) {
    UINT_PTR owned = (UINT_PTR)InterlockedExchangePointer((void *volatile *)&lw_socket, (void *)LW_INVALID);
    if (owned != LW_INVALID) lw_ws.closesocket(owned);
}

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

/* The world's worker: the relay's carstate answers on the own socket. */
static DWORD WINAPI lw_worker(void *argument) {
    LONG serial = (LONG)(intptr_t)argument;
    static unsigned char packet[1024];
    unsigned char from[64];
    while (lw_serial == serial && fi_launch_network_allowed()) {
        UINT_PTR s = lw_socket;
        if (s == LW_INVALID) break;
        int from_size = (int)sizeof(from);
        int n = lw_ws.recvfrom(s, (char *)packet, sizeof(packet), 0, from, &from_size);
        if (lw_serial != serial || !fi_launch_network_allowed()) break;
        if (n >= 2 && lw_be16(packet) == 0x3E91u) {
            azimuth_carstate_received(packet, n); /* the relay passing another car's headlights and roof */
        } else if (n < 0) {
            Sleep(100); /* nothing sent from it yet (unbound), or the timeout */
        }
    }
    return 0;
}

/* freeroam.c: the game joined a world (`remote`: its sockaddr_in6). */
void azimuth_world_lan_entered(const void *remote) {
    if (!fi_launch_protocol() || !remote || !fi_launch_network_allowed()) return;
    const unsigned char *r = remote;
    LONG serial = InterlockedIncrement(&lw_serial);
    lw_lan = 0;
    lw_close_socket();
    memory_copy(lw_world, r, sizeof(lw_world));
    /* FIUP proves game and hook kinds separately on the forwarding socket, and a world elsewhere is
       reached through the joining player's proxy socket: no own socket for either */
    if (azimuth_proxy_proof_mode() || !lw_private(r + 8)) return;
    UINT_PTR s = lw_ws_load() ? lw_ws.socket(23 /* AF_INET6 */, 2 /* SOCK_DGRAM */, 17 /* IPPROTO_UDP */) : LW_INVALID;
    if (s == LW_INVALID) {
        log_append("CARSTATE: no socket for the world's relay on this network; other cars' lights and roofs stay "
                   "as the game has them.\r\n");
        return;
    }
    DWORD off = 0, timeout = 100;
    lw_ws.setsockopt(s, 41 /* IPPROTO_IPV6 */, 27 /* IPV6_V6ONLY */, (const char *)&off, sizeof(off));
    lw_ws.setsockopt(s, 0xffff /* SOL_SOCKET */, 0x1006 /* SO_RCVTIMEO */, (const char *)&timeout, sizeof(timeout));
    lw_socket = s;
    lw_lan = 1;
    if (!fi_launch_network_allowed()) {
        azimuth_world_lan_left();
        return;
    }
    HANDLE t = CreateThread(0, 0, lw_worker, (void *)(intptr_t)serial, 0, 0);
    if (t) CloseHandle(t);
    log_append("CARSTATE: the world's relay on this network is reached from this PC's own socket.\r\n");
}

/* carstate.c: a message to the world's relay from this hook's own socket, for a world on this
   network that the game reached directly (no proxy socket to speak from; the relay binds a
   private peer's other ports). 1 sent. The relay's answers come back on the same socket. */
int azimuth_world_lan_send(const void *data, int size) {
    UINT_PTR s = lw_socket;
    if (!fi_launch_network_allowed() || !lw_lan || s == LW_INVALID) return 0;
    return lw_ws.sendto(s, (const char *)data, size, 0, lw_world, (int)sizeof(lw_world)) == size;
}

/* freeroam.c and launch.c: the game left the world (or online was suspended). */
void azimuth_world_lan_left(void) {
    InterlockedIncrement(&lw_serial);
    lw_lan = 0;
    lw_close_socket();
}

/* ------------------------------------------------------- remote paint load boundary */

#define LW_QUEUE_REMOTE_RVA 0x9ee3d0u
#define LW_NOTIFY_VTABLE_RVA 0x2c62ff0u
#define LW_NOTIFY_FAILURE_RVA 0x1db440u
#define LW_NOTIFY_RELEASE_RVA 0x3224acu
#define LW_CODE_END_RVA 0x1dcc000u
#define LW_CAPTURE_RECEIVER 5u

/* CR-S59: other players' liveries load as the game loads them (the owner lifted the stock-only
   policy v1). The boundary stays installed so a later decision can restore a policy here. */
static int lw_remote_stock_only(void) { return 0; }

/* Win64: manager, versioned id, pointer to one owned notify reference, opaque fourth register.
   The game's id=-1 path does no queue allocation but still releases that owned reference. */
typedef void (*LwQueueRemote)(void *, unsigned long long, void **, uintptr_t);
typedef void (*LwNotifyFailure)(void *);
static unsigned char *lw_game;
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
    /* Mode 0 is the one policy this version reports; since CR-S59 it passes the load through. */
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
                release < (uintptr_t)lw_game + 0x1000 || release >= (uintptr_t)lw_game + LW_CODE_END_RVA) return;
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
    int ready = lw_receiver_install(game_image);
    fi_launch_capture(LW_CAPTURE_RECEIVER, ready);
    log_append(ready ? "LIVERY: remote load boundary installed (other players' liveries load from the server).\r\n"
                     : "LIVERY: remote load boundary unavailable; online worlds must remain disabled.\r\n");
}
