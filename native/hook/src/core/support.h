/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md. */
#ifndef FI_SUPPORT_H
#define FI_SUPPORT_H
#include "infinity_base.h"
void memory_copy(void *destination, const void *source, size_t size);
int memory_equal(const void *left, const void *right, size_t size);
size_t text_length(const char *text);
const char *text_find(const char *text, size_t length, const char *needle);
const char *text_find_unique(const char *text, size_t length, const char *needle);
int text_equal_ignore_case(const char *left, const char *right);
int memory_read(const void *address, void *output, size_t size);
int memory_read_pointer(const void *address, void **value);
int float_is_finite(float value);
int memory_is_writable(const void *address, size_t size);
/* A 14-byte absolute jump at `at` to `hook`; the first `size` bytes (checked equal to `expected`,
   position independent, at least 14) move to `trampoline`, which goes on after them. The
   trampoline (callable as the original function), or 0 when the bytes differ. */
void *memory_detour14(unsigned char *at, const unsigned char *expected, unsigned size, const void *hook,
                      unsigned char *trampoline);
void status_set(const char *message);
void status_append(const char *message);
const char *status_get(void);
int path_join(wchar_t output[PATH_CAPACITY], const wchar_t *directory, const wchar_t *name);
#endif
