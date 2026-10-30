/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Car meets online. Both changes need the online world (free roam on) and check every byte they
   change first; a byte that differs leaves that change out and says so in the log.

   - Joining a car meet invitation. The game joins the session the invitation names through
     MatchRequester::SetSession (game+0x17b8c50), but the lobby type its next OpenConnection
     carries (MatchRequester+0x160) is still the car meet's, so the joiner would land in a car
     meet world again. A detour sets it from the synced invitation before the game's own code
     runs: online free roam (1) is lobby type 2, Road Trip (2) is lobby type 1. Only a car meet
     lobby type (3, or 6 on the island) is ever changed, so every other join is as before. The
     invitation is the CarMeetSynced singleton's (its holder at game+0x34d5c68, the object at
     holder+0x20, as game+0x4021b0 returns it; the type from its virtual getter at slot 0xa0).
   - "Start Online Session". The car meet screen's online test ends with the local car's network
     readiness (game+0x830b8c: test al,al; je), which is never true here, so the online entries
     stay hidden. The je becomes two nops.
   - An offer of the session the game is already in. While the game looks for a lobby, the
     matchmaker's check of a found ticket compares the offered session's name with the game's
     current one (game+0x17b07be: test eax,eax on the comparison; jnz) and drops an equal offer:
     the ticket stays found and nothing joins, so "Joining session" never ends. The game keeps
     its last session after leaving it, and a new ticket can land on that same shared session
     (a car meet or Road Trip world, or a quick rejoin). The jnz becomes jmp, so the offer is
     followed. The check runs only during a search, never inside an established session. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define CM_SET_SESSION_RVA 0x17b8c50u
#define CM_SYNCED_HOLDER_RVA 0x34d5c68u
#define CM_SYNCED_GET_RVA 0x4021b0u
#define CM_INVITE_TYPE_SLOT (0xa0u / 8u)
#define CM_LOBBY_TYPE_OFFSET 0x160u
#define CM_ONLINE_TEST_RVA 0x830b8cu
#define CM_SAME_SESSION_RVA 0x17b07beu

/* mov [rsp+8],rbx; mov [rsp+10h],rsi; push rdi; sub rsp,20h */
static const unsigned char cm_set_session_bytes[15] = {0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x74,
                                                       0x24, 0x10, 0x57, 0x48, 0x83, 0xec, 0x20};
/* lea rax,[rcx+20h]; ret */
static const unsigned char cm_synced_get_bytes[5] = {0x48, 0x8d, 0x41, 0x20, 0xc3};
/* test al,al; je +5; mov dil,1; jmp +3; xor dil,dil */
static const unsigned char cm_online_test_bytes[12] = {0x84, 0xc0, 0x74, 0x05, 0x40, 0xb7,
                                                       0x01, 0xeb, 0x03, 0x40, 0x32, 0xff};
/* test eax,eax; jnz +4; xor bl,bl; jmp +2; mov bl,1 */
static const unsigned char cm_same_session_bytes[10] = {0x85, 0xc0, 0x75, 0x04, 0x32, 0xdb, 0xeb, 0x02, 0xb3, 0x01};

typedef void (*SetSessionFn)(void *requester, void **session);
typedef unsigned char (*InviteTypeFn)(void *synced);
static unsigned char *cm_game;
static SetSessionFn cm_set_session_original;

/* The running invitation's type: 1 online free roam, 2 Road Trip; 0 when there is none. */
static unsigned cm_invite_type(void) {
    void *holder = 0, *synced = 0, **vtable = 0;
    if (!memory_read_pointer(cm_game + CM_SYNCED_HOLDER_RVA, &holder) ||
        !memory_read_pointer((unsigned char *)holder + 0x20, &synced) || !memory_read_pointer(synced, (void **)&vtable)) {
        return 0;
    }
    void *getter = 0;
    if (!memory_read_pointer(vtable + CM_INVITE_TYPE_SLOT, &getter)) {
        return 0;
    }
    return ((InviteTypeFn)(uintptr_t)getter)(synced);
}

static void cm_set_session(void *requester, void **session) {
    int *lobby = (int *)((unsigned char *)requester + CM_LOBBY_TYPE_OFFSET);
    int current = 0;
    if (memory_read(lobby, &current, sizeof(current)) && (current == 3 || current == 6)) {
        unsigned invite = cm_invite_type();
        if (invite == 1 || invite == 2) {
            *lobby = invite == 2 ? 1 : 2;
            log_append(invite == 2 ? "CARMEET: joining a Road Trip invitation, lobby type 1.\r\n"
                                   : "CARMEET: joining an online free roam invitation, lobby type 2.\r\n");
        }
    }
    cm_set_session_original(requester, session);
}

static int cm_patch_online_test(void) {
    unsigned char *at = cm_game + CM_ONLINE_TEST_RVA, current[sizeof(cm_online_test_bytes)];
    if (!memory_read(at, current, sizeof(current)) || !memory_equal(current, cm_online_test_bytes, sizeof(current))) {
        return 0;
    }
    static const unsigned char nops[2] = {0x90, 0x90};
    DWORD old_protection, ignored;
    if (!VirtualProtect(at + 2, sizeof(nops), PAGE_EXECUTE_READWRITE, &old_protection)) {
        return 0;
    }
    memory_copy(at + 2, nops, sizeof(nops));
    FlushInstructionCache(GetCurrentProcess(), at + 2, sizeof(nops));
    VirtualProtect(at + 2, sizeof(nops), old_protection, &ignored);
    return 1;
}

static int cm_patch_same_session(void) {
    unsigned char *at = cm_game + CM_SAME_SESSION_RVA, current[sizeof(cm_same_session_bytes)];
    if (!memory_read(at, current, sizeof(current)) || !memory_equal(current, cm_same_session_bytes, sizeof(current))) {
        return 0;
    }
    static const unsigned char jmp = 0xeb;
    DWORD old_protection, ignored;
    if (!VirtualProtect(at + 2, 1, PAGE_EXECUTE_READWRITE, &old_protection)) {
        return 0;
    }
    memory_copy(at + 2, &jmp, 1);
    FlushInstructionCache(GetCurrentProcess(), at + 2, 1);
    VirtualProtect(at + 2, 1, old_protection, &ignored);
    return 1;
}

void fi_carmeet_apply(unsigned char *game_image) {
    static long installed;
    if (InterlockedCompareExchange(&installed, 1, 0) != 0) {
        return;
    }
    cm_game = game_image;
    unsigned char current[sizeof(cm_synced_get_bytes)];
    unsigned char *code = VirtualAlloc(0, 64, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    void *original = 0;
    if (code && memory_read(game_image + CM_SYNCED_GET_RVA, current, sizeof(current)) &&
        memory_equal(current, cm_synced_get_bytes, sizeof(current))) {
        original = memory_detour14(game_image + CM_SET_SESSION_RVA, cm_set_session_bytes, sizeof(cm_set_session_bytes),
                                   (const void *)(uintptr_t)cm_set_session, code);
    }
    if (original) {
        cm_set_session_original = (SetSessionFn)(uintptr_t)original;
        log_append("CARMEET: invitation joins take the invitation's lobby type.\r\n");
    } else {
        if (code) {
            VirtualFree(code, 0, MEM_RELEASE);
        }
        log_append("CARMEET: the session join code is not as expected; invitation joins left as is.\r\n");
    }
    log_append(cm_patch_online_test() ? "CARMEET: the online car meet entries are shown.\r\n"
                                      : "CARMEET: the online car meet test is not as expected; left as is.\r\n");
    log_append(cm_patch_same_session() ? "CARMEET: an offer of the current session is joined.\r\n"
                                       : "CARMEET: the found-session check is not as expected; left as is.\r\n");
}

/* ------------------------------------------------------------ ticket diagnostics */
/* The game's POST .../hoppers/CarMeet_qos body (net.c, before it is forwarded): the scalar values
   of its ticketAttributes and its giveUpDuration, one bounded line. Keys that could identify a
   player or a session (xuid, gamertag, session, name, address, host, device, token, playerid, user,
   member, party, friend) are left out; a string is shown only when short and plain, else <text>;
   objects and arrays as {..} and [..]. Read only, never a protocol decision. */
#define CM_TICKET_LINE 480u
#define CM_TICKET_KEY 48u

static void cm_line(char *line, size_t *n, const char *text, size_t length) {
    for (size_t i = 0; i < length && text[i] && *n + 1 < CM_TICKET_LINE; ++i) line[(*n)++] = text[i];
    line[*n] = 0;
}

static char cm_lower(char c) { return c >= 'A' && c <= 'Z' ? (char)(c + 32) : c; }

/* `key` (length `length`) contains `needle` (lower case), ignoring case */
static int cm_key_has(const char *key, size_t length, const char *needle) {
    size_t m = 0;
    while (needle[m]) ++m;
    for (size_t i = 0; i + m <= length; ++i) {
        size_t j = 0;
        while (j < m && cm_lower(key[i + j]) == needle[j]) ++j;
        if (j == m) return 1;
    }
    return 0;
}

static int cm_key_private(const char *key, size_t length) {
    static const char *const words[] = {"xuid", "gamertag", "session", "name", "addr", "host", "device",
                                        "token", "playerid", "user", "member", "party", "friend", "secure"};
    for (size_t i = 0; i < sizeof(words) / sizeof(words[0]); ++i) {
        if (cm_key_has(key, length, words[i])) return 1;
    }
    return 0;
}

static size_t cm_space(const char *text, size_t size, size_t at) {
    while (at < size && (text[at] == ' ' || text[at] == '\t' || text[at] == '\r' || text[at] == '\n')) ++at;
    return at;
}

/* past the JSON string starting at `at` (its opening quote), or `size` when it does not end */
static size_t cm_string_end(const char *text, size_t size, size_t at) {
    for (++at; at < size; ++at) {
        if (text[at] == '\\') ++at;
        else if (text[at] == '"') return at + 1;
    }
    return size;
}

/* past the value at `at` (string, object, array or a bare literal) */
static size_t cm_value_end(const char *text, size_t size, size_t at) {
    if (at >= size) return size;
    if (text[at] == '"') return cm_string_end(text, size, at);
    if (text[at] == '{' || text[at] == '[') {
        unsigned depth = 0;
        while (at < size) {
            char c = text[at];
            if (c == '"') { at = cm_string_end(text, size, at); continue; }
            if (c == '{' || c == '[') ++depth;
            else if ((c == '}' || c == ']') && !--depth) return at + 1;
            ++at;
        }
        return size;
    }
    while (at < size && text[at] != ',' && text[at] != '}' && text[at] != ']' && text[at] != ' ' && text[at] != '\r' &&
           text[at] != '\n' && text[at] != '\t') {
        ++at;
    }
    return at;
}

/* key=value for the line (the key already allowed); a plain short string as is, else <text> */
static void cm_line_value(char *line, size_t *n, const char *key, size_t key_length, const char *value, size_t length) {
    cm_line(line, n, " ", 1);
    cm_line(line, n, key, key_length < CM_TICKET_KEY ? key_length : CM_TICKET_KEY);
    cm_line(line, n, "=", 1);
    if (!length) return;
    if (value[0] == '{') { cm_line(line, n, "{..}", 4); return; }
    if (value[0] == '[') { cm_line(line, n, "[..]", 4); return; }
    if (value[0] == '"') {
        int plain = length >= 2 && length - 2 <= 24;
        for (size_t i = 1; plain && i + 1 < length; ++i) {
            char c = value[i];
            plain = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-';
        }
        if (plain) cm_line(line, n, value, length);
        else cm_line(line, n, "<text>", 6);
        return;
    }
    cm_line(line, n, value, length < 24 ? length : 24);
}

/* the value of the first `"key"` found at any depth in `text`: its start, or `size` */
static size_t cm_find_value(const char *text, size_t size, const char *key) {
    const char *found = text_find(text, size, key);
    if (!found) return size;
    size_t at = cm_space(text, size, (size_t)(found - text) + text_length(key));
    if (at >= size || text[at] != ':') return size;
    return cm_space(text, size, at + 1);
}

void fi_carmeet_ticket_log(const unsigned char *body, size_t size) {
    const char *text = (const char *)body;
    char line[CM_TICKET_LINE];
    size_t n = 0;
    line[0] = 0;
    cm_line(line, &n, "CARMEET: ticket", 15);
    size_t at = text && size ? cm_find_value(text, size, "\"ticketAttributes\"") : size;
    if (!text || at >= size || text[at] != '{') {
        cm_line(line, &n, " without ticketAttributes", 25);
    } else {
        size_t end = cm_value_end(text, size, at);
        at = cm_space(text, end, at + 1);
        for (unsigned count = 0; at < end && text[at] == '"' && count < 32; ++count) {
            size_t key_end = cm_string_end(text, end, at);
            const char *key = text + at + 1;
            size_t key_length = key_end > at + 1 ? key_end - at - 2 : 0;
            at = cm_space(text, end, key_end);
            if (at >= end || text[at] != ':') break;
            at = cm_space(text, end, at + 1);
            size_t value_end = cm_value_end(text, end, at);
            if (!cm_key_private(key, key_length)) cm_line_value(line, &n, key, key_length, text + at, value_end - at);
            at = cm_space(text, end, value_end);
            if (at < end && text[at] == ',') at = cm_space(text, end, at + 1);
        }
    }
    at = text && size ? cm_find_value(text, size, "\"giveUpDuration\"") : size;
    if (at < size) cm_line_value(line, &n, "giveUpDuration", 14, text + at, cm_value_end(text, size, at) - at);
    cm_line(line, &n, "\r\n", 2);
    log_append(line);
}
