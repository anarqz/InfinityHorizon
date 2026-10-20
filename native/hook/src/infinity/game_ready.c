/* ForzaInfinity hook, GPL-3.0-or-later. Loaded-game world access; no game data leaves this module. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define GR_UPDATE_RVA 0x2260d0u
#define GR_GAME_RVA 0x34b3e18u
#define GR_MANAGER_RVA 0x34aba60u
#define GR_SCHEDULER_RVA 0x367a6d0u
#define GR_CAR_VTABLE_RVA 0x2c5cb80u
#define GR_SAMPLE_MS 250u
#define GR_PUBLISH_MS 500u
#define GR_FRESH_MS 1500u

/* CForza's vtable slot 18 returns bool in AL (RVA 0x2271b4). Its caller tests AL
   at RVA 0xf74c8c; sampling must preserve that result or the game loop can stop. */
typedef unsigned char (*GrUpdate)(void *, float);
static unsigned char *gr_image;
static GrUpdate gr_original;
static SRWLOCK gr_lock = SRWLOCK_INIT;
static struct {
    unsigned generation;
    unsigned garage_id;
    int ready;
    void *player, *record, *car;
    unsigned long long sampled, published;
} gr_state;

static void *gr_pointer(const void *at) {
    void *p = 0;
    return memory_read_pointer(at, &p) ? p : 0;
}

/* All offsets are the supported executable's getters, verified by read-only frontend/in-game
   comparison. Run only on the main game thread, after its update. No virtual calls or refs. */
static int gr_sample(void **player, void **record, void **local_car, unsigned *active_id) {
    unsigned char *game = gr_pointer(gr_image + GR_GAME_RVA);
    unsigned char selector = 0;
    uintptr_t count = 0;
    if (!game || !memory_read(game + 0x34, &selector, 1) || selector != 1 ||
        !memory_read(game + 0x40, &count, 8) || !count || count > 16) return 0;
    unsigned char *players = gr_pointer(game + 0x38);
    unsigned char *entry = players ? gr_pointer(players) : 0;
    unsigned char *p = entry ? gr_pointer(entry + 0x10) : 0;
    unsigned char *r = p ? gr_pointer(p + 0x348) : 0;
    unsigned garage_id = 0;
    if (!p || !r || !gr_pointer(p + 0x330) ||
        !memory_read(r + 0xae8 + 0x260, &garage_id, 4) || !garage_id ||
        !gr_pointer(gr_image + GR_SCHEDULER_RVA)) return 0;
    unsigned char *manager = gr_pointer(gr_image + GR_MANAGER_RVA);
    unsigned char *context = manager ? gr_pointer(manager + 0x48) : 0;
    unsigned char *network = context ? gr_pointer(context + 0x98) : 0;
    uintptr_t begin = 0, end = 0;
    if (!network || !memory_read(network + 0x458, &begin, 8) || !memory_read(network + 0x460, &end, 8) ||
        !begin || end < begin || (end - begin) % 8 || (end - begin) / 8 > 1024) return 0;
    for (uintptr_t at = begin; at < end; at += 8) {
        unsigned char *car = gr_pointer((void *)at);
        unsigned char excluded = 1;
        unsigned kind = 1;
        if (!car || gr_pointer(car) != gr_image + GR_CAR_VTABLE_RVA || gr_pointer(car + 0x4d00) != p ||
            !memory_read(car + 0x530a, &excluded, 1) || excluded ||
            !memory_read(car + 0x4d08, &kind, 4) || kind) continue;
        *player = p; *record = r; *local_car = car;
        *active_id = garage_id;
        return 1;
    }
    return 0;
}

static void gr_observe(int ready, void *player, void *record, void *car, unsigned garage_id, unsigned long long now) {
    AcquireSRWLockExclusive(&gr_lock);
    if (!gr_state.generation || gr_state.ready != ready || gr_state.player != player ||
        gr_state.record != record || gr_state.car != car || gr_state.garage_id != garage_id) {
        ++gr_state.generation;
        if (!gr_state.generation) ++gr_state.generation;
        gr_state.published = 0;
    }
    gr_state.ready = ready;
    gr_state.player = player; gr_state.record = record; gr_state.car = car;
    gr_state.garage_id = garage_id;
    gr_state.sampled = now;
    unsigned generation = gr_state.generation;
    int publish = !gr_state.published || now - gr_state.published >= GR_PUBLISH_MS;
    if (publish) gr_state.published = now;
    ReleaseSRWLockExclusive(&gr_lock);
    if (publish && fi_launch_protocol()) {
        char text[24], digits[12];
        unsigned n = 0, d = 0;
        do { digits[d++] = (char)('0' + generation % 10); generation /= 10; } while (generation);
        while (d) text[n++] = digits[--d];
        text[n++] = ':'; text[n++] = ready ? '1' : '0'; text[n] = 0;
        fi_launch_event(5, text);
    }
}

int fi_game_ready(unsigned *generation) {
    AcquireSRWLockShared(&gr_lock);
    int ready = gr_state.ready && GetTickCount64() - gr_state.sampled <= GR_FRESH_MS;
    if (generation) *generation = gr_state.generation;
    ReleaseSRWLockShared(&gr_lock);
    return ready;
}

static unsigned char gr_update(void *self, float dt) {
    unsigned char result = gr_original(self, dt);
    unsigned long long now = GetTickCount64();
    if (now - gr_state.sampled >= GR_SAMPLE_MS) {
        void *player = 0, *record = 0, *car = 0;
        unsigned garage_id = 0;
        int ready = gr_sample(&player, &record, &car, &garage_id);
        gr_observe(ready, player, record, car, garage_id, now);
    }
    fi_livery_game_tick();
    return result;
}

static int gr_install(unsigned char *image) {
    /* Four complete instructions and two pushes, no RIP-relative operands. */
    static const unsigned char expected[14] = {0x48, 0x8b, 0xc4, 0xc5, 0xfa, 0x11, 0x48,
                                              0x10, 0x48, 0x89, 0x48, 0x08, 0x53, 0x56};
    unsigned char current[14];
    if (gr_original) return 1;
    if (!memory_read(image + GR_UPDATE_RVA, current, 14) || !memory_equal(current, expected, 14)) return 0;
    unsigned char *code = VirtualAlloc(0, 32, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if (!code) return 0;
    memory_copy(code, current, 14);
    unsigned char jump[14] = {0xff, 0x25, 0, 0, 0, 0};
    uintptr_t back = (uintptr_t)(image + GR_UPDATE_RVA + 14), target = (uintptr_t)gr_update;
    memory_copy(jump + 6, &back, 8);
    memory_copy(code + 14, jump, 14);
    FlushInstructionCache(GetCurrentProcess(), code, 28);
    DWORD old, ignored;
    if (!VirtualProtect(image + GR_UPDATE_RVA, 14, PAGE_EXECUTE_READWRITE, &old)) {
        VirtualFree(code, 0, MEM_RELEASE); return 0;
    }
    gr_image = image;
    gr_original = (GrUpdate)(uintptr_t)code;
    memory_copy(jump + 6, &target, 8);
    memory_copy(image + GR_UPDATE_RVA, jump, 14);
    FlushInstructionCache(GetCurrentProcess(), image + GR_UPDATE_RVA, 14);
    VirtualProtect(image + GR_UPDATE_RVA, 14, old, &ignored);
    return 1;
}

void fi_game_ready_apply(unsigned char *image) {
    log_append(gr_install(image) ? "GAME: loaded-player/car readiness installed on the main update.\r\n"
                                 : "GAME: readiness unavailable (update bytes differ); world entry stays disabled.\r\n");
}
