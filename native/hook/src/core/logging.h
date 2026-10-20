/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md. */
#ifndef FI_LOGGING_H
#define FI_LOGGING_H
#include "infinity_base.h"
void log_reset(void);
void log_set_directory(const wchar_t *directory);
/* [hook] Log=0: nothing more is written to infinity.log this run (log_start still keeps the last
   run's log as infinity.previous.log). */
void log_set_enabled(int enabled);
void log_append(const char *message);
void log_append_value(const char *label, int value);
/* This run's infinity.log in `directory`: the last one becomes infinity.previous.log, then the
   banner (MOD_BANNER) and the status line (status_get) start the new one. */
void log_start(const wchar_t *directory);
#endif
