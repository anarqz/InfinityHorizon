/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md.

   The constants every module shares: the hook's identity, its file names and the game build it
   is made for. Only names and numbers; nothing here comes from the game's files. */
#ifndef FI_INFINITY_BASE_H
#define FI_INFINITY_BASE_H
#define WIN32_LEAN_AND_MEAN
#include <stddef.h>
#include <stdint.h>
#include <windows.h>

/* The banner is "ForzaInfinity <MOD_VERSION> - Azimuth TLS adapter". MOD_VERSION keeps the
   MAJOR.MINOR.PATCH+fh2online.N shape: the self-update of older hooks and the launcher read
   the first "+fh2online.<digits>" of this file and compare only N. */
#define MOD_NAME "ForzaInfinity"
#define MOD_VERSION "0.3.0+fh2online.97"
#define MOD_WIDE_TEXT(text) L##text
#define MOD_WIDE(text) MOD_WIDE_TEXT(text)
#define MOD_VERSION_WIDE MOD_WIDE(MOD_VERSION)
#define MOD_BANNER MOD_NAME " " MOD_VERSION " - Azimuth TLS adapter"

/* Our files. Infinity.cfg sits next to azimuth.exe; everything else in data\bin, the folder of
   this DLL. None of these names is ExtraOptions' (its FH2_ExtraOptions.ini/.log live next to
   azimuth.exe and are never read or written here). */
#define FI_CFG_NAME L"Infinity.cfg"
#define MOD_LOG_NAME L"infinity.log"
#define MOD_PREVIOUS_LOG_NAME L"infinity.previous.log"
#define FI_NETWORK_LOG_NAME L"infinity-network.jsonl"
/* data\bin\FH2_ExtraOptions.ini: the settings file of hooks up to 0.2.74+fh2online.58. Read once,
   by the migration into Infinity.cfg [hook] (cfg.c), and never written. */
#define FI_LEGACY_INI_NAME L"FH2_ExtraOptions.ini"

#define GAME_EXE_NAME L"forza_durango_release_final.exe"
#define PATH_CAPACITY 32768
/* The supported game build, as Azimuth maps it (PE header TimeDateStamp and SizeOfImage). */
#define GAME_IMAGE_SIZE 0x3a18000u
#define GAME_TIMESTAMP 0x565501edu
#define ARRAY_COUNT(array) (sizeof(array) / sizeof((array)[0]))
#endif
