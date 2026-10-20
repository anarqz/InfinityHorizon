/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Is the player's Forza Horizon 2 Extra Options (RokkuDayo) in this process, and which?
   ExtraOptions 0.2.98 is a winmm.dll proxy next to azimuth.exe (Windows loads it for azimuth.exe's
   timeBeginPeriod import; Wine only with WINEDLLOVERRIDES="winmm.dll=n,b"). It patches the game
   right after our DllMain, so its state means something only from the first game callback on:
   fi_eo_detect runs there (runtime.c's fi_game_callback), once, and everything else reads the
   result. Only the module already mapped is looked at, never the file on disk:
   1. the module loaded from <folder of azimuth.exe>\winmm.dll, looked up by that full path
      (with ExtraOptions the process always has two modules named winmm.dll: its proxy, and the
      System32 one the proxy forwards to, and which of them a lookup by base name returns is up
      to the loader); only when that finds nothing, GetModuleHandleW("winmm.dll") and its folder.
      None in that folder is ABSENT (no winmm, or the system's: Wine without the override, or no
      ExtraOptions). Wine without the override loads its own winmm even for the file next to
      azimuth.exe and reports that file's path: its "Wine builtin DLL" header mark is ABSENT too;
   2. its export Fh2TweaksStatus, else FOREIGN_WINMM (another proxy);
   3. its VS_FIXEDFILEINFO (version resource 1): 0.2.98.0, else OTHER_VERSION;
   4. Fh2TweaksStatus(): "OK:..." is 0298_OK, anything else (DISABLED, REFUSED, INACTIVE) is
      0298_INACTIVE.
   Headlight and roof sync (carstate.c) runs only for 0298_OK, and not with [hook] CarState=off. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

static FiEo eo_result;
/* 0 not looked at yet, 1 looking, 2 known */
static volatile LONG eo_stage;
static char eo_line[400];

static wchar_t eo_lower(wchar_t c) {
    return c >= L'A' && c <= L'Z' ? (wchar_t)(c + 32) : c;
}

/* The length of the folder part of `path` (up to and with its last separator). */
static size_t eo_folder_length(const wchar_t *path) {
    size_t n = 0, folder = 0;
    for (; path[n]; ++n) {
        if (path[n] == L'\\' || path[n] == L'/') {
            folder = n + 1;
        }
    }
    return folder;
}

static int eo_same_folder(const wchar_t *a, const wchar_t *b) {
    size_t n = eo_folder_length(a), m = eo_folder_length(b);
    if (!n || n != m) {
        return 0;
    }
    for (size_t i = 0; i < n; ++i) {
        wchar_t x = eo_lower(a[i]), y = eo_lower(b[i]);
        if (x != y && !((x == L'/' || x == L'\\') && (y == L'/' || y == L'\\'))) {
            return 0;
        }
    }
    return 1;
}

static void eo_add(char *out, size_t cap, const char *text) {
    size_t n = text_length(out);
    while (text && *text && n + 1 < cap) {
        out[n++] = *text++;
    }
    out[n] = 0;
}

static void eo_number(char *out, size_t cap, unsigned value) {
    char digits[12];
    unsigned k = 0;
    do {
        digits[k++] = (char)('0' + value % 10);
        value /= 10;
    } while (value && k < sizeof(digits));
    char text[12];
    for (unsigned i = 0; i < k; ++i) {
        text[i] = digits[k - 1 - i];
    }
    text[k] = 0;
    eo_add(out, cap, text);
}

/* VS_FIXEDFILEINFO of `module`'s version resource: 1 found. */
static int eo_file_version(HMODULE module, unsigned *ms, unsigned *ls) {
    HRSRC info = FindResourceW(module, MAKEINTRESOURCEW(1), MAKEINTRESOURCEW(16) /* RT_VERSION */);
    HGLOBAL loaded = info ? LoadResource(module, info) : 0;
    const unsigned char *data = loaded ? LockResource(loaded) : 0;
    DWORD size = info ? SizeofResource(module, info) : 0;
    if (!data || size < 92) {
        return 0;
    }
    /* VS_VERSIONINFO: 6-byte header, L"VS_VERSION_INFO", padding to 4, then VS_FIXEDFILEINFO */
    for (DWORD at = 0; at + 16 <= size && at < 128; at += 4) {
        unsigned signature;
        memory_copy(&signature, data + at, 4);
        if (signature == 0xfeef04bdu) {
            memory_copy(ms, data + at + 8, 4);
            memory_copy(ls, data + at + 12, 4);
            return 1;
        }
    }
    return 0;
}

static void eo_compose_line(void) {
    eo_line[0] = 0;
    switch (eo_result.state) {
    case FI_EO_0298_OK:
        eo_add(eo_line, sizeof(eo_line), "EO: ExtraOptions 0.2.98 present and active (");
        eo_add(eo_line, sizeof(eo_line), eo_result.status);
        eo_add(eo_line, sizeof(eo_line), fi_eo_sync_enabled() ? "); headlight and roof sync on"
                                                               : "); headlight and roof sync off ([hook] CarState=off)");
        break;
    case FI_EO_0298_INACTIVE:
        eo_add(eo_line, sizeof(eo_line), "EO: ExtraOptions 0.2.98 present but not active (");
        eo_add(eo_line, sizeof(eo_line), eo_result.status);
        eo_add(eo_line, sizeof(eo_line), "); headlight and roof sync off");
        break;
    case FI_EO_OTHER_VERSION:
        eo_add(eo_line, sizeof(eo_line), "EO: ExtraOptions ");
        eo_add(eo_line, sizeof(eo_line), eo_result.version);
        eo_add(eo_line, sizeof(eo_line), " present; not the supported 0.2.98; headlight and roof sync off");
        break;
    case FI_EO_FOREIGN_WINMM:
        eo_add(eo_line, sizeof(eo_line), "EO: a winmm.dll that is not ExtraOptions is next to azimuth.exe; headlight "
                                         "and roof sync off");
        break;
    default:
        eo_add(eo_line, sizeof(eo_line),
               eo_result.on_disk ? "EO: winmm.dll is next to azimuth.exe but not in the process (on Wine/Proton it "
                                   "needs WINEDLLOVERRIDES=\"winmm.dll=n,b\"); headlight and roof sync off"
                                 : "EO: not present; headlight and roof sync off");
    }
}

void fi_eo_detect(void) {
    static wchar_t self[PATH_CAPACITY], winmm[PATH_CAPACITY], local[PATH_CAPACITY];
    if (InterlockedCompareExchange(&eo_stage, 1, 0) != 0) {
        return;
    }
    eo_result.state = FI_EO_ABSENT;
    eo_result.version[0] = eo_result.status[0] = 0;
    eo_result.on_disk = 0;
    DWORD n = GetModuleFileNameW(0, self, PATH_CAPACITY);
    size_t folder = n && n < PATH_CAPACITY ? eo_folder_length(self) : 0;
    if (folder && folder + 10 < PATH_CAPACITY) {
        memory_copy(local, self, folder * sizeof(wchar_t));
        memory_copy(local + folder, L"winmm.dll", sizeof(L"winmm.dll"));
        eo_result.on_disk = GetFileAttributesW(local) != INVALID_FILE_ATTRIBUTES;
    }
    /* by its full path first: ExtraOptions' proxy and the System32 winmm it forwards to share
       the base name, and GetModuleHandleW without a path may return either */
    HMODULE module = folder ? GetModuleHandleW(local) : 0;
    if (!module) {
        module = GetModuleHandleW(L"winmm.dll");
    }
    DWORD m = module ? GetModuleFileNameW(module, winmm, PATH_CAPACITY) : 0;
    char mark[16];
    int builtin = module && memory_read((const unsigned char *)module + 0x40, mark, sizeof(mark)) &&
                  memory_equal(mark, "Wine builtin DLL", sizeof(mark));
    if (module && !builtin && m && m < PATH_CAPACITY && folder && eo_same_folder(winmm, self)) {
        typedef const char *(*StatusFn)(void);
        StatusFn status = (StatusFn)(uintptr_t)GetProcAddress(module, "Fh2TweaksStatus");
        unsigned ms = 0, ls = 0;
        if (!status) {
            eo_result.state = FI_EO_FOREIGN_WINMM;
        } else if (!eo_file_version(module, &ms, &ls)) {
            eo_result.state = FI_EO_OTHER_VERSION;
            eo_add(eo_result.version, sizeof(eo_result.version), "(no version)");
        } else if (ms != 0x00000002u || ls != 0x00620000u) {
            eo_result.state = FI_EO_OTHER_VERSION;
            eo_number(eo_result.version, sizeof(eo_result.version), ms >> 16);
            eo_add(eo_result.version, sizeof(eo_result.version), ".");
            eo_number(eo_result.version, sizeof(eo_result.version), ms & 0xffffu);
            eo_add(eo_result.version, sizeof(eo_result.version), ".");
            eo_number(eo_result.version, sizeof(eo_result.version), ls >> 16);
        } else {
            memory_copy(eo_result.version, "0.2.98", 7);
            const char *text = status();
            size_t k = 0;
            char c = 0;
            /* its text, read safely: printable ASCII, up to the first line break */
            while (text && k + 1 < sizeof(eo_result.status) && memory_read(text + k, &c, 1) && c && c != '\r' &&
                   c != '\n') {
                eo_result.status[k++] = c >= 0x20 && c < 0x7f ? c : '?';
            }
            eo_result.status[k] = 0;
            eo_result.state = k >= 3 && memory_equal(eo_result.status, "OK:", 3) ? FI_EO_0298_OK : FI_EO_0298_INACTIVE;
        }
    }
    InterlockedExchange(&eo_stage, 2);
    eo_compose_line();
    log_append(eo_line);
    log_append(".\r\n");
}

/* ExtraOptions arms its loader at azimuth.exe's timeBeginPeriod call, sets its status to
   "Waiting for Azimuth to load its original TLS helper.", and patches the game when that load
   (ours) returns. A caller that is not a game callback (the account thread, runtime.c) waits
   here, at most `timeout_ms`, until that status has moved on; at once when there is no
   ExtraOptions in the process. */
void fi_eo_settle(unsigned timeout_ms) {
    static wchar_t self[PATH_CAPACITY], local[PATH_CAPACITY];
    DWORD n = GetModuleFileNameW(0, self, PATH_CAPACITY);
    size_t folder = n && n < PATH_CAPACITY ? eo_folder_length(self) : 0;
    if (!folder || folder + 10 >= PATH_CAPACITY) {
        return;
    }
    memory_copy(local, self, folder * sizeof(wchar_t));
    memory_copy(local + folder, L"winmm.dll", sizeof(L"winmm.dll"));
    HMODULE module = GetModuleHandleW(local);
    char mark[16];
    if (!module || (memory_read((const unsigned char *)module + 0x40, mark, sizeof(mark)) &&
                    memory_equal(mark, "Wine builtin DLL", sizeof(mark)))) {
        return;
    }
    typedef const char *(*StatusFn)(void);
    StatusFn status = (StatusFn)(uintptr_t)GetProcAddress(module, "Fh2TweaksStatus");
    for (unsigned waited = 0; status && waited < timeout_ms; waited += 50) {
        const char *text = status();
        char head[8];
        if (!text || !memory_read(text, head, sizeof(head)) || !memory_equal(head, "Waiting ", sizeof(head))) {
            return;
        }
        Sleep(50);
    }
}

const FiEo *fi_eo(void) {
    return eo_stage == 2 ? &eo_result : 0;
}

int fi_eo_sync_enabled(void) {
    return eo_stage == 2 && eo_result.state == FI_EO_0298_OK && fi_cfg()->carstate != FI_CARSTATE_OFF;
}

const wchar_t *fi_eo_summary(int *version_follows) {
    *version_follows = 0;
    if (eo_stage != 2) {
        return L"ExtraOptions: not looked at yet.";
    }
    switch (eo_result.state) {
    case FI_EO_0298_OK:
        return fi_eo_sync_enabled() ? L"ExtraOptions v0.2.98 found: headlight and roof sync on."
                                    : L"ExtraOptions v0.2.98 found; headlight and roof sync is off in Infinity.cfg (CarState=off).";
    case FI_EO_0298_INACTIVE:
        return L"ExtraOptions v0.2.98 is installed but not active (its FH2_ExtraOptions.log says why): headlight and roof sync off.";
    case FI_EO_OTHER_VERSION:
        *version_follows = 1;
        return L"ExtraOptions found, not v0.2.98 (headlight and roof sync needs v0.2.98): ";
    case FI_EO_FOREIGN_WINMM:
        return L"A winmm.dll that is not ExtraOptions is next to azimuth.exe: headlight and roof sync off.";
    default:
        return eo_result.on_disk ? L"ExtraOptions is installed but did not load (on Steam Deck or Linux the game needs WINEDLLOVERRIDES=\"winmm.dll=n,b\"): headlight and roof sync off."
                                 : L"ExtraOptions not installed: everything works except headlight and roof sync.";
    }
}

const char *fi_eo_line(void) {
    return eo_stage == 2 ? eo_line : "EO: not looked at yet (the game has not started)";
}
