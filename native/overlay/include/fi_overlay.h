// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
/* infinity-overlay.dll host ABI, version 1 (docs/launcher/ORCHESTRATOR.md §4.2). Stable: add
 * fields only at the end, raise `size`, keep `abi` at 1 while old fields keep their meaning.
 *
 * The hook loads the DLL on a worker thread after the ticket (never in DllMain), resolves
 * `fi_overlay_start` and calls it once. The call returns at once; the overlay's work continues on
 * its own threads. The DLL is never unloaded (it pins itself). */
#ifndef FI_OVERLAY_H
#define FI_OVERLAY_H

#include <stdint.h>
#include <wchar.h>

#ifdef __cplusplus
extern "C" {
#endif

#define FI_OVERLAY_ABI 1u
#define FI_OVERLAY_DEV 0x1u /* flags bit 0: development host (the D3D12 test app) */
#define FI_OVERLAY_OFFLINE 0x2u /* flags bit 1 (OD-118): the launcher's Play offline; online-only tabs say "Offline mode" */

/* livery_sync results (OD-65 amendment: the current car's paint before a world). Retired by CR-S59
   (the server keeps the liveries from the game's own calls): this overlay never calls it, and a hook
   with CR-S59 answers FI_LIVERY_UNAVAILABLE at once. The slot stays for the ABI. */
#define FI_LIVERY_SYNCED 0          /* uploaded: world mates fetch it by its id */
#define FI_LIVERY_STOCK 1           /* the current car has no design: nothing to upload */
#define FI_LIVERY_TIMEOUT (-1)      /* not done within timeout_ms (the upload may still finish) */
#define FI_LIVERY_FAILED (-2)       /* the server or the game refused it */
#define FI_LIVERY_UNAVAILABLE (-3)  /* this hook cannot upload (yet) */

/* The size of the first host (no livery_sync): an overlay accepts it and reads no further. */
#define FI_OVERLAY_HOST_V1_SIZE 40u

/* fi_hook_capture `what` values. */
#define FI_CAPTURE_SWAPCHAIN 1u /* IDXGISwapChain3* (borrowed: the overlay never releases it) */
#define FI_CAPTURE_QUEUE 2u     /* ID3D12CommandQueue* (borrowed) */

typedef struct FiOverlayHost {
    uint32_t abi;                    /* FI_OVERLAY_ABI */
    uint32_t size;                   /* sizeof(FiOverlayHost): 48 on x64 (40 without livery_sync) */
    const wchar_t *ui_pipe;          /* L"\\\\.\\pipe\\ForzaInfinity-<32 hex>-ui", NUL-terminated; copied */
    void *(*capture)(uint32_t what); /* hook export fi_hook_capture, or NULL */
    void (*log)(const char *line);   /* hook's log_append (UTF-8, no newline), or NULL: then
                                        data\bin\infinity-overlay.log next to the DLL */
    uint32_t flags;                  /* FI_OVERLAY_DEV */
    /* size >= 48 (hook 0.3.0+fh2online.65 and later): retired (CR-S59), never called by this
       overlay; it returned a FI_LIVERY_* code within timeout_ms, on an overlay thread. */
    int32_t (*livery_sync)(uint32_t timeout_ms);
} FiOverlayHost;

/* Return values. */
#define FI_OVERLAY_OK 0
#define FI_OVERLAY_E_NULL (-1)     /* host is NULL */
#define FI_OVERLAY_E_ABI (-2)      /* abi != FI_OVERLAY_ABI */
#define FI_OVERLAY_E_SIZE (-3)     /* size smaller than this DLL reads */
#define FI_OVERLAY_E_PIPE (-4)     /* ui_pipe missing or malformed */
#define FI_OVERLAY_E_AGAIN (-5)    /* already started in this process */
#define FI_OVERLAY_E_PANIC (-6)    /* failed while starting (caught) */
#define FI_OVERLAY_E_PLATFORM (-7) /* not a Windows build */

/* Returns at once: 0 started (work continues on the overlay's own threads), <0 refused. */
__declspec(dllexport) int32_t fi_overlay_start(const FiOverlayHost *host);

/* Optional export (overlay 0.2.0, OD-112; resolve with GetProcAddress, absent before): the game's
 * player card (PLAYER DETAILS) for a player. Any thread, returns at once; the guide opens on the
 * player's pane. xuid: decimal digits; gamertag: UTF-8, may be NULL. Both NUL-terminated, copied. */
__declspec(dllexport) void fi_overlay_player_card(const char *xuid, const char *gamertag);

/* Optional exports (T19, OD-138; resolve with GetProcAddress, absent before; docs/launcher/PNG_FIT.md):
 * fit a picture into a byte budget as a PNG the game reads (the game decodes through WIC as 24/32bpp
 * BGR(A)/RGBA only, livery-share.md §4d). The input is any picture WIC decodes. Each step scales it
 * (Fant, aspect kept) to steps_permille[i]/1000 of its sides and writes an 8-bit, non-interlaced PNG
 * with only IHDR/IDAT/IEND (colour type 6 RGBA with keep_alpha, else 2 RGB), adaptive row filters,
 * zlib level 9; the first step at most max_bytes wins. Inputs over 4096x4096 pixels (16.8 Mpx) are
 * refused from their header, before any decode (part of version 1). Any thread: COM is initialised
 * for the call (an STA thread keeps its apartment). Nothing is allocated for the caller. */
#define FI_PNG_FIT_VERSION 1u     /* FiPngFit.version this overlay reads (fi_overlay_png_fit_version) */
#define FI_PNG_FIT_MAX_STEPS 16u
typedef struct FiPngFit {
    uint32_t version;        /* FI_PNG_FIT_VERSION; a newer version than the overlay's is refused */
    uint32_t size;           /* sizeof(FiPngFit): 92 for version 1 */
    uint32_t max_bytes;      /* the PNG answered is at most this many bytes */
    uint32_t hard_cap_bytes; /* and never more than this, whatever out_cap (0: out_cap only) */
    uint32_t keep_alpha;     /* 1: colour type 6 (RGBA); 0: colour type 2 (RGB, alpha dropped) */
    uint32_t min_width;      /* a step narrower than this ends the search (at least 1) */
    uint32_t step_count;     /* used entries of steps_permille (1..FI_PNG_FIT_MAX_STEPS) */
    uint32_t steps_permille[FI_PNG_FIT_MAX_STEPS]; /* 1..1000, tried in order (1000: original size) */
} FiPngFit;

#define FI_PNG_FIT_OK 0
#define FI_PNG_FIT_E_ARGS (-1)    /* a null pointer, empty or > 4 MiB input, a bad step list */
#define FI_PNG_FIT_E_VERSION (-2) /* fit->version newer than this overlay, or size too small */
#define FI_PNG_FIT_E_COM (-3)     /* COM or the WIC factory unavailable on this thread */
#define FI_PNG_FIT_E_DECODE (-4)  /* not a picture WIC decodes, a side over 16384, or over 4096x4096 pixels */
#define FI_PNG_FIT_E_ENCODE (-5)  /* scaling or writing failed */
#define FI_PNG_FIT_E_TOO_BIG (-6) /* no step came within max_bytes (and the caps) */

/* Returns FI_PNG_FIT_OK with *out_size bytes in `out` and the PNG's sides, or < 0 (outputs 0). */
__declspec(dllexport) int32_t fi_overlay_fit_png(const void *in, size_t in_size, const FiPngFit *fit, void *out,
                                                 size_t out_cap, size_t *out_size, uint32_t *out_width,
                                                 uint32_t *out_height);
/* The highest FiPngFit.version fi_overlay_fit_png reads. */
__declspec(dllexport) uint32_t fi_overlay_png_fit_version(void);

/* Optional exports (OD-147, docs/launcher/VOICE_CHAT.md §11.4, §12; resolve with GetProcAddress, absent before):
 * voice chat for the game's own UI. The hook answers the game's talking/muted/voice-ability queries from these,
 * so the game's player list and HUD show our voice chat. Any thread, lock-free, returns at once (the game calls
 * its queries every frame from its UI thread); never blocks on the pipe.
 *
 * fi_overlay_voice_state(xuid): FI_VOICE_* bits for that player as this player hears them, or 0 when the player
 * is unknown or voice chat is off in the lobby or in this player's settings. xuid 0 is the local player (the hook
 * knows its own XUID; the overlay does not): FI_VOICE_ABLE when voice is on and the microphone is not muted, plus
 * FI_VOICE_TALKING while it sends (VC-3). */
#define FI_VOICE_ABLE 0x1u    /* has voice: the game shows a microphone and offers Mute/Unmute */
#define FI_VOICE_TALKING 0x2u /* heard talking now (never set while muted) */
#define FI_VOICE_MUTED 0x4u   /* muted by this player, for the lobby by the host, or voice-blocked */
__declspec(dllexport) uint32_t fi_overlay_voice_state(uint64_t xuid);

/* The game's own Mute/Unmute action on a player (muted 1/0): becomes the overlay's mute (0x1C VoiceCommand). */
__declspec(dllexport) void fi_overlay_voice_mute(uint64_t xuid, int32_t muted);

#ifdef __cplusplus
}
#endif

#endif /* FI_OVERLAY_H */
