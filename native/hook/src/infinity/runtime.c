/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   What the hook does at DLL_PROCESS_ATTACH, once azimuth_tls.c found the mapped game image
   (TimeDateStamp and SizeOfImage of the supported build). There is no patch table and no
   ExtraOptions code here any more: every module checks the bytes of its own sites before it
   writes them, and none of those sites is one of the player's ExtraOptions 0.2.98
   (tests/eo098-sites.tsv, tools/collision_check.py). With ExtraOptions present this runs before
   it patches the game; without it, the same steps run the same way. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/runtime.h"
#include "../core/support.h"

static volatile LONG fi_game_called;

/* What needs the player's ExtraOptions to have run: its state, the expansions statement it may
   have changed, and where its features went for a player who came from the hook that had them
   inside. Once, from the first of the triggers below. */
static void fi_game_first(const char *where) {
    if (where) {
        log_append("EO: looked at from ");
        log_append(where);
        log_append(" (before any car update callback).\r\n");
    }
    fi_eo_detect();
    azimuth_expansions_after_eo();
    /* the first start after the self-update from a hook that had ExtraOptions inside, and no
       ExtraOptions of the player's own: say where those features went (an overlay notice).
       ExtraOptions v0.2.98 runs only on Azimuth 0.9.10 (it refuses any other azimuth.exe), so on
       an older Azimuth the player is told to update that first. */
    const FiEo *eo = fi_eo();
    if (fi_cfg_migrated() && eo && eo->state == FI_EO_ABSENT) {
        if (azimuth_host_build((const unsigned char *)GetModuleHandleW(0)) == AZIMUTH_0910) {
            azimuth_account_set_notice("ExtraOptions features are now separate. Install ExtraOptions v0.2.98, then reinstall the Infinity hook from the launcher.");
        } else {
            azimuth_account_set_notice("ExtraOptions features are now separate. Update Azimuth to 0.9.10, install ExtraOptions v0.2.98, then reinstall the Infinity hook from the launcher.");
        }
    }
}

/* The car update (a game thread, after ExtraOptions). */
void fi_game_callback(void) {
    if (fi_game_called || InterlockedCompareExchange(&fi_game_called, 1, 0)) {
        return;
    }
    fi_game_first(0);
}

/* The first identity or network request can arrive before a car exists. */
void fi_game_callback_from(const char *where) {
    if (fi_game_called || InterlockedCompareExchange(&fi_game_called, 1, 0)) {
        return;
    }
    fi_game_first(where);
}

/* The account thread, once the start's /me round trip is over (and the game no longer waits
   for it): if no game callback came yet, after ExtraOptions (if any) has left its "Waiting"
   state. */
void fi_game_callback_late(void) {
    if (fi_game_called) {
        return;
    }
    fi_eo_settle(10000);
    fi_game_callback_from("the account thread");
}

int fi_runtime_initialize(unsigned char *game_image, const wchar_t *executable_path, const wchar_t *bin,
                          const wchar_t *root) {
    log_reset();
    fi_cfg_load(root, bin);
    /* before the player's ExtraOptions runs (it repairs save commit markers right after this
       DllMain when its DisableAzimuthSaveChecksums=1): which save revisions have no commit now */
    azimuth_cloud_note_unsealed(bin);
    log_set_enabled(fi_cfg()->log);
    azimuth_lang_force(fi_cfg()->language);
    status_set("OK: ForzaInfinity hook initialized.");
    log_start(bin);
    log_set_directory(bin);
    fi_cfg_flush_log();
    azimuth_apply_livery_compatibility();
    azimuth_apply_network(bin);
    /* the car database's SHA-256 goes with every request (the server only lets the game's own
       cars play online) */
    if (azimuth_net_proxy_enabled()) {
        azimuth_world_hash_car_db(executable_path);
    }
    azimuth_apply_freeroam(game_image, bin, executable_path);
    azimuth_apply_account(bin);
    /* OD-73: no game page, menu or text is edited any more; the language is only read */
    azimuth_lang_apply(game_image);
    azimuth_apply_expansions(game_image);
    azimuth_apply_events(game_image);
    azimuth_apply_economy_probe(game_image);
    azimuth_apply_no_game_clips(game_image);
    fi_carstate_apply(game_image);
    fi_livery_apply(game_image);
    fi_livery_share_apply(game_image); /* T19: the share's preview (OD-138) */
    fi_game_ready_apply(game_image);
    fi_gamer_card_apply(game_image); /* OD-112: PLAYER DETAILS opens the overlay's player pane */
    fi_voice_ui_apply(game_image);   /* OD-147 VC-2b: voice chat in the game's UI (VoiceUi=1 only) */
    fi_activity_apply(game_image);   /* OD-121: the activity report on RetrieveForUser */
    fi_mod_host_start(game_image);
    return 1;
}
