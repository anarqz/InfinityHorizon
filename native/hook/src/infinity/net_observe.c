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

   A sessiondirectory answer that is a session document also gets "NETWORK: session #<id> host=
   <#hash/length> gsp=<status> members: <key>:<a|-><r|-> addr=<8 chars> tok=<#hash/length>; ..."
   (fi_netobs_session_text: at most 12 members, tokens only as a length and a 16-bit hash, an
   address only its first 8 base64 characters), only when it differs from the last one logged for
   the same URL (the repeat table, key (session, URL)); 64 such lines per family, then counts.

   Repeats collapse (T29). A game polls its own session every few seconds with the same answer; each
   family keeps NO_SLOTS recent requests, keyed by a 64-bit hash of (method, URL) (never the URL
   itself), with a hash of the last answer's line past its "#id" (status and logged fields, or the
   offline/error HRESULT). An answer equal to the last one of its key is not a line: it is counted,
   and "NETWORK: answer #<id> <status> <fields> repeated <n> times (same as #<first>)" goes out when n
   is a power of two. Once a key's answers repeat, its opens collapse the same way ("NETWORK: open
   #<id> repeated <n> times (same method and URL as #<first>)"); a changed answer is a line again and
   the next open of its key too. The least recently used slot is reused; under one SRW lock.

   Budgets. Each host family logs its first NO_FIRST GET opens and NO_FIRST GET answers, and apart
   from them its first NO_FIRST non-GET (PUT, POST, DELETE, ...) opens and answers, then a count line
   at every power of two (their own budget: net.c's other opens stop at 64 lines). Collapsed repeats
   use no budget, so a burst of GET polling never hides a PUT join. */
#include "infinity.h"
#include "../core/logging.h"

#define NO_FIRST 64
#define NO_SLOTS 16
#define NO_CALLS_MAX 0x7fffffffL
#define NO_VALUE_MAX 40u
#define NO_NAME_MAX 8u
#define NO_KEY_MAX 24u
#define NO_DEPTH 64u
#define NO_SCAN_MAX (1u << 20)
#define NO_LINE 768u
#define NO_OFFLINE_ERROR ((HRESULT)0x800C0005L) /* net.c's NET_OFFLINE_ERROR */

static const char *const no_family_names[FI_NETOBS_FAMILIES] = {"", "momatch", "sessiondirectory"};
enum { NO_CLASS_GET, NO_CLASS_OTHER, NO_CLASSES };
static const char *const no_class_names[NO_CLASSES] = {"GET", "non-GET"};
static volatile LONG no_opens[FI_NETOBS_FAMILIES][NO_CLASSES], no_answers[FI_NETOBS_FAMILIES][NO_CLASSES];
static volatile LONG no_sessions[FI_NETOBS_FAMILIES]; /* session lines (their own budget) */

/* A recent request of a family: hashes only (no URL, no body). */
typedef struct {
    unsigned long long key;    /* (method, URL); 0 is a free slot */
    unsigned long long answer; /* the last answer's line past its id */
    unsigned long long used;   /* the table's tick when last touched */
    long open_id, answer_id;   /* the last logged open and answer */
    LONG open_repeats, answer_repeats;
    int has_answer;
} NoSlot;

static NoSlot no_slots[FI_NETOBS_FAMILIES][NO_SLOTS];
static unsigned long long no_tick;
static SRWLOCK no_lock = SRWLOCK_INIT;

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
       NO_ROLE_SYSTEM, NO_ROLE_SYSTEM_TARGET, NO_ROLE_MEMBERS,
       /* the session line (fi_netobs_session_text) */
       NO_ROLE_PROPERTIES, NO_ROLE_PROPERTIES_SYSTEM, NO_ROLE_GSP, NO_ROLE_GSP_PROPERTIES, NO_ROLE_GSP_SYSTEM,
       NO_ROLE_GSP_CUSTOM, NO_ROLE_MEMBER, NO_ROLE_MEMBER_PROPERTIES, NO_ROLE_MEMBER_PSYSTEM,
       NO_ROLE_MEMBER_CONSTANTS, NO_ROLE_MEMBER_CSYSTEM };
enum { NO_FIELD_NONE, NO_FIELD_TICKET, NO_FIELD_TARGET_TEMPLATE, NO_FIELD_TARGET_NAME, NO_FIELD_MM,
       NO_FIELD_MM_TEMPLATE, NO_FIELD_MM_NAME, NO_FIELD_GSP_SYSTEM, NO_FIELD_GSP_CUSTOM, NO_FIELDS };
/* a session's own scalars: the host's token and each member's state (measured, never kept) */
enum { NO_EXTRA_NONE, NO_EXTRA_HOST, NO_EXTRA_TOKEN, NO_EXTRA_ADDRESS, NO_EXTRA_ACTIVE, NO_EXTRA_RESERVED };

#define NO_MEMBERS_MAX 12u
#define NO_ADDRESS_CHARS 8u
typedef struct {
    char key[NO_NAME_MAX + 1];       /* "me" or the index, [A-Za-z0-9_.-] */
    signed char active, reserved;    /* -1 absent, 0, 1 */
    char address[NO_ADDRESS_CHARS + 1];
    unsigned token_length, token_hash; /* the deviceToken's length and a 16-bit hash */
    int has_address, has_token;
} NoMember;

typedef struct {
    char value[NO_FIELDS][NO_VALUE_MAX + 1];
    unsigned char found[NO_FIELDS];
    unsigned members;
    int members_state; /* 0 none, 1 open (cut), 2 closed */
    NoMember member[NO_MEMBERS_MAX];
    unsigned member_count;
    int member_more, member_current; /* more than NO_MEMBERS_MAX; the member being read, -1 none */
    char member_key[NO_NAME_MAX + 1];
    int has_host;
    unsigned host_length, host_hash;
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
               : no_key_is(key, length, "properties")     ? NO_ROLE_PROPERTIES
                                                          : NO_ROLE_NONE;
    case NO_ROLE_SERVERS:
        return no_key_is(key, length, "matchmaking")          ? NO_ROLE_MM
               : no_key_is(key, length, "gameServerPlatform") ? NO_ROLE_GSP
                                                              : NO_ROLE_NONE;
    case NO_ROLE_MM: return no_key_is(key, length, "properties") ? NO_ROLE_MM_PROPERTIES : NO_ROLE_NONE;
    case NO_ROLE_MM_PROPERTIES: return no_key_is(key, length, "system") ? NO_ROLE_SYSTEM : NO_ROLE_NONE;
    case NO_ROLE_SYSTEM: return no_key_is(key, length, "targetSessionRef") ? NO_ROLE_SYSTEM_TARGET : NO_ROLE_NONE;
    case NO_ROLE_PROPERTIES: return no_key_is(key, length, "system") ? NO_ROLE_PROPERTIES_SYSTEM : NO_ROLE_NONE;
    case NO_ROLE_GSP: return no_key_is(key, length, "properties") ? NO_ROLE_GSP_PROPERTIES : NO_ROLE_NONE;
    case NO_ROLE_GSP_PROPERTIES:
        return no_key_is(key, length, "system")   ? NO_ROLE_GSP_SYSTEM
               : no_key_is(key, length, "custom") ? NO_ROLE_GSP_CUSTOM
                                                  : NO_ROLE_NONE;
    case NO_ROLE_MEMBERS: return NO_ROLE_MEMBER; /* each key is a member ("me" or its index) */
    case NO_ROLE_MEMBER:
        return no_key_is(key, length, "properties")  ? NO_ROLE_MEMBER_PROPERTIES
               : no_key_is(key, length, "constants") ? NO_ROLE_MEMBER_CONSTANTS
                                                     : NO_ROLE_NONE;
    case NO_ROLE_MEMBER_PROPERTIES: return no_key_is(key, length, "system") ? NO_ROLE_MEMBER_PSYSTEM : NO_ROLE_NONE;
    case NO_ROLE_MEMBER_CONSTANTS: return no_key_is(key, length, "system") ? NO_ROLE_MEMBER_CSYSTEM : NO_ROLE_NONE;
    default: return NO_ROLE_NONE;
    }
}

/* The session scalar under `key` in an object of role `parent`. */
static int no_extra(unsigned char parent, const char *key, size_t length) {
    switch (parent) {
    case NO_ROLE_PROPERTIES_SYSTEM: return no_key_is(key, length, "host") ? NO_EXTRA_HOST : NO_EXTRA_NONE;
    case NO_ROLE_MEMBER:
        return no_key_is(key, length, "deviceToken") ? NO_EXTRA_TOKEN
               : no_key_is(key, length, "reserved")  ? NO_EXTRA_RESERVED
                                                     : NO_EXTRA_NONE;
    case NO_ROLE_MEMBER_PSYSTEM:
        return no_key_is(key, length, "active")                ? NO_EXTRA_ACTIVE
               : no_key_is(key, length, "reserved")            ? NO_EXTRA_RESERVED
               : no_key_is(key, length, "secureDeviceAddress") ? NO_EXTRA_ADDRESS
                                                               : NO_EXTRA_NONE;
    case NO_ROLE_MEMBER_CSYSTEM: return no_key_is(key, length, "reserved") ? NO_EXTRA_RESERVED : NO_EXTRA_NONE;
    default: return NO_EXTRA_NONE;
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
    case NO_ROLE_GSP_SYSTEM: return no_key_is(key, length, "status") ? NO_FIELD_GSP_SYSTEM : NO_FIELD_NONE;
    case NO_ROLE_GSP_CUSTOM: return no_key_is(key, length, "status") ? NO_FIELD_GSP_CUSTOM : NO_FIELD_NONE;
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

/* The string whose opening quote is at `i`, measured only: its length (an escape counts one) and
   a 16-bit hash of its bytes, so the host's token can be matched to a member's without either
   being written. Past its closing quote, or 0 when it does not end. */
static size_t no_measure(const unsigned char *p, size_t n, size_t i, unsigned *length, unsigned *hash) {
    unsigned h = 2166136261u, count = 0;
    for (++i; i < n; ++i) {
        unsigned char c = p[i];
        if (c == '"') {
            *length = count;
            *hash = (h ^ (h >> 16)) & 0xffffu;
            return i + 1;
        }
        if (c == '\\') {
            if (i + 1 >= n) return 0;
            c = p[++i];
        }
        h = (h ^ c) * 16777619u;
        if (count < 0xffffffffu) ++count;
    }
    return 0;
}

/* `out` (cap + 1 bytes) gets the safe characters of `key`, at most `cap`. */
static void no_safe_copy(char *out, size_t cap, const char *key, size_t length) {
    size_t n = 0;
    for (; n < cap && n < length && key[n]; ++n) out[n] = no_safe((unsigned char)key[n]) ? key[n] : '?';
    out[n] = 0;
}

static int no_literal_byte(unsigned char c) {
    return c != ',' && c != '}' && c != ']' && c != ':' && c != '"' && c != '{' && c != '[' && c != ' ' &&
           c != '\t' && c != '\r' && c != '\n';
}

static void no_scan(const unsigned char *p, size_t n, NoFields *fields) {
    enum { WANT_VALUE, WANT_KEY, WANT_NEXT };
    unsigned char role[NO_DEPTH], array[NO_DEPTH];
    size_t depth = 0, members_level = 0;
    int state = WANT_VALUE, field = NO_FIELD_NONE, extra = NO_EXTRA_NONE;
    unsigned char child = NO_ROLE_ROOT;
    fields->member_current = -1;
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
                if (c == '{' && child == NO_ROLE_MEMBER) { /* a member: its slot, while there is room */
                    fields->member_current = -1;
                    if (fields->member_count < NO_MEMBERS_MAX) {
                        NoMember *m = &fields->member[fields->member_count];
                        for (size_t k = 0; k <= NO_NAME_MAX; ++k) m->key[k] = fields->member_key[k];
                        m->active = m->reserved = -1;
                        m->address[0] = 0;
                        m->token_length = m->token_hash = 0;
                        m->has_address = m->has_token = 0;
                        fields->member_current = (int)fields->member_count++;
                    } else {
                        fields->member_more = 1;
                    }
                }
                ++depth;
                state = c == '{' ? WANT_KEY : WANT_VALUE;
                child = NO_ROLE_NONE;
                field = NO_FIELD_NONE;
                extra = NO_EXTRA_NONE;
                ++i;
                continue;
            }
            if (c == ']' && depth && array[depth - 1]) { /* an empty array (or a trailing comma) */
                state = WANT_NEXT;
                continue;
            }
            if (extra) { /* a session scalar: measured or a few characters, into its member */
                NoMember *m = fields->member_current >= 0 ? &fields->member[fields->member_current] : 0;
                if (c == '"') {
                    unsigned length = 0, hash = 0;
                    size_t next = no_measure(p, n, i, &length, &hash);
                    if (!next) return;
                    if (extra == NO_EXTRA_HOST && !fields->has_host) {
                        fields->has_host = 1;
                        fields->host_length = length;
                        fields->host_hash = hash;
                    } else if (extra == NO_EXTRA_TOKEN && m && !m->has_token) {
                        m->has_token = 1;
                        m->token_length = length;
                        m->token_hash = hash;
                    } else if (extra == NO_EXTRA_ADDRESS && m && !m->has_address) {
                        size_t ignored;
                        if (!no_string(p, n, i, m->address, NO_ADDRESS_CHARS, &ignored, 0)) return;
                        m->has_address = 1;
                    }
                    i = next;
                } else {
                    size_t start = i;
                    while (i < n && no_literal_byte(p[i])) ++i;
                    if (i == start || i >= n) return;
                    int truth = i - start == 4 && p[start] == 't' && p[start + 1] == 'r' && p[start + 2] == 'u' &&
                                p[start + 3] == 'e';
                    int lie = i - start == 5 && p[start] == 'f' && p[start + 1] == 'a';
                    if (m && (truth || lie)) {
                        if (extra == NO_EXTRA_ACTIVE && m->active < 0) m->active = (signed char)truth;
                        if (extra == NO_EXTRA_RESERVED && (m->reserved < 0 || truth)) m->reserved = (signed char)truth;
                    }
                }
                extra = NO_EXTRA_NONE;
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
            if (parent == NO_ROLE_MEMBERS) no_safe_copy(fields->member_key, NO_NAME_MAX, key, length);
            child = no_child(parent, key, length);
            field = no_field(parent, key, length);
            extra = no_extra(parent, key, length);
            state = WANT_VALUE;
        } else {
            if (c == ',') {
                state = array[depth - 1] ? WANT_VALUE : WANT_KEY;
                child = NO_ROLE_NONE;
                field = NO_FIELD_NONE;
                extra = NO_EXTRA_NONE;
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

static void no_fields_init(NoFields *fields) {
    for (int f = 0; f < NO_FIELDS; ++f) { /* no memset: the hook links no C runtime */
        fields->value[f][0] = 0;
        fields->found[f] = 0;
    }
    fields->members = 0;
    fields->members_state = 0;
    fields->member_count = 0;
    fields->member_more = 0;
    fields->member_current = -1;
    fields->member_key[0] = 0;
    fields->has_host = 0;
    fields->host_length = fields->host_hash = 0;
}

static void no_hash16(NoLine *line, unsigned hash, unsigned length) {
    static const char hex[] = "0123456789abcdef";
    char out[6] = {'#', hex[hash >> 12 & 15], hex[hash >> 8 & 15], hex[hash >> 4 & 15], hex[hash & 15], 0};
    no_text(line, out);
    no_text(line, "/");
    no_number(line, length);
}

/* The members line of a session document into `out` (no line end); 0 when `body` is not a session
   (no members object and no host). "NETWORK: session #<id> host=<#hash/length|-> gsp=<status|->
   members: <key>:<a|-><r|-> addr=<8 chars|-> tok=<#hash/length|->; ..." with at most
   NO_MEMBERS_MAX members ("; +" past them). The host's token and each deviceToken only as a length
   and a 16-bit hash (equal hashes: that member hosts); an address only its first 8 base64
   characters; a member only by its key; no XUID, gamertag or other field. */
int fi_netobs_session_text(char *out, size_t capacity, long id, const unsigned char *body, size_t size) {
    NoLine line;
    NoFields fields;
    line.size = 0;
    line.text[0] = 0;
    no_fields_init(&fields);
    no_scan(body, size, &fields);
    if (capacity) out[0] = 0;
    if (!fields.members_state && !fields.has_host) return 0;
    no_text(&line, "NETWORK: session #");
    no_number(&line, (unsigned long long)(unsigned long)id);
    no_text(&line, " host=");
    if (fields.has_host) no_hash16(&line, fields.host_hash, fields.host_length);
    else no_text(&line, "-");
    no_text(&line, " gsp=");
    no_text(&line, fields.found[NO_FIELD_GSP_SYSTEM]   ? fields.value[NO_FIELD_GSP_SYSTEM]
                   : fields.found[NO_FIELD_GSP_CUSTOM] ? fields.value[NO_FIELD_GSP_CUSTOM]
                                                       : "-");
    no_text(&line, " members:");
    for (unsigned k = 0; k < fields.member_count; ++k) {
        const NoMember *m = &fields.member[k];
        no_text(&line, k ? "; " : " ");
        no_text(&line, m->key[0] ? m->key : "?");
        no_text(&line, ":");
        no_text(&line, m->active > 0 ? "a" : "-");
        no_text(&line, m->reserved > 0 ? "r" : "-");
        no_text(&line, " addr=");
        no_text(&line, m->has_address && m->address[0] ? m->address : "-");
        no_text(&line, " tok=");
        if (m->has_token) no_hash16(&line, m->token_hash, m->token_length);
        else no_text(&line, "-");
    }
    if (!fields.member_count) no_text(&line, " none");
    if (fields.member_more || fields.members_state == 1) no_text(&line, "; +");
    size_t n = 0;
    for (; capacity && n + 1 < capacity && n < line.size; ++n) out[n] = line.text[n];
    if (capacity) out[n] = 0;
    return 1;
}

/* The answer line of an answered request into `out` (no line end). */
void fi_netobs_answer_text(char *out, size_t capacity, long id, unsigned status, const unsigned char *body,
                           size_t size) {
    NoLine line;
    NoFields fields;
    line.size = 0;
    line.text[0] = 0;
    no_fields_init(&fields);
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

/* GET (or no method) or anything else. */
static int no_class(const wchar_t *method, size_t length) {
    if (!method || !length) return NO_CLASS_GET;
    return length == 3 && no_lower(method[0]) == L'g' && no_lower(method[1]) == L'e' && no_lower(method[2]) == L't'
               ? NO_CLASS_GET
               : NO_CLASS_OTHER;
}

/* FNV-1a, 64 bits; never 0 (a free slot). */
static unsigned long long no_hash_bytes(unsigned long long h, const unsigned char *p, size_t n) {
    for (size_t i = 0; i < n; ++i) {
        h ^= p[i];
        h *= 0x100000001b3ull;
    }
    return h;
}

static unsigned long long no_key(const wchar_t *method, size_t method_length, const wchar_t *url, size_t url_length) {
    unsigned long long h = 0xcbf29ce484222325ull;
    if (method) h = no_hash_bytes(h, (const unsigned char *)method, method_length * sizeof(wchar_t));
    h = no_hash_bytes(h, (const unsigned char *)"\0\0", 2); /* the method's end */
    if (url) h = no_hash_bytes(h, (const unsigned char *)url, url_length * sizeof(wchar_t));
    return h ? h : 1;
}

/* The slot of `key` (held lock), or 0. */
static NoSlot *no_find(int family, unsigned long long key) {
    for (int i = 0; i < NO_SLOTS; ++i) {
        if (no_slots[family][i].key == key) return &no_slots[family][i];
    }
    return 0;
}

/* A fresh slot for `key` (held lock): a free one, or the least recently used. */
static NoSlot *no_claim(int family, unsigned long long key) {
    NoSlot *slot = &no_slots[family][0];
    for (int i = 0; i < NO_SLOTS; ++i) {
        NoSlot *s = &no_slots[family][i];
        if (!s->key) {
            slot = s;
            break;
        }
        if (s->used < slot->used) slot = s;
    }
    slot->key = key;
    slot->answer = 0;
    slot->open_id = slot->answer_id = 0;
    slot->open_repeats = slot->answer_repeats = 0;
    slot->has_answer = 0;
    return slot;
}

static LONG no_more(LONG n) { return n < NO_CALLS_MAX ? n + 1 : NO_CALLS_MAX; }
static int no_power_of_two(LONG n) { return n > 0 && !(n & (n - 1)); }

/* Whether a line of `family` and `kind` is still within its class's budget (past it, a count
   line at each power of two, `tail` before its end). */
static int no_budget(volatile LONG *calls, int family, int klass, const char *kind, long id, const char *tail) {
    LONG count = no_next(calls);
    if (count <= NO_FIRST) return 1;
    if (no_power_of_two(count)) {
        NoLine line;
        line.size = 0;
        line.text[0] = 0;
        no_text(&line, "NETWORK: ");
        no_text(&line, no_family_names[family]);
        no_text(&line, " ");
        if (klass >= 0) no_text(&line, no_class_names[klass]);
        no_text(&line, kind);
        no_number(&line, (unsigned long long)count);
        no_text(&line, " (last #");
        no_number(&line, (unsigned long long)(unsigned long)id);
        no_text(&line, tail);
        no_text(&line, "; lines past the first 64 are counted only)\r\n");
        log_append(line.text);
    }
    return 0;
}

int fi_netobs_open(int family, long id, const wchar_t *method, size_t method_length, const wchar_t *url,
                   size_t url_length) {
    if (!no_family_ok(family)) return 0;
    unsigned long long key = no_key(method, method_length, url, url_length);
    LONG repeats = 0;
    long first = 0;
    AcquireSRWLockExclusive(&no_lock);
    NoSlot *slot = no_find(family, key);
    if (slot && slot->answer_repeats) { /* its answers repeat: so does this open */
        repeats = slot->open_repeats = no_more(slot->open_repeats);
        first = slot->open_id;
    } else {
        if (!slot) slot = no_claim(family, key);
        slot->open_id = id;
        slot->open_repeats = 0;
    }
    slot->used = ++no_tick;
    ReleaseSRWLockExclusive(&no_lock);
    if (repeats) {
        if (no_power_of_two(repeats)) {
            NoLine line;
            line.size = 0;
            line.text[0] = 0;
            no_text(&line, "NETWORK: open #");
            no_number(&line, (unsigned long long)(unsigned long)id);
            no_text(&line, " repeated ");
            no_number(&line, (unsigned long long)repeats);
            no_text(&line, repeats == 1 ? " time" : " times");
            no_text(&line, " (same method and URL as #");
            no_number(&line, (unsigned long long)(unsigned long)first);
            no_text(&line, ")\r\n");
            log_append(line.text);
        }
        return 0;
    }
    int klass = no_class(method, method_length);
    return no_budget(&no_opens[family][klass], family, klass, " opens: ", id, "");
}

static void no_answer_line(int family, long id, const wchar_t *method, size_t method_length, const wchar_t *url,
                           size_t url_length, int answered, unsigned status, HRESULT error, const unsigned char *body,
                           size_t size) {
    NoLine line;
    line.size = 0;
    line.text[0] = 0;
    if (answered) {
        fi_netobs_answer_text(line.text, NO_LINE - 80, id, status, body, size);
        while (line.text[line.size]) ++line.size;
    } else {
        no_text(&line, "NETWORK: answer #");
        no_number(&line, (unsigned long long)(unsigned long)id);
        no_text(&line, error == NO_OFFLINE_ERROR ? " offline " : " error ");
        no_hex(&line, (unsigned)error);
    }
    /* what it answered: the line past "NETWORK: answer #<id>" */
    size_t past = 17; /* strlen("NETWORK: answer #") */
    while (past < line.size && line.text[past] != ' ') ++past;
    unsigned long long key = no_key(method, method_length, url, url_length);
    unsigned long long what = no_hash_bytes(0xcbf29ce484222325ull, (const unsigned char *)line.text + past,
                                            line.size - past);
    LONG repeats = 0;
    long first = 0;
    AcquireSRWLockExclusive(&no_lock);
    NoSlot *slot = no_find(family, key);
    if (slot && slot->has_answer && slot->answer == what) {
        repeats = slot->answer_repeats = no_more(slot->answer_repeats);
        first = slot->answer_id;
    } else {
        if (!slot) slot = no_claim(family, key);
        slot->answer = what;
        slot->has_answer = 1;
        slot->answer_id = id;
        slot->answer_repeats = 0; /* a changed answer: its next open is a line again */
        slot->open_repeats = 0;
    }
    slot->used = ++no_tick;
    ReleaseSRWLockExclusive(&no_lock);
    if (repeats) {
        if (!no_power_of_two(repeats)) return;
        no_text(&line, " repeated ");
        no_number(&line, (unsigned long long)repeats);
        no_text(&line, repeats == 1 ? " time" : " times");
        no_text(&line, " (same as #");
        no_number(&line, (unsigned long long)(unsigned long)first);
        no_text(&line, ")\r\n");
        log_append(line.text);
        return;
    }
    NoLine tail;
    tail.size = 0;
    tail.text[0] = 0;
    no_text(&tail, " ");
    if (answered) no_number(&tail, status);
    else no_text(&tail, error == NO_OFFLINE_ERROR ? "offline" : "error");
    int klass = no_class(method, method_length);
    if (!no_budget(&no_answers[family][klass], family, klass, " answers: ", id, tail.text)) return;
    no_text(&line, "\r\n");
    log_append(line.text);
}

/* A session document's members line, when it differs from the last one of the same URL (any
   method): its key in the repeat table is (session, URL), its value the line past its id. */
static void no_session_line(int family, long id, const wchar_t *url, size_t url_length, const unsigned char *body,
                            size_t size) {
    NoLine line;
    line.size = 0;
    if (!fi_netobs_session_text(line.text, NO_LINE - 2, id, body, size)) return;
    while (line.text[line.size]) ++line.size;
    size_t past = 18; /* strlen("NETWORK: session #") */
    while (past < line.size && line.text[past] != ' ') ++past;
    unsigned long long key = no_key(L"session", 7, url, url_length);
    unsigned long long what = no_hash_bytes(0xcbf29ce484222325ull, (const unsigned char *)line.text + past,
                                            line.size - past);
    int same = 0;
    AcquireSRWLockExclusive(&no_lock);
    NoSlot *slot = no_find(family, key);
    if (slot && slot->has_answer && slot->answer == what) {
        same = 1;
    } else {
        if (!slot) slot = no_claim(family, key);
        slot->answer = what;
        slot->has_answer = 1;
        slot->answer_id = id;
    }
    slot->used = ++no_tick;
    ReleaseSRWLockExclusive(&no_lock);
    if (same || !no_budget(&no_sessions[family], family, -1, "session lines: ", id, "")) return;
    no_text(&line, "\r\n");
    log_append(line.text);
}

void fi_netobs_answer(int family, long id, const wchar_t *method, size_t method_length, const wchar_t *url,
                      size_t url_length, int answered, unsigned status, HRESULT error, const unsigned char *body,
                      size_t size) {
    if (!no_family_ok(family)) return;
    no_answer_line(family, id, method, method_length, url, url_length, answered, status, error, body, size);
    if (family == FI_NETOBS_SESSIONDIRECTORY && answered) no_session_line(family, id, url, url_length, body, size);
}