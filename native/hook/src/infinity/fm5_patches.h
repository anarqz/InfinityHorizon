/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   FM5 1.0.0.20 only (OD-206, docs/launcher/FM5_INFINITY_HANDOFF.md 3), from the game's first
   IXMLHTTPRequest2, as the LAN lab did. Each site's original bytes are checked before it is
   written, and only in memory. Included by net.c alone. */
#ifndef FI_FM5_PATCHES_H
#define FI_FM5_PATCHES_H

static int fm5_patch(unsigned char *at, const unsigned char *expected, const unsigned char *bytes, unsigned size) {
    unsigned char current[16];
    DWORD old_protection, ignored;
    if (size > sizeof(current) || !memory_read(at, current, size) || !memory_equal(current, expected, size) ||
        !VirtualProtect(at, size, PAGE_EXECUTE_READWRITE, &old_protection)) {
        return 0;
    }
    memory_copy(at, bytes, size);
    FlushInstructionCache(GetCurrentProcess(), at, size);
    VirtualProtect(at, size, old_protection, &ignored);
    return 1;
}

/* Read only: each change of the states the hopper screen decides on (for RE, response spec 9). */
static DWORD WINAPI fm5_watch(void *parameter) {
    static const struct {
        const char *label;
        unsigned global, offset, bytes;
    } watched[] = {
        {"FM5 WATCH party state +48", 0x261d198u, 0x48u, 4},
        {"FM5 WATCH party requested +4c", 0x261d198u, 0x4cu, 4},
        {"FM5 WATCH party flag +290", 0x261d198u, 0x290u, 1},
        {"FM5 WATCH cloud config +11510", 0x261d1a0u, 0x11510u, 4},
        {"FM5 WATCH sessions +38", 0x261d1a0u, 0x38u, 4},
        {"FM5 WATCH ticket +114f8", 0x261d1a0u, 0x114f8u, 4},
        {"FM5 WATCH match requester +b0", 0x261d188u, 0xb0u, 4},
    };
    static unsigned last[ARRAY_COUNT(watched)];
    unsigned char *game = parameter;
    for (unsigned i = 0; i < ARRAY_COUNT(watched); ++i) {
        last[i] = 0xdeadbeefu;
    }
    for (;;) {
        for (unsigned i = 0; i < ARRAY_COUNT(watched); ++i) {
            unsigned char *object = 0;
            unsigned value = 0;
            if (memory_read(game + watched[i].global, &object, sizeof(object)) && object &&
                memory_read(object + watched[i].offset, &value, watched[i].bytes) && value != last[i]) {
                last[i] = value;
                log_append_value(watched[i].label, (int)value);
            }
        }
        Sleep(200);
    }
}

/* Azimuth's store answers every privilege check with Restricted, and FM5's MULTIPLAYER path stops
   silently on it: the completion now does what NoIssue does (mov rax,[rcx]; mov dword [rax+3Ch],3; ret).
   DIAGNOSTIC until the real Party fix (lobby spec 11): the hopper screen ignores Party state 6, by no
   jump to the failure branch at 0x14032ef84 and always the "party ready" branch at 0x14032ef93. */
static void fm5_first_request(void) {
    static volatile LONG done;
    static const unsigned char privilege_old[11] = {0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x4c, 0x8b, 0xca, 0x48, 0x8b};
    static const unsigned char privilege_new[11] = {0x48, 0x8b, 0x01, 0xc7, 0x40, 0x3c, 0x03, 0x00, 0x00, 0x00, 0xc3};
    static const unsigned char state6_old[2] = {0x74, 0x9f}, state6_new[2] = {0x90, 0x90};
    static const unsigned char ready_old[1] = {0x75}, ready_new[1] = {0xeb};
    unsigned char *game = fi_game_selected_image;
    if (!game || InterlockedExchange(&done, 1)) {
        return;
    }
    log_append(fm5_patch(game + 0x7d58d0u, privilege_old, privilege_new, sizeof(privilege_new))
                   ? "FM5: privilege answer forced to NoIssue.\r\n"
                   : "FM5: privilege patch NOT applied (unexpected bytes).\r\n");
    int skipped = fm5_patch(game + 0x32ef84u, state6_old, state6_new, sizeof(state6_new)) &&
                  fm5_patch(game + 0x32ef93u, ready_old, ready_new, sizeof(ready_new));
    log_append(skipped ? "FM5: hopper screen ignores Party (diagnostic).\r\n"
                       : "FM5: Party skip NOT applied (unexpected bytes).\r\n");
    HANDLE thread = CreateThread(0, 0, fm5_watch, game, 0, 0);
    if (thread) {
        CloseHandle(thread);
    }
}
#endif
