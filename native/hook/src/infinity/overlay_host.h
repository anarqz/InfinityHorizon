/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The hook's side of FiOverlayHost (native/overlay/include/fi_overlay.h): what launch.c hands
   fi_overlay_start. Fields are only added at the end with `size` raised; the overlay accepts an
   older size and reads no further. */
#ifndef FI_OVERLAY_HOST_H
#define FI_OVERLAY_HOST_H
#include <stddef.h>
#include <stdint.h>

typedef struct {
    unsigned abi, size;
    const wchar_t *ui_pipe;
    void *(*capture)(unsigned what);
    void (*log)(const char *line);
    unsigned flags;
    int (*livery_sync)(unsigned timeout_ms); /* livery.c: retired by CR-S59, always unavailable */
    /* OD-154 VC-2: voice_net.c fi_voice_send; this player's XUID (the handoff's), 0 unknown */
    int32_t (*voice_send)(const uint8_t *datagram, uint32_t size);
    uint64_t xuid;
} LaunchOverlayHost;

_Static_assert(offsetof(LaunchOverlayHost, livery_sync) == 40, "FiOverlayHost.livery_sync is at 40");
_Static_assert(offsetof(LaunchOverlayHost, voice_send) == 48, "FiOverlayHost.voice_send is at 48");
_Static_assert(offsetof(LaunchOverlayHost, xuid) == 56, "FiOverlayHost.xuid is at 56");
_Static_assert(sizeof(LaunchOverlayHost) == 64, "FiOverlayHost is 64 bytes");
#endif
