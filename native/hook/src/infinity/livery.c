/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74:
   adapters/azimuth_livery.c), GPL-3.0-or-later. See LICENSE and NOTICE.md. */
#include "infinity.h"
#include "../core/logging.h"

/* 2x MSAA liveries on Azimuth 0.9.6 to 0.9.9: their mask helper does not recognise two samples
   and selects the wrong fragment bits; the patch below (as the hook had it up to
   0.2.74+fh2online.58) makes it do both. 0.9.10 rewrote the helper (FUN_14014bde0: the 2x, 4x
   and 8x FMASK views, quality levels included), so there is nothing to patch there. These host
   bytes are not the player's ExtraOptions' business on those builds: ExtraOptions 0.2.98 refuses
   any azimuth.exe but 0.9.10's (adapters/azimuth.c arm_loader), so it never runs where this
   does, and on 0.9.10 this does nothing. Kept for the players a self-update brings from
   0.2.74+fh2online.58 on an older Azimuth (the launcher itself requires 0.9.10). */
/* FiOverlayHost.livery_sync (OD-65 amendment, OD-66): the pre-world run of the current car's
   paint is retired (CR-S59, owner 2026-10-27 "Liveries are stored on the server again; the live
   lobby sync retires"): the server keeps every ticket's liveries from the game's own calls, so
   this answers "unavailable" at once. The slot stays in the host so the overlay ABI is unchanged. */
int fi_livery_sync(unsigned timeout_ms) {
    (void)timeout_ms;
    return FI_LIVERY_UNAVAILABLE;
}

#define MASK_HELPER_RVA 0x100b70u
#define MASK_HELPER_SIZE 0x2d2u

static const unsigned char recognize_two_samples[] = {0xf6, 0xc3, 0x06, 0x74, 0x1c};
static const unsigned char select_fragment_bits[] = {0x8b, 0xfb, 0xd1, 0xef, 0x45, 0x8b, 0xda, 0x45,
                                                     0x8b, 0xc2, 0x90, 0x90, 0x90, 0x90, 0x90};
static const HostCodePatch mask_patches[] = {
    {0x4b, sizeof(recognize_two_samples), recognize_two_samples},
    {0xbc, sizeof(select_fragment_bits), select_fragment_bits}};

static const unsigned char original_hash[32] = {
    0x25, 0xc8, 0xa4, 0xc4, 0x90, 0xb9, 0x62, 0x91, 0x7d, 0xed, 0x6c, 0xe2, 0x5f, 0xbc, 0xe7, 0xf2,
    0x92, 0x5c, 0xaa, 0xce, 0x86, 0x77, 0x94, 0x8e, 0x8a, 0x4c, 0xf3, 0x5b, 0x0e, 0x5c, 0x6e, 0x63};
static const unsigned char patched_hash[32] = {
    0xf7, 0x9b, 0x2e, 0x1c, 0x76, 0xb7, 0x3d, 0xcb, 0x14, 0x01, 0x76, 0xdd, 0x7e, 0x3e, 0xed, 0xcc,
    0xe1, 0x2e, 0x70, 0x20, 0x06, 0x78, 0x5c, 0xb9, 0x85, 0x4f, 0x92, 0x7b, 0x28, 0xe9, 0x0e, 0x02};

static const HostPatchSet mask_patch_set = {MASK_HELPER_RVA, MASK_HELPER_SIZE,
                                            original_hash,   patched_hash,
                                            mask_patches,    ARRAY_COUNT(mask_patches)};

static const unsigned char original_hash_097[32] = {
    0x6b, 0x39, 0x07, 0xf0, 0xed, 0x57, 0xca, 0x8c, 0x3c, 0x86, 0x90, 0xa9, 0x16, 0xa1, 0x92, 0x70,
    0xa8, 0xa9, 0xe6, 0x7d, 0xc8, 0x7b, 0xec, 0xf9, 0x15, 0x11, 0x91, 0x66, 0x1e, 0x9f, 0x82, 0xcb};
static const unsigned char patched_hash_097[32] = {
    0x5f, 0x7f, 0x89, 0x5f, 0x0f, 0xd1, 0x40, 0xa7, 0x97, 0x38, 0x05, 0xcf, 0xb6, 0xe0, 0x63, 0xc2,
    0x49, 0x35, 0x87, 0xb3, 0xae, 0xd0, 0x31, 0x4c, 0x7e, 0xed, 0xa1, 0xcb, 0x9f, 0x36, 0x17, 0x3c};
static const HostPatchSet mask_patch_set_097 = {0x104320,          MASK_HELPER_SIZE,
                                                original_hash_097, patched_hash_097,
                                                mask_patches,      ARRAY_COUNT(mask_patches)};

static const unsigned char mask_original_hash_098[32] = {
    0x52, 0x98, 0xe9, 0x2f, 0xa1, 0x14, 0xfe, 0xbc, 0xc3, 0x35, 0xa7, 0xaf, 0x69, 0xd2, 0x78, 0x1d,
    0x1c, 0xbd, 0x2f, 0x44, 0xa5, 0x55, 0x93, 0xea, 0xaf, 0xdd, 0x4a, 0x65, 0x6b, 0xbf, 0x9f, 0x75};

static const unsigned char mask_patched_hash_098[32] = {
    0xc1, 0x4a, 0x0f, 0xce, 0xab, 0xe9, 0xce, 0x31, 0x6d, 0x97, 0x96, 0x51, 0xa6, 0xb2, 0x8c, 0x9d,
    0x73, 0xeb, 0x2c, 0xa7, 0x23, 0x3b, 0x32, 0x33, 0x2b, 0x49, 0xb6, 0x87, 0x28, 0xb5, 0xcf, 0x72};

static const HostPatchSet mask_patch_set_098 = {
    0x1046c0,     MASK_HELPER_SIZE,         mask_original_hash_098, mask_patched_hash_098,
    mask_patches, ARRAY_COUNT(mask_patches)};

static const unsigned char mask_original_hash_099[32] = {
    0xec, 0xa7, 0xbe, 0xe6, 0xc4, 0x80, 0xc3, 0xe8, 0xab, 0x13, 0x5b, 0xc3, 0xf6, 0x3a, 0x47, 0x76,
    0xab, 0xaa, 0x43, 0x1c, 0x7b, 0x44, 0xae, 0x99, 0x5b, 0x91, 0x8a, 0x91, 0x82, 0x19, 0xf8, 0x4b};
static const unsigned char mask_patched_hash_099[32] = {
    0x7e, 0x23, 0x14, 0x3f, 0x8f, 0x6a, 0xbd, 0xeb, 0x9c, 0x57, 0x52, 0x72, 0xe1, 0x3a, 0xe6, 0xfd,
    0xf3, 0x80, 0x6e, 0xca, 0xc3, 0x0e, 0xee, 0x08, 0x63, 0xa6, 0xc7, 0x09, 0x94, 0xe9, 0x5f, 0xa3};
static const HostPatchSet mask_patch_set_099 = {0x13dc40,
                                                0x2ea,
                                                mask_original_hash_099,
                                                mask_patched_hash_099,
                                                mask_patches,
                                                ARRAY_COUNT(mask_patches)};

/* Other players' layered liveries (free roam, car meets): the game asks the device's
   CreatePlacementTexture2D (vtable +0x200) for a 4096x4096 R8G8B8A8_UNORM_SRGB texture with 13
   mips, no bind flags and CPU read (desc 4096, 4096, 13, 1, 29, 1, 0, 0, 0, 0x20000, 0), placed
   in its own memory, and Azimuth stops the game ("requires GPU-write/CPU-map/unknown placement
   semantics"; 0.9.9 FUN_140121c40, 0.9.7 FUN_1400ea0b0). Its second accepted profile is a
   shader resource (bind flags 8, no CPU access, one sample, 1-15 mips, 1-2048 slices, usage
   0/1); the patch drops those two tests (the jne after cmp [desc+0x20],8 and after
   cmp [desc+0x24],0) so the texture is made as a placed shader resource instead.
   The game then copies the rendered livery into it on the GPU (context +0x548, at
   game+0x10ebb59) and waits for the GPU (a readback), and Azimuth stops on a "GPU write into a
   CPU-tiled title texture" (+0x128 of the texture set): in its copy check (0.9.9
   FUN_1401a5760) and in its other GPU-write check (FUN_1401a4740). Those two jne go too, so
   the copy happens on the GPU side. What the GPU writes may not reach the game's CPU copy, so a
   remote livery can look wrong, but the game goes on. Only 0.9.9 and 0.9.10 (the same bytes,
   moved) have the write patches, and the texture patch is applied only after them. */
static const unsigned char placement_nops[] = {0x90, 0x90, 0x90, 0x90, 0x90, 0x90};
static const HostCodePatch placement_patches[] = {{0x05, sizeof(placement_nops), placement_nops},
                                                  {0x0f, sizeof(placement_nops), placement_nops}};
static const unsigned char placement_original_hash[32] = {
    0x89, 0x89, 0x43, 0xc4, 0x8a, 0xf4, 0x37, 0xf4, 0x06, 0x01, 0x0b, 0xec, 0x1d, 0xfb, 0x7c, 0x0e,
    0x71, 0x77, 0x07, 0x0a, 0xed, 0x1e, 0xf9, 0xae, 0xae, 0x47, 0x6d, 0x8a, 0xd4, 0x97, 0x9e, 0x6e};
static const unsigned char placement_patched_hash[32] = {
    0xfc, 0xe0, 0x02, 0xef, 0x96, 0xe5, 0x81, 0x40, 0xc2, 0x89, 0xab, 0x5b, 0x7c, 0x03, 0xd2, 0xf5,
    0x01, 0xbe, 0x1a, 0x31, 0x5d, 0xdb, 0x88, 0xd8, 0xe3, 0x61, 0x70, 0x69, 0x13, 0x30, 0x57, 0xb2};
static const HostPatchSet placement_patch_set_099 = {
    0x121e36, 0x15, placement_original_hash, placement_patched_hash,
    placement_patches, ARRAY_COUNT(placement_patches)};

/* cmp qword [texture+0x128],0 / jne stop, in FUN_1401a4740 and FUN_1401a5760 (0.9.9) */
static const unsigned char tiled_nops[] = {0x90, 0x90};
static const HostCodePatch tiled_patches[] = {{0x08, sizeof(tiled_nops), tiled_nops}};
static const unsigned char tiled_write_original_hash[32] = {
    0xcb, 0x51, 0x53, 0xdd, 0x17, 0x35, 0x34, 0xad, 0xcd, 0x08, 0xf7, 0xe0, 0x17, 0xf3, 0xa9, 0x9d,
    0x0d, 0xfe, 0x5f, 0x03, 0x0f, 0x29, 0x32, 0x18, 0xa5, 0x03, 0x04, 0x7b, 0xb7, 0xdf, 0x73, 0x7f};
static const unsigned char tiled_write_patched_hash[32] = {
    0x2d, 0xc0, 0xa1, 0x37, 0x88, 0x54, 0xff, 0x1d, 0x0e, 0x74, 0x0c, 0x2b, 0xaa, 0xc5, 0x3d, 0xbd,
    0x77, 0x10, 0x6c, 0x2a, 0xc1, 0xeb, 0x6d, 0xd1, 0xff, 0x09, 0xe4, 0xee, 0x1e, 0x8f, 0x0e, 0x73};
static const unsigned char tiled_copy_original_hash[32] = {
    0x40, 0x56, 0xf8, 0x89, 0x35, 0xac, 0xf8, 0xf0, 0xb5, 0xc0, 0x08, 0x64, 0x41, 0x44, 0x58, 0x4b,
    0x6d, 0x11, 0xef, 0x7a, 0xb6, 0x1b, 0xbd, 0x41, 0xab, 0x8e, 0x96, 0x5c, 0x53, 0x32, 0xdc, 0x69};
static const unsigned char tiled_copy_patched_hash[32] = {
    0xe3, 0x87, 0xb7, 0x25, 0xbb, 0xa5, 0x7b, 0xc9, 0xe7, 0x1f, 0xcd, 0x87, 0x17, 0xbc, 0x15, 0x56,
    0x3b, 0xb9, 0x77, 0x0e, 0xbe, 0xc3, 0x42, 0x2f, 0x09, 0x06, 0x6e, 0x8b, 0x04, 0x56, 0x68, 0x29};
static const HostPatchSet tiled_patch_sets_099[] = {
    {0x1a475e, 0x0a, tiled_write_original_hash, tiled_write_patched_hash, tiled_patches,
     ARRAY_COUNT(tiled_patches)},
    {0x1a57ed, 0x0a, tiled_copy_original_hash, tiled_copy_patched_hash, tiled_patches,
     ARRAY_COUNT(tiled_patches)}};

/* 0.9.10: FUN_14012f990, FUN_1401b8210, FUN_1401b9230 */
static const HostPatchSet placement_patch_set_0910 = {
    0x12fb86, 0x15, placement_original_hash, placement_patched_hash,
    placement_patches, ARRAY_COUNT(placement_patches)};
static const HostPatchSet tiled_patch_sets_0910[] = {
    {0x1b822e, 0x0a, tiled_write_original_hash, tiled_write_patched_hash, tiled_patches,
     ARRAY_COUNT(tiled_patches)},
    {0x1b92bd, 0x0a, tiled_copy_original_hash, tiled_copy_patched_hash, tiled_patches,
     ARRAY_COUNT(tiled_patches)}};

/* 1.0.3 (FH_AZIMUTH_103), byte for byte the code of 1.0.2: CreatePlacementTexture2D 0x301190
   tests BindFlags already in ecx, so the two jne sit at +0x03 and +0x0d of a 0x13-byte range; the
   write and copy paths share one check, 0x68cb50 (cmp qword [rcx+0x148],0 / jne). */
static const HostCodePatch placement_patches_103[] = {{0x03, sizeof(placement_nops), placement_nops},
                                                      {0x0d, sizeof(placement_nops), placement_nops}};
static const unsigned char placement_original_hash_103[32] = {
    0x41, 0xde, 0xae, 0x9f, 0x9a, 0x5a, 0x32, 0x89, 0x93, 0x24, 0x0e, 0xa3, 0xdf, 0x50, 0x06, 0x20,
    0x37, 0xf5, 0xf4, 0x92, 0xf2, 0x43, 0x1c, 0x27, 0xaa, 0x8b, 0x85, 0x3f, 0x92, 0xda, 0xd0, 0x85};
static const unsigned char placement_patched_hash_103[32] = {
    0x5f, 0x7b, 0x84, 0x38, 0xdd, 0x27, 0x15, 0x6c, 0xdf, 0x53, 0x0e, 0x0c, 0xd6, 0x15, 0x8b, 0x21,
    0xab, 0x0f, 0x80, 0xa9, 0x8a, 0xdf, 0xe7, 0x4f, 0x21, 0x8f, 0x87, 0x3c, 0x87, 0x1d, 0xec, 0x33};
static const HostPatchSet placement_patch_set_103 = {
    0x3013db, 0x13, placement_original_hash_103, placement_patched_hash_103,
    placement_patches_103, ARRAY_COUNT(placement_patches_103)};
static const unsigned char tiled_original_hash_103[32] = {
    0x6f, 0x17, 0x14, 0x16, 0x8c, 0xd2, 0xa0, 0xd4, 0xdd, 0x71, 0x5a, 0x8c, 0x9f, 0x88, 0xe8, 0x19,
    0xae, 0xaf, 0xda, 0x07, 0x58, 0x9a, 0xef, 0x02, 0x7e, 0x31, 0xb6, 0xd7, 0x22, 0x50, 0xa9, 0x51};
static const unsigned char tiled_patched_hash_103[32] = {
    0xea, 0x4d, 0xa0, 0x0b, 0x81, 0x82, 0x3e, 0x9e, 0x9e, 0xb3, 0x12, 0xcc, 0xa2, 0xbb, 0x86, 0x68,
    0xa9, 0x29, 0xad, 0xb0, 0xec, 0xd3, 0x95, 0xbb, 0x01, 0x5e, 0x47, 0x1e, 0x79, 0x03, 0x4e, 0x65};
static const HostPatchSet tiled_patch_sets_103[] = {
    {0x68cb6e, 0x0a, tiled_original_hash_103, tiled_patched_hash_103, tiled_patches,
     ARRAY_COUNT(tiled_patches)}};

/* 1.0.4: the same bytes again, CreatePlacementTexture2D 0x1f0b20 (+0x24b) and the check 0x55c4d0
   (+0x1e); the 1.0.3 hashes and patches. */
static const HostPatchSet placement_patch_set_104 = {
    0x1f0d6b, 0x13, placement_original_hash_103, placement_patched_hash_103,
    placement_patches_103, ARRAY_COUNT(placement_patches_103)};
static const HostPatchSet tiled_patch_sets_104[] = {
    {0x55c4ee, 0x0a, tiled_original_hash_103, tiled_patched_hash_103, tiled_patches,
     ARRAY_COUNT(tiled_patches)}};

static int apply_placement(unsigned char *base) {
    AzimuthBuild build = azimuth_host_build(base);
    if (build == AZIMUTH_103 || build == AZIMUTH_104) {
        const HostPatchSet *tiled = build == AZIMUTH_103 ? tiled_patch_sets_103 : tiled_patch_sets_104;
        int writes = azimuth_patch_host_codes(base, tiled, ARRAY_COUNT(tiled_patch_sets_103));
        return writes > 0 ? azimuth_patch_host_code(base, build == AZIMUTH_103 ? &placement_patch_set_103
                                                                               : &placement_patch_set_104)
                          : writes;
    }
    if (build != AZIMUTH_099 && build != AZIMUTH_0910) {
        return 0;
    }
    const HostPatchSet *tiled = build == AZIMUTH_099 ? tiled_patch_sets_099 : tiled_patch_sets_0910;
    int writes = azimuth_patch_host_codes(base, tiled, ARRAY_COUNT(tiled_patch_sets_099));
    return writes > 0 ? azimuth_patch_host_code(base, build == AZIMUTH_099 ? &placement_patch_set_099
                                                                           : &placement_patch_set_0910)
                      : writes;
}

static int apply_livery(unsigned char *base) {
    static const HostPatchSet *const versions[] = {
        [AZIMUTH_096] = &mask_patch_set,
        [AZIMUTH_097] = &mask_patch_set_097,
        [AZIMUTH_098] = &mask_patch_set_098,
        [AZIMUTH_099] = &mask_patch_set_099,
    };
    AzimuthBuild build = azimuth_host_build(base);
    return build > AZIMUTH_UNKNOWN && build < ARRAY_COUNT(versions) ? azimuth_patch_host_code(base, versions[build])
                                                                     : 0;
}

/* The 2x MSAA mask (0.9.6 to 0.9.9), then the placement texture and tiled-write patches (0.9.9,
   0.9.10). */
void azimuth_apply_livery_compatibility(void) {
    int result = apply_livery((unsigned char *)GetModuleHandleW(NULL));
    AzimuthBuild host = azimuth_host_build((const unsigned char *)GetModuleHandleW(NULL));
    if (host == AZIMUTH_0910 || host == AZIMUTH_103 || host == AZIMUTH_104) {
        log_append(host == AZIMUTH_104   ? "AZIMUTH: 2x MSAA liveries are handled by Azimuth 1.0.4 itself.\r\n"
                   : host == AZIMUTH_103 ? "AZIMUTH: 2x MSAA liveries are handled by Azimuth 1.0.3 itself.\r\n"
                                         : "AZIMUTH: 2x MSAA liveries are handled by Azimuth 0.9.10 itself.\r\n");
    } else if (result > 0) {
        log_append("AZIMUTH: 2x MSAA livery fragment-mask compatibility applied; "
                   "texture resolution and sample count preserved.\r\n");
    } else if (result < 0) {
        log_append("AZIMUTH: livery compatibility could not change memory protection; "
                   "host code left unchanged.\r\n");
    } else {
        log_append("AZIMUTH: 2x MSAA livery compatibility skipped; "
                   "host is not a supported inspected mask-helper build.\r\n");
    }
    result = apply_placement((unsigned char *)GetModuleHandleW(NULL));
    log_append(result > 0   ? "AZIMUTH: layered liveries of other players: CPU-read placement "
                              "textures are made as shader resources.\r\n"
               : result < 0 ? "AZIMUTH: placement texture patch could not change memory "
                              "protection; host code left unchanged.\r\n"
                            : "AZIMUTH: placement texture patch skipped; not a 0.9.9, "
                              "0.9.10, 1.0.3 or 1.0.4 build with the inspected bytes.\r\n");
}
