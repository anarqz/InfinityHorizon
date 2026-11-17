/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Daylight headlights of the player's own car on presets of its own (CR-L53, OD-248). Headlights
   forced on light up only at night when the car's light presets
   say so: the daylight threshold lives on the light *preset* (beams +0x9c/+0xa0, glows +0x44/+0x48),
   and a preset is shared by every light of its light set (a car model's), so zeroing it (what
   ExtraOptions' headlight_beams.c and FH2 ANNA did for the player's car) lights every car of that
   model, traffic included. Here the player's car gets its own presets instead. (OD-248 answers,
   9c259f73: no daylight trick on remote cars in v1; their forced-on headlights look as the game
   draws them. The engine keeps a small table of cars only so the previous local car's presets go
   back when the player changes car.)
   - for each headlight instance (amount type 13 or 17) of the car's asset, its light record (the
     light manager's mapping +0x28 -> the beam record, system +0, or the glow record, system
     +0x678), its preset index (beam record +0x30; glow record +0xc, low 15 bits);
   - a preset of our own from the scene's pool (beams: game+0x10be480 on system+0x38, 0xc0 each;
     glows: game+0x10be5f0 on system+0x6b0, 0x120 each), a copy of the original's definition with
     the threshold at 0, attached to that record with the game's own attach (beams game+0x10f1eb0,
     which keeps the presets' user counts and sets the system's +0x100 so the beam presets are
     uploaded again; glows game+0x10f1e20, uploaded every frame);
   - when the headlights go back to the game's, the car is no longer the player's (or no update
     for CL_APPLY_MS), the original preset goes back on each record that
     still holds ours, and ours is kept for the next car (never freed: the pools are fixed arrays,
     and ours are at most CL_SPARE per kind).
   A pool with no free preset leaves that light as the game has it: the shared preset is never
   written. Everything happens inside fi_carlights_upload, called by ExtraOptions.Infinity's
   upload detour (headlight_beams.c, patch 0007) just before the game uploads that system, on the
   renderer's thread; the game thread only says which car wants it (a lock between the two).
   Static RE: Ghidra FH2_DURANGO (WINDOWS, 2026-11-17): 0x1410be480/0x1410be5f0 pop a pool's
   free list and return the index (-1 empty); 0x1410c8ce0/0x1410c8b00 register a light set's
   presets the same way; 0x1410c7e00 (CreateLight) attaches with 0x1410f1eb0 / 0x1410f1e20;
   0x1410e2fa0 / 0x1410e2e80 move the users count (+0x30 / +0xec) from the old preset to the new.
   The beam upload 0x1410f6c40 reads a preset's +0x50..+0x70, +0x98..+0xa4 and the texture
   handle at +0xb0 (its +0x80/+0x84): those are what a copy carries (plus +0x40..+0x50), and the
   handle (pointer, control block whose +8 is the strong count) gets a reference of ours. A glow
   preset is plain data (0xec bytes copied by 0x1410bae90). Live probe P4 (SAURON) confirms. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define CL_ALLOC_BEAM_PRESET_RVA 0x10be480u
#define CL_ALLOC_GLOW_PRESET_RVA 0x10be5f0u
#define CL_ATTACH_BEAM_RVA 0x10f1eb0u
#define CL_ATTACH_GLOW_RVA 0x10f1e20u
#define CL_RENDERER_VTABLE_A_RVA 0x2ec1450u
#define CL_RENDERER_VTABLE_B_RVA 0x2ec2b70u
#define CL_CARS 4 /* the local car, and the ones it was a moment ago, still going back */
#define CL_LIGHTS 8 /* headlight records per car and kind (a car has 2..6) */
#define CL_SPARE 64 /* presets of ours per kind, attached or kept */
#define CL_APPLY_MS 200u /* the car's renderer is followed only this long after its last update */
#define CL_GONE_MS 2000u /* a scene not uploaded this long is gone: what was in it is dropped unwritten */
#define CL_NONE 0xffffffffu

#ifndef CL_NOW
#define CL_NOW() GetTickCount64()
#endif

typedef unsigned (*PresetAlloc)(void *pool);
typedef void (*PresetAttach)(void *system, unsigned record, unsigned preset);

typedef struct {
    unsigned record, original, ours;
    int seen;
} ClLight;

typedef struct {
    void *car, *renderer;
    int want;
    unsigned long long seen_at;
    void *system[2]; /* the beam and glow systems its lights are in */
    uintptr_t print[2]; /* and their fingerprints (cl_print) */
    unsigned long long uploaded_at[2];
    ClLight lights[2][CL_LIGHTS];
    int count[2];
} ClCar;

typedef struct {
    void *system;
    uintptr_t print; /* the system's fingerprint when it was allocated */
    unsigned long long seen_at;
    unsigned index;
    int attached;
} ClSpare;

static unsigned char *cl_game;
static ClCar cl_cars[CL_CARS];
static ClSpare cl_spare[2][CL_SPARE];
static int cl_spare_count[2];
static SRWLOCK cl_lock = SRWLOCK_INIT;
static volatile LONG cl_logged;
static uintptr_t cl_current; /* the fingerprint of the system being uploaded (under cl_lock) */

/* zeroed byte by byte (volatile: the freestanding DLL has no memset to be turned into) */
static void cl_zero(void *at, size_t size) {
    volatile unsigned char *p = at;
    while (size--) {
        *p++ = 0;
    }
}

void fi_carlights_bind(unsigned char *game_image) {
    AcquireSRWLockExclusive(&cl_lock);
    if (cl_game != game_image) {
        cl_zero(cl_cars, sizeof(cl_cars));
        cl_spare_count[0] = cl_spare_count[1] = 0;
    }
    cl_game = game_image;
    ReleaseSRWLockExclusive(&cl_lock);
}

/* ------------------------------------------------------------- reading */

static unsigned char *cl_record(unsigned char *owner, unsigned offset, unsigned index, size_t stride) {
    uintptr_t range[2] = {0, 0};
    if (!owner || index == CL_NONE || !memory_read(owner + offset, range, sizeof(range)) || !range[0] ||
        range[1] < range[0] || (range[1] - range[0]) % stride || (range[1] - range[0]) / stride > 100000 ||
        index >= (range[1] - range[0]) / stride) {
        return 0;
    }
    return (unsigned char *)(range[0] + index * stride);
}

static void *cl_renderer(void *car) {
    void *proxy = 0, *actual = 0, *vtable = 0;
    if (!cl_game || !car || !memory_read_pointer((unsigned char *)car + 0x4d38, &proxy) ||
        !memory_read_pointer((unsigned char *)proxy + 8, &actual) || !memory_read_pointer(actual, &vtable) ||
        (vtable != cl_game + CL_RENDERER_VTABLE_A_RVA && vtable != cl_game + CL_RENDERER_VTABLE_B_RVA)) {
        return 0;
    }
    return actual;
}

/* A system's fingerprint: where its light records and presets arrays are. A system freed and made
   again (a new scene) at the same address has new arrays, so nothing of ours from the old one is
   ever written into it. 0 when unreadable. */
static uintptr_t cl_print(unsigned char *system, int glow) {
    uintptr_t records = 0, presets = 0;
    if (!system || !memory_read(system + (glow ? 0x678 : 0), &records, sizeof(records)) ||
        !memory_read(system + (glow ? 0x6b0 : 0x38), &presets, sizeof(presets)) || !records || !presets) {
        return 0;
    }
    return presets ^ (records << 1 | records >> (8 * sizeof(records) - 1));
}

/* The preset index a light record holds. */
static unsigned cl_preset_of(unsigned char *system, int glow, unsigned record) {
    unsigned char *light = cl_record(system, glow ? 0x678 : 0, record, 0x40);
    unsigned preset = CL_NONE;
    if (!light || !memory_read(light + (glow ? 0xc : 0x30), &preset, 4)) {
        return CL_NONE;
    }
    return glow ? preset & 0x7fff : preset;
}

static unsigned char *cl_preset(unsigned char *system, int glow, unsigned index) {
    return glow ? cl_record(system, 0x6b0, index, 0x120) : cl_record(system, 0x38, index, 0xc0);
}

/* ------------------------------------------------------------- our presets */

static int cl_is_ours(void *system, int glow, unsigned index) {
    for (int i = 0; i < cl_spare_count[glow]; ++i) {
        if (cl_spare[glow][i].system == system && cl_spare[glow][i].print == cl_current &&
            cl_spare[glow][i].index == index) {
            return 1;
        }
    }
    return 0;
}

/* A preset of ours not attached to anything, else a new one from the pool (CL_NONE when the pool
   or our share of it is used up). */
static unsigned cl_take(unsigned char *system, int glow) {
    for (int i = 0; i < cl_spare_count[glow]; ++i) {
        ClSpare *s = &cl_spare[glow][i];
        if (s->system != system || s->print != cl_current || s->attached) {
            continue;
        }
        unsigned char *preset = cl_preset(system, glow, s->index);
        int users = -1;
        if (!preset || !memory_read(preset + (glow ? 0xec : 0x30), &users, 4) || users != 0) {
            cl_spare[glow][i--] = cl_spare[glow][--cl_spare_count[glow]]; /* not ours any more: forgotten */
            continue;
        }
        s->attached = 1;
        return s->index;
    }
    if (cl_spare_count[glow] >= CL_SPARE) {
        return CL_NONE;
    }
    unsigned index = ((PresetAlloc)(uintptr_t)(cl_game + (glow ? CL_ALLOC_GLOW_PRESET_RVA : CL_ALLOC_BEAM_PRESET_RVA)))(
        system + (glow ? 0x6b0 : 0x38));
    unsigned char *preset = index == CL_NONE ? 0 : cl_preset(system, glow, index);
    if (!preset) {
        return CL_NONE;
    }
    int users = 0;
    memory_copy(preset + (glow ? 0xec : 0x30), &users, 4); /* as the game's own registration starts it */
    cl_spare[glow][cl_spare_count[glow]++] = (ClSpare){system, cl_current, CL_NOW(), index, 1};
    return index;
}

static void cl_give_back(void *system, int glow, unsigned index) {
    for (int i = 0; i < cl_spare_count[glow]; ++i) {
        if (cl_spare[glow][i].system == system && cl_spare[glow][i].print == cl_current &&
            cl_spare[glow][i].index == index) {
            cl_spare[glow][i].attached = 0;
        }
    }
}

/* One reference more on a texture handle's control block (+8 strong count). */
static void cl_reference(unsigned char *control) {
    if (control && memory_is_writable(control + 8, 4)) {
        InterlockedIncrement((volatile LONG *)(control + 8));
    }
}

/* One reference less, only while another remains (ours never destroys the texture). */
static void cl_unreference(unsigned char *control) {
    if (!control || !memory_is_writable(control + 8, 4)) {
        return;
    }
    volatile LONG *count = (volatile LONG *)(control + 8);
    for (LONG c = *count; c > 1; c = *count) {
        if (InterlockedCompareExchange(count, c - 1, c) == c) {
            return;
        }
    }
}

/* `ours` becomes a copy of `original` that shows in daylight: 1 done. */
static int cl_copy(unsigned char *system, int glow, unsigned original, unsigned ours) {
    unsigned char *from = cl_preset(system, glow, original), *to = cl_preset(system, glow, ours);
    unsigned char data[0xec];
    if (!from || !to || !memory_is_writable(to, glow ? 0xec : 0xc0)) {
        return 0;
    }
    const float zero[2] = {0, 0};
    if (glow) {
        if (!memory_read(from, data, 0xec)) {
            return 0;
        }
        memory_copy(to, data, 0xec); /* the users count (+0xec) stays ours */
        memory_copy(to + 0x44, zero, sizeof(zero));
        return 1;
    }
    /* +0x40..+0x78 and +0x98..+0xb0: the definition but its name (std::string at +0x78), which
       stays our preset's own; then the texture handle with a reference of ours */
    unsigned char *handle[2] = {0, 0}, *mine[2] = {0, 0};
    if (!memory_read(from + 0x40, data, 0x38) || !memory_read(from + 0x98, data + 0x38, 0x18) ||
        !memory_read(from + 0xb0, handle, sizeof(handle)) || !memory_read(to + 0xb0, mine, sizeof(mine))) {
        return 0;
    }
    memory_copy(to + 0x40, data, 0x38);
    memory_copy(to + 0x98, data + 0x38, 0x18);
    if (mine[1] != handle[1]) {
        cl_reference(handle[1]);
        cl_unreference(mine[1]);
    }
    memory_copy(to + 0xb0, handle, sizeof(handle));
    memory_copy(to + 0x9c, zero, sizeof(zero));
    return 1;
}

static void cl_attach(unsigned char *system, int glow, unsigned record, unsigned preset) {
    ((PresetAttach)(uintptr_t)(cl_game + (glow ? CL_ATTACH_GLOW_RVA : CL_ATTACH_BEAM_RVA)))(system, record, preset);
}

/* ------------------------------------------------------------- a car */

/* The original back on every light of `c` in `system` that still holds ours; ours kept for the next
   car. A record that holds something else was rebuilt by the game: it is left alone. */
static void cl_release(ClCar *c, unsigned char *system, int glow) {
    if (c->system[glow] != system) {
        return;
    }
    for (int i = 0; i < c->count[glow]; ++i) {
        ClLight *l = &c->lights[glow][i];
        if (cl_preset_of(system, glow, l->record) == l->ours) {
            cl_attach(system, glow, l->record, l->original);
        }
        int shared = 0;
        for (int j = i + 1; j < c->count[glow]; ++j) {
            shared |= c->lights[glow][j].ours == l->ours;
        }
        if (!shared) {
            cl_give_back(system, glow, l->ours);
        }
    }
    c->count[glow] = 0;
    c->system[glow] = 0;
}

/* Every headlight of `c` in `system` on a preset of ours (a light whose original is already copied
   for this car shares that copy). */
static void cl_apply(ClCar *c, unsigned char *system, int glow) {
    unsigned char *renderer = c->renderer, *context = 0, *manager = 0, *backing = 0, *asset = 0;
    uintptr_t instances[2] = {0, 0};
    if (!memory_read_pointer(renderer + 0x10, (void **)&context) ||
        !memory_read_pointer(context + 0x380, (void **)&manager) ||
        !memory_read_pointer(manager + (glow ? 0x18 : 8), (void **)&backing) || backing != system) {
        return; /* this car is in another scene */
    }
    if (c->system[glow] && c->system[glow] != system) {
        cl_release(c, c->system[glow], glow); /* not reached: a car does not change scenes */
    }
    if (!memory_read_pointer(renderer + 0x1278, (void **)&asset) || !memory_read(asset + 0x4be0, instances, 16) ||
        !instances[0] || instances[1] < instances[0] || (instances[1] - instances[0]) % 8 ||
        (instances[1] - instances[0]) / 8 > 512) {
        return;
    }
    c->system[glow] = system;
    for (int i = 0; i < c->count[glow]; ++i) {
        c->lights[glow][i].seen = 0;
    }
    for (uintptr_t at = instances[0]; at < instances[1]; at += 8) {
        unsigned char *instance = 0, data[0x54];
        if (!memory_read_pointer((void *)at, (void **)&instance) || !memory_read(instance, data, sizeof(data))) {
            continue;
        }
        unsigned short amount;
        unsigned mapping_index, record = CL_NONE;
        memory_copy(&amount, data + 4, 2);
        memory_copy(&mapping_index, data + 0x50, 4);
        if (amount != 13 && amount != 17) {
            continue;
        }
        unsigned char *mapping = cl_record(manager, 0x28, mapping_index, 24);
        if (!mapping || !memory_read(mapping + (glow ? 4 : 0), &record, 4)) {
            continue;
        }
        unsigned preset = cl_preset_of(system, glow, record);
        if (preset == CL_NONE) {
            continue;
        }
        ClLight *known = 0;
        for (int i = 0; i < c->count[glow] && !known; ++i) {
            if (c->lights[glow][i].record == record) {
                known = &c->lights[glow][i];
            }
        }
        if (known && preset == known->ours) {
            known->seen = 1;
            continue;
        }
        if (cl_is_ours(system, glow, preset)) {
            continue; /* another car's copy (never: a record is one car's) */
        }
        if (known) {
            known->record = CL_NONE; /* the game put another preset on it: dropped, then taken anew */
        }
        if (c->count[glow] >= CL_LIGHTS) {
            continue;
        }
        unsigned ours = CL_NONE;
        for (int i = 0; i < c->count[glow] && ours == CL_NONE; ++i) {
            if (c->lights[glow][i].original == preset && c->lights[glow][i].record != CL_NONE) {
                ours = c->lights[glow][i].ours;
            }
        }
        if (ours == CL_NONE) {
            ours = cl_take(system, glow);
            if (ours == CL_NONE) {
                continue; /* the pool is used up: this light stays as the game has it */
            }
            if (!cl_copy(system, glow, preset, ours)) {
                cl_give_back(system, glow, ours);
                continue;
            }
        }
        cl_attach(system, glow, record, ours);
        c->lights[glow][c->count[glow]++] = (ClLight){record, preset, ours, 1};
    }
    /* lights gone from the car (a record the game dropped or rebuilt): forgotten; a copy no light
       holds any more goes back */
    int kept = 0;
    for (int i = 0; i < c->count[glow]; ++i) {
        ClLight l = c->lights[glow][i];
        if (l.seen && l.record != CL_NONE) {
            c->lights[glow][kept++] = l;
            continue;
        }
        if (l.record != CL_NONE && cl_preset_of(system, glow, l.record) == l.ours) {
            cl_attach(system, glow, l.record, l.original);
        }
    }
    int before = c->count[glow];
    c->count[glow] = kept;
    if (kept != before) {
        for (int k = 0; k < cl_spare_count[glow]; ++k) {
            ClSpare *s = &cl_spare[glow][k];
            if (s->system != system || !s->attached) {
                continue;
            }
            int held = 0;
            for (int n = 0; n < CL_CARS && !held; ++n) {
                for (int i = 0; i < cl_cars[n].count[glow] && !held; ++i) {
                    held = cl_cars[n].system[glow] == system && cl_cars[n].lights[glow][i].ours == s->index;
                }
            }
            s->attached = held;
        }
    }
}

/* ------------------------------------------------------------- the two threads */

/* The player's car (NULL: none) and whether its headlights are forced on; every other car of the
   table is no longer wanted and goes back at the next uploads. */
void fi_carlights_local(void *car, int on) {
    void *renderer = on && car ? cl_renderer(car) : 0;
    unsigned long long now = CL_NOW();
    AcquireSRWLockExclusive(&cl_lock);
    ClCar *slot = 0, *free_slot = 0;
    for (int i = 0; i < CL_CARS; ++i) {
        ClCar *c = &cl_cars[i];
        if (car && c->car == car) {
            slot = c;
        } else {
            c->want = 0; /* not the player's car (any more) */
            if (!c->car && !c->count[0] && !c->count[1] && !free_slot) {
                free_slot = c;
            }
        }
    }
    if (!slot && renderer && free_slot) {
        slot = free_slot;
        slot->car = car;
    }
    if (slot) {
        slot->want = renderer != 0;
        slot->renderer = renderer;
        slot->seen_at = now;
    }
    ReleaseSRWLockExclusive(&cl_lock);
}

/* What `c` holds of kind `glow` is forgotten without a write (its scene is gone). */
static void cl_drop(ClCar *c, int glow) {
    c->count[glow] = 0;
    c->system[glow] = 0;
    c->print[glow] = 0;
}

void fi_carlights_upload(void *system_pointer, int glow) {
    unsigned char *system = system_pointer;
    glow = glow ? 1 : 0;
    uintptr_t print = cl_print(system, glow);
    if (!cl_game || !system || !print) {
        return;
    }
    unsigned long long now = CL_NOW();
    int attached = 0;
    AcquireSRWLockExclusive(&cl_lock);
    cl_current = print;
    /* our presets of a scene made again (same address, new arrays) or not uploaded for CL_GONE_MS:
       forgotten, never written */
    for (int i = 0; i < cl_spare_count[glow]; ++i) {
        ClSpare *s = &cl_spare[glow][i];
        if (s->system == system && s->print == print) {
            s->seen_at = now;
        } else if (s->system == system || now - s->seen_at >= CL_GONE_MS) {
            cl_spare[glow][i--] = cl_spare[glow][--cl_spare_count[glow]];
        }
    }
    for (int i = 0; i < CL_CARS; ++i) {
        ClCar *c = &cl_cars[i];
        if (c->system[glow] && ((c->system[glow] == system && c->print[glow] != print) ||
                                (c->system[glow] != system && now - c->uploaded_at[glow] >= CL_GONE_MS))) {
            cl_drop(c, glow);
        }
        if (!c->car && !c->count[0] && !c->count[1]) {
            continue;
        }
        /* the renderer is followed only just after an update of the car: once it is gone, never */
        if (c->car && c->want && c->renderer && now - c->seen_at < CL_APPLY_MS) {
            cl_apply(c, system, glow);
        } else {
            c->want = 0;
            c->renderer = 0;
            cl_release(c, system, glow);
        }
        if (c->system[glow] == system) {
            c->print[glow] = print;
            c->uploaded_at[glow] = now;
        }
        if (!c->want && !c->count[0] && !c->count[1]) {
            cl_zero(c, sizeof(*c)); /* gone: its slot is free */
            continue;
        }
        attached += c->count[glow];
    }
    ReleaseSRWLockExclusive(&cl_lock);
    if (attached && !glow && InterlockedCompareExchange(&cl_logged, 1, 0) == 0) {
        log_append("CARLIGHTS: the player's headlights forced on show in daylight on presets of the car's own.\r\n");
    }
}