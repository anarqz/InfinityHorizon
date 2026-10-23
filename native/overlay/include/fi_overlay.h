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

/* livery_sync results (OD-65 amendment: the current car's paint goes to the server before a world). */
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
    /* size >= 48 (hook 0.3.0+fh2online.65 and later): uploads the current car's paint and returns
       a FI_LIVERY_* code within timeout_ms; called on an overlay thread, never the game's. */
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

#ifdef __cplusplus
}
#endif

#endif /* FI_OVERLAY_H */
