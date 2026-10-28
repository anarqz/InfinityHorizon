/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md.

   azimuth.exe itself: which build it is (PE TimeDateStamp and SizeOfImage) and a guarded way
   to change a few of its bytes (hash of the whole range checked before and after). */
#ifndef FI_AZIMUTH_HOST_H
#define FI_AZIMUTH_HOST_H
#include "infinity_base.h"

typedef struct {
    unsigned offset, size;
    const unsigned char *bytes;
} HostCodePatch;

typedef struct {
    unsigned rva, size;
    const unsigned char *original_hash, *patched_hash;
    const HostCodePatch *patches;
    unsigned patch_count;
} HostPatchSet;

/* The builds the hook recognises. Only AZIMUTH_0910 is supported next to ExtraOptions 0.2.98;
   0.9.7 and 0.9.9 keep their tables for the players a self-update moves from 0.2.74+fh2online.58
   (the launcher requires 0.9.10). */
typedef enum {
    AZIMUTH_UNKNOWN,
    AZIMUTH_096,
    AZIMUTH_097,
    AZIMUTH_098,
    AZIMUTH_099,
    AZIMUTH_0910,
    /* ExtraOptions.Infinity (OD-125): the only build of the winmm.dll route */
    AZIMUTH_103
} AzimuthBuild;

AzimuthBuild azimuth_host_build(const unsigned char *base);

/* 1 = applied, 2 = already applied, 0 = unknown build, -1 = write failed. */
int azimuth_patch_host_code(unsigned char *base, const HostPatchSet *set);
int azimuth_patch_host_codes(unsigned char *base, const HostPatchSet *sets, unsigned count);
#endif
