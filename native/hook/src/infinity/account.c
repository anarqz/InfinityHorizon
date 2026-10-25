/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* Identity bridge: launcher ticket identity, or the legacy Beta account stored on this PC.
   No profile, social or world UI lives in the hook (OD-73). Legacy token validation and
   save preparation remain before the game's first identity read. */
#include "infinity.h"
#include "offsets.h"
#include "../core/logging.h"
#include "../core/sha256.h"
#include "../core/support.h"

#define ACCOUNT_FILE L"FH2Online_Accounts.ini"
#define ACCOUNT_WAIT_MS 30000u
#define ACCOUNT_LAUNCH_WAIT_MS 5000u
#define ACCOUNT_SAVED_MAX 3

typedef HRESULT (*AccountGetterFn)(void *, void **);
typedef HRESULT (*AccountHstringCreateFn)(const wchar_t *, unsigned, void **);
typedef BOOL(WINAPI *WritePrivateProfileStringWFn)(LPCWSTR, LPCWSTR, LPCWSTR, LPCWSTR);
typedef BOOL(WINAPI *GetVolumeInformationWFn)(LPCWSTR, LPWSTR, DWORD, LPDWORD, LPDWORD, LPDWORD,
                                               LPWSTR, DWORD);
typedef LONG(WINAPI *RegGetValueWFn)(HKEY, LPCWSTR, LPCWSTR, DWORD, LPDWORD, PVOID, LPDWORD);
typedef void(WINAPI *SleepFn)(DWORD);

/* The getters' slots and the player-name string for this Azimuth build (azimuth_offsets.c). */
static const AzimuthOffsets *account_offsets;
static AccountGetterFn account_info_original[3];
static AccountGetterFn account_xuid_original;

static unsigned char *account_host;
static wchar_t account_directory[PATH_CAPACITY], account_file[PATH_CAPACITY];
static const wchar_t *account_server;
static volatile LONG account_ready = 1; /* token check finished (or not needed) */
static volatile LONG account_link;      /* server: 0 not checked yet, 1 answering, 2 not answering */
static int account_available;           /* legacy identity configuration is ready */
static volatile LONG account_storm_island; /* the online profile has Storm Island */
#define STORM_ISLAND_PRODUCT "06f06bff-35ee-4821-a289-fbc1765d6b76"
static int account_active;              /* this session plays as the account */
static wchar_t account_token[72];
static wchar_t account_xuid[24];
static wchar_t account_gamertag[32]; /* Name#12345: up to 20 + 6 */
static wchar_t account_web_key[24];
static wchar_t account_online[4];
static char account_discriminator[6];
static char account_device[65];
static SleepFn account_sleep;
static volatile LONG account_noob; /* the active profile is Noob (server/src/db/accounts.rs MODE_NOOB) */

/* ------------------------------------------------------------------ text */

static size_t wide_len(const wchar_t *text) {
    size_t n = 0;
    while (text && text[n]) {
        ++n;
    }
    return n;
}

static void hex_text(const unsigned char *data, size_t size, char *out) {
    static const char digits[] = "0123456789abcdef";
    for (size_t i = 0; i < size; ++i) {
        out[2 * i] = digits[data[i] >> 4];
        out[2 * i + 1] = digits[data[i] & 15];
    }
    out[2 * size] = 0;
}

/* Appends ASCII `text` to `out` (capacity `cap`, kept NUL-terminated). */
static void text_add(char *out, size_t cap, const char *text) {
    size_t n = text_length(out);
    while (*text && n + 1 < cap) {
        out[n++] = *text++;
    }
    out[n] = 0;
}

static void wide_to_ascii(const wchar_t *in, char *out, size_t cap) {
    size_t n = 0;
    while (in && in[n] && n + 1 < cap) {
        out[n] = in[n] < 128 ? (char)in[n] : '?';
        ++n;
    }
    out[n] = 0;
}

/* UTF-8 (server messages) to UTF-16. */
static void utf8_to_wide(const char *in, wchar_t *out, size_t cap) {
    out[0] = 0;
    if (in && *in) {
        int n = MultiByteToWideChar(CP_UTF8, 0, in, -1, out, (int)cap);
        out[n > 0 ? n - 1 : 0] = 0;
    }
}

/* The string value of "key" in a flat JSON object (no nesting), escapes \" and \\ only. */
static int json_string(const char *json, const char *key, char *out, size_t cap) {
    size_t key_length = text_length(key);
    out[0] = 0;
    for (const char *p = json; p && *p; ++p) {
        if (*p != '"' || !memory_equal(p + 1, key, key_length) || p[1 + key_length] != '"') {
            continue;
        }
        p += key_length + 2;
        while (*p == ' ' || *p == ':') {
            ++p;
        }
        if (*p != '"') {
            return 0;
        }
        size_t n = 0;
        for (++p; *p && *p != '"'; ++p) {
            char c = *p;
            if (c == '\\' && p[1]) {
                c = *++p == 'n' ? '\n' : *p;
            }
            if (n + 1 < cap) {
                out[n++] = c;
            }
        }
        out[n] = 0;
        return 1;
    }
    return 0;
}

static void account_log(const char *prefix, const char *detail) {
    static char line[640];
    line[0] = 0;
    text_add(line, sizeof(line), "ACCOUNT: ");
    text_add(line, sizeof(line), prefix);
    text_add(line, sizeof(line), detail);
    text_add(line, sizeof(line), "\r\n");
    log_append(line);
}

/* ----------------------------------------------------------- fingerprint */

static int account_fingerprint(void) {
    static wchar_t guid[64];
    static char material[256];
    unsigned char full[32], device[32];
    DWORD size = sizeof(guid), serial = 0;
    HMODULE advapi = LoadLibraryExW(L"advapi32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    RegGetValueWFn reg_get = advapi ? (RegGetValueWFn)(uintptr_t)GetProcAddress(advapi, "RegGetValueW") : 0;
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    GetVolumeInformationWFn volume =
        kernel ? (GetVolumeInformationWFn)(uintptr_t)GetProcAddress(kernel, "GetVolumeInformationW") : 0;
    guid[0] = 0;
    /* RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY */
    if (!reg_get || reg_get(HKEY_LOCAL_MACHINE, L"SOFTWARE\\Microsoft\\Cryptography", L"MachineGuid",
                            0x00000002 | 0x00010000, 0, guid, &size) != ERROR_SUCCESS) {
        log_append("ACCOUNT: MachineGuid unavailable; the fingerprint uses the volume only.\r\n");
        guid[0] = 0;
    }
    wchar_t root[4] = {account_directory[0], L':', L'\\', 0};
    if (!volume || !volume(root, 0, 0, &serial, 0, 0, 0, 0)) {
        serial = 0;
    }
    if (!guid[0] && !serial) {
        return 0;
    }
    char guid_ascii[64], serial_hex[9];
    wide_to_ascii(guid, guid_ascii, sizeof(guid_ascii));
    unsigned char serial_bytes[4] = {(unsigned char)(serial >> 24), (unsigned char)(serial >> 16),
                                     (unsigned char)(serial >> 8), (unsigned char)serial};
    hex_text(serial_bytes, 4, serial_hex);
    material[0] = 0;
    text_add(material, sizeof(material), "FH2Online/v1|");
    text_add(material, sizeof(material), guid_ascii);
    text_add(material, sizeof(material), "|");
    text_add(material, sizeof(material), serial_hex);
    Sha256Context hash;
    sha256_initialize(&hash);
    sha256_update(&hash, (const unsigned char *)material, text_length(material));
    sha256_finalize(&hash, full);

    unsigned long long number = 0;
    for (int i = 0; i < 8; ++i) {
        number = number << 8 | full[i];
    }
    number %= 100000u;
    for (int i = 4; i >= 0; --i) {
        account_discriminator[i] = (char)('0' + number % 10);
        number /= 10;
    }
    account_discriminator[5] = 0;

    /* per-server device id: SHA-256(hex(fingerprint) | host:port) */
    char full_hex[65], server[300];
    hex_text(full, 32, full_hex);
    wide_to_ascii(account_server, server, sizeof(server));
    material[0] = 0;
    text_add(material, sizeof(material), full_hex);
    text_add(material, sizeof(material), "|");
    text_add(material, sizeof(material), server);
    sha256_initialize(&hash);
    sha256_update(&hash, (const unsigned char *)material, text_length(material));
    sha256_finalize(&hash, device);
    hex_text(device, 32, account_device);
    return 1;
}

/* ---------------------------------------------------------- local storage */

static int account_write(const wchar_t *key, const wchar_t *value) {
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    WritePrivateProfileStringWFn write =
        kernel ? (WritePrivateProfileStringWFn)(uintptr_t)GetProcAddress(kernel, "WritePrivateProfileStringW") : 0;
    return write && write(account_server, key, value, account_file);
}

/* ------------------------------------------------- profiles on this PC */
/* A PC keeps up to ACCOUNT_SAVED_MAX profiles per server (the server lets a PC create as
   many). The one in use is the [host:port] section, as always (older hooks and the cloud save
   read it); the others wait in [host:port|<xuid>] sections with the same keys, so switching
   moves whole sections and each profile keeps its own SaveCommit. A profile signed in on
   another PC loses its token here but stays listed (with its name and last site key), and the
   PC that created it can play it again without the key (POST device/recover).
   The same server reached at another address (127.0.0.1, its LAN address, a name) keeps the
   profiles: a PC with none for the address asks the server which of its other sections'
   tokens it knows, by their SHA-1 (POST accounts/known; the tokens themselves never go to
   another server), and those are copied in. */
#define ACCOUNT_PROFILE_KEYS 7
#define ACCOUNT_SECTIONS_CAP 8192u
static const wchar_t *const account_record_keys[ACCOUNT_PROFILE_KEYS] = {
    L"Token", L"Xuid", L"Gamertag", L"WebKey", L"SaveCommit", L"StormIsland", L"Tier"};
typedef struct {
    wchar_t values[ACCOUNT_PROFILE_KEYS][72];
} AccountRecord;
static wchar_t account_sections[ACCOUNT_SECTIONS_CAP];

static int account_write_in(const wchar_t *section, const wchar_t *key, const wchar_t *value) {
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    WritePrivateProfileStringWFn write =
        kernel ? (WritePrivateProfileStringWFn)(uintptr_t)GetProcAddress(kernel, "WritePrivateProfileStringW") : 0;
    return write && write(section, key, value, account_file);
}

static void record_read(const wchar_t *section, AccountRecord *record) {
    for (unsigned i = 0; i < ACCOUNT_PROFILE_KEYS; ++i) {
        GetPrivateProfileStringW(section, account_record_keys[i], L"", record->values[i], 72, account_file);
    }
}

static void record_write(const wchar_t *section, const AccountRecord *record) {
    for (unsigned i = 0; i < ACCOUNT_PROFILE_KEYS; ++i) {
        account_write_in(section, account_record_keys[i], record->values[i]);
    }
}

/* [host:port|<xuid>] */
static int record_section(const wchar_t *xuid, wchar_t *out, size_t cap) {
    size_t s = wide_len(account_server), x = wide_len(xuid);
    if (!x || s + x + 2 > cap) {
        return 0;
    }
    memory_copy(out, account_server, s * sizeof(wchar_t));
    out[s] = L'|';
    memory_copy(out + s + 1, xuid, (x + 1) * sizeof(wchar_t));
    return 1;
}

static int wide_equal(const wchar_t *a, const wchar_t *b) {
    size_t n = wide_len(a);
    return n == wide_len(b) && memory_equal(a, b, n * sizeof(wchar_t));
}

/* Every section name of FH2Online_Accounts.ini, one after another (NUL each, a final NUL). */
static const wchar_t *account_section_names(void) {
    account_sections[0] = account_sections[1] = 0;
    GetPrivateProfileStringW(0, 0, L"", account_sections, ACCOUNT_SECTIONS_CAP, account_file);
    return account_sections;
}

/* `name` is one of this server's saved profiles ([host:port|xuid]). */
static int record_is_mine(const wchar_t *name) {
    size_t s = wide_len(account_server);
    return wide_len(name) > s + 1 && memory_equal(name, account_server, s * sizeof(wchar_t)) && name[s] == L'|';
}

/* The profile in use goes to its waiting section (before another takes its place). At most
   ACCOUNT_SAVED_MAX - 1 wait: one more, and the first one found is forgotten here (it
   stays on the server). */
static void record_stash(const AccountRecord *active) {
    wchar_t section[320];
    if (!active->values[1][0] || !record_section(active->values[1], section, ARRAY_COUNT(section))) {
        return;
    }
    record_write(section, active);
    int waiting = 0;
    const wchar_t *first = 0;
    for (const wchar_t *name = account_section_names(); *name; name += wide_len(name) + 1) {
        if (record_is_mine(name) && !wide_equal(name, section)) {
            ++waiting;
            first = first ? first : name;
        }
    }
    if (waiting >= ACCOUNT_SAVED_MAX - 1 && first) {
        account_write_in(first, 0, 0);
    }
}

/* The globals again from the [host:port] section (after a section moved in or back). */
static void account_load_active(void) {
    GetPrivateProfileStringW(account_server, L"Token", L"", account_token, ARRAY_COUNT(account_token), account_file);
    GetPrivateProfileStringW(account_server, L"Xuid", L"", account_xuid, ARRAY_COUNT(account_xuid), account_file);
    GetPrivateProfileStringW(account_server, L"Gamertag", L"", account_gamertag, ARRAY_COUNT(account_gamertag),
                             account_file);
    GetPrivateProfileStringW(account_server, L"WebKey", L"", account_web_key, ARRAY_COUNT(account_web_key),
                             account_file);
    GetPrivateProfileStringW(account_server, L"Online", L"1", account_online, ARRAY_COUNT(account_online),
                             account_file);
}

/* SHA-1 (the server keeps device tokens by their SHA-1 hex). */
static unsigned account_rol(unsigned v, int n) {
    return v << n | v >> (32 - n);
}

static void account_sha1_hex(const char *text, char out[41]) {
    unsigned h[5] = {0x67452301u, 0xefcdab89u, 0x98badcfeu, 0x10325476u, 0xc3d2e1f0u};
    size_t length = text_length(text);
    unsigned char block[64];
    unsigned long long bits = (unsigned long long)length * 8;
    size_t total = (length + 9 + 63) / 64 * 64;
    for (size_t offset = 0; offset < total; offset += 64) {
        for (size_t i = 0; i < 64; ++i) {
            size_t at = offset + i;
            block[i] = at < length ? (unsigned char)text[at]
                       : at == length ? 0x80
                       : at >= total - 8 ? (unsigned char)(bits >> (8 * (total - 1 - at)))
                                          : 0;
        }
        unsigned w[80];
        for (int i = 0; i < 16; ++i) {
            w[i] = (unsigned)block[4 * i] << 24 | (unsigned)block[4 * i + 1] << 16 | (unsigned)block[4 * i + 2] << 8 |
                   block[4 * i + 3];
        }
        for (int i = 16; i < 80; ++i) {
            w[i] = account_rol(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
        }
        unsigned a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
        for (int i = 0; i < 80; ++i) {
            unsigned f, k;
            if (i < 20) {
                f = (b & c) | (~b & d), k = 0x5a827999u;
            } else if (i < 40) {
                f = b ^ c ^ d, k = 0x6ed9eba1u;
            } else if (i < 60) {
                f = (b & c) | (b & d) | (c & d), k = 0x8f1bbcdcu;
            } else {
                f = b ^ c ^ d, k = 0xca62c1d6u;
            }
            unsigned t = account_rol(a, 5) + f + e + k + w[i];
            e = d, d = c, c = account_rol(b, 30), b = a, a = t;
        }
        h[0] += a, h[1] += b, h[2] += c, h[3] += d, h[4] += e;
    }
    unsigned char digest[20];
    for (int i = 0; i < 20; ++i) {
        digest[i] = (unsigned char)(h[i / 4] >> (24 - 8 * (i % 4)));
    }
    hex_text(digest, 20, out);
}

/* 64 hex digits, as the server makes tokens. */
static int account_token_shape(const wchar_t *token) {
    if (wide_len(token) != 64) {
        return 0;
    }
    for (int i = 0; i < 64; ++i) {
        wchar_t c = token[i];
        if (!((c >= L'0' && c <= L'9') || (c >= L'a' && c <= L'f') || (c >= L'A' && c <= L'F'))) {
            return 0;
        }
    }
    return 1;
}

/* Sections of other addresses with a token: the candidates account_adopt asks about. */
static int account_other_tokens(void) {
    int found = 0;
    for (const wchar_t *name = account_section_names(); *name; name += wide_len(name) + 1) {
        wchar_t token[72];
        if (wide_equal(name, account_server) || record_is_mine(name)) {
            continue;
        }
        GetPrivateProfileStringW(name, L"Token", L"", token, ARRAY_COUNT(token), account_file);
        found += account_token_shape(token);
    }
    return found;
}

/* The profiles of this server saved under another address are copied in (the first, when
   this address had none, becomes the one in use, with its Online). 1 when one is in use now. */
static int account_adopt(void) {
    static char body[16 * 46 + 32], reply[4096];
    static wchar_t names[16][320];
    static char hashes[16][41];
    int count = 0;
    body[0] = 0;
    text_add(body, sizeof(body), "{\"tokens\":[");
    for (const wchar_t *name = account_section_names(); *name && count < 16; name += wide_len(name) + 1) {
        wchar_t token[72];
        char ascii[72];
        if (wide_equal(name, account_server) || record_is_mine(name) || wide_len(name) >= 320) {
            continue;
        }
        GetPrivateProfileStringW(name, L"Token", L"", token, ARRAY_COUNT(token), account_file);
        if (!account_token_shape(token)) {
            continue;
        }
        wide_to_ascii(token, ascii, sizeof(ascii));
        account_sha1_hex(ascii, hashes[count]);
        memory_copy(names[count], name, (wide_len(name) + 1) * sizeof(wchar_t));
        text_add(body, sizeof(body), count ? ",\"" : "\"");
        text_add(body, sizeof(body), hashes[count]);
        text_add(body, sizeof(body), "\"");
        ++count;
    }
    text_add(body, sizeof(body), "]}");
    if (!count || azimuth_net_call_quick(L"POST", L"/infinity/v1/accounts/known", 0, body, reply, sizeof(reply), 4000) !=
                      200) {
        return account_token[0] != 0;
    }
    size_t length = text_length(reply);
    for (int i = 0; i < count; ++i) {
        if (!text_find(reply, length, hashes[i])) {
            continue;
        }
        AccountRecord record;
        wchar_t online[4], section[320], have[24];
        char tag[32];
        record_read(names[i], &record);
        GetPrivateProfileStringW(names[i], L"Online", L"1", online, ARRAY_COUNT(online), account_file);
        /* already here (in use, or waiting) */
        GetPrivateProfileStringW(account_server, L"Xuid", L"", have, ARRAY_COUNT(have), account_file);
        if (wide_equal(have, record.values[1]) ||
            (record_section(record.values[1], section, ARRAY_COUNT(section)) &&
             GetPrivateProfileStringW(section, L"Xuid", L"", have, ARRAY_COUNT(have), account_file))) {
            continue;
        }
        if (!account_token[0]) {
            record_write(account_server, &record);
            account_write(L"Online", online);
            account_load_active();
        } else {
            record_stash(&record);
        }
        wide_to_ascii(record.values[2], tag, sizeof(tag));
        account_log("this server's profile, saved under another address, is kept here too: ", tag);
    }
    return account_token[0] != 0;
}

/* -------------------------------------------------------- identity patch */

/* Rewrites Azimuth's player-name std::wstring (ptr/inline buffer, size +0x10, capacity
   +0x18) in place; the gamertag fits whenever "Player 1"-style names already did. */
/* The name the game shows: its gamertag fields hold 15 characters, so the name is cut at 15
   (Store::game_tag). OD-120: names are plain; a legacy Name#12345 (a ticket from a server before
   OD-120) shows as the name alone, never its number. */
static void account_plain_tag(wchar_t *out, size_t cap) {
    size_t k = 0;
    while (k + 1 < cap && account_gamertag[k] && account_gamertag[k] != L'#') {
        out[k] = account_gamertag[k];
        ++k;
    }
    out[k] = 0;
}

static void account_game_tag(wchar_t out[16]) {
    account_plain_tag(out, 16);
}

static int account_set_player_name(void) {
    if (!account_host || !account_offsets) {
        return 0;
    }
    unsigned char *string = account_host + account_offsets->account_name;
    unsigned long long capacity = 0;
    wchar_t tag[16];
    account_game_tag(tag);
    size_t length = wide_len(tag);
    if (!memory_read(string + 0x18, &capacity, 8) || capacity < length || capacity > 4096) {
        return 0;
    }
    wchar_t *text = capacity > 7 ? *(wchar_t **)string : (wchar_t *)string;
    if (!text || !memory_is_writable(text, (length + 1) * sizeof(wchar_t))) {
        return 0;
    }
    memory_copy(text, tag, (length + 1) * sizeof(wchar_t));
    *(unsigned long long *)(string + 0x10) = length;
    return 1;
}

static void account_wait(void) {
    /* a launcher start waits for the pipe handshake only (launch.c, at most 5 s): then the session
       is offline for good (an identity read now cannot change later), unless it went online in
       that same moment */
    unsigned limit = fi_launch_protocol() ? ACCOUNT_LAUNCH_WAIT_MS : ACCOUNT_WAIT_MS;
    for (unsigned waited = 0; !account_ready; waited += 20) {
        if (waited >= limit) {
            if (limit == ACCOUNT_WAIT_MS || fi_launch_late() || waited >= limit + ACCOUNT_LAUNCH_WAIT_MS) {
                InterlockedExchange(&account_ready, 1);
                break;
            }
        }
        account_sleep(20);
    }
}

/* launch.c: the handshake is over (online or not); the getters stop waiting. */
void azimuth_account_launch_done(void) {
    InterlockedExchange(&account_ready, 1);
}

/* launch.c, once the ticket checked out and before account_ready: the identity it names. Nothing
   is written to FH2Online_Accounts.ini (the launcher owns profiles). */
void azimuth_account_launch_identity(const char *xuid, const char *gamertag, int free_tier, int noob, int storm_island) {
    char tag[32];
    utf8_to_wide(xuid, account_xuid, ARRAY_COUNT(account_xuid));
    utf8_to_wide(gamertag, account_gamertag, ARRAY_COUNT(account_gamertag));
    fi_tier_set(free_tier ? FI_TIER_FREE : FI_TIER_VIP);
    InterlockedExchange(&account_noob, noob);
    InterlockedExchange(&account_storm_island, storm_island);
    if (storm_island) {
        azimuth_expansions_granted();
    }
    account_active = 1;
    wide_to_ascii(account_gamertag, tag, sizeof(tag));
    account_log(account_set_player_name() ? "playing as " : "playing as (name not applied) ", tag);
}

static HRESULT account_get_xuid(void *self, void **out) {
    fi_game_callback_from("the game's first identity request");
    account_wait();
    if (!account_active || !out) {
        return account_xuid_original(self, out);
    }
    AccountHstringCreateFn create = (AccountHstringCreateFn)(uintptr_t)(account_host + account_offsets->hstring_create);
    return create(account_xuid, (unsigned)wide_len(account_xuid), out);
}

/* Gamertag / display names: answered from the account directly. Azimuth rewrites its
   player-name string (e.g. when a controller is paired again), so patching it once is not
   enough. */
static HRESULT account_get_name(int slot, void *self, void **out) {
    fi_game_callback_from("the game's first identity request");
    account_wait();
    if (!account_active || !out) {
        return account_info_original[slot](self, out);
    }
    AccountHstringCreateFn create = (AccountHstringCreateFn)(uintptr_t)(account_host + account_offsets->hstring_create);
    wchar_t name[ARRAY_COUNT(account_gamertag)];
    account_plain_tag(name, ARRAY_COUNT(name));
    return create(name, (unsigned)wide_len(name), out);
}
static HRESULT account_get_info_0(void *self, void **out) {
    return account_get_name(0, self, out);
}
static HRESULT account_get_info_1(void *self, void **out) {
    return account_get_name(1, self, out);
}
static HRESULT account_get_info_2(void *self, void **out) {
    return account_get_name(2, self, out);
}
static const AccountGetterFn account_info_hooks[3] = {account_get_info_0, account_get_info_1,
                                                       account_get_info_2};

static int account_patch_slot(unsigned slot_rva, unsigned target_rva, AccountGetterFn hook, AccountGetterFn *original) {
    void **slot = (void **)(account_host + slot_rva);
    void *expected = account_host + target_rva, *current = 0;
    DWORD old_protection, ignored;
    if (!memory_read(slot, &current, sizeof(current)) || current != expected ||
        !VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old_protection)) {
        return 0;
    }
    *original = (AccountGetterFn)(uintptr_t)expected;
    *slot = (void *)(uintptr_t)hook;
    VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
    return 1;
}

/* ------------------------------------------------------------ server calls */

/* Takes xuid/gamertag (and token, web key when present) from a server reply. */
/* The active profile's mode (account_noob), from the server (/me, sign-in) or the launcher's
   ticket: Noob profiles have no progress gates. */

int azimuth_account_noob(void) {
    return account_noob != 0;
}

/* The last reply account_take read had "tier". */
static int account_tier_said;

static int account_take(const char *reply) {
    char value[96];
    if (!json_string(reply, "xuid", value, sizeof(value)) || !value[0]) {
        return 0;
    }
    utf8_to_wide(value, account_xuid, ARRAY_COUNT(account_xuid));
    if (!json_string(reply, "gamertag", value, sizeof(value)) || !value[0]) {
        return 0;
    }
    utf8_to_wide(value, account_gamertag, ARRAY_COUNT(account_gamertag));
    if (json_string(reply, "token", value, sizeof(value)) && value[0]) {
        utf8_to_wide(value, account_token, ARRAY_COUNT(account_token));
    }
    if (json_string(reply, "webKey", value, sizeof(value)) && value[0]) {
        utf8_to_wide(value, account_web_key, ARRAY_COUNT(account_web_key));
    }
    if (json_string(reply, "mode", value, sizeof(value))) {
        InterlockedExchange(&account_noob, text_equal_ignore_case(value, "noob"));
    }
    /* the tier is the server's word (tier.c): "free" or "vip", in every reply taken here
       (/register, /login, device/recover, /me). A reply without one keeps the tier known so far
       (the Beta server never sends it: vip, the default); it never turns a free profile into a
       Supporter. Kept as Tier= for a start when the server does not answer. */
    if (json_string(reply, "tier", value, sizeof(value))) {
        int free_tier = text_equal_ignore_case(value, "free");
        fi_tier_set(free_tier ? FI_TIER_FREE : FI_TIER_VIP);
        account_write(L"Tier", free_tier ? L"free" : L"vip");
        account_tier_said = 1;
    } else {
        account_tier_said = 0;
    }
    size_t length = text_length(reply);
    if (text_find(reply, length, "\"expansions\"")) {
        int storm = text_find(reply, length, STORM_ISLAND_PRODUCT) != 0;
        InterlockedExchange(&account_storm_island, storm);
        account_write(L"StormIsland", storm ? L"1" : L"0");
        if (storm) {
            azimuth_expansions_granted();
        }
    }
    return 1;
}

/* X-Fi-Session and X-Fi-Hook in a launcher session (ACCOUNTS.md section 11), else the Beta's
   X-Fh2-Token (only ever sent to the server it came from). */
size_t azimuth_account_auth_header(wchar_t *out, size_t cap) {
    if (fi_launch_protocol()) {
        return fi_launch_headers(out, cap);
    }
    static const wchar_t prefix[] = L"X-Fh2-Token: ";
    size_t n = ARRAY_COUNT(prefix) - 1, t = wide_len(account_token);
    if (!account_token[0] || n + t + 1 > cap) {
        if (cap) {
            out[0] = 0;
        }
        return 0;
    }
    memory_copy(out, prefix, n * sizeof(wchar_t));
    memory_copy(out + n, account_token, (t + 1) * sizeof(wchar_t));
    return n + t;
}

static const wchar_t *account_token_header(void) {
    static wchar_t header[1200];
    azimuth_account_auth_header(header, ARRAY_COUNT(header));
    return header;
}

static DWORD WINAPI account_thread(void *unused) {
    static char reply[2048], text[400], gamertag[32];
    (void)unused;
    /* no profile under this address: one of this server's saved under another comes in */
    if (!account_token[0]) {
        if (!account_adopt() || account_online[0] != L'1') {
            log_append(account_token[0] ? "ACCOUNT: account kept but Online=0; playing the offline profile.\r\n"
                                        : "ACCOUNT: no legacy account on this server; use the Infinity Launcher.\r\n");
            InterlockedExchange(&account_ready, 1);
            return 0;
        }
        wchar_t storm[4];
        GetPrivateProfileStringW(account_server, L"StormIsland", L"0", storm, ARRAY_COUNT(storm), account_file);
        InterlockedExchange(&account_storm_island, storm[0] == L'1');
    }
    DWORD status = azimuth_net_call(L"GET", L"/infinity/v1/me", account_token_header(), 0, reply, sizeof(reply));
    if (status == 200 && account_take(reply)) {
        account_active = 1;
        if (!account_tier_said) {
            /* the Beta server: every profile is a Supporter's. The new server always says it (PLAN
               9.1): a /me without it there is a server fault, and nothing here opens anything a
               free profile had not (the tier stays what it was) */
            log_append(fi_tier_free() ? "ACCOUNT: the server's /me has no tier; the profile stays free (as last said).\r\n"
                                      : "ACCOUNT: the server's /me has no tier (the Beta server): Supporter, as before.\r\n");
        }
        /* the server's word for the player, in their language (a moderation reset: db/reset.rs) */
        if (json_string(reply, "notice", text, sizeof(text)) && text[0]) {
            log_append("ACCOUNT: legacy server notice received; no overlay in a Beta session.\r\n");
        }
        InterlockedExchange(&account_link, 1);
        if (fi_tier_free()) {
            /* a free profile has no cloud save: its saves stay on this PC, nothing is restored */
            log_append("CLOUD: free profile: saves stay on this PC (no cloud save, no restore).\r\n");
        } else {
            azimuth_cloud_start(account_directory, account_xuid, account_token, account_file, account_server);
        }
    } else if (status == 401) {
        /* the profile logged in on another PC (or the server forgot it): this token is dead */
        account_token[0] = 0;
        account_write(L"Token", L"");
        account_log("the server no longer knows this PC's token (logged in elsewhere); forgotten.", "");
    } else if (status == 403) {
        json_string(reply, "error", text, sizeof(text));
        account_log("the server refused this PC's account; playing offline: ", text);
    } else {
        /* server unreachable: keep the stored identity, the token is sent anyway */
        account_active = account_xuid[0] && account_gamertag[0];
        account_log("server unreachable; using the stored account.", "");
        if (account_active && !fi_tier_free()) {
            azimuth_cloud_put_aside(account_directory, account_xuid);
        }
    }
    if (account_active) {
        wide_to_ascii(account_gamertag, gamertag, sizeof(gamertag));
        account_log(account_set_player_name() ? "playing as " : "playing as (name not applied) ", gamertag);
    }
    InterlockedExchange(&account_ready, 1);
    /* the game no longer waits for this thread: what needs the player's ExtraOptions runs from
       here if no game callback did yet */
    fi_game_callback_late();
    return 0;
}

int azimuth_account_storm_island(void) {
    return account_storm_island != 0;
}

const wchar_t *azimuth_account_gamertag(void) {
    return account_gamertag;
}

/* The server's /info names the newest hook (latestHook, in release latestRelease): a newer one
   than this is installed by azimuth_update.c. */
static void account_check_update(const char *reply) {
    /* Beta startup only; launcher sessions use the launcher updater. */
    char latest[48], release[24];
    if (json_string(reply, "latestHook", latest, sizeof(latest))) {
        json_string(reply, "latestRelease", release, sizeof(release));
        azimuth_update_offer(latest, release);
    }
}

/* One legacy Beta startup check; launcher sessions do not run this thread. */
static DWORD WINAPI account_link_thread(void *unused) {
    static char reply[1024];
    (void)unused;
    DWORD status = azimuth_net_call(L"GET", L"/infinity/v1/info", 0, 0, reply, sizeof(reply));
    InterlockedExchange(&account_link, status == 200 ? 1 : 2);
    if (status == 200) {
        account_check_update(reply);
    }
    return 0;
}

/* A notice for the player (English UTF-8, translated): an overlay toast through the launcher
   (OD-73). Legacy Beta sessions have no launcher connection and therefore no toast. */
void azimuth_account_set_notice(const char *english) {
    fi_launch_notice(FI_NOTICE_WARNING, azimuth_tr8(english));
}

void azimuth_account_force_notice(const char *english) {
    fi_launch_notice(FI_NOTICE_ERROR, azimuth_tr8(english));
}

unsigned long long azimuth_account_xuid_value(void) {
    unsigned long long xuid = 0;
    if (!account_ready || !account_active) {
        return 0;
    }
    for (const wchar_t *p = account_xuid; *p >= L'0' && *p <= L'9'; ++p) {
        xuid = xuid * 10 + (unsigned)(*p - L'0');
    }
    return xuid;
}

/* The Beta token, or in a launcher session a marker while it is live (never sent: every request
   takes its header from azimuth_account_auth_header). */
const wchar_t *azimuth_account_token(void) {
    if (fi_launch_protocol()) {
        return account_ready && account_active && fi_launch_live() ? L"(launcher session)" : 0;
    }
    return account_ready && account_active && account_token[0] ? account_token : 0;
}

const wchar_t *azimuth_account_token_wait(void) {
    account_wait();
    return azimuth_account_token();
}

/* A launcher start (FI_LAUNCH=v1;pipe=): the identity comes from the launcher's ticket
   (launch.c), so there is no account file, fingerprint, link, cloud or update thread here, and
   profiles are owned by the launcher (ADR 0102). */
static void account_apply_launch(const wchar_t *directory) {
    account_host = (unsigned char *)GetModuleHandleW(NULL);
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    account_sleep = kernel ? (SleepFn)(uintptr_t)GetProcAddress(kernel, "Sleep") : 0;
    account_offsets = azimuth_offsets(account_host);
    if (!account_sleep || !account_offsets) {
        log_append("ACCOUNT: unsupported host; the launcher's session cannot set the identity: offline.\r\n");
        return;
    }
    int patched = account_patch_slot(account_offsets->account_xuid_slot, account_offsets->account_xuid_target,
                                     account_get_xuid, &account_xuid_original);
    for (unsigned i = 0; i < 3; ++i) {
        patched += account_patch_slot(account_offsets->account_info_slots[i], account_offsets->account_info_targets[i],
                                      account_info_hooks[i], &account_info_original[i]);
    }
    if (patched != 4) {
        log_append("ACCOUNT: LocalUser getters not where expected; the launcher's session cannot set the identity: "
                   "offline.\r\n");
        return;
    }

    account_ready = 0;
    if (!fi_launch_start(directory)) {
        account_ready = 1;
    }
}

/* OD-118 (P1): the launcher's Play offline (FI_LAUNCH=v1;offline) names the chosen local profile
   (offline_identity.c); the getters answer with it at once, so its save is its own User_<hex xuid>.
   No account file, server, cloud or update thread. Absent or invalid: Azimuth's own offline profile. */
static int account_apply_offline(void) {
    wchar_t xuid[24], tag[24];
    DWORD nx = GetEnvironmentVariableW(L"FI_OFFLINE_XUID", xuid, ARRAY_COUNT(xuid));
    DWORD nt = GetEnvironmentVariableW(L"FI_OFFLINE_GAMERTAG", tag, ARRAY_COUNT(tag));
    unsigned long long value = 0;
    if (!nx && !nt) {
        return 0;
    }
    if (!nx || nx >= ARRAY_COUNT(xuid) || !nt || nt >= ARRAY_COUNT(tag) || !fi_offline_identity(xuid, tag, &value)) {
        log_append("ACCOUNT: the launcher's offline profile is not valid; playing Azimuth's offline profile.\r\n");
        return 0;
    }
    account_host = (unsigned char *)GetModuleHandleW(NULL);
    account_offsets = azimuth_offsets(account_host);
    if (!account_offsets) {
        log_append("ACCOUNT: unsupported host; playing Azimuth's offline profile.\r\n");
        return 0;
    }
    int patched = account_patch_slot(account_offsets->account_xuid_slot, account_offsets->account_xuid_target,
                                     account_get_xuid, &account_xuid_original);
    for (unsigned i = 0; i < 3; ++i) {
        patched += account_patch_slot(account_offsets->account_info_slots[i], account_offsets->account_info_targets[i],
                                      account_info_hooks[i], &account_info_original[i]);
    }
    if (patched != 4) {
        log_append("ACCOUNT: LocalUser getters not where expected; playing Azimuth's offline profile.\r\n");
        return 0;
    }
    memory_copy(account_xuid, xuid, (wide_len(xuid) + 1) * sizeof(wchar_t));
    memory_copy(account_gamertag, tag, (wide_len(tag) + 1) * sizeof(wchar_t));
    account_active = 1;
    account_ready = 1; /* nothing to wait for: the identity is known before the game asks */
    char ascii[32];
    wide_to_ascii(account_gamertag, ascii, sizeof(ascii));
    account_log("playing offline as ", ascii);
    return 1;
}

static void account_apply(const wchar_t *directory) {
    if (fi_cfg()->launch == FI_LAUNCH_OFFLINE) {
        fi_launch_offline_overlay(); /* OD-118: the overlay of the launcher's Play offline, if it named one */
        if (account_apply_offline()) {
            return;
        }
    }
    if (fi_launch_protocol()) {
        account_apply_launch(directory);
        return;
    }
    if (!azimuth_net_proxy_enabled() || !(account_server = azimuth_net_server_name())) {
        return;
    }
    account_host = (unsigned char *)GetModuleHandleW(NULL);
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    account_sleep = kernel ? (SleepFn)(uintptr_t)GetProcAddress(kernel, "Sleep") : 0;
    size_t n = wide_len(directory);
    account_offsets = azimuth_offsets(account_host);
    if (!account_sleep || !account_offsets || n >= PATH_CAPACITY ||
        !path_join(account_file, directory, ACCOUNT_FILE)) {
        log_append("ACCOUNT: unsupported host or paths; FH2Online accounts unavailable.\r\n");
        return;
    }
    memory_copy(account_directory, directory, (n + 1) * sizeof(wchar_t));
    if (!account_fingerprint()) {
        log_append("ACCOUNT: no fingerprint for this PC; FH2Online accounts unavailable.\r\n");
        return;
    }
    account_available = 1;
    GetPrivateProfileStringW(account_server, L"Token", L"", account_token, ARRAY_COUNT(account_token), account_file);
    GetPrivateProfileStringW(account_server, L"Xuid", L"", account_xuid, ARRAY_COUNT(account_xuid), account_file);
    GetPrivateProfileStringW(account_server, L"Gamertag", L"", account_gamertag, ARRAY_COUNT(account_gamertag),
                             account_file);
    GetPrivateProfileStringW(account_server, L"WebKey", L"", account_web_key, ARRAY_COUNT(account_web_key),
                             account_file);
    GetPrivateProfileStringW(account_server, L"Online", L"1", account_online, ARRAY_COUNT(account_online),
                             account_file);
    account_log("fingerprint #", account_discriminator);
    /* none under this address, but tokens under others: the account thread asks the server
       whether one of them is its (account_adopt) before the game reads the identity */
    int adopt = !account_token[0] && account_other_tokens();
    if (!account_token[0] && !adopt) {
        log_append("ACCOUNT: no legacy account on this server; use the Infinity Launcher.\r\n");
        return;
    }
    if (!adopt && account_online[0] != L'1') {
        log_append("ACCOUNT: account kept but Online=0; playing the offline profile.\r\n");
        return;
    }
    /* The cached tier until /me answers; only the server can replace it. */
    wchar_t tier[8];
    GetPrivateProfileStringW(account_server, L"Tier", L"vip", tier, ARRAY_COUNT(tier), account_file);
    fi_tier_set(tier[0] == L'f' ? FI_TIER_FREE : FI_TIER_VIP);
    /* the last reply's expansions, for the game's first licence pass (before /me answers) */
    wchar_t storm[4];
    GetPrivateProfileStringW(account_server, L"StormIsland", L"0", storm, ARRAY_COUNT(storm), account_file);
    InterlockedExchange(&account_storm_island, storm[0] == L'1');
    int patched = account_patch_slot(account_offsets->account_xuid_slot, account_offsets->account_xuid_target,
                                     account_get_xuid, &account_xuid_original);
    for (unsigned i = 0; i < 3; ++i) {
        patched += account_patch_slot(account_offsets->account_info_slots[i], account_offsets->account_info_targets[i],
                                      account_info_hooks[i], &account_info_original[i]);
    }
    if (patched != 4) {
        log_append("ACCOUNT: LocalUser getters not where expected; playing the offline profile.\r\n");
        return;
    }
    account_ready = 0;
    HANDLE thread = CreateThread(0, 0, account_thread, 0, 0, 0);
    if (!thread) {
        account_ready = 1;
        return;
    }
    CloseHandle(thread);
}

void azimuth_apply_account(const wchar_t *directory) {
    account_apply(directory);
    /* after the stored profile is read (and its check started): the link thread looks at it */
    if (account_available) {
        HANDLE link = CreateThread(0, 0, account_link_thread, 0, 0, 0);
        if (link) {
            CloseHandle(link);
        }
    }
}
