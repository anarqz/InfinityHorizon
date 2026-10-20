/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   <Azimuth>\Infinity.cfg, the only settings file the hook reads (PLAN.md "Infinity.cfg [hook]").
   INI syntax as Kernel32's GetPrivateProfileStringW reads it: [Section] lines, Key=Value lines,
   names case-insensitive, spaces around them trimmed, only the first section of a name is read
   (a second [hook] or [relay] is ignored, as Kernel32, Wine and the launcher do), the first of
   two equal keys wins, a value in quotes loses them, comments on their own line (";"), no inline
   comments. The file is
   decoded here rather than through Kernel32 so that the hook reads it the same way on Windows
   and Wine: UTF-16LE with BOM (what the launcher writes), UTF-8 (without BOM, read as UTF-8 when
   it is valid UTF-8, else in the ANSI code page as Kernel32 would; a UTF-8 BOM is skipped, with a
   warning), at most 64 KiB.

   [hook] is ours (the table below). [relay] belongs to the launcher; the hook reads it for the
   world it hosts when the relay is the launcher's infinity.exe (world.c). [launcher] is never
   read. ExtraOptions' <Azimuth>\FH2_ExtraOptions.ini is never read or written.

   The one write: when Infinity.cfg has no [hook] (no launcher yet, or a launcher that leaves the
   section to the hook), the ForzaInfinity keys of hooks up to 0.2.74+fh2online.58, kept in
   data\bin\FH2_ExtraOptions.ini ([Network] Mode, Server, AutoUpdate; [FreeRoam] Enabled, Direct,
   World), go into a [hook] section appended to the file (created UTF-16LE with BOM when there is
   none), in the file's own encoding, through a temporary file and an atomic rename, every byte
   before it unchanged. Only values that differ from the defaults are written. That INI is then
   not read again (it is not changed or removed). ExtraOptions' keys in it are never copied. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define CFG_MAX_BYTES (64u * 1024u)
#define CFG_LOG_CAP 4096u
#define CFG_DEFAULT_SERVER L"https://infinity.anarqz.dev"

static FiConfig cfg_values;
static int cfg_migrated; /* this start brought the older hooks' INI over */
static wchar_t cfg_file[PATH_CAPACITY], cfg_legacy[PATH_CAPACITY], cfg_temp[PATH_CAPACITY];
static char cfg_log_lines[CFG_LOG_CAP];
static size_t cfg_log_used;
/* the decoded text of the file being read (one at a time: DllMain, then HOST WORLD) */
static wchar_t cfg_text[CFG_MAX_BYTES + 1];
static unsigned char cfg_bytes[CFG_MAX_BYTES + 2];
static unsigned char cfg_out[2 * CFG_MAX_BYTES + 4096];
static SRWLOCK cfg_lock = SRWLOCK_INIT;

/* ------------------------------------------------------------------ text */

static size_t wlen(const wchar_t *t) {
    size_t n = 0;
    while (t && t[n]) {
        ++n;
    }
    return n;
}

static wchar_t wlower(wchar_t c) {
    return c >= L'A' && c <= L'Z' ? (wchar_t)(c + 32) : c;
}

/* `a` (length n) equals the NUL-terminated `b`, ignoring ASCII case. */
static int wequal_n(const wchar_t *a, size_t n, const wchar_t *b) {
    size_t m = wlen(b);
    if (m != n) {
        return 0;
    }
    for (size_t i = 0; i < n; ++i) {
        if (wlower(a[i]) != wlower(b[i])) {
            return 0;
        }
    }
    return 1;
}

static int wequal(const wchar_t *a, const wchar_t *b) {
    return wequal_n(a, wlen(a), b);
}

static int wstarts(const wchar_t *text, const wchar_t *prefix) {
    for (size_t i = 0; prefix[i]; ++i) {
        if (wlower(text[i]) != wlower(prefix[i])) {
            return 0;
        }
    }
    return 1;
}

static void wcopy(wchar_t *out, size_t cap, const wchar_t *in, size_t n) {
    if (n >= cap) {
        n = cap - 1;
    }
    memory_copy(out, in, n * sizeof(wchar_t));
    out[n] = 0;
}

/* No CRT here: a zero-initialised struct would be a memset call. */
static void cfg_zero(void *p, size_t n) {
    volatile unsigned char *b = p;
    while (n--) {
        *b++ = 0;
    }
}

static void cfg_note(const char *text) {
    size_t n = text_length(text);
    if (cfg_log_used + n + 1 < CFG_LOG_CAP) {
        memory_copy(cfg_log_lines + cfg_log_used, text, n);
        cfg_log_used += n;
        cfg_log_lines[cfg_log_used] = 0;
    }
}

/* Appends `text` as ASCII ('?' for anything else: the log is ASCII). */
static void cfg_note_wide(const wchar_t *text) {
    char ascii[300];
    size_t n = 0;
    for (; text && text[n] && n + 1 < sizeof(ascii); ++n) {
        ascii[n] = text[n] >= 0x20 && text[n] < 0x7f ? (char)text[n] : '?';
    }
    ascii[n] = 0;
    cfg_note(ascii);
}

/* ------------------------------------------------------------- reading */

/* `path` decoded into cfg_text: 1 read, 0 no file, -1 not readable (a CFG: line says why).
   `*encoding` is what it was written in. */
static int cfg_decode(const wchar_t *path, int *encoding, size_t *byte_count, const char *what) {
    HANDLE file = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, 0, OPEN_EXISTING,
                              FILE_ATTRIBUTE_NORMAL, 0);
    *encoding = FI_CFG_NONE;
    *byte_count = 0;
    cfg_text[0] = 0;
    if (file == INVALID_HANDLE_VALUE) {
        return GetLastError() == ERROR_FILE_NOT_FOUND || GetLastError() == ERROR_PATH_NOT_FOUND ? 0 : -1;
    }
    LARGE_INTEGER size;
    DWORD got = 0;
    if (!GetFileSizeEx(file, &size) || size.QuadPart > CFG_MAX_BYTES) {
        CloseHandle(file);
        cfg_note("CFG: ");
        cfg_note(what);
        cfg_note(" is larger than 64 KiB; not read.\r\n");
        return -1;
    }
    int ok = ReadFile(file, cfg_bytes, (DWORD)size.QuadPart, &got, 0) && got == (DWORD)size.QuadPart;
    CloseHandle(file);
    if (!ok) {
        cfg_note("CFG: ");
        cfg_note(what);
        cfg_note(" could not be read.\r\n");
        return -1;
    }
    *byte_count = got;
    const unsigned char *b = cfg_bytes;
    size_t chars = 0;
    if (got >= 2 && b[0] == 0xff && b[1] == 0xfe) {
        *encoding = FI_CFG_UTF16LE;
        for (size_t i = 2; i + 1 < got; i += 2) {
            cfg_text[chars++] = (wchar_t)(b[i] | b[i + 1] << 8);
        }
    } else if (got >= 2 && b[0] == 0xfe && b[1] == 0xff) {
        cfg_note("CFG: ");
        cfg_note(what);
        cfg_note(" is UTF-16 big-endian; not read (save it as UTF-16LE or UTF-8).\r\n");
        return -1;
    } else {
        size_t skip = got >= 3 && b[0] == 0xef && b[1] == 0xbb && b[2] == 0xbf ? 3 : 0;
        int n = 0;
        if (got > skip) {
            n = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char *)b + skip, (int)(got - skip), cfg_text,
                                    (int)CFG_MAX_BYTES);
            *encoding = skip ? FI_CFG_UTF8_BOM : FI_CFG_UTF8;
            if (n <= 0) {
                n = MultiByteToWideChar(CP_ACP, 0, (const char *)b + skip, (int)(got - skip), cfg_text,
                                        (int)CFG_MAX_BYTES);
                *encoding = FI_CFG_ANSI;
            }
        } else {
            *encoding = skip ? FI_CFG_UTF8_BOM : FI_CFG_UTF8;
        }
        chars = n > 0 ? (size_t)n : 0;
    }
    cfg_text[chars] = 0;
    return 1;
}

/* One line of cfg_text: [start, end), without its line break. */
typedef struct {
    const wchar_t *start, *end;
} CfgSpan;

static void cfg_trim(CfgSpan *s) {
    while (s->start < s->end && (*s->start == L' ' || *s->start == L'\t')) {
        ++s->start;
    }
    while (s->end > s->start && (s->end[-1] == L' ' || s->end[-1] == L'\t')) {
        --s->end;
    }
}

/* The value of `key` in the first [`section`] of cfg_text (into `out`): 1 found. Also tells
   whether the section exists at all. */
static int cfg_lookup(const wchar_t *section, const wchar_t *key, wchar_t *out, size_t cap, int *section_found) {
    const wchar_t *p = cfg_text;
    int in_section = 0, seen = 0;
    out[0] = 0;
    while (*p) {
        CfgSpan line = {p, p};
        while (*line.end && *line.end != L'\n' && *line.end != L'\r') {
            ++line.end;
        }
        p = line.end;
        while (*p == L'\r' || *p == L'\n') {
            ++p;
        }
        cfg_trim(&line);
        if (line.start == line.end || *line.start == L';') {
            continue;
        }
        if (*line.start == L'[') {
            if (in_section) {
                break; /* the end of the first section of that name: later ones are not read */
            }
            const wchar_t *close = line.start + 1;
            while (close < line.end && *close != L']') {
                ++close;
            }
            CfgSpan name = {line.start + 1, close};
            cfg_trim(&name);
            in_section = wequal_n(name.start, (size_t)(name.end - name.start), section);
            seen |= in_section;
            continue;
        }
        if (!in_section) {
            continue;
        }
        const wchar_t *equal = line.start;
        while (equal < line.end && *equal != L'=') {
            ++equal;
        }
        if (equal >= line.end) {
            continue;
        }
        CfgSpan name = {line.start, equal}, value = {equal + 1, line.end};
        cfg_trim(&name);
        cfg_trim(&value);
        if (!wequal_n(name.start, (size_t)(name.end - name.start), key)) {
            continue;
        }
        if (value.end - value.start >= 2 &&
            ((*value.start == L'"' && value.end[-1] == L'"') || (*value.start == L'\'' && value.end[-1] == L'\''))) {
            ++value.start;
            --value.end;
        }
        wcopy(out, cap, value.start, (size_t)(value.end - value.start));
        if (section_found) {
            *section_found = 1;
        }
        return 1;
    }
    if (section_found) {
        *section_found = seen;
    }
    return 0;
}

/* How many [section] headers of that name the decoded file has. */
static unsigned cfg_section_count(const wchar_t *section) {
    unsigned count = 0;
    for (const wchar_t *p = cfg_text; *p;) {
        CfgSpan line = {p, p};
        while (*line.end && *line.end != L'\n' && *line.end != L'\r') {
            ++line.end;
        }
        p = line.end;
        while (*p == L'\r' || *p == L'\n') {
            ++p;
        }
        cfg_trim(&line);
        if (line.start < line.end && *line.start == L'[') {
            const wchar_t *close = line.start + 1;
            while (close < line.end && *close != L']') {
                ++close;
            }
            CfgSpan name = {line.start + 1, close};
            cfg_trim(&name);
            count += (unsigned)wequal_n(name.start, (size_t)(name.end - name.start), section);
        }
    }
    return count;
}

static int cfg_has_section(const wchar_t *section) {
    wchar_t unused[2];
    int found = 0;
    cfg_lookup(section, L"\x1f", unused, ARRAY_COUNT(unused), &found);
    return found;
}

/* --------------------------------------------------------- the [hook] keys */

static void cfg_defaults(FiConfig *c) {
    cfg_zero(c, sizeof(*c));
    c->net_mode = FI_NET_PROXY;
    wcopy(c->server, ARRAY_COUNT(c->server), CFG_DEFAULT_SERVER, wlen(CFG_DEFAULT_SERVER));
    c->auto_update = 1;
    c->freeroam = 1;
    c->carstate = FI_CARSTATE_AUTO;
    c->language = FI_LANG_AUTO;
    c->log = 1;
}

static void cfg_invalid(const wchar_t *key, const wchar_t *value, const char *fallback) {
    cfg_note("CFG: [hook] ");
    cfg_note_wide(key);
    cfg_note("=");
    cfg_note_wide(value);
    cfg_note(" is not valid; using ");
    cfg_note(fallback);
    cfg_note(".\r\n");
}

/* host[:port] after an optional http:// or https://: 1 when it is a server address. */
static int cfg_server_ok(const wchar_t *v) {
    if (wstarts(v, L"https://")) {
        v += 8;
    } else if (wstarts(v, L"http://")) {
        v += 7;
    }
    size_t host = 0;
    while (v[host] && v[host] != L':' && v[host] != L'/') {
        wchar_t c = v[host];
        if (c <= L' ' || c == L'"' || c == L'\\' || c >= 0x7f) {
            return 0;
        }
        ++host;
    }
    if (!host || host > 253) {
        return 0;
    }
    if (v[host] == L':') {
        unsigned port = 0, digits = 0;
        for (size_t i = host + 1; v[i] && v[i] != L'/'; ++i, ++digits) {
            if (v[i] < L'0' || v[i] > L'9' || (port = port * 10 + (unsigned)(v[i] - L'0')) > 65535) {
                return 0;
            }
        }
        if (!digits || !port) {
            return 0;
        }
    }
    return 1;
}

static int cfg_bool(const wchar_t *key, const wchar_t *value, int fallback) {
    if (wequal(value, L"1")) {
        return 1;
    }
    if (wequal(value, L"0")) {
        return 0;
    }
    cfg_invalid(key, value, fallback ? "1" : "0");
    return fallback;
}

/* The [hook] values of cfg_text over the defaults in `c`. */
static void cfg_read_hook(FiConfig *c) {
    wchar_t v[300];
    if (cfg_lookup(L"hook", L"NetMode", v, ARRAY_COUNT(v), 0)) {
        if (wequal(v, L"Proxy")) {
            c->net_mode = FI_NET_PROXY;
        } else if (wequal(v, L"Capture")) {
            c->net_mode = FI_NET_CAPTURE;
        } else if (wequal(v, L"Off")) {
            c->net_mode = FI_NET_OFF;
        } else {
            cfg_invalid(L"NetMode", v, "Proxy");
        }
    }
    if (cfg_lookup(L"hook", L"Server", v, ARRAY_COUNT(v), 0) && v[0]) {
        if (cfg_server_ok(v) && wlen(v) < ARRAY_COUNT(c->server)) {
            wcopy(c->server, ARRAY_COUNT(c->server), v, wlen(v));
        } else {
            cfg_invalid(L"Server", v, "https://infinity.anarqz.dev");
        }
    }
    if (cfg_lookup(L"hook", L"AutoUpdate", v, ARRAY_COUNT(v), 0)) {
        c->auto_update = cfg_bool(L"AutoUpdate", v, 1);
    }
    if (cfg_lookup(L"hook", L"FreeRoam", v, ARRAY_COUNT(v), 0)) {
        c->freeroam = cfg_bool(L"FreeRoam", v, 1);
    }
    if (cfg_lookup(L"hook", L"FreeRoamDirect", v, ARRAY_COUNT(v), 0)) {
        c->freeroam_direct = cfg_bool(L"FreeRoamDirect", v, 0);
    }
    if (cfg_lookup(L"hook", L"FreeRoamWorld", v, ARRAY_COUNT(v), 0)) {
        if (!v[0] || (cfg_server_ok(v) && wlen(v) < ARRAY_COUNT(c->freeroam_world) && !wstarts(v, L"http"))) {
            wcopy(c->freeroam_world, ARRAY_COUNT(c->freeroam_world), v, wlen(v));
        } else {
            cfg_invalid(L"FreeRoamWorld", v, "none");
        }
    }
    if (cfg_lookup(L"hook", L"CarState", v, ARRAY_COUNT(v), 0)) {
        if (wequal(v, L"auto")) {
            c->carstate = FI_CARSTATE_AUTO;
        } else if (wequal(v, L"off")) {
            c->carstate = FI_CARSTATE_OFF;
        } else {
            cfg_invalid(L"CarState", v, "auto");
        }
    }
    if (cfg_lookup(L"hook", L"Language", v, ARRAY_COUNT(v), 0)) {
        if (wequal(v, L"auto")) {
            c->language = FI_LANG_AUTO;
        } else if (wequal(v, L"en")) {
            c->language = FI_LANG_EN;
        } else if (wequal(v, L"pt-BR")) {
            c->language = FI_LANG_PT_BR;
        } else {
            cfg_invalid(L"Language", v, "auto");
        }
    }
    if (cfg_lookup(L"hook", L"Log", v, ARRAY_COUNT(v), 0)) {
        c->log = cfg_bool(L"Log", v, 1);
    }
    if (cfg_lookup(L"hook", L"RelayCommand", v, ARRAY_COUNT(v), 0)) {
        if (wequal(v, L"auto")) {
            c->relay_legacy = 0;
        } else if (wequal(v, L"legacy")) {
            c->relay_legacy = 1;
        } else {
            cfg_invalid(L"RelayCommand", v, "auto");
        }
    }
}

/* ------------------------------------------------------------ migration */

/* The hook's own keys of data\bin\FH2_ExtraOptions.ini (in cfg_text), with the meaning hooks up
   to 0.2.74+fh2online.58 gave them (net.c and freeroam.c read them with these rules; the INI
   merge of those hooks wrote Mode=Proxy and the default Server when they were missing). */
/* A value of that INI without a comment after it (" ;", as players wrote them there). */
static int cfg_legacy_value(const wchar_t *section, const wchar_t *key, wchar_t *out, size_t cap) {
    if (!cfg_lookup(section, key, out, cap, 0)) {
        return 0;
    }
    for (size_t i = 1; out[i]; ++i) {
        if (out[i] == L';' && (out[i - 1] == L' ' || out[i - 1] == L'\t')) {
            size_t end = i - 1;
            while (end && (out[end - 1] == L' ' || out[end - 1] == L'\t')) {
                --end;
            }
            out[end] = 0;
            break;
        }
    }
    return 1;
}

static void cfg_read_legacy(FiConfig *c, char *migrated, size_t cap) {
    wchar_t v[300];
    migrated[0] = 0;
#define CFG_MIGRATED(name)                                                                         \
    do {                                                                                           \
        size_t n_ = text_length(migrated), k_ = text_length(name);                                 \
        if (n_ + k_ + 3 < cap) {                                                                   \
            if (n_) {                                                                              \
                migrated[n_++] = ',', migrated[n_++] = ' ';                                        \
            }                                                                                      \
            memory_copy(migrated + n_, name, k_ + 1);                                              \
        }                                                                                          \
    } while (0)
    if (cfg_legacy_value(L"Network", L"Mode", v, ARRAY_COUNT(v))) {
        int mode = wstarts(v, L"proxy") ? FI_NET_PROXY : wstarts(v, L"capture") ? FI_NET_CAPTURE : FI_NET_OFF;
        if (mode != c->net_mode) {
            c->net_mode = mode;
            CFG_MIGRATED("NetMode");
        }
    }
    if (cfg_legacy_value(L"Network", L"Server", v, ARRAY_COUNT(v)) && v[0] && !wequal(v, L"__SERVER__") &&
        !wequal(v, c->server)) {
        if (cfg_server_ok(v) && wlen(v) < ARRAY_COUNT(c->server)) {
            wcopy(c->server, ARRAY_COUNT(c->server), v, wlen(v));
            CFG_MIGRATED("Server");
        } else {
            cfg_note("CFG: data\\bin\\FH2_ExtraOptions.ini [Network] Server is not a server address; not migrated.\r\n");
        }
    }
    if (cfg_legacy_value(L"Network", L"AutoUpdate", v, ARRAY_COUNT(v)) && (v[0] == L'0' || wstarts(v, L"off"))) {
        c->auto_update = 0;
        CFG_MIGRATED("AutoUpdate");
    }
    if (cfg_legacy_value(L"FreeRoam", L"Enabled", v, ARRAY_COUNT(v)) && v[0] == L'0') {
        c->freeroam = 0;
        CFG_MIGRATED("FreeRoam");
    }
    if (cfg_legacy_value(L"FreeRoam", L"Direct", v, ARRAY_COUNT(v)) && v[0] == L'1') {
        c->freeroam_direct = 1;
        CFG_MIGRATED("FreeRoamDirect");
    }
    if (cfg_legacy_value(L"FreeRoam", L"World", v, ARRAY_COUNT(v)) && v[0]) {
        if (cfg_server_ok(v) && !wstarts(v, L"http") && wlen(v) < ARRAY_COUNT(c->freeroam_world)) {
            wcopy(c->freeroam_world, ARRAY_COUNT(c->freeroam_world), v, wlen(v));
            CFG_MIGRATED("FreeRoamWorld");
        }
    }
#undef CFG_MIGRATED
}

/* FNV-1a of the [hook] key lines as the hook writes them (each trimmed, then "\n"): the
   "; hook-written <8 hex>" line says the keys below are still the hook's (cfg_take_installer). */
static unsigned cfg_keys_hash(const wchar_t *keys, size_t chars) {
    unsigned hash = 2166136261u;
    for (size_t i = 0; i < chars; ++i) {
        if (keys[i] == L'\r') {
            continue;
        }
        hash = (hash ^ (unsigned)keys[i]) * 16777619u;
    }
    return hash;
}

#define CFG_MARK L"; hook-written "

/* The [hook] block for `c` (UTF-16, CRLF; every key that differs from the defaults). */
static size_t cfg_block(const FiConfig *c, int from_legacy, wchar_t *out, size_t cap) {
    static const wchar_t *const header[] = {
        L"[hook]",
        L"; ForzaInfinity hook settings. Every key is optional; a missing key uses its default:",
        L"; NetMode=Proxy (or Capture, Off), Server=https://infinity.anarqz.dev, AutoUpdate=1,",
        L"; FreeRoam=1, FreeRoamDirect=0, FreeRoamWorld= (host:port), CarState=auto (or off),",
        L"; Language=auto (or en, pt-BR), Log=1, RelayCommand=auto (or legacy).",
    };
    static wchar_t keys[1024];
    size_t n = 0, k = 0;
#define CFG_PUT(buffer, used, limit, text)                                                         \
    do {                                                                                           \
        const wchar_t *t_ = (text);                                                                \
        size_t k_ = wlen(t_);                                                                      \
        if (used + k_ + 3 < (limit)) {                                                             \
            memory_copy(buffer + used, t_, k_ * sizeof(wchar_t));                                  \
            used += k_;                                                                            \
        }                                                                                          \
    } while (0)
#define CFG_KEY(text) CFG_PUT(keys, k, ARRAY_COUNT(keys), text)
#define CFG_KEY_LINE(text)                                                                         \
    do {                                                                                           \
        CFG_KEY(text);                                                                             \
        CFG_KEY(L"\r\n");                                                                          \
    } while (0)
#define CFG_LINE(text)                                                                             \
    do {                                                                                           \
        CFG_PUT(out, n, cap, text);                                                                \
        CFG_PUT(out, n, cap, L"\r\n");                                                             \
    } while (0)
    FiConfig d;
    cfg_defaults(&d);
    if (c->net_mode != d.net_mode) {
        CFG_KEY_LINE(c->net_mode == FI_NET_OFF ? L"NetMode=Off" : c->net_mode == FI_NET_CAPTURE ? L"NetMode=Capture"
                                                                                              : L"NetMode=Proxy");
    }
    if (!wequal(c->server, d.server)) {
        CFG_KEY(L"Server=");
        CFG_KEY_LINE(c->server);
    }
    if (c->auto_update != d.auto_update) {
        CFG_KEY_LINE(L"AutoUpdate=0");
    }
    if (c->freeroam != d.freeroam) {
        CFG_KEY_LINE(L"FreeRoam=0");
    }
    if (c->freeroam_direct != d.freeroam_direct) {
        CFG_KEY_LINE(L"FreeRoamDirect=1");
    }
    if (c->freeroam_world[0]) {
        CFG_KEY(L"FreeRoamWorld=");
        CFG_KEY_LINE(c->freeroam_world);
    }
    if (c->carstate != d.carstate) {
        CFG_KEY_LINE(L"CarState=off");
    }
    if (c->language != d.language) {
        CFG_KEY_LINE(c->language == FI_LANG_EN ? L"Language=en" : L"Language=pt-BR");
    }
    if (c->log != d.log) {
        CFG_KEY_LINE(L"Log=0");
    }
    if (c->relay_legacy != d.relay_legacy) {
        CFG_KEY_LINE(L"RelayCommand=legacy");
    }
    keys[k] = 0;
    for (size_t i = 0; i < ARRAY_COUNT(header); ++i) {
        CFG_LINE(header[i]);
    }
    CFG_LINE(from_legacy ? L"; Written once by the hook from data\\bin\\FH2_ExtraOptions.ini ([Network], [FreeRoam]);"
                         : L"; Written once by the hook (there were no older settings to bring over).");
    if (from_legacy) {
        CFG_LINE(L"; that file is not read any more.");
    }
    /* while the keys are as written, an older ForzaInfinity installer that sets the server in
       data\bin\FH2_ExtraOptions.ini is still followed (cfg_take_installer) */
    wchar_t mark[64];
    unsigned hash = cfg_keys_hash(keys, k);
    size_t m = wlen(CFG_MARK);
    memory_copy(mark, CFG_MARK, m * sizeof(wchar_t));
    for (int i = 7; i >= 0; --i) {
        mark[m++] = L"0123456789abcdef"[(hash >> (4 * i)) & 15];
    }
    mark[m] = 0;
    CFG_PUT(out, n, cap, mark);
    CFG_LINE(L" (keys as the hook wrote them: data\\bin\\FH2_ExtraOptions.ini [Network] from an older installer still applies)");
    CFG_PUT(out, n, cap, keys);
#undef CFG_LINE
#undef CFG_KEY_LINE
#undef CFG_KEY
#undef CFG_PUT
    out[n] = 0;
    return n;
}

/* Infinity.cfg = the first `n` bytes of cfg_out, through Infinity.cfg.hook-new: 1 written. */
static int cfg_commit(size_t n, int exists) {
    HANDLE file = CreateFileW(cfg_temp, GENERIC_WRITE, 0, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, 0);
    if (file == INVALID_HANDLE_VALUE) {
        return 0;
    }
    DWORD written = 0;
    int ok = WriteFile(file, cfg_out, (DWORD)n, &written, 0) && written == n && FlushFileBuffers(file);
    CloseHandle(file);
    /* an Infinity.cfg that appeared meanwhile is not replaced by one made from nothing */
    DWORD flags = MOVEFILE_WRITE_THROUGH | (exists ? MOVEFILE_REPLACE_EXISTING : 0);
    if (!ok || !MoveFileExW(cfg_temp, cfg_file, flags)) {
        DeleteFileW(cfg_temp);
        return 0;
    }
    return 1;
}

/* Infinity.cfg = its current bytes (`old`, `old_size`, encoding `encoding`; a new UTF-16LE file
   when it is empty or does not `exist`) then the block, through Infinity.cfg.hook-new.
   1 written. */
static int cfg_write(int exists, const unsigned char *old, size_t old_size, int encoding, const wchar_t *block,
                     size_t chars) {
    size_t n = 0;
    if (old_size) {
        memory_copy(cfg_out, old, old_size);
        n = old_size;
    } else {
        cfg_out[n++] = 0xff, cfg_out[n++] = 0xfe;
        encoding = FI_CFG_UTF16LE;
    }
    int wide = encoding == FI_CFG_UTF16LE;
    /* the block starts on a line of its own */
    int ends_with_break = old_size && (wide ? old_size >= 4 && old[old_size - 2] == '\n' && old[old_size - 1] == 0
                                            : old[old_size - 1] == '\n');
    static const wchar_t crlf[] = L"\r\n";
    for (int pass = 0; pass < 2; ++pass) {
        const wchar_t *text = pass ? block : crlf;
        size_t count = pass ? chars : (old_size && !ends_with_break && !(wide && old_size == 2) ? 2 : 0);
        if (!count) {
            continue;
        }
        if (wide) {
            for (size_t i = 0; i < count && n + 2 <= sizeof(cfg_out); ++i) {
                cfg_out[n++] = (unsigned char)(text[i] & 0xff);
                cfg_out[n++] = (unsigned char)(text[i] >> 8);
            }
        } else {
            int k = WideCharToMultiByte(CP_UTF8, 0, text, (int)count, (char *)cfg_out + n, (int)(sizeof(cfg_out) - n),
                                        0, 0);
            if (k <= 0) {
                return 0;
            }
            n += (size_t)k;
        }
    }
    return cfg_commit(n, exists);
}

static void cfg_note_values(const FiConfig *c);

/* ------------------------------------------- an older installer's server */
/* Installers and updaters up to v0.4.5 (and Install.cmd from their zips, which players keep)
   set the server in data\bin\FH2_ExtraOptions.ini [Network]; ForzaInfinity-Update passes on the
   Server= it finds there. While Infinity.cfg's [hook] is still exactly as the hook wrote it (its
   "; hook-written" line matches its keys) and that INI was written after Infinity.cfg, the INI's
   [Network]/[FreeRoam] keys are taken again and [hook] is written again from them, in place
   (UTF-16LE files only: the hook's and the launcher's), every other byte as it was. A [hook]
   edited by hand, or by anything but the hook, is never overwritten. */

/* The first [hook] of cfg_text, [start, end) in characters (its header line up to the next
   section header, or the end), and whether its "; hook-written" line matches its keys. */
static int cfg_hook_span(size_t *start, size_t *end, int *pristine) {
    const wchar_t *p = cfg_text, *found = 0, *mark = 0;
    unsigned hash = 2166136261u;
    *pristine = 0;
    while (*p) {
        const wchar_t *raw = p;
        CfgSpan line = {p, p};
        while (*line.end && *line.end != L'\n' && *line.end != L'\r') {
            ++line.end;
        }
        p = line.end;
        while (*p == L'\r' || *p == L'\n') {
            ++p;
        }
        cfg_trim(&line);
        if (line.start < line.end && *line.start == L'[') {
            if (found) {
                *end = (size_t)(raw - cfg_text);
                break;
            }
            const wchar_t *close = line.start + 1;
            while (close < line.end && *close != L']') {
                ++close;
            }
            CfgSpan name = {line.start + 1, close};
            cfg_trim(&name);
            if (wequal_n(name.start, (size_t)(name.end - name.start), L"hook")) {
                found = raw;
                *start = (size_t)(raw - cfg_text);
                *end = wlen(cfg_text);
            }
            continue;
        }
        if (!found || line.start == line.end) {
            continue;
        }
        if (*line.start == L';') {
            if (!mark && (size_t)(line.end - line.start) >= wlen(CFG_MARK) + 8 && wstarts(line.start, CFG_MARK)) {
                mark = line.start + wlen(CFG_MARK);
            }
            continue;
        }
        for (const wchar_t *c = line.start; c < line.end; ++c) {
            hash = (hash ^ (unsigned)*c) * 16777619u;
        }
        hash = (hash ^ (unsigned)L'\n') * 16777619u;
    }
    if (!found) {
        return 0;
    }
    if (mark) {
        unsigned written = 0;
        int ok = 1;
        for (int i = 0; i < 8; ++i) {
            wchar_t c = mark[i];
            int d = c >= L'0' && c <= L'9' ? c - L'0' : c >= L'a' && c <= L'f' ? c - L'a' + 10 : -1;
            ok &= d >= 0;
            written = written << 4 | (unsigned)(d < 0 ? 0 : d);
        }
        *pristine = ok && written == hash;
    }
    return 1;
}

static unsigned long long cfg_written_at(const wchar_t *path) {
    WIN32_FILE_ATTRIBUTE_DATA data;
    if (!GetFileAttributesExW(path, GetFileExInfoStandard, &data)) {
        return 0;
    }
    return (unsigned long long)data.ftLastWriteTime.dwHighDateTime << 32 | data.ftLastWriteTime.dwLowDateTime;
}

/* cfg_values were read from a [hook] (cfg_text, `size` bytes in cfg_bytes, `encoding`). */
static void cfg_take_installer(int encoding, size_t size) {
    static unsigned char saved[CFG_MAX_BYTES + 2];
    static wchar_t block[2048];
    size_t start = 0, end = 0;
    int pristine;
    unsigned long long cfg_time = cfg_written_at(cfg_file), legacy_time = cfg_written_at(cfg_legacy);
    if (!legacy_time || legacy_time <= cfg_time || !cfg_hook_span(&start, &end, &pristine)) {
        return;
    }
    if (!pristine || encoding != FI_CFG_UTF16LE) {
        cfg_note("CFG: data\\bin\\FH2_ExtraOptions.ini is newer than Infinity.cfg, but [hook] there is not as the "
                 "hook wrote it: kept as it is (an older installer's server is not taken).\r\n");
        return;
    }
    memory_copy(saved, cfg_bytes, size);
    int legacy_encoding;
    size_t legacy_size;
    if (cfg_decode(cfg_legacy, &legacy_encoding, &legacy_size, "data\\bin\\FH2_ExtraOptions.ini") <= 0) {
        return;
    }
    FiConfig d;
    char migrated[128];
    cfg_defaults(&d);
    cfg_read_legacy(&d, migrated, sizeof(migrated));
    /* the keys the INI never had stay as [hook] says */
    d.carstate = cfg_values.carstate;
    d.language = cfg_values.language;
    d.log = cfg_values.log;
    d.relay_legacy = cfg_values.relay_legacy;
    d.encoding = cfg_values.encoding;
    d.has_hook_section = 1;
    if (d.net_mode == cfg_values.net_mode && wequal(d.server, cfg_values.server) &&
        d.auto_update == cfg_values.auto_update && d.freeroam == cfg_values.freeroam &&
        d.freeroam_direct == cfg_values.freeroam_direct && wequal(d.freeroam_world, cfg_values.freeroam_world)) {
        return; /* it says what [hook] says */
    }
    size_t chars = cfg_block(&d, 1, block, ARRAY_COUNT(block));
    size_t head = 2 + 2 * start, tail = 2 + 2 * end, n = 0;
    if (tail > size || head > tail) {
        return;
    }
    memory_copy(cfg_out, saved, head);
    n = head;
    for (size_t i = 0; i < chars; ++i) {
        cfg_out[n++] = (unsigned char)(block[i] & 0xff);
        cfg_out[n++] = (unsigned char)(block[i] >> 8);
    }
    memory_copy(cfg_out + n, saved + tail, size - tail);
    n += size - tail;
    int written = cfg_commit(n, 1);
    memory_copy(&cfg_values, &d, sizeof(d)); /* no struct assignment: no CRT memcpy here */
    cfg_note(written ? "CFG: data\\bin\\FH2_ExtraOptions.ini was written after Infinity.cfg (an older ForzaInfinity "
                       "installer or updater set the server there): [hook] written again from it:"
                     : "CFG: data\\bin\\FH2_ExtraOptions.ini was written after Infinity.cfg (an older ForzaInfinity "
                       "installer or updater); Infinity.cfg could not be written, its values are used this session:");
    cfg_note_values(&cfg_values);
    cfg_note(".\r\n");
}

/* ------------------------------------------------------------------ API */

static const char *cfg_encoding_name(int encoding) {
    switch (encoding) {
    case FI_CFG_UTF16LE:
        return "UTF-16LE";
    case FI_CFG_UTF8:
        return "UTF-8";
    case FI_CFG_UTF8_BOM:
        return "UTF-8 with BOM";
    case FI_CFG_ANSI:
        return "ANSI";
    default:
        return "none";
    }
}

static void cfg_note_values(const FiConfig *c) {
    cfg_note(" NetMode=");
    cfg_note(c->net_mode == FI_NET_PROXY ? "Proxy" : c->net_mode == FI_NET_CAPTURE ? "Capture" : "Off");
    cfg_note(" Server=");
    cfg_note_wide(c->server);
    cfg_note(c->auto_update ? " AutoUpdate=1" : " AutoUpdate=0");
    cfg_note(c->freeroam ? " FreeRoam=1" : " FreeRoam=0");
    cfg_note(c->freeroam_direct ? " FreeRoamDirect=1" : " FreeRoamDirect=0");
    cfg_note(" FreeRoamWorld=");
    cfg_note_wide(c->freeroam_world);
    cfg_note(c->carstate == FI_CARSTATE_OFF ? " CarState=off" : " CarState=auto");
    cfg_note(c->language == FI_LANG_EN ? " Language=en" : c->language == FI_LANG_PT_BR ? " Language=pt-BR"
                                                                                      : " Language=auto");
    cfg_note(c->log ? " Log=1" : " Log=0");
    cfg_note(c->relay_legacy ? " RelayCommand=legacy" : " RelayCommand=auto");
}

static void cfg_load_files(const wchar_t *root, const wchar_t *bin) {
    static unsigned char original[CFG_MAX_BYTES + 2];
    static wchar_t block[2048];
    char migrated[128];
    AcquireSRWLockExclusive(&cfg_lock);
    cfg_log_used = 0;
    cfg_log_lines[0] = 0;
    cfg_defaults(&cfg_values);
    if (!path_join(cfg_file, root, FI_CFG_NAME) || !path_join(cfg_temp, root, L"Infinity.cfg.hook-new") ||
        !path_join(cfg_legacy, bin, FI_LEGACY_INI_NAME)) {
        cfg_note("CFG: the paths are too long; defaults this session.\r\n");
        ReleaseSRWLockExclusive(&cfg_lock);
        return;
    }
    int encoding;
    size_t size;
    int read = cfg_decode(cfg_file, &encoding, &size, "Infinity.cfg");
    cfg_values.encoding = read > 0 ? encoding : FI_CFG_NONE;
    if (read < 0) {
        cfg_note("CFG: Infinity.cfg is not usable; defaults this session (nothing written).\r\n");
        ReleaseSRWLockExclusive(&cfg_lock);
        return;
    }
    if (read > 0 && encoding == FI_CFG_UTF8_BOM) {
        cfg_note("CFG: Infinity.cfg is UTF-8 with a byte order mark; read anyway (Kernel32 tools misread its first "
                 "line: save it as UTF-16LE, the launcher's format, or UTF-8 without one).\r\n");
    }
    if (read > 0) {
        static const wchar_t *const named[] = {L"hook", L"relay"};
        for (size_t i = 0; i < ARRAY_COUNT(named); ++i) {
            if (cfg_section_count(named[i]) > 1) {
                cfg_note("CFG: Infinity.cfg has [");
                cfg_note_wide(named[i]);
                cfg_note("] more than once; only the first is read.\r\n");
            }
        }
    }
    if (read > 0 && cfg_has_section(L"hook")) {
        cfg_values.has_hook_section = 1;
        cfg_read_hook(&cfg_values);
        cfg_note("CFG: Infinity.cfg read (");
        cfg_note(cfg_encoding_name(encoding));
        cfg_note("):");
        cfg_note_values(&cfg_values);
        cfg_note(".\r\n");
        cfg_take_installer(encoding, size);
        ReleaseSRWLockExclusive(&cfg_lock);
        return;
    }
    /* no [hook]: the older hooks' keys come in once */
    if (read > 0) {
        memory_copy(original, cfg_bytes, size);
    }
    size_t original_size = read > 0 ? size : 0;
    int legacy_encoding;
    size_t legacy_size;
    int legacy = cfg_decode(cfg_legacy, &legacy_encoding, &legacy_size, "data\\bin\\FH2_ExtraOptions.ini");
    migrated[0] = 0;
    if (legacy > 0) {
        cfg_read_legacy(&cfg_values, migrated, sizeof(migrated));
    }
    size_t chars = cfg_block(&cfg_values, legacy > 0, block, ARRAY_COUNT(block));
    if (cfg_write(read > 0, original, original_size, read > 0 ? encoding : FI_CFG_UTF16LE, block, chars)) {
        cfg_values.has_hook_section = 1;
        cfg_migrated = legacy > 0;
        if (read <= 0) {
            cfg_values.encoding = FI_CFG_UTF16LE;
        }
        cfg_note(read > 0 ? "CFG: [hook] added to Infinity.cfg" : "CFG: Infinity.cfg created (UTF-16LE) with [hook]");
        if (legacy > 0) {
            cfg_note(migrated[0] ? " (from data\\bin\\FH2_ExtraOptions.ini: " : " (data\\bin\\FH2_ExtraOptions.ini had ");
            cfg_note(migrated[0] ? migrated : "every key at its default");
            cfg_note(migrated[0] ? "; that file is not read any more)" : "; that file is not read any more)");
        } else {
            cfg_note(" (no older settings: every key at its default)");
        }
        cfg_note(":");
    } else {
        cfg_note("CFG: could not write Infinity.cfg; running with ");
        cfg_note(legacy > 0 ? "the values of data\\bin\\FH2_ExtraOptions.ini" : "the defaults");
        cfg_note(" this session:");
    }
    cfg_note_values(&cfg_values);
    cfg_note(".\r\n");
    ReleaseSRWLockExclusive(&cfg_lock);
}

/* The 32 hex digits of a launch nonce (either case) into `nonce`: 1 when `hex` is exactly that. */
static int cfg_nonce(const wchar_t *hex, unsigned char nonce[16]) {
    for (int i = 0; i < 32; ++i) {
        wchar_t c = hex[i];
        int digit = c >= L'0' && c <= L'9' ? c - L'0' : c >= L'a' && c <= L'f' ? c - L'a' + 10
                                                       : c >= L'A' && c <= L'F' ? c - L'A' + 10
                                                                                : -1;
        if (digit < 0) {
            return 0;
        }
        nonce[i / 2] = (unsigned char)(i & 1 ? nonce[i / 2] | digit : digit << 4);
    }
    return hex[32] == 0;
}

/* FI_LAUNCH: the launcher started this game (fi_launcher_core::session::parse_launch_env).
   - v1;pipe=<32 hex>: the launcher's session (FI_LAUNCH_PROTOCOL): the network layer is on
     whatever [hook] NetMode and Server say, and goes to the server the launcher's ticket names
     once launch.c has it (no ticket: offline). The nonce names the pipe.
   - v1;file=<32 hex> (the native Linux launcher's file handoff, LAU-M7): not in this series, so
     offline (FI_LAUNCH_FILE).
   - v1;offline (Play offline), or anything else (FI_LAUNCH_MALFORMED): offline.
   Offline means no account, cloud or update threads (ONLINE_OFFLINE.md section 5). The value
   itself (the nonce) is never logged. */
static void cfg_launch(void) {
    wchar_t value[160];
    SetLastError(0);
    DWORD n = GetEnvironmentVariableW(L"FI_LAUNCH", value, ARRAY_COUNT(value));
    if (!n && GetLastError() == ERROR_ENVVAR_NOT_FOUND) {
        return;
    }
    int fits = n && n < ARRAY_COUNT(value);
    unsigned char nonce[16];
    int kind = fits && wequal(value, L"v1;offline")                              ? FI_LAUNCH_OFFLINE
               : fits && wstarts(value, L"v1;pipe=") && cfg_nonce(value + 8, nonce) ? FI_LAUNCH_PROTOCOL
               : fits && wstarts(value, L"v1;file=") && cfg_nonce(value + 8, nonce) ? FI_LAUNCH_FILE
                                                                                    : FI_LAUNCH_MALFORMED;
    cfg_values.launch = kind;
    if (kind == FI_LAUNCH_PROTOCOL) {
        memory_copy(cfg_values.launch_nonce, nonce, sizeof(nonce));
    }
    cfg_values.net_mode = kind == FI_LAUNCH_PROTOCOL ? FI_NET_PROXY : FI_NET_OFF;
    cfg_note(kind == FI_LAUNCH_OFFLINE ? "CFG: started by the launcher's Play offline (FI_LAUNCH=v1;offline): offline "
                                         "this session, whatever [hook] NetMode says.\r\n"
             : kind == FI_LAUNCH_PROTOCOL
                 ? "CFG: started by the launcher (FI_LAUNCH, pipe): online only through the launcher's session, "
                   "whatever [hook] NetMode and Server say.\r\n"
             : kind == FI_LAUNCH_FILE
                 ? "CFG: started by the launcher (FI_LAUNCH, file); this hook does not take the launcher's file link "
                   "yet: offline this session.\r\n"
                 : "CFG: FI_LAUNCH is set but is not a launch link this hook knows: offline this session.\r\n");
}

/* The older hooks' log in data\bin (FH2_ExtraOptions.log, .previous.log: ours up to .58, never
   ExtraOptions', whose log is next to azimuth.exe) is renamed FH2_ExtraOptions.58.log, so nothing
   reads a stale one as this hook's. */
static void cfg_retire_old_logs(const wchar_t *bin) {
    static const wchar_t *const names[2][2] = {{L"FH2_ExtraOptions.log", L"FH2_ExtraOptions.58.log"},
                                               {L"FH2_ExtraOptions.previous.log", L"FH2_ExtraOptions.58.previous.log"}};
    static wchar_t from[PATH_CAPACITY], to[PATH_CAPACITY];
    for (int i = 0; i < 2; ++i) {
        if (path_join(from, bin, names[i][0]) && path_join(to, bin, names[i][1]) &&
            GetFileAttributesW(from) != INVALID_FILE_ATTRIBUTES &&
            MoveFileExW(from, to, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
            cfg_note("CFG: data\\bin\\");
            cfg_note_wide(names[i][0]);
            cfg_note(" (the log of hooks up to 0.2.74+fh2online.58) renamed ");
            cfg_note_wide(names[i][1]);
            cfg_note(".\r\n");
        }
    }
}

void fi_cfg_load(const wchar_t *root, const wchar_t *bin) {
    cfg_load_files(root, bin);
    AcquireSRWLockExclusive(&cfg_lock);
    cfg_launch();
    cfg_retire_old_logs(bin);
    ReleaseSRWLockExclusive(&cfg_lock);
}

const FiConfig *fi_cfg(void) {
    return &cfg_values;
}

int fi_cfg_migrated(void) {
    return cfg_migrated;
}

const wchar_t *fi_cfg_path(void) {
    return cfg_file;
}

void fi_cfg_flush_log(void) {
    if (cfg_log_used) {
        log_append(cfg_log_lines);
    }
}

/* ------------------------------------------------------------- [relay] */

static void cfg_utf8(const wchar_t *in, char *out, size_t cap) {
    int n = WideCharToMultiByte(CP_UTF8, 0, in, -1, out, (int)cap, 0, 0);
    out[n > 0 ? n - 1 : 0] = 0;
}

static unsigned cfg_number(const wchar_t *v, unsigned fallback, unsigned low, unsigned high) {
    unsigned n = 0;
    if (!v[0]) {
        return fallback;
    }
    for (size_t i = 0; v[i]; ++i) {
        if (v[i] < L'0' || v[i] > L'9' || n > 100000) {
            return fallback;
        }
        n = n * 10 + (unsigned)(v[i] - L'0');
    }
    return n >= low && n <= high ? n : fallback;
}

int fi_cfg_relay(FiRelayConfig *out) {
    wchar_t v[300];
    int encoding, found = 0;
    size_t size;
    cfg_zero(out, sizeof(*out));
    memory_copy(out->visibility, "public", 7);
    out->max_players = 8;
    out->port = 5101;
    out->races = 1;
    AcquireSRWLockExclusive(&cfg_lock);
    size_t saved_log = cfg_log_used;
    int read = cfg_file[0] && cfg_decode(cfg_file, &encoding, &size, "Infinity.cfg") > 0;
    cfg_log_used = saved_log; /* the start's lines are flushed already */
    if (read && cfg_has_section(L"relay")) {
        found = 1;
        /* the launcher's limits (PRODUCT.md 6.3); the server checks them again */
        if (cfg_lookup(L"relay", L"Name", v, 33, 0)) {
            cfg_utf8(v, out->name, sizeof(out->name));
        }
        if (cfg_lookup(L"relay", L"Message", v, 121, 0)) {
            cfg_utf8(v, out->message, sizeof(out->message));
        }
        if (cfg_lookup(L"relay", L"Visibility", v, ARRAY_COUNT(v), 0) &&
            (wequal(v, L"public") || wequal(v, L"unlisted") || wequal(v, L"friends") || wequal(v, L"off"))) {
            for (size_t i = 0; v[i] && i + 1 < sizeof(out->visibility); ++i) {
                out->visibility[i] = (char)wlower(v[i]);
                out->visibility[i + 1] = 0;
            }
        }
        if (cfg_lookup(L"relay", L"Password", v, 33, 0)) {
            cfg_utf8(v, out->password, sizeof(out->password));
        }
        if (cfg_lookup(L"relay", L"MaxPlayers", v, ARRAY_COUNT(v), 0)) {
            out->max_players = cfg_number(v, 8, 1, 12);
        }
        if (cfg_lookup(L"relay", L"Port", v, ARRAY_COUNT(v), 0)) {
            out->port = cfg_number(v, 5101, 1024, 65535);
        }
        if (cfg_lookup(L"relay", L"Races", v, ARRAY_COUNT(v), 0)) {
            out->races = !wequal(v, L"0");
        }
    }
    ReleaseSRWLockExclusive(&cfg_lock);
    return found;
}
