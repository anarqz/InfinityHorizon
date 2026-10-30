/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* FH2Online free roam online.

   FH2's live game ran on a dedicated server: each console joins an Xbox Live
   session (MPSD), is handed the server's SecureDeviceAddress, opens a
   SecureDeviceAssociation to it and talks NerdCore over UDP from its socket
   [::]:5100. A player's dedicated_server.exe plays that server (a relay); the
   ForzaInfinity server answers the session calls. Azimuth answers every piece
   on the console side offline, so this module stands in for them, as the
   fh2-mods "fh2net" probe showed (github.com/conner-mcvay/fh2-mods,
   tools/online, MIT):

     QualityOfService      MeasureQualityOfServiceAsync completes, 20 ms each.
     SecureDeviceAddress   GetLocal / FromBase64String objects answer their
                           base64 (Azimuth's answer an empty string, and XSAPI
                           then never writes the session member).
     SecureDeviceAssociation  CreateAssociationAsync completes with an
                           association whose remote socket address is the
                           world the server put in the session.
     Party                 Register{Game,Match}SessionAsync complete.
     Winsock               the game's ws2_32 imports go to Windows' ws2_32
                           (Azimuth's WSASocketW fails with WSAENETDOWN), and
                           its IPv6 socket is made dual-stack.
     Game patches          the free roam entry does not wait for the server's
                           weather or traffic, which no one sends yet.

   The world's address travels in the session as the server's
   secureDeviceAddress: base64 of "fh2inf:<host>:<port>" (an IPv6 host in
   brackets). [hook] FreeRoamWorld=<host>:<port> (Infinity.cfg) overrides it for tests. A world on
   another PC is reached through azimuth_proxy.c (F3: STUN, punching, the source
   filter); [hook] FreeRoamDirect=1 sends the game straight to it instead. Both are every
   profile's (OD-140: no tier gate).

   [hook] FreeRoam=0 turns all of it off (on by default since fh2online.46). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define FR_AF_INET 2
#define FR_AF_INET6 23
#define FR_IPPROTO_IPV6 41
#define FR_IPV6_V6ONLY 27
#define FR_DEFAULT_PORT 5101
#define FR_QOS_LATENCY_MS 20u
#define FR_QOS_MAX 16
#define FR_SLOTS 32
#define FR_E_BOUNDS ((HRESULT)0x8000000BL)
#define FR_E_NOTIMPL ((HRESULT)0x80004001L)
#define FR_E_ILLEGAL_DELEGATE ((HRESULT)0x80000018L)
#define FR_HSTRING_MAGIC 0x32485346u

typedef struct FrCom FrCom;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(FrCom *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(FrCom *);
    ULONG(STDMETHODCALLTYPE *Release)(FrCom *);
} FrComVtbl;
struct FrCom {
    const FrComVtbl *lpVtbl;
};
typedef HRESULT(STDMETHODCALLTYPE *FrCall6)(void *, uintptr_t, uintptr_t, uintptr_t, uintptr_t,
                                            uintptr_t, uintptr_t);

typedef struct {
    unsigned short family, port; /* port in network order */
    unsigned flow;
    unsigned char address[16];
    unsigned scope;
} FrSockaddrIn6;

static int fr_enabled;
static unsigned char *fr_game;
static wchar_t fr_world_override[128];

static const GUID fr_iid_unknown = {0x00000000, 0x0000, 0x0000, {0xc0, 0, 0, 0, 0, 0, 0, 0x46}};
static const GUID fr_iid_inspectable = {0xaf86e2e0, 0xb12d, 0x4c6a, {0x9c, 0x5a, 0xd7, 0xaa, 0x65, 0x10, 0x1e, 0x90}};
static const GUID fr_iid_agile = {0x94ea2b94, 0xe9cc, 0x49e0, {0xc0, 0xff, 0xee, 0x64, 0xca, 0x8f, 0x5b, 0x90}};
static const GUID fr_iid_async_info = {0x00000036, 0x0000, 0x0000, {0xc0, 0, 0, 0, 0, 0, 0, 0x46}};
static const GUID fr_iid_qos_statics = {0xe58ad357, 0xb957, 0x4cce, {0x92, 0xad, 0x83, 0x30, 0xfb, 0x13, 0x35, 0x2e}};
static const GUID fr_iid_address_statics = {0x02addb0f, 0x4880, 0x4b1e, {0x98, 0xb8, 0xee, 0x63, 0x8a, 0xd8, 0x9b, 0x55}};
static const GUID fr_iid_template_statics = {0x46745a18, 0x697a, 0x47e1, {0xbb, 0x62, 0xcc, 0x71, 0xde, 0x0f, 0xdd, 0x8c}};
static const GUID fr_iid_party_statics = {0xe6110c4c, 0x3660, 0x4230, {0x98, 0x80, 0x20, 0x8b, 0x14, 0x9a, 0x3c, 0xa4}};
/* IIterable<Windows.Xbox.Networking.QualityOfServiceMeasurement> */
static const GUID fr_iid_qos_iterable = {0x36a3a9bb, 0x712a, 0x5297, {0xb5, 0xa0, 0xec, 0xdc, 0x0c, 0xb2, 0x4b, 0x9f}};

/* ---------------------------------------------------------------- helpers */

static void *fr_alloc(size_t size) {
    return HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, size);
}

static void fr_free(void *pointer) {
    if (pointer) {
        HeapFree(GetProcessHeap(), 0, pointer);
    }
}

static int fr_guid_is(REFIID iid, const GUID *guid) {
    return iid && memory_equal(iid, guid, sizeof(GUID));
}

/* IUnknown's family of classic COM interfaces (IMarshal, IWeakReferenceSource...): refused. */
static int fr_is_classic_com(REFIID iid) {
    const unsigned char *id = (const unsigned char *)iid;
    static const unsigned char tail[12] = {0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46};
    return !fr_guid_is(iid, &fr_iid_unknown) && memory_equal(id + 4, tail, sizeof(tail));
}

typedef struct {
    char text[384];
    size_t size;
} FrLine;

/* Declared without an initializer: zeroing the whole buffer would call memset. */
#define FR_LINE(name) \
    FrLine name;      \
    name.size = 0;    \
    name.text[0] = 0

static void fr_text(FrLine *line, const char *text) {
    while (text && *text && line->size + 3 < sizeof(line->text)) {
        line->text[line->size++] = *text++;
    }
    line->text[line->size] = 0;
}

static void fr_wide(FrLine *line, const wchar_t *text, size_t length) {
    for (size_t i = 0; text && i < length && line->size + 3 < sizeof(line->text); ++i) {
        wchar_t c = text[i];
        line->text[line->size++] = c >= 32 && c < 127 ? (char)c : '?';
    }
    line->text[line->size] = 0;
}

static void fr_number(FrLine *line, unsigned long long value) {
    char digits[24];
    unsigned count = 0;
    do {
        digits[count++] = (char)('0' + value % 10);
        value /= 10;
    } while (value);
    char text[24];
    for (unsigned i = 0; i < count; ++i) {
        text[i] = digits[count - 1 - i];
    }
    text[count] = 0;
    fr_text(line, text);
}

static void fr_hex(FrLine *line, unsigned long long value, unsigned digits) {
    static const char hex[] = "0123456789abcdef";
    char text[20];
    for (unsigned i = 0; i < digits && i < 16; ++i) {
        text[i] = hex[(value >> (4 * (digits - 1 - i))) & 15];
    }
    text[digits < 16 ? digits : 16] = 0;
    fr_text(line, text);
}

static void fr_end(FrLine *line) {
    fr_text(line, "\r\n");
    log_append(line->text);
}

static void fr_log(const char *message) {
    FR_LINE(line);
    fr_text(&line, "FREEROAM: ");
    fr_text(&line, message);
    fr_end(&line);
}

static void fr_log_hr(const char *message, HRESULT result) {
    FR_LINE(line);
    fr_text(&line, "FREEROAM: ");
    fr_text(&line, message);
    fr_text(&line, " 0x");
    fr_hex(&line, (unsigned)result, 8);
    fr_end(&line);
}

/* An HSTRING's text (Azimuth's layout, as in azimuth_net.c); null is the empty string. */
static size_t fr_hstring(void *value, wchar_t *out, size_t capacity) {
    struct {
        unsigned magic, length;
        unsigned long long reserved;
        const wchar_t *text;
    } header;
    out[0] = 0;
    if (!value || !memory_read(value, &header, sizeof(header)) || header.magic != FR_HSTRING_MAGIC) {
        return 0;
    }
    size_t length = header.length < capacity - 1 ? header.length : capacity - 1;
    if (length && !memory_read(header.text, out, length * sizeof(wchar_t))) {
        length = 0;
    }
    out[length] = 0;
    return length;
}

static size_t fr_wide_length(const wchar_t *text) {
    size_t length = 0;
    while (text && text[length]) {
        ++length;
    }
    return length;
}

static int fr_wide_equal(const wchar_t *left, const wchar_t *right) {
    while (*left && *left == *right) {
        ++left;
        ++right;
    }
    return *left == *right;
}

static const char fr_base64_digits[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

static void fr_base64_encode(const char *text, wchar_t *out, size_t capacity) {
    size_t length = text_length(text), o = 0;
    for (size_t i = 0; i < length && o + 5 < capacity; i += 3) {
        unsigned v = (unsigned)(unsigned char)text[i] << 16;
        if (i + 1 < length) v |= (unsigned)(unsigned char)text[i + 1] << 8;
        if (i + 2 < length) v |= (unsigned char)text[i + 2];
        out[o++] = (wchar_t)fr_base64_digits[(v >> 18) & 63];
        out[o++] = (wchar_t)fr_base64_digits[(v >> 12) & 63];
        out[o++] = i + 1 < length ? (wchar_t)fr_base64_digits[(v >> 6) & 63] : L'=';
        out[o++] = i + 2 < length ? (wchar_t)fr_base64_digits[v & 63] : L'=';
    }
    out[o] = 0;
}

/* Base64 (standard alphabet, padding optional) into `out`; the decoded length, or -1. */
static int fr_base64_decode(const wchar_t *text, char *out, size_t capacity) {
    unsigned value = 0, bits = 0;
    size_t o = 0;
    for (; *text && *text != L'='; ++text) {
        int digit = -1;
        for (int d = 0; d < 64; ++d) {
            if ((wchar_t)fr_base64_digits[d] == *text) {
                digit = d;
                break;
            }
        }
        if (digit < 0) {
            return -1;
        }
        value = (value << 6) | (unsigned)digit;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            if (o + 1 >= capacity) {
                return -1;
            }
            out[o++] = (char)((value >> bits) & 0xff);
        }
    }
    out[o] = 0;
    return (int)o;
}

/* ------------------------------------------------------------- winsock */
/* Windows' ws2_32, loaded on first use (the mod imports Kernel32 only). */

typedef int(WINAPI *FrBindFn)(UINT_PTR, const void *, int);
typedef int(WINAPI *FrSendToFn)(UINT_PTR, const char *, int, int, const void *, int);
typedef int(WINAPI *FrWsaSendToFn)(UINT_PTR, void *, DWORD, DWORD *, DWORD, const void *, int, void *, void *);
typedef int(WINAPI *FrRecvFromFn)(UINT_PTR, char *, int, int, void *, int *);
typedef int(WINAPI *FrWsaRecvFromFn)(UINT_PTR, void *, DWORD, DWORD *, DWORD *, void *, int *, void *, void *);
typedef int(WINAPI *FrSendFn)(UINT_PTR, const char *, int, int);
typedef int(WINAPI *FrWsaSendFn)(UINT_PTR, void *, DWORD, DWORD *, DWORD, void *, void *);
typedef int(WINAPI *FrRecvFn)(UINT_PTR, char *, int, int);
typedef int(WINAPI *FrWsaRecvFn)(UINT_PTR, void *, DWORD, DWORD *, DWORD *, void *, void *);
typedef int(WINAPI *FrSocketFn)(UINT_PTR);
typedef int(WINAPI *FrShutdownFn)(UINT_PTR, int);
typedef void(WINAPI *FrSetErrorFn)(int);
typedef BOOL(WINAPI *FrCancelIoFn)(HANDLE, LPOVERLAPPED);
typedef int(WINAPI *FrSetOptionFn)(UINT_PTR, int, int, const char *, int);
typedef int(WINAPI *FrPtonFn)(int, const wchar_t *, void *);

static HMODULE fr_ws2;
static FrBindFn fr_real_bind;
static FrSendToFn fr_real_sendto;
static FrWsaSendToFn fr_real_wsa_sendto;
static FrRecvFromFn fr_real_recvfrom;
static FrWsaRecvFromFn fr_real_wsa_recvfrom;
static FrSendFn fr_real_send;
static FrWsaSendFn fr_real_wsa_send;
static FrRecvFn fr_real_recv;
static FrWsaRecvFn fr_real_wsa_recv;
static FrSocketFn fr_real_close;
static FrShutdownFn fr_real_shutdown;
static FrSetErrorFn fr_set_error;
static FrCancelIoFn fr_cancel_io;
static SRWLOCK fr_socket_lock = SRWLOCK_INIT;
#define FR_SOCKET_LIMIT 64u
static UINT_PTR fr_bound_sockets[FR_SOCKET_LIMIT];
/* Set when the car database changed during the session (azimuth_world.c): from then on the
   game's socket sends to [::1]:9, so the world stops hearing this player and drops it, and
   the game, hearing nothing back, leaves the world. Only a copy of the destination is
   changed; the game's buffers and its overlapped sends stay as they are. */
static volatile LONG fr_cut;
static const FrSockaddrIn6 fr_nowhere = {FR_AF_INET6, 0x0900, 0, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1}, 0};
static FrSetOptionFn fr_real_setsockopt;
static FrPtonFn fr_real_pton;
static volatile LONG fr_sockets_state; /* 0 not yet, 1 done, 2 gave up */
static volatile UINT_PTR fr_game_socket;

static int fr_load_ws2(void) {
    if (!fr_ws2) {
        fr_ws2 = LoadLibraryExW(L"ws2_32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (!fr_ws2) {
            return 0;
        }
        fr_real_bind = (FrBindFn)(uintptr_t)GetProcAddress(fr_ws2, "bind");
        fr_real_sendto = (FrSendToFn)(uintptr_t)GetProcAddress(fr_ws2, "sendto");
        fr_real_wsa_sendto = (FrWsaSendToFn)(uintptr_t)GetProcAddress(fr_ws2, "WSASendTo");
        fr_real_recvfrom = (FrRecvFromFn)(uintptr_t)GetProcAddress(fr_ws2, "recvfrom");
        fr_real_wsa_recvfrom = (FrWsaRecvFromFn)(uintptr_t)GetProcAddress(fr_ws2, "WSARecvFrom");
        fr_real_send = (FrSendFn)(uintptr_t)GetProcAddress(fr_ws2, "send");
        fr_real_wsa_send = (FrWsaSendFn)(uintptr_t)GetProcAddress(fr_ws2, "WSASend");
        fr_real_recv = (FrRecvFn)(uintptr_t)GetProcAddress(fr_ws2, "recv");
        fr_real_wsa_recv = (FrWsaRecvFn)(uintptr_t)GetProcAddress(fr_ws2, "WSARecv");
        fr_real_close = (FrSocketFn)(uintptr_t)GetProcAddress(fr_ws2, "closesocket");
        fr_real_shutdown = (FrShutdownFn)(uintptr_t)GetProcAddress(fr_ws2, "shutdown");
        fr_set_error = (FrSetErrorFn)(uintptr_t)GetProcAddress(fr_ws2, "WSASetLastError");
        fr_cancel_io = (FrCancelIoFn)(uintptr_t)GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "CancelIoEx");
        fr_real_setsockopt = (FrSetOptionFn)(uintptr_t)GetProcAddress(fr_ws2, "setsockopt");
        fr_real_pton = (FrPtonFn)(uintptr_t)GetProcAddress(fr_ws2, "InetPtonW");
    }
    return fr_real_bind && fr_real_setsockopt && fr_real_pton && fr_real_sendto && fr_real_wsa_sendto &&
           fr_real_recvfrom && fr_real_wsa_recvfrom && fr_real_send && fr_real_wsa_send && fr_real_recv &&
           fr_real_wsa_recv && fr_real_close && fr_real_shutdown && fr_set_error && fr_cancel_io;
}

/* The game's socket is IPv6 ([::]:5100, the manifest's SecureConsoleSocket) and Windows makes
   those IPv6-only: clearing IPV6_V6ONLY lets it reach IPv4 worlds as ::ffff:a.b.c.d. */
static int WINAPI fr_bind(UINT_PTR socket, const void *address, int length) {
    unsigned short family = 0;
    unsigned slot = FR_SOCKET_LIMIT;
    AcquireSRWLockExclusive(&fr_socket_lock);
    if (address && length >= 2) memory_read(address, &family, sizeof(family));
    if (family == FR_AF_INET6 || family == 2 /* AF_INET */) {
        if (!fi_launch_network_allowed()) {
            ReleaseSRWLockExclusive(&fr_socket_lock);
            fr_set_error(10050); /* WSAENETDOWN: never replace the quarantined world socket */
            return -1;
        }
        slot = 0;
        while (slot < FR_SOCKET_LIMIT && fr_bound_sockets[slot] && fr_bound_sockets[slot] != socket) ++slot;
        if (slot == FR_SOCKET_LIMIT) {
            ReleaseSRWLockExclusive(&fr_socket_lock);
            fr_set_error(10055); /* WSAENOBUFS: never leave a bound socket outside quarantine */
            return -1;
        }
    }
    if (family == FR_AF_INET6) {
        DWORD off = 0;
        fr_real_setsockopt(socket, FR_IPPROTO_IPV6, FR_IPV6_V6ONLY, (const char *)&off, sizeof(off));
    }
    int result = fr_real_bind(socket, address, length);
    if (result == 0) {
        if (slot < FR_SOCKET_LIMIT) fr_bound_sockets[slot] = socket;
        if (family == FR_AF_INET6) fr_game_socket = socket;
    }
    ReleaseSRWLockExclusive(&fr_socket_lock);
    FR_LINE(line);
    fr_text(&line, "FREEROAM: game socket bound, family ");
    fr_number(&line, family);
    fr_text(&line, result == 0 ? " (dual-stack)." : " FAILED.");
    fr_end(&line);
    return result;
}

static int fr_is_cut(UINT_PTR socket) {
    return fr_cut && socket == fr_game_socket;
}

static int fr_is_suspended(UINT_PTR socket) {
    (void)socket;
    /* All game import traffic is denied, including an unbound send or a late new socket. */
    if (!fi_launch_network_allowed()) {
        fr_set_error(10050); /* WSAENETDOWN */
        return 1;
    }
    return 0;
}

static int WINAPI fr_close(UINT_PTR socket) {
    AcquireSRWLockExclusive(&fr_socket_lock);
    if (fr_game_socket == socket) fr_game_socket = 0;
    for (unsigned i = 0; i < FR_SOCKET_LIMIT; ++i) {
        if (fr_bound_sockets[i] == socket) fr_bound_sockets[i] = 0;
    }
    int result = fr_real_close(socket);
    ReleaseSRWLockExclusive(&fr_socket_lock);
    return result;
}

static int WINAPI fr_sendto(UINT_PTR socket, const char *data, int size, int flags, const void *to, int to_size) {
    if (fr_is_suspended(socket)) return -1;
    if (fr_is_cut(socket)) {
        return fr_real_sendto(socket, data, size, flags, &fr_nowhere, sizeof(fr_nowhere));
    }
    return fr_real_sendto(socket, data, size, flags, to, to_size);
}

static int WINAPI fr_wsa_sendto(UINT_PTR socket, void *buffers, DWORD count, DWORD *sent, DWORD flags,
                                const void *to, int to_size, void *overlapped, void *completion) {
    if (fr_is_suspended(socket)) return -1;
    if (fr_is_cut(socket)) {
        return fr_real_wsa_sendto(socket, buffers, count, sent, flags, &fr_nowhere, sizeof(fr_nowhere), overlapped,
                                  completion);
    }
    return fr_real_wsa_sendto(socket, buffers, count, sent, flags, to, to_size, overlapped, completion);
}

static int WINAPI fr_recvfrom(UINT_PTR socket, char *data, int size, int flags, void *from, int *from_size) {
    if (fr_is_suspended(socket)) return -1;
    int result = fr_real_recvfrom(socket, data, size, flags, from, from_size);
    return fr_is_suspended(socket) ? -1 : result;
}

static int WINAPI fr_wsa_recvfrom(UINT_PTR socket, void *buffers, DWORD count, DWORD *received, DWORD *flags,
                                 void *from, int *from_size, void *overlapped, void *completion) {
    /* Serialize asynchronous registration with cancellation. Never replace WSA_IO_PENDING
       with an immediate failure: its caller must retain buffers until the cancellation completes. */
    if (overlapped) AcquireSRWLockExclusive(&fr_socket_lock);
    if (fr_is_suspended(socket)) {
        if (overlapped) ReleaseSRWLockExclusive(&fr_socket_lock);
        return -1;
    }
    int result = fr_real_wsa_recvfrom(socket, buffers, count, received, flags, from, from_size, overlapped, completion);
    if (overlapped) {
        ReleaseSRWLockExclusive(&fr_socket_lock);
        return result;
    }
    return fr_is_suspended(socket) ? -1 : result;
}

static int WINAPI fr_send(UINT_PTR socket, const char *data, int size, int flags) {
    return fr_is_suspended(socket) ? -1 : fr_real_send(socket, data, size, flags);
}

static int WINAPI fr_wsa_send(UINT_PTR socket, void *buffers, DWORD count, DWORD *sent, DWORD flags,
                             void *overlapped, void *completion) {
    return fr_is_suspended(socket) ? -1 : fr_real_wsa_send(socket, buffers, count, sent, flags, overlapped, completion);
}

static int WINAPI fr_recv(UINT_PTR socket, char *data, int size, int flags) {
    if (fr_is_suspended(socket)) return -1;
    int result = fr_real_recv(socket, data, size, flags);
    return fr_is_suspended(socket) ? -1 : result;
}

static int WINAPI fr_wsa_recv(UINT_PTR socket, void *buffers, DWORD count, DWORD *received, DWORD *flags,
                             void *overlapped, void *completion) {
    if (overlapped) AcquireSRWLockExclusive(&fr_socket_lock);
    if (fr_is_suspended(socket)) {
        if (overlapped) ReleaseSRWLockExclusive(&fr_socket_lock);
        return -1;
    }
    int result = fr_real_wsa_recv(socket, buffers, count, received, flags, overlapped, completion);
    if (overlapped) {
        ReleaseSRWLockExclusive(&fr_socket_lock);
        return result;
    }
    return fr_is_suspended(socket) ? -1 : result;
}

/* Shutdown retains game ownership of its handle (no descriptor reuse/double close). Existing
   receives are released by Winsock; all subsequent sends/receives and binds fail closed. */
void azimuth_freeroam_suspend(void) {
    AcquireSRWLockExclusive(&fr_socket_lock);
    if (fr_real_shutdown) {
        for (unsigned i = 0; i < FR_SOCKET_LIMIT; ++i) {
            if (fr_bound_sockets[i]) {
                fr_cancel_io((HANDLE)fr_bound_sockets[i], NULL);
                fr_real_shutdown(fr_bound_sockets[i], 2 /* SD_BOTH */);
            }
        }
    }
    ReleaseSRWLockExclusive(&fr_socket_lock);
}

/* Cuts this game off the world it is in (see fr_cut). */
void azimuth_freeroam_cut(void) {
    if (!InterlockedExchange(&fr_cut, 1)) {
        fr_log("the game's world traffic is cut (the car database changed).");
    }
}

/* Points every ws2_32 import of the game at Windows' ws2_32 (bind at fr_bind, sendto and
   WSASendTo through fr_sendto / fr_wsa_sendto). Returns 1 when
   done, 0 while Azimuth has not bound the imports yet, -1 when it cannot be done. */
static int fr_install_sockets(void) {
    if (!fr_load_ws2()) {
        return -1;
    }
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    if (!memory_read(fr_game, &dos, sizeof(dos)) || !memory_read(fr_game + dos.e_lfanew, &nt, sizeof(nt))) {
        return -1;
    }
    IMAGE_DATA_DIRECTORY directory = nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!directory.VirtualAddress) {
        return -1;
    }
    unsigned patched = 0, from_azimuth = 0;
    unsigned char *azimuth = (unsigned char *)GetModuleHandleW(NULL);
    for (IMAGE_IMPORT_DESCRIPTOR *d = (IMAGE_IMPORT_DESCRIPTOR *)(fr_game + directory.VirtualAddress);
         d->Name; ++d) {
        if (!text_equal_ignore_case((const char *)(fr_game + d->Name), "ws2_32.dll")) {
            continue;
        }
        ULONGLONG *names = (ULONGLONG *)(fr_game + (d->OriginalFirstThunk ? d->OriginalFirstThunk : d->FirstThunk));
        void **table = (void **)(fr_game + d->FirstThunk);
        for (unsigned k = 0; names[k]; ++k) {
            int by_ordinal = IMAGE_SNAP_BY_ORDINAL64(names[k]) != 0;
            if ((ULONGLONG)(uintptr_t)table[k] == names[k] || !table[k]) {
                return 0; /* not bound yet */
            }
            const char *name = by_ordinal ? 0 : (const char *)((IMAGE_IMPORT_BY_NAME *)(fr_game + names[k]))->Name;
            void *real = by_ordinal
                             ? (void *)(uintptr_t)GetProcAddress(fr_ws2, (LPCSTR)(uintptr_t)IMAGE_ORDINAL64(names[k]))
                             : (void *)(uintptr_t)GetProcAddress(fr_ws2, name);
            if (!real) {
                continue;
            }
            if (real == (void *)(uintptr_t)fr_real_bind) {
                real = (void *)(uintptr_t)fr_bind;
            } else if (real == (void *)(uintptr_t)fr_real_sendto) {
                real = (void *)(uintptr_t)fr_sendto;
            } else if (real == (void *)(uintptr_t)fr_real_wsa_sendto) {
                real = (void *)(uintptr_t)fr_wsa_sendto;
            } else if (real == (void *)(uintptr_t)fr_real_recvfrom) {
                real = (void *)(uintptr_t)fr_recvfrom;
            } else if (real == (void *)(uintptr_t)fr_real_wsa_recvfrom) {
                real = (void *)(uintptr_t)fr_wsa_recvfrom;
            } else if (real == (void *)(uintptr_t)fr_real_send) {
                real = (void *)(uintptr_t)fr_send;
            } else if (real == (void *)(uintptr_t)fr_real_wsa_send) {
                real = (void *)(uintptr_t)fr_wsa_send;
            } else if (real == (void *)(uintptr_t)fr_real_recv) {
                real = (void *)(uintptr_t)fr_recv;
            } else if (real == (void *)(uintptr_t)fr_real_wsa_recv) {
                real = (void *)(uintptr_t)fr_wsa_recv;
            } else if (real == (void *)(uintptr_t)fr_real_close) {
                real = (void *)(uintptr_t)fr_close;
            }
            if (table[k] == real) {
                continue;
            }
            MEMORY_BASIC_INFORMATION info;
            if (VirtualQuery(table[k], &info, sizeof(info)) && info.AllocationBase == azimuth) {
                ++from_azimuth;
            }
            DWORD old_protection, ignored;
            if (VirtualProtect(&table[k], sizeof(void *), PAGE_READWRITE, &old_protection)) {
                table[k] = real;
                VirtualProtect(&table[k], sizeof(void *), old_protection, &ignored);
                ++patched;
            }
        }
    }
    FR_LINE(line);
    fr_text(&line, "FREEROAM: ");
    fr_number(&line, patched);
    fr_text(&line, " ws2_32 imports of the game go to Windows' ws2_32 (");
    fr_number(&line, from_azimuth);
    fr_text(&line, " were Azimuth's).");
    fr_end(&line);
    return 1;
}

static void fr_sockets(void) {
    if (fr_sockets_state) {
        return;
    }
    int result = fr_install_sockets();
    if (result) {
        if (!InterlockedExchange(&fr_sockets_state, result > 0 ? 1 : 2) && result < 0) {
            fr_log("the game's Winsock imports could not be redirected.");
        }
    }
}

/* ------------------------------------------------------ completed operations */
/* IAsyncOperation<T> / IAsyncAction already completed: the operation interface at +0,
   IAsyncInfo at +8. Every interface the caller asks for except IAsyncInfo and classic COM
   is the operation itself (the IIDs are generic instances this build does not spell out). */

typedef struct FrOperation FrOperation;
struct FrOperation {
    void *const *operation;
    void *const *info;
    volatile LONG references, handler_set;
    unsigned id;
    FrCom *result; /* null for an action */
    FrCom *handler;
};
static volatile LONG fr_operation_ids;

static FrOperation *fr_op(void *self, int info) {
    return (FrOperation *)((unsigned char *)self - (info ? sizeof(void *) : 0));
}

static HRESULT fr_op_query(FrOperation *op, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (fr_guid_is(iid, &fr_iid_async_info)) {
        *out = &op->info;
    } else if (fr_is_classic_com(iid)) {
        return E_NOINTERFACE;
    } else {
        *out = &op->operation;
    }
    InterlockedIncrement(&op->references);
    return S_OK;
}
static ULONG fr_op_release(FrOperation *op) {
    LONG left = InterlockedDecrement(&op->references);
    if (!left) {
        if (op->handler) {
            op->handler->lpVtbl->Release(op->handler);
        }
        if (op->result) {
            op->result->lpVtbl->Release(op->result);
        }
        fr_free(op);
    }
    return (ULONG)left;
}
static HRESULT STDMETHODCALLTYPE fr_opa_query(void *s, REFIID i, void **o) { return fr_op_query(fr_op(s, 0), i, o); }
static ULONG STDMETHODCALLTYPE fr_opa_add_ref(void *s) { return (ULONG)InterlockedIncrement(&fr_op(s, 0)->references); }
static ULONG STDMETHODCALLTYPE fr_opa_release(void *s) { return fr_op_release(fr_op(s, 0)); }
static HRESULT STDMETHODCALLTYPE fr_opi_query(void *s, REFIID i, void **o) { return fr_op_query(fr_op(s, 1), i, o); }
static ULONG STDMETHODCALLTYPE fr_opi_add_ref(void *s) { return (ULONG)InterlockedIncrement(&fr_op(s, 1)->references); }
static ULONG STDMETHODCALLTYPE fr_opi_release(void *s) { return fr_op_release(fr_op(s, 1)); }

static HRESULT STDMETHODCALLTYPE fr_iids(void *self, ULONG *count, void **iids) {
    (void)self;
    if (count) *count = 0;
    if (iids) *iids = 0;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_class_name(void *self, void **name) {
    (void)self;
    if (name) *name = 0;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_trust(void *self, int *level) {
    (void)self;
    if (level) *level = 0;
    return S_OK;
}

typedef HRESULT(STDMETHODCALLTYPE *FrInvokeFn)(FrCom *, void *, int);
static HRESULT STDMETHODCALLTYPE fr_op_put_completed(void *self, FrCom *handler) {
    FrOperation *op = fr_op(self, 0);
    if (InterlockedCompareExchange(&op->handler_set, 1, 0)) {
        return FR_E_ILLEGAL_DELEGATE;
    }
    if (handler) {
        handler->lpVtbl->AddRef(handler);
        op->handler = handler;
        /* Already complete: WinRT invokes the handler right away. */
        ((FrInvokeFn)((void *const *)(const void *)handler->lpVtbl)[3])(handler, &op->operation, 1);
    }
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_op_get_completed(void *self, FrCom **handler) {
    FrOperation *op = fr_op(self, 0);
    if (!handler) {
        return E_POINTER;
    }
    *handler = op->handler;
    if (op->handler) {
        op->handler->lpVtbl->AddRef(op->handler);
    }
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_op_get_results(void *self, FrCom **result) {
    FrOperation *op = fr_op(self, 0);
    if (!op->result) {
        return S_OK; /* IAsyncAction::GetResults takes no argument */
    }
    if (!result) {
        return E_POINTER;
    }
    op->result->lpVtbl->AddRef(op->result);
    *result = op->result;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_op_get_id(void *self, unsigned *id) {
    if (!id) return E_POINTER;
    *id = fr_op(self, 1)->id;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_op_get_status(void *self, int *status) {
    (void)self;
    if (!status) return E_POINTER;
    *status = 1; /* AsyncStatus::Completed */
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_op_get_error(void *self, HRESULT *error) {
    (void)self;
    if (!error) return E_POINTER;
    *error = S_OK;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_op_nothing(void *self) {
    (void)self;
    return S_OK;
}
static void *const fr_op_vtable[9] = {
    (void *)(uintptr_t)fr_opa_query,        (void *)(uintptr_t)fr_opa_add_ref,
    (void *)(uintptr_t)fr_opa_release,      (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,       (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_op_put_completed, (void *)(uintptr_t)fr_op_get_completed,
    (void *)(uintptr_t)fr_op_get_results};
static void *const fr_op_info_vtable[11] = {
    (void *)(uintptr_t)fr_opi_query,     (void *)(uintptr_t)fr_opi_add_ref,
    (void *)(uintptr_t)fr_opi_release,   (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,    (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_op_get_id,     (void *)(uintptr_t)fr_op_get_status,
    (void *)(uintptr_t)fr_op_get_error,  (void *)(uintptr_t)fr_op_nothing,
    (void *)(uintptr_t)fr_op_nothing};

/* Takes over a reference to `result` (null makes an IAsyncAction). */
static HRESULT fr_completed(FrCom *result, void **out) {
    if (!out) {
        if (result) result->lpVtbl->Release(result);
        return E_POINTER;
    }
    *out = 0;
    FrOperation *op = fr_alloc(sizeof(*op));
    if (!op) {
        if (result) result->lpVtbl->Release(result);
        return E_OUTOFMEMORY;
    }
    op->operation = fr_op_vtable;
    op->info = fr_op_info_vtable;
    op->references = 1;
    op->id = (unsigned)InterlockedIncrement(&fr_operation_ids);
    op->result = result;
    *out = &op->operation;
    return S_OK;
}

/* ------------------------------------------------------------- wrappers */
/* A wrapper around one of Azimuth's objects: answers its own interface (iid) and the
   IInspectable family itself, forwards every other slot, and replaces a few. Methods past
   IInspectable take at most six integer or pointer arguments in these interfaces. */

typedef struct {
    void *const *lpVtbl;
    volatile LONG references;
    FrCom *inner;
    const GUID *iid;
    wchar_t base64[96]; /* addresses: what the object was made from */
} FrWrap;

#define FR_FORWARD(slot)                                                                           \
    static HRESULT STDMETHODCALLTYPE fr_forward##slot(FrWrap *self, uintptr_t a, uintptr_t b,      \
                                                      uintptr_t c, uintptr_t d, uintptr_t e,       \
                                                      uintptr_t f) {                               \
        FrCall6 call = (FrCall6)((void *const *)(const void *)self->inner->lpVtbl)[slot];          \
        return call(self->inner, a, b, c, d, e, f);                                                \
    }
FR_FORWARD(3) FR_FORWARD(4) FR_FORWARD(5) FR_FORWARD(6) FR_FORWARD(7) FR_FORWARD(8)
FR_FORWARD(9) FR_FORWARD(10) FR_FORWARD(11) FR_FORWARD(12) FR_FORWARD(13) FR_FORWARD(14)
FR_FORWARD(15) FR_FORWARD(16) FR_FORWARD(17) FR_FORWARD(18) FR_FORWARD(19) FR_FORWARD(20)
FR_FORWARD(21) FR_FORWARD(22) FR_FORWARD(23) FR_FORWARD(24) FR_FORWARD(25) FR_FORWARD(26)
FR_FORWARD(27) FR_FORWARD(28) FR_FORWARD(29) FR_FORWARD(30) FR_FORWARD(31)
#undef FR_FORWARD

static HRESULT STDMETHODCALLTYPE fr_wrap_query(FrWrap *self, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (fr_guid_is(iid, &fr_iid_unknown) || fr_guid_is(iid, &fr_iid_inspectable) ||
        fr_guid_is(iid, &fr_iid_agile) || (self->iid && fr_guid_is(iid, self->iid))) {
        InterlockedIncrement(&self->references);
        *out = self;
        return S_OK;
    }
    HRESULT result = self->inner->lpVtbl->QueryInterface(self->inner, iid, out);
    if (SUCCEEDED(result) && *out == (void *)self->inner) {
        /* The same interface of the inner object: hand out the wrapper instead. */
        self->inner->lpVtbl->Release(self->inner);
        InterlockedIncrement(&self->references);
        *out = self;
    }
    return result;
}
static ULONG STDMETHODCALLTYPE fr_wrap_add_ref(FrWrap *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}
static ULONG STDMETHODCALLTYPE fr_wrap_release(FrWrap *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        self->inner->lpVtbl->Release(self->inner);
        fr_free(self);
    }
    return (ULONG)left;
}

static void fr_vtable_base(void **vtable) {
    static void *const forwards[FR_SLOTS] = {
        (void *)(uintptr_t)fr_wrap_query, (void *)(uintptr_t)fr_wrap_add_ref,
        (void *)(uintptr_t)fr_wrap_release, (void *)(uintptr_t)fr_forward3,
        (void *)(uintptr_t)fr_forward4, (void *)(uintptr_t)fr_forward5,
        (void *)(uintptr_t)fr_forward6, (void *)(uintptr_t)fr_forward7,
        (void *)(uintptr_t)fr_forward8, (void *)(uintptr_t)fr_forward9,
        (void *)(uintptr_t)fr_forward10, (void *)(uintptr_t)fr_forward11,
        (void *)(uintptr_t)fr_forward12, (void *)(uintptr_t)fr_forward13,
        (void *)(uintptr_t)fr_forward14, (void *)(uintptr_t)fr_forward15,
        (void *)(uintptr_t)fr_forward16, (void *)(uintptr_t)fr_forward17,
        (void *)(uintptr_t)fr_forward18, (void *)(uintptr_t)fr_forward19,
        (void *)(uintptr_t)fr_forward20, (void *)(uintptr_t)fr_forward21,
        (void *)(uintptr_t)fr_forward22, (void *)(uintptr_t)fr_forward23,
        (void *)(uintptr_t)fr_forward24, (void *)(uintptr_t)fr_forward25,
        (void *)(uintptr_t)fr_forward26, (void *)(uintptr_t)fr_forward27,
        (void *)(uintptr_t)fr_forward28, (void *)(uintptr_t)fr_forward29,
        (void *)(uintptr_t)fr_forward30, (void *)(uintptr_t)fr_forward31};
    memory_copy(vtable, forwards, sizeof(forwards));
}

/* Takes over the caller's reference to `inner`; on failure the caller keeps it. */
static FrWrap *fr_wrap(FrCom *inner, void *const *vtable, const GUID *iid) {
    FrWrap *wrap = fr_alloc(sizeof(*wrap));
    if (wrap) {
        wrap->lpVtbl = vtable;
        wrap->references = 1;
        wrap->inner = inner;
        wrap->iid = iid;
    }
    return wrap;
}

/* --------------------------------------------------------- the world address */

static FrSockaddrIn6 fr_world_address(const wchar_t *base64) {
    FrSockaddrIn6 address;
    address.family = FR_AF_INET6;
    address.port = 0;
    address.flow = 0;
    address.scope = 0;
    for (unsigned i = 0; i < 16; ++i) {
        ((volatile unsigned char *)address.address)[i] = 0;
    }
    wchar_t host[96];
    unsigned port = FR_DEFAULT_PORT;
    const char *source = "default";
    char decoded[128];
    host[0] = 0;
    const wchar_t *spec = 0;
    wchar_t spec_buffer[128];
    /* [hook] FreeRoamWorld is every profile's (OD-140: no tier gate, in any protocol) */
    if (fr_world_override[0]) {
        spec = fr_world_override;
        source = "[hook] FreeRoamWorld";
    } else if (base64 && base64[0] && fr_base64_decode(base64, decoded, sizeof(decoded)) > 7 &&
               memory_equal(decoded, "fh2inf:", 7)) {
        size_t i = 0;
        for (; decoded[7 + i] && i + 1 < ARRAY_COUNT(spec_buffer); ++i) {
            spec_buffer[i] = (wchar_t)(unsigned char)decoded[7 + i];
        }
        spec_buffer[i] = 0;
        spec = spec_buffer;
        source = "session";
    }
    if (spec) {
        /* host:port, [v6]:port, or a bare host */
        size_t length = fr_wide_length(spec), start = 0, end = length, colon = (size_t)-1;
        if (spec[0] == L'[') {
            start = 1;
            for (end = 1; end < length && spec[end] != L']'; ++end) {
            }
            if (end + 1 < length && spec[end + 1] == L':') colon = end + 1;
        } else {
            for (size_t i = 0; i < length; ++i) {
                if (spec[i] == L':') {
                    colon = colon == (size_t)-1 ? i : (size_t)-2; /* two colons: bare IPv6 */
                }
            }
            if (colon == (size_t)-2) colon = (size_t)-1;
            else if (colon != (size_t)-1) end = colon;
        }
        if (end - start < ARRAY_COUNT(host)) {
            memory_copy(host, spec + start, (end - start) * sizeof(wchar_t));
            host[end - start] = 0;
        }
        if (colon != (size_t)-1) {
            unsigned value = 0;
            for (size_t i = colon + 1; i < length && spec[i] >= L'0' && spec[i] <= L'9'; ++i) {
                value = value * 10 + (unsigned)(spec[i] - L'0');
            }
            if (value && value < 65536) port = value;
        }
    }
    unsigned char v4[4];
    if (!host[0] || !fr_load_ws2()) {
        address.address[15] = 1; /* ::1, a dedicated_server on this PC */
        source = host[0] ? "no ws2_32; default" : source;
    } else if (fr_real_pton(FR_AF_INET, host, v4) == 1) {
        address.address[10] = address.address[11] = 0xff; /* ::ffff:a.b.c.d */
        memory_copy(address.address + 12, v4, 4);
    } else if (fr_real_pton(FR_AF_INET6, host, address.address) != 1) {
        for (unsigned i = 0; i < 16; ++i) {
            ((volatile unsigned char *)address.address)[i] = 0;
        }
        address.address[15] = 1;
        source = "unreadable host; default";
    }
    address.port = (unsigned short)(((port & 0xff) << 8) | (port >> 8));
    if(fi_launch_protocol()) {
        log_append("FREEROAM: session destination resolved (address omitted).\r\n");
        return address;
    }
    FR_LINE(line);
    fr_text(&line, "FREEROAM: world [");
    fr_wide(&line, host[0] ? host : L"::1", host[0] ? fr_wide_length(host) : 3);
    fr_text(&line, "]:");
    fr_number(&line, port);
    fr_text(&line, " (from the ");
    fr_text(&line, source);
    fr_text(&line, ").");
    fr_end(&line);
    return address;
}

/* -------------------------------------------------------- the association */
/* The game (SecureIPResolver, 0x1417A0FF0) reads slot 9, GetRemoteSocketAddressBytes(length,
   bytes), and calls slot 8, DestroyAsync, when it leaves (0x14179EA70). Other slots are
   logged and refused until a capture shows the game using them. */

typedef struct {
    void *const *lpVtbl;
    volatile LONG references;
    FrSockaddrIn6 remote;
} FrAssociation;

static HRESULT STDMETHODCALLTYPE fr_assoc_query(FrAssociation *self, REFIID iid, void **out) {
    if (!out) return E_POINTER;
    *out = 0;
    if (fr_is_classic_com(iid)) return E_NOINTERFACE;
    InterlockedIncrement(&self->references);
    *out = self;
    return S_OK;
}
static ULONG STDMETHODCALLTYPE fr_assoc_add_ref(FrAssociation *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}
static ULONG STDMETHODCALLTYPE fr_assoc_release(FrAssociation *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) fr_free(self);
    return (ULONG)left;
}
static HRESULT STDMETHODCALLTYPE fr_assoc_destroy(FrAssociation *self, void **out) {
    (void)self;
    fr_log("association DestroyAsync -> completed.");
    azimuth_proxy_stop();
    fi_follow_left(); /* no lobby type until the game's next OpenConnection */
    azimuth_world_left();
    azimuth_world_lan_left();
    if (fi_launch_protocol()) {
        /* OD-54/OD-57: the launcher hosts; it closes the world when the game leaves free roam */
        azimuth_world_launcher_host(0);
        fi_launch_event(FI_EVENT_FREE_ROAM_LEFT, "");
    }
    return fr_completed(0, out);
}
static HRESULT STDMETHODCALLTYPE fr_assoc_remote_bytes(FrAssociation *self, UINT32 length, BYTE *bytes) {
    if (!bytes || length < sizeof(self->remote)) {
        return E_INVALIDARG;
    }
    for (UINT32 i = 0; i < length; ++i) {
        ((volatile BYTE *)bytes)[i] = 0;
    }
    memory_copy(bytes, &self->remote, sizeof(self->remote));
    return S_OK;
}
static volatile LONG fr_assoc_unknown_logged;
#define FR_ASSOC_UNKNOWN(slot)                                                                     \
    static HRESULT STDMETHODCALLTYPE fr_assoc_slot##slot(FrAssociation *self) {                    \
        (void)self;                                                                                \
        if (InterlockedIncrement(&fr_assoc_unknown_logged) <= 20) {                                \
            fr_log("association slot " #slot " called; not implemented.");                         \
        }                                                                                          \
        return FR_E_NOTIMPL;                                                                       \
    }
FR_ASSOC_UNKNOWN(6) FR_ASSOC_UNKNOWN(7) FR_ASSOC_UNKNOWN(10) FR_ASSOC_UNKNOWN(11)
FR_ASSOC_UNKNOWN(12) FR_ASSOC_UNKNOWN(13) FR_ASSOC_UNKNOWN(14) FR_ASSOC_UNKNOWN(15)
#undef FR_ASSOC_UNKNOWN
static void *const fr_assoc_vtable[16] = {
    (void *)(uintptr_t)fr_assoc_query,    (void *)(uintptr_t)fr_assoc_add_ref,
    (void *)(uintptr_t)fr_assoc_release,  (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,     (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_assoc_slot6,    (void *)(uintptr_t)fr_assoc_slot7,
    (void *)(uintptr_t)fr_assoc_destroy,  (void *)(uintptr_t)fr_assoc_remote_bytes,
    (void *)(uintptr_t)fr_assoc_slot10,   (void *)(uintptr_t)fr_assoc_slot11,
    (void *)(uintptr_t)fr_assoc_slot12,   (void *)(uintptr_t)fr_assoc_slot13,
    (void *)(uintptr_t)fr_assoc_slot14,   (void *)(uintptr_t)fr_assoc_slot15};

/* Template slot 8: CreateAssociationAsync(remote SecureDeviceAddress, behavior, out). */
static HRESULT STDMETHODCALLTYPE fr_template_create(FrWrap *self, FrCom *remote, int behavior, void **out) {
    (void)self;
    (void)behavior;
    if (!fi_launch_network_allowed()) {
        if (out) *out = 0;
        return E_ACCESSDENIED;
    }
    wchar_t base64[128];
    base64[0] = 0;
    if (remote) {
        typedef HRESULT(STDMETHODCALLTYPE * Base64Fn)(FrCom *, void **);
        void *text = 0;
        Base64Fn get = (Base64Fn)((void *const *)(const void *)remote->lpVtbl)[10];
        if (SUCCEEDED(get(remote, &text))) {
            fr_hstring(text, base64, ARRAY_COUNT(base64));
        }
        /* The HSTRING is the caller's to free; Azimuth's strings are reference-less here, as in
           azimuth_net.c, so it is left alone. */
    }
    FrAssociation *association = fr_alloc(sizeof(*association));
    if (!association) {
        return E_OUTOFMEMORY;
    }
    association->lpVtbl = fr_assoc_vtable;
    association->references = 1;
    association->remote = fr_world_address(base64);
    if (!fi_launch_network_allowed()) {
        fr_assoc_release(association);
        if (out) *out = 0;
        return E_ACCESSDENIED;
    }
    /* Role comes from the server's destination, before a joining proxy rewrites it to loopback. */
    FrSockaddrIn6 original_remote = association->remote;
    if (azimuth_proxy_start(&association->remote, &association->remote) < 0) {
        fr_assoc_release(association);
        if (out) *out = 0;
        return E_ACCESSDENIED;
    }
    azimuth_world_lan_entered(&association->remote); /* carstate's own socket for a world on this network */
    if (fi_launch_protocol()) {
        /* OD-54: the world is the launcher's relay on loopback when this PC hosts it (the server
           answers the host 127.0.0.1:<port>, MATCHMAKING_API.md 4a); the launcher hears which */
        const unsigned char *a = original_remote.address;
        int loopback = 1;
        for (int i = 0; i < 15 && loopback; ++i) {
            loopback = a[i] == 0;
        }
        loopback = (loopback && a[15] == 1) ||
                   (a[10] == 0xff && a[11] == 0xff && a[12] == 127 && !a[0] && !a[1] && !a[2] && !a[3] && !a[4] &&
                    !a[5] && !a[6] && !a[7] && !a[8] && !a[9]);
        unsigned port = ((original_remote.port & 0xffu) << 8) | (original_remote.port >> 8);
        if (loopback && port && port != 9) { /* ::1 port 9 is "nowhere" */
            char detail[16];
            size_t n = 0;
            const char *head = "host:";
            while (head[n]) {
                detail[n] = head[n];
                ++n;
            }
            char digits[6];
            int d = 0;
            for (unsigned p = port; p && d < 5; p /= 10) {
                digits[d++] = (char)('0' + p % 10);
            }
            while (d) {
                detail[n++] = digits[--d];
            }
            detail[n] = 0;
            azimuth_world_launcher_host(port);
            fi_launch_event(FI_EVENT_FREE_ROAM_ENTERED, detail);
        } else if (port && port != 9) {
            fi_launch_event(FI_EVENT_FREE_ROAM_ENTERED, "join"); /* never an address in a log or a frame */
        }
    }
    fr_log("SecureDeviceAssociationTemplate.CreateAssociationAsync -> completed.");
    return fr_completed((FrCom *)(void *)association, out);
}

static void *fr_template_vtable[FR_SLOTS];

/* Statics slot 6: GetTemplateByName(name, out); the template is wrapped. */
static HRESULT STDMETHODCALLTYPE fr_template_by_name(FrWrap *self, void *name, FrCom **out) {
    typedef HRESULT(STDMETHODCALLTYPE * Fn)(FrCom *, void *, FrCom **);
    HRESULT result = ((Fn)((void *const *)(const void *)self->inner->lpVtbl)[6])(self->inner, name, out);
    if (SUCCEEDED(result) && out && *out) {
        FrWrap *wrap = fr_wrap(*out, fr_template_vtable, 0);
        if (wrap) *out = (FrCom *)(void *)wrap;
    }
    wchar_t text[64];
    FR_LINE(line);
    fr_text(&line, "FREEROAM: association template \"");
    fr_wide(&line, text, fr_hstring(name, text, ARRAY_COUNT(text)));
    fr_text(&line, SUCCEEDED(result) ? "\" wrapped." : "\" refused by Azimuth.");
    fr_end(&line);
    return result;
}

/* -------------------------------------------------------- device addresses */
/* Statics slot 9 GetLocal(out), slot 11 FromBase64String(text, out); instance slot 10
   GetBase64String(out) answers what the object was made from. */

static wchar_t fr_local_base64[48];

static HRESULT STDMETHODCALLTYPE fr_address_base64(FrWrap *self, void **out) {
    return azimuth_net_make_hstring(self->base64, out);
}
static void *fr_address_vtable[FR_SLOTS];

static HRESULT fr_address_from(FrWrap *statics, const wchar_t *base64, FrCom **out) {
    typedef HRESULT(STDMETHODCALLTYPE * Fn)(FrCom *, void *, FrCom **);
    void *text = 0;
    HRESULT result = azimuth_net_make_hstring(base64, &text);
    if (FAILED(result)) {
        return result;
    }
    result = ((Fn)((void *const *)(const void *)statics->inner->lpVtbl)[11])(statics->inner, text, out);
    if (SUCCEEDED(result) && out && *out) {
        FrWrap *wrap = fr_wrap(*out, fr_address_vtable, 0);
        if (wrap) {
            size_t length = fr_wide_length(base64);
            if (length >= ARRAY_COUNT(wrap->base64)) length = ARRAY_COUNT(wrap->base64) - 1;
            memory_copy(wrap->base64, base64, length * sizeof(wchar_t));
            wrap->base64[length] = 0;
            *out = (FrCom *)(void *)wrap;
        }
    }
    return result;
}
static HRESULT STDMETHODCALLTYPE fr_address_local(FrWrap *self, FrCom **out) {
    HRESULT result = fr_address_from(self, fr_local_base64, out);
    static volatile LONG logged;
    if (!InterlockedExchange(&logged, 1)) fr_log_hr("SecureDeviceAddress.GetLocal ->", result);
    return result;
}
static HRESULT STDMETHODCALLTYPE fr_address_from_base64(FrWrap *self, void *text, FrCom **out) {
    wchar_t base64[96];
    fr_hstring(text, base64, ARRAY_COUNT(base64));
    return fr_address_from(self, base64, out);
}

/* ------------------------------------------------------------------ QoS */
/* What the game reads (0x14179B030): result slot 8 GetMeasurementsForMetric(metric, out
   IVectorView), QI'd for IIterable and walked (First 6; iterator Current 6, HasCurrent 7,
   MoveNext 8). Per measurement: Status 6 (1 = success), Metric 7, MetricValue 8 (an
   IPropertyValue read with GetUInt32) and DeviceAddress 9 (the address it passed in). */

typedef struct {
    volatile LONG references;
    FrCom *address[FR_QOS_MAX];
    unsigned count;
    int metric;
} FrQosData;

static void fr_qos_data_release(FrQosData *data) {
    if (!InterlockedDecrement(&data->references)) {
        for (unsigned i = 0; i < data->count; ++i) data->address[i]->lpVtbl->Release(data->address[i]);
        fr_free(data);
    }
}

/* Every object below: lpVtbl, references, data, index. */
typedef struct {
    void *const *lpVtbl;
    volatile LONG references;
    FrQosData *data;
    unsigned index;
    void *const *iterable; /* a list's second interface, IIterable */
} FrQosObject;

static HRESULT STDMETHODCALLTYPE fr_qos_query(FrQosObject *self, REFIID iid, void **out) {
    if (!out) return E_POINTER;
    *out = 0;
    if (fr_is_classic_com(iid)) return E_NOINTERFACE;
    InterlockedIncrement(&self->references);
    *out = self;
    return S_OK;
}
static ULONG STDMETHODCALLTYPE fr_qos_add_ref(FrQosObject *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}
static ULONG STDMETHODCALLTYPE fr_qos_release(FrQosObject *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        if (self->data) fr_qos_data_release(self->data);
        fr_free(self);
    }
    return (ULONG)left;
}
static FrQosObject *fr_qos_new(void *const *vtable, FrQosData *data, unsigned index) {
    FrQosObject *object = fr_alloc(sizeof(*object));
    if (object) {
        object->lpVtbl = vtable;
        object->references = 1;
        object->data = data;
        object->index = index;
        if (data) InterlockedIncrement(&data->references);
    }
    return object;
}
static HRESULT STDMETHODCALLTYPE fr_qos_none(FrQosObject *self) {
    (void)self;
    return FR_E_NOTIMPL;
}

/* IPropertyValue holding the latency: every numeric getter answers it. */
static HRESULT STDMETHODCALLTYPE fr_value_type(void *self, int *type) { (void)self; *type = 6; return S_OK; } /* UInt32 */
static HRESULT STDMETHODCALLTYPE fr_value_scalar(void *self, BYTE *value) { (void)self; *value = 1; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_value_8(void *self, BYTE *value) { (void)self; *value = FR_QOS_LATENCY_MS; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_value_16(void *self, UINT16 *value) { (void)self; *value = FR_QOS_LATENCY_MS; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_value_32(void *self, UINT32 *value) { (void)self; *value = FR_QOS_LATENCY_MS; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_value_64(void *self, UINT64 *value) { (void)self; *value = FR_QOS_LATENCY_MS; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_value_f32(void *self, float *value) { (void)self; *value = (float)FR_QOS_LATENCY_MS; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_value_f64(void *self, double *value) { (void)self; *value = FR_QOS_LATENCY_MS; return S_OK; }
static void *const fr_value_vtable[24] = {
    (void *)(uintptr_t)fr_qos_query,   (void *)(uintptr_t)fr_qos_add_ref,
    (void *)(uintptr_t)fr_qos_release, (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,  (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_value_type,  (void *)(uintptr_t)fr_value_scalar,
    (void *)(uintptr_t)fr_value_8,     (void *)(uintptr_t)fr_value_16,
    (void *)(uintptr_t)fr_value_16,    (void *)(uintptr_t)fr_value_32,
    (void *)(uintptr_t)fr_value_32,    (void *)(uintptr_t)fr_value_64,
    (void *)(uintptr_t)fr_value_64,    (void *)(uintptr_t)fr_value_f32,
    (void *)(uintptr_t)fr_value_f64,   (void *)(uintptr_t)fr_qos_none,
    (void *)(uintptr_t)fr_qos_none,    (void *)(uintptr_t)fr_qos_none,
    (void *)(uintptr_t)fr_qos_none,    (void *)(uintptr_t)fr_qos_none,
    (void *)(uintptr_t)fr_qos_none,    (void *)(uintptr_t)fr_qos_none};

/* QualityOfServiceMeasurement */
static HRESULT STDMETHODCALLTYPE fr_measure_status(FrQosObject *self, int *status) { (void)self; *status = 1; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_measure_metric(FrQosObject *self, int *metric) { *metric = self->data->metric; return S_OK; }
static HRESULT STDMETHODCALLTYPE fr_measure_value(FrQosObject *self, void **value) {
    (void)self;
    *value = fr_qos_new(fr_value_vtable, 0, 0);
    return *value ? S_OK : E_OUTOFMEMORY;
}
static HRESULT STDMETHODCALLTYPE fr_measure_address(FrQosObject *self, FrCom **address) {
    *address = self->data->address[self->index];
    (*address)->lpVtbl->AddRef(*address);
    return S_OK;
}
static void *const fr_measure_vtable[14] = {
    (void *)(uintptr_t)fr_qos_query,       (void *)(uintptr_t)fr_qos_add_ref,
    (void *)(uintptr_t)fr_qos_release,     (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,      (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_measure_status,  (void *)(uintptr_t)fr_measure_metric,
    (void *)(uintptr_t)fr_measure_value,   (void *)(uintptr_t)fr_measure_address,
    (void *)(uintptr_t)fr_qos_none,        (void *)(uintptr_t)fr_qos_none,
    (void *)(uintptr_t)fr_qos_none,        (void *)(uintptr_t)fr_qos_none};

/* IIterator<QualityOfServiceMeasurement> */
static HRESULT STDMETHODCALLTYPE fr_iter_current(FrQosObject *self, void **out) {
    *out = 0;
    if (self->index >= self->data->count) return FR_E_BOUNDS;
    *out = fr_qos_new(fr_measure_vtable, self->data, self->index);
    return *out ? S_OK : E_OUTOFMEMORY;
}
static HRESULT STDMETHODCALLTYPE fr_iter_has(FrQosObject *self, BYTE *has) {
    *has = self->index < self->data->count;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_iter_next(FrQosObject *self, BYTE *has) {
    if (self->index < self->data->count) ++self->index;
    *has = self->index < self->data->count;
    return S_OK;
}
static void *const fr_iter_vtable[10] = {
    (void *)(uintptr_t)fr_qos_query,    (void *)(uintptr_t)fr_qos_add_ref,
    (void *)(uintptr_t)fr_qos_release,  (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,   (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_iter_current, (void *)(uintptr_t)fr_iter_has,
    (void *)(uintptr_t)fr_iter_next,    (void *)(uintptr_t)fr_qos_none};

/* IVectorView<QualityOfServiceMeasurement> at +0 and IIterable<...> at `iterable`. */
static FrQosObject *fr_list_from_iterable(void *self) {
    return (FrQosObject *)((unsigned char *)self - offsetof(FrQosObject, iterable));
}
static HRESULT STDMETHODCALLTYPE fr_list_query(FrQosObject *self, REFIID iid, void **out) {
    if (!out) return E_POINTER;
    *out = 0;
    if (fr_is_classic_com(iid)) return E_NOINTERFACE;
    InterlockedIncrement(&self->references);
    *out = fr_guid_is(iid, &fr_iid_qos_iterable) ? (void *)&self->iterable : (void *)self;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_list_at(FrQosObject *self, UINT32 index, void **out) {
    *out = 0;
    if (index >= self->data->count) return FR_E_BOUNDS;
    *out = fr_qos_new(fr_measure_vtable, self->data, index);
    return *out ? S_OK : E_OUTOFMEMORY;
}
static HRESULT STDMETHODCALLTYPE fr_list_size(FrQosObject *self, UINT32 *size) {
    *size = self->data->count;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE fr_iterable_query(void *self, REFIID iid, void **out) { return fr_list_query(fr_list_from_iterable(self), iid, out); }
static ULONG STDMETHODCALLTYPE fr_iterable_add_ref(void *self) { return fr_qos_add_ref(fr_list_from_iterable(self)); }
static ULONG STDMETHODCALLTYPE fr_iterable_release(void *self) { return fr_qos_release(fr_list_from_iterable(self)); }
static HRESULT STDMETHODCALLTYPE fr_iterable_first(void *self, void **out) {
    *out = fr_qos_new(fr_iter_vtable, fr_list_from_iterable(self)->data, 0);
    return *out ? S_OK : E_OUTOFMEMORY;
}
static void *const fr_list_vtable[10] = {
    (void *)(uintptr_t)fr_list_query,  (void *)(uintptr_t)fr_qos_add_ref,
    (void *)(uintptr_t)fr_qos_release, (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,  (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_list_at,     (void *)(uintptr_t)fr_list_size,
    (void *)(uintptr_t)fr_qos_none,    (void *)(uintptr_t)fr_qos_none};
static void *const fr_iterable_vtable[7] = {
    (void *)(uintptr_t)fr_iterable_query,   (void *)(uintptr_t)fr_iterable_add_ref,
    (void *)(uintptr_t)fr_iterable_release, (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,       (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_iterable_first};

/* MeasureQualityOfServiceResult: slot 8 GetMeasurementsForMetric(metric, out). */
static HRESULT STDMETHODCALLTYPE fr_result_for_metric(FrQosObject *self, int metric, void **out) {
    FrQosObject *list = fr_qos_new(fr_list_vtable, self->data, 0);
    *out = list;
    if (!list) return E_OUTOFMEMORY;
    list->iterable = fr_iterable_vtable;
    self->data->metric = metric;
    return S_OK;
}
static void *const fr_result_vtable[13] = {
    (void *)(uintptr_t)fr_qos_query,         (void *)(uintptr_t)fr_qos_add_ref,
    (void *)(uintptr_t)fr_qos_release,       (void *)(uintptr_t)fr_iids,
    (void *)(uintptr_t)fr_class_name,        (void *)(uintptr_t)fr_trust,
    (void *)(uintptr_t)fr_qos_none,          (void *)(uintptr_t)fr_qos_none,
    (void *)(uintptr_t)fr_result_for_metric, (void *)(uintptr_t)fr_qos_none,
    (void *)(uintptr_t)fr_qos_none,          (void *)(uintptr_t)fr_qos_none,
    (void *)(uintptr_t)fr_qos_none};

/* The addresses the game measures: its IIterable<SecureDeviceAddress>, walked (First 6;
   iterator Current 6, HasCurrent 7, MoveNext 8). */
static void fr_qos_collect(FrQosData *data, FrCom *iterable) {
    typedef HRESULT(STDMETHODCALLTYPE * OutFn)(FrCom *, FrCom **);
    typedef HRESULT(STDMETHODCALLTYPE * FlagFn)(FrCom *, BYTE *);
    FrCom *iterator = 0;
    if (!iterable ||
        FAILED(((OutFn)((void *const *)(const void *)iterable->lpVtbl)[6])(iterable, &iterator)) ||
        !iterator) {
        return;
    }
    void *const *slots = (void *const *)(const void *)iterator->lpVtbl;
    BYTE has = 0;
    ((FlagFn)slots[7])(iterator, &has);
    while (has && data->count < FR_QOS_MAX) {
        FrCom *address = 0;
        if (SUCCEEDED(((OutFn)slots[6])(iterator, &address)) && address) {
            data->address[data->count++] = address;
        }
        if (FAILED(((FlagFn)slots[8])(iterator, &has))) break;
    }
    iterator->lpVtbl->Release(iterator);
}

/* Statics slot 6: MeasureQualityOfServiceAsync(addresses, metrics, timeout, probes, out). */
static HRESULT STDMETHODCALLTYPE fr_qos_measure(FrWrap *self, FrCom *addresses, void *metrics,
                                               UINT32 timeout, UINT32 probes, void **out) {
    (void)self, (void)metrics, (void)timeout, (void)probes;
    FrQosData *data = fr_alloc(sizeof(*data));
    if (!data) return E_OUTOFMEMORY;
    data->references = 1;
    fr_qos_collect(data, addresses);
    FrQosObject *result = fr_qos_new(fr_result_vtable, data, 0);
    fr_qos_data_release(data);
    if (!result) return E_OUTOFMEMORY;
    FR_LINE(line);
    fr_text(&line, "FREEROAM: QualityOfService measured ");
    fr_number(&line, data->count);
    fr_text(&line, " address(es) at 20 ms.");
    fr_end(&line);
    return fr_completed((FrCom *)(void *)result, out);
}

/* ---------------------------------------------------------------- party */
/* Slots 6 and 7 (user, session reference, out): Register{Game,Match}SessionAsync, which the
   game calls after writing its sessions. Azimuth's actions fail, and that fails the session
   activity (0x1417AE470) before the world is reached. */
static HRESULT STDMETHODCALLTYPE fr_party_register(FrWrap *self, void *user, void *session, void **out) {
    (void)self, (void)user, (void)session;
    static volatile LONG logged;
    if (InterlockedIncrement(&logged) <= 4) fr_log("Party session registered (completed).");
    return fr_completed(0, out);
}

/* -------------------------------------------------------------- vtables */

static void *fr_qos_statics_vtable[FR_SLOTS], *fr_address_statics_vtable[FR_SLOTS],
    *fr_template_statics_vtable[FR_SLOTS], *fr_party_statics_vtable[FR_SLOTS];
static volatile LONG fr_vtables_ready;

static void fr_build_vtables(void) {
    if (InterlockedCompareExchange(&fr_vtables_ready, 1, 0)) {
        return;
    }
    fr_vtable_base(fr_qos_statics_vtable);
    fr_qos_statics_vtable[6] = (void *)(uintptr_t)fr_qos_measure;
    fr_vtable_base(fr_address_statics_vtable);
    fr_address_statics_vtable[9] = (void *)(uintptr_t)fr_address_local;
    fr_address_statics_vtable[11] = (void *)(uintptr_t)fr_address_from_base64;
    fr_vtable_base(fr_address_vtable);
    fr_address_vtable[10] = (void *)(uintptr_t)fr_address_base64;
    fr_vtable_base(fr_template_statics_vtable);
    fr_template_statics_vtable[6] = (void *)(uintptr_t)fr_template_by_name;
    fr_vtable_base(fr_template_vtable);
    fr_template_vtable[8] = (void *)(uintptr_t)fr_template_create;
    fr_vtable_base(fr_party_statics_vtable);
    fr_party_statics_vtable[6] = (void *)(uintptr_t)fr_party_register;
    fr_party_statics_vtable[7] = (void *)(uintptr_t)fr_party_register;
    InterlockedExchange(&fr_vtables_ready, 2);
}

int azimuth_freeroam_filter(const wchar_t *name, const void *iid, void **out) {
    if (!fr_enabled || !name || !out || !*out || !iid) {
        return 0;
    }
    /* The game's network classes are activated after its imports are bound. */
    fr_sockets();
    while (fr_vtables_ready != 2) {
        fr_build_vtables();
    }
    void *const *vtable = 0;
    const GUID *guid = 0;
    const char *what = 0;
    /* the game activates some of these every frame: each activation is wrapped, only the first few are logged */
    static volatile LONG wrapped_logs[4];
    int kind = 0;
    if (fr_guid_is(iid, &fr_iid_qos_statics) &&
        fr_wide_equal(name, L"Windows.Xbox.Networking.QualityOfService")) {
        vtable = fr_qos_statics_vtable, guid = &fr_iid_qos_statics, what = "QualityOfService";
    } else if (fr_guid_is(iid, &fr_iid_address_statics) &&
               fr_wide_equal(name, L"Windows.Xbox.Networking.SecureDeviceAddress")) {
        vtable = fr_address_statics_vtable, guid = &fr_iid_address_statics, what = "SecureDeviceAddress", kind = 1;
    } else if (fr_guid_is(iid, &fr_iid_template_statics) &&
               fr_wide_equal(name, L"Windows.Xbox.Networking.SecureDeviceAssociationTemplate")) {
        vtable = fr_template_statics_vtable, guid = &fr_iid_template_statics, what = "SecureDeviceAssociationTemplate", kind = 2;
    } else if (fr_guid_is(iid, &fr_iid_party_statics) &&
               fr_wide_equal(name, L"Windows.Xbox.Multiplayer.Party")) {
        vtable = fr_party_statics_vtable, guid = &fr_iid_party_statics, what = "Party", kind = 3;
    }
    if (!vtable) {
        return 0;
    }
    FrWrap *wrap = fr_wrap((FrCom *)*out, vtable, guid);
    if (!wrap) {
        return 0;
    }
    *out = wrap;
    if (InterlockedIncrement(&wrapped_logs[kind]) <= 4) {
        FR_LINE(line);
        fr_text(&line, "FREEROAM: ");
        fr_text(&line, what);
        fr_text(&line, " statics wrapped.");
        fr_end(&line);
    }
    return 1;
}

/* ----------------------------------------------------------- game patches */

typedef struct {
    unsigned rva;
    unsigned char size;
    unsigned char original[3], patched[3];
    const char *what;
} FrPatch;

/* The free roam entry (media\stateflow.zip, network.xml) waits for the server's weather
   (WaitForFullWeatherReady, update 0x1409641D0: jump to the transition at 0x140964234;
   ApplyFullWeatherFromServer skips itself without a weather object, 0x140865A31) and for its
   traffic (NetworkWaitForAllPendingTraffic, update 0x1408E05E0: drop the jne on the traffic
   manager's pending flag). Without the second, the players' cars are never unghosted. */
static const FrPatch fr_patches[] = {
    {0x9641ea, 2, {0x48, 0x8b, 0x0d}, {0xeb, 0x48, 0}, "weather wait"},
    {0x8e0658, 2, {0x75, 0x0d, 0}, {0x90, 0x90, 0}, "traffic wait"},
};

static void fr_apply_patches(void) {
    for (unsigned i = 0; i < ARRAY_COUNT(fr_patches); ++i) {
        const FrPatch *patch = &fr_patches[i];
        unsigned char *at = fr_game + patch->rva;
        unsigned char current[3];
        /* The weather patch checks the whole 3-byte mov it overwrites. */
        unsigned checked = patch->original[2] ? 3u : patch->size;
        FR_LINE(line);
        fr_text(&line, "FREEROAM: skip the ");
        fr_text(&line, patch->what);
        if (!memory_read(at, current, checked) || !memory_equal(current, patch->original, checked)) {
            fr_text(&line, ": unexpected bytes, left as is.");
            fr_end(&line);
            continue;
        }
        DWORD old_protection, ignored;
        if (!VirtualProtect(at, patch->size, PAGE_EXECUTE_READWRITE, &old_protection)) {
            fr_text(&line, ": not writable.");
            fr_end(&line);
            continue;
        }
        memory_copy(at, patch->patched, patch->size);
        FlushInstructionCache(GetCurrentProcess(), at, patch->size);
        VirtualProtect(at, patch->size, old_protection, &ignored);
        fr_text(&line, ".");
        fr_end(&line);
    }
}

/* ------------------------------------------------------------------ setup */

/* Infinity.cfg [hook] FreeRoam, FreeRoamWorld and FreeRoamDirect (cfg.c, read at start). */
void azimuth_apply_freeroam(unsigned char *game_image, const wchar_t *directory, const wchar_t *executable) {
    const FiConfig *config = fi_cfg();
    fr_enabled = config->freeroam != 0;
    if (!fr_enabled) {
        log_append("FREEROAM: off (Infinity.cfg [hook] FreeRoam=0).\r\n");
        return;
    }
    if (!azimuth_net_proxy_enabled()) {
        fr_enabled = 0;
        log_append("FREEROAM: off; it needs [hook] NetMode=Proxy.\r\n");
        return;
    }
    fr_game = game_image;
    azimuth_world_set_directory(directory);
    /* the car database is hashed from azimuth_apply_network (runtime.c), free roam on or off */
    (void)executable;
    size_t n = 0;
    while (config->freeroam_world[n] && n + 1 < ARRAY_COUNT(fr_world_override)) {
        fr_world_override[n] = config->freeroam_world[n];
        ++n;
    }
    fr_world_override[n] = 0;
    azimuth_proxy_configure(config->freeroam_direct != 0);
    fr_base64_encode("fh2inf:local", fr_local_base64, ARRAY_COUNT(fr_local_base64));
    fr_build_vtables();
    fr_apply_patches();
    fi_carmeet_apply(game_image);
    fi_follow_apply(game_image);
    /* The Winsock imports are redirected on the first WinRT activation, on a game thread:
       loading ws2_32 here would happen under the loader lock. */
    log_append(fr_world_override[0] ? "FREEROAM: on; the world is [hook] FreeRoamWorld.\r\n"
                                    : "FREEROAM: on; the world comes from the session.\r\n");
}
