/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The cloud line gate, and the account's tier as display text only.

   OD-140 (owner, 2026-10-29: "Não deve mais ter nenhuma restrição de free-tier. Todos os players
   podem tudo."): the hook keeps no tier gate. The tier ("free" or "vip", from the launch ticket
   or the Beta server's /me) is kept only as the text shown for the account: "Infinity Racer" or
   "Infinity Supporter", the Supporter badge (CR-S69, OD-129). Nothing routes, gates or logs by
   it, and nothing by the profile's mode either.

   OD-128 (owner, 2026-10-28: "If cloud saves are enabled to the current profile, open all gates
   and send the requests. If cloud saves are off for the current profile type, gate it as well";
   kept by the owner with OD-140): in a launcher session the launch ticket's save line (CR-S51
   `line`), known before the game's first request, alone decides. `line: true` sends every game
   service to the server. Without a line the universe's services are answered here
   (fi_tier_no_line_route): the profile has no cloud line, so the server keeps nothing of it:
   - the game's Forza services (*.forzamotorsport.net, by X-ClassName and X-Method): User.Logon,
     GetPermissionsTable, GetEconomySettings, Xls User.GetUserData, Xls
     Notifications.RetrieveForUser and the livery calls of CR-S59 (ContentVersioning
     Create/Retrieve/RemoveReferenceTo VersionedLivery, OnlineProfile.UpdateLivery, Storefront
     GetLiveriesForDrivatar/GetFeaturedLiveriesForRandomCars: the server keeps every ticket's
     liveries) go to the server; the rest of OnlineProfile.* (the profile and garage syncs: no
     cloud copy without a line) is answered here as the server answers a sync it took (200, an
     empty bin/xtsw reply); everything else is answered here with the 503 the game already gets
     offline;
   - Xbox Live: the friends list (social.xboxlive.com, profile.xboxlive.com .../people/...) and
     friends' presence (userpresence.xboxlive.com .../groups/People) are answered here empty; the
     free roam services (sessiondirectory, gameserverds, gameserverms, momatch),
     profile.xboxlive.com and userpresence go to the server; every other host gets the offline
     503 here.
   Outside a launcher session (the retired Beta protocol, OD-69) there is no line and no gate:
   every request goes to the server, as a Supporter's Hardcore profile's always did. */
#include "infinity.h"
#include "../core/support.h"

static volatile LONG tier_value = FI_TIER_VIP; /* display only (OD-140) */
static volatile LONG tier_line = -1;          /* -1 no launch ticket; 0 or 1 the ticket's save line */

void fi_tier_set_line(int line) {
    InterlockedExchange(&tier_line, line < 0 ? -1 : line ? 1 : 0);
}

int fi_tier_universe(void) {
    /* OD-128: the ticket's line alone; no ticket (the Beta protocol): open (OD-140) */
    return tier_line != 0;
}

void fi_tier_set(int tier) {
    InterlockedExchange(&tier_value, tier == FI_TIER_FREE ? FI_TIER_FREE : FI_TIER_VIP);
}

int fi_tier(void) {
    return (int)tier_value;
}

static wchar_t tier_lower(wchar_t c) {
    return c >= L'A' && c <= L'Z' ? (wchar_t)(c + 32) : c;
}

/* `text` (length n) equals `name`, ignoring ASCII case. */
static int tier_equal(const wchar_t *text, size_t n, const wchar_t *name) {
    size_t i = 0;
    for (; i < n && name[i]; ++i) {
        if (tier_lower(text[i]) != tier_lower(name[i])) {
            return 0;
        }
    }
    return i == n && !name[i];
}

/* The value of header `name` in "Name: value\r\n" lines: its start and length, or 0. */
static const wchar_t *tier_header(const wchar_t *headers, const wchar_t *name, size_t *length) {
    size_t k = 0;
    while (name[k]) {
        ++k;
    }
    for (const wchar_t *line = headers; line && *line;) {
        const wchar_t *end = line;
        while (*end && *end != L'\r' && *end != L'\n') {
            ++end;
        }
        const wchar_t *colon = line;
        while (colon < end && *colon != L':') {
            ++colon;
        }
        if (colon < end && tier_equal(line, (size_t)(colon - line), name)) {
            const wchar_t *value = colon + 1;
            while (value < end && *value == L' ') {
                ++value;
            }
            *length = (size_t)(end - value);
            return value;
        }
        line = end;
        while (*line == L'\r' || *line == L'\n') {
            ++line;
        }
    }
    return 0;
}

/* The URL's host and path (path up to '?'). */
static void tier_split(const wchar_t *url, const wchar_t **host, size_t *host_length, const wchar_t **path,
                       size_t *path_length) {
    const wchar_t *p = url;
    for (const wchar_t *s = url; s[0] && s[1] && s[2]; ++s) {
        if (s[0] == L':' && s[1] == L'/' && s[2] == L'/') {
            p = s + 3;
            break;
        }
    }
    *host = p;
    while (*p && *p != L'/' && *p != L':' && *p != L'?') {
        ++p;
    }
    *host_length = (size_t)(p - *host);
    while (*p && *p != L'/' && *p != L'?') {
        ++p; /* a port */
    }
    *path = p;
    while (*p && *p != L'?') {
        ++p;
    }
    *path_length = (size_t)(p - *path);
}

/* `host` is `domain` or a name under it. */
static int tier_host_is(const wchar_t *host, size_t n, const wchar_t *domain) {
    size_t k = 0;
    while (domain[k]) {
        ++k;
    }
    if (n < k || !tier_equal(host + n - k, k, domain)) {
        return 0;
    }
    return n == k || host[n - k - 1] == L'.';
}

static int tier_contains(const wchar_t *text, size_t n, const wchar_t *needle) {
    size_t k = 0;
    while (needle[k]) {
        ++k;
    }
    for (size_t i = 0; i + k <= n; ++i) {
        if (tier_equal(text + i, k, needle)) {
            return 1;
        }
    }
    return 0;
}

static int tier_ends(const wchar_t *text, size_t n, const wchar_t *suffix) {
    size_t k = 0;
    while (suffix[k]) {
        ++k;
    }
    return n >= k && tier_equal(text + n - k, k, suffix);
}

/* ----------------------------------------------------------- foreign XUIDs */

/* Output text with a capacity: `ok` drops to 0 once it would overflow. */
typedef struct {
    char *out;
    size_t n, cap;
    int ok;
} TierText;

static void tier_put(TierText *t, const char *s) {
    for (; *s; ++s) {
        if (t->n + 1 >= t->cap) {
            t->ok = 0;
            t->out[t->n] = 0;
            return;
        }
        t->out[t->n++] = *s;
    }
    t->out[t->n] = 0;
}

static void tier_put_number(TierText *t, unsigned long long v) {
    char digits[24];
    int n = 0;
    do {
        digits[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    char c[2] = {0, 0};
    while (n) {
        c[0] = digits[--n];
        tier_put(t, c);
    }
}

/* The number after `xuid(` or `xuid=` in a URL (0 when none). */
static unsigned long long tier_url_xuid(const wchar_t *url) {
    for (const wchar_t *p = url; p && *p; ++p) {
        if (tier_lower(p[0]) == L'x' && tier_lower(p[1]) == L'u' && tier_lower(p[2]) == L'i' &&
            tier_lower(p[3]) == L'd' && (p[4] == L'(' || p[4] == L'=')) {
            unsigned long long v = 0;
            const wchar_t *d = p + 5;
            if (*d < L'0' || *d > L'9') {
                continue;
            }
            for (; *d >= L'0' && *d <= L'9'; ++d) {
                v = v * 10 + (unsigned long long)(*d - L'0');
            }
            return v;
        }
    }
    return 0;
}

int fi_tier_foreign_route(const wchar_t *method, const wchar_t *url, unsigned long long own, int no_line, char *json,
                          size_t json_cap) {
    const wchar_t *host, *path;
    size_t host_length, path_length;
    int get = method && tier_lower(method[0]) == L'g' && tier_lower(method[1]) == L'e' &&
              tier_lower(method[2]) == L't' && !method[3];
    if (json && json_cap) {
        json[0] = 0;
    }
    if (!url || !own) {
        return FI_ROUTE_PASS;
    }
    tier_split(url, &host, &host_length, &path, &path_length);
    unsigned long long named = tier_url_xuid(url);
    if (!named || named == own) {
        return FI_ROUTE_PASS;
    }
    /* another player: with a cloud line reads go to the server and writes are not sent; without
       one their presence reads as offline here and anything else about them is not sent */
    if (no_line && get && tier_host_is(host, host_length, L"userpresence.xboxlive.com")) {
        TierText t = {json, 0, json_cap, 1};
        if (tier_ends(path, path_length, L"/groups/People")) {
            tier_put(&t, "[]"); /* a list: the friends' presence, empty (the server's own answer) */
            return t.ok ? FI_ROUTE_JSON : FI_ROUTE_UNAVAILABLE;
        }
        tier_put(&t, "{\"xuid\":\"");
        tier_put_number(&t, named);
        tier_put(&t, "\",\"state\":\"Offline\"}");
        return t.ok ? FI_ROUTE_JSON : FI_ROUTE_UNAVAILABLE;
    }
    return no_line || !get ? FI_ROUTE_UNAVAILABLE : FI_ROUTE_PASS;
}

int fi_tier_no_line_route(const wchar_t *url, const wchar_t *headers, const char **json, wchar_t *what,
                          size_t what_capacity) {
    const wchar_t *host, *path;
    size_t host_length, path_length;
    *json = 0;
    if (what && what_capacity) {
        what[0] = 0;
    }
    if (!url) {
        return FI_ROUTE_PASS;
    }
    tier_split(url, &host, &host_length, &path, &path_length);
    if (tier_host_is(host, host_length, L"forzamotorsport.net")) {
        size_t class_length = 0, method_length = 0;
        const wchar_t *class_name = tier_header(headers, L"X-ClassName", &class_length);
        const wchar_t *call = tier_header(headers, L"X-Method", &method_length);
        size_t comma = 0;
        while (class_name && comma < class_length && class_name[comma] != L',') {
            ++comma;
        }
        if (what && what_capacity) {
            size_t n = 0;
            for (size_t i = 0; class_name && i < comma && n + 2 < what_capacity; ++i) {
                what[n++] = class_name[i];
            }
            what[n++] = L'.';
            for (size_t i = 0; call && i < method_length && n + 1 < what_capacity; ++i) {
                what[n++] = call[i];
            }
            what[n] = 0;
        }
        if (class_name && call && tier_equal(class_name, comma, L"Forza.WebServices.User") &&
            (tier_equal(call, method_length, L"Logon") || tier_equal(call, method_length, L"GetPermissionsTable") ||
             tier_equal(call, method_length, L"GetEconomySettings"))) {
            return FI_ROUTE_PASS;
        }
        if (class_name && call && tier_equal(class_name, comma, L"Xls.WebServices.User") &&
            tier_equal(call, method_length, L"GetUserData")) {
            return FI_ROUTE_PASS;
        }
        /* OD-121: the 30 s inbox poll carries the activity report (activity.c); the server answers a
           ticket without a line with the empty inbox */
        if (class_name && call && tier_equal(class_name, comma, L"Xls.WebServices.Notifications") &&
            tier_equal(call, method_length, L"RetrieveForUser")) {
            return FI_ROUTE_PASS;
        }
        /* CR-S59: the liveries are stored on the server for every ticket (each profile's own, served
           to readers of its mode); the two Storefront livery lists are answered empty there */
        if (class_name && call &&
            ((tier_equal(class_name, comma, L"Forza.WebServices.ContentVersioning") &&
              (tier_equal(call, method_length, L"CreateVersionedLivery") ||
               tier_equal(call, method_length, L"RetrieveVersionedLiveryById") ||
               tier_equal(call, method_length, L"RemoveReferenceToVersionedLivery"))) ||
             (tier_equal(class_name, comma, L"Forza.WebServices.OnlineProfile") && tier_equal(call, method_length, L"UpdateLivery")) ||
             (tier_equal(class_name, comma, L"Forza.WebServices.Storefront") &&
              (tier_equal(call, method_length, L"GetLiveriesForDrivatar") ||
               tier_equal(call, method_length, L"GetFeaturedLiveriesForRandomCars"))))) {
            return FI_ROUTE_PASS;
        }
        if (class_name && tier_equal(class_name, comma, L"Forza.WebServices.OnlineProfile")) {
            return FI_ROUTE_EMPTY_XTSW;
        }
        return FI_ROUTE_UNAVAILABLE;
    }
    if (what && what_capacity) {
        size_t n = 0;
        for (size_t i = 0; i < host_length && n + 1 < what_capacity; ++i) {
            what[n++] = host[i];
        }
        what[n] = 0;
    }
    if (tier_host_is(host, host_length, L"social.xboxlive.com")) {
        *json = "{\"totalCount\":0,\"people\":[]}";
        return FI_ROUTE_JSON;
    }
    if (tier_host_is(host, host_length, L"profile.xboxlive.com") && tier_contains(path, path_length, L"/people/")) {
        *json = "{\"profileUsers\":[]}";
        return FI_ROUTE_JSON;
    }
    if (tier_host_is(host, host_length, L"userpresence.xboxlive.com") && tier_ends(path, path_length, L"/groups/People")) {
        *json = "[]";
        return FI_ROUTE_JSON;
    }
    static const wchar_t *const sent[] = {L"sessiondirectory.xboxlive.com", L"gameserverds.xboxlive.com",
                                          L"gameserverms.xboxlive.com",     L"momatch.xboxlive.com",
                                          L"profile.xboxlive.com",          L"userpresence.xboxlive.com"};
    for (size_t i = 0; i < sizeof(sent) / sizeof(sent[0]); ++i) {
        if (tier_host_is(host, host_length, sent[i])) {
            return FI_ROUTE_PASS;
        }
    }
    return FI_ROUTE_UNAVAILABLE;
}
