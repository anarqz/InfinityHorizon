/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The interface between the hook's modules (src/infinity). Function names keep the azimuth_
   prefix of the Beta line (0.2.74+fh2online.N), whose code this is; the new modules use fi_. */
#ifndef FI_INFINITY_H
#define FI_INFINITY_H
#include "../core/azimuth_host.h"
#include "../core/infinity_base.h"

void fi_mod_host_start(void *image);

int fi_launch_relay_network_hint(const unsigned char *payload,size_t size);
int fi_launch_udp_diagnostic(const unsigned char *payload,size_t size);
int azimuth_proxy_relay_candidates(const unsigned char *payload,size_t size);

/* cfg.c: <Azimuth>\Infinity.cfg. [hook] is the hook's (PLAN.md "Infinity.cfg [hook]"), read
   once at start (and written once, when the section is missing, from the older hooks'
   data\bin\FH2_ExtraOptions.ini); [relay] is the launcher's, read for HOST WORLD. */
enum { FI_NET_OFF, FI_NET_CAPTURE, FI_NET_PROXY };
enum { FI_CARSTATE_AUTO, FI_CARSTATE_OFF };
enum { FI_LANG_AUTO, FI_LANG_EN, FI_LANG_PT_BR };
enum { FI_CFG_NONE, FI_CFG_UTF16LE, FI_CFG_UTF8, FI_CFG_UTF8_BOM, FI_CFG_ANSI };
typedef struct {
    int net_mode;             /* NetMode: FI_NET_* (default Proxy) */
    wchar_t server[256];      /* Server (default https://infinity.anarqz.dev) */
    int auto_update;          /* AutoUpdate 0/1 (1) */
    int freeroam;             /* FreeRoam 0/1 (1) */
    int freeroam_direct;      /* FreeRoamDirect 0/1 (0) */
    wchar_t freeroam_world[128]; /* FreeRoamWorld host:port ("") */
    int carstate;             /* CarState: FI_CARSTATE_* (auto) */
    int language;             /* Language: FI_LANG_* (auto) */
    int log;                  /* Log 0/1 (1) */
    int relay_legacy;         /* RelayCommand: 0 auto, 1 legacy */
    int encoding;             /* FI_CFG_*: how Infinity.cfg is written (FI_CFG_NONE: no file) */
    int has_hook_section;     /* the file has [hook] now */
    int launch;               /* FI_LAUNCH_*: started by the launcher (FI_LAUNCH set) */
    unsigned char launch_nonce[16]; /* FI_LAUNCH_PROTOCOL: the nonce of v1;pipe= (never logged) */
} FiConfig;
/* PROTOCOL: v1;pipe=<nonce>, online through launch.c; every other value plays offline. */
enum { FI_LAUNCH_NONE, FI_LAUNCH_OFFLINE, FI_LAUNCH_PROTOCOL, FI_LAUNCH_MALFORMED, FI_LAUNCH_FILE };
typedef struct {
    char name[100], message[400], visibility[16], password[100]; /* UTF-8 */
    unsigned max_players, port;
    int races;
} FiRelayConfig;
void fi_cfg_load(const wchar_t *root, const wchar_t *bin);
const FiConfig *fi_cfg(void);
const wchar_t *fi_cfg_path(void);
/* 1 when this start moved the older hooks' data\bin\FH2_ExtraOptions.ini keys into [hook]
   (the first start after 0.2.74+fh2online.58, whose DLL had ExtraOptions' features inside). */
int fi_cfg_migrated(void);
/* Writes the CFG: lines of fi_cfg_load to infinity.log (once it is open). */
void fi_cfg_flush_log(void);
/* Infinity.cfg [relay] with the launcher's defaults and limits: 1 when the file has [relay]. */
int fi_cfg_relay(FiRelayConfig *out);

void azimuth_apply_livery_compatibility(void);
void fi_game_ready_apply(unsigned char *game_image);
int fi_game_ready(unsigned *generation);
void azimuth_apply_network(const wchar_t *directory);
void azimuth_apply_economy_probe(unsigned char *game_image);
/* events.c: entering an event in free roam online goes back to free roam (the relay does
   not run races yet). */
void azimuth_apply_events(unsigned char *game_image);
void azimuth_apply_no_game_clips(unsigned char *game_image);

/* net.c: the configured server, for the account module. */
const wchar_t *azimuth_net_server_name(void);
int azimuth_net_proxy_enabled(void);
DWORD azimuth_net_call(const wchar_t *method, const wchar_t *path, const wchar_t *extra_header,
                       const char *body, char *out, size_t capacity);
DWORD azimuth_net_request(const wchar_t *method, const wchar_t *path, const wchar_t *headers,
                          const wchar_t *content_type, const void *body, size_t body_size, void *out,
                          size_t capacity, size_t *received);
DWORD azimuth_net_call_quick(const wchar_t *method, const wchar_t *path, const wchar_t *extra_header,
                             const char *body, char *out, size_t capacity, int timeout_ms);
/* GET https://<host><path> into `out` (update.c reads the release from GitHub). */
DWORD azimuth_net_fetch(const wchar_t *host, const wchar_t *path, void *out, size_t capacity, size_t *received);
/* [Network] AutoUpdate (default 1). */
int azimuth_net_auto_update(void);
/* cloud.c: the online profile's save, kept on the server. */
void azimuth_cloud_start(const wchar_t *bin, const wchar_t *xuid, const wchar_t *token, const wchar_t *ini,
                         const wchar_t *section);
/* 1 uploaded, 2 the server keeps no save for this profile (it is free: 403 "why":"free"), 0 not
   uploaded (`why`). */
/* At DLL_PROCESS_ATTACH (before the player's ExtraOptions runs): the save revisions that have no
   "commit" file now (left torn by a crash, or added by hand). They are never uploaded this
   session, and the online profile's are put aside (saves\fh2online-backup) before the game
   opens its save: ExtraOptions' DisableAzimuthSaveChecksums=1 seals such a revision as it is. */
void azimuth_cloud_note_unsealed(const wchar_t *bin);
/* The same putting aside for a Supporter's online profile played while the server cannot be
   reached at start (`xuid` decimal). */
void azimuth_cloud_put_aside(const wchar_t *bin, const wchar_t *xuid);
int azimuth_cloud_upload_offline(const wchar_t *bin, const wchar_t *token, const wchar_t *ini, const wchar_t *section,
                                 const char **why);

/* launch.c: the launcher's session (FI_LAUNCH=v1;pipe=<nonce>, ONLINE_OFFLINE.md section 3). */
int fi_launch_protocol(void);
/* Starts the launch thread (account.c, instead of the Beta account thread): 1 when it runs. */
int fi_launch_start(const wchar_t *bin);
/* The game asked who plays before the handshake ended: 1 when the session is now offline for
   good, 0 when it went online in that same moment. */
int fi_launch_late(void);
/* The ticket checked out (the session went online, perhaps ended since). */
int fi_launch_online(void);
/* A live session: online, not ended by the game plane, the token not expired. */
int fi_launch_live(void);
/* CR-L32: terminal online quarantine; local identity/save ownership stays unchanged. */
int fi_launch_suspended(void);
/* Also denies UDP work at the token deadline, before the control thread's next turn. */
int fi_launch_network_allowed(void);
/* "X-Fi-Session: <token>\r\nX-Fi-Hook: <MOD_VERSION>" into `out`: its length, 0 without a live
   session. */
size_t fi_launch_headers(wchar_t *out, size_t cap);
/* The game plane answered `status` to a request sent with the session: 1 when the game gets the
   local offline 503 instead (401/403, CR-S4); a first 401 asks the launcher for a token. */
int fi_launch_refused(DWORD status);
/* Frames for the launcher, written by the launch thread: Event{kind, detail} (1 a save revision
   was written) and Capture{what, ok} (1 DXGI queue and swapchain, 4 the overlay's load). Nothing
   without a launcher session. */
void fi_launch_event(unsigned kind, const char *detail);
/* fi_launch_event kinds (the launcher's HookEvents): 3 the game joined a free roam world (detail
   "host:<port>" when it is this PC's relay, "join" otherwise), 4 it left free roam, 6 a notice for
   the player (detail "<level>|<text>", level 0 info, 1 warning, 2 error; the text already in the
   game's language, cut at 250 bytes on a UTF-8 boundary): the launcher shows it as an overlay toast
   (OD-73, instead of the pause menu's lines). */
#define FI_EVENT_FREE_ROAM_ENTERED 3u
#define FI_EVENT_FREE_ROAM_LEFT 4u
#define FI_EVENT_NOTICE 6u
#define FI_NOTICE_INFO 0u
#define FI_NOTICE_WARNING 1u
#define FI_NOTICE_ERROR 2u
void fi_launch_notice(unsigned level, const char *utf8);

/* livery.c: the current car's paint to the server before a world (OD-65 amendment); the overlay
   calls it through FiOverlayHost.livery_sync. Codes as include/fi_overlay.h FI_LIVERY_*. */
#define FI_LIVERY_SYNCED 0
#define FI_LIVERY_STOCK 1
#define FI_LIVERY_TIMEOUT (-1)
#define FI_LIVERY_FAILED (-2)
#define FI_LIVERY_UNAVAILABLE (-3)
int fi_livery_sync(unsigned timeout_ms);

/* livery_world.c (OD-66, MATCHMAKING_API.md 5a): world mates' paints through the world's relay.
   net.c routes the game's content-versioning calls here in a launcher session (1: answered,
   `*reply` freed with fi_livery_free); freeroam.c says when a world is joined and left; proxy.c
   hands the relay's livery messages on. */
unsigned long long fi_livery_content_id(unsigned car, const unsigned char *data, size_t size);
int fi_livery_route(const wchar_t *headers, const unsigned char *body, size_t size, unsigned char **reply,
                    size_t *reply_size);
void fi_livery_free(unsigned char *reply);
void azimuth_livery_world_entered(const void *remote);
void azimuth_livery_world_left(void);
void azimuth_livery_received(const unsigned char *packet, int size);
int azimuth_world_lan_send(const void *data, int size);
/* the current paint's world id before a world: the game image (runtime.c), the game thread's
   per-frame turn (game_ready.c's main update), the overlay's ask (livery.c fi_livery_sync) */
void fi_livery_apply(unsigned char *game_image);
/* Remote-paint policy v1: mode 0 (stock_only) only. The queue boundary must be installed
   before a launcher session may create/join a world; Capture.what 5 reports this readiness. */
int fi_livery_remote_ready(void);
int fi_livery_remote_policy(unsigned mode);
void fi_livery_game_tick(void);
int fi_livery_run_current(unsigned timeout_ms);
/* OD-54: the launcher's relay port while this PC hosts in a launcher session (0: not hosting). */
void azimuth_world_launcher_host(unsigned port);
void fi_launch_capture(unsigned what, int ok);
/* RelaunchRequest{reason}, then ExitProcess(0x46490001): the launcher starts the game again
   (never returns). */
void fi_launch_relaunch(unsigned reason);
/* account.c: the handshake is over (the identity getters stop waiting). */
void azimuth_account_launch_done(void);
/* account.c: the identity of the launcher's ticket (xuid decimal, gamertag UTF-8). */
void azimuth_account_launch_identity(const char *xuid, const char *gamertag, int free_tier, int noob, int storm_island);
/* The header line(s) that authenticate the hook's requests: X-Fi-Session and X-Fi-Hook in a
   launcher session, else the Beta X-Fh2-Token. Its length, 0 when there is none. */
size_t azimuth_account_auth_header(wchar_t *out, size_t cap);
/* net.c: the launcher ticket's server (https://host[:port]; http://127.0.0.1:port in a
   development build): 1 when it parses. */
int azimuth_net_set_launch_server(const char *url);

/* account.c: the player's FH2Online account on that server. */
void azimuth_apply_account(const wchar_t *directory);
const wchar_t *azimuth_account_token(void);
/* The same once the profile has been read (waits for it at the start, up to 30 s). */
const wchar_t *azimuth_account_token_wait(void);
/* progress.c: a profile sync (OnlineProfile.UpdateAll) beyond what play earns since the
   last one sent is held back (1); the totals it reads and the limit check, for the tests. */
int azimuth_progress_hold(const wchar_t *headers, const void *body, size_t size);
int azimuth_progress_read(const unsigned char *body, size_t size, unsigned out[4]);
int azimuth_progress_over(const unsigned base[4], const unsigned now[4], double hours);
/* The XUID this session plays as online (0 offline). */
unsigned long long azimuth_account_xuid_value(void);
/* FIUP proof material is private to authenticated control/proxy memory. */
unsigned long long fi_launch_server_unix(void);
void fi_udp_zero(void *memory, size_t size);
unsigned long long fi_udp_u64(const unsigned char *p);
void fi_udp_context_clear(void);
int fi_udp_context_set(const unsigned char *p, size_t n);
int fi_udp_context_get(unsigned char *out, unsigned long *serial);
int fi_udp_random(unsigned char *out, size_t n);
int fi_udp_valid(const unsigned char *p, size_t n);
int fi_udp_sign(unsigned char *p, size_t n, const unsigned char *key);
int fi_udp_verify(const unsigned char *p, size_t n, const unsigned char *key);
int azimuth_proxy_proof_mode(void);
/* proxy.c (F3): the joining player's UDP proxy between the game and a world on another
   PC (STUN, the punch exchange, the source filter). start returns 1 with `local` (a 28-byte
   sockaddr_in6) when the game should send there instead of `world`. */
void azimuth_proxy_configure(int direct);
int azimuth_proxy_start(const void *world, void *local);
void azimuth_proxy_stop(void);
/* Sends to the world from the proxy's socket (the address the relay knows this player by); 0
   while there is no world (or no proxy). */
int azimuth_proxy_send_world(const void *data, int size);
/* carstate.c: the ExtraOptions headlights (0 the game's, 1 forced on, 2 forced off) and
   roof (0 closed, 1 open) of the players' cars, through the world's relay; only while the
   player's ExtraOptions 0.2.98 is active (fi_eo_sync_enabled). carstate_hook.c: the car update
   and animation event detours that read and show them. */
void fi_carstate_apply(unsigned char *game_image);
void azimuth_carstate_local(int lights, int roof);
void azimuth_carstate_received(const unsigned char *data, int size);
int azimuth_carstate_any(void);
/* Whether azimuth_carstate_wanted needs `car`'s position now (else it is passed NULL). */
int azimuth_carstate_needs_position(void *car);
/* What car `car` (at `position`, NULL if unknown) should show, when it is a player's: 1 and the
   state, else 0. */
int azimuth_carstate_wanted(void *car, const float position[3], int *lights, int *roof);
/* TEST CONNECTION's free roam part (about 1.5 s: STUN from a socket of its own). */
void azimuth_proxy_nat_test(wchar_t *out, size_t cap);
/* world.c: dedicated_server.net while this PC hosts a world (1), else 0. */
int azimuth_world_net_text(char *out, size_t cap);
/* The legacy or launcher-ticket gamertag used for local identity replies. */
const wchar_t *azimuth_account_gamertag(void);
/* The active profile is Noob (its own save, no progress gates), from the server's word. */
int azimuth_account_noob(void);
void azimuth_account_force_notice(const char *english);
/* Legacy Beta cloud stop: no more online saves until restart. That mode has no overlay
   connection. Launcher save failures are handled by the launcher; no game pause is forced. */
void azimuth_cloud_stop(const char *why);
int azimuth_cloud_stopped(void);
/* Translated Event 6 toast when connected to the launcher. Legacy Beta callers have no
   overlay connection and their notice is not displayed. */
void azimuth_account_set_notice(const char *english);
/* lang.c: Infinity's texts in Portuguese (Brazil) when the game runs in it, else in
   English. azimuth_tr / azimuth_tr8 look a text up by its English. The game's language is read
   from its string-table manager in `game_image` whenever a text is needed. */
void azimuth_lang_apply(unsigned char *game_image);
/* Infinity.cfg [hook] Language (FI_LANG_*): en / pt-BR fix it, auto follows the game. */
void azimuth_lang_force(int language);
int azimuth_lang_portuguese(void);
const wchar_t *azimuth_tr(const wchar_t *english);
const char *azimuth_tr8(const char *english);
/* update.c: the self-update, offered the server's latestHook / latestRelease. */
enum { UPDATE_NONE, UPDATE_AVAILABLE, UPDATE_DOWNLOADING, UPDATE_INSTALLED };
void azimuth_update_offer(const char *latest, const char *release);

/* net.c: an HSTRING made by Azimuth's WindowsCreateString. */
HRESULT azimuth_net_make_hstring(const wchar_t *text, void **out);
/* freeroam.c: free roam online ([FreeRoam] Enabled=1). The filter may wrap a WinRT
   class's statics that Azimuth answered; it returns 1 if it did. */
void azimuth_apply_freeroam(unsigned char *game_image, const wchar_t *directory, const wchar_t *executable);
int azimuth_freeroam_filter(const wchar_t *name, const void *iid, void **out);
/* Cuts the game's traffic to the world it is in (the car database changed). */
void azimuth_freeroam_cut(void);
void azimuth_freeroam_suspend(void);
/* world.c: the legacy relay lifecycle and current launcher host-port bridge. */
void azimuth_world_set_directory(const wchar_t *directory);
int azimuth_world_hosting(void);
/* The relay's UDP port while this PC hosts (server.cfg Port), else 0. */
unsigned azimuth_world_port(void);
const wchar_t *azimuth_world_config_path(void);
void azimuth_world_left(void);
/* The SHA-256 of the game's car database (X-Fh2-CarDb), hashed on a thread at start. */
void azimuth_world_hash_car_db(const wchar_t *executable);
const wchar_t *azimuth_world_car_db_header(void);
/* Waits while the car database is still being hashed (at most `ms`). */
void azimuth_world_car_db_wait(unsigned ms);
/* Observe the game's legacy private free-roam request. */
void azimuth_world_on_request(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size);
/* expansions.c: Storm Island for profiles that have it (the server's /me lists it). */
void azimuth_apply_expansions(unsigned char *game_image);
void azimuth_expansions_granted(void);
/* The game's own statement may change when the player's ExtraOptions patches it (after our
   DllMain): called once from the first game callback (fi_game_callback). */
void azimuth_expansions_after_eo(void);
/* account.c: whether the online profile has Storm Island (the last reply, kept in the
   accounts file). */
int azimuth_account_storm_island(void);

/* tier.c: the account's tier, as the server says it (/me "tier"; none, as the Beta server
   answers, is vip). A free account hosts and joins free worlds and has nothing else of the
   universe: fi_tier_free_route says what its game requests get. */
enum { FI_TIER_VIP, FI_TIER_FREE };
enum { FI_ROUTE_PASS, FI_ROUTE_UNAVAILABLE, FI_ROUTE_EMPTY_XTSW, FI_ROUTE_JSON };
void fi_tier_set(int tier);
int fi_tier(void);
int fi_tier_free(void);
/* OD-52: the profile is a Supporter's Hardcore one, the only kind with data on the servers; a
   free or Noob profile's game requests get the free answers. */
void fi_tier_set_noob(int noob);
int fi_tier_universe(void);
/* For a free account: FI_ROUTE_PASS (to the server), FI_ROUTE_UNAVAILABLE (503 here),
   FI_ROUTE_EMPTY_XTSW (200, an empty bin/xtsw reply here) or FI_ROUTE_JSON (200, `*json` here);
   `what` gets "Class.Method" or the host, for the log. */
int fi_tier_free_route(const wchar_t *method, const wchar_t *url, const wchar_t *headers, const char **json,
                       wchar_t *what, size_t what_capacity);
/* Foreign XUIDs (owner via the Mac, 2026-10-19, GAME_PLATFORM §3a): in a launcher session only
   the chosen profile (`own`) talks to the server. A URL naming another XUID (xuid(N), xuid=N):
   free → userpresence GET answered Offline here, anything else 503 here; Supporter → reads sent,
   writes 503 here. A free ticket's profile.xboxlive.com batch naming anyone but `own` (the
   save's drivatars) is answered here with generated gamertags (`own_tag` for the player).
   FI_ROUTE_JSON fills `json` (capacity `json_cap`). */
int fi_tier_foreign_route(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size,
                          unsigned long long own, const wchar_t *own_tag, int free_tier, char *json, size_t json_cap);
void fi_tier_generated_gamertag(unsigned long long xuid, char *out, size_t cap);

/* eo_detect.c: the player's ExtraOptions in this process (looked at once, from the first game
   callback, when ExtraOptions has run): absent, 0.2.98 active or not, another version, or a
   winmm.dll that is not ExtraOptions. */
enum { FI_EO_ABSENT = 1, FI_EO_0298_OK, FI_EO_0298_INACTIVE, FI_EO_OTHER_VERSION, FI_EO_FOREIGN_WINMM };
typedef struct {
    int state;          /* FI_EO_* */
    char version[24];   /* "0.2.98", "0.2.88", "(no version)" */
    char status[160];   /* its Fh2TweaksStatus text (0.2.98 only) */
    int on_disk;        /* <Azimuth>\winmm.dll exists (whether or not it is loaded) */
} FiEo;
void fi_eo_detect(void);
/* Waits, at most `timeout_ms`, while the player's ExtraOptions is still "Waiting" for Azimuth's
   TLS helper (it patches the game when our DllMain returns); at once without ExtraOptions. */
void fi_eo_settle(unsigned timeout_ms);
/* 0 until fi_eo_detect ran. */
const FiEo *fi_eo(void);
/* Headlight and roof sync: ExtraOptions 0.2.98 active, and [hook] CarState is not off. */
int fi_eo_sync_enabled(void);
/* The "EO: ..." line of infinity.log. */
const char *fi_eo_line(void);
/* The same for the player (English, for azimuth_tr), with the version appended by the caller
   when `*version_follows` (another version). */
const wchar_t *fi_eo_summary(int *version_follows);

/* runtime.c: called by every game callback of ours (the car update): the
   first call, which comes after the player's ExtraOptions (if any) has patched the game, runs
   what needs that (ExtraOptions' state, the expansions statement). */
void fi_game_callback(void);
/* The same from the game's first identity or network request (`where` is logged), and from the
   account thread when its start round trip is over (it waits for ExtraOptions to have left its
   "Waiting" state first): as a fallback before any car updates. */
void fi_game_callback_from(const char *where);
void fi_game_callback_late(void);

#endif
