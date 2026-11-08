/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The selected game's patch and watch tables (core/game_profile.h, OD-206), from the game's first
   IXMLHTTPRequest2. Included by net.c alone. */
#ifndef FI_GAME_PATCHES_H
#define FI_GAME_PATCHES_H

static int game_patch_apply(unsigned char *at, const FiGamePatch *patch) {
    unsigned char current[sizeof(patch->original)];
    DWORD old_protection, ignored;
    if (patch->size > sizeof(current) || !memory_read(at, current, patch->size) ||
        !memory_equal(current, patch->original, patch->size) ||
        !VirtualProtect(at, patch->size, PAGE_EXECUTE_READWRITE, &old_protection)) {
        return 0;
    }
    memory_copy(at, patch->replacement, patch->size);
    FlushInstructionCache(GetCurrentProcess(), at, patch->size);
    VirtualProtect(at, patch->size, old_protection, &ignored);
    return 1;
}

/* "<label> 0x<16 hex digits>" and a line break. */
static void game_watch_log_pointer(const char *label, const void *pointer) {
    char line[24] = " 0x";
    uintptr_t value = (uintptr_t)pointer;
    for (unsigned i = 0; i < 16; ++i) {
        line[3 + i] = "0123456789abcdef"[(value >> (60 - 4 * i)) & 15];
    }
    line[19] = '\r';
    line[20] = '\n';
    line[21] = 0;
    log_append(label);
    log_append(line);
}

static DWORD WINAPI game_watch_thread(void *parameter) {
    enum { GAME_WATCH_MAX = 24, GAME_HEARTBEAT_MS = 1000 };
    static unsigned last[GAME_WATCH_MAX], raw[GAME_WATCH_MAX], reported[GAME_WATCH_MAX];
    static ULONGLONG changed_at[GAME_WATCH_MAX];
    const FiGameProfile *game = fi_game_profile();
    unsigned char *image = parameter;
    unsigned count = game->watch_count < GAME_WATCH_MAX ? game->watch_count : GAME_WATCH_MAX;
    for (unsigned i = 0; i < count; ++i) {
        last[i] = raw[i] = reported[i] = 0xdeadbeefu;
    }
    for (;;) {
        ULONGLONG now = GetTickCount64();
        for (unsigned i = 0; i < count; ++i) {
            const FiGameWatch *watch = &game->watches[i];
            unsigned char *object = 0;
            unsigned value = 0;
            int global_ok = memory_read(image + watch->global, &object, sizeof(object));
            int object_ok = global_ok && object;
            if (watch->event == FI_WATCH_EVENT_LOBBY_KIND) {
                /* OD-224 (T-1108-L): the game's lobby kind to the launcher (Event 10) at each change; 0 when no lobby object; a read
                   that faults reports nothing */
                unsigned kind = 0;
                int read_ok = object_ok && watch->kind == FI_WATCH_CHAIN && fi_watch_value(memory_read, object, watch, &value);
                if (fi_watch_lobby_kind_of(global_ok, object != 0, read_ok, value, &kind) && kind != reported[i]) {
                    char detail[4];
                    reported[i] = kind;
                    fi_activity_screen_detail(kind, detail); /* the same decimal 0..255 as Event 9 */
                    fi_launch_event(FI_EVENT_LOBBY_KIND, detail);
                }
                value = 0;
            }
            if (watch->event == FI_WATCH_EVENT_SCREEN) {
                /* OD-223 (T-1108-M): the game's screen to the launcher (Event 9) at each change; unknown when the walk fails or one
                   of the profile's screen_terms does not hold (FM5: the hoppers' cloud config not loaded) */
                int read_ok = object_ok && watch->kind == FI_WATCH_CHAIN && fi_watch_value(memory_read, object, watch, &value);
                unsigned screen = fi_watch_screen_gated(memory_read, image, game, read_ok, value);
                if (screen != reported[i]) {
                    char detail[4];
                    reported[i] = screen;
                    fi_activity_screen_detail(screen, detail);
                    fi_launch_event(FI_EVENT_SCREEN, detail);
                }
            }
            if (!object_ok) {
                continue;
            }
            if (watch->kind == FI_WATCH_POINTER_ONCE) {
                if (last[i] != 1) {
                    last[i] = 1;
                    game_watch_log_pointer(watch->label, object);
                }
                continue;
            }
            if (watch->kind == FI_WATCH_HEARTBEAT) {
                if (watch->bytes > sizeof(value) || !memory_read(object + watch->offset, &value, watch->bytes)) {
                    continue;
                }
                if (value != raw[i]) {
                    raw[i] = value;
                    changed_at[i] = now;
                }
                value = changed_at[i] && now - changed_at[i] <= GAME_HEARTBEAT_MS;
            } else if (!fi_watch_value(memory_read, object, watch, &value)) {
                /* VALUE, NONNULL and CHAIN (game_profile.h; a CHAIN row's value is its walk's, never object+offset) */
                continue;
            }
            if (value != last[i]) {
                last[i] = value;
                log_append_value(watch->label, (int)value);
            }
        }
        Sleep(200);
    }
}

/* Every term of the selected game's polled readiness, plus its heartbeat within the row's heartbeat_window_ms (W9). Read only, every 250 ms;
   the publishing (and its 500 ms cadence and 1.5 s freshness) is game_ready.c's, as for FH2. */
static int game_ready_term(unsigned char *image, const FiReadyTerm *term) {
    unsigned char *object = 0;
    unsigned long long value = 0;
    if (term->bytes > sizeof(value) || !memory_read(image + term->global, &object, sizeof(object)) || !object ||
        !memory_read(object + term->offset, &value, term->bytes)) {
        return 0;
    }
    return term->op == FI_READY_EQ ? value == term->value : term->op == FI_READY_NE ? value != term->value : value >= term->value;
}

static DWORD WINAPI game_ready_thread(void *parameter) {
    const FiGameProfile *game = fi_game_profile();
    unsigned char *image = parameter;
    unsigned beat = 0;
    ULONGLONG beat_at = 0;
    int logged = -1;
    for (;;) {
        ULONGLONG now = GetTickCount64();
        unsigned char *beat_object = 0;
        unsigned value = 0;
        if (memory_read(image + game->heartbeat_global, &beat_object, sizeof(beat_object)) && beat_object &&
            memory_read(beat_object + game->heartbeat_offset, &value, sizeof(value)) && value != beat) {
            beat = value;
            beat_at = now;
        }
        /* the row's window (FM5 3000 ms: a frame gap of a few seconds is a normal hitch, game_profile.h) */
        int ready = fi_ready_heartbeat_fresh(beat_at, now, game->heartbeat_window_ms);
        for (unsigned i = 0; ready && i < game->ready_term_count; ++i) {
            ready = game_ready_term(image, &game->ready_terms[i]);
        }
        void *identity[2] = {0, 0};
        for (unsigned i = 0; i < 2; ++i) {
            memory_read(image + game->ready_identity[i], &identity[i], sizeof(identity[i]));
        }
        fi_game_ready_polled(ready, identity[0], identity[1]);
        if (ready != logged) {
            logged = ready;
            log_append(ready ? "GAME READY: 1 (the launcher may seat this game in a lobby).\r\n"
                             : "GAME READY: 0.\r\n");
        }
        Sleep(250);
    }
}

static void game_first_request(void) {
    static volatile LONG done;
    const FiGameProfile *game = fi_game_profile();
    unsigned char *image = fi_game_selected_image;
    if (!image || (!game->patch_count && !game->watch_count && !game->ready_term_count) || InterlockedExchange(&done, 1)) {
        return;
    }
    if (game->ready_term_count) {
        HANDLE ready = CreateThread(0, 0, game_ready_thread, image, 0, 0);
        if (ready) {
            CloseHandle(ready);
        }
    }
    for (unsigned i = 0; i < game->patch_count; ++i) {
        const FiGamePatch *patch = &game->patches[i];
        log_append(game_patch_apply(image + patch->rva, patch) ? "GAME PATCH: " : "GAME PATCH NOT APPLIED (unexpected bytes): ");
        log_append(patch->what);
        log_append(".\r\n");
    }
    if (game->watch_count) {
        HANDLE thread = CreateThread(0, 0, game_watch_thread, image, 0, 0);
        if (thread) {
            CloseHandle(thread);
        }
    }
}
#endif
