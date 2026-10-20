/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md. */
#ifndef FI_RUNTIME_H
#define FI_RUNTIME_H
#include "infinity_base.h"
/* azimuth_tls.c: DLL_PROCESS_ATTACH (Azimuth's TLS helper load, under the loader lock). */
void fi_process_attach(HINSTANCE instance);
/* src/infinity/runtime.c: everything the hook does once the mapped game image is found.
   `executable` is the game's forza_durango_release_final.exe, `bin` this DLL's folder
   (data\bin) and `root` the folder of azimuth.exe. 1 when it ran. */
int fi_runtime_initialize(unsigned char *game_image, const wchar_t *executable, const wchar_t *bin,
                          const wchar_t *root);
#endif
