/* Infinity native mod host. GPL-3.0-or-later; see LICENSE. */
#include "infinity.h"
#include "sign.h"
#include "../core/logging.h"
#include "../core/support.h"
#include "../core/sha256.h"
#include "../../../mod/include/infinity_mod_host.h"

static wchar_t mod_plan[PATH_CAPACITY], mod_runtime[PATH_CAPACITY];
static unsigned char mod_plan_hash[32], mod_runtime_hash[32];
static void *mod_game;
static HANDLE mod_runtime_lock = INVALID_HANDLE_VALUE;
static HANDLE mod_parent_locks[256];
static unsigned mod_parent_count;
static InfinityModsQueryV2Fn mod_query2;
static int mod_online;
static volatile LONG mod_busy;

void fi_mod_host_wait(unsigned ms) {
    for (ULONGLONG end = GetTickCount64() + ms; mod_busy && GetTickCount64() < end;) Sleep(50);
}

static int mod_hex(const wchar_t *text, unsigned char *out, unsigned bytes) {
    for (unsigned i = 0; i < bytes; ++i) {
        unsigned value = 0;
        for (unsigned j = 0; j < 2; ++j) {
            wchar_t c = text[i * 2 + j];
            unsigned digit = c >= L'0' && c <= L'9' ? (unsigned)(c - L'0') : c >= L'a' && c <= L'f' ? (unsigned)(c - L'a' + 10) : 16;
            if (digit > 15) return 0;
            value = value * 16 + digit;
        }
        out[i] = (unsigned char)value;
    }
    return 1;
}
static int mod_hash_env(const wchar_t *name, unsigned char out[32]) {
    wchar_t text[66];
    return GetEnvironmentVariableW(name, text, 66) == 64 && mod_hex(text, out, 32);
}
static int mod_plain(HANDLE h, int directory) {
    BY_HANDLE_FILE_INFORMATION info;
    return h != INVALID_HANDLE_VALUE && GetFileInformationByHandle(h, &info)
        && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)
        && !!(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) == directory;
}

static int mod_signature_ok(const unsigned char digest[32]) {
    static wchar_t path[PATH_CAPACITY];
    size_t n = 0;
    while (mod_runtime[n] && n + 5 < PATH_CAPACITY) { path[n] = mod_runtime[n]; ++n; }
    if (mod_runtime[n]) return 0;
    path[n++] = L'.'; path[n++] = L's'; path[n++] = L'i'; path[n++] = L'g'; path[n] = 0;
    HANDLE file = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, 0);
    unsigned char signature[64];
    DWORD read = 0;
    LARGE_INTEGER size;
    int ok = mod_plain(file, 0) && GetFileSizeEx(file, &size) && size.QuadPart == 64
        && ReadFile(file, signature, sizeof signature, &read, 0) && read == sizeof signature;
    if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
    if (!ok) return 0;
    if (azimuth_sign_mod_runtime_ok(signature, digest, azimuth_sign_release_key)) return 1;
#ifdef FI_DEV
    wchar_t text[66];
    unsigned char key[32];
    if (GetEnvironmentVariableW(L"FI_DEV_HOOK_KEY", text, 66) == 64 && mod_hex(text, key, 32)
        && azimuth_sign_mod_runtime_ok(signature, digest, key)) {
        log_append("MODS: the runtime is signed with this PC's development key (development hook).\r\n");
        return 1;
    }
#endif
    return 0;
}

static const char *mod_lock_runtime(void) {
    static wchar_t absolute[PATH_CAPACITY];
    DWORD n = GetFullPathNameW(mod_runtime, PATH_CAPACITY, absolute, 0);
    if (n < 4 || n >= PATH_CAPACITY || !memory_equal(absolute, mod_runtime, ((size_t)n + 1) * sizeof(wchar_t)) || absolute[1] != L':' || absolute[2] != L'\\') return "the runtime path is not plain";
    for (DWORD i = 3; i < n; ++i) {
        if (absolute[i] == L':' || absolute[i] == L'/') return "the runtime path is not plain";
        if (absolute[i] != L'\\') continue;
        if (absolute[i - 1] == L'.' || absolute[i - 1] == L' ' || mod_parent_count >= 256) return "the runtime path is not plain";
        absolute[i] = 0;
        HANDLE parent = CreateFileW(absolute, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE, 0, OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, 0);
        absolute[i] = L'\\';
        if (!mod_plain(parent, 1)) { if (parent != INVALID_HANDLE_VALUE) CloseHandle(parent); return "a folder of the runtime path is a link"; }
        mod_parent_locks[mod_parent_count++] = parent;
    }
    if (absolute[n - 1] == L'.' || absolute[n - 1] == L' ') return "the runtime path is not plain";
    mod_runtime_lock = CreateFileW(mod_runtime, GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, 0);
    if (!mod_plain(mod_runtime_lock, 0)) return "the runtime is missing or a link";
    LARGE_INTEGER size;
    if (!GetFileSizeEx(mod_runtime_lock, &size) || size.QuadPart <= 0 || size.QuadPart > 64 * 1024 * 1024) return "the runtime has no plausible size";
    Sha256Context context; static unsigned char buffer[16384]; unsigned char actual[32]; DWORD read;
    sha256_initialize(&context);
    for (;;) {
        if (!ReadFile(mod_runtime_lock, buffer, sizeof buffer, &read, 0)) return "the runtime cannot be read";
        if (!read) break;
        sha256_update(&context, buffer, read);
    }
    sha256_finalize(&context, actual);
    if (!memory_equal(actual, mod_runtime_hash, 32)) return "the runtime is not the one the launcher prepared";
    if (!mod_signature_ok(actual)) return "the runtime's signature does not verify";
    return 0;
}

static void mod_refuse(const char *why) {
    InterlockedExchange(&mod_busy, 0);
    log_append("MODS: not loaded (");
    log_append(why);
    log_append("); this session plays without its selected mods.\r\n");
}

static void mod_failure(void) {
    InterlockedExchange(&mod_busy, 0);
    if (mod_online && mod_query2) {
        static InfinityModsSnapshotV2 last; last.size = sizeof last; last.abi_version = 2;
        if (!mod_query2(&last)) { last.failed = 1; fi_launch_mods_report(&last); fi_launch_mods_flush(1000); }
    }
    log_append(mod_online ? "MODS: selected runtime failed; ending the session.\r\n" : "MODS: selected runtime failed; ending the offline session.\r\n");
    ExitProcess(0x46490004);
}

static int mod_report(void) {
    if (!mod_query2) return !mod_online;
    static InfinityModsSnapshotV2 snapshot; snapshot.size = sizeof snapshot; snapshot.abi_version = 2;
    if (mod_query2(&snapshot)) return 0;
    if (mod_online && snapshot.plan_version < 3) return 0;
    if (mod_online) fi_launch_mods_report(&snapshot);
    return 1;
}
static DWORD WINAPI mod_worker(void *unused) {
    (void)unused;

    fi_eo_settle(10000);
    const char *why = mod_lock_runtime();
    if (why) { mod_refuse(why); return 0; }
    if (GetModuleHandleW(L"infinity_mod.dll")) { mod_refuse("another infinity_mod.dll is already loaded"); return 0; }
    HMODULE library = LoadLibraryExW(mod_runtime, 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!library) { mod_refuse("the runtime does not load"); return 0; }
    InfinityModsStartPlanV1Fn start; FARPROC start_address = GetProcAddress(library, "InfinityModsStartPlanV1");
    memory_copy(&start, &start_address, sizeof start);
    InfinityModsQueryV1Fn query; FARPROC query_address = GetProcAddress(library, "InfinityModsQueryV1");
    memory_copy(&query, &query_address, sizeof query);
    FARPROC query2_address = GetProcAddress(library, "InfinityModsQueryV2");
    memory_copy(&mod_query2, &query2_address, sizeof mod_query2);

    if (!start || !query || (mod_online && !mod_query2)) { mod_refuse("the runtime lacks its entry points"); return 0; }
    if (start(mod_game, mod_plan, mod_plan_hash)) { mod_refuse("the runtime refused the plan"); return 0; }
    if (!mod_report()) { mod_failure(); return 0; }
    ULONGLONG deadline = GetTickCount64() + 70000;
    int ready = 0;
    for (;;) {
        static InfinityModsSnapshotV1 snapshot; snapshot.size = sizeof snapshot; snapshot.abi_version = 1;
        if (query(&snapshot) || snapshot.failed || (!snapshot.ready && GetTickCount64() >= deadline)) { mod_failure(); return 0; }
        if (!mod_report()) { mod_failure(); return 0; }
        if (snapshot.ready && !ready) {
            ready = 1;
            InterlockedExchange(&mod_busy, 0);
            log_append(mod_online ? "MODS: selected native mods ready (launcher session; reported).\r\n" : "MODS: selected native mods ready (offline session).\r\n");
        }
        Sleep(250);
    }
}

void fi_mod_host_start(void *image) {
    DWORD n = GetEnvironmentVariableW(L"FI_MOD_PLAN", mod_plan, PATH_CAPACITY);
    if (!n) return;

    mod_online = fi_cfg()->launch == FI_LAUNCH_PROTOCOL;
    if (n >= PATH_CAPACITY || (fi_cfg()->launch != FI_LAUNCH_OFFLINE && !mod_online)) { mod_refuse("the session is not a launcher session"); return; }
    if (!mod_hash_env(L"FI_MOD_PLAN_HASH", mod_plan_hash) || !mod_hash_env(L"FI_MOD_RUNTIME_HASH", mod_runtime_hash)) { mod_refuse("the launcher's hashes are missing"); return; }
    n = GetEnvironmentVariableW(L"FI_MOD_RUNTIME", mod_runtime, PATH_CAPACITY);
    if (!n || n >= PATH_CAPACITY) { mod_refuse("the runtime path is missing"); return; }
    mod_game = image;
    InterlockedExchange(&mod_busy, 1);
    HANDLE thread = CreateThread(0, 0, mod_worker, 0, 0, 0);
    if (!thread) { mod_refuse("no worker thread"); return; }
    CloseHandle(thread);
}
