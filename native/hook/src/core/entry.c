/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md.

   The DLL's entry point. Azimuth loads this file as data\bin\azimuth-tls.dll, its static-TLS
   helper (azimuth_tls.c). With the player's ExtraOptions 0.2.98 present, that load runs inside
   ExtraOptions' LoadLibraryExW hook, so DllMain comes before ExtraOptions patches the game:
   nothing here may touch a byte ExtraOptions checks (tests/eo098-sites.tsv). */
#include "runtime.h"
#include "support.h"

/* The status line of infinity.log ("OK: ..." / "REFUSED: ..."). ExtraOptions exports its own
   as Fh2TweaksStatus; ours has its own name so tools never confuse the two modules. */
__declspec(dllexport) const char *InfinityStatus(void) {
    return status_get();
}

BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, LPVOID reserved) {
    (void)reserved;
    if (reason == DLL_PROCESS_ATTACH) {
        fi_process_attach(instance);
    }
    return TRUE;
}
