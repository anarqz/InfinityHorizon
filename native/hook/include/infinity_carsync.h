/* SPDX-License-Identifier: MIT

   Copyright (c) 2026 Alexandre Moraes and the Infinity Launcher contributors

   Permission is hereby granted, free of charge, to any person obtaining a copy of this file (the
   "Software"), to deal in the Software without restriction, including without limitation the
   rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the
   Software, and to permit persons to whom the Software is furnished to do so, subject to the
   following conditions: the above copyright notice and this permission notice shall be included
   in all copies or substantial portions of the Software.

   THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
   BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
   NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
   DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
   OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

   This one header is MIT (owner decision, 2026-11-17) so mods of any licence can include it; the
   rest of the hook stays GPL-3.0-or-later (native/hook/NOTICE.md). It is self-contained: it
   includes only system headers.

   ForzaInfinity hook: the car-state sync API for mods (CR-L53, OD-248). A mod copies or includes
   this one file.

   A mod (an ExtraOptions plugin such as Underglow) shows its own state on the cars of the other
   players in an online free-roam world who have the same mod. It gives the hook up to 64 opaque
   bytes for the local car ("blob"); the hook sends them through the world's relay (attribute 0xC0FD,
   docs/contracts/MATCHMAKING_API.md §5f) and keeps what the other players' mods sent, keyed by the
   car the hook matched to each player. The hook never reads the bytes.

   Rules (§5f "Who gets what"):
   - a mod sends and receives only after InfinityCarSync_Register in this process: the hook keeps a
     received blob only for a registered sync id, and drops the others unread;
   - the sync id is the protocol's registry id of the mod (1 = Underglow; ids are never reused, an
     incompatible blob layout takes a new id; 2 is FH2 ANNA's parts and signals and is not a blob);
   - the blob version byte is the mod's own, for compatible additions only;
   - the relay passes a blob only between two players whose seats both declared the mod, and
     forgets it 5 s after its sender stopped sending it; the hook does the same (a blob older than
     5 s is gone). Every received byte must still be validated by the mod: nothing here is
     authority.
   - the hook sends on a change (at most 10 times a second) and every second; at most two blobs go
     in one datagram (rotated when more mods are registered);
   - a mod must always have a blob out, its "off" state included: publish an explicit off blob (the
     mod's own bytes, e.g. a flags byte of 0) right after Register, never nothing. The world sends
     0xC0FD to a hook only while that hook sends some, so a mod with nothing published receives
     nothing from the others either.

   The hook may be absent (offline, another loader, an older hook): resolve the API at run time and
   work without it. InfinityCarSync_Resolve below finds the module that exports it (the hook is
   azimuth-tls.dll, or inside ExtraOptions.Infinity's winmm.dll) with GetProcAddress only.

   Threads: every function may be called from any thread; Get is cheap (a shared lock, no
   allocation) and meant for each frame. Pointers are not kept past the call.

   ABI: INFINITY_CARSYNC_VERSION 1. Every export keeps its signature for the life of version 1;
   additions get new exports; InfinityCarSyncInfo only grows at its end (its `size` says how much
   the caller has room for). A hook with another major version refuses Register. */
#ifndef INFINITY_CARSYNC_H
#define INFINITY_CARSYNC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define INFINITY_CARSYNC_VERSION 1u
/* the largest blob (bytes) */
#define INFINITY_CARSYNC_BLOB_MAX 64u
/* the most mods one process may register */
#define INFINITY_CARSYNC_MODS_MAX 4u

/* results (int32_t): > 0 success (Get: the blob's length), 0 nothing (Get: no blob for that car) */
#define INFINITY_CARSYNC_OK 1
#define INFINITY_CARSYNC_NONE 0
#define INFINITY_CARSYNC_E_VERSION (-1)      /* abi_version is not this hook's */
#define INFINITY_CARSYNC_E_ID (-2)           /* sync id 0, or one that is not a blob (2) */
#define INFINITY_CARSYNC_E_FULL (-3)         /* INFINITY_CARSYNC_MODS_MAX mods already registered */
#define INFINITY_CARSYNC_E_UNREGISTERED (-4) /* Publish/Get before Register of that id */
#define INFINITY_CARSYNC_E_LENGTH (-5)       /* Publish: length 0 or > INFINITY_CARSYNC_BLOB_MAX, or bytes NULL */
#define INFINITY_CARSYNC_E_CAPACITY (-6)     /* Get: capacity smaller than the blob (nothing copied) */
#define INFINITY_CARSYNC_E_ARGUMENT (-7)     /* a NULL pointer, or Info's size below 8 */

/* the sync ids of the registry (§5f) */
#define INFINITY_CARSYNC_ID_UNDERGLOW 1u
#define INFINITY_CARSYNC_ID_ANNA 2u /* 0xC0FC, sent by the hook itself: not a blob id */

typedef struct InfinityCarSyncInfo {
    uint32_t size;       /* in: sizeof(InfinityCarSyncInfo) of the caller; out: the bytes filled */
    uint32_t version;    /* INFINITY_CARSYNC_VERSION of the hook */
    uint32_t blob_max;   /* INFINITY_CARSYNC_BLOB_MAX */
    uint32_t mods_max;   /* INFINITY_CARSYNC_MODS_MAX */
    uint32_t flags;      /* bit 0: the sync runs now (online free roam, ExtraOptions active) */
} InfinityCarSyncInfo;

/* The exports (C, the platform's default x64 calling convention). */
typedef uint32_t (*InfinityCarSync_VersionFn)(void);
typedef int32_t (*InfinityCarSync_InfoFn)(InfinityCarSyncInfo *info);
/* Register a mod's sync id; again with the same id only updates its blob version. */
typedef int32_t (*InfinityCarSync_RegisterFn)(uint32_t abi_version, uint8_t sync_id, uint8_t blob_version);
/* The local car's blob, `length` 1..64 bytes (copied; 0 is refused). An unchanged blob costs
   nothing: call it whenever the mod's state may have changed. Once published, the blob goes with
   every refresh for as long as the process runs. */
typedef int32_t (*InfinityCarSync_PublishFn)(uint8_t sync_id, const void *bytes, uint32_t length);
/* Another player's blob for `car` (a ForzaCar pointer of the game, as the mod's car hooks see it;
   only the remote players' cars, ForzaCar kind 2 or 5, that the hook matched to their owner):
   copies it into `out` and returns its length, with its version in *blob_version (may be NULL);
   0 when that car has none. */
typedef int32_t (*InfinityCarSync_GetFn)(const void *car, uint8_t sync_id, void *out, uint32_t capacity,
                                         uint8_t *blob_version);

#ifdef INFINITY_CARSYNC_EXPORTS
/* the hook's side (native/hook/src/infinity/carstate.c) */
__declspec(dllexport) uint32_t InfinityCarSync_Version(void);
__declspec(dllexport) int32_t InfinityCarSync_Info(InfinityCarSyncInfo *info);
__declspec(dllexport) int32_t InfinityCarSync_Register(uint32_t abi_version, uint8_t sync_id, uint8_t blob_version);
__declspec(dllexport) int32_t InfinityCarSync_Publish(uint8_t sync_id, const void *bytes, uint32_t length);
__declspec(dllexport) int32_t InfinityCarSync_Get(const void *car, uint8_t sync_id, void *out, uint32_t capacity,
                                                  uint8_t *blob_version);
#endif

#ifdef INFINITY_CARSYNC_RESOLVER
/* A mod's side: define INFINITY_CARSYNC_RESOLVER in one source file (after <windows.h>) and call
   InfinityCarSync_Resolve once (e.g. when the plugin starts, and again later if it returned 0: the
   hook is in the process from the game's start). 1 when every function was found and the hook's
   version is INFINITY_CARSYNC_VERSION; 0 otherwise, with `api` zeroed: the mod then works alone. */
#include <windows.h>
#include <psapi.h> /* K32EnumProcessModules (kernel32 since Windows 7) */
typedef struct InfinityCarSyncApi {
    InfinityCarSync_VersionFn Version;
    InfinityCarSync_InfoFn Info;
    InfinityCarSync_RegisterFn Register;
    InfinityCarSync_PublishFn Publish;
    InfinityCarSync_GetFn Get;
} InfinityCarSyncApi;

static int InfinityCarSync_ResolveFrom(HMODULE module, InfinityCarSyncApi *api) {
    InfinityCarSyncApi found;
    found.Version = (InfinityCarSync_VersionFn)(void *)GetProcAddress(module, "InfinityCarSync_Version");
    found.Info = (InfinityCarSync_InfoFn)(void *)GetProcAddress(module, "InfinityCarSync_Info");
    found.Register = (InfinityCarSync_RegisterFn)(void *)GetProcAddress(module, "InfinityCarSync_Register");
    found.Publish = (InfinityCarSync_PublishFn)(void *)GetProcAddress(module, "InfinityCarSync_Publish");
    found.Get = (InfinityCarSync_GetFn)(void *)GetProcAddress(module, "InfinityCarSync_Get");
    if (!found.Version || !found.Info || !found.Register || !found.Publish || !found.Get ||
        found.Version() != INFINITY_CARSYNC_VERSION) {
        return 0;
    }
    *api = found;
    return 1;
}

static int InfinityCarSync_Resolve(InfinityCarSyncApi *api) {
    HMODULE modules[512];
    DWORD needed = 0;
    api->Version = 0, api->Info = 0, api->Register = 0, api->Publish = 0, api->Get = 0;
    /* the standalone hook first, then every module of the process (ExtraOptions.Infinity's
       winmm.dll shares its base name with System32's, so it is not looked up by name) */
    HMODULE hook = GetModuleHandleW(L"azimuth-tls.dll");
    if (hook && InfinityCarSync_ResolveFrom(hook, api)) {
        return 1;
    }
    if (!K32EnumProcessModules(GetCurrentProcess(), modules, sizeof(modules), &needed)) {
        return 0;
    }
    DWORD count = needed / sizeof(HMODULE);
    for (DWORD i = 0; i < count && i < sizeof(modules) / sizeof(modules[0]); ++i) {
        if (InfinityCarSync_ResolveFrom(modules[i], api)) {
            return 1;
        }
    }
    return 0;
}
#endif

#ifdef __cplusplus
}
#endif
#endif
