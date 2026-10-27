/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The account's tier, as the server says it (/me "tier": "free" or "vip"; no tier, as the Beta
   server answers, is "vip", so nothing changes until a server sends it), and what a free
   account's game requests get (OD-24, OD-25: a free account hosts a world and joins free worlds
   through the server's directory; it sees and creates nothing of the universe: no records,
   leaderboards, rivals, ghosts, clubs, designs or storefront, no friends, no Message Center, no
   cloud data). The server is the authority; the hook only keeps the game from asking:

   - the game's Forza services (*.forzamotorsport.net, by X-ClassName and X-Method):
     User.Logon and User.GetPermissionsTable go to the server (the identity, and the table
     without which the game takes the online services for down); OnlineProfile.* (the profile
     and garage syncs: a free profile has no cloud copy) is answered here as the server answers
     a sync it took (200, an empty bin/xtsw reply); everything else is answered here with the
     503 the game already gets offline;
   - Xbox Live: the friends list (social.xboxlive.com, profile.xboxlive.com .../people/...) and
     friends' presence (userpresence.xboxlive.com .../groups/People) are answered here empty;
     the player's own presence posts are taken here; another player's presence is 503; the free
     roam services (sessiondirectory, gameserverds, gameserverms, momatch) and the rest go to the
     server as for anyone.
   In a launcher session (the platform, OD-40, CR-S14, GAME_PLATFORM.md §3a) the list is closed:
   a free ticket sends only the free roam services, profile.xboxlive.com (minus the friends
   list), userpresence (minus friends' presence) and, of the Forza services, User.Logon,
   GetPermissionsTable, GetEconomySettings, Xls User.GetUserData, Xls Notifications.RetrieveForUser
   and the livery calls of CR-S59 (ContentVersioning Create/Retrieve/RemoveReferenceTo VersionedLivery,
   OnlineProfile.UpdateLivery, Storefront GetLiveriesForDrivatar/GetFeaturedLiveriesForRandomCars:
   the server keeps every ticket's liveries);
   the rest of OnlineProfile.* is answered as taken, as above, and every other game request, on
   any host, gets the offline 503 here.
   Nothing here changes for an Infinity Supporter (vip) on a Hardcore profile.

   OD-52: only a Supporter's Hardcore profile keeps data on the servers. A Supporter playing a
   Noob profile gets exactly what a free one gets (fi_tier_universe() is 0), so the hook never
   asks for what the server refuses to a vip noob ticket (OD-51, both sides). The tier itself
   stays vip: the queue priority and the overlay's Supporter features are the account's. */
#include "infinity.h"
#include "../core/support.h"

static volatile LONG tier_value = FI_TIER_VIP;
static volatile LONG tier_noob = 0;

void fi_tier_set_noob(int noob) {
    InterlockedExchange(&tier_noob, noob ? 1 : 0);
}

int fi_tier_universe(void) {
    return tier_value != FI_TIER_FREE && !tier_noob;
}

void fi_tier_set(int tier) {
    InterlockedExchange(&tier_value, tier == FI_TIER_FREE ? FI_TIER_FREE : FI_TIER_VIP);
}

int fi_tier(void) {
    return (int)tier_value;
}

int fi_tier_free(void) {
    return tier_value == FI_TIER_FREE;
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

static int strcmp_ascii(const char *a, const char *b) {
    while (*a && *a == *b) {
        ++a, ++b;
    }
    return (unsigned char)*a - (unsigned char)*b;
}

static void tier_put(TierText *t, const char *s) {
    for (; *s; ++s) {
        if (t->n + 1 >= t->cap) {
            t->ok = 0;
            return;
        }
        t->out[t->n++] = *s;
    }
    t->out[t->n] = 0;
}

static void tier_put_wide(TierText *t, const wchar_t *s) {
    char c[2] = {0, 0};
    for (; s && *s; ++s) {
        c[0] = *s >= 32 && *s < 127 && *s != L'"' && *s != L'\\' ? (char)*s : '?';
        tier_put(t, c);
    }
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

static const char *const tier_first[] = {"Apex",  "Late",   "Neon",  "Curb",    "Ghost",    "Redline", "Slip",  "Turbo",
                                         "Horizon", "Gravel", "Nitro", "Midnight", "Drift",  "Torque",  "Chrome", "Dusty",
                                         "Rapid", "Silent", "Wild",  "Lucky",   "Brave",    "Crimson", "Solar", "Rally"};
static const char *const tier_second[] = {"Hunter", "Braker", "Rider", "Kisser", "Line",  "Rita",  "Sam",   "Taxi",
                                          "Hero",   "Grace",  "Nico",  "Mara",   "King",  "Fox",   "Wolf",  "Viper",
                                          "Pilot",  "Comet",  "Duke",  "Nova",   "Rex",   "Joy",   "Blaze", "Storm"};

void fi_tier_generated_gamertag(unsigned long long xuid, char *out, size_t cap) {
    /* the Beta server's generated_gamertag (forza.rs): stable, readable, at most 15 characters */
    unsigned long long h = xuid ^ 0x9e3779b97f4a7c15ull;
    h = (h ^ (h >> 30)) * 0xbf58476d1ce4e5b9ull;
    h = (h ^ (h >> 27)) * 0x94d049bb133111ebull;
    h ^= h >> 31;
    char tag[40];
    TierText t = {tag, 0, sizeof(tag), 1};
    tag[0] = 0;
    tier_put(&t, tier_first[h % (sizeof(tier_first) / sizeof(tier_first[0]))]);
    tier_put(&t, tier_second[(h >> 16) % (sizeof(tier_second) / sizeof(tier_second[0]))]);
    tier_put_number(&t, (h >> 32) % 100);
    size_t n = 0;
    for (; tag[n] && n < 15 && n + 1 < cap; ++n) {
        out[n] = tag[n];
    }
    if (cap) {
        out[n] = 0;
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

/* The JSON array of `key` in `body`: its first byte after '[' and its end (']'), or 0. */
static const unsigned char *tier_json_array(const unsigned char *body, size_t size, const char *key,
                                            const unsigned char **end) {
    size_t k = 0;
    while (key[k]) {
        ++k;
    }
    for (size_t i = 0; i + k + 2 < size; ++i) {
        if (body[i] != '"') {
            continue;
        }
        size_t j = 0;
        while (j < k && body[i + 1 + j] == (unsigned char)key[j]) {
            ++j;
        }
        if (j != k || body[i + 1 + k] != '"') {
            continue;
        }
        size_t p = i + 2 + k;
        while (p < size && (body[p] == ' ' || body[p] == ':' || body[p] == '\t' || body[p] == '\r' || body[p] == '\n')) {
            ++p;
        }
        if (p >= size || body[p] != '[') {
            return 0;
        }
        for (size_t q = p + 1; q < size; ++q) {
            if (body[q] == ']') {
                *end = body + q;
                return body + p + 1;
            }
        }
        return 0;
    }
    return 0;
}

int fi_tier_foreign_route(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size,
                          unsigned long long own, const wchar_t *own_tag, int free_tier, char *json, size_t json_cap) {
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
    if (named && named != own) {
        /* another player's presence reads as offline; anything else about them is not sent */
        if (free_tier && get && tier_host_is(host, host_length, L"userpresence.xboxlive.com")) {
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
        return free_tier || !get ? FI_ROUTE_UNAVAILABLE : FI_ROUTE_PASS;
    }
    if (!free_tier || get || !tier_host_is(host, host_length, L"profile.xboxlive.com") ||
        !tier_ends(path, path_length, L"/batch/profile/settings")) {
        return FI_ROUTE_PASS;
    }
    /* in a launcher session the server answers it (OD-63, Mac 2026-10-19): the real gamertags of
       the players the ticket shares a world with, generated ones for everyone else (drivatars) */
    if (fi_launch_protocol()) {
        return FI_ROUTE_PASS;
    }
    /* outside one (the Beta), a free ticket's batch lookup naming anyone but the player (the
       save's drivatars): answered here as the Beta server answered it, with a generated gamertag */
    const unsigned char *ids_end = 0, *ids = tier_json_array(body, size, "userIds", &ids_end);
    if (!ids) {
        return FI_ROUTE_PASS;
    }
    int foreign = 0;
    for (const unsigned char *p = ids; p < ids_end;) {
        while (p < ids_end && (*p < '0' || *p > '9')) {
            ++p;
        }
        unsigned long long v = 0;
        int digits = 0;
        for (; p < ids_end && *p >= '0' && *p <= '9'; ++p, ++digits) {
            v = v * 10 + (unsigned long long)(*p - '0');
        }
        foreign |= digits && v != own;
    }
    if (!foreign) {
        return FI_ROUTE_PASS;
    }
    const unsigned char *settings_end = 0, *settings = tier_json_array(body, size, "settings", &settings_end);
    TierText t = {json, 0, json_cap, 1};
    tier_put(&t, "{\"profileUsers\":[");
    int first_user = 1;
    for (const unsigned char *p = ids; p < ids_end;) {
        while (p < ids_end && (*p < '0' || *p > '9')) {
            ++p;
        }
        unsigned long long v = 0;
        int digits = 0;
        for (; p < ids_end && *p >= '0' && *p <= '9'; ++p, ++digits) {
            v = v * 10 + (unsigned long long)(*p - '0');
        }
        if (!digits) {
            continue;
        }
        char tag[16];
        fi_tier_generated_gamertag(v, tag, sizeof(tag));
        tier_put(&t, first_user ? "{\"id\":\"" : ",{\"id\":\"");
        first_user = 0;
        tier_put_number(&t, v);
        tier_put(&t, "\",\"hostId\":\"");
        tier_put_number(&t, v);
        tier_put(&t, "\",\"settings\":[");
        int first_setting = 1;
        for (const unsigned char *s = settings; s && s < settings_end;) {
            while (s < settings_end && *s != '"') {
                ++s;
            }
            const unsigned char *name = ++s;
            while (s < settings_end && *s != '"') {
                ++s;
            }
            if (s >= settings_end) {
                break;
            }
            char setting[32];
            size_t n = 0;
            for (const unsigned char *c = name; c < s && n + 1 < sizeof(setting); ++c) {
                setting[n++] = (char)(*c >= 32 && *c < 127 && *c != '\\' ? *c : '?');
            }
            setting[n] = 0;
            ++s;
            tier_put(&t, first_setting ? "{\"id\":\"" : ",{\"id\":\"");
            first_setting = 0;
            tier_put(&t, setting);
            tier_put(&t, "\",\"value\":\"");
            int name_like = !strcmp_ascii(setting, "Gamertag") || !strcmp_ascii(setting, "GameDisplayName") ||
                            !strcmp_ascii(setting, "AppDisplayName");
            if (name_like) {
                if (v == own && own_tag && *own_tag) {
                    tier_put_wide(&t, own_tag);
                } else {
                    tier_put(&t, tag);
                }
            } else if (!strcmp_ascii(setting, "AccountTier")) {
                tier_put(&t, "Gold");
            } else if (!strcmp_ascii(setting, "Gamerscore")) {
                tier_put(&t, "0");
            } else if (!strcmp_ascii(setting, "GameDisplayPicRaw") || !strcmp_ascii(setting, "AppDisplayPicRaw")) {
                /* an absolute URL: an empty one crashes the game (Beta, game+0x124f916) */
                tier_put(&t, "http://gamerpic.fh2online/pic?xuid=");
                tier_put_number(&t, v);
            }
            tier_put(&t, "\"}");
        }
        tier_put(&t, "],\"isSponsoredUser\":false}");
    }
    tier_put(&t, "]}");
    return t.ok ? FI_ROUTE_JSON : FI_ROUTE_UNAVAILABLE;
}

int fi_tier_free_route(const wchar_t *method, const wchar_t *url, const wchar_t *headers, const char **json,
                       wchar_t *what, size_t what_capacity) {
    const wchar_t *host, *path;
    size_t host_length, path_length;
    int platform = fi_launch_protocol();
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
             (platform && tier_equal(call, method_length, L"GetEconomySettings")))) {
            return FI_ROUTE_PASS;
        }
        if (platform && class_name && call && tier_equal(class_name, comma, L"Xls.WebServices.User") &&
            tier_equal(call, method_length, L"GetUserData")) {
            return FI_ROUTE_PASS;
        }
        /* OD-121: the 30 s inbox poll carries the activity report (activity.c); the free game plane
           answers it with the empty inbox */
        if (platform && class_name && call && tier_equal(class_name, comma, L"Xls.WebServices.Notifications") &&
            tier_equal(call, method_length, L"RetrieveForUser")) {
            return FI_ROUTE_PASS;
        }
        /* CR-S59: the liveries are stored on the server for every ticket (each profile's own, served
           to readers of its mode); the two Storefront livery lists are answered empty there */
        if (platform && class_name && call &&
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
    if (tier_host_is(host, host_length, L"userpresence.xboxlive.com")) {
        if (tier_ends(path, path_length, L"/groups/People")) {
            *json = "[]";
            return FI_ROUTE_JSON;
        }
        if (platform) {
            return FI_ROUTE_PASS;
        }
        if (method && !(tier_lower(method[0]) == L'g' && tier_lower(method[1]) == L'e' &&
                        tier_lower(method[2]) == L't' && !method[3])) {
            *json = "{}";
            return FI_ROUTE_JSON;
        }
        return FI_ROUTE_UNAVAILABLE;
    }
    if (platform) {
        static const wchar_t *const sent[] = {L"sessiondirectory.xboxlive.com", L"gameserverds.xboxlive.com",
                                              L"gameserverms.xboxlive.com", L"momatch.xboxlive.com",
                                              L"profile.xboxlive.com"};
        for (size_t i = 0; i < sizeof(sent) / sizeof(sent[0]); ++i) {
            if (tier_host_is(host, host_length, sent[i])) {
                return FI_ROUTE_PASS;
            }
        }
        return FI_ROUTE_UNAVAILABLE;
    }
    return FI_ROUTE_PASS;
}
