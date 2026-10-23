// Adapted from Trinculo54/fh2-mod-platform 6e851d89 hooks.cpp; MIT, see licenses/FH2-hooks-MIT.txt.
// Hooks: one place where every mod changes game code (API 3).
//
// Three kinds, all ending up in the same chain:
//   function  an inline detour at the start of a function, made with MinHook
//   call      a CALL rel32 (or JMP rel32) at one call site, retargeted
//   pointer   a function pointer in memory (a vtable slot, a callback table)
//
// Every patched place is a Site. The game is sent to the Site's entry stub,
// a "jmp [slot]" whose slot holds the newest enabled hook's detour. Each
// hook's `original` is a stub of its own, pointing at the next older hook's
// detour, and the oldest one's at the real code (MinHook's trampoline, the
// call's old target, or the pointer's old value). So several mods can hook
// the same function, and any of them can come and go in any order: only
// 8-byte slots change, atomically, and nothing a running thread may be in
// is ever freed.
//
// Before anything is written, the bytes are checked: against every other
// hook's (no overlaps), and for game code against the exe on disk, so code
// that a dependency (or a mod patching the game by itself) has already
// changed is never hooked over. Hooks in game code also wait for Extra
// Options to finish patching (wait_for_code): it validates all of its sites
// before writing any, and refuses every patch if one doesn't match.
#include "runtime_internal.hpp"


#include <windows.h>
#include <tlhelp32.h>

#include "MinHook.h"
#include "hde/hde64.h"

#include <algorithm>
#include <cstddef>
#include <cstdlib>
#include <atomic>
#include <cstdio>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

struct InfinityHook;

namespace infinity_mod {
namespace {

namespace fs = std::filesystem;

enum Kind { kFunction, kCall, kPointer, kPatch, kInstruction };
const char* const kKindName[] = {"function", "call", "pointer", "patch", "instruction"};

std::mutex g_mu;

// ---- small helpers ---------------------------------------------------------------

bool query(const void* p, MEMORY_BASIC_INFORMATION& mbi) {
    return VirtualQuery(p, &mbi, sizeof mbi) == sizeof mbi && mbi.State == MEM_COMMIT &&
           !(mbi.Protect & (PAGE_GUARD | PAGE_NOACCESS));
}

bool readable(const void* p, std::size_t n) {
    MEMORY_BASIC_INFORMATION mbi{};
    if (!query(p, mbi)) return false;
    return static_cast<const char*>(p) + n <= static_cast<const char*>(mbi.BaseAddress) + mbi.RegionSize;
}

bool executable(const void* p) {
    MEMORY_BASIC_INFORMATION mbi{};
    return query(p, mbi) &&
           (mbi.Protect & (PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY));
}

const char* game_base() { return static_cast<const char*>(find_game()); }

bool in_game(const void* p) {
    const char* g = game_base();
    const char* c = static_cast<const char*>(p);
    return g && c >= g && c < g + kGameSizeOfImage;
}

std::string hex(uintptr_t v) {
    char b[24];
    std::snprintf(b, sizeof b, "0x%llX", static_cast<unsigned long long>(v));
    return b;
}

// "game+0xA68870", "WheresMyBuddy.dll+0x1234" or "0x7FF6...".
std::string where(const void* p) {
    if (!p) return "null";
    if (in_game(p)) return "game+" + hex(static_cast<const char*>(p) - game_base());
    HMODULE m = nullptr;
    if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           static_cast<LPCWSTR>(p), &m)) {
        wchar_t path[MAX_PATH];
        const DWORD n = GetModuleFileNameW(m, path, MAX_PATH);
        if (n && n < MAX_PATH)
            return narrow(fs::path{path}.filename().wstring()) + "+" +
                   hex(static_cast<const char*>(p) - reinterpret_cast<const char*>(m));
    }
    return hex(reinterpret_cast<uintptr_t>(p));
}

// ---- stubs -------------------------------------------------------------------------
//
// A block is two pages: code (read/execute) with 512 stubs of
// "jmp [rip+0xFFA]; int3; int3", each reading the qword at the same index
// in the data page (read/write) right after it.

struct Stub {
    void* code = nullptr;
    void* volatile* slot = nullptr;
};

constexpr std::size_t kPage = 0x1000;
constexpr int kStubsPerBlock = kPage / 8;
constexpr int kMaxBlocks     = 64;

struct Block {
    char* code;
    int used;
};
Block g_blocks[kMaxBlocks];
int g_nblocks = 0;

// Free memory within +-1 GB of `close_to` (anywhere if null), so a rel32 from
// there reaches it.
char* alloc_near(const void* close_to, std::size_t size) {
    if (!close_to)
        return static_cast<char*>(VirtualAlloc(nullptr, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE));
    SYSTEM_INFO si{};
    GetSystemInfo(&si);
    const uintptr_t gran   = si.dwAllocationGranularity;
    const uintptr_t origin = reinterpret_cast<uintptr_t>(close_to);
    const uintptr_t range  = 0x40000000;
    const uintptr_t lo     = std::max<uintptr_t>(origin > range ? origin - range : 0,
                                                 reinterpret_cast<uintptr_t>(si.lpMinimumApplicationAddress));
    const uintptr_t hi     = std::min<uintptr_t>(origin + range,
                                                 reinterpret_cast<uintptr_t>(si.lpMaximumApplicationAddress));
    MEMORY_BASIC_INFORMATION mbi{};
    // Upwards first (past the game image is usually free), then down.
    for (uintptr_t a = (origin + gran - 1) / gran * gran; a + size < hi;) {
        if (VirtualQuery(reinterpret_cast<void*>(a), &mbi, sizeof mbi) != sizeof mbi) break;
        if (mbi.State == MEM_FREE)
            if (void* p = VirtualAlloc(reinterpret_cast<void*>(a), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE))
                return static_cast<char*>(p);
        const uintptr_t next = reinterpret_cast<uintptr_t>(mbi.BaseAddress) + mbi.RegionSize;
        a = std::max(a + gran, (next + gran - 1) / gran * gran);
    }
    for (uintptr_t a = origin / gran * gran; a > lo + gran;) {
        a -= gran;
        if (VirtualQuery(reinterpret_cast<void*>(a), &mbi, sizeof mbi) != sizeof mbi) break;
        if (mbi.State == MEM_FREE) {
            if (void* p = VirtualAlloc(reinterpret_cast<void*>(a), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE))
                return static_cast<char*>(p);
        } else if (mbi.AllocationBase) {
            a = reinterpret_cast<uintptr_t>(mbi.AllocationBase) / gran * gran;
        }
    }
    return nullptr;
}

bool reaches(const void* from, const void* to) {
    const auto d = reinterpret_cast<intptr_t>(to) - reinterpret_cast<intptr_t>(from);
    return d > -0x7FFF0000LL && d < 0x7FFF0000LL;
}

Stub new_stub(const void* close_to) {
    Block* b = nullptr;
    for (int i = 0; i < g_nblocks && !b; ++i)
        if (g_blocks[i].used < kStubsPerBlock && (!close_to || reaches(close_to, g_blocks[i].code))) b = &g_blocks[i];
    if (!b) {
        if (g_nblocks == kMaxBlocks) return {};
        char* mem = alloc_near(close_to, 2 * kPage);
        if (!mem) return {};
        static const unsigned char kStub[8] = {0xFF, 0x25, 0xFA, 0x0F, 0x00, 0x00, 0xCC, 0xCC};
        for (int i = 0; i < kStubsPerBlock; ++i) std::memcpy(mem + 8 * i, kStub, 8);
        DWORD old = 0;
        VirtualProtect(mem, kPage, PAGE_EXECUTE_READ, &old);
        FlushInstructionCache(GetCurrentProcess(), mem, kPage);
        b  = &g_blocks[g_nblocks];
        *b = {mem, 0};
        ++g_nblocks; // published after the block is ready, for crash reports
    }
    const int i = b->used++;
    return {b->code + 8 * i, reinterpret_cast<void* volatile*>(b->code + kPage + 8 * i)};
}

void set_slot(void* volatile* slot, void* v) { InterlockedExchangePointer(const_cast<void**>(slot), v); }

// ---- the game exe on disk ----------------------------------------------------------

struct DiskImage {
    bool tried = false;
    HANDLE file = INVALID_HANDLE_VALUE;
    uint64_t image_base = 0;
    std::vector<IMAGE_SECTION_HEADER> sections;
} g_disk;

void open_disk_image() {
    if (g_disk.tried) return;
    g_disk.tried = true;
    std::vector<fs::path> candidates;
    if (!game_mount().empty()) candidates.push_back(game_mount() / L"forza_durango_release_final.exe");
    candidates.push_back(azimuth_dir() / L"forza_durango_release_final.exe"); // Mezcal
    for (const auto& c : candidates) {
        HANDLE f = CreateFileW(c.c_str(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
                               FILE_ATTRIBUTE_NORMAL, nullptr);
        if (f == INVALID_HANDLE_VALUE) continue;
        char head[0x1000];
        DWORD got = 0;
        bool ok   = ReadFile(f, head, sizeof head, &got, nullptr) && got == sizeof head;
        const auto* dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(head);
        const auto* nt  = ok && dos->e_magic == IMAGE_DOS_SIGNATURE && dos->e_lfanew > 0 && dos->e_lfanew < 0x800
                              ? reinterpret_cast<const IMAGE_NT_HEADERS64*>(head + dos->e_lfanew)
                              : nullptr;
        ok = nt && nt->Signature == IMAGE_NT_SIGNATURE && nt->OptionalHeader.SizeOfImage == kGameSizeOfImage;
        if (ok) {
            const auto* sec = IMAGE_FIRST_SECTION(nt);
            const unsigned n = nt->FileHeader.NumberOfSections;
            ok = reinterpret_cast<const char*>(sec + n) <= head + sizeof head;
            if (ok) {
                g_disk.sections.assign(sec, sec + n);
                g_disk.image_base = nt->OptionalHeader.ImageBase;
                g_disk.file       = f;
                plog("hooks: checking game code against " + narrow(c.wstring()));
                return;
            }
        }
        CloseHandle(f);
    }
    plog("hooks: couldn't read the game exe from the game folder, so hooks can't check whether game code "
         "was already changed outside the platform");
}

// Every page of the exe's executable sections is executable in memory. True when the sections
// aren't known (the exe couldn't be read): there's nothing to check against.
bool code_sections_executable(std::string* first_bad) {
    const char* base = game_base();
    if (!base || g_disk.sections.empty()) return true;
    for (const auto& sec : g_disk.sections) {
        if (!(sec.Characteristics & IMAGE_SCN_MEM_EXECUTE)) continue;
        const char* p   = base + sec.VirtualAddress;
        const char* end = p + sec.Misc.VirtualSize;
        while (p < end) {
            MEMORY_BASIC_INFORMATION mbi{};
            if (VirtualQuery(p, &mbi, sizeof mbi) != sizeof mbi) return false;
            const bool exec = mbi.State == MEM_COMMIT &&
                              (mbi.Protect & (PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY));
            if (!exec) {
                if (first_bad) *first_bad = where(p) + " isn't executable yet";
                return false;
            }
            p = static_cast<const char*>(mbi.BaseAddress) + mbi.RegionSize;
        }
    }
    return true;
}

// 1 if the game's bytes at [p, p+n) are as the exe on disk has them, 0 if
// not, -1 if that can't be told. `pointer` compares one 8-byte pointer,
// allowing for the image being mapped somewhere other than its preferred
// base.
int matches_disk(const void* p, std::size_t n, bool pointer) {
    open_disk_image();
    if (g_disk.file == INVALID_HANDLE_VALUE) return -1;
    const char* base = game_base();
    const auto rva   = static_cast<uint32_t>(static_cast<const char*>(p) - base);
    for (const auto& s : g_disk.sections) {
        if (rva < s.VirtualAddress || rva + n > s.VirtualAddress + std::max(s.Misc.VirtualSize, s.SizeOfRawData))
            continue;
        if (rva + n > s.VirtualAddress + s.SizeOfRawData) return -1; // not in the file (zero-filled)
        unsigned char disk[32];
        if (n > sizeof disk) return -1;
        OVERLAPPED ov{};
        const uint64_t off = s.PointerToRawData + (rva - s.VirtualAddress);
        ov.Offset          = static_cast<DWORD>(off);
        ov.OffsetHigh      = static_cast<DWORD>(off >> 32);
        DWORD got          = 0;
        if (!ReadFile(g_disk.file, disk, static_cast<DWORD>(n), &got, &ov) || got != n) return -1;
        if (pointer) {
            // Only a pointer that's an address in the image on disk (a vtable
            // slot, a callback table) can be compared. Imports hold name
            // RVAs on disk and are filled in by the loader, and many tables
            // are zero until the game fills them.
            uint64_t want = 0, have = 0;
            std::memcpy(&want, disk, 8);
            std::memcpy(&have, p, 8);
            if (want < g_disk.image_base || want >= g_disk.image_base + kGameSizeOfImage) return -1;
            return have == want + (reinterpret_cast<uint64_t>(base) - g_disk.image_base) ? 1 : 0;
        }
        if (reinterpret_cast<uint64_t>(base) != g_disk.image_base) return -1; // relocated code differs anyway
        return std::memcmp(disk, p, n) == 0 ? 1 : 0;
    }
    return -1;
}

// ---- manifest-declared dependency readiness -----------------------------------------
std::atomic<bool> g_code_ready{false};
std::vector<Readiness> g_readiness;
bool code_ready() { return g_code_ready.load(std::memory_order_acquire); }
bool dependencies_ready() {
    FILETIME started{}, exited{}, kernel{}, user{};
    if (!GetProcessTimes(GetCurrentProcess(), &started, &exited, &kernel, &user)) return false;
    for (const auto& requirement : g_readiness) {
        auto module = GetModuleHandleW(requirement.module.c_str());
        if (!module) return false;
        wchar_t module_path[32768]; auto n = GetModuleFileNameW(module, module_path, 32768);
        if (!n || n >= 32768 || _wcsicmp(module_path, requirement.module.c_str())) return false;
        HANDLE f = CreateFileW(requirement.log.c_str(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            nullptr, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
        if (f == INVALID_HANDLE_VALUE) return false;
        BY_HANDLE_FILE_INFORMATION info{}; FILETIME written{}; std::string text(65536, '\0'); DWORD got = 0;
        bool ok = GetFileInformationByHandle(f, &info) && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)
            && GetFileTime(f, nullptr, nullptr, &written) && CompareFileTime(&written, &started) >= 0
            && ReadFile(f, text.data(), static_cast<DWORD>(text.size()), &got, nullptr);
        CloseHandle(f); text.resize(got);
        if (!ok || text.find(requirement.success) == std::string::npos) return false;
    }
    return true;
}

void open_disk_image();
bool code_sections_executable(std::string* first_bad);

// Azimuth maps the game as plain memory and only marks its code executable afterwards; Extra
// Options can be done before that. Hooking in between fails ("it isn't code"), which happened
// once: six of WreckingBall's hooks and WheresMyBuddy's within a few milliseconds of "ready".
// So "ready" also waits until every executable section of the exe on disk is executable here.
bool wait_for_executable_code() {
    {
        std::scoped_lock lk{g_mu};
        open_disk_image();
    }
    const ULONGLONG t0 = GetTickCount64();
    std::string bad;
    while (!code_sections_executable(&bad)) {
        if (GetTickCount64() - t0 >= 10000) {
            plog("hooks: the game's code still isn't all executable after 10 seconds (" + bad + "); refusing hooks");
            return false;
        }
        Sleep(10);
    }
    if (const ULONGLONG waited = GetTickCount64() - t0; waited >= 10)
        plog("hooks: waited " + std::to_string(waited) + " ms for the game's code to be marked executable");
    return true;
}

// Wait until manifest-declared dependencies settle and code pages are executable.
void watch_for_code() {
    std::thread([] {
        while (!find_game()) Sleep(50);
        const ULONGLONG deadline = GetTickCount64() + 20000;
        while (!dependencies_ready()) {
            if (GetTickCount64() >= deadline) { plog("dependency readiness timed out; refusing hooks"); return; }
            Sleep(50);
        }
        if (!wait_for_executable_code()) return;
        plog("hooks: game code and declared dependencies are ready");
        g_code_ready.store(true, std::memory_order_release);
    }).detach();
}

// ---- records, for crash reports ------------------------------------------------------
// Plain fixed-size data, readable from the crash handler.

struct Record {
    char text[160]; // "WheresMyBuddy: pointer game+0x2D44F30 (ChampionshipDrivatarsState::Serialize)"
    uintptr_t tramp;
    uintptr_t code_lo, code_hi; // an instruction hook's stub (saving registers, the moved instructions)
    volatile LONG enabled;
};
constexpr int kMaxRecords = 256;
Record g_records[kMaxRecords];
volatile LONG g_nrecords = 0;

// ---- sites -------------------------------------------------------------------------

struct InstrList; // an instruction hook site's callbacks, newest first (see below)

struct Site {
    Kind kind;
    char* at;            // function start, call instruction, pointer, patched bytes or hooked instruction
    void* original;      // trampoline, the call's old target, the pointer's old value
    Stub entry;
    char* lo;            // bytes this site changes or depends on
    char* hi;
    std::vector<unsigned char> before; // [lo, hi) before the first patch
    bool live = false;
    std::vector<InfinityHook*> chain; // in the order they were last enabled
    std::vector<unsigned char> patch; // kPatch: the new bytes; kInstruction: the jump to `thunk`
    char* thunk = nullptr;            // kInstruction: saves the registers, calls the hooks, runs the moved code
    std::size_t thunk_size = 0;
    std::atomic<InstrList*> list{nullptr}; // kInstruction: what the thunk calls
};
std::vector<std::unique_ptr<Site>> g_sites;
bool g_minhook = false;

} // namespace

void configure_readiness(std::vector<Readiness> requirements) { g_readiness = std::move(requirements); }

} // namespace infinity_mod

struct InfinityHook {
    InfinityMod* mod;
    std::string name;
    infinity_mod::Site* site;
    void* detour;          // kInstruction: the mod's InfinityInstructionFn
    void* user = nullptr;  // kInstruction: its argument
    infinity_mod::Stub next;
    bool enabled = false;
    int record   = -1;
    std::atomic<bool> crashed{false}; // kInstruction: its fn crashed, so it's skipped
};

namespace infinity_mod {
namespace {

std::vector<std::unique_ptr<InfinityHook>> g_hooks;

std::string describe(const InfinityHook* h) {
    return std::string(kKindName[h->site->kind]) + " " + where(h->site->at) +
           (h->name.empty() ? "" : " (" + h->name + ")");
}

bool is_padding(const unsigned char* p, std::size_t n) {
    if (p[0] != 0x00 && p[0] != 0x90 && p[0] != 0xCC) return false;
    for (std::size_t i = 1; i < n; ++i)
        if (p[i] != p[0]) return false;
    return true;
}

// The bytes MinHook will copy into the trampoline or overwrite, as it
// decides them (trampoline.c): instructions up to 5 bytes, or up to a return
// or jump out, with the jump written over padding after it or above the
// function. False if MinHook couldn't hook it.
bool function_range(const unsigned char* t, char** lo, char** hi) {
    const auto start = reinterpret_cast<uintptr_t>(t);
    std::size_t pos  = 0;
    uintptr_t jmp_dest = 0; // the furthest jump target inside the first 5 bytes
    bool finished      = false;
    do {
        if (!readable(t + pos, 16)) return false;
        hde64s hs{};
        hde64_disasm(t + pos, &hs);
        if (hs.flags & F_ERROR) return false;
        if (pos >= 5) break; // long enough: MinHook jumps back here, this instruction isn't copied
        const uintptr_t inst = start + pos;
        bool resized         = false; // MinHook rewrites it as a longer, absolute form
        if ((hs.modrm & 0xC7) == 0x05) {
            if (hs.opcode == 0xFF && hs.modrm_reg == 4) finished = true; // jmp [rip+x]
        } else if (hs.opcode == 0xE8) {
            resized = true;
        } else if ((hs.opcode & 0xFD) == 0xE9) {
            const uintptr_t dest = inst + hs.len + (hs.opcode == 0xEB ? static_cast<int8_t>(hs.imm.imm8)
                                                                      : static_cast<int32_t>(hs.imm.imm32));
            if (start <= dest && dest < start + 5) {
                jmp_dest = std::max(jmp_dest, dest);
            } else {
                resized  = true;
                finished = inst >= jmp_dest;
            }
        } else if ((hs.opcode & 0xF0) == 0x70 || (hs.opcode & 0xFC) == 0xE0 || (hs.opcode2 & 0xF0) == 0x80) {
            const bool short_form = (hs.opcode & 0xF0) == 0x70 || (hs.opcode & 0xFC) == 0xE0;
            const uintptr_t dest  = inst + hs.len + (short_form ? static_cast<int8_t>(hs.imm.imm8)
                                                                : static_cast<int32_t>(hs.imm.imm32));
            if (start <= dest && dest < start + 5) jmp_dest = std::max(jmp_dest, dest);
            else if ((hs.opcode & 0xFC) == 0xE0) return false; // LOOP/JECXZ out: MinHook can't
            else resized = true;
        } else if ((hs.opcode & 0xFE) == 0xC2) {
            finished = inst >= jmp_dest;
        }
        if (inst < jmp_dest && resized) return false;
        pos += hs.len;
    } while (!finished);
    auto* c = reinterpret_cast<char*>(const_cast<unsigned char*>(t));
    if (pos < 5 && !is_padding(t + pos, 5 - pos)) {
        // Too short: a short jmp at the start, to a long one in the padding above it.
        if (pos < 2 && !is_padding(t + pos, 2 - pos)) return false;
        if (!executable(t - 5) || !is_padding(t - 5, 5)) return false;
        *lo = c - 5;
        *hi = c + std::max<std::size_t>(pos, 2);
    } else {
        *lo = c;
        *hi = c + std::max<std::size_t>(pos, 5);
    }
    return true;
}

Site* find_site(Kind k, const void* at) {
    for (auto& s : g_sites)
        if (s->kind == k && s->at == at) return s.get();
    return nullptr;
}

// A hook of another kind (or another place) whose bytes overlap these.
const Site* overlapping(const char* lo, const char* hi) {
    for (auto& s : g_sites)
        if (lo < s->hi && s->lo < hi) return s.get();
    return nullptr;
}

std::string owners(const Site* s) {
    std::string out;
    for (const InfinityHook* h : s->chain)
        if (out.find(h->mod->name) == std::string::npos) out += (out.empty() ? "" : ", ") + h->mod->name;
    return out.empty() ? "a hook" : out;
}

bool check_bytes(InfinityMod* m, const char* what, char* lo, char* hi, bool pointer) {
    if (const Site* o = overlapping(lo, hi)) {
        log_line(m->name, std::string("can't hook ") + what + ": it overlaps " + owners(o) + "'s " +
                              kKindName[o->kind] + " hook at " + where(o->at));
        return false;
    }
    if (in_game(lo) && matches_disk(lo, static_cast<std::size_t>(hi - lo), pointer) != 1) {
        log_line(m->name, std::string("can't hook ") + what +
                              ": game bytes could not be verified against disk (missing, relocated or changed by "
                              "a mod that patches the game by itself)");
        return false;
    }
    return true;
}

bool common_checks(InfinityMod* m, const char* what, void* at, void* detour, void** original) {
    if (!m) return false;
    if (!at || !detour || !original) {
        log_line(m->name, std::string("can't hook ") + what + ": null address, detour or original");
        return false;
    }
    if (in_game(at) && !code_ready()) {
        log_line(m->name, std::string("can't hook ") + what +
                              " yet: call wait_for_code first, so declared dependencies can finish before you patch the game");
        return false;
    }
    if (!executable(detour)) {
        log_line(m->name, std::string("can't hook ") + what + ": the detour isn't code");
        return false;
    }
    return true;
}

// Patches and instruction hooks have no `original` to call: no stub of their own.
InfinityHook* add_hook(InfinityMod* m, const char* name, Site* s, void* detour, void** original, void* user = nullptr) {
    auto h    = std::make_unique<InfinityHook>();
    h->mod    = m;
    h->name   = name ? name : "";
    h->site   = s;
    h->detour = detour;
    h->user   = user;
    if (original) {
        h->next = new_stub(s->at);
        if (!h->next.code) {
            log_line(m->name, "can't hook " + where(s->at) + ": out of memory for hook stubs");
            return nullptr;
        }
        set_slot(h->next.slot, s->original);
        *original = h->next.code;
    }
    InfinityHook* raw = h.get();
    s->chain.push_back(raw);
    g_hooks.push_back(std::move(h));
    if (g_nrecords < kMaxRecords) {
        Record& r = g_records[g_nrecords];
        std::snprintf(r.text, sizeof r.text, "%s: %s", m->name.c_str(), describe(raw).c_str());
        r.tramp   = s->kind == kFunction ? reinterpret_cast<uintptr_t>(s->original) : 0;
        r.code_lo = reinterpret_cast<uintptr_t>(s->thunk);
        r.code_hi = r.code_lo + s->thunk_size;
        r.enabled = 0;
        raw->record = g_nrecords;
        InterlockedIncrement(&g_nrecords);
    }
    log_line(m->name, "hooked " + describe(raw) +
                          (s->chain.size() > 1 ? " (shared with " + owners(s) + ")" : ""));
    return raw;
}

Site* new_site(Kind k, char* at, void* original, char* lo, char* hi) {
    auto s      = std::make_unique<Site>();
    s->kind     = k;
    s->at       = at;
    s->original = original;
    s->lo       = lo;
    s->hi       = hi;
    s->before.assign(lo, hi);
    Site* raw = s.get();
    g_sites.push_back(std::move(s));
    return raw;
}

// ---- writing patches ------------------------------------------------------------------

// For a rel32 that straddles a 16-byte boundary, which no single atomic
// write covers: pause every other thread (as MinHook does), make sure none
// of them is on the instruction, write, and let them go. Nothing may
// allocate or lock while they're paused, so the list is made first.
bool frozen_write(char* site, int32_t rel) {
    std::vector<HANDLE> threads;
    threads.reserve(256);
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if (snap == INVALID_HANDLE_VALUE) return false;
    THREADENTRY32 te{};
    te.dwSize = sizeof te;
    const DWORD pid = GetCurrentProcessId(), self = GetCurrentThreadId();
    for (BOOL more = Thread32First(snap, &te); more; more = Thread32Next(snap, &te))
        if (te.th32OwnerProcessID == pid && te.th32ThreadID != self)
            if (HANDLE h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, FALSE, te.th32ThreadID))
                threads.push_back(h);
    CloseHandle(snap);
    const auto lo = reinterpret_cast<DWORD64>(site), hi = lo + 5;
    bool done = false;
    for (int attempt = 0; attempt < 100 && !done; ++attempt) {
        for (HANDLE h : threads) SuspendThread(h);
        bool clear = true;
        for (HANDLE h : threads) {
            CONTEXT ctx{};
            ctx.ContextFlags = CONTEXT_CONTROL;
            if (GetThreadContext(h, &ctx) && ctx.Rip > lo && ctx.Rip < hi) clear = false;
        }
        if (clear) {
            std::memcpy(site + 1, &rel, 4);
            done = true;
        }
        for (HANDLE h : threads) ResumeThread(h);
        if (!done) Sleep(1);
    }
    for (HANDLE h : threads) CloseHandle(h);
    return done;
}

// Writes `n` bytes of code at `at` with every other thread paused and none of them inside
// (at, at+n): one may be about to run the instruction at `at`, but none half way through the
// bytes being changed. Nothing may allocate or lock while they're paused.
bool frozen_write_bytes(char* at, const unsigned char* bytes, std::size_t n) {
    std::vector<HANDLE> threads;
    threads.reserve(256);
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if (snap == INVALID_HANDLE_VALUE) return false;
    THREADENTRY32 te{};
    te.dwSize = sizeof te;
    const DWORD pid = GetCurrentProcessId(), self = GetCurrentThreadId();
    for (BOOL more = Thread32First(snap, &te); more; more = Thread32Next(snap, &te))
        if (te.th32OwnerProcessID == pid && te.th32ThreadID != self)
            if (HANDLE h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, FALSE, te.th32ThreadID))
                threads.push_back(h);
    CloseHandle(snap);
    DWORD old = 0;
    if (!VirtualProtect(at, n, PAGE_EXECUTE_READWRITE, &old)) {
        for (HANDLE h : threads) CloseHandle(h);
        return false;
    }
    const auto lo = reinterpret_cast<DWORD64>(at), hi = lo + n;
    bool done = false;
    for (int attempt = 0; attempt < 100 && !done; ++attempt) {
        for (HANDLE h : threads) SuspendThread(h);
        bool clear = true;
        for (HANDLE h : threads) {
            CONTEXT ctx{};
            ctx.ContextFlags = CONTEXT_CONTROL;
            if (GetThreadContext(h, &ctx) && ctx.Rip > lo && ctx.Rip < hi) clear = false;
        }
        if (clear) {
            std::memcpy(at, bytes, n);
            FlushInstructionCache(GetCurrentProcess(), at, n);
            done = true;
        }
        for (HANDLE h : threads) ResumeThread(h);
        if (!done) Sleep(1);
    }
    for (HANDLE h : threads) CloseHandle(h);
    DWORD ignored = 0;
    VirtualProtect(at, n, old, &ignored);
    return done;
}

// Atomically rewrites the rel32 of the 5-byte CALL/JMP at `site`.
bool write_rel32(char* site, int32_t rel) {
    DWORD old = 0;
    if (!VirtualProtect(site, 5, PAGE_EXECUTE_READWRITE, &old)) return false;
    bool ok             = false;
    const auto addr     = reinterpret_cast<uintptr_t>(site + 1);
    const auto a8       = addr & ~uintptr_t{7};
    const auto a16      = addr & ~uintptr_t{15};
    if (addr - a8 <= 4) {
        auto* q = reinterpret_cast<volatile LONG64*>(a8);
        for (int i = 0; i < 64 && !ok; ++i) {
            LONG64 cur = *q, nxt = cur;
            std::memcpy(reinterpret_cast<char*>(&nxt) + (addr - a8), &rel, 4);
            ok = InterlockedCompareExchange64(q, nxt, cur) == cur;
        }
    } else if (addr - a16 <= 12) {
        auto* q = reinterpret_cast<LONG64*>(a16);
        for (int i = 0; i < 64 && !ok; ++i) {
            alignas(16) LONG64 cur[2] = {q[0], q[1]};
            LONG64 nxt[2]             = {cur[0], cur[1]};
            std::memcpy(reinterpret_cast<char*>(nxt) + (addr - a16), &rel, 4);
#ifdef _MSC_VER
            ok = _InterlockedCompareExchange128(q, nxt[1], nxt[0], cur) != 0;
#else
            unsigned __int128 c, x;
            std::memcpy(&c, cur, 16);
            std::memcpy(&x, nxt, 16);
            ok = __sync_bool_compare_and_swap(reinterpret_cast<unsigned __int128*>(q), c, x);
#endif
        }
    }
    if (!ok) ok = frozen_write(site, rel);
    DWORD ignored = 0;
    VirtualProtect(site, 5, old, &ignored);
    FlushInstructionCache(GetCurrentProcess(), site, 5);
    return ok;
}

bool write_pointer(void** slot, void* expect, void* value) {
    // Keep the page executable if it was: the slot may share it with code.
    MEMORY_BASIC_INFORMATION mbi{};
    if (!query(slot, mbi)) return false;
    const DWORD cur      = mbi.Protect & 0xFF;
    const bool writable  = cur == PAGE_READWRITE || cur == PAGE_EXECUTE_READWRITE;
    const bool exec      = cur == PAGE_EXECUTE_READ || cur == PAGE_EXECUTE_READWRITE || cur == PAGE_EXECUTE_WRITECOPY ||
                      cur == PAGE_EXECUTE;
    DWORD old = 0;
    if (!writable && !VirtualProtect(slot, sizeof(void*), exec ? PAGE_EXECUTE_READWRITE : PAGE_READWRITE, &old))
        return false;
    const bool ok = InterlockedCompareExchangePointer(slot, value, expect) == expect;
    DWORD ignored = 0;
    if (!writable) VirtualProtect(slot, sizeof(void*), old, &ignored);
    if (exec) FlushInstructionCache(GetCurrentProcess(), slot, sizeof(void*));
    return ok;
}

// What a site has to do once its chain has changed.
enum class Change { none, patch, unpatch };

// Rewires the stubs of `s` for its enabled hooks, and says whether the
// patch has to go in or come out.
void publish_list(Site* s, const std::vector<InfinityHook*>& on); // instruction hooks (below)

Change rewire(Site* s) {
    if (s->kind == kPatch || s->kind == kInstruction) {
        std::vector<InfinityHook*> on;
        for (InfinityHook* h : s->chain)
            if (h->enabled) on.push_back(h);
        if (s->kind == kInstruction) publish_list(s, on);
        if (on.empty()) return s->live ? Change::unpatch : Change::none;
        return s->live ? Change::none : Change::patch;
    }
    std::vector<InfinityHook*> on;
    for (InfinityHook* h : s->chain)
        if (h->enabled) on.push_back(h);
        else set_slot(h->next.slot, s->original); // an off hook's `original` skips every detour
    if (on.empty()) {
        set_slot(s->entry.slot, s->original);
        return s->live ? Change::unpatch : Change::none;
    }
    // Inner links first, then the entry, so a thread already in a detour
    // always finds a complete chain.
    for (std::size_t i = 0; i < on.size(); ++i) set_slot(on[i]->next.slot, i ? on[i - 1]->detour : s->original);
    set_slot(s->entry.slot, on.back()->detour);
    return s->live ? Change::none : Change::patch;
}

// Puts the patch of a call or pointer site in or out. Function sites are
// done through MinHook's queue by the caller.
bool apply_simple(Site* s, bool in) {
    if (s->kind == kPatch || s->kind == kInstruction)
        return frozen_write_bytes(s->lo, in ? s->patch.data() : s->before.data(), s->before.size());
    if (s->kind == kCall) {
        void* to = in ? s->entry.code : s->original;
        return write_rel32(s->at, static_cast<int32_t>(static_cast<char*>(to) - (s->at + 5)));
    }
    void** slot = reinterpret_cast<void**>(s->at);
    return in ? write_pointer(slot, s->original, s->entry.code) : write_pointer(slot, s->entry.code, s->original);
}

void set_records(Site* s) {
    for (InfinityHook* h : s->chain)
        if (h->record >= 0) g_records[h->record].enabled = h->enabled && s->live;
}

// Enables or disables `hook`, or every hook of `m` if it's null.
int switch_hooks(InfinityMod* m, InfinityHook* hook, bool on) {
    if (!m) return 0;
    std::scoped_lock lk{g_mu};
    if (hook && std::none_of(g_hooks.begin(), g_hooks.end(), [&](const auto& h) { return h.get() == hook; })) {
        log_line(m->name, "can't switch a hook: that's not a hook handle");
        return 0;
    }
    std::vector<InfinityHook*> mine;
    for (auto& h : g_hooks)
        if (h->mod == m && (!hook || h.get() == hook) && h->enabled != on) mine.push_back(h.get());
    if (hook && hook->mod != m) {
        log_line(m->name, "can't switch another mod's hook");
        return 0;
    }
    if (hook && mine.empty()) return 1; // already that way
    std::vector<Site*> sites;
    for (InfinityHook* h : mine) {
        h->enabled = on;
        if (on) {
            // The most recently enabled detour runs first: move it to the end.
            auto& ch = h->site->chain;
            ch.erase(std::find(ch.begin(), ch.end(), h));
            ch.push_back(h);
        }
        if (std::find(sites.begin(), sites.end(), h->site) == sites.end()) sites.push_back(h->site);
    }
    int ok = 1;
    std::vector<Site*> queued;
    for (Site* s : sites) {
        const Change c = rewire(s);
        if (c == Change::none) continue;
        const bool in = c == Change::patch;
        if (in && std::memcmp(s->before.data(), s->lo, s->before.size()) != 0) {
            log_line(m->name, "can't enable the hook at " + where(s->at) +
                                  ": the code there changed after it was hooked");
            for (InfinityHook* h : s->chain)
                if (h->mod == m && std::find(mine.begin(), mine.end(), h) != mine.end()) h->enabled = false;
            rewire(s);
            ok = 0;
            continue;
        }
        if (s->kind == kFunction) {
            const MH_STATUS st = (in ? MH_QueueEnableHook : MH_QueueDisableHook)(s->at);
            if (st == MH_OK) {
                queued.push_back(s);
            } else {
                log_line(m->name, "MinHook couldn't queue " + where(s->at) + ": " + MH_StatusToString(st));
                ok = 0;
            }
        } else if (apply_simple(s, in)) {
            s->live = in;
        } else {
            log_line(m->name, std::string("couldn't ") + (in ? "patch " : "unpatch ") + where(s->at));
            ok = 0;
        }
    }
    if (!queued.empty()) {
        // One pass: MinHook suspends the other threads once for all of them.
        const MH_STATUS st = MH_ApplyQueued();
        if (st != MH_OK) {
            log_line(m->name, std::string("MinHook couldn't apply all the hooks: ") + MH_StatusToString(st));
            ok = 0;
        }
        // MinHook stops at the first failure, keeping what it did before it.
        // Go by the bytes, and leave nothing queued for the next caller.
        for (Site* s : queued) {
            const bool was = s->live;
            s->live        = std::memcmp(s->before.data(), s->lo, s->before.size()) != 0;
            (s->live ? MH_QueueEnableHook : MH_QueueDisableHook)(s->at);
            if (s->live == was) ok = 0;
        }
    }
    for (Site* s : sites) {
        set_records(s);
        if (!s->live)
            for (InfinityHook* h : s->chain)
                if (h->enabled) {
                    // Couldn't go live: put the chain back as it is.
                    h->enabled = false;
                }
        if (!s->live) rewire(s);
    }
    int count = 0;
    for (InfinityHook* h : mine) count += h->enabled == on;
    if (count)
        log_line(m->name, std::string(on ? "enabled " : "disabled ") + std::to_string(count) +
                              (count == 1 ? " hook" : " hooks") +
                              (hook ? " (" + describe(hook) + ")" : std::string{}));
    return ok;
}

// ---- the API -------------------------------------------------------------------------

// A target in the game that isn't executable yet gets up to 2 seconds (see wait_for_executable_code).
void settle(const void* code) {
    if (!in_game(code)) return;
    for (int i = 0; i < 200 && !executable(code); ++i) Sleep(10);
}

InfinityHook* api_hook_function(InfinityMod* m, const char* name, void* target, void* detour, void** original) {
    const std::string what = "function " + where(target) + (name ? std::string(" (") + name + ")" : "");
    if (!common_checks(m, what.c_str(), target, detour, original)) return nullptr;
    settle(target);
    std::scoped_lock lk{g_mu};
    if (!executable(target)) {
        log_line(m->name, "can't hook " + what + ": it isn't code");
        return nullptr;
    }
    Site* s = find_site(kFunction, target);
    if (!s) {
        char *lo = nullptr, *hi = nullptr;
        if (!function_range(static_cast<unsigned char*>(target), &lo, &hi)) {
            log_line(m->name, "can't hook " + what + ": MinHook can't patch the start of that function");
            return nullptr;
        }
        if (!check_bytes(m, what.c_str(), lo, hi, false)) return nullptr;
        if (!g_minhook) {
            const MH_STATUS st = MH_Initialize();
            if (st != MH_OK && st != MH_ERROR_ALREADY_INITIALIZED) {
                log_line(m->name, std::string("can't hook: MinHook didn't start: ") + MH_StatusToString(st));
                return nullptr;
            }
            g_minhook = true;
        }
        const Stub entry = new_stub(target);
        if (!entry.code) {
            log_line(m->name, "can't hook " + what + ": out of memory for hook stubs");
            return nullptr;
        }
        void* tramp        = nullptr;
        const MH_STATUS st = MH_CreateHook(target, entry.code, &tramp);
        if (st != MH_OK) {
            log_line(m->name, "can't hook " + what + ": " + MH_StatusToString(st));
            return nullptr;
        }
        s        = new_site(kFunction, static_cast<char*>(target), tramp, lo, hi);
        s->entry = entry;
        set_slot(entry.slot, tramp);
    }
    return add_hook(m, name, s, detour, original);
}

InfinityHook* api_hook_call(InfinityMod* m, const char* name, void* call_site, void* detour, void** original) {
    const std::string what = "call " + where(call_site) + (name ? std::string(" (") + name + ")" : "");
    if (!common_checks(m, what.c_str(), call_site, detour, original)) return nullptr;
    settle(call_site);
    std::scoped_lock lk{g_mu};
    auto* at = static_cast<char*>(call_site);
    Site* s  = find_site(kCall, at);
    if (!s) {
        unsigned char op = 0;
        int32_t rel      = 0;
        if (!executable(at) || !readable(at, 5) || ((op = static_cast<unsigned char>(*at)) != 0xE8 && op != 0xE9)) {
            log_line(m->name, "can't hook " + what + ": there's no CALL or JMP rel32 there");
            return nullptr;
        }
        std::memcpy(&rel, at + 1, 4);
        if (!check_bytes(m, what.c_str(), at, at + 5, false)) return nullptr;
        const Stub entry = new_stub(at);
        if (!entry.code || !reaches(at + 5, entry.code)) {
            log_line(m->name, "can't hook " + what + ": no memory for a stub within reach of it");
            return nullptr;
        }
        s        = new_site(kCall, at, at + 5 + rel, at, at + 5);
        s->entry = entry;
        set_slot(entry.slot, s->original);
    }
    return add_hook(m, name, s, detour, original);
}

InfinityHook* api_hook_pointer(InfinityMod* m, const char* name, void** slot, void* detour, void** original) {
    const std::string what = "pointer " + where(slot) + (name ? std::string(" (") + name + ")" : "");
    if (!common_checks(m, what.c_str(), slot, detour, original)) return nullptr;
    if (readable(slot, 8) && !(reinterpret_cast<uintptr_t>(slot) & 7)) settle(*slot);
    std::scoped_lock lk{g_mu};
    auto* at = reinterpret_cast<char*>(slot);
    Site* s  = find_site(kPointer, at);
    if (!s) {
        if ((reinterpret_cast<uintptr_t>(slot) & 7) || !readable(slot, 8)) {
            log_line(m->name, "can't hook " + what + ": not a readable, aligned pointer");
            return nullptr;
        }
        void* old = *slot;
        if (!old || !executable(old)) {
            log_line(m->name, "can't hook " + what + ": it doesn't point at code");
            return nullptr;
        }
        if (!check_bytes(m, what.c_str(), at, at + 8, true)) return nullptr;
        const Stub entry = new_stub(at);
        if (!entry.code) {
            log_line(m->name, "can't hook " + what + ": out of memory for hook stubs");
            return nullptr;
        }
        s        = new_site(kPointer, at, old, at, at + 8);
        s->entry = entry;
        set_slot(entry.slot, old);
    }
    return add_hook(m, name, s, detour, original);
}

// ---- API 7: byte patches -------------------------------------------------------------

InfinityHook* api_patch_bytes(InfinityMod* m, const char* name, void* at, const void* expect, const void* bytes, uint32_t size) {
    const std::string what = "patch " + where(at) + (name ? std::string(" (") + name + ")" : "");
    if (!m) return nullptr;
    if (!at || !bytes || size == 0 || size > 32) {
        log_line(m->name, "can't make " + what + ": it needs an address, 1 to 32 bytes and the bytes themselves");
        return nullptr;
    }
    if (in_game(at) && !code_ready()) {
        log_line(m->name, "can't make " + what +
                              " yet: call wait_for_code first, so declared dependencies can finish before you patch the game");
        return nullptr;
    }
    std::scoped_lock lk{g_mu};
    auto* lo = static_cast<char*>(at);
    if (!readable(lo, size)) {
        log_line(m->name, "can't make " + what + ": that memory isn't readable");
        return nullptr;
    }
    if (expect && std::memcmp(lo, expect, size) != 0) {
        log_line(m->name, "can't make " + what + ": the bytes there aren't the ones the mod expected");
        return nullptr;
    }
    if (!check_bytes(m, what.c_str(), lo, lo + size, false)) return nullptr;
    Site* s = new_site(kPatch, lo, nullptr, lo, lo + size);
    s->patch.assign(static_cast<const unsigned char*>(bytes), static_cast<const unsigned char*>(bytes) + size);
    return add_hook(m, name, s, nullptr, nullptr);
}

// ---- API 7: instruction hooks --------------------------------------------------------
//
// The hooked instruction (and as many after it as make 5 bytes) is replaced with a JMP to the
// site's thunk, in memory near the game:
//
//   pad      256 bytes of int3 (see the unwind info below)
//   save     pushfq; sub rsp, 0x190 (+0x100 for ymm upper halves, with AVX); the registers into
//            an InfinityContext at rsp; rbx = rsp
//   call     rsp aligned, dispatch(ctx, site): every enabled hook's fn, newest first
//   restore  the registers (with the hooks' changes) and flags; rsp as it was; jmp over:
//   pad      256 bytes of int3
//   moved    the replaced instructions, rewritten for their new address
//   back     jmp [rip]; dq the instruction after them
//   unwind   two RUNTIME_FUNCTIONs, registered relative to the game's base: the save/call
//            part undoes its frame (rbx is the frame register) and chains to the game
//            function's own unwind info, and the moved code chains to it directly. Their unwind
//            info has no prolog, so the 256-byte pads keep the chained game info's prolog out
//            of the way (every code of it applies, as at the hooked instruction, which has to
//            be past the prolog). Stack walks through a hook, for crash reports and for
//            exceptions thrown inside a hook's fn (caught by dispatch), see the game's
//            callers as before.

struct InstrEntry {
    InfinityInstructionFn fn;
    void* user;
    InfinityHook* hook;
};
struct InstrList {
    std::size_t n;
    InstrEntry e[1];
};

constexpr uint32_t kCtxSize = 0x190; // sizeof(InfinityContext)
static_assert(sizeof(InfinityContext) == kCtxSize, "InfinityContext's layout is what the thunk writes");
static_assert(offsetof(InfinityContext, rflags) == 0x80 && offsetof(InfinityContext, xmm) == 0x90, "InfinityContext's layout");

void log_hook_crash(InfinityHook* h, DWORD code) {
    notify_hook_fault();
    char b[16];
    std::snprintf(b, sizeof b, "0x%08lX", static_cast<unsigned long>(code));
    log_line(h->mod->name, "its instruction hook " + where(h->site->at) + (h->name.empty() ? "" : " (" + h->name + ")") +
                               " crashed (exception " + b + "); that hook is skipped from now on");
}

bool call_instr(const InstrEntry& e, InfinityContext* ctx, DWORD* code) {
    __try {
        e.fn(ctx, e.user);
        return true;
    } __except (*code = GetExceptionCode(), EXCEPTION_EXECUTE_HANDLER) {
        return false;
    }
}

// Called by every instruction hook's thunk, on the game's thread.
void dispatch(InfinityContext* ctx, Site* s) {
    const InstrList* l = s->list.load(std::memory_order_acquire);
    if (!l) return;
    const uint64_t rsp = ctx->rsp;
    for (std::size_t i = 0; i < l->n; ++i) {
        const InstrEntry& e = l->e[i];
        if (e.hook->crashed.load(std::memory_order_relaxed)) continue;
        DWORD code = 0;
        if (!call_instr(e, ctx, &code) && !e.hook->crashed.exchange(true)) log_hook_crash(e.hook, code);
    }
    ctx->rsp = rsp;
}

void publish_list(Site* s, const std::vector<InfinityHook*>& on) {
    // Never freed: a thread may still be reading the old list (a few bytes per change).
    auto* l = static_cast<InstrList*>(std::calloc(1, sizeof(InstrList) + sizeof(InstrEntry) * (on.size() + 1)));
    if (!l) return;
    l->n = on.size();
    for (std::size_t i = 0; i < on.size(); ++i) {
        InfinityHook* h = on[on.size() - 1 - i]; // the most recently enabled first
        l->e[i]    = {reinterpret_cast<InfinityInstructionFn>(h->detour), h->user, h};
    }
    s->list.store(l, std::memory_order_release);
}

// -- the game's function table

struct UnwindHead {
    uint8_t version_flags, prolog, count, frame;
};
constexpr uint8_t kUnwChain = 4;

// The image (the game, or a loaded DLL) that `p` is in: its base and size, or null.
const char* image_of(const void* p, uint32_t* size) {
    if (in_game(p)) return *size = kGameSizeOfImage, game_base();
    HMODULE m = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            static_cast<LPCWSTR>(p), &m))
        return nullptr;
    const auto* b   = reinterpret_cast<const char*>(m);
    const auto* dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(b);
    *size = reinterpret_cast<const IMAGE_NT_HEADERS64*>(b + dos->e_lfanew)->OptionalHeader.SizeOfImage;
    return b;
}

const RUNTIME_FUNCTION* image_pdata(const char* base, std::size_t* n) {
    *n = 0;
    if (!base || !readable(base, 0x400)) return nullptr;
    const auto* dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(base);
    const auto* nt  = reinterpret_cast<const IMAGE_NT_HEADERS64*>(base + dos->e_lfanew);
    const auto& dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXCEPTION];
    if (!dir.VirtualAddress || !dir.Size || !readable(base + dir.VirtualAddress, dir.Size)) return nullptr;
    *n = dir.Size / sizeof(RUNTIME_FUNCTION);
    return reinterpret_cast<const RUNTIME_FUNCTION*>(base + dir.VirtualAddress);
}

const RUNTIME_FUNCTION* find_function(const RUNTIME_FUNCTION* t, std::size_t n, uint32_t rva) {
    std::size_t lo = 0, hi = n;
    while (lo < hi) {
        const std::size_t mid = (lo + hi) / 2;
        if (rva < t[mid].BeginAddress) hi = mid;
        else if (rva >= t[mid].EndAddress) lo = mid + 1;
        else return &t[mid];
    }
    return nullptr;
}

const UnwindHead* unwind_of(const char* base, const RUNTIME_FUNCTION* f) {
    uint32_t u       = f->UnwindData;
    if (u & 1) u = reinterpret_cast<const RUNTIME_FUNCTION*>(base + (u & ~1u))->UnwindData; // shared entry
    return reinterpret_cast<const UnwindHead*>(base + u);
}

const RUNTIME_FUNCTION* chained(const UnwindHead* u) {
    if (!((u->version_flags >> 3) & kUnwChain)) return nullptr;
    return reinterpret_cast<const RUNTIME_FUNCTION*>(reinterpret_cast<const uint16_t*>(u + 1) + ((u->count + 1) & ~1));
}

const RUNTIME_FUNCTION* primary_of(const char* base, const RUNTIME_FUNCTION* f) {
    for (int i = 0; i < 32; ++i) {
        const RUNTIME_FUNCTION* c = chained(unwind_of(base, f));
        if (!c) return f;
        f = c;
    }
    return f;
}

// -- decoding

struct Insn {
    const unsigned char* at;
    hde64s hs;
};

bool decode(const unsigned char* p, hde64s* hs) {
    if (!readable(p, 16)) return false;
    std::memset(hs, 0, sizeof *hs);
    hde64_disasm(p, hs);
    return !(hs->flags & F_ERROR) && hs->len;
}

int imm_size(const hde64s& hs) {
    return (hs.flags & F_IMM64) ? 8 : (hs.flags & F_IMM32) ? 4 : (hs.flags & F_IMM16) ? 2 : (hs.flags & F_IMM8) ? 1 : 0;
}

bool rip_relative(const hde64s& hs) { return (hs.flags & F_MODRM) && hs.modrm_mod == 0 && hs.modrm_rm == 5; }

enum class Branch { none, call, jmp, jcc, loop };

// A relative CALL/JMP/Jcc/LOOP and where it goes.
Branch branch_of(const unsigned char* ip, const hde64s& hs, const unsigned char** target) {
    const unsigned char* next = ip + hs.len;
    if (hs.opcode == 0xE8) return *target = next + static_cast<int32_t>(hs.imm.imm32), Branch::call;
    if (hs.opcode == 0xE9) return *target = next + static_cast<int32_t>(hs.imm.imm32), Branch::jmp;
    if (hs.opcode == 0xEB) return *target = next + static_cast<int8_t>(hs.imm.imm8), Branch::jmp;
    if ((hs.opcode & 0xF0) == 0x70) return *target = next + static_cast<int8_t>(hs.imm.imm8), Branch::jcc;
    if (hs.opcode == 0x0F && (hs.opcode2 & 0xF0) == 0x80)
        return *target = next + static_cast<int32_t>(hs.imm.imm32), Branch::jcc;
    if ((hs.opcode & 0xFC) == 0xE0) return *target = next + static_cast<int8_t>(hs.imm.imm8), Branch::loop;
    return Branch::none;
}

// The instructions an instruction hook at `at` replaces (5 bytes or more), after checking it can
// go there: `at` starts an instruction (decoding the function from its start reaches it), it's
// past the prolog, the replaced bytes stay inside the function, and no relative jump in the
// function lands inside them (only `at` itself). `fn_out` is the function table entry for `at`.
bool instruction_range(const char* base, const unsigned char* at, std::vector<Insn>* out,
                       const RUNTIME_FUNCTION** fn_out, std::string* why) {
    std::size_t n               = 0;
    const RUNTIME_FUNCTION* tab = image_pdata(base, &n);
    if (!tab) return *why = "the function table of the code it's in isn't readable", false;
    const auto rva              = static_cast<uint32_t>(reinterpret_cast<const char*>(at) - base);
    const RUNTIME_FUNCTION* f   = find_function(tab, n, rva);
    if (!f) return *why = "it isn't inside a function its function table knows", false;
    *fn_out                     = f;
    const UnwindHead* u         = unwind_of(base, f);
    if (rva < f->BeginAddress + u->prolog) return *why = "it's in the function's prolog", false;
    // from the start of the function (or this part of it) to `at`
    const auto* p   = reinterpret_cast<const unsigned char*>(base + f->BeginAddress);
    const auto* end = reinterpret_cast<const unsigned char*>(base + f->EndAddress);
    hde64s hs{};
    while (p < at) {
        if (!decode(p, &hs)) return *why = "the function doesn't decode cleanly up to it", false;
        p += hs.len;
    }
    if (p != at) return *why = "it isn't the start of an instruction", false;
    // the replaced instructions
    while (p < at + 5) {
        if (!decode(p, &hs) || p + hs.len > end) return *why = "the 5 bytes it needs run past the end of the function", false;
        const unsigned char* t = nullptr;
        if (branch_of(p, hs, &t) == Branch::loop) return *why = "a LOOP/JRCXZ is among the replaced instructions", false;
        out->push_back({p, hs});
        p += hs.len;
    }
    const unsigned char* last = p;
    // no jump into (at, last) from anywhere in the function (every part of it)
    const RUNTIME_FUNCTION* prim = primary_of(base, f);
    for (std::size_t i = 0; i < n; ++i) {
        const RUNTIME_FUNCTION* g = &tab[i];
        if (g != prim && primary_of(base, g) != prim) continue;
        const auto* q    = reinterpret_cast<const unsigned char*>(base + g->BeginAddress);
        const auto* qend = reinterpret_cast<const unsigned char*>(base + g->EndAddress);
        while (q < qend) {
            if (!decode(q, &hs)) break; // data or padding at the end: nothing more to learn here
            const unsigned char* t = nullptr;
            if (branch_of(q, hs, &t) != Branch::none && t > at && t < last) {
                return *why = "the function jumps into the bytes the hook would replace (from " + where(q) + ")", false;
            }
            q += hs.len;
        }
    }
    return true;
}

// -- the thunk

// Memory for thunks, near the game (rel32 and the game's RVAs reach it), above its base.
struct Arena {
    char* mem;
    std::size_t used;
};
std::vector<Arena> g_arenas;
constexpr std::size_t kArenaSize = 0x10000;

char* arena_alloc(std::size_t size, const void* near_to, const char* base, uint32_t image_size) {
    size = (size + 15) & ~std::size_t{15};
    for (Arena& a : g_arenas)
        if (a.used + size <= kArenaSize && reaches(near_to, a.mem) && a.mem > base && a.mem + kArenaSize - base < 0x7FFF0000) {
            char* p = a.mem + a.used;
            a.used += size;
            return p;
        }
    char* mem = alloc_near(base + image_size, kArenaSize); // just past the image: above its base
    if (!mem || mem < base || !reaches(near_to, mem) || mem + kArenaSize - base > 0x7FFF0000) return nullptr;
    DWORD old = 0;
    std::memset(mem, 0xCC, kArenaSize);
    VirtualProtect(mem, kArenaSize, PAGE_EXECUTE_READ, &old);
    g_arenas.push_back({mem, size});
    return mem;
}

struct Code {
    std::vector<unsigned char> b;
    void u8(unsigned v) { b.push_back(static_cast<unsigned char>(v)); }
    void bytes(std::initializer_list<unsigned> v) {
        for (unsigned x : v) u8(x);
    }
    void u32(uint32_t v) {
        for (int i = 0; i < 4; ++i) u8((v >> (8 * i)) & 0xFF);
    }
    void u64(uint64_t v) {
        for (int i = 0; i < 8; ++i) u8((v >> (8 * i)) & 0xFF);
    }
    // mov [rsp+off], reg / mov reg, [rsp+off] (reg 0-15, off < 0x80)
    void gpr(bool store, int reg, uint8_t off) {
        u8(0x48 | (reg >= 8 ? 4 : 0));
        u8(store ? 0x89 : 0x8B);
        if (off == 0) bytes({static_cast<unsigned>(0x04 | ((reg & 7) << 3)), 0x24});
        else bytes({static_cast<unsigned>(0x44 | ((reg & 7) << 3)), 0x24, off});
    }
    // movdqu [rsp+off32], xmmN / movdqu xmmN, [rsp+off32]
    void xmm(bool store, int reg, uint32_t off) {
        u8(0xF3);
        if (reg >= 8) u8(0x44);
        bytes({0x0F, store ? 0x7Fu : 0x6Fu, static_cast<unsigned>(0x84 | ((reg & 7) << 3)), 0x24});
        u32(off);
    }
    void jmp_abs(const void* to) { // jmp [rip+0]; dq to
        bytes({0xFF, 0x25, 0, 0, 0, 0});
        u64(reinterpret_cast<uint64_t>(to));
    }
};

// GPR numbers in InfinityContext order: rax rcx rdx rbx rsp rbp rsi rdi r8..r15 (the encoding's own).
constexpr uint8_t kGprOff(int r) { return static_cast<uint8_t>(8 * r); }

// The replaced instructions, rewritten to run at `at_new` (where `code` will be copied).
bool relocate(const std::vector<Insn>& insns, char* at_new, Code* code, std::string* why) {
    const std::size_t start = code->b.size(); // at_new is where this offset lands
    for (const Insn& in : insns) {
        const auto* ip   = in.at;
        const hde64s& hs = in.hs;
        char* here       = at_new + (code->b.size() - start);
        const unsigned char* t = nullptr;
        const Branch br  = branch_of(ip, hs, &t);
        if (br == Branch::call) {
            if (reaches(here + 5, t)) {
                code->u8(0xE8);
                code->u32(static_cast<uint32_t>(reinterpret_cast<const char*>(t) - (here + 5)));
            } else { // call [rip+2]; jmp +8; dq t
                code->bytes({0xFF, 0x15, 2, 0, 0, 0, 0xEB, 8});
                code->u64(reinterpret_cast<uint64_t>(t));
            }
        } else if (br == Branch::jmp) {
            code->jmp_abs(t);
        } else if (br == Branch::jcc) {
            const unsigned cc = hs.opcode == 0x0F ? (hs.opcode2 & 0x0F) : (hs.opcode & 0x0F);
            code->bytes({0x70u | (cc ^ 1), 14}); // the opposite condition skips the jump
            code->jmp_abs(t);
        } else if (rip_relative(hs)) {
            const int disp_at = hs.len - imm_size(hs) - 4;
            int32_t disp      = 0;
            std::memcpy(&disp, ip + disp_at, 4);
            const char* target = reinterpret_cast<const char*>(ip) + hs.len + disp;
            const int64_t nd   = target - (here + hs.len);
            if (nd < INT32_MIN || nd > INT32_MAX) return *why = "a moved instruction's data is out of reach", false;
            const std::size_t at0 = code->b.size();
            code->b.insert(code->b.end(), ip, ip + hs.len);
            const auto d32 = static_cast<int32_t>(nd);
            std::memcpy(code->b.data() + at0 + disp_at, &d32, 4);
        } else {
            code->b.insert(code->b.end(), ip, ip + hs.len);
        }
    }
    return true;
}

// Builds the thunk for an instruction hook at `at`; `s` is its site (its address is in the code).
bool build_thunk(Site* s, const std::vector<Insn>& insns, const char* base, uint32_t image_size,
                 const RUNTIME_FUNCTION* fn, std::string* why) {
    constexpr std::size_t kPad = 256;
    char* const at   = s->at;
    const std::size_t replaced = static_cast<std::size_t>(insns.back().at + insns.back().hs.len - insns.front().at);
    // An upper bound: pads, save/restore (~500), moved code (each instruction at most 16 bytes,
    // or 16 for a rewritten branch), the jump back, unwind info.
    const std::size_t size = 2 * kPad + 1024 + 24 * insns.size() + 16 + 128;
    char* mem = arena_alloc(size, at, base, image_size);
    if (!mem) return *why = "no memory for its stub near that code", false;

    // With AVX, the upper halves of ymm0-15 are kept too (above the InfinityContext): the game may
    // hold 256-bit values across the hooked instruction, and a fn compiled for AVX may clobber
    // them (its 128-bit VEX instructions zero them).
#ifndef PF_AVX_INSTRUCTIONS_AVAILABLE
#define PF_AVX_INSTRUCTIONS_AVAILABLE 39
#endif
    const bool avx       = IsProcessorFeaturePresent(PF_AVX_INSTRUCTIONS_AVAILABLE) != 0;
    const uint32_t frame = kCtxSize + (avx ? 0x100 : 0);
    // vextractf128 [rsp+off], ymmN, 1 / vinsertf128 ymmN, ymmN, [rsp+off], 1
    auto ymm_hi = [](Code& k, bool store, int n, uint32_t off) {
        k.u8(0xC4);
        k.u8((n >= 8 ? 0x00 : 0x80) | 0x60 | 0x03);
        k.u8((store ? 0x78 : ((~n & 0xF) << 3)) | 0x04 | 0x01);
        k.u8(store ? 0x19 : 0x18);
        k.bytes({static_cast<unsigned>(0x84 | ((n & 7) << 3)), 0x24});
        k.u32(off);
        k.u8(1);
    };

    Code c;
    for (std::size_t i = 0; i < kPad; ++i) c.u8(0xCC);
    const std::size_t save_at = c.b.size();
    // prolog: pushfq; sub rsp, frame; mov [rsp+0x18], rbx; mov rbx, rsp
    c.u8(0x9C);
    c.bytes({0x48, 0x81, 0xEC});
    c.u32(frame);
    c.gpr(true, 3, kGprOff(3));
    c.bytes({0x48, 0x89, 0xE3});
    for (int r = 0; r < 16; ++r)
        if (r != 3 && r != 4) c.gpr(true, r, kGprOff(r));
    // ctx.rsp = rsp + frame + 8; ctx.rflags = the pushed flags
    c.bytes({0x48, 0x8D, 0x84, 0x24});
    c.u32(frame + 8);
    c.gpr(true, 0, kGprOff(4));
    c.bytes({0x48, 0x8B, 0x84, 0x24});
    c.u32(frame);
    c.bytes({0x48, 0x89, 0x84, 0x24});
    c.u32(0x80);
    for (int x = 0; x < 16; ++x) c.xmm(true, x, 0x90 + 16 * x);
    if (avx)
        for (int x = 0; x < 16; ++x) ymm_hi(c, true, x, kCtxSize + 16 * x);
    // and rsp, -16; sub rsp, 0x20; rcx = ctx; rdx = site; call dispatch
    c.bytes({0x48, 0x83, 0xE4, 0xF0, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x89, 0xD9, 0x48, 0xBA});
    c.u64(reinterpret_cast<uint64_t>(s));
    c.bytes({0x48, 0xB8});
    c.u64(reinterpret_cast<uint64_t>(&dispatch));
    c.bytes({0xFF, 0xD0});
    // restore: rsp = rbx; xmm; flags back into the pushed slot; GPRs; add rsp; popfq
    c.bytes({0x48, 0x89, 0xDC});
    for (int x = 0; x < 16; ++x) c.xmm(false, x, 0x90 + 16 * x);
    if (avx)
        for (int x = 0; x < 16; ++x) ymm_hi(c, false, x, kCtxSize + 16 * x);
    c.bytes({0x48, 0x8B, 0x84, 0x24});
    c.u32(0x80);
    c.bytes({0x48, 0x89, 0x84, 0x24});
    c.u32(frame);
    for (int r = 15; r >= 0; --r)
        if (r != 3 && r != 4 && r != 0) c.gpr(false, r, kGprOff(r));
    c.gpr(false, 0, kGprOff(0));
    c.gpr(false, 3, kGprOff(3));
    c.bytes({0x48, 0x81, 0xC4});
    c.u32(frame);
    c.u8(0x9D);
    // jmp over the pad to the moved code (rel32; patched in below)
    c.u8(0xE9);
    const std::size_t jmp_rel = c.b.size();
    c.u32(0);
    const std::size_t save_end = c.b.size();
    for (std::size_t i = 0; i < kPad; ++i) c.u8(0xCC);
    const std::size_t moved_at = c.b.size();
    {
        const auto rel = static_cast<uint32_t>(moved_at - save_end);
        std::memcpy(c.b.data() + jmp_rel, &rel, 4);
    }
    if (!relocate(insns, mem + moved_at, &c, why)) return false;
    c.jmp_abs(at + replaced);
    const std::size_t moved_end = c.b.size();
    while (c.b.size() & 3) c.u8(0xCC);
    // unwind info for the save/call part: no prolog; frame register rbx at offset 0; codes, in
    // the order they're undone: SET_FPREG, SAVE_NONVOL rbx at 0x18, ALLOC_LARGE 0x190, ALLOC_SMALL 8
    const std::size_t unw1 = c.b.size();
    c.bytes({0x01 | (kUnwChain << 3), 0, 6, 0x03});
    c.bytes({0, 0x03});        // UWOP_SET_FPREG
    c.bytes({0, 0x34, 3, 0});  // UWOP_SAVE_NONVOL rbx, offset 0x18 / 8
    c.bytes({0, 0x01, frame / 8 & 0xFF, frame / 8 >> 8}); // UWOP_ALLOC_LARGE (info 0), size / 8
    c.bytes({0, 0x02});        // UWOP_ALLOC_SMALL (info 0): 8 bytes
    c.u32(fn->BeginAddress);   // chained: the game function's entry
    c.u32(fn->EndAddress);
    c.u32(fn->UnwindData);
    // the moved code: no codes of its own, chained the same way
    const std::size_t unw2 = c.b.size();
    c.bytes({0x01 | (kUnwChain << 3), 0, 0, 0});
    c.u32(fn->BeginAddress);
    c.u32(fn->EndAddress);
    c.u32(fn->UnwindData);
    const std::size_t table = c.b.size();
    const auto rva = [&](std::size_t off) { return static_cast<uint32_t>(mem + off - base); };
    c.u32(rva(0));
    c.u32(rva(save_end));
    c.u32(rva(unw1));
    c.u32(rva(save_end));
    c.u32(rva(moved_end));
    c.u32(rva(unw2));
    if (c.b.size() > size) return *why = "its stub came out too big", false;
    DWORD old = 0;
    VirtualProtect(mem, c.b.size(), PAGE_EXECUTE_READWRITE, &old);
    std::memcpy(mem, c.b.data(), c.b.size());
    VirtualProtect(mem, c.b.size(), PAGE_EXECUTE_READ, &old);
    FlushInstructionCache(GetCurrentProcess(), mem, c.b.size());
    if (!RtlAddFunctionTable(reinterpret_cast<PRUNTIME_FUNCTION>(mem + table), 2, reinterpret_cast<DWORD64>(base)))
        plog("hooks: couldn't register the unwind info of the instruction hook at " + where(at) +
             " (stack walks through it may stop there)");
    s->thunk      = mem;
    s->thunk_size = moved_end;
    // the patch: jmp rel32 to the save part, then int3s over the rest of the replaced bytes
    s->patch.assign(replaced, 0xCC);
    s->patch[0]    = 0xE9;
    const auto rel = static_cast<int32_t>(mem + save_at - (at + 5));
    std::memcpy(s->patch.data() + 1, &rel, 4);
    return true;
}

InfinityHook* api_hook_instruction(InfinityMod* m, const char* name, void* at, InfinityInstructionFn fn, void* user) {
    const std::string what = "instruction " + where(at) + (name ? std::string(" (") + name + ")" : "");
    void* dummy = nullptr;
    if (!common_checks(m, what.c_str(), at, reinterpret_cast<void*>(fn), &dummy)) return nullptr;
    uint32_t image_size = 0;
    const char* image   = image_of(at, &image_size);
    if (!image) {
        log_line(m->name, "can't hook " + what + ": it isn't in the game or a loaded DLL");
        return nullptr;
    }
    settle(at);
    std::scoped_lock lk{g_mu};
    if (!executable(at)) {
        log_line(m->name, "can't hook " + what + ": it isn't code");
        return nullptr;
    }
    auto* p = static_cast<char*>(at);
    Site* s = find_site(kInstruction, p);
    if (!s) {
        std::vector<Insn> insns;
        const RUNTIME_FUNCTION* f = nullptr;
        std::string why;
        if (!instruction_range(image, reinterpret_cast<const unsigned char*>(p), &insns, &f, &why)) {
            log_line(m->name, "can't hook " + what + ": " + why);
            return nullptr;
        }
        char* hi = const_cast<char*>(reinterpret_cast<const char*>(insns.back().at + insns.back().hs.len));
        if (!check_bytes(m, what.c_str(), p, hi, false)) return nullptr;
        s = new_site(kInstruction, p, nullptr, p, hi);
        if (!build_thunk(s, insns, image, image_size, f, &why)) {
            log_line(m->name, "can't hook " + what + ": " + why);
            g_sites.pop_back();
            return nullptr;
        }
    }
    return add_hook(m, name, s, reinterpret_cast<void*>(fn), nullptr, user);
}

int api_enable_hooks(InfinityMod* m, InfinityHook* h) { return switch_hooks(m, h, true); }
int api_disable_hooks(InfinityMod* m, InfinityHook* h) { return switch_hooks(m, h, false); }

void* api_wait_for_code(uint32_t timeout_ms) {
    const ULONGLONG until = timeout_ms == 0xFFFFFFFFu ? ~0ull : GetTickCount64() + timeout_ms;
    for (;;) {
        if (code_ready()) return find_game();
        if (GetTickCount64() >= until) return nullptr;
        Sleep(50);
    }
}

} // namespace

void fill_hook_api(InfinityModApiV1& api) {
    watch_for_code();
    api.hook_function = api_hook_function;
    api.hook_call     = api_hook_call;
    api.hook_pointer  = api_hook_pointer;
    api.enable_hooks  = api_enable_hooks;
    api.disable_hooks = api_disable_hooks;
    api.wait_for_code = api_wait_for_code;
    api.patch_bytes      = api_patch_bytes;
    api.hook_instruction = api_hook_instruction;
}

// For crash reports: which hook's trampoline or stub `p` is in.
bool hook_at(const void* p, char* out, std::size_t n) {
    const auto a  = reinterpret_cast<uintptr_t>(p);
    const LONG nr = g_nrecords;
    for (LONG i = 0; i < nr && i < kMaxRecords; ++i)
        if (g_records[i].tramp && a >= g_records[i].tramp && a < g_records[i].tramp + 64) {
            std::snprintf(out, n, "the trampoline (copied game code) of %s", g_records[i].text);
            return true;
        }
    for (LONG i = 0; i < nr && i < kMaxRecords; ++i)
        if (g_records[i].code_lo && a >= g_records[i].code_lo && a < g_records[i].code_hi) {
            std::snprintf(out, n, "the stub of %s (saving registers, or the instructions it moved)", g_records[i].text);
            return true;
        }
    for (int i = 0; i < g_nblocks; ++i)
        if (a >= reinterpret_cast<uintptr_t>(g_blocks[i].code) &&
            a < reinterpret_cast<uintptr_t>(g_blocks[i].code) + 2 * kPage) {
            std::snprintf(out, n, "a platform hook stub");
            return true;
        }
    return false;
}

int hook_count() { return static_cast<int>(std::min<LONG>(static_cast<LONG>(g_nrecords), kMaxRecords)); }

const char* hook_line(int i, bool* enabled) {
    if (i < 0 || i >= hook_count()) return nullptr;
    *enabled = g_records[i].enabled != 0;
    return g_records[i].text;
}

} // namespace infinity_mod
