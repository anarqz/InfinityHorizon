/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The account's tier as display text only, and the foreign-XUID rule.

   OD-140 (owner, 2026-10-29: "Não deve mais ter nenhuma restrição de free-tier. Todos os players
   podem tudo."): the hook keeps no tier gate. The tier ("free" or "vip", from the launch ticket
   or the Beta server's /me) is kept only as the text shown for the account: "Infinity Racer" or
   "Infinity Supporter", the Supporter badge (CR-S69, OD-129). Nothing routes, gates or logs by
   it, and nothing by the profile's mode either.

   OD-239 (9, CR-S111, T-1111-AZIMUTH) replaces OD-128's line gate: the launch ticket's save line
   (CR-S51 `line`) means only whether the profile has a cloud save (the launcher's, ADR 0103). It
   routes nothing: with or without it every game service goes to the server, the universe, other
   players' reads, the profile syncs (OnlineProfile.UpdateAll/UpdateAllTables: they feed the
   drivatars, OD-239 10) and the liveries included (the owner wants liveries shared). */
#include "infinity.h"
#include "../core/support.h"

static volatile LONG tier_value = FI_TIER_VIP; /* display only (OD-140) */

void fi_tier_set(int tier) {
    InterlockedExchange(&tier_value, tier == FI_TIER_FREE ? FI_TIER_FREE : FI_TIER_VIP);
}

int fi_tier(void) {
    return (int)tier_value;
}

static wchar_t tier_lower(wchar_t c) {
    return c >= L'A' && c <= L'Z' ? (wchar_t)(c + 32) : c;
}


/* ----------------------------------------------------------- foreign XUIDs */

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

int fi_tier_foreign_route(const wchar_t *method, const wchar_t *url, unsigned long long own) {
    int get = method && tier_lower(method[0]) == L'g' && tier_lower(method[1]) == L'e' &&
              tier_lower(method[2]) == L't' && !method[3];
    if (!url || !own) {
        return FI_ROUTE_PASS;
    }
    unsigned long long named = tier_url_xuid(url);
    if (!named || named == own) {
        return FI_ROUTE_PASS;
    }
    /* another player, with or without a save line (OD-239 9): reads go to the server, writes are not sent */
    return get ? FI_ROUTE_PASS : FI_ROUTE_UNAVAILABLE;
}