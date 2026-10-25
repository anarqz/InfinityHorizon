/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Idle in game (OD-121, GATE_API.md section 4a, GAME_PLATFORM.md section 3a; RE:
   re/afk-activity-signals.md section RE). The hook reports what the player is doing with the game's own
   30 s Notifications.RetrieveForUser, as X-Fi-Activity: v1.<seq>.<odo_m>.<idle_s>.<rp>, and passes
   the server's X-Fi-Activity-State to the launcher (Event 7), which warns through the overlay.

   Three vtable slots in the game's .data, each taken only when it holds the function this build is
   known to have there (any mismatch leaves it untouched):
   - EntityModel::PlayerCarStatsComponent slot 3 (the per-tick update): around each call, the
     growth of +0x160 (distance since the last bank) goes into the hook's own monotonic odometer,
     in metres. It survives banks (which zero +0x160) and car swaps. Only one instance is counted
     at a time (the local car's); another instance ticking while it does is counted as skipped;
   - EntityModel::GameInactivityComponent slot 2 (Update(dt)): +0x120 is the game's seconds since
     the last player input;
   - EntityModel::RichPresenceModule slot 4 (Update(dt)): +0x70 is the current screen id; it also
     samples the input manager's "input this frame" byte, the fallback when the inactivity
     component does not tick.
   All or nothing: without the build check and the odometer (and at least one idle source) no
   header is ever sent, so the server sees no_signal, never idle. Only within a launcher session.

   The wrappers are assembly thunks: the original gets its four register arguments (integer and
   floating point) and its return value exactly as the game passed them. Game memory is outside
   the signed files' integrity root; the report is the player's own claim (OD-121). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define ACT_STATS_SLOT_RVA 0x2caad70u      /* PlayerCarStatsComponent vtable 0x2caad58, slot 3 */
#define ACT_STATS_UPDATE_RVA 0x5e7520u     /* its value: a jmp to Update 0x5ebd60 */
#define ACT_INACTIVITY_SLOT_RVA 0x2c98e80u /* GameInactivityComponent vtable 0x2c98e70, slot 2 */
#define ACT_INACTIVITY_UPDATE_RVA 0x55cf90u
#define ACT_PRESENCE_SLOT_RVA 0x2cbb150u   /* RichPresenceModule vtable 0x2cbb130, slot 4 */
#define ACT_PRESENCE_UPDATE_RVA 0x6622c0u
#define ACT_INPUT_MANAGER_RVA 0x365d7c8u   /* *(this)+0xab98: input this frame */
#define ACT_INPUT_BYTE 0xab98u
#define ACT_DISTANCE 0x160u                /* float, distance since the last bank */
#define ACT_IDLE_SECONDS 0x120u            /* float, seconds since the last input */
#define ACT_SCREEN 0x70u                   /* int, the rich-presence enum */
#define ACT_FRESH_MS 5000u                 /* a source older than this is not ticking */
#define ACT_SWITCH_MS 5000u                /* the counted car may change after this long unseen */
#define ACT_MAX_STEP_M 1000.0              /* more than this in one tick is not driving */
#define ACT_MAX_ODO 9007199254740992ull    /* 2^53 */
#define ACT_MAX_IDLE 86400u
#define ACT_INSTANCES 8

uintptr_t fi_activity_stats_original, fi_activity_inactivity_original, fi_activity_presence_original;
uint64_t fi_activity_stats_before(void *self);
void fi_activity_stats_after(void *self, uint64_t before);
uint64_t fi_activity_none_before(void *self);
void fi_activity_inactivity_after(void *self, uint64_t before);
void fi_activity_presence_after(void *self, uint64_t before);
void fi_activity_stats_thunk(void);
void fi_activity_inactivity_thunk(void);
void fi_activity_presence_thunk(void);

/* rbx keeps `this`; the four register arguments and the original's rax/xmm0 are kept around the
   before/after calls (stack arguments are not: these updates take at most four). */
#define ACT_THUNK(name, before, after, original)                                                          \
    __asm__(".intel_syntax noprefix\n"                                                                    \
            ".text\n"                                                                                     \
            ".globl " #name "\n"                                                                          \
            ".p2align 4\n"                                                                                \
            ".seh_proc " #name "\n" #name ":\n"                                                           \
            "push rbx\n"                                                                                  \
            ".seh_pushreg rbx\n"                                                                          \
            "sub rsp, 0x70\n"                                                                             \
            ".seh_stackalloc 0x70\n"                                                                      \
            ".seh_endprologue\n"                                                                          \
            "mov rbx, rcx\n"                                                                              \
            "mov qword ptr [rsp+0x20], rdx\n"                                                             \
            "mov qword ptr [rsp+0x28], r8\n"                                                              \
            "mov qword ptr [rsp+0x30], r9\n"                                                              \
            "movsd qword ptr [rsp+0x38], xmm1\n"                                                          \
            "movsd qword ptr [rsp+0x40], xmm2\n"                                                          \
            "movsd qword ptr [rsp+0x48], xmm3\n"                                                          \
            "call " #before "\n"                                                                          \
            "mov qword ptr [rsp+0x50], rax\n"                                                             \
            "mov rcx, rbx\n"                                                                              \
            "mov rdx, qword ptr [rsp+0x20]\n"                                                             \
            "mov r8, qword ptr [rsp+0x28]\n"                                                              \
            "mov r9, qword ptr [rsp+0x30]\n"                                                              \
            "movsd xmm1, qword ptr [rsp+0x38]\n"                                                          \
            "movsd xmm2, qword ptr [rsp+0x40]\n"                                                          \
            "movsd xmm3, qword ptr [rsp+0x48]\n"                                                          \
            "call qword ptr [rip + " #original "]\n"                                                      \
            "mov qword ptr [rsp+0x58], rax\n"                                                             \
            "movsd qword ptr [rsp+0x60], xmm0\n"                                                          \
            "mov rcx, rbx\n"                                                                              \
            "mov rdx, qword ptr [rsp+0x50]\n"                                                             \
            "call " #after "\n"                                                                           \
            "mov rax, qword ptr [rsp+0x58]\n"                                                             \
            "movsd xmm0, qword ptr [rsp+0x60]\n"                                                          \
            "add rsp, 0x70\n"                                                                             \
            "pop rbx\n"                                                                                   \
            "ret\n"                                                                                       \
            ".seh_endproc\n"                                                                              \
            ".att_syntax prefix\n")

ACT_THUNK(fi_activity_stats_thunk, fi_activity_stats_before, fi_activity_stats_after, fi_activity_stats_original);
ACT_THUNK(fi_activity_inactivity_thunk, fi_activity_none_before, fi_activity_inactivity_after,
          fi_activity_inactivity_original);
ACT_THUNK(fi_activity_presence_thunk, fi_activity_none_before, fi_activity_presence_after,
          fi_activity_presence_original);

static unsigned char *act_game;
static volatile LONG act_ready;  /* 1: the odometer and an idle source are installed */
static volatile LONG act_has_inactivity, act_has_presence;
static SRWLOCK act_lock = SRWLOCK_INIT;
static double act_odometer;      /* metres, under act_lock */
static void *act_car;            /* the counted instance, under act_lock */
static unsigned long long act_car_at;
static void *act_seen[ACT_INSTANCES];
static unsigned act_seen_count, act_seen_overflow;
static unsigned long long act_skipped_ticks, act_rejected_ticks;
static double act_skipped_m;
static volatile LONG64 act_odometer_m;
static volatile LONG64 act_idle_ms, act_idle_at, act_input_at, act_presence_at, act_started_at;
static volatile LONG act_screen;
static volatile LONG64 act_seq;
static char act_last_state[48]; /* under act_lock */
static volatile LONG act_reports;

/* ------------------------------------------------------------------ the wrappers */

static unsigned long long act_now(void) { return GetTickCount64(); }

uint64_t fi_activity_none_before(void *self) {
    (void)self;
    return 0;
}

uint64_t fi_activity_stats_before(void *self) {
    uint32_t bits = 0;
    if (self) memory_copy(&bits, (unsigned char *)self + ACT_DISTANCE, sizeof(bits));
    return bits;
}

/* One tick of a stats component: the counted instance is the first one ticking, and keeps that
   role while it ticks at all (parked included); another instance takes over only once it has not
   ticked for ACT_SWITCH_MS (a car swap). Only the counted instance's growth goes in. */
void fi_activity_add_distance(void *self, float before, float after, unsigned long long now) {
    AcquireSRWLockExclusive(&act_lock);
    unsigned known = 0;
    for (unsigned i = 0; i < act_seen_count; ++i) known |= act_seen[i] == self;
    if (!known) {
        if (act_seen_count < ACT_INSTANCES) act_seen[act_seen_count++] = self;
        else ++act_seen_overflow;
    }
    int moved = float_is_finite(before) && float_is_finite(after) && after > before; /* else still, or banked */
    double step = moved ? (double)after - (double)before : 0.0;
    if (act_car != self && act_car && now - act_car_at < ACT_SWITCH_MS) {
        if (moved) {
            ++act_skipped_ticks; /* another component while the counted one ticks */
            act_skipped_m += step;
        }
    } else {
        act_car = self;
        act_car_at = now;
        if (step > ACT_MAX_STEP_M) {
            ++act_rejected_ticks;
        } else if (moved) {
            act_odometer += step;
            double whole = act_odometer < (double)ACT_MAX_ODO ? act_odometer : (double)ACT_MAX_ODO;
            InterlockedExchange64(&act_odometer_m, (LONG64)whole);
        }
    }
    ReleaseSRWLockExclusive(&act_lock);
}

void fi_activity_stats_after(void *self, uint64_t before) {
    float previous, current;
    uint32_t bits = (uint32_t)before;
    if (!self) return;
    memory_copy(&previous, &bits, sizeof(previous));
    memory_copy(&current, (unsigned char *)self + ACT_DISTANCE, sizeof(current));
    fi_activity_add_distance(self, previous, current, act_now());
}

void fi_activity_inactivity_after(void *self, uint64_t before) {
    float seconds;
    (void)before;
    if (!self) return;
    memory_copy(&seconds, (unsigned char *)self + ACT_IDLE_SECONDS, sizeof(seconds));
    if (!float_is_finite(seconds) || seconds < 0.0f) return;
    double ms = (double)seconds * 1000.0;
    InterlockedExchange64(&act_idle_ms, ms < (double)ACT_MAX_IDLE * 1000.0 ? (LONG64)ms : (LONG64)ACT_MAX_IDLE * 1000);
    InterlockedExchange64(&act_idle_at, (LONG64)act_now());
}

void fi_activity_presence_after(void *self, uint64_t before) {
    int screen;
    (void)before;
    if (!self) return;
    unsigned long long now = act_now();
    memory_copy(&screen, (unsigned char *)self + ACT_SCREEN, sizeof(screen));
    InterlockedExchange(&act_screen, screen < 0 ? 0 : screen > 255 ? 255 : screen);
    InterlockedExchange64(&act_presence_at, (LONG64)now);
    unsigned char *manager = 0;
    memory_copy(&manager, act_game + ACT_INPUT_MANAGER_RVA, sizeof(manager));
    if (manager && manager[ACT_INPUT_BYTE]) InterlockedExchange64(&act_input_at, (LONG64)now);
}

/* ------------------------------------------------------------------ installing */

static int act_image_ok(unsigned char *image) {
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    return image && memory_read(image, &dos, sizeof(dos)) && dos.e_magic == IMAGE_DOS_SIGNATURE &&
           dos.e_lfanew >= 64 && dos.e_lfanew <= 4096 && memory_read(image + dos.e_lfanew, &nt, sizeof(nt)) &&
           nt.Signature == IMAGE_NT_SIGNATURE && nt.FileHeader.Machine == IMAGE_FILE_MACHINE_AMD64 &&
           nt.FileHeader.TimeDateStamp == GAME_TIMESTAMP && nt.OptionalHeader.Magic == IMAGE_NT_OPTIONAL_HDR64_MAGIC &&
           nt.OptionalHeader.SizeOfImage == GAME_IMAGE_SIZE;
}

/* Swaps one slot when it holds the expected function: 1 when taken. */
static int act_take_slot(unsigned slot_rva, unsigned expected_rva, void (*thunk)(void), uintptr_t *original) {
    void **slot = (void **)(act_game + slot_rva);
    void *current = 0;
    DWORD old_protection, ignored;
    if (!memory_read(slot, &current, sizeof(current)) || current != (void *)(act_game + expected_rva) ||
        !VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old_protection)) {
        return 0;
    }
    *original = (uintptr_t)current;
    InterlockedExchangePointer(slot, (void *)(uintptr_t)thunk);
    VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
    return 1;
}

void fi_activity_apply(unsigned char *game_image) {
    if (!fi_launch_protocol() || !azimuth_net_proxy_enabled()) {
        return; /* only a launcher session reports */
    }
    if (!act_image_ok(game_image)) {
        log_append("ACTIVITY: not the supported game build: no activity report (the server sees no signal).\r\n");
        return;
    }
    act_game = game_image;
    InterlockedExchange64(&act_started_at, (LONG64)act_now());
    if (!act_take_slot(ACT_STATS_SLOT_RVA, ACT_STATS_UPDATE_RVA, fi_activity_stats_thunk, &fi_activity_stats_original)) {
        log_append("ACTIVITY: the car stats update is not where expected: no activity report.\r\n");
        return;
    }
    InterlockedExchange(&act_has_inactivity, act_take_slot(ACT_INACTIVITY_SLOT_RVA, ACT_INACTIVITY_UPDATE_RVA,
                                                           fi_activity_inactivity_thunk, &fi_activity_inactivity_original));
    InterlockedExchange(&act_has_presence, act_take_slot(ACT_PRESENCE_SLOT_RVA, ACT_PRESENCE_UPDATE_RVA,
                                                         fi_activity_presence_thunk, &fi_activity_presence_original));
    if (!act_has_inactivity && !act_has_presence) {
        log_append("ACTIVITY: neither idle source is where expected: no activity report.\r\n");
        return; /* the stats wrapper stays and only counts */
    }
    log_append(act_has_inactivity && act_has_presence ? "ACTIVITY: odometer, input idle and screen installed.\r\n"
               : act_has_inactivity ? "ACTIVITY: odometer and input idle installed (no screen).\r\n"
                                    : "ACTIVITY: odometer and screen installed (idle from the input byte).\r\n");
    InterlockedExchange(&act_ready, 1);
}

/* ------------------------------------------------------------------ the report */

static size_t act_put_number(char *out, size_t at, size_t cap, unsigned long long value) {
    char digits[20];
    unsigned count = 0;
    do {
        digits[count++] = (char)('0' + value % 10);
        value /= 10;
    } while (value);
    if (at + count >= cap) return 0;
    while (count) out[at++] = digits[--count];
    return at;
}

/* "v1.<seq>.<odo_m>.<idle_s>.<rp>" into `out` (NUL-terminated), each field clamped to the
   contract's range: its length, 0 when it does not fit. */
size_t fi_activity_format(unsigned long long seq, unsigned long long odo_m, unsigned long long idle_s, unsigned rp,
                          char *out, size_t cap) {
    unsigned long long fields[4] = {seq < ACT_MAX_ODO ? seq : ACT_MAX_ODO, odo_m < ACT_MAX_ODO ? odo_m : ACT_MAX_ODO,
                                    idle_s < ACT_MAX_IDLE ? idle_s : ACT_MAX_IDLE, rp < 255 ? rp : 255};
    size_t at = 0;
    if (!out || cap < 4) return 0;
    out[at++] = 'v';
    out[at++] = '1';
    for (unsigned i = 0; i < 4; ++i) {
        if (at + 1 >= cap) return 0;
        out[at++] = '.';
        if (!(at = act_put_number(out, at, cap, fields[i]))) return 0;
    }
    out[at] = 0;
    return at;
}

/* The value of header `name` (case-insensitive) in CRLF/LF header lines: its start and length. */
static const wchar_t *act_header(const wchar_t *headers, const wchar_t *name, size_t *length) {
    size_t name_length = 0;
    while (name[name_length]) ++name_length;
    for (const wchar_t *line = headers; line && *line;) {
        size_t n = 0, i = 0;
        while (line[n] && line[n] != L'\n') ++n;
        while (i < name_length && i < n) {
            wchar_t a = line[i], b = name[i];
            if (a >= L'A' && a <= L'Z') a = (wchar_t)(a + 32);
            if (b >= L'A' && b <= L'Z') b = (wchar_t)(b + 32);
            if (a != b) break;
            ++i;
        }
        if (i == name_length && i < n && line[i] == L':') {
            const wchar_t *value = line + i + 1;
            size_t left = n - i - 1;
            while (left && (*value == L' ' || *value == L'\t')) ++value, --left;
            while (left && (value[left - 1] == L'\r' || value[left - 1] == L' ' || value[left - 1] == L'\t')) --left;
            *length = left;
            return value;
        }
        line += line[n] ? n + 1 : n;
    }
    return 0;
}

static int act_is(const wchar_t *text, size_t length, const char *expected) {
    size_t i = 0;
    for (; i < length && expected[i]; ++i) {
        if (text[i] != (wchar_t)expected[i]) return 0;
    }
    return i == length && !expected[i];
}

/* The report line for a request with these headers ("X-Fi-Activity: v1...", no CRLF): its
   length, 0 when nothing is sent (not Xls.WebServices.Notifications.RetrieveForUser, not
   installed, or no idle source ticking now: a ping without a fresh one would claim idleness). */
size_t fi_activity_header(const wchar_t *request_headers, wchar_t *out, size_t cap) {
    static const char name[] = "X-Fi-Activity: ";
    size_t method_length = 0, class_length = 0;
    const wchar_t *method = act_header(request_headers, L"X-Method", &method_length);
    const wchar_t *class_name = act_header(request_headers, L"X-ClassName", &class_length);
    size_t comma = 0;
    while (class_name && comma < class_length && class_name[comma] != L',') ++comma;
    while (comma && class_name[comma - 1] == L' ') --comma;
    if (!act_ready || !method || !act_is(method, method_length, "RetrieveForUser") || !class_name ||
        !act_is(class_name, comma, "Xls.WebServices.Notifications")) {
        return 0;
    }
    unsigned long long now = act_now(), idle_ms;
    int presence_fresh = act_has_presence && act_presence_at && now - (unsigned long long)act_presence_at <= ACT_FRESH_MS;
    if (act_has_inactivity && act_idle_at && now - (unsigned long long)act_idle_at <= ACT_FRESH_MS) {
        idle_ms = (unsigned long long)act_idle_ms + (now - (unsigned long long)act_idle_at);
    } else if (presence_fresh) {
        /* the game's timer does not tick (gated): seconds since the input byte was last seen */
        unsigned long long since = (unsigned long long)(act_input_at > act_started_at ? act_input_at : act_started_at);
        idle_ms = now > since ? now - since : 0;
    } else {
        return 0;
    }
    unsigned screen = presence_fresh ? (unsigned)act_screen : 0;
    char value[96];
    size_t length = fi_activity_format((unsigned long long)InterlockedIncrement64(&act_seq),
                                       (unsigned long long)act_odometer_m, idle_ms / 1000, screen, value, sizeof(value));
    if (!length || sizeof(name) - 1 + length + 1 > cap) return 0;
    size_t at = 0;
    for (size_t i = 0; i < sizeof(name) - 1; ++i) out[at++] = (wchar_t)name[i];
    for (size_t i = 0; i < length; ++i) out[at++] = (wchar_t)value[i];
    out[at] = 0;
    LONG reports = InterlockedIncrement(&act_reports);
    if (reports % 10 == 1) {
        /* evidence for the live check (T-AFK4): how many stats instances ticked, and what was skipped */
        unsigned long long skipped, rejected, skipped_m, seen;
        AcquireSRWLockShared(&act_lock);
        skipped = act_skipped_ticks;
        rejected = act_rejected_ticks;
        skipped_m = (unsigned long long)act_skipped_m;
        seen = act_seen_count + act_seen_overflow;
        ReleaseSRWLockShared(&act_lock);
        char line[200];
        size_t n = 0;
        static const char a[] = "ACTIVITY: report ", b[] = "; stats instances ", c[] = ", skipped ticks ",
                          d[] = " (", e[] = " m), rejected ", f[] = "; idle from ";
        const char *parts[] = {a, b, c, d, e, f};
        unsigned long long numbers[] = {(unsigned long long)reports, seen, skipped, skipped_m, rejected};
        for (unsigned i = 0; i < 6; ++i) {
            for (const char *p = parts[i]; *p && n + 1 < sizeof(line); ++p) line[n++] = *p;
            if (i < 5 && !(n = act_put_number(line, n, sizeof(line), numbers[i]))) return at;
        }
        const char *source = act_has_inactivity && act_idle_at && now - (unsigned long long)act_idle_at <= ACT_FRESH_MS
                                 ? "the game.\r\n"
                                 : "the input byte.\r\n";
        for (const char *p = source; *p && n + 1 < sizeof(line); ++p) line[n++] = *p;
        line[n] = 0;
        log_append(line);
    }
    return at;
}

/* X-Fi-Activity-State's value, checked: "active|menu|idle|no_signal[;evict_in=N]" (N 1 to 6
   digits). 1 when valid, with the value copied to `out` and N (0 when absent) to `evict_in`. */
int fi_activity_parse_state(const wchar_t *value, size_t length, char *out, size_t cap, unsigned *evict_in) {
    static const char *const states[] = {"active", "menu", "idle", "no_signal"};
    static const char evict[] = ";evict_in=";
    size_t state = 0;
    *evict_in = 0;
    if (!value || length + 1 > cap || length > 40) return 0;
    while (state < length && value[state] != L';') ++state;
    int known = 0;
    for (unsigned i = 0; i < 4; ++i) known |= act_is(value, state, states[i]);
    if (!known) return 0;
    if (state < length) {
        size_t at = state, digits = 0;
        unsigned seconds = 0;
        for (size_t i = 0; evict[i]; ++i, ++at) {
            if (at >= length || value[at] != (wchar_t)evict[i]) return 0;
        }
        for (; at < length; ++at, ++digits) {
            if (value[at] < L'0' || value[at] > L'9' || digits == 6) return 0;
            seconds = seconds * 10 + (unsigned)(value[at] - L'0');
        }
        if (!digits) return 0;
        *evict_in = seconds;
    }
    for (size_t i = 0; i < length; ++i) out[i] = (char)value[i];
    out[length] = 0;
    return 1;
}

/* The server's answer to a request that carried the report: a new state (or any eviction
   countdown) goes to the launcher as Event 7, which warns through the overlay. */
void fi_activity_answer(const wchar_t *response_headers) {
    size_t length = 0;
    const wchar_t *value = act_header(response_headers, L"X-Fi-Activity-State", &length);
    char state[48];
    unsigned evict_in = 0;
    if (!act_ready || !value) return;
    if (!fi_activity_parse_state(value, length, state, sizeof(state), &evict_in)) {
        log_append("ACTIVITY: the server's activity state is malformed: ignored.\r\n");
        return;
    }
    size_t same = 0;
    AcquireSRWLockExclusive(&act_lock);
    while (state[same] && state[same] == act_last_state[same]) ++same;
    int unchanged = !state[same] && !act_last_state[same];
    if (!unchanged) memory_copy(act_last_state, state, length + 1);
    ReleaseSRWLockExclusive(&act_lock);
    if (!evict_in && unchanged) return;
    if (!evict_in) {
        log_append("ACTIVITY: the server now sees this session as ");
        log_append(state);
        log_append(".\r\n");
    }
    fi_launch_event(FI_EVENT_ACTIVITY, state);
}
