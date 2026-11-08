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
    FI_WATCH_POINTER_ONCE = 3, /* the pointer at `global` itself, in hex, once it is set (offset and bytes unused) */
    FI_WATCH_CHAIN = 4         /* a pointer chain (W16): the value (bytes 1 to 4), at each change; see FiGameWatch */
};

/* Read only, for reverse engineering: a pointer at `global`, then `bytes` at `offset` in what it points to.
   FI_WATCH_CHAIN: `chain_count` (1 to FI_WATCH_CHAIN_MAX) offsets, `offset` unused. Starting from the pointer at `global`,
   each offset but the last is added and the pointer there read (null stops the walk, nothing is logged); the last offset
   addresses the `bytes` that are logged at each change. Other kinds leave chain_count 0. */
enum { FI_WATCH_CHAIN_MAX = 6 };
/* OD-223 (T-1108-M): a row whose value also goes to the launcher as the game's screen, Event 9 (infinity.h FI_EVENT_SCREEN, detail the
   decimal id 0..255), at each change; a value past 255 or a walk that fails reads FI_WATCH_SCREEN_UNKNOWN (the launcher then knows no
   screen it accepts invites on). For a game whose screen activity.c does not report (FM5). 0: logged only. */
#define FI_WATCH_EVENT_SCREEN 9u
#define FI_WATCH_SCREEN_UNKNOWN 255u
/* OD-224 (T-1108-L): a row whose value also goes to the launcher as the game's lobby kind, Event 10 (infinity.h FI_EVENT_LOBBY_KIND,
   detail the decimal kind 0..255), at each change: the value of the walk, 0 when the row's global is null (no lobby object), 255 past
   254; a read that faults reports nothing (a transient fault must never read as "left the lobby"). The launcher ends a private
   lobby's seat when the kind leaves its GameDefinition.PrivateLobbyKind (FM5: 5). 0: logged only. */
#define FI_WATCH_EVENT_LOBBY_KIND 10u

/* OD-224: the kind an FI_WATCH_EVENT_LOBBY_KIND row reports: `global_ok` the read of its global succeeded, `object_set` it is not
   null, `read_ok` the walk succeeded with `value`. 1 and `*kind` when there is something to report, 0 when nothing is. Pure, for
   tests. */
static inline int fi_watch_lobby_kind_of(int global_ok, int object_set, int read_ok, unsigned value, unsigned *kind) {
    if (!global_ok) return 0;
    if (!object_set) {
        *kind = 0;
        return 1;
    }
    if (!read_ok) return 0;
    *kind = value < FI_WATCH_SCREEN_UNKNOWN ? value : FI_WATCH_SCREEN_UNKNOWN;
    return 1;
}
typedef struct {
    const char *label;
    unsigned global, offset, bytes, kind;
    unsigned chain_count, chain[FI_WATCH_CHAIN_MAX];
    unsigned event;
} FiGameWatch;

/* OD-223: the screen an FI_WATCH_EVENT_SCREEN row reports from a read: its value when the read succeeded and fits 0..254, else
   FI_WATCH_SCREEN_UNKNOWN. Pure, for tests. */
static inline unsigned fi_watch_screen_of(int read_ok, unsigned value) {
    return read_ok && value < FI_WATCH_SCREEN_UNKNOWN ? value : FI_WATCH_SCREEN_UNKNOWN;
}

/* The chain walk of a FI_WATCH_CHAIN row, with the reader passed in so a test can run it on a fake heap
   (game_patches.h passes memory_read). 1 and the value (little endian, `bytes` wide) when every pointer was readable and set. */
typedef int (*FiWatchReader)(const void *address, void *output, size_t size);
static inline int fi_watch_chain_read(FiWatchReader read, const unsigned char *root, const FiGameWatch *watch,
                                      unsigned long long *value) {
    const unsigned char *at = root;
    *value = 0;
    if (watch->kind != FI_WATCH_CHAIN || !at || !watch->chain_count || watch->chain_count > FI_WATCH_CHAIN_MAX ||
        watch->bytes < 1 || watch->bytes > sizeof(*value)) {
        return 0;
    }
    for (unsigned i = 0; i + 1 < watch->chain_count; ++i) {
        if (!read(at + watch->chain[i], &at, sizeof(at)) || !at) {
            return 0;
        }
    }
    return read(at + watch->chain[watch->chain_count - 1], value, watch->bytes);
}

/* The value a VALUE, NONNULL or CHAIN row logs, from `object` (the pointer read at `global`): VALUE the `bytes` (1 to 4) at
   `offset`; NONNULL 1 when the 8 bytes at `offset` are set; CHAIN the walk's last value (8-byte rows: 1 set, 0 null). 0 when a
   read failed or a chain link was null (nothing logged). A CHAIN row never reads `object + offset`: live FM5 (SAURON, W16) logged
   the low half of *(CLobby+0) and *(U+0), the objects' vftables (310387240 = 0x1280c9e8, 310543072), when the watch thread fell
   through from the walk to that read. HEARTBEAT and POINTER_ONCE stay the thread's own (they keep state). */
static inline int fi_watch_value(FiWatchReader read, const unsigned char *object, const FiGameWatch *watch, unsigned *value) {
    unsigned long long wide = 0;
    *value = 0;
    if (watch->kind == FI_WATCH_CHAIN) {
        if (!fi_watch_chain_read(read, object, watch, &wide)) return 0;
        *value = watch->bytes > sizeof(*value) ? wide != 0 : (unsigned)wide;
        return 1;
    }
    if (watch->kind == FI_WATCH_NONNULL) {
        if (watch->bytes > sizeof(wide) || !read(object + watch->offset, &wide, watch->bytes)) return 0;
        *value = wide != 0;
        return 1;
    }
    if (watch->kind != FI_WATCH_VALUE || watch->bytes < 1 || watch->bytes > sizeof(*value)) return 0;
    return read(object + watch->offset, value, watch->bytes);
}

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
       every term holds and the 4 bytes at heartbeat_offset of *heartbeat_global changed within the last
       heartbeat_window_ms (fi_ready_heartbeat_fresh). The generation changes with ready and with the two identity pointers. */
    const FiReadyTerm *ready_terms;
    unsigned ready_term_count;
    unsigned heartbeat_global, heartbeat_offset;
    unsigned ready_identity[2];
    unsigned heartbeat_window_ms;
    /* W7b (OD-206, CR-S99 4, lobby spec 11.3-11.4, route 1): the game takes an invite by Party GameSessionReady. The hook keeps
       the delegate the game adds at IPartyStatics slot 16 (freeroam.c), reports Capture 14, and fires it on the launcher's
       SessionReady (launch.c). 0: slot 16 is only observed, as for FH2. */
    int party_session_ready;
    /* fm5-10, fm5-11 (W7b, OD-206, lobby spec 11.9): the image RVA of the game's own IAT slot for combase!WindowsCreateString
       (FM5: 0x101bca0, read through 0x140136a80; T3 VERIFIED it holds 0x14017f370 in azimuth.exe, Azimuth's FSH2 layout). The
       hook's own session reference makes its HSTRINGs with the function stored there, so the game frees them with its own
       WindowsDeleteString. 0: none (FH2). */
    unsigned hstring_create_rva;
    /* fm5-12 (W7b): the same for combase!WindowsDeleteString (FM5: 0x101bcf8, SAURON-verified; it holds Azimuth's delete, 0x14017f460
       in azimuth.exe 1.0.4, which throws on a non-FSH2 string, so only strings the game's create made are passed). 0: none (FH2);
       the hook then falls back to finding the delete next to the create. */
    unsigned hstring_delete_iat_rva;
    /* OD-223 (T-1108-M): the terms (as ready_terms) the game's screen row (FI_WATCH_EVENT_SCREEN) needs besides its walk: while one fails
       Event 9 reports FI_WATCH_SCREEN_UNKNOWN, so the launcher takes no invite there. FM5: its hopper cloud config loaded (SAURON). */
    const FiReadyTerm *screen_terms;
    unsigned screen_term_count;
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
    /* W7b (OD-206, lobby spec 11.4 Option A, 11.7 attempt 1): ValidatePartyGameSessionRequest's roster check (0x140be50f0)
       fails an invited session unless the Party roster holds another member of it, and Azimuth's roster never does. Its
       "no member" branch at 0x140be51a5 (mov dword [rdi+0Ch],19h) stores 0x18 instead, the value of the found branch at
       0x140be5196, which then calls +0x10 on the lookup result (a release, INFERRED; nothing to release here). Only byte
       0x140be51a8 changes; all seven are checked. VERIFIED bytes (coordinator, against the binary). */
    {0xbe51a5u, 7, {0xc7, 0x47, 0x0c, 0x19, 0x00, 0x00, 0x00}, {0xc7, 0x47, 0x0c, 0x18, 0x00, 0x00, 0x00},
     "invited session passes the Party roster check (W7b, lobby spec 11.4)"},
};

/* The states the hopper screen decides on (response spec 9). */
static const FiGameWatch fi_fm5_watches[] = {
    {"FM5 WATCH party state +48", 0x261d198u, 0x48u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH party requested +4c", 0x261d198u, 0x4cu, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH party flag +290", 0x261d198u, 0x290u, 1, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH cloud config +11510", 0x261d1a0u, 0x11510u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH sessions +38", 0x261d1a0u, 0x38u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH ticket +114f8", 0x261d1a0u, 0x114f8u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH match requester +b0", 0x261d188u, 0xb0u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    /* R8 (lobby spec 12): the game-ready predicate's terms, to verify it on the first boot. F+0x5c, the frame
       timestamp, changes every frame and is not logged here. */
    {"FM5 WATCH ready S+10", 0x261d1a0u, 0x10u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH ready L+40 (1 race)", 0x25e6e40u, 0x40u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH ready L+38", 0x25e6e40u, 0x38u, 4, FI_WATCH_VALUE, 0, {0}, 0},
    {"FM5 WATCH ready F+48 (set in a race)", 0x25d0398u, 0x48u, 8, FI_WATCH_NONNULL, 0, {0}, 0},
    /* the predicate's main risk: the frame timestamp stopping on some screen */
    {"FM5 WATCH ready heartbeat F+5c", 0x25d0398u, 0x5cu, 4, FI_WATCH_HEARTBEAT, 0, {0}, 0},
    {"FM5 WATCH ready F", 0x25d0398u, 0, 0, FI_WATCH_POINTER_ONCE, 0, {0}, 0},
    /* W16 (lobby spec 12 "Screen id, for diagnosis"): the UserInterface U=*(0x25f88a0); the top screen id is
       **(*(*(*(U+0x58)+0x20)+8)+0x10) (0x1408e6580; 0 when *(s+0x28)==0, not checked here). VERIFIED statically (SAURON), id 0x6e is a
       race start. Read it to tell when the lobby screen is on top. The GameOptions chain itself (screen+0x360 -> CLobby, CLobby+0x130 ->
       go, go+0x38 owned at -2, go+0x40 granted alongside) has no stable root yet: add its two rows below as
       {label, 0x25f88a0u, 0, 4, FI_WATCH_CHAIN, n, {...}} once the lobby screen has a known global or chain from U (docs/launcher/batch-1107/ITEM5.md). */
    /* W16 (SAURON): the CLobby singleton, TBasicSingleton<CLobby>: its ctor writes the global at 0x25e6e40 and its dtors zero it. Chain only,
       any null link stops the walk. GameOptions go = *(CLobby+0x130) resets when the lobby is left. Expected: before a grant go+38 is not
       -2; after engine msg 11 go+38 and go+40 read -2; after msg 12 go+38 is no longer -2. */
    {"FM5 WATCH go+38 (-2 = local options owner)", 0x25e6e40u, 0, 4, FI_WATCH_CHAIN, 2, {0x130u, 0x38u}, 0},
    {"FM5 WATCH go+40 (-2 too after the grant)", 0x25e6e40u, 0, 4, FI_WATCH_CHAIN, 2, {0x130u, 0x40u}, 0},
    {"FM5 WATCH lobby session +148 (non-zero)", 0x25e6e40u, 0, 8, FI_WATCH_CHAIN, 1, {0x148u}, 0},
    /* OD-224 (T-1108-L, SAURON): also Event 10: 5 a private match (lobby spec 13.4), 7 Waiting-to-Start, 0 none; it stays 5 through
       that match's race and results and drops to 0 when the player leaves it (16:06:56, screen 110 -> 67) */
    {"FM5 WATCH lobby kind +38 (5/6/7)", 0x25e6e40u, 0, 4, FI_WATCH_CHAIN, 1, {0x38u}, FI_WATCH_EVENT_LOBBY_KIND},
    {"FM5 WATCH lobby race +40 (1 from race load to end)", 0x25e6e40u, 0, 4, FI_WATCH_CHAIN, 1, {0x40u}, 0},
    /* OD-223 (T-1108-M): also Event 9, so the launcher takes FM5 invites on its MULTIPLAYER hoppers screen only (GameDefinition
       InviteAcceptScreens; SAURON's live map: 101 main menu, 67 hoppers, 34 joining, 110 lobby, 98/68 race load/race, 155/154/211
       results), and only once fi_fm5_screen_terms hold */
    {"FM5 WATCH top screen id", 0x25f88a0u, 0, 4, FI_WATCH_CHAIN, 5, {0x58u, 0x20u, 0x8u, 0x10u, 0}, FI_WATCH_EVENT_SCREEN},
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

/* OD-223 (T-1108-M, SAURON): an invite may be taken only once the hoppers screen's cloud config is loaded: the u32 at
   *(0x14261d1a0)+0x11510 (watch row "cloud config": 1 pending, 3 ok, 4 failed; 1 for about 0.6 s on entry, 30 s on the stuck path).
   Until then the screen row reports FI_WATCH_SCREEN_UNKNOWN. The failing accept (games-10, 15:55:26) came on screen 101 with it 0;
   the good one (16:01:40) on 67 with it 3. */
#define FI_FM5_CLOUD_CONFIG_OK 3u
static const FiReadyTerm fi_fm5_screen_terms[] = {
    {0x261d1a0u, 0x11510u, 4, FI_READY_EQ, FI_FM5_CLOUD_CONFIG_OK},
};

/* FM5's heartbeat window: the game clamps a frame's delta at 4000 ms (0x14013f841, cmp 0FA0h), so a gap of a few seconds is a
   normal hitch, and T2's stall of just over 1 s dropped GAME READY for one poll (SAURON). A race start is still seen at once:
   L+0x40 is 1 for the whole load and race. */
#define FI_FM5_HEARTBEAT_WINDOW_MS 3000u

static const FiGameProfile fi_game_profiles[] = {
    {FI_GAME_FH2, "FH2", GAME_TIMESTAMP, GAME_IMAGE_SIZE, FI_FEATURES_ALL, L"title-1a96545b", L"media\\stripped\\gamedbRC.slt", 2,
     0, 0, 0, 0, 0, 0, 0, 0, {0, 0}, 0, 0, 0, 0, 0, 0},
    {FI_GAME_FM5, "FM5", 0x53d8d1f2u, 0x279d000u, 0, L"title-7b35d917", L"media\\db\\gamedb.slt", -1,
     fi_fm5_patches, sizeof(fi_fm5_patches) / sizeof(fi_fm5_patches[0]),
     fi_fm5_watches, sizeof(fi_fm5_watches) / sizeof(fi_fm5_watches[0]),
     fi_fm5_ready, sizeof(fi_fm5_ready) / sizeof(fi_fm5_ready[0]), 0x25d0398u, 0x5cu, {0x25d0398u, 0x261d1a0u},
     FI_FM5_HEARTBEAT_WINDOW_MS, 1, 0x101bca0u, 0x101bcf8u, fi_fm5_screen_terms, sizeof(fi_fm5_screen_terms) / sizeof(fi_fm5_screen_terms[0])},
};

/* OD-223: what the game's screen row reports, the walk's value `value` (`read_ok` when it succeeded) gated by the profile's
   screen_terms read from `image` with `read`: FI_WATCH_SCREEN_UNKNOWN unless the walk succeeded, fits 0..254 and every term holds. Pure,
   for tests (game_patches.h passes memory_read). */
static inline unsigned fi_watch_screen_gated(FiWatchReader read, const unsigned char *image, const FiGameProfile *game, int read_ok,
                                             unsigned value) {
    for (unsigned i = 0; read_ok && i < game->screen_term_count; ++i) {
        const FiReadyTerm *term = &game->screen_terms[i];
        const unsigned char *object = 0;
        unsigned long long seen = 0;
        read_ok = term->bytes <= sizeof(seen) && read(image + term->global, &object, sizeof(object)) && object &&
                  read(object + term->offset, &seen, term->bytes) &&
                  (term->op == FI_READY_EQ ? seen == term->value : term->op == FI_READY_NE ? seen != term->value : seen >= term->value);
    }
    return fi_watch_screen_of(read_ok, value);
}

/* The polled heartbeat's term (game_patches.h game_ready_thread): it changed at `beat_at` (0: never seen change) and is
   fresh while no more than `window_ms` have passed since. Pure, for tests. */
static inline int fi_ready_heartbeat_fresh(unsigned long long beat_at, unsigned long long now, unsigned window_ms) {
    return beat_at && now >= beat_at && now - beat_at <= window_ms;
}

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
