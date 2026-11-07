/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The games the hook knows, by PE identity (OD-206): one row of fi_game_profiles per build, and code
   asks for a feature, never for a game. Adding a game is a new row (native/hook/docs/ADDING_A_GAME.md).
   ExtraOptions.Infinity's game-map callback (azimuth_tls.c) selects the row once, before anything else
   runs. Nothing selected means FH2, so FH2's behaviour and the hook's FH2-only tests are unchanged.
   Header only: every module, and every test that includes a module's source, gets it as it is. */
#ifndef FI_GAME_PROFILE_H
#define FI_GAME_PROFILE_H

typedef enum { FI_GAME_FH2 = 1, FI_GAME_FM5 = 2 } FiGameId;

/* What a game has. A feature is only set for a game whose addresses and data formats it was built on. */
enum {
    /* the runtime.c list at the game's addresses: expansions, events, car state, overlay's game UI, mods,
       and what follows ExtraOptions' features */
    FI_FEATURE_GAME_MODULES = 1u << 0,
    FI_FEATURE_CLOUD_LINE = 1u << 1, /* the save line gate, the profile sync hold, the Beta protocol's cloud */
    FI_FEATURE_LIVERIES = 1u << 2,   /* livery routes and replies, Azimuth's livery compatibility */
    FI_FEATURE_FREE_ROAM = 1u << 3,  /* free roam's game patches, car meets, follow, Watch and parts captures */
    FI_FEATURES_ALL = (1u << 4) - 1u
};

/* In memory only, from the game's first IXMLHTTPRequest2, and only over its exact original bytes. */
typedef struct {
    unsigned rva;
    unsigned char size;
    unsigned char original[15], replacement[15];
    const char *what;
} FiGamePatch;

/* How a watch row logs. */
enum {
    FI_WATCH_VALUE = 0,        /* the value (bytes 1 to 4), at each change */
    FI_WATCH_NONNULL = 1,      /* a pointer (bytes 8): 1 set, 0 null, at each change */
    FI_WATCH_HEARTBEAT = 2,    /* a counter (bytes 4): 1 while it changed within the last second, 0 when it stopped */
    FI_WATCH_POINTER_ONCE = 3  /* the pointer at `global` itself, in hex, once it is set (offset and bytes unused) */
};

/* Read only, for reverse engineering: a pointer at `global`, then `bytes` at `offset` in what it points to. */
typedef struct {
    const char *label;
    unsigned global, offset, bytes, kind;
} FiGameWatch;

/* A game whose main update the hook does not detour has its readiness (the launcher's Event 5) polled from
   these terms: each reads a pointer at `global`, then `bytes` (1 to 8) at `offset`, and compares it with `value`. */
enum { FI_READY_EQ = 0, FI_READY_NE = 1, FI_READY_GE = 2 };
typedef struct {
    unsigned global, offset, bytes, op;
    unsigned long long value;
} FiReadyTerm;

typedef struct {
    FiGameId id;
    char name[4];
    unsigned timestamp, image_size;   /* PE TimeDateStamp and SizeOfImage, as Azimuth maps it */
    unsigned features;
    const wchar_t *title_section;     /* its section in data\azimuth.ini */
    const wchar_t *car_db;            /* the car database, relative to Mount */
    int private_hopper;               /* constants.custom.hopperId of its private session write */
    const FiGamePatch *patches;
    unsigned patch_count;
    const FiGameWatch *watches;
    unsigned watch_count;
    /* Polled readiness (none: the game's own detour, FH2's game_ready.c, or no readiness at all). Ready while
       every term holds and the 4 bytes at heartbeat_offset of *heartbeat_global changed within the last second.
       The generation changes with ready and with the two identity pointers. */
    const FiReadyTerm *ready_terms;
    unsigned ready_term_count;
    unsigned heartbeat_global, heartbeat_offset;
    unsigned ready_identity[2];
} FiGameProfile;

/* FM5 1.0.0.20 (docs/launcher/FM5_INFINITY_HANDOFF.md 3).
   Azimuth's store answers every privilege check with Restricted, and FM5's MULTIPLAYER path stops silently on it:
   the completion now does what NoIssue does (mov rax,[rcx]; mov dword [rax+3Ch],3; ret).
   DIAGNOSTIC until the real Party fix (lobby spec 11): the hopper screen ignores Party state 6, by no jump to the
   failure branch and always the "party ready" branch. */
static const FiGamePatch fi_fm5_patches[] = {
    {0x7d58d0u, 11, {0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x4c, 0x8b, 0xca, 0x48, 0x8b},
     {0x48, 0x8b, 0x01, 0xc7, 0x40, 0x3c, 0x03, 0x00, 0x00, 0x00, 0xc3}, "privilege answer forced to NoIssue"},
    {0x32ef84u, 2, {0x74, 0x9f}, {0x90, 0x90}, "hopper screen ignores Party state 6 (diagnostic)"},
    {0x32ef93u, 1, {0x75}, {0xeb}, "hopper screen takes the party ready branch (diagnostic)"},
};

/* The states the hopper screen decides on (response spec 9). */
static const FiGameWatch fi_fm5_watches[] = {
    {"FM5 WATCH party state +48", 0x261d198u, 0x48u, 4, FI_WATCH_VALUE},
    {"FM5 WATCH party requested +4c", 0x261d198u, 0x4cu, 4, FI_WATCH_VALUE},
    {"FM5 WATCH party flag +290", 0x261d198u, 0x290u, 1, FI_WATCH_VALUE},
    {"FM5 WATCH cloud config +11510", 0x261d1a0u, 0x11510u, 4, FI_WATCH_VALUE},
    {"FM5 WATCH sessions +38", 0x261d1a0u, 0x38u, 4, FI_WATCH_VALUE},
    {"FM5 WATCH ticket +114f8", 0x261d1a0u, 0x114f8u, 4, FI_WATCH_VALUE},
    {"FM5 WATCH match requester +b0", 0x261d188u, 0xb0u, 4, FI_WATCH_VALUE},
    /* R8 (lobby spec 12): the game-ready predicate's terms, to verify it on the first boot. F+0x5c, the frame
       timestamp, changes every frame and is not logged here. */
    {"FM5 WATCH ready S+10", 0x261d1a0u, 0x10u, 4, FI_WATCH_VALUE},
    {"FM5 WATCH ready L+40 (1 race)", 0x25e6e40u, 0x40u, 4, FI_WATCH_VALUE},
    {"FM5 WATCH ready L+38", 0x25e6e40u, 0x38u, 4, FI_WATCH_VALUE},
    {"FM5 WATCH ready F+48 (set in a race)", 0x25d0398u, 0x48u, 8, FI_WATCH_NONNULL},
    /* the predicate's main risk: the frame timestamp stopping on some screen */
    {"FM5 WATCH ready heartbeat F+5c", 0x25d0398u, 0x5cu, 4, FI_WATCH_HEARTBEAT},
    {"FM5 WATCH ready F", 0x25d0398u, 0, 0, FI_WATCH_POINTER_ONCE},
};

/* SAURON's R8 (lobby spec 12, static, verified by T1's first values): S=*0x14261d1a0, L=*0x1425e6e40,
   F=*0x1425d0398 (the heartbeat writer's own object, set for the whole run). */
static const FiReadyTerm fi_fm5_ready[] = {
    {0x261d1a0u, 0x10u, 8, FI_READY_NE, 0},     /* S+0x10 set */
    {0x261d1a0u, 0x38u, 4, FI_READY_GE, 2},     /* S+0x38 >= 2 */
    {0x261d1a0u, 0x38u, 4, FI_READY_NE, 3},     /* ... and not 3 */
    {0x261d1a0u, 0x114f8u, 4, FI_READY_EQ, 1},  /* S+0x114f8 == 1 */
    {0x25e6e40u, 0x40u, 4, FI_READY_NE, 1},     /* L+0x40 != 1: no race loading or running */
    {0x25d0398u, 0x48u, 8, FI_READY_EQ, 0},     /* F+0x48 null in menus */
};

static const FiGameProfile fi_game_profiles[] = {
    {FI_GAME_FH2, "FH2", GAME_TIMESTAMP, GAME_IMAGE_SIZE, FI_FEATURES_ALL, L"title-1a96545b", L"media\\stripped\\gamedbRC.slt", 2,
     0, 0, 0, 0, 0, 0, 0, 0, {0, 0}},
    {FI_GAME_FM5, "FM5", 0x53d8d1f2u, 0x279d000u, 0, L"title-7b35d917", L"media\\db\\gamedb.slt", -1,
     fi_fm5_patches, sizeof(fi_fm5_patches) / sizeof(fi_fm5_patches[0]),
     fi_fm5_watches, sizeof(fi_fm5_watches) / sizeof(fi_fm5_watches[0]),
     fi_fm5_ready, sizeof(fi_fm5_ready) / sizeof(fi_fm5_ready[0]), 0x25d0398u, 0x5cu, {0x25d0398u, 0x261d1a0u}},
};

__declspec(selectany) const FiGameProfile *volatile fi_game_selected;
__declspec(selectany) unsigned char *volatile fi_game_selected_image;

static inline const FiGameProfile *fi_game_profile_lookup(unsigned timestamp, unsigned image_size) {
    for (unsigned i = 0; i < sizeof(fi_game_profiles) / sizeof(fi_game_profiles[0]); ++i) {
        if (fi_game_profiles[i].timestamp == timestamp && fi_game_profiles[i].image_size == image_size) {
            return &fi_game_profiles[i];
        }
    }
    return 0;
}

static inline const FiGameProfile *fi_game_profile(void) {
    const FiGameProfile *selected = fi_game_selected;
    return selected ? selected : &fi_game_profiles[0];
}

static inline int fi_game_has(unsigned feature) {
    return (fi_game_profile()->features & feature) == feature;
}

/* A JSON integer at `p`: optional '-', at most 6 digits, after optional white space. */
static inline int fi_hopper_parse(const unsigned char *p, const unsigned char *end, int *value) {
    while (p < end && (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n')) ++p;
    int negative = p < end && *p == '-', number = 0;
    unsigned digits = 0;
    if (negative) ++p;
    for (; p < end && *p >= '0' && *p <= '9' && digits < 6; ++p, ++digits) number = number * 10 + (*p - '0');
    if (!digits || (p < end && *p >= '0' && *p <= '9')) return 0;
    *value = negative ? -number : number;
    return 1;
}
#endif
