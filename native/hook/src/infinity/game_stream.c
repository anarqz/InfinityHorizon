/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The game's IOSys streams (game_stream.h; T19, livery-share.md §4b). IOSys::CDynamicMemoryStream:
   vtable +0x10 AddRef, +0x18 Release, +0x38 Seek(offset, origin) -> bool, +0x90 Read(buffer, n) -> n,
   +0x98 Write(buffer, n), +0xd8 size; fields +0x30 data, +0x38 size, +0x3c position, +0x60 owns flag,
   +0x64 capacity. game+0x11e48e0 builds one with the game allocator (0x68 bytes), the stream base
   constructor, the vtable and those two fields, then AddRef, Write, Seek 0; this does the same, after
   checking that sequence's bytes (its call targets and vtable are rel32 inside them) and the vtable
   slots it calls. */
#include "game_stream.h"
#include "../core/support.h"

#define GS_BUILD_RVA 0x11e4ac6u        /* the construction in game+0x11e48e0 */
#define GS_ALLOC_RVA 0x3229c0u         /* the game allocator it calls */
#define GS_CTOR_RVA 0x11e06f0u         /* the stream base constructor it calls */
#define GS_VTABLE_RVA 0x2eac160u       /* IOSys::CDynamicMemoryStream's vtable */
#define GS_OBJECT_SIZE 0x68u
#define GS_ADD_REF 0x10u
#define GS_RELEASE 0x18u
#define GS_SEEK 0x38u
#define GS_READ 0x90u
#define GS_WRITE 0x98u
#define GS_SIZE 0xd8u
#define GS_OWNS 0x60u
#define GS_CAPACITY 0x64u

/* mov ecx,68h; call alloc; mov rbx,rax; mov [rbp-61h],rax; test rax,rax; je; mov rcx,rax;
   call ctor; lea rax,[vtable]; mov [rbx],rax; mov byte [rbx+60h],0; mov [rbx+64h],r12d */
static const unsigned char gs_build_bytes[48] = {
    0xb9, 0x68, 0x00, 0x00, 0x00, 0xe8, 0xf0, 0xde, 0x13, 0xff, 0x48, 0x8b, 0xd8, 0x48, 0x89, 0x45,
    0x9f, 0x48, 0x85, 0xc0, 0x74, 0x1c, 0x48, 0x8b, 0xc8, 0xe8, 0x0c, 0xbc, 0xff, 0xff, 0x48, 0x8d,
    0x05, 0x75, 0x76, 0xcc, 0x01, 0x48, 0x89, 0x03, 0xc6, 0x43, 0x60, 0x00, 0x44, 0x89, 0x63, 0x64};
/* The vtable slots called here and the functions they hold in the supported build. */
static const struct {
    unsigned slot, rva;
} gs_slots[6] = {{GS_ADD_REF, 0x17a73f0u}, {GS_RELEASE, 0x21ce50u}, {GS_SEEK, 0x1200430u},
                 {GS_READ, 0x11fd590u},    {GS_WRITE, 0x1205df0u},  {GS_SIZE, 0x1091220u}};

typedef void *(*GsAllocFn)(size_t size);
typedef void *(*GsCtorFn)(void *object);
typedef int (*GsRefFn)(void *object);
typedef unsigned char (*GsSeekFn)(void *object, int offset, int origin);
typedef int (*GsReadFn)(void *object, void *buffer, int size);
typedef int (*GsWriteFn)(void *object, const void *buffer, int size);
typedef int (*GsSizeFn)(void *object);
/* The overlay's optional exports (include/fi_overlay.h). */
typedef int (*GsFitPngFn)(const void *in, size_t in_size, const FiPngFitV1 *fit, void *out, size_t out_cap,
                          size_t *out_size, unsigned *out_width, unsigned *out_height);
typedef unsigned (*GsFitVersionFn)(void);

static unsigned char *gs_image;
static GsAllocFn gs_alloc;
static GsCtorFn gs_ctor;
static void *gs_vtable;

int fi_game_stream_install(unsigned char *image) {
    if (gs_alloc) return gs_image == image;
    unsigned char current[sizeof(gs_build_bytes)];
    if (!image || !memory_read(image + GS_BUILD_RVA, current, sizeof(current)) ||
        !memory_equal(current, gs_build_bytes, sizeof(current)))
        return 0;
    for (unsigned i = 0; i < ARRAY_COUNT(gs_slots); ++i) {
        void *method = 0;
        if (!memory_read(image + GS_VTABLE_RVA + gs_slots[i].slot, &method, sizeof(method)) ||
            method != (void *)(image + gs_slots[i].rva))
            return 0;
    }
    gs_image = image;
    gs_ctor = (GsCtorFn)(uintptr_t)(image + GS_CTOR_RVA);
    gs_vtable = image + GS_VTABLE_RVA;
    gs_alloc = (GsAllocFn)(uintptr_t)(image + GS_ALLOC_RVA);
    return 1;
}

static void *gs_slot(void *object, unsigned slot) {
    void *vtable = 0, *method = 0;
    if (!object || !memory_read(object, &vtable, sizeof(vtable)) || !vtable ||
        !memory_read((unsigned char *)vtable + slot, &method, sizeof(method)))
        return 0;
    return method;
}

void fi_game_stream_release(void *object) {
    GsRefFn release = (GsRefFn)(uintptr_t)gs_slot(object, GS_RELEASE);
    if (release) release(object);
}

long long fi_game_stream_size(void *stream) {
    GsSizeFn size = (GsSizeFn)(uintptr_t)gs_slot(stream, GS_SIZE);
    if (!size) return -1;
    int value = size(stream);
    return value < 0 ? -1 : value;
}

int fi_game_stream_read_all(void *stream, unsigned char *buffer, unsigned size) {
    GsSeekFn seek = (GsSeekFn)(uintptr_t)gs_slot(stream, GS_SEEK);
    GsReadFn read = (GsReadFn)(uintptr_t)gs_slot(stream, GS_READ);
    if (!seek || !read || !seek(stream, 0, 0)) return 0;
    unsigned done = 0;
    while (done < size) {
        int got = read(stream, buffer + done, (int)(size - done));
        if (got <= 0 || (unsigned)got > size - done) return 0;
        done += (unsigned)got;
    }
    return 1;
}

void *fi_game_stream_new(const unsigned char *data, unsigned size) {
    if (!gs_alloc || !gs_ctor || !gs_vtable || size > 0x7fffffffu) return 0;
    unsigned char *object = gs_alloc(GS_OBJECT_SIZE);
    if (!object) return 0;
    gs_ctor(object);
    void *vtable = gs_vtable;
    unsigned char owns = 0;
    int capacity = 0;
    memory_copy(object, &vtable, sizeof(vtable));
    memory_copy(object + GS_OWNS, &owns, 1);
    memory_copy(object + GS_CAPACITY, &capacity, sizeof(capacity));
    GsRefFn add_ref = (GsRefFn)(uintptr_t)gs_slot(object, GS_ADD_REF);
    GsWriteFn write = (GsWriteFn)(uintptr_t)gs_slot(object, GS_WRITE);
    GsSeekFn seek = (GsSeekFn)(uintptr_t)gs_slot(object, GS_SEEK);
    if (!add_ref || !write || !seek) return 0; /* not reached: the slots are checked at install */
    add_ref(object);
    write(object, data, (int)size);
    if (!seek(object, 0, 0) || fi_game_stream_size(object) != (long long)size) {
        fi_game_stream_release(object);
        return 0;
    }
    return object;
}

/* A PNG's IHDR sides and form: 8-bit, colour type 6 or 2, not interlaced (what the game writes and
   reads, livery-share.md §4d). */
static int gs_png_ok(const unsigned char *png, size_t size, unsigned *width, unsigned *height) {
    static const unsigned char head[16] = {0x89, 'P', 'N', 'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 'I', 'H', 'D', 'R'};
    if (size < 33 || !memory_equal(png, head, sizeof(head))) return 0;
    *width = (unsigned)png[16] << 24 | (unsigned)png[17] << 16 | (unsigned)png[18] << 8 | png[19];
    *height = (unsigned)png[20] << 24 | (unsigned)png[21] << 16 | (unsigned)png[22] << 8 | png[23];
    return *width && *height && png[24] == 8 && (png[25] == 6 || png[25] == 2) && png[28] == 0;
}

int fi_game_stream_fit_png(void *stream, unsigned size, const FiPngFitV1 *fit, void **fitted, unsigned *width,
                           unsigned *height, unsigned *bytes, const char **why) {
    *fitted = 0;
    GsFitPngFn fit_png = (GsFitPngFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_fit_png");
    GsFitVersionFn version = (GsFitVersionFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_png_fit_version");
    if (!fit_png || !version) {
        *why = "no overlay fit";
        return 0;
    }
    if (version() < fit->version) {
        *why = "older overlay fit";
        return 0;
    }
    if (!size || size > FI_GAME_STREAM_READ_CAP) {
        *why = size ? "too large to read" : "empty stream";
        return 0;
    }
    unsigned char *in = VirtualAlloc(0, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    unsigned char *out = VirtualAlloc(0, FI_GAME_STREAM_FIT_CAP, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    size_t written = 0;
    unsigned w = 0, h = 0, png_w = 0, png_h = 0;
    int ok = 0;
    if (!in || !out) {
        *why = "no memory";
    } else if (!fi_game_stream_read_all(stream, in, size)) {
        *why = "stream read failed";
    } else if (fit_png(in, size, fit, out, FI_GAME_STREAM_FIT_CAP, &written, &w, &h) != 0 || !written) {
        *why = "fit failed";
    } else if (written > fit->max_bytes || written > FI_GAME_STREAM_FIT_CAP ||
               (fit->hard_cap_bytes && written > fit->hard_cap_bytes)) {
        *why = "fit over budget";
    } else if (!gs_png_ok(out, written, &png_w, &png_h) || png_w != w || png_h != h) {
        *why = "fit is not the expected PNG";
    } else if (!(*fitted = fi_game_stream_new(out, (unsigned)written))) {
        *why = "stream build failed";
    } else {
        *width = w;
        *height = h;
        *bytes = (unsigned)written;
        ok = 1;
    }
    if (in) VirtualFree(in, 0, MEM_RELEASE);
    if (out) VirtualFree(out, 0, MEM_RELEASE);
    return ok;
}
