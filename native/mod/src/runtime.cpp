// Copyright (C) 2026 Alexandre Moraes. GPL-3.0-only; see repository LICENSE.
#include "runtime_internal.hpp"
#include <bcrypt.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <cstring>
#include <memory>
#include <mutex>
#include <set>
#include <thread>
#include <vector>
#include "../third_party/sqlite/sqlite3.h"

namespace {
struct FileLock {
    HANDLE file = INVALID_HANDLE_VALUE;
    std::vector<HANDLE> parents;
    ~FileLock() { if (file != INVALID_HANDLE_VALUE) CloseHandle(file); for (auto h : parents) CloseHandle(h); }
};

bool none(const wchar_t* path) { return path && path[0] == L'-' && path[1] == 0; }
struct Entry {
    bool data = false;
    InfinityMod owner;
    InfinityModApiV1 api{};
    InfinityModReportV1 report{};
    std::wstring binary, settings;
    std::unique_ptr<FileLock> binary_lock, settings_lock;
    uint32_t dependencies = 0;
    ULONGLONG deadline = 0;
    bool init_returned = false;
    HMODULE library = nullptr;
};
std::mutex state_mutex;
std::atomic<bool> started{false};
std::atomic<bool> hook_fault{false};
bool failed = false, complete = false;

uint32_t plan_version = 0;
uint8_t plan_nonce[16]{}, plan_digest[32]{};
std::vector<std::unique_ptr<Entry>> entries;
void* game_image = nullptr;
std::filesystem::path azimuth, mount;
std::wstring azimuth_text;
std::unique_ptr<FileLock> session_database_lock;
InfinityModApiV1 hooks{};

bool valid_image(void* image) {
    if (!image) return false;
    __try {
        const auto* dos = static_cast<const IMAGE_DOS_HEADER*>(image);
        if (dos->e_magic != IMAGE_DOS_SIGNATURE || dos->e_lfanew < 64 || dos->e_lfanew > 0x800) return false;
        const auto* nt = reinterpret_cast<const IMAGE_NT_HEADERS64*>(static_cast<const char*>(image) + dos->e_lfanew);
        return nt->Signature == IMAGE_NT_SIGNATURE && nt->FileHeader.Machine == IMAGE_FILE_MACHINE_AMD64
            && nt->FileHeader.TimeDateStamp == 0x565501EDu && nt->OptionalHeader.Magic == IMAGE_NT_OPTIONAL_HDR64_MAGIC
            && nt->OptionalHeader.SizeOfImage == infinity_mod::kGameSizeOfImage;
    } __except (EXCEPTION_EXECUTE_HANDLER) { return false; }
}
bool valid_host() {
    wchar_t path[32768];
    const DWORD n = GetModuleFileNameW(nullptr, path, 32768);
    return n && n < 32768 && _wcsicmp(std::filesystem::path(path).filename().c_str(), L"azimuth.exe") == 0;
}
bool valid_path(const wchar_t* value) {
    if (!value) return false;
    const auto n = wcsnlen_s(value, 32768);
    if (n < 4 || n >= 32768 || !((value[0] >= L'A' && value[0] <= L'Z') || (value[0] >= L'a' && value[0] <= L'z'))
        || value[1] != L':' || value[2] != L'\\') return false;
    const std::filesystem::path path(value);
    if (path.lexically_normal().wstring() != value) return false;
    for (const auto& part : path.relative_path()) {
        auto text = part.wstring();
        if (text.empty() || text.back() == L'.' || text.back() == L' ') return false;
    }
    return std::wstring(value + 3).find_first_of(L":/*?\"<>|") == std::wstring::npos;
}
bool identity(const char* value, bool version) {
    auto n = strnlen_s(value, 65);
    if (!n || n > 64) return false;
    if (!version && (!((value[0] >= 'a' && value[0] <= 'z') || (value[0] >= '0' && value[0] <= '9')) || value[n - 1] == '.')) return false;
    for (size_t i = 0; i < n; ++i) {
        auto c = value[i];
        if (!(c >= 'a' && c <= 'z') && !(c >= '0' && c <= '9') && c != '-'
            && !(!version && (c == '.' || c == '_'))
            && !(version && (c == '.' || c == '+' || (c >= 'A' && c <= 'Z')))) return false;
        if (!version && c == '.' && i + 1 < n && value[i + 1] == '.') return false;
    }
    return true;
}
bool nonzero(const uint8_t* hash) { uint8_t any = 0; for (int i=0;i<32;++i) any |= hash[i]; return any != 0; }
bool plain(HANDLE handle, bool directory) {
    BY_HANDLE_FILE_INFORMATION info{};
    return GetFileInformationByHandle(handle, &info)
        && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)
        && ((info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0) == directory;
}
std::unique_ptr<FileLock> verify_file(const std::wstring& path, const uint8_t expected[32], DWORD share = FILE_SHARE_READ) {
    if (!valid_path(path.c_str())) return {};
    auto lock = std::make_unique<FileLock>();
    auto parent = std::filesystem::path(path).parent_path();
    for (;;) {
        auto h = CreateFileW(parent.c_str(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE,
            nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
        if (h == INVALID_HANDLE_VALUE) return {};
        lock->parents.push_back(h);
        if (!plain(h, true)) return {};
        if (parent == parent.root_path()) break;
        parent = parent.parent_path();
    }
    lock->file = CreateFileW(path.c_str(), GENERIC_READ, share, nullptr, OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN, nullptr);
    LARGE_INTEGER length{};
    if (lock->file == INVALID_HANDLE_VALUE || !plain(lock->file, false) || !GetFileSizeEx(lock->file, &length)
        || length.QuadPart < 0 || length.QuadPart > 64 * 1024 * 1024) return {};
    BCRYPT_ALG_HANDLE algorithm{};
    BCRYPT_HASH_HANDLE hash{};
    if (BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0) < 0) return {};
    bool ok = BCryptCreateHash(algorithm, &hash, nullptr, 0, nullptr, 0, 0) >= 0;
    uint8_t buffer[65536], actual[32]{};
    DWORD count = 0;
    while (ok) {
        if (!ReadFile(lock->file, buffer, sizeof buffer, &count, nullptr)) { ok = false; break; }
        if (!count) break;
        ok = BCryptHashData(hash, buffer, count, 0) >= 0;
    }
    if (ok) ok = BCryptFinishHash(hash, actual, sizeof actual, 0) >= 0 && std::memcmp(actual, expected, 32) == 0;
    if (hash) BCryptDestroyHash(hash);
    BCryptCloseAlgorithmProvider(algorithm, 0);
    return ok ? std::move(lock) : nullptr;
}
Entry* owned(InfinityMod* mod) {
    for (auto& entry : entries) if (&entry->owner == mod) return entry.get();
    return nullptr;
}
bool active(InfinityMod* mod) {
    auto e = owned(mod);
    return !failed && !hook_fault && e && (e->init_returned || GetTickCount64() < e->deadline) && (e->report.state == INFINITY_MOD_READY
        || (e->report.state == INFINITY_MOD_INITIALIZING && GetTickCount64() < e->deadline));
}
void fail_locked() {
    failed = true;
    complete = false;
    for (auto& e : entries) {
        e->report.state = INFINITY_MOD_FAILED;
        if (hooks.disable_hooks) hooks.disable_hooks(&e->owner, nullptr);
    }
}
void report(InfinityMod* mod, uint32_t state) {
    std::lock_guard lock(state_mutex);
    auto e = owned(mod);
    if (!e || failed) return;
    if (state == INFINITY_MOD_FAILED) fail_locked();
    else if (state == INFINITY_MOD_READY && e->report.state == INFINITY_MOD_INITIALIZING)
    {
        if (GetTickCount64() >= e->deadline) fail_locked();
        else e->report.state = INFINITY_MOD_READY;
    }
}
void fill_api(Entry& e) {
    auto& api = e.api;
    api.size = sizeof api; api.abi_version = INFINITY_MOD_ABI_V1; api.mod = &e.owner;
    api.azimuth_dir = azimuth_text.c_str(); api.settings_path = e.settings.c_str(); api.report_state = report;
    api.log = [](InfinityMod* m, const char* text) {
        std::lock_guard lock(state_mutex);
        if (owned(m) && text) infinity_mod::log_line(m->name, std::string(text, strnlen_s(text, 1024)));
    };
    api.hook_function = [](InfinityMod* m, const char* n, void* t, void* d, void** o) -> InfinityHook* {
        std::lock_guard lock(state_mutex); return active(m) ? hooks.hook_function(m,n,t,d,o) : nullptr;
    };
    api.hook_call = [](InfinityMod* m, const char* n, void* t, void* d, void** o) -> InfinityHook* {
        std::lock_guard lock(state_mutex); return active(m) ? hooks.hook_call(m,n,t,d,o) : nullptr;
    };
    api.hook_pointer = [](InfinityMod* m, const char* n, void** t, void* d, void** o) -> InfinityHook* {
        std::lock_guard lock(state_mutex); return active(m) ? hooks.hook_pointer(m,n,t,d,o) : nullptr;
    };
    api.patch_bytes = [](InfinityMod* m, const char* n, void* t, const void* a, const void* b, uint32_t s) -> InfinityHook* {
        std::lock_guard lock(state_mutex); return active(m) ? hooks.patch_bytes(m,n,t,a,b,s) : nullptr;
    };
    api.hook_instruction = [](InfinityMod* m, const char* n, void* t, InfinityInstructionFn f, void* u) -> InfinityHook* {
        std::lock_guard lock(state_mutex); return active(m) ? hooks.hook_instruction(m,n,t,f,u) : nullptr;
    };
    api.enable_hooks = [](InfinityMod* m, InfinityHook* h) -> int {
        std::lock_guard lock(state_mutex); return active(m) ? hooks.enable_hooks(m,h) : 0;
    };
    api.disable_hooks = [](InfinityMod* m, InfinityHook* h) -> int {
        std::lock_guard lock(state_mutex); return owned(m) ? hooks.disable_hooks(m,h) : 0;
    };
    api.wait_for_code = hooks.wait_for_code;
}
int guarded_initialize(InfinityModInitializeV1Fn init, const InfinityModApiV1* api) {
    __try { return init(api); } __except (EXCEPTION_EXECUTE_HANDLER) { return 1; }
}
void load_selected(uint32_t timeout_ms = 65000) {
    try {
        infinity_mod::fill_hook_api(hooks);
        for (size_t i = 0; i < entries.size(); ++i) {
            auto& e = *entries[i];
            {
                std::lock_guard lock(state_mutex);
                if (hook_fault && !failed) fail_locked();
                if (failed) return;
                for (size_t j = 0; j < i; ++j)
                    if ((e.dependencies & (1u << j)) && entries[j]->report.state != INFINITY_MOD_READY) { fail_locked(); return; }
                if (e.data) { e.init_returned = true; e.report.state = INFINITY_MOD_READY; continue; }
                fill_api(e);
                e.deadline = GetTickCount64() + timeout_ms;
                e.report.state = INFINITY_MOD_INITIALIZING;
            }
            if (GetModuleHandleW(e.binary.c_str()) || GetModuleHandleW(std::filesystem::path(e.binary).filename().c_str()))
                throw std::runtime_error("preloaded mod");
            e.library = LoadLibraryExW(e.binary.c_str(), nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
            if (!e.library) throw std::runtime_error("load failed");
            {
                std::lock_guard lock(state_mutex);
                if (hook_fault && !failed) fail_locked();
                if (failed) return;
            }
            auto init = reinterpret_cast<InfinityModInitializeV1Fn>(GetProcAddress(e.library, "InfinityModInitializeV1"));
            if (!init || guarded_initialize(init, &e.api) != 0) throw std::runtime_error("initialization failed");
            {
                std::lock_guard lock(state_mutex);
                e.init_returned = true;
                if (GetTickCount64() >= e.deadline && !failed) fail_locked();
            }
            for (;;) {
                {
                    std::lock_guard lock(state_mutex);
                    if (failed) return;
                    if (hook_fault) { fail_locked(); return; }
                    if (e.report.state == INFINITY_MOD_READY) break;
                    if (GetTickCount64() >= e.deadline) { fail_locked(); return; }
                }
                Sleep(20);
            }
        }
        std::lock_guard lock(state_mutex);
        if (hook_fault && !failed) fail_locked();
        if (!failed) complete = true;
    } catch (...) { std::lock_guard lock(state_mutex); fail_locked(); }
}
bool prepare(const InfinityModsLaunchV1& launch) {
    std::vector<std::unique_ptr<Entry>> pending;
    for (uint32_t i=0;i<launch.count;++i) {
        const auto& item = launch.mods[i];
        const bool data = none(item.binary_path), no_settings = none(item.settings_path);
        if (!identity(item.id, false) || !identity(item.version, true)
            || (data ? nonzero(item.binary_sha256) : !valid_path(item.binary_path) || !nonzero(item.binary_sha256))
            || (no_settings ? nonzero(item.settings_sha256) || !data : !valid_path(item.settings_path) || !nonzero(item.settings_sha256))
            || !nonzero(item.compatibility_sha256) || (item.dependency_mask & ~((1u << i) - 1))) return false;
        for (auto& previous : pending)
            if (previous->owner.name == item.id || (!data && !previous->data && (!_wcsicmp(previous->binary.c_str(), item.binary_path)
                || !_wcsicmp(std::filesystem::path(previous->binary).filename().c_str(), std::filesystem::path(item.binary_path).filename().c_str())))) return false;
        auto e = std::make_unique<Entry>();
        e->data = data;
        e->owner.name = item.id; e->binary = item.binary_path; e->settings = item.settings_path;
        e->dependencies = item.dependency_mask;
        if (!data && !(e->binary_lock = verify_file(e->binary, item.binary_sha256))) return false;
        if (!no_settings && !(e->settings_lock = verify_file(e->settings, item.settings_sha256))) return false;
        std::memcpy(e->report.id, item.id, sizeof item.id); std::memcpy(e->report.version, item.version, sizeof item.version);
        std::memcpy(e->report.binary_sha256, item.binary_sha256, 32);
        std::memcpy(e->report.settings_sha256, item.settings_sha256, 32);
        std::memcpy(e->report.compatibility_sha256, item.compatibility_sha256, 32);
        pending.push_back(std::move(e));
    }
    azimuth = launch.azimuth_directory; mount = launch.game_directory;
    azimuth_text = azimuth.wstring(); game_image = launch.game_image;
    entries = std::move(pending);
    return true;
}
}

namespace infinity_mod {
void notify_hook_fault() { hook_fault.store(true); }
void* find_game() { return game_image; }
std::filesystem::path azimuth_dir() { return azimuth; }
std::filesystem::path game_mount() { return mount; }
std::string narrow(const std::wstring& value) {
    if (value.empty()) return {};
    int count = WideCharToMultiByte(CP_UTF8, 0, value.data(), static_cast<int>(value.size()), nullptr, 0, nullptr, nullptr);
    if (count <= 0) return {};
    std::string out(count, '\0');
    WideCharToMultiByte(CP_UTF8, 0, value.data(), static_cast<int>(value.size()), out.data(), count, nullptr, nullptr);
    return out;
}
void log_line(const std::string& who, const std::string& text) {
    auto line = "[infinity_mod:" + who.substr(0,64) + "] " + text.substr(0,1024) + "\n";
    OutputDebugStringA(line.c_str());
}
}

extern "C" __declspec(dllexport) int InfinityModsStartV1(const InfinityModsLaunchV1* launch) {
    if (!launch || launch->size != sizeof *launch || launch->abi_version != 1 || launch->count > INFINITY_MOD_MAX_COUNT
        || (launch->count && !launch->mods) || !valid_path(launch->azimuth_directory) || !valid_path(launch->game_directory)
        || !valid_host() || !valid_image(launch->game_image)) return 1;
    std::lock_guard lock(state_mutex);
    if (started.exchange(true)) return 2;
    try {
        if (!prepare(*launch)) { failed = true; return 1; }
        HMODULE pinned = nullptr;
        if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            reinterpret_cast<LPCWSTR>(&InfinityModsStartV1), &pinned)) { failed = true; return 3; }
        std::thread([] { load_selected(); }).detach();
        return 0;
    } catch (...) { failed = true; return 3; }
}
namespace {
template <typename Snapshot> void snapshot_locked(Snapshot& result) {
    if (hook_fault && !failed) fail_locked();
    if (!failed)
        for (const auto& e : entries)
            if ((e->report.state == INFINITY_MOD_INITIALIZING || (e->report.state == INFINITY_MOD_READY && !e->init_returned))
                && GetTickCount64() >= e->deadline) { fail_locked(); break; }
    result.count = static_cast<uint32_t>(entries.size()); result.failed = failed;
    result.ready = started && complete && !failed;
    for (size_t i=0;i<entries.size();++i) result.mods[i] = entries[i]->report;
}
}
extern "C" __declspec(dllexport) int InfinityModsQueryV1(InfinityModsSnapshotV1* result) {
    if (!result || result->size != sizeof *result || result->abi_version != 1) return 1;
    std::lock_guard lock(state_mutex);
    *result = {}; result->size = sizeof *result; result->abi_version = 1;
    snapshot_locked(*result);
    return 0;
}

extern "C" __declspec(dllexport) int InfinityModsQueryV2(InfinityModsSnapshotV2* result) {
    if (!result || result->size != sizeof *result || result->abi_version != 2) return 1;
    std::lock_guard lock(state_mutex);
    *result = {}; result->size = sizeof *result; result->abi_version = 2;
    result->plan_version = plan_version;
    std::memcpy(result->nonce, plan_nonce, sizeof plan_nonce); std::memcpy(result->plan_sha256, plan_digest, sizeof plan_digest);
    snapshot_locked(*result);
    return 0;
}

namespace {
struct PlanReader {
    const std::vector<uint8_t>& bytes;
    size_t at = 0;
    void read(void* out, size_t n) {
        if (at > bytes.size() || n > bytes.size() - at) throw std::runtime_error("truncated plan");
        std::memcpy(out, bytes.data() + at, n); at += n;
    }
    uint32_t number() { uint32_t n; read(&n, sizeof n); return n; }
    std::wstring text() {
        auto n = number(); if (!n || n > 32767) throw std::runtime_error("invalid text length");
        std::wstring value(n, L'\0'); read(value.data(), size_t(n) * 2);
        if (value.find(L'\0') != std::wstring::npos) throw std::runtime_error("embedded null");
        return value;
    }
};
struct ParsedPlan {
    std::wstring azimuth, game, database;
    std::array<std::wstring, INFINITY_MOD_MAX_COUNT> binary, settings;
    std::array<InfinityModSelectionV1, INFINITY_MOD_MAX_COUNT> mods{};
    std::array<std::vector<std::string>, INFINITY_MOD_MAX_COUNT> tables;
    std::array<std::array<uint8_t, 32>, INFINITY_MOD_MAX_COUNT> sql{};
    uint8_t baseline_hash[32]{}, prepared_hash[32]{}, sql_hash[32]{};
    std::vector<infinity_mod::Readiness> readiness;
    uint32_t count = 0;
    uint32_t version = 0;
    uint8_t nonce[16]{};
};
void parse_plan(const std::vector<uint8_t>& bytes, ParsedPlan& plan) {
    PlanReader reader{bytes}; char magic[8]; reader.read(magic, 8);
    if (!std::memcmp(magic, "FIMOD003", 8) || !std::memcmp(magic, "FIMOD004", 8)) {
        plan.version = magic[7] == '4' ? 4 : 3; reader.read(plan.nonce, 16);
        uint8_t any = 0; for (auto b : plan.nonce) any |= b;
        if (!any) throw std::runtime_error("missing launch nonce");
    } else if (!std::memcmp(magic, "FIMOD002", 8)) {
        plan.version = 2;
    } else {
        throw std::runtime_error("unknown plan format");
    }
    plan.count = reader.number();
    if (!plan.count || plan.count > INFINITY_MOD_MAX_COUNT) throw std::runtime_error("invalid selection count");
    plan.azimuth = reader.text(); plan.game = reader.text();
    auto database_count = reader.number(); if (database_count > 1) throw std::runtime_error("unsupported database count");
    if (database_count) { plan.database = reader.text(); auto prefix = plan.game + L"\\"; if (!valid_path(plan.database.c_str()) || _wcsnicmp(plan.database.c_str(), prefix.c_str(), prefix.size())) throw std::runtime_error("database outside game folder"); }
    reader.read(plan.baseline_hash, 32); reader.read(plan.prepared_hash, 32); reader.read(plan.sql_hash, 32);
    auto requirements = reader.number(); if (requirements > 32) throw std::runtime_error("too many readiness checks");
    for (uint32_t i = 0; i < requirements; ++i) {
        auto module = reader.text(), log = reader.text(), success = reader.text();
        auto prefix = plan.azimuth + L"\\";
        if (!valid_path(module.c_str()) || !valid_path(log.c_str()) || success.size() > 512
            || _wcsnicmp(module.c_str(), prefix.c_str(), prefix.size()) || _wcsnicmp(log.c_str(), prefix.c_str(), prefix.size())) throw std::runtime_error("invalid readiness path");
        plan.readiness.push_back({module, log, infinity_mod::narrow(success)});
    }
    for (uint32_t i = 0; i < plan.count; ++i) {
        auto id = reader.text(), version = reader.text();
        if (id.size() > 64 || version.size() > 64) throw std::runtime_error("invalid identity");
        for (size_t j = 0; j < id.size(); ++j) { if (id[j] > 127) throw std::runtime_error("invalid id"); plan.mods[i].id[j] = char(id[j]); }
        for (size_t j = 0; j < version.size(); ++j) { if (version[j] > 127) throw std::runtime_error("invalid version"); plan.mods[i].version[j] = char(version[j]); }
        plan.binary[i] = reader.text(); plan.settings[i] = reader.text();
        plan.mods[i].binary_path = plan.binary[i].c_str(); plan.mods[i].settings_path = plan.settings[i].c_str();
        reader.read(plan.mods[i].binary_sha256, 32); reader.read(plan.mods[i].settings_sha256, 32);
        reader.read(plan.mods[i].compatibility_sha256, 32); plan.mods[i].dependency_mask = reader.number();
        auto count = reader.number(); if (count > (none(plan.binary[i].c_str()) ? 64u : 32u)) throw std::runtime_error("too many tables");
        for (uint32_t j = 0; j < count; ++j) {
            auto table = infinity_mod::narrow(reader.text());
            if (table.empty() || table.size() > 64 || table.find_first_not_of("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_") != std::string::npos
                || !_strnicmp(table.c_str(), "sqlite_", 7) || !_strnicmp(table.c_str(), "_infinity", 9)) throw std::runtime_error("invalid table scope");
            plan.tables[i].push_back(table);
        }
        if (plan.version < 4) continue;
        reader.read(plan.sql[i].data(), 32);
        const bool data = none(plan.binary[i].c_str()), sql = nonzero(plan.sql[i].data());
        if (sql && (!data || plan.tables[i].empty() || database_count != 1)) throw std::runtime_error("invalid data SQL");
        if (data && !plan.tables[i].empty() && !sql) throw std::runtime_error("data tables without SQL");
        for (uint32_t j = 0; j < i; ++j) {
            if (plan.mods[i].dependency_mask & (1u << j)) continue;
            for (auto& a : plan.tables[i]) for (auto& b : plan.tables[j])
                if (!_stricmp(a.c_str(), b.c_str())) throw std::runtime_error("table overlap without dependency");
        }
    }
    if (reader.at != bytes.size()) throw std::runtime_error("trailing plan data");
}
}
#include "database_prepare.inl"

extern "C" __declspec(dllexport) int InfinityModsStartPlanV1(void* image, const wchar_t* path, const uint8_t hash[32]) {
    try {
        if (!hash || !valid_path(path) || !valid_host() || !valid_image(image)) return 1;

        wchar_t mode[48]{};
        const DWORD mode_length = GetEnvironmentVariableW(L"FI_LAUNCH", mode, 48);
        bool online = false;
        if (mode_length == 10 && !wcscmp(mode, L"v1;offline")) {
            online = false;
        } else if (mode_length == 40 && !wcsncmp(mode, L"v1;pipe=", 8)
            && std::all_of(mode + 8, mode + 40, [](wchar_t c) { return (c >= L'0' && c <= L'9') || (c >= L'a' && c <= L'f'); })) {
            online = true;
        } else {
            return 1;
        }
        auto locked = verify_file(path, hash); if (!locked) return 1;
        LARGE_INTEGER size{};
        if (!GetFileSizeEx(locked->file, &size) || size.QuadPart < 12 || size.QuadPart > 1024 * 1024) return 1;
        LARGE_INTEGER zero{}; if (!SetFilePointerEx(locked->file, zero, nullptr, FILE_BEGIN)) return 1;
        std::vector<uint8_t> bytes(size_t(size.QuadPart)); DWORD read = 0;
        if (!ReadFile(locked->file, bytes.data(), DWORD(bytes.size()), &read, nullptr) || read != bytes.size()) return 1;
        ParsedPlan plan; parse_plan(bytes, plan);

        if (online && plan.version < 3) return 1;
        {
            std::lock_guard lock(state_mutex);
            if (started) return 2;
            plan_version = plan.version;
            std::memcpy(plan_nonce, plan.nonce, sizeof plan_nonce); std::memcpy(plan_digest, hash, sizeof plan_digest);
        }
        wchar_t exe[32768]; auto length = GetModuleFileNameW(nullptr, exe, 32768);
        if (!length || length >= 32768 || _wcsicmp(std::filesystem::path(exe).parent_path().c_str(), plan.azimuth.c_str())) return 1;
        auto prefix = plan.azimuth + L"\\";
        for (uint32_t i = 0; i < plan.count; ++i)
            if ((!none(plan.binary[i].c_str()) && _wcsnicmp(plan.binary[i].c_str(), prefix.c_str(), prefix.size()))
                || (!none(plan.settings[i].c_str()) && _wcsnicmp(plan.settings[i].c_str(), prefix.c_str(), prefix.size()))) return 1;
        if (!plan.database.empty()) {
            if (!nonzero(plan.prepared_hash) || !nonzero(plan.sql_hash)) return 1;
            for (auto suffix : { L"-wal", L"-shm", L"-journal" }) if (std::filesystem::exists(plan.database + suffix)) return 1;

            session_database_lock = verify_file(plan.database, plan.prepared_hash, FILE_SHARE_READ | FILE_SHARE_WRITE);
            if (!session_database_lock) return 1;
        }
        infinity_mod::configure_readiness(std::move(plan.readiness));
        InfinityModsLaunchV1 launch{sizeof launch, 1, image, plan.azimuth.c_str(), plan.game.c_str(), plan.count, plan.mods.data()};
        return InfinityModsStartV1(&launch);
    } catch (...) { return 1; }
}
BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) DisableThreadLibraryCalls(instance);
    return TRUE;
}
