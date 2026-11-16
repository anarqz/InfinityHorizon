//! Play sessions and admission (ADR 0008 §2-§3, ACCOUNTS.md §7, SAVES_API.md §3), built by
//! workstream A of the local environment v0 (docs/server/LOCAL_ENV.md §9).
//!
//! One transaction per decision, locks in a fixed order (the global lock while a global online
//! capacity is set, OD-228, and the game's, both through `sessions::lock_game_of`, for a decision
//! that may admit, OD-71, then `sessions::lock_pool`, then the profile's
//! `save_head` through `saves::acquire_in` / `renew_in` / `release_in`, then `play_session` rows
//! and the profile's read models), the database clock read under the pool lock. Every function here runs in the
//! caller's transaction: on `Err` the caller rolls back, so a refusal or a failure anywhere leaves
//! neither a lease nor a session. A new session also takes its account's row
//! `FOR KEY SHARE`, then its profile's `FOR SHARE`, right after the pool lock, so it never
//! interleaves with a Hardcore -> Noob change ([`crate::purge`], OD-52) and never leases a retired
//! save line.
//!
//! Platform v1 (the gate, ADR 0015) runs the same functions through [`crate::gate`], which adds
//! the game, maintenance, tickets and liveness from game requests: a session may hold no save
//! lease (`save_line = false`), the slot is kept while either the heartbeat or
//! a game request (`last_seen_at`) is within the lease, and nothing lives past
//! [`SILENCE_CEILING_MS`] without a signal. Every decision locks the session row after the pool, so
//! the sweeper ([`crate::gate::sweep`], `FOR UPDATE SKIP LOCKED`) and a heartbeat never both win.
//! The slot's length depends on the pool's tier ([`SlotTtl`], OD-40): a Supporter's is the save
//! lease, a free session's the free liveness window ([`FREE_LIVENESS_MS`] by default); each decision
//! records it in `play_session.lease_ttl_ms`, so a free session lapses when neither its heartbeat
//! (`expires_at`) nor a game request (`last_seen_at + window`) is within the window.
//!
//! **Who holds the save lease (OD-52, [`crate::cloud`]).** Only a session of a `vip` pool whose
//! account is a Supporter and whose profile is `hardcore`, at the decision's database clock: the
//! create, a queued session's admission, and every heartbeat check it again. A Supporter's noob
//! session holds a `vip` slot without a lease (the lease TTL, a fifth of it as its heartbeat). A
//! heartbeat of a leased session that no longer meets the rule (the access ended, the profile left
//! `hardcore`) releases the lease and goes on lease-less (`save_line = false`). Every grant and
//! renewal records the account's `access_until` on the line, a grant past the coverage the line
//! last saw marks it lapsed, and so does meeting the Hardcore profile of a non-Supporter (the
//! renewal mark, SAVES_API.md §4.7).
//!
//! - [`create_in`]: expire this account's stale sessions, apply the one-live-session-per-account
//!   rule (`SESSIONS.IN_USE_ELSEWHERE` unless the same device or `takeOver`), expire the pool's
//!   stale sessions and count the active ones; a free slot and nobody queued ahead → lease +
//!   `active` session (`max_until = now + 12 h`, `game_until`), else `queued` (position,
//!   `expires_at = now + 90 s`), beyond `queue_limit` → `POOL.FULL`. Idempotent by the session id.
//! - [`poll_in`]: refreshes a queued session's `expires_at`; admits it when it is within the free
//!   slots (FIFO by `queued_at`: nobody jumps the line).
//! - [`heartbeat_in`]: renews the lease and the slot; resumes a lapsed lease
//!   (`AcquireMode::Resume`) and re-runs admission under the pool lock when the slot was freed
//!   meanwhile (state `expired`); past `max_until` the session ends (`max_age`).
//! - [`end_in`]: releases the lease, closes the session, writes its `session_summary` and the
//!   profile's `profile_progress` ([`crate::progression::session_ended_in`]).
//! - [`reap_pool_in`] (`fi-cli sessions-reap`): expires silent sessions (`expired`), drops
//!   unpolled queued ones (`queue_left`), ends sessions past `max_until` (`max_age`), each with its
//!   session summary.
//! - Pools: [`upsert_pools_in`] from `FI_POOLS_JSON` (as the migrator; `fi_runtime` reads only),
//!   [`occupancy`] for `GET /internal/v1/pools`; the local entitlement writer
//!   ([`ensure_local_entitlement_in`], [`set_local_entitlement_in`]).
//!
//! The session's fencing token lives only in `save_head`: a session row says which pool slot it
//! holds and until when; the lease says whether it may write.

use crate::accounts::{self, AccessRow};
use crate::cloud;
use crate::identity::{IdentityError, ms, ts};
use crate::saves;
use crate::sessions;
use fi_domain::{
    DeviceId, GameRules, PlaySessionId, PoolName, PoolRouting, ProfileId, Tier, UnixMs,
};
use fi_saves::{AcquireMode, AcquireRequest, FenceToken, Lease, LeaseHolder, SaveError};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// A queued session not polled for this long leaves the queue (Beta `STALE`).
pub const QUEUE_TTL_MS: i64 = 90_000;
/// The absolute end of a session after its admission (ACCOUNTS.md §7).
pub const MAX_AGE_MS: i64 = 12 * 3_600_000;
/// The game plane serves a VIP session this long after VIP access ended.
pub const VIP_GRACE_MS: i64 = 2 * 3_600_000;
/// What a queued launcher waits between polls.
pub const RETRY_AFTER_SECS: u32 = 5;
/// The owner's ceiling (OD-26, ADR 0015 §4): a session silent this long (no heartbeat, no game
/// request) ends whatever the lease configuration; also the longest lease a session may record.
pub const SILENCE_CEILING_MS: i64 = 3_600_000;
/// The free liveness window (OD-40, GATE_API.md §4 "Free sessions"; `FI_FREE_LIVENESS_MS`): a
/// session of a free pool that no heartbeat and no game request touched for this long frees its
/// slot. The default and the longest: the game access ticket's lifetime, so the launcher's call
/// at 80% of it always lands before its ticket expires.
pub const FREE_LIVENESS_MS: u32 = 600_000;
/// The shortest free liveness window a deployment may configure (local timing knob).
pub const FREE_LIVENESS_MIN_MS: u32 = 30_000;

/// How long an admitted slot holds without a signal, by its pool's tier and its save line (ADR
/// 0015 §3, OD-40, CR-S51), and which sessions hold a save line (Hardcore only, OD-239). Every
/// decision picks a length with [`SlotTtl::for_session`] and records it in
/// `play_session.lease_ttl_ms`, which the sweeper, the admission's cleanup, occupancy and the
/// ticket route read back, so none of them needs the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotTtl {
    /// `vip` pools (Supporters): the save lease (`FI_LEASE_TTL_MS`); the launcher heartbeats
    /// every fifth of it.
    pub lease_ms: u32,
    /// Free pools (`matchmaking`): the free liveness window (`FI_FREE_LIVENESS_MS`); the
    /// launcher's call every 80% of it renews the ticket and the slot only.
    pub free_ms: u32,
}

impl SlotTtl {
    /// The same length for both tiers (tests of the lease semantics alone).
    pub const fn uniform(ms: u32) -> Self {
        Self {
            lease_ms: ms,
            free_ms: ms,
        }
    }

    /// The slot length of a session of a pool of `pool_tier` that holds a save line or not: the
    /// lease for a session with a line whatever its pool (CR-S51) and for any `vip` pool session,
    /// else the free window.
    pub fn for_session(self, pool_tier: &str, save_line: bool) -> u32 {
        if save_line {
            self.lease_ms
        } else {
            self.for_tier(pool_tier)
        }
    }

    /// The slot length of a session of a pool of `pool_tier`: the lease for `vip`, the free
    /// window for anything else (the rule of [`game_until_for`]).
    pub fn for_tier(self, pool_tier: &str) -> u32 {
        if pool_tier == "vip" {
            self.lease_ms
        } else {
            self.free_ms
        }
    }
}

/// `heartbeatEveryMs` of a session whose slot lasts `ttl_ms` (GATE_API.md §4): a fifth of the
/// lease for a session with a save line whatever its pool (CR-S51) and for a Supporter's (60 s for
/// 5 minutes), 80% of a free session's window without a line (OD-40: 480 s for 10 minutes).
pub fn heartbeat_every_ms(pool_tier: &str, save_line: bool, ttl_ms: u32) -> u32 {
    if save_line || pool_tier == "vip" {
        ttl_ms / 5
    } else {
        ttl_ms / 5 * 4
    }
}

/// Why an admission decision refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// A reversible crash-safety online freeze; login and save handoff remain available.
    #[error("account online access temporarily frozen")]
    Frozen {
        /// Expiry of this reversible action.
        until: UnixMs,
    },
    /// A moderator kicked the account with a re-admission cooldown that has not run out
    /// (`SESSIONS.KICKED {until}`, CORE_API.md §5, register CR-S62).
    #[error("kicked by a moderator; admission refused until the cooldown ends")]
    Kicked {
        /// The cooldown's end.
        until: UnixMs,
    },
    /// Another PC holds this profile, or another profile of the account is live
    /// (`SESSIONS.IN_USE_ELSEWHERE`).
    #[error("the account is playing on another device")]
    InUseElsewhere {
        /// The other device's label.
        device_label: String,
        /// Since when.
        since: UnixMs,
    },
    /// The queue itself is full (`POOL.FULL`).
    #[error("the pool's queue is full")]
    PoolFull,
    /// A dedicated session for an account without `role = 'admin'` (OD-109,
    /// `AUTH.ROLE_REQUIRED {role: "admin"}`), read in the admission's transaction.
    #[error("a dedicated session needs the admin role")]
    RoleRequired,
    /// A `modded` admission for a Hardcore profile (OD-112 follow-up: Hardcore allows no
    /// multiplayer mod; `REQUEST.INVALID {field: "modded", reason: "hardcore"}`).
    #[error("a hardcore profile is never modded")]
    ModdedHardcore,
    /// The routed pool does not exist or does not accept sessions (`POOL.FULL`).
    #[error("the pool does not accept sessions")]
    PoolClosed,
    /// The session id names another account's or profile's session.
    #[error("the session id belongs to another session")]
    KeyReused,
    /// No such session for this account.
    #[error("no such play session")]
    NotFound,
    /// The session is closed (`SESSIONS.ENDED` with the reason).
    #[error("the play session ended ({reason})")]
    Ended {
        /// `end_reason`.
        reason: String,
    },
    /// A heartbeat or token for a session still in the queue.
    #[error("the play session is still queued")]
    Queued,
    /// A launch whose body always carries a lease ([`SaveLinePolicy::Always`]) for a profile that
    /// keeps no save line (OD-52: not a Supporter's Hardcore profile; `SAVES.NOT_ELIGIBLE`).
    #[error("this profile keeps no cloud save line")]
    NotEligible {
        /// The account's tier at the decision.
        tier: Tier,
        /// The profile's mode.
        mode: String,
    },
    /// A save-lease refusal (`SAVES.LEASE_LOST`, `SAVES.LEASE_EXPIRED`, `SAVES.UNAVAILABLE`, ...).
    #[error(transparent)]
    Save(#[from] SaveError),
    /// A lock or statement timeout, a deadlock: retry.
    #[error("database busy")]
    Busy,
    /// Any other database failure.
    #[error("database: {0}")]
    Db(sqlx::Error),
    /// An event did not validate (a bug: nothing is written).
    #[error("event: {0}")]
    Event(String),
}

impl From<sqlx::Error> for AdmissionError {
    fn from(e: sqlx::Error) -> Self {
        match IdentityError::from(e) {
            IdentityError::Db(e) => AdmissionError::Db(e),
            _ => AdmissionError::Busy,
        }
    }
}

async fn crash_access_in(conn: &mut PgConnection, account: Uuid) -> Result<(), AdmissionError> {
    if let Some(until) = crate::crash::frozen_until_in(conn, account).await? {
        return Err(AdmissionError::Frozen { until });
    }
    Ok(())
}

/// A kick's re-admission cooldown (CR-S62): `Kicked` while `account.kicked_until` is ahead.
async fn kick_access_in(conn: &mut PgConnection, account: Uuid) -> Result<(), AdmissionError> {
    let until: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT kicked_until FROM account WHERE id = $1 AND kicked_until > clock_timestamp()",
    )
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    match until {
        Some(until) => Err(AdmissionError::Kicked { until: ms(until) }),
        None => Ok(()),
    }
}

/// A lapsed (`expired`) session created before the account's latest kick is never resumed by a
/// heartbeat (CR-S62): the kick ended every live session, and one that had lapsed must not come
/// back with the refresh family the kick leaves alone.
async fn kicked_since_in(
    conn: &mut PgConnection,
    account: Uuid,
    session: PlaySessionId,
) -> Result<bool, AdmissionError> {
    // The account's row in KEY SHARE (it waits for a kick's FOR UPDATE), then the read: a kick
    // that committed meanwhile is seen, and one that starts after this waits for the resume to
    // commit and ends the session it made active again (review CR-S62 #1).
    sqlx::query("SELECT 1 FROM account WHERE id = $1 FOR KEY SHARE")
        .bind(account)
        .execute(&mut *conn)
        .await?;
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM account a, play_session s WHERE a.id = $1 AND s.id = $2 \
           AND a.kicked_at >= s.created_at)",
    )
    .bind(account)
    .bind(session.as_uuid())
    .fetch_one(&mut *conn)
    .await?)
}

impl From<IdentityError> for AdmissionError {
    fn from(e: IdentityError) -> Self {
        match e {
            IdentityError::Db(e) => AdmissionError::Db(e),
            IdentityError::Busy => AdmissionError::Busy,
            IdentityError::NotFound => AdmissionError::NotFound,
            other => AdmissionError::Event(other.to_string()),
        }
    }
}

/// The launcher install a session runs on (its device binding).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// `device_binding.id`.
    pub binding_id: Uuid,
    /// The install id.
    pub device_id: DeviceId,
    /// The PC name.
    pub label: String,
    /// `windows`, `proton`, `linux`.
    pub platform: String,
}

/// The device of a launcher refresh family (its live binding).
pub async fn device_of_family_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    family_id: Uuid,
) -> Result<Option<Device>, AdmissionError> {
    // Hold this before admission takes any pool lock. A replacement login locks the account
    // exclusively, so it either ends a committed old admission or this read sees revocation.
    sqlx::query("SELECT 1 FROM account WHERE id = $1 FOR KEY SHARE")
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    let row = sqlx::query(
        "SELECT d.id, d.device_id, d.label, d.platform FROM refresh_token r \
         JOIN device_binding d ON d.id = r.device_binding_id \
         WHERE r.family_id = $1 AND r.account_id = $2 AND r.client = 'launcher' \
           AND r.revoked_at IS NULL AND r.expires_at > clock_timestamp() AND d.unbound_at IS NULL LIMIT 1",
    )
    .bind(family_id)
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(Device {
            binding_id: r.try_get("id")?,
            device_id: DeviceId::from_uuid(r.try_get("device_id")?)
                .ok_or(AdmissionError::NotFound)?,
            label: r.try_get("label")?,
            platform: r.try_get("platform")?,
        })
    })
    .transpose()
}

/// The label of one of the account's devices (bound or not), for `SaveHead.deviceLabel`.
pub async fn device_label_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    device_id: Uuid,
) -> Result<Option<String>, AdmissionError> {
    Ok(sqlx::query_scalar(
        "SELECT label FROM device_binding WHERE account_id = $1 AND device_id = $2 \
         ORDER BY unbound_at DESC NULLS FIRST, bound_at DESC LIMIT 1",
    )
    .bind(account_id)
    .bind(device_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// A live profile of an account, as play sessions and saves need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRef {
    /// Id.
    pub profile_id: ProfileId,
    /// XUID.
    pub xuid: u64,
    /// `Name#12345`.
    pub gamertag: String,
    /// `fh2`.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
}

/// The account's live career of `game` of the profile `xuid` (CR-S98: a XUID names the profile,
/// the game its career; CAREERS.md §3).
pub async fn profile_by_xuid_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    game: &str,
) -> Result<Option<ProfileRef>, AdmissionError> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT id, xuid, name, game, mode FROM profile \
         WHERE account_id = $1 AND xuid = $2 AND game = $3 AND status = 'active'",
    )
    .bind(account_id)
    .bind(xuid)
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(ProfileRef {
            profile_id: ProfileId::from_uuid(r.try_get("id")?).ok_or(AdmissionError::NotFound)?,
            xuid: r.try_get::<i64, _>("xuid")? as u64,
            gamertag: r.try_get::<Option<String>, _>("name")?.unwrap_or_default(),
            game: r.try_get("game")?,
            mode: r.try_get("mode")?,
        })
    })
    .transpose()
}

/// An admitted session as the launcher and the game plane need it (tickets, session tokens,
/// heartbeats).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    /// The session.
    pub session_id: PlaySessionId,
    /// Its account.
    pub account_id: Uuid,
    /// Its profile.
    pub profile_id: ProfileId,
    /// The profile's XUID.
    pub xuid: u64,
    /// `Name#12345`.
    pub gamertag: String,
    /// `fh2`.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The pool it holds a slot of.
    pub pool: String,
    /// The pool's tier.
    pub pool_tier: String,
    /// The pool's game-plane base URL (the ticket's `server`).
    pub game_endpoint: String,
    /// The account's tier at this decision (database clock).
    pub tier: Tier,
    /// The account's `access_until` at this decision: a Supporter's ticket never outlives it
    /// (ACCOUNTS.md §7).
    pub access_until: Option<UnixMs>,
    /// The car database the session declared.
    pub car_db: Option<Vec<u8>>,
    /// The launcher's handoff nonce for the launch ticket.
    pub nonce: Option<String>,
    /// The save lease the session holds; `None` for a session without a save line (every session
    /// but a Supporter Hardcore's, OD-52).
    pub lease: Option<Lease>,
    /// The slot's length ([`SlotTtl::for_tier`]): the save lease (`FI_LEASE_TTL_MS`) on a `vip`
    /// pool, the free liveness window (`FI_FREE_LIVENESS_MS`) on a free one.
    pub lease_ttl_ms: u32,
    /// When the slot lapses without a signal (heartbeat, lease renewal).
    pub expires_at: UnixMs,
    /// Admission + 12 h.
    pub max_until: UnixMs,
    /// Until when the game plane serves it.
    pub game_until: UnixMs,
    /// The database clock of the decision.
    pub now: UnixMs,
    /// A dedicated lobby's host session (OD-109): no game, no save line, no content binding.
    pub dedicated: bool,
    /// Whether the session holds its save line now (CR-S51): the game access ticket's `line`.
    pub save_line: bool,
}

impl Admitted {
    /// Hex of the declared car database, or empty.
    pub fn car_db_hex(&self) -> String {
        self.car_db
            .as_deref()
            .map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
            .unwrap_or_default()
    }

    /// The game access ticket's `cdb` (OD-113, GATE_API.md §3): the declared car database of a
    /// session bound to the content check (CR-S51: a Hardcore session with a save line, not
    /// dedicated; a Hardcore session is never modded), else empty, so the game plane compares no
    /// car database for it.
    pub fn ticket_car_db(&self) -> String {
        if !self.dedicated && self.save_line && self.mode == "hardcore" {
            self.car_db_hex()
        } else {
            String::new()
        }
    }

    /// `heartbeatEveryMs` of this session ([`heartbeat_every_ms`]).
    pub fn heartbeat_every_ms(&self) -> u32 {
        heartbeat_every_ms(&self.pool_tier, self.save_line, self.lease_ttl_ms)
    }

    /// Why the game plane ends before `max_until`, for `gamePlane.reason`.
    pub fn game_plane_reason(&self) -> &'static str {
        if self.game_until < self.max_until {
            "vip_ended"
        } else {
            "max_age"
        }
    }
}

/// The answer to a create or a poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Admitted; the lease is held.
    Active(Box<Admitted>),
    /// Waiting for a slot.
    Queued {
        /// The session.
        session_id: PlaySessionId,
        /// 1 = next.
        position: u32,
    },
}

/// What a launch asks for (`POST /launcher/v1/sessions`).
#[derive(Clone, Debug)]
pub struct CreateRequest<'a> {
    /// Minted by the launcher (also the `Idempotency-Key`).
    pub session_id: PlaySessionId,
    /// The signed-in account.
    pub account_id: Uuid,
    /// The profile to play.
    pub profile_id: ProfileId,
    /// The launcher install.
    pub device: &'a Device,
    /// Which pool serves each tier.
    pub routing: &'a PoolRouting,
    /// Launcher version.
    pub launcher_version: &'a str,
    /// The handoff nonce (32 lower-case hex digits), kept for the ticket of a later admission.
    pub nonce: &'a str,
    /// SHA-256 of the car database, when declared.
    pub car_db: Option<[u8; 32]>,
    /// Revoke a session open on another PC (the player confirmed).
    pub take_over: bool,
    /// The slot's length per pool tier (`FI_LEASE_TTL_MS`, `FI_FREE_LIVENESS_MS`).
    pub ttl: SlotTtl,
    /// The profile's game; the routed pool must serve it.
    pub game: &'a str,
    /// What the caller does with a session that would hold no lease.
    pub save_line: SaveLinePolicy,
    /// A dedicated lobby's host session (OD-109, GATE_API.md §2 "Dedicated sessions"):
    /// [`create_dedicated_in`] instead of the ordinary admission.
    pub dedicated: bool,
    /// A modded session (OD-112 follow-up, GATE_API.md §2 "Modded sessions"): self-declared,
    /// written with the row (`play_session.modded`), never changed.
    pub modded: bool,
}

/// What a launch does when its session would hold no save lease (OD-52: the profile keeps no
/// save line, or its line is retired).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveLinePolicy {
    /// The `/launcher/v1/sessions` aliases, whose body always carries a lease: such a launch is
    /// refused before anything is written (a retired line `SAVES.PROFILE_NOT_FOUND`, any other
    /// profile without a save line [`AdmissionError::NotEligible`]).
    Always,
    /// `/gate/v1`: the lease when the session keeps a save line (its tier and mode in the
    /// profile is Hardcore, OD-239, in any pool; its line not retired), else a session
    /// without one (the name predates OD-52, when every `vip` pool session held one).
    VipPools,
}

/// Whether a session holds the profile's save lease (OD-239, CR-S111): a Hardcore profile of a
/// game with a line, whatever the tier and the pool (`GameRules::line_mode`); a Noob (Azimuth)
/// profile never does.
fn holds_save_line(game: &str, mode: &str) -> bool {
    GameRules::of(game).line_mode(mode)
}

/// The profile's mode as stored now (`hardcore` | `noob`).
async fn profile_mode_in(
    conn: &mut PgConnection,
    profile: ProfileId,
) -> Result<String, AdmissionError> {
    sqlx::query_scalar("SELECT mode FROM profile WHERE id = $1")
        .bind(profile.as_uuid())
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(AdmissionError::NotFound)
}

/// One play session row with what every decision needs.
#[derive(Clone, Debug)]
struct SessionRow {
    id: PlaySessionId,
    account_id: Uuid,
    profile_id: ProfileId,
    pool: String,
    state: String,
    queued_at: Option<UnixMs>,
    expires_at: UnixMs,
    max_until: Option<UnixMs>,
    end_reason: Option<String>,
    car_db: Option<Vec<u8>>,
    nonce: Option<String>,
    xuid: u64,
    gamertag: String,
    game: String,
    mode: String,
    pool_tier: String,
    game_endpoint: String,
    device_id: DeviceId,
    device_label: String,
    save_line: bool,
    lease_ttl_ms: Option<u32>,
    last_seen_at: Option<UnixMs>,
    dedicated: bool,
    modded: bool,
}

const SESSION_SQL: &str = "SELECT s.id, s.account_id, s.profile_id, s.pool, s.state, s.queued_at, s.expires_at, \
    s.max_until, s.end_reason, s.car_db_sha256, s.launch_nonce, s.save_line, s.lease_ttl_ms, s.last_seen_at, s.dedicated, s.modded, p.xuid, p.name, s.game, p.mode, \
    pl.tier AS pool_tier, pl.game_endpoint, d.device_id, d.label AS device_label \
    FROM play_session s JOIN profile p ON p.id = s.profile_id JOIN pool pl ON pl.name = s.pool \
    JOIN device_binding d ON d.id = s.device_binding_id WHERE s.id = $1";

fn session_from_row(r: &PgRow) -> Result<SessionRow, AdmissionError> {
    let corrupt = || AdmissionError::Event("play_session row with a nil id".into());
    Ok(SessionRow {
        id: PlaySessionId::from_uuid(r.try_get("id")?).ok_or_else(corrupt)?,
        account_id: r.try_get("account_id")?,
        profile_id: ProfileId::from_uuid(r.try_get("profile_id")?).ok_or_else(corrupt)?,
        pool: r.try_get("pool")?,
        state: r.try_get("state")?,
        queued_at: r.try_get::<Option<OffsetDateTime>, _>("queued_at")?.map(ms),
        expires_at: ms(r.try_get("expires_at")?),
        max_until: r.try_get::<Option<OffsetDateTime>, _>("max_until")?.map(ms),
        end_reason: r.try_get("end_reason")?,
        car_db: r.try_get("car_db_sha256")?,
        nonce: r.try_get("launch_nonce")?,
        xuid: r.try_get::<i64, _>("xuid")? as u64,
        gamertag: r.try_get::<Option<String>, _>("name")?.unwrap_or_default(),
        game: r.try_get("game")?,
        mode: r.try_get("mode")?,
        pool_tier: r.try_get("pool_tier")?,
        game_endpoint: r.try_get("game_endpoint")?,
        device_id: DeviceId::from_uuid(r.try_get("device_id")?).ok_or_else(corrupt)?,
        device_label: r.try_get("device_label")?,
        save_line: r.try_get("save_line")?,
        lease_ttl_ms: r
            .try_get::<Option<i32>, _>("lease_ttl_ms")?
            .map(|t| t as u32),
        last_seen_at: r
            .try_get::<Option<OffsetDateTime>, _>("last_seen_at")?
            .map(ms),
        dedicated: r.try_get("dedicated")?,
        modded: r.try_get("modded")?,
    })
}

async fn load_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
) -> Result<Option<SessionRow>, AdmissionError> {
    sqlx::query(SESSION_SQL)
        .bind(session.as_uuid())
        .fetch_optional(&mut *conn)
        .await?
        .as_ref()
        .map(session_from_row)
        .transpose()
}

/// The session of `account`, with its pool locked and the database clock read after the lock.
/// Locks a session's decision: with `admits` (a poll, a heartbeat) the game's lock first when the
/// session's first read could lead to an admission (`queued` for a poll, `expired` for a heartbeat,
/// OD-71); a leave never admits and takes the pool's alone.
async fn lock_session_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    admits: Option<&str>,
) -> Result<(SessionRow, UnixMs), AdmissionError> {
    let first = load_in(conn, session)
        .await?
        .filter(|s| s.account_id == account_id)
        .ok_or(AdmissionError::NotFound)?;
    // A decision that may admit (a queued session's poll, an expired one's heartbeat) holds the
    // game's lock first (OD-71); an active session's heartbeat or a leave the pool's alone.
    if admits == Some(first.state.as_str()) {
        let name = PoolName::new(&first.pool).map_err(|_| AdmissionError::PoolClosed)?;
        sessions::lock_game_of(conn, &name).await?;
    }
    lock_pool_named(conn, &first.pool).await?;
    // Then the row: the sweeper locks the rows it closes (SKIP LOCKED), so a heartbeat or a leave
    // and the sweeper decide one after the other, each on what the other committed.
    sqlx::query("SELECT 1 FROM play_session WHERE id = $1 FOR UPDATE")
        .bind(session.as_uuid())
        .execute(&mut *conn)
        .await?;
    let now = crate::identity::now_in(conn).await?;
    // Re-read under the lock: another decision may have changed the row while this one waited.
    let row = load_in(conn, session)
        .await?
        .ok_or(AdmissionError::NotFound)?;
    Ok((row, now))
}

/// The game's lock for an admission decided under a pool lock taken without it (the session's
/// state changed between the first read and the lock): without waiting, [`AdmissionError::Busy`]
/// (retry) when another decision of the game holds it. Held already: a no-op.
async fn game_lock_late_in(conn: &mut PgConnection, pool: &str) -> Result<(), AdmissionError> {
    let name = PoolName::new(pool).map_err(|_| AdmissionError::PoolClosed)?;
    if sessions::try_lock_game_of(conn, &name).await? {
        Ok(())
    } else {
        Err(AdmissionError::Busy)
    }
}

async fn lock_pool_named(conn: &mut PgConnection, pool: &str) -> Result<(), AdmissionError> {
    let name = PoolName::new(pool).map_err(|_| AdmissionError::PoolClosed)?;
    sessions::lock_pool(conn, &name).await?;
    Ok(())
}

/// `game_until` (ACCOUNTS.md §7): `max_until` on a free pool; on a VIP pool `min(access end + 2 h,
/// max_until)`, and at once for suspended or revoked access.
pub fn game_until_for(
    pool_tier: &str,
    access: &AccessRow,
    now: UnixMs,
    max_until: UnixMs,
) -> UnixMs {
    if pool_tier != "vip" {
        return max_until;
    }
    if access.status == "suspended" || access.status.starts_with("revoked") {
        return now.min(max_until);
    }
    match access.access_until {
        Some(until) => until.plus_millis(VIP_GRACE_MS).min(max_until),
        None => now.min(max_until),
    }
}

fn tier_of(access: &AccessRow) -> Tier {
    if access.vip { Tier::Vip } else { Tier::Free }
}

/// The slot an admitted session holds: its save lease (if it has a save line) and its expiry.
#[derive(Clone, Debug)]
struct Slot {
    lease: Option<Lease>,
    ttl_ms: u32,
    expires_at: UnixMs,
}

impl Slot {
    fn leased(lease: Lease) -> Self {
        Self {
            ttl_ms: lease.ttl_ms,
            expires_at: lease.expires_at,
            lease: Some(lease),
        }
    }

    fn bare(ttl_ms: u32, now: UnixMs) -> Self {
        Self {
            lease: None,
            ttl_ms,
            expires_at: now.plus_millis(i64::from(ttl_ms)),
        }
    }
}

fn admitted(
    row: &SessionRow,
    slot: Slot,
    access: &AccessRow,
    now: UnixMs,
    (max_until, game_until): (UnixMs, UnixMs),
) -> Admitted {
    let save_line = slot.lease.is_some();
    Admitted {
        session_id: row.id,
        account_id: row.account_id,
        profile_id: row.profile_id,
        xuid: row.xuid,
        gamertag: row.gamertag.clone(),
        game: row.game.clone(),
        mode: row.mode.clone(),
        pool: row.pool.clone(),
        pool_tier: row.pool_tier.clone(),
        game_endpoint: row.game_endpoint.clone(),
        tier: tier_of(access),
        access_until: access.access_until,
        car_db: row.car_db.clone(),
        nonce: row.nonce.clone(),
        lease: slot.lease,
        lease_ttl_ms: slot.ttl_ms,
        expires_at: slot.expires_at,
        max_until,
        game_until,
        now,
        dedicated: row.dedicated,
        save_line,
    }
}

/// What a session that was just closed ends, in the caller's transaction: the worlds it hosts
/// close (`session_ended`, OD-57: nobody stays in a world without its host), its profile's seats
/// are released, and its `session_summary` and the profile's `profile_progress` are written
/// (`fi_db::progression::session_ended_guarded`: a read-model failure never blocks the close; no outbox, owner 2026-10-26).
async fn ended_in(conn: &mut PgConnection, session: Uuid) -> Result<(), AdmissionError> {
    crate::matchmaking::end_session_in(conn, session).await?;
    let r = sqlx::query(
        "SELECT profile_id, end_revision FROM play_session WHERE id = $1 AND ended_at IS NOT NULL",
    )
    .bind(session)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(r) = r {
        let profile: Uuid = r.try_get("profile_id")?;
        let end: Option<i64> = r.try_get("end_revision")?;
        crate::progression::session_ended_guarded(conn, session, profile, end).await?;
    }
    Ok(())
}

/// Closes a live session (`queued` or `active`) as `state` with `reason` and writes its
/// session summary. `end_revision` defaults to the line's head. `false` when it was not live.
async fn close_in(
    conn: &mut PgConnection,
    session: Uuid,
    state: &str,
    reason: &str,
    end_revision: Option<u64>,
) -> Result<bool, AdmissionError> {
    let closed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE play_session s SET state = $2, ended_at = clock_timestamp(), end_reason = $3, \
         end_revision = COALESCE($4, (SELECT h.revision FROM save_head h WHERE h.profile_id = s.profile_id)) \
         WHERE s.id = $1 AND s.state IN ('queued', 'active') RETURNING s.id",
    )
    .bind(session)
    .bind(state)
    .bind(reason)
    .bind(end_revision.map(|r| r as i64))
    .fetch_optional(&mut *conn)
    .await?;
    if closed.is_some() {
        ended_in(conn, session).await?;
    }
    Ok(closed.is_some())
}

/// What a reaper pass (or an admission's cleanup) closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reaped {
    /// Active sessions whose lease lapsed on both signals (`expired`).
    pub expired: u32,
    /// Queued sessions not polled for 90 s (`queue_left`).
    pub queue_left: u32,
    /// Active sessions past `max_until` (`max_age`).
    pub max_age: u32,
    /// Active sessions with no signal for [`SILENCE_CEILING_MS`] whatever their lease (`expired`).
    pub silent: u32,
}

impl Reaped {
    pub(crate) fn add(&mut self, other: Reaped) {
        self.expired += other.expired;
        self.queue_left += other.queue_left;
        self.max_age += other.max_age;
        self.silent += other.silent;
    }

    pub(crate) fn count(&mut self, stale: Stale) {
        match stale {
            Stale::MaxAge => self.max_age += 1,
            Stale::Silent => self.silent += 1,
            Stale::Expired => self.expired += 1,
            Stale::QueueLeft => self.queue_left += 1,
        }
    }

    /// Sessions closed in all.
    pub fn total(&self) -> u32 {
        self.expired + self.queue_left + self.max_age + self.silent
    }
}

/// Why a live session is stale, in the order a pass closes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stale {
    /// Active past `max_until`.
    MaxAge,
    /// Active with no heartbeat and no game request for the ceiling, whatever the lease.
    Silent,
    /// Active whose slot lapsed on both signals (`greatest(last_heartbeat_at, last_seen_at) +
    /// lease`) and that holds no live save lease (a commit extends the lease, not the row).
    Expired,
    /// Queued and not polled for 90 s.
    QueueLeft,
}

impl Stale {
    pub(crate) const ALL: [Stale; 4] = [
        Stale::MaxAge,
        Stale::Silent,
        Stale::Expired,
        Stale::QueueLeft,
    ];

    /// The predicate over `play_session s`, by the database clock. Re-evaluated by the closing
    /// UPDATE itself, so a signal committed after the candidate was read always wins.
    pub(crate) fn predicate(self) -> String {
        let lease_live = "EXISTS (SELECT 1 FROM save_head h WHERE h.profile_id = s.profile_id \
                          AND h.lease_session_id = s.id AND h.lease_expires_at > clock_timestamp())";
        let ttl = format!(
            "make_interval(secs => COALESCE(s.lease_ttl_ms, {}) / 1000.0)",
            fi_saves::lease::DEFAULT_TTL_MS
        );
        match self {
            Stale::MaxAge => "s.state = 'active' AND s.max_until <= clock_timestamp()".to_owned(),
            Stale::Silent => format!(
                "s.state = 'active' AND greatest(s.last_heartbeat_at, COALESCE(s.last_seen_at, s.last_heartbeat_at)) \
                 + make_interval(secs => {}) <= clock_timestamp()",
                SILENCE_CEILING_MS / 1000
            ),
            Stale::Expired => format!(
                "s.state = 'active' AND s.expires_at <= clock_timestamp() \
                 AND (s.last_seen_at IS NULL OR s.last_seen_at + {ttl} <= clock_timestamp()) AND NOT {lease_live}"
            ),
            Stale::QueueLeft => {
                "s.state = 'queued' AND s.expires_at <= clock_timestamp()".to_owned()
            }
        }
    }

    /// The predicate of an `active` session that still holds its slot (the negation of
    /// [`Stale::MaxAge`] and [`Stale::Expired`]): what occupancy and the public queue count.
    pub(crate) fn live_active() -> String {
        let ttl = format!(
            "make_interval(secs => COALESCE(s.lease_ttl_ms, {}) / 1000.0)",
            fi_saves::lease::DEFAULT_TTL_MS
        );
        format!(
            "s.state = 'active' AND s.max_until > clock_timestamp() AND (s.expires_at > clock_timestamp() \
             OR s.last_seen_at + {ttl} > clock_timestamp() \
             OR EXISTS (SELECT 1 FROM save_head h WHERE h.profile_id = s.profile_id AND h.lease_session_id = s.id \
                        AND h.lease_expires_at > clock_timestamp()))"
        )
    }

    /// [`Self::live_active`] without dedicated sessions (OD-109): what every active-session
    /// count reads (occupancy, the public queue summary, the online load).
    pub(crate) fn counted_active() -> String {
        format!("NOT s.dedicated AND {}", Self::live_active())
    }

    fn closed_as(self) -> (&'static str, &'static str) {
        match self {
            Stale::MaxAge => ("ended", "max_age"),
            Stale::Silent | Stale::Expired => ("expired", "expired"),
            Stale::QueueLeft => ("ended", "queue_left"),
        }
    }
}

/// Closes `session` as `stale` if it still is (the predicate is re-read by this statement) and
/// writes its session summary. `false` when it was not (a signal arrived, or it was closed).
pub(crate) async fn close_if_stale_in(
    conn: &mut PgConnection,
    session: Uuid,
    stale: Stale,
) -> Result<bool, AdmissionError> {
    let (state, reason) = stale.closed_as();
    let sql = format!(
        "UPDATE play_session s SET state = $2, ended_at = clock_timestamp(), end_reason = $3, \
         end_revision = (SELECT h.revision FROM save_head h WHERE h.profile_id = s.profile_id) \
         WHERE s.id = $1 AND {} RETURNING s.id",
        stale.predicate()
    );
    let closed: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(session)
        .bind(state)
        .bind(reason)
        .fetch_optional(&mut *conn)
        .await?;
    if closed.is_some() {
        ended_in(conn, session).await?;
    }
    Ok(closed.is_some())
}

/// Ends an active session as `idle` (OD-121 enforcement, GATE_API.md §4a) with its
/// session summary, like the sweeper's closes: the caller holds the session's row lock; the lease
/// is left to lapse. `false` when it was not active any more.
pub(crate) async fn close_idle_in(
    conn: &mut PgConnection,
    session: Uuid,
) -> Result<bool, crate::identity::IdentityError> {
    let closed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE play_session s SET state = 'ended', ended_at = clock_timestamp(), end_reason = 'idle', \
         end_revision = (SELECT h.revision FROM save_head h WHERE h.profile_id = s.profile_id) \
         WHERE s.id = $1 AND s.state = 'active' RETURNING s.id",
    )
    .bind(session)
    .fetch_optional(&mut *conn)
    .await?;
    if closed.is_some() {
        ended_in(conn, session)
            .await
            .map_err(|e| crate::identity::IdentityError::Event(e.to_string()))?;
    }
    Ok(closed.is_some())
}

/// Whose stale sessions to close.
enum Scope<'a> {
    Pool(&'a str),
    Account(Uuid),
    /// Every pool of a game (OD-71: a stale session of another pool holds a game slot).
    Game(&'a str),
    /// Every pool of every game (OD-228: a stale session of another game holds a global slot).
    All,
}

/// Closes stale sessions of `scope` ([`Stale`]), each with its session summary. The lease itself
/// is left as it is: a lapsed lease can still be resumed by its session while nobody else took the
/// line (SAVES_API.md §3.2).
async fn close_stale_in(
    conn: &mut PgConnection,
    scope: Scope<'_>,
) -> Result<Reaped, AdmissionError> {
    let (pool, account, game) = match scope {
        Scope::Pool(p) => (Some(p.to_owned()), None, None),
        Scope::Account(a) => (None, Some(a), None),
        Scope::Game(g) => (None, None, Some(g.to_owned())),
        Scope::All => (None, None, None),
    };
    let mut reaped = Reaped::default();
    for stale in Stale::ALL {
        // SKIP LOCKED: a row the sweeper (or another decision) holds is being decided now; this
        // admission never waits for it under the pool lock. At worst a stale row still counts
        // for a moment, so the error is a queued launch, never an overfilled pool.
        let sql = format!(
            "SELECT s.id FROM play_session s WHERE ($1::text IS NULL OR s.pool = $1) \
             AND ($2::uuid IS NULL OR s.account_id = $2) AND ($3::text IS NULL OR s.game = $3) \
             AND {} FOR UPDATE OF s SKIP LOCKED",
            stale.predicate()
        );
        let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(pool.as_deref())
            .bind(account)
            .bind(game.as_deref())
            .fetch_all(&mut *conn)
            .await?;
        for id in ids {
            if close_if_stale_in(conn, id, stale).await? {
                reaped.count(stale);
            }
        }
    }
    Ok(reaped)
}

/// Active and queued sessions of a pool (after [`close_stale_in`]).
pub(crate) async fn counts_in(
    conn: &mut PgConnection,
    game: &str,
    pool: &str,
) -> Result<(i64, i64), AdmissionError> {
    let row = sqlx::query(
        "SELECT count(*) FILTER (WHERE state = 'active') AS active, count(*) FILTER (WHERE state = 'queued') AS queued \
         FROM play_session WHERE game = $1 AND pool = $2 AND state IN ('active', 'queued') AND NOT dedicated",
    )
    .bind(game)
    .bind(pool)
    .fetch_one(&mut *conn)
    .await?;
    Ok((row.try_get("active")?, row.try_get("queued")?))
}

/// Position of a queued session (1 = next), FIFO by `queued_at` then id.
async fn position_in(conn: &mut PgConnection, row: &SessionRow) -> Result<u32, AdmissionError> {
    let ahead: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM play_session WHERE game = $1 AND pool = $2 AND state = 'queued' AND (queued_at, id) < ($3, $4)",
    )
    .bind(&row.game)
    .bind(&row.pool)
    .bind(row.queued_at.map(ts))
    .bind(row.id.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    Ok(ahead as u32 + 1)
}

struct PoolRow {
    game: String,
    tier: String,
    capacity: Option<i64>,
    queue_limit: i64,
    accepting: bool,
}

async fn pool_in(conn: &mut PgConnection, name: &str) -> Result<Option<PoolRow>, AdmissionError> {
    let row = sqlx::query(
        "SELECT game, tier, capacity, queue_limit, accepting FROM pool WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(PoolRow {
            game: r.try_get("game")?,
            tier: r.try_get("tier")?,
            capacity: r.try_get::<Option<i32>, _>("capacity")?.map(i64::from),
            queue_limit: i64::from(r.try_get::<i32, _>("queue_limit")?),
            accepting: r.try_get("accepting")?,
        })
    })
    .transpose()
}

fn has_slot(capacity: Option<i64>, active: i64) -> bool {
    capacity.is_none_or(|c| active < c)
}

/// A game's online capacity and what holds it (OD-71, GATE_API.md §2 "Online capacity"), with the
/// global online capacity across every game (OD-228), read under the global and the game's locks
/// after the stale sessions they count were closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GameLoad {
    /// `game.online_capacity` (`None`: unlimited).
    capacity: Option<i64>,
    /// Active sessions of the game, every pool.
    active: i64,
    /// `global_online_capacity.capacity` (`None`: unlimited).
    global_capacity: Option<i64>,
    /// Active sessions of every game.
    global_active: i64,
}

impl GameLoad {
    /// Slots the game can give a launch now, whatever its tier (OD-129: no Supporter priority):
    /// the smaller of the game's room and the global room.
    fn room(&self, _tier: &str) -> i64 {
        let game = self.capacity.map_or(i64::MAX, |c| c - self.active);
        let global = self
            .global_capacity
            .map_or(i64::MAX, |c| c - self.global_active);
        game.min(global)
    }
}

/// The global online capacity (OD-228): `None` unlimited (also without the singleton row).
async fn global_capacity_in(conn: &mut PgConnection) -> Result<Option<i64>, AdmissionError> {
    let capacity: Option<Option<i32>> =
        sqlx::query_scalar("SELECT capacity FROM global_online_capacity WHERE singleton")
            .fetch_optional(&mut *conn)
            .await?;
    Ok(capacity.flatten().map(i64::from))
}

/// Closes the stale sessions a decision on `pool` must not count: every game's under a global
/// online capacity (OD-228: a silent session of another game would hold a global slot until the
/// hourly sweep), the whole game's when it has an online capacity (a silent session of another
/// pool would hold a game slot), else the pool's alone, as before OD-71.
async fn close_stale_for_in(
    conn: &mut PgConnection,
    game: &str,
    pool: &str,
) -> Result<(), AdmissionError> {
    if global_capacity_in(conn).await?.is_some() {
        close_stale_in(conn, Scope::All).await?;
        return Ok(());
    }
    let capped: Option<bool> =
        sqlx::query_scalar("SELECT online_capacity IS NOT NULL FROM game WHERE code = $1")
            .bind(game)
            .fetch_optional(&mut *conn)
            .await?;
    if capped == Some(true) {
        close_stale_in(conn, Scope::Game(game)).await?;
    } else {
        close_stale_in(conn, Scope::Pool(pool)).await?;
    }
    Ok(())
}

/// [`GameLoad`] of `game`, without the sessions `excluded` (a launch's replaced sessions, counted as
/// already closed: review OD-71 finding 1).
async fn game_load_in(
    conn: &mut PgConnection,
    game: &str,
    excluded: &[Uuid],
) -> Result<GameLoad, AdmissionError> {
    let capacity: Option<i32> =
        sqlx::query_scalar("SELECT online_capacity FROM game WHERE code = $1")
            .bind(game)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(AdmissionError::PoolClosed)?;
    // OD-228: the global cap, counted over every game without the excluded sessions.
    let global_capacity = global_capacity_in(conn).await?;
    let global_active: i64 = if global_capacity.is_some() {
        sqlx::query_scalar(
            "SELECT count(*) FROM play_session WHERE state = 'active' AND NOT dedicated AND id <> ALL($1)",
        )
        .bind(excluded)
        .fetch_one(&mut *conn)
        .await?
    } else {
        0
    };
    // An uncapped game: nothing to count for it (and no game lock was taken).
    if capacity.is_none() {
        return Ok(GameLoad {
            capacity: None,
            active: 0,
            global_capacity,
            global_active,
        });
    }
    let r = sqlx::query(
        "SELECT g.online_capacity, \
           (SELECT count(*) FROM play_session s WHERE s.game = g.code AND s.state = 'active' \
              AND NOT s.dedicated AND s.id <> ALL($2)) AS active \
         FROM game g WHERE g.code = $1",
    )
    .bind(game)
    .bind(excluded)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AdmissionError::PoolClosed)?;
    Ok(GameLoad {
        capacity: r
            .try_get::<Option<i32>, _>("online_capacity")?
            .map(i64::from),
        active: r.try_get("active")?,
        global_capacity,
        global_active,
    })
}

/// The access row of an account (the entitlement stand-in locally), read in the transaction.
async fn access_of(conn: &mut PgConnection, account: Uuid) -> Result<AccessRow, AdmissionError> {
    Ok(accounts::access_in(conn, account).await?)
}

/// The lease `session` holds on its line, as stored (possibly lapsed), if it still holds it.
async fn stored_lease_in(
    conn: &mut PgConnection,
    profile: ProfileId,
    session: PlaySessionId,
) -> Result<Option<(FenceToken, Lease)>, AdmissionError> {
    let row = sqlx::query(
        "SELECT lease_token, lease_device_id, lease_device_label, lease_acquired_at, lease_expires_at, lease_ttl_ms, \
         lease_accounted_at FROM save_head WHERE profile_id = $1 AND lease_session_id = $2",
    )
    .bind(profile.as_uuid())
    .bind(session.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let token = FenceToken::new(r.try_get::<i64, _>("lease_token")? as u64)
        .ok_or_else(|| AdmissionError::Event("save_head lease token 0".into()))?;
    let lease = Lease {
        holder: LeaseHolder {
            session,
            device: DeviceId::from_uuid(r.try_get("lease_device_id")?)
                .ok_or_else(|| AdmissionError::Event("save_head lease device nil".into()))?,
            device_label: r.try_get("lease_device_label")?,
        },
        token,
        acquired_at: ms(r.try_get("lease_acquired_at")?),
        expires_at: ms(r.try_get("lease_expires_at")?),
        ttl_ms: r.try_get::<i32, _>("lease_ttl_ms")? as u32,
        accounted_at: ms(r.try_get("lease_accounted_at")?),
    };
    Ok(Some((token, lease)))
}

/// Releases the lease a replaced session still holds (its writes are fenced at once).
async fn release_held_in(
    conn: &mut PgConnection,
    profile: ProfileId,
    session: PlaySessionId,
) -> Result<(), AdmissionError> {
    if let Some((token, _)) = stored_lease_in(conn, profile, session).await? {
        saves::release_in(conn, profile, session, token).await?;
    }
    Ok(())
}

/// Whether the profile's save line is retired (OD-52, SAVES_API.md §8).
async fn line_retired_in(
    conn: &mut PgConnection,
    profile: ProfileId,
) -> Result<bool, AdmissionError> {
    Ok(
        sqlx::query_scalar("SELECT retired_at IS NOT NULL FROM save_head WHERE profile_id = $1")
            .bind(profile.as_uuid())
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(false),
    )
}

async fn head_revision_in(
    conn: &mut PgConnection,
    profile: ProfileId,
) -> Result<i64, AdmissionError> {
    Ok(
        sqlx::query_scalar("SELECT revision FROM save_head WHERE profile_id = $1")
            .bind(profile.as_uuid())
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(0),
    )
}

fn holder(row: &SessionRow) -> LeaseHolder {
    LeaseHolder {
        session: row.id,
        device: row.device_id,
        device_label: row.device_label.clone(),
    }
}

fn in_use(e: SaveError) -> AdmissionError {
    match e {
        SaveError::LeaseHeld {
            device_label,
            expires_at,
            ..
        } => AdmissionError::InUseElsewhere {
            device_label,
            since: expires_at,
        },
        other => AdmissionError::Save(other),
    }
}

/// Grants the slot (and the lease, for a session with a save line) and marks the session `active`
/// (admission of a queued session).
async fn activate_in(
    conn: &mut PgConnection,
    row: &SessionRow,
    mode: AcquireMode,
    ttl: SlotTtl,
    now: UnixMs,
) -> Result<Admitted, AdmissionError> {
    // OD-52 at this decision's clock: the account may have lapsed while the session waited.
    let access = access_of(conn, row.account_id).await?;
    let save_line = row.save_line && holds_save_line(&row.game, &row.mode);
    // CR-S51: a session with a line holds the lease's slot whatever its pool.
    let ttl_ms = ttl.for_session(&row.pool_tier, save_line);
    let slot = if save_line {
        let grant = saves::acquire_in(
            conn,
            row.profile_id,
            &AcquireRequest {
                holder: holder(row),
                ttl_ms,
                mode,
            },
        )
        .await
        .map_err(in_use)?;
        cloud::cover_in(conn, row.profile_id, access.access_until, now, true).await?;
        Slot::leased(grant.lease)
    } else {
        Slot::bare(ttl_ms, now)
    };
    let max_until = now.plus_millis(MAX_AGE_MS);
    let game_until = game_until_for(&row.pool_tier, &access, now, max_until);
    let start = head_revision_in(conn, row.profile_id).await?;
    sqlx::query(
        "UPDATE play_session SET state = 'active', admitted_at = $2, last_heartbeat_at = $2, expires_at = $3, \
         max_until = $4, game_until = $5, start_revision = $6, lease_ttl_ms = $7, save_line = $8 WHERE id = $1",
    )
    .bind(row.id.as_uuid())
    .bind(ts(now))
    .bind(ts(slot.expires_at))
    .bind(ts(max_until))
    .bind(ts(game_until))
    .bind(start)
    .bind(slot.ttl_ms as i32)
    .bind(save_line)
    .execute(&mut *conn)
    .await?;
    // CR-S63: a queued profile's first admission is announced (`player_joined`), once.
    crate::discord::first_admission_in(conn, row.profile_id.as_uuid()).await?;
    Ok(admitted(row, slot, &access, now, (max_until, game_until)))
}

/// A live session of the account that a new launch meets.
struct Live {
    id: Uuid,
    profile_id: ProfileId,
    pool: String,
    state: String,
    device_id: Uuid,
    device_label: String,
    since: UnixMs,
}

/// `POST /launcher/v1/sessions` (SAVES_API.md §3.1, ACCOUNTS.md §7) and the gate's admission
/// (GATE_API.md §2). Idempotent by the session id: a replay answers as [`poll_in`] does.
pub async fn create_in(
    conn: &mut PgConnection,
    req: &CreateRequest<'_>,
) -> Result<Outcome, AdmissionError> {
    crash_access_in(conn, req.account_id).await?;
    kick_access_in(conn, req.account_id).await?;
    if req.dedicated {
        return create_dedicated_in(conn, req).await;
    }
    if let Some(existing) = load_in(conn, req.session_id).await? {
        return replay_in(conn, &existing, req).await;
    }
    // The tier decides the pool (by the database clock), before any lock: a plain read. The
    // profile's mode, with it, decides the save line (OD-52), read under the profile's lock below.
    let access = access_of(conn, req.account_id).await?;
    let pool_name = &GameRules::of(req.game).pool(req.routing, access.vip);
    let pool = pool_in(conn, pool_name.as_str())
        .await?
        .filter(|p| p.accepting && p.game == req.game)
        .ok_or(AdmissionError::PoolClosed)?;
    sessions::lock_game_of(conn, pool_name).await?;
    sessions::lock_pool(conn, pool_name).await?;
    // A concurrent first create with this id committed while this one waited for the lock.
    if let Some(existing) = load_in(conn, req.session_id).await? {
        return replay_in(conn, &existing, req).await;
    }
    // OD-52: the profile's row, shared. A Hardcore -> Noob change (fi_db::purge) holds it
    // FOR NO KEY UPDATE while it looks for this profile's sessions and retires the line, so a
    // launch either commits first (and the change refuses it) or runs after the change and sees
    // the retired line, read by a later statement (READ COMMITTED: after the lock). The account's
    // row first, in the KEY SHARE the session insert's foreign key takes anyway: an account write
    // (accounts.rs: account FOR UPDATE, then its profiles) then makes this wait before it holds
    // the profile, never after. Order: pool, account, profile, session rows, heads.
    sqlx::query("SELECT 1 FROM account WHERE id = $1 FOR KEY SHARE")
        .bind(req.account_id)
        .execute(&mut *conn)
        .await?;
    // CR-S62: a kick that committed while this waited for the account's row (its FOR UPDATE)
    // set the cooldown: read again under the lock.
    kick_access_in(conn, req.account_id).await?;
    // A profile deletion (fi_db::erasure::delete_profile_in, CR-S61) that committed while this
    // waited for the lock left a tombstone: read under the lock, refused.
    let live: Option<bool> =
        sqlx::query_scalar("SELECT status <> 'deleted' FROM profile WHERE id = $1 FOR SHARE")
            .bind(req.profile_id.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    if live != Some(true) {
        return Err(AdmissionError::NotFound);
    }
    let retired = line_retired_in(conn, req.profile_id).await?;
    // A retired line takes no lease (SAVES_API.md §8): a body that always carries one refuses
    // the launch before anything is written; a gate session plays without a save line.
    if retired && req.save_line == SaveLinePolicy::Always {
        return Err(AdmissionError::Save(SaveError::ProfileNotFound));
    }
    // OD-239: only a Hardcore profile keeps a save line; the same
    // refusal for a body that always carries a lease (the mode read after the profile's lock).
    let profile_mode = profile_mode_in(conn, req.profile_id).await?;
    let eligible = holds_save_line(req.game, &profile_mode);
    if !eligible && req.save_line == SaveLinePolicy::Always {
        return Err(AdmissionError::NotEligible {
            tier: cloud::tier(&access),
            mode: profile_mode,
        });
    }
    let now = crate::identity::now_in(conn).await?;
    close_stale_in(conn, Scope::Account(req.account_id)).await?;
    close_stale_for_in(conn, req.game, pool_name.as_str()).await?;

    // One live session per account (ACCOUNTS.md §7): the same device replaces its own session
    // silently (a crashed launcher), anything else needs the player's takeover. The rows are
    // locked before any save head, the order every other decision takes (pool, session row,
    // head), so a takeover from another pool never deadlocks with the replaced session's
    // heartbeat.
    let rows = sqlx::query(
        "SELECT s.id, s.profile_id, s.pool, s.state, d.device_id, d.label, COALESCE(s.admitted_at, s.created_at) AS since \
         FROM play_session s JOIN device_binding d ON d.id = s.device_binding_id \
         WHERE s.account_id = $1 AND s.state IN ('queued', 'active') AND NOT s.dedicated \
         ORDER BY s.created_at FOR UPDATE OF s",
    )
    .bind(req.account_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut replaced = Vec::new();
    for r in &rows {
        let live = Live {
            id: r.try_get("id")?,
            profile_id: ProfileId::from_uuid(r.try_get("profile_id")?)
                .ok_or(AdmissionError::NotFound)?,
            pool: r.try_get("pool")?,
            state: r.try_get("state")?,
            device_id: r.try_get("device_id")?,
            device_label: r.try_get("label")?,
            since: ms(r.try_get("since")?),
        };
        let same_device = live.device_id == req.device.device_id.as_uuid();
        if !(same_device || req.take_over) {
            return Err(AdmissionError::InUseElsewhere {
                device_label: live.device_label,
                since: live.since,
            });
        }
        replaced.push(live);
    }
    let (active, queued) = counts_in(conn, req.game, pool_name.as_str()).await?;
    let freed = |state: &str| {
        replaced
            .iter()
            .filter(|l| l.pool == pool_name.as_str() && l.state == state)
            .count() as i64
    };
    let (active, queued) = (active - freed("active"), queued - freed("queued"));
    // OD-71 and OD-228: the game's and the global online capacity, without the sessions this
    // launch replaces.
    let replaced_ids: Vec<Uuid> = replaced.iter().map(|l| l.id).collect();
    let load = game_load_in(conn, req.game, &replaced_ids).await?;
    let admit = has_slot(pool.capacity, active) && queued == 0 && load.room(&pool.tier) >= 1;
    if !admit && queued >= pool.queue_limit {
        return Err(AdmissionError::PoolFull);
    }
    // OD-52: the lease only for a session that keeps a save line, on a line not retired.
    let save_line = eligible && !retired;
    // The slot's length (OD-40, CR-S51): the lease's with a save line in any pool and on `vip`
    // with or without one, else the free window.
    let ttl_ms = req.ttl.for_session(&pool.tier, save_line);

    // Leases first (pool, then heads), then the session rows.
    let session = req.session_id;
    let mut grant = None;
    if admit && save_line {
        let mode = if req.take_over {
            AcquireMode::Takeover
        } else {
            AcquireMode::Normal
        };
        let g = saves::acquire_in(
            conn,
            req.profile_id,
            &AcquireRequest {
                holder: LeaseHolder {
                    session,
                    device: req.device.device_id,
                    device_label: req.device.label.clone(),
                },
                ttl_ms,
                mode,
            },
        )
        .await
        .map_err(in_use)?;
        cloud::cover_in(conn, req.profile_id, access.access_until, now, true).await?;
        grant = Some(g);
    }
    for live in &replaced {
        let live_session = PlaySessionId::from_uuid(live.id).ok_or(AdmissionError::NotFound)?;
        // The new grant already fenced a replaced session of this profile; any other lease a
        // replaced session holds is released so its writes stop now.
        if !(grant.is_some() && live.profile_id == req.profile_id) {
            release_held_in(conn, live.profile_id, live_session).await?;
        }
        close_in(conn, live.id, "ended", "takeover", None).await?;
    }
    let car_db = req.car_db.map(|d| d.to_vec());
    if admit {
        let slot = match grant {
            Some(g) => Slot::leased(g.lease),
            None => Slot::bare(ttl_ms, now),
        };
        let max_until = now.plus_millis(MAX_AGE_MS);
        let game_until = game_until_for(&pool.tier, &access, now, max_until);
        let start = head_revision_in(conn, req.profile_id).await?;
        sqlx::query(
            "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, game, state, launcher_version, \
             car_db_sha256, admitted_at, last_heartbeat_at, expires_at, max_until, game_until, start_revision, launch_nonce, \
             save_line, lease_ttl_ms, modded) \
             VALUES ($1, $2, $3, $4, $5, $6, 'active', $7, $8, $9, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(session.as_uuid())
        .bind(req.account_id)
        .bind(req.profile_id.as_uuid())
        .bind(req.device.binding_id)
        .bind(pool_name.as_str())
        .bind(req.game)
        .bind(req.launcher_version)
        .bind(&car_db)
        .bind(ts(now))
        .bind(ts(slot.expires_at))
        .bind(ts(max_until))
        .bind(ts(game_until))
        .bind(start)
        .bind(req.nonce)
        .bind(save_line)
        .bind(slot.ttl_ms as i32)
        .bind(req.modded)
        .execute(&mut *conn)
        .await?;
        // CR-S63: the profile's first admission is announced (`player_joined`), once.
        crate::discord::first_admission_in(conn, req.profile_id.as_uuid()).await?;
        let row = load_in(conn, session)
            .await?
            .ok_or(AdmissionError::NotFound)?;
        return Ok(Outcome::Active(Box::new(admitted(
            &row,
            slot,
            &access,
            now,
            (max_until, game_until),
        ))));
    }
    sqlx::query(
        "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, game, state, launcher_version, \
         car_db_sha256, queued_at, last_heartbeat_at, expires_at, launch_nonce, save_line, lease_ttl_ms, modded) \
         VALUES ($1, $2, $3, $4, $5, $6, 'queued', $7, $8, $9, $9, $10, $11, $12, $13, $14)",
    )
    .bind(session.as_uuid())
    .bind(req.account_id)
    .bind(req.profile_id.as_uuid())
    .bind(req.device.binding_id)
    .bind(pool_name.as_str())
    .bind(req.game)
    .bind(req.launcher_version)
    .bind(&car_db)
    .bind(ts(now))
    .bind(ts(now.plus_millis(QUEUE_TTL_MS)))
    .bind(req.nonce)
    .bind(save_line)
    .bind(ttl_ms as i32)
    .bind(req.modded)
    .execute(&mut *conn)
    .await?;
    Ok(Outcome::Queued {
        session_id: session,
        position: queued as u32 + 1,
    })
}

/// A create whose session id already exists: the same session if it is this account's and
/// profile's, answered like a poll.
async fn replay_in(
    conn: &mut PgConnection,
    existing: &SessionRow,
    req: &CreateRequest<'_>,
) -> Result<Outcome, AdmissionError> {
    if existing.account_id != req.account_id
        || existing.profile_id != req.profile_id
        || existing.dedicated != req.dedicated
        || existing.modded != req.modded
    {
        return Err(AdmissionError::KeyReused);
    }
    poll_in(conn, req.session_id, req.account_id, req.ttl).await
}

/// Whether the account holds `role = 'admin'` now (OD-109), read in the caller's transaction.
/// With `lock` (an admission, before any pool or session-row lock) the account row is held
/// `FOR SHARE` until commit, so a concurrent demotion (an `UPDATE` of the row) either commits
/// first and is seen here or waits for this admission and then finds the session it must end at
/// its next heartbeat. A heartbeat or ticket refresh reads it unlocked: it already holds the
/// session row, and a demotion is enforced by the next heartbeat anyway.
async fn is_admin_in(
    conn: &mut PgConnection,
    account: Uuid,
    lock: bool,
) -> Result<bool, AdmissionError> {
    let sql = if lock {
        "SELECT role FROM account WHERE id = $1 FOR SHARE"
    } else {
        "SELECT role FROM account WHERE id = $1"
    };
    let role: Option<String> = sqlx::query_scalar(sql)
        .bind(account)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(role.as_deref() == Some("admin"))
}

/// The admission of a dedicated lobby's host session (OD-109, GATE_API.md §2 "Dedicated
/// sessions"): an admin only (read here, in the transaction), no save lease and nothing written
/// to the profile's save line, never queued, outside the pool's and the game's online capacity
/// (no game lock, no count), `max_until = game_until = now + 12 h` (each heartbeat rolls both).
/// It coexists with one ordinary session of the account; at most one dedicated session is live
/// per account: the same device (or `takeOver`) replaces the previous one (`takeover`), another
/// device is [`AdmissionError::InUseElsewhere`]. A replay answers as [`poll_in`] does.
async fn create_dedicated_in(
    conn: &mut PgConnection,
    req: &CreateRequest<'_>,
) -> Result<Outcome, AdmissionError> {
    if !is_admin_in(conn, req.account_id, true).await? {
        return Err(AdmissionError::RoleRequired);
    }
    if let Some(existing) = load_in(conn, req.session_id).await? {
        return replay_in(conn, &existing, req).await;
    }
    let access = access_of(conn, req.account_id).await?;
    let pool_name = &GameRules::of(req.game).pool(req.routing, access.vip);
    let pool = pool_in(conn, pool_name.as_str())
        .await?
        .filter(|p| p.accepting && p.game == req.game)
        .ok_or(AdmissionError::PoolClosed)?;
    // The pool's lock alone (as a heartbeat or a leave): nothing here is counted, so the game's
    // lock (OD-71) is not needed; the order is still pool, account, profile, session rows.
    sessions::lock_pool(conn, pool_name).await?;
    if let Some(existing) = load_in(conn, req.session_id).await? {
        return replay_in(conn, &existing, req).await;
    }
    sqlx::query("SELECT 1 FROM account WHERE id = $1 FOR KEY SHARE")
        .bind(req.account_id)
        .execute(&mut *conn)
        .await?;
    // CR-S62: a kick that committed while this waited for the account's row (its FOR UPDATE)
    // set the cooldown: read again under the lock.
    kick_access_in(conn, req.account_id).await?;
    // A profile deletion (fi_db::erasure::delete_profile_in, CR-S61) that committed while this
    // waited for the lock left a tombstone: read under the lock, refused.
    let live: Option<bool> =
        sqlx::query_scalar("SELECT status <> 'deleted' FROM profile WHERE id = $1 FOR SHARE")
            .bind(req.profile_id.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    if live != Some(true) {
        return Err(AdmissionError::NotFound);
    }
    let now = crate::identity::now_in(conn).await?;
    close_stale_in(conn, Scope::Account(req.account_id)).await?;
    let rows = sqlx::query(
        "SELECT s.id, d.device_id, d.label, COALESCE(s.admitted_at, s.created_at) AS since \
         FROM play_session s JOIN device_binding d ON d.id = s.device_binding_id \
         WHERE s.account_id = $1 AND s.state IN ('queued', 'active') AND s.dedicated \
         ORDER BY s.created_at FOR UPDATE OF s",
    )
    .bind(req.account_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut replaced = Vec::with_capacity(rows.len());
    for r in &rows {
        let device_id: Uuid = r.try_get("device_id")?;
        if !(device_id == req.device.device_id.as_uuid() || req.take_over) {
            return Err(AdmissionError::InUseElsewhere {
                device_label: r.try_get("label")?,
                since: ms(r.try_get("since")?),
            });
        }
        replaced.push(r.try_get::<Uuid, _>("id")?);
    }
    for id in replaced {
        close_in(conn, id, "ended", "takeover", None).await?;
    }
    let ttl_ms = req.ttl.for_tier(&pool.tier);
    let slot = Slot::bare(ttl_ms, now);
    let max_until = now.plus_millis(MAX_AGE_MS);
    let game_until = max_until;
    let start = head_revision_in(conn, req.profile_id).await?;
    sqlx::query(
        "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, game, state, launcher_version, \
         car_db_sha256, admitted_at, last_heartbeat_at, expires_at, max_until, game_until, start_revision, launch_nonce, \
         save_line, lease_ttl_ms, dedicated, modded) \
         VALUES ($1, $2, $3, $4, $5, $6, 'active', $7, NULL, $8, $8, $9, $10, $11, $12, $13, false, $14, true, $15)",
    )
    .bind(req.session_id.as_uuid())
    .bind(req.account_id)
    .bind(req.profile_id.as_uuid())
    .bind(req.device.binding_id)
    .bind(pool_name.as_str())
    .bind(req.game)
    .bind(req.launcher_version)
    .bind(ts(now))
    .bind(ts(slot.expires_at))
    .bind(ts(max_until))
    .bind(ts(game_until))
    .bind(start)
    .bind(req.nonce)
    .bind(slot.ttl_ms as i32)
    .bind(req.modded)
    .execute(&mut *conn)
    .await?;
    let row = load_in(conn, req.session_id)
        .await?
        .ok_or(AdmissionError::NotFound)?;
    Ok(Outcome::Active(Box::new(admitted(
        &row,
        slot,
        &access,
        now,
        (max_until, game_until),
    ))))
}

/// `GET /launcher/v1/sessions/{id}` (SAVES_API.md §3.1): a queued session refreshes its place and
/// is admitted when it is within the free slots; an active one answers its current lease.
pub async fn poll_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    ttl: SlotTtl,
) -> Result<Outcome, AdmissionError> {
    poll_with_in(conn, session, account_id, ttl, true).await
}

/// The slot an active session holds now: its stored lease (save line) or its row's expiry
/// (`ttl_ms` only for a row from before `lease_ttl_ms` was recorded).
async fn current_slot_in(
    conn: &mut PgConnection,
    row: &SessionRow,
    ttl_ms: u32,
) -> Result<Slot, AdmissionError> {
    if row.save_line {
        let (_, lease) = stored_lease_in(conn, row.profile_id, row.id)
            .await?
            .ok_or(SaveError::LeaseLost { current: None })?;
        Ok(Slot::leased(lease))
    } else {
        Ok(Slot {
            lease: None,
            ttl_ms: row.lease_ttl_ms.unwrap_or(ttl_ms),
            expires_at: row.expires_at,
        })
    }
}

/// [`poll_in`]; with `admit = false` (the game is in maintenance) a queued session keeps its
/// place without being admitted.
pub(crate) async fn poll_with_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    ttl: SlotTtl,
    admit: bool,
) -> Result<Outcome, AdmissionError> {
    crash_access_in(conn, account_id).await?;
    let (row, now) = lock_session_in(conn, session, account_id, Some("queued")).await?;
    let ttl_ms = ttl.for_session(&row.pool_tier, row.save_line);
    match row.state.as_str() {
        "active" => {
            let slot = current_slot_in(conn, &row, ttl_ms).await?;
            let access = access_of(conn, account_id).await?;
            let bounds = bounds_in(conn, row.id).await?;
            Ok(Outcome::Active(Box::new(admitted(
                &row, slot, &access, now, bounds,
            ))))
        }
        "queued" => {
            if row.expires_at <= now {
                close_in(conn, row.id.as_uuid(), "ended", "queue_left", None).await?;
                return Err(AdmissionError::Ended {
                    reason: "queue_left".into(),
                });
            }
            game_lock_late_in(conn, &row.pool).await?;
            close_stale_for_in(conn, &row.game, &row.pool).await?;
            let capacity = pool_in(conn, &row.pool)
                .await?
                .ok_or(AdmissionError::PoolClosed)?
                .capacity;
            let (active, _) = counts_in(conn, &row.game, &row.pool).await?;
            // OD-71: within the pool's slots, the game's and the global ones (OD-228; OD-129:
            // nobody waits behind a tier).
            let load = game_load_in(conn, &row.game, &[]).await?;
            let in_pool = position_in(conn, &row).await?;
            let position = in_pool;
            let pool_free = capacity.map_or(i64::MAX, |c| c - active);
            if admit
                && i64::from(in_pool) <= pool_free
                && i64::from(in_pool) <= load.room(&row.pool_tier)
            {
                let admitted = activate_in(conn, &row, AcquireMode::Normal, ttl, now).await?;
                return Ok(Outcome::Active(Box::new(admitted)));
            }
            sqlx::query("UPDATE play_session SET expires_at = $2 WHERE id = $1")
                .bind(row.id.as_uuid())
                .bind(ts(now.plus_millis(QUEUE_TTL_MS)))
                .execute(&mut *conn)
                .await?;
            Ok(Outcome::Queued {
                session_id: row.id,
                position,
            })
        }
        _ => Err(ended(&row)),
    }
}

fn ended(row: &SessionRow) -> AdmissionError {
    match row.end_reason.as_deref() {
        Some("takeover") => AdmissionError::Save(SaveError::LeaseLost { current: None }),
        reason => AdmissionError::Ended {
            reason: reason.unwrap_or(row.state.as_str()).to_owned(),
        },
    }
}

async fn bounds_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
) -> Result<(UnixMs, UnixMs), AdmissionError> {
    let r = sqlx::query("SELECT max_until, game_until FROM play_session WHERE id = $1")
        .bind(session.as_uuid())
        .fetch_one(&mut *conn)
        .await?;
    let get = |c: &str| -> Result<UnixMs, AdmissionError> {
        r.try_get::<Option<OffsetDateTime>, _>(c)?
            .map(ms)
            .ok_or_else(|| AdmissionError::Event(format!("active session without {c}")))
    };
    Ok((get("max_until")?, get("game_until")?))
}

/// `POST /launcher/v1/sessions/{id}/heartbeat` (SAVES_API.md §3.2).
pub async fn heartbeat_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    token: FenceToken,
    ttl: SlotTtl,
) -> Result<Admitted, AdmissionError> {
    heartbeat_any_in(conn, session, account_id, Some(token), ttl).await
}

/// [`heartbeat_in`] for sessions with or without a save line: the fencing token is required (and
/// renews the lease) only when the session holds one; otherwise the heartbeat renews the slot
/// alone. Either way the slot then lasts its pool tier's length from now
/// ([`SlotTtl::for_tier`]: a free session's liveness window, OD-40).
pub(crate) async fn heartbeat_any_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    token: Option<FenceToken>,
    ttl: SlotTtl,
) -> Result<Admitted, AdmissionError> {
    crash_access_in(conn, account_id).await?;
    let (row, now) = lock_session_in(conn, session, account_id, Some("expired")).await?;
    let max_until = match row.state.as_str() {
        "queued" => return Err(AdmissionError::Queued),
        "active" | "expired" => row
            .max_until
            .ok_or_else(|| AdmissionError::Event("an admitted session without max_until".into()))?,
        _ => return Err(ended(&row)),
    };
    if row.state == "expired" && kicked_since_in(conn, account_id, row.id).await? {
        return Err(AdmissionError::Ended {
            reason: "kicked".into(),
        });
    }
    // OD-109: a dedicated session lives only while its account is an admin (`revoked`), and its
    // heartbeat rolls the 12 h bound instead of ending it (`max_age`).
    if row.dedicated && !is_admin_in(conn, account_id, false).await? {
        close_in(conn, row.id.as_uuid(), "revoked", "revoked", None).await?;
        return Err(AdmissionError::Ended {
            reason: "revoked".into(),
        });
    }
    let max_until = if row.dedicated && row.state == "active" {
        now.plus_millis(MAX_AGE_MS)
    } else {
        max_until
    };
    let access = access_of(conn, account_id).await?;
    // OD-52: the lease does not outlive the profile's eligibility (the access ended, the profile
    // left `hardcore`): this heartbeat releases it and the session goes on without a save line.
    let drop_line = row.save_line && !holds_save_line(&row.game, &row.mode);
    // CR-S51: the lease's slot while the session keeps its line, whatever its pool.
    let ttl_ms = ttl.for_session(&row.pool_tier, row.save_line && !drop_line);
    // A session without a save line ignores a token (it never held the lease, or no longer may).
    let token = match (row.save_line && !drop_line, token) {
        (true, None) => return Err(SaveError::InvalidArgument("fenceToken").into()),
        (true, token) => token,
        (false, _) => None,
    };
    if max_until <= now {
        // No session keeps a pool by heartbeats alone: the launcher re-admits (and re-reads the
        // tier) through POST /sessions; the same device replaces this lease at once.
        close_in(conn, row.id.as_uuid(), "ended", "max_age", None).await?;
        return Err(AdmissionError::Ended {
            reason: "max_age".into(),
        });
    }
    if drop_line {
        release_held_in(conn, row.profile_id, row.id).await?;
        sqlx::query("UPDATE play_session SET save_line = false WHERE id = $1")
            .bind(row.id.as_uuid())
            .execute(&mut *conn)
            .await?;
    }
    let slot = if row.state == "active" {
        match token {
            Some(token) => {
                let lease = match saves::renew_in(conn, row.profile_id, row.id, token, ttl_ms).await
                {
                    Ok(lease) => lease,
                    Err(SaveError::LeaseExpired { .. }) => {
                        resume_in(conn, &row, token, ttl_ms).await?
                    }
                    Err(e) => return Err(e.into()),
                };
                Slot::leased(lease)
            }
            None => Slot::bare(ttl_ms, now),
        }
    } else if row.dedicated {
        // An expired dedicated session is not revived: the launcher admits a new one.
        return Err(SaveError::LeaseLost { current: None }.into());
    } else {
        // `expired`: the slot was freed. Re-admission under the pool lock, never jumping the
        // queue; then the lease is resumed only if nobody was granted the line since.
        game_lock_late_in(conn, &row.pool).await?;
        close_stale_for_in(conn, &row.game, &row.pool).await?;
        let capacity = pool_in(conn, &row.pool)
            .await?
            .ok_or(AdmissionError::PoolClosed)?
            .capacity;
        let (active, queued) = counts_in(conn, &row.game, &row.pool).await?;
        let load = game_load_in(conn, &row.game, &[]).await?;
        let others: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM play_session WHERE account_id = $1 AND state IN ('queued', 'active') AND NOT dedicated",
        )
        .bind(account_id)
        .fetch_one(&mut *conn)
        .await?;
        if !has_slot(capacity, active) || queued > 0 || others > 0 || load.room(&row.pool_tier) < 1
        {
            return Err(SaveError::LeaseLost { current: None }.into());
        }
        let slot = match token {
            Some(token) => Slot::leased(resume_in(conn, &row, token, ttl_ms).await?),
            None => Slot::bare(ttl_ms, now),
        };
        sqlx::query(
            "UPDATE play_session SET state = 'active', ended_at = NULL, end_reason = NULL, end_revision = NULL WHERE id = $1",
        )
        .bind(row.id.as_uuid())
        .execute(&mut *conn)
        .await?;
        slot
    };
    if slot.lease.is_some() {
        cloud::cover_in(conn, row.profile_id, access.access_until, now, false).await?;
    }
    let game_until = if row.dedicated {
        max_until
    } else {
        game_until_for(&row.pool_tier, &access, now, max_until)
    };
    sqlx::query(
        "UPDATE play_session SET expires_at = $2, last_heartbeat_at = $3, game_until = $4, lease_ttl_ms = $5, \
         max_until = $6 WHERE id = $1",
    )
    .bind(row.id.as_uuid())
    .bind(ts(slot.expires_at))
    .bind(ts(now))
    .bind(ts(game_until))
    .bind(slot.ttl_ms as i32)
    .bind(ts(max_until))
    .execute(&mut *conn)
    .await?;
    Ok(admitted(&row, slot, &access, now, (max_until, game_until)))
}

async fn resume_in(
    conn: &mut PgConnection,
    row: &SessionRow,
    token: FenceToken,
    ttl_ms: u32,
) -> Result<Lease, AdmissionError> {
    let grant = saves::acquire_in(
        conn,
        row.profile_id,
        &AcquireRequest {
            holder: holder(row),
            ttl_ms,
            mode: AcquireMode::Resume { previous: token },
        },
    )
    .await?;
    Ok(grant.lease)
}

/// `POST /launcher/v1/sessions/{id}/token` (SAVES_API.md §3.3) and the gate's ticket route: the
/// session as it stands, for a fresh ticket, while it is active and its slot holds (its live
/// lease, or for a session without a save line a heartbeat or game request within the lease).
/// Locks the session row (run it in a transaction, so a concurrent leave or sweep cannot close
/// the session between this read and the ticket's record); renews nothing.
pub async fn token_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
) -> Result<Admitted, AdmissionError> {
    crash_access_in(conn, account_id).await?;
    sqlx::query("SELECT 1 FROM play_session WHERE id = $1 FOR UPDATE")
        .bind(session.as_uuid())
        .execute(&mut *conn)
        .await?;
    let row = load_in(conn, session)
        .await?
        .filter(|s| s.account_id == account_id)
        .ok_or(AdmissionError::NotFound)?;
    match row.state.as_str() {
        "active" => {}
        "queued" => return Err(AdmissionError::Queued),
        // CR-S62: a kicked session says so (the launcher's "removed by a moderator").
        _ if row.end_reason.as_deref() == Some("kicked") => {
            return Err(AdmissionError::Ended {
                reason: "kicked".into(),
            });
        }
        _ => return Err(SaveError::LeaseLost { current: None }.into()),
    }
    // OD-109: no fresh ticket for a dedicated session whose account lost `admin` (its next
    // heartbeat ends it `revoked`; this read-only route ends nothing).
    if row.dedicated && !is_admin_in(conn, account_id, false).await? {
        return Err(AdmissionError::Ended {
            reason: "revoked".into(),
        });
    }
    let now = crate::identity::now_in(conn).await?;
    let ttl = row.lease_ttl_ms.unwrap_or(fi_saves::lease::DEFAULT_TTL_MS);
    let slot = if row.save_line {
        let (_, lease) = stored_lease_in(conn, row.profile_id, row.id)
            .await?
            .filter(|(_, l)| l.is_live(now))
            .ok_or(SaveError::LeaseLost { current: None })?;
        Slot::leased(lease)
    } else {
        let seen_live = row
            .last_seen_at
            .is_some_and(|t| t.plus_millis(i64::from(ttl)) > now);
        if row.expires_at <= now && !seen_live {
            return Err(SaveError::LeaseLost { current: None }.into());
        }
        Slot {
            lease: None,
            ttl_ms: ttl,
            expires_at: row.expires_at,
        }
    };
    let access = access_of(conn, account_id).await?;
    let bounds = bounds_in(conn, row.id).await?;
    Ok(admitted(&row, slot, &access, now, bounds))
}

/// Ends all queued/active/resumable expired sessions during login replacement (OD-93).
/// The caller holds the account FOR UPDATE. No pool lock is needed: this only removes capacity,
/// and create holds the account before its pool lock through `device_of_family_in`.
/// Session rows precede heads, matching heartbeat; every lease release keeps normal fencing,
/// play-time accounting and audit, and closing emits the usual world/seat cleanup and session summary.
pub(crate) async fn replace_login_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(), AdmissionError> {
    let rows = sqlx::query(
        "SELECT id, profile_id, state FROM play_session WHERE account_id = $1 \
         AND state IN ('queued', 'active', 'expired') ORDER BY id FOR UPDATE",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    for row in rows {
        let session =
            PlaySessionId::from_uuid(row.try_get("id")?).ok_or(AdmissionError::NotFound)?;
        let profile =
            ProfileId::from_uuid(row.try_get("profile_id")?).ok_or(AdmissionError::NotFound)?;
        release_held_in(conn, profile, session).await?;
        if row.try_get::<String, _>("state")? == "expired" {
            // Expiry already closed it. Make it terminal without emitting twice;
            // heartbeat must never resume an old login's expired row after replacement.
            sqlx::query(
                "UPDATE play_session SET state = 'ended', end_reason = 'takeover' WHERE id = $1",
            )
            .bind(session.as_uuid())
            .execute(&mut *conn)
            .await?;
        } else {
            close_in(conn, session.as_uuid(), "ended", "takeover", None).await?;
        }
    }
    Ok(())
}

/// `POST /launcher/v1/sessions/{id}/end` (SAVES_API.md §3.4): releases the lease, closes the
/// session (`game_exit` or `player`), writes its session summary. Idempotent: a closed session answers
/// `Ok`.
pub async fn end_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    token: FenceToken,
    reason: &str,
    end_revision: Option<u64>,
) -> Result<(), AdmissionError> {
    end_any_in(conn, session, account_id, Some(token), reason, end_revision).await
}

/// [`end_in`]; without a fencing token the lease this session still holds is released by its
/// stored token (the gate's `leave`: the launcher's platform session owns the play session).
pub(crate) async fn end_any_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    token: Option<FenceToken>,
    reason: &str,
    end_revision: Option<u64>,
) -> Result<(), AdmissionError> {
    let (row, _) = lock_session_in(conn, session, account_id, None).await?;
    let held = match (row.save_line, token) {
        (false, _) => None,
        (true, Some(t)) => Some(t),
        (true, None) => stored_lease_in(conn, row.profile_id, row.id)
            .await?
            .map(|(t, _)| t),
    };
    match row.state.as_str() {
        "active" => {
            if let Some(token) = held {
                // With the launcher's own token a mismatch is SAVES.LEASE_LOST, as before.
                saves::release_in(conn, row.profile_id, row.id, token).await?;
            }
            close_in(conn, row.id.as_uuid(), "ended", reason, end_revision).await?;
        }
        "queued" => {
            close_in(conn, row.id.as_uuid(), "ended", reason, end_revision).await?;
        }
        "expired" => {
            // Closed already (its session summary was written); a late end still frees the lapsed
            // lease if it is this session's.
            if let Some(token) = held {
                match saves::release_in(conn, row.profile_id, row.id, token).await {
                    Ok(_) | Err(SaveError::LeaseLost { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// `POST /launcher/v1/sessions/{id}/installed` (SAVES_API.md §3.5): an audit row.
pub async fn installed_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    revision: u64,
    content_sha256: &str,
) -> Result<(), AdmissionError> {
    let row = load_in(conn, session)
        .await?
        .filter(|s| s.account_id == account_id)
        .ok_or(AdmissionError::NotFound)?;
    sqlx::query(
        "INSERT INTO save_event (profile_id, kind, session_id, revision, detail) VALUES ($1, 'installed', $2, $3, $4)",
    )
    .bind(row.profile_id.as_uuid())
    .bind(row.id.as_uuid())
    .bind(revision as i64)
    .bind(serde_json::json!({ "content": content_sha256 }))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One reaper pass over a pool, under its lock (`fi-cli sessions-reap`).
pub async fn reap_pool_in(conn: &mut PgConnection, pool: &str) -> Result<Reaped, AdmissionError> {
    lock_pool_named(conn, pool).await?;
    close_stale_in(conn, Scope::Pool(pool)).await
}

/// One reaper pass over every pool, one transaction per pool.
pub async fn reap(db: &PgPool) -> Result<Reaped, AdmissionError> {
    let pools: Vec<String> = sqlx::query_scalar("SELECT name FROM pool ORDER BY name")
        .fetch_all(db)
        .await?;
    let mut total = Reaped::default();
    for pool in pools {
        let mut tx = db.begin().await?;
        let reaped = reap_pool_in(&mut tx, &pool).await?;
        tx.commit().await?;
        total.add(reaped);
    }
    Ok(total)
}

/// A pool as `FI_POOLS_JSON` describes it (LOCAL_ENV.md §3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolSpec {
    /// `pool.name`.
    pub name: PoolName,
    /// The game it serves (`fh2` when the entry names none).
    pub game: String,
    /// `free` | `vip`.
    pub tier: Tier,
    /// Concurrent active sessions (`None`: unlimited).
    pub capacity: Option<u32>,
    /// Longest queue.
    pub queue_limit: u32,
    /// Live sessions per game-plane instance the autoscaler plans with (configuration only).
    pub instance_capacity: Option<u32>,
    /// The game plane's base URL.
    pub game_endpoint: String,
    /// Whether it admits.
    pub accepting: bool,
}

/// Parses `FI_POOLS_JSON`: an array of `{name, game?, tier, capacity, queueLimit,
/// instanceCapacity, gameEndpoint, accepting?}`. The error names the entry and the field, never a value.
pub fn parse_pools_json(text: &str) -> Result<Vec<PoolSpec>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "FI_POOLS_JSON is not JSON".to_owned())?;
    let items = value
        .as_array()
        .ok_or_else(|| "FI_POOLS_JSON is an array of pools".to_owned())?;
    let mut out: Vec<PoolSpec> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("FI_POOLS_JSON[{i}] is not an object"))?;
        const FIELDS: [&str; 8] = [
            "name",
            "game",
            "tier",
            "capacity",
            "queueLimit",
            "instanceCapacity",
            "gameEndpoint",
            "accepting",
        ];
        if let Some(k) = obj.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            return Err(format!("FI_POOLS_JSON[{i}].{k} is not a pool field"));
        }
        let field = |k: &str| obj.get(k).filter(|v| !v.is_null());
        let bad = |k: &str| format!("FI_POOLS_JSON[{i}].{k} is invalid");
        let count = |k: &str| -> Result<Option<u32>, String> {
            field(k)
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|n| *n > 0 && *n <= i32::MAX as u32)
                        .ok_or_else(|| bad(k))
                })
                .transpose()
        };
        let name = field("name")
            .and_then(|v| v.as_str())
            .and_then(|n| PoolName::new(n).ok())
            .ok_or_else(|| bad("name"))?;
        let game = match field("game") {
            None => "fh2".to_owned(),
            Some(v) => v
                .as_str()
                .filter(|g| {
                    (2..=16).contains(&g.len())
                        && g.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                        && g.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                })
                .ok_or_else(|| bad("game"))?
                .to_owned(),
        };
        let tier = match field("tier").and_then(|v| v.as_str()) {
            Some("free") => Tier::Free,
            Some("vip") => Tier::Vip,
            _ => return Err(bad("tier")),
        };
        let game_endpoint = field("gameEndpoint")
            .and_then(|v| v.as_str())
            .filter(|e| {
                let rest = e
                    .strip_prefix("http://")
                    .or_else(|| e.strip_prefix("https://"));
                rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
            })
            .map(|e| e.trim_end_matches('/').to_owned())
            .ok_or_else(|| bad("gameEndpoint"))?;
        let accepting = match field("accepting") {
            None => true,
            Some(v) => v.as_bool().ok_or_else(|| bad("accepting"))?,
        };
        let queue_limit = match field("queueLimit") {
            None => 20_000,
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n <= i32::MAX as u32)
                .ok_or_else(|| bad("queueLimit"))?,
        };
        if out.iter().any(|p| p.name == name) {
            return Err(format!("FI_POOLS_JSON[{i}].name appears twice"));
        }
        out.push(PoolSpec {
            capacity: count("capacity")?,
            instance_capacity: count("instanceCapacity")?,
            name,
            game,
            tier,
            queue_limit,
            game_endpoint,
            accepting,
        });
    }
    Ok(out)
}

/// Upserts the pool rows (as the migrator: `fi_runtime` has SELECT only). Rows not in `specs` are
/// left as they are; a pool's game is set when it is created and never moved (its sessions'
/// foreign key names it). A free pool with a capacity also sets its game's matchmaking free limit
/// (`mm_free_slot.free_limit`). Returns how many rows changed.
pub async fn upsert_pools_in(
    conn: &mut PgConnection,
    specs: &[PoolSpec],
) -> Result<u64, sqlx::Error> {
    let mut changed = 0;
    for p in specs {
        changed += sqlx::query(
            "INSERT INTO pool (name, game, tier, capacity, queue_limit, game_endpoint, accepting) VALUES ($1, $7, $2, $3, $4, $5, $6) \
             ON CONFLICT (name) DO UPDATE SET tier = EXCLUDED.tier, capacity = EXCLUDED.capacity, queue_limit = EXCLUDED.queue_limit, \
             game_endpoint = EXCLUDED.game_endpoint, accepting = EXCLUDED.accepting, row_version = pool.row_version + 1 \
             WHERE (pool.tier, pool.capacity, pool.queue_limit, pool.game_endpoint, pool.accepting) IS DISTINCT FROM \
             (EXCLUDED.tier, EXCLUDED.capacity, EXCLUDED.queue_limit, EXCLUDED.game_endpoint, EXCLUDED.accepting)",
        )
        .bind(p.name.as_str())
        .bind(p.tier.as_str())
        .bind(p.capacity.map(|c| c as i32))
        .bind(p.queue_limit as i32)
        .bind(&p.game_endpoint)
        .bind(p.accepting)
        .bind(&p.game)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        // A free pool's capacity is its game's free limit in matchmaking (MATCHMAKING_API.md §1):
        // one number, kept in step here so the gate's admission and `/mm/v1/capacity` agree
        // (server/REVIEW_LOG.md S-MM1). An unlimited free pool leaves the limit as it is.
        if p.tier.as_str() == "free"
            && let Some(capacity) = p.capacity
        {
            changed += sqlx::query(
                "UPDATE mm_free_slot SET free_limit = $2, updated_at = clock_timestamp() \
                 WHERE game = $1 AND free_limit IS DISTINCT FROM $2",
            )
            .bind(&p.game)
            .bind(i32::try_from(capacity).unwrap_or(i32::MAX))
            .execute(&mut *conn)
            .await?
            .rows_affected();
        }
    }
    Ok(changed)
}

/// Checks that monetization's `entitlement` table exists (migration
/// `20261018120000_billing_entitlements.sql`, which also grants `fi_runtime` what
/// `PUT /internal/v1/local/entitlements` needs). Local only (`fi-cli local-seed` refuses prod).
/// Until 2026-10-18 this created a stand-in table; the real one now comes with the migrations.
pub async fn ensure_local_entitlement_in(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1 FROM entitlement LIMIT 0")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `PUT /internal/v1/local/entitlements` (GAME_PLATFORM.md §3.2): upserts the account's
/// `entitlement` row directly (`product = 'vip'`, provider `admin`, `version + 1`, `state` NULL so
/// fi-billing rebuilds its cache by replay) and answers the account's access by the ACCOUNTS.md §7 rule
/// at the database clock. `status = "none"` stores status `none` without access. `NotFound` for
/// an unknown account. A local stand-in for fi-billing: it writes no ledger event, so ADR 0305
/// §3's single-writer rule allows it by name (fi-billing `tests/no_foreign_writers.rs`, CR-M3).
pub async fn set_local_entitlement_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    status: &str,
    access_until: Option<UnixMs>,
    plan: Option<&str>,
) -> Result<AccessRow, AdmissionError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM account WHERE id = $1 AND status <> 'deleted')",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?;
    if !exists {
        return Err(AdmissionError::NotFound);
    }
    let access_until = if status == "none" { None } else { access_until };
    sqlx::query(
        "INSERT INTO entitlement (account_id, product, status, plan, provider, coverage_end, access_until, \
           auto_renew, past_due, blocked, version) VALUES ($1, 'vip', $2, $3, 'admin', $4, $4, false, false, false, 1) \
         ON CONFLICT (account_id, product) DO UPDATE SET status = EXCLUDED.status, plan = EXCLUDED.plan, \
         provider = EXCLUDED.provider, coverage_end = EXCLUDED.coverage_end, access_until = EXCLUDED.access_until, \
         version = entitlement.version + 1, state = NULL",
    )
    .bind(account_id)
    .bind(status)
    .bind(plan)
    .bind(access_until.map(ts))
    .execute(&mut *conn)
    .await?;
    access_of(conn, account_id).await
}

/// A pool with its live counts (`GET /internal/v1/pools`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occupancy {
    /// Name.
    pub name: String,
    /// The game it serves.
    pub game: String,
    /// `free` | `vip`.
    pub tier: String,
    /// Admission capacity (`None`: unlimited).
    pub capacity: Option<u32>,
    /// Longest queue.
    pub queue_limit: u32,
    /// Whether it admits.
    pub accepting: bool,
    /// Game-plane base URL.
    pub game_endpoint: String,
    /// Active sessions that are live by the database clock (a slot lapsed on both signals or
    /// `max_until` passed no longer counts, even before the sweeper closes the row).
    pub active: u32,
    /// Queued sessions polled within 90 s.
    pub queued: u32,
}

/// Every pool with its live counts, by the database clock.
pub async fn occupancy(db: &PgPool) -> Result<Vec<Occupancy>, AdmissionError> {
    let sql = format!(
        "SELECT p.name, p.game, p.tier, p.capacity, p.queue_limit, p.accepting, p.game_endpoint, \
         count(s.id) FILTER (WHERE {}) AS active, \
         count(s.id) FILTER (WHERE s.state = 'queued' AND s.expires_at > clock_timestamp()) AS queued \
         FROM pool p LEFT JOIN play_session s ON s.pool = p.name AND s.state IN ('active', 'queued') \
         GROUP BY p.name ORDER BY p.name",
        Stale::counted_active()
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(db).await?;
    rows.iter()
        .map(|r| {
            Ok(Occupancy {
                name: r.try_get("name")?,
                game: r.try_get("game")?,
                tier: r.try_get("tier")?,
                capacity: r.try_get::<Option<i32>, _>("capacity")?.map(|c| c as u32),
                queue_limit: r.try_get::<i32, _>("queue_limit")? as u32,
                accepting: r.try_get("accepting")?,
                game_endpoint: r.try_get("game_endpoint")?,
                active: r.try_get::<i64, _>("active")? as u32,
                queued: r.try_get::<i64, _>("queued")? as u32,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_pools_json_parses() {
        let text = r#"[{"name":"vip","tier":"vip","capacity":12,"queueLimit":100,"instanceCapacity":4,"gameEndpoint":"http://127.0.0.1:8480/pools/vip"},
 {"name":"free","tier":"free","capacity":6,"queueLimit":100,"instanceCapacity":3,"gameEndpoint":"http://127.0.0.1:8480/pools/free"}]"#;
        let pools = parse_pools_json(text).unwrap();
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0].name.as_str(), "vip");
        assert_eq!(pools[0].tier, Tier::Vip);
        assert_eq!(pools[0].capacity, Some(12));
        assert_eq!(pools[1].instance_capacity, Some(3));
        assert_eq!(pools[1].game_endpoint, "http://127.0.0.1:8480/pools/free");
        assert_eq!(pools[1].game, "fh2");
        let mm = parse_pools_json(
            r#"[{"name":"matchmaking","game":"fh2","tier":"free","capacity":50,"gameEndpoint":"http://127.0.0.1:8480"}]"#,
        )
        .unwrap();
        assert_eq!(
            (mm[0].name.as_str(), mm[0].game.as_str()),
            ("matchmaking", "fh2")
        );
        assert!(pools[1].accepting);
    }

    #[test]
    fn malformed_pools_are_refused_without_echoing_values() {
        for bad in [
            "not json",
            "{}",
            r#"[{"name":"VIP","tier":"vip","gameEndpoint":"http://x"}]"#,
            r#"[{"name":"vip","tier":"gold","gameEndpoint":"http://x"}]"#,
            r#"[{"name":"vip","tier":"vip","gameEndpoint":"ftp://x"}]"#,
            r#"[{"name":"vip","tier":"vip","gameEndpoint":"http://x","capacity":0}]"#,
            r#"[{"name":"vip","tier":"vip","gameEndpoint":"http://x","secret":"s3cr3t"}]"#,
            r#"[{"name":"vip","game":"FH2","tier":"vip","gameEndpoint":"http://x"}]"#,
            r#"[{"name":"vip","tier":"vip","gameEndpoint":"http://x"},{"name":"vip","tier":"vip","gameEndpoint":"http://y"}]"#,
        ] {
            let e = parse_pools_json(bad).unwrap_err();
            assert!(!e.contains("s3cr3t"), "{e}");
        }
        let minimal =
            parse_pools_json(r#"[{"name":"vip","tier":"vip","gameEndpoint":"http://x/"}]"#)
                .unwrap();
        assert_eq!(minimal[0].capacity, None);
        assert_eq!(minimal[0].queue_limit, 20_000);
        assert_eq!(minimal[0].game_endpoint, "http://x");
    }

    #[test]
    fn the_slot_and_the_heartbeat_follow_the_pool_tier() {
        let ttl = SlotTtl {
            lease_ms: 300_000,
            free_ms: FREE_LIVENESS_MS,
        };
        assert_eq!(ttl.for_tier("vip"), 300_000);
        assert_eq!(ttl.for_tier("free"), 600_000);
        // CR-S51: a session with a save line holds the lease's slot whatever its pool.
        assert_eq!(ttl.for_session("free", true), 300_000);
        assert_eq!(ttl.for_session("free", false), 600_000);
        assert_eq!(ttl.for_session("vip", false), 300_000);
        assert_eq!(SlotTtl::uniform(2_000).for_tier("free"), 2_000);
        // Supporters: a fifth of the lease (60 s for the contract's 5 minutes).
        assert_eq!(heartbeat_every_ms("vip", true, 300_000), 60_000);
        assert_eq!(heartbeat_every_ms("vip", false, 300_000), 60_000);
        assert_eq!(heartbeat_every_ms("vip", true, 20_000), 4_000);
        // CR-S51: a free session with a save line heartbeats at the lease's cadence.
        assert_eq!(heartbeat_every_ms("free", true, 300_000), 60_000);
        // Free (OD-40): 80% of the window; 8 minutes in production, 72 s for compose's 90 s.
        assert_eq!(heartbeat_every_ms("free", false, FREE_LIVENESS_MS), 480_000);
        assert_eq!(heartbeat_every_ms("free", false, 90_000), 72_000);
        assert_eq!(
            heartbeat_every_ms("free", false, FREE_LIVENESS_MIN_MS),
            24_000
        );
        // The longest window: one call lands within the game access ticket's 10 minutes.
        assert!(heartbeat_every_ms("free", false, FREE_LIVENESS_MS) < 600_000);
    }

    #[test]
    fn the_room_is_the_smaller_of_the_game_and_the_global_one() {
        let load = |capacity, active, global_capacity, global_active| GameLoad {
            capacity,
            active,
            global_capacity,
            global_active,
        };
        assert_eq!(load(None, 0, None, 0).room("free"), i64::MAX);
        assert_eq!(load(Some(50), 10, None, 0).room("free"), 40);
        // OD-228: the global cap binds a game under its own, and an uncapped game.
        assert_eq!(load(Some(50), 10, Some(100), 98).room("vip"), 2);
        assert_eq!(load(None, 0, Some(100), 100).room("free"), 0);
        assert_eq!(load(Some(5), 5, Some(100), 20).room("free"), 0);
        // Lowered under the live count: no room, nobody ended.
        assert!(load(None, 0, Some(10), 12).room("vip") < 1);
    }

    #[test]
    fn game_until_follows_the_pool_and_the_access() {
        let now = UnixMs::from_millis(1_800_000_000_000);
        let max = now.plus_millis(MAX_AGE_MS);
        let vip = |until: Option<UnixMs>, status: &str| AccessRow {
            vip: true,
            status: status.into(),
            access_until: until,
            plan: None,
            auto_renew: false,
            past_due: false,
        };
        assert_eq!(game_until_for("free", &AccessRow::free(), now, max), max);
        assert_eq!(
            game_until_for(
                "vip",
                &vip(Some(now.plus_millis(86_400_000)), "active"),
                now,
                max
            ),
            max
        );
        let ends = now.plus_millis(60_000);
        assert_eq!(
            game_until_for("vip", &vip(Some(ends), "active"), now, max),
            ends.plus_millis(VIP_GRACE_MS)
        );
        assert_eq!(
            game_until_for("vip", &vip(Some(ends), "revoked_refund"), now, max),
            now
        );
        assert_eq!(game_until_for("vip", &AccessRow::free(), now, max), now);
    }
}
