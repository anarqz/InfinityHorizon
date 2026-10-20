/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* The launcher's session (FI_LAUNCH=v1;pipe=<nonce>): ONLINE_OFFLINE.md section 3, launcher
   ORCHESTRATOR.md section 5, HOOK_MIGRATION.md section 4.

   infinity.exe creates \\.\pipe\ForzaInfinity-<nonce> (first instance, current user only, remote
   clients refused) and starts azimuth.exe with FI_LAUNCH in its environment. cfg.c classifies the
   value at DLL_PROCESS_ATTACH (no I/O there); account.c starts launch_thread instead of the Beta
   account thread, and the game's identity getters wait for it (at most 5 s). The thread:
   1. WaitNamedPipeW (5 s), CreateFileW (read/write, no sharing, SQOS identification), and
      checks that the pipe's server is the process that started the game
      (GetNamedPipeServerProcessId against the parent pid that Toolhelp32 reports for this
      process): another process holding the name gets nothing;
   2. writes Hello{1, pid, MOD_VERSION} and reads one frame, Ticket or Refused, within 5 s.
   Every failure plays offline, as without the launcher, and diagnostics record why.

   Frames (fi_launcher_core::session::Frame, golden bytes in session.rs): u32 LE length of type +
   payload (at most 65536), u8 type, payload; strings and blobs are u16 LE length + bytes. One
   static buffer holds a frame; nothing here uses the heap. Kernel32 only: the pipe and Toolhelp32
   functions are looked up with GetProcAddress (as account.c does), the rest is in kernel32.def. */
#include "infinity.h"
#include "sign.h"
#include "../core/logging.h"
#include "../core/support.h"
#include "../core/sha256.h"
#include <tlhelp32.h>

#define LAUNCH_PROTOCOL 1u
#define LAUNCH_MAX_FRAME 65536u   /* type + payload: session::MAX_FRAME */
#define LAUNCH_MAX_CLAIMS 4096u   /* longer claims are refused (HOOK_MIGRATION.md section 4) */
#define LAUNCH_MAX_TOKEN 1024u    /* a PASETO v4.public session token is about 400 bytes */
#define LAUNCH_PIPE_WAIT_MS 5000u
#define LAUNCH_FRAME_WAIT_MS 5000u
#define LAUNCH_POLL_MS 20u
#define LAUNCH_MAX_LIFETIME 900u     /* exp - iat (fi_auth::ticket::MAX_LIFETIME_SECS) */
#define LAUNCH_MAX_SKEW 86400u       /* the PC clock may be off by a day (dual boot) */
#define LAUNCH_REFRESH_MS 120000u    /* RefreshRequest when the token has less than 2 minutes */
#define LAUNCH_READ_POLL_MS 100u     /* the reader loop's wait for a frame between its other work */
#define LAUNCH_OUTBOX 65536u
#define LAUNCH_EXIT_RELAUNCH 0x46490001u /* fi_launcher_core::launch::EXIT_RELAUNCH ("FI" 1) */

enum {
    FRAME_HELLO = 0x01,
    FRAME_TICKET = 0x02,
    FRAME_REFRESH_REQUEST = 0x03,
    FRAME_TOKEN = 0x04,
    FRAME_NOTICE = 0x05,
    FRAME_RELAUNCH_REQUEST = 0x06,
    FRAME_BYE = 0x07,
    FRAME_REFUSED = 0x08,
    FRAME_EVENT = 0x09,
    FRAME_CAPTURE = 0x13,
    FRAME_OVERLAY = 0x14,
};

/* Where the session stands (launch_state). */
enum { LAUNCH_IDLE, LAUNCH_WAITING, LAUNCH_ONLINE, LAUNCH_OFFLINE, LAUNCH_LATE };
/* Why this launcher session is offline (launch_why), for diagnostics. */
enum {
    LAUNCH_WHY_NONE,
    LAUNCH_WHY_NO_PIPE,   /* no pipe, or it did not answer in time */
    LAUNCH_WHY_PID,       /* the pipe's server is not the process that started the game */
    LAUNCH_WHY_REFUSED,   /* the launcher refused this start */
    LAUNCH_WHY_TICKET,    /* the ticket does not check out */
    LAUNCH_WHY_PROTOCOL,  /* a frame this protocol does not allow */
    LAUNCH_WHY_LATE,      /* the game asked who plays before the launcher answered */
};

typedef BOOL(WINAPI *WaitNamedPipeWFn)(LPCWSTR, DWORD);
typedef BOOL(WINAPI *GetNamedPipeServerProcessIdFn)(HANDLE, PULONG);
typedef BOOL(WINAPI *PeekNamedPipeFn)(HANDLE, LPVOID, DWORD, LPDWORD, LPDWORD, LPDWORD);
typedef HANDLE(WINAPI *CreateToolhelp32SnapshotFn)(DWORD, DWORD);
typedef BOOL(WINAPI *Process32WFn)(HANDLE, LPPROCESSENTRY32W);
typedef void(WINAPI *GetSystemTimeAsFileTimeFn)(LPFILETIME);

static struct {
    WaitNamedPipeWFn wait_named_pipe;
    GetNamedPipeServerProcessIdFn server_pid;
    PeekNamedPipeFn peek;
    CreateToolhelp32SnapshotFn snapshot;
    Process32WFn first, next;
    GetSystemTimeAsFileTimeFn system_time;
} launch_api;

static volatile LONG launch_state;
static volatile LONG launch_why;
/* The session token for game traffic (X-Fi-Session) and when it expires (GetTickCount64 ms, so
   a clock change during play moves nothing). launch_lock guards both. */
static SRWLOCK launch_lock = SRWLOCK_INIT;
static char launch_token[LAUNCH_MAX_TOKEN + 1];
static unsigned long long launch_deadline_ms;
/* The server's clock minus this PC's, when this PC's is far off (from the ticket's iat). */
static long long launch_clock_offset;
static volatile LONG launch_ended;      /* the game plane refused the session twice: offline */
static volatile LONG launch_refused_401; /* 401s since the last token */
static volatile LONG launch_refresh_wanted;
static volatile LONG launch_refresh_sent;  /* a RefreshRequest went for the current token */
/* Frames for the launcher (Event, Capture, RelaunchRequest), queued by any thread and written by
   the launch thread, the pipe's only user. */
static SRWLOCK launch_out_lock = SRWLOCK_INIT;
static unsigned char launch_outbox[LAUNCH_OUTBOX];
static size_t launch_outbox_used;
static volatile LONG launch_outbox_flushed; /* everything queued so far was written */
static volatile LONG launch_connected;      /* the pipe is open after a good handshake */
static HANDLE launch_pipe = INVALID_HANDLE_VALUE;
static wchar_t launch_bin[PATH_CAPACITY];
/* One frame, read or written: the 4-byte length, then type and payload. */
static unsigned char launch_frame[4 + LAUNCH_MAX_FRAME];
static size_t launch_frame_size; /* type + payload of the frame in launch_frame */

/* ------------------------------------------------------------------ text */

static void launch_text_add(char *out, size_t cap, const char *text) {
    size_t n = text_length(out);
    while (text && *text && n + 1 < cap) {
        out[n++] = *text++;
    }
    out[n] = 0;
}

static void launch_number(char *out, size_t cap, unsigned long long value) {
    char digits[24];
    int n = 0;
    do {
        digits[n++] = (char)('0' + value % 10);
        value /= 10;
    } while (value);
    char text[24];
    for (int i = 0; i < n; ++i) {
        text[i] = digits[n - 1 - i];
    }
    text[n] = 0;
    launch_text_add(out, cap, text);
}

/* "LAUNCH: <text><number><tail>\r\n" (number left out when `with_number` is 0). */
static void launch_log(const char *text, int with_number, unsigned long long number, const char *tail) {
    char line[400];
    line[0] = 0;
    launch_text_add(line, sizeof(line), "LAUNCH: ");
    launch_text_add(line, sizeof(line), text);
    if (with_number) {
        launch_number(line, sizeof(line), number);
    }
    launch_text_add(line, sizeof(line), tail);
    launch_text_add(line, sizeof(line), "\r\n");
    log_append(line);
}

/* ----------------------------------------------------------------- state */

int fi_launch_protocol(void) {
    return fi_cfg()->launch == FI_LAUNCH_PROTOCOL;
}

/* Offline for the rest of this session (once the handshake failed). */
static void launch_fail(int why) {
    InterlockedExchange(&launch_why, why);
    InterlockedCompareExchange(&launch_state, LAUNCH_OFFLINE, LAUNCH_WAITING);
}

static int launch_resolve(void) {
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    if (!kernel) {
        return 0;
    }
    launch_api.wait_named_pipe = (WaitNamedPipeWFn)(uintptr_t)GetProcAddress(kernel, "WaitNamedPipeW");
    launch_api.server_pid = (GetNamedPipeServerProcessIdFn)(uintptr_t)GetProcAddress(kernel, "GetNamedPipeServerProcessId");
    launch_api.peek = (PeekNamedPipeFn)(uintptr_t)GetProcAddress(kernel, "PeekNamedPipe");
    launch_api.snapshot = (CreateToolhelp32SnapshotFn)(uintptr_t)GetProcAddress(kernel, "CreateToolhelp32Snapshot");
    launch_api.first = (Process32WFn)(uintptr_t)GetProcAddress(kernel, "Process32FirstW");
    launch_api.next = (Process32WFn)(uintptr_t)GetProcAddress(kernel, "Process32NextW");
    launch_api.system_time = (GetSystemTimeAsFileTimeFn)(uintptr_t)GetProcAddress(kernel, "GetSystemTimeAsFileTime");
    return launch_api.wait_named_pipe && launch_api.server_pid && launch_api.peek && launch_api.snapshot &&
           launch_api.first && launch_api.next && launch_api.system_time;
}

/* The pid of the process that started this one (Toolhelp32), 0 when unknown. */
static DWORD launch_parent_pid(void) {
    DWORD self = GetCurrentProcessId(), parent = 0;
    HANDLE snapshot = launch_api.snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) {
        return 0;
    }
    PROCESSENTRY32W entry;
    entry.dwSize = sizeof(entry);
    for (BOOL more = launch_api.first(snapshot, &entry); more; more = launch_api.next(snapshot, &entry)) {
        if (entry.th32ProcessID == self) {
            parent = entry.th32ParentProcessID;
            break;
        }
    }
    CloseHandle(snapshot);
    return parent;
}


/* --------------------------------------------------------------- frames */

typedef struct {
    const unsigned char *at;
    size_t left;
    int bad;
} LaunchReader;

static unsigned long long launch_le(LaunchReader *r, unsigned size) {
    unsigned long long value = 0;
    if (r->left < size) {
        r->bad = 1;
        return 0;
    }
    for (unsigned i = 0; i < size; ++i) {
        value |= (unsigned long long)r->at[i] << (8 * i);
    }
    r->at += size;
    r->left -= size;
    return value;
}

/* A u16-length string or blob: its bytes stay in the frame buffer. */
static const unsigned char *launch_blob(LaunchReader *r, size_t *size) {
    *size = (size_t)launch_le(r, 2);
    if (r->bad || r->left < *size) {
        r->bad = 1;
        *size = 0;
        return 0;
    }
    const unsigned char *data = r->at;
    r->at += *size;
    r->left -= *size;
    return data;
}

typedef struct {
    unsigned char *at;
    size_t used, cap;
    int bad;
} LaunchWriter;

static void launch_put(LaunchWriter *w, const void *data, size_t size) {
    if (w->bad || size > w->cap - w->used) {
        w->bad = 1;
        return;
    }
    memory_copy(w->at + w->used, data, size);
    w->used += size;
}

static void launch_put_le(LaunchWriter *w, unsigned long long value, unsigned size) {
    unsigned char bytes[8];
    for (unsigned i = 0; i < size; ++i) {
        bytes[i] = (unsigned char)(value >> (8 * i));
    }
    launch_put(w, bytes, size);
}

/* Starts a frame of `type` in `out` (its length is filled by launch_frame_end). */
static void launch_frame_begin(LaunchWriter *w, unsigned char *out, size_t cap, unsigned type) {
    w->at = out;
    w->used = 0;
    w->cap = cap;
    w->bad = 0;
    launch_put_le(w, 0, 4);
    launch_put_le(w, type, 1);
}

/* The frame's total size (0 when it did not fit). */
static size_t launch_frame_end(LaunchWriter *w) {
    if (w->bad || w->used - 4 > LAUNCH_MAX_FRAME) {
        return 0;
    }
    size_t body = w->used - 4;
    for (unsigned i = 0; i < 4; ++i) {
        w->at[i] = (unsigned char)(body >> (8 * i));
    }
    return w->used;
}

/* Hello{protocol, pid, version} into `out`: its size, 0 when it does not fit. */
static size_t launch_encode_hello(unsigned char *out, size_t cap, unsigned protocol, unsigned pid, const char *version) {
    LaunchWriter w;
    size_t n = text_length(version);
    launch_frame_begin(&w, out, cap, FRAME_HELLO);
    launch_put_le(&w, protocol, 2);
    launch_put_le(&w, pid, 4);
    launch_put_le(&w, n, 2);
    launch_put(&w, version, n);
    return launch_frame_end(&w);
}

static int launch_write(const unsigned char *data, size_t size) {
    DWORD written = 0;
    while (size) {
        if (!WriteFile(launch_pipe, data, (DWORD)size, &written, 0) || !written) {
            return 0;
        }
        data += written;
        size -= written;
    }
    return 1;
}

/* Reads `size` bytes within `wait_ms`: 1 read, 0 timed out, -1 the pipe broke (*error:
   GetLastError). With `whole`, nothing is read until all `size` bytes are there (a frame's
   length is never taken half, so a time-out leaves the stream where it was). Polls with
   PeekNamedPipe, so the writes of this same thread never wait behind a blocking read on the
   synchronous handle. */
static int launch_read(unsigned char *out, size_t size, unsigned wait_ms, int whole, DWORD *error) {
    unsigned long long until = GetTickCount64() + wait_ms;
    while (size) {
        DWORD available = 0, got = 0;
        if (!launch_api.peek(launch_pipe, 0, 0, 0, &available, 0)) {
            *error = GetLastError();
            return -1;
        }
        if (!available || (whole && available < size)) {
            if (GetTickCount64() >= until) {
                return 0;
            }
            Sleep(LAUNCH_POLL_MS);
            continue;
        }
        if (!ReadFile(launch_pipe, out, (DWORD)(available < size ? available : size), &got, 0) || !got) {
            *error = GetLastError();
            return -1;
        }
        out += got;
        size -= got;
    }
    return 1;
}

/* One frame into launch_frame: its length within `wait_ms`, then its body within 5 s. 1 read
   (launch_frame_size, type at launch_frame[4]), 0 no frame yet, -1 the pipe broke (or a frame
   was cut), -2 a length this protocol refuses (0, or above 64 KiB). */
static int launch_read_frame(unsigned wait_ms, DWORD *error) {
    int got = launch_read(launch_frame, 4, wait_ms, 1, error);
    if (got <= 0) {
        return got;
    }
    size_t length = (size_t)launch_frame[0] | (size_t)launch_frame[1] << 8 | (size_t)launch_frame[2] << 16 |
                    (size_t)launch_frame[3] << 24;
    if (!length || length > LAUNCH_MAX_FRAME) {
        return -2;
    }
    got = launch_read(launch_frame + 4, length, LAUNCH_FRAME_WAIT_MS, 0, error);
    if (got <= 0) {
        return -1;
    }
    launch_frame_size = length;
    return 1;
}

/* ---------------------------------------------------------------- outbox */

/* Queues one frame for the launcher (any thread): 1 queued, 0 no room (it is dropped). */
static int launch_queue(const unsigned char *frame, size_t size) {
    int queued = 0;
    AcquireSRWLockExclusive(&launch_out_lock);
    if (size && size <= LAUNCH_OUTBOX - launch_outbox_used) {
        memory_copy(launch_outbox + launch_outbox_used, frame, size);
        launch_outbox_used += size;
        InterlockedExchange(&launch_outbox_flushed, 0);
        queued = 1;
    }
    ReleaseSRWLockExclusive(&launch_out_lock);
    return queued;
}

/* The launch thread: writes what is queued. 0 when the pipe broke. */
static int launch_flush(void) {
    static unsigned char pending[LAUNCH_OUTBOX];
    AcquireSRWLockExclusive(&launch_out_lock);
    size_t size = launch_outbox_used;
    memory_copy(pending, launch_outbox, size);
    launch_outbox_used = 0;
    ReleaseSRWLockExclusive(&launch_out_lock);
    int ok = !size || launch_write(pending, size);
    InterlockedExchange(&launch_outbox_flushed, 1);
    return ok;
}

/* A frame of `type` with the payload `payload` (`size` bytes), queued. */
static int launch_queue_frame(unsigned type, const void *payload, size_t size) {
    unsigned char frame[512];
    LaunchWriter w;
    launch_frame_begin(&w, frame, sizeof(frame), type);
    launch_put(&w, payload, size);
    size_t n = launch_frame_end(&w);
    return n && launch_queue(frame, n);
}

/* Event{kind, detail}; kind 2 is reserved legacy, kind 6 carries overlay notices. */
void fi_launch_event(unsigned kind, const char *detail) {
    unsigned char payload[260];
    size_t n = detail ? text_length(detail) : 0;
    if (!fi_launch_protocol() || n > sizeof(payload) - 3) {
        return;
    }
    payload[0] = (unsigned char)kind;
    payload[1] = (unsigned char)n;
    payload[2] = (unsigned char)(n >> 8);
    memory_copy(payload + 3, detail, n);
    launch_queue_frame(FRAME_EVENT, payload, n + 3);
}

/* Event 6's detail into `out` (at least 253 bytes): "<level>|<text>", the text cut at 250 bytes,
   never inside a UTF-8 sequence; its length, 0 for no text. */
static size_t launch_notice_detail(unsigned level, const char *utf8, char *out) {
    size_t n = utf8 ? text_length(utf8) : 0;
    if (!n) {
        return 0;
    }
    if (n > 250) {
        n = 250;
        while (n > 0 && ((unsigned char)utf8[n] & 0xc0u) == 0x80u) {
            --n; /* utf8[n] continues a sequence: cut before the sequence's first byte */
        }
    }
    out[0] = (char)('0' + (level > FI_NOTICE_ERROR ? FI_NOTICE_ERROR : level));
    out[1] = '|';
    memory_copy(out + 2, utf8, n);
    out[n + 2] = 0;
    return n + 2;
}

void fi_launch_notice(unsigned level, const char *utf8) {
    char detail[256];
    if (launch_notice_detail(level, utf8, detail)) {
        fi_launch_event(FI_EVENT_NOTICE, detail);
    }
}

/* Capture{what, ok} (1 the DXGI queue and swapchain, 4 the overlay's load; ORCHESTRATOR.md 6). */
void fi_launch_capture(unsigned what, int ok) {
    unsigned char payload[2] = {(unsigned char)what, (unsigned char)(ok != 0)};
    if (fi_launch_protocol()) {
        launch_queue_frame(FRAME_CAPTURE, payload, sizeof(payload));
    }
}

/* --------------------------------------------------------------- ticket */

/* The Ticket frame's parts (they point into launch_frame). */
typedef struct {
    const unsigned char *claims, *signature, *token;
    size_t claims_size, token_size;
    unsigned long long token_expires;
} LaunchTicket;

static int launch_parse_ticket(LaunchTicket *t) {
    LaunchReader r = {launch_frame + 5, launch_frame_size - 1, 0};
    t->claims = launch_blob(&r, &t->claims_size);
    if (r.left < 64) {
        return 0;
    }
    t->signature = r.at;
    r.at += 64;
    r.left -= 64;
    t->token = launch_blob(&r, &t->token_size);
    t->token_expires = launch_le(&r, 8);
    return !r.bad && !r.left;
}

/* ----------------------------------------------------------------- claims */
/* The claims are one flat JSON object (fi_auth::ticket::TicketClaims: strings and integers;
   fields this hook does not know are skipped, whatever they hold). This reader walks the object
   member by member, so a key is only ever matched where a key stands, never inside a value. The
   signature is checked over the raw bytes before anything but `kid` is read. */

typedef struct {
    const char *at, *end;
} LaunchJson;

static void json_space(LaunchJson *j) {
    while (j->at < j->end && (*j->at == ' ' || *j->at == '\t' || *j->at == '\r' || *j->at == '\n')) {
        ++j->at;
    }
}

/* Past one string (at its opening quote): 1, or 0 when it does not end. */
static int json_skip_string(LaunchJson *j) {
    for (++j->at; j->at < j->end; ++j->at) {
        if (*j->at == '\\') {
            ++j->at;
        } else if (*j->at == '"') {
            ++j->at;
            return 1;
        }
    }
    return 0;
}

/* Past one value of any kind (nested objects and arrays by depth): 1, or 0 when malformed. */
static int json_skip_value(LaunchJson *j) {
    int depth = 0;
    json_space(j);
    do {
        if (j->at >= j->end) {
            return 0;
        }
        char c = *j->at;
        if (c == '"') {
            if (!json_skip_string(j)) {
                return 0;
            }
        } else if (c == '{' || c == '[') {
            ++depth;
            ++j->at;
        } else if (c == '}' || c == ']') {
            if (--depth < 0) {
                return 0;
            }
            ++j->at;
        } else {
            /* a number or a literal; inside a nested value also a separator or a space */
            const char *start = j->at;
            while (j->at < j->end && *j->at != ',' && *j->at != '}' && *j->at != ']' && *j->at != '"' &&
                   *j->at != '{' && *j->at != '[' && *j->at != ' ' && *j->at != '\t' && *j->at != '\r' &&
                   *j->at != '\n') {
                ++j->at;
            }
            if (j->at == start) {
                if (depth == 0) {
                    return 0;
                }
                ++j->at;
            }
        }
    } while (depth > 0);
    return 1;
}

/* The value of member `key` of the object `json` (`size` bytes): its first byte in `*value`
   and its extent in `*value_end` (a string: from the quote after the opening one to the closing
   one). 1 found, 0 absent or malformed. */
static int json_member(const char *json, size_t size, const char *key, const char **value, const char **value_end) {
    LaunchJson j = {json, json + size};
    size_t key_size = text_length(key);
    json_space(&j);
    if (j.at >= j.end || *j.at++ != '{') {
        return 0;
    }
    for (;;) {
        json_space(&j);
        if (j.at < j.end && *j.at == '}') {
            return 0;
        }
        if (j.at >= j.end || *j.at != '"') {
            return 0;
        }
        const char *name = j.at + 1;
        if (!json_skip_string(&j)) {
            return 0;
        }
        size_t name_size = (size_t)(j.at - 1 - name);
        json_space(&j);
        if (j.at >= j.end || *j.at++ != ':') {
            return 0;
        }
        json_space(&j);
        const char *start = j.at;
        if (!json_skip_value(&j)) {
            return 0;
        }
        if (name_size == key_size && memory_equal(name, key, key_size)) {
            *value = start;
            *value_end = j.at;
            return 1;
        }
        json_space(&j);
        if (j.at < j.end && *j.at == ',') {
            ++j.at;
            continue;
        }
        return 0;
    }
}

/* A string member without escapes (every claim this hook reads is plain ASCII or UTF-8 without
   quotes): its bytes in `*text`, its length in `*size`. */
static int json_text(const char *json, size_t size, const char *key, const char **text, size_t *length) {
    const char *value, *end;
    if (!json_member(json, size, key, &value, &end) || end - value < 2 || *value != '"' || end[-1] != '"') {
        return 0;
    }
    for (const char *p = value + 1; p < end - 1; ++p) {
        if (*p == '\\' || (unsigned char)*p < 0x20) {
            return 0;
        }
    }
    *text = value + 1;
    *length = (size_t)(end - value - 2);
    return 1;
}

/* An unsigned integer member (JSON number, digits only, below 2^64). */
static int json_u64(const char *json, size_t size, const char *key, unsigned long long *out) {
    const char *value, *end;
    unsigned long long n = 0;
    if (!json_member(json, size, key, &value, &end) || value == end) {
        return 0;
    }
    for (const char *p = value; p < end; ++p) {
        if (*p < '0' || *p > '9' || n > (~0ull - (unsigned)(*p - '0')) / 10) {
            return 0;
        }
        n = n * 10 + (unsigned)(*p - '0');
    }
    *out = n;
    return 1;
}

static int launch_starts(const char *text, size_t size, const char *prefix) {
    size_t n = text_length(prefix);
    return size >= n && memory_equal(text, prefix, n);
}

static unsigned long long launch_unix_now(void) {
    FILETIME now;
    launch_api.system_time(&now);
    unsigned long long ticks = (unsigned long long)now.dwHighDateTime << 32 | now.dwLowDateTime;
    return ticks / 10000000ull - 11644473600ull;
}

/* What the ticket says, once it checked out. */
static struct {
    char xuid[24], gamertag[40], tier[16], mode[16], server[256];
    unsigned long long iat, exp;
    int storm_island;
} launch_claims;

static int launch_copy(char *out, size_t cap, const char *text, size_t size) {
    if (!size || size >= cap) {
        return 0;
    }
    memory_copy(out, text, size);
    out[size] = 0;
    return 1;
}

#ifdef FI_DEV
/* The --dev build also takes http://<127.0.0.1 or a private LAN IPv4>:<port> (OD-34 LAN tests;
   fi_launcher_core::devnet::plain_http_url). */
static int launch_plain_http_ok(const char *text, size_t length) {
    if (!launch_starts(text, length, "http://")) {
        return 0;
    }
    size_t i = 7;
    unsigned octet[4] = {0, 0, 0, 0};
    for (int k = 0; k < 4; ++k) {
        size_t start = i;
        while (i < length && text[i] >= '0' && text[i] <= '9' && i - start < 3) {
            octet[k] = octet[k] * 10u + (unsigned)(text[i] - '0');
            ++i;
        }
        if (i == start || octet[k] > 255u || i >= length || text[i] != (k < 3 ? '.' : ':')) {
            return 0;
        }
        ++i;
    }
    size_t port_start = i;
    unsigned port = 0;
    while (i < length && text[i] >= '0' && text[i] <= '9' && i - port_start < 5) {
        port = port * 10u + (unsigned)(text[i] - '0');
        ++i;
    }
    if (i == port_start || !port || port > 65535u || (i < length && text[i] != '/')) {
        return 0;
    }
    return (octet[0] == 127 && octet[1] == 0 && octet[2] == 0 && octet[3] == 1) || octet[0] == 10 ||
           (octet[0] == 172 && octet[1] >= 16 && octet[1] <= 31) || (octet[0] == 192 && octet[1] == 168);
}
#endif

/* The ticket's checks (ONLINE_OFFLINE.md 3.2, fi_launcher_core::session::verify_ticket): the
   strict Ed25519 signature under the key its kid names, v 1, this launch's nonce, a lifetime of
   at most 15 minutes, the PC clock within a day of it, an https:// server (a development build
   also takes http://127.0.0.1:). Then the identity it names. 1 when all hold. */
static int launch_ticket_ok(const LaunchTicket *t) {
    const char *claims = (const char *)t->claims, *text;
    size_t size = t->claims_size, length;
    if (!json_text(claims, size, "kid", &text, &length)) {
        launch_log("REFUSED: the ticket's claims have no kid: offline.", 0, 0, "");
        return 0;
    }
    const AzimuthLaunchKey *key = azimuth_sign_launch_key(text, length);
    if (!key || !key->present) {
        launch_log(key ? "REFUSED: this build has no public key yet for the ticket's kid: offline."
                       : "REFUSED: the ticket is signed with a key this build does not know: offline.",
                   0, 0, "");
        return 0;
    }
    if (!azimuth_sign_launch_ok(t->signature, t->claims, size, key->key)) {
        launch_log("REFUSED: the ticket's signature does not verify: offline.", 0, 0, "");
        return 0;
    }
    unsigned long long v = 0, iat = 0, exp = 0, xuid = 0, now = launch_unix_now();
    if (!json_u64(claims, size, "v", &v) || v != 1) {
        launch_log("REFUSED: the ticket is not version 1: offline.", 0, 0, "");
        return 0;
    }
    static const char digits[] = "0123456789abcdef";
    const unsigned char *nonce = fi_cfg()->launch_nonce;
    int same = json_text(claims, size, "nonce", &text, &length) && length == 32;
    for (size_t i = 0; same && i < 32; ++i) {
        char c = text[i] >= 'A' && text[i] <= 'F' ? (char)(text[i] + 32) : text[i];
        same = c == digits[i & 1 ? nonce[i / 2] & 15 : nonce[i / 2] >> 4];
    }
    if (!same) {
        launch_log("REFUSED: the ticket belongs to another launch (nonce): offline.", 0, 0, "");
        return 0;
    }
    if (!json_u64(claims, size, "iat", &iat) || !json_u64(claims, size, "exp", &exp) || exp <= iat ||
        exp - iat > LAUNCH_MAX_LIFETIME) {
        launch_log("REFUSED: the ticket's lifetime is out of bounds: offline.", 0, 0, "");
        return 0;
    }
    if (iat > now + LAUNCH_MAX_SKEW || exp + LAUNCH_MAX_SKEW < now) {
        launch_log("REFUSED: the ticket expired (or this PC's clock is more than a day off): offline.", 0, 0, "");
        return 0;
    }
    if (!json_text(claims, size, "server", &text, &length) ||
        !(launch_starts(text, length, "https://")
#ifdef FI_DEV
          || launch_plain_http_ok(text, length)
#endif
          )) {
        launch_log("REFUSED: the ticket's game server is not https: offline.", 0, 0, "");
        return 0;
    }
    if (!launch_copy(launch_claims.server, sizeof(launch_claims.server), text, length)) {
        launch_log("REFUSED: the ticket's game server address is too long: offline.", 0, 0, "");
        return 0;
    }
    if (!json_u64(claims, size, "xuid", &xuid) || !xuid || xuid >= 1ull << 53 ||
        !json_text(claims, size, "gamertag", &text, &length) ||
        !launch_copy(launch_claims.gamertag, sizeof(launch_claims.gamertag), text, length)) {
        launch_log("REFUSED: the ticket names no player (xuid, gamertag): offline.", 0, 0, "");
        return 0;
    }
    launch_claims.xuid[0] = 0;
    launch_number(launch_claims.xuid, sizeof(launch_claims.xuid), xuid);
    /* display only (T5): the server enforces the pool on every request */
    if (!json_text(claims, size, "tier", &text, &length) ||
        !launch_copy(launch_claims.tier, sizeof(launch_claims.tier), text, length)) {
        launch_copy(launch_claims.tier, sizeof(launch_claims.tier), "free", 4);
    }
    if (!json_text(claims, size, "mode", &text, &length) ||
        !launch_copy(launch_claims.mode, sizeof(launch_claims.mode), text, length)) {
        launch_copy(launch_claims.mode, sizeof(launch_claims.mode), "hardcore", 8);
    }
    /* CR-L11, a reserved claim: the products the profile owns (Storm Island) */
    const char *value, *end;
    launch_claims.storm_island = json_member(claims, size, "expansions", &value, &end) &&
                                 text_find(value, (size_t)(end - value), "06f06bff-35ee-4821-a289-fbc1765d6b76") != 0;
    launch_claims.iat = iat;
    launch_claims.exp = exp;
    return 1;
}

/* ---------------------------------------------------------------- token */

/* The token `token` (`size` bytes, printable ASCII) that expires at `expires_unix` (the
   server's clock) becomes the one game traffic carries: 1, or 0 when it is not a token. */
static int launch_set_token(const unsigned char *token, size_t size, unsigned long long expires_unix) {
    if (!size || size > LAUNCH_MAX_TOKEN) {
        return 0;
    }
    for (size_t i = 0; i < size; ++i) {
        if (token[i] <= 0x20 || token[i] >= 0x7f) {
            return 0;
        }
    }
    long long now = (long long)launch_unix_now() + launch_clock_offset;
    long long left = (long long)expires_unix - now;
    if (left < 0) {
        left = 0;
    } else if (left > 3600) {
        left = 3600; /* a session token lives 15 minutes (ACCOUNTS.md section 4) */
    }
    AcquireSRWLockExclusive(&launch_lock);
    memory_copy(launch_token, token, size);
    launch_token[size] = 0;
    launch_deadline_ms = GetTickCount64() + (unsigned long long)left * 1000u;
    ReleaseSRWLockExclusive(&launch_lock);
    InterlockedExchange(&launch_refused_401, 0);
    InterlockedExchange(&launch_refresh_wanted, 0);
    return 1;
}

/* Milliseconds the token has left (0 when there is none or it expired). */
static unsigned long long launch_token_left_ms(void) {
    unsigned long long now = GetTickCount64(), left = 0;
    AcquireSRWLockExclusive(&launch_lock);
    if (launch_token[0] && launch_deadline_ms > now) {
        left = launch_deadline_ms - now;
    }
    ReleaseSRWLockExclusive(&launch_lock);
    return left;
}

/* A live session: the ticket checked out, the game plane has not ended it, and the token has
   not expired (after the launcher is gone the game plays on until then, ORCHESTRATOR.md 1.6). */
int fi_launch_live(void) {
    return launch_state == LAUNCH_ONLINE && !launch_ended && launch_token_left_ms() > 0;
}

/* "X-Fi-Session: <token>\r\nX-Fi-Hook: <MOD_VERSION>" into `out`: its length, 0 without a live
   session (or when it does not fit). */
size_t fi_launch_headers(wchar_t *out, size_t cap) {
    static const char session[] = "X-Fi-Session: ", hook[] = "\r\nX-Fi-Hook: " MOD_VERSION;
    size_t n = 0;
    if (!fi_launch_live() || !cap) {
        return 0;
    }
    AcquireSRWLockExclusive(&launch_lock);
    for (const char *parts[3] = {session, launch_token, hook}, **p = parts; p < parts + 3; ++p) {
        for (const char *c = *p; *c && n + 1 < cap; ++c) {
            out[n++] = (wchar_t)(unsigned char)*c;
        }
    }
    ReleaseSRWLockExclusive(&launch_lock);
    out[n] = 0;
    size_t want = sizeof(session) - 1 + text_length(launch_token) + sizeof(hook) - 1;
    return n == want ? n : 0;
}

/* The game plane answered `status` to a request sent with the session (CR-S4,
   ONLINE_OFFLINE.md section 10): 401 and 403 are never passed to the game, which gets the
   local offline 503 instead (1). A first 401 (AUTH.SESSION_EXPIRED) asks the launcher for a
   new token; a second one before a new token arrived ends the session: offline from then on.
   403 (another pool, a refused car database or hook) refuses that request only. */
int fi_launch_refused(DWORD status) {
    if (!fi_launch_protocol() || (status != 401 && status != 403)) {
        return 0;
    }
    if (status == 401) {
        LONG count = InterlockedIncrement(&launch_refused_401);
        if (count == 1) {
            InterlockedExchange(&launch_refresh_wanted, 1);
            log_append("LAUNCH: the game plane refused the session token (401): asking the launcher for a new one.\r\n");
        } else if (count == 2 && !InterlockedExchange(&launch_ended, 1)) {
            log_append("LAUNCH: the game plane refused the session token again: online session ended, offline from now "
                       "on.\r\n");
        }
    } else {
        static volatile LONG logged;
        if (!InterlockedExchange(&logged, 1)) {
            log_append("LAUNCH: the game plane refused a request (403): the game gets the offline answer for it.\r\n");
        }
    }
    return 1;
}

/* The session goes live: the server, the token and the identity of the ticket. 1 when it did;
   0 when the game already read its identity (fi_launch_late won). */
static int launch_go_online(const LaunchTicket *t) {
    long long skew = (long long)launch_claims.iat - (long long)launch_unix_now();
    /* a ticket is minted at most 15 minutes before it arrives: a larger gap is this PC's clock */
    launch_clock_offset = skew > (long long)LAUNCH_MAX_LIFETIME || skew < -(long long)LAUNCH_MAX_LIFETIME ? skew : 0;
    if (!launch_set_token(t->token, t->token_size, t->token_expires)) {
        launch_log("REFUSED: the ticket's session token is not a token: offline.", 0, 0, "");
        return 0;
    }
    if (!azimuth_net_set_launch_server(launch_claims.server)) {
        launch_log("REFUSED: the ticket's game server address does not parse: offline.", 0, 0, "");
        return 0;
    }
    if (InterlockedCompareExchange(&launch_state, LAUNCH_ONLINE, LAUNCH_WAITING) != LAUNCH_WAITING) {
        launch_log("the ticket checked out after the game read its identity: offline this session.", 0, 0, "");
        return 0;
    }
    int free_tier = text_equal_ignore_case(launch_claims.tier, "free");
    int noob = text_equal_ignore_case(launch_claims.mode, "noob");
    azimuth_account_launch_identity(launch_claims.xuid, launch_claims.gamertag, free_tier, noob, launch_claims.storm_island);
    fi_tier_set_noob(noob); /* OD-52: a vip noob ticket gets the free answers */
    launch_log(free_tier ? "online through the launcher's session (free tier, display only); token for " :
                           "online through the launcher's session (Infinity Supporter, display only); token for ",
               1, launch_token_left_ms() / 1000u, " s.");
    return 1;
}

/* --------------------------------------------------------------- thread */

static void launch_close(void) {
    if (launch_pipe != INVALID_HANDLE_VALUE) {
        CloseHandle(launch_pipe);
        launch_pipe = INVALID_HANDLE_VALUE;
    }
}

/* The handshake: 1 online (the ticket checked out), 0 offline (launch_why set). */
static int launch_handshake(void) {
    static wchar_t name[64];
    static const wchar_t prefix[] = L"\\\\.\\pipe\\ForzaInfinity-";
    static const char digits[] = "0123456789abcdef";
    const unsigned char *nonce = fi_cfg()->launch_nonce;
    size_t n = 0;
    for (; prefix[n]; ++n) {
        name[n] = prefix[n];
    }
    for (int i = 0; i < 16; ++i) {
        name[n++] = (wchar_t)digits[nonce[i] >> 4];
        name[n++] = (wchar_t)digits[nonce[i] & 15];
    }
    name[n] = 0;
    if (!launch_api.wait_named_pipe(name, LAUNCH_PIPE_WAIT_MS)) {
        launch_log("the launcher's pipe did not answer within 5 s (error ", 1, GetLastError(), "): offline.");
        launch_fail(LAUNCH_WHY_NO_PIPE);
        return 0;
    }
    /* SQOS identification: whoever serves the name can identify this client but never
       impersonate it (the PID check below runs only after the open). */
    launch_pipe = CreateFileW(name, GENERIC_READ | GENERIC_WRITE, 0, 0, OPEN_EXISTING,
                              SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, 0);
    if (launch_pipe == INVALID_HANDLE_VALUE) {
        launch_log("the launcher's pipe could not be opened (error ", 1, GetLastError(), "): offline.");
        launch_fail(LAUNCH_WHY_NO_PIPE);
        return 0;
    }
    ULONG server = 0;
    DWORD parent = launch_parent_pid();
    if (!launch_api.server_pid(launch_pipe, &server) || !parent || server != parent) {
        launch_log("REFUSED: the pipe's server (pid ", 1, server, ") is not the process that started the game: offline.");
        launch_close();
        launch_fail(LAUNCH_WHY_PID);
        return 0;
    }
    size_t size = launch_encode_hello(launch_frame, sizeof(launch_frame), LAUNCH_PROTOCOL, GetCurrentProcessId(), MOD_VERSION);
    DWORD error = 0;
    if (!size || !launch_write(launch_frame, size)) {
        launch_log("the Hello frame could not be written (error ", 1, GetLastError(), "): offline.");
        launch_close();
        launch_fail(LAUNCH_WHY_NO_PIPE);
        return 0;
    }
    int got = launch_read_frame(LAUNCH_FRAME_WAIT_MS, &error);
    if (got <= 0) {
        if (got == -2) {
            launch_log("REFUSED: the launcher's first frame is longer than 64 KiB: offline.", 0, 0, "");
            launch_fail(LAUNCH_WHY_PROTOCOL);
        } else {
            launch_log(got ? "the launcher closed the pipe before its ticket (error " : "no ticket within 5 s (error ", 1,
                       error, "): offline.");
            launch_fail(LAUNCH_WHY_NO_PIPE);
        }
        launch_close();
        return 0;
    }
    unsigned type = launch_frame[4];
    if (type == FRAME_REFUSED) {
        LaunchReader r = {launch_frame + 5, launch_frame_size - 1, 0};
        size_t length;
        launch_blob(&r, &length); /* validate and consume; the launcher owns the visible reason */
        if (r.bad || r.left) {
            launch_log("REFUSED: a Refused frame that does not parse: offline.", 0, 0, "");
            launch_fail(LAUNCH_WHY_PROTOCOL);
        } else {
            launch_log("the launcher has no session for this start (Refused): offline.", 0, 0, "");
            launch_fail(LAUNCH_WHY_REFUSED);
        }
        launch_close();
        return 0;
    }
    LaunchTicket ticket;
    if (type != FRAME_TICKET || !launch_parse_ticket(&ticket)) {
        launch_log(type == FRAME_TICKET ? "REFUSED: a Ticket frame that does not parse: offline."
                                        : "REFUSED: the launcher's first frame is neither Ticket nor Refused (type ",
                   type != FRAME_TICKET, type, type == FRAME_TICKET ? "" : "): offline.");
        launch_close();
        launch_fail(LAUNCH_WHY_PROTOCOL);
        return 0;
    }
    if (ticket.claims_size > LAUNCH_MAX_CLAIMS || ticket.token_size > LAUNCH_MAX_TOKEN || !launch_ticket_ok(&ticket)) {
        if (ticket.claims_size > LAUNCH_MAX_CLAIMS || ticket.token_size > LAUNCH_MAX_TOKEN) {
            launch_log("REFUSED: the ticket's claims or token are longer than this hook takes: offline.", 0, 0, "");
        }
        launch_close();
        launch_fail(LAUNCH_WHY_TICKET);
        return 0;
    }
    if (!launch_go_online(&ticket)) {
        launch_close();
        launch_fail(launch_state == LAUNCH_LATE ? LAUNCH_WHY_LATE : LAUNCH_WHY_TICKET);
        return 0;
    }
    return 1;
}

/* The game has to start again (a profile or setting change that needs a new process): the
   launcher does it (it holds the session and the saves), so instead of starting azimuth.exe
   here the hook asks with RelaunchRequest{reason} and exits with EXIT_RELAUNCH, which the
   launcher's exit classification turns into a new launch (ORCHESTRATOR.md 1.5, B10). The
   frame gets 2 s to go; the exit code alone is enough when the pipe is gone. */
void fi_launch_relaunch(unsigned reason) {
    unsigned char frame[6] = {2, 0, 0, 0, FRAME_RELAUNCH_REQUEST, (unsigned char)reason};
    launch_queue(frame, sizeof(frame));
    for (unsigned waited = 0; waited < 2000 && launch_connected && !launch_outbox_flushed; waited += 20) {
        Sleep(20);
    }
    launch_log("RelaunchRequest (reason ", 1, reason, "): the launcher starts the game again.");
    ExitProcess(LAUNCH_EXIT_RELAUNCH);
}


/* The Overlay frame (0x14, ORCHESTRATOR.md 4.2): {u8 enabled, [32] sha256, str path}. */
static int launch_overlay_frame(LaunchReader *r);

/* One frame from the launcher during the session: 1 handled, 0 a protocol error. */
static int launch_session_frame(void) {
    LaunchReader r = {launch_frame + 5, launch_frame_size - 1, 0};
    unsigned type = launch_frame[4];
    size_t size;
    const unsigned char *text;
    switch (type) {
    case FRAME_TOKEN: {
        text = launch_blob(&r, &size);
        unsigned long long expires = launch_le(&r, 8);
        if (r.bad || r.left || !launch_set_token(text, size, expires)) {
            launch_log("REFUSED: a Token frame that does not parse.", 0, 0, "");
            return 0;
        }
        InterlockedExchange(&launch_refresh_sent, 0);
        launch_log("new session token from the launcher, for ", 1, launch_token_left_ms() / 1000u, " s.");
        return 1;
    }
    case FRAME_NOTICE: {
        unsigned level = (unsigned)launch_le(&r, 1);
        text = launch_blob(&r, &size);
        if (r.bad || r.left || level > 2) {
            launch_log("REFUSED: a Notice frame that does not parse.", 0, 0, "");
            return 0;
        }
        launch_log("the launcher's notice (level ", 1, level, ") was accepted; the launcher owns its overlay toast.");
        return 1;
    }
    case FRAME_OVERLAY:
        return launch_overlay_frame(&r);
    default:
        launch_log("REFUSED: a frame the launcher does not send during a session (type ", 1, type, ").");
        return 0;
    }
}

/* After the handshake, on the same thread: the launcher's frames (Token, Notice, Overlay, Bye),
   the queued frames for it, and a RefreshRequest when the token has less than 2 minutes. When
   the pipe goes, the game plays on with the token it has until that expires (the launcher has
   died or closed: ORCHESTRATOR.md 1.6), then offline. */
static void launch_session(void) {
    InterlockedExchange(&launch_connected, 1);
    for (;;) {
        unsigned long long left = launch_token_left_ms();
        if ((launch_refresh_wanted || (left && left < LAUNCH_REFRESH_MS)) && !InterlockedExchange(&launch_refresh_sent, 1)) {
            static const unsigned char refresh[5] = {1, 0, 0, 0, FRAME_REFRESH_REQUEST};
            launch_queue(refresh, sizeof(refresh));
            InterlockedExchange(&launch_refresh_wanted, 0);
            launch_log("RefreshRequest: the session token has ", 1, left / 1000u, " s left.");
        }
        if (!launch_flush()) {
            launch_log("launcher gone (the pipe broke on a write, error ", 1, GetLastError(),
                       "): playing on until the session token expires.");
            break;
        }
        DWORD error = 0;
        int got = launch_read_frame(LAUNCH_READ_POLL_MS, &error);
        if (got == 0) {
            continue;
        }
        if (got < 0) {
            if (got == -2) {
                launch_log("REFUSED: a frame longer than 64 KiB: the pipe is closed; playing on until the session token "
                           "expires.", 0, 0, "");
            } else {
                launch_log(error == ERROR_BROKEN_PIPE ? "launcher gone (broken pipe): playing on until the session token "
                                                        "expires."
                                                      : "launcher gone (the pipe failed, error ",
                           error != ERROR_BROKEN_PIPE, error,
                           error == ERROR_BROKEN_PIPE ? "" : "): playing on until the session token expires.");
            }
            break;
        }
        if (launch_frame[4] == FRAME_BYE) {
            LaunchReader r = {launch_frame + 5, launch_frame_size - 1, 0};
            unsigned reason = (unsigned)launch_le(&r, 1);
            launch_log("the launcher said Bye (reason ", 1, reason, "): playing on until the session token expires.");
            break;
        }
        if (!launch_session_frame()) {
            launch_log("the pipe is closed; playing on until the session token expires.", 0, 0, "");
            break;
        }
    }
    InterlockedExchange(&launch_connected, 0);
    InterlockedExchange(&launch_outbox_flushed, 1);
    launch_close();
}

static DWORD WINAPI launch_thread(void *unused) {
    (void)unused;
    int online = 0;
    if (!launch_resolve()) {
        log_append("LAUNCH: Kernel32 has no named pipe or Toolhelp32 functions: offline.\r\n");
        launch_fail(LAUNCH_WHY_NO_PIPE);
    } else {
        online = launch_handshake();
    }
    azimuth_account_launch_done();
    if (online) {
        launch_session();
    }
    return 0;
}

/* The overlay loader (ORCHESTRATOR.md 4.2, include/fi_overlay.h of fi-launcher-overlay): the
   launcher names infinity-overlay.dll and its SHA-256; a worker thread checks the file, loads it
   (LoadLibraryExW with the full path, never in DllMain) and calls fi_overlay_start once. The
   overlay talks to the launcher on its own -ui pipe; Capture{4, ok} tells the launcher. */
typedef struct {
    unsigned abi, size;
    const wchar_t *ui_pipe;
    void *(*capture)(unsigned what);
    void (*log)(const char *line);
    unsigned flags;
    int (*livery_sync)(unsigned timeout_ms); /* livery.c (OD-65 amendment) */
} LaunchOverlayHost;
typedef int (*LaunchOverlayStart)(const LaunchOverlayHost *host);

static wchar_t launch_overlay_path[PATH_CAPACITY];
static unsigned char launch_overlay_sha[32];
static wchar_t launch_ui_pipe[72];
static volatile LONG launch_overlay_started;

static void launch_overlay_log(const char *line) {
    char text[512];
    text[0] = 0;
    launch_text_add(text, sizeof(text), "OVERLAY: ");
    launch_text_add(text, sizeof(text), line);
    launch_text_add(text, sizeof(text), "\r\n");
    log_append(text);
}

static int launch_overlay_hash_ok(void) {
    HANDLE file = CreateFileW(launch_overlay_path, GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING, 0, 0);
    if (file == INVALID_HANDLE_VALUE) {
        launch_log("overlay: the DLL could not be opened (error ", 1, GetLastError(), ").");
        return 0;
    }
    static unsigned char chunk[65536];
    Sha256Context context;
    sha256_initialize(&context);
    DWORD got = 0;
    int ok = 1;
    for (;;) {
        if (!ReadFile(file, chunk, sizeof(chunk), &got, 0)) {
            ok = 0;
            break;
        }
        if (!got) {
            break;
        }
        sha256_update(&context, chunk, got);
    }
    CloseHandle(file);
    unsigned char digest[32];
    sha256_finalize(&context, digest);
    for (int i = 0; ok && i < 32; ++i) {
        ok = digest[i] == launch_overlay_sha[i];
    }
    if (!ok) {
        launch_log("REFUSED: the overlay DLL is not the file the launcher named (SHA-256).", 0, 0, "");
    }
    return ok;
}

static DWORD WINAPI launch_overlay_thread(void *unused) {
    (void)unused;
    int ok = 0;
    if (launch_overlay_hash_ok()) {
        HMODULE dll = LoadLibraryExW(launch_overlay_path, 0, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (!dll) {
            launch_log("overlay: LoadLibraryExW failed (error ", 1, GetLastError(), ").");
        } else {
            LaunchOverlayStart start = (LaunchOverlayStart)(void *)GetProcAddress(dll, "fi_overlay_start");
            if (!start) {
                launch_log("overlay: the DLL has no fi_overlay_start.", 0, 0, "");
            } else {
                LaunchOverlayHost host = {1u, (unsigned)sizeof(LaunchOverlayHost), launch_ui_pipe, 0,
                                          launch_overlay_log, 0, fi_livery_sync};
                int result = start(&host);
                ok = result == 0;
                launch_log(ok ? "overlay started (hotkey F4)." : "overlay: fi_overlay_start refused (",
                           !ok, (unsigned long long)(long long)result, ok ? "" : ").");
            }
        }
    }
    fi_launch_capture(4, ok);
    return 0;
}

static int launch_overlay_frame(LaunchReader *r) {
    unsigned enabled = (unsigned)launch_le(r, 1);
    if (r->left < 32) {
        launch_log("REFUSED: an Overlay frame that does not parse.", 0, 0, "");
        return 0;
    }
    for (int i = 0; i < 32; ++i) {
        launch_overlay_sha[i] = r->at[i];
    }
    r->at += 32;
    r->left -= 32;
    size_t size;
    const unsigned char *path = launch_blob(r, &size);
    if (r->bad || r->left) {
        launch_log("REFUSED: an Overlay frame that does not parse.", 0, 0, "");
        return 0;
    }
    if (!enabled) {
        launch_log("the overlay is off.", 0, 0, "");
        fi_launch_capture(4, 0);
        return 1;
    }
    if (InterlockedExchange(&launch_overlay_started, 1)) {
        return 1; /* one overlay per process */
    }
    int wide = size && size < PATH_CAPACITY
                   ? MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char *)path, (int)size,
                                         launch_overlay_path, PATH_CAPACITY - 1)
                   : 0;
    if (wide <= 0) {
        launch_log("REFUSED: the overlay path is empty or not UTF-8.", 0, 0, "");
        fi_launch_capture(4, 0);
        return 1;
    }
    launch_overlay_path[wide] = 0;
    /* The UI pipe is the control pipe's name plus -ui (overlay_ui::ui_pipe_name). */
    static const wchar_t prefix[] = L"\\\\.\\pipe\\ForzaInfinity-";
    static const char digits[] = "0123456789abcdef";
    const unsigned char *nonce = fi_cfg()->launch_nonce;
    size_t n = 0;
    for (; prefix[n]; ++n) {
        launch_ui_pipe[n] = prefix[n];
    }
    for (int i = 0; i < 16; ++i) {
        launch_ui_pipe[n++] = (wchar_t)digits[nonce[i] >> 4];
        launch_ui_pipe[n++] = (wchar_t)digits[nonce[i] & 15];
    }
    launch_ui_pipe[n++] = L'-';
    launch_ui_pipe[n++] = L'u';
    launch_ui_pipe[n++] = L'i';
    launch_ui_pipe[n] = 0;
    HANDLE thread = CreateThread(0, 0, launch_overlay_thread, 0, 0, 0);
    if (!thread) {
        launch_log("overlay: the loader thread could not start.", 0, 0, "");
        fi_launch_capture(4, 0);
        return 1;
    }
    CloseHandle(thread);
    launch_log("the launcher offers the overlay: loading it.", 0, 0, "");
    return 1;
}

/* account.c, instead of the Beta account thread: 1 when the thread runs. */
int fi_launch_start(const wchar_t *bin) {
    size_t n = 0;
    while (bin[n] && n + 1 < ARRAY_COUNT(launch_bin)) {
        launch_bin[n] = bin[n];
        ++n;
    }
    launch_bin[n] = 0;
    InterlockedExchange(&launch_state, LAUNCH_WAITING);
    HANDLE thread = CreateThread(0, 0, launch_thread, 0, 0, 0);
    if (!thread) {
        log_append("LAUNCH: the launch thread could not start: offline.\r\n");
        launch_fail(LAUNCH_WHY_NO_PIPE);
        return 0;
    }
    CloseHandle(thread);
    return 1;
}

/* The game asked who plays and the launcher has not answered in time: the session stays offline
   (an identity read now cannot change later). 1 when it did; 0 when the session went online just
   now (the caller waits for it). */
int fi_launch_late(void) {
    LONG before = InterlockedCompareExchange(&launch_state, LAUNCH_LATE, LAUNCH_WAITING);
    if (before == LAUNCH_WAITING) {
        InterlockedExchange(&launch_why, LAUNCH_WHY_LATE);
        log_append("LAUNCH: the game asked who plays before the launcher answered (5 s): offline this session.\r\n");
        return 1;
    }
    return before != LAUNCH_ONLINE;
}

int fi_launch_online(void) {
    return launch_state == LAUNCH_ONLINE;
}
