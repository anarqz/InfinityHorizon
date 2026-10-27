/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* FH2Online cloud save: while the game plays an online profile, the server keeps the newest
   save revision of the account, so the profile continues on any PC it logs in from.

   Azimuth keeps a profile in <saves>\player-1\User_<XUID in hex>\revisions\<16 hex digits>\
   (<saves> is data\saves up to 0.9.8; 0.9.9 moved it into a folder per game, see
   saves_folder): a "commit" file (the SHA-256, size and name of each file under data\, checked when
   the game loads it) and data\*. A revision is bundled as is, each file compressed by
   azimuth_codec.c ("FH2SAVE1", see server/src/db/saves.rs), and sent to PUT /infinity/v1/save.
   Uploads use method 2; downloads also take method 1, the XPRESS Huffman of ntdll that older
   hooks sent from Windows (Wine has neither, so the hook decodes both itself).

   At start (on the account thread, before the game asks for the XUID and opens the save):
   - the server has the save this PC has: nothing to do;
   - the local save is the one last synced (SaveCommit in FH2Online_Accounts.ini) and the
     server's is another: the profile was played elsewhere, the server's revision is written
     here as a new, higher revision;
   - the server still has the last synced save and the local one is newer (the last session's
     revisions after the watcher's last look, or written as the game closed): it is uploaded,
     and the server's save gates measure it; refused, the server's save comes back as below;
   - both changed, or the server says to restore its save: the server wins; the local revision
     is moved to saves\fh2online-backup.
   A session that starts without the server forgets SaveCommit, so what it saves never reaches
   the server: the next start puts it aside for the server's save.
   While playing, a thread uploads each new revision (checked every minute).
   A downloaded bundle is installed only if every file matches the commit's SHA-256 and size,
   and only the fixed file names below are ever written. */
#include "codec.h"
#include "infinity.h"
#include "../core/logging.h"
#include "../core/sha256.h"
#include "../core/support.h"

#define CLOUD_PATH 1024
#define CLOUD_MAX_BUNDLE (512u << 10) /* as the server takes (db/saves.rs) */
#define CLOUD_MAX_FILE (8u << 20)
#define CLOUD_POLL_MS 60000u

typedef HANDLE(WINAPI *FindFirstFileWFn)(LPCWSTR, WIN32_FIND_DATAW *);
typedef BOOL(WINAPI *FindNextFileWFn)(HANDLE, WIN32_FIND_DATAW *);
typedef BOOL(WINAPI *FindCloseFn)(HANDLE);
typedef BOOL(WINAPI *CreateDirectoryWFn)(LPCWSTR, void *);
typedef BOOL(WINAPI *MoveFileExWFn)(LPCWSTR, LPCWSTR, DWORD);
typedef BOOL(WINAPI *WritePrivateProfileStringWFn)(LPCWSTR, LPCWSTR, LPCWSTR, LPCWSTR);
typedef void(WINAPI *SleepFn)(DWORD);

static const char *const cloud_files[] = {"commit",          "data/BinarySaveState", "data/PlayerDatabase",
                                          "data/ProfileData", "data/SaveState",       "data/TransactionLogFile",
                                          "data/VersionFlags"};
#define CLOUD_FILE_COUNT (sizeof(cloud_files) / sizeof(cloud_files[0]))

static FindFirstFileWFn cloud_find_first;
static FindNextFileWFn cloud_find_next;
static FindCloseFn cloud_find_close;
static CreateDirectoryWFn cloud_create_directory;
static MoveFileExWFn cloud_move;
static WritePrivateProfileStringWFn cloud_write_profile;
static SleepFn cloud_sleep;

static wchar_t cloud_saves[CLOUD_PATH];    /* data\saves (0.9.9: see saves_folder) */
static wchar_t cloud_profile[CLOUD_PATH];  /* <saves>\player-1\User_<hex> */
static wchar_t cloud_ini[CLOUD_PATH];
static wchar_t cloud_section[300];
static wchar_t cloud_headers[200];         /* X-Fh2-Token: ... */
static wchar_t cloud_synced[72];           /* SaveCommit: what server and PC last agreed on */
static void *cloud_workspace;              /* codec_lz_encode's hash table */

static int cloud_resolve(void);

/* ------------------------------------------------------------- helpers */

static void *cloud_alloc(size_t size) {
    return size ? HeapAlloc(GetProcessHeap(), 0, size) : 0;
}

static void cloud_free(void *p) {
    if (p) {
        HeapFree(GetProcessHeap(), 0, p);
    }
}

static size_t wlen(const wchar_t *t) {
    size_t n = 0;
    while (t && t[n]) {
        ++n;
    }
    return n;
}

/* out = a + b (+ c), within CLOUD_PATH. */
static int wjoin(wchar_t *out, const wchar_t *a, const wchar_t *b, const wchar_t *c) {
    size_t n = 0;
    const wchar_t *parts[3] = {a, b, c};
    for (int i = 0; i < 3; ++i) {
        for (const wchar_t *p = parts[i]; p && *p; ++p) {
            if (n + 1 >= CLOUD_PATH) {
                return 0;
            }
            out[n++] = *p;
        }
    }
    out[n] = 0;
    return 1;
}

/* cloud_saves = the folder Azimuth keeps the saves in, from `bin` (data\bin). 0.9.9 moved them
   from data\saves into a folder per game under data\titles, named after FH2's title id
   (1A96545B), and 0.9.10 keeps them there: the first of those names that exists, else
   data\saves. */
static int saves_folder(const wchar_t *bin) {
    static wchar_t joined[CLOUD_PATH];
    static const wchar_t *const per_game[] = {L"\\..\\titles\\1A96545B\\saves", L"\\..\\titles\\title-1a96545b\\saves"};
    AzimuthBuild build = azimuth_host_build((const unsigned char *)GetModuleHandleW(0));
    if (build == AZIMUTH_099 || build == AZIMUTH_0910) {
        for (size_t i = 0; i < sizeof(per_game) / sizeof(per_game[0]); ++i) {
            DWORD attributes;
            if (wjoin(joined, bin, per_game[i], 0) && GetFullPathNameW(joined, CLOUD_PATH, cloud_saves, 0) &&
                (attributes = GetFileAttributesW(cloud_saves)) != INVALID_FILE_ATTRIBUTES &&
                (attributes & FILE_ATTRIBUTE_DIRECTORY)) {
                return 1;
            }
        }
    }
    return wjoin(joined, bin, L"\\..\\saves", 0) && GetFullPathNameW(joined, CLOUD_PATH, cloud_saves, 0);
}

/* "data/X" -> L"data\\X" */
static void file_name_wide(const char *name, wchar_t *out, size_t cap) {
    size_t n = 0;
    for (; name[n] && n + 1 < cap; ++n) {
        out[n] = name[n] == '/' ? L'\\' : (wchar_t)name[n];
    }
    out[n] = 0;
}

static void hex16(unsigned long long value, wchar_t out[17]) {
    static const wchar_t digits[] = L"0123456789abcdef";
    for (int i = 15; i >= 0; --i) {
        out[i] = digits[value & 15];
        value >>= 4;
    }
    out[16] = 0;
}

static void sha_hex(const unsigned char *data, size_t size, char out[65]) {
    static const char digits[] = "0123456789abcdef";
    unsigned char digest[32];
    Sha256Context hash;
    sha256_initialize(&hash);
    sha256_update(&hash, data, size);
    sha256_finalize(&hash, digest);
    for (int i = 0; i < 32; ++i) {
        out[2 * i] = digits[digest[i] >> 4];
        out[2 * i + 1] = digits[digest[i] & 15];
    }
    out[64] = 0;
}

static void cloud_log(const char *a, const char *b) {
    static char line[400];
    size_t n = 0;
    const char *parts[3] = {"CLOUD: ", a, b};
    for (int i = 0; i < 3; ++i) {
        for (const char *p = parts[i]; p && *p && n + 3 < sizeof(line); ++p) {
            line[n++] = *p;
        }
    }
    line[n++] = '\r';
    line[n++] = '\n';
    line[n] = 0;
    log_append(line);
}

/* Whole file into a new buffer (NULL when missing or larger than `max`). */
static unsigned char *read_file(const wchar_t *path, size_t max, size_t *size) {
    HANDLE file = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0,
                              OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
    LARGE_INTEGER length;
    unsigned char *data = 0;
    *size = 0;
    if (file == INVALID_HANDLE_VALUE) {
        return 0;
    }
    if (GetFileSizeEx(file, &length) && length.QuadPart >= 0 && (unsigned long long)length.QuadPart <= max &&
        (data = cloud_alloc((size_t)length.QuadPart + 1)) != 0) {
        size_t done = 0;
        while (done < (size_t)length.QuadPart) {
            DWORD got = 0;
            if (!ReadFile(file, data + done, (DWORD)((size_t)length.QuadPart - done), &got, 0) || !got) {
                break;
            }
            done += got;
        }
        if (done == (size_t)length.QuadPart) {
            *size = done;
        } else {
            cloud_free(data);
            data = 0;
        }
    }
    CloseHandle(file);
    return data;
}

static int write_file(const wchar_t *path, const unsigned char *data, size_t size) {
    HANDLE file = CreateFileW(path, GENERIC_WRITE, 0, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, 0);
    size_t done = 0;
    if (file == INVALID_HANDLE_VALUE) {
        return 0;
    }
    while (done < size) {
        DWORD wrote = 0;
        if (!WriteFile(file, data + done, (DWORD)(size - done), &wrote, 0) || !wrote) {
            break;
        }
        done += wrote;
    }
    CloseHandle(file);
    return done == size;
}

/* ------------------------------------------------------------ revisions */

typedef struct {
    int found;
    unsigned long long number;
    wchar_t path[CLOUD_PATH]; /* ...\revisions\<hex> */
    char commit[65];          /* SHA-256 of its commit file */
} CloudRevision;

static int parse_hex16(const wchar_t *name, unsigned long long *value) {
    unsigned long long v = 0;
    for (int i = 0; i < 16; ++i) {
        wchar_t c = name[i];
        int d = c >= L'0' && c <= L'9' ? c - L'0' : c >= L'a' && c <= L'f' ? c - L'a' + 10
                : c >= L'A' && c <= L'F' ? c - L'A' + 10 : -1;
        if (d < 0) {
            return 0;
        }
        v = v << 4 | (unsigned)d;
    }
    *value = v;
    return name[16] == 0;
}

/* -------------------------------------- revisions without a commit at start */
/* The player's ExtraOptions 0.2.98 (DisableAzimuthSaveChecksums=1, its default) writes a
   "commit" for every revision under the saves folder that has none, right after our DllMain,
   from the bytes as they are (adapters/azimuth_markers.c). A revision a crash or a suspend left
   half written, which Azimuth would skip, then becomes the newest one. So the revisions without
   a commit at DLL_PROCESS_ATTACH (before ExtraOptions, and before the game writes anything) are
   noted: none of them is ever taken as a save or a creation this session, whatever they have
   later, and the online profile's are moved to saves\fh2online-backup before the game opens its
   save (cloud_put_aside_unsealed), so the game plays the last complete one, as without
   ExtraOptions. */
#define CLOUD_UNSEALED_MAX 32
static struct {
    unsigned long long number;
    wchar_t container[CLOUD_PATH]; /* ...\player-1\User_<hex> (the folder that has revisions\) */
} cloud_unsealed[CLOUD_UNSEALED_MAX];
static unsigned cloud_unsealed_count;

static wchar_t cloud_wlower(wchar_t c) {
    return c >= L'A' && c <= L'Z' ? (wchar_t)(c + 32) : c == L'/' ? L'\\' : c;
}

static int cloud_same_path(const wchar_t *a, const wchar_t *b) {
    size_t n = wlen(a);
    if (n != wlen(b)) {
        return 0;
    }
    for (size_t i = 0; i < n; ++i) {
        if (cloud_wlower(a[i]) != cloud_wlower(b[i])) {
            return 0;
        }
    }
    return 1;
}

/* Revision `number` of the container `base` had no commit when the game started. */
static int cloud_was_unsealed(const wchar_t *base, unsigned long long number) {
    for (unsigned i = 0; i < cloud_unsealed_count; ++i) {
        if (cloud_unsealed[i].number == number && cloud_same_path(cloud_unsealed[i].container, base)) {
            return 1;
        }
    }
    return 0;
}

/* Notes the revisions of `container` (a folder with revisions\) that have no commit. */
static void cloud_note_container(const wchar_t *container) {
    static wchar_t pattern[CLOUD_PATH], commit_path[CLOUD_PATH];
    static WIN32_FIND_DATAW entry;
    if (!wjoin(pattern, container, L"\\revisions\\*", 0)) {
        return;
    }
    HANDLE find = cloud_find_first(pattern, &entry);
    if (find == INVALID_HANDLE_VALUE) {
        return;
    }
    do {
        unsigned long long number;
        if (!(entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || !parse_hex16(entry.cFileName, &number) ||
            !wjoin(commit_path, container, L"\\revisions\\", entry.cFileName) ||
            !wjoin(commit_path, commit_path, L"\\commit", 0) ||
            GetFileAttributesW(commit_path) != INVALID_FILE_ATTRIBUTES) {
            continue;
        }
        if (cloud_unsealed_count < CLOUD_UNSEALED_MAX) {
            cloud_unsealed[cloud_unsealed_count].number = number;
            wjoin(cloud_unsealed[cloud_unsealed_count].container, container, 0, 0);
            ++cloud_unsealed_count;
        }
    } while (cloud_find_next(find, &entry));
    cloud_find_close(find);
}

void azimuth_cloud_note_unsealed(const wchar_t *bin) {
    static wchar_t pattern[CLOUD_PATH], level0[CLOUD_PATH], pattern1[CLOUD_PATH], container[CLOUD_PATH];
    static WIN32_FIND_DATAW entry0, entry1;
    if (!cloud_resolve() || !saves_folder(bin) || !wjoin(pattern, cloud_saves, L"\\*", 0)) {
        return;
    }
    /* <saves>\<player>\<container>\revisions\<16 hex>, the depth ExtraOptions repairs at */
    HANDLE find0 = cloud_find_first(pattern, &entry0);
    if (find0 == INVALID_HANDLE_VALUE) {
        return;
    }
    do {
        if (!(entry0.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || entry0.cFileName[0] == L'.' ||
            !wjoin(level0, cloud_saves, L"\\", entry0.cFileName) || !wjoin(pattern1, level0, L"\\*", 0)) {
            continue;
        }
        HANDLE find1 = cloud_find_first(pattern1, &entry1);
        if (find1 == INVALID_HANDLE_VALUE) {
            continue;
        }
        do {
            if ((entry1.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) && entry1.cFileName[0] != L'.' &&
                wjoin(container, level0, L"\\", entry1.cFileName)) {
                cloud_note_container(container);
            }
        } while (cloud_find_next(find1, &entry1));
        cloud_find_close(find1);
    } while (cloud_find_next(find0, &entry0));
    cloud_find_close(find0);
    if (cloud_unsealed_count) {
        char line[96] = "CLOUD: ";
        size_t n = 7;
        unsigned count = cloud_unsealed_count;
        char digits[4];
        unsigned k = 0;
        do {
            digits[k++] = (char)('0' + count % 10);
            count /= 10;
        } while (count && k < sizeof(digits));
        while (k) {
            line[n++] = digits[--k];
        }
        line[n] = 0;
        log_append(line);
        log_append(cloud_unsealed_count == CLOUD_UNSEALED_MAX
                       ? " (or more) save revisions had no commit at start; none of them is used or uploaded.\r\n"
                       : " save revision(s) had no commit at start; none of them is used or uploaded.\r\n");
    }
}

/* The online profile's revisions noted above go to saves\fh2online-backup\<profile>-<rev>-unsealed
   (before the game opens its save). The player's ExtraOptions may be writing a commit into one
   at this moment: a move that fails is tried again a few times; the revision is not used either
   way. */
static void cloud_put_aside_unsealed(void) {
    static wchar_t source[CLOUD_PATH], backup[CLOUD_PATH], target[CLOUD_PATH];
    const wchar_t *profile = cloud_profile + wlen(cloud_profile);
    while (profile > cloud_profile && profile[-1] != L'\\') {
        --profile;
    }
    for (unsigned i = 0; i < cloud_unsealed_count; ++i) {
        wchar_t name[17];
        if (!cloud_same_path(cloud_unsealed[i].container, cloud_profile)) {
            continue;
        }
        hex16(cloud_unsealed[i].number, name);
        if (!wjoin(source, cloud_profile, L"\\revisions\\", name) ||
            !wjoin(backup, cloud_saves, L"\\fh2online-backup", 0) || !wjoin(target, backup, L"\\", profile) ||
            !wjoin(target, target, L"-", name) || !wjoin(target, target, L"-unsealed", 0)) {
            continue;
        }
        if (GetFileAttributesW(source) == INVALID_FILE_ATTRIBUTES) {
            continue; /* moved already */
        }
        cloud_create_directory(backup, 0);
        int moved = 0;
        for (int attempt = 0; attempt < 5 && !(moved = cloud_move(source, target, 0) != 0); ++attempt) {
            cloud_sleep(200);
        }
        char hex[17];
        for (int k = 0; k < 17; ++k) {
            hex[k] = (char)name[k];
        }
        cloud_log(moved ? "a revision without a commit at start (a crash mid-save, or added by hand; ExtraOptions' "
                          "DisableAzimuthSaveChecksums=1 seals such a one as it is) is kept in "
                          "saves\\fh2online-backup, not played or uploaded: "
                        : "a revision without a commit at start could not be moved aside (not uploaded either): ",
                  hex);
    }
}

/* The newest revision with a commit file of the container in `base` (one without a commit at
   start never counts); `highest` gets the highest number seen at all. */
static void newest_revision_in(const wchar_t *base, CloudRevision *out, unsigned long long *highest) {
    static wchar_t pattern[CLOUD_PATH], commit_path[CLOUD_PATH];
    static WIN32_FIND_DATAW entry;
    out->found = 0;
    if (highest) {
        *highest = 0;
    }
    if (!wjoin(pattern, base, L"\\revisions\\*", 0)) {
        return;
    }
    HANDLE find = cloud_find_first(pattern, &entry);
    if (find == INVALID_HANDLE_VALUE) {
        return;
    }
    unsigned long long best = 0;
    int have = 0;
    do {
        unsigned long long number;
        if (!(entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || !parse_hex16(entry.cFileName, &number)) {
            continue;
        }
        if (highest && number > *highest) {
            *highest = number;
        }
        wchar_t name[17];
        hex16(number, name);
        if (!wjoin(commit_path, base, L"\\revisions\\", name) || !wjoin(commit_path, commit_path, L"\\commit", 0)) {
            continue;
        }
        HANDLE file = CreateFileW(commit_path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0,
                                  OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
        if (file == INVALID_HANDLE_VALUE) {
            continue;
        }
        CloseHandle(file);
        if (cloud_was_unsealed(base, number)) {
            continue; /* sealed after the start (by the player's ExtraOptions): not a save of ours */
        }
        if (!have || number > best) {
            best = number;
            have = 1;
        }
    } while (cloud_find_next(find, &entry));
    cloud_find_close(find);
    if (!have) {
        return;
    }
    wchar_t name[17];
    hex16(best, name);
    if (!wjoin(out->path, base, L"\\revisions\\", name) || !wjoin(commit_path, out->path, L"\\commit", 0)) {
        return;
    }
    size_t size;
    unsigned char *commit = read_file(commit_path, 1 << 16, &size);
    if (!commit) {
        return;
    }
    sha_hex(commit, size, out->commit);
    cloud_free(commit);
    out->number = best;
    out->found = 1;
}

/* The profile's own newest revision. */
static void newest_revision(CloudRevision *out, unsigned long long *highest) {
    newest_revision_in(cloud_profile, out, highest);
}

/* ---------------------------------------------------------------- bundle */

static void put32(unsigned char *p, unsigned v) {
    for (int i = 0; i < 4; ++i) {
        p[i] = (unsigned char)(v >> (8 * i));
    }
}

static unsigned get32(const unsigned char *p) {
    return (unsigned)p[0] | (unsigned)p[1] << 8 | (unsigned)p[2] << 16 | (unsigned)p[3] << 24;
}

/* The revision as an FH2SAVE1 bundle (caller frees), or NULL. */
static unsigned char *bundle_revision(const CloudRevision *revision, size_t *size) {
    static wchar_t path[CLOUD_PATH], name[64];
    unsigned char *files[CLOUD_FILE_COUNT] = {0};
    size_t sizes[CLOUD_FILE_COUNT] = {0}, total = 20;
    unsigned char *bundle = 0;
    unsigned count = 0;
    *size = 0;
    for (unsigned i = 0; i < CLOUD_FILE_COUNT; ++i) {
        file_name_wide(cloud_files[i], name, ARRAY_COUNT(name));
        if (wjoin(path, revision->path, L"\\", name)) {
            files[i] = read_file(path, CLOUD_MAX_FILE, &sizes[i]);
        }
        if (files[i]) {
            total += 2 + text_length(cloud_files[i]) + 9 + sizes[i];
            ++count;
        } else if (i == 0) {
            goto done; /* no commit: not a complete revision */
        }
    }
    bundle = cloud_alloc(total);
    if (!bundle) {
        goto done;
    }
    memory_copy(bundle, "FH2SAVE1", 8);
    put32(bundle + 8, (unsigned)revision->number);
    put32(bundle + 12, (unsigned)(revision->number >> 32));
    put32(bundle + 16, count);
    size_t at = 20;
    for (unsigned i = 0; i < CLOUD_FILE_COUNT; ++i) {
        if (!files[i]) {
            continue;
        }
        size_t name_length = text_length(cloud_files[i]);
        bundle[at] = (unsigned char)name_length;
        bundle[at + 1] = 0;
        memory_copy(bundle + at + 2, cloud_files[i], name_length);
        at += 2 + name_length;
        unsigned char *header = bundle + at;
        at += 9;
        size_t packed = 0;
        int method = 0;
        if (sizes[i] >= 64 && cloud_workspace &&
            (packed = codec_lz_encode(files[i], sizes[i], bundle + at, sizes[i] - 1, cloud_workspace)) != 0) {
            method = 2;
        } else {
            memory_copy(bundle + at, files[i], sizes[i]);
            packed = sizes[i];
        }
        put32(header, (unsigned)sizes[i]);
        put32(header + 4, (unsigned)packed);
        header[8] = (unsigned char)method;
        at += packed;
    }
    *size = at;
done:
    for (unsigned i = 0; i < CLOUD_FILE_COUNT; ++i) {
        cloud_free(files[i]);
    }
    return bundle;
}

/* "<sha256> <size> <name>" lines of a commit: does `data` match the entry for `name`? */
static int commit_lists(const unsigned char *commit, size_t commit_size, const char *name, const unsigned char *data,
                        size_t size) {
    char want[65];
    sha_hex(data, size, want);
    size_t name_length = text_length(name);
    for (size_t at = 0; at < commit_size;) {
        size_t end = at;
        while (end < commit_size && commit[end] != '\n') {
            ++end;
        }
        /* 64 hex, space, decimal size, space, name */
        if (end - at > 66 && memory_equal(commit + at, want, 64) && commit[at + 64] == ' ') {
            size_t p = at + 65, listed = 0;
            while (p < end && commit[p] >= '0' && commit[p] <= '9') {
                listed = listed * 10 + (size_t)(commit[p++] - '0');
            }
            if (p < end && commit[p] == ' ' && end - p - 1 == name_length && memory_equal(commit + p + 1, name, name_length) &&
                listed == size) {
                return 1;
            }
        }
        at = end + 1;
    }
    return 0;
}

/* Unpacks a server bundle into `directory` (a new folder). Every data file must be listed in
   the bundle's commit with its SHA-256 and size. Returns the revision number, or 0. */
static unsigned long long install_bundle(const unsigned char *bundle, size_t size, const wchar_t *directory) {
    static wchar_t path[CLOUD_PATH], name[64];
    unsigned char *out[CLOUD_FILE_COUNT] = {0};
    size_t out_size[CLOUD_FILE_COUNT] = {0};
    unsigned long long revision = 0, result = 0;
    if (size < 20 || !memory_equal(bundle, "FH2SAVE1", 8)) {
        return 0;
    }
    revision = (unsigned long long)get32(bundle + 8) | (unsigned long long)get32(bundle + 12) << 32;
    unsigned count = get32(bundle + 16);
    size_t at = 20;
    if (!count || count > CLOUD_FILE_COUNT) {
        return 0;
    }
    for (unsigned k = 0; k < count; ++k) {
        if (at + 2 > size) {
            goto done;
        }
        size_t name_length = bundle[at] | (size_t)bundle[at + 1] << 8;
        at += 2;
        if (at + name_length + 9 > size) {
            goto done;
        }
        int index = -1;
        for (unsigned i = 0; i < CLOUD_FILE_COUNT; ++i) {
            if (text_length(cloud_files[i]) == name_length && memory_equal(cloud_files[i], bundle + at, name_length)) {
                index = (int)i;
            }
        }
        at += name_length;
        unsigned raw = get32(bundle + at), stored = get32(bundle + at + 4);
        int method = bundle[at + 8];
        at += 9;
        if (index < 0 || out[index] || raw > CLOUD_MAX_FILE || stored > size - at || method > 2 ||
            (method == 0 && stored != raw)) {
            goto done;
        }
        out[index] = cloud_alloc(raw ? raw : 1);
        if (!out[index]) {
            goto done;
        }
        if (method == 0) {
            memory_copy(out[index], bundle + at, raw);
        } else if (!(method == 1 ? codec_xpress_huffman_decode(bundle + at, stored, out[index], raw)
                                 : codec_lz_decode(bundle + at, stored, out[index], raw))) {
            goto done;
        }
        out_size[index] = raw;
        at += stored;
    }
    if (at != size || !out[0]) {
        goto done;
    }
    /* integrity: each data file is the one the commit names */
    for (unsigned i = 1; i < CLOUD_FILE_COUNT; ++i) {
        if (out[i] && !commit_lists(out[0], out_size[0], cloud_files[i] + 5, out[i], out_size[i])) {
            cloud_log("downloaded save does not match its commit: ", cloud_files[i]);
            goto done;
        }
    }
    if (!wjoin(path, directory, L"\\data", 0) || !cloud_create_directory(directory, 0) || !cloud_create_directory(path, 0)) {
        goto done;
    }
    for (unsigned i = 0; i < CLOUD_FILE_COUNT; ++i) {
        if (!out[i]) {
            continue;
        }
        file_name_wide(cloud_files[i], name, ARRAY_COUNT(name));
        if (!wjoin(path, directory, L"\\", name) || !write_file(path, out[i], out_size[i])) {
            goto done;
        }
    }
    result = revision ? revision : 1;
done:
    for (unsigned i = 0; i < CLOUD_FILE_COUNT; ++i) {
        cloud_free(out[i]);
    }
    return result;
}

/* ---------------------------------------------------------------- server */

static void cloud_set_synced(const char *commit) {
    for (int i = 0; i < 64; ++i) {
        cloud_synced[i] = (wchar_t)commit[i];
    }
    cloud_synced[64] = 0;
    if (cloud_write_profile) {
        cloud_write_profile(cloud_section, L"SaveCommit", cloud_synced, cloud_ini);
    }
}

/* RestoreSave=1: this PC's save gives way to the server's at the next start (progress that was
   not accepted stays in saves\fh2online-backup). */
static int cloud_restore_marked(void) {
    wchar_t value[4] = {0};
    GetPrivateProfileStringW(cloud_section, L"RestoreSave", L"", value, ARRAY_COUNT(value), cloud_ini);
    return value[0] == L'1';
}

static void cloud_mark_restore(int on) {
    if (cloud_write_profile && cloud_section[0]) {
        cloud_write_profile(cloud_section, L"RestoreSave", on ? L"1" : 0, cloud_ini);
    }
}

/* Set when progress of this session was not accepted (by the server, or held back by the hook):
   nothing more is saved online until the game restarts (legacy Beta path only).
   The game is never closed for it: the player restarts when they choose, and that start puts
   the cloud save in place. */
static volatile LONG cloud_stopped;

int azimuth_cloud_stopped(void) {
    return cloud_stopped != 0;
}

void azimuth_cloud_stop(const char *why) {
    if (InterlockedExchange(&cloud_stopped, 1)) {
        return;
    }
    cloud_mark_restore(1);
    log_append("CLOUD: sync stopped (");
    log_append(why);
    log_append("): nothing more is saved online; the next start loads the cloud save.\r\n");
    azimuth_account_force_notice("Cloud sync stopped: the server did not accept some of this session's progress. Nothing from now on is saved online. Restart the game to play on from your cloud save.");
}

static int synced_is(const char *commit) {
    for (int i = 0; i < 64; ++i) {
        if (cloud_synced[i] != (wchar_t)commit[i]) {
            return 0;
        }
    }
    return cloud_synced[64] == 0;
}

/* The server's save: 1 found (commit filled), 0 none, -1 unreachable or refused. */
/* GET save/info: 0 no save on the server, 1 its commit in `commit`, 2 the same but the server
   says to restore it ("restore": a reset, or an upload it refused: its save wins over the local
   one, whatever the local one is), -1 unreachable. */
static int server_info(char commit[65]) {
    static char reply[512];
    size_t got = 0;
    DWORD status = azimuth_net_request(L"GET", L"/infinity/v1/save/info", cloud_headers, L"application/json", 0, 0,
                                       reply, sizeof(reply) - 1, &got);
    reply[got] = 0;
    if (status == 404) {
        return 0;
    }
    if (status != 200) {
        return -1;
    }
    const char *p = reply;
    while (*p && !(p[0] == '"' && memory_equal(p + 1, "commit\"", 7))) {
        ++p;
    }
    if (!*p) {
        return -1;
    }
    p += 8;
    while (*p == ':' || *p == ' ' || *p == '"') {
        ++p;
    }
    for (int i = 0; i < 64; ++i) {
        if (!p[i]) {
            return -1;
        }
        commit[i] = p[i];
    }
    commit[64] = 0;
    return text_find(reply, text_length(reply), "\"restore\":true") ? 2 : 1;
}

/* The HTTP status of the last upload (0: never sent), and whether its refusal said the profile
   is free (403 {"why":"free"}: a free profile has no cloud save, PLAN 9.1). */
static DWORD cloud_upload_status;
static int cloud_upload_free;

static int upload(const CloudRevision *revision) {
    static wchar_t headers[300];
    size_t size;
    unsigned char *bundle = bundle_revision(revision, &size);
    if (!bundle) {
        cloud_log("could not read the save revision.", "");
        return 0;
    }
    if (size > CLOUD_MAX_BUNDLE) {
        cloud_free(bundle);
        cloud_log("save too big for the server.", "");
        return 0;
    }
    size_t n = wlen(cloud_headers);
    memory_copy(headers, cloud_headers, n * sizeof(wchar_t));
    static const wchar_t name[] = L"\r\nX-Fh2-Save-Commit: ";
    memory_copy(headers + n, name, sizeof(name));
    n += ARRAY_COUNT(name) - 1;
    for (int i = 0; i < 64; ++i) {
        headers[n++] = (wchar_t)revision->commit[i];
    }
    headers[n] = 0;
    char reply[256];
    size_t got = 0;
    DWORD status = azimuth_net_request(L"PUT", L"/infinity/v1/save", headers, L"application/octet-stream", bundle,
                                       size, reply, sizeof(reply) - 1, &got);
    cloud_free(bundle);
    cloud_upload_status = status;
    cloud_upload_free = 0;
    if (status != 200) {
        reply[got] = 0;
        cloud_upload_free = status == 403 && text_find(reply, got, "\"why\":\"free\"") != 0;
        cloud_log(cloud_upload_free ? "the server keeps no save for a free profile: " : "upload refused: ", reply);
        if (status == 409) {
            azimuth_cloud_stop("the server refused the save");
        }
        return 0;
    }
    cloud_set_synced(revision->commit);
    cloud_log("save uploaded, commit ", revision->commit);
    return 1;
}

/* Downloads the server's save as revision `number`. */
static int download(unsigned long long number) {
    static wchar_t incoming[CLOUD_PATH], target[CLOUD_PATH], revisions[CLOUD_PATH];
    unsigned char *bundle = cloud_alloc(CLOUD_MAX_BUNDLE);
    size_t size = 0;
    int ok = 0;
    if (!bundle) {
        return 0;
    }
    DWORD status = azimuth_net_request(L"GET", L"/infinity/v1/save", cloud_headers, L"application/json", 0, 0, bundle,
                                       CLOUD_MAX_BUNDLE, &size);
    wchar_t name[17], stamp[17];
    hex16(number, name);
    hex16(GetTickCount64(), stamp); /* a fresh folder even after a failed attempt */
    if (status == 200 && size < CLOUD_MAX_BUNDLE && wjoin(incoming, cloud_saves, L"\\fh2online-incoming-", stamp) &&
        wjoin(revisions, cloud_profile, L"\\revisions", 0) && wjoin(target, revisions, L"\\", name)) {
        cloud_create_directory(cloud_profile, 0);
        cloud_create_directory(revisions, 0);
        ok = install_bundle(bundle, size, incoming) && cloud_move(incoming, target, 0);
    }
    cloud_free(bundle);
    return ok;
}

/* Moves a local revision out of the way (kept in saves\fh2online-backup). */
static void back_up(const CloudRevision *revision) {
    static wchar_t backup[CLOUD_PATH], target[CLOUD_PATH];
    wchar_t name[17];
    hex16(revision->number, name);
    const wchar_t *profile = cloud_profile + wlen(cloud_profile);
    while (profile > cloud_profile && profile[-1] != L'\\') {
        --profile;
    }
    if (wjoin(backup, cloud_saves, L"\\fh2online-backup", 0) && wjoin(target, backup, L"\\", profile) &&
        wjoin(target, target, L"-", name)) {
        cloud_create_directory(backup, 0);
        if (cloud_move(revision->path, target, 0)) {
            cloud_log("local save kept in saves\\fh2online-backup (the server's was newer).", "");
        }
    }
}

/* ------------------------------------------------------------- creations */

/* The player's own files beside the profile's save: every other container in player-1
   (Livery_*, BaseLivery_*, GarageThumbnail_*, LayerGroup_*, tunes: designs, vinyl groups and
   tuning setups). Azimuth keeps them for the whole PC, not per profile. Each one's newest
   revision is kept on the server for the account (server/src/db/creations.rs, "FH2FILE1"), so
   they follow the profile to another PC:
   - at start: a creation the server keeps and this PC lacks is written here, unless this PC
     had synced it before (the player deleted it: it is deleted on the server too); one this PC
     has and the server does not (or another revision of) is uploaded;
   - while playing (every minute): new or changed ones are uploaded, deleted ones deleted.
   What was synced is listed in saves\fh2online-creations-<profile>.txt ("name commit" lines).
   A downloaded creation is installed only if every file matches its commit's SHA-256 and size. */

#define CREATION_MAX 320
#define CREATION_NAME 80
#define CREATION_FILES 16
#define CREATION_MAX_BUNDLE (1u << 20) /* as the server takes */
/* Uploads (and deletions) per round, at start and then every minute; a refused one ends the
   round: the first sync of a PC with many creations is spread over minutes instead of filling
   the server's connections (2026-10-17). */
#define CREATION_PER_ROUND 4

typedef struct {
    char name[CREATION_NAME];
    char commit[65];
} Creation;

typedef struct {
    unsigned count;
    Creation item[CREATION_MAX];
} CreationList;

static CreationList creations_local, creations_server, creations_synced;
static wchar_t creations_player[CLOUD_PATH]; /* <saves>\player-1 */
static wchar_t creations_file[CLOUD_PATH];   /* <saves>\fh2online-creations-User_<hex>.txt */

static int creation_name_ok(const char *name) {
    size_t n = text_length(name), kind = 0;
    /* garage thumbnails (one per car, made again by the game) are not synced: the server does not
       keep them */
    if (n > 16 && memory_equal(name, "GarageThumbnail_", 16)) {
        return 0;
    }
    /* CR-S59: another player's livery the game cached ("MultiplayerLivery_<id>") is theirs, never this
       profile's creation */
    if (n > 18 && memory_equal(name, "MultiplayerLivery_", 18)) {
        return 0;
    }
    while (kind < n && ((name[kind] | 32) >= 'a' && (name[kind] | 32) <= 'z')) {
        ++kind;
    }
    if (!kind || kind > 24 || kind >= n || name[kind] != '_' || n - kind - 1 > 48 || n >= CREATION_NAME) {
        return 0;
    }
    for (size_t i = kind + 1; i < n; ++i) {
        char c = name[i];
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '_')) {
            return 0;
        }
    }
    return !(kind == 4 && (name[0] | 32) == 'u' && (name[1] | 32) == 's' && (name[2] | 32) == 'e' && (name[3] | 32) == 'r');
}

static Creation *creation_find(CreationList *list, const char *name) {
    for (unsigned i = 0; i < list->count; ++i) {
        if (text_length(list->item[i].name) == text_length(name) && memory_equal(list->item[i].name, name, text_length(name))) {
            return &list->item[i];
        }
    }
    return 0;
}

static void creation_put(CreationList *list, const char *name, const char *commit) {
    Creation *c = creation_find(list, name);
    if (!c) {
        if (list->count >= CREATION_MAX) {
            return;
        }
        c = &list->item[list->count++];
        memory_copy(c->name, name, text_length(name) + 1);
    }
    memory_copy(c->commit, commit, 64);
    c->commit[64] = 0;
}

static void creation_remove(CreationList *list, const char *name) {
    Creation *c = creation_find(list, name);
    if (c) {
        --list->count;
        if (c != &list->item[list->count]) {
            memory_copy(c, &list->item[list->count], sizeof(*c));
        }
    }
}

static void creation_path(wchar_t *out, const char *name) {
    static wchar_t wide[CREATION_NAME];
    size_t n = 0;
    for (; name[n] && n + 1 < CREATION_NAME; ++n) {
        wide[n] = (wchar_t)name[n];
    }
    wide[n] = 0;
    wjoin(out, creations_player, L"\\", wide);
}

/* Every creation container in player-1 with a complete revision. 0 when player-1 cannot be
   read or holds no profile at all (then nothing is taken as deleted). */
static int creations_scan(CreationList *list) {
    static wchar_t pattern[CLOUD_PATH];
    static WIN32_FIND_DATAW entry;
    static CloudRevision revision;
    static char name[CREATION_NAME];
    int profiles = 0;
    list->count = 0;
    if (!wjoin(pattern, creations_player, L"\\*", 0)) {
        return 0;
    }
    HANDLE find = cloud_find_first(pattern, &entry);
    if (find == INVALID_HANDLE_VALUE) {
        return 0;
    }
    do {
        if (!(entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)) {
            continue;
        }
        size_t n = 0;
        int ascii = 1;
        for (; entry.cFileName[n] && n + 1 < CREATION_NAME; ++n) {
            ascii &= entry.cFileName[n] < 128;
            name[n] = (char)entry.cFileName[n];
        }
        name[n] = 0;
        if (n > 5 && memory_equal(name, "User_", 5)) {
            ++profiles;
            continue;
        }
        if (!ascii || entry.cFileName[n] || !creation_name_ok(name)) {
            continue;
        }
        static wchar_t base[CLOUD_PATH];
        creation_path(base, name);
        newest_revision_in(base, &revision, 0);
        if (revision.found) {
            creation_put(list, name, revision.commit);
        }
    } while (cloud_find_next(find, &entry));
    cloud_find_close(find);
    return profiles > 0;
}

static void creations_load(void) {
    size_t size;
    unsigned char *text = read_file(creations_file, 1 << 20, &size);
    creations_synced.count = 0;
    if (!text) {
        return;
    }
    for (size_t at = 0; at < size;) {
        size_t end = at;
        while (end < size && text[end] != '\n') {
            ++end;
        }
        size_t space = at;
        while (space < end && text[space] != ' ') {
            ++space;
        }
        if (space - at < CREATION_NAME && end - space - 1 >= 64) {
            char name[CREATION_NAME], commit[65];
            memory_copy(name, text + at, space - at);
            name[space - at] = 0;
            memory_copy(commit, text + space + 1, 64);
            commit[64] = 0;
            if (creation_name_ok(name)) {
                creation_put(&creations_synced, name, commit);
            }
        }
        at = end + 1;
    }
    cloud_free(text);
}

static void creations_save(void) {
    static char text[CREATION_MAX * (CREATION_NAME + 67)];
    size_t n = 0;
    for (unsigned i = 0; i < creations_synced.count; ++i) {
        size_t len = text_length(creations_synced.item[i].name);
        memory_copy(text + n, creations_synced.item[i].name, len);
        n += len;
        text[n++] = ' ';
        memory_copy(text + n, creations_synced.item[i].commit, 64);
        n += 64;
        text[n++] = '\n';
    }
    write_file(creations_file, (const unsigned char *)text, n);
}

/* The data file names a commit lists ("<sha256> <size> <name>" lines), into `names`. */
static unsigned commit_names(const unsigned char *commit, size_t size, char names[CREATION_FILES][56]) {
    unsigned count = 0;
    for (size_t at = 0; at < size && count < CREATION_FILES;) {
        size_t end = at;
        while (end < size && commit[end] != '\n') {
            ++end;
        }
        size_t p = at + 65;
        if (end - at > 66 && commit[at + 64] == ' ') {
            while (p < end && commit[p] >= '0' && commit[p] <= '9') {
                ++p;
            }
            size_t len = end - p - 1;
            if (p < end && commit[p] == ' ' && len && len < 48) {
                int ok = 1;
                for (size_t i = 0; i < len; ++i) {
                    char c = (char)commit[p + 1 + i];
                    ok &= (c >= '0' && c <= '9') || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '_' || c == '.';
                }
                if (ok) {
                    memory_copy(names[count], "data/", 5);
                    memory_copy(names[count] + 5, commit + p + 1, len);
                    names[count][5 + len] = 0;
                    ++count;
                }
            }
        }
        at = end + 1;
    }
    return count;
}

/* A creation's newest revision as an FH2FILE1 bundle (caller frees), or NULL. */
static unsigned char *creation_bundle(const wchar_t *revision, size_t *size) {
    static wchar_t path[CLOUD_PATH], wide[64];
    static char names[CREATION_FILES + 1][56];
    unsigned char *files[CREATION_FILES + 1] = {0};
    size_t sizes[CREATION_FILES + 1] = {0}, total = 12;
    unsigned char *bundle = 0;
    unsigned count = 0;
    *size = 0;
    memory_copy(names[0], "commit", 7);
    if (!wjoin(path, revision, L"\\commit", 0) || !(files[0] = read_file(path, 1 << 16, &sizes[0]))) {
        return 0;
    }
    count = 1 + commit_names(files[0], sizes[0], (char (*)[56])names[1]);
    for (unsigned i = 0; i < count; ++i) {
        if (i) {
            file_name_wide(names[i], wide, ARRAY_COUNT(wide));
            if (!wjoin(path, revision, L"\\", wide) || !(files[i] = read_file(path, CREATION_MAX_BUNDLE, &sizes[i]))) {
                goto done;
            }
        }
        total += 2 + text_length(names[i]) + 9 + sizes[i];
    }
    if (total > CREATION_MAX_BUNDLE || !(bundle = cloud_alloc(total))) {
        goto done;
    }
    memory_copy(bundle, "FH2FILE1", 8);
    put32(bundle + 8, count);
    size_t at = 12;
    for (unsigned i = 0; i < count; ++i) {
        size_t name_length = text_length(names[i]);
        bundle[at] = (unsigned char)name_length;
        bundle[at + 1] = 0;
        memory_copy(bundle + at + 2, names[i], name_length);
        at += 2 + name_length;
        unsigned char *header = bundle + at;
        at += 9;
        size_t packed = 0;
        int method = 0;
        if (sizes[i] >= 64 && cloud_workspace &&
            (packed = codec_lz_encode(files[i], sizes[i], bundle + at, sizes[i] - 1, cloud_workspace)) != 0) {
            method = 2;
        } else {
            memory_copy(bundle + at, files[i], sizes[i]);
            packed = sizes[i];
        }
        put32(header, (unsigned)sizes[i]);
        put32(header + 4, (unsigned)packed);
        header[8] = (unsigned char)method;
        at += packed;
    }
    *size = at;
done:
    for (unsigned i = 0; i <= CREATION_FILES; ++i) {
        cloud_free(files[i]);
    }
    return bundle;
}

/* Writes a downloaded FH2FILE1 bundle as `name`'s first revision. 1 when installed. */
static int creation_install(const char *name, const unsigned char *bundle, size_t size) {
    static wchar_t base[CLOUD_PATH], revisions[CLOUD_PATH], incoming[CLOUD_PATH], target[CLOUD_PATH], path[CLOUD_PATH],
        wide[64];
    static char names[CREATION_FILES][56];
    unsigned char *out[CREATION_FILES] = {0};
    size_t out_size[CREATION_FILES] = {0};
    int ok = 0, commit = -1;
    if (size < 12 || !memory_equal(bundle, "FH2FILE1", 8)) {
        return 0;
    }
    unsigned count = get32(bundle + 8);
    size_t at = 12;
    if (!count || count > CREATION_FILES) {
        return 0;
    }
    for (unsigned k = 0; k < count; ++k) {
        if (at + 2 > size) {
            goto done;
        }
        size_t name_length = bundle[at] | (size_t)bundle[at + 1] << 8;
        at += 2;
        if (name_length >= 56 || at + name_length + 9 > size) {
            goto done;
        }
        memory_copy(names[k], bundle + at, name_length);
        names[k][name_length] = 0;
        at += name_length;
        unsigned raw = get32(bundle + at), stored = get32(bundle + at + 4);
        int method = bundle[at + 8];
        at += 9;
        int data = name_length > 5 && memory_equal(names[k], "data/", 5);
        for (size_t i = data ? 5 : 0; data && i < name_length; ++i) {
            char c = names[k][i];
            data = (c >= '0' && c <= '9') || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '_' || c == '.';
        }
        int is_commit = name_length == 6 && memory_equal(names[k], "commit", 6);
        if ((!data && !is_commit) || (is_commit && commit >= 0) || raw > CREATION_MAX_BUNDLE || stored > size - at ||
            (method != 0 && method != 2) || (method == 0 && stored != raw)) {
            goto done;
        }
        if (is_commit) {
            commit = (int)k;
        }
        out[k] = cloud_alloc(raw ? raw : 1);
        if (!out[k] || (method == 0 ? (memory_copy(out[k], bundle + at, raw), 0)
                                    : !codec_lz_decode(bundle + at, stored, out[k], raw))) {
            goto done;
        }
        out_size[k] = raw;
        at += stored;
    }
    if (at != size || commit < 0) {
        goto done;
    }
    for (unsigned k = 0; k < count; ++k) {
        if ((int)k != commit && !commit_lists(out[commit], out_size[commit], names[k] + 5, out[k], out_size[k])) {
            cloud_log("downloaded creation does not match its commit: ", name);
            goto done;
        }
    }
    wchar_t stamp[17];
    hex16(GetTickCount64(), stamp);
    creation_path(base, name);
    if (!wjoin(revisions, base, L"\\revisions", 0) || !wjoin(target, revisions, L"\\0000000000000001", 0) ||
        !wjoin(incoming, cloud_saves, L"\\fh2online-incoming-", stamp) || !wjoin(path, incoming, L"\\data", 0) ||
        !cloud_create_directory(incoming, 0) || !cloud_create_directory(path, 0)) {
        goto done;
    }
    for (unsigned k = 0; k < count; ++k) {
        file_name_wide(names[k], wide, ARRAY_COUNT(wide));
        if (!wjoin(path, incoming, L"\\", wide) || !write_file(path, out[k], out_size[k])) {
            goto done;
        }
    }
    cloud_create_directory(base, 0);
    cloud_create_directory(revisions, 0);
    ok = cloud_move(incoming, target, 0) != 0;
done:
    for (unsigned k = 0; k < CREATION_FILES; ++k) {
        cloud_free(out[k]);
    }
    return ok;
}

static void creation_route(wchar_t *out, const char *name) {
    static wchar_t wide[CREATION_NAME];
    size_t n = 0;
    for (; name[n] && n + 1 < CREATION_NAME; ++n) {
        wide[n] = (wchar_t)name[n];
    }
    wide[n] = 0;
    wjoin(out, L"/infinity/v1/creations/", wide, 0);
}

static int creation_upload(const char *name, const char *commit) {
    static wchar_t base[CLOUD_PATH], route[CLOUD_PATH], headers[300];
    static CloudRevision revision;
    char reply[200];
    size_t got = 0, size = 0;
    creation_path(base, name);
    newest_revision_in(base, &revision, 0);
    if (!revision.found || !memory_equal(revision.commit, commit, 64)) {
        return 0;
    }
    unsigned char *bundle = creation_bundle(revision.path, &size);
    if (!bundle) {
        cloud_log("could not read the creation ", name);
        return 0;
    }
    wchar_t hash[65];
    for (int i = 0; i < 64; ++i) {
        hash[i] = (wchar_t)commit[i];
    }
    hash[64] = 0;
    wjoin(headers, cloud_headers, L"\r\nX-Fh2-Commit: ", hash);
    creation_route(route, name);
    DWORD status = azimuth_net_request(L"PUT", route, headers, L"application/octet-stream", bundle, size, reply,
                                       sizeof(reply) - 1, &got);
    cloud_free(bundle);
    if (status != 200) {
        reply[got] = 0;
        cloud_log("creation upload refused: ", reply);
        return 0;
    }
    return 1;
}

static int creation_delete(const char *name) {
    static wchar_t route[CLOUD_PATH];
    char reply[64];
    size_t got = 0;
    creation_route(route, name);
    DWORD status = azimuth_net_request(L"DELETE", route, cloud_headers, L"application/json", 0, 0, reply, sizeof(reply), &got);
    return status == 204 || status == 200;
}

static int creation_download(const char *name) {
    static wchar_t route[CLOUD_PATH];
    unsigned char *bundle = cloud_alloc(CREATION_MAX_BUNDLE);
    size_t size = 0;
    int ok = 0;
    if (!bundle) {
        return 0;
    }
    creation_route(route, name);
    DWORD status = azimuth_net_request(L"GET", route, cloud_headers, L"application/json", 0, 0, bundle,
                                       CREATION_MAX_BUNDLE, &size);
    if (status == 200 && size < CREATION_MAX_BUNDLE) {
        ok = creation_install(name, bundle, size);
    }
    cloud_free(bundle);
    return ok;
}

/* GET creations: {"creations":[{"commit":"...","name":"..."}, ...]} into creations_server. */
static int creations_fetch(void) {
    size_t cap = CREATION_MAX * (CREATION_NAME + 100) + 64, got = 0;
    char *reply = cloud_alloc(cap);
    creations_server.count = 0;
    if (!reply) {
        return 0;
    }
    DWORD status = azimuth_net_request(L"GET", L"/infinity/v1/creations", cloud_headers, L"application/json", 0, 0, reply,
                                       cap - 1, &got);
    if (status != 200) {
        cloud_free(reply);
        return 0;
    }
    reply[got] = 0;
    for (char *p = reply; (p = (char *)text_find(p, text_length(p), "{")) != 0; ++p) {
        char *end = p;
        while (*end && *end != '}') {
            ++end;
        }
        static char name[CREATION_NAME], commit[65];
        name[0] = 0;
        commit[0] = 0;
        for (int key = 0; key < 2; ++key) {
            const char *k = key ? "\"commit\":\"" : "\"name\":\"";
            const char *v = text_find(p, (size_t)(end - p), k);
            if (!v) {
                continue;
            }
            v += text_length(k);
            size_t n = 0;
            char *into = key ? commit : name;
            size_t cap_key = key ? 64 : CREATION_NAME - 1;
            while (v + n < end && v[n] != '"' && n < cap_key) {
                into[n] = v[n];
                ++n;
            }
            into[n] = 0;
        }
        if (creation_name_ok(name) && text_length(commit) == 64) {
            creation_put(&creations_server, name, commit);
        }
        if (!*end) {
            break;
        }
        p = end;
    }
    cloud_free(reply);
    return 1;
}

/* At start, once the session is in step with the server. */
static void creations_start(const wchar_t *profile_folder_name) {
    unsigned up = 0, down = 0, gone = 0;
    if (!wjoin(creations_player, cloud_saves, L"\\player-1", 0) ||
        !wjoin(creations_file, cloud_saves, L"\\fh2online-creations-", profile_folder_name) ||
        !wjoin(creations_file, creations_file, L".txt", 0)) {
        return;
    }
    creations_load();
    if (!creations_scan(&creations_local) || !creations_fetch()) {
        log_append("CLOUD: creations not synced this time (player-1 or the server could not be read).\r\n");
        return;
    }
    for (unsigned i = 0; i < creations_server.count; ++i) {
        Creation *s = &creations_server.item[i];
        if (creation_find(&creations_local, s->name)) {
            continue;
        }
        if (creation_find(&creations_synced, s->name)) {
            /* synced from this PC before and gone now: the player deleted it */
            if (creation_delete(s->name)) {
                creation_remove(&creations_synced, s->name);
                ++gone;
            }
        } else if (creation_download(s->name)) {
            creation_put(&creations_local, s->name, s->commit);
            creation_put(&creations_synced, s->name, s->commit);
            ++down;
        }
    }
    int round = CREATION_PER_ROUND; /* the rest go up with the next rounds (creations_poll) */
    for (unsigned i = 0; i < creations_local.count; ++i) {
        Creation *l = &creations_local.item[i];
        Creation *s = creation_find(&creations_server, l->name);
        if (s && memory_equal(s->commit, l->commit, 64)) {
            creation_put(&creations_synced, l->name, l->commit);
        } else if (round > 0) {
            if (!creation_upload(l->name, l->commit)) {
                round = 0;
                continue;
            }
            creation_put(&creations_synced, l->name, l->commit);
            ++up;
            --round;
        }
    }
    creations_save();
    static char line[160];
    line[0] = 0;
    char number[12];
    static const char *const parts[] = {"CLOUD: creations in sync (", 0, " kept on the server; ", 0, " uploaded, ", 0, " downloaded, ",
                           0, " deleted).\r\n"};
    unsigned values[4];
    values[0] = creations_server.count;
    values[1] = up;
    values[2] = down;
    values[3] = gone;
    size_t n = 0;
    for (unsigned i = 0; i < ARRAY_COUNT(parts); ++i) {
        const char *part = parts[i];
        if (!part) {
            unsigned v = values[i / 2], d = 0;
            char digits[12];
            do {
                digits[d++] = (char)('0' + v % 10);
                v /= 10;
            } while (v);
            for (unsigned j = 0; j < d; ++j) {
                number[j] = digits[d - 1 - j];
            }
            number[d] = 0;
            part = number;
        }
        for (; *part && n + 1 < sizeof(line); ++part) {
            line[n++] = *part;
        }
    }
    line[n] = 0;
    log_append(line);
}

/* Every minute while playing: new or changed creations go up, deleted ones are deleted. */
static void creations_poll(void) {
    int changed = 0;
    if (!creations_player[0] || !creations_scan(&creations_local)) {
        return;
    }
    int round = CREATION_PER_ROUND;
    for (unsigned i = 0; i < creations_local.count && round > 0; ++i) {
        Creation *l = &creations_local.item[i];
        Creation *s = creation_find(&creations_synced, l->name);
        if (s && memory_equal(s->commit, l->commit, 64)) {
            continue;
        }
        if (!creation_upload(l->name, l->commit)) {
            break; /* refused or unreachable: the next minute */
        }
        creation_put(&creations_synced, l->name, l->commit);
        cloud_log("creation uploaded: ", l->name);
        changed = 1;
        --round;
    }
    round = CREATION_PER_ROUND;
    for (unsigned i = creations_synced.count; i-- > 0 && round > 0;) {
        static char name[CREATION_NAME];
        memory_copy(name, creations_synced.item[i].name, CREATION_NAME);
        if (!creation_find(&creations_local, name) && (--round, creation_delete(name))) {
            creation_remove(&creations_synced, name);
            cloud_log("creation deleted: ", name);
            changed = 1;
        }
    }
    if (changed) {
        creations_save();
    }
}

/* ----------------------------------------------------------------- entry */

/* Set once this session started in step with the server's save: only then are its new
   revisions uploaded. */
static volatile LONG cloud_session_synced;

static DWORD WINAPI cloud_watch(void *unused) {
    static CloudRevision revision;
    (void)unused;
    for (;;) {
        cloud_sleep(CLOUD_POLL_MS);
        if (!cloud_session_synced || cloud_stopped) {
            continue;
        }
        creations_poll();
        char server[65];
        int state = server_info(server);
        if (state == 2) {
            /* the server refused progress of this session (or reset the profile) */
            azimuth_cloud_stop("the server wants its save back");
            continue;
        }
        newest_revision(&revision, 0);
        if (!revision.found || synced_is(revision.commit)) {
            continue;
        }
        /* someone else's newer save is on the server (this PC was logged out elsewhere), or the
           server wants its own back (the next start restores it) */
        if (state < 0 || state == 2 || (state > 0 && !synced_is(server) && !memory_equal(server, revision.commit, 64))) {
            continue;
        }
        upload(&revision);
    }
}

static int cloud_resolve(void) {
    if (cloud_workspace) {
        return 1; /* resolved already */
    }
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    if (!kernel) {
        return 0;
    }
    cloud_find_first = (FindFirstFileWFn)(uintptr_t)GetProcAddress(kernel, "FindFirstFileW");
    cloud_find_next = (FindNextFileWFn)(uintptr_t)GetProcAddress(kernel, "FindNextFileW");
    cloud_find_close = (FindCloseFn)(uintptr_t)GetProcAddress(kernel, "FindClose");
    cloud_create_directory = (CreateDirectoryWFn)(uintptr_t)GetProcAddress(kernel, "CreateDirectoryW");
    cloud_move = (MoveFileExWFn)(uintptr_t)GetProcAddress(kernel, "MoveFileExW");
    cloud_write_profile = (WritePrivateProfileStringWFn)(uintptr_t)GetProcAddress(kernel, "WritePrivateProfileStringW");
    cloud_sleep = (SleepFn)(uintptr_t)GetProcAddress(kernel, "Sleep");
    if (!cloud_find_first || !cloud_find_next || !cloud_find_close || !cloud_create_directory || !cloud_move ||
        !cloud_write_profile || !cloud_sleep) {
        return 0;
    }
    return (cloud_workspace = cloud_alloc(CODEC_LZ_WORK_SIZE)) != 0;
}

/* Creating a profile: the newest revision of the offline player's save (User_9000000000000,
   Azimuth's own user, XUID 0x0009000000000000) becomes the new account's first cloud save, so
   the online profile goes on from the player's offline progress. `bin`/`token`/`ini`/`section`
   as for azimuth_cloud_start. Returns 1, 2 when the server keeps no save for this profile
   because it is free (nothing to copy), or 0 with `why` saying what failed. */
int azimuth_cloud_upload_offline(const wchar_t *bin, const wchar_t *token, const wchar_t *ini, const wchar_t *section,
                                 const char **why) {
    static CloudRevision offline;
    if (fi_launch_protocol()) {
        *why = "the launcher keeps the saves of its sessions";
        return 0;
    }
    if (!cloud_resolve()) {
        *why = "system functions missing";
        return 0;
    }
    if (!saves_folder(bin) ||
        !wjoin(cloud_profile, cloud_saves, L"\\player-1\\User_9000000000000", 0) || wlen(ini) >= CLOUD_PATH ||
        wlen(section) >= ARRAY_COUNT(cloud_section) || wlen(token) > 100) {
        *why = "paths too long";
        return 0;
    }
    wjoin(cloud_ini, ini, 0, 0);
    wjoin(cloud_section, section, 0, 0);
    wjoin(cloud_headers, L"X-Fh2-Token: ", token, 0);
    newest_revision(&offline, 0);
    if (!offline.found) {
        *why = "no offline save";
        return 0;
    }
    if (!upload(&offline)) {
        *why = "the server did not take the save";
        return cloud_upload_free ? 2 : 0;
    }
    return 1;
}

/* Called on the account thread once the server accepted the token, before the game opens the
   save. `bin` is data\bin (the mod's folder), `xuid` the account's decimal XUID, `token` its
   device token, `ini`/`section` where SaveCommit is kept. Then watches for new revisions. */
/* cloud_saves and cloud_profile for the account `xuid` (decimal); `h` gets User_'s hex part. */
static int cloud_profile_for(const wchar_t *bin, const wchar_t *xuid, wchar_t hex[17], const wchar_t **h) {
    unsigned long long value = 0;
    for (const wchar_t *p = xuid; *p; ++p) {
        if (*p < L'0' || *p > L'9') {
            return 0;
        }
        value = value * 10 + (unsigned)(*p - L'0');
    }
    /* User_<XUID in upper-case hex, no leading zeros> */
    wchar_t *at = hex + 16;
    *at = 0;
    do {
        *--at = L"0123456789ABCDEF"[value & 15];
        value >>= 4;
    } while (value);
    *h = at;
    return xuid[0] && saves_folder(bin) && wjoin(cloud_profile, cloud_saves, L"\\player-1\\User_", at);
}

/* A session that plays a Supporter's online profile while the server cannot be reached: its
   revisions without a commit at start go aside too, or one the player's ExtraOptions sealed
   would be the "newer save" the next start uploads. */
void azimuth_cloud_put_aside(const wchar_t *bin, const wchar_t *xuid) {
    wchar_t hex[17];
    const wchar_t *h;
    if (cloud_resolve() && cloud_unsealed_count && cloud_profile_for(bin, xuid, hex, &h)) {
        cloud_put_aside_unsealed();
    }
}

void azimuth_cloud_start(const wchar_t *bin, const wchar_t *xuid, const wchar_t *token, const wchar_t *ini,
                         const wchar_t *section) {
    static CloudRevision local;
    unsigned long long highest = 0;
    if (fi_launch_protocol()) {
        /* a launcher session: saves are the launcher's (ADR 0103), never this module's */
        return;
    }
    if (!cloud_resolve()) {
        log_append("CLOUD: system functions missing; no cloud save.\r\n");
        return;
    }
    wchar_t hex[17];
    const wchar_t *h;
    if (!cloud_profile_for(bin, xuid, hex, &h) || wlen(ini) >= CLOUD_PATH ||
        wlen(section) >= ARRAY_COUNT(cloud_section) || wlen(token) > 100) {
        return;
    }
    wjoin(cloud_ini, ini, 0, 0);
    wjoin(cloud_section, section, 0, 0);
    wjoin(cloud_headers, L"X-Fh2-Token: ", token, 0);
    GetPrivateProfileStringW(cloud_section, L"SaveCommit", L"", cloud_synced, ARRAY_COUNT(cloud_synced), cloud_ini);

    cloud_put_aside_unsealed();
    newest_revision(&local, &highest);
    char server[65];
    int state = server_info(server);
    /* the last session's progress was not accepted: the cloud save, whatever this one is (never
       for a Noob profile: the server takes all of its progress, so a mark left from before is
       dropped and the local save stays) */
    int forced = cloud_restore_marked();
    if (forced && azimuth_account_noob()) {
        forced = 0;
        cloud_mark_restore(0);
        log_append("CLOUD: a Noob profile keeps its own save; the earlier restore mark was dropped.\r\n");
    }
    if (forced && state == 1) {
        state = 2;
    }
    if (forced && state != 2) {
        cloud_mark_restore(0);
    }
    /* The server's save is the online profile: it only moves on while the game is connected. A
       local save newer than the last one synced, with the server still on that one, is the last
       session's own progress and is uploaded (the server's save gates measure it); any other
       local save that differs from the server's was edited or played without the server, and is
       put aside (saves\fh2online-backup) for the server's. A session that could not reach the
       server uploads nothing at all, not even at the next start. */
    /* fh2online.58: a server that cannot be reached keeps the last agreed save (SaveCommit), so
       this session's progress goes up at the next start, as the last session's newer save */
    int take_server = 0;
    if (state < 0) {
        log_append("CLOUD: server unreachable; this session's progress goes online at the next start.\r\n");
    } else if (state == 0) {
        if (local.found) {
            upload(&local); /* the profile's first save */
        }
        InterlockedExchange(&cloud_session_synced, 1);
    } else if (local.found && memory_equal(server, local.commit, 64) && state == 1) {
        if (!synced_is(server)) {
            cloud_set_synced(server);
        }
        InterlockedExchange(&cloud_session_synced, 1);
        log_append("CLOUD: save in sync with the server.\r\n");
    } else if (local.found && state == 1 && synced_is(server)) {
        if (upload(&local)) {
            InterlockedExchange(&cloud_session_synced, 1);
            log_append("CLOUD: the last session's newer save was uploaded.\r\n");
        } else if (cloud_upload_status == 409) {
            take_server = 1; /* the server refused it: its save comes first */
        } else {
            /* fh2online.58: not a refusal (no answer, a server error): the save stays on this PC
               and goes up at the next start; nothing is replaced */
            log_append("CLOUD: the last session's save could not be uploaded now; it stays on this PC and is sent at the next start.\r\n");
        }
    } else {
        take_server = 1;
    }
    if (take_server) {
        if (local.found && !memory_equal(server, local.commit, 64)) {
            back_up(&local);
            /* the game may have read the local one already: the player is told to restart (what
               it would upload from it is measured by the server's save gates anyway) */
            azimuth_account_set_notice("The online profile's save on this PC differed from the server's, so the server's was put in its place. If the game shows other progress, restart it.");
            log_append("CLOUD: the local save differs from the server's; the server's is the profile's (the local one is kept in saves\\fh2online-backup).\r\n");
        }
        if (download(highest + 1)) {
            cloud_set_synced(server);
            InterlockedExchange(&cloud_session_synced, 1);
            cloud_log("save downloaded from the server, commit ", server);
            /* fh2online.58: in step with the server again, so this session syncs as any other (a
               refusal at this start used to leave the sync stopped and the mark set, and the whole
               session was lost at the next start) */
            cloud_mark_restore(0);
            InterlockedExchange(&cloud_stopped, 0);
        } else {
            log_append("CLOUD: could not download the server's save; this session's progress will not reach the online profile.\r\n");
        }
    }
    if (cloud_session_synced) {
        static wchar_t folder[CLOUD_PATH];
        if (wjoin(folder, L"User_", h, 0)) {
            creations_start(folder);
        }
    }
    HANDLE thread = CreateThread(0, 0, cloud_watch, 0, 0, 0);
    if (thread) {
        CloseHandle(thread);
    }
}
