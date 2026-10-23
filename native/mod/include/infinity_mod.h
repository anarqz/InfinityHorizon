// Copyright (c) 2026 Alexandre Moraes. MIT; see the accompanying SDK license.
#pragma once
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
#define INFINITY_MOD_ABI_V1 1u
#define INFINITY_MOD_MAX_COUNT 32u
typedef struct InfinityMod InfinityMod;
typedef struct InfinityHook InfinityHook;
typedef struct InfinityXmm { union { float f[4]; double d[2]; uint32_t u32[4]; uint64_t u64[2]; }; } InfinityXmm;
typedef struct InfinityContext {
    uint64_t rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi;
    uint64_t r8, r9, r10, r11, r12, r13, r14, r15, rflags, reserved;
    InfinityXmm xmm[16];
} InfinityContext;
typedef void (*InfinityInstructionFn)(InfinityContext*, void*);
enum InfinityModState { INFINITY_MOD_SELECTED = 0, INFINITY_MOD_INITIALIZING = 1,
    INFINITY_MOD_READY = 2, INFINITY_MOD_FAILED = 3 };
typedef struct InfinityModApiV1 {
    uint32_t size, abi_version;
    InfinityMod* mod;
    const wchar_t* azimuth_dir;
    const wchar_t* settings_path;
    void (*report_state)(InfinityMod*, uint32_t state);
    void (*log)(InfinityMod*, const char*);
    InfinityHook* (*hook_function)(InfinityMod*, const char*, void*, void*, void**);
    InfinityHook* (*hook_call)(InfinityMod*, const char*, void*, void*, void**);
    InfinityHook* (*hook_pointer)(InfinityMod*, const char*, void**, void*, void**);
    int (*enable_hooks)(InfinityMod*, InfinityHook*);
    int (*disable_hooks)(InfinityMod*, InfinityHook*);
    void* (*wait_for_code)(uint32_t);
    InfinityHook* (*patch_bytes)(InfinityMod*, const char*, void*, const void*, const void*, uint32_t);
    InfinityHook* (*hook_instruction)(InfinityMod*, const char*, void*, InfinityInstructionFn, void*);
} InfinityModApiV1;
typedef int (*InfinityModInitializeV1Fn)(const InfinityModApiV1*);
typedef struct InfinityModDatabaseApiV1 {
    uint32_t size, abi_version;
    InfinityMod* mod;
    const wchar_t* settings_path;
    int (*queue_sql)(InfinityMod*, const char* sql);
} InfinityModDatabaseApiV1;
typedef int (*InfinityModRegisterDatabaseV1Fn)(const InfinityModDatabaseApiV1*);
#ifdef __cplusplus
}
#endif
