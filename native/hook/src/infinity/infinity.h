/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The interface between the hook's modules (src/infinity). Function names keep the azimuth_
   prefix of the Beta line (0.2.74+fh2online.N), whose code this is; the new modules use fi_. */
#ifndef FI_INFINITY_H
#define FI_INFINITY_H
#include "../core/azimuth_host.h"
#include "../core/infinity_base.h"

/* W-13 (watchdog.c): the hang watchdog. The launcher pipe thread beats with each 1 Hz Capture
   lease and disarms at its end; a beat missing 3 s dumps every thread's stack and the hook's locks
   to infinity.log. Started with the launcher session (fi_launch_start). */
int fi_watchdog_start(void);
void fi_watchdog_beat(void);
void fi_watchdog_disarm(void);
/* The hook's SRW locks, watched (owner, shared count, last waiter) for the dump. Built with
   FI_LOCK_WATCH (build.py, the DLL only), every Acquire/Release in src/infinity goes through
   these; unit tests that compile one module keep the plain Windows calls. */
void fi_lock_exclusive(SRWLOCK *lock, const char *name);
void fi_unlock_exclusive(SRWLOCK *lock);
void fi_lock_shared(SRWLOCK *lock, const char *name);
void fi_unlock_shared(SRWLOCK *lock);
#ifdef FI_LOCK_WATCH
#define AcquireSRWLockExclusive(lock) fi_lock_exclusive((lock), #lock)
#define ReleaseSRWLockExclusive(lock) fi_unlock_exclusive(lock)
#define AcquireSRWLockShared(lock) fi_lock_shared((lock), #lock)
#define ReleaseSRWLockShared(lock) fi_unlock_shared(lock)
#endif

void fi_mod_host_start(void *image);
void fi_mod_host_wait(unsigned ms);

int fi_launch_relay_network_hint(const unsigned char *payload,size_t size);
int fi_launch_udp_diagnostic(const unsigned char *payload,size_t size);
int azimuth_proxy_relay_candidates(const unsigned char *payload,size_t size);
/* CR-L41.3 Discord presence (launch.c): Progress (0x21) after an UpdateAll the hook lets through,
   Car (0x22) from the readiness sample's current car key (current+0x40, `now_ms` GetTickCount64);
   each sent only in a live session and only when changed (Car also held 1 s and at most one per
   5 s; a key <= 0 never). */
int fi_launch_progress(unsigned level, unsigned long long credits);
int fi_launch_car(int car_key, unsigned long long now_ms);

/* cfg.c: <Azimuth>\Infinity.cfg. [hook] is the hook's (PLAN.md "Infinity.cfg [hook]"), read
   once at start (and written once, when the section is missing, from the older hooks'
   data\bin\FH2_ExtraOptions.ini). */
enum { FI_NET_OFF, FI_NET_CAPTURE, FI_NET_PROXY };
enum { FI_CARSTATE_AUTO, FI_CARSTATE_OFF };
enum { FI_LANG_AUTO, FI_LANG_EN, FI_LANG_PT_BR };
enum { FI_CFG_NONE, FI_CFG_UTF16LE, FI_CFG_UTF8, FI_CFG_UTF8_BOM, FI_CFG_ANSI };
typedef struct {
    int net_mode;             /* NetMode: FI_NET_* (default Proxy) */
    wchar_t server[256];      /* Server (default https://api.infinity-horizon.com) */
    int auto_update;          /* AutoUpdate 0/1 (1) */
    int freeroam;             /* FreeRoam 0/1 (1) */
    wchar_t freeroam_world[128]; /* FreeRoamWorld host:port ("") */
    int carstate;             /* CarState: FI_CARSTATE_* (auto) */
    int language;             /* Language: FI_LANG_* (auto) */
    int log;                  /* Log 0/1 (1) */
    int encoding;             /* FI_CFG_*: how Infinity.cfg is written (FI_CFG_NONE: no file) */
    int has_hook_section;     /* the file has [hook] now */
    int launch;               /* FI_LAUNCH_*: started by the launcher (FI_LAUNCH set) */
    unsigned char launch_nonce[16]; /* FI_LAUNCH_PROTOCOL: the nonce of v1;pipe= (never logged) */
} FiConfig;
/* PROTOCOL: v1;pipe=<nonce>, online through launch.c; every other value plays offline. */
enum { FI_LAUNCH_NONE, FI_LAUNCH_OFFLINE, FI_LAUNCH_PROTOCOL, FI_LAUNCH_MALFORMED, FI_LAUNCH_FILE };
void fi_cfg_load(const wchar_t *root, const wchar_t *bin);
const FiConfig *fi_cfg(void);
const wchar_t *fi_cfg_path(void);
/* 1 when this start moved the older hooks' data\bin\FH2_ExtraOptions.ini keys into [hook]
   (the first start after 0.2.74+fh2online.58, whose DLL had ExtraOptions' features inside). */
int fi_cfg_migrated(void);
/* Writes the CFG: lines of fi_cfg_load to infinity.log (once it is open). */
void fi_cfg_flush_log(void);

void azimuth_apply_livery_compatibility(void);
void fi_game_ready_apply(unsigned char *game_image);
int fi_game_ready(unsigned *generation);
/* OD-206 W9: a polled game's readiness (game_patches.h), published as the detoured one is: `first` and
   `second` are the identity whose change starts a new generation. */
void fi_game_ready_polled(int ready, void *first, void *second);
/* OD-239 8 (game_patches.h): an Azimuth session's (`azimuth`, the ticket's mode noob) save rows, before the game reads its
   identity; nothing for Hardcore. */
void fi_game_azimuth_save(int azimuth);
/* race_conn.c (T38b): the lobby's race connectivity (0 Undefined, 1 Online), read only. check: the
   build's bytes as expected; apply (from fi_game_ready_apply); read: the value, 0 when unreadable;
   tick (the main update's sample): a change is one bounded line; teardown: the value at the
   association's DestroyAsync, one bounded line. */
int fi_race_conn_check(const unsigned char *image);
void fi_race_conn_apply(unsigned char *image);
int fi_race_conn_read(int *value);
void fi_race_conn_tick(void);
void fi_race_conn_teardown(void);
void azimuth_apply_network(const wchar_t *directory);
void azimuth_apply_economy_probe(unsigned char *game_image);
/* events.c: an event entered in free roam online runs when the world's relay grants it; it goes
   back to free roam when the relay denies it, or does not answer within EV_ANSWER_WAIT_MS (races
   off, or an older relay). */
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
/* The same putting aside for an online profile played while the server cannot be
   reached at start (`xuid` decimal). */
void azimuth_cloud_put_aside(const wchar_t *bin, const wchar_t *xuid);
int azimuth_cloud_upload_offline(const wchar_t *bin, const wchar_t *token, const wchar_t *ini, const wchar_t *section,
                                 const char **why);

/* launch.c: the launcher's session (FI_LAUNCH=v1;pipe=<nonce>, ONLINE_OFFLINE.md section 3). */
int fi_launch_protocol(void);
/* Starts the launch thread (account.c, instead of the Beta account thread): 1 when it runs. */
int fi_launch_start(const wchar_t *bin);
/* launch.c (OD-118): the overlay named for the launcher's Play offline (FI_OFFLINE_OVERLAY*); 1 when loading. */
int fi_launch_offline_overlay(void);
/* offline_identity.c (OD-118): the launcher's Play offline profile, FI_OFFLINE_XUID/FI_OFFLINE_GAMERTAG; 1 when valid. */
int fi_offline_identity(const wchar_t *xuid, const wchar_t *gamertag, unsigned long long *value);
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
   "join"; OD-208: never "host:<port>", this PC hosts no world), 4 it left free roam, 6 a notice for
   the player (detail "<level>|<text>", level 0 info, 1 warning, 2 error; the text already in the
   game's language, cut at 250 bytes on a UTF-8 boundary): the launcher shows it as an overlay toast
   (OD-73, instead of the pause menu's lines). */
#define FI_EVENT_FREE_ROAM_ENTERED 3u
#define FI_EVENT_FREE_ROAM_LEFT 4u
#define FI_EVENT_NOTICE 6u
/* 7 (OD-121): the server's X-Fi-Activity-State for this session (detail "active|menu|idle|no_signal"
   with ";evict_in=N" while eviction is enforced): the launcher warns through the overlay. */
#define FI_EVENT_ACTIVITY 7u
/* OD-208: the game left the online world on its own (detail "cars": the car database changed); the launcher leaves the seat */
#define FI_EVENT_WORLD_LEFT 8u
/* OD-211 (CR-S102): the game's rich-presence screen id changed (detail its decimal id, 0..255; activity.c): the launcher's online
   toasts are not shown on a race screen */
#define FI_EVENT_SCREEN 9u
/* OD-224 (T-1108-L): the game's lobby kind changed (detail its decimal value, 0..255, 0 none; game_profile.h
   FI_WATCH_EVENT_LOBBY_KIND rows, FM5 only): the launcher ends a private lobby's seat when the game leaves it */
#define FI_EVENT_LOBBY_KIND 10u
/* the decimal detail of Event 9 into `out` (at least 4 bytes, NUL-terminated); its length */
size_t fi_activity_screen_detail(unsigned screen, char *out);
#define FI_NOTICE_INFO 0u
#define FI_NOTICE_WARNING 1u
#define FI_NOTICE_ERROR 2u
void fi_launch_notice(unsigned level, const char *utf8);

/* livery.c: FiOverlayHost.livery_sync (the OD-65 pre-world upload, retired by CR-S59) answers
   FI_LIVERY_UNAVAILABLE at once: the server keeps the liveries. Codes as include/fi_overlay.h. */
#define FI_LIVERY_UNAVAILABLE (-3)
int fi_livery_sync(unsigned timeout_ms);

/* livery_world.c (CR-S59): net.c notes the game's livery calls here in a launcher session (1: a
   livery call, logged; nothing is answered here, every one goes to the server). */
int fi_livery_route(const wchar_t *headers, const unsigned char *body, size_t size);
void fi_livery_reply(const wchar_t *headers, const unsigned char *request_body, size_t request_size,
                     unsigned status, const unsigned char *reply, size_t reply_size);
/* The world's own LAN socket for carstate.c: freeroam.c says when a world is joined and left. */
void azimuth_world_lan_entered(const void *remote);
void azimuth_world_lan_left(void);
int azimuth_world_lan_send(const void *data, int size);
/* The remote-paint receiver boundary, installed on the game image (runtime.c). Policy v1: mode 0
   only, a pass-through since CR-S59; Capture.what 5 reports this readiness. */
void fi_livery_apply(unsigned char *game_image);
/* livery_share.c (T19, OD-138): a share preview over 190000 bytes scaled down to an RGBA PNG by the
   overlay's fi_overlay_fit_png (game_stream.c), or the share goes without one; the Create stub's
   refusals logged. Install: bit 0 the preview detour, bit 1 the stub's. */
int fi_livery_share_install(unsigned char *game_image);
void fi_livery_share_apply(unsigned char *game_image);
int fi_livery_share_original_fits(unsigned size);
int fi_livery_remote_ready(void);
int fi_livery_remote_policy(unsigned mode);
void fi_launch_capture(unsigned what, int ok);
/* CR-L40: FREE ROAM ONLINE's matchmaking request (POST momatch .../hoppers/FreeRoam_qos) first
   asks the launcher for a lobby and waits up to 20 s on the calling net worker (`aborted`: the
   request's abort flag). The launcher's result (0 ready, 1 none, 2 not online), -1 when nothing
   was asked or nothing came; the request is forwarded either way. */
int fi_launch_free_roam_request(const wchar_t *method, const wchar_t *url);
int fi_launch_free_roam(const wchar_t *method, const wchar_t *url, volatile LONG *aborted);
/* T34 (OD-182): the launcher's seat for a lobby kind (0 free roam, 1 car meet, 2 Road Trip) as its
   last handshake left it: 1 a world is ready, 0 none, -1 not asked yet, unanswered or no live
   launcher session, FI_SEAT_CANCELLED after a CancelMatchmaking (until the next search's handshake).
   fi_launch_seat_ask asks again on the calling net worker (the launcher joins the first open lobby
   or opens one) and returns the result as fi_launch_free_roam. */
#define FI_SEAT_CANCELLED (-2)
int fi_launch_seat(unsigned kind);
int fi_launch_seat_ask(unsigned kind, volatile LONG *aborted);
/* W-10: 0 only when the launcher says no lobby is pending (launch.c); 1 otherwise, or unknown. */
int fi_launch_lobby_pending(void);
/* T-L6 (OD-204, WORLDS_OFFICIAL 12.2): the game's private session create (a sessiondirectory PUT, launch.c's predicate) waits
   on the calling net worker until the launcher holds its private official lobby (FreeRoamNeed kind 3, only once the launcher
   answered Capture{12, 1}). 0: forward the request (not a private create, an older launcher, or Ready); 1: do not forward it,
   answer NET_OFFLINE_ERROR (refused, none, no answer within the wait, or no live session). fi_launch_private_kind: the
   predicate alone (3, or 0). */
int fi_launch_private_create(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size,
                             volatile LONG *aborted);
unsigned fi_launch_private_kind(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size);
/* T-L4 (OD-204, WORLDS_OFFICIAL 12.5): the game's Invite friends (SystemUI slot 9). 1: the launcher serves private lobbies
   (Capture{12, 1}): the slot completes locally and InviteFriends (0x29) asks the launcher for its friend picker. 0: forward the
   slot to Azimuth as before (an older launcher, or no private lobbies). */
int fi_launch_invite_friends(void);
/* W7b (OD-206, CR-S99 4, lobby spec 11.3-11.4, route 1; freeroam.c): fire the GameSessionReady delegate the game added at
   IPartyStatics slot 16 with an args object whose slot 6 hands out a MultiplayerSessionReference of exactly this session (sender
   NULL). Only for a profile with party_session_ready. Returns a session_ready.h FI_SESSION_READY_* result; `hresult` gets the
   failing HRESULT (S_OK otherwise). Every step is logged, without a XUID or a gamertag. */
unsigned fi_party_session_ready(const char *scid, const char *template_name, const char *name, unsigned *hresult);
/* W-10 (freeroam.c): the Party calls still watched (async, Started), logged with `why`; and
   follow.c's game-thread check after a CancelMatchmaking. */
void azimuth_freeroam_pending_log(const char *why);
/* ticket_guard.c (T34, OD-182): a "found" matchmaking ticket answer (GET momatch .../hoppers/
   {FreeRoam,CarMeet,RoadTrip}_qos/tickets/{id}) reaches the game only while the launcher holds a
   seat; otherwise the launcher is asked again and the answer says "searching" (the game polls it
   again; also once after a successful ask) or, after FI_TICKET_ASKS asks in one search or with
   online closed, "expired" (the game's own end of a search); after a CancelMatchmaking, "canceled".
   fi_ticket_guard_search: a new search (the hopper's POST) starts the asks again. The status the
   answer should carry instead, 0 to pass it through. Then the rewrite of
   the body (fi_ticket_rewrite: its new size, 0 when it does not fit). A target the game then leaves
   (a session directory PUT of members.me null on it before any association) is logged loudly:
   fi_ticket_guard_request on each forwarded request, fi_ticket_guard_associated from
   CreateAssociationAsync. */
#define FI_TICKET_ASKS 3u
int fi_ticket_hopper(const wchar_t *method, const wchar_t *url);
int fi_ticket_found(const unsigned char *body, size_t size, size_t *value_at);
size_t fi_ticket_target(const unsigned char *body, size_t size, char *name, size_t capacity);
const char *fi_ticket_guard_check(unsigned kind, const wchar_t *url, const unsigned char *body, size_t size,
                                  volatile LONG *aborted);
void fi_ticket_guard_search(unsigned kind);
size_t fi_ticket_rewrite(const unsigned char *body, size_t size, size_t value_at, const char *status, unsigned char *out,
                         size_t capacity);
void fi_ticket_guard_request(const wchar_t *method, const wchar_t *url, const unsigned char *body, size_t size);
void fi_ticket_guard_associated(void);
/* N-H2: the native mod runtime's snapshot as a ModsReport (0x1b) for the launcher; the session
   writer sends it once it changed. 0 outside a launcher session. */
struct InfinityModsSnapshotV2;
int fi_launch_mods_report(const struct InfinityModsSnapshotV2 *snapshot);
size_t fi_launch_mods_encode(unsigned char *out, size_t cap, const struct InfinityModsSnapshotV2 *snapshot);
void fi_launch_mods_flush(unsigned ms);
/* An optional export of the overlay the launcher had loaded (0 without one). */
void *fi_launch_overlay_export(const char *name);
/* OD-112: the game's PLAYER DETAILS opens the overlay's player pane instead of the system
   profile card (gamer_card.c; one vtable slot, guarded by the game build and its bytes). */
void fi_gamer_card_apply(unsigned char *game_image);
/* OD-154 VC-2b: the game's own player list and HUD show the overlay's voice chat (voice_ui.c;
   always on since OD-144 VC-T9, guarded by the game build and its bytes). */
void fi_voice_ui_apply(unsigned char *game_image);
/* OD-193 F-2: the game's PLAYER ACTIONS offers ADD / REMOVE / ACCEPT / CANCEL FRIEND and REPORT PLAYER through the
   overlay's fi_overlay_player_* exports (player_actions.c; guarded by the game build, its bytes and slots). */
void fi_player_actions_apply(unsigned char *game_image);
/* voice_net.c (OD-154 VC-2): FiOverlayHost.voice_send, and the voice kinds (0x3E93, 0x3E94) the
   world's relay sends to this hook's sockets (proxy.c, carstate.c, livery_world.c): 1 when d is
   one of them (passed to the overlay's fi_overlay_voice if well formed), 0 for any other kind. */
int fi_voice_send(const uint8_t *datagram, uint32_t size);
int fi_voice_received(const unsigned char *d, int n);
int fi_voice_shape(const unsigned char *d, size_t n, unsigned kind, unsigned attribute);
/* the sockets voice uses: proxy.c and livery_world.c (LAN); OD-208: never a host's */
int azimuth_proxy_world_ready(void);
int azimuth_world_lan_ready(void);
/* activity.c (OD-121): the odometer, input idle and screen wrappers (launcher sessions only). */
void fi_activity_apply(unsigned char *game_image);
/* "X-Fi-Activity: v1.<seq>.<odo_m>.<idle_s>.<rp>" for a RetrieveForUser request (no CRLF): its
   length, 0 when nothing is sent. */
size_t fi_activity_header(const wchar_t *request_headers, wchar_t *out, size_t cap);
/* The response's X-Fi-Activity-State, if any: a new state goes to the launcher (Event 7). */
void fi_activity_answer(const wchar_t *response_headers);
/* RelaunchRequest{reason}, then ExitProcess(0x46490001): the launcher starts the game again
   (never returns). */
void fi_launch_relaunch(unsigned reason);
/* account.c: the handshake is over (the identity getters stop waiting). */
void azimuth_account_launch_done(void);
/* account.c: the identity of the launcher's ticket (xuid decimal, gamertag UTF-8; tier FI_TIER_*,
   display only). */
void azimuth_account_launch_identity(const char *xuid, const char *gamertag, int tier, int noob, int storm_island);
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
   PC (STUN, the FIUP proof, the source filter; a launcher session only, OD-208). start returns 1 with `local` (a 28-byte
   sockaddr_in6) when the game should send there instead of `world`. */
int azimuth_proxy_start(const void *world, void *local);
void azimuth_proxy_stop(void);
/* Sends to the world from the proxy's socket (the address the relay knows this player by); 0
   while there is no world (or no proxy). */
int azimuth_proxy_send_world(const void *data, int size);
/* carstate.c: the ExtraOptions headlights (0 the game's, 1 forced on, 2 forced off) and
   roof (0 closed, 1 open) of the players' cars, through the world's relay; only while the
   supported ExtraOptions (FI_EO_NAME) is active (fi_eo_sync_enabled). carstate_hook.c: the car update
   and animation event detours that read and show them. */
void fi_carstate_apply(unsigned char *game_image);
/* carmeet.c: car meet invitation joins and the online car meet entries (free roam on). */
void fi_carmeet_apply(unsigned char *game_image);
/* carmeet.c: the CarMeet_qos POST's ticketAttributes (identifying keys left out) in one log line. */
void fi_carmeet_ticket_log(const unsigned char *body, size_t size);
/* net_observe.c: matchmaking and session directory answers in the log (observation only). The
   family of a URL by its host (momatch.* or sessiondirectory.*, ignoring case); whether an open
   line of that family is still within its own budget (GET and non-GET apart; a repeat of a polled
   request collapses into a count; past the budget, a count line at each power of two); one bounded
   answer line (a few safe fields of a JSON body, or the HRESULT when nothing answered), or a count
   when it equals the last answer of the same method and URL. */
enum { FI_NETOBS_NONE, FI_NETOBS_MOMATCH, FI_NETOBS_SESSIONDIRECTORY, FI_NETOBS_FAMILIES };
int fi_netobs_family(const wchar_t *url, size_t length);
int fi_netobs_open(int family, long id, const wchar_t *method, size_t method_length, const wchar_t *url,
                   size_t url_length);
void fi_netobs_answer(int family, long id, const wchar_t *method, size_t method_length, const wchar_t *url,
                      size_t url_length, int answered, unsigned status, HRESULT error, const unsigned char *body,
                      size_t size);
void fi_netobs_answer_text(char *out, size_t capacity, long id, unsigned status, const unsigned char *body,
                           size_t size);
/* A session document's members line ("NETWORK: session #<id> host=... gsp=... members: ..."; no
   identity, tokens as lengths); 0 when the body is not a session. Logged by fi_netobs_answer for
   sessiondirectory answers when it changed for the URL. */
int fi_netobs_session_text(char *out, size_t capacity, long id, const unsigned char *body, size_t size);
/* follow.c (OD-151): the game starts its own matchmaking for the launcher's FollowHost move
   (the player confirms with (X)). Applied with car meets (free roam on). */
void fi_follow_apply(unsigned char *game_image);
void fi_follow_host(unsigned lobby, unsigned is_public, int move);
/* The launcher's CancelMatchmaking (0x24): it gave up on its seat; the game's own matchmaking
   disconnect is sent on the module's next update (follow.c). */
void fi_follow_cancel(void);
/* The joiner's proxy: each datagram the game sends its world (an OpenConnection's lobby type is
   what the game is in; follow.c skips a start of it). fi_follow_left: an association destroyed; the
   game left its session unless it sent an OpenConnection in the last 3 s (the new connection's). */
void fi_follow_datagram(const unsigned char *data, size_t size);
void fi_follow_left(void);
/* watch.c (AC-2): in a Hardcore world the joiner measures the other players' cars against the
   launcher's rules (Watch, 0x25: fi_watch_rules, 0 when it does not parse; version 0 clears) and
   reports each ended episode (Observed, 0x26: fi_launch_peer_observation, 1 queued; `per_car`, NR-5:
   layout 2 with the held speed and the car ordinal, 0xFFFFFFFF unknown). The proxy
   passes each datagram the world sends the game (fi_watch_datagram) and the world's watch roster
   (fi_watch_roster, STUN kind 0x3E95 attribute 0xC0F7: peer index -> XUID). fi_watch_stop: the session ended. */
int fi_watch_rules(const unsigned char *payload, size_t size);
void fi_watch_datagram(const unsigned char *data, size_t size);
void fi_watch_roster(const unsigned char *data, int size);
void fi_watch_stop(void);
int fi_launch_peer_observation(unsigned long long xuid, unsigned kind, unsigned value, unsigned sustain_ms, unsigned rules_version,
                               unsigned peer_index, int per_car, unsigned held, unsigned car);
/* NR-6: another player's model and 43 part rows read from its relayed player object (PlayerParts, 0x27;
   flags bit 0 the model, bit 1 the parts, bit 2 a new object); 1 queued. */
int fi_launch_player_parts(unsigned long long xuid, unsigned peer_index, unsigned flags, unsigned model, const unsigned *parts);
void azimuth_carstate_local(int lights, int roof);
void azimuth_carstate_received(const unsigned char *data, int size);
int azimuth_carstate_any(void);
/* Whether azimuth_carstate_wanted needs `car`'s position now (else it is passed NULL). */
int azimuth_carstate_needs_position(void *car);
/* What car `car` (at `position`, NULL if unknown) should show, when it is a player's: 1 and the
   state, else 0. */
int azimuth_carstate_wanted(void *car, const float position[3], int *lights, int *roof);
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
/* The main update's tick (game_ready.c): logs and releases observed async calls that settled.
   fi_game_ready_ticking (game_ready.c) is 1 once that tick runs; without it nothing is watched. */
void azimuth_freeroam_observe_tick(void);
int fi_game_ready_ticking(void);
/* Cuts the game's traffic to the world it is in (the car database changed). */
void azimuth_freeroam_cut(void);
void azimuth_freeroam_suspend(void);
/* cardb.c: the SHA-256 of the game's car database (X-Fh2-CarDb), hashed on a thread at start (OD-208: players host no world). */
void azimuth_world_hash_car_db(const wchar_t *executable);
const wchar_t *azimuth_world_car_db_header(void);
/* Waits while the car database is still being hashed (at most `ms`). */
void azimuth_world_car_db_wait(unsigned ms);
/* expansions.c: Storm Island for profiles that have it (the server's /me lists it). */
void azimuth_apply_expansions(unsigned char *game_image);
void azimuth_expansions_granted(void);
/* The game's own statement may change when the player's ExtraOptions patches it (after our
   DllMain): called once from the first game callback (fi_game_callback). */
void azimuth_expansions_after_eo(void);
/* account.c: whether the online profile has Storm Island (the last reply, kept in the
   accounts file). */
int azimuth_account_storm_island(void);

/* tier.c: the cloud line gate (OD-128) and the account's tier, display text only (OD-140: the
   "Infinity Racer" / "Infinity Supporter" badge; nothing routes or gates by it). */
enum { FI_TIER_VIP, FI_TIER_FREE };
enum { FI_ROUTE_PASS, FI_ROUTE_UNAVAILABLE, FI_ROUTE_EMPTY_XTSW, FI_ROUTE_JSON };
void fi_tier_set(int tier);
int fi_tier(void);
/* OD-128: in a launcher session the ticket's save line (CR-S51 `line`: 1 or 0) alone opens or shuts
   the universe's services, whatever tier and mode; -1 (no ticket, the Beta protocol): open (OD-140). */
void fi_tier_set_line(int line);
int fi_tier_universe(void);
/* Without a cloud line: FI_ROUTE_PASS (to the server), FI_ROUTE_UNAVAILABLE (503 here),
   FI_ROUTE_EMPTY_XTSW (200, an empty bin/xtsw reply here) or FI_ROUTE_JSON (200, `*json` here);
   `what` gets "Class.Method" or the host, for the log. */
int fi_tier_no_line_route(const wchar_t *url, const wchar_t *headers, const char **json, wchar_t *what,
                          size_t what_capacity);
/* Foreign XUIDs (owner via the Mac, 2026-10-19, GAME_PLATFORM §3a): in a launcher session only
   the chosen profile (`own`) talks to the server. A URL naming another XUID (xuid(N), xuid=N):
   no cloud line (`no_line`) → userpresence GET sent (a friend's presence, OD-193), anything else 503 here;
   with a line → reads sent, writes 503 here. FI_ROUTE_JSON fills `json` (capacity `json_cap`). */
int fi_tier_foreign_route(const wchar_t *method, const wchar_t *url, unsigned long long own, int no_line, char *json,
                          size_t json_cap);

/* The ExtraOptions build headlight and roof sync supports: the player's ExtraOptions 0.2.98 (the
   TLS-helper hook on Azimuth 0.9.10), or, in ExtraOptions.Infinity (FI_EO_BUILD, OD-125/OD-127,
   Azimuth 1.0.4), the ExtraOptions 0.6.0 linked into this same winmm.dll: its version resource
   (FILEVERSION 0.6.0.0) and its Fh2TweaksStatus are this module's own. FI_EO_NAME is for
   infinity.log, FI_EO_SHOWN for the player (azimuth_tr's tables are keyed by it, lang.c);
   FI_EO_W makes a wide literal of either. */
#ifdef FI_EO_BUILD
#define FI_EO_SUPPORTED_MS 0x00000006u
#define FI_EO_SUPPORTED_LS 0x00000000u
#define FI_EO_SUPPORTED "0.6.0"
#define FI_EO_NAME "ExtraOptions.Infinity (ExtraOptions 0.6.0)"
#define FI_EO_SHOWN "ExtraOptions.Infinity (ExtraOptions v0.6.0)"
#else
#define FI_EO_SUPPORTED_MS 0x00000002u
#define FI_EO_SUPPORTED_LS 0x00620000u
#define FI_EO_SUPPORTED "0.2.98"
#define FI_EO_NAME "ExtraOptions 0.2.98"
#define FI_EO_SHOWN "ExtraOptions v0.2.98"
#endif
#define FI_EO_W_(text) L##text
#define FI_EO_W(text) FI_EO_W_(text)

/* eo_detect.c: the ExtraOptions in this process (looked at once, from the first game callback,
   when ExtraOptions has run): absent, the supported build (FI_EO_SUPPORTED) active or not,
   another version, or a winmm.dll that is not ExtraOptions. The FI_EO_0298_* names are kept from
   the first supported build; they mean FI_EO_SUPPORTED in either build. */
enum { FI_EO_ABSENT = 1, FI_EO_0298_OK, FI_EO_0298_INACTIVE, FI_EO_OTHER_VERSION, FI_EO_FOREIGN_WINMM };
typedef struct {
    int state;          /* FI_EO_* */
    char version[24];   /* FI_EO_SUPPORTED, another ("0.2.88"), or "(no version)" */
    char status[160];   /* its Fh2TweaksStatus text (the supported build only) */
    int on_disk;        /* <Azimuth>\winmm.dll exists (whether or not it is loaded) */
} FiEo;
void fi_eo_detect(void);
/* Waits, at most `timeout_ms`, while the player's ExtraOptions is still "Waiting" for Azimuth's
   TLS helper (it patches the game when our DllMain returns); at once without ExtraOptions. */
void fi_eo_settle(unsigned timeout_ms);
/* 0 until fi_eo_detect ran. */
const FiEo *fi_eo(void);
/* Headlight and roof sync: the supported ExtraOptions (FI_EO_NAME) active, and [hook] CarState
   is not off. */
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
