/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The game's PLAYER DETAILS (OD-112 follow-up; RE: re/gamer-card, gamer-card.md). FH2 shows a
   player card through the popup class PopupShowPlayerCard: its show method (vtable slot 145)
   calls the WinRT ISystemUIStatics::ShowProfileCardAsync with the target's XUID as a decimal
   HSTRING and keeps the IAsyncAction it returns; the popup's per-frame update closes the popup
   once that action is not Started, or when there is none. This file replaces slot 145 only:

   - the XUID is read from the popup (+0x470) with WindowsGetStringRawBuffer through the game's
     own import, the implementation that created the string, and must be 1-20 ASCII digits;
   - it goes to the overlay the launcher loaded (fi_overlay_player_card, an optional export that
     returns at once); the popup gets no action (+0x460 stays null) and closes on the next frame,
     as a card shown and dismissed. No failed HRESULT ever reaches the game's SystemUI wrappers
     (they throw on one);
   - without that overlay (no launcher session, an older overlay) the original show runs: the
     game behaves as before.

   Read only: nothing of the player card is changed, and no social action happens here (OD-73).
   Installed once, after checking the game build (TimeDateStamp, SizeOfImage), the slot's current
   value and the original method's first bytes; any mismatch leaves the game untouched. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define GC_SLOT_RVA 0x2e36940u   /* PopupShowPlayerCard vtable 0x2e364b8, slot 145 */
#define GC_SHOW_RVA 0xe38860u    /* its show method */
#define GC_RAW_BUFFER_IAT 0x1dd0ed8u /* combase WindowsGetStringRawBuffer, the game's import */
#define GC_XUID_OFFSET 0x470u    /* HSTRING, the target's XUID in decimal */

static const unsigned char gc_show_bytes[16] = {0x40, 0x57, 0x48, 0x83, 0xec, 0x30, 0x48, 0xc7,
                                                0x44, 0x24, 0x20, 0xfe, 0xff, 0xff, 0xff, 0x48};

typedef void (*GcShowFn)(void *popup);
typedef const wchar_t *(WINAPI *GcRawBufferFn)(void *hstring, UINT32 *length);
typedef void (*GcPlayerCardFn)(const char *xuid, const char *gamertag);

static unsigned char *gc_image;
static GcShowFn gc_original;

/* The XUID text as `out` (NUL-terminated, at most 20 digits): its length, 0 when it is not
   1-20 ASCII digits. */
static unsigned gc_xuid(const wchar_t *text, UINT32 length, char out[21]) {
    if (!text || length == 0 || length > 20) return 0;
    for (UINT32 i = 0; i < length; ++i) {
        if (text[i] < L'0' || text[i] > L'9') return 0;
        out[i] = (char)text[i];
    }
    out[length] = 0;
    return length;
}

/* The game build and the bytes this file replaces: 1 when slot 145 may be taken. */
static int gc_image_ok(unsigned char *image) {
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    void *slot = 0, *raw = 0;
    unsigned char show[sizeof(gc_show_bytes)];
    return image && memory_read(image, &dos, sizeof(dos)) && dos.e_magic == IMAGE_DOS_SIGNATURE &&
           dos.e_lfanew >= 64 && dos.e_lfanew <= 4096 && memory_read(image + dos.e_lfanew, &nt, sizeof(nt)) &&
           nt.Signature == IMAGE_NT_SIGNATURE && nt.FileHeader.Machine == IMAGE_FILE_MACHINE_AMD64 &&
           nt.FileHeader.TimeDateStamp == GAME_TIMESTAMP && nt.OptionalHeader.Magic == IMAGE_NT_OPTIONAL_HDR64_MAGIC &&
           nt.OptionalHeader.SizeOfImage == GAME_IMAGE_SIZE &&
           memory_read(image + GC_SLOT_RVA, &slot, sizeof(slot)) && slot == (void *)(image + GC_SHOW_RVA) &&
           memory_read(image + GC_SHOW_RVA, show, sizeof(show)) && memory_equal(show, gc_show_bytes, sizeof(show)) &&
           memory_read(image + GC_RAW_BUFFER_IAT, &raw, sizeof(raw)) && raw;
}

/* Slot 145, on the game's UI thread. */
static void gc_show(void *popup) {
    GcPlayerCardFn card = (GcPlayerCardFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_player_card");
    if (!card) {
        gc_original(popup); /* no overlay to show it: the game's own card, as before */
        return;
    }
    void *hstring = 0, *raw = 0;
    if (!popup || !memory_read((unsigned char *)popup + GC_XUID_OFFSET, &hstring, sizeof(hstring)) || !hstring ||
        !memory_read(gc_image + GC_RAW_BUFFER_IAT, &raw, sizeof(raw)) || !raw) {
        return; /* nothing to show; the popup closes on its next frame */
    }
    UINT32 length = 0;
    const wchar_t *text = ((GcRawBufferFn)(uintptr_t)raw)(hstring, &length);
    char xuid[21];
    if (gc_xuid(text, length, xuid)) card(xuid, 0);
}

void fi_gamer_card_apply(unsigned char *game_image) {
    if (!gc_image_ok(game_image)) {
        log_append("GAMER CARD: the game's player card is not the supported build: PLAYER DETAILS shows the game's own card.\r\n");
        return;
    }
    void **slot = (void **)(game_image + GC_SLOT_RVA);
    DWORD old_protection, ignored;
    if (!VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old_protection)) {
        log_append("GAMER CARD: not installed (VirtualProtect failed).\r\n");
        return;
    }
    gc_image = game_image;
    gc_original = (GcShowFn)(uintptr_t)(game_image + GC_SHOW_RVA);
    *slot = (void *)(uintptr_t)gc_show;
    VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
    log_append("GAMER CARD: PLAYER DETAILS opens the overlay's player pane (the game's card without the overlay).\r\n");
}
