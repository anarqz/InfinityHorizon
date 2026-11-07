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
    static unsigned last[GAME_WATCH_MAX], raw[GAME_WATCH_MAX];
    static ULONGLONG changed_at[GAME_WATCH_MAX];
    const FiGameProfile *game = fi_game_profile();
    unsigned char *image = parameter;
    unsigned count = game->watch_count < GAME_WATCH_MAX ? game->watch_count : GAME_WATCH_MAX;
    for (unsigned i = 0; i < count; ++i) {
        last[i] = raw[i] = 0xdeadbeefu;
    }
    for (;;) {
        ULONGLONG now = GetTickCount64();
        for (unsigned i = 0; i < count; ++i) {
            const FiGameWatch *watch = &game->watches[i];
            unsigned char *object = 0;
            if (!memory_read(image + watch->global, &object, sizeof(object)) || !object) {
                continue;
            }
            unsigned value = 0;
            if (watch->kind == FI_WATCH_POINTER_ONCE) {
                if (last[i] != 1) {
                    last[i] = 1;
                    game_watch_log_pointer(watch->label, object);
                }
                continue;
            }
            if (watch->kind == FI_WATCH_NONNULL) {
                unsigned long long pointer = 0;
                if (watch->bytes > sizeof(pointer) || !memory_read(object + watch->offset, &pointer, watch->bytes)) {
                    continue;
                }
                value = pointer != 0;
            } else {
                if (watch->bytes > sizeof(value) || !memory_read(object + watch->offset, &value, watch->bytes)) {
                    continue;
                }
                if (watch->kind == FI_WATCH_HEARTBEAT) {
                    if (value != raw[i]) {
                        raw[i] = value;
                        changed_at[i] = now;
                    }
                    value = changed_at[i] && now - changed_at[i] <= GAME_HEARTBEAT_MS;
                }
            }
            if (value != last[i]) {
                last[i] = value;
                log_append_value(watch->label, (int)value);
            }
        }
        Sleep(200);
    }
}

static void game_first_request(void) {
    static volatile LONG done;
    const FiGameProfile *game = fi_game_profile();
    unsigned char *image = fi_game_selected_image;
    if (!image || (!game->patch_count && !game->watch_count) || InterlockedExchange(&done, 1)) {
        return;
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
