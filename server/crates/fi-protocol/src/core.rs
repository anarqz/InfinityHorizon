//! DTOs of `/core/v1`, the web's cookie session and `/mcp` (docs/contracts/CORE_API.md; platform
//! v1, ADR 0013, 0018, 0207): status, the notice board, strikes, the social hub, the admin and
//! moderation bodies, the MCP tool inputs and the OAuth discovery documents. Mirror:
//! `packages/contracts/src/core.ts` (the tests below compare every interface's fields with the
//! serialized Rust shape). Routes: [`crate::routes::core`].
//!
//! Requests refuse unknown fields; responses may grow (clients ignore unknown fields). The OAuth
//! discovery documents use the RFCs' snake_case member names.

use crate::accounts::{Deletion, Profile};
use crate::portal::Viewer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// §2.2 Status --------------------------------------------------------------------------------

/// `GET /core/v1/status` → 200 (public, `Cache-Control: public, max-age=10, s-maxage=10`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// `ok` | `degraded` (a probed service is down) | `maintenance` (a game is in maintenance).
    pub status: String,
    /// When this answer was built.
    pub generated_at: String,
    /// Per game.
    pub games: Vec<GameStatus>,
    /// The gold snapshot; `null` until the first gold build (ADR 0016 §4).
    pub stats: Option<GoldStats>,
    /// Last readiness probe per service: `ok` | `down` | `unknown` (not configured).
    pub services: ServiceHealth,
}

/// One game in [`Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameStatus {
    /// `fh2`.
    pub id: String,
    /// Whether the game takes players (false also while in maintenance).
    pub accepting: bool,
    /// End of the maintenance in force.
    pub maintenance_until: Option<String>,
    /// Live sessions per pool (`vip`, `matchmaking`, ...).
    pub queue: BTreeMap<String, QueueCounts>,
    /// Live aggregates of the game (CR-L40; additive, absent from older servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<GameLive>,
}

/// [`GameStatus::live`] (CR-L40): aggregates only, no identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameLive {
    /// Admitted, unexpired, non-dedicated sessions of the game (the sum of `queue.*.active`).
    pub online: u64,
    /// The game's online capacity (OD-71); `null` when unlimited.
    pub capacity: Option<u32>,
    /// Of `online`, sessions whose profile is Hardcore.
    pub hardcore: u64,
    /// Of `online`, sessions whose profile is Noob.
    pub noob: u64,
    /// Open worlds inside their TTL, every mode, modded or not, dedicated included.
    pub lobbies: u64,
    /// Non-deleted profiles of the game per mode.
    pub profiles: ModeCounts,
}

/// A count per mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeCounts {
    /// Hardcore.
    pub hardcore: u64,
    /// Noob.
    pub noob: u64,
}

/// Live sessions of one pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueCounts {
    /// Admitted.
    pub active: u64,
    /// Waiting.
    pub queued: u64,
    /// Capacity, `null` when unlimited.
    pub capacity: Option<u32>,
}

/// The consolidated gold snapshot (`gold_snapshot`, ADR 0016 §4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoldStats {
    /// When the gold build ran.
    pub built_at: String,
    /// Players known.
    pub players_total: u64,
    /// Supporters.
    pub supporters: u64,
    /// Worlds open at the build.
    pub worlds_open: u64,
    /// Top boards (shape owned by the saves lane's gold build).
    pub top_boards: Vec<serde_json::Value>,
}

/// `GET /core/v1/summary?game=fh2` → 200 (public, `Cache-Control: public, max-age=300,
/// s-maxage=300`): the server summary of one game (CORE_API.md §2.4; owner-decisions, OD-122
/// follow-up 2026-10-26 "a nightly server summary"). Aggregates only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSummary {
    /// `fh2`.
    pub game: String,
    /// When this answer was built.
    pub generated_at: String,
    /// Live: admitted, unexpired, non-dedicated sessions of the game now (the sum of
    /// [`GameStatus::queue`]'s `active`).
    pub online_now: u64,
    /// The last nightly snapshot; `null` until `fi-cli consolidate-nightly` first ran.
    pub nightly: Option<NightlySummary>,
}

/// The nightly part of [`ServerSummary`], stored as the `summary` gold snapshot of the game.
/// "Day" figures cover the UTC day [`NightlySummary::day`]; "total" figures count everything up
/// to the end of that day, so a rerun for the same day gives the same numbers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NightlySummary {
    /// When the nightly job built it.
    pub built_at: String,
    /// The UTC day covered, `YYYY-MM-DD`.
    pub day: String,
    /// Players.
    pub players: SummaryPlayers,
    /// Time played.
    pub time: SummaryTime,
    /// Online play.
    pub online: SummaryOnline,
    /// Progression read from the saves.
    pub progression: SummaryProgression,
}

/// Players of [`NightlySummary`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryPlayers {
    /// Accounts (not deleted) owning a profile of the game that is not deleted.
    pub accounts: u64,
    /// Profiles of the game that are not deleted.
    pub profiles: u64,
    /// Distinct profiles with an admitted session overlapping the day.
    pub active_day: u64,
    /// The same over the 7 days ending with the day.
    pub active_week: u64,
    /// The same over the 30 days ending with the day.
    pub active_month: u64,
}

/// Time played in [`NightlySummary`]: admitted, non-dedicated sessions, from admission to their
/// end (an open session to the end of the day).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryTime {
    /// Seconds played up to the end of the day.
    pub play_seconds_total: u64,
    /// Seconds played within the day.
    pub play_seconds_day: u64,
}

/// Online play in [`NightlySummary`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryOnline {
    /// Worlds opened (hosted) up to the end of the day.
    pub worlds_hosted: u64,
    /// Worlds opened within the day.
    pub worlds_hosted_day: u64,
    /// World joins (matchmaking grants a host reported the peer for) up to the end of the day.
    pub joins: u64,
    /// World joins within the day.
    pub joins_day: u64,
    /// Admitted, non-dedicated play sessions up to the end of the day.
    pub sessions_total: u64,
    /// Sessions admitted within the day.
    pub sessions_day: u64,
    /// Accepted race results the game reported while connected, up to the end of the day.
    pub races: u64,
    /// Accepted race results received within the day.
    pub races_day: u64,
}

/// Progression in [`NightlySummary`]: each profile's save head as of the end of the day (its
/// newest revision committed before it; profiles not deleted).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryProgression {
    /// Σ the saves' `TotalWinnings` (lifetime credits earned from races).
    pub credits_earned: u64,
    /// Σ the saves' XP.
    pub xp_total: u64,
    /// XP of accepted `xp` facts of the sessions that ended within the day.
    pub xp_day: u64,
    /// Σ the saves' driver level.
    pub levels_total: u64,
}

/// Readiness per service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceHealth {
    /// fi-gate.
    pub gate: String,
    /// fi-saves.
    pub saves: String,
    /// fi-matchmaking.
    pub matchmaking: String,
    /// The relay VM.
    pub relay: String,
}

// §2.3 Notices -------------------------------------------------------------------------------

/// A notice of the board.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    /// Id (`maintenance:<game>` for the derived maintenance notice).
    pub id: String,
    /// The game, `null` for every game.
    pub game: Option<String>,
    /// `news` | `maintenance` | `warning` | `event`.
    pub kind: String,
    /// `info` | `warning` | `error`.
    pub level: String,
    /// Title.
    pub title: String,
    /// Markdown, at most 4 KiB.
    pub body: String,
    /// `en` | `pt-BR`.
    pub lang: String,
    /// Shown from.
    pub since: String,
    /// Shown until.
    pub until: Option<String>,
}

/// `GET /core/v1/notices` and `GET /core/v1/admin/notices` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoticeList {
    /// Newest `since` first.
    pub items: Vec<Notice>,
}

/// `POST /core/v1/admin/notices` (ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewNotice {
    /// The game, `null` for every game.
    pub game: Option<String>,
    /// `news` | `warning` | `event` (`maintenance` notices come from the switch).
    pub kind: String,
    /// `info` | `warning` | `error`.
    pub level: String,
    /// 1 to 120 characters.
    pub title: String,
    /// Markdown, 1 byte to 4 KiB.
    pub body: String,
    /// `en` | `pt-BR`.
    pub lang: String,
    /// From (default now).
    #[serde(default)]
    pub since: Option<String>,
    /// Until.
    #[serde(default)]
    pub until: Option<String>,
}

/// A notice about the caller's account (`GET /core/v1/me/notices`): ACCOUNTS.md §5 kinds plus
/// `queue` and `maintenance`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountNotice {
    /// `deletion_pending` | `strike` | `parked_save` | `vip_ending` | `update_required` |
    /// `queue` | `maintenance` | `kicked` (CR-S62) | `voice_blocked` (CR-S68).
    pub kind: String,
    /// `info` | `warning` | `error`.
    pub level: String,
    /// English text (clients branch on `kind`).
    pub text: String,
    /// Since when.
    pub since: String,
    /// The game it is about.
    pub game: Option<String>,
    /// `queue`: the 1-based place in line.
    pub position: Option<u64>,
    /// The end of what the notice is about (CR-S68: `voice_blocked`'s block); absent otherwise
    /// (the other notices' bytes are unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// `GET /core/v1/me/notices` → 200 (never cached).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountNoticeList {
    /// Items.
    pub items: Vec<AccountNotice>,
}

// §3 Account management ----------------------------------------------------------------------

/// A strike on one of the account's profiles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Strike {
    /// Id.
    pub id: String,
    /// Game.
    pub game: String,
    /// The profile's XUID.
    pub xuid: String,
    /// `save_gate` | `profile_gate` | `race_time` | `top_speed` | `manual`.
    pub source: String,
    /// Reason.
    pub reason: String,
    /// Created.
    pub created_at: String,
    /// The review, when there was one.
    pub reviewed: Option<StrikeReview>,
}

/// A strike's review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrikeReview {
    /// When.
    pub at: String,
    /// `upheld` | `overturned`.
    pub outcome: String,
}

/// `GET /core/v1/me/strikes` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrikeList {
    /// Newest first.
    pub items: Vec<Strike>,
}

/// `GET /core/v1/me/support` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportInfo {
    /// Where to get help.
    pub links: SupportLinks,
    /// The report bundle upload token; `null` until that upload exists (CORE_API.md §3).
    pub report_token: Option<String>,
}

/// Support links.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportLinks {
    /// Ko-fi page.
    pub kofi: String,
    /// Help centre.
    pub help: String,
    /// Where to report a problem.
    pub report: String,
}

// Web session (PORTAL_API.md §1 as amended, ADR 0207 §3) --------------------------------------

/// `POST /portal/v1/sessions` on core → 201: the session travels in the `__Host-fi_session`
/// cookie, never in the body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum WebSignInResult {
    /// Signed in: the cookie is set.
    SignedIn {
        /// The session's sliding expiry.
        expires_at: String,
        /// Who signed in.
        viewer: Box<Viewer>,
    },
    /// Unknown identity: create an account (`POST /portal/v1/accounts`) or link a Beta account.
    NeedsAccount {
        /// `fi_it_…`, 15 minutes, single use.
        identity_ticket: String,
        /// Its expiry.
        identity_ticket_expires_at: String,
        /// The verified e-mail, when there is one.
        email: Option<String>,
    },
}

/// `POST /portal/v1/accounts` on core → 201 (the cookie is set).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSession {
    /// The session's sliding expiry.
    pub expires_at: String,
    /// Who signed in.
    pub viewer: Viewer,
}

// §4 Social hub ------------------------------------------------------------------------------

/// A player's public card (no account id, no e-mail).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerCard {
    /// Decimal XUID.
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// Game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// Level.
    pub level: Option<i32>,
    /// The owner's tier: `free` | `vip` (shown as "Infinity Supporter").
    pub tier: String,
    /// Club, when clubs exist (POR-M5).
    pub club: Option<ClubRef>,
    /// Playing now.
    pub online: bool,
    /// Profile created.
    pub since: String,
}

/// A club reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubRef {
    /// Id.
    pub id: String,
    /// Tag.
    pub tag: String,
}

/// `GET /core/v1/social/players` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerCardList {
    /// By name.
    pub items: Vec<PlayerCard>,
    /// Cursor of the next page.
    pub next: Option<String>,
}

/// `GET /core/v1/social/players/{xuid}` for a Supporter viewer: the card plus `full`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerProfile {
    /// The card.
    #[serde(flatten)]
    pub card: PlayerCard,
    /// What only Supporters see.
    pub full: FullProfile,
}

/// The Supporter part of a player's profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullProfile {
    /// The progress projection, when it has a row.
    pub progress: Option<ProgressSummary>,
    /// Boards summary (`null` until the universe serving tables exist).
    pub boards: Option<serde_json::Value>,
    /// Creations (`null` until creations v2).
    pub creations: Option<u32>,
    /// Friends (`null` until friends).
    pub friends: Option<u32>,
}

/// `profile_progress` as shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSummary {
    /// Level.
    pub level: Option<i32>,
    /// Experience.
    pub xp: Option<i64>,
    /// Credits.
    pub credits: Option<i64>,
    /// Cars owned.
    pub cars: Option<i32>,
    /// Races.
    pub races: i32,
    /// Seconds played.
    pub play_seconds: i64,
}

/// A comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    /// Id.
    pub id: String,
    /// `player:<xuid>` | `club:<id>` | `news:<id>`.
    pub target: String,
    /// The author's profile.
    pub author: CommentAuthor,
    /// Plain text, at most 1 KiB.
    pub body: String,
    /// Posted.
    pub created_at: String,
    /// Always `false` in lists (hidden comments are not listed).
    pub hidden: bool,
}

/// A comment's author.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentAuthor {
    /// Decimal XUID.
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
}

/// `GET /core/v1/social/comments` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentList {
    /// Newest first.
    pub items: Vec<Comment>,
    /// Cursor of the next page.
    pub next: Option<String>,
}

/// `POST /core/v1/social/comments` (session + Supporter + challenged).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewComment {
    /// `player:<xuid>` | `club:<id>` | `news:<id>`.
    pub target: String,
    /// Plain text, 1 byte to 1 KiB.
    pub body: String,
}

/// `POST /core/v1/social/reports` (session + challenged).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewReport {
    /// `player:<xuid>` | `comment:<id>`.
    pub target: String,
    /// `cheating` | `abuse` | `other`.
    pub reason: String,
    /// Up to 1000 characters.
    #[serde(default)]
    pub text: Option<String>,
}

/// `POST /core/v1/social/reports` and `POST /launcher/v1/reports` → 202.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportAccepted {
    /// The report (the reporter's open one on that target when it existed; from the game, also
    /// its report of that player within 24 hours).
    pub id: String,
}

/// `POST /launcher/v1/reports` (the launcher's access token, `Idempotency-Key`; CORE_API.md §4.3,
/// OD-63): any player card the game shows (OD-193, CR-S90); a world named must be one the two
/// shared in the last 30 minutes. No free text (v1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameReport {
    /// The game (`fh2`).
    pub game: String,
    /// The target profile's XUID, decimal.
    pub xuid: String,
    /// `cheating` | `abuse` | `other`.
    pub reason: String,
    /// The world the two shared (the roster's), when the overlay names it; absent from the game's
    /// own player menus (OD-193).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_id: Option<String>,
}

// §4.4 Voice mutes (CR-S68) -------------------------------------------------------------------

/// `POST /launcher/v1/voice/mutes` (the launcher's access token; CORE_API.md §4.4): the overlay's
/// mute or unmute of a player's voice. Idempotent by state (no `Idempotency-Key`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceMute {
    /// The target profile's XUID, decimal.
    pub xuid: String,
    /// One of [`VOICE_MUTE_ACTIONS`].
    pub action: String,
    /// The world the two shared (a mute needs it within the last 30 minutes).
    pub world_id: String,
}

/// `POST /launcher/v1/voice/mutes` → 202.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceMuteAccepted {
    /// The target's XUID, decimal (as sent, canonical).
    pub xuid: String,
    /// The caller's state toward the target now.
    pub action: String,
}

/// [`VoiceMute::action`] values.
pub const VOICE_MUTE_ACTIONS: [&str; 2] = ["muted", "unmuted"];

// §4.1 Friends --------------------------------------------------------------------------------

/// A profile of a friend (or of a request's other account), as the lists show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendProfile {
    /// Decimal XUID.
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// Game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// CR-S102 (CORE_API.md §4.2a): its page on the play dashboard
    /// (`<FI_PUBLIC_CORE_URL>/{game}/{mode}/profiles/{name}`), absent when the website does not
    /// list it (CR-S83 `web_listed`, or hidden by the report rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_url: Option<String>,
}

/// A friend account's verified Discord link (CORE_API.md §4.2a, §5a.2; CR-S102).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendDiscord {
    /// The Discord handle.
    pub username: String,
    /// The Discord display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_name: Option<String>,
    /// The avatar on Discord's CDN (Discord's default avatar when the user has none).
    pub avatar_url: String,
}

/// What a friend does in its world (CORE_API.md §4.2a, CR-S102).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendDoing {
    /// The world's activity: `freeroam` | `carmeet` | `roadtrip` | `race`.
    pub activity: String,
    /// The world is a private official world (MATCHMAKING_API.md §2.10).
    pub private: bool,
    /// FH2 only: the hook's latest activity report (at most 90 s old) shows a race screen; absent
    /// without such a report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub racing: Option<bool>,
}

/// A friend whose account started a play session since the caller's previous inbox poll
/// (CORE_API.md §4.2a, CR-S102).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendOnline {
    /// The friend's address ([`Friend::id`]).
    pub friend_id: String,
    /// The plain name of the profile it plays.
    pub gamertag: String,
    /// Its game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// When that session was admitted.
    pub since: String,
}

/// A friend: another account (OD-55), never more than its profiles' public cards show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Friend {
    /// The friend's stable address (a decimal XUID: the account's first profile's, whatever that
    /// profile's state); the routes take it, or any XUID of the friend's profiles.
    pub id: String,
    /// The friend account's live profiles, of every game and mode (empty when all were deleted).
    pub profiles: Vec<FriendProfile>,
    /// The caller's favorite mark (the caller's side only).
    pub favorite: bool,
    /// One of the friend's profiles has an active play session now.
    pub online: bool,
    /// When the friendship was made.
    pub since: String,
    /// The friend account's tier now: `free` | `vip` (OD-56).
    pub tier: String,
    /// The profile the friend plays now (or played last), absent when it never played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<FriendPlaying>,
    /// The friend's newest live invite to the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<FriendInvite>,
    /// CR-S102: when the friend account was created (absent on older servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_since: Option<String>,
    /// CR-S102: the friend account's verified Discord link, absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<FriendDiscord>,
}

/// The profile a friend plays (CORE_API.md §4.1 "Presence", OD-56).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendPlaying {
    /// Decimal XUID.
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// Game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// `offline` | `in_game` | `in_world`.
    pub state: String,
    /// `in_world`: the world it hosts or holds a seat in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_id: Option<String>,
    /// `in_world` with `?asXuid=`: a join of that profile would be granted now (the code aside).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joinable: Option<bool>,
    /// OD-190: only when `world_id` is an official world the friend is seated in, its activity
    /// (`freeroam` | `carmeet` | `roadtrip`): "Join friend" is `POST /mm/v1/join {worldId}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub official_activity: Option<String>,
    /// CR-S102: `in_world` only, the world's activity, whether it is private, and a race in
    /// progress (CORE_API.md §4.2a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doing: Option<FriendDoing>,
}

/// A friend's live invite to the caller (the details are on `GET /launcher/v1/invites`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendInvite {
    /// Invite id.
    pub id: String,
    /// The world.
    pub world_id: String,
    /// When it lapses.
    pub expires_at: String,
}

/// A live friend request, incoming or outgoing: the other account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendRequest {
    /// The other account's stable address (as [`Friend::id`]).
    pub id: String,
    /// Its live profiles (outgoing: only those a public card shows).
    pub profiles: Vec<FriendProfile>,
    /// When the request was made.
    pub since: String,
}

/// `GET /core/v1/me/friends` (and the launcher's alias) → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendList {
    /// Listed friends, favorites first, then the oldest friendships.
    pub friends: Vec<Friend>,
    /// Live requests to the caller, newest first.
    pub incoming: Vec<FriendRequest>,
    /// Live requests of the caller, newest first.
    pub outgoing: Vec<FriendRequest>,
}

/// `POST /core/v1/me/friends` (and the launcher's alias) → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
// A wire answer built once per request: the friend row's presence (OD-56) makes this variant
// larger than the other, which costs nothing here.
#[allow(clippy::large_enum_variant)]
pub enum FriendAddResult {
    /// A request is pending (just made, or made before).
    Requested {
        /// The request.
        request: FriendRequest,
    },
    /// The two are friends (just made because the target had asked the caller, or before).
    Friends {
        /// The friend.
        friend: Friend,
    },
}

/// `POST /core/v1/me/friends` and `POST /launcher/v1/friends` (session + Supporter account).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FriendAdd {
    /// The game of the profile named (`fh2`): gamertags are unique per game.
    pub game: String,
    /// The plain name of any of the other player's profiles, case-insensitive (OD-120); the legacy
    /// `Name#12345` is still accepted during the transition, the number exact. Exactly one of
    /// `gamertag` and `xuid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gamertag: Option<String>,
    /// The decimal XUID of any of the other player's profiles in `game` (the game's own player
    /// menus, OD-193; register CR-S89).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xuid: Option<String>,
}

/// `PUT /core/v1/me/friends/{friendId}/favorite` and the launcher's alias.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FriendFavorite {
    /// Set (`true`) or unset the caller's favorite mark.
    pub favorite: bool,
}

// §4.2 Invites to play (OD-56) -------------------------------------------------------------------

/// Who sends it: the host's profile (an invite) or the asker's (a request).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteFrom {
    /// Decimal XUID.
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// `free` | `vip`.
    pub tier: String,
}

/// A private world's game session (MATCHMAKING_API.md §2.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteSession {
    /// Its service configuration.
    pub scid: String,
    /// Its template.
    pub template_name: String,
    /// Its name.
    pub name: String,
}

/// A pending invite to the caller's account, or a request to join the caller's world.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteReceived {
    /// Id (`POST /launcher/v1/invites/{inviteId}/decline`).
    pub id: String,
    /// `invite` (accept: join the world) | `request` (accept: the host's
    /// `POST /mm/v1/worlds/{worldId}/requests/{requestId}/accept`).
    pub kind: String,
    /// The world as the caller's current profile sees it.
    pub world: crate::matchmaking::WorldView,
    /// The sender.
    pub from: InviteFrom,
    /// The world's mode is the caller's current profile's (absent: no current profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_mode: Option<bool>,
    /// An invite to a private official world (MATCHMAKING_API.md §2.10, CR-S96). Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private: bool,
    /// The game session linked to that world, once its owner's game wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<InviteSession>,
    /// When it was made.
    pub created_at: String,
    /// When it lapses.
    pub expires_at: String,
}

/// `GET /launcher/v1/invites` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteList {
    /// Pending invites, newest first (at most 20).
    pub items: Vec<InviteReceived>,
    /// When it was read.
    pub generated_at: String,
    /// CR-S102: friends who came online since the previous poll of this device, oldest first
    /// (at most 20; absent on older servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<Vec<FriendOnline>>,
    /// CR-S106: the caller's own invites and requests of the last 2 minutes (or still pending),
    /// with their state, newest first (at most 20; absent on older servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<Vec<InviteSent>>,
}

/// The account an invite or request the caller sent is addressed to now (CR-S106).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteTo {
    /// Its address ([`Friend::id`]: the account's first profile's decimal XUID).
    pub friend_id: String,
    /// The plain name of its profile in the world's game (empty when it has none).
    pub gamertag: String,
}

/// An invite or request to join the caller sent, with its state (CR-S106, CORE_API.md §4.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteSent {
    /// Id.
    pub id: String,
    /// `invite` | `request`.
    pub kind: String,
    /// The world.
    pub world_id: String,
    /// Who answers it now (a request to a private official world: its current leader).
    pub to: InviteTo,
    /// `pending` | `accepted` | `declined` | `expired` | `withdrawn` (a pending one past
    /// `expiresAt` reads `expired`).
    pub state: String,
    /// When it lapses (or lapsed).
    pub expires_at: String,
}

// §5 Admin and moderation --------------------------------------------------------------------

/// `GET` / `PUT /core/v1/admin/maintenance` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Maintenance {
    /// Game.
    pub game: String,
    /// End of the maintenance in force, `null` when none.
    pub until: Option<String>,
    /// Message shown with it.
    pub message: Option<String>,
}

/// `GET`/`PUT /core/v1/admin/capacity` → 200 (OD-71): a game's online capacity and its live
/// sessions now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameCapacity {
    /// Game.
    pub game: String,
    /// The most active sessions of the game at once (`null`: unlimited).
    pub online_capacity: Option<u32>,
    /// Live sessions now, every pool.
    pub active: u32,
    /// Waiting sessions now, every pool.
    pub queued: u32,
}

/// `PUT /core/v1/admin/capacity` (ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameCapacityUpdate {
    /// Game.
    pub game: String,
    /// 1 to 100,000, or `null` for unlimited.
    pub online_capacity: Option<u32>,
}

/// `PUT /core/v1/admin/maintenance` (ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MaintenanceUpdate {
    /// Game.
    pub game: String,
    /// End (`null` lifts it).
    pub until: Option<String>,
    /// Up to 500 characters.
    #[serde(default)]
    pub message: Option<String>,
}

/// `GET /core/v1/admin/accounts?gamertag=|xuid=|email=` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLookupList {
    /// Matching profiles.
    pub items: Vec<AccountLookup>,
}

/// One lookup match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLookup {
    /// Account.
    pub account_id: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// Game.
    pub game: String,
    /// Decimal XUID.
    pub xuid: String,
}

/// `GET /core/v1/admin/accounts/{accountId}` → 200 (MOD, ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminAccountView {
    /// The account.
    pub account: AdminAccount,
    /// Live profiles.
    pub profiles: Vec<Profile>,
    /// `free` | `vip`.
    pub tier: String,
    /// Strikes, newest first.
    pub strikes: Vec<Strike>,
    /// Play sessions.
    pub sessions: AdminSessions,
    /// Reports it made and received.
    pub reports: ReportCounts,
    /// Its latest audit rows.
    pub audit: Vec<AdminAction>,
    /// Its voice blocks and the mutes about it (CR-S68).
    pub voice: AdminVoice,
    /// The cheat flags of its profiles (MATCHMAKING_API.md §3.9): live, or ended within the 30-day
    /// repeat window, the latest first.
    pub cheat: Vec<AdminCheatFlag>,
}

/// One profile's cheat flag as staff see it (CORE_API.md §5, MATCHMAKING_API.md §3.9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminCheatFlag {
    /// The profile.
    pub profile_id: String,
    /// The flag's end (in the past once it ended or was lifted).
    pub until: String,
    /// The flag's first moment.
    pub first_at: String,
    /// Its repeat count (1 = first; each new flag within 30 days of the last doubles).
    pub episode: i32,
    /// `speed` | `climb` | `car` | `parts`: the kinds of its quorums.
    pub kinds: Vec<String>,
    /// In force now (the pineapple pool applies).
    pub in_force: bool,
    /// Staff lifted it (`DELETE /core/v1/admin/cheat-flags/{profileId}`).
    pub lifted: bool,
}

/// `DELETE /core/v1/admin/cheat-flags/{profileId}` (MOD, ADM; MATCHMAKING_API.md §3.9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheatUnflag {
    /// 1 to 500 characters, no control characters (kept in the audit row; never shown to the
    /// player).
    pub reason: String,
}

/// An account's voice moderation state (CR-S68, CORE_API.md §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminVoice {
    /// A moderator voice ban in force, its end.
    pub banned_until: Option<String>,
    /// A community mute in force, its end.
    pub community_until: Option<String>,
    /// The newest 50 mute events about the account.
    pub mutes: Vec<VoiceMuteRecord>,
}

/// One mute event about an account (moderators only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceMuteRecord {
    /// Who muted.
    pub muter_account_id: String,
    /// `muted` | `unmuted`.
    pub action: String,
    /// Which world.
    pub world_id: String,
    /// Its game.
    pub game: String,
    /// When.
    pub at: String,
}

/// The account in the moderation view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminAccount {
    /// Id.
    pub id: String,
    /// `active` | `deletion_pending` | `deleted`.
    pub status: String,
    /// `player` | `moderator` | `admin`.
    pub role: String,
    /// `registered` | `beta_import`.
    pub origin: String,
    /// Created.
    pub created_at: String,
    /// The recorded ban (in force or ended), `null` when none.
    pub ban: Option<Ban>,
    /// Pending deletion.
    pub deletion: Option<Deletion>,
    /// `a***@example.test` for ADM, `null` for MOD.
    pub email_masked: Option<String>,
    /// The verified Discord user (CR-S63; MOD and ADM), `null` when unlinked. Separate from the
    /// operator's free-text Discord note.
    #[serde(default)]
    pub discord_link: Option<DiscordLink>,
}

/// A ban.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ban {
    /// Since.
    pub since: String,
    /// Until (`null`: permanent).
    pub until: Option<String>,
    /// Reason.
    pub reason: Option<String>,
    /// Still in force.
    pub active: bool,
}

/// Sessions in the moderation view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSessions {
    /// The live one, when any.
    pub live: Option<LiveSession>,
    /// The last ten.
    pub recent: Vec<SessionRecord>,
}

/// A live session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSession {
    /// Game.
    pub game: String,
    /// Pool.
    pub pool: String,
    /// Since.
    pub since: String,
}

/// A past or present session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    /// Id.
    pub id: String,
    /// Game.
    pub game: String,
    /// Pool.
    pub pool: String,
    /// `queued` | `active` | `ended` | `expired` | `revoked`.
    pub state: String,
    /// Created.
    pub created_at: String,
    /// Ended.
    pub ended_at: Option<String>,
    /// End reason.
    pub end_reason: Option<String>,
}

/// Report counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportCounts {
    /// Filed by the account.
    pub made: u64,
    /// About the account.
    pub received: u64,
}

/// `POST /core/v1/admin/strikes` (MOD, ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewStrike {
    /// The profile's decimal XUID.
    pub xuid: String,
    /// `manual`.
    pub source: String,
    /// 1 to 500 characters.
    pub reason: String,
}

/// `POST /core/v1/admin/strikes/{id}/review` (MOD, ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StrikeReviewRequest {
    /// `upheld` | `overturned`.
    pub outcome: String,
    /// Up to 500 characters (kept in the audit row).
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /core/v1/admin/bans` (ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewBan {
    /// The account.
    pub account_id: String,
    /// End (`null`: permanent).
    pub until: Option<String>,
    /// 1 to 500 characters.
    pub reason: String,
}

/// `POST /core/v1/admin/bans` → 201.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BanApplied {
    /// The account.
    pub account_id: String,
    /// End (`null`: permanent).
    pub until: Option<String>,
}

/// `POST /core/v1/admin/kicks` (MOD, ADM; register CR-S62): ends the account's live play
/// sessions without a ban. Exactly one of `xuid` and `accountId`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewKick {
    /// A profile's XUID (decimal), naming its account.
    #[serde(default)]
    pub xuid: Option<String>,
    /// The account.
    #[serde(default)]
    pub account_id: Option<String>,
    /// 1 to 500 characters (kept in the audit row; never shown to the player).
    pub reason: String,
    /// Minutes the account may not be admitted again, 0 to 60 (absent: 0).
    #[serde(default)]
    pub cooldown_minutes: Option<u32>,
}

/// `POST /core/v1/admin/kicks` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KickApplied {
    /// The account.
    pub account_id: String,
    /// Live play sessions (queued or active) the kick ended.
    pub sessions_ended: u64,
    /// Admission is refused until then (`SESSIONS.KICKED`); `null` without a cooldown in force.
    pub until: Option<String>,
}

/// `POST /core/v1/admin/voice-bans` (MOD, ADM; CR-S68, CORE_API.md §5): exactly one of `xuid` and
/// `accountId`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewVoiceBan {
    /// A profile's XUID (decimal), naming its account.
    #[serde(default)]
    pub xuid: Option<String>,
    /// The account.
    #[serde(default)]
    pub account_id: Option<String>,
    /// 1 to 43200 (30 days).
    pub minutes: u32,
    /// 1 to 500 characters (kept in the audit row; never shown to the player).
    pub reason: String,
}

/// `POST /core/v1/admin/voice-bans` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceBanApplied {
    /// The account.
    pub account_id: String,
    /// The ban's end.
    pub until: String,
}

// Discord (CR-S63, CORE_API.md §5a) ------------------------------------------------------------

/// A verified Discord user as the feed names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordUser {
    /// The snowflake (decimal).
    pub id: String,
    /// The unique handle.
    pub username: String,
    /// The display name.
    pub global_name: Option<String>,
    /// The avatar on Discord's CDN (Discord's default avatar when the user has none).
    pub avatar_url: String,
    /// When the handle was last read from Discord (the bot refreshes it after 24 hours).
    pub refreshed_at: String,
}

/// A person named by a feed row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPerson {
    /// The gamertag at that moment; `null` when the account had no live profile.
    pub name: Option<String>,
    /// The account's verified Discord user at read time.
    pub discord: Option<DiscordUser>,
}

/// A record row's values (`speed_record`, `route_record`; CORE_API.md §5a "Records", CR-S64).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicRecord {
    /// `speed_trap` | `speed_zone` | `route`.
    pub board: String,
    /// `hardcore` | `noob` (modes never meet).
    pub mode: String,
    /// `speed_mps` (metres per second) | `time_ms` (whole milliseconds).
    pub unit: String,
    /// The new record.
    pub value: f64,
    /// The record it beat.
    pub previous_value: f64,
    /// The game's car id.
    pub car_id: i32,
    /// The car's catalog name; `null` for a car the catalog does not list (car packs).
    pub car_name: Option<String>,
    /// `gameScoreboardId`: the camera of a speed board, the event of a route record.
    pub board_id: i32,
    /// The route's track; `null` on speed boards.
    pub route_id: Option<i32>,
    /// The route's English name; `null` on speed boards or for a track the catalog does not list.
    pub route_name: Option<String>,
}

/// One row of `GET /core/v1/feed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicEvent {
    /// Its cursor (opaque).
    pub id: String,
    /// When written.
    pub at: String,
    /// The game, `null` for an account-level row without a display profile.
    pub game: Option<String>,
    /// `player_joined` | `lobby_opened` | `player_kicked` | `player_banned` | `speed_record` |
    /// `route_record` | `voice_community_muted` | `voice_banned` | `voice_unbanned` (CR-S68; a reader ignores
    /// kinds it does not know).
    pub kind: String,
    /// `public` (#activity) | `staff` (#moderation-chat).
    pub audience: String,
    /// Who it is about.
    pub subject: Option<EventPerson>,
    /// Who did it (kicks and bans), or a record's previous holder (`null` when hidden).
    pub actor: Option<EventPerson>,
    /// A ban's end or a kick's cooldown.
    pub until: Option<String>,
    /// A record's values (the record kinds; `null` on the others, and absent from a server
    /// before CR-S64).
    #[serde(default)]
    pub record: Option<PublicRecord>,
}

/// `GET /core/v1/feed` → 200.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicEventPage {
    /// Oldest first.
    pub items: Vec<PublicEvent>,
    /// The cursor to pass as `after` next.
    pub next: Option<String>,
    /// `after` named a row no longer kept.
    pub gap: bool,
}

/// The account's verified Discord link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordLink {
    /// The snowflake (decimal).
    pub discord_user_id: String,
    /// The unique handle.
    pub username: String,
    /// The display name.
    pub global_name: Option<String>,
    /// The avatar on Discord's CDN, the account's avatar (Discord's default when none).
    pub avatar_url: String,
    /// When linked.
    pub linked_at: String,
    /// When the handle was last read from Discord.
    pub refreshed_at: String,
}

/// A profile's announce switch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileAnnounce {
    /// Decimal XUID.
    pub xuid: String,
    /// Game.
    pub game: String,
    /// Gamertag.
    pub gamertag: String,
    /// Whether the bot may name it in #activity.
    pub announce: bool,
}

/// `PUT /core/v1/me/profiles/{xuid}/announce`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileAnnounceUpdate {
    /// The switch.
    pub announce: bool,
}

/// `GET /core/v1/me/discord` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordSettings {
    /// The server offers linking (`FI_DISCORD_LINK_URL` set).
    pub available: bool,
    /// The account's link.
    pub link: Option<DiscordLink>,
    /// The account's live profiles.
    pub profiles: Vec<ProfileAnnounce>,
}

/// `POST /core/v1/me/discord/link` → 201.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordLinkCode {
    /// The bot's page to open in the browser (carries the one-time code).
    pub url: String,
    /// When the code expires.
    pub expires_at: String,
}

/// `POST /core/v1/discord/link-preview` (bot).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscordLinkCodeRequest {
    /// The one-time code.
    pub code: String,
}

/// `POST /core/v1/discord/link-preview` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordLinkPreview {
    /// The gamertags of the account the code links.
    pub names: Vec<String>,
    /// When the code expires.
    pub expires_at: String,
}

/// `PUT /core/v1/discord/links` (bot).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscordLinkRequest {
    /// The one-time code.
    pub code: String,
    /// The snowflake (decimal) from Discord's `GET /users/@me`.
    pub discord_user_id: String,
    /// Its `username`.
    pub username: String,
    /// Its `global_name`.
    #[serde(default)]
    pub global_name: Option<String>,
    /// Its `avatar` hash (`null`: Discord's default avatar).
    #[serde(default)]
    pub avatar: Option<String>,
}

/// `PUT /core/v1/discord/links` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordLinked {
    /// The gamertags of the linked account.
    pub names: Vec<String>,
    /// When linked.
    pub linked_at: String,
}

/// `PUT /core/v1/discord/users/{discordUserId}` (bot): a refreshed handle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscordUserUpdate {
    /// Its `username`.
    pub username: String,
    /// Its `global_name`.
    #[serde(default)]
    pub global_name: Option<String>,
    /// Its `avatar` hash (`null`: Discord's default avatar).
    #[serde(default)]
    pub avatar: Option<String>,
}

/// `POST /core/v1/admin/profiles/{profileId}/seed-revoke` (ADM; CR-S52, CORE_API.md §5): the
/// source of a Hardcore line seeded from another profile was found altered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeedRevoke {
    /// 1 to 500 characters (the audit's and the ban's reason; never shown to the player).
    pub reason: String,
}

/// `POST /core/v1/admin/profiles/{profileId}/seed-revoke` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedRevoked {
    /// The seeded Hardcore profile.
    pub profile_id: String,
    /// Its account, now blocked pending review (`ACCOUNTS.BANNED {review: "seed_from_profile"}`).
    pub account_id: String,
    /// The profile the line was seeded from.
    pub source_profile_id: String,
    /// When the line was retired (the first revoke's instant on a replay of the same request).
    pub revoked_at: String,
}

/// A player's car livery as a moderator sees it (CR-S59; never the design's bytes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminLivery {
    /// The versioned livery id (a decimal string: 62 bits).
    pub id: String,
    /// The owner's account.
    pub account_id: String,
    /// The owner's XUID (decimal).
    pub xuid: String,
    /// The game.
    pub game: String,
    /// The car (the game's car id).
    pub car_id: i32,
    /// The design's name, as the player typed it.
    pub name: String,
    /// The design's layer count.
    pub number_of_layers: i16,
    /// The design's size in bytes (0: none stored).
    pub size: i32,
    /// The design's SHA-256 (hex), `null` when none or kept in the row before CR-S59.
    pub sha256: Option<String>,
    /// When it was created.
    pub created_at: String,
    /// When a moderator hid it (`null`: shown).
    pub hidden_at: Option<String>,
}

/// `GET /core/v1/admin/accounts/{accountId}/liveries?cursor=` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminLiveryList {
    /// Newest first, at most 100 a page.
    pub items: Vec<AdminLivery>,
    /// The next page's `cursor` when this page is full.
    pub next: Option<String>,
    /// Whether the account is blocked from uploading liveries.
    pub uploads_blocked: bool,
}

/// `DELETE /core/v1/admin/liveries/{id}` (MOD, ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveryDelete {
    /// 1 to 500 characters.
    pub reason: String,
}

/// `POST /core/v1/admin/accounts/{accountId}/liveries` (MOD, ADM): every livery of the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveryBulk {
    /// `hide` | `delete`.
    pub action: String,
    /// 1 to 500 characters.
    pub reason: String,
}

/// `POST /core/v1/admin/accounts/{accountId}/liveries` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveryBulkApplied {
    /// The account.
    pub account_id: String,
    /// `hide` | `delete`.
    pub action: String,
    /// The liveries affected.
    pub affected: u64,
}

/// `PUT /core/v1/admin/liveries/{id}` (MOD, ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveryVisibility {
    /// `true` hides it from everyone but its owner.
    pub hidden: bool,
    /// 1 to 500 characters.
    pub reason: String,
}

/// `PUT /core/v1/admin/accounts/{accountId}/livery-uploads` (MOD, ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveryUploadsUpdate {
    /// `true` blocks every profile of the account from uploading liveries.
    pub blocked: bool,
    /// 1 to 500 characters.
    pub reason: String,
}

/// `PUT /core/v1/admin/accounts/{accountId}/livery-uploads` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveryUploads {
    /// The account.
    pub account_id: String,
    /// Whether it is blocked now.
    pub blocked: bool,
}

/// A report in the moderation queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminReport {
    /// Id.
    pub id: String,
    /// `player:<xuid>` | `comment:<id>`.
    pub target: String,
    /// The account behind the target.
    pub target_account_id: Option<String>,
    /// The reporter.
    pub reporter_account_id: String,
    /// `cheating` | `abuse` | `other`.
    pub reason: String,
    /// Free text.
    pub text: Option<String>,
    /// `open` | `dismissed` | `actioned`.
    pub state: String,
    /// Filed.
    pub created_at: String,
    /// The decision.
    pub decided: Option<ReportDecision>,
    /// `web` (`POST /core/v1/social/reports`) | `game` (`POST /launcher/v1/reports`, OD-63).
    pub source: String,
    /// A report from the game: the world the two shared, when the overlay named it.
    pub world_id: Option<String>,
}

/// A report's decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportDecision {
    /// When.
    pub at: String,
    /// `dismiss` | `hide` | `strike` | `ban`.
    pub action: String,
    /// Note.
    pub note: Option<String>,
}

/// `GET /core/v1/admin/reports` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminReportList {
    /// Oldest first for `open`, newest first otherwise.
    pub items: Vec<AdminReport>,
    /// Cursor of the next page.
    pub next: Option<String>,
}

/// `POST /core/v1/admin/reports/{id}` (MOD, ADM; `ban` ADM only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReportDecisionRequest {
    /// `dismiss` | `hide` | `strike` | `ban`.
    pub action: String,
    /// Up to 500 characters; the strike's or ban's reason for those actions.
    #[serde(default)]
    pub note: Option<String>,
}

/// `PUT /core/v1/admin/accounts/{accountId}/role` (ADM).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleUpdate {
    /// `player` | `moderator` | `admin`.
    pub role: String,
}

/// `PUT /core/v1/admin/accounts/{accountId}/role` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleChanged {
    /// The account.
    pub account_id: String,
    /// The role now.
    pub role: String,
    /// The role before.
    pub previous: String,
}

/// `POST /core/v1/admin/accounts/{accountId}/erase` → 202.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErasureScheduled {
    /// The account.
    pub account_id: String,
    /// When requested.
    pub requested_at: String,
    /// When the erasure job purges it.
    pub purge_at: String,
}

/// An audit row (`admin_action`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminAction {
    /// Id.
    pub id: String,
    /// When.
    pub at: String,
    /// Who.
    pub actor: AuditActor,
    /// `strike.create`, `account.ban`, ...
    pub action: String,
    /// `account:<id>`, `strike:<id>`, ...
    pub target: Option<String>,
    /// Reason or note.
    pub reason: Option<String>,
    /// The request's id.
    pub request_id: Option<String>,
}

/// The actor of an audit row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditActor {
    /// The account (`null` for an operator row of fi-billing's CLI).
    pub account_id: Option<String>,
    /// `moderator` | `admin` | `operator`.
    pub role: String,
}

/// `GET /core/v1/admin/audit` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditList {
    /// Newest first.
    pub items: Vec<AdminAction>,
    /// Cursor of the next page.
    pub next: Option<String>,
}

/// `GET /core/v1/admin/assessments` → 200 (OD-62, CLOUD_SAVES.md §7.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardAssessmentList {
    /// Newest first.
    pub items: Vec<GuardAssessment>,
    /// Cursor of the next page (the last row's id when the page is full).
    pub next: Option<String>,
}

/// One Hardcore guardrails assessment (`integrity_assessment`). No e-mail, IP or gamertag.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardAssessment {
    /// Its id (UUIDv7).
    pub id: String,
    /// When it was recorded.
    pub at: String,
    /// The game.
    pub game: String,
    /// The profile's XUID (decimal).
    pub xuid: String,
    /// The profile's mode then.
    pub mode: String,
    /// `save_commit` | `profile_sync` | `tables_sync` | `save_audit` | `save_nightly` (the
    /// nightly guardrails pass over a committed revision, CLOUD_SAVES.md §7.5) | `seed_check`
    /// (CR-S52: a seed check of this profile's save; nil `sessionId` for a dry run).
    pub source: String,
    /// The play session.
    pub session_id: String,
    /// Hours measured (wall time between syncs, play time between revisions; at most 720, the fast
    /// rates counting at most one).
    pub hours: f64,
    /// What changed.
    pub deltas: GuardDeltas,
    /// The rates per hour after it.
    pub rates: GuardRates,
    /// 0-100.
    pub score: u8,
    /// Reason codes (`fi_guard::ReasonCode`).
    pub reasons: Vec<String>,
    /// The refusing rule's code (a ceiling's, or `ANOMALY_SCORE`); `null` when accepted.
    pub refusal: Option<String>,
    /// `accepted` | `refused` | `flagged` (`save_nightly` only: what the inline guardrails would
    /// have refused, with that rule in `refusal`; nothing was refused or rolled back).
    pub decision: String,
    /// The score is 60 or more.
    pub would_block: bool,
    /// The rule set it was made with.
    pub rules_version: String,
    /// The revision a save audit read or the nightly pass assessed; `null` for the other
    /// sources.
    pub revision: Option<i64>,
    /// A save audit's numbers (OD-67, ADR 0021); `null` for the other sources.
    pub audit: Option<GuardAudit>,
    /// A seed check's bundle SHA-256, hex (CR-S52); `null` for the other sources.
    #[serde(default)]
    pub bundle_sha256: Option<String>,
    /// A seed check's target: the XUID of the Hardcore profile it would seed (CR-S52); `null` for
    /// the other sources.
    #[serde(default)]
    pub seed_target: Option<String>,
}

/// What the save audit read in a revision's PlayerDatabase (`fi_gamedb::audit::Facts`); `null`
/// where a rule did not run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardAudit {
    /// Why nothing was read (`fi_playerdb::Unreadable` code, or `BLOB_MISSING`).
    pub unreadable: Option<String>,
    /// Garage cars.
    pub cars: Option<i64>,
    /// Cars with no origin bit (the starter pick included).
    pub bought: Option<i64>,
    /// Bought cars not in the base database (not priced).
    pub unpriced: Option<i64>,
    /// Σ base price of the priced bought cars, the starter pick excepted.
    pub bought_value: Option<i64>,
    /// Purchased parts.
    pub parts: Option<i64>,
    /// Σ price paid over them.
    pub parts_paid: Option<i64>,
    /// Unlocked perks.
    pub perks: Option<i64>,
    /// Σ token cost of the known perks.
    pub perks_cost: Option<i64>,
    /// Skill tokens earned.
    pub tokens: Option<i64>,
    /// What the save could have spent since its seed (margin included).
    pub budget: Option<i64>,
    /// Car vouchers (`FreeCars` rows).
    pub vouchers: Option<i64>,
}

/// What an assessment measured (`next − previous`), `null` where a side lacks the number.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardDeltas {
    /// Credits.
    pub credits: Option<i64>,
    /// XP.
    pub xp: Option<i64>,
    /// Levels.
    pub levels: Option<i64>,
    /// Race winnings.
    pub winnings: Option<i64>,
    /// Cars.
    pub cars: Option<i64>,
    /// Credits beyond winnings and the level-up spin allowance.
    pub unexplained: Option<i64>,
    /// A save against the profile's last accepted game sync.
    pub game: Option<GuardGameGap>,
}

/// A save's numbers minus the game's last accepted sync's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardGameGap {
    /// Credits.
    pub credits: i64,
    /// XP.
    pub xp: i64,
    /// Levels.
    pub levels: i64,
    /// Hours since that sync.
    pub hours_since: f64,
}

/// The guardrails' rates per hour.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardRates {
    /// Credits.
    pub credits: f64,
    /// Unexplained credits.
    pub unexplained: f64,
    /// Race winnings.
    pub winnings: f64,
    /// XP.
    pub xp: f64,
    /// Levels.
    pub levels: f64,
    /// Cars.
    pub cars: f64,
    /// Unexplained credits over a day (the slow rate, a 24-hour time constant; 0 in rows recorded
    /// before it, rules `guard-1`).
    #[serde(default)]
    pub day_unexplained: f64,
    /// Race winnings over a day.
    #[serde(default)]
    pub day_winnings: f64,
    /// XP over a day.
    #[serde(default)]
    pub day_xp: f64,
    /// Levels over a day.
    #[serde(default)]
    pub day_levels: f64,
}

// §6 MCP -------------------------------------------------------------------------------------

/// `platform_status` input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformStatusInput {}

/// `queue_status` input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueStatusInput {
    /// One game (default every game).
    #[serde(default)]
    pub game: Option<String>,
}

/// `player_lookup` input: exactly one of the two.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlayerLookupInput {
    /// A plain name or a name prefix (OD-120).
    #[serde(default)]
    pub gamertag: Option<String>,
    /// An account id.
    #[serde(default)]
    pub account_id: Option<String>,
}

/// `strikes_list` input: one of `xuid` and `accountId`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StrikesListInput {
    /// A profile's XUID.
    #[serde(default)]
    pub xuid: Option<String>,
    /// An account id.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Game.
    pub game: String,
}

/// `notices_list` input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoticesListInput {
    /// Game.
    pub game: String,
}

/// `audit_tail` input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditTailInput {
    /// Rows, at most 50.
    pub n: u32,
}

/// `worlds_list` input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorldsListInput {
    /// Game.
    pub game: String,
}

/// RFC 9728 protected-resource metadata of `/mcp`
/// (`/.well-known/oauth-protected-resource/mcp`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// `https://api.<domain>/mcp`.
    pub resource: String,
    /// The issuer(s).
    pub authorization_servers: Vec<String>,
    /// `platform:read`, `moderation:write`.
    pub scopes_supported: Vec<String>,
    /// `header`.
    pub bearer_methods_supported: Vec<String>,
    /// A human name.
    pub resource_name: String,
}

/// RFC 8414 authorization-server metadata of the MCP issuer
/// (`/.well-known/oauth-authorization-server/mcp/oauth`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    /// `https://api.<domain>/mcp/oauth`.
    pub issuer: String,
    /// `<issuer>/authorize`.
    pub authorization_endpoint: String,
    /// `<issuer>/token`.
    pub token_endpoint: String,
    /// `<issuer>/register` (controlled: preconfigured clients only).
    pub registration_endpoint: String,
    /// `<issuer>/revoke`.
    pub revocation_endpoint: String,
    /// `<issuer>/jwks`.
    pub jwks_uri: String,
    /// `code`.
    pub response_types_supported: Vec<String>,
    /// `authorization_code`, `refresh_token`.
    pub grant_types_supported: Vec<String>,
    /// `S256`.
    pub code_challenge_methods_supported: Vec<String>,
    /// `none` (public clients with PKCE).
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// `platform:read`, `moderation:write`.
    pub scopes_supported: Vec<String>,
}

#[cfg(test)]
mod tests {
    //! Wire behaviour of the core DTOs; the TypeScript field parity lives in `tests/ts_mirror.rs`.

    use super::*;
    use crate::accounts::{Access, Notice as AccountNoticeV0};
    use serde_json::json;

    fn at() -> String {
        "2026-10-19T12:00:00.000000Z".into()
    }

    fn card() -> PlayerCard {
        PlayerCard {
            xuid: "2535100000000001".into(),
            gamertag: "Synthetic#00001".into(),
            game: "fh2".into(),
            mode: "noob".into(),
            level: Some(3),
            tier: "free".into(),
            club: Some(ClubRef {
                id: "c".into(),
                tag: "T".into(),
            }),
            online: false,
            since: at(),
        }
    }

    fn viewer() -> Viewer {
        Viewer {
            account_id: "acc".into(),
            role: "player".into(),
            profiles: vec![],
            access: Access {
                tier: "free".into(),
                status: "none".into(),
                access_until: None,
                plan: None,
                provider: None,
                auto_renew: false,
                past_due: false,
            },
            notices: Vec::<AccountNoticeV0>::new(),
            deletion: None,
        }
    }

    #[test]
    fn requests_refuse_unknown_fields_and_the_card_flattens_into_the_profile() {
        assert!(
            serde_json::from_value::<NewComment>(
                json!({"target": "player:1", "body": "x", "extra": 1})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<NewBan>(
                json!({"accountId": "a", "until": null, "reason": "r", "x": 1})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<NewKick>(json!({"accountId": "a", "reason": "r", "x": 1}))
                .is_err()
        );
        let kick: NewKick = serde_json::from_value(json!({"xuid": "1", "reason": "r"})).unwrap();
        assert_eq!((kick.account_id, kick.cooldown_minutes), (None, None));
        let report: NewReport =
            serde_json::from_value(json!({"target": "comment:c", "reason": "other"})).unwrap();
        assert_eq!(report.text, None);
        let game: GameReport =
            serde_json::from_value(json!({"game": "fh2", "xuid": "1", "reason": "abuse"})).unwrap();
        assert_eq!(game.world_id, None);
        assert!(
            serde_json::from_value::<GameReport>(
                json!({"game": "fh2", "xuid": "1", "reason": "abuse", "text": "no free text"})
            )
            .is_err()
        );
        let add: FriendAdd =
            serde_json::from_value(json!({"game": "fh2", "gamertag": "A#00001"})).unwrap();
        assert_eq!(add.game, "fh2");
        for bad in [
            json!({"gamertag": "A#00001"}),
            json!({"game": "fh2", "gamertag": "A#00001", "fromXuid": "1"}),
        ] {
            assert!(serde_json::from_value::<FriendAdd>(bad).is_err());
        }
        assert!(
            serde_json::from_value::<FriendFavorite>(json!({"favorite": true, "fromXuid": "1"}))
                .is_err()
        );
        let made = serde_json::to_value(FriendAddResult::Requested {
            request: FriendRequest {
                id: "1".into(),
                profiles: vec![],
                since: at(),
            },
        })
        .unwrap();
        assert_eq!(made["state"], "requested");
        assert_eq!(made["request"]["id"], "1");
        let profile = serde_json::to_value(PlayerProfile {
            card: card(),
            full: FullProfile {
                progress: None,
                boards: None,
                creations: None,
                friends: None,
            },
        })
        .unwrap();
        assert_eq!(profile["gamertag"], "Synthetic#00001");
        assert!(profile["full"]["progress"].is_null());
        let signed = serde_json::to_value(WebSignInResult::SignedIn {
            expires_at: at(),
            viewer: Box::new(viewer()),
        })
        .unwrap();
        assert_eq!(signed["status"], "signed_in");
        assert!(
            signed.get("token").is_none(),
            "the session is a cookie, never in the body"
        );
    }
}
