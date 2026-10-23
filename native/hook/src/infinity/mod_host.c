/* Infinity native mod host. GPL-3.0-or-later; see LICENSE. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"
#include "../core/sha256.h"
#include "../../../mod/include/infinity_mod_host.h"

#ifdef FI_DEV
static wchar_t mod_plan[PATH_CAPACITY], mod_runtime[PATH_CAPACITY];
static unsigned char mod_plan_hash[32], mod_runtime_hash[32];
static void *mod_game;
static HANDLE mod_runtime_lock = INVALID_HANDLE_VALUE;
static HANDLE mod_parent_locks[256];
static unsigned mod_parent_count;

static int mod_hash_env(const wchar_t *name, unsigned char out[32]) {
    wchar_t text[66];
    if (GetEnvironmentVariableW(name, text, 66) != 64) return 0;
    for (unsigned i = 0; i < 32; ++i) {
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
static int mod_plain(HANDLE h, int directory) {
    BY_HANDLE_FILE_INFORMATION info;
    return h != INVALID_HANDLE_VALUE && GetFileInformationByHandle(h, &info)
        && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)
        && !!(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) == directory;
}
static int mod_lock_runtime(void) {
    static wchar_t absolute[PATH_CAPACITY];
    DWORD n = GetFullPathNameW(mod_runtime, PATH_CAPACITY, absolute, 0);
    if (n < 4 || n >= PATH_CAPACITY || !memory_equal(absolute, mod_runtime, ((size_t)n + 1) * sizeof(wchar_t)) || absolute[1] != L':' || absolute[2] != L'\\') return 0;
    for (DWORD i = 3; i < n; ++i) {
        if (absolute[i] == L':' || absolute[i] == L'/') return 0;
        if (absolute[i] != L'\\') continue;
        if (absolute[i - 1] == L'.' || absolute[i - 1] == L' ' || mod_parent_count >= 256) return 0;
        absolute[i] = 0;
        HANDLE parent = CreateFileW(absolute, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE, 0, OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, 0);
        absolute[i] = L'\\';
        if (!mod_plain(parent, 1)) { if (parent != INVALID_HANDLE_VALUE) CloseHandle(parent); return 0; }
        mod_parent_locks[mod_parent_count++] = parent;
    }
    if (absolute[n - 1] == L'.' || absolute[n - 1] == L' ') return 0;
    mod_runtime_lock = CreateFileW(mod_runtime, GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, 0);
    if (!mod_plain(mod_runtime_lock, 0)) return 0;
    LARGE_INTEGER size;
    if (!GetFileSizeEx(mod_runtime_lock, &size) || size.QuadPart <= 0 || size.QuadPart > 64 * 1024 * 1024) return 0;
    Sha256Context context; static unsigned char buffer[16384]; unsigned char actual[32]; DWORD read;
    sha256_initialize(&context);
    for (;;) {
        if (!ReadFile(mod_runtime_lock, buffer, sizeof buffer, &read, 0)) return 0;
        if (!read) break;
        sha256_update(&context, buffer, read);
    }
    sha256_finalize(&context, actual);
    return memory_equal(actual, mod_runtime_hash, 32);
}
static InfinityModsQueryV2Fn mod_query2;
static int mod_online; /* a launcher session (FI_LAUNCH v1;pipe=): native mods inside a modded lobby (OD-112) */
static void mod_failure(void) {
    if (mod_online && mod_query2) {
        /* the launcher learns that the mods failed before the session ends (N-H2) */
        static InfinityModsSnapshotV2 last; last.size = sizeof last; last.abi_version = 2;
        if (!mod_query2(&last)) { last.failed = 1; fi_launch_mods_report(&last); fi_launch_mods_flush(1000); }
    }
    log_append(mod_online ? "MODS: selected runtime failed; ending the session.\r\n" : "MODS: selected runtime failed; ending the offline session.\r\n");
    ExitProcess(0x46490004);
}
/* V2's snapshot to the launcher: 0 when an online session lacks the plan that binds it (FIMOD003). */
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
    if (!mod_lock_runtime() || GetModuleHandleW(L"infinity_mod.dll")) { mod_failure(); return 0; }
    HMODULE library = LoadLibraryExW(mod_runtime, 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!library) { mod_failure(); return 0; }
    InfinityModsStartPlanV1Fn start; FARPROC start_address = GetProcAddress(library, "InfinityModsStartPlanV1");
    memory_copy(&start, &start_address, sizeof start);
    InfinityModsQueryV1Fn query; FARPROC query_address = GetProcAddress(library, "InfinityModsQueryV1");
    memory_copy(&query, &query_address, sizeof query);
    FARPROC query2_address = GetProcAddress(library, "InfinityModsQueryV2");
    memory_copy(&mod_query2, &query2_address, sizeof mod_query2);
    /* online: only a runtime that reports what it loaded, for this launch's plan */
    if (!start || !query || (mod_online && !mod_query2) || start(mod_game, mod_plan, mod_plan_hash) || !mod_report()) { mod_failure(); return 0; }
    ULONGLONG deadline = GetTickCount64() + 70000;
    int ready = 0;
    for (;;) {
        static InfinityModsSnapshotV1 snapshot; snapshot.size = sizeof snapshot; snapshot.abi_version = 1;
        if (query(&snapshot) || snapshot.failed || (!snapshot.ready && GetTickCount64() >= deadline)) { mod_failure(); return 0; }
        if (!mod_report()) { mod_failure(); return 0; }
        if (snapshot.ready && !ready) {
            ready = 1;
            log_append(mod_online ? "MODS: selected native mods ready (launcher session; reported).\r\n" : "MODS: selected native mods ready (offline session).\r\n");
        }
        Sleep(250);
    }
}
#endif

void fi_mod_host_start(void *image) {
#ifdef FI_DEV
    DWORD n = GetEnvironmentVariableW(L"FI_MOD_PLAN", mod_plan, PATH_CAPACITY);
    if (!n) return;
    /* offline as before; a launcher session too (OD-112), where the runtime must report a FIMOD003 plan */
    mod_online = fi_cfg()->launch == FI_LAUNCH_PROTOCOL;
    if (n >= PATH_CAPACITY || (fi_cfg()->launch != FI_LAUNCH_OFFLINE && !mod_online)
        || !mod_hash_env(L"FI_MOD_PLAN_HASH", mod_plan_hash) || !mod_hash_env(L"FI_MOD_RUNTIME_HASH", mod_runtime_hash)) { mod_failure(); return; }
    n = GetEnvironmentVariableW(L"FI_MOD_RUNTIME", mod_runtime, PATH_CAPACITY);
    if (!n || n >= PATH_CAPACITY) { mod_failure(); return; }
    mod_game = image;
    HANDLE thread = CreateThread(0, 0, mod_worker, 0, 0, 0);
    if (!thread) { mod_failure(); return; }
    CloseHandle(thread);
#else
    (void)image;
#endif
}
