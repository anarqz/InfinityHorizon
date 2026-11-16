//! Route paths of the account plane (ACCOUNTS.md §1, §5; SAVES_API.md §3-§4; PORTAL_API.md §1, §3).
//!
//! The server's router mounts exactly these strings, so a path in the contract and the path served
//! cannot drift (CR-L3: the constants moved here when the router landed, SER-1.1). Path parameters
//! use the router's `{name}` syntax, the same notation as the contract. The launcher keeps its own
//! copies in `fi_launcher_core::wire::routes` until it migrates (LAU-M2).

/// Served by every deployment.
pub mod shared {
    /// The process answers.
    pub const HEALTH_LIVE: &str = "/health/live";
    /// Its dependencies answer (Postgres, the schema at this build's migration).
    pub const HEALTH_READY: &str = "/health/ready";
    /// `{service, version, revision, project, labels}`.
    pub const META: &str = "/meta";
}

/// `/launcher/v1` (ACCOUNTS.md §5, SAVES_API.md §3-§4).
pub mod launcher {
    /// Prefix of every launcher route.
    pub const BASE: &str = "/launcher/v1";
    /// *public* `GET`: `ServerInfo`.
    pub const INFO: &str = "/launcher/v1/info";
    /// *public* `POST`: the Identity Platform exchange (`TokenGrant` → `SignInResult`).
    pub const AUTH_TOKEN: &str = "/launcher/v1/auth/token";
    /// *public* `POST` (`local` instances): create an e-mail and password credential (ACCOUNTS.md §5).
    pub const AUTH_SIGN_UP: &str = "/launcher/v1/auth/sign-up";
    /// *public* `POST` (`local` instances): an ID token for the credential.
    pub const AUTH_SIGN_IN: &str = "/launcher/v1/auth/sign-in";
    /// *public* `POST` (`local` instances): change the credential's password.
    pub const AUTH_PASSWORD: &str = "/launcher/v1/auth/password";
    /// *public* `POST` (`local` instances): send the e-mail confirmation link again (CR-S53).
    pub const AUTH_CONFIRMATION: &str = "/launcher/v1/auth/confirmation";
    /// *public* `POST` (`local` instances): confirm the address with the mailed 6-digit code and a
    /// current ID token; answers a confirmed ID token (CR-S53 follow-up).
    pub const AUTH_CONFIRMATION_CODE: &str = "/launcher/v1/auth/confirmation/code";
    /// *public* `POST` (`local` instances): mail a password link; always 202 (CR-S53).
    pub const AUTH_PASSWORD_RESET: &str = "/launcher/v1/auth/password-reset";
    /// *public* `GET` page / `POST` form (`local` instances): the confirmation link's page (CR-S53).
    pub const AUTH_EMAIL_CONFIRM: &str = "/launcher/v1/auth/email/confirm";
    /// *public* `GET` page / `POST` form (`local` instances): the password link's page (CR-S53).
    pub const AUTH_EMAIL_PASSWORD: &str = "/launcher/v1/auth/email/password";
    /// *public* `POST`: create the account and its first profile from an identity ticket.
    pub const ACCOUNTS: &str = "/launcher/v1/accounts";
    /// *public* `POST`: link an imported Beta account to an identity.
    pub const LEGACY_LINK: &str = "/launcher/v1/legacy/link";
    /// *public* `POST` (body auth): rotate the refresh token.
    pub const TOKEN: &str = "/launcher/v1/token";
    /// `POST`: revoke this refresh family.
    pub const SIGN_OUT: &str = "/launcher/v1/sign-out";
    /// `GET`: the account, profiles, access, devices, notices.
    pub const ME: &str = "/launcher/v1/me";
    /// `POST`: another profile.
    pub const PROFILES: &str = "/launcher/v1/profiles";
    /// `DELETE`: delete the profile's career of `?game=` (default `fh2`) from the cloud at once
    /// (CR-S61, CR-S98). Every `/profiles/{xuid}/*` route takes `?game=` to pick the career.
    pub const PROFILE: &str = "/launcher/v1/profiles/{xuid}";
    /// `POST`: Hardcore → Noob.
    pub const PROFILE_MODE: &str = "/launcher/v1/profiles/{xuid}/mode";
    /// `POST`: the free rename after an OD-120 clash rename (or keep the name).
    pub const PROFILE_NAME: &str = "/launcher/v1/profiles/{xuid}/name";
    /// `POST`: whether the profile appears on the play dashboard (CR-S83, OD-168 follow-up DB-Q4).
    pub const PROFILE_WEBSITE: &str = "/launcher/v1/profiles/{xuid}/website";
    /// `GET`: what the launcher's Discord presence shows of the caller's own profile: its club,
    /// level and credits (ACCOUNTS.md §5 "Presence", CR-S72, OD-136).
    pub const PROFILE_PRESENCE: &str = "/launcher/v1/profiles/{xuid}/presence";
    /// `DELETE`: delete the profile above its careers, every career first (CR-S98, CAREERS.md §3).
    pub const PLAYER: &str = "/launcher/v1/players/{xuid}";
    /// `POST`: start the profile's career of another game (CR-S98, CAREERS.md §3).
    pub const PLAYER_CAREERS: &str = "/launcher/v1/players/{xuid}/careers";
    /// `POST` request / `DELETE` cancel the account's deletion.
    pub const ME_DELETION: &str = "/launcher/v1/me/deletion";
    /// `DELETE`: unbind a device.
    pub const ME_DEVICE: &str = "/launcher/v1/me/devices/{deviceId}";
    /// `POST`: a one-time code that signs the browser in to the portal.
    pub const PORTAL_CODES: &str = "/launcher/v1/portal-codes";
    /// `POST`: create (admission) a play session.
    pub const SESSIONS: &str = "/launcher/v1/sessions";
    /// `GET`: poll a queued session.
    pub const SESSION: &str = "/launcher/v1/sessions/{sessionId}";
    /// `POST`: renew the lease and the session token.
    pub const SESSION_HEARTBEAT: &str = "/launcher/v1/sessions/{sessionId}/heartbeat";
    /// `POST`: a fresh session token between heartbeats.
    pub const SESSION_TOKEN: &str = "/launcher/v1/sessions/{sessionId}/token";
    /// `POST`: end the session.
    pub const SESSION_END: &str = "/launcher/v1/sessions/{sessionId}/end";
    /// `POST`: the revision the launcher installed.
    pub const SESSION_INSTALLED: &str = "/launcher/v1/sessions/{sessionId}/installed";
    /// `GET` head / `PUT` commit.
    pub const SAVE: &str = "/launcher/v1/profiles/{xuid}/save";
    /// `GET`: history, newest first.
    pub const SAVE_REVISIONS: &str = "/launcher/v1/profiles/{xuid}/save/revisions";
    /// `GET`: one revision's bundle.
    pub const SAVE_REVISION_BUNDLE: &str =
        "/launcher/v1/profiles/{xuid}/save/revisions/{revision}/bundle";
    /// `POST`: restore an older revision as the new head.
    pub const SAVE_RESTORE: &str = "/launcher/v1/profiles/{xuid}/save/restore";
    /// `POST` park / `GET` list parked bundles.
    pub const SAVE_PARKED: &str = "/launcher/v1/profiles/{xuid}/save/parked";
    /// `GET`: a parked bundle.
    pub const SAVE_PARKED_BUNDLE: &str = "/launcher/v1/profiles/{xuid}/save/parked/{id}/bundle";
    /// `POST`: a Hardcore line's revision 1 from the server's seed (SAVES_API.md §4.8, OD-60).
    pub const SAVE_SEED: &str = "/launcher/v1/profiles/{xuid}/save/seed";
    /// `POST`: the dry-run check of another profile's save as this Hardcore line's start
    /// (SAVES_API.md §4.10, CR-S52); nothing is committed.
    pub const SAVE_CHECK: &str = "/launcher/v1/profiles/{xuid}/save/check";
    /// `POST`: the dry run of a player's `upload` on top of `If-Match` (SAVES_API.md §4.11,
    /// CR-S71); nothing is stored, no lease needed.
    pub const SAVE_UPLOAD_CHECK: &str = "/launcher/v1/profiles/{xuid}/save/upload-check";
    /// `GET`: the game's active Hardcore seed bundle (SAVES_API.md §4.8, OD-60).
    pub const GAME_SEED_HARDCORE: &str = "/launcher/v1/games/{game}/seeds/hardcore";
    /// `GET` the account's friends and requests / `POST` a request by gamertag (CORE_API.md §4.1,
    /// served by core: the alias of [`super::core::ME_FRIENDS`]).
    pub const FRIENDS: &str = "/launcher/v1/friends";
    /// `DELETE`: unfriend, cancel or decline, whatever links the two accounts.
    pub const FRIEND: &str = "/launcher/v1/friends/{friendId}";
    /// `POST`: accept `friendId`'s request.
    pub const FRIEND_ACCEPT: &str = "/launcher/v1/friends/{friendId}/accept";
    /// `POST`: decline `friendId`'s request.
    pub const FRIEND_DECLINE: &str = "/launcher/v1/friends/{friendId}/decline";
    /// `PUT`: set or unset the caller's favorite mark.
    pub const FRIEND_FAVORITE: &str = "/launcher/v1/friends/{friendId}/favorite";
    /// `GET`: the account's pending invites to play and requests to join its world (CORE_API.md
    /// §4.2, served by core; the launcher polls it about every 10 s).
    pub const INVITES: &str = "/launcher/v1/invites";
    /// `POST`: decline an invite or a request.
    pub const INVITE_DECLINE: &str = "/launcher/v1/invites/{inviteId}/decline";
    /// `POST`: report a player from the game (CORE_API.md §4.3, OD-63, OD-193; served by core).
    pub const REPORTS: &str = "/launcher/v1/reports";
    /// `POST`: the overlay's voice mute or unmute of a player (CORE_API.md §4.4, CR-S68; served
    /// by core).
    pub const VOICE_MUTES: &str = "/launcher/v1/voice/mutes";
}

/// `/portal/v1` (PORTAL_API.md §1, §3): the portal server's server-to-server API.
pub mod portal {
    /// Prefix of every portal route.
    pub const BASE: &str = "/portal/v1";
    /// `GET`: `{status: "ok"}` when the surface can serve.
    pub const HEALTH: &str = "/portal/v1/health";
    /// `POST`: the Identity Platform exchange (`PortalSignIn` → `PortalSignInResult`).
    pub const SESSIONS: &str = "/portal/v1/sessions";
    /// `POST`: redeem a launcher portal code.
    pub const SESSIONS_LAUNCHER: &str = "/portal/v1/sessions/launcher";
    /// `DELETE`: sign this browser out.
    pub const SESSIONS_CURRENT: &str = "/portal/v1/sessions/current";
    /// `POST`: an account without profiles, from an identity ticket.
    pub const ACCOUNTS: &str = "/portal/v1/accounts";
    /// `POST`: link an imported Beta account (gamertag and site key).
    pub const LEGACY_LINK: &str = "/portal/v1/legacy/link";
    /// `GET`: the `Viewer`.
    pub const ME: &str = "/portal/v1/me";
    /// `GET` list / `DELETE ?others=true` the account's token families.
    pub const ME_SESSIONS: &str = "/portal/v1/me/sessions";
    /// `DELETE`: one token family.
    pub const ME_SESSION: &str = "/portal/v1/me/sessions/{id}";
    /// `POST` request / `DELETE` cancel the account's deletion.
    pub const ME_DELETION: &str = "/portal/v1/me/deletion";
    /// `POST` start / `GET` status of the data export.
    pub const ME_EXPORT: &str = "/portal/v1/me/export";
}

/// `/game/v1` (GAME_PLATFORM.md §2): the platform surface of a pool's game plane, behind the
/// session token (`X-Fi-Session`); every answer carries `X-Fi-Pool` and `X-Fi-Instance`.
pub mod game {
    /// Prefix of every platform route of the game plane.
    pub const BASE: &str = "/game/v1";
    /// `GET`: the session as the token and Postgres see it.
    pub const SESSION: &str = "/game/v1/session";
    /// `POST`: a progression fact, idempotent by `X-Fh2-Request-Id`.
    pub const PROGRESSION: &str = "/game/v1/progression";
    /// `POST`: presence of the session on this pool's instance.
    pub const PRESENCE: &str = "/game/v1/presence";
}

/// `/core/v1` (CORE_API.md): the public, account, social and admin surface of `services/core`
/// (platform v1, ADR 0013). The identity routes stay on [`launcher`] and the web's on [`portal`].
pub mod core {
    /// Prefix of every core route.
    pub const BASE: &str = "/core/v1";
    /// `GET` (public, cached): health summary, maintenance flag, gold stats, service map.
    pub const STATUS: &str = "/core/v1/status";
    /// `GET` (public, cached): the server summary of one game (`?game=fh2`; nightly + online now).
    pub const SUMMARY: &str = "/core/v1/summary";
    /// Global latched online policy (ONLINE_CONTROL.md).
    pub const ONLINE_STATUS: &str = "/core/v1/online-status";
    /// `GET` (public, cached): the launcher notice board for one game (`?game=fh2`).
    pub const NOTICES: &str = "/core/v1/notices";
    /// `GET`: the caller's notices (strikes, queue, maintenance, deletion) across games.
    pub const ME_NOTICES: &str = "/core/v1/me/notices";
    /// `GET`: the caller's strikes for one game.
    pub const ME_STRIKES: &str = "/core/v1/me/strikes";
    /// `GET` (session): support links.
    pub const ME_SUPPORT: &str = "/core/v1/me/support";
    /// `GET` (session): the Discord link and each profile's announce switch (CR-S63).
    pub const ME_DISCORD: &str = "/core/v1/me/discord";
    /// `POST` a one-time link code / `DELETE` the link (session; CR-S63).
    pub const ME_DISCORD_LINK: &str = "/core/v1/me/discord/link";
    /// `PUT` (session): a profile's announce switch (CR-S63).
    pub const ME_PROFILE_ANNOUNCE: &str = "/core/v1/me/profiles/{xuid}/announce";
    /// `GET` (bot key): the public event feed (CR-S63).
    pub const FEED: &str = "/core/v1/feed";
    /// `POST` (bot key): what a link code would link (CR-S63).
    pub const DISCORD_LINK_PREVIEW: &str = "/core/v1/discord/link-preview";
    /// `PUT` (bot key): spend a link code (CR-S63).
    pub const DISCORD_LINKS: &str = "/core/v1/discord/links";
    /// `PUT` (bot key): refresh a linked user's handle (CR-S63).
    pub const DISCORD_USER: &str = "/core/v1/discord/users/{discordUserId}";
    /// `GET` list / `POST` create of a public player's basic card (social hub).
    pub const SOCIAL_PLAYERS: &str = "/core/v1/social/players";
    /// `GET`: one player's card (`{xuid}`), full profile for Supporters.
    pub const SOCIAL_PLAYER: &str = "/core/v1/social/players/{xuid}";
    /// `GET` list / `POST` write of comments on a target (`?target=...`); writes are challenged.
    pub const SOCIAL_COMMENTS: &str = "/core/v1/social/comments";
    /// `DELETE` (author or MOD): one comment.
    pub const SOCIAL_COMMENT: &str = "/core/v1/social/comments/{id}";
    /// `POST`: a report (challenged).
    pub const SOCIAL_REPORTS: &str = "/core/v1/social/reports";
    /// `GET` the account's friends and requests / `POST` a request by gamertag (session +
    /// Supporter account, CORE_API.md §4.1).
    pub const ME_FRIENDS: &str = "/core/v1/me/friends";
    /// `DELETE`: unfriend, cancel or decline, whatever links the two accounts.
    pub const ME_FRIEND: &str = "/core/v1/me/friends/{friendId}";
    /// `POST`: accept `friendId`'s request.
    pub const ME_FRIEND_ACCEPT: &str = "/core/v1/me/friends/{friendId}/accept";
    /// `POST`: decline `friendId`'s request.
    pub const ME_FRIEND_DECLINE: &str = "/core/v1/me/friends/{friendId}/decline";
    /// `PUT`: set or unset the caller's favorite mark.
    pub const ME_FRIEND_FAVORITE: &str = "/core/v1/me/friends/{friendId}/favorite";
    /// `GET` / `PUT` (ADM): the maintenance switch of a game.
    pub const ADMIN_MAINTENANCE: &str = "/core/v1/admin/maintenance";
    /// `GET`/`PUT` a game's online capacity (OD-71).
    pub const ADMIN_CAPACITY: &str = "/core/v1/admin/capacity";
    /// `GET` list / `POST` create (ADM): notice board entries.
    pub const ADMIN_NOTICES: &str = "/core/v1/admin/notices";
    /// `DELETE` (ADM): one notice.
    pub const ADMIN_NOTICE: &str = "/core/v1/admin/notices/{id}";
    /// `GET` (MOD; e-mail ADM): account lookup by gamertag, XUID or e-mail.
    pub const ADMIN_ACCOUNTS: &str = "/core/v1/admin/accounts";
    /// `GET` (MOD, ADM): a player's moderation view by account id.
    pub const ADMIN_ACCOUNT: &str = "/core/v1/admin/accounts/{accountId}";
    /// `POST` (MOD, ADM): a strike on a profile.
    pub const ADMIN_STRIKES: &str = "/core/v1/admin/strikes";
    /// `POST` (MOD, ADM): review one strike (`uphold` | `overturn`).
    pub const ADMIN_STRIKE_REVIEW: &str = "/core/v1/admin/strikes/{id}/review";
    /// `POST` (ADM): ban an account (revokes sessions and tickets).
    pub const ADMIN_BANS: &str = "/core/v1/admin/bans";
    /// `DELETE` (ADM): lift a ban.
    pub const ADMIN_BAN: &str = "/core/v1/admin/bans/{accountId}";
    /// `POST` (MOD, ADM): kick an account's live play sessions, no ban (CR-S62).
    pub const ADMIN_KICKS: &str = "/core/v1/admin/kicks";
    /// `POST` (MOD, ADM): ban an account from voice chat (CR-S68).
    pub const ADMIN_VOICE_BANS: &str = "/core/v1/admin/voice-bans";
    /// `DELETE` (MOD, ADM): lift a voice ban and a community mute (CR-S68).
    pub const ADMIN_VOICE_BAN: &str = "/core/v1/admin/voice-bans/{accountId}";
    /// `DELETE` (MOD, ADM): lift a Hardcore profile's cheat flag (MATCHMAKING_API.md §3.9).
    pub const ADMIN_CHEAT_FLAG: &str = "/core/v1/admin/cheat-flags/{profileId}";
    /// `GET` list / `POST` decide (MOD, ADM): open reports.
    pub const ADMIN_REPORTS: &str = "/core/v1/admin/reports";
    /// `POST` (MOD, ADM; `ban` ADM): decide a report.
    pub const ADMIN_REPORT: &str = "/core/v1/admin/reports/{id}";
    /// `PUT` (ADM): an account's role.
    pub const ADMIN_ROLE: &str = "/core/v1/admin/accounts/{accountId}/role";
    /// `POST` (ADM): the forced 14-day deletion.
    pub const ADMIN_ERASE: &str = "/core/v1/admin/accounts/{accountId}/erase";
    /// `GET` (MOD, ADM): the audit log (`admin_action`), paged.
    pub const ADMIN_AUDIT: &str = "/core/v1/admin/audit";
    /// `GET` (MOD, ADM): the Hardcore guardrails' assessments (OD-62), paged.
    pub const ADMIN_ASSESSMENTS: &str = "/core/v1/admin/assessments";
    /// `GET` paged / `POST` hide or delete all (MOD, ADM): an account's car liveries (CR-S59).
    pub const ADMIN_ACCOUNT_LIVERIES: &str = "/core/v1/admin/accounts/{accountId}/liveries";
    /// `PUT` (MOD, ADM): block or unblock an account's livery uploads (CR-S59).
    pub const ADMIN_LIVERY_UPLOADS: &str = "/core/v1/admin/accounts/{accountId}/livery-uploads";
    /// `PUT` hide or show / `DELETE` (MOD, ADM): one livery (CR-S59).
    pub const ADMIN_LIVERY: &str = "/core/v1/admin/liveries/{id}";
    /// `POST` (ADM): a Hardcore line seeded from another profile is retired and the account
    /// blocked pending review (CR-S52, CORE_API.md §5).
    pub const ADMIN_SEED_REVOKE: &str = "/core/v1/admin/profiles/{profileId}/seed-revoke";
    /// The MCP endpoint (streamable HTTP; ADR 0018), outside `/core/v1`.
    pub const MCP: &str = "/mcp";
    /// OAuth protected-resource metadata of the MCP endpoint (RFC 9728).
    pub const MCP_RESOURCE_METADATA: &str = "/.well-known/oauth-protected-resource/mcp";
    /// OAuth authorization-server metadata of the MCP issuer (RFC 8414).
    pub const MCP_AUTH_METADATA: &str = "/.well-known/oauth-authorization-server/mcp/oauth";
}

/// `/gate/v1` (GATE_API.md): the queue, the game access ticket, heartbeats and leave, served by
/// `services/gate`. The `/launcher/v1/sessions*` routes of [`launcher`] are its aliases until the
/// launcher migrates (register CR-S11).
pub mod gate {
    /// Prefix of every gate route.
    pub const BASE: &str = "/gate/v1";
    /// `POST`: ask for admission to a game (queue or ticket); `GET /{id}` polls.
    pub const SESSIONS: &str = "/gate/v1/sessions";
    /// `GET`: the session's state (queued with position, or active with its ticket).
    pub const SESSION: &str = "/gate/v1/sessions/{sessionId}";
    /// `POST`: the liveness heartbeat; renews the ticket and the slot.
    pub const SESSION_HEARTBEAT: &str = "/gate/v1/sessions/{sessionId}/heartbeat";
    /// `POST`: a fresh ticket between heartbeats.
    pub const SESSION_TICKET: &str = "/gate/v1/sessions/{sessionId}/ticket";
    /// `POST`: manual leave when the game closes (idempotent).
    pub const SESSION_LEAVE: &str = "/gate/v1/sessions/{sessionId}/leave";
    /// `GET` (public, cached): queue lengths and capacity per game and pool.
    pub const QUEUE: &str = "/gate/v1/queue";
    /// `GET` (internal): every pool with its live counts (moves here from `/internal/v1/pools`).
    pub const INTERNAL_POOLS: &str = "/gate/v1/internal/pools";
}

/// `/saves/v1` (UNIVERSE_API.md): the Supporters' universe reads and the in-game progression
/// writes served by `services/saves` behind the game access ticket; the launcher's save line of
/// SAVES_API.md keeps its `/launcher/v1/profiles/{xuid}/save*` routes on this deployable.
pub mod universe {
    /// Prefix of every universe route.
    pub const BASE: &str = "/saves/v1";
    // `leaderboards`, `rivals`, `ghosts` and `ghosts/{id}` were retired on 2026-10-27
    // (UNIVERSE_API.md): the game's boards and ghosts are the Forza RPC's.
    /// `GET`: players near the caller's last known position (`?lat=&lon=&radiusM=`), cached.
    pub const NEARBY: &str = "/saves/v1/nearby";
    /// `POST`: additive progress from the game's own netcode (idempotent by `X-Fh2-Request-Id`).
    pub const PROGRESS: &str = "/saves/v1/progress";
}

/// `/mm/v1` (MATCHMAKING_API.md, CR-L17): the world directory, join grants and punches, served
/// by `services/matchmaking`.
pub mod mm {
    /// Prefix of every matchmaking route.
    pub const BASE: &str = "/mm/v1";
    /// `GET ?filter=`: at most five worlds of the caller's mode (OD-58); `POST`: the settings and
    /// the heartbeat of the host's world in one call.
    pub const WORLDS: &str = "/mm/v1/worlds";
    /// `GET`: one world as the caller sees it; `PUT`: open or change the host's world; `DELETE`:
    /// close it (the host left free roam).
    pub const WORLD: &str = "/mm/v1/worlds/{worldId}";
    /// `GET`: the open world with this code (OD-57).
    pub const WORLD_BY_CODE: &str = "/mm/v1/worlds/by-code/{code}";
    /// `POST` (host, every 15 s): the heartbeat.
    pub const WORLD_ANNOUNCE: &str = "/mm/v1/worlds/{worldId}/announce";
    /// `POST` (host): a new code for a friends or private world.
    pub const WORLD_CODE: &str = "/mm/v1/worlds/{worldId}/code";
    /// `POST` (host, Supporter): invite a friend to play (OD-56).
    pub const WORLD_INVITES: &str = "/mm/v1/worlds/{worldId}/invites";
    /// `POST` (Supporter): ask the world's host to let the caller in (OD-59).
    pub const WORLD_REQUESTS: &str = "/mm/v1/worlds/{worldId}/requests";
    /// `POST` (host): accept a request to join: the asker gets a seat without the code.
    pub const WORLD_REQUEST_ACCEPT: &str = "/mm/v1/worlds/{worldId}/requests/{requestId}/accept";
    /// `POST` (host): kick or ban a peer of the host's world (Supporter hosts).
    pub const WORLD_MODERATION: &str = "/mm/v1/worlds/{worldId}/moderation";
    /// `GET` (the host, or a profile with a live seat in it): the world's roster (OD-63).
    pub const WORLD_MEMBERS: &str = "/mm/v1/worlds/{worldId}/members";
    /// `PUT` (a private official world's current leader): who may join it from now on (§2.11,
    /// CR-S101).
    pub const WORLD_JOIN_MODES: &str = "/mm/v1/worlds/{worldId}/modes";
    /// `POST` (host): a car meet invite's free-roam session (§3.8, S9; other projects used as reference).
    pub const WORLD_GAME_SESSIONS: &str = "/mm/v1/worlds/{worldId}/game-sessions";
    /// `POST` (host or seated, Hardcore worlds): a peer observation (§3.9, OD-147).
    pub const WORLD_OBSERVATIONS: &str = "/mm/v1/worlds/{worldId}/observations";
    /// `GET`: the thresholds a hook measures peers against (`WatchRules`, §3.9).
    pub const WATCH: &str = "/mm/v1/watch";
    /// `POST` (a relay pinned in `FI_RELAYS`, signed; no ticket): the current `WatchRules` (§5d).
    pub const RELAY_WATCH: &str = "/mm/v1/relay/watch";
    /// `POST` (a relay pinned in `FI_RELAYS`, signed; no ticket): a batch of detector reports (§5d).
    pub const RELAY_REPORTS: &str = "/mm/v1/relay/reports";
    /// `POST`: reserve a seat (`granted | full | locked | not_allowed | code | latency`).
    pub const JOIN: &str = "/mm/v1/join";
    /// `DELETE`: leave one seat by its grant id.
    pub const GRANT: &str = "/mm/v1/join/{grantId}";
    /// `GET`: the caller's seat (or hosted world); `DELETE`: leave it.
    pub const SEAT: &str = "/mm/v1/seat";
    /// `POST`: keep a reserved seat (every 20 s, up to its hold).
    pub const SEAT_RENEW: &str = "/mm/v1/seat/renew";
    /// `POST`: private authenticated UDP NAT rendezvous for the caller's exact live grant.
    pub const SEAT_NETWORK: &str = "/mm/v1/seat/network";
    /// `GET` (host): pending punches, grants included.
    pub const PUNCHES: &str = "/mm/v1/punches";
    /// `GET` (public, cached): the free limit's state.
    pub const CAPACITY: &str = "/mm/v1/capacity";
    /// `GET` (a world agent of `FI_WORLDS`, signed `FI-World-Agent`; no ticket): the startup
    /// handshake (§2.9, OD-188/189).
    pub const OFFICIAL_HELLO: &str = "/mm/v1/official/hello";
    /// `PUT` / `DELETE` (world agent): open, change or close one official world.
    pub const OFFICIAL_WORLD: &str = "/mm/v1/official/worlds/{worldId}";
    /// `POST` (world agent, every 15 s): an official world's heartbeat and punches.
    pub const OFFICIAL_WORLD_ANNOUNCE: &str = "/mm/v1/official/worlds/{worldId}/announce";
    /// `POST` (world agent): the engine's kick of a peer.
    pub const OFFICIAL_WORLD_MODERATION: &str = "/mm/v1/official/worlds/{worldId}/moderation";
    /// `POST` (world agent, OD-190): the car meet session of an official `carmeet` world (§2.9).
    pub const OFFICIAL_WORLD_GAME_SESSIONS: &str = "/mm/v1/official/worlds/{worldId}/game-sessions";
}

/// `/internal/v1` (GAME_PLATFORM.md §3): schedulers, operators (OIDC in
/// production; open on the compose network locally).
pub mod internal {
    /// Prefix of every internal route.
    pub const BASE: &str = "/internal/v1";
    /// `GET`: every pool with its live counts (the autoscaler stand-in reads it).
    pub const POOLS: &str = "/internal/v1/pools";
    /// `PUT`: the local entitlement stand-in (only with `FI_LOCAL_ADMIN=true`, never in prod).
    pub const LOCAL_ENTITLEMENTS: &str = "/internal/v1/local/entitlements";
}

/// `/internal/dashboard/v1` (DASHBOARD_API.md, CR-S82): the play dashboard's reads, mounted only on
/// the platform's second listener (`FI_INTERNAL_BIND`), never on the public port.
pub mod dashboard {
    /// Prefix of every dashboard route.
    pub const BASE: &str = "/internal/dashboard/v1";
    /// `GET`: the game's live aggregates.
    pub const LIVE: &str = "/internal/dashboard/v1/live";
    /// `GET`: the listed lobbies of one mode.
    pub const LOBBIES: &str = "/internal/dashboard/v1/lobbies";
    /// `GET`: every board of every family (DASHBOARD_API.md §3 `BoardIndex`).
    pub const BOARDS: &str = "/internal/dashboard/v1/boards";
    /// `GET`: one page of one board (§3 `BoardPage`).
    pub const BOARD: &str = "/internal/dashboard/v1/boards/{family}/{board}";
    /// `GET`: website-listed players of one mode (§4 `PlayerList`).
    pub const PLAYERS: &str = "/internal/dashboard/v1/players";
    /// `GET`: one player's profile, any mode (§4 `PlayerProfile`).
    pub const PLAYER: &str = "/internal/dashboard/v1/players/{name}";
    /// `GET`: a player's Discord avatar, `image/png` (§4).
    pub const MEDIA_AVATAR: &str = "/internal/dashboard/v1/media/avatar/{name}";
    /// `GET`: every profile listed on the website now, by opaque ref (§5.1 `ListedProfiles`).
    pub const LISTED: &str = "/internal/dashboard/v1/listed";
    /// `GET`: the non-private clubs of one mode (§6 `ClubList`).
    pub const CLUBS: &str = "/internal/dashboard/v1/clubs";
    /// `GET`: one non-private club (§6 `ClubPage`).
    pub const CLUB: &str = "/internal/dashboard/v1/clubs/{tag}";
    /// `GET`: a shared creation's preview image (§7), by its opaque id.
    pub const MEDIA_CREATION: &str = "/internal/dashboard/v1/media/creation/{id}";
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCHER: &[&str] = &[
        launcher::INFO,
        launcher::AUTH_TOKEN,
        launcher::AUTH_SIGN_UP,
        launcher::AUTH_SIGN_IN,
        launcher::AUTH_PASSWORD,
        launcher::AUTH_CONFIRMATION,
        launcher::AUTH_CONFIRMATION_CODE,
        launcher::AUTH_PASSWORD_RESET,
        launcher::AUTH_EMAIL_CONFIRM,
        launcher::AUTH_EMAIL_PASSWORD,
        launcher::ACCOUNTS,
        launcher::LEGACY_LINK,
        launcher::TOKEN,
        launcher::SIGN_OUT,
        launcher::ME,
        launcher::PROFILES,
        launcher::PROFILE,
        launcher::PROFILE_MODE,
        launcher::PROFILE_NAME,
        launcher::PROFILE_WEBSITE,
        launcher::PROFILE_PRESENCE,
        launcher::PLAYER,
        launcher::PLAYER_CAREERS,
        launcher::ME_DELETION,
        launcher::ME_DEVICE,
        launcher::PORTAL_CODES,
        launcher::SESSIONS,
        launcher::SESSION,
        launcher::SESSION_HEARTBEAT,
        launcher::SESSION_TOKEN,
        launcher::SESSION_END,
        launcher::SESSION_INSTALLED,
        launcher::SAVE,
        launcher::SAVE_REVISIONS,
        launcher::SAVE_REVISION_BUNDLE,
        launcher::SAVE_RESTORE,
        launcher::SAVE_PARKED,
        launcher::SAVE_PARKED_BUNDLE,
        launcher::SAVE_CHECK,
        launcher::SAVE_UPLOAD_CHECK,
        launcher::FRIENDS,
        launcher::FRIEND,
        launcher::FRIEND_ACCEPT,
        launcher::FRIEND_DECLINE,
        launcher::FRIEND_FAVORITE,
        launcher::INVITES,
        launcher::INVITE_DECLINE,
        launcher::REPORTS,
        launcher::VOICE_MUTES,
    ];
    const MM: &[&str] = &[
        mm::WORLDS,
        mm::WORLD,
        mm::WORLD_BY_CODE,
        mm::WORLD_ANNOUNCE,
        mm::WORLD_CODE,
        mm::WORLD_INVITES,
        mm::WORLD_REQUESTS,
        mm::WORLD_REQUEST_ACCEPT,
        mm::WORLD_MODERATION,
        mm::WORLD_MEMBERS,
        mm::WORLD_JOIN_MODES,
        mm::JOIN,
        mm::GRANT,
        mm::SEAT,
        mm::SEAT_RENEW,
        mm::SEAT_NETWORK,
        mm::PUNCHES,
        mm::CAPACITY,
        mm::OFFICIAL_HELLO,
        mm::OFFICIAL_WORLD,
        mm::OFFICIAL_WORLD_ANNOUNCE,
        mm::OFFICIAL_WORLD_MODERATION,
        mm::OFFICIAL_WORLD_GAME_SESSIONS,
    ];
    const PORTAL: &[&str] = &[
        portal::HEALTH,
        portal::SESSIONS,
        portal::SESSIONS_LAUNCHER,
        portal::SESSIONS_CURRENT,
        portal::ACCOUNTS,
        portal::LEGACY_LINK,
        portal::ME,
        portal::ME_SESSIONS,
        portal::ME_SESSION,
        portal::ME_DELETION,
        portal::ME_EXPORT,
    ];

    /// `/core/v1` (the MCP endpoint and its discovery documents live outside it).
    const CORE: &[&str] = &[
        core::STATUS,
        core::SUMMARY,
        core::NOTICES,
        core::ME_NOTICES,
        core::ME_STRIKES,
        core::ME_SUPPORT,
        core::ME_DISCORD,
        core::ME_DISCORD_LINK,
        core::ME_PROFILE_ANNOUNCE,
        core::FEED,
        core::DISCORD_LINK_PREVIEW,
        core::DISCORD_LINKS,
        core::DISCORD_USER,
        core::SOCIAL_PLAYERS,
        core::SOCIAL_PLAYER,
        core::SOCIAL_COMMENTS,
        core::SOCIAL_COMMENT,
        core::SOCIAL_REPORTS,
        core::ME_FRIENDS,
        core::ME_FRIEND,
        core::ME_FRIEND_ACCEPT,
        core::ME_FRIEND_DECLINE,
        core::ME_FRIEND_FAVORITE,
        core::ADMIN_MAINTENANCE,
        core::ADMIN_CAPACITY,
        core::ADMIN_NOTICES,
        core::ADMIN_NOTICE,
        core::ADMIN_ACCOUNTS,
        core::ADMIN_ACCOUNT,
        core::ADMIN_STRIKES,
        core::ADMIN_STRIKE_REVIEW,
        core::ADMIN_BANS,
        core::ADMIN_BAN,
        core::ADMIN_KICKS,
        core::ADMIN_VOICE_BANS,
        core::ADMIN_VOICE_BAN,
        core::ADMIN_CHEAT_FLAG,
        core::ADMIN_REPORTS,
        core::ADMIN_REPORT,
        core::ADMIN_ROLE,
        core::ADMIN_ERASE,
        core::ADMIN_AUDIT,
        core::ADMIN_ASSESSMENTS,
        core::ADMIN_ACCOUNT_LIVERIES,
        core::ADMIN_LIVERY_UPLOADS,
        core::ADMIN_LIVERY,
        core::ADMIN_SEED_REVOKE,
    ];
    const GAME: &[&str] = &[game::SESSION, game::PROGRESSION, game::PRESENCE];
    const MEDIA: &[&str] = &[media::PREVIEW];
    const INTERNAL: &[&str] = &[internal::POOLS, internal::LOCAL_ENTITLEMENTS];
    const DASHBOARD: &[&str] = &[
        dashboard::LIVE,
        dashboard::LOBBIES,
        dashboard::BOARDS,
        dashboard::BOARD,
        dashboard::PLAYERS,
        dashboard::PLAYER,
        dashboard::MEDIA_AVATAR,
        dashboard::LISTED,
        dashboard::CLUBS,
        dashboard::CLUB,
        dashboard::MEDIA_CREATION,
    ];

    #[test]
    fn every_route_sits_under_its_surface_and_is_well_formed() {
        for (base, routes) in [
            (launcher::BASE, LAUNCHER),
            (mm::BASE, MM),
            (portal::BASE, PORTAL),
            (core::BASE, CORE),
            (game::BASE, GAME),
            (internal::BASE, INTERNAL),
            (dashboard::BASE, DASHBOARD),
            (media::BASE, MEDIA),
        ] {
            for route in routes {
                let rest = route
                    .strip_prefix(base)
                    .unwrap_or_else(|| panic!("{route} not under {base}"));
                assert!(rest.starts_with('/') && !rest.ends_with('/'), "{route}");
                for segment in rest[1..].split('/') {
                    let ok = match segment.strip_prefix('{') {
                        Some(name) => name.strip_suffix('}').is_some_and(|n| {
                            !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphabetic())
                        }),
                        None => {
                            !segment.is_empty()
                                && segment.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                        }
                    };
                    assert!(ok, "{route}: segment {segment:?}");
                }
            }
        }
    }

    #[test]
    fn routes_are_unique() {
        let mut all: Vec<&str> = LAUNCHER
            .iter()
            .chain(MM)
            .chain(PORTAL)
            .chain(CORE)
            .chain(GAME)
            .chain(INTERNAL)
            .chain(DASHBOARD)
            .copied()
            .collect();
        all.sort_unstable();
        let before = all.len();
        all.dedup();
        assert_eq!(all.len(), before);
    }

    #[test]
    fn the_settled_names_of_cr_l3_and_i_10() {
        assert_eq!(launcher::AUTH_TOKEN, "/launcher/v1/auth/token");
        assert_eq!(launcher::TOKEN, "/launcher/v1/token");
        assert_eq!(launcher::LEGACY_LINK, "/launcher/v1/legacy/link");
    }
}
/// Crash intake, a separate authenticated deployable (CRASH_API.md).
pub mod crash {
    /// Authenticated metadata-only capabilities.
    pub const CAPABILITIES: &str = "/crash/v1/capabilities";
    /// Create metadata.
    pub const REPORTS: &str = "/crash/v1/reports";
    /// Owner's status.
    pub const REPORT: &str = "/crash/v1/reports/{id}";
    /// Disabled legacy upload route (CRASH.DUMPS_DISABLED); never buffers a body.
    pub const MINIDUMP: &str = "/crash/v1/reports/{id}/minidump";
}

/// CR-L29 authenticated compatibility policy, observations and permissions (CONTENT_API.md).
pub mod content {
    /// Launcher bearer GET, known game query.
    pub const CAPABILITIES: &str = "/gate/v1/content/capabilities";
    /// Launcher bearer POST, bounded compatibility observation.
    pub const SCANS: &str = "/gate/v1/content/scans";
    /// Launcher bearer GET, own future/current session permission.
    pub const PERMISSION: &str = "/gate/v1/sessions/{sessionId}/content";
}

/// `/media/v1`: public images on the core origin (ACCOUNTS.md §5a, CR-S77).
pub mod media {
    /// Prefix of every media route.
    pub const BASE: &str = "/media/v1";
    /// *public* `GET`/`HEAD`: a Storefront livery file's preview PNG; `{file}` = `<token>.png`.
    pub const PREVIEW: &str = "/media/v1/previews/{file}";
    /// The URL path of the preview named by `token_hex` (32 lowercase hex digits).
    pub fn preview_path(token_hex: &str) -> String {
        format!("{BASE}/previews/{token_hex}.png")
    }
}
