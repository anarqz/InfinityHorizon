/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74:
   features/vehicle_controls.c and features/autodrive_game.c: the local car, a car's animation
   and its events), GPL-3.0-or-later. See LICENSE and NOTICE.md.

   Headlights and roof of the players' cars in online free roam, next to the player's own
   ExtraOptions 0.2.98 (which is what changes them on a car: right stick click, hold 3 s) and
   without touching any byte of it. carstate.c carries the states through the world's relay;
   this file finds them on the cars and shows the others' on theirs.

   Two 14-byte detours (the prologues are position independent and checked byte for byte):
   - game+0x2a9400, the car update (the vtable slot game+0x2c5cc00 holds it; ExtraOptions
     replaces that slot with its own wrapper, which calls this function directly, so the
     detour sees every car with or without ExtraOptions, and the slot is never touched);
   - game+0xf5d300, a car's animation event ("roof:open", "roof:close", "roof:toggle"...; the
     game calls it, and ExtraOptions calls it to move a roof): the last roof event of each car
     is its roof state.
   Only with ExtraOptions 0.2.98 active (fi_eo_sync_enabled):
   - the local car's state is read from the car itself: +0x51a1 is "headlights forced", +0x51a2
     their value (the game's own setters at game+0xf74ee0 / +0xf74ef0 write exactly those two
     bytes; ExtraOptions toggles them through the setters), so lights = 0 (the game's), 1
     (forced on), 2 (forced off); the roof from its last event. It goes to the relay when it
     changes and every second (carstate.c);
   - another player's car, matched by position (carstate.c), gets its owner's headlights through
     the same setters around the game's update and its roof through the same events.
   Without ExtraOptions none of that runs: a detour is one flag check, nothing is sent, nothing
   received is shown. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define CS_CAR_UPDATE_RVA 0x2a9400u
#define CS_ANIMATION_EVENT_RVA 0xf5d300u
#define CS_PLAYER_MANAGER_RVA 0x34aba60u
#define CS_GET_LOCAL_PLAYER_RVA 0x21e0c0u
#define CS_GET_PLAYER_CAR_RVA 0x2da060u
#define CS_PLAYER_CAR_VTABLE_RVA 0x2c5cb80u
#define CS_HEADLIGHT_MANUAL_RVA 0xf74ee0u
#define CS_HEADLIGHT_VALUE_RVA 0xf74ef0u
#define CS_LIGHTS_UPDATE_RVA 0xf5c900u
#define CS_ANIMATION_DETAIL_RVA 0x16fbe90u
#define CS_FIND_ANIMATION_EVENT_RVA 0x16fd4d0u
#define CS_TARGET_VTABLE_RVA 0x2c5ce28u
#define CS_POSITION_GETTER_RVA 0xf62290u
#define CS_CODE_END_RVA 0x1dcc000u
#define CS_ROOF_CARS 16
#define CS_REMOTE_CARS 16

typedef void (*CarUpdateFn)(void *, float, float, unsigned);
typedef void (*AnimationEventFn)(void *, const char *);
typedef void (*HeadlightSetter)(void *, unsigned char);
typedef void (*CarLightsUpdate)(void *, float);
typedef void (*AnimationDetail)(void *, int);
typedef void *(*PlayerGetter)(void *, int);
typedef void *(*CarGetter)(void *, void **);
typedef void (*ObjectAction)(void *);
typedef void *(*VectorGetter)(void *, float *);

typedef struct {
    union {
        char text[16];
        const char *pointer;
    } storage;
    size_t length, capacity;
} AnimationName;
typedef void *(*FindAnimationEvent)(void *, void **, const AnimationName *);

/* the original prologues (position independent: no rip-relative operand) */
static const unsigned char cs_car_update_bytes[15] = {0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x18, 0x57,
                                                     0x48, 0x81, 0xec, 0xa0, 0x00, 0x00, 0x00};
static const unsigned char cs_animation_event_bytes[16] = {0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b,
                                                          0x89, 0x40, 0x4d, 0x00, 0x00, 0x48, 0x8b, 0xda};

static unsigned char *cs_game;
static CarUpdateFn cs_car_update_original;
static AnimationEventFn cs_animation_event_original;
static volatile LONG cs_installed;

/* ------------------------------------------------------------- game objects */

static uintptr_t cs_method(void *object, unsigned slot) {
    void *vtable;
    uintptr_t address;
    if (!memory_read_pointer(object, &vtable) || !memory_read((unsigned char *)vtable + slot, &address, 8) ||
        address < (uintptr_t)cs_game + 0x1000 || address >= (uintptr_t)cs_game + CS_CODE_END_RVA) {
        return 0;
    }
    return address;
}

static void cs_release_car(void *car) {
    uintptr_t release = car ? cs_method(car, 0x18) : 0;
    if (release) {
        ((ObjectAction)release)(car);
    }
}

/* The local player's car (a reference: cs_release_car), as ExtraOptions finds it. */
static void *cs_local_car(void) {
    void *game, *car = 0, *vtable = 0;
    if (!memory_read_pointer(cs_game + CS_PLAYER_MANAGER_RVA, &game)) {
        return 0;
    }
    void *player = ((PlayerGetter)(uintptr_t)(cs_game + CS_GET_LOCAL_PLAYER_RVA))(game, 0);
    if (!player) {
        return 0;
    }
    ((CarGetter)(uintptr_t)(cs_game + CS_GET_PLAYER_CAR_RVA))(player, &car);
    if (car && (!memory_read_pointer(car, &vtable) || vtable != cs_game + CS_PLAYER_CAR_VTABLE_RVA)) {
        cs_release_car(car);
        return 0;
    }
    return car;
}

static void *cs_car_animation(void *car) {
    void *animator, *animation;
    if (!memory_read_pointer((unsigned char *)car + 0x4d40, &animator) ||
        !memory_read_pointer((unsigned char *)animator + 0x1c8, &animation)) {
        return 0;
    }
    return animation;
}

static int cs_has_animation_event(void *animation, const char *name) {
    uintptr_t map[7];
    if (!animation || !memory_read((unsigned char *)animation + 0x268, map, sizeof(map)) || !map[0] || !map[2] ||
        !map[6] || map[5] > 65535 || map[6] > map[5] + 1 || !map[1] || map[1] > 65536) {
        return 0;
    }
    AnimationName key;
    for (size_t i = 0; i < sizeof(key.storage.text); ++i) {
        key.storage.text[i] = 0;
    }
    key.length = text_length(name);
    key.capacity = 15;
    if (key.length < sizeof(key.storage.text)) {
        memory_copy(key.storage.text, name, key.length + 1);
    } else {
        key.storage.pointer = name;
        key.capacity = key.length;
    }
    void *entry = 0;
    ((FindAnimationEvent)(uintptr_t)(cs_game + CS_FIND_ANIMATION_EVENT_RVA))((unsigned char *)animation + 0x268, &entry,
                                                                            &key);
    return entry && entry != (void *)map[0];
}

/* The car's position: its target's slot 0 (game+0xf62290 -> the body's +0x270), checked first
   as ExtraOptions checks its velocity getter. */
static int cs_car_position(void *car, float out[3]) {
    unsigned char *target = (unsigned char *)car + 0x20;
    void *body, *vtable, *target_vtable;
    uintptr_t getter;
    if (!memory_read_pointer(target, &target_vtable) || target_vtable != cs_game + CS_TARGET_VTABLE_RVA ||
        !memory_read((unsigned char *)target_vtable, &getter, sizeof(getter)) ||
        getter != (uintptr_t)cs_game + CS_POSITION_GETTER_RVA || !memory_read_pointer(target + 0x4be8, &body) ||
        !memory_read_pointer(body, &vtable) || !memory_read((unsigned char *)vtable + 0x270, &getter, sizeof(getter)) ||
        getter < (uintptr_t)cs_game + 0x1000 || getter >= (uintptr_t)cs_game + CS_CODE_END_RVA) {
        return 0;
    }
    _Alignas(16) float position[4] = {0, 0, 0, 0};
    ((VectorGetter)(uintptr_t)(cs_game + CS_POSITION_GETTER_RVA))(target, position);
    for (unsigned i = 0; i < 3; ++i) {
        if (!float_is_finite(position[i])) {
            return 0;
        }
        out[i] = position[i];
    }
    return 1;
}

/* ------------------------------------------------------------- roof events */

static struct {
    void *car, *animation;
    int open;
    unsigned long long used_at;
} cs_roofs[CS_ROOF_CARS];
static SRWLOCK cs_roof_lock = SRWLOCK_INIT;

/* 0 not a roof event, 1 open, 2 close, 3 toggle. Every animation event of every car comes here
   while the sync is on: one read of the name's first bytes (byte by byte only when those are
   at the end of what is readable). */
static int cs_roof_event(const char *event) {
    char text[12];
    size_t n = 0;
    if (memory_read(event, text, sizeof(text) - 1)) {
        if (!memory_equal(text, "roof:", 5)) {
            return 0;
        }
        while (n < sizeof(text) - 1 && text[n]) {
            ++n;
        }
    } else {
        while (n < sizeof(text) - 1 && memory_read(event + n, &text[n], 1) && text[n]) {
            ++n;
        }
    }
    text[n] = 0;
    if (n < 9 || !memory_equal(text, "roof:", 5)) {
        return 0;
    }
    return n == 9 && memory_equal(text + 5, "open", 5)    ? 1
           : n == 10 && memory_equal(text + 5, "close", 6) ? 2
           : n == 11 && memory_equal(text + 5, "toggle", 7) ? 3
                                                          : 0;
}

static void cs_record_roof(void *car, int kind) {
    void *animation = cs_car_animation(car);
    unsigned long long now = GetTickCount64();
    AcquireSRWLockExclusive(&cs_roof_lock);
    int slot = -1, oldest = 0;
    for (int i = 0; i < CS_ROOF_CARS; ++i) {
        if (cs_roofs[i].car == car) {
            slot = i;
            break;
        }
        if (cs_roofs[i].used_at < cs_roofs[oldest].used_at) {
            oldest = i;
        }
    }
    if (slot < 0 || cs_roofs[slot].animation != animation) {
        slot = slot < 0 ? oldest : slot;
        cs_roofs[slot].car = car;
        cs_roofs[slot].animation = animation;
        cs_roofs[slot].open = 0; /* a car (re)loaded comes with its roof closed */
    }
    cs_roofs[slot].open = kind == 1 ? 1 : kind == 2 ? 0 : !cs_roofs[slot].open;
    cs_roofs[slot].used_at = now;
    ReleaseSRWLockExclusive(&cs_roof_lock);
}

/* The roof of `car` as its last event left it (closed when none was seen). */
static int cs_roof_open(void *car) {
    void *animation = cs_car_animation(car);
    int open = 0;
    AcquireSRWLockExclusive(&cs_roof_lock);
    for (int i = 0; i < CS_ROOF_CARS; ++i) {
        if (cs_roofs[i].car == car) {
            open = cs_roofs[i].animation == animation && cs_roofs[i].open;
            break;
        }
    }
    ReleaseSRWLockExclusive(&cs_roof_lock);
    return open;
}

static void cs_animation_event(void *car, const char *event) {
    if (fi_eo_sync_enabled() && car && event) {
        int kind = cs_roof_event(event);
        if (kind) {
            cs_record_roof(car, kind);
        }
    }
    cs_animation_event_original(car, event);
}

/* -------------------------------------------------------- the local car */

/* `car` is the one the game is updating right now: its two bytes are read directly. */
static void cs_report_local(void *car) {
    const volatile unsigned char *lights = (const volatile unsigned char *)car + 0x51a1;
    unsigned char forced = lights[0], value = lights[1];
    azimuth_carstate_local(forced ? (value ? 1 : 2) : 0, cs_roof_open(car));
}

/* The local player's car, looked up once per frame (the car update's `frame` argument), and at
   least every 100 ms in case that argument does not move: every car of the frame compares
   itself with it. Only the address is kept (the reference is released at once): it is
   compared, never followed. */
static void *cs_local_cached;
static unsigned cs_local_frame;
static unsigned long long cs_local_at;

static void *cs_local_car_for(unsigned frame) {
    unsigned long long now = GetTickCount64();
    if (!cs_local_at || frame != cs_local_frame || now - cs_local_at >= 100) {
        void *car = cs_local_car();
        cs_local_cached = car;
        cs_local_frame = frame;
        cs_local_at = now;
        cs_release_car(car);
    }
    return cs_local_cached;
}

/* ------------------------------------------------------ the others' cars */

static struct {
    void *car, *animation;
    int lights, roof;
} cs_remote_cars[CS_REMOTE_CARS];

/* What it last showed on `car` (a slot for it when `make`). */
static int cs_remote_slot(void *car, int make) {
    int free_slot = -1;
    for (int i = 0; i < CS_REMOTE_CARS; ++i) {
        if (cs_remote_cars[i].car == car) {
            return i;
        }
        if (!cs_remote_cars[i].car && free_slot < 0) {
            free_slot = i;
        }
    }
    if (!make) {
        return -1;
    }
    int i = free_slot >= 0 ? free_slot : (int)((uintptr_t)car >> 4) % CS_REMOTE_CARS;
    cs_remote_cars[i].car = car;
    cs_remote_cars[i].animation = 0;
    cs_remote_cars[i].lights = 0;
    cs_remote_cars[i].roof = 0;
    return i;
}

/* A player's car: its headlights as forced on its owner's screen (written around the game's
   update; when they change, the lights update game+0xf5c900 runs so they show at once, the
   car's own update keeping them after), and its roof moved by the same animation events. */
static void cs_show_remote(void *car, int lights, int open, int after, float dt) {
    int i = cs_remote_slot(car, 1);
    if (lights) {
        ((HeadlightSetter)(uintptr_t)(cs_game + CS_HEADLIGHT_VALUE_RVA))(car, lights == 1);
        ((HeadlightSetter)(uintptr_t)(cs_game + CS_HEADLIGHT_MANUAL_RVA))(car, 1);
    } else if (cs_remote_cars[i].lights) {
        ((HeadlightSetter)(uintptr_t)(cs_game + CS_HEADLIGHT_MANUAL_RVA))(car, 0); /* back to the game's own */
    }
    if (!after) {
        return;
    }
    if (lights != cs_remote_cars[i].lights) {
        void *presentation, *list;
        if (memory_read_pointer((unsigned char *)car + 0x4d38, &presentation) &&
            memory_read_pointer((unsigned char *)car + 0x51c0, &list)) {
            ((CarLightsUpdate)(uintptr_t)(cs_game + CS_LIGHTS_UPDATE_RVA))(car, dt);
        }
        cs_remote_cars[i].lights = lights;
    }
    void *animation = cs_car_animation(car);
    if (animation != cs_remote_cars[i].animation) {
        cs_remote_cars[i].animation = animation;
        cs_remote_cars[i].roof = 0; /* a car (re)loaded comes closed */
    }
    if (open != cs_remote_cars[i].roof) {
        const char *event = open ? "roof:open" : "roof:close";
        if (animation && cs_has_animation_event(animation, event)) {
            ((AnimationDetail)(uintptr_t)(cs_game + CS_ANIMATION_DETAIL_RVA))(animation, 0);
            ((AnimationEventFn)(uintptr_t)(cs_game + CS_ANIMATION_EVENT_RVA))(car, event);
        }
        cs_remote_cars[i].roof = open;
    }
    int detail;
    if (open && animation && memory_read((unsigned char *)animation + 0x10, &detail, 4) && detail != 0) {
        ((AnimationDetail)(uintptr_t)(cs_game + CS_ANIMATION_DETAIL_RVA))(animation, 0);
    }
}

static int cs_remote_state(void *car, int *lights, int *open) {
    if (!azimuth_carstate_any()) {
        return 0;
    }
    float position[3];
    int known = azimuth_carstate_needs_position(car) && cs_car_position(car, position);
    if (!azimuth_carstate_wanted(car, known ? position : 0, lights, open)) {
        int i = cs_remote_slot(car, 0);
        if (i >= 0) {
            if (cs_remote_cars[i].lights) {
                ((HeadlightSetter)(uintptr_t)(cs_game + CS_HEADLIGHT_MANUAL_RVA))(car, 0);
            }
            cs_remote_cars[i].car = 0;
        }
        return 0;
    }
    return 1;
}

/* -------------------------------------------------------- the car update */

static void cs_car_update(void *self, float dt, float frame_dt, unsigned frame) {
    fi_game_callback();
    if (!self || !fi_eo_sync_enabled()) {
        cs_car_update_original(self, dt, frame_dt, frame);
        return;
    }
    int owned = cs_local_car_for(frame) == self;
    int lights = 0, open = 0;
    int remote = !owned && cs_remote_state(self, &lights, &open);
    if (remote) {
        cs_show_remote(self, lights, open, 0, dt);
    }
    cs_car_update_original(self, dt, frame_dt, frame);
    if (remote) {
        cs_show_remote(self, lights, open, 1, dt);
    }
    if (owned) {
        cs_report_local(self);
    }
}

/* ------------------------------------------------------------- installing */

/* A 14-byte jump at `at` to `hook`, the first `size` bytes (checked equal to `expected`) moved
   to a trampoline that goes on after them. The trampoline, or 0. */
static void *cs_detour(unsigned char *at, const unsigned char *expected, unsigned size, const void *hook,
                       unsigned char *trampoline) {
    unsigned char current[16];
    if (size > sizeof(current) || !memory_read(at, current, size) || !memory_equal(current, expected, size)) {
        return 0;
    }
    uintptr_t back = (uintptr_t)(at + size), target = (uintptr_t)hook;
    memory_copy(trampoline, current, size);
    unsigned char jump[14] = {0xff, 0x25, 0, 0, 0, 0};
    memory_copy(jump + 6, &back, 8);
    memory_copy(trampoline + size, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), trampoline, size + sizeof(jump));
    memory_copy(jump + 6, &target, 8);
    DWORD old_protection, ignored;
    if (!VirtualProtect(at, sizeof(jump), PAGE_EXECUTE_READWRITE, &old_protection)) {
        return 0;
    }
    memory_copy(at, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), at, sizeof(jump));
    VirtualProtect(at, sizeof(jump), old_protection, &ignored);
    return trampoline;
}

/* 1 both detours in place. */
static int cs_install(unsigned char *game_image) {
    if (InterlockedCompareExchange(&cs_installed, 1, 0) != 0) {
        return 1;
    }
    cs_game = game_image;
    unsigned char *code = VirtualAlloc(0, 128, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if (!code) {
        InterlockedExchange(&cs_installed, 0);
        return 0;
    }
    /* the event detour first: the update detour calls the events of the others' cars */
    void *event = cs_detour(game_image + CS_ANIMATION_EVENT_RVA, cs_animation_event_bytes,
                            sizeof(cs_animation_event_bytes), (const void *)(uintptr_t)cs_animation_event, code + 64);
    if (!event) {
        log_append("CARSTATE: the animation event code is not as expected; no headlight and roof sync.\r\n");
        return 0;
    }
    cs_animation_event_original = (AnimationEventFn)(uintptr_t)event;
    void *update = cs_detour(game_image + CS_CAR_UPDATE_RVA, cs_car_update_bytes, sizeof(cs_car_update_bytes),
                             (const void *)(uintptr_t)cs_car_update, code);
    if (!update) {
        log_append("CARSTATE: the car update code is not as expected; no headlight and roof sync.\r\n");
        return 0;
    }
    cs_car_update_original = (CarUpdateFn)(uintptr_t)update;
    return 1;
}

void fi_carstate_apply(unsigned char *game_image) {
    if (fi_cfg()->carstate == FI_CARSTATE_OFF) {
        log_append("CARSTATE: off (Infinity.cfg [hook] CarState=off).\r\n");
        return;
    }
    if (!azimuth_net_proxy_enabled() || !fi_cfg()->freeroam) {
        return; /* no online free roam this session: nothing to sync */
    }
    if (cs_install(game_image)) {
        log_append("CARSTATE: car update and animation event hooks in place; headlight and roof sync runs only "
                   "with the player's ExtraOptions 0.2.98 active.\r\n");
    }
}
