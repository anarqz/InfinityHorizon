/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   memory_detour14 (support.h): the 14-byte detour every module uses, in a file of its own so
   host tests can build it with their own memory helpers. */
#include "support.h"

void *memory_detour14(unsigned char *at, const unsigned char *expected, unsigned size, const void *hook,
                      unsigned char *trampoline) {
    unsigned char current[16];
    if (size < 14 || size > sizeof(current) || !memory_read(at, current, size) || !memory_equal(current, expected, size)) {
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
