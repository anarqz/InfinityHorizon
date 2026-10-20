/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* Expansions the Infinity profile has (the server lists them in /me: every profile has Storm
   Island). Storm Island's data is in the base game (ContentOffers 31, MountPath game:\); what
   the game lacks is a licence, so its rival events and the island asked the marketplace for the
   offer and stopped at MARKETPLACE ERROR.

   Each licence pass (FUN_14028b8e0) first runs "UPDATE ContentOffers SET IsPurchased=0", then
   marks the licensed offers and reads them into its caches. Its lea r8 at game+0x28b998 is
   pointed at our statement instead, which also marks offer 31 purchased and installed, written
   in the zero padding at the end of .rdata's last page (within the lea's reach). The game's own
   statement stays as it was for profiles without the expansion.

   The player's ExtraOptions 0.2.98 with UnlockDLC=1 (its default) changes that statement's '0'
   to '1' (its site 0x1e03795): every offer purchased. It patches after our DllMain, so the
   statement may read '0' when we first look and '1' later. Both are accepted, and the cave holds
   two statements: ours ("only 31", for a game without ExtraOptions' unlock) and the same with
   ExtraOptions' "every offer" kept. The lea points at the one that matches the byte as it is
   now, and azimuth_expansions_after_eo moves it, once ExtraOptions has run, if the byte
   changed. ExtraOptions' own byte is never written. */

#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define EXPANSIONS_RESET_LEA_RVA 0x28b998u
#define EXPANSIONS_RESET_SQL_RVA 0x1e03770u
#define EXPANSIONS_CAVE_RVA 0x2c46f30u
#define EXPANSIONS_CAVE_SIZE 0xd0u
/* The value of IsPurchased in the game's statement: ExtraOptions' DLC site. */
#define EXPANSIONS_EO_BYTE 37u
/* Where the second statement starts in the cave. */
#define EXPANSIONS_CAVE_WITH_EO 0x60u

static const char expansions_reset_sql[] = "UPDATE ContentOffers SET IsPurchased=0";
static const char expansions_unlock_sql[] =
    "UPDATE ContentOffers SET IsPurchased=(Id=31),IsInstalled=(IsInstalled OR Id=31)";
static const char expansions_unlock_with_eo_sql[] =
    "UPDATE ContentOffers SET IsPurchased=1,IsInstalled=(IsInstalled OR Id=31)";
_Static_assert(sizeof(expansions_unlock_sql) <= EXPANSIONS_CAVE_WITH_EO, "the first statement fits");
_Static_assert(EXPANSIONS_CAVE_WITH_EO + sizeof(expansions_unlock_with_eo_sql) <= EXPANSIONS_CAVE_SIZE,
               "the second statement fits");
_Static_assert(sizeof(expansions_reset_sql) - 2 == EXPANSIONS_EO_BYTE, "the byte ExtraOptions changes");

static unsigned char *expansions_game;
static volatile LONG expansions_done;
/* 0 not unlocked, 1 the lea points at ours, 2 at ours with ExtraOptions' unlock kept */
static volatile LONG expansions_variant;

static int expansions_write(void *target, const void *bytes, size_t n) {
    DWORD old_protection, ignored;
    if (!VirtualProtect(target, n, PAGE_EXECUTE_READWRITE, &old_protection)) {
        return 0;
    }
    memory_copy(target, bytes, n);
    VirtualProtect(target, n, old_protection, &ignored);
    return 1;
}

/* The game's reset statement: 0 not as expected, '0' as shipped, '1' with ExtraOptions'
   UnlockDLC. */
static char expansions_reset_value(void) {
    char current[sizeof(expansions_reset_sql)];
    if (!memory_read(expansions_game + EXPANSIONS_RESET_SQL_RVA, current, sizeof(current))) {
        return 0;
    }
    char value = current[EXPANSIONS_EO_BYTE];
    current[EXPANSIONS_EO_BYTE] = '0';
    return memory_equal(current, expansions_reset_sql, sizeof(current)) && (value == '0' || value == '1') ? value : 0;
}

/* Points the lea at the cave statement for `value` ('0' or '1'): 1 done. */
static int expansions_point(char value) {
    unsigned char *lea = expansions_game + EXPANSIONS_RESET_LEA_RVA;
    unsigned char *target = expansions_game + EXPANSIONS_CAVE_RVA + (value == '1' ? EXPANSIONS_CAVE_WITH_EO : 0);
    int32_t displacement = (int32_t)(target - (lea + 7));
    if (!expansions_write(lea + 3, &displacement, sizeof(displacement))) {
        return 0;
    }
    FlushInstructionCache(GetCurrentProcess(), lea, 7);
    InterlockedExchange(&expansions_variant, value == '1' ? 2 : 1);
    return 1;
}

static void expansions_unlock(void) {
    unsigned char *lea = expansions_game + EXPANSIONS_RESET_LEA_RVA;
    unsigned char *cave = expansions_game + EXPANSIONS_CAVE_RVA;
    unsigned char code[7], free_bytes[EXPANSIONS_CAVE_SIZE];
    int32_t displacement;
    if (InterlockedCompareExchange(&expansions_done, 1, 0) != 0) {
        return;
    }
    /* lea r8, [rip + disp32] to the game's reset statement, and an unused cave */
    if (!memory_read(lea, code, sizeof(code)) || code[0] != 0x4c || code[1] != 0x8d || code[2] != 0x05) {
        log_append("EXPANSIONS: licence pass not as expected; Storm Island stays locked.\r\n");
        return;
    }
    memory_copy(&displacement, code + 3, sizeof(displacement));
    char value = expansions_reset_value();
    if (lea + sizeof(code) + displacement != expansions_game + EXPANSIONS_RESET_SQL_RVA || !value) {
        log_append("EXPANSIONS: licence pass not as expected; Storm Island stays locked.\r\n");
        return;
    }
    if (!memory_read(cave, free_bytes, sizeof(free_bytes))) {
        log_append("EXPANSIONS: no room for the statement; Storm Island stays locked.\r\n");
        return;
    }
    for (size_t i = 0; i < sizeof(free_bytes); ++i) {
        if (free_bytes[i]) {
            log_append("EXPANSIONS: no room for the statement; Storm Island stays locked.\r\n");
            return;
        }
    }
    if (!expansions_write(cave, expansions_unlock_sql, sizeof(expansions_unlock_sql)) ||
        !expansions_write(cave + EXPANSIONS_CAVE_WITH_EO, expansions_unlock_with_eo_sql,
                          sizeof(expansions_unlock_with_eo_sql)) ||
        !expansions_point(value)) {
        log_append("EXPANSIONS: could not write the statement; Storm Island stays locked.\r\n");
        return;
    }
    log_append(value == '1' ? "EXPANSIONS: Storm Island unlocked for this profile; the player's ExtraOptions "
                              "unlocks every offer, which stays so.\r\n"
                            : "EXPANSIONS: Storm Island unlocked for this profile.\r\n");
}

void azimuth_expansions_granted(void) {
    if (expansions_game) {
        expansions_unlock();
    }
}

void azimuth_apply_expansions(unsigned char *game_image) {
    expansions_game = game_image;
    if (azimuth_account_storm_island()) {
        expansions_unlock();
    }
}

/* The first game callback after ExtraOptions ran (runtime.c): the lea follows the byte. */
void azimuth_expansions_after_eo(void) {
    LONG variant = expansions_variant;
    if (!variant || !expansions_game) {
        return;
    }
    char value = expansions_reset_value();
    if (value && (value == '1') != (variant == 2) && expansions_point(value)) {
        log_append(value == '1' ? "EXPANSIONS: the player's ExtraOptions unlocks every offer; Storm Island's "
                                  "statement keeps that.\r\n"
                                : "EXPANSIONS: the game's offers are as shipped; Storm Island's statement "
                                  "marks only it.\r\n");
    }
}
