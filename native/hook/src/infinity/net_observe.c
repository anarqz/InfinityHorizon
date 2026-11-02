/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   What the game got from matchmaking (momatch.*) and the session directory (sessiondirectory.*),
   in infinity.log next to net.c's "NETWORK: open" lines. Observation only: called after a request
   is complete, it reads the response and never changes it.

   One line per request: "NETWORK: answer #<id> <status>" and, from a JSON body, only
     ticketStatus=<value>                          a ticket document's state
     target=<templateName>/<name, 8 chars>         its targetSessionRef
     mm=<value>                                    a session's servers.matchmaking.properties.system.status
     mmtarget=<templateName>/<name, 8 chars>       that object's targetSessionRef
     members=<count>                               the keys of the session's members object ("+" when cut)
   or "NETWORK: answer #<id> offline|error 0x<HRESULT>" when nothing answered. A value keeps only
   [A-Za-z0-9_.-] (anything else is '?') and at most 40 characters; nothing else of the body is
   written (no member, XUID, gamertag or token). The scan is bounded: at most NO_SCAN_MAX bytes, never
   past the body, NO_DEPTH levels, no allocation; a body that is not a JSON object yields no field.

   Each host family logs its first NO_FIRST opens and its first NO_FIRST answers, then a count line
   at every power of two (their own budget: net.c's other opens stop at 64 lines). */
#include "infinity.h"
#include "../core/logging.h"

#define NO_FIRST 64
#define NO_CALLS_MAX 0x7fffffffL
#define NO_VALUE_MAX 40u
#define NO_NAME_MAX 8u
#define NO_KEY_MAX 24u
#define NO_DEPTH 64u
#define NO_SCAN_MAX (1u << 20)
#define NO_LINE 384u
#define NO_OFFLINE_ERROR ((HRESULT)0x800C0005L) /* net.c's NET_OFFLINE_ERROR */

static const char *const no_family_names[FI_NETOBS_FAMILIES] = {"", "momatch", "sessiondirectory"};
static volatile LONG no_opens[FI_NETOBS_FAMILIES], no_answers[FI_NETOBS_FAMILIES];

/* ------------------------------------------------------------------ the host */

static wchar_t no_lower(wchar_t c) { return c >= L'A' && c <= L'Z' ? (wchar_t)(c + 32) : c; }

static int no_host_is(const wchar_t *host, size_t length, const wchar_t *prefix) {
    size_t m = 0;
    while (prefix[m]) ++m;
    if (length <= m) return 0;
    for (size_t i = 0; i < m; ++i) {
        if (no_lower(host[i]) != prefix[i]) return 0;
    }
    return 1;
}

int fi_netobs_family(const wchar_t *url, size_t length) {
    size_t i = 0;
    if (!url) return FI_NETOBS_NONE;
    while (i < length && i < 16 && ((no_lower(url[i]) >= L'a' && no_lower(url[i]) <= L'z') ||
                                    (url[i] >= L'0' && url[i] <= L'9') || url[i] == L'+' || url[i] == L'-' ||
                                    url[i] == L'.')) {
        ++i;
    }
    if (!i || i + 3 > length || url[i] != L':' || url[i + 1] != L'/' || url[i + 2] != L'/') return FI_NETOBS_NONE;
    size_t start = i + 3, end = start;
    while (end < length && url[end] && url[end] != L'/' && url[end] != L'?' && url[end] != L'#' && url[end] != L'\\') {
        ++end;
    }
    for (size_t k = start; k < end; ++k) {
        if (url[k] == L'@') start = k + 1; /* user info is not the host */
    }
    size_t host_end = start;
    while (host_end < end && url[host_end] != L':') ++host_end; /* the port is not the host */
    if (no_host_is(url + start, host_end - start, L"momatch.")) return FI_NETOBS_MOMATCH;
    if (no_host_is(url + start, host_end - start, L"sessiondirectory.")) return FI_NETOBS_SESSIONDIRECTORY;
    return FI_NETOBS_NONE;
}

/* ------------------------------------------------------------------ the line */

typedef struct {
    char text[NO_LINE];
    size_t size;
} NoLine;

static void no_text(NoLine *line, const char *text) {
    for (size_t i = 0; text[i] && line->size + 1 < NO_LINE; ++i) line->text[line->size++] = text[i];
    line->text[line->size] = 0;
}

static void no_number(NoLine *line, unsigned long long value) {
    char digits[24];
    size_t n = 0;
    do {
        digits[n++] = (char)('0' + value % 10);
        value /= 10;
    } while (value && n < sizeof(digits));
    char out[24];
    for (size_t i = 0; i < n; ++i) out[i] = digits[n - 1 - i];
    out[n] = 0;
    no_text(line, out);
}

static void no_hex(NoLine *line, unsigned value) {
    static const char hex[] = "0123456789ABCDEF";
    char out[11] = {'0', 'x'};
    for (int i = 0; i < 8; ++i) out[2 + i] = hex[(value >> (28 - 4 * i)) & 15];
    out[10] = 0;
    no_text(line, out);
}

/* ------------------------------------------------------------------ the scan */

enum { NO_ROLE_NONE, NO_ROLE_ROOT, NO_ROLE_TARGET, NO_ROLE_SERVERS, NO_ROLE_MM, NO_ROLE_MM_PROPERTIES,
       NO_ROLE_SYSTEM, NO_ROLE_SYSTEM_TARGET, NO_ROLE_MEMBERS };
enum { NO_FIELD_NONE, NO_FIELD_TICKET, NO_FIELD_TARGET_TEMPLATE, NO_FIELD_TARGET_NAME, NO_FIELD_MM,
       NO_FIELD_MM_TEMPLATE, NO_FIELD_MM_NAME, NO_FIELDS };

typedef struct {
    char value[NO_FIELDS][NO_VALUE_MAX + 1];
    unsigned char found[NO_FIELDS];
    unsigned members;
    int members_state; /* 0 none, 1 open (cut), 2 closed */
} NoFields;

static int no_key_is(const char *key, size_t length, const char *name) {
    size_t i = 0;
    while (name[i] && i < length && key[i] == name[i]) ++i;
    return !name[i] && i == length;
}

/* The role of an object entered under `key` from an object of role `parent`. */
static unsigned char no_child(unsigned char parent, const char *key, size_t length) {
    switch (parent) {
    case NO_ROLE_ROOT:
        return no_key_is(key, length, "targetSessionRef") ? NO_ROLE_TARGET
               : no_key_is(key, length, "servers")        ? NO_ROLE_SERVERS
               : no_key_is(key, length, "members")        ? NO_ROLE_MEMBERS
                                                          : NO_ROLE_NONE;
    case NO_ROLE_SERVERS: return no_key_is(key, length, "matchmaking") ? NO_ROLE_MM : NO_ROLE_NONE;
    case NO_ROLE_MM: return no_key_is(key, length, "properties") ? NO_ROLE_MM_PROPERTIES : NO_ROLE_NONE;
    case NO_ROLE_MM_PROPERTIES: return no_key_is(key, length, "system") ? NO_ROLE_SYSTEM : NO_ROLE_NONE;
    case NO_ROLE_SYSTEM: return no_key_is(key, length, "targetSessionRef") ? NO_ROLE_SYSTEM_TARGET : NO_ROLE_NONE;
    default: return NO_ROLE_NONE;
    }
}

/* The field a scalar under `key` in an object of role `parent` fills. */
static int no_field(unsigned char parent, const char *key, size_t length) {
    switch (parent) {
    case NO_ROLE_ROOT: return no_key_is(key, length, "ticketStatus") ? NO_FIELD_TICKET : NO_FIELD_NONE;
    case NO_ROLE_TARGET:
        return no_key_is(key, length, "templateName") ? NO_FIELD_TARGET_TEMPLATE
               : no_key_is(key, length, "name")       ? NO_FIELD_TARGET_NAME
                                                      : NO_FIELD_NONE;
    case NO_ROLE_SYSTEM: return no_key_is(key, length, "status") ? NO_FIELD_MM : NO_FIELD_NONE;
    case NO_ROLE_SYSTEM_TARGET:
        return no_key_is(key, length, "templateName") ? NO_FIELD_MM_TEMPLATE
               : no_key_is(key, length, "name")       ? NO_FIELD_MM_NAME
                                                      : NO_FIELD_NONE;
    default: return NO_FIELD_NONE;
    }
}

static int no_safe(unsigned char c) {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_' || c == '.' ||
           c == '-';
}

static size_t no_space(const unsigned char *p, size_t n, size_t i) {
    while (i < n && (p[i] == ' ' || p[i] == '\t' || p[i] == '\r' || p[i] == '\n')) ++i;
    return i;
}

/* The string whose opening quote is at `i`: its characters into `out` (at most `cap`; a key raw,
   `*length` cap + 1 when it is longer or escaped so it matches nothing; a value with each escape
   as one '?' and unsafe bytes as '?'). Past its closing quote, or 0 when it does not end. */
static size_t no_string(const unsigned char *p, size_t n, size_t i, char *out, size_t cap, size_t *length, int key) {
    size_t used = 0;
    int spoiled = 0;
    for (++i; i < n; ++i) {
        unsigned char c = p[i];
        if (c == '"') {
            *length = spoiled ? cap + 1 : used;
            if (out) out[used < cap ? used : cap] = 0;
            return i + 1;
        }
        if (c == '\\') {
            if (i + 1 >= n) return 0;
            ++i;
            if (p[i] == 'u') {
                for (unsigned k = 0; k < 4 && i + 1 < n && p[i + 1] != '"'; ++k) ++i;
            }
            spoiled |= key;
            c = '?';
        } else if (!key && !no_safe(c)) {
            c = '?';
        }
        if (used < cap) {
            if (out) out[used] = (char)c;
            ++used;
        } else {
            spoiled |= key;
        }
    }
    return 0;
}

static int no_literal_byte(unsigned char c) {
    return c != ',' && c != '}' && c != ']' && c != ':' && c != '"' && c != '{' && c != '[' && c != ' ' &&
           c != '\t' && c != '\r' && c != '\n';
}

static void no_scan(const unsigned char *p, size_t n, NoFields *fields) {
    enum { WANT_VALUE, WANT_KEY, WANT_NEXT };
    unsigned char role[NO_DEPTH], array[NO_DEPTH];
    size_t depth = 0, members_level = 0;
    int state = WANT_VALUE, field = NO_FIELD_NONE;
    unsigned char child = NO_ROLE_ROOT;
    if (!p) return;
    if (n > NO_SCAN_MAX) n = NO_SCAN_MAX;
    size_t i = no_space(p, n, 0);
    if (i >= n || p[i] != '{') return; /* only a JSON object is a document */
    while ((i = no_space(p, n, i)) < n) {
        unsigned char c = p[i];
        if (state == WANT_VALUE) {
            if (c == '{' || c == '[') {
                if (depth >= NO_DEPTH) return;
                role[depth] = c == '{' ? child : NO_ROLE_NONE;
                array[depth] = c == '[';
                if (c == '{' && child == NO_ROLE_MEMBERS && !fields->members_state) {
                    fields->members_state = 1;
                    members_level = depth;
                }
                ++depth;
                state = c == '{' ? WANT_KEY : WANT_VALUE;
                child = NO_ROLE_NONE;
                field = NO_FIELD_NONE;
                ++i;
                continue;
            }
            if (c == ']' && depth && array[depth - 1]) { /* an empty array (or a trailing comma) */
                state = WANT_NEXT;
                continue;
            }
            char *out = field && !fields->found[field] ? fields->value[field] : 0;
            size_t cap = field == NO_FIELD_TARGET_NAME || field == NO_FIELD_MM_NAME ? NO_NAME_MAX : NO_VALUE_MAX, length;
            if (c == '"') {
                size_t next = no_string(p, n, i, out, cap, &length, 0);
                if (!next) return;
                i = next;
            } else {
                size_t start = i;
                while (i < n && no_literal_byte(p[i])) {
                    if (out && i - start < cap) out[i - start] = no_safe(p[i]) ? (char)p[i] : '?';
                    ++i;
                }
                if (i == start || i >= n) return; /* not a value, or cut inside one */
                if (out) out[i - start < cap ? i - start : cap] = 0;
            }
            if (out) fields->found[field] = 1;
            state = WANT_NEXT;
        } else if (state == WANT_KEY) {
            if (c == '}') { /* an empty object (or a trailing comma) */
                state = WANT_NEXT;
                continue;
            }
            if (c != '"') return;
            char key[NO_KEY_MAX + 1];
            size_t length;
            size_t next = no_string(p, n, i, key, NO_KEY_MAX, &length, 1);
            if (!next) return;
            if (fields->members_state == 1 && depth == members_level + 1 && fields->members < 0xffffffffu) {
                ++fields->members;
            }
            i = no_space(p, n, next);
            if (i >= n || p[i] != ':') return;
            ++i;
            unsigned char parent = role[depth - 1];
            child = no_child(parent, key, length);
            field = no_field(parent, key, length);
            state = WANT_VALUE;
        } else {
            if (c == ',') {
                state = array[depth - 1] ? WANT_VALUE : WANT_KEY;
                child = NO_ROLE_NONE;
                field = NO_FIELD_NONE;
                ++i;
                continue;
            }
            if ((c == '}' && !array[depth - 1]) || (c == ']' && array[depth - 1])) {
                --depth;
                if (fields->members_state == 1 && depth == members_level) fields->members_state = 2;
                if (!depth) return; /* the document ends */
                ++i;
                continue;
            }
            return;
        }
    }
}

/* The answer line of an answered request into `out` (no line end). */
void fi_netobs_answer_text(char *out, size_t capacity, long id, unsigned status, const unsigned char *body,
                           size_t size) {
    NoLine line;
    NoFields fields;
    line.size = 0;
    line.text[0] = 0;
    for (int f = 0; f < NO_FIELDS; ++f) { /* no memset: the hook links no C runtime */
        fields.value[f][0] = 0;
        fields.found[f] = 0;
    }
    fields.members = 0;
    fields.members_state = 0;
    no_scan(body, size, &fields);
    no_text(&line, "NETWORK: answer #");
    no_number(&line, (unsigned long long)(unsigned long)id);
    no_text(&line, " ");
    no_number(&line, status);
    if (fields.found[NO_FIELD_TICKET]) {
        no_text(&line, " ticketStatus=");
        no_text(&line, fields.value[NO_FIELD_TICKET]);
    }
    if (fields.found[NO_FIELD_TARGET_TEMPLATE] || fields.found[NO_FIELD_TARGET_NAME]) {
        no_text(&line, " target=");
        no_text(&line, fields.value[NO_FIELD_TARGET_TEMPLATE]);
        no_text(&line, "/");
        no_text(&line, fields.value[NO_FIELD_TARGET_NAME]);
    }
    if (fields.found[NO_FIELD_MM]) {
        no_text(&line, " mm=");
        no_text(&line, fields.value[NO_FIELD_MM]);
    }
    if (fields.found[NO_FIELD_MM_TEMPLATE] || fields.found[NO_FIELD_MM_NAME]) {
        no_text(&line, " mmtarget=");
        no_text(&line, fields.value[NO_FIELD_MM_TEMPLATE]);
        no_text(&line, "/");
        no_text(&line, fields.value[NO_FIELD_MM_NAME]);
    }
    if (fields.members_state) {
        no_text(&line, " members=");
        no_number(&line, fields.members);
        if (fields.members_state == 1) no_text(&line, "+");
    }
    size_t n = 0;
    for (; capacity && n + 1 < capacity && n < line.size; ++n) out[n] = line.text[n];
    if (capacity) out[n] = 0;
}

/* ------------------------------------------------------------------ the budget */

/* The next count of `calls`, saturated (never wraps). */
static LONG no_next(volatile LONG *calls) {
    for (;;) {
        LONG seen = *calls;
        if (seen >= NO_CALLS_MAX) return NO_CALLS_MAX;
        if (InterlockedCompareExchange(calls, seen + 1, seen) == seen) return seen + 1;
    }
}

static int no_family_ok(int family) { return family > FI_NETOBS_NONE && family < FI_NETOBS_FAMILIES; }

int fi_netobs_open(int family, long id) {
    if (!no_family_ok(family)) return 0;
    LONG count = no_next(&no_opens[family]);
    if (count <= NO_FIRST) return 1;
    if (!(count & (count - 1))) {
        NoLine line;
        line.size = 0;
        line.text[0] = 0;
        no_text(&line, "NETWORK: ");
        no_text(&line, no_family_names[family]);
        no_text(&line, " opens: ");
        no_number(&line, (unsigned long long)count);
        no_text(&line, " (last #");
        no_number(&line, (unsigned long long)(unsigned long)id);
        no_text(&line, "; lines past the first 64 are counted only)\r\n");
        log_append(line.text);
    }
    return 0;
}

void fi_netobs_answer(int family, long id, int answered, unsigned status, HRESULT error, const unsigned char *body,
                      size_t size) {
    if (!no_family_ok(family)) return;
    LONG count = no_next(&no_answers[family]);
    NoLine line;
    line.size = 0;
    line.text[0] = 0;
    if (count > NO_FIRST) {
        if (count & (count - 1)) return;
        no_text(&line, "NETWORK: ");
        no_text(&line, no_family_names[family]);
        no_text(&line, " answers: ");
        no_number(&line, (unsigned long long)count);
        no_text(&line, " (last #");
        no_number(&line, (unsigned long long)(unsigned long)id);
        no_text(&line, " ");
        if (answered) no_number(&line, status);
        else no_text(&line, error == NO_OFFLINE_ERROR ? "offline" : "error");
        no_text(&line, "; lines past the first 64 are counted only)\r\n");
        log_append(line.text);
        return;
    }
    if (answered) {
        fi_netobs_answer_text(line.text, NO_LINE - 2, id, status, body, size);
        line.size = 0;
        while (line.text[line.size]) ++line.size;
    } else {
        no_text(&line, "NETWORK: answer #");
        no_number(&line, (unsigned long long)(unsigned long)id);
        no_text(&line, error == NO_OFFLINE_ERROR ? " offline " : " error ");
        no_hex(&line, (unsigned)error);
    }
    no_text(&line, "\r\n");
    log_append(line.text);
}
