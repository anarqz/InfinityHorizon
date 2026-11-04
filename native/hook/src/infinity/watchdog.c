/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   W-13: the hang watchdog. F-6 (2026-11-04): a host's game froze at a car meet invitation's
   countdown end; every game thread and the hook's own pipe thread sat in waits nobody signalled,
   and the only evidence was /proc on the Linux side (no user-mode stacks). This thread writes
   the evidence from inside the process when it happens.

   - The pipe thread beats (fi_watchdog_beat) every time it sends its 1 Hz Capture lease
     (launch.c launch_session); its end disarms (fi_watchdog_disarm). A beat missing for
     WD_STALL_MS is a stall: every thread's stack is written to infinity.log as module+offset,
     then the hook's locks with the thread that holds or waits for each. One dump per stall; while
     the same stall goes on, again after WD_REPEAT_MS, at most WD_DUMPS in all. A beat after a
     reported stall says how long it lasted.
   - Nothing here takes a hook lock, and nothing calls into the game, WinRT or the loader while
     the process may be stalled. The thread list comes from NtQuerySystemInformation into a
     buffer allocated at start. The lock table is written first, before any thread is touched.
     Each thread of this process (checked: a recycled id is never suspended) is suspended only
     for GetThreadContext and a copy of its stack window (ReadProcessMemory), then resumed; the
     watchdog never suspends itself, and the hook has no log-writer thread (every caller of
     log_append writes its own line, without a lock, core/logging.c). The unwind runs on that
     copy: each module's own exception directory replaces RtlLookupFunctionEntry (which takes
     the loader's function-table lock, which a frozen thread may hold), RtlVirtualUnwind (no
     lock) steps, and a frame register or stack pointer outside the copy ends the walk, so a
     thread that kept running can never make it read a wild address. Lines go out through
     infinity.log's handle opened at start (log_raw: WriteFile only), stamped from
     GetSystemTimeAsFileTime (UTC, no time zone lookup). Time is unbiased interrupt time, so a
     sleep of the PC is no stall.
   - The hook's SRW locks are watched when the DLL is built with FI_LOCK_WATCH (infinity.h maps
     Acquire/Release to fi_lock_* here): a fixed table of WD_LOCKS slots, filled lock-free, keeps
     each lock's exclusive owner, its shared count and last waiter. The dump reads it without
     taking anything. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define WD_STALL_MS 3000u
#define WD_REPEAT_MS 30000u
#define WD_DUMPS 3u
#define WD_POLL_MS 250u
#define WD_FRAMES 32u
#define WD_LOCKS 64u
#define WD_QUERY_SIZE (4u << 20)
#define WD_LINE 768u
#define WD_COPY_WINDOW (256u << 10) /* the most of a stack copied (from its stack pointer up) */
#define WD_COPY_SIZE (1u << 20)     /* the copy's buffer: past the window, zeros for a deep frame's reads */

/* The watchdog's clock in ms: unbiased interrupt time, which stops while the PC sleeps (review: a resume from sleep is
   never a stall). The beat and the lock table use it too. */
static ULONGLONG wd_now(void) {
    ULONGLONG time = 0;
    QueryUnbiasedInterruptTime(&time);
    return time / 10000u;
}

/* ------------------------------------------------------------- lock table */

typedef struct {
    SRWLOCK *volatile lock;
    const char *volatile name;
    volatile LONG owner;  /* the exclusive holder's thread id, 0 none */
    volatile LONG shared; /* shared holders now */
    volatile LONG reader; /* the last shared holder's thread id */
    volatile LONG waiter; /* the last thread that started waiting for it, 0 when it got it */
    volatile LONG64 since; /* the tick the exclusive holder took it */
} WdLock;
static WdLock wd_locks[WD_LOCKS];

static WdLock *wd_slot(SRWLOCK *lock, const char *name) {
    unsigned at = (unsigned)(((uintptr_t)lock >> 3) % WD_LOCKS);
    for (unsigned i = 0; i < WD_LOCKS; ++i) {
        WdLock *slot = &wd_locks[(at + i) % WD_LOCKS];
        SRWLOCK *seen = slot->lock;
        if (seen == lock) return slot;
        if (!seen) {
            if (!InterlockedCompareExchangePointer((void *volatile *)&slot->lock, lock, 0)) {
                slot->name = name;
                return slot;
            }
            if (slot->lock == lock) return slot;
        }
    }
    return 0; /* the table is full: the lock still works, unwatched */
}

void fi_lock_exclusive(SRWLOCK *lock, const char *name) {
    WdLock *slot = wd_slot(lock, name);
    LONG me = (LONG)GetCurrentThreadId();
    if (slot) InterlockedExchange(&slot->waiter, me);
    (AcquireSRWLockExclusive)(lock);
    if (slot) {
        InterlockedCompareExchange(&slot->waiter, 0, me);
        InterlockedExchange64(&slot->since, (LONG64)wd_now());
        InterlockedExchange(&slot->owner, me);
    }
}

void fi_unlock_exclusive(SRWLOCK *lock) {
    WdLock *slot = wd_slot(lock, 0);
    if (slot) InterlockedExchange(&slot->owner, 0);
    (ReleaseSRWLockExclusive)(lock);
}

void fi_lock_shared(SRWLOCK *lock, const char *name) {
    WdLock *slot = wd_slot(lock, name);
    LONG me = (LONG)GetCurrentThreadId();
    if (slot) InterlockedExchange(&slot->waiter, me);
    (AcquireSRWLockShared)(lock);
    if (slot) {
        InterlockedCompareExchange(&slot->waiter, 0, me);
        InterlockedIncrement(&slot->shared);
        InterlockedExchange(&slot->reader, me);
    }
}

void fi_unlock_shared(SRWLOCK *lock) {
    WdLock *slot = wd_slot(lock, 0);
    if (slot) InterlockedDecrement(&slot->shared);
    (ReleaseSRWLockShared)(lock);
}

/* ------------------------------------------------------------- NT layouts */

typedef LONG(NTAPI *WdQuerySystemFn)(ULONG, void *, ULONG, ULONG *);
typedef LONG(NTAPI *WdQueryThreadFn)(HANDLE, ULONG, void *, ULONG, ULONG *);
typedef PEXCEPTION_ROUTINE(NTAPI *WdUnwindFn)(DWORD, DWORD64, DWORD64, PRUNTIME_FUNCTION, PCONTEXT, PVOID *,
                                               PDWORD64, PKNONVOLATILE_CONTEXT_POINTERS);

/* SystemProcessInformation (5): the fields used, at their x64 offsets */
#define WD_PROC_NEXT 0u
#define WD_PROC_THREADS 4u
#define WD_PROC_ID 80u
#define WD_PROC_SIZE 256u
/* SYSTEM_THREAD_INFORMATION (x64, 80 bytes) */
#define WD_THREAD_SIZE 80u
#define WD_THREAD_START 32u
#define WD_THREAD_ID 48u
#define WD_THREAD_SWITCHES 64u
#define WD_THREAD_STATE 68u
#define WD_THREAD_WAIT 72u

static WdQuerySystemFn wd_query_system;
static WdQueryThreadFn wd_query_thread;
static WdUnwindFn wd_unwind;
static unsigned char *wd_query;
static unsigned char *wd_copy;  /* one thread's stack window at a time (WD_COPY_SIZE, from the start) */
static DWORD64 wd_copy_dirty;    /* how much of it the last thread used */
static HANDLE wd_file = INVALID_HANDLE_VALUE;
static DWORD wd_self;
static volatile LONG64 wd_beat;   /* the last beat's tick; 0 disarmed */
static volatile LONG wd_beater;   /* the beating (pipe) thread */
static volatile LONG wd_reported; /* the stall in progress was dumped at least once */
static volatile LONG64 wd_stall_since;
static unsigned wd_stalls;

/* ------------------------------------------------------------------ lines */

typedef struct {
    char text[WD_LINE];
    size_t size;
} WdLine;

static void wd_put(WdLine *line, const char *text) {
    while (*text && line->size + 3 < WD_LINE) line->text[line->size++] = *text++;
    line->text[line->size] = 0;
}

static void wd_dec(WdLine *line, unsigned long long value) {
    char digits[24];
    int n = 0;
    do {
        digits[n++] = (char)('0' + value % 10u);
        value /= 10u;
    } while (value && n < 24);
    while (n && line->size + 3 < WD_LINE) line->text[line->size++] = digits[--n];
    line->text[line->size] = 0;
}

static void wd_hex(WdLine *line, unsigned long long value) {
    static const char hex[] = "0123456789abcdef";
    char digits[16];
    int n = 0;
    do {
        digits[n++] = hex[value & 15u];
        value >>= 4;
    } while (value && n < 16);
    wd_put(line, "0x");
    while (n && line->size + 3 < WD_LINE) line->text[line->size++] = digits[--n];
    line->text[line->size] = 0;
}

/* "[UTC HH:MM:SS.mmm] WATCHDOG: " */
static void wd_begin(WdLine *line) {
    FILETIME now;
    GetSystemTimeAsFileTime(&now);
    unsigned long long ms = ((unsigned long long)now.dwHighDateTime << 32 | now.dwLowDateTime) / 10000u % 86400000u;
    unsigned parts[4] = {(unsigned)(ms / 3600000u), (unsigned)(ms / 60000u % 60u), (unsigned)(ms / 1000u % 60u),
                         (unsigned)(ms % 1000u)};
    line->size = 0;
    line->text[0] = 0;
    wd_put(line, "[UTC ");
    for (int i = 0; i < 4; ++i) {
        if (i == 3 && parts[i] < 100u) wd_put(line, "0");
        if (parts[i] < 10u) wd_put(line, "0");
        wd_dec(line, parts[i]);
        wd_put(line, i < 2 ? ":" : i == 2 ? "." : "] WATCHDOG: ");
    }
}

static void wd_end(WdLine *line) {
    line->text[line->size++] = '\r';
    line->text[line->size++] = '\n';
    line->text[line->size] = 0;
    log_raw(wd_file, line->text);
}

static int wd_read(const void *at, void *out, size_t size) {
    SIZE_T got = 0;
    return at && ReadProcessMemory(GetCurrentProcess(), at, out, size, &got) && got == size;
}

/* ---------------------------------------------------------------- modules */

typedef struct {
    unsigned char *base;
    DWORD size;           /* SizeOfImage */
    DWORD pdata, pdata_size; /* the exception directory */
    char name[40];
} WdModule;

/* The PE image `address` lies in (VirtualQuery's allocation base with an MZ/PE header): 1 found. */
static int wd_module(DWORD64 address, WdModule *module) {
    MEMORY_BASIC_INFORMATION info;
    if (!address || !VirtualQuery((const void *)(uintptr_t)address, &info, sizeof(info)) || info.State != MEM_COMMIT ||
        !info.AllocationBase) {
        return 0;
    }
    unsigned char *base = info.AllocationBase;
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    if (!wd_read(base, &dos, sizeof(dos)) || dos.e_magic != IMAGE_DOS_SIGNATURE || dos.e_lfanew <= 0 ||
        dos.e_lfanew > 0x1000 || !wd_read(base + dos.e_lfanew, &nt, sizeof(nt)) || nt.Signature != IMAGE_NT_SIGNATURE ||
        nt.OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC) {
        return 0;
    }
    module->base = base;
    module->size = nt.OptionalHeader.SizeOfImage;
    if (address - (DWORD64)(uintptr_t)base >= module->size) return 0;
    IMAGE_DATA_DIRECTORY exceptions = nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXCEPTION];
    IMAGE_DATA_DIRECTORY exports = nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXPORT];
    module->pdata = exceptions.VirtualAddress;
    module->pdata_size = exceptions.Size;
    module->name[0] = 0;
    IMAGE_EXPORT_DIRECTORY directory;
    if (exports.VirtualAddress && exports.VirtualAddress < module->size &&
        wd_read(base + exports.VirtualAddress, &directory, sizeof(directory)) && directory.Name &&
        directory.Name < module->size) {
        char name[40];
        size_t n = 0;
        if (wd_read(base + directory.Name, name, sizeof(name) - 1)) {
            while (n + 1 < sizeof(name) && name[n]) ++n; /* one read: the name sits inside the image */
        } else {
            while (n + 1 < sizeof(name) && wd_read(base + directory.Name + n, &name[n], 1) && name[n]) ++n;
        }
        name[n] = 0;
        memory_copy(module->name, name, n + 1);
    }
    return 1;
}

/* RtlLookupFunctionEntry's answer, from the module's own .pdata (binary search), with no lock. */
static int wd_function(const WdModule *module, DWORD64 address, RUNTIME_FUNCTION *function) {
    if (!module->pdata || module->pdata_size < sizeof(RUNTIME_FUNCTION) || module->pdata >= module->size) return 0;
    DWORD rva = (DWORD)(address - (DWORD64)(uintptr_t)module->base);
    size_t low = 0, high = module->pdata_size / sizeof(RUNTIME_FUNCTION);
    const unsigned char *table = module->base + module->pdata;
    while (low < high) {
        size_t middle = low + (high - low) / 2;
        RUNTIME_FUNCTION entry;
        if (!wd_read(table + middle * sizeof(entry), &entry, sizeof(entry))) return 0;
        if (rva < entry.BeginAddress) high = middle;
        else if (rva >= entry.EndAddress) low = middle + 1;
        else {
            *function = entry;
            return 1;
        }
    }
    return 0;
}

/* "name+0xoff" (the module's export name, else "image@0xbase"), or the bare address. */
static void wd_where(WdLine *line, DWORD64 address) {
    WdModule module;
    if (!wd_module(address, &module)) {
        wd_hex(line, address);
        return;
    }
    if (module.name[0]) wd_put(line, module.name);
    else {
        wd_put(line, "image@");
        wd_hex(line, (unsigned long long)(uintptr_t)module.base);
    }
    wd_put(line, "+");
    wd_hex(line, address - (DWORD64)(uintptr_t)module.base);
}

/* ----------------------------------------------------------------- stacks */

/* The thread's stack bounds from its TEB (NT_TIB StackBase, StackLimit): 1 known. */
static int wd_stack(HANDLE thread, DWORD64 *low, DWORD64 *high) {
    struct {
        LONG exit_status;
        LONG pad;
        void *teb;
        void *client[2];
        ULONG_PTR affinity;
        LONG priority, base_priority;
    } basic;
    NT_TIB tib;
    if (!wd_query_thread || wd_query_thread(thread, 0, &basic, sizeof(basic), 0) < 0 || !basic.teb ||
        !wd_read(basic.teb, &tib, sizeof(tib))) {
        return 0;
    }
    *low = (DWORD64)(uintptr_t)tib.StackLimit;
    *high = (DWORD64)(uintptr_t)tib.StackBase;
    return *low < *high;
}

/* A general register of `context` by its unwind number (0 RAX ... 15 R15). */
static DWORD64 *wd_register(CONTEXT *context, unsigned number) {
    DWORD64 *const registers[16] = {&context->Rax, &context->Rcx, &context->Rdx, &context->Rbx, &context->Rsp,
                                    &context->Rbp, &context->Rsi, &context->Rdi, &context->R8,  &context->R9,
                                    &context->R10, &context->R11, &context->R12, &context->R13, &context->R14,
                                    &context->R15};
    return registers[number & 15u];
}

static void wd_thread_stack(DWORD id, const unsigned char *entry) {
    WdLine line;
    wd_begin(&line);
    wd_put(&line, "thread ");
    wd_dec(&line, id);
    ULONG switches = 0, state = 0, wait = 0;
    memory_copy(&switches, entry + WD_THREAD_SWITCHES, sizeof(switches));
    memory_copy(&state, entry + WD_THREAD_STATE, sizeof(state));
    memory_copy(&wait, entry + WD_THREAD_WAIT, sizeof(wait));
    if (id == (DWORD)wd_beater) wd_put(&line, " (the pipe thread)");
    HANDLE thread = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, FALSE, id);
    /* review: a thread id from the snapshot may be another process's by now: never suspended */
    if (thread && GetProcessIdOfThread(thread) != GetCurrentProcessId()) {
        CloseHandle(thread);
        thread = 0;
    }
    if (!thread) {
        wd_put(&line, ": not opened");
        wd_end(&line);
        return;
    }
    void *start = 0; /* ThreadQuerySetWin32StartAddress (9): the snapshot's start address is often 0 */
    if (!wd_query_thread || wd_query_thread(thread, 9, &start, sizeof(start), 0) < 0) start = 0;
    wd_put(&line, " start ");
    wd_where(&line, (DWORD64)(uintptr_t)start);
    wd_put(&line, " state ");
    wd_dec(&line, state);
    wd_put(&line, " wait ");
    wd_dec(&line, wait);
    wd_put(&line, " switches ");
    wd_dec(&line, switches);
    CONTEXT context;
    for (size_t i = 0; i < sizeof(context); ++i) ((volatile unsigned char *)&context)[i] = 0;
    context.ContextFlags = CONTEXT_CONTROL | CONTEXT_INTEGER;
    DWORD64 low = 0, high = 0, copied = 0;
    int bounded = wd_stack(thread, &low, &high), captured = 0;
    /* suspended only for its context and a copy of its stack window (both system calls): the walk runs on the copy after
       the thread goes on, so a thread that keeps running can never make it read a stale or wild address */
    if (SuspendThread(thread) != (DWORD)-1) {
        captured = GetThreadContext(thread, &context) != 0;
        if (captured && bounded && wd_copy && context.Rsp >= low && context.Rsp < high) {
            copied = high - context.Rsp < WD_COPY_WINDOW ? high - context.Rsp : WD_COPY_WINDOW;
            if (!wd_read((const void *)(uintptr_t)context.Rsp, wd_copy, (size_t)copied)) copied = 0;
        }
        ResumeThread(thread);
    }
    CloseHandle(thread);
    if (!captured) {
        wd_put(&line, ": no context");
        wd_end(&line);
        return;
    }
    /* what a deeper frame reads past the copy (a large allocation) is zeros, never another thread's stack or a fault */
    for (DWORD64 i = copied; i < wd_copy_dirty; ++i) wd_copy[i] = 0;
    wd_copy_dirty = copied;
    /* the walk's addresses are the copy's: real = copy - delta */
    const DWORD64 origin = context.Rsp, copy = (DWORD64)(uintptr_t)wd_copy, delta = copy - origin;
    const DWORD64 copy_end = copy + copied;
    if (context.Rbp >= origin && context.Rbp < high) context.Rbp += delta;
    context.Rsp = copy;
    wd_put(&line, ":");
    for (unsigned frame = 0; frame < WD_FRAMES && context.Rip; ++frame) {
        if (line.size + 64 >= WD_LINE) {
            wd_end(&line);
            wd_begin(&line);
            wd_put(&line, "  thread ");
            wd_dec(&line, id);
            wd_put(&line, " more:");
        }
        wd_put(&line, " ");
        wd_where(&line, context.Rip);
        if (!copied || context.Rsp < copy || context.Rsp + 8 > copy_end) break;
        WdModule module;
        RUNTIME_FUNCTION function;
        if (wd_unwind && wd_module(context.Rip, &module) && wd_function(&module, context.Rip, &function)) {
            /* an indirect entry names another entry (RtlLookupFunctionEntry follows it; RtlVirtualUnwind does not) */
            if ((function.UnwindData & 1u) &&
                (!wd_read(module.base + (function.UnwindData & ~1u), &function, sizeof(function)) || (function.UnwindData & 1u))) {
                break;
            }
            unsigned char header[4];
            if (function.UnwindData >= module.size || !wd_read(module.base + function.UnwindData, header, sizeof(header))) break;
            /* a frame register: its value is a real stack address (taken into the copy) or the walk stops */
            if (header[3] & 15u) {
                DWORD64 *frame_register = wd_register(&context, header[3] & 15u);
                if (*frame_register >= origin && *frame_register < origin + copied) *frame_register += delta;
                if (*frame_register < copy || *frame_register >= copy_end) break;
            }
            void *handler = 0;
            DWORD64 establisher = 0;
            wd_unwind(0 /* UNW_FLAG_NHANDLER */, (DWORD64)(uintptr_t)module.base, context.Rip, &function, &context,
                      &handler, &establisher, 0);
        } else {
            /* a leaf, or code with no unwind data: its return address is at the stack pointer (in the copy) */
            context.Rip = *(const DWORD64 *)(uintptr_t)context.Rsp;
            context.Rsp += 8;
        }
        /* a restored frame pointer is a real stack address: kept in the copy's terms */
        if (context.Rbp >= origin && context.Rbp < origin + copied) context.Rbp += delta;
    }
    wd_end(&line);
}

static void wd_dump_locks(void) {
    WdLine line;
    unsigned watched = 0, idle = 0;
    ULONGLONG now = wd_now();
    for (unsigned i = 0; i < WD_LOCKS; ++i) {
        WdLock *slot = &wd_locks[i];
        if (!slot->lock) continue;
        ++watched;
        LONG owner = slot->owner, shared = slot->shared, waiter = slot->waiter;
        if (!owner && shared <= 0 && !waiter) {
            ++idle;
            continue;
        }
        wd_begin(&line);
        wd_put(&line, "lock ");
        wd_put(&line, slot->name ? slot->name : "?");
        if (owner) {
            wd_put(&line, " held by thread ");
            wd_dec(&line, (unsigned long long)owner);
            wd_put(&line, " for ");
            wd_dec(&line, now - (ULONGLONG)slot->since);
            wd_put(&line, " ms");
        }
        if (shared > 0) {
            wd_put(&line, " shared by ");
            wd_dec(&line, (unsigned long long)shared);
            wd_put(&line, " (last thread ");
            wd_dec(&line, (unsigned long long)slot->reader);
            wd_put(&line, ")");
        }
        if (waiter) {
            wd_put(&line, " waited for by thread ");
            wd_dec(&line, (unsigned long long)waiter);
        }
        wd_end(&line);
    }
    wd_begin(&line);
    wd_dec(&line, watched);
    wd_put(&line, watched ? " hook lock(s) watched, " : " hook locks watched (a build without FI_LOCK_WATCH), ");
    wd_dec(&line, idle);
    wd_put(&line, " free.");
    wd_end(&line);
}

static void wd_dump(ULONGLONG late, unsigned round) {
    WdLine line;
    wd_begin(&line);
    wd_put(&line, "no launcher heartbeat for ");
    wd_dec(&line, late);
    wd_put(&line, " ms (stall ");
    wd_dec(&line, wd_stalls);
    wd_put(&line, ", dump ");
    wd_dec(&line, round + 1u);
    wd_put(&line, " of ");
    wd_dec(&line, WD_DUMPS);
    wd_put(&line, "): the hook's locks, then every thread's stack (module+offset).");
    wd_end(&line);
    wd_dump_locks(); /* first: written before any thread is suspended (review M5) */
    ULONG needed = 0;
    LONG status = wd_query && wd_query_system ? wd_query_system(5, wd_query, WD_QUERY_SIZE, &needed) : -1;
    unsigned threads = 0;
    if (status >= 0) {
        DWORD me = GetCurrentProcessId();
        for (size_t at = 0; at + WD_PROC_SIZE <= WD_QUERY_SIZE;) {
            const unsigned char *process = wd_query + at;
            ULONG next = 0, count = 0;
            unsigned long long pid = 0;
            memory_copy(&next, process + WD_PROC_NEXT, sizeof(next));
            memory_copy(&count, process + WD_PROC_THREADS, sizeof(count));
            memory_copy(&pid, process + WD_PROC_ID, sizeof(pid));
            if (pid == me) {
                for (ULONG t = 0; t < count && at + WD_PROC_SIZE + (t + 1u) * WD_THREAD_SIZE <= WD_QUERY_SIZE; ++t) {
                    const unsigned char *entry = process + WD_PROC_SIZE + t * WD_THREAD_SIZE;
                    unsigned long long tid = 0;
                    memory_copy(&tid, entry + WD_THREAD_ID, sizeof(tid));
                    if ((DWORD)tid == wd_self) continue; /* never itself */
                    wd_thread_stack((DWORD)tid, entry);
                    ++threads;
                }
                break;
            }
            if (!next) break;
            at += next;
        }
    }
    if (!threads) {
        wd_begin(&line);
        wd_put(&line, "no thread list (NtQuerySystemInformation ");
        wd_hex(&line, (unsigned long long)(unsigned)status);
        wd_put(&line, ", ");
        wd_dec(&line, needed);
        wd_put(&line, " bytes needed).");
        wd_end(&line);
    }
}

static DWORD WINAPI wd_thread(void *unused) {
    (void)unused;
    LONG64 stalled = 0; /* the beat the current stall started after */
    unsigned dumps = 0;
    ULONGLONG next = 0;
    for (;;) {
        Sleep(WD_POLL_MS);
        LONG64 beat = InterlockedCompareExchange64(&wd_beat, 0, 0);
        ULONGLONG now = wd_now();
        if (!beat || now - (ULONGLONG)beat < WD_STALL_MS) {
            if (stalled && InterlockedExchange(&wd_reported, 0)) {
                WdLine line;
                wd_begin(&line);
                wd_put(&line, beat ? "the launcher heartbeat is back after " : "the pipe session ended after a stall of ");
                wd_dec(&line, now - (ULONGLONG)wd_stall_since);
                wd_put(&line, " ms.");
                wd_end(&line);
            }
            stalled = 0;
            continue;
        }
        if (beat != stalled) {
            stalled = beat;
            dumps = 0;
            next = 0;
            ++wd_stalls;
            InterlockedExchange64(&wd_stall_since, beat);
        }
        if (dumps >= WD_DUMPS || now < next) continue;
        wd_dump(now - (ULONGLONG)beat, dumps);
        InterlockedExchange(&wd_reported, 1);
        ++dumps;
        next = now + WD_REPEAT_MS;
    }
}

void fi_watchdog_beat(void) {
    InterlockedExchange(&wd_beater, (LONG)GetCurrentThreadId());
    InterlockedExchange64(&wd_beat, (LONG64)wd_now());
}

void fi_watchdog_disarm(void) {
    InterlockedExchange64(&wd_beat, 0);
}

/* At the launcher session's start (launch.c fi_launch_start): resolves what the dump needs and
   allocates its buffer now, so a stall never loads, allocates or opens anything. 1 running. */
int fi_watchdog_start(void) {
    static volatile LONG started;
    if (InterlockedExchange(&started, 1)) return 1;
    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    if (ntdll) {
        wd_query_system = (WdQuerySystemFn)(uintptr_t)GetProcAddress(ntdll, "NtQuerySystemInformation");
        wd_query_thread = (WdQueryThreadFn)(uintptr_t)GetProcAddress(ntdll, "NtQueryInformationThread");
        wd_unwind = (WdUnwindFn)(uintptr_t)GetProcAddress(ntdll, "RtlVirtualUnwind");
    }
    wd_query = VirtualAlloc(0, WD_QUERY_SIZE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    wd_copy = VirtualAlloc(0, WD_COPY_SIZE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    wd_file = log_open_append();
    HANDLE thread = CreateThread(0, 0, wd_thread, 0, 0, &wd_self);
    if (!thread) {
        log_append("WATCHDOG: the thread could not start; stalls are not dumped this session.\r\n");
        return 0;
    }
    CloseHandle(thread);
    log_append(wd_file != INVALID_HANDLE_VALUE && wd_query && wd_copy && wd_query_system && wd_unwind
                   ? "WATCHDOG: on; a launcher heartbeat missing 3 s dumps every thread's stack and the hook's locks.\r\n"
                   : "WATCHDOG: on, partly (no log handle, buffer or ntdll entry): a stall dump may be short.\r\n");
    return 1;
}
