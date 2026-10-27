/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* Legacy Beta free-roam relay lifecycle, plus the current launcher's host-port bridge.
   Launcher sessions create and manage worlds in the C# launcher; the hook only connects the
   game. Legacy private free-roam requests can still spawn/announce a hidden relay job.
   No menu-driven host, browse or join API remains (OD-73). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/sha256.h"
#include "../core/support.h"

#define WORLD_CFG_NAME L"server.cfg"
#define WORLD_EXE_NAME L"dedicated_server.exe"
#define WORLD_LOG_NAME L"dedicated_server.log"
#define WORLD_NET_NAME L"dedicated_server.net"
#define WORLD_LAUNCHER_EXE L"infinity.exe"
#define WORLD_LAUNCHER_LOG L"infinity-relay.log"
#define WORLD_LAUNCHER_NET L"infinity-relay.net"
/* How long a just-started infinity.exe relay is watched for an early exit, and the code of a
   launcher build without the relay. */
#define WORLD_RELAY_PROBE_MS 1000u
#define WORLD_RELAY_ABSENT_EXIT 3u
#define WORLD_HEARTBEAT_MS 15000u
#define WORLD_PUNCH_POLL_MS 3000u
#define WORLD_CFG_MAX 16384u

static wchar_t world_directory[PATH_CAPACITY];
static HANDLE world_job, world_process;
/* The write end of the relay's stdin: "kick <xuid>" lines (the server's kick list) and
   "punch <ip:port>" lines (players about to join). */
static HANDLE world_relay_input;
/* world_generation: each world's heartbeat thread stops once it changes. */
static volatile LONG world_on, world_generation, world_listed;
/* The car database changed during the session (world_car_db_tampered). */
static volatile LONG world_car_db_changed;
/* This session's infinity.exe has no relay (it ended with WORLD_RELAY_ABSENT_EXIT). */
static volatile LONG world_launcher_relay_absent;
static SRWLOCK world_lock = SRWLOCK_INIT;

/* What server.cfg (or Infinity.cfg [relay]) says (UTF-8 values, as they go into the
   announcement). */
typedef struct {
    char name[100], message[400], visibility[16], password[100];
    unsigned max_players, port;
} WorldConfig;
static WorldConfig world_config;
/* the world's relay is the launcher's infinity.exe (1) or dedicated_server.exe (0) */
static volatile LONG world_launcher_relay;
static char world_lan[64];
static unsigned world_players;
/* OD-54: in a launcher session the launcher runs the relay; the port the game reached it on */
static volatile LONG world_launcher_port;

void azimuth_world_launcher_host(unsigned port) {
    if (port && !fi_launch_network_allowed()) return;
    InterlockedExchange(&world_launcher_port, (LONG)port);
    if (port && !fi_launch_network_allowed()) InterlockedExchange(&world_launcher_port, 0);
}

void azimuth_world_set_directory(const wchar_t *directory) {
    size_t n = 0;
    while (directory && directory[n] && n + 1 < ARRAY_COUNT(world_directory)) {
        world_directory[n] = directory[n];
        ++n;
    }
    world_directory[n] = 0;
}

unsigned azimuth_world_port(void) {
    if (!fi_launch_network_allowed()) return 0;
    return world_on ? world_config.port : (unsigned)world_launcher_port;
}

int azimuth_world_hosting(void) {
    if (!fi_launch_network_allowed()) return 0;
    return world_on != 0 || world_launcher_port != 0;
}

/* <Azimuth>\infinity.exe into `exe` when the relay is the launcher's: it is there and
   [hook] RelayCommand is not legacy. */
static int world_launcher_exe(wchar_t *exe, size_t cap) {
    const wchar_t *cfg = fi_cfg_path();
    size_t n = 0, folder = 0;
    if (fi_cfg()->relay_legacy || world_launcher_relay_absent || !cfg) {
        return 0;
    }
    for (; cfg[n]; ++n) {
        if (cfg[n] == L'\\' || cfg[n] == L'/') {
            folder = n + 1;
        }
    }
    static const wchar_t name[] = WORLD_LAUNCHER_EXE;
    if (!folder || folder + ARRAY_COUNT(name) >= cap) {
        return 0;
    }
    memory_copy(exe, cfg, folder * sizeof(wchar_t));
    memory_copy(exe + folder, name, sizeof(name));
    return GetFileAttributesW(exe) != INVALID_FILE_ATTRIBUTES;
}

/* The settings file of the world's relay: Infinity.cfg ([relay]) or data\bin\server.cfg. */
const wchar_t *azimuth_world_config_path(void) {
    static wchar_t path[PATH_CAPACITY], exe[PATH_CAPACITY];
    if (world_launcher_exe(exe, ARRAY_COUNT(exe))) {
        return fi_cfg_path();
    }
    if (!path_join(path, world_directory, WORLD_CFG_NAME)) {
        path[0] = 0;
    }
    return path;
}

/* The relay's STUN/UPnP results file. */
static const wchar_t *world_net_name(void) {
    return world_launcher_relay ? WORLD_LAUNCHER_NET : WORLD_NET_NAME;
}

static void world_copy_utf8(char *out, size_t cap, const char *in) {
    size_t n = 0;
    while (in[n] && n + 1 < cap) {
        out[n] = in[n];
        ++n;
    }
    /* never half a UTF-8 sequence: cut before a character whose bytes did not all fit */
    while (n && in[n] && ((unsigned char)in[n] & 0xc0) == 0x80) {
        --n;
    }
    out[n] = 0;
}

/* Infinity.cfg [relay] into `config` (the launcher's defaults when the section is missing). */
static void world_read_relay_section(WorldConfig *config) {
    FiRelayConfig relay;
    fi_cfg_relay(&relay);
    world_copy_utf8(config->name, sizeof(config->name), relay.name);
    world_copy_utf8(config->message, sizeof(config->message), relay.message);
    world_copy_utf8(config->visibility, sizeof(config->visibility), relay.visibility);
    world_copy_utf8(config->password, sizeof(config->password), relay.password);
    config->max_players = relay.max_players;
    config->port = relay.port;
}

/* ------------------------------------------------------------ server.cfg */

/* The same file and keys as dedicated/server.cfg (UTF-8, CRLF). */
static const char world_default_cfg[] =
    "; ForzaInfinity: configuração do relay legado de free roam.\r\n"
    "; Vale a cada vez que o mundo é aberto. Toda chave é opcional: sem ela, vale o padrão\r\n"
    "; mostrado aqui. Comentários começam com \";\" (no meio da linha, precisa de um espaço antes).\r\n"
    "\r\n"
    "[World]\r\n"
    "; Nome do mundo na lista (vazio = o seu perfil).\r\n"
    "Name=\r\n"
    "; Mensagem para quem entra.\r\n"
    "Message=\r\n"
    "; Quem acha o mundo: public (todos), friends (só amigos) ou off (ninguém; mundo particular).\r\n"
    "Visibility=public\r\n"
    "; Senha (vazio = sem senha). Um mundo com senha não aparece na lista: só se entra\r\n"
    "; pelas ações do jogador (ENTRAR NO MUNDO), digitando a senha.\r\n"
    "Password=\r\n"
    "; Máximo de jogadores ao mesmo tempo, com você, de 1 a 12 (12 é o limite do jogo).\r\n"
    "MaxPlayers=8\r\n"
    "\r\n"
    "[Network]\r\n"
    "; Porta UDP do mundo. Para jogar pela internet, ela precisa estar acessível\r\n"
    "; (UPnP no roteador, IPv6 ou a porta encaminhada para este PC).\r\n"
    "Port=5101\r\n"
    "\r\n"
    "[Access]\r\n"
    "; Perfis que podem entrar, separados por vírgula (vazio = todos).\r\n"
    "Allow=\r\n"
    "; Perfis que não podem entrar, separados por vírgula.\r\n"
    "Ban=\r\n"
    "\r\n"
    "[Events]\r\n"
    "; Corridas do mundo: 1 = o mundo conduz as corridas dos eventos; 0 = quem entra num evento\r\n"
    "; volta ao free roam.\r\n"
    "Races=1\r\n";

static int world_write_default_cfg(const wchar_t *path) {
    HANDLE file = CreateFileW(path, GENERIC_WRITE, 0, 0, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, 0);
    if (file == INVALID_HANDLE_VALUE) {
        return 0;
    }
    DWORD written = 0;
    int ok = WriteFile(file, world_default_cfg, (DWORD)(sizeof(world_default_cfg) - 1), &written, 0) &&
             written == sizeof(world_default_cfg) - 1;
    CloseHandle(file);
    if (ok) {
        log_append("WORLD: server.cfg written with the defaults.\r\n");
    }
    return ok;
}

static void world_copy_value(char *out, size_t capacity, const char *value, size_t length) {
    size_t n = length < capacity - 1 ? length : capacity - 1;
    for (size_t i = 0; i < n; ++i) {
        out[i] = value[i];
    }
    out[n] = 0;
}

static unsigned world_number(const char *value, size_t length, unsigned fallback) {
    unsigned n = 0;
    if (!length) {
        return fallback;
    }
    for (size_t i = 0; i < length; ++i) {
        if (value[i] < '0' || value[i] > '9' || n > 100000) {
            return fallback;
        }
        n = n * 10 + (unsigned)(value[i] - '0');
    }
    return n;
}

static int world_key_is(const char *key, size_t length, const char *name) {
    size_t n = text_length(name);
    if (n != length) {
        return 0;
    }
    for (size_t i = 0; i < n; ++i) {
        char a = key[i], b = name[i];
        if (a >= 'A' && a <= 'Z') a = (char)(a - 'A' + 'a');
        if (b >= 'A' && b <= 'Z') b = (char)(b - 'A' + 'a');
        if (a != b) {
            return 0;
        }
    }
    return 1;
}

/* A small INI reader: [Section], Key=Value, ";" comments (mid-line after a space or tab). */
static int world_read_cfg(WorldConfig *config) {
    static char text[WORLD_CFG_MAX];
    world_copy_value(config->name, sizeof(config->name), "", 0);
    world_copy_value(config->message, sizeof(config->message), "", 0);
    world_copy_value(config->visibility, sizeof(config->visibility), "public", 6);
    world_copy_value(config->password, sizeof(config->password), "", 0);
    config->max_players = 8;
    config->port = 5101;
    HANDLE file = CreateFileW(azimuth_world_config_path(), GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING,
                              FILE_ATTRIBUTE_NORMAL, 0);
    if (file == INVALID_HANDLE_VALUE) {
        return 0;
    }
    DWORD size = 0;
    int ok = ReadFile(file, text, sizeof(text) - 1, &size, 0);
    CloseHandle(file);
    if (!ok) {
        return 0;
    }
    text[size] = 0;
    char section[16];
    section[0] = 0;
    size_t at = 0;
    if (size >= 3 && (unsigned char)text[0] == 0xef && (unsigned char)text[1] == 0xbb && (unsigned char)text[2] == 0xbf) {
        at = 3; /* UTF-8 BOM */
    }
    while (at < size) {
        size_t start = at, end;
        while (at < size && text[at] != '\n') {
            ++at;
        }
        end = at++;
        if (end > start && text[end - 1] == '\r') {
            --end;
        }
        while (start < end && (text[start] == ' ' || text[start] == '\t')) {
            ++start;
        }
        if (start >= end || text[start] == ';' || text[start] == '#') {
            continue;
        }
        if (text[start] == '[') {
            size_t close = start + 1;
            while (close < end && text[close] != ']') {
                ++close;
            }
            world_copy_value(section, sizeof(section), text + start + 1, close - start - 1);
            continue;
        }
        size_t equal = start;
        while (equal < end && text[equal] != '=') {
            ++equal;
        }
        if (equal >= end) {
            continue;
        }
        size_t key_end = equal;
        while (key_end > start && (text[key_end - 1] == ' ' || text[key_end - 1] == '\t')) {
            --key_end;
        }
        size_t value = equal + 1, value_end = end;
        for (size_t i = value; i + 1 < end; ++i) {
            if ((text[i] == ' ' || text[i] == '\t') && text[i + 1] == ';') {
                value_end = i;
                break;
            }
        }
        while (value < value_end && (text[value] == ' ' || text[value] == '\t')) {
            ++value;
        }
        while (value_end > value && (text[value_end - 1] == ' ' || text[value_end - 1] == '\t')) {
            --value_end;
        }
        const char *key = text + start, *v = text + value;
        size_t key_length = key_end - start, length = value_end - value;
        if (world_key_is(section, text_length(section), "World")) {
            if (world_key_is(key, key_length, "Name")) {
                world_copy_value(config->name, sizeof(config->name), v, length);
            } else if (world_key_is(key, key_length, "Message")) {
                world_copy_value(config->message, sizeof(config->message), v, length);
            } else if (world_key_is(key, key_length, "Visibility")) {
                world_copy_value(config->visibility, sizeof(config->visibility), v, length);
            } else if (world_key_is(key, key_length, "Password")) {
                world_copy_value(config->password, sizeof(config->password), v, length);
            } else if (world_key_is(key, key_length, "MaxPlayers")) {
                config->max_players = world_number(v, length, 8);
            }
        } else if (world_key_is(section, text_length(section), "Network") &&
                   world_key_is(key, key_length, "Port")) {
            config->port = world_number(v, length, 5101);
        }
    }
    if (config->max_players < 1 || config->max_players > 12) {
        config->max_players = 8;
    }
    if (config->port < 1 || config->port > 65535 || config->port == 5100) {
        config->port = 5101;
    }
    if (!world_key_is(config->visibility, text_length(config->visibility), "public") &&
        !world_key_is(config->visibility, text_length(config->visibility), "friends") &&
        !world_key_is(config->visibility, text_length(config->visibility), "off")) {
        world_copy_value(config->visibility, sizeof(config->visibility), "public", 6);
    }
    return 1;
}

/* ------------------------------------------------------------- LAN address */

typedef int(WINAPI *WorldStartupFn)(WORD, void *);
typedef UINT_PTR(WINAPI *WorldSocketFn)(int, int, int);
typedef int(WINAPI *WorldConnectFn)(UINT_PTR, const void *, int);
typedef int(WINAPI *WorldNameFn)(UINT_PTR, void *, int *);
typedef int(WINAPI *WorldCloseFn)(UINT_PTR);

/* This PC's address on the network the default route leaves by: a UDP socket "connected" to
   a public address sends nothing, and its local name is that address. */
static void world_find_lan(void) {
    static unsigned char wsa[512];
    world_lan[0] = 0;
    HMODULE ws2 = LoadLibraryExW(L"ws2_32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!ws2) {
        return;
    }
    WorldStartupFn startup = (WorldStartupFn)(uintptr_t)GetProcAddress(ws2, "WSAStartup");
    WorldSocketFn make = (WorldSocketFn)(uintptr_t)GetProcAddress(ws2, "socket");
    WorldConnectFn join = (WorldConnectFn)(uintptr_t)GetProcAddress(ws2, "connect");
    WorldNameFn name = (WorldNameFn)(uintptr_t)GetProcAddress(ws2, "getsockname");
    WorldCloseFn close = (WorldCloseFn)(uintptr_t)GetProcAddress(ws2, "closesocket");
    if (!startup || !make || !join || !name || !close || startup(0x0202, wsa)) {
        return;
    }
    UINT_PTR s = make(2 /* AF_INET */, 2 /* SOCK_DGRAM */, 17 /* IPPROTO_UDP */);
    if (s == (UINT_PTR)~(UINT_PTR)0) {
        return;
    }
    unsigned char to[16] = {2, 0, 0, 53, 8, 8, 8, 8, 0, 0, 0, 0, 0, 0, 0, 0}, mine[16];
    int length = sizeof(mine);
    if (!join(s, to, sizeof(to)) && !name(s, mine, &length) && mine[4]) {
        char *o = world_lan;
        for (int i = 0; i < 4; ++i) {
            unsigned v = mine[4 + i];
            if (v >= 100) *o++ = (char)('0' + v / 100);
            if (v >= 10) *o++ = (char)('0' + (v / 10) % 10);
            *o++ = (char)('0' + v % 10);
            if (i < 3) *o++ = '.';
        }
        *o = 0;
    }
    close(s);
}

/* ------------------------------------------------------------ announcement */

typedef struct {
    char *data;
    size_t size, capacity;
} WorldText;

static void world_add(WorldText *t, const char *text) {
    while (text && *text && t->size + 1 < t->capacity) {
        t->data[t->size++] = *text++;
    }
    t->data[t->size] = 0;
}

static void world_add_json(WorldText *t, const char *text) {
    static const char hex[] = "0123456789abcdef";
    world_add(t, "\"");
    for (; text && *text && t->size + 8 < t->capacity; ++text) {
        unsigned char c = (unsigned char)*text;
        if (c == '"' || c == '\\') {
            t->data[t->size++] = '\\';
            t->data[t->size++] = (char)c;
        } else if (c < 0x20) {
            const char esc[7] = {'\\', 'u', '0', '0', hex[c >> 4], hex[c & 15], 0};
            world_add(t, esc);
        } else {
            t->data[t->size++] = (char)c;
        }
    }
    t->data[t->size] = 0;
    world_add(t, "\"");
}

static void world_add_number(WorldText *t, unsigned value) {
    char digits[12], text[12];
    unsigned n = 0;
    do {
        digits[n++] = (char)('0' + value % 10);
        value /= 10;
    } while (value);
    for (unsigned i = 0; i < n; ++i) {
        text[i] = digits[n - 1 - i];
    }
    text[n] = 0;
    world_add(t, text);
}

static const wchar_t *world_token_header(void) {
    static wchar_t header[1200];
    if (!azimuth_account_token() || !azimuth_account_auth_header(header, ARRAY_COUNT(header))) {
        return 0;
    }
    return header;
}

/* The server's "kick":["<xuid>",...] (players whose car database changed, later the host's
   bans) go to the relay as "kick <xuid>" lines on its stdin: only this PC can reach it. XUIDs
   are digits only; anything else is skipped. */
static void world_pass_kicks(const char *reply) {
    const char *at = text_find(reply, text_length(reply), "\"kick\":[");
    if (!at || !world_relay_input) {
        return;
    }
    for (at += 8; *at && *at != ']';) {
        char line[40];
        size_t n = 0;
        while (*at && *at != ']' && (*at < '0' || *at > '9')) {
            ++at;
        }
        if (*at < '0' || *at > '9') {
            break;
        }
        line[n++] = 'k', line[n++] = 'i', line[n++] = 'c', line[n++] = 'k', line[n++] = ' ';
        while (*at >= '0' && *at <= '9' && n < 30) {
            line[n++] = *at++;
        }
        while (*at >= '0' && *at <= '9') {
            ++at; /* longer than any xuid: not ours */
        }
        line[n++] = '\n';
        DWORD written = 0;
        WriteFile(world_relay_input, line, (DWORD)n, &written, 0);
        log_append("WORLD: the server asked to kick a player; passed to the relay.\r\n");
    }
}

/* An address as the relay and the server write it: digits, hex letters, dots, colons and
   brackets only, 4 to 63 characters. */
static int world_address_text(const char *a, size_t n) {
    if (n < 4 || n > 63) {
        return 0;
    }
    for (size_t i = 0; i < n; ++i) {
        char c = a[i];
        int ok = (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F') || c == '.' || c == ':' ||
                 c == '[' || c == ']';
        if (!ok) {
            return 0;
        }
    }
    return 1;
}

/* The server's "punch":["<ip:port>",...] go to the relay as "punch <ip:port>" lines. */
static void world_pass_punches(const char *reply) {
    const char *at = text_find(reply, text_length(reply), "\"punch\":[");
    if (!at || !world_relay_input) {
        return;
    }
    unsigned passed = 0;
    for (at += 9; *at && *at != ']';) {
        if (*at != '"') {
            ++at;
            continue;
        }
        const char *start = ++at;
        while (*at && *at != '"') {
            ++at;
        }
        size_t n = (size_t)(at - start);
        if (*at == '"') {
            ++at;
        }
        if (!world_address_text(start, n)) {
            continue;
        }
        char line[80];
        memory_copy(line, "punch ", 6);
        memory_copy(line + 6, start, n);
        line[6 + n] = '\n';
        DWORD written = 0;
        WriteFile(world_relay_input, line, (DWORD)(7 + n), &written, 0);
        ++passed;
    }
    if (passed) {
        log_append("WORLD: a player is joining; punch passed to the relay.\r\n");
    }
}

/* dedicated_server.net while a world is on (TEST CONNECTION shows it). */
int azimuth_world_net_text(char *out, size_t cap) {
    static wchar_t path[PATH_CAPACITY];
    out[0] = 0;
    if (!world_on || cap < 2 || !path_join(path, world_directory, world_net_name())) {
        return 0;
    }
    HANDLE file = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0,
                              OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
    if (file == INVALID_HANDLE_VALUE) {
        return 0;
    }
    DWORD size = 0;
    int ok = ReadFile(file, out, (DWORD)(cap - 1), &size, 0);
    CloseHandle(file);
    out[ok ? size : 0] = 0;
    return ok && size;
}

/* dedicated_server.net (the relay's STUN and UPnP results) as the announcement's
   ,"public":[...],"nat":"..." (nothing before the relay wrote it). */
static void world_add_net(WorldText *t) {
    static wchar_t path[PATH_CAPACITY];
    static char text[1024];
    if (!path_join(path, world_directory, world_net_name())) {
        return;
    }
    HANDLE file = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0,
                              OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
    if (file == INVALID_HANDLE_VALUE) {
        return;
    }
    DWORD size = 0;
    int ok = ReadFile(file, text, sizeof(text) - 1, &size, 0);
    CloseHandle(file);
    if (!ok) {
        return;
    }
    text[size] = 0;
    const char *nat = "unknown";
    unsigned count = 0;
    world_add(t, ",\"public\":[");
    for (char *line = text; *line;) {
        char *end = line;
        while (*end && *end != '\n' && *end != '\r') {
            ++end;
        }
        size_t n = (size_t)(end - line);
        if (n == 8 && memory_equal(line, "nat cone", 8)) {
            nat = "cone";
        } else if (n == 13 && memory_equal(line, "nat symmetric", 13)) {
            nat = "symmetric";
        } else if (count < 3 && ((n > 7 && memory_equal(line, "public ", 7)) || (n > 5 && memory_equal(line, "upnp ", 5)))) {
            const char *a = line + (line[0] == 'p' ? 7 : 5);
            size_t k = n - (size_t)(a - line);
            if (world_address_text(a, k)) {
                char one[64];
                memory_copy(one, a, k);
                one[k] = 0;
                if (count++) {
                    world_add(t, ",");
                }
                world_add_json(t, one);
            }
        }
        line = *end ? end + 1 : end;
    }
    world_add(t, "],\"nat\":");
    world_add_json(t, nat);
}

/* POST /infinity/v1/worlds (start and heartbeat). Returns the HTTP status. */
static DWORD world_announce(void) {
    if (!fi_launch_network_allowed()) return 0;
    static char body[1536], reply[4096];
    WorldText t = {body, 0, sizeof(body)};
    body[0] = 0;
    AcquireSRWLockExclusive(&world_lock);
    world_add(&t, "{\"name\":");
    world_add_json(&t, world_config.name);
    world_add(&t, ",\"message\":");
    world_add_json(&t, world_config.message);
    world_add(&t, ",\"visibility\":");
    world_add_json(&t, world_config.visibility);
    world_add(&t, ",\"password\":");
    world_add_json(&t, world_config.password);
    world_add(&t, ",\"maxPlayers\":");
    world_add_number(&t, world_config.max_players);
    world_add(&t, ",\"port\":");
    world_add_number(&t, world_config.port);
    world_add(&t, ",\"lan\":[");
    if (world_lan[0]) {
        world_add_json(&t, world_lan);
    }
    world_add(&t, world_listed ? "],\"listed\":true,\"version\":" : "],\"listed\":false,\"version\":");
    world_add_json(&t, MOD_VERSION);
    world_add_net(&t);
    world_add(&t, "}");
    ReleaseSRWLockExclusive(&world_lock);
    DWORD status = azimuth_net_call(L"POST", L"/infinity/v1/worlds", world_token_header(), body, reply, sizeof(reply));
    if (!fi_launch_network_allowed()) return 0;
    if (status == 200) {
        world_pass_kicks(reply);
        world_pass_punches(reply);
        /* "players": n */
        const char *p = text_find(reply, text_length(reply), "\"players\":");
        if (p) {
            unsigned n = 0;
            for (p += 10; *p == ' '; ++p) {
            }
            for (; *p >= '0' && *p <= '9'; ++p) n = n * 10 + (unsigned)(*p - '0');
            world_players = n;
        }
    }
    return status;
}

static void world_log_status(const char *what, DWORD value) {
    char text[96];
    WorldText t = {text, 0, sizeof(text)};
    text[0] = 0;
    world_add(&t, "WORLD: ");
    world_add(&t, what);
    world_add_number(&t, (unsigned)value);
    world_add(&t, ".\r\n");
    log_append(text);
}

static void world_end(const char *why) {
    if (!InterlockedExchange(&world_on, 0)) {
        return;
    }
    InterlockedIncrement(&world_generation);
    if (world_job) {
        TerminateJobObject(world_job, 0);
    }
    if (world_process) {
        CloseHandle(world_process);
        world_process = 0;
    }
    static char reply[256];
    azimuth_net_call(L"DELETE", L"/infinity/v1/worlds", world_token_header(), 0, reply, sizeof(reply));
    log_append("WORLD: ended (");
    log_append(why);
    log_append(").\r\n");
}

/* GET /infinity/v1/worlds/punches: players about to join (F3). */
static void world_poll_punches(void) {
    static char reply[4096];
    if (azimuth_net_call(L"GET", L"/infinity/v1/worlds/punches", world_token_header(), 0, reply, sizeof(reply)) == 200) {
        world_pass_punches(reply);
    }
}

static DWORD WINAPI world_heartbeat(void *argument) {
    LONG generation = (LONG)(intptr_t)argument;
    for (;;) {
        for (unsigned waited = 0; waited < WORLD_HEARTBEAT_MS && world_generation == generation; waited += 500) {
            Sleep(500);
            if ((waited + 500) % WORLD_PUNCH_POLL_MS == 0 && waited + 500 < WORLD_HEARTBEAT_MS && world_on &&
                world_generation == generation) {
                world_poll_punches();
            }
        }
        if (!world_on || world_generation != generation) {
            break;
        }
        DWORD code = 0;
        if (world_process && GetExitCodeProcess(world_process, &code) && code != STILL_ACTIVE) {
            world_log_status("the relay stopped, exit code ", code);
            world_end("the relay stopped");
            break;
        }
        DWORD status = world_announce();
        if (status != 200) {
            world_log_status("heartbeat answered ", status);
        }
    }
    return 0;
}

/* --------------------------------------------------------------- start/stop */

static int world_job_ready(void) {
    static JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits;
    if (world_job) {
        return 1;
    }
    world_job = CreateJobObjectW(0, 0);
    if (!world_job) {
        return 0;
    }
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if (!SetInformationJobObject(world_job, JobObjectExtendedLimitInformation, &limits, sizeof(limits))) {
        CloseHandle(world_job);
        world_job = 0;
        return 0;
    }
    return 1;
}

/* 1 when the just-started infinity.exe relay ended within WORLD_RELAY_PROBE_MS (`code`: its exit
   code; WORLD_RELAY_ABSENT_EXIT also marks the session as without a launcher relay), 0 while it
   runs. */
static int world_relay_probe(HANDLE process, DWORD *code) {
    *code = STILL_ACTIVE;
    if (WaitForSingleObject(process, WORLD_RELAY_PROBE_MS) != WAIT_OBJECT_0 || !GetExitCodeProcess(process, code)) {
        return 0;
    }
    if (*code == WORLD_RELAY_ABSENT_EXIT) {
        InterlockedExchange(&world_launcher_relay_absent, 1);
    }
    return 1;
}

/* The relay's command line: "<exe>" --config "<cfg>" (dedicated_server.exe), or
   "<infinity.exe>" relay --cfg "<cfg>" --log "<log>" --net "<net>" (the launcher's relay mode,
   PRODUCT.md 4.1). Its length (0 when it does not fit). */
static size_t world_command(int launcher, const wchar_t *exe, const wchar_t *cfg, const wchar_t *log,
                            const wchar_t *net, wchar_t *out, size_t cap) {
    const wchar_t *parts[9] = {L"\"", exe, L"\" --config \"", cfg, L"\"", 0, 0, 0, 0};
    if (launcher) {
        parts[2] = L"\" relay --cfg \"";
        parts[4] = L"\" --log \"";
        parts[5] = log;
        parts[6] = L"\" --net \"";
        parts[7] = net;
        parts[8] = L"\"";
    }
    size_t n = 0;
    for (size_t p = 0; p < ARRAY_COUNT(parts) && parts[p]; ++p) {
        for (size_t i = 0; parts[p][i]; ++i) {
            if (n + 1 >= cap) {
                out[0] = 0;
                return 0;
            }
            out[n++] = parts[p][i];
        }
    }
    out[n] = 0;
    return n;
}

/* 1 started, 0 failed (`why` says it in English, for azimuth_tr). */
static int world_open(int listed, const wchar_t **why) {
    static wchar_t exe[PATH_CAPACITY], log_path[PATH_CAPACITY], command[PATH_CAPACITY + 64];
    static STARTUPINFOW startup;
    static PROCESS_INFORMATION process;
    static SECURITY_ATTRIBUTES inherit;
    *why = L"";
    if (fi_launch_protocol()) {
        /* OD-54: worlds are opened from the overlay; the launcher runs the relay */
        *why = L"Worlds are opened from the Infinity overlay (hold LB+RB).";
        return 0;
    }
    if (world_on) {
        return 1;
    }
    if (!azimuth_account_token()) {
        *why = L"Hosting a world needs your online profile.";
        return 0;
    }
    if (world_car_db_changed) {
        *why = L"The car database changed after the game started. Restart the game to play online.";
        return 0;
    }
    int launcher = world_launcher_exe(exe, ARRAY_COUNT(exe));
    if (!launcher &&
        (!path_join(exe, world_directory, WORLD_EXE_NAME) || GetFileAttributesW(exe) == INVALID_FILE_ATTRIBUTES)) {
        *why = L"dedicated_server.exe is missing from the game's data\\bin folder. Reinstall ForzaInfinity.";
        return 0;
    }
    const wchar_t *cfg = azimuth_world_config_path();
    if (!launcher && GetFileAttributesW(cfg) == INVALID_FILE_ATTRIBUTES) {
        world_write_default_cfg(cfg);
    }
    InterlockedExchange(&world_launcher_relay, launcher);
    AcquireSRWLockExclusive(&world_lock);
    if (launcher) {
        world_read_relay_section(&world_config);
    } else {
        world_read_cfg(&world_config);
    }
    ReleaseSRWLockExclusive(&world_lock);
    world_find_lan();
    if (!world_job_ready()) {
        *why = L"The world could not be started (job object).";
        return 0;
    }
    /* "<exe>" --config "<cfg>", or "<infinity.exe>" relay --cfg "<cfg>" --log "<log>" --net "<net>" */
    static wchar_t net_path[PATH_CAPACITY];
    int paths = path_join(log_path, world_directory, launcher ? WORLD_LAUNCHER_LOG : WORLD_LOG_NAME) &&
                path_join(net_path, world_directory, WORLD_LAUNCHER_NET);
    if (!paths || !world_command(launcher, exe, cfg, log_path, net_path, command, ARRAY_COUNT(command))) {
        *why = launcher ? L"The world could not be started (infinity.exe relay did not run)."
                        : L"The world could not be started (dedicated_server.exe did not run).";
        return 0;
    }
    inherit.nLength = sizeof(inherit);
    inherit.bInheritHandle = TRUE;
    HANDLE log_file = INVALID_HANDLE_VALUE;
    if (paths && launcher) {
        /* infinity.exe writes its own --log; its console goes to the end of the same file (both
           append, so an early failure is kept with the rest) */
        HANDLE fresh = CreateFileW(log_path, GENERIC_WRITE, FILE_SHARE_READ, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, 0);
        if (fresh != INVALID_HANDLE_VALUE) {
            CloseHandle(fresh);
        }
        log_file = CreateFileW(log_path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, &inherit, OPEN_ALWAYS,
                               FILE_ATTRIBUTE_NORMAL, 0);
    } else if (paths) {
        log_file = CreateFileW(log_path, GENERIC_WRITE, FILE_SHARE_READ, &inherit, CREATE_ALWAYS,
                               FILE_ATTRIBUTE_NORMAL, 0);
    }
    /* the relay's stdin: a pipe only this process writes to (kick lines) */
    HANDLE input_read = 0, input_write = 0;
    if (CreatePipe(&input_read, &input_write, &inherit, 0)) {
        SetHandleInformation(input_write, HANDLE_FLAG_INHERIT, 0);
    } else {
        input_read = input_write = 0;
    }
    startup.cb = sizeof(startup);
    startup.dwFlags = STARTF_USESHOWWINDOW | STARTF_USESTDHANDLES;
    startup.wShowWindow = SW_HIDE;
    startup.hStdInput = input_read;
    startup.hStdOutput = log_file != INVALID_HANDLE_VALUE ? log_file : 0;
    startup.hStdError = log_file != INVALID_HANDLE_VALUE ? log_file : 0;
    BOOL created = CreateProcessW(exe, command, 0, 0, TRUE, CREATE_NO_WINDOW | CREATE_SUSPENDED, 0, world_directory,
                                  &startup, &process);
    if (input_read) {
        CloseHandle(input_read);
    }
    if (!created && input_write) {
        CloseHandle(input_write);
        input_write = 0;
    }
    if (log_file != INVALID_HANDLE_VALUE) {
        CloseHandle(log_file);
    }
    if (!created) {
        world_log_status("CreateProcess failed, error ", GetLastError());
        *why = launcher ? L"The world could not be started (infinity.exe relay did not run)."
                        : L"The world could not be started (dedicated_server.exe did not run).";
        return 0;
    }
    if (!AssignProcessToJobObject(world_job, process.hProcess)) {
        /* Without the job it would outlive the game: do not run it. */
        world_log_status("AssignProcessToJobObject failed, error ", GetLastError());
        TerminateProcess(process.hProcess, 1);
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
        if (input_write) {
            CloseHandle(input_write);
        }
        *why = L"The world could not be started (job object).";
        return 0;
    }
    ResumeThread(process.hThread);
    CloseHandle(process.hThread);
    DWORD early = 0;
    if (launcher && world_relay_probe(process.hProcess, &early)) {
        CloseHandle(process.hProcess);
        if (input_write) {
            CloseHandle(input_write);
        }
        if (early == WORLD_RELAY_ABSENT_EXIT) {
            log_append("WORLD: this infinity.exe has no relay (exit code 3); the world uses dedicated_server.exe.\r\n");
            return world_open(listed, why);
        }
        world_log_status("infinity.exe relay stopped at once, exit code ", early);
        *why = L"The world could not be started (infinity.exe relay did not run).";
        return 0;
    }
    world_process = process.hProcess;
    if (world_relay_input) {
        CloseHandle(world_relay_input);
    }
    world_relay_input = input_write;
    world_players = 1;
    InterlockedExchange(&world_listed, listed);
    InterlockedExchange(&world_on, 1);
    log_append(launcher ? "WORLD: infinity.exe relay started (killed with the game); settings from Infinity.cfg [relay].\r\n"
                        : "WORLD: dedicated_server.exe started (killed with the game).\r\n");
    DWORD status = world_announce();
    if (status != 200) {
        world_log_status("announcement answered ", status);
        world_end("the server refused the announcement");
        *why = status == 404 ? L"This server does not take hosted worlds yet."
                             : L"The server did not take the world. Try again in a moment.";
        return 0;
    }
    HANDLE thread = CreateThread(0, 0, world_heartbeat, (void *)(intptr_t)world_generation, 0, 0);
    if (thread) {
        CloseHandle(thread);
    }
    return 1;
}


/* The association with the world was destroyed: the game left the online session (HORIZON
   SOLO), or a join failed. The world this PC hosts goes with it. */
void azimuth_world_left(void) {
    if (world_on) {
        world_end("left the online session");
    }
}

static int world_has(const unsigned char *data, size_t size, const char *needle) {
    return data && text_find((const char *)data, size, needle) != 0;
}

static int world_wide_has(const wchar_t *text, const wchar_t *needle) {
    for (; text && *text; ++text) {
        size_t i = 0;
        while (needle[i] && text[i] == needle[i]) {
            ++i;
        }
        if (!needle[i]) {
            return 1;
        }
    }
    return 0;
}

/* Every request of the game, before it is forwarded (azimuth_net.c, a worker thread): a
   private free roam session being written opens this PC's world first, so the server's
   answer already points the game at it. */
void azimuth_world_on_request(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size) {
    if (fi_launch_protocol() || !world_directory[0] || world_on || !method || method[0] != L'P' || method[1] != L'U' ||
        !world_wide_has(url, L"sessiondirectory.xboxlive.com") || !world_has(body, size, "\"gameServerPlatform\"") ||
        !world_has(body, size, "\"hopperId\":2") || world_has(body, size, "\"matchmaking\"")) {
        return;
    }
    const wchar_t *why = L"";
    log_append(world_open(0, &why) ? "WORLD: private free roam session: this PC hosts it (unlisted).\r\n"
                                   : "WORLD: private free roam session, but the world did not open.\r\n");
}

/* ------------------------------------------------------------ the car database */
/* Players only meet with the same cars installed (imported FM5 cars are rows in it): the
   SHA-256 of the game's media\stripped\gamedbRC.slt goes with every request to the server
   (X-Fh2-CarDb, azimuth_net.c) and the server keeps worlds apart by it. Hashed once, on a
   thread of its own, when the game starts. */

#define WORLD_CAR_DB L"media\\stripped\\gamedbRC.slt"
#define WORLD_HASH_CHUNK 65536u

static wchar_t world_car_db_path[PATH_CAPACITY];
static wchar_t world_car_db_header[96];
static volatile LONG world_car_db_ready;
/* 0 not started, 1 hashing, 2 done (ready or unreadable): requests wait only while 1. */
static volatile LONG world_car_db_state;
static unsigned char world_car_db_digest[32];

#define WORLD_REHASH_MS 30000u

static void world_car_db_tampered(void);

/* SHA-256 of the car database; 0 when it cannot be read. */
static int world_digest_car_db(unsigned char digest[32]) {
    unsigned char *chunk = HeapAlloc(GetProcessHeap(), 0, WORLD_HASH_CHUNK);
    HANDLE file = CreateFileW(world_car_db_path, GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING,
                              FILE_ATTRIBUTE_NORMAL, 0);
    if (!chunk || file == INVALID_HANDLE_VALUE) {
        if (chunk) HeapFree(GetProcessHeap(), 0, chunk);
        if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
        return 0;
    }
    Sha256Context hash;
    DWORD got = 0;
    sha256_initialize(&hash);
    while (ReadFile(file, chunk, WORLD_HASH_CHUNK, &got, 0) && got) {
        sha256_update(&hash, chunk, got);
    }
    CloseHandle(file);
    HeapFree(GetProcessHeap(), 0, chunk);
    sha256_finalize(&hash, digest);
    return 1;
}

/* Hashed when the game starts, then again every 30 s: a database changed during the session
   (cars swapped for an advantage) cuts the player off the world and tells the server
   (world_car_db_tampered). A file that cannot be read for a moment is not a change. A
   modified client can lie about all of this: it only stops honest clients from switching
   cars mid-session, and the world's own checks are the relay's. */
static DWORD WINAPI world_hash_car_db(void *unused) {
    (void)unused;
    unsigned char digest[32];
    if (!world_digest_car_db(digest)) {
        InterlockedExchange(&world_car_db_state, 2);
        log_append("WORLD: the car database could not be read; the server will refuse online play.\r\n");
        return 0;
    }
    memory_copy(world_car_db_digest, digest, sizeof(digest));
    static const wchar_t prefix[] = L"X-Fh2-CarDb: ";
    static const char hex[] = "0123456789abcdef";
    size_t n = 0;
    for (size_t i = 0; prefix[i]; ++i) world_car_db_header[n++] = prefix[i];
    for (int i = 0; i < 32; ++i) {
        world_car_db_header[n++] = (wchar_t)hex[digest[i] >> 4];
        world_car_db_header[n++] = (wchar_t)hex[digest[i] & 15];
    }
    world_car_db_header[n] = 0;
    InterlockedExchange(&world_car_db_ready, 1);
    InterlockedExchange(&world_car_db_state, 2);
    char line[96];
    WorldText t = {line, 0, sizeof(line)};
    line[0] = 0;
    world_add(&t, "WORLD: car database ");
    for (int i = 0; i < 8; ++i) {
        char pair[3] = {hex[digest[i] >> 4], hex[digest[i] & 15], 0};
        world_add(&t, pair);
    }
    world_add(&t, "... (sent to the server: only the game's own cars play online).\r\n");
    log_append(line);
    for (;;) {
        Sleep(WORLD_REHASH_MS);
        if (world_digest_car_db(digest) && !memory_equal(digest, world_car_db_digest, sizeof(digest))) {
            world_car_db_tampered();
            return 0;
        }
    }
}

/* `executable` is the game's forza_durango_release_final.exe in its Mount folder. */
void azimuth_world_hash_car_db(const wchar_t *executable) {
    size_t n = 0, slash = 0;
    if (InterlockedCompareExchange(&world_car_db_state, 1, 0) != 0) {
        return;
    }
    while (executable && executable[n] && n + 1 < ARRAY_COUNT(world_car_db_path)) {
        world_car_db_path[n] = executable[n];
        if (executable[n] == L'\\' || executable[n] == L'/') slash = n + 1;
        ++n;
    }
    world_car_db_path[slash] = 0;
    static const wchar_t rest[] = WORLD_CAR_DB;
    if (slash + ARRAY_COUNT(rest) >= ARRAY_COUNT(world_car_db_path)) {
        InterlockedExchange(&world_car_db_state, 2);
        return;
    }
    memory_copy(world_car_db_path + slash, rest, sizeof(rest));
    HANDLE thread = CreateThread(0, 0, world_hash_car_db, 0, 0, 0);
    if (thread) {
        CloseHandle(thread);
    } else {
        InterlockedExchange(&world_car_db_state, 2);
    }
}

/* Waits (at most `ms`) while the car database is being hashed: the server refuses an account's
   requests without the hash, so the game's first ones wait for it. */
void azimuth_world_car_db_wait(unsigned ms) {
    for (unsigned waited = 0; world_car_db_state == 1 && waited < ms; waited += 20) {
        Sleep(20);
    }
}

/* "X-Fh2-CarDb: <sha-256>", or 0 until it is known. */
const wchar_t *azimuth_world_car_db_header(void) {
    return world_car_db_ready ? world_car_db_header : 0;
}

/* The car database changed during the session: off the world, and the server is told (it
   drops this player from the world it is in and asks that world's host to kick it). The
   X-Fh2-CarDb header now says "changed", so the server keeps this game out of every world
   until it restarts. */
static void world_car_db_tampered(void) {
    static char reply[256];
    if (InterlockedExchange(&world_car_db_changed, 1)) {
        return;
    }
    log_append("WORLD: the car database changed during the session; this game leaves the online world.\r\n");
    static const wchar_t changed[] = L"X-Fh2-CarDb: changed";
    memory_copy(world_car_db_header, changed, sizeof(changed));
    azimuth_freeroam_cut();
    if (world_on) {
        world_end("the car database changed");
    }
    azimuth_net_call(L"POST", L"/infinity/v1/worlds/left", world_token_header(), "{\"reason\":\"cars\"}", reply,
                     sizeof(reply));
}
