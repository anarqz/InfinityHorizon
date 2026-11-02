/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The game's own matchmaking tickets (T34, OD-182): the game joins the first open lobby or gets
   one of its own, and never reaches "the session you were trying to join no longer exists".

   A ticket's status answer (GET momatch .../hoppers/{FreeRoam,CarMeet,RoadTrip}_qos/tickets/{id})
   says "found" with a target session. The game's ticket (Network::Live::Ticket, its status poll at
   game+0x183bf70 through LiveMatch.GetStatusForMatchTicket) reads ticketStatus as searching,
   expired, found or canceled: "searching" keeps the ticket open and polls it again about five
   seconds later, "found" resolves and joins the target, "expired" and "canceled" end the search.
   A car meet's search falls back to creating its own meet when its matchmaking timeout passes
   (NetworkMatchmakingTimeoutComponent, the stateflow's fallback_to_create_session).

   So a "found" answer reaches the game only while the launcher holds a seat in a world for that
   lobby (its last FreeRoamNeed handshake answered ready: launch.c's fi_launch_seat). Without one
   the launcher is asked again on this request's net worker (it joins the first open lobby or opens
   one). Seated now, the answer still says "searching" once: the server chose its target before the
   launcher sat down, and the game's next poll names the seat's world, which goes through. Not
   seated, the answer says "searching" (only the status changes) and the game asks again; after
   FI_TICKET_ASKS asks in one search (a new search, the hopper's POST, starts them again), or at
   once when online is closed (result 2), it says "expired". After the launcher's CancelMatchmaking
   it says "canceled" without asking, until the next search ("canceled" ends a ticket as "expired"
   does). Outside a live launcher session, for a kind not asked yet, or after a handshake nobody
   answered, nothing changes. One log line per changed answer, with the server's status.

   The target the game then leaves. When the game reads a found target it cannot use, its
   matchmaker drops the found session and writes members.me null on that target; the join step
   then finds no session (NetworkJoinMatch's NoSession) and the game shows "the session you were
   trying to join no longer exists". That decision is the game's own: the hook cannot take it back,
   so such a PUT within 30 s of a passed "found" and before any CreateAssociationAsync is logged
   loudly, once per found answer, with the target's first 8 characters. Never an identity, a body
   or a token. Bounded scans: no allocation, at most TG_DEPTH levels, never past the body. */
#include "infinity.h"
#include "../core/logging.h"

#define TG_DEPTH 64u
#define TG_NAME 64u
#define TG_LEFT_MS 30000ull
#define TG_LINE 320u

static const char *const tg_kinds[3] = {"free roam", "car meet", "Road Trip"};
static SRWLOCK tg_lock = SRWLOCK_INIT;
static unsigned tg_asks[3];                /* asks without a seat since the last passed answer */
static char tg_target[TG_NAME];            /* the last passed target's name ("" none) */
static unsigned long long tg_found_at;     /* when it passed */
static int tg_associated, tg_left_logged;  /* an association since; its leave already logged */

/* ------------------------------------------------------------------ text */

static char tg_lower(char c) { return c >= 'A' && c <= 'Z' ? (char)(c + 32) : c; }
static wchar_t tg_wlower(wchar_t c) { return c >= L'A' && c <= L'Z' ? (wchar_t)(c + 32) : c; }

/* `text` starts with `prefix` (ASCII, lower case), ignoring case */
static int tg_wstarts(const wchar_t *text, const wchar_t *prefix) {
    for (; *prefix; ++text, ++prefix) {
        if (tg_wlower(*text) != *prefix) return 0;
    }
    return 1;
}

/* past the host of an http(s) URL whose host starts with `host` (lower case), or null */
static const wchar_t *tg_path(const wchar_t *url, const wchar_t *host) {
    if (!url) return 0;
    if (tg_wstarts(url, L"https://")) url += 8;
    else if (tg_wstarts(url, L"http://")) url += 7;
    else return 0;
    if (!tg_wstarts(url, host)) return 0;
    while (*url && *url != L'/' && *url != L'?' && *url != L'#') {
        if (*url == L'@' || *url == L'\\') return 0;
        ++url;
    }
    return *url == L'/' ? url : 0;
}

int fi_ticket_hopper(const wchar_t *method, const wchar_t *url) {
    static const wchar_t *const hoppers[3] = {L"freeroam_qos", L"carmeet_qos", L"roadtrip_qos"};
    static const size_t lengths[3] = {12, 11, 12};
    if (!method || !tg_wstarts(method, L"get") || method[3]) return 0;
    const wchar_t *at = tg_path(url, L"momatch.");
    if (!at) return 0;
    for (; *at && *at != L'?' && *at != L'#'; ++at) {
        if (!tg_wstarts(at, L"/hoppers/")) continue;
        const wchar_t *hopper = at + 9;
        for (unsigned k = 0; k < 3; ++k) {
            if (!tg_wstarts(hopper, hoppers[k]) || !tg_wstarts(hopper + lengths[k], L"/tickets/")) continue;
            const wchar_t *id = hopper + lengths[k] + 9, *end = id;
            while (*end && *end != L'/' && *end != L'?' && *end != L'#') ++end;
            return end > id && *end != L'/' ? (int)k + 1 : 0;
        }
        return 0;
    }
    return 0;
}

/* ------------------------------------------------------------------ JSON */

static size_t tg_space(const char *t, size_t size, size_t at) {
    while (at < size && (t[at] == ' ' || t[at] == '\t' || t[at] == '\r' || t[at] == '\n')) ++at;
    return at;
}

/* past the string whose opening quote is at `at`, or `size` when it does not end */
static size_t tg_string_end(const char *t, size_t size, size_t at) {
    for (++at; at < size; ++at) {
        if (t[at] == '\\') ++at;
        else if (t[at] == '"') return at + 1;
    }
    return size;
}

/* past the value at `at`, or `size` when it does not end or nests deeper than TG_DEPTH */
static size_t tg_value_end(const char *t, size_t size, size_t at) {
    if (at >= size) return size;
    if (t[at] == '"') return tg_string_end(t, size, at);
    if (t[at] == '{' || t[at] == '[') {
        unsigned depth = 0;
        while (at < size) {
            char c = t[at];
            if (c == '"') {
                at = tg_string_end(t, size, at);
                continue;
            }
            if (c == '{' || c == '[') {
                if (++depth > TG_DEPTH) return size;
            } else if ((c == '}' || c == ']') && !--depth) {
                return at + 1;
            }
            ++at;
        }
        return size;
    }
    while (at < size && t[at] != ',' && t[at] != '}' && t[at] != ']' && t[at] != ' ' && t[at] != '\t' &&
           t[at] != '\r' && t[at] != '\n') {
        ++at;
    }
    return at;
}

/* the value of `key` (exact, no escapes) among the members of the object whose '{' is at `at`:
   its start, or `size` when the object has no such member or does not parse */
static size_t tg_member(const char *t, size_t size, size_t at, const char *key) {
    size_t key_length = 0;
    while (key[key_length]) ++key_length;
    if (at >= size || t[at] != '{') return size;
    at = tg_space(t, size, at + 1);
    while (at < size && t[at] == '"') {
        size_t key_end = tg_string_end(t, size, at);
        if (key_end >= size) return size;
        size_t length = key_end - at - 2;
        int same = length == key_length;
        for (size_t i = 0; same && i < length; ++i) same = t[at + 1 + i] == key[i];
        at = tg_space(t, size, key_end);
        if (at >= size || t[at] != ':') return size;
        at = tg_space(t, size, at + 1);
        if (same) return at < size ? at : size;
        at = tg_space(t, size, tg_value_end(t, size, at));
        if (at >= size || t[at] != ',') return size;
        at = tg_space(t, size, at + 1);
    }
    return size;
}

/* the top-level object's '{' (past white space and a UTF-8 byte order mark), or `size` */
static size_t tg_root(const char *t, size_t size) {
    size_t at = size >= 3 && (unsigned char)t[0] == 0xef && (unsigned char)t[1] == 0xbb && (unsigned char)t[2] == 0xbf ? 3 : 0;
    at = tg_space(t, size, at);
    return at < size && t[at] == '{' ? at : size;
}

int fi_ticket_found(const unsigned char *body, size_t size, size_t *value_at) {
    const char *t = (const char *)body;
    if (!t || !size) return 0;
    size_t at = tg_member(t, size, tg_root(t, size), "ticketStatus");
    if (at + 7 > size || t[at] != '"' || t[at + 6] != '"') return 0;
    static const char found[] = "found";
    for (unsigned i = 0; i < 5; ++i) {
        if (tg_lower(t[at + 1 + i]) != found[i]) return 0;
    }
    if (value_at) *value_at = at;
    return 1;
}

size_t fi_ticket_target(const unsigned char *body, size_t size, char *name, size_t capacity) {
    const char *t = (const char *)body;
    if (!capacity) return 0;
    name[0] = 0;
    if (!t || !size) return 0;
    size_t ref = tg_member(t, size, tg_root(t, size), "targetSessionRef");
    size_t at = ref < size ? tg_member(t, size, ref, "name") : size;
    if (at >= size || t[at] != '"') return 0;
    size_t end = tg_string_end(t, size, at), n = 0;
    if (end > size || end < at + 2) return 0;
    for (size_t i = at + 1; i + 1 < end; ++i) {
        char c = t[i];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_' || c == '.' ||
              c == '-') ||
            n + 1 >= capacity) {
            name[0] = 0;
            return 0; /* not a plain name, or too long to keep: no target */
        }
        name[n++] = c;
    }
    name[n] = 0;
    return n;
}

size_t fi_ticket_rewrite(const unsigned char *body, size_t size, size_t value_at, const char *status, unsigned char *out,
                         size_t capacity) {
    size_t length = 0;
    while (status[length]) ++length;
    if (!body || value_at + 7 > size || size - 7 + length + 2 > capacity) return 0;
    size_t n = 0;
    for (size_t i = 0; i < value_at; ++i) out[n++] = body[i];
    out[n++] = '"';
    for (size_t i = 0; i < length; ++i) out[n++] = (unsigned char)status[i];
    out[n++] = '"';
    for (size_t i = value_at + 7; i < size; ++i) out[n++] = body[i];
    return n;
}

/* ------------------------------------------------------------------ the log */

typedef struct {
    char text[TG_LINE];
    size_t size;
} TgLine;

static void tg_text(TgLine *line, const char *text) {
    for (size_t i = 0; text[i] && line->size + 1 < TG_LINE; ++i) line->text[line->size++] = text[i];
    line->text[line->size] = 0;
}

static void tg_number(TgLine *line, unsigned long long value) {
    char digits[24];
    unsigned n = 0;
    do {
        digits[n++] = (char)('0' + value % 10);
        value /= 10;
    } while (value && n < sizeof(digits));
    while (n && line->size + 1 < TG_LINE) line->text[line->size++] = digits[--n];
    line->text[line->size] = 0;
}

static void tg_name(TgLine *line, const char *name) {
    char cut[9];
    unsigned n = 0;
    while (name[n] && n < 8) {
        cut[n] = name[n];
        ++n;
    }
    cut[n] = 0;
    tg_text(line, n ? cut : "-");
}

/* ------------------------------------------------------------------ the guard */

/* the guard's line: "TICKET: a <kind> ticket <what> (the server said found): answered "<status>" <why>" */
static const char *tg_answer(unsigned kind, const char *what, int ask, int result, const char *status, const char *why) {
    TgLine line;
    line.size = 0;
    line.text[0] = 0;
    tg_text(&line, "TICKET: a ");
    tg_text(&line, tg_kinds[kind]);
    tg_text(&line, " ticket ");
    tg_text(&line, what);
    if (ask) {
        tg_text(&line, " (ask ");
        tg_number(&line, (unsigned)ask);
        tg_text(&line, " of ");
        tg_number(&line, FI_TICKET_ASKS);
        tg_text(&line, result < 0 ? ", no answer)" : ", result ");
        if (result >= 0) {
            tg_number(&line, (unsigned)result);
            tg_text(&line, ")");
        }
    }
    tg_text(&line, " (the server said \"found\"): answered \"");
    tg_text(&line, status);
    tg_text(&line, "\" ");
    tg_text(&line, why);
    tg_text(&line, ".\r\n");
    log_append(line.text);
    return status;
}

void fi_ticket_guard_search(unsigned kind) {
    if (kind > 2) return;
    AcquireSRWLockExclusive(&tg_lock);
    tg_asks[kind] = 0;
    ReleaseSRWLockExclusive(&tg_lock);
}

const char *fi_ticket_guard_check(unsigned kind, const unsigned char *body, size_t size, volatile LONG *aborted) {
    if (kind > 2) return 0;
    int seat = fi_launch_seat(kind);
    if (seat == FI_SEAT_CANCELLED) {
        /* the launcher gave this search up: never undone by asking again (until the next search) */
        return tg_answer(kind, "was found after the launcher's CancelMatchmaking", 0, 0, "canceled",
                         "(the game ends this search)");
    }
    if (seat < 0) return 0; /* no live launcher session, or this lobby was never asked for (or no answer) */
    if (seat == 0) {
        AcquireSRWLockExclusive(&tg_lock);
        unsigned asks = tg_asks[kind];
        if (asks < FI_TICKET_ASKS) tg_asks[kind] = asks + 1;
        ReleaseSRWLockExclusive(&tg_lock);
        if (asks >= FI_TICKET_ASKS) {
            return tg_answer(kind, "was found but the launcher has no seat after 3 asks", 0, 0, "expired",
                             "(the game ends this search)");
        }
        int result = fi_launch_seat_ask(kind, aborted);
        if (result == 2) {
            return tg_answer(kind, "was found but online is closed", (int)asks + 1, result, "expired",
                             "(the game ends this search)");
        }
        if (result != 0) {
            return tg_answer(kind, "was found but the launcher has no seat", (int)asks + 1, result, "searching",
                             "(the game asks again)");
        }
        /* the server chose this target before the launcher sat down: the next poll names the seat's world */
        return tg_answer(kind, "was found; the launcher is seated after asking again", (int)asks + 1, result,
                         "searching", "once (the next answer names the seat's world)");
    }
    char name[TG_NAME];
    fi_ticket_target(body, size, name, sizeof(name));
    AcquireSRWLockExclusive(&tg_lock);
    tg_asks[kind] = 0;
    for (unsigned i = 0; i < TG_NAME; ++i) tg_target[i] = name[i];
    tg_found_at = GetTickCount64();
    tg_associated = 0;
    tg_left_logged = 0;
    ReleaseSRWLockExclusive(&tg_lock);
    return 0;
}

/* `url`'s path names the session `name` (".../sessions/<name>" then the end, '/', '?' or '#'), ignoring case */
static int tg_names_session(const wchar_t *path, const char *name) {
    for (const wchar_t *at = path; *at && *at != L'?' && *at != L'#'; ++at) {
        if (!tg_wstarts(at, L"/sessions/")) continue;
        const wchar_t *s = at + 10;
        size_t i = 0;
        while (name[i] && tg_wlower(s[i]) == (wchar_t)tg_lower(name[i])) ++i;
        if (!name[i] && (!s[i] || s[i] == L'/' || s[i] == L'?' || s[i] == L'#')) return 1;
    }
    return 0;
}

/* the body's members.me is null: a leave of that session */
static int tg_leaves(const unsigned char *body, size_t size) {
    const char *t = (const char *)body;
    if (!t || !size) return 0;
    size_t members = tg_member(t, size, tg_root(t, size), "members");
    size_t me = members < size ? tg_member(t, size, members, "me") : size;
    return me + 4 <= size && t[me] == 'n' && t[me + 1] == 'u' && t[me + 2] == 'l' && t[me + 3] == 'l';
}

void fi_ticket_guard_request(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size) {
    if (!method || !tg_wstarts(method, L"put") || method[3]) return;
    const wchar_t *path = tg_path(url, L"sessiondirectory.");
    if (!path) return;
    char name[TG_NAME];
    unsigned long long since;
    AcquireSRWLockShared(&tg_lock);
    int armed = tg_target[0] && !tg_associated && !tg_left_logged;
    for (unsigned i = 0; i < TG_NAME; ++i) name[i] = tg_target[i];
    since = tg_found_at;
    ReleaseSRWLockShared(&tg_lock);
    unsigned long long now = GetTickCount64(), waited = now > since ? now - since : 0;
    if (!armed || waited > TG_LEFT_MS || !tg_names_session(path, name) || !tg_leaves(body, size)) return;
    AcquireSRWLockExclusive(&tg_lock);
    int first = !tg_left_logged;
    tg_left_logged = 1;
    ReleaseSRWLockExclusive(&tg_lock);
    if (!first) return;
    TgLine line;
    line.size = 0;
    line.text[0] = 0;
    tg_text(&line, "TICKET: WARNING: the game left its matched target ");
    tg_name(&line, name);
    tg_text(&line, " ");
    tg_number(&line, waited);
    tg_text(&line, " ms after the found answer, before any association: its matchmaker could not use that "
                   "session (\"the session no longer exists\" follows; the hook cannot undo it).\r\n");
    log_append(line.text);
}

void fi_ticket_guard_associated(void) {
    AcquireSRWLockExclusive(&tg_lock);
    tg_associated = 1;
    ReleaseSRWLockExclusive(&tg_lock);
}
