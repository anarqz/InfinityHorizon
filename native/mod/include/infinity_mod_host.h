// Copyright (c) 2026 Alexandre Moraes. MIT; see ../LICENSE-SDK.txt.
#pragma once
#include "infinity_mod.h"
#ifdef __cplusplus
extern "C" {
#endif
typedef struct InfinityModSelectionV1 {
    char id[65], version[65];
    const wchar_t* binary_path;
    const wchar_t* settings_path;
    uint8_t binary_sha256[32], settings_sha256[32], compatibility_sha256[32];
    uint32_t dependency_mask;
} InfinityModSelectionV1;
typedef struct InfinityModsLaunchV1 {
    uint32_t size, abi_version;
    void* game_image;
    const wchar_t* azimuth_directory;
    const wchar_t* game_directory;
    uint32_t count;
    const InfinityModSelectionV1* mods;
} InfinityModsLaunchV1;
typedef struct InfinityModReportV1 {
    char id[65], version[65];
    uint8_t binary_sha256[32], settings_sha256[32], compatibility_sha256[32];
    uint32_t state;
} InfinityModReportV1;
typedef struct InfinityModsSnapshotV1 {
    uint32_t size, abi_version, count, ready, failed;
    InfinityModReportV1 mods[INFINITY_MOD_MAX_COUNT];
} InfinityModsSnapshotV1;
typedef int (*InfinityModsStartV1Fn)(const InfinityModsLaunchV1*);
typedef int (*InfinityModsQueryV1Fn)(InfinityModsSnapshotV1*);
typedef int (*InfinityModsStartPlanV1Fn)(void* game_image, const wchar_t* path, const uint8_t sha256[32]);
#ifdef __cplusplus
}
#endif
