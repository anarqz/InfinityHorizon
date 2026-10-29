/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The game's IOSys streams, for detours that replace the bytes a stream carries (T19; PNG_FIT.md).
   A site detour (livery_share.c; a later photo share) only adds its own pinned site and FiPngFitV1
   values: reading a stream, fitting a picture through the overlay and building the game's
   IOSys::CDynamicMemoryStream are here. */
#ifndef FI_GAME_STREAM_H
#define FI_GAME_STREAM_H
#include "infinity.h"

/* The overlay's FiPngFit, version 1 (include/fi_overlay.h of the overlay; the same layout, checked by
   tests/livery_share_test.c). */
#define FI_PNG_FIT_V1 1u
#define FI_PNG_FIT_V1_STEPS 16u
typedef struct FiPngFitV1 {
    unsigned version, size, max_bytes, hard_cap_bytes, keep_alpha, min_width, step_count;
    unsigned steps_permille[FI_PNG_FIT_V1_STEPS];
} FiPngFitV1;

#define FI_GAME_STREAM_READ_CAP (4u << 20) /* the largest stream read for a fit */
#define FI_GAME_STREAM_FIT_CAP 409600u     /* the output buffer handed to the overlay */

/* 1 when the game's memory-stream construction (game+0x11e48e0) and the vtable slots used here are
   the supported build's; then fi_game_stream_new can build streams. Idempotent. */
int fi_game_stream_install(unsigned char *game_image);
/* The stream's size (vtable 0xd8), -1 when it cannot be asked. */
long long fi_game_stream_size(void *stream);
/* All `size` bytes from the start (seek 0x38, read 0x90): 1 when every byte came. */
int fi_game_stream_read_all(void *stream, unsigned char *buffer, unsigned size);
/* A new CDynamicMemoryStream with `data` (one reference, positioned at 0), built as
   game+0x11e48e0 builds one; 0 on any failure. */
void *fi_game_stream_new(const unsigned char *data, unsigned size);
/* Release one reference (vtable 0x18). */
void fi_game_stream_release(void *object);

/* The picture in `stream` (`size` bytes) fitted by the overlay's fi_overlay_fit_png into a new
   stream: 1 with *fitted, *width, *height and *bytes; 0 with *why (no overlay or export, an older
   overlay, too large to read, a read, fit or build failure). The source stream is not released. */
int fi_game_stream_fit_png(void *stream, unsigned size, const FiPngFitV1 *fit, void **fitted, unsigned *width,
                           unsigned *height, unsigned *bytes, const char **why);
#endif
