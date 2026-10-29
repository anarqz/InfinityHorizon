/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The livery share's preview (T19, OD-138; RE: livery-share.md §4a-§4d; docs/launcher/PNG_FIT.md).
   "Share a livery" reads the design's bigThumb.png from Connected Storage and hands it, as an IOSys
   stream, to the share popup's preview completion (game+0xe2d0a0). A preview of 217,051 bytes then
   fails inside the game before any request is built (the share step's Create stub answers negative
   and the popup shows the generic Storefront error); one of 195,340 bytes shares. No fixed limit was
   found between the two, so the preview is fitted by size (owner, 2026-10-29):

   - at most LS_PREVIEW_TARGET bytes: the original runs with the stream untouched ("passed");
   - larger: the overlay's fi_overlay_fit_png scales it down (75 %, then 2/3, then smaller steps)
     into an 8-bit RGBA PNG (colour type 6, only IHDR/IDAT/IEND, highest zlib level: the form the
     game writes and reads, §4d) of at most LS_PREVIEW_TARGET bytes; game_stream.c builds a new
     IOSys::CDynamicMemoryStream with it, which replaces the stream in the by-value reference the
     completion owns, and the original runs ("rgba WxH N bytes");
   - when that fails (no overlay or export, a decode error, still too big, a stream method failing):
     the share goes out without a preview, as §4a describes: the reference is released, the popup's
     preview (+0xe0658) is left null, its state (+0xe05e8) set to 6 and its "preview ready" mark
     (+0xe05d0) to 1, which is what the original does after storing a stream; the original is not
     called ("sent without preview");
   - no stream at all (the read failed): the original runs and shows the game's own error.

   One log line per completion, sizes only. A second detour, on the Create stub game+0xf14b30, logs
   its negative result when the share step game+0xe412b0 called it (the value that step passes to the
   popup's error handler, ui+0x460 vtable[1]) with the size of the preview last handed to the game.

   Both detours replace whole instructions without RIP-relative operands (14-byte absolute jump, the
   rest of the pinned range left as it was) after checking the game build (TimeDateStamp,
   SizeOfImage) and the pinned bytes; the preview detour also needs game_stream.c's checks. None of
   the ranges is an ExtraOptions site or another detour (tests/fi-sites.tsv, eo_sites_test). */
#include "game_stream.h"
#include "../core/logging.h"
#include "../core/support.h"

#define LS_COMPLETE_RVA 0xe2d0a0u    /* PopupShareLivery's preview completion */
#define LS_CREATE_RVA 0xf14b30u      /* the StorefrontLivery.Create stub */
#define LS_CREATE_CALL_RVA 0xe413b9u /* the share step's call of it (game+0xe412b0) */

/* PopupShareLivery fields (§4a): the preview reference, the popup state, "preview ready". */
#define LS_POPUP_PREVIEW 0xe0658u
#define LS_POPUP_STATE 0xe05e8u
#define LS_POPUP_READY 0xe05d0u
#define LS_STATE_PREVIEW_READY 6u

#define LS_PREVIEW_TARGET 190000u /* below the largest preview known to share (195,340) */
#define LS_PREVIEW_MAX 409600u    /* the game's PreviewPayload read cap on the way back (§4c) */

/* mov [rsp+10h],rdx; push rsi; push rdi; push r14; sub rsp,30h; mov qword [rsp+20h],-2 */
static const unsigned char ls_complete_bytes[22] = {
    0x48, 0x89, 0x54, 0x24, 0x10, 0x56, 0x57, 0x41, 0x56, 0x48, 0x83,
    0xec, 0x30, 0x48, 0xc7, 0x44, 0x24, 0x20, 0xfe, 0xff, 0xff, 0xff};
/* push rbp; push rbx; push rsi; push rdi; lea rbp,[rsp-48h]; sub rsp,148h */
static const unsigned char ls_create_bytes[17] = {0x40, 0x55, 0x53, 0x56, 0x57, 0x48, 0x8d, 0x6c, 0x24,
                                                  0xb8, 0x48, 0x81, 0xec, 0x48, 0x01, 0x00, 0x00};
/* call Create; test eax,eax (the share step; the return address is the test) */
static const unsigned char ls_create_call_bytes[7] = {0xe8, 0x72, 0x37, 0x0d, 0x00, 0x85, 0xc0};

/* The livery-share fit (owner, 2026-10-29): 190,000 bytes, never over 409,600, 75 % then 2/3 then
   smaller, alpha kept; a step under 64 pixels wide ends the search. */
static const FiPngFitV1 ls_fit = {FI_PNG_FIT_V1, sizeof(FiPngFitV1), LS_PREVIEW_TARGET, LS_PREVIEW_MAX, 1, 64, 10,
                                  {750, 667, 600, 500, 400, 333, 250, 200, 150, 100}};

typedef void (*LsCompleteFn)(void **self, void **stream);
typedef uintptr_t (*LsCreateFn)(uintptr_t, uintptr_t, uintptr_t, uintptr_t, uintptr_t, uintptr_t, uintptr_t,
                                uintptr_t, uintptr_t, uintptr_t);

static unsigned char *ls_image;
static LsCompleteFn ls_complete_original;
static LsCreateFn ls_create_original;
static const void *ls_create_return; /* the share step's return address from Create */
static volatile LONG ls_last_preview; /* the preview size last handed to the game, bytes */

/* 1: a preview of `size` bytes goes to the game as it is; 0: it is fitted first, or the share goes
   without one. */
int fi_livery_share_original_fits(unsigned size) {
    return size <= LS_PREVIEW_TARGET;
}

/* ------------------------------------------------------------------- log text (sizes only) */

static void ls_add(char *text, unsigned cap, const char *more) {
    unsigned n = 0;
    while (n + 1 < cap && text[n]) ++n;
    while (n + 1 < cap && *more) text[n++] = *more++;
    text[n] = 0;
}

static void ls_add_number(char *text, unsigned cap, unsigned value) {
    char digits[12], out[12];
    unsigned d = 0, n = 0;
    do { digits[d++] = (char)('0' + value % 10); value /= 10; } while (value);
    while (d) out[n++] = digits[--d];
    out[n] = 0;
    ls_add(text, cap, out);
}

static void ls_add_hex(char *text, unsigned cap, unsigned value) {
    char out[11];
    out[0] = '0'; out[1] = 'x';
    for (int i = 0; i < 8; ++i) out[2 + i] = "0123456789ABCDEF"[(value >> (28 - 4 * i)) & 0xf];
    out[10] = 0;
    ls_add(text, cap, out);
}

/* "LIVERY SHARE: preview <size> bytes: <what>[ <w>x<h> <n> bytes][ (<why>)]." */
static void ls_log(unsigned size, const char *what, const unsigned *rgba, const char *why) {
    char text[160];
    text[0] = 0;
    ls_add(text, sizeof(text), "LIVERY SHARE: preview ");
    ls_add_number(text, sizeof(text), size);
    ls_add(text, sizeof(text), " bytes: ");
    ls_add(text, sizeof(text), what);
    if (rgba) {
        ls_add(text, sizeof(text), " ");
        ls_add_number(text, sizeof(text), rgba[0]);
        ls_add(text, sizeof(text), "x");
        ls_add_number(text, sizeof(text), rgba[1]);
        ls_add(text, sizeof(text), " ");
        ls_add_number(text, sizeof(text), rgba[2]);
        ls_add(text, sizeof(text), " bytes");
    }
    if (why) {
        ls_add(text, sizeof(text), " (");
        ls_add(text, sizeof(text), why);
        ls_add(text, sizeof(text), ")");
    }
    ls_add(text, sizeof(text), ".\r\n");
    log_append(text);
}

/* ------------------------------------------------------------------------ the completion */

/* §4a: the share continues without a preview. The completion owns the reference in *stream (a
   by-value TReference: the original releases it on return), so it is released here. */
static int ls_without_preview(void **self, void **stream) {
    unsigned char *popup = 0;
    void *old = 0, *none = 0;
    unsigned state = LS_STATE_PREVIEW_READY, ready = 1;
    if (!self || !memory_read(self, &popup, sizeof(popup)) || !popup ||
        !memory_read(popup + LS_POPUP_PREVIEW, &old, sizeof(old)))
        return 0;
    void *source = 0;
    if (stream && memory_read(stream, &source, sizeof(source)) && source) {
        memory_copy(stream, &none, sizeof(none));
        fi_game_stream_release(source);
    }
    memory_copy(popup + LS_POPUP_PREVIEW, &none, sizeof(none));
    if (old) fi_game_stream_release(old);
    memory_copy(popup + LS_POPUP_STATE, &state, sizeof(state));
    memory_copy(popup + LS_POPUP_READY, &ready, sizeof(ready));
    return 1;
}

/* The detour of game+0xe2d0a0: void (PopupShareLivery **self, TReference<CBinaryStream> stream). */
static void ls_complete(void **self, void **stream) {
    void *source = 0;
    if (!stream || !memory_read(stream, &source, sizeof(source)) || !source) {
        ls_complete_original(self, stream); /* no stream: the game's own error */
        return;
    }
    long long measured = fi_game_stream_size(source);
    unsigned size = measured < 0 ? 0 : (unsigned)measured;
    if (measured >= 0 && fi_livery_share_original_fits(size)) {
        InterlockedExchange(&ls_last_preview, (LONG)size);
        ls_log(size, "passed", 0, 0);
        ls_complete_original(self, stream);
        return;
    }
    unsigned rgba[3] = {0, 0, 0};
    void *fitted = 0;
    const char *why = "stream size unknown";
    if (measured >= 0 && fi_game_stream_fit_png(source, size, &ls_fit, &fitted, &rgba[0], &rgba[1], &rgba[2], &why)) {
        memory_copy(stream, &fitted, sizeof(fitted));
        fi_game_stream_release(source);
        InterlockedExchange(&ls_last_preview, (LONG)rgba[2]);
        ls_log(size, "rgba", rgba, 0);
        ls_complete_original(self, stream);
        return;
    }
    if (!ls_without_preview(self, stream)) {
        InterlockedExchange(&ls_last_preview, (LONG)size);
        ls_log(size, "passed", 0, "popup unreadable");
        ls_complete_original(self, stream);
        return;
    }
    InterlockedExchange(&ls_last_preview, 0);
    ls_log(size, "sent without preview", 0, why);
}

/* The Create stub's result as the share step sees it: logged when negative and the caller is the
   share step (the value it passes to the popup's error handler). */
static void ls_create_result(uintptr_t result, const void *return_address) {
    int code = (int)(unsigned)result;
    if (code >= 0 || !ls_create_return || return_address != ls_create_return) return;
    char text[160];
    text[0] = 0;
    ls_add(text, sizeof(text), "LIVERY SHARE: the Create stub refused before sending: ");
    ls_add_hex(text, sizeof(text), (unsigned)code);
    ls_add(text, sizeof(text), " (preview last sent ");
    ls_add_number(text, sizeof(text), (unsigned)ls_last_preview);
    ls_add(text, sizeof(text), " bytes).\r\n");
    log_append(text);
}

/* The detour of game+0xf14b30 (ten arguments at both call sites; integer only). */
static uintptr_t ls_create(uintptr_t a1, uintptr_t a2, uintptr_t a3, uintptr_t a4, uintptr_t a5, uintptr_t a6,
                           uintptr_t a7, uintptr_t a8, uintptr_t a9, uintptr_t a10) {
    uintptr_t result = ls_create_original(a1, a2, a3, a4, a5, a6, a7, a8, a9, a10);
    ls_create_result(result, __builtin_return_address(0));
    return result;
}

/* ---------------------------------------------------------------------------- install */

static int ls_bytes_at(unsigned char *image, unsigned rva, const unsigned char *bytes, unsigned size) {
    unsigned char current[32];
    return size <= sizeof(current) && memory_read(image + rva, current, size) && memory_equal(current, bytes, size);
}

static int ls_image_ok(unsigned char *image) {
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    return image && memory_read(image, &dos, sizeof(dos)) && dos.e_magic == IMAGE_DOS_SIGNATURE &&
           dos.e_lfanew >= 64 && dos.e_lfanew <= 4096 && memory_read(image + dos.e_lfanew, &nt, sizeof(nt)) &&
           nt.Signature == IMAGE_NT_SIGNATURE && nt.FileHeader.Machine == IMAGE_FILE_MACHINE_AMD64 &&
           nt.FileHeader.TimeDateStamp == GAME_TIMESTAMP && nt.OptionalHeader.Magic == IMAGE_NT_OPTIONAL_HDR64_MAGIC &&
           nt.OptionalHeader.SizeOfImage == GAME_IMAGE_SIZE;
}

/* A 14-byte absolute jump at image+rva to `target`; the pinned `size` bytes move to a trampoline
   that jumps back to image+rva+size. Returns the trampoline, or 0 with the game untouched. */
static void *ls_detour(unsigned char *image, unsigned rva, const unsigned char *bytes, unsigned size, void *target) {
    unsigned char *code = VirtualAlloc(0, 64, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!code) return 0;
    unsigned char jump[14] = {0xff, 0x25, 0, 0, 0, 0};
    uintptr_t back = (uintptr_t)(image + rva + size), to = (uintptr_t)target;
    memory_copy(code, bytes, size);
    memory_copy(jump + 6, &back, sizeof(back));
    memory_copy(code + size, jump, sizeof(jump));
    DWORD old_protection, ignored;
    if (!VirtualProtect(code, 64, PAGE_EXECUTE_READ, &old_protection)) {
        VirtualFree(code, 0, MEM_RELEASE);
        return 0;
    }
    FlushInstructionCache(GetCurrentProcess(), code, 64);
    unsigned char *at = image + rva;
    if (!VirtualProtect(at, sizeof(jump), PAGE_EXECUTE_READWRITE, &old_protection)) {
        VirtualFree(code, 0, MEM_RELEASE);
        return 0;
    }
    memory_copy(jump + 6, &to, sizeof(to));
    memory_copy(at, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), at, sizeof(jump));
    VirtualProtect(at, sizeof(jump), old_protection, &ignored);
    return code;
}

int fi_livery_share_install(unsigned char *image) {
    int done = 0;
    if (!ls_image_ok(image)) return 0;
    if (ls_image && ls_image != image) return 0;
    ls_image = image;
    if (!ls_complete_original && fi_game_stream_install(image) &&
        ls_bytes_at(image, LS_COMPLETE_RVA, ls_complete_bytes, sizeof(ls_complete_bytes))) {
        void *code = ls_detour(image, LS_COMPLETE_RVA, ls_complete_bytes, sizeof(ls_complete_bytes),
                               (void *)(uintptr_t)ls_complete);
        ls_complete_original = (LsCompleteFn)(uintptr_t)code;
    }
    if (ls_complete_original) done |= 1;
    if (!ls_create_original && ls_bytes_at(image, LS_CREATE_CALL_RVA, ls_create_call_bytes, sizeof(ls_create_call_bytes)) &&
        ls_bytes_at(image, LS_CREATE_RVA, ls_create_bytes, sizeof(ls_create_bytes))) {
        ls_create_return = image + LS_CREATE_CALL_RVA + 5;
        void *code = ls_detour(image, LS_CREATE_RVA, ls_create_bytes, sizeof(ls_create_bytes),
                               (void *)(uintptr_t)ls_create);
        ls_create_original = (LsCreateFn)(uintptr_t)code;
    }
    if (ls_create_original) done |= 2;
    return done;
}

void fi_livery_share_apply(unsigned char *game_image) {
    int done = fi_livery_share_install(game_image);
    log_append(done & 1 ? "LIVERY SHARE: previews over 190000 bytes are scaled down to RGBA PNGs through the "
                          "overlay, or the share goes without one.\r\n"
                        : "LIVERY SHARE: the preview completion is not the supported build; previews go as they "
                          "are.\r\n");
    log_append(done & 2 ? "LIVERY SHARE: the Create stub's refusals are logged.\r\n"
                        : "LIVERY SHARE: the Create stub is not the supported build; refusals are not logged.\r\n");
}
