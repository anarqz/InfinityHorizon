/* OD-118 (P1): the identity of the launcher's Play offline (FI_LAUNCH=v1;offline). The launcher
   passes the chosen local profile as FI_OFFLINE_XUID (decimal) and FI_OFFLINE_GAMERTAG; account.c
   answers the game's LocalUser getters with them, so the game saves in User_<hex xuid> and each
   local profile keeps its own save. Accepted XUIDs: the local offline range (CR-L36,
   0x9000000000001-0x90000000000FF, never issued by the server), or a server-issued XUID (at or
   above BETA_FLOOR) that a Noob profile continues offline. Nothing here goes online. */
#include "infinity.h"

#define FI_OFFLINE_FIRST 0x9000000000001ULL
#define FI_OFFLINE_LAST 0x90000000000FFULL
#define FI_ACCOUNT_FLOOR 2535000000000000ULL

int fi_offline_identity(const wchar_t *xuid, const wchar_t *gamertag, unsigned long long *value) {
    if (!xuid || !gamertag || !value) {
        return 0;
    }
    unsigned long long v = 0;
    size_t n = 0;
    for (; xuid[n]; ++n) {
        if (n >= 20 || xuid[n] < L'0' || xuid[n] > L'9') {
            return 0;
        }
        unsigned long long digit = (unsigned long long)(xuid[n] - L'0');
        if (v > (~0ULL - digit) / 10) {
            return 0;
        }
        v = v * 10 + digit;
    }
    if (n == 0 || xuid[0] == L'0' || (!(v >= FI_OFFLINE_FIRST && v <= FI_OFFLINE_LAST) && v < FI_ACCOUNT_FLOOR)) {
        return 0;
    }
    /* the launcher's profile name rule (ProfileName.cs): 1-20 ASCII letters or digits, a letter first */
    size_t t = 0;
    for (; gamertag[t]; ++t) {
        wchar_t c = gamertag[t];
        int letter = (c >= L'a' && c <= L'z') || (c >= L'A' && c <= L'Z');
        int digit = c >= L'0' && c <= L'9';
        if (t >= 20 || !(letter || (digit && t > 0))) {
            return 0;
        }
    }
    if (t == 0) {
        return 0;
    }
    *value = v;
    return 1;
}
