//! The matchmaking control plane (migrations `20261019000300_universe.sql` and
//! `20261019170000_worlds_overlay.sql`; ADR 0017 §1; docs/contracts/MATCHMAKING_API.md): worlds,
//! host bans, seats (join grants), invites and requests to join, punches and capacity. Every
//! decision is one transaction on Postgres, so it holds across every matchmaking instance.
//!
//! **Modes never meet** (OD-56) unless the host opens its world to both (OD-141, CR-S76: `modes:
//! "any"`, [`WorldRow::admits`]): the list, the reads, joins, requests, seats and the roster are
//! of the worlds that admit the caller's mode. **What a caller sees** of a world is one statement fragment ([`WorldRow`], its
//! facts as seen by a [`Viewer`]) and one pure function ([`assess`]: `canJoin` and `why`), shared
//! by the list ([`list_worlds`] and [`select`], OD-58), the reads, [`join`], the seat, and core's
//! invite inbox and friend presence. A world's audience (OD-57) is the host's choice; friends and
//! private worlds carry an 8-digit code ([`new_code`]); wrong codes are throttled per account
//! ([`CODE_LIMIT`], [`CODE_DAY_LIMIT`]). The world's roster ([`members`], OD-63) is its host and
//! its live seats, read by them only, in one statement.
//!
//! Lock order (no cycle): a join (and a request's acceptance) first takes the seat holder's
//! profile advisory lock ([`PROFILE_LOCK_NAMESPACE`]: one seat decision per profile at a time),
//! then the holder account (`FOR KEY SHARE`), profile (`FOR SHARE`), and live session
//! (`FOR SHARE`), before the world/grant FKs; then the world row
//! (`FOR UPDATE`, as hosting, closing, invites and requests do), then grant
//! and invite rows. A used grant holds its seat only while its world is open and inside its TTL.
//!
//! Seats (`mm_grant`) move `issued` (reserved for 60 s, renewed up to `hold_until`) → `used` (the
//! host's announce lists the peer) → `released`, or `issued` → `expired` | `released` | `kicked`;
//! `end_reason` says how a seat ended (`world_closed` when its world closed with it live);
//! `announced` marks a grant's first move out of `issued`. A world never
//! queues (OD-58). Invites and requests (`world_invite`, OD-56, OD-59) hold a seat while pending
//! ([`OFFER_TTL_SECS`], at most [`MAX_PENDING`] per world). A world closes on the host's delete,
//! past its 45 s TTL ([`sweep_in`]), when its host opens another, when the host's play session
//! ends ([`close_worlds_of_session_in`]), or on a heartbeat after it has had nobody but its host
//! for the idle time ([`host_with_idle`], OD-107); closing ends its live seats and pending offers
//! and clears its address, location and code.

use crate::identity::{IdentityError, ms};
use crate::limits::{self, Hit, Limit};
use fi_domain::UnixMs;
use fi_domain::geo::{Estimate, PingClass};
use sqlx::{PgConnection, PgPool, Row};
use std::net::{IpAddr, SocketAddr};
use time::OffsetDateTime;
use uuid::Uuid;

/// World reads per account (12 per minute: the list, one world, a world by code).
pub const LIST_LIMIT: Limit = Limit {
    name: "mm.list",
    max: 12,
    window_s: 60,
};
/// Relayed seats per account (CR-S57 review M4): seats granted on a relay or moved to one, 10 per
/// hour; past it the answer is 409 `PORTAL.CONFLICT {reason: "relay_busy"}`.
pub const RELAY_LIMIT: Limit = Limit {
    name: "mm.relay",
    max: 10,
    window_s: 3600,
};
/// Live relayed seats per world by default (`FI_RELAY_SEATS_PER_WORLD`, CR-S57 review M4).
pub const DEFAULT_RELAY_SEATS_PER_WORLD: u32 = 4;
/// Joins per account (6 per minute).
pub const JOIN_LIMIT: Limit = Limit {
    name: "mm.join",
    max: 6,
    window_s: 60,
};
/// Heartbeats per account (6 per minute; the launcher sends one every 15 s).
pub const ANNOUNCE_LIMIT: Limit = Limit {
    name: "mm.announce",
    max: 6,
    window_s: 60,
};
/// Settings changes and new codes per account (6 per minute).
pub const SETTINGS_LIMIT: Limit = Limit {
    name: "mm.settings",
    max: 6,
    window_s: 60,
};
/// Seat reads, renewals and leaves per account (30 per minute; the launcher renews every 20 s).
pub const SEAT_LIMIT: Limit = Limit {
    name: "mm.seat",
    max: 30,
    window_s: 60,
};
/// Authenticated socket observations, including retries (12 per account per minute).
pub const NETWORK_LIMIT: Limit = Limit {
    name: "mm.network",
    max: 12,
    window_s: 60,
};
/// Network hints never remain fresh for more than 30 seconds after their observation.
pub const NETWORK_TTL_MS: i64 = 30_000;
/// Invites and requests a sending account makes (20 per 10 minutes).
pub const INVITE_LIMIT: Limit = Limit {
    name: "mm.invite",
    max: 20,
    window_s: 600,
};
/// Invites and requests an answering account receives (10 per 10 minutes).
pub const INVITED_LIMIT: Limit = Limit {
    name: "mm.invited",
    max: 10,
    window_s: 600,
};
/// Wrong world codes per account (5 per 10 minutes, OD-57): then every request carrying a code
/// waits until the window ends.
pub const CODE_LIMIT: Limit = Limit {
    name: "mm.code",
    max: 5,
    window_s: 600,
};
/// Wrong world codes per account per day (20): the wait grows to the day's end.
pub const CODE_DAY_LIMIT: Limit = Limit {
    name: "mm.code.day",
    max: 20,
    window_s: 86_400,
};
/// Advisory-lock namespace of one profile's seat decisions.
pub const PROFILE_LOCK_NAMESPACE: i32 = 0x6d6d_0002;
/// Advisory-lock namespace of one account's code attempts (the throttle's check and count are one
/// step).
pub const CODE_LOCK_NAMESPACE: i32 = 0x6d6d_0003;
/// Punches reads per account (the host's relay polls every 3 s).
pub const PUNCHES_LIMIT: Limit = Limit {
    name: "mm.punches",
    max: 40,
    window_s: 60,
};
/// A host's other actions per account: closing a world, moderation, accepting requests.
pub const HOST_LIMIT: Limit = Limit {
    name: "mm.host",
    max: 30,
    window_s: 60,
};
/// Roster reads per play session (30 per minute; the overlay polls it, OD-63). Keyed by the
/// session id ([`limit_key`] hashes whichever id it is given).
pub const MEMBERS_LIMIT: Limit = Limit {
    name: "mm.members",
    max: 30,
    window_s: 60,
};
/// Seconds a kicked grant stays on the host's punches (`kicked: true`).
pub const KICKED_VISIBLE_SECS: i64 = 60;
/// Seconds a seat lives without a renew (and a relay grant at most).
pub const GRANT_TTL_SECS: i64 = 60;
/// Seconds a world lives past its last announce.
pub const WORLD_TTL_SECS: i64 = 45;
/// Seconds a seated (`used`) peer may be missing from the host's heartbeat `peers` in a row before
/// its seat is released (S33, MATCHMAKING_API.md §2.5, register CR-S86): a player moving between the
/// engines of one world (car meet -> free roam) drops out of the host's peers for a moment.
/// Heartbeats come every 15 s, so in practice two or three consecutive misses.
pub const SEAT_MISSING_GRACE_SECS: i64 = 30;
/// Peers a dedicated world's heartbeat may list (OD-109: its host is not a player); an ordinary
/// world's, one fewer (its host holds a seat).
pub const MAX_PEERS_DEDICATED: usize = fi_protocol::matchmaking::MAX_PLAYERS as usize;
/// The newest open worlds of a game and mode the list looks at (OD-58 keeps five of them).
pub const LIST_SCAN: i64 = 1_000;
/// Worlds per filter answer (OD-58).
pub const LIST_CAP: usize = 5;
/// Locked Supporter worlds per filter answer for a free ticket (OD-58).
pub const LOCKED_CAP: usize = 3;
/// Worlds of a `filter=all` answer (CR-L40).
pub const ALL_CAP: usize = 50;
/// Seconds an invite or a request to join holds its seat (OD-59: about two minutes).
pub const OFFER_TTL_SECS: i64 = 120;
/// Pending invites and requests one world holds at once (OD-59).
pub const MAX_PENDING: i64 = 4;
/// Seconds after its end a seat is still reported by the seat read.
pub const ENDED_VISIBLE_SECS: i64 = 300;
/// Digits of a world code.
pub const CODE_DIGITS: usize = 8;
/// Seconds a seat may be renewed for by default (`FI_MM_SEAT_HOLD_SECS`; the Beta's 10 minutes).
pub const DEFAULT_SEAT_HOLD_SECS: i64 = 600;
/// How long a world may have nobody but its host before its heartbeat closes it (`idle`,
/// MATCHMAKING_API.md §3.3 item 4, OD-107; `FI_MM_WORLD_IDLE_SECS`).
pub const DEFAULT_WORLD_IDLE_SECS: i64 = 300;

/// The rate-limit key of `limit` for `account` (SHA-256 of the account id, no subject in clear).
pub fn limit_key(limit: &Limit, account: Uuid) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("mm:{account}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{hex}", limit.name)
}

/// Counts one request of `account` on `limit`.
pub async fn rate_limit(db: &PgPool, limit: &Limit, account: Uuid) -> Result<Hit, IdentityError> {
    limits::hit(db, &limit_key(limit, account), limit).await
}

/// Whether `account` may try a code now (neither code limit is spent). Takes the account's code
/// lock first, so concurrent attempts are checked and counted one after the other.
pub async fn code_allowed_in(conn: &mut PgConnection, account: Uuid) -> Result<Hit, IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(CODE_LOCK_NAMESPACE)
        .bind(account.to_string())
        .execute(&mut *conn)
        .await?;
    for limit in [&CODE_DAY_LIMIT, &CODE_LIMIT] {
        if let Hit::Limited { retry_after_secs } =
            limits::peek_in(conn, &limit_key(limit, account), limit).await?
        {
            return Ok(Hit::Limited { retry_after_secs });
        }
    }
    Ok(Hit::Allowed)
}

/// Counts one wrong code of `account` (both windows).
pub async fn wrong_code_in(conn: &mut PgConnection, account: Uuid) -> Result<(), IdentityError> {
    for limit in [&CODE_LIMIT, &CODE_DAY_LIMIT] {
        limits::hit_in(conn, &limit_key(limit, account), limit).await?;
    }
    Ok(())
}

/// A fresh code: 8 decimal digits, uniform (rejection sampling over 32 random bits).
pub fn random_code() -> Result<String, IdentityError> {
    const SPAN: u32 = 100_000_000;
    const LIMIT: u32 = (u32::MAX / SPAN) * SPAN;
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).map_err(|_| IdentityError::Event("no randomness".into()))?;
        let n = u32::from_le_bytes(bytes);
        if n < LIMIT {
            return Ok(format!("{:08}", n % SPAN));
        }
    }
}

// ---- Mod sets (OD-112, MATCHMAKING_API.md §2.7) -----------------------------------------------

pub use fi_protocol::matchmaking::{MODS_MAX, ModEntry, RelayLatencies};

/// A validated mod set (empty: vanilla). Display only: mods never decide a seat or a world
/// (T-1110-NOMODMATCH).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModSet {
    /// Sorted by id, unique, at most [`MODS_MAX`].
    pub entries: Vec<ModEntry>,
}

impl ModSet {
    /// The vanilla set.
    pub fn vanilla() -> Self {
        Self::default()
    }

    /// The set has mods.
    pub fn modded(&self) -> bool {
        !self.entries.is_empty()
    }
}

fn mod_id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

fn mod_version_ok(v: &str) -> bool {
    (1..=16).contains(&v.len())
        && v.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

fn mod_fingerprint_ok(f: &str) -> bool {
    f.len() == 64
        && f.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Validates a mod set as sent (absent or empty: vanilla): at most [`MODS_MAX`] entries, ids
/// `^[a-z0-9][a-z0-9._-]{0,63}$`, versions 1-16 of `[0-9A-Za-z._+-]`, fingerprints 64 lowercase
/// hex digits, ids unique and sorted (byte order). Never reorders: anything else is
/// `REQUEST.INVALID {field: "mods"}` ([`MmError::Invalid`]).
pub fn mod_set(entries: Option<&[ModEntry]>) -> Result<ModSet, MmError> {
    let entries = entries.unwrap_or_default();
    let invalid = |reason| MmError::Invalid {
        field: "mods",
        reason,
    };
    if entries.len() > MODS_MAX {
        return Err(invalid("at most 100 mods"));
    }
    for e in entries {
        if !mod_id_ok(&e.id) {
            return Err(invalid("an id is ^[a-z0-9][a-z0-9._-]{0,63}$"));
        }
        if !mod_version_ok(&e.version) {
            return Err(invalid("a version is 1 to 16 of [0-9A-Za-z._+-]"));
        }
        if !mod_fingerprint_ok(&e.fingerprint) {
            return Err(invalid("a fingerprint is 64 lowercase hex digits"));
        }
    }
    if entries
        .windows(2)
        .any(|w| w[0].id.as_bytes() >= w[1].id.as_bytes())
    {
        return Err(invalid("unique ids, sorted by id"));
    }
    Ok(ModSet {
        entries: entries.to_vec(),
    })
}

/// A caller's declaration against its session (OD-181, MATCHMAKING_API.md §2.7): a session admitted
/// without `modded` declares none (`REQUEST.INVALID {field: "mods"}`); a modded session's empty
/// set is shown as none (free roam's automatic step carries no set). Mods never decide a seat
/// (T-1110-NOMODMATCH).
fn mods_from_json(value: serde_json::Value) -> Vec<ModEntry> {
    serde_json::from_value(value).unwrap_or_default()
}

/// Who calls: the ticket's profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// `fh2`.
    pub game: String,
    /// Profile.
    pub profile_id: Uuid,
    /// Account.
    pub account_id: Uuid,
    /// Play session.
    pub session_id: Uuid,
    /// XUID.
    pub xuid: i64,
    /// `free` | `vip`.
    pub tier: String,
    /// `hardcore` | `noob` (the ticket's: modes never meet).
    pub mode: String,
}

impl Caller {
    /// A Supporter ticket (noob profiles included: tier decides worlds, OD-52).
    pub fn is_vip(&self) -> bool {
        self.tier == "vip"
    }

    /// The caller as a viewer of worlds from `ip`.
    pub fn viewer(&self, ip: Option<String>) -> Viewer {
        Viewer {
            account_id: self.account_id,
            profile_id: self.profile_id,
            xuid: self.xuid,
            ip,
        }
    }

    /// The caller's rules for [`assess`] at `cell`.
    pub fn rules(&self, cell: Option<i32>) -> Rules<'_> {
        Rules {
            profile_id: self.profile_id,
            vip: self.is_vip(),
            mode: Some(self.mode.as_str()),
            cell,
        }
    }
}

/// Refusals that are not join statuses.
#[derive(Debug, thiserror::Error)]
pub enum MmError {
    /// `MM.WORLD_NOT_FOUND`.
    #[error("no such world")]
    WorldNotFound,
    /// `MM.NOT_HOST`.
    #[error("not the host of this world")]
    NotHost,
    /// `MM.ALREADY_HOSTING {worldId}`.
    #[error("already hosting another world")]
    AlreadyHosting(Uuid),
    /// The target XUID is no profile of this game.
    #[error("unknown player")]
    UnknownPlayer,
    /// `PORTAL.NOT_FOUND` (an invite or request to anyone it may not reach).
    #[error("not found")]
    NotFound,
    /// `PORTAL.CONFLICT {reason}`.
    #[error("conflict: {0}")]
    Conflict(&'static str),
    /// `REQUEST.RATE_LIMITED` with `Retry-After`.
    #[error("rate limited")]
    RateLimited(i64),
    /// The ticket's play session has ended (`AUTH.SESSION_EXPIRED`).
    #[error("the play session has ended")]
    SessionEnded,
    /// The live seat or world expired while building an opt-in snapshot; retry, do not signal departure.
    #[error("the seat snapshot expired while it was read")]
    SnapshotChanged,
    /// A dedicated lobby's session (OD-109) as a guest: it only hosts (`AUTH.FORBIDDEN {reason:
    /// "dedicated_host_only"}`). The game plane refuses its ticket first; this is the defence in
    /// depth of joins and requests to join.
    #[error("a dedicated session only hosts")]
    DedicatedHostOnly,
    /// A request field the route could not judge alone (`REQUEST.INVALID {field}`): a
    /// dedicated world's `maxPlayers` below 2, an ordinary world's heartbeat listing more than
    /// 11 peers (OD-109).
    #[error("invalid {field}: {reason}")]
    Invalid {
        /// The body field.
        field: &'static str,
        /// What it must be.
        reason: &'static str,
    },
    /// No open official world takes the caller now (CR-S100, WORLDS_OFFICIAL.md §13): every one
    /// of its placement is full, or no spare of it is idle. Never a player-facing answer by
    /// itself: the service waits a bounded time for the agent to open one, polling, and only
    /// then answers 503 `SERVICE.UNAVAILABLE` with `Retry-After` (an incident, logged).
    #[error("no official world has room now")]
    NoOfficialRoom,
    /// Database failure.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for MmError {
    fn from(e: sqlx::Error) -> Self {
        MmError::Db(e.into())
    }
}

fn event_err(e: impl std::fmt::Display) -> IdentityError {
    IdentityError::Event(e.to_string())
}

async fn now_in(conn: &mut PgConnection) -> Result<UnixMs, IdentityError> {
    let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    Ok(ms(now))
}

async fn lock_profile_in(conn: &mut PgConnection, profile: Uuid) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(PROFILE_LOCK_NAMESPACE)
        .bind(profile.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Acquire every mutable parent of a new world/grant before the session and child rows.
/// Login replacement holds account -> session; admission and mode change hold account ->
/// profile -> session. Taking these late through FK checks would invert either order.
async fn lock_holder_in(
    conn: &mut PgConnection,
    account: Uuid,
    profile: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT 1 FROM account WHERE id = $1 FOR KEY SHARE")
        .bind(account)
        .execute(&mut *conn)
        .await?;
    // SHARE also prevents the holder's current mode changing during issuance, matching admission.
    sqlx::query("SELECT 1 FROM profile WHERE id = $1 FOR SHARE")
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// ---- What a viewer sees -------------------------------------------------------------------------

/// Who looks at worlds: the account (friendships, invites), the profile (its own world, bans,
/// kicks) and its XUID (allow-lists), from an address (same address = `local`; never stored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Viewer {
    /// Account.
    pub account_id: Uuid,
    /// Profile (nil: none).
    pub profile_id: Uuid,
    /// XUID (0: none).
    pub xuid: i64,
    /// The request's address, canonical text.
    pub ip: Option<String>,
}

/// A world's facts as a [`Viewer`] sees them (never its address).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldRow {
    /// Id.
    pub id: Uuid,
    /// Game.
    pub game: String,
    /// Name.
    pub name: String,
    /// Host profile (`None`: an official world, OD-188/189).
    pub host_profile_id: Option<Uuid>,
    /// Host account (`None`: an official world).
    pub host_account_id: Option<Uuid>,
    /// An official world's Worlds host (`FI_WORLDS` `vmId`, MATCHMAKING_API.md §2.9); `None`: a
    /// player hosts it.
    pub host_vm_id: Option<String>,
    /// Host XUID.
    pub host_xuid: i64,
    /// Host gamertag.
    pub host_gamertag: String,
    /// Host tier.
    pub host_tier: String,
    /// `free` | `supporter` (the host's tier).
    pub kind: String,
    /// Mode (the host's).
    pub mode: String,
    /// Who may play in it (OD-141, CR-S76): the host's mode, or `any` (Hardcore and Noob).
    pub open_modes: String,
    /// The lobby engines its host's world server runs besides free roam (`carmeet`, `roadtrip`).
    pub lobbies: Vec<String>,
    /// What it runs now (OD-150): `freeroam` | `roadtrip`.
    pub activity: String,
    /// The lobby's voice setting (CR-S67): `off` | `open` | `proximity`.
    pub voice: String,
    /// The host's game is not in the world while its launcher holds it for the other players
    /// (OD-180.2, S29, CR-S85): the last heartbeat's `hostAway`.
    pub host_away: bool,
    /// Players: the host (not a dedicated world's, OD-109) and the peers holding a `used` seat
    /// (the host's own report is not trusted).
    pub players: i32,
    /// The host's last report (`1 + peers`, a dedicated world's `peers`), for the record only.
    pub reported: i32,
    /// Seats (a dedicated world's: guests only).
    pub max_players: i32,
    /// Opened by a dedicated session (OD-109): the host is not a player, no idle close.
    pub dedicated: bool,
    /// The host's mod set (OD-112; empty: vanilla). The list's scan reads it empty and
    /// [`embed_details`] fills it for the selected worlds.
    pub mods: Vec<ModEntry>,
    /// The host declared a non-empty set (display only).
    pub modded: bool,
    /// The embedded members (OD-112): filled by [`embed_details`] only.
    pub members: Option<Vec<fi_protocol::matchmaking::ListMember>>,
    /// Message.
    pub message: Option<String>,
    /// Races on.
    pub races: bool,
    /// Opened.
    pub opened_at: UnixMs,
    /// `everyone` | `supporters` | `friends` | `private`.
    pub audience: String,
    /// The code (friends and private worlds; shown to the host only).
    pub code: Option<String>,
    /// The host's latency limit (ms).
    pub max_rtt_ms: Option<i32>,
    /// The host's cell.
    pub geo_cell: Option<i32>,
    /// The host's region.
    pub region: Option<String>,
    /// A private official world (CR-S96).
    pub private: bool,
    /// Open and inside its TTL.
    pub open: bool,
    /// Reserved seats (`issued`, unexpired).
    pub reserved: i64,
    /// Seats held by pending invites and requests.
    pub held: i64,
    /// Of those, the viewer's own (an invite to its account, its account's request).
    pub own_held: i64,
    /// The viewer's address is the host's.
    pub same_address: bool,
    /// The host is a profile of one of the viewer's friend accounts.
    pub friend_host: bool,
    /// A profile of one of the viewer's friend accounts is seated.
    pub friend_seated: bool,
    /// The viewer's account holds a live invite (pending or accepted, unexpired).
    pub invited: bool,
    /// The host bans the viewer, its allow-list leaves it out, or it kicked it from this world.
    pub barred: bool,
    /// `direct` | `relay` (CR-S57): `relay` = born on a relay (a symmetric host), fixed at open.
    pub transport: String,
    /// The world's relay (public key), only when `transport` is `relay`.
    pub relay_key: Option<String>,
    /// Its name in `FI_RELAYS` when the world opened (display).
    pub relay_name: Option<String>,
    /// A pineapple world (OD-149, MATCHMAKING_API.md §3.9): opened by a profile flagged at that
    /// moment, for its life.
    pub pool: bool,
    /// The viewer's profile is flagged now (Hardcore only, [`viewer_flagged!`]): it may join only
    /// pineapple worlds, and the list shows it no other.
    pub viewer_flagged: bool,
}

/// Whether the viewer's profile (`$2`) is in the pineapple pool now (OD-149): a live cheat flag of
/// a Hardcore profile. Uncorrelated, so Postgres reads it once per statement.
macro_rules! viewer_flagged {
    () => {
        "EXISTS (SELECT 1 FROM cheat_flag cf JOIN profile vp ON vp.id = cf.profile_id \
           WHERE cf.profile_id = $2 AND cf.until > clock_timestamp() AND vp.mode = 'hardcore')"
    };
}

/// The facts statement; `$1` the viewer's account, `$2` its profile, `$3` its XUID, `$4` its
/// address (text). Callers append a `WHERE` from `$5` on.
macro_rules! facts {
    () => {
        facts!("w.mods")
    };
    ($mods:literal) => {
        concat!("SELECT w.id, w.game, w.name, w.host_profile_id, w.host_account_id, coalesce(p.xuid, 0) AS host_xuid, \
         coalesce(p.name, w.name) AS host_gamertag, w.host_vm_id, w.host_tier, w.kind, w.mode, CASE WHEN w.join_mode IS NOT NULL THEN w.join_mode WHEN w.any_mode THEN 'any' ELSE w.mode END AS open_modes, w.lobbies, w.activity, w.voice, w.host_away, \
         ((CASE WHEN w.dedicated OR w.host_kind = 'official' THEN 0 ELSE 1 END) \
            + (SELECT count(*) FROM mm_grant s WHERE s.world_id = w.id AND s.state = 'used'))::int AS players, \
         w.players AS reported, w.max_players, w.dedicated, ", $mods, " AS mods, jsonb_array_length(w.mods) > 0 AS modded, w.message, w.races, w.opened_at, w.audience, w.code, w.max_rtt_ms, \
         w.geo_cell, w.region, w.official_owner_account_id IS NOT NULL AS official_private, (w.closed_at IS NULL AND w.expires_at > clock_timestamp()) AS open, \
         (SELECT count(*) FROM mm_grant g WHERE g.world_id = w.id AND g.state = 'issued' \
            AND g.expires_at > clock_timestamp()) AS reserved, \
         (SELECT count(*) FROM world_invite i WHERE i.world_id = w.id AND i.state = 'pending' \
            AND i.expires_at > clock_timestamp()) AS held, \
         (SELECT count(*) FROM world_invite i WHERE i.world_id = w.id AND i.state = 'pending' \
            AND i.expires_at > clock_timestamp() \
            AND ((i.kind = 'invite' AND i.to_account_id = $1) OR (i.kind = 'request' AND i.from_account_id = $1))) AS own_held, \
         coalesce(w.endpoint_ip = $4::text::inet, false) AS same_address, \
         EXISTS (SELECT 1 FROM account_friendship f WHERE f.account_id = $1 AND f.friend_id = w.host_account_id) AS friend_host, \
         EXISTS (SELECT 1 FROM mm_grant g JOIN account_friendship f ON f.account_id = $1 AND f.friend_id = g.account_id \
            WHERE g.world_id = w.id AND g.state = 'used') AS friend_seated, \
         EXISTS (SELECT 1 FROM world_invite i WHERE i.world_id = w.id AND i.kind = 'invite' AND i.to_account_id = $1 \
            AND i.state IN ('pending', 'accepted') AND i.expires_at > clock_timestamp()) AS invited, \
         (EXISTS (SELECT 1 FROM world_ban b WHERE b.game = w.game AND b.host_profile_id = w.host_profile_id \
               AND b.target_profile_id = $2 AND (b.until IS NULL OR b.until > clock_timestamp())) \
            OR (w.allow IS NOT NULL AND w.host_profile_id <> $2 AND NOT ($3 = ANY (w.allow))) \
            OR EXISTS (SELECT 1 FROM mm_grant k WHERE k.world_id = w.id AND k.profile_id = $2 AND k.state = 'kicked')) AS barred, \
         w.transport, w.relay_key, w.relay_name, w.pool, ", viewer_flagged!(), " AS viewer_flagged \
         FROM world w LEFT JOIN profile p ON p.id = w.host_profile_id")
    };
}

/// The list's scan (`world_open_list`). Mods never decide which worlds a caller sees
/// (T-1110-NOMODMATCH): vanilla and modded worlds share one list. The scan reads no mod set
/// (`[]`): [`embed_details`] reads the selected worlds' sets.
///
/// OD-141 (CR-S76): the newest worlds of the caller's mode (the partial index of the side), then
/// the newest `any` worlds of the other mode (`world_open_any`), each bounded by `$7`; [`select`]
/// orders the candidates.
macro_rules! list_sql {
    () => {
        concat!(
            "(",
            facts!("'[]'::jsonb"),
            " WHERE w.game = $5 AND w.mode = $6 AND w.closed_at IS NULL \
               AND w.expires_at > clock_timestamp() \
               AND (w.audience <> 'private' OR w.host_profile_id = $2) AND w.host_kind <> 'official' \
               AND ($8::text IS NULL OR w.activity = $8) AND (w.pool OR NOT ",
            viewer_flagged!(),
            ") \
             ORDER BY w.opened_at DESC, w.id DESC LIMIT $7) \
             UNION ALL (",
            facts!("'[]'::jsonb"),
            " WHERE w.game = $5 AND w.mode <> $6 AND w.any_mode AND w.closed_at IS NULL \
               AND w.expires_at > clock_timestamp() \
               AND (w.audience <> 'private' OR w.host_profile_id = $2) AND w.host_kind <> 'official' \
               AND ($8::text IS NULL OR w.activity = $8) AND (w.pool OR NOT ",
            viewer_flagged!(),
            ") \
             ORDER BY w.opened_at DESC, w.id DESC LIMIT $7)"
        )
    };
}

macro_rules! friends_list_sql {
    () => {
        concat!(
            facts!("'[]'::jsonb"),
            " WHERE w.id IN ( \
                 SELECT h.id FROM account_friendship f JOIN world h ON h.game = $5 AND h.host_account_id = f.friend_id \
                   AND h.closed_at IS NULL WHERE f.account_id = $1 \
                 UNION SELECT g.world_id FROM account_friendship f JOIN profile fp ON fp.account_id = f.friend_id AND fp.game = $5 \
                   JOIN mm_grant g ON g.game = $5 AND g.profile_id = fp.id AND g.state = 'used' WHERE f.account_id = $1) \
               AND w.game = $5 AND (w.mode = $6 OR w.any_mode) AND w.closed_at IS NULL \
               AND w.expires_at > clock_timestamp() \
               AND (w.audience <> 'private' OR w.host_profile_id = $2) AND w.host_kind <> 'official' \
               AND ($8::text IS NULL OR w.activity = $8) AND (w.pool OR NOT ",
            viewer_flagged!(),
            ") \
             ORDER BY w.opened_at DESC, w.id DESC LIMIT $7"
        )
    };
}

/// The SQL of [`list_worlds`] (tests EXPLAIN it): the newest open worlds, modded or not, of a game (`$5`) and mode (`$6`), private ones only to their host, at most `$7`, then
/// at most `$7` of the other mode's worlds opened to `any` (OD-141).
pub const LIST_SQL: &str = list_sql!();

/// The SQL of the `friends` filter (tests EXPLAIN it): the open worlds of
/// `$5`/`$6` hosted by a profile of one of the caller's friend accounts (`world_open_host_account`)
/// or with one seated (`mm_grant_one_live`), whatever their age, private ones only to their host,
/// at most `$7`.
pub const FRIENDS_LIST_SQL: &str = friends_list_sql!();

/// The SQL of the `all` filter (CR-L40; tests EXPLAIN it): the newest open worlds of the game
/// `$5`, every mode and both sides of `modded`, private ones only to their host, at most `$7`
/// (`$6`, the caller's mode, is bound and unused: the viewer's mode decides `why`, not the scan).
pub const ALL_LIST_SQL: &str = concat!(
    facts!("'[]'::jsonb"),
    " WHERE w.game = $5 AND $6::text IS NOT NULL AND w.closed_at IS NULL \
       AND w.expires_at > clock_timestamp() \
       AND (w.audience <> 'private' OR w.host_profile_id = $2) AND w.host_kind <> 'official' \
       AND ($8::text IS NULL OR w.activity = $8) AND (w.pool OR NOT ",
    viewer_flagged!(),
    ") \
     ORDER BY w.opened_at DESC, w.id DESC LIMIT $7"
);

const BY_IDS_SQL: &str = concat!(facts!(), " WHERE w.id = ANY($5)");
const BY_CODE_SQL: &str = concat!(
    facts!(),
    " WHERE w.game = $5 AND w.code = $6 AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
       AND w.host_kind <> 'official'"
);

/// Whether a world of the host's `mode` and its `open_modes` admits a profile of `viewer` mode
/// (OD-141, CR-S76: an `any` world admits Hardcore and Noob; any other only its host's mode).
/// CR-S101 (§2.11): a private official world's leader may restrict it to one mode (`join_mode`),
/// which is then its `open_modes` while `mode` stays; so the mode compared is `open_modes`, which
/// equals `mode` for every world that is neither `any` nor restricted.
pub fn admits(_mode: &str, open_modes: &str, viewer: &str) -> bool {
    open_modes == OPEN_MODES_ANY || open_modes == viewer
}

/// [`WorldRow::open_modes`] of a world open to Hardcore and Noob.
pub const OPEN_MODES_ANY: &str = "any";

impl WorldRow {
    /// Whether a profile of `mode` may be listed, join, ask and be seated here ([`admits`]).
    pub fn admits(&self, mode: &str) -> bool {
        admits(&self.mode, &self.open_modes, mode)
    }
}

fn world_row(r: &sqlx::postgres::PgRow) -> Result<WorldRow, sqlx::Error> {
    Ok(WorldRow {
        id: r.try_get("id")?,
        game: r.try_get("game")?,
        name: r.try_get("name")?,
        host_profile_id: r.try_get("host_profile_id")?,
        host_account_id: r.try_get("host_account_id")?,
        host_vm_id: r.try_get("host_vm_id")?,
        host_xuid: r.try_get("host_xuid")?,
        host_gamertag: r
            .try_get::<Option<String>, _>("host_gamertag")?
            .unwrap_or_default(),
        host_tier: r.try_get("host_tier")?,
        kind: r.try_get("kind")?,
        mode: r.try_get("mode")?,
        open_modes: r.try_get("open_modes")?,
        lobbies: r.try_get("lobbies")?,
        activity: r.try_get("activity")?,
        voice: r.try_get("voice")?,
        host_away: r.try_get("host_away")?,
        players: r.try_get("players")?,
        reported: r.try_get("reported")?,
        max_players: r.try_get("max_players")?,
        dedicated: r.try_get("dedicated")?,
        mods: mods_from_json(r.try_get("mods")?),
        modded: r.try_get("modded")?,
        members: None,
        message: r.try_get("message")?,
        races: r.try_get("races")?,
        opened_at: ms(r.try_get("opened_at")?),
        audience: r.try_get("audience")?,
        code: r.try_get("code")?,
        max_rtt_ms: r.try_get("max_rtt_ms")?,
        geo_cell: r.try_get("geo_cell")?,
        region: r.try_get("region")?,
        private: r.try_get("official_private")?,
        open: r.try_get("open")?,
        reserved: r.try_get("reserved")?,
        held: r.try_get("held")?,
        own_held: r.try_get("own_held")?,
        same_address: r.try_get("same_address")?,
        friend_host: r.try_get("friend_host")?,
        friend_seated: r.try_get("friend_seated")?,
        invited: r.try_get("invited")?,
        barred: r.try_get("barred")?,
        transport: r.try_get("transport")?,
        relay_key: r.try_get("relay_key")?,
        relay_name: r.try_get("relay_name")?,
        pool: r.try_get("pool")?,
        viewer_flagged: r.try_get("viewer_flagged")?,
    })
}

fn bind_viewer<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    v: &'q Viewer,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    q.bind(v.account_id)
        .bind(v.profile_id)
        .bind(v.xuid)
        .bind(v.ip.as_deref())
}

/// The worlds `ids` as `viewer` sees them (any state; the caller checks [`WorldRow::open`]).
pub async fn worlds_by_ids_in(
    conn: &mut PgConnection,
    viewer: &Viewer,
    ids: &[Uuid],
) -> Result<Vec<WorldRow>, IdentityError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = bind_viewer(sqlx::query(BY_IDS_SQL), viewer)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.iter().map(world_row).collect::<Result<_, _>>()?)
}

async fn world_in(
    conn: &mut PgConnection,
    viewer: &Viewer,
    id: Uuid,
) -> Result<Option<WorldRow>, IdentityError> {
    Ok(worlds_by_ids_in(conn, viewer, &[id])
        .await?
        .into_iter()
        .next())
}

/// The embedded details statement (OD-112, tests EXPLAIN it): for the worlds `$1`, their mod set
/// and their members as the roster orders them (§3.7: the host unless dedicated, then the seated
/// peers by arrival, then the live reserved seats by issue), at most 12, without XUIDs or
/// relations; each member with its declared set (OD-181: the host's row the world's, a seat's its
/// own). One statement, one snapshot.
pub const DETAILS_SQL: &str = "SELECT w.id, w.mods, coalesce((SELECT jsonb_agg(jsonb_build_object('gamertag', x.gamertag,        'tier', x.tier, 'role', x.role, 'mods', x.mods) ORDER BY x.rank, x.since, x.grant_id) FROM (          SELECT m.rank, m.role, m.tier, m.since, m.grant_id, m.mods,                 p.name AS gamertag          FROM (SELECT 0 AS rank, 'host' AS role, w.host_profile_id AS profile_id, w.host_tier AS tier,                       w.opened_at AS since, NULL::uuid AS grant_id, w.mods WHERE NOT w.dedicated                UNION ALL                SELECT CASE WHEN g.state = 'used' THEN 1 ELSE 2 END,                       CASE WHEN g.state = 'used' THEN 'seated' ELSE 'reserved' END, g.profile_id, g.tier,                       CASE WHEN g.state = 'used' THEN coalesce(g.used_at, g.issued_at) ELSE g.issued_at END, g.id, g.mods                FROM mm_grant g WHERE g.world_id = w.id                  AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) m          JOIN profile p ON p.id = m.profile_id          ORDER BY m.rank, m.since, m.grant_id LIMIT 12) x), '[]'::jsonb) AS members      FROM world w WHERE w.id = ANY($1)";

/// Fills [`WorldRow::mods`] and [`WorldRow::members`] of `rows` (OD-112: the list's selection, at
/// most [`LIST_CAP`] + [`LOCKED_CAP`], one world, a world by code) in one bounded statement
/// ([`DETAILS_SQL`]). Run after the selection, on a connection of its own.
pub async fn embed_details<'a>(
    db: &PgPool,
    rows: impl IntoIterator<Item = &'a mut WorldRow>,
) -> Result<(), IdentityError> {
    let mut rows: Vec<&mut WorldRow> = rows.into_iter().collect();
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let found = sqlx::query(DETAILS_SQL).bind(&ids).fetch_all(db).await?;
    let mut by_id = std::collections::HashMap::new();
    for r in &found {
        let id: Uuid = r.try_get("id")?;
        let mods = mods_from_json(r.try_get("mods")?);
        let members: Vec<fi_protocol::matchmaking::ListMember> =
            serde_json::from_value(r.try_get("members")?).unwrap_or_default();
        by_id.insert(id, (mods, members));
    }
    for row in rows.iter_mut() {
        if let Some((mods, members)) = by_id.remove(&row.id) {
            row.mods = mods;
            row.members = Some(members);
        }
    }
    Ok(())
}

/// [`embed_details`] for what the viewer may see of each assessed world (OD-112 review,
/// MATCHMAKING_API.md §2.7): a world it could not join without a code or an invite
/// ([`Assessed::needs_code`]: a `private` world, a `friends` world of a host who is not its
/// friend, unless invited or its host) shows no mod set and no members (`modded` stays), unless
/// the caller gave the world's code (`code_given`, the lookup by code). A locked Supporter world
/// shown to a free ticket (`why: "tier"`) keeps its details.
pub async fn embed_visible<'a>(
    db: &PgPool,
    rows: impl IntoIterator<Item = &'a mut Assessed>,
    code_given: bool,
) -> Result<(), IdentityError> {
    let mut shown = Vec::new();
    for a in rows {
        if a.needs_code && !code_given {
            a.row.mods.clear();
            a.row.members = None;
        } else {
            shown.push(&mut a.row);
        }
    }
    embed_details(db, shown).await
}

/// Why a world cannot be joined now (MATCHMAKING_API.md §2.1), in the order decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Why {
    /// The viewer hosts it.
    Own,
    /// Another mode.
    Mode,
    /// The pineapple pool (OD-149): a pineapple world to a clean viewer, or a clean world to a
    /// flagged one.
    Pool,
    /// A `supporters` world, free ticket.
    Tier,
    /// Banned, off the allow-list, or kicked.
    Host,
    /// A `friends` world, not a friend, no invite, no code.
    Friends,
    /// A `private` world without an invite or the code.
    Code,
    /// No open seat.
    Full,
    /// Over the host's latency limit.
    Latency,
}

impl Why {
    /// The wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Why::Own => "own",
            Why::Mode => "mode",
            Why::Pool => "pool",
            Why::Tier => "tier",
            Why::Host => "host",
            Why::Friends => "friends",
            Why::Code => "code",
            Why::Full => "full",
            Why::Latency => "latency",
        }
    }
}

/// The viewer's side of [`assess`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules<'a> {
    /// The viewer's profile (its own world).
    pub profile_id: Uuid,
    /// A Supporter.
    pub vip: bool,
    /// The viewer's mode (`None`: unknown, not checked).
    pub mode: Option<&'a str>,
    /// The viewer's cell (behind the edge).
    pub cell: Option<i32>,
}

/// A world with what the viewer may do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessed {
    /// The facts.
    pub row: WorldRow,
    /// Viewer → host.
    pub estimate: Estimate,
    /// Seats left for this viewer.
    pub open_seats: i64,
    /// No seat for this viewer.
    pub full: bool,
    /// This viewer must give the code.
    pub needs_code: bool,
    /// The viewer is the host.
    pub own: bool,
    /// `None`: a join now would be granted (with the code when `needs_code`).
    pub why: Option<Why>,
}

impl Assessed {
    /// A join now would be granted.
    pub fn can_join(&self) -> bool {
        self.why.is_none()
    }
}

/// What the viewer may do with `w` (`code_ok`: it gave the world's code).
pub fn assess(w: &WorldRow, v: &Rules<'_>, code_ok: bool) -> Assessed {
    let own = w.host_profile_id == Some(v.profile_id);
    let estimate = if own {
        Estimate::between(None, None, true)
    } else {
        Estimate::between(v.cell, w.geo_cell, w.same_address)
    };
    let taken = i64::from(w.players) + w.reserved + w.held - w.own_held;
    let open_seats = (i64::from(w.max_players) - taken).max(0);
    let full = taken >= i64::from(w.max_players);
    let passed = w.invited || code_ok;
    let needs_code = !own
        && !w.invited
        && (w.audience == "private" || (w.audience == "friends" && !w.friend_host));
    let max_rtt = w.max_rtt_ms.and_then(|m| u32::try_from(m).ok());
    let why = if own {
        Some(Why::Own)
    } else if v.mode.is_some_and(|m| !w.admits(m)) {
        Some(Why::Mode)
    } else if w.pool != w.viewer_flagged {
        Some(Why::Pool)
    } else if w.barred {
        Some(Why::Host)
    } else if w.audience == "friends" && !w.friend_host && !passed {
        Some(Why::Friends)
    } else if w.audience == "private" && !passed {
        Some(Why::Code)
    } else if full {
        Some(Why::Full)
    } else if !estimate.within(max_rtt) {
        Some(Why::Latency)
    } else {
        None
    };
    Assessed {
        row: w.clone(),
        estimate,
        open_seats,
        full,
        needs_code,
        own,
        why,
    }
}

/// The list's filters (OD-58: one is required).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Every world the viewer may see (open with the owner; one entry here).
    Global,
    /// `local` or `near` worlds.
    Near,
    /// Worlds a friend hosts or is seated in.
    Friends,
    /// Every visible world of the game, every mode, modded and vanilla (CR-L40).
    All,
}

impl Filter {
    /// Parses a wire string.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "global" => Some(Filter::Global),
            "near" => Some(Filter::Near),
            "friends" => Some(Filter::Friends),
            "all" => Some(Filter::All),
            _ => None,
        }
    }

    /// The wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Filter::Global => "global",
            Filter::Near => "near",
            Filter::Friends => "friends",
            Filter::All => "all",
        }
    }
}

/// The candidates of `filter` as the caller sees them: the newest open worlds of its game and mode
/// ([`LIST_SQL`]), or for `friends` every open world a friend hosts or is seated in
/// ([`FRIENDS_LIST_SQL`]), on one side of `modded` (OD-112: vanilla worlds only, or modded ones
/// only); the caller then [`select`]s and [`embed_details`] the selection.
pub async fn list_worlds(
    db: &PgPool,
    caller: &Caller,
    ip: Option<String>,
    filter: Filter,
) -> Result<Vec<WorldRow>, IdentityError> {
    list_worlds_running(db, caller, ip, filter, None).await
}

/// [`list_worlds`] of the worlds running `activity` now (OD-150; `None`: every world), filtered
/// in the scan, so the newest worlds of another activity never crowd them out.
pub async fn list_worlds_running(
    db: &PgPool,
    caller: &Caller,
    ip: Option<String>,
    filter: Filter,
    activity: Option<&str>,
) -> Result<Vec<WorldRow>, IdentityError> {
    let viewer = caller.viewer(ip);
    let sql = match filter {
        Filter::All => ALL_LIST_SQL,
        Filter::Friends => FRIENDS_LIST_SQL,
        _ => LIST_SQL,
    };
    let rows = bind_viewer(sqlx::query(sql), &viewer)
        .bind(&caller.game)
        .bind(&caller.mode)
        .bind(LIST_SCAN)
        .bind(activity)
        .fetch_all(db)
        .await?;
    Ok(rows.iter().map(world_row).collect::<Result<_, _>>()?)
}

/// The one selection of the list (OD-58): what a filter shows, in its order, capped.
///
/// Hidden: worlds that bar the viewer; with `not_full`, worlds without a seat for it. `near`
/// keeps `local` and `near` worlds, `friends` the worlds a friend hosts or is seated in. Order:
/// the filter's key (`near`: the class; `friends`: a friend hosts), then `canJoin`, then players
/// descending, then the newest. A Supporter gets the first [`LIST_CAP`]; a free viewer the first
/// [`LIST_CAP`] its tier allows and, apart, the first [`LOCKED_CAP`] Supporter-only worlds.
pub fn select(
    rows: Vec<Assessed>,
    filter: Filter,
    free: bool,
    not_full: bool,
) -> (Vec<Assessed>, Vec<Assessed>) {
    let mut rows: Vec<Assessed> = rows
        .into_iter()
        .filter(|a| !a.row.barred && !(not_full && a.full))
        .filter(|a| match filter {
            Filter::Global | Filter::All => true,
            Filter::Near => matches!(a.estimate.class, PingClass::Local | PingClass::Near),
            Filter::Friends => a.row.friend_host || a.row.friend_seated,
        })
        .collect();
    if filter == Filter::All {
        // CR-L40: `canJoin` first, then the estimated round trip (unknown last), then players
        // descending; every world in `items`, at most [`ALL_CAP`], `locked` empty.
        rows.sort_by_key(|a| {
            (
                !a.can_join(),
                a.estimate.rtt_ms.unwrap_or(u32::MAX),
                std::cmp::Reverse(a.row.players),
                std::cmp::Reverse(a.row.opened_at.millis()),
                a.row.id,
            )
        });
        rows.truncate(ALL_CAP);
        return (rows, Vec::new());
    }
    rows.sort_by_key(|a| {
        let first = match filter {
            Filter::Global | Filter::All => 0,
            Filter::Near => a.estimate.class as i64,
            Filter::Friends => i64::from(!a.row.friend_host),
        };
        (
            first,
            !a.can_join(),
            std::cmp::Reverse(a.row.players),
            std::cmp::Reverse(a.row.opened_at.millis()),
            a.row.id,
        )
    });
    if !free {
        rows.truncate(LIST_CAP);
        return (rows, Vec::new());
    }
    let (mut locked, mut items): (Vec<Assessed>, Vec<Assessed>) =
        rows.into_iter().partition(|a| a.why == Some(Why::Tier));
    items.truncate(LIST_CAP);
    locked.truncate(LOCKED_CAP);
    (items, locked)
}

/// One open world as `caller` sees it: one the list would show it, its own, or one its account is
/// invited to; `None` otherwise.
pub async fn world_for_caller(
    db: &PgPool,
    caller: &Caller,
    ip: Option<String>,
    id: Uuid,
) -> Result<Option<WorldRow>, IdentityError> {
    let mut conn = db.acquire().await?;
    let viewer = caller.viewer(ip);
    let Some(w) = world_in(&mut conn, &viewer, id).await?.filter(|w| {
        w.open
            && w.game == caller.game
            // CR-S101: a private official world's leader may restrict it to the other mode while
            // the caller is seated there; its access (below) decides who reads it.
            && (w.private || w.admits(&caller.mode))
            && !w.barred
            // OD-149: a flagged caller reads only pineapple worlds (a clean one sees them marked).
            && (w.pool || !w.viewer_flagged)
            && (w.audience != "private" || w.host_profile_id == Some(caller.profile_id) || w.invited)
    }) else {
        return Ok(None);
    };
    // CR-S96 (§2.10): a private official world only to an account with access or a live invite.
    if w.private
        && !private_access_in(&mut conn, id, caller.account_id)
            .await?
            .any()
    {
        return Ok(None);
    }
    // OD-190 (MATCHMAKING_API.md §2.9): an official world only to a caller seated in it or a
    // friend of someone seated in it.
    if w.host_vm_id.is_some() && !w.private && !w.friend_seated {
        let seated: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = $1 AND g.profile_id = $2 \
               AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())))",
        )
        .bind(id)
        .bind(caller.profile_id)
        .fetch_one(&mut *conn)
        .await?;
        if !seated {
            return Ok(None);
        }
    }
    Ok(Some(w))
}

/// What a code lookup found.
#[derive(Clone, Debug, PartialEq)]
pub enum ByCode {
    /// The open world with that code, of the caller's mode, that does not bar it.
    Found(Box<WorldRow>),
    /// None: counted as a wrong code.
    Wrong,
    /// The code limits are spent.
    Limited(i64),
}

/// `GET /mm/v1/worlds/by-code/{code}` (`code`: 8 digits): the exact lookup outside the list.
pub async fn world_by_code(
    db: &PgPool,
    caller: &Caller,
    ip: Option<String>,
    code: &str,
) -> Result<ByCode, IdentityError> {
    let mut tx = db.begin().await?;
    if let Hit::Limited { retry_after_secs } = code_allowed_in(&mut tx, caller.account_id).await? {
        return Ok(ByCode::Limited(retry_after_secs));
    }
    let viewer = caller.viewer(ip);
    let found = bind_viewer(sqlx::query(BY_CODE_SQL), &viewer)
        .bind(&caller.game)
        .bind(code)
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| world_row(&r))
        .transpose()?
        .filter(|w| w.admits(&caller.mode) && !w.barred && (w.pool || !w.viewer_flagged));
    let answer = match found {
        Some(w) => ByCode::Found(Box::new(w)),
        None => {
            wrong_code_in(&mut tx, caller.account_id).await?;
            ByCode::Wrong
        }
    };
    tx.commit().await?;
    Ok(answer)
}

// ---- Relay fallback (CR-S57) -------------------------------------------------------------------

/// A relay the server trusts (`FI_RELAYS`, MATCHMAKING_API.md §5b): its public key and name. The
/// service keeps its endpoint and signs it into grants; storage needs only the identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedRelay {
    /// Public key (standard base64 of 32 bytes; the beholder identity).
    pub key: String,
    /// The operator's name for it (display).
    pub name: String,
}

/// What a host's `PUT` (or combined `POST`) says about relays: its NAT test (read when the world
/// opens) and its measured round trips (`None`: not sent, the stored map is kept).
#[derive(Clone, Copy, Debug, Default)]
pub struct HostRelay<'a> {
    /// `open` | `restricted` | `symmetric` (validated by the service).
    pub nat: Option<&'a str>,
    /// Public key → ms.
    pub relays: Option<&'a RelayLatencies>,
    /// The relays the server trusts, in `FI_RELAYS` order (empty: no relay).
    pub trusted: &'a [TrustedRelay],
    /// The host asked for a relay-born world ("Always connect through the relay", OD-146).
    pub relay_only: bool,
    /// The host was observed over IPv6 while its external port is an IPv4 measurement (CR-S40):
    /// a new world is born on a relay (its direct route cannot be told), an open one keeps its
    /// stored route.
    pub family_mismatch: bool,
    /// What the world runs now (OD-150; validated by the service): `None` keeps it (a new world:
    /// `freeroam`). `roadtrip` needs `roadtrip` among the world's `lobbies`.
    pub activity: Option<&'a str>,
    /// The lobby's voice setting (CR-S67; validated by the service): `None` keeps it (a new world:
    /// `open`). Only the host's routes set it, at any time.
    pub voice: Option<&'a str>,
    /// The heartbeat's `hostAway` (OD-180.2, S29, CR-S85; MATCHMAKING_API.md §2.5, §3.3 item 6):
    /// `Some` on every heartbeat and combined announce (absent on the wire is `Some(false)`),
    /// `None` on a `PUT` (kept). With `Some(true)`, an open world left with no other player (no
    /// live seat, no pending invite or request, no peer listed) closes (`host_away`).
    pub host_away: Option<bool>,
}

/// What a joiner says about relays (CR-S57).
#[derive(Clone, Copy, Debug, Default)]
pub struct JoinRelay<'a> {
    /// The region the launcher chose (OD-205; `None`: any): official placement only.
    pub region: Option<&'a str>,
    /// The joiner's NAT test.
    pub nat: Option<&'a str>,
    /// Public key → ms.
    pub relays: Option<&'a RelayLatencies>,
    /// The relays the server trusts, in `FI_RELAYS` order.
    pub trusted: &'a [TrustedRelay],
    /// Live relayed seats allowed per world (`FI_RELAY_SEATS_PER_WORLD`).
    pub max_relayed: u32,
    /// The joiner asked for a relay-born seat ("Always connect through the relay", OD-146).
    pub relay_only: bool,
}

/// A grant's relay and transport, decided at grant time and stored on the seat, so every
/// re-signed copy (join answer, seat read, renew, punches) names the same relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRelay {
    /// The relay's key (`None`: no fallback for this pair).
    pub key: Option<String>,
    /// `direct` | `relay`.
    pub transport: &'static str,
}

impl GrantRelay {
    /// No relay: always direct.
    pub const fn none() -> Self {
        Self {
            key: None,
            transport: "direct",
        }
    }
}

/// The relay a pair meets at (MATCHMAKING_API.md §5b): among the trusted relays (in `trusted`
/// order), those both sides reported; the one minimising the larger round trip, then the smaller
/// sum, then the list order. Both maps are required (CR-S57 review M1): a side without one is an
/// older launcher whose grant verifier refuses the unknown `relay` claim, so its pair gets no
/// relay. A relay the server does not trust is never chosen. (A world's own relay is
/// `choose_relay(Some(host), Some(host), ..)`: the host's lowest.)
pub fn choose_relay<'t>(
    host: Option<&RelayLatencies>,
    joiner: Option<&RelayLatencies>,
    trusted: &'t [TrustedRelay],
) -> Option<&'t TrustedRelay> {
    let (host, joiner) = (host?, joiner?);
    let side = |map: &RelayLatencies, key: &str| map.get(key).map(|ms| u64::from(*ms));
    trusted
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let h = side(host, &r.key)?;
            let j = side(joiner, &r.key)?;
            Some(((h.max(j), h + j, i), r))
        })
        .min_by_key(|(score, _)| *score)
        .map(|(_, r)| r)
}

/// A grant's relay and transport in a world (§5b): a relay-born world's relay (while trusted),
/// else the pair's choice; `relay` when the world is, or when a symmetric joiner meets a
/// restricted host; a grant without a relay is always direct. A joiner without a relay report
/// (an older launcher, an accepted request) cannot enter a relay-born world:
/// [`MmError::Conflict`] `launcher_update_required` (CR-S57 review M1).
pub fn grant_relay(
    world_transport: &str,
    world_relay: Option<&str>,
    host_nat: Option<&str>,
    host_relays: Option<&RelayLatencies>,
    joiner: &JoinRelay<'_>,
) -> Result<GrantRelay, MmError> {
    let key = if world_transport == "relay" {
        let key = world_relay
            .filter(|k| joiner.trusted.iter().any(|t| t.key == *k))
            .map(str::to_owned);
        if key.is_some() && joiner.relays.is_none() {
            return Err(MmError::Conflict("launcher_update_required"));
        }
        key
    } else {
        choose_relay(host_relays, joiner.relays, joiner.trusted).map(|r| r.key.clone())
    };
    // OD-146: a joiner who always connects through the relay gets a relay-born seat or a refusal,
    // never a silently direct one.
    if joiner.relay_only && key.is_none() {
        return Err(MmError::Conflict("no_relay"));
    }
    let relay_now = world_transport == "relay"
        || joiner.relay_only
        || (joiner.nat == Some("symmetric") && host_nat == Some("restricted"));
    Ok(GrantRelay {
        transport: if key.is_some() && relay_now {
            "relay"
        } else {
            "direct"
        },
        key,
    })
}

/// Live relayed seats of `world` other than `except`, on the caller's transaction.
async fn relayed_seats_in(
    conn: &mut PgConnection,
    world: Uuid,
    except: Option<Uuid>,
) -> Result<i64, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM mm_grant WHERE world_id = $1 AND transport = 'relay' \
           AND (state = 'used' OR (state = 'issued' AND expires_at > clock_timestamp())) \
           AND ($2::uuid IS NULL OR id <> $2)",
    )
    .bind(world)
    .bind(except)
    .fetch_one(&mut *conn)
    .await?)
}

/// CR-S57 review M4: a new relayed seat needs room in its world (`max_relayed`) and in its
/// account's hourly budget ([`RELAY_LIMIT`], counted here); else `relay_busy`.
async fn relay_room_in(
    conn: &mut PgConnection,
    world: Uuid,
    except: Option<Uuid>,
    account: Uuid,
    max_relayed: u32,
) -> Result<(), MmError> {
    if relayed_seats_in(conn, world, except).await? >= i64::from(max_relayed) {
        return Err(MmError::Conflict("relay_busy"));
    }
    if let Hit::Limited { .. } =
        limits::hit_in(conn, &limit_key(&RELAY_LIMIT, account), &RELAY_LIMIT).await?
    {
        return Err(MmError::Conflict("relay_busy"));
    }
    Ok(())
}

/// Only the trusted entries of `relays`, as stored JSON (`{publicKey: ms}`).
fn trusted_relays_json(relays: &RelayLatencies, trusted: &[TrustedRelay]) -> serde_json::Value {
    serde_json::Value::Object(
        relays
            .iter()
            .filter(|(k, _)| trusted.iter().any(|t| &t.key == *k))
            .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
            .collect(),
    )
}

fn relays_from_json(value: Option<serde_json::Value>) -> Option<RelayLatencies> {
    value.and_then(|v| serde_json::from_value(v).ok())
}

/// A world's relay facts for a grant (§5b).
struct WorldRelay {
    transport: String,
    key: Option<String>,
    nat: Option<String>,
    relays: Option<RelayLatencies>,
    /// OD-160: the world admits Hardcore profiles (`modes` `hardcore` or `any`), so the
    /// relay-as-exception caps do not apply to it.
    hardcore: bool,
}

/// A world's relay facts for a grant, on the caller's transaction (the world row is locked).
async fn world_relay_in(conn: &mut PgConnection, world: Uuid) -> Result<WorldRelay, IdentityError> {
    let (transport, key, nat, relays, hardcore): (
        String,
        Option<String>,
        Option<String>,
        Option<serde_json::Value>,
        bool,
    ) = sqlx::query_as(
        "SELECT transport, relay_key, nat, relays, (mode = 'hardcore' OR any_mode) FROM world WHERE id = $1",
    )
    .bind(world)
    .fetch_one(&mut *conn)
    .await?;
    Ok(WorldRelay {
        transport,
        key,
        nat,
        relays: relays_from_json(relays),
        hardcore,
    })
}

/// A beholder check older than this no longer says anything (MATCHMAKING_API.md §5b "Relay by
/// mode": relays announce every 30 s and the beholder lists those seen within 90 s).
pub const RELAY_DIRECTORY_FRESH_SECS: i64 = 90;
/// A relay counts as down only once it has been missing from every check for this long (two
/// checks 30 s apart at least): one empty or partial listing never opens a world direct.
pub const RELAY_DIRECTORY_MISSED_SECS: i64 = 30;

/// OD-160: records one read of the beholder's relay list: for every trusted relay, whether the
/// list named it, checked now, and since when it has been missing (kept across consecutive
/// misses, cleared when listed). Idempotent; instances may race (the latest check wins).
pub async fn record_relay_directory(
    db: &PgPool,
    trusted: &[TrustedRelay],
    listed: &[String],
) -> Result<(), IdentityError> {
    let keys: Vec<String> = trusted.iter().map(|t| t.key.clone()).collect();
    let seen: Vec<bool> = trusted.iter().map(|t| listed.contains(&t.key)).collect();
    sqlx::query(
        "INSERT INTO mm_relay_directory (relay_key, listed, checked_at, missed_since) \
         SELECT k, l, clock_timestamp(), CASE WHEN l THEN NULL ELSE clock_timestamp() END \
         FROM unnest($1::text[], $2::boolean[]) AS t(k, l) \
         ON CONFLICT (relay_key) DO UPDATE SET listed = EXCLUDED.listed, checked_at = EXCLUDED.checked_at, \
           missed_since = CASE WHEN EXCLUDED.listed THEN NULL \
             ELSE coalesce(mm_relay_directory.missed_since, EXCLUDED.checked_at) END",
    )
    .bind(&keys)
    .bind(&seen)
    .execute(db)
    .await?;
    Ok(())
}

/// OD-160: the trusted relays down: the latest check, within [`RELAY_DIRECTORY_FRESH_SECS`], did
/// not list it, and it has been missing for [`RELAY_DIRECTORY_MISSED_SECS`] at least. No recent
/// check: none is down.
pub async fn relays_down_in(
    conn: &mut PgConnection,
    trusted: &[TrustedRelay],
) -> Result<Vec<String>, IdentityError> {
    if trusted.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<String> = trusted.iter().map(|t| t.key.clone()).collect();
    Ok(sqlx::query_scalar(
        "SELECT relay_key FROM mm_relay_directory WHERE relay_key = ANY($1) AND NOT listed \
           AND checked_at > clock_timestamp() - make_interval(secs => $2) \
           AND missed_since <= clock_timestamp() - make_interval(secs => $3)",
    )
    .bind(&keys)
    .bind(RELAY_DIRECTORY_FRESH_SECS as f64)
    .bind(RELAY_DIRECTORY_MISSED_SECS as f64)
    .fetch_all(&mut *conn)
    .await?)
}

/// What a new world's host reports for the transport decision (OD-160).
#[derive(Clone, Copy, Debug)]
pub struct OpenReport<'a> {
    /// The world admits Hardcore profiles (`modes` `hardcore` or `any`).
    pub hardcore: bool,
    /// The host's NAT test.
    pub nat: Option<&'a str>,
    /// OD-146's `transport: "relay"` (ignored for a Hardcore world).
    pub relay_only: bool,
    /// CR-S40: the host's route family cannot be told.
    pub family_mismatch: bool,
    /// The host's round trips.
    pub relays: Option<&'a RelayLatencies>,
}

/// A new world's relay (MATCHMAKING_API.md §5b, OD-160): `Some` = relay-born on it, `None` =
/// direct. A host that needs a relay (symmetric NAT, a route of unknown family, OD-146's toggle in
/// a Noob world) gets its lowest available trusted relay, else its lowest trusted one, else
/// `no_relay`. A Hardcore world otherwise opens on the host's lowest available trusted relay, and
/// direct only when there is none. A Noob world otherwise opens direct.
pub fn open_relay<'t>(
    report: &OpenReport<'_>,
    trusted: &'t [TrustedRelay],
    down: &[String],
) -> Result<Option<&'t TrustedRelay>, MmError> {
    let up: Vec<TrustedRelay> = trusted
        .iter()
        .filter(|t| !down.contains(&t.key))
        .cloned()
        .collect();
    // `choose_relay` over the available ones, mapped back to the caller's slice.
    let available = choose_relay(report.relays, report.relays, &up)
        .and_then(|c| trusted.iter().find(|t| t.key == c.key));
    let needs = report.nat == Some("symmetric")
        || report.family_mismatch
        || (!report.hardcore && report.relay_only);
    if needs {
        return available
            .or_else(|| choose_relay(report.relays, report.relays, trusted))
            .map(Some)
            .ok_or(MmError::Conflict("no_relay"));
    }
    Ok(if report.hardcore { available } else { None })
}

/// What a joiner's fallback renew found (CR-S57).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallBack {
    /// The seat now travels through its relay.
    Relay,
    /// The caller's live seat names no relay, or one the server no longer trusts (409
    /// `PORTAL.CONFLICT {reason: "no_relay"}`).
    NoRelay,
    /// No room for another relayed seat in the world or the account's hour (409
    /// `PORTAL.CONFLICT {reason: "relay_busy"}`).
    Busy,
    /// No live seat of the caller with that id (the renew answers the seat as it is).
    NotFound,
    /// The seat is in an official world, direct always (409 `PORTAL.CONFLICT {reason:
    /// "official_direct"}`, OD-189).
    OfficialDirect,
}

/// `POST /mm/v1/seat/renew {grantId, transport: "relay"}` (§3.2, §5b): the joiner's punch failed
/// and it moved to its grant's relay; recorded on its live seat (reserved or seated) of this
/// session, so the host's punches say `transport: "relay"`. Idempotent.
pub async fn fall_back(
    db: &PgPool,
    caller: &Caller,
    grant_id: Uuid,
    trusted: &[TrustedRelay],
    max_relayed: u32,
) -> Result<FallBack, IdentityError> {
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, caller.profile_id).await?;
    // World before grant, as join, renew and hosting.
    let row: Option<(Uuid, Option<String>, String, bool, bool)> = sqlx::query_as(
        "SELECT g.world_id, g.relay_key, g.transport, (w.mode = 'hardcore' OR w.any_mode), w.host_kind = 'official' \
         FROM mm_grant g JOIN world w ON w.id = g.world_id \
         WHERE g.id = $1 AND g.game = $2 AND g.profile_id = $3 AND g.session_id = $4 \
           AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())) \
         FOR UPDATE OF w",
    )
    .bind(grant_id)
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(caller.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((world, key, transport, hardcore, official)) = row else {
        return Ok(FallBack::NotFound);
    };
    if official {
        return Ok(FallBack::OfficialDirect);
    }
    // CR-S57 review L8: a relay delisted since the grant is signed as direct, so no fallback.
    if !key.is_some_and(|k| trusted.iter().any(|t| t.key == k)) {
        return Ok(FallBack::NoRelay);
    }
    if transport == "relay" {
        return Ok(FallBack::Relay);
    }
    // OD-160: no relay-as-exception caps in a Hardcore world.
    let room = if hardcore {
        Ok(())
    } else {
        relay_room_in(
            &mut tx,
            world,
            Some(grant_id),
            caller.account_id,
            max_relayed,
        )
        .await
    };
    match room {
        Ok(()) => {}
        Err(MmError::Db(e)) => return Err(e),
        Err(_) => {
            // The hour's count stays counted only when it was taken; nothing else changed.
            tx.commit().await?;
            return Ok(FallBack::Busy);
        }
    }
    sqlx::query("UPDATE mm_grant SET transport = 'relay' WHERE id = $1")
        .bind(grant_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(FallBack::Relay)
}

/// What a joiner's direct renew found (OD-160).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoDirect {
    /// The seat now goes direct.
    Direct,
    /// The seat already went direct (nothing changed).
    AlreadyDirect,
    /// Not a Hardcore world (409 `PORTAL.CONFLICT {reason: "no_direct"}`).
    Refused,
    /// No live seat of the caller with that id (the renew answers the seat as it is).
    NotFound,
}

/// `POST /mm/v1/seat/renew {grantId, transport: "direct"}` (§3.2, §5b "Relay by mode"): a
/// Hardcore world's joiner could not reach the world's relay and reached the host directly;
/// recorded on its live seat (reserved or seated) of this session, keeping the seat's relay, so
/// the host's punches say `transport: "direct"`. Idempotent.
pub async fn go_direct(
    db: &PgPool,
    caller: &Caller,
    grant_id: Uuid,
) -> Result<GoDirect, IdentityError> {
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, caller.profile_id).await?;
    // World before grant, as join, renew and hosting.
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT g.transport, (w.mode = 'hardcore' OR w.any_mode) OR w.host_kind = 'official' \
         FROM mm_grant g JOIN world w ON w.id = g.world_id \
         WHERE g.id = $1 AND g.game = $2 AND g.profile_id = $3 AND g.session_id = $4 \
           AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())) \
         FOR UPDATE OF w",
    )
    .bind(grant_id)
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(caller.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((transport, hardcore)) = row else {
        return Ok(GoDirect::NotFound);
    };
    if !hardcore {
        return Ok(GoDirect::Refused);
    }
    if transport == "direct" {
        tx.commit().await?;
        return Ok(GoDirect::AlreadyDirect);
    }
    sqlx::query("UPDATE mm_grant SET transport = 'direct' WHERE id = $1")
        .bind(grant_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(GoDirect::Direct)
}

// ---- Hosting ------------------------------------------------------------------------------------

/// A world's settings (`PUT /mm/v1/worlds/{id}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Name.
    pub name: String,
    /// Mode (the ticket's).
    pub mode: String,
    /// `modes` (OD-141, CR-S76): the host's mode or `any`; `None` (absent on the wire): the host's
    /// mode when the world opens, unchanged on an open world. Fixed once the world opens.
    pub open_modes: Option<String>,
    /// Seats (1-12).
    pub max_players: i32,
    /// `everyone` | `supporters` | `friends` | `private`.
    pub audience: String,
    /// Latency limit: 60, 100 or 150 ms.
    pub max_rtt_ms: Option<i32>,
    /// Races.
    pub races: bool,
    /// Message.
    pub message: Option<String>,
    /// Supporter hosts: the allow-list.
    pub allow: Option<Vec<i64>>,
    /// The host's mod set (OD-112; fixed once the world opens).
    pub mods: ModSet,
    /// The lobby engines the host's world server runs besides free roam (validated by the service).
    pub lobbies: Vec<String>,
}

/// Where the host's relay is: the caller's public address and location (from the request), and
/// the port and LAN addresses (from the body; `None`: keep).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The caller's public IP (text form).
    pub ip: String,
    /// Its cell.
    pub cell: Option<i32>,
    /// Its region.
    pub region: Option<String>,
    /// Selected public relay port (external mapping when supplied).
    pub port: Option<i32>,
    /// Local relay bind port; `None` with no endpoint preserves the stored pair.
    pub local_port: Option<i32>,
    /// Up to 4 private addresses of the host's PC (text form; MATCHMAKING_API.md §4a).
    pub lan: Option<Vec<String>>,
}

/// Secret credential for one grant's UDP socket proof; never a public world/roster/event field.
#[derive(Clone, PartialEq, Eq)]
pub struct UdpProofKey([u8; 32]);

impl UdpProofKey {
    /// Wraps an exact-length credential from trusted storage or a synthetic fixture.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Reads the credential for the authenticated HTTPS response only.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for UdpProofKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UdpProofKey([REDACTED])")
    }
}

fn udp_proof_key(r: &sqlx::postgres::PgRow) -> Result<Option<UdpProofKey>, sqlx::Error> {
    let bytes: Option<Vec<u8>> = r.try_get("udp_proof_key")?;
    // Legacy NULL and malformed storage never become a successful proof credential.
    Ok(bytes
        .and_then(|b| b.try_into().ok())
        .map(UdpProofKey::from_bytes))
}

/// One grant the host's relay must allow or drop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PunchRow {
    /// Grant.
    pub grant_id: Uuid,
    /// World.
    pub world_id: Uuid,
    /// Joiner's profile.
    pub profile_id: Uuid,
    /// Joiner's XUID.
    pub xuid: i64,
    /// Joiner's tier.
    pub tier: String,
    /// `issued`, `used` or `kicked`.
    pub state: String,
    /// When it was issued (a used grant is re-signed fresh by the service).
    pub issued_at: UnixMs,
    /// Its expiry.
    pub expires_at: UnixMs,
    /// This grant's private UDP proof credential; absent on legacy or kicked grants.
    pub udp_proof_key: Option<UdpProofKey>,
    /// Fresh peer destinations, only for the exact authenticated host; never verified authority.
    pub candidates: Option<NetworkHintsRow>,
    /// The relay this pair meets at (CR-S57; `None`: no fallback).
    pub relay_key: Option<String>,
    /// `direct` | `relay`: `relay` = the host holds a relay leg for this grant.
    pub transport: String,
    /// The joiner's account's voice block in force (CR-S67), signed into the grant.
    pub voice_blocked_until: Option<UnixMs>,
    /// The seat's declared set (`mm_grant.mods`; CR-L53 signs its sync ids).
    pub mods: Vec<ModEntry>,
}

/// An authorized routing hint. Debug never contains its address or port.
#[derive(Clone, PartialEq, Eq)]
pub struct NetworkCandidateRow {
    /// `public` or `lan`.
    pub kind: &'static str,
    /// Canonical unicast destination.
    pub ip: String,
    /// The port belonging to this destination.
    pub port: u16,
}

impl std::fmt::Debug for NetworkCandidateRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NetworkCandidateRow([REDACTED])")
    }
}

/// Fresh destinations for the authenticated counterpart only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkHintsRow {
    /// Monotonic revision within this grant.
    pub revision: u32,
    /// At most one public and four LAN destinations.
    pub candidates: Vec<NetworkCandidateRow>,
    /// Server response clock, used to conservatively calculate remaining TTL.
    pub generated_at: UnixMs,
    /// Original observation +30 s, clipped by world/grant/session authority.
    pub expires_at: UnixMs,
}

/// The host destinations returned to the exact live joiner's network observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkAnswer {
    /// Exact grant.
    pub grant_id: Uuid,
    /// Its world.
    pub world_id: Uuid,
    /// Accepted revision (equal retries keep their original freshness).
    pub revision: u32,
    /// Fresh host destinations, empty when an idempotent retry has already expired.
    pub candidates: Vec<NetworkCandidateRow>,
    /// Server response clock.
    pub generated_at: UnixMs,
    /// Original observation's clipped expiry, even when already expired.
    pub expires_at: UnixMs,
}

#[derive(PartialEq, Eq)]
struct CanonicalNetworkEndpoint {
    ip: String,
    public_port: i32,
    local_port: i32,
    lan: Vec<String>,
}

fn unicast_ip(text: &str) -> Option<IpAddr> {
    let ip = text.parse::<IpAddr>().ok()?.to_canonical();
    if ip.is_unspecified() || ip.is_multicast() || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast())
    {
        None
    } else {
        Some(ip)
    }
}

fn canonical_network_endpoint(endpoint: &Endpoint) -> Result<CanonicalNetworkEndpoint, MmError> {
    let bad = || MmError::Conflict("network_endpoint");
    let ip = unicast_ip(&endpoint.ip).ok_or_else(bad)?.to_string();
    let public_port = endpoint
        .port
        .filter(|p| (1..=65535).contains(p))
        .ok_or_else(bad)?;
    let local_port = endpoint
        .local_port
        .filter(|p| (1..=65535).contains(p))
        .ok_or_else(bad)?;
    let lan = endpoint.lan.as_deref().unwrap_or_default();
    if lan.len() > 4 {
        return Err(bad());
    }
    let mut canonical = Vec::new();
    for item in lan {
        let address = unicast_ip(item)
            .filter(crate::freeroam::is_lan)
            .ok_or_else(bad)?;
        let address = address.to_string();
        if !canonical.contains(&address) {
            canonical.push(address);
        }
    }
    Ok(CanonicalNetworkEndpoint {
        ip,
        public_port,
        local_port,
        lan: canonical,
    })
}

fn network_endpoint_row(
    r: &sqlx::postgres::PgRow,
) -> Result<Option<CanonicalNetworkEndpoint>, sqlx::Error> {
    let ip: Option<String> = r.try_get("network_ip")?;
    let Some(ip) = ip else { return Ok(None) };
    let endpoint = Endpoint {
        ip,
        cell: None,
        region: None,
        port: r.try_get("network_public_port")?,
        local_port: r.try_get("network_local_port")?,
        lan: Some(r.try_get("network_lan")?),
    };
    Ok(canonical_network_endpoint(&endpoint).ok())
}

fn network_candidates(
    endpoint: &CanonicalNetworkEndpoint,
    viewer_ip: &str,
) -> Vec<NetworkCandidateRow> {
    let mut candidates = Vec::new();
    let mut destinations = std::collections::BTreeSet::<SocketAddr>::new();
    let public = unicast_ip(&endpoint.ip);
    if let Some(ip) = public
        && let Ok(port @ 1..) = u16::try_from(endpoint.public_port)
    {
        destinations.insert(SocketAddr::new(ip, port));
        candidates.push(NetworkCandidateRow {
            kind: "public",
            ip: ip.to_string(),
            port,
        });
    }
    if public.is_some()
        && public == unicast_ip(viewer_ip)
        && let Ok(port @ 1..) = u16::try_from(endpoint.local_port)
    {
        for address in endpoint.lan.iter().take(4) {
            if let Some(ip) = unicast_ip(address).filter(crate::freeroam::is_lan)
                && destinations.insert(SocketAddr::new(ip, port))
            {
                candidates.push(NetworkCandidateRow {
                    kind: "lan",
                    ip: ip.to_string(),
                    port,
                });
            }
        }
    }
    candidates
}

fn network_expiry(r: &sqlx::postgres::PgRow, seen: UnixMs) -> Result<UnixMs, sqlx::Error> {
    let mut expires = seen
        .plus_millis(NETWORK_TTL_MS)
        .min(ms(r.try_get("world_expires_at")?))
        .min(ms(r.try_get("host_game_until")?))
        .min(ms(r.try_get("peer_game_until")?));
    if r.try_get::<String, _>("state")? == "issued" {
        expires = expires.min(ms(r.try_get("expires_at")?));
    }
    Ok(expires)
}

fn network_hints_row(r: &sqlx::postgres::PgRow) -> Result<Option<NetworkHintsRow>, sqlx::Error> {
    if !r.try_get::<bool, _>("peer_live")? || r.try_get::<String, _>("state")? == "kicked" {
        return Ok(None);
    }
    let revision: Option<i64> = r.try_get("network_revision")?;
    let seen: Option<OffsetDateTime> = r.try_get("network_seen_at")?;
    let (Some(revision), Some(seen), Some(endpoint)) = (revision, seen, network_endpoint_row(r)?)
    else {
        return Ok(None);
    };
    let Ok(revision @ 1..) = u32::try_from(revision) else {
        return Ok(None);
    };
    let generated_at = ms(r.try_get("generated_at")?);
    let expires_at = network_expiry(r, ms(seen))?;
    if expires_at <= generated_at {
        return Ok(None);
    }
    let host_ip: Option<String> = r.try_get("host_ip")?;
    Ok(Some(NetworkHintsRow {
        revision,
        candidates: network_candidates(&endpoint, host_ip.as_deref().unwrap_or_default()),
        generated_at,
        expires_at,
    }))
}

/// Publishes one observation of the exact grant's native proxy socket; no seat/world renewal.
/// HTTP validates the wire shape and counts [`NETWORK_LIMIT`] before calling this transaction.
pub async fn seat_network(
    db: &PgPool,
    caller: &Caller,
    grant_id: Uuid,
    revision: u32,
    endpoint: &Endpoint,
) -> Result<NetworkAnswer, MmError> {
    if revision == 0 {
        return Err(MmError::Conflict("network_endpoint"));
    }
    let endpoint = canonical_network_endpoint(endpoint)?;
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, caller.profile_id).await?;
    // Immutable grant identity finds its world first; never lock a grant before that world.
    let world_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT w.id FROM world w JOIN mm_grant g ON g.world_id = w.id \
         WHERE g.id = $1 AND g.game = $2 AND g.profile_id = $3 AND g.account_id = $4 AND g.session_id = $5 \
         FOR UPDATE OF w",
    )
    .bind(grant_id).bind(&caller.game).bind(caller.profile_id).bind(caller.account_id).bind(caller.session_id)
    .fetch_optional(&mut *tx).await?;
    let world_id = world_id.ok_or(MmError::WorldNotFound)?;
    let row = sqlx::query(
        "SELECT g.state, g.expires_at, g.network_revision, host(g.network_ip) AS network_ip, \
           g.network_public_port, g.network_local_port, g.network_seen_at, \
           ARRAY(SELECT host(a) FROM unnest(g.network_lan) WITH ORDINALITY AS t(a, n) ORDER BY n) AS network_lan, \
           host(w.endpoint_ip) AS host_ip, w.endpoint_port AS host_public_port, \
           coalesce(w.endpoint_local_port, w.endpoint_port) AS host_local_port, \
           ARRAY(SELECT host(a) FROM unnest(w.endpoint_lan) WITH ORDINALITY AS t(a, n) ORDER BY n) AS host_lan, \
           w.expires_at AS world_expires_at, coalesce(hs.game_until, w.expires_at) AS host_game_until, ps.game_until AS peer_game_until, \
           date_trunc('milliseconds', clock_timestamp()) AS generated_at \
         FROM mm_grant g JOIN world w ON w.id = g.world_id \
           LEFT JOIN play_session hs ON hs.id = w.host_session_id LEFT JOIN profile hp ON hp.id = w.host_profile_id \
           JOIN play_session ps ON ps.id = g.session_id JOIN profile pp ON pp.id = g.profile_id \
         WHERE g.id = $1 AND g.game = $2 AND g.profile_id = $3 AND g.account_id = $4 AND g.session_id = $5 \
           AND g.udp_proof_key IS NOT NULL \
           AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())) \
           AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() AND (w.mode = $6 OR w.any_mode) \
           AND (w.audience <> 'supporters' OR $7) \
           AND (pp.mode = w.mode OR w.any_mode) AND pp.game = w.game \
           AND (w.host_kind = 'official' OR (hp.mode = w.mode AND hp.game = w.game \
             AND hs.profile_id = w.host_profile_id AND hs.account_id = w.host_account_id \
             AND hs.state = 'active' AND hs.game_until > clock_timestamp())) \
           AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id \
           AND ps.state = 'active' AND ps.game_until > clock_timestamp() FOR UPDATE OF g",
    )
    .bind(grant_id).bind(&caller.game).bind(caller.profile_id).bind(caller.account_id).bind(caller.session_id)
    // OD-129: `supporters` worlds are everyone's (Supporter is a badge).
    .bind(&caller.mode).bind(true).fetch_optional(&mut *tx).await?
    .ok_or(MmError::WorldNotFound)?;
    let generated: OffsetDateTime = row.try_get("generated_at")?;
    let previous: Option<i64> = row.try_get("network_revision")?;
    let seen = match previous {
        Some(old) if i64::from(revision) < old => {
            return Err(MmError::Conflict("network_revision"));
        }
        Some(old) if i64::from(revision) == old => {
            if network_endpoint_row(&row)?.as_ref() != Some(&endpoint) {
                return Err(MmError::Conflict("network_revision"));
            }
            row.try_get("network_seen_at")?
        }
        _ => {
            sqlx::query(
                "UPDATE mm_grant SET network_ip = $2::text::inet, network_public_port = $3, \
                   network_local_port = $4, network_lan = $5::text[]::inet[], network_revision = $6, network_seen_at = $7 \
                 WHERE id = $1",
            )
            .bind(grant_id).bind(&endpoint.ip).bind(endpoint.public_port).bind(endpoint.local_port)
            .bind(&endpoint.lan).bind(i64::from(revision)).bind(generated).execute(&mut *tx).await?;
            generated
        }
    };
    let generated_at = ms(generated);
    let expires_at = network_expiry(&row, ms(seen))?;
    let host = CanonicalNetworkEndpoint {
        ip: row
            .try_get::<Option<String>, _>("host_ip")?
            .unwrap_or_default(),
        public_port: row.try_get("host_public_port")?,
        local_port: row.try_get("host_local_port")?,
        lan: row.try_get("host_lan")?,
    };
    let candidates = if expires_at > generated_at {
        network_candidates(&host, &endpoint.ip)
    } else {
        Vec::new()
    };
    tx.commit().await?;
    Ok(NetworkAnswer {
        grant_id,
        world_id,
        revision,
        candidates,
        generated_at,
        expires_at,
    })
}

/// What hosting did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hosted {
    /// The world as its host sees it.
    pub world: Assessed,
    /// The host's punches now.
    pub punches: Vec<PunchRow>,
    /// This call opened the world.
    pub opened: bool,
}

#[derive(Clone, Debug)]
struct GrantRow {
    id: Uuid,
    world_id: Uuid,
    xuid: i64,
    state: String,
    issued_at: UnixMs,
    expires_at: UnixMs,
    hold_until: UnixMs,
    session_id: Option<Uuid>,
    udp_proof_key: Option<UdpProofKey>,
    relay_key: Option<String>,
    transport: String,
    missing_since: Option<UnixMs>,
    /// The seat's declared set (`mm_grant.mods`, OD-112; CR-L53 signs its sync ids).
    mods: Vec<ModEntry>,
}

const GRANT_COLUMNS: &str = "id, world_id, profile_id, account_id, xuid, tier, state, issued_at, expires_at, hold_until, announced, session_id, udp_proof_key, relay_key, transport, missing_since, mods";

fn grant_row(r: &sqlx::postgres::PgRow) -> Result<GrantRow, sqlx::Error> {
    Ok(GrantRow {
        id: r.try_get("id")?,
        world_id: r.try_get("world_id")?,
        xuid: r.try_get("xuid")?,
        state: r.try_get("state")?,
        issued_at: ms(r.try_get("issued_at")?),
        expires_at: ms(r.try_get("expires_at")?),
        hold_until: ms(r.try_get("hold_until")?),
        session_id: r.try_get("session_id")?,
        udp_proof_key: udp_proof_key(r)?,
        relay_key: r.try_get("relay_key")?,
        transport: r.try_get("transport")?,
        missing_since: r
            .try_get::<Option<OffsetDateTime>, _>("missing_since")?
            .map(ms),
        mods: mods_from_json(r.try_get("mods")?),
    })
}

/// Moves `grant` to `state` (`used`, or a terminal one with its `end_reason`) and marks it
/// `announced`.
async fn move_grant(
    conn: &mut PgConnection,
    grant: &GrantRow,
    state: &str,
    end_reason: Option<&str>,
) -> Result<(), IdentityError> {
    let terminal = state != "used";
    let reason = if terminal {
        Some(end_reason.unwrap_or(state))
    } else {
        None
    };
    sqlx::query(
        "UPDATE mm_grant SET state = $2, announced = true, end_reason = $4, \
           used_at = CASE WHEN $2 = 'used' THEN clock_timestamp() ELSE used_at END, \
           ended_at = CASE WHEN $3 THEN clock_timestamp() ELSE NULL END \
         WHERE id = $1",
    )
    .bind(grant.id)
    .bind(state)
    .bind(terminal)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn live_grants_of_world(
    conn: &mut PgConnection,
    world: Uuid,
) -> Result<Vec<GrantRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE world_id = $1 AND state IN ('issued', 'used') ORDER BY issued_at FOR UPDATE"
    )))
    .bind(world)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(grant_row).collect::<Result<_, _>>()?)
}

async fn close_world_in(
    conn: &mut PgConnection,
    world: Uuid,
    reason: &str,
    now: UnixMs,
) -> Result<(), IdentityError> {
    let closed = sqlx::query(
        "UPDATE world SET closed_at = clock_timestamp(), close_reason = $2, code = NULL, geo_cell = NULL, \
           region = NULL, endpoint_ip = NULL, endpoint_lan = '{}', peers = '{}', allow = NULL, \
           official_owner_profile_id = NULL, official_owner_account_id = NULL, private_leader_xuid = NULL, \
           private_lone_since = NULL \
         WHERE id = $1 AND closed_at IS NULL",
    )
    .bind(world)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    if closed.rows_affected() == 0 {
        return Ok(());
    }
    sqlx::query("DELETE FROM world_access WHERE world_id = $1")
        .bind(world)
        .execute(&mut *conn)
        .await?;
    crate::freeroam::end_private_sessions_in(conn, world).await?;
    for g in live_grants_of_world(conn, world).await? {
        if g.state == "issued" && g.expires_at <= now {
            move_grant(conn, &g, "expired", None).await?;
        } else {
            move_grant(conn, &g, "released", Some("world_closed")).await?;
        }
    }
    end_offers_in(conn, OfferScope::World(world), "withdrawn").await?;
    Ok(())
}

/// The pineapple pool's moves on a cheat flag (OD-149, MATCHMAKING_API.md §3.9 "On the flag"), in
/// the flag's transaction (fi_db::observations), which already holds the profile's row lock
/// (`FOR NO KEY UPDATE`, before its world and grant rows, the order of hosting and joining): the
/// open clean worlds it hosts close (never a dedicated one: dedicated worlds take no part) (`pool`; their seats end `world_closed`, their offers are
/// withdrawn) and its live seats in clean worlds end (`released`, reason `pool`). Pineapple worlds
/// stay as they are. Idempotent: a repeat quorum finds nothing left to move.
pub async fn pool_in(conn: &mut PgConnection, profile: Uuid) -> Result<(), IdentityError> {
    let worlds: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM world WHERE host_profile_id = $1 AND closed_at IS NULL AND NOT pool AND NOT dedicated \
         ORDER BY id FOR NO KEY UPDATE",
    )
    .bind(profile)
    .fetch_all(&mut *conn)
    .await?;
    let now = now_in(conn).await?;
    for w in worlds {
        close_world_in(conn, w, "pool", now).await?;
    }
    let seats = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE profile_id = $1 AND state IN ('issued', 'used') \
           AND world_id IN (SELECT id FROM world WHERE NOT pool) ORDER BY id FOR UPDATE"
    )))
    .bind(profile)
    .fetch_all(&mut *conn)
    .await?;
    for r in &seats {
        let g = grant_row(r)?;
        if g.state == "issued" && g.expires_at <= now {
            move_grant(conn, &g, "expired", None).await?;
        } else {
            move_grant(conn, &g, "released", Some("pool")).await?;
        }
    }
    Ok(())
}

/// A code no open world of `game` uses.
async fn unused_code_in(conn: &mut PgConnection, game: &str) -> Result<String, IdentityError> {
    for _ in 0..16 {
        let code = random_code()?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM world WHERE game = $1 AND code = $2 AND closed_at IS NULL)",
        )
        .bind(game)
        .bind(&code)
        .fetch_one(&mut *conn)
        .await?;
        if !taken {
            return Ok(code);
        }
    }
    Err(IdentityError::Busy)
}

fn needs_code(audience: &str) -> bool {
    matches!(audience, "friends" | "private")
}

/// Opens, changes and keeps alive the caller's world `world_id`, in one transaction: with
/// `settings` (`PUT`) it opens the world (TTL 45 s) or changes its settings; the
/// location and TTL refresh every time; the complete address tuple changes only with an explicit
/// endpoint. With `peers` (the heartbeat), granted peers
/// it lists become `used` and seats of peers it no longer lists are released. Errors:
/// [`MmError::AlreadyHosting`] (another live world of the profile), [`MmError::NotHost`] (another
/// host's id, or `PUT` on a closed or lapsed world), [`MmError::WorldNotFound`] (a heartbeat of an
/// unknown, closed or lapsed world: a lapsed one is closed, never revived),
/// [`MmError::Conflict`] `modes` (another `modes` on an open world, OD-141),
/// [`MmError::SessionEnded`] (the ticket's play session is over), [`MmError::Conflict`] `pool` (a
/// world the host's cheat flag closed, while the flag lasts, OD-149). A heartbeat follows its
/// original host session; every host keeps its audience (OD-129). Another session must use a
/// fresh world UUID; reusing the previous session's world returns conflict `host_session`.
/// No idle rule: the deployable's routes call [`host_with_relay`] (settings, combined announce and
/// heartbeat, with the host's relay report and activity).
pub async fn host(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    settings: Option<&Settings>,
    endpoint: &Endpoint,
    peers: Option<&[i64]>,
) -> Result<Hosted, MmError> {
    host_with_idle(db, caller, world_id, settings, endpoint, peers, None).await
}

/// Whether the open world `world` has had nobody but its host for `idle_secs` (OD-107,
/// MATCHMAKING_API.md §3.3 item 4): opened at least that long ago, no live seat, no pending invite
/// or request, and no seat issued or ended and no offer sent or ended within that time. Read on
/// the caller's transaction, under the world's row lock (a join or an offer locks it too).
pub async fn world_idle_in(
    conn: &mut PgConnection,
    world: Uuid,
    idle_secs: i64,
) -> Result<bool, IdentityError> {
    let idle: Option<bool> = sqlx::query_scalar(
        "SELECT w.opened_at <= clock_timestamp() - make_interval(secs => $2) \
           AND NOT EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id \
             AND (g.state IN ('issued', 'used') OR g.issued_at > clock_timestamp() - make_interval(secs => $2) \
               OR g.ended_at > clock_timestamp() - make_interval(secs => $2))) \
           AND NOT EXISTS (SELECT 1 FROM world_invite i WHERE i.world_id = w.id \
             AND (i.state = 'pending' OR i.created_at > clock_timestamp() - make_interval(secs => $2) \
               OR i.ended_at > clock_timestamp() - make_interval(secs => $2))) \
         FROM world w WHERE w.id = $1 AND w.closed_at IS NULL",
    )
    .bind(world)
    .bind(idle_secs as f64)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(idle == Some(true))
}

/// Whether the open world `world` has nobody but its host (OD-180.2, MATCHMAKING_API.md §3.3
/// item 6): no live seat (`issued` or `used`; a lapsed reservation counts until it is ended) and
/// no pending invite or request. Read on the caller's transaction, under the world's row lock.
pub async fn world_empty_in(conn: &mut PgConnection, world: Uuid) -> Result<bool, IdentityError> {
    let empty: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = $1 AND g.state IN ('issued', 'used')) \
           AND NOT EXISTS (SELECT 1 FROM world_invite i WHERE i.world_id = $1 AND i.state = 'pending')",
    )
    .bind(world)
    .fetch_one(&mut *conn)
    .await?;
    Ok(empty)
}

/// Whether play session `session` hosts an open world whose host is away (`host_away`) and that
/// still holds another player's live seat (OD-180.2, S29): the gate's idle judgement (GATE_API.md
/// §4a) leaves such a session alone, since its game is closed on purpose while its launcher serves
/// the others.
pub async fn holds_away_world_in(
    conn: &mut PgConnection,
    session: Uuid,
) -> Result<bool, IdentityError> {
    let holds: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM world w WHERE w.host_session_id = $1 AND w.closed_at IS NULL \
           AND w.host_away AND EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id AND g.state IN ('issued', 'used')))",
    )
    .bind(session)
    .fetch_one(&mut *conn)
    .await?;
    Ok(holds)
}

/// [`host`] with the idle rule (OD-107): with `idle_secs`, a `PUT` or heartbeat of an open world
/// that [`world_idle_in`] finds idle, and whose heartbeat lists no peer, closes it (`idle`) and
/// answers as for a closed world ([`MmError::NotHost`] for a `PUT`, [`MmError::WorldNotFound`]
/// for a heartbeat), so the host's launcher stops hosting it. `None` (or a non-positive value)
/// turns the rule off.
pub async fn host_with_idle(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    settings: Option<&Settings>,
    endpoint: &Endpoint,
    peers: Option<&[i64]>,
    idle_secs: Option<i64>,
) -> Result<Hosted, MmError> {
    host_with_relay(
        db,
        caller,
        world_id,
        settings,
        endpoint,
        peers,
        idle_secs,
        &HostRelay::default(),
    )
    .await
}

/// [`host_with_idle`] with the host's relay report (CR-S57, MATCHMAKING_API.md §5b): a world
/// opened with `nat: "symmetric"` is born on the host's lowest trusted relay ([`choose_relay`];
/// none: [`MmError::Conflict`] `no_relay` and nothing opens); a `PUT` with `relays` stores the
/// trusted entries for the joiners' choice. The transport never changes after open.
#[allow(clippy::too_many_arguments)]
pub async fn host_with_relay(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    settings: Option<&Settings>,
    endpoint: &Endpoint,
    peers: Option<&[i64]>,
    idle_secs: Option<i64>,
    relay: &HostRelay<'_>,
) -> Result<Hosted, MmError> {
    if settings.is_some_and(|s| {
        s.mode != caller.mode
            || s.open_modes
                .as_deref()
                .is_some_and(|m| m != OPEN_MODES_ANY && m != caller.mode)
    }) {
        return Err(MmError::NotHost);
    }
    let mut tx = db.begin().await?;
    lock_holder_in(&mut tx, caller.account_id, caller.profile_id).await?;
    // The ticket's session must still be live, under a share lock taken before any world row: a
    // session's end (the session row, then its worlds) then never misses a world opened here, and
    // the two never wait on each other in a cycle.
    let live: Option<(bool, bool, bool)> = sqlx::query_as(
        "SELECT s.state = 'active' AND s.game_until > clock_timestamp() \
           AND s.profile_id = $2 AND s.account_id = $3 AND p.mode = $4 AND p.game = $5, s.dedicated, s.modded \
         FROM play_session s JOIN profile p ON p.id = s.profile_id WHERE s.id = $1 FOR SHARE OF s",
    )
    .bind(caller.session_id)
    .bind(caller.profile_id)
    .bind(caller.account_id)
    .bind(&caller.mode)
    .bind(&caller.game)
    .fetch_optional(&mut *tx)
    .await?;
    // OD-109: a dedicated session's world is dedicated (its host session never changes, so
    // neither does the flag): the host is not a player, it may list 12 peers, and it needs room
    // for at least 2 guests.
    let dedicated = match live {
        Some((true, dedicated, _)) => dedicated,
        _ => return Err(MmError::SessionEnded),
    };
    // OD-236: the session's modded flag never constrains the declared set (display only).
    if dedicated && settings.is_some_and(|s| s.max_players < 2) {
        return Err(MmError::Invalid {
            field: "maxPlayers",
            reason: "2 to 12 guests in a dedicated world",
        });
    }
    let peer_cap = if dedicated {
        MAX_PEERS_DEDICATED
    } else {
        MAX_PEERS_DEDICATED - 1
    };
    if peers.is_some_and(|p| p.len() > peer_cap) {
        return Err(MmError::Invalid {
            field: "peers",
            reason: "too many entries",
        });
    }
    let now = now_in(&mut tx).await?;
    // Another host's id first (a world's host never changes, so no lock is needed to say so).
    let owner: Option<(Option<Uuid>, String)> =
        sqlx::query_as("SELECT host_profile_id, game FROM world WHERE id = $1")
            .bind(world_id)
            .fetch_optional(&mut *tx)
            .await?;
    if owner.is_some_and(|(h, g)| h != Some(caller.profile_id) || g != caller.game) {
        return Err(MmError::NotHost);
    }
    let open: Option<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT id, expires_at FROM world WHERE game = $1 AND host_profile_id = $2 AND closed_at IS NULL FOR UPDATE",
    )
    .bind(&caller.game)
    .bind(caller.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    // Only opening (or changing) a world can conflict with another one; a heartbeat names its own.
    if let Some((id, expires)) = open
        && id != world_id
        && settings.is_some()
    {
        if ms(expires) > now {
            return Err(MmError::AlreadyHosting(id));
        }
        close_world_in(&mut tx, id, "replaced", now).await?;
    }
    type Existing = (
        Option<Uuid>,
        Option<OffsetDateTime>,
        String,
        Option<String>,
        String,
        bool,
        // NULL on a closed world whose host profile was deleted over 90 days ago (PD-Q1).
        Option<Uuid>,
        String,
        serde_json::Value,
    );
    let existing: Option<Existing> = sqlx::query_as(
        "SELECT host_profile_id, closed_at, game, code, audience, expires_at <= clock_timestamp(), host_session_id, mode, \
           mods FROM world WHERE id = $1 FOR UPDATE",
    )
    .bind(world_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((_, _, _, _, _, _, session, mode, _)) = &existing {
        if *session != Some(caller.session_id) {
            return Err(MmError::Conflict("host_session"));
        }
        if *mode != caller.mode {
            return Err(MmError::NotHost);
        }
    }
    // A world past its TTL that the sweep has not closed yet is closed here, never revived.
    if let Some((_, None, _, _, _, true, ..)) = existing {
        close_world_in(&mut tx, world_id, "expired", now).await?;
        tx.commit().await?;
        return Err(if settings.is_some() {
            MmError::NotHost
        } else {
            MmError::WorldNotFound
        });
    }
    // Nobody but the host for the idle time (OD-107): closed, and answered as a closed world.
    // A dedicated world is exempt (OD-109): it lives while its session heartbeats.
    if let (Some(secs), Some((_, None, ..))) = (idle_secs.filter(|s| *s > 0), &existing)
        && !dedicated
        && peers.is_none_or(<[i64]>::is_empty)
        && world_idle_in(&mut tx, world_id, secs).await?
    {
        close_world_in(&mut tx, world_id, "idle", now).await?;
        tx.commit().await?;
        return Err(if settings.is_some() {
            MmError::NotHost
        } else {
            MmError::WorldNotFound
        });
    }
    // OD-149 (§3.9): the host of a clean world the flag closed learns why: its heartbeat, `PUT` or
    // combined announce of that world answers 409 `pool` (not the closed-world 404/403) while the
    // flag lasts.
    if let Some((h, Some(_), game, ..)) = &existing
        && *h == Some(caller.profile_id)
        && *game == caller.game
    {
        let pooled: bool = sqlx::query_scalar(
            "SELECT w.close_reason IS NOT DISTINCT FROM 'pool' AND EXISTS (SELECT 1 FROM cheat_flag cf \
               JOIN profile fp ON fp.id = cf.profile_id \
               WHERE cf.profile_id = w.host_profile_id AND cf.until > clock_timestamp() AND fp.mode = 'hardcore') \
             FROM world w WHERE w.id = $1",
        )
        .bind(world_id)
        .fetch_one(&mut *tx)
        .await?;
        if pooled {
            return Err(MmError::Conflict("pool"));
        }
    }
    let kind = if caller.is_vip() { "supporter" } else { "free" };
    // The host's own seat in the reported count: none in a dedicated world.
    let host_seat: i32 = if dedicated { 0 } else { 1 };
    let opened = match (&existing, settings) {
        (Some((h, _, game, ..)), _) if *h != Some(caller.profile_id) || *game != caller.game => {
            return Err(MmError::NotHost);
        }
        (Some((_, Some(_), ..)), Some(_)) => return Err(MmError::NotHost),
        (Some((_, Some(_), ..)), None) | (None, None) => return Err(MmError::WorldNotFound),
        (Some((_, None, _, code, ..)), Some(s)) => {
            // OD-112: a world's mod set is fixed once it opens (a seat granted under the rule
            // stays valid); the same set is an ordinary idempotent PUT.
            if existing
                .as_ref()
                .is_some_and(|e| mods_from_json(e.8.clone()) != s.mods.entries)
            {
                return Err(MmError::Conflict("mods"));
            }
            // OD-141 (CR-S76): who may play is chosen at creation; absent keeps it.
            if let Some(m) = &s.open_modes {
                let current: String = sqlx::query_scalar(
                    "SELECT CASE WHEN any_mode THEN 'any' ELSE mode END FROM world WHERE id = $1",
                )
                .bind(world_id)
                .fetch_one(&mut *tx)
                .await?;
                if current != *m {
                    return Err(MmError::Conflict("modes"));
                }
            }
            let code = match (needs_code(&s.audience), code) {
                (true, Some(c)) => Some(c.clone()),
                (true, None) => Some(unused_code_in(&mut tx, &caller.game).await?),
                (false, _) => None,
            };
            let allow = s.allow.clone();
            sqlx::query(
                "UPDATE world SET name = $2, mode = $3, max_players = $4, audience = $5, max_rtt_ms = $6, races = $7, \
                   message = $8, allow = $9, host_tier = $10, kind = $11, code = $12 \
                 WHERE id = $1",
            )
            .bind(world_id)
            .bind(&s.name)
            .bind(&s.mode)
            .bind(s.max_players)
            .bind(&s.audience)
            .bind(s.max_rtt_ms)
            .bind(s.races)
            .bind(&s.message)
            .bind(&allow)
            .bind(&caller.tier)
            .bind(kind)
            .bind(&code)
            .execute(&mut *tx)
            .await?;
            false
        }
        (Some((_, None, _, code, audience, ..)), None) => {
            // OD-129: every host keeps its audience and allow-list, whatever its tier.
            let (audience, code) = (audience.clone(), code.clone());
            sqlx::query(
                "UPDATE world SET host_tier = $2, kind = $3, audience = $4, code = $5, \
                   allow = CASE WHEN $6 THEN allow ELSE NULL END WHERE id = $1",
            )
            .bind(world_id)
            .bind(&caller.tier)
            .bind(kind)
            .bind(&audience)
            .bind(&code)
            .bind(true)
            .execute(&mut *tx)
            .await?;
            false
        }
        (None, Some(s)) => {
            let port = endpoint.port.ok_or(MmError::Db(IdentityError::Event(
                "a new world needs its port".into(),
            )))?;
            let code = if needs_code(&s.audience) {
                Some(unused_code_in(&mut tx, &caller.game).await?)
            } else {
                None
            };
            let allow = s.allow.clone();
            // CR-S57: a symmetric host's lobby is born on its lowest trusted relay, fixed for the
            // world's life; with none reachable nothing opens.
            // OD-146: a host that always connects through the relay opens it the same way.
            // OD-160: a Hardcore world (`modes` hardcore or any) opens on the host's lowest
            // available trusted relay whatever the host asked, direct only with none.
            let hardcore = s.mode == "hardcore" || s.open_modes.as_deref() == Some(OPEN_MODES_ANY);
            let down = relays_down_in(&mut tx, relay.trusted).await?;
            let report = OpenReport {
                hardcore,
                nat: relay.nat,
                relay_only: relay.relay_only,
                family_mismatch: relay.family_mismatch,
                relays: relay.relays,
            };
            let world_relay = open_relay(&report, relay.trusted, &down)?;
            if world_relay.is_some() && !hardcore {
                // CR-S57 review M4: one live relay-born Noob world per account (OD-160: Hardcore
                // worlds neither are limited nor count).
                let busy: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM world WHERE host_account_id = $1 AND transport = 'relay' \
                       AND mode <> 'hardcore' AND NOT any_mode \
                       AND closed_at IS NULL AND expires_at > clock_timestamp() AND id <> $2)",
                )
                .bind(caller.account_id)
                .bind(world_id)
                .fetch_one(&mut *tx)
                .await?;
                if busy {
                    return Err(MmError::Conflict("relay_busy"));
                }
            }
            sqlx::query(
                "INSERT INTO world (id, game, host_profile_id, host_account_id, host_session_id, host_tier, kind, name, mode, \
                   players, peak_players, max_players, races, message, audience, max_rtt_ms, code, endpoint_ip, endpoint_port, \
                   allow, opened_at, expires_at, endpoint_local_port, dedicated, mods, \
                   nat, transport, relay_key, relay_name, relays, any_mode, lobbies, pool) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $21, $21, $10, $11, $12, $13, $14, $15, $16::text::inet, $17, $18, \
                   date_trunc('milliseconds', clock_timestamp()), clock_timestamp() + make_interval(secs => $19), $20, $22, $23, \
                   $24, $25, $26, $27, $28, $29, $30, \
                   NOT $22 AND $9 = 'hardcore' AND EXISTS (SELECT 1 FROM cheat_flag cf JOIN profile fp ON fp.id = cf.profile_id \
                     WHERE cf.profile_id = $3 AND cf.until > clock_timestamp() AND fp.mode = 'hardcore'))",
            )
            .bind(world_id)
            .bind(&caller.game)
            .bind(caller.profile_id)
            .bind(caller.account_id)
            .bind(caller.session_id)
            .bind(&caller.tier)
            .bind(kind)
            .bind(&s.name)
            .bind(&s.mode)
            .bind(s.max_players)
            .bind(s.races)
            .bind(&s.message)
            .bind(&s.audience)
            .bind(s.max_rtt_ms)
            .bind(&code)
            .bind(&endpoint.ip)
            .bind(port)
            .bind(&allow)
            .bind(WORLD_TTL_SECS as f64)
            .bind(endpoint.local_port.or(endpoint.port))
            .bind(host_seat)
            .bind(dedicated)
            .bind(serde_json::to_value(&s.mods.entries).map_err(event_err)?)
            .bind(relay.nat)
            .bind(if world_relay.is_some() {
                "relay"
            } else {
                "direct"
            })
            .bind(world_relay.map(|r| r.key.as_str()))
            .bind(world_relay.map(|r| r.name.as_str()))
            .bind(
                relay
                    .relays
                    .map(|r| trusted_relays_json(r, relay.trusted)),
            )
            .bind(s.open_modes.as_deref() == Some(OPEN_MODES_ANY))
            .bind(&s.lobbies)
            .execute(&mut *tx)
            .await?;
            // CR-S63: a public world announced on Discord (`lobby_opened`; one per host per
            // 10 minutes, never a dedicated, Supporter-only, friends or private one).
            if s.audience == "everyone" && !dedicated {
                crate::discord::lobby_opened_in(&mut tx, caller.profile_id).await?;
            }
            true
        }
    };
    // OD-150: the world's current activity; a Road Trip only in a world whose host runs it.
    if let Some(activity) = relay.activity {
        let changed = sqlx::query(
            "UPDATE world SET activity = $2 WHERE id = $1 AND ($2 <> 'roadtrip' OR 'roadtrip' = ANY(lobbies))",
        )
        .bind(world_id)
        .bind(activity)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // A PUT or combined announce is refused; a bare heartbeat keeps the stored activity and
        // still counts (its liveness, peers and seats are never lost to a refused change).
        if changed == 0 && settings.is_some() {
            return Err(MmError::Invalid {
                field: "activity",
                reason: "roadtrip needs roadtrip in the world's lobbies",
            });
        }
    }
    // CR-S67: the lobby's voice setting, the host's at any time (no condition: every value applies).
    if let Some(voice) = relay.voice {
        sqlx::query("UPDATE world SET voice = $2 WHERE id = $1 AND voice <> $2")
            .bind(world_id)
            .bind(voice)
            .execute(&mut *tx)
            .await?;
    }
    // CR-S57: a later PUT refreshes the host's relay round trips (never the transport).
    if let (false, Some(_), Some(relays)) = (opened, settings, relay.relays) {
        sqlx::query("UPDATE world SET relays = $2 WHERE id = $1")
            .bind(world_id)
            .bind(trusted_relays_json(relays, relay.trusted))
            .execute(&mut *tx)
            .await?;
    }
    // CR-S40: an endpoint-less heartbeat retains the entire routing tuple. A new
    // HTTPS observation must not replace its IP while keeping the old mapped port.
    // Location/TTL still refresh, and a heartbeat can update the players.
    let players = peers.map(|p| {
        i32::try_from(p.len())
            .unwrap_or(12)
            .saturating_add(host_seat)
            .min(12)
    });
    // CR-S40 / IPv6 hosts: a route whose family cannot be told keeps the stored one (the world
    // stays alive; the heartbeat still counts).
    let (port, lan, local_port) = if relay.family_mismatch {
        (None, None, None)
    } else {
        (endpoint.port, endpoint.lan.clone(), endpoint.local_port)
    };
    sqlx::query(
        "UPDATE world SET endpoint_ip = CASE WHEN $5::integer IS NULL THEN endpoint_ip ELSE $2::text::inet END, geo_cell = $3, region = $4, \
           endpoint_port = coalesce($5, endpoint_port), endpoint_lan = coalesce($6::text[]::inet[], endpoint_lan), \
           endpoint_local_port = CASE WHEN $5::integer IS NULL THEN endpoint_local_port ELSE coalesce($10, $5) END, \
           peers = coalesce($7, peers), players = coalesce($8, players), \
           peak_players = greatest(peak_players, coalesce($8, players)), \
           announced_at = clock_timestamp(), expires_at = clock_timestamp() + make_interval(secs => $9) \
         WHERE id = $1",
    )
    .bind(world_id)
    .bind(&endpoint.ip)
    .bind(endpoint.cell)
    .bind(&endpoint.region)
    .bind(port)
    .bind(&lan)
    .bind(peers)
    .bind(players)
    .bind(WORLD_TTL_SECS as f64)
    .bind(local_port)
    .execute(&mut *tx)
    .await?;
    if let Some(peers) = peers {
        move_seats_in(&mut tx, world_id, peers, now).await?;
    }
    // OD-180.2 (S29, CR-S85): the host's game is away while its launcher holds the world for the
    // other players. Every heartbeat states it (absent: false); a `PUT` keeps it.
    if let Some(away) = relay.host_away {
        sqlx::query("UPDATE world SET host_away = $2 WHERE id = $1 AND host_away <> $2")
            .bind(world_id)
            .bind(away)
            .execute(&mut *tx)
            .await?;
        // §3.3 item 6: an away host's world with no other player left closes at once (after this
        // heartbeat's seat moves), answered as a closed world so the launcher stops hosting it. A
        // reserved seat or a pending invite or request keeps it (a guest on its way in). Never a
        // world this call opened, nor a dedicated one (its host is not a player).
        if away
            && !opened
            && !dedicated
            && peers.is_none_or(<[i64]>::is_empty)
            && world_empty_in(&mut tx, world_id).await?
        {
            close_world_in(&mut tx, world_id, "host_away", now).await?;
            tx.commit().await?;
            return Err(if settings.is_some() {
                MmError::NotHost
            } else {
                MmError::WorldNotFound
            });
        }
    }
    let viewer = caller.viewer(Some(endpoint.ip.clone()));
    let row = world_in(&mut tx, &viewer, world_id)
        .await?
        .ok_or(MmError::WorldNotFound)?;
    let punches = punches_in(&mut tx, world_id, caller).await?;
    tx.commit().await?;
    Ok(Hosted {
        world: assess(&row, &caller.rules(endpoint.cell), false),
        punches,
        opened,
    })
}

// ---- Official worlds (OD-188/189, MATCHMAKING_API.md §2.9) --------------------------------------

/// A Worlds host of `FI_WORLDS` as the store needs it.
#[derive(Clone, Copy, Debug)]
pub struct OfficialHost<'a> {
    /// `vmId`.
    pub vm_id: &'a str,
    /// The operator's name (the worlds' name and display host).
    pub name: &'a str,
    /// The engine's public IP literal.
    pub ip: &'a str,
    /// Its public UDP port.
    pub port: i32,
    /// Open worlds at most.
    pub max_worlds: i64,
    /// The region its `FI_WORLDS` entry declares (OD-205).
    pub region: Option<&'a str>,
}

/// `PUT /mm/v1/official/worlds/{id}` (validated by the service).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfficialSettings {
    /// One of the served games.
    pub game: String,
    /// `hardcore` | `noob`, or `any` (CR-S101: a mixed world, stored as `any_mode` with the
    /// game's first mode, [`official_stored_mode`]).
    pub mode: String,
    /// `freeroam` | `carmeet` | `roadtrip` (OD-190) or `race` (CR-S99); fixed once open.
    pub activity: String,
    /// Guests, 2 to the game's seats (`GameRules::max_players`).
    pub max_players: i32,
    /// A pool world (CR-S103, WORLDS_OFFICIAL.md §14): flagged sessions only; fixed once open.
    pub pool: bool,
}

/// The SQL of an official world's punches (tests EXPLAIN it): [`PUNCHES_SQL`] without the host
/// session (an official world has none); `$3` is the world's `vmId`, `$4` unused.
pub static OFFICIAL_PUNCHES_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let joins = "FROM mm_grant g JOIN world w ON w.id = g.world_id JOIN play_session s ON s.id = w.host_session_id JOIN profile hp ON hp.id = w.host_profile_id LEFT JOIN";
    let host = "AND w.host_session_id = $3 AND w.mode = $4 AND hp.mode = w.mode AND hp.game = w.game AND s.profile_id = w.host_profile_id AND s.account_id = w.host_account_id AND s.state = 'active' AND s.game_until > clock_timestamp()";
    assert!(PUNCHES_SQL.contains(joins) && PUNCHES_SQL.contains(host));
    let sql = PUNCHES_SQL
        .replace(
            "s.game_until AS host_game_until",
            "w.expires_at AS host_game_until",
        )
        .replace(
            joins,
            "FROM mm_grant g JOIN world w ON w.id = g.world_id LEFT JOIN",
        )
        .replace(
            host,
            "AND w.host_kind = 'official' AND w.host_vm_id = $3 AND $4::text IS NOT NULL",
        )
        // OD-191: a car meet seat moving to a group world stays listed (as seated) until its
        // group seat is no longer a live reservation, or for at most 60 s.
        .replace(
            "g.tier, g.state, g.issued_at",
            "g.tier, CASE WHEN g.moving_to IS NOT NULL AND g.state = 'released' THEN 'used' ELSE g.state END AS state, g.issued_at",
        )
        .replace(
            "OR (g.state = 'kicked' AND g.ended_at > clock_timestamp() - make_interval(secs => $2))) ",
            "OR (g.state = 'kicked' AND g.ended_at > clock_timestamp() - make_interval(secs => $2)) \
             OR (g.state = 'released' AND g.moving_to IS NOT NULL AND g.moving_until > clock_timestamp() \
               AND EXISTS (SELECT 1 FROM mm_grant m WHERE m.id = g.moving_to AND m.state = 'issued' \
                 AND m.expires_at > clock_timestamp()))) ",
        );
    assert!(sql.contains("g.moving_to IS NOT NULL AND g.state = 'released' THEN 'used'"));
    assert!(sql.contains("m.id = g.moving_to"));
    sql
});

/// How long a car meet seat moving to a group world stays in the car meet's punches at most
/// (OD-191, WORLDS_OFFICIAL.md §11.4 step 3).
pub const MOVING_SEAT_SECS: i64 = 60;

/// The viewer of an agent's answers: nobody (no profile, no address).
fn nobody() -> Viewer {
    Viewer {
        account_id: Uuid::nil(),
        profile_id: Uuid::nil(),
        xuid: 0,
        ip: None,
    }
}

fn nobody_rules() -> Rules<'static> {
    Rules {
        profile_id: Uuid::nil(),
        vip: false,
        mode: None,
        cell: None,
    }
}

/// The world `world_id` as the agent of `vm` may touch it: `Ok(Some((game, closed_or_lapsed)))`
/// for one of its own, `Ok(None)` for an unknown id, [`MmError::NotHost`] for anyone else's.
async fn official_owner_in(
    conn: &mut PgConnection,
    vm: &str,
    world_id: Uuid,
) -> Result<Option<(String, String, bool)>, MmError> {
    let row: Option<(Option<String>, String, String, bool)> = sqlx::query_as(
        "SELECT host_vm_id, game, mode, closed_at IS NOT NULL OR expires_at <= clock_timestamp() \
         FROM world WHERE id = $1 FOR UPDATE",
    )
    .bind(world_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None => Ok(None),
        Some((Some(v), game, mode, gone)) if v == vm => Ok(Some((game, mode, gone))),
        Some(_) => Err(MmError::NotHost),
    }
}

async fn official_hosted_in(
    conn: &mut PgConnection,
    vm: &str,
    world_id: Uuid,
    opened: bool,
) -> Result<Hosted, MmError> {
    let row = world_in(conn, &nobody(), world_id)
        .await?
        .ok_or(MmError::WorldNotFound)?;
    let rows = sqlx::query(OFFICIAL_PUNCHES_SQL.as_str())
        .bind(world_id)
        .bind(KICKED_VISIBLE_SECS as f64)
        .bind(vm)
        .bind(&row.mode)
        .fetch_all(&mut *conn)
        .await?;
    let punches = rows
        .iter()
        .map(punch_row)
        .collect::<Result<_, sqlx::Error>>()?;
    Ok(Hosted {
        world: assess(&row, &nobody_rules(), false),
        punches,
        opened,
    })
}

/// The `mode` stored for an official world the agent opens as `mode` (CR-S101): its own for
/// `hardcore` | `noob`; for `any`, the game's first mode ([`fi_domain::GameRules::modes`]: FH2's
/// `hardcore`, FM5's `noob`). A mixed world's stored mode never decides who joins (`any_mode`
/// does); it is only the value the per-mode columns hold, as a player host's own mode.
pub fn official_stored_mode<'a>(game: &str, mode: &'a str) -> &'a str {
    if mode == OPEN_MODES_ANY {
        fi_domain::GameRules::of(game).modes[0].as_str()
    } else {
        mode
    }
}

/// An official world's `lobbies` (WORLDS_OFFICIAL.md §11.1, §11.5 item 1): the engine's public
/// free roam refuses car meet lobby types, so only a `carmeet` world lists `carmeet`.
fn official_lobbies(activity: &str) -> Vec<String> {
    match activity {
        "carmeet" => vec!["carmeet".into()],
        "roadtrip" => vec!["roadtrip".into()],
        _ => Vec::new(),
    }
}

/// `PUT /mm/v1/official/worlds/{id}`: opens one official world of `host` (at most
/// `host.max_worlds` open, the VM's safety ceiling, CR-S100; the `vmId` is locked while counting)
/// or changes its seats. Records the ceiling in `official_vm` for `fi-cli worlds-ceiling`'s alert.
/// [`MmError::Conflict`] `max_worlds` / `official_settings` (another game, mode, activity or
/// `pool`, CR-S103); [`MmError::NotHost`] for another's id or a closed one.
pub async fn official_put(
    db: &PgPool,
    host: &OfficialHost<'_>,
    world_id: Uuid,
    s: &OfficialSettings,
) -> Result<Hosted, MmError> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('fi-official:' || $1, 0))")
        .bind(host.vm_id)
        .execute(&mut *tx)
        .await?;
    // CR-S100: the ceiling the alert job compares this VM's open worlds against (written only
    // when it changed, so a steady agent costs one read here).
    sqlx::query(
        "INSERT INTO official_vm (vm_id, max_worlds) VALUES ($1, $2) \
         ON CONFLICT (vm_id) DO UPDATE SET max_worlds = EXCLUDED.max_worlds, updated_at = clock_timestamp() \
         WHERE official_vm.max_worlds <> EXCLUDED.max_worlds",
    )
    .bind(host.vm_id)
    .bind(i32::try_from(host.max_worlds).unwrap_or(i32::MAX))
    .execute(&mut *tx)
    .await?;
    // OD-190: a `carmeet` world admits both modes (OD-180); CR-S101: so does a mixed spare.
    let mixed = s.mode == OPEN_MODES_ANY;
    let any_mode = s.activity == "carmeet" || mixed;
    let stored_mode = official_stored_mode(&s.game, &s.mode);
    let lobbies = official_lobbies(&s.activity);
    let opened = match official_owner_in(&mut tx, host.vm_id, world_id).await? {
        Some((_, _, true)) => return Err(MmError::NotHost),
        Some((game, mode, false)) => {
            // OD-190: one activity per official world for its life.
            let (activity, stored_any, pool): (String, bool, bool) =
                sqlx::query_as("SELECT activity, any_mode, pool FROM world WHERE id = $1")
                    .bind(world_id)
                    .fetch_one(&mut *tx)
                    .await?;
            // CR-S101: `any` names a mixed world, a mode the stored one (a promoted spare is
            // mixed from then on, whatever its agent said). CR-S103: `pool` is fixed too.
            let same_mode = if mixed { stored_any } else { mode == s.mode };
            if game != s.game || !same_mode || activity != s.activity || pool != s.pool {
                return Err(MmError::Conflict("official_settings"));
            }
            sqlx::query("UPDATE world SET max_players = $2, name = $3 WHERE id = $1")
                .bind(world_id)
                .bind(s.max_players)
                .bind(host.name)
                .execute(&mut *tx)
                .await?;
            false
        }
        None => {
            let open: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM world WHERE host_kind = 'official' AND host_vm_id = $1 \
                   AND closed_at IS NULL AND expires_at > clock_timestamp()",
            )
            .bind(host.vm_id)
            .fetch_one(&mut *tx)
            .await?;
            if open >= host.max_worlds {
                return Err(MmError::Conflict("max_worlds"));
            }
            sqlx::query(
                "INSERT INTO world (id, game, host_kind, host_vm_id, host_profile_id, host_account_id, host_session_id, \
                   host_tier, kind, name, mode, players, peak_players, max_players, races, audience, endpoint_ip, \
                   endpoint_port, opened_at, expires_at, activity, lobbies, any_mode, voice, region, pool) \
                 VALUES ($1, $2, 'official', $3, NULL, NULL, NULL, 'free', 'free', $4, $5, 0, 0, $6, true, 'everyone', \
                   $7::text::inet, $8, date_trunc('milliseconds', clock_timestamp()), \
                   clock_timestamp() + make_interval(secs => $9), $10, $11, $12, \
                   CASE WHEN $2 = 'fm5' THEN 'open' ELSE 'proximity' END, $13, $14)",
            )
            .bind(world_id)
            .bind(&s.game)
            .bind(host.vm_id)
            .bind(host.name)
            .bind(stored_mode)
            .bind(s.max_players)
            .bind(host.ip)
            .bind(host.port)
            .bind(WORLD_TTL_SECS as f64)
            .bind(&s.activity)
            .bind(&lobbies)
            .bind(any_mode)
            .bind(host.region)
            .bind(s.pool)
            .execute(&mut *tx)
            .await?;
            true
        }
    };
    let hosted = official_hosted_in(&mut tx, host.vm_id, world_id, opened).await?;
    tx.commit().await?;
    Ok(hosted)
}

/// `POST /mm/v1/official/worlds/{id}/announce`: the heartbeat of an official world (TTL, seat
/// moves as §2.5's, voice; OD-190: the activity is fixed at `PUT`) and its punches. [`MmError::WorldNotFound`] for an
/// unknown, closed or lapsed world (a lapsed one is closed here), [`MmError::NotHost`] for anyone
/// else's.
pub async fn official_announce(
    db: &PgPool,
    host: &OfficialHost<'_>,
    world_id: Uuid,
    peers: &[i64],
    voice: Option<&str>,
) -> Result<Hosted, MmError> {
    official_announce_in_progress(db, host, world_id, peers, voice, false).await
}

/// [`official_announce`] with the world's `inProgress` (OD-194: an official Road Trip past its
/// vote): while set, [`OFFICIAL_PLACEMENT_SQL`] places nobody new in it. Every announce sets it.
pub async fn official_announce_in_progress(
    db: &PgPool,
    host: &OfficialHost<'_>,
    world_id: Uuid,
    peers: &[i64],
    voice: Option<&str>,
    in_progress: bool,
) -> Result<Hosted, MmError> {
    official_announce_with(db, host, world_id, peers, voice, in_progress, None).await
}

/// [`official_announce_in_progress`] with the leader the engine reports (WORLDS_OFFICIAL.md
/// §12.8b): stored on a private world when it is a member with a live seat, ignored otherwise.
#[allow(clippy::too_many_arguments)]
pub async fn official_announce_with(
    db: &PgPool,
    host: &OfficialHost<'_>,
    world_id: Uuid,
    peers: &[i64],
    voice: Option<&str>,
    in_progress: bool,
    leader: Option<i64>,
) -> Result<Hosted, MmError> {
    // OD-192/OD-222: an official lobby's voice is set when it opens (FH2 proximity, FM5 open) and
    // nobody changes its options; an announced voice setting is ignored.
    let _ = voice;
    if peers.len() > fi_domain::game::MAX_SEATS as usize {
        return Err(MmError::Invalid {
            field: "peers",
            reason: "too many entries",
        });
    }
    let mut tx = db.begin().await?;
    let now = now_in(&mut tx).await?;
    // CR-S99: a world takes its game's seats (FH2 12, FM5 16).
    let seats = match official_owner_in(&mut tx, host.vm_id, world_id).await? {
        None => return Err(MmError::WorldNotFound),
        Some((_, _, true)) => {
            close_world_in(&mut tx, world_id, "expired", now).await?;
            tx.commit().await?;
            return Err(MmError::WorldNotFound);
        }
        Some((game, _, false)) => fi_domain::GameRules::of(&game).max_players as usize,
    };
    if peers.len() > seats {
        return Err(MmError::Invalid {
            field: "peers",
            reason: "too many entries",
        });
    }
    let players = i32::try_from(peers.len()).unwrap_or(0);

    sqlx::query(
        "UPDATE world SET peers = $2, players = $3, peak_players = greatest(peak_players, $3), \
           announced_at = clock_timestamp(), in_progress = $5, \
           expires_at = clock_timestamp() + make_interval(secs => $4) WHERE id = $1",
    )
    .bind(world_id)
    .bind(peers)
    .bind(players)
    .bind(WORLD_TTL_SECS as f64)
    .bind(in_progress)
    .execute(&mut *tx)
    .await?;
    move_seats_in(&mut tx, world_id, peers, now).await?;
    // OD-191: a group world closes once no seat in it is live (nobody arrived within the hold, or
    // the last one left).
    let empty_group: bool = sqlx::query_scalar(
        "SELECT w.official_group AND NOT EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id \
           AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) \
         FROM world w WHERE w.id = $1",
    )
    .bind(world_id)
    .fetch_one(&mut *tx)
    .await?;
    if empty_group || private_lone_lapsed_in(&mut tx, world_id).await? {
        close_world_in(&mut tx, world_id, "expired", now).await?;
        tx.commit().await?;
        return Err(MmError::WorldNotFound);
    }
    crate::freeroam::extend_private_sessions_in(&mut tx, world_id).await?;
    // §12.8b: a leader who lost its seat stops leading, a report naming a seated member replaces it.
    sqlx::query(
        "UPDATE world w SET private_leader_xuid = CASE WHEN $2::bigint IS NOT NULL AND EXISTS (SELECT 1 FROM mm_grant g \
             WHERE g.world_id = w.id AND g.xuid = $2 \
               AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) THEN $2 \
           WHEN EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id AND g.xuid = w.private_leader_xuid \
               AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) THEN w.private_leader_xuid \
           END WHERE w.id = $1 AND w.official_owner_account_id IS NOT NULL",
    )
    .bind(world_id)
    .bind(leader)
    .execute(&mut *tx)
    .await?;
    let hosted = official_hosted_in(&mut tx, host.vm_id, world_id, false).await?;
    tx.commit().await?;
    Ok(hosted)
}

/// `DELETE /mm/v1/official/worlds/{id}`: closes an official world of `host` (`host_closed`;
/// idempotent for its own closed worlds).
pub async fn official_close(
    db: &PgPool,
    host: &OfficialHost<'_>,
    world_id: Uuid,
) -> Result<(), MmError> {
    let mut tx = db.begin().await?;
    if official_owner_in(&mut tx, host.vm_id, world_id)
        .await?
        .is_none()
    {
        return Err(MmError::WorldNotFound);
    }
    let now = now_in(&mut tx).await?;
    close_world_in(&mut tx, world_id, "host_closed", now).await?;
    tx.commit().await?;
    Ok(())
}

/// `POST /mm/v1/official/worlds/{id}/moderation {action: "kick"}`: ends the peer's live grant in
/// this open official world (`kicked`; no rejoin while it is open). [`MmError::WorldNotFound`] for
/// an unknown, closed or lapsed world, [`MmError::UnknownPlayer`] for an unknown XUID.
pub async fn official_kick(
    db: &PgPool,
    host: &OfficialHost<'_>,
    world_id: Uuid,
    target_xuid: i64,
) -> Result<(), MmError> {
    let mut tx = db.begin().await?;
    let game = match official_owner_in(&mut tx, host.vm_id, world_id).await? {
        Some((game, _, false)) => game,
        _ => return Err(MmError::WorldNotFound),
    };
    let target: Uuid = sqlx::query_scalar(
        "SELECT id FROM profile WHERE xuid = $1 AND game = $2 \
         ORDER BY (status = 'deleted'), created_at DESC LIMIT 1",
    )
    .bind(target_xuid)
    .bind(&game)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(MmError::UnknownPlayer)?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE profile_id = $1 AND world_id = $2 \
           AND state IN ('issued', 'used') FOR UPDATE"
    )))
    .bind(target)
    .bind(world_id)
    .fetch_all(&mut *tx)
    .await?;
    for r in &rows {
        move_grant(&mut tx, &grant_row(r)?, "kicked", None).await?;
    }
    tx.commit().await?;
    Ok(())
}

macro_rules! official_placement_sql {
    () => {
        "SELECT w.id FROM world w \
     WHERE w.game = $1 AND w.activity = $3 AND (w.mode = $2 OR w.any_mode) AND w.closed_at IS NULL \
       AND w.host_kind = 'official' AND w.expires_at > clock_timestamp() AND w.pool = $5 \
       AND w.id <> ALL($6::uuid[]) AND ($8::text IS NULL OR w.region IS NULL OR w.region = $8) \
       AND (NOT w.in_progress OR EXISTS (SELECT 1 FROM mm_grant o WHERE o.world_id = w.id AND o.profile_id = $4 \
             AND o.session_id = $7 AND (o.state = 'used' OR (o.state = 'issued' AND o.expires_at > clock_timestamp())))) \
       AND (NOT w.official_group OR EXISTS (SELECT 1 FROM mm_grant o WHERE o.world_id = w.id AND o.profile_id = $4 \
             AND o.session_id = $7 AND (o.state = 'used' OR (o.state = 'issued' AND o.expires_at > clock_timestamp())))) \
       AND NOT EXISTS (SELECT 1 FROM mm_grant k WHERE k.world_id = w.id AND k.profile_id = $4 AND k.state = 'kicked') \
       AND (EXISTS (SELECT 1 FROM mm_grant o WHERE o.world_id = w.id AND o.profile_id = $4 AND o.session_id = $7 \
             AND (o.state = 'used' OR (o.state = 'issued' AND o.expires_at > clock_timestamp()))) \
         OR (SELECT count(*) FROM mm_grant g WHERE g.world_id = w.id AND (g.state = 'used' \
             OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) \
         + (SELECT count(*) FROM world_invite i WHERE i.world_id = w.id AND i.state = 'pending' \
             AND i.expires_at > clock_timestamp()) < w.max_players) \
     ORDER BY EXISTS (SELECT 1 FROM mm_grant o WHERE o.world_id = w.id AND o.profile_id = $4 AND o.session_id = $7 \
             AND (o.state = 'used' OR (o.state = 'issued' AND o.expires_at > clock_timestamp()))) DESC, \
       w.opened_at, w.id"
    };
}
/// A player seated in an official world, as the agent's game-mode session route needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfficialSeated {
    /// Profile.
    pub profile_id: Uuid,
    /// Its account.
    pub account_id: Uuid,
    /// The seat's play session.
    pub session_id: Uuid,
    /// XUID.
    pub xuid: i64,
    /// Its name (`Name#12345`).
    pub gamertag: Option<String>,
    /// The seat's tier.
    pub tier: String,
    /// The profile's mode.
    pub mode: String,
}

/// `POST /mm/v1/official/worlds/{id}/game-sessions` (OD-190): the seated players of `xuids` in
/// `host`'s open `carmeet` world, in `xuids` order (each holds a live seat: `issued` and unexpired,
/// or `used`), and the world's game. [`MmError::WorldNotFound`] for an unknown, closed or lapsed
/// world, [`MmError::NotHost`] for anyone else's, [`MmError::Conflict`] `official_settings` for a
/// world of another activity, [`MmError::Invalid`] `xuids` for one not seated there.
pub async fn official_seated(
    db: &PgPool,
    host: &OfficialHost<'_>,
    world_id: Uuid,
    xuids: &[i64],
) -> Result<(String, Vec<OfficialSeated>), MmError> {
    let mut tx = db.begin().await?;
    let out = official_seated_in(&mut tx, host, world_id, xuids).await?;
    tx.commit().await?;
    Ok(out)
}

async fn official_seated_in(
    tx: &mut PgConnection,
    host: &OfficialHost<'_>,
    world_id: Uuid,
    xuids: &[i64],
) -> Result<(String, Vec<OfficialSeated>), MmError> {
    let game = match official_owner_in(&mut *tx, host.vm_id, world_id).await? {
        Some((game, _, false)) => game,
        _ => return Err(MmError::WorldNotFound),
    };
    let activity: String = sqlx::query_scalar("SELECT activity FROM world WHERE id = $1")
        .bind(world_id)
        .fetch_one(&mut *tx)
        .await?;
    if activity != "carmeet" {
        return Err(MmError::Conflict("official_settings"));
    }
    let rows: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT g.profile_id, g.account_id, g.session_id, g.xuid, g.tier, p.name, p.mode FROM mm_grant g \
           JOIN profile p ON p.id = g.profile_id \
         WHERE g.world_id = $1 AND g.xuid = ANY($2) AND g.session_id IS NOT NULL \
           AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))",
    )
    .bind(world_id)
    .bind(xuids)
    .fetch_all(&mut *tx)
    .await?;
    let mut seated = Vec::with_capacity(xuids.len());
    for x in xuids {
        let r = rows
            .iter()
            .find(|r| r.try_get::<i64, _>("xuid").ok() == Some(*x))
            .ok_or(MmError::Invalid {
                field: "xuids",
                reason: "players seated in this world",
            })?;
        seated.push(OfficialSeated {
            profile_id: r.try_get("profile_id")?,
            account_id: r.try_get("account_id")?,
            session_id: r.try_get("session_id")?,
            xuid: *x,
            gamertag: r.try_get("name")?,
            tier: r.try_get("tier")?,
            mode: r.try_get("mode")?,
        });
    }
    Ok((game, seated))
}

/// The live seat of `xuid` in world `world_id` (`used`, or `issued` and unexpired), as the car
/// meet's game-mode session needs its owner (MATCHMAKING_API.md §3.8, CR-S92: the invitation's
/// starter, `xuids[0]`); `None` when it holds none.
pub async fn seated_player(
    db: &PgPool,
    world_id: Uuid,
    xuid: i64,
) -> Result<Option<OfficialSeated>, IdentityError> {
    let row: Option<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT g.profile_id, g.account_id, g.session_id, g.tier, p.name, p.mode FROM mm_grant g \
           JOIN profile p ON p.id = g.profile_id \
         WHERE g.world_id = $1 AND g.xuid = $2 AND g.session_id IS NOT NULL \
           AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())) \
         ORDER BY g.issued_at DESC LIMIT 1",
    )
    .bind(world_id)
    .bind(xuid)
    .fetch_optional(db)
    .await?;
    let Some(r) = row else {
        return Ok(None);
    };
    Ok(Some(OfficialSeated {
        profile_id: r.try_get("profile_id")?,
        account_id: r.try_get("account_id")?,
        session_id: r.try_get("session_id")?,
        xuid,
        gamertag: r.try_get("name")?,
        tier: r.try_get("tier")?,
        mode: r.try_get("mode")?,
    }))
}

/// What [`official_group_open`] made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupOpened {
    /// The new group world.
    pub world_id: Uuid,
    /// Its activity (`freeroam` | `roadtrip`).
    pub activity: String,
    /// The game.
    pub game: String,
    /// The members in `xuids` order, each with its new reserved seat's session.
    pub members: Vec<OfficialSeated>,
}

/// `POST /mm/v1/official/worlds/{id}/game-sessions` (OD-191, WORLDS_OFFICIAL.md §11.4): opens a
/// group world of `host` running `activity` for the players `xuids` seated in its `carmeet` world
/// `carmeet`, reserves one `issued` direct seat each there (`hold_secs`) and ends their car meet
/// seats (`released`), in one transaction. The group world counts toward `max_worlds`
/// ([`MmError::Conflict`] `max_worlds`). Other refusals as [`official_seated`].
pub async fn official_group_open(
    db: &PgPool,
    host: &OfficialHost<'_>,
    carmeet: Uuid,
    xuids: &[i64],
    activity: &str,
    hold_secs: i64,
) -> Result<GroupOpened, MmError> {
    if xuids.is_empty() {
        return Err(MmError::Invalid {
            field: "xuids",
            reason: "players seated in this world",
        });
    }
    let mut tx = db.begin().await?;
    // The profiles first (a member's join holds its profile, then a world), in one order.
    let mut profiles: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT g.profile_id, g.account_id FROM mm_grant g WHERE g.world_id = $1 AND g.xuid = ANY($2) \
           AND g.state IN ('issued', 'used')",
    )
    .bind(carmeet)
    .bind(xuids)
    .fetch_all(&mut *tx)
    .await?;
    profiles.sort();
    profiles.dedup();
    for (profile, account) in &profiles {
        lock_profile_in(&mut tx, *profile).await?;
        lock_holder_in(&mut tx, *account, *profile).await?;
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('fi-official:' || $1, 0))")
        .bind(host.vm_id)
        .execute(&mut *tx)
        .await?;
    let (game, seated) = official_seated_in(&mut tx, host, carmeet, xuids).await?;
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM world WHERE host_kind = 'official' AND host_vm_id = $1 \
           AND closed_at IS NULL AND expires_at > clock_timestamp()",
    )
    .bind(host.vm_id)
    .fetch_one(&mut *tx)
    .await?;
    if open >= host.max_worlds {
        return Err(MmError::Conflict("max_worlds"));
    }
    let mode = seated[0].mode.clone();
    let any_mode = seated.iter().any(|m| m.mode != mode);
    let world_id = Uuid::now_v7();
    let max_players = i32::try_from(seated.len()).unwrap_or(12).clamp(2, 12);
    sqlx::query(
        "INSERT INTO world (id, game, host_kind, host_vm_id, host_profile_id, host_account_id, host_session_id, \
           host_tier, kind, name, mode, players, peak_players, max_players, races, audience, endpoint_ip, \
           endpoint_port, opened_at, expires_at, activity, lobbies, any_mode, official_group, voice) \
         VALUES ($1, $2, 'official', $3, NULL, NULL, NULL, 'free', 'free', $4, $5, 0, 0, $6, true, 'everyone', \
           $7::text::inet, $8, date_trunc('milliseconds', clock_timestamp()), \
           clock_timestamp() + make_interval(secs => $9), $10, $11, $12, true, \
           CASE WHEN $2 = 'fm5' THEN 'open' ELSE 'proximity' END)",
    )
    .bind(world_id)
    .bind(&game)
    .bind(host.vm_id)
    .bind(host.name)
    .bind(&mode)
    .bind(max_players)
    .bind(host.ip)
    .bind(host.port)
    .bind(WORLD_TTL_SECS as f64)
    .bind(activity)
    .bind(official_lobbies(activity))
    .bind(any_mode)
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE world_id = $1 AND xuid = ANY($2) \
           AND state IN ('issued', 'used') FOR UPDATE"
    )))
    .bind(carmeet)
    .bind(xuids)
    .fetch_all(&mut *tx)
    .await?;
    let moving: Vec<GrantRow> = rows.iter().map(grant_row).collect::<Result<_, _>>()?;
    for g in &moving {
        move_grant(&mut tx, g, "released", None).await?;
    }
    for m in &seated {
        let (id, ..) = insert_grant_in(
            &mut tx,
            &game,
            world_id,
            m.profile_id,
            m.account_id,
            m.xuid,
            &m.tier,
            m.session_id,
            hold_secs,
            &ModSet::vanilla(),
            &GrantRelay::none(),
        )
        .await?;
        // The car meet seat stays in the car meet's punches until this seat is reached.
        sqlx::query(
            "UPDATE mm_grant SET moving_to = $3, moving_until = clock_timestamp() + make_interval(secs => $4) \
             WHERE world_id = $1 AND profile_id = $2 AND state = 'released' AND id = ANY($5)",
        )
        .bind(carmeet)
        .bind(m.profile_id)
        .bind(id)
        .bind(MOVING_SEAT_SECS as f64)
        .bind(moving.iter().map(|g| g.id).collect::<Vec<Uuid>>())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(GroupOpened {
        world_id,
        activity: activity.to_owned(),
        game,
        members: seated,
    })
}

/// The SQL of the placement (tests EXPLAIN it): the oldest open official world of a game (`$1`)
/// and activity (`$3`) of the caller's mode (`$2`, or any mode for a `carmeet` world: OD-190,
/// `any_mode`), not `in_progress` (OD-194: a Road Trip past its vote) unless the caller's session
/// is already seated there, with a free seat that does not bar the profile `$4` (kicked), and a pool
/// world exactly when the caller is in the pineapple pool (`$5`; CR-S103, WORLDS_OFFICIAL.md §14). `$6`: world ids already tried; `$7`
/// the caller's session: a world it holds a live seat of this session in comes first, whatever its
/// seats (the quick join answers that seat again).
pub const OFFICIAL_PLACEMENT_SQL: &str = concat!(official_placement_sql!(), " LIMIT 1");

/// The quick join's pick: [`OFFICIAL_PLACEMENT_SQL`], locked (the world row before any grant row).
const OFFICIAL_PLACEMENT_LOCK_SQL: &str =
    concat!(official_placement_sql!(), " LIMIT 1 FOR UPDATE OF w");

/// How many official worlds one quick join tries before [`MmError::NoOfficialRoom`] (a race for the
/// last seat).
const QUICK_JOIN_TRIES: usize = 3;

async fn caller_flagged_in(conn: &mut PgConnection, caller: &Caller) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM cheat_flag cf WHERE cf.profile_id = $1 AND cf.until > clock_timestamp()) \
           AND $2 = 'hardcore'",
    )
    .bind(caller.profile_id)
    .bind(&caller.mode)
    .fetch_one(&mut *conn)
    .await
}

/// The official placement (MATCHMAKING_API.md §2.9): the world a quick join of `activity` would
/// take now (`None`: no open official world takes the caller). A read; [`official_quick_join`]
/// decides in its own transaction.
pub async fn official_placement(
    db: &PgPool,
    caller: &Caller,
    activity: &str,
) -> Result<Option<Uuid>, IdentityError> {
    let mut conn = db.acquire().await?;
    let flagged = caller_flagged_in(&mut conn, caller).await?;
    Ok(sqlx::query_scalar(OFFICIAL_PLACEMENT_SQL)
        .bind(&caller.game)
        .bind(&caller.mode)
        .bind(activity)
        .bind(caller.profile_id)
        .bind(flagged)
        .bind(Vec::<Uuid>::new())
        .bind(caller.session_id)
        .bind(None::<&str>)
        .fetch_optional(&mut *conn)
        .await?)
}

/// `POST /mm/v1/join {activity}` without `worldId` and `code`: the quick join (OD-190,
/// MATCHMAKING_API.md §2.9, WORLDS_OFFICIAL.md §11.2), one transaction. A live seat of this
/// session already in an open official world of `activity` is answered again (`Granted`); else
/// the caller's other live seat ends (`released`) and it is seated, direct, in the oldest open
/// official world of `activity` and its mode (any for `carmeet`) with a free seat, a pool world
/// exactly when the caller is flagged (CR-S103). None: [`MmError::NoOfficialRoom`] (rolled back:
/// the old seat stays; the service waits for the agent, CR-S100); a flagged caller with no open
/// pool world of its game and activity at all (FM5, or an agent without pool spares) `pool`;
/// `relay.relay_only` `official_direct`. `input.world_id` and `input.code` are ignored.
pub async fn official_quick_join(
    db: &PgPool,
    caller: &Caller,
    activity: &str,
    input: &JoinInput,
    relay: &JoinRelay<'_>,
) -> Result<JoinOutcome, MmError> {
    if relay.relay_only {
        return Err(MmError::Conflict("official_direct"));
    }
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, caller.profile_id).await?;
    lock_holder_in(&mut tx, caller.account_id, caller.profile_id).await?;
    // The grant's future FK row first, as [`join_with_relay`].
    let live_caller: Option<(bool, bool, bool)> = sqlx::query_as(
        "SELECT profile_id = $2 AND account_id = $3 AND state = 'active' AND game_until > clock_timestamp(), dedicated, modded \
         FROM play_session WHERE id = $1 FOR SHARE",
    )
    .bind(caller.session_id)
    .bind(caller.profile_id)
    .bind(caller.account_id)
    .fetch_optional(&mut *tx)
    .await?;
    match live_caller {
        Some((true, false, _)) => {}
        Some((true, true, _)) => return Err(MmError::DedicatedHostOnly),
        _ => return Err(MmError::SessionEnded),
    };
    let flagged = caller_flagged_in(&mut tx, caller).await?;
    if flagged && !pool_served_in(&mut tx, &caller.game, activity).await? {
        return Err(MmError::Conflict("pool"));
    }
    let now = now_in(&mut tx).await?;
    expire_own(
        &mut tx,
        &caller.game,
        caller.profile_id,
        &caller.mode,
        true,
        now,
    )
    .await?;
    let viewer = caller.viewer(input.ip.clone());
    let mut tried: Vec<Uuid> = Vec::new();
    let mut old_seat_handled = false;
    for _ in 0..QUICK_JOIN_TRIES {
        let picked: Option<Uuid> = sqlx::query_scalar(OFFICIAL_PLACEMENT_LOCK_SQL)
            .bind(&caller.game)
            .bind(&caller.mode)
            .bind(activity)
            .bind(caller.profile_id)
            .bind(flagged)
            .bind(&tried)
            .bind(caller.session_id)
            .bind(relay.region)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(world_id) = picked else {
            break;
        };
        if !old_seat_handled {
            old_seat_handled = true;
            if let Some(g) = live_grant_of(&mut tx, &caller.game, caller.profile_id).await? {
                if g.world_id == world_id && g.session_id == Some(caller.session_id) {
                    let voice_blocked_until =
                        crate::voice::blocked_until_in(&mut tx, caller.account_id).await?;
                    let host_vm_id: Option<String> =
                        sqlx::query_scalar("SELECT host_vm_id FROM world WHERE id = $1")
                            .bind(world_id)
                            .fetch_one(&mut *tx)
                            .await?;
                    tx.commit().await?;
                    return Ok(JoinOutcome::Granted(GrantIssued {
                        id: g.id,
                        world_id,
                        host_xuid: 0,
                        host_vm_id,
                        issued_at: g.issued_at,
                        expires_at: g.expires_at,
                        hold_until: g.hold_until,
                        udp_proof_key: g.udp_proof_key,
                        relay_key: g.relay_key,
                        transport: g.transport,
                        voice_blocked_until,
                        mods: g.mods,
                    }));
                }
                // OD-190: the quick join leaves whatever the caller was in.
                move_grant(&mut tx, &g, "released", None).await?;
            }
        }
        let w = world_in(&mut tx, &viewer, world_id)
            .await?
            .ok_or(MmError::WorldNotFound)?;
        let a = assess(&w, &caller.rules(input.cell), false);
        match a.why {
            None => {}
            Some(Why::Pool) => return Err(MmError::Conflict("pool")),
            // Full (a race for the last seat) or anything else: the next world.
            Some(_) => {
                tried.push(world_id);
                continue;
            }
        }
        let (id, issued, expires, hold, udp_proof_key) = insert_grant_in(
            &mut tx,
            &caller.game,
            world_id,
            caller.profile_id,
            caller.account_id,
            caller.xuid,
            &caller.tier,
            caller.session_id,
            input.hold_secs,
            &input.mods,
            &GrantRelay::none(),
        )
        .await?;
        let voice_blocked_until =
            crate::voice::blocked_until_in(&mut tx, caller.account_id).await?;
        tx.commit().await?;
        return Ok(JoinOutcome::Granted(GrantIssued {
            id,
            world_id,
            host_xuid: w.host_xuid,
            host_vm_id: w.host_vm_id.clone(),
            issued_at: issued,
            expires_at: expires,
            hold_until: hold,
            udp_proof_key: Some(udp_proof_key),
            relay_key: None,
            transport: "direct".to_owned(),
            voice_blocked_until,
            mods: input.mods.entries.clone(),
        }));
    }
    // Rolled back: the caller's old seat stays; the service waits for the agent (CR-S100).
    Err(MmError::NoOfficialRoom)
}

/// Whether a flagged session of `game` has anywhere to go for `activity` (CR-S103,
/// WORLDS_OFFICIAL.md §14): the game keeps pool worlds (FH2, not FM5) and an open official pool
/// world of that activity exists, full or not. False: 409 `pool {until}` at once, since waiting
/// for an agent that opens none would never end.
async fn pool_served_in(
    conn: &mut PgConnection,
    game: &str,
    activity: &str,
) -> Result<bool, sqlx::Error> {
    if !fi_domain::GameRules::of(game).pool_worlds {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM world WHERE game = $1 AND activity = $2 AND host_kind = 'official' \
           AND pool AND closed_at IS NULL AND expires_at > clock_timestamp())",
    )
    .bind(game)
    .bind(activity)
    .fetch_one(&mut *conn)
    .await
}

/// Whether a retry of the quick join (`private` false) or of the private create (`private` true)
/// of `activity` could find room now (CR-S100's bounded wait polls this between attempts): one
/// read on one pooled connection, no transaction, no lock. For the quick join, a world
/// [`OFFICIAL_PLACEMENT_SQL`] would place the caller in; for the private create, an idle spare of
/// the caller's pool side. A `true` is a hint: the retry decides in its own transaction.
pub async fn official_room(
    db: &PgPool,
    caller: &Caller,
    activity: &str,
    private: bool,
    region: Option<&str>,
) -> Result<bool, IdentityError> {
    let mut conn = db.acquire().await?;
    let flagged = caller_flagged_in(&mut conn, caller).await?;
    if private {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM world w WHERE {})",
            spare_candidate_sql()
        );
        return Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(&caller.game)
            .bind(&caller.mode)
            .bind(activity)
            .bind(Vec::<Uuid>::new())
            .bind(region)
            .bind(flagged)
            .fetch_one(&mut *conn)
            .await?);
    }
    Ok(sqlx::query_scalar::<_, Uuid>(OFFICIAL_PLACEMENT_SQL)
        .bind(&caller.game)
        .bind(&caller.mode)
        .bind(activity)
        .bind(caller.profile_id)
        .bind(flagged)
        .bind(Vec::<Uuid>::new())
        .bind(caller.session_id)
        .bind(region)
        .fetch_optional(&mut *conn)
        .await?
        .is_some())
}

// ---- The per-VM safety ceiling's alert (OD-209, OD-218, CR-S100; WORLDS_OFFICIAL.md §13) -------

/// A VM is alerted when its open official worlds pass this share of its `maxWorlds` (percent).
pub const CEILING_ALERT_PERCENT: i64 = 80;

/// One Worlds VM against its safety ceiling (`fi-cli worlds-ceiling`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfficialCeiling {
    /// `vmId`.
    pub vm_id: String,
    /// Its `FI_WORLDS` `maxWorlds`, as the last `PUT` saw it.
    pub max_worlds: i64,
    /// Its open official worlds now (spares, public, private and group worlds alike).
    pub open: i64,
    /// Whether the owner holds an unrecovered alert of it.
    pub alerted: bool,
}

impl OfficialCeiling {
    /// Past [`CEILING_ALERT_PERCENT`] of the ceiling (8 → 7 worlds; 64 → 52).
    pub fn over(&self) -> bool {
        self.open * 100 > self.max_worlds * CEILING_ALERT_PERCENT
    }
}

/// Every VM an agent has `PUT` a world of, with its open official worlds now (a read of the jobs
/// login, `fi_maintenance`).
pub async fn official_ceilings(db: &PgPool) -> Result<Vec<OfficialCeiling>, IdentityError> {
    let rows: Vec<(String, i32, i64, bool)> = sqlx::query_as(
        "SELECT v.vm_id, v.max_worlds, \
           (SELECT count(*) FROM world w WHERE w.host_kind = 'official' AND w.host_vm_id = v.vm_id \
              AND w.closed_at IS NULL AND w.expires_at > clock_timestamp()), \
           v.alerted_at IS NOT NULL \
         FROM official_vm v ORDER BY v.vm_id",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(vm_id, max, open, alerted)| OfficialCeiling {
            vm_id,
            max_worlds: i64::from(max),
            open,
            alerted,
        })
        .collect())
}

/// Records that the alert (`alerted` true) or the recovery (false) of `vm` went out, at `open`
/// worlds; only from the other state, so a repeat changes nothing (`false`: it was already so).
pub async fn official_ceiling_mark(
    db: &PgPool,
    vm: &str,
    alerted: bool,
    open: i64,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE official_vm SET alerted_at = CASE WHEN $2 THEN clock_timestamp() END, alerted_open = $3 \
         WHERE vm_id = $1 AND (alerted_at IS NOT NULL) <> $2",
    )
    .bind(vm)
    .bind(alerted)
    .bind(i32::try_from(open).unwrap_or(i32::MAX))
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

// ---- Private official worlds (OD-204, CR-S96; WORLDS_OFFICIAL.md §12) -----------------------------

/// A lone seat this long closes a private world (A11, §12.7).
pub const PRIVATE_LONE_SECS: i64 = 600;

/// The capability of a client that handles private official worlds.
pub use fi_protocol::gate::FEATURE_PRIVATE_OFFICIAL;

/// What an account holds of a private world.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrivateAccess {
    /// A `world_access` row: the owner, or a friend who accepted.
    pub seated: bool,
    /// A live invite.
    pub invited: bool,
}

impl PrivateAccess {
    /// Either.
    pub fn any(self) -> bool {
        self.seated || self.invited
    }
}

async fn private_access_in(
    conn: &mut PgConnection,
    world: Uuid,
    account: Uuid,
) -> Result<PrivateAccess, IdentityError> {
    let (seated, invited): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM world_access a WHERE a.world_id = $1 AND a.account_id = $2), \
           EXISTS (SELECT 1 FROM world_invite i WHERE i.world_id = $1 AND i.to_account_id = $2 \
             AND i.kind = 'invite' AND i.state = 'pending' AND i.expires_at > clock_timestamp())",
    )
    .bind(world)
    .bind(account)
    .fetch_one(&mut *conn)
    .await?;
    Ok(PrivateAccess { seated, invited })
}

/// Whether `account` may take a new seat in a private world, given what it holds there: its owner
/// always; someone who accepted while still a friend of the owner or of the current leader (B,
/// §12.3); an invitee who is a friend of the current leader, or of the owner when nobody leads
/// (§12.8b). A live seat of the same session never gets here: [`join_with_relay`] answers it first.
async fn private_may_enter_in(
    conn: &mut PgConnection,
    world: Uuid,
    account: Uuid,
    access: PrivateAccess,
) -> Result<bool, IdentityError> {
    if !access.any() {
        return Ok(false);
    }
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT official_owner_account_id FROM world WHERE id = $1")
            .bind(world)
            .fetch_one(&mut *conn)
            .await?;
    let Some(owner) = owner else {
        return Ok(false);
    };
    if owner == account {
        return Ok(true);
    }
    let leader = private_leader_in(conn, world).await?.map(|(_, a)| a);
    if leader == Some(account) {
        return Ok(true);
    }
    let friend_of_owner = mutual_friends_in(conn, owner, account).await?;
    let friend_of_leader = match leader {
        Some(leader) => mutual_friends_in(conn, leader, account).await?,
        None => false,
    };
    Ok(if access.seated {
        friend_of_owner || friend_of_leader
    } else if leader.is_some() {
        friend_of_leader
    } else {
        friend_of_owner
    })
}

/// The current leader of a private world, profile and account (WORLDS_OFFICIAL.md §12.8b): the
/// member the agent last reported, while seated; else its owner, while seated; else nobody. A
/// seatless leader counts as no leader. The engine picks and moves the leader, following the game.
async fn private_leader_in(
    conn: &mut PgConnection,
    world: Uuid,
) -> Result<Option<(Uuid, Uuid)>, IdentityError> {
    Ok(private_leader_row_in(conn, world)
        .await?
        .map(|(profile, account, _)| (profile, account)))
}

/// [`private_leader_in`] with the leader's XUID.
async fn private_leader_row_in(
    conn: &mut PgConnection,
    world: Uuid,
) -> Result<Option<(Uuid, Uuid, i64)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT p.id, p.account_id, p.xuid FROM world w JOIN profile p ON p.game = w.game \
           AND (p.xuid = w.private_leader_xuid OR p.id = w.official_owner_profile_id) \
         WHERE w.id = $1 AND w.official_owner_account_id IS NOT NULL \
           AND EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id AND g.profile_id = p.id \
             AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) \
         ORDER BY (p.xuid IS NOT DISTINCT FROM w.private_leader_xuid) DESC LIMIT 1",
    )
    .bind(world)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn add_world_access_in(
    conn: &mut PgConnection,
    world: Uuid,
    account: Uuid,
    via: &str,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO world_access (world_id, account_id, via) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(world)
    .bind(account)
    .bind(via)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn session_has_feature_in(
    conn: &mut PgConnection,
    session: Uuid,
    feature: &str,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM play_session WHERE id = $1 AND $2 = ANY (features))",
    )
    .bind(session)
    .bind(feature)
    .fetch_one(&mut *conn)
    .await?)
}

/// The announce's lone-seat rule (A11): records since when a private world has at most one live
/// seat, and whether that passed [`PRIVATE_LONE_SECS`]. Pending invites do not count (§12.2).
async fn private_lone_lapsed_in(
    conn: &mut PgConnection,
    world: Uuid,
) -> Result<bool, IdentityError> {
    let lapsed: Option<bool> = sqlx::query_scalar(
        "UPDATE world w SET private_lone_since = CASE WHEN (SELECT count(*) FROM mm_grant g WHERE g.world_id = w.id \
             AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) >= 2 THEN NULL \
             ELSE coalesce(w.private_lone_since, clock_timestamp()) END \
         WHERE w.id = $1 AND w.official_owner_account_id IS NOT NULL \
         RETURNING coalesce(w.private_lone_since < clock_timestamp() - make_interval(secs => $2), false)",
    )
    .bind(world)
    .bind(PRIVATE_LONE_SECS as f64)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(lapsed.unwrap_or(false))
}

/// The open private world this account may rejoin (§2.10), of the caller's game and mode.
async fn private_rejoin_in(
    conn: &mut PgConnection,
    caller: &Caller,
) -> Result<Option<Uuid>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT w.id FROM world_access a JOIN world w ON w.id = a.world_id \
         WHERE a.account_id = $1 AND w.game = $2 AND (w.mode = $3 OR w.any_mode) \
           AND (w.join_mode IS NULL OR w.join_mode = $3) AND w.closed_at IS NULL \
           AND w.expires_at > clock_timestamp() \
           AND NOT EXISTS (SELECT 1 FROM mm_grant k WHERE k.world_id = w.id AND k.profile_id = $4 AND k.state = 'kicked') \
         ORDER BY a.added_at DESC LIMIT 1",
    )
    .bind(caller.account_id)
    .bind(&caller.game)
    .bind(&caller.mode)
    .bind(caller.profile_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// When the caller's anti-cheat flag ends, for 409 `pool`'s `until` (A10; a clean caller: `None`).
pub async fn pool_until(db: &PgPool, caller: &Caller) -> Result<Option<UnixMs>, IdentityError> {
    if caller.mode != "hardcore" {
        return Ok(None);
    }
    let mut conn = db.acquire().await?;
    crate::observations::flag_of_in(&mut conn, caller.profile_id).await
}

/// `POST /mm/v1/join {activity, private: true}` (MATCHMAKING_API.md §2.10): the caller's own
/// private world of `activity`, or a spare of it promoted with the world row locked and re-checked;
/// the caller's other live seat ends in the same transaction and it gets a direct joiner seat. Any
/// idle spare of the caller's placement is promoted (CR-S100: no reserve rule; the agent refills),
/// a pool spare exactly when the caller is flagged (CR-S103). Never allocates for a client without
/// the capability ([`MmError::Conflict`] `client_update`); a flagged caller with no open pool world
/// of its game and activity at all `pool`; `relay_only` `official_direct`; no idle spare
/// [`MmError::NoOfficialRoom`], rolled back: the old seat stays (the service waits for the agent);
/// its own open world full `lobbies_full`. Another activity than its open private world's (OD-226):
/// that world is left (detached from the caller's profile, its seat there `released`) and a spare
/// of `activity` promoted as if it had none; with no such spare, rolled back as above.
pub async fn official_private_create(
    db: &PgPool,
    caller: &Caller,
    activity: &str,
    input: &JoinInput,
    relay: &JoinRelay<'_>,
) -> Result<JoinOutcome, MmError> {
    if relay.relay_only {
        return Err(MmError::Conflict("official_direct"));
    }
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, caller.profile_id).await?;
    lock_holder_in(&mut tx, caller.account_id, caller.profile_id).await?;
    let live_caller: Option<(bool, bool, bool)> = sqlx::query_as(
        "SELECT profile_id = $2 AND account_id = $3 AND state = 'active' AND game_until > clock_timestamp(), dedicated, modded \
         FROM play_session WHERE id = $1 FOR SHARE",
    )
    .bind(caller.session_id)
    .bind(caller.profile_id)
    .bind(caller.account_id)
    .fetch_optional(&mut *tx)
    .await?;
    match live_caller {
        Some((true, false, _)) => {}
        Some((true, true, _)) => return Err(MmError::DedicatedHostOnly),
        _ => return Err(MmError::SessionEnded),
    };
    if !session_has_feature_in(&mut tx, caller.session_id, FEATURE_PRIVATE_OFFICIAL).await? {
        return Err(MmError::Conflict("client_update"));
    }
    let flagged = caller_flagged_in(&mut tx, caller).await?;
    if flagged && !pool_served_in(&mut tx, &caller.game, activity).await? {
        return Err(MmError::Conflict("pool"));
    }
    let now = now_in(&mut tx).await?;
    expire_own(
        &mut tx,
        &caller.game,
        caller.profile_id,
        &caller.mode,
        true,
        now,
    )
    .await?;
    // One private world per owner: a lapsed one closes here, so the unique index holds.
    let own: Option<(Uuid, String, bool, bool)> = sqlx::query_as(
        "SELECT id, activity, expires_at > clock_timestamp(), pool FROM world \
         WHERE official_owner_profile_id = $1 AND closed_at IS NULL FOR UPDATE",
    )
    .bind(caller.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    let mut world = None;
    if let Some((id, own_activity, live, own_pool)) = own {
        if !live {
            close_world_in(&mut tx, id, "expired", now).await?;
        } else if own_pool != flagged {
            // CR-S103 (§14): the owner's flag changed since it opened (a flag raised mid-session
            // already ended its seat there, `pool_in`). The world stays private for the friends
            // in it, as after an owner's erasure (owner profile NULL, account kept), and frees the
            // one-private-world index so the owner gets a lobby of its own side.
            sqlx::query("UPDATE world SET official_owner_profile_id = NULL WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        } else if own_activity != activity {
            // OD-226 (§12.2 item 11): an activity picked inside a private lobby leaves it for that
            // activity's world. The caller's seat there ends below as any leave (the announce then
            // closes the world once no seat is live, OD-213); the world is detached from the
            // owner's profile as above, so the friends still in it keep it and the one-private-world
            // index takes the new one.
            sqlx::query("UPDATE world SET official_owner_profile_id = NULL WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        } else {
            world = Some(id);
        }
    }
    let promoted = world.is_none();
    if promoted {
        world = promote_spare_in(&mut tx, caller, activity, relay.region, flagged).await?;
    }
    let Some(world_id) = world else {
        // Rolled back: the caller's old seat stays; the service waits for the agent (CR-S100).
        return Err(MmError::NoOfficialRoom);
    };
    let vm: Option<String> = sqlx::query_scalar("SELECT host_vm_id FROM world WHERE id = $1")
        .bind(world_id)
        .fetch_one(&mut *tx)
        .await?;
    let old_seat = live_grant_of(&mut tx, &caller.game, caller.profile_id).await?;
    if let Some(g) = &old_seat
        && g.world_id == world_id
        && g.session_id == Some(caller.session_id)
    {
        let g = g.clone();
        let voice_blocked_until =
            crate::voice::blocked_until_in(&mut tx, caller.account_id).await?;
        tx.commit().await?;
        return Ok(JoinOutcome::Granted(GrantIssued {
            id: g.id,
            world_id,
            host_xuid: 0,
            host_vm_id: vm,
            issued_at: g.issued_at,
            expires_at: g.expires_at,
            hold_until: g.hold_until,
            udp_proof_key: g.udp_proof_key,
            relay_key: g.relay_key,
            transport: g.transport,
            voice_blocked_until,
            mods: g.mods,
        }));
    }
    // Its own open world, as any new seat: a kick, its mode and its seats still decide (rolled back).
    if !promoted {
        let w = world_in(&mut tx, &caller.viewer(input.ip.clone()), world_id)
            .await?
            .ok_or(MmError::WorldNotFound)?;
        match assess(&w, &caller.rules(input.cell), false).why {
            None => {}
            Some(Why::Mode) => return Ok(JoinOutcome::NotAllowed(Refusal::Mode)),
            Some(Why::Host) => return Ok(JoinOutcome::NotAllowed(Refusal::Host)),
            Some(Why::Pool) => return Err(MmError::Conflict("pool")),
            Some(_) => return Err(MmError::Conflict("lobbies_full")),
        }
    }
    if let Some(g) = old_seat {
        move_grant(&mut tx, &g, "released", None).await?;
    }
    if promoted {
        sqlx::query(
            "UPDATE world SET official_group = true, official_owner_profile_id = $2, \
               official_owner_account_id = $3, private_lone_since = clock_timestamp(), \
               any_mode = true, join_mode = NULL WHERE id = $1",
        )
        .bind(world_id)
        .bind(caller.profile_id)
        .bind(caller.account_id)
        .execute(&mut *tx)
        .await?;
    }
    add_world_access_in(&mut tx, world_id, caller.account_id, "owner").await?;
    let (id, issued, expires, hold, udp_proof_key) = insert_grant_in(
        &mut tx,
        &caller.game,
        world_id,
        caller.profile_id,
        caller.account_id,
        caller.xuid,
        &caller.tier,
        caller.session_id,
        input.hold_secs,
        &input.mods,
        &GrantRelay::none(),
    )
    .await?;
    let voice_blocked_until = crate::voice::blocked_until_in(&mut tx, caller.account_id).await?;
    tx.commit().await?;
    Ok(JoinOutcome::Granted(GrantIssued {
        id,
        world_id,
        host_xuid: 0,
        host_vm_id: vm,
        issued_at: issued,
        expires_at: expires,
        hold_until: hold,
        udp_proof_key: Some(udp_proof_key),
        relay_key: None,
        transport: "direct".to_owned(),
        voice_blocked_until,
        mods: input.mods.entries.clone(),
    }))
}

/// A live seat of the world `{w}` (an `mm_grant g` predicate).
const SPARE_LIVE_SEAT: &str = "g.world_id = {w}.id \
    AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))";

/// An idle official spare `w`: open, public, not in progress, no peer, no live seat, no pending invite.
fn spare_idle_sql() -> String {
    format!(
        "w.host_kind = 'official' AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
         AND NOT w.official_group AND NOT w.in_progress AND cardinality(w.peers) = 0 \
         AND NOT EXISTS (SELECT 1 FROM mm_grant g WHERE {seat}) \
         AND NOT EXISTS (SELECT 1 FROM world_invite i WHERE i.world_id = w.id AND i.state = 'pending' \
           AND i.expires_at > clock_timestamp())",
        seat = SPARE_LIVE_SEAT.replace("{w}", "w"),
    )
}

/// A spare `w` a private create may promote: `$1` game, `$2` the caller's mode (or a mixed spare),
/// `$3` activity, `$4` ids already tried, `$5` region, `$6` whether the caller is flagged (a pool
/// spare exactly then, CR-S103), and idle ([`spare_idle_sql`]).
fn spare_candidate_sql() -> String {
    format!(
        "w.game = $1 AND (w.mode = $2 OR w.any_mode) AND w.activity = $3 AND w.id <> ALL($4::uuid[]) \
         AND ($5::text IS NULL OR w.region IS NULL OR w.region = $5) AND w.pool = $6 AND {}",
        spare_idle_sql()
    )
}

/// The oldest idle spare of `activity` the caller's mode may join (CR-S101: a mixed one first, then
/// one of the caller's mode an older agent still opens), of the caller's pool side (CR-S103), its
/// row locked and re-checked in a new statement: a quick join may have seated someone while this
/// one waited for the lock, and the waiting statement's snapshot would not show that seat
/// (WORLDS_OFFICIAL.md §12.2 item 5). CR-S100 removed CR-S96's reserve rule: any idle spare is
/// promoted, and the agent refills (none: [`MmError::NoOfficialRoom`] upstream, after which the
/// service waits for the agent's next spare).
async fn promote_spare_in(
    conn: &mut PgConnection,
    caller: &Caller,
    activity: &str,
    region: Option<&str>,
    flagged: bool,
) -> Result<Option<Uuid>, IdentityError> {
    let candidate = spare_candidate_sql();
    let idle = spare_idle_sql();
    let mut tried: Vec<Uuid> = Vec::new();
    for _ in 0..QUICK_JOIN_TRIES {
        let peek: Option<(Uuid, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT w.id, w.host_vm_id FROM world w WHERE {candidate} ORDER BY w.any_mode DESC, w.opened_at, w.id LIMIT 1"
        )))
        .bind(&caller.game)
        .bind(&caller.mode)
        .bind(activity)
        .bind(&tried)
        .bind(region)
        .bind(flagged)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((id, vm)) = peek else {
            return Ok(None);
        };
        // The VM before the world row, as `official_put` and `official_group_open`.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('fi-official:' || $1, 0))")
            .bind(&vm)
            .execute(&mut *conn)
            .await?;
        let locked: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT w.id FROM world w WHERE w.id = $7 AND {candidate} FOR UPDATE OF w"
        )))
        .bind(&caller.game)
        .bind(&caller.mode)
        .bind(activity)
        .bind(&tried)
        .bind(region)
        .bind(flagged)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        if locked.is_none() {
            tried.push(id);
            continue;
        }
        // A new statement, so a new snapshot.
        let still_idle: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {idle} FROM world w WHERE w.id = $1"
        )))
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
        if still_idle {
            return Ok(Some(id));
        }
        tried.push(id);
    }
    Ok(None)
}

/// A heartbeat's seat moves (§2.5): listed `issued` seats become `used`, lapsed ones `expired`,
/// and a seated peer missing for [`SEAT_MISSING_GRACE_SECS`] in a row `released` (S33, CR-S86).
async fn move_seats_in(
    conn: &mut PgConnection,
    world_id: Uuid,
    peers: &[i64],
    now: UnixMs,
) -> Result<(), IdentityError> {
    for g in live_grants_of_world(conn, world_id).await? {
        let present = peers.contains(&g.xuid);
        match (g.state.as_str(), present) {
            ("issued", true) => move_grant(conn, &g, "used", None).await?,
            ("issued", false) if g.expires_at <= now => {
                move_grant(conn, &g, "expired", None).await?
            }
            // S33 (CR-S86): a seated peer is released only after SEAT_MISSING_GRACE_SECS
            // missing in a row; the first miss starts the clock (a retry keeps it), a
            // heartbeat listing it again clears it.
            ("used", false) => match g.missing_since {
                Some(since) if now.millis() - since.millis() >= SEAT_MISSING_GRACE_SECS * 1000 => {
                    move_grant(conn, &g, "released", None).await?
                }
                Some(_) => {}
                None => {
                    sqlx::query(
                        "UPDATE mm_grant SET missing_since = clock_timestamp() \
                             WHERE id = $1 AND missing_since IS NULL",
                    )
                    .bind(g.id)
                    .execute(&mut *conn)
                    .await?;
                }
            },
            ("used", true) if g.missing_since.is_some() => {
                sqlx::query("UPDATE mm_grant SET missing_since = NULL WHERE id = $1")
                    .bind(g.id)
                    .execute(&mut *conn)
                    .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// `POST /mm/v1/worlds/{id}/code`: a new code for the caller's open friends or private world
/// (the old one stops working at once).
pub async fn new_code(db: &PgPool, caller: &Caller, world_id: Uuid) -> Result<Assessed, MmError> {
    let mut tx = db.begin().await?;
    let found: Option<(Option<Uuid>, String, bool)> = sqlx::query_as(
        "SELECT host_profile_id, audience, closed_at IS NULL AND expires_at > clock_timestamp() \
         FROM world WHERE id = $1 AND game = $2 FOR UPDATE",
    )
    .bind(world_id)
    .bind(&caller.game)
    .fetch_optional(&mut *tx)
    .await?;
    match found {
        None | Some((_, _, false)) => return Err(MmError::WorldNotFound),
        Some((h, _, _)) if h != Some(caller.profile_id) => return Err(MmError::NotHost),
        Some((_, audience, _)) if !needs_code(&audience) => {
            return Err(MmError::Conflict("no_code"));
        }
        Some(_) => {}
    }
    let code = unused_code_in(&mut tx, &caller.game).await?;
    sqlx::query("UPDATE world SET code = $2 WHERE id = $1")
        .bind(world_id)
        .bind(&code)
        .execute(&mut *tx)
        .await?;
    let row = world_in(&mut tx, &caller.viewer(None), world_id)
        .await?
        .ok_or(MmError::WorldNotFound)?;
    tx.commit().await?;
    Ok(assess(&row, &caller.rules(None), false))
}

/// `PUT /mm/v1/worlds/{id}/modes {modes}` (OD-210, CR-S101, MATCHMAKING_API.md §2.11): the current
/// leader of an open private official world picks who may join it from now on, `any` (Everyone)
/// or one mode. Only `join_mode` changes: the world stays `any_mode`, so every seat already
/// granted or reserved keeps being read, renewed, punched and routed whatever its mode (§3.2's
/// per-mode defence reads `any_mode`), while every new join, invite acceptance and rejoin reads the
/// restriction through [`WorldRow::open_modes`] ([`assess`]). Idempotent. [`MmError::WorldNotFound`]
/// for an unknown, closed or lapsed world (or another game's); [`MmError::Conflict`] `not_leader`
/// for any other world or caller; [`MmError::Invalid`] `modes` for a mode the world's game has no
/// profiles of.
pub async fn set_private_modes(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    modes: &str,
) -> Result<Assessed, MmError> {
    let join_mode = match modes {
        OPEN_MODES_ANY => None,
        m => {
            let mode: fi_domain::ProfileMode = m.parse().map_err(|_| MmError::Invalid {
                field: "modes",
                reason: "any, hardcore or noob",
            })?;
            if !fi_domain::GameRules::of(&caller.game).allows(mode) {
                return Err(MmError::Invalid {
                    field: "modes",
                    reason: "any, or a mode of this game",
                });
            }
            Some(mode.as_str())
        }
    };
    let mut tx = db.begin().await?;
    let found: Option<(bool, bool)> = sqlx::query_as(
        "SELECT closed_at IS NULL AND expires_at > clock_timestamp(), official_owner_account_id IS NOT NULL \
         FROM world WHERE id = $1 AND game = $2 FOR UPDATE",
    )
    .bind(world_id)
    .bind(&caller.game)
    .fetch_optional(&mut *tx)
    .await?;
    match found {
        None | Some((false, _)) => return Err(MmError::WorldNotFound),
        Some((true, false)) => return Err(MmError::Conflict("not_leader")),
        Some((true, true)) => {}
    }
    let leader = private_leader_in(&mut tx, world_id).await?;
    if leader.map(|(profile, _)| profile) != Some(caller.profile_id) {
        return Err(MmError::Conflict("not_leader"));
    }
    sqlx::query("UPDATE world SET any_mode = true, join_mode = $2 WHERE id = $1")
        .bind(world_id)
        .bind(join_mode)
        .execute(&mut *tx)
        .await?;
    let row = world_in(&mut tx, &caller.viewer(None), world_id)
        .await?
        .ok_or(MmError::WorldNotFound)?;
    tx.commit().await?;
    Ok(assess(&row, &caller.rules(None), false))
}

/// The SQL of the punches of a world (tests EXPLAIN it): live grants and those kicked within
/// [`KICKED_VISIBLE_SECS`]. `$3` is the host session and `$4` its current mode; the world and
/// its hosting session are rechecked in the same statement that reads credentials.
pub const PUNCHES_SQL: &str = "SELECT g.id, g.world_id, g.profile_id, g.xuid, g.tier, g.state, g.issued_at, g.expires_at, \
     g.relay_key, g.transport, g.mods, \
     (SELECT CASE WHEN greatest(va.voice_banned_until, va.voice_community_until) > clock_timestamp() \
           THEN greatest(va.voice_banned_until, va.voice_community_until) END \
         FROM account va WHERE va.id = g.account_id) AS voice_blocked_until, \
     CASE WHEN g.state = 'kicked' THEN NULL ELSE g.udp_proof_key END AS udp_proof_key, \
     g.network_revision, host(g.network_ip) AS network_ip, g.network_public_port, g.network_local_port, g.network_seen_at, \
     ARRAY(SELECT host(a) FROM unnest(g.network_lan) WITH ORDINALITY AS t(a, n) ORDER BY n) AS network_lan, \
     host(w.endpoint_ip) AS host_ip, w.expires_at AS world_expires_at, s.game_until AS host_game_until, \
     ps.game_until AS peer_game_until, date_trunc('milliseconds', clock_timestamp()) AS generated_at, \
     coalesce(ps.state = 'active' AND ps.game_until > clock_timestamp() \
       AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id \
       AND (pp.mode = w.mode OR w.any_mode) AND pp.game = w.game, false) AS peer_live \
     FROM mm_grant g JOIN world w ON w.id = g.world_id JOIN play_session s ON s.id = w.host_session_id \
       JOIN profile hp ON hp.id = w.host_profile_id LEFT JOIN play_session ps ON ps.id = g.session_id \
       LEFT JOIN profile pp ON pp.id = g.profile_id \
     WHERE g.world_id = $1 AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()) \
       OR (g.state = 'kicked' AND g.ended_at > clock_timestamp() - make_interval(secs => $2))) \
       AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
       AND w.host_session_id = $3 AND w.mode = $4 AND hp.mode = w.mode AND hp.game = w.game \
       AND s.profile_id = w.host_profile_id AND s.account_id = w.host_account_id \
       AND s.state = 'active' AND s.game_until > clock_timestamp() \
     ORDER BY g.issued_at, g.id";

async fn punches_in(
    conn: &mut PgConnection,
    world: Uuid,
    caller: &Caller,
) -> Result<Vec<PunchRow>, IdentityError> {
    let rows = sqlx::query(PUNCHES_SQL)
        .bind(world)
        .bind(KICKED_VISIBLE_SECS as f64)
        .bind(caller.session_id)
        .bind(&caller.mode)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(punch_row)
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

fn punch_row(r: &sqlx::postgres::PgRow) -> Result<PunchRow, sqlx::Error> {
    Ok(PunchRow {
        grant_id: r.try_get("id")?,
        world_id: r.try_get("world_id")?,
        profile_id: r.try_get("profile_id")?,
        xuid: r.try_get("xuid")?,
        tier: r.try_get("tier")?,
        state: r.try_get("state")?,
        issued_at: ms(r.try_get("issued_at")?),
        expires_at: ms(r.try_get("expires_at")?),
        udp_proof_key: if r.try_get("peer_live")? {
            udp_proof_key(r)?
        } else {
            None
        },
        candidates: network_hints_row(r)?,
        relay_key: r.try_get("relay_key")?,
        transport: r.try_get("transport")?,
        voice_blocked_until: r
            .try_get::<Option<OffsetDateTime>, _>("voice_blocked_until")?
            .map(ms),
        mods: mods_from_json(r.try_get("mods")?),
    })
}

/// The punches of the caller's live world and exact hosting session (empty without one).
pub async fn punches(db: &PgPool, caller: &Caller) -> Result<Vec<PunchRow>, IdentityError> {
    let mut conn = db.acquire().await?;
    let world: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM world WHERE game = $1 AND host_profile_id = $2 AND closed_at IS NULL \
           AND expires_at > clock_timestamp() AND host_session_id = $3 AND mode = $4",
    )
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(caller.session_id)
    .bind(&caller.mode)
    .fetch_optional(&mut *conn)
    .await?;
    match world {
        Some(w) => punches_in(&mut conn, w, caller).await,
        None => Ok(Vec::new()),
    }
}

/// `DELETE /mm/v1/worlds/{id}`: the host closes its world, e.g. when its game left free roam
/// (idempotent: closing a closed world of the caller's is fine).
pub async fn close_world(db: &PgPool, caller: &Caller, world: Uuid) -> Result<(), MmError> {
    let mut tx = db.begin().await?;
    let host: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT host_profile_id FROM world WHERE id = $1 AND game = $2 FOR UPDATE",
    )
    .bind(world)
    .bind(&caller.game)
    .fetch_optional(&mut *tx)
    .await?;
    match host {
        None => return Err(MmError::WorldNotFound),
        Some(h) if h != Some(caller.profile_id) => return Err(MmError::NotHost),
        Some(_) => {}
    }
    let now = now_in(&mut tx).await?;
    close_world_in(&mut tx, world, "host_closed", now).await?;
    tx.commit().await?;
    Ok(())
}

/// What a play session's end leaves in worlds, on the transaction that ends the session (the gate's
/// leave, takeover and sweep through fi_db::admission; an account ban through fi_db::moderation):
/// the open worlds it hosts close (`session_ended`, MATCHMAKING_API.md §3.3) and its profile's live
/// seats end (`released`): a seat never outlives its holder's session, so it can never follow the
/// profile into another mode or tier.
pub async fn end_session_in(conn: &mut PgConnection, session: Uuid) -> Result<(), IdentityError> {
    close_worlds_of_session_in(conn, session).await?;
    let holder: Option<(String, Uuid)> = sqlx::query_as(
        "SELECT p.game, s.profile_id FROM play_session s JOIN profile p ON p.id = s.profile_id WHERE s.id = $1",
    )
    .bind(session)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((game, profile)) = holder {
        release_seats_in(conn, &game, profile).await?;
    }
    Ok(())
}

/// Ends a profile's live seats (`released`) and closes the open worlds it hosts
/// (`session_ended`): a mode change (fi_db::purge) runs it, so nothing of the old mode stays.
pub async fn release_profile_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
) -> Result<(), IdentityError> {
    let worlds: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM world WHERE game = $1 AND host_profile_id = $2 AND closed_at IS NULL FOR UPDATE",
    )
    .bind(game)
    .bind(profile)
    .fetch_all(&mut *conn)
    .await?;
    let now = now_in(conn).await?;
    for w in worlds {
        close_world_in(conn, w, "session_ended", now).await?;
    }
    release_seats_in(conn, game, profile).await
}

async fn release_seats_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
) -> Result<(), IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE game = $1 AND profile_id = $2 AND state IN ('issued', 'used') FOR UPDATE"
    )))
    .bind(game)
    .bind(profile)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    for r in &rows {
        move_grant(conn, &grant_row(r)?, "released", None).await?;
    }
    Ok(())
}

/// Closes the open worlds a play session hosts (`session_ended`, MATCHMAKING_API.md §3.3), in the
/// transaction that ends the session ([`end_session_in`]). How many it closed.
pub async fn close_worlds_of_session_in(
    conn: &mut PgConnection,
    session: Uuid,
) -> Result<u64, IdentityError> {
    let worlds: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM world WHERE host_session_id = $1 AND closed_at IS NULL ORDER BY id FOR UPDATE",
    )
    .bind(session)
    .fetch_all(&mut *conn)
    .await?;
    if worlds.is_empty() {
        return Ok(0);
    }
    let now = now_in(conn).await?;
    for w in &worlds {
        close_world_in(conn, *w, "session_ended", now).await?;
    }
    Ok(worlds.len() as u64)
}

/// A host's moderation action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Revoke the target's grant in this world (any host); it cannot rejoin while it is open.
    Kick,
    /// Supporter hosts: refuse the target in every world of this host, until `until`.
    Ban,
    /// Supporter hosts: lift a ban.
    Unban,
}

impl Action {
    /// Wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::Kick => "kick",
            Action::Ban => "ban",
            Action::Unban => "unban",
        }
    }
}

/// Applies a host's moderation action on `world` to the profile with `target_xuid`.
pub async fn moderate(
    db: &PgPool,
    caller: &Caller,
    world: Uuid,
    action: Action,
    target_xuid: i64,
    until: Option<UnixMs>,
    reason: Option<&str>,
) -> Result<(), MmError> {
    let mut tx = db.begin().await?;
    let host: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT host_profile_id FROM world WHERE id = $1 AND game = $2 FOR UPDATE",
    )
    .bind(world)
    .bind(&caller.game)
    .fetch_optional(&mut *tx)
    .await?;
    match host {
        None => return Err(MmError::WorldNotFound),
        Some(h) if h != Some(caller.profile_id) => return Err(MmError::NotHost),
        Some(_) => {}
    }
    let target: Uuid = sqlx::query_scalar(
        "SELECT id FROM profile WHERE xuid = $1 AND game = $2 \
         ORDER BY (status = 'deleted'), created_at DESC LIMIT 1",
    )
    .bind(target_xuid)
    .bind(&caller.game)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(MmError::UnknownPlayer)?;
    match action {
        Action::Unban => {
            sqlx::query(
                "DELETE FROM world_ban WHERE game = $1 AND host_profile_id = $2 AND target_profile_id = $3",
            )
            .bind(&caller.game)
            .bind(caller.profile_id)
            .bind(target)
            .execute(&mut *tx)
            .await?;
        }
        Action::Ban => {
            sqlx::query(
                "INSERT INTO world_ban (game, host_profile_id, target_profile_id, until, reason) VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (game, host_profile_id, target_profile_id) DO UPDATE SET until = EXCLUDED.until, \
                   reason = EXCLUDED.reason, created_at = clock_timestamp()",
            )
            .bind(&caller.game)
            .bind(caller.profile_id)
            .bind(target)
            .bind(until.map(crate::identity::ts))
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        }
        Action::Kick => {}
    }
    if action != Action::Unban {
        // Kick: the grant in this world. Ban: any live grant in the host's open worlds.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE profile_id = $1 AND game = $2 AND state IN ('issued', 'used') \
               AND world_id IN (SELECT id FROM world WHERE host_profile_id = $3 AND game = $2 AND (id = $4 OR $5)) FOR UPDATE"
        )))
        .bind(target)
        .bind(&caller.game)
        .bind(caller.profile_id)
        .bind(world)
        .bind(action == Action::Ban)
        .fetch_all(&mut *tx)
        .await?;
        for r in &rows {
            move_grant(&mut tx, &grant_row(r)?, "kicked", None).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

// ---- Joining and seats --------------------------------------------------------------------------

/// A seat reserved for the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantIssued {
    /// Grant id.
    pub id: Uuid,
    /// World.
    pub world_id: Uuid,
    /// Host's XUID (for the relay grant; 0 for an official world).
    pub host_xuid: i64,
    /// An official world's `vmId` (the grant's `host: {official}`, OD-188/189).
    pub host_vm_id: Option<String>,
    /// Issued at.
    pub issued_at: UnixMs,
    /// Expires at (60 s, renewable).
    pub expires_at: UnixMs,
    /// The latest a renew reaches.
    pub hold_until: UnixMs,
    /// This grant's private UDP proof credential; absent on legacy or kicked grants.
    pub udp_proof_key: Option<UdpProofKey>,
    /// The relay this pair meets at (CR-S57; `None`: no fallback).
    pub relay_key: Option<String>,
    /// `direct` | `relay`.
    pub transport: String,
    /// The joiner's account's voice block in force (CR-S67), signed into the grant.
    pub voice_blocked_until: Option<UnixMs>,
    /// The seat's declared set (`mm_grant.mods`; CR-L53 signs its sync ids).
    pub mods: Vec<ModEntry>,
}

/// `not_allowed` reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// One's own world.
    OwnWorld,
    /// Another mode.
    Mode,
    /// A `supporters` world, free ticket.
    Tier,
    /// The host's ban, allow-list or kick.
    Host,
    /// A `friends` world, not a friend, no invite, no code.
    Friends,
}

impl Refusal {
    /// The wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Refusal::OwnWorld => "own_world",
            Refusal::Mode => "mode",
            Refusal::Tier => "tier",
            Refusal::Host => "host",
            Refusal::Friends => "friends",
        }
    }
}

/// The join decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    /// A seat.
    Granted(GrantIssued),
    /// No seat.
    Full {
        /// Players, reserved and held seats.
        players: u32,
        /// Seats.
        max_players: u32,
    },
    /// A live seat elsewhere.
    Locked {
        /// Its world.
        world_id: Uuid,
        /// The grant.
        grant_id: Uuid,
        /// Its expiry (a used seat: now).
        expires_at: UnixMs,
    },
    /// `not_allowed` with its reason.
    NotAllowed(Refusal),
    /// `not_allowed {reason: "latency"}`: the estimate and the host's limit.
    Latency {
        /// Estimated round trip (ms).
        rtt_ms: u32,
        /// The host's limit (ms).
        max_rtt_ms: u32,
    },
    /// The world needs a code and none or a wrong one was given (wrong ones count).
    Code,
    /// The world closed or lapsed.
    Closed,
    /// The code limits are spent.
    RateLimited(i64),
}

/// What a join asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinInput {
    /// The world (or `None`: by `code`).
    pub world_id: Option<Uuid>,
    /// The code (8 digits).
    pub code: Option<String>,
    /// The caller's address (text) and cell, for the latency limit and same-address.
    pub ip: Option<String>,
    /// Its cell.
    pub cell: Option<i32>,
    /// Seconds a seat may be renewed for (`FI_MM_SEAT_HOLD_SECS`).
    pub hold_secs: i64,
    /// The caller's mod set (OD-112; [`ModSet::vanilla`] when the client sent none).
    pub mods: ModSet,
    /// The activity the caller asked for (OD-150; `None`: any): a world running another one is
    /// [`MmError::Conflict`] `activity`.
    pub activity: Option<String>,
}

/// Ends the seats of `profile` that no longer hold one: issued ones past their time (`expired`),
/// issued or used ones whose world closed, outlived its TTL or lost its host (`released`,
/// `world_closed`), and live ones the
/// holder may no longer keep: another mode's world, or a Supporter-only world for a free ticket
/// (`released`; defence in depth, a session's end already released them).
async fn expire_own(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
    mode: &str,
    vip: bool,
    now: UnixMs,
) -> Result<(), IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT g.{}, ((w.mode <> $3 AND NOT w.any_mode) OR (w.audience = 'supporters' AND NOT $4)) AS barred, \
           NOT EXISTS (SELECT 1 FROM play_session ps JOIN profile pp ON pp.id = g.profile_id \
             WHERE ps.id = g.session_id AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id \
               AND ps.state = 'active' AND ps.game_until > clock_timestamp() AND pp.mode = $3 AND pp.game = g.game) AS holder_invalid \
         FROM mm_grant g JOIN world w ON w.id = g.world_id \
         WHERE g.game = $1 AND g.profile_id = $2 \
           AND ((g.state = 'issued' AND g.expires_at <= clock_timestamp()) \
             OR (g.state IN ('issued', 'used') AND (w.closed_at IS NOT NULL OR w.expires_at <= clock_timestamp() \
               OR (w.host_kind = 'player' AND NOT EXISTS (SELECT 1 FROM play_session s JOIN profile hp ON hp.id = w.host_profile_id \
                 WHERE s.id = w.host_session_id AND s.profile_id = w.host_profile_id \
                   AND s.account_id = w.host_account_id AND s.state = 'active' AND s.game_until > clock_timestamp() \
                   AND hp.mode = w.mode AND hp.game = w.game)))) \
             OR (g.state IN ('issued', 'used') AND ((w.mode <> $3 AND NOT w.any_mode) OR (w.audience = 'supporters' AND NOT $4) \
               OR NOT EXISTS (SELECT 1 FROM play_session ps JOIN profile pp ON pp.id = g.profile_id \
                 WHERE ps.id = g.session_id AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id \
                   AND ps.state = 'active' AND ps.game_until > clock_timestamp() AND pp.mode = $3 AND pp.game = g.game)))) \
         FOR UPDATE OF g",
        GRANT_COLUMNS.replace(", ", ", g.")
    )))
    .bind(game)
    .bind(profile)
    .bind(mode)
    .bind(vip)
    .fetch_all(&mut *conn)
    .await?;
    for r in &rows {
        let g = grant_row(r)?;
        let barred: bool = r.try_get("barred")?;
        if barred || r.try_get::<bool, _>("holder_invalid")? {
            move_grant(conn, &g, "released", None).await?;
        } else if g.state == "issued" && g.expires_at <= now {
            move_grant(conn, &g, "expired", None).await?;
        } else {
            move_grant(conn, &g, "released", Some("world_closed")).await?;
        }
    }
    Ok(())
}

async fn live_grant_of(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
) -> Result<Option<GrantRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE game = $1 AND profile_id = $2 AND state IN ('issued', 'used') FOR UPDATE"
    )))
    .bind(game)
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(grant_row).transpose()?)
}

/// Inserts a seat for `profile` in `world`.
#[allow(clippy::too_many_arguments)]
async fn insert_grant_in(
    conn: &mut PgConnection,
    game: &str,
    world: Uuid,
    profile: Uuid,
    account: Uuid,
    xuid: i64,
    tier: &str,
    session_id: Uuid,
    hold_secs: i64,
    mods: &ModSet,
    relay: &GrantRelay,
) -> Result<(Uuid, UnixMs, UnixMs, UnixMs, UdpProofKey), IdentityError> {
    let id = Uuid::now_v7();
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|_| IdentityError::Event("no randomness".into()))?;
    let udp_proof_key = UdpProofKey::from_bytes(key);
    let hold = hold_secs.clamp(GRANT_TTL_SECS, 900);
    let (issued, expires, until): (OffsetDateTime, OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "INSERT INTO mm_grant (id, game, world_id, profile_id, account_id, xuid, tier, state, issued_at, expires_at, hold_until, udp_proof_key, session_id, relay_key, transport, mods) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'issued', date_trunc('seconds', statement_timestamp()), \
           date_trunc('seconds', statement_timestamp()) + make_interval(secs => $8), \
           date_trunc('seconds', statement_timestamp()) + make_interval(secs => $9), $10, $11, $12, $13, $14) \
         RETURNING issued_at, expires_at, hold_until",
    )
    .bind(id)
    .bind(game)
    .bind(world)
    .bind(profile)
    .bind(account)
    .bind(xuid)
    .bind(tier)
    .bind(GRANT_TTL_SECS as f64)
    .bind(hold as f64)
    .bind(udp_proof_key.as_bytes().as_slice())
    .bind(session_id)
    .bind(relay.key.as_deref())
    .bind(relay.transport)
    .bind(serde_json::to_value(&mods.entries).map_err(event_err)?)
    .fetch_one(&mut *conn)
    .await?;
    Ok((id, ms(issued), ms(expires), ms(until), udp_proof_key))
}

/// The declared set of the caller's play session's most recent grant (`mm_grant.mods`; vanilla
/// when it has none), for free roam's automatic step (MATCHMAKING_API.md §4a step 3, OD-248
/// answer 1, CR-L53): the ticket carries no set, so its seat copies the session's latest one. A
/// stored set that no longer validates reads as vanilla. Display and §5f `sync` only: mods
/// never choose the world (OD-235).
pub async fn latest_session_mods(db: &PgPool, caller: &Caller) -> Result<ModSet, MmError> {
    let mods: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT mods FROM mm_grant WHERE account_id = $1 AND profile_id = $2 AND game = $3 \
           AND session_id = $4 ORDER BY issued_at DESC, id DESC LIMIT 1",
    )
    .bind(caller.account_id)
    .bind(caller.profile_id)
    .bind(&caller.game)
    .bind(caller.session_id)
    .fetch_optional(db)
    .await?;
    let entries = mods.map(mods_from_json).unwrap_or_default();
    Ok(mod_set(Some(&entries)).unwrap_or_default())
}

/// `POST /mm/v1/join` in one transaction (MATCHMAKING_API.md §3.1 for the order). A live invite of
/// the caller's account passes `friends` and `code`; the first grant through it, or with the
/// caller's own pending request, ends those `accepted`.
pub async fn join(db: &PgPool, caller: &Caller, input: &JoinInput) -> Result<JoinOutcome, MmError> {
    join_with_relay(db, caller, input, &JoinRelay::default()).await
}

/// [`join`] with the joiner's relay report (CR-S57, MATCHMAKING_API.md §5b): a new seat stores the
/// relay its pair meets at ([`grant_relay`]) and its transport.
pub async fn join_with_relay(
    db: &PgPool,
    caller: &Caller,
    input: &JoinInput,
    relay: &JoinRelay<'_>,
) -> Result<JoinOutcome, MmError> {
    let mut tx = db.begin().await?;
    // One seat decision per profile at a time: a concurrent join waits, then sees the first one's
    // grant (`locked`), never a unique-index error.
    lock_profile_in(&mut tx, caller.profile_id).await?;
    lock_holder_in(&mut tx, caller.account_id, caller.profile_id).await?;
    // Acquire the grant's future FK row before the world/grants. Session ending locks it first
    // too, so inserting a replacement grant cannot deadlock on a late FK KEY SHARE lock.
    let live_caller: Option<(bool, bool, bool)> = sqlx::query_as(
        "SELECT profile_id = $2 AND account_id = $3 AND state = 'active' AND game_until > clock_timestamp(), dedicated, modded \
         FROM play_session WHERE id = $1 FOR SHARE",
    )
    .bind(caller.session_id).bind(caller.profile_id).bind(caller.account_id).fetch_optional(&mut *tx).await?;
    match live_caller {
        Some((true, false, _)) => {}
        Some((true, true, _)) => return Err(MmError::DedicatedHostOnly),
        _ => return Err(MmError::SessionEnded),
    };
    let now = now_in(&mut tx).await?;
    if input.code.is_some()
        && let Hit::Limited { retry_after_secs } =
            code_allowed_in(&mut tx, caller.account_id).await?
    {
        return Ok(JoinOutcome::RateLimited(retry_after_secs));
    }
    // The world row before any grant row: the lock order of hosting and closing.
    let target: Option<(Uuid, bool)> =
        match (input.world_id, input.code.as_deref()) {
            (Some(id), _) => {
                sqlx::query_as(
                    "SELECT w.id, w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
                       AND (w.host_kind = 'official' OR EXISTS (SELECT 1 FROM play_session s JOIN profile hp ON hp.id = w.host_profile_id \
                         WHERE s.id = w.host_session_id AND s.profile_id = w.host_profile_id \
                           AND s.account_id = w.host_account_id AND s.state = 'active' AND s.game_until > clock_timestamp() \
                           AND hp.mode = w.mode AND hp.game = w.game)) FROM world w \
                     WHERE w.id = $1 AND w.game = $2 FOR UPDATE OF w",
                )
                .bind(id)
                .bind(&caller.game)
                .fetch_optional(&mut *tx)
                .await?
            }
            // By code: only a world of the caller's mode exists for it (no oracle across modes).
            (None, Some(code)) => sqlx::query_as(
                "SELECT w.id, true FROM world w WHERE w.game = $1 AND w.code = $2 AND (w.mode = $3 OR w.any_mode) AND w.closed_at IS NULL \
                   AND w.expires_at > clock_timestamp() \
                   AND EXISTS (SELECT 1 FROM play_session s JOIN profile hp ON hp.id = w.host_profile_id \
                     WHERE s.id = w.host_session_id AND s.profile_id = w.host_profile_id \
                       AND s.account_id = w.host_account_id AND s.state = 'active' AND s.game_until > clock_timestamp() \
                       AND hp.mode = w.mode AND hp.game = w.game) FOR UPDATE OF w",
            )
            .bind(&caller.game)
            .bind(code)
            .bind(&caller.mode)
            .fetch_optional(&mut *tx)
            .await?,
            (None, None) => None,
        };
    // The session cannot change under our share lock, but its deadline can pass while waiting
    // for the world. Recheck the clock before replaying or creating any credential.
    let caller_live: bool =
        sqlx::query_scalar("SELECT game_until > clock_timestamp() FROM play_session WHERE id = $1")
            .bind(caller.session_id)
            .fetch_one(&mut *tx)
            .await?;
    if !caller_live {
        return Err(MmError::SessionEnded);
    }
    expire_own(
        &mut tx,
        &caller.game,
        caller.profile_id,
        &caller.mode,
        true,
        now,
    )
    .await?;
    if let Some(g) = live_grant_of(&mut tx, &caller.game, caller.profile_id).await? {
        if target.is_some_and(|(id, open)| open && id == g.world_id)
            && g.state == "issued"
            && g.session_id == Some(caller.session_id)
        {
            let (host_xuid, host_vm_id): (i64, Option<String>) = sqlx::query_as(
                "SELECT coalesce(p.xuid, 0), w.host_vm_id FROM world w LEFT JOIN profile p ON p.id = w.host_profile_id WHERE w.id = $1",
            )
            .bind(g.world_id)
            .fetch_one(&mut *tx)
            .await?;
            let voice_blocked_until =
                crate::voice::blocked_until_in(&mut tx, caller.account_id).await?;
            tx.commit().await?;
            return Ok(JoinOutcome::Granted(GrantIssued {
                id: g.id,
                world_id: g.world_id,
                host_xuid,
                host_vm_id,
                issued_at: g.issued_at,
                expires_at: g.expires_at,
                hold_until: g.hold_until,
                udp_proof_key: g.udp_proof_key,
                relay_key: g.relay_key,
                transport: g.transport,
                voice_blocked_until,
                mods: g.mods,
            }));
        }
        tx.commit().await?;
        return Ok(JoinOutcome::Locked {
            world_id: g.world_id,
            grant_id: g.id,
            expires_at: if g.state == "used" { now } else { g.expires_at },
        });
    }
    let (world_id, open) = match (target, input.world_id) {
        (Some(t), _) => t,
        (None, Some(_)) => return Err(MmError::WorldNotFound),
        (None, None) => {
            if input.code.is_some() {
                wrong_code_in(&mut tx, caller.account_id).await?;
            }
            tx.commit().await?;
            return Ok(JoinOutcome::Code);
        }
    };
    if !open {
        tx.commit().await?;
        return Ok(JoinOutcome::Closed);
    }
    let viewer = caller.viewer(input.ip.clone());
    let w = world_in(&mut tx, &viewer, world_id)
        .await?
        .ok_or(MmError::WorldNotFound)?;
    // By code alone, a world that bars the caller reads like a wrong code (counted), as the lookup
    // by code does: the code says nothing about who it bars.
    // OD-149: so does a clean world's code for a flagged caller (its lookups see no clean world).
    if input.world_id.is_none() && (w.barred || (w.viewer_flagged && !w.pool)) {
        wrong_code_in(&mut tx, caller.account_id).await?;
        tx.commit().await?;
        return Ok(JoinOutcome::Code);
    }
    let code_ok = input.code.is_some() && input.code == w.code;
    let a = assess(&w, &caller.rules(input.cell), code_ok);
    let wrong_code = input.code.is_some() && !code_ok && a.needs_code;
    let refused = match a.why {
        None => None,
        Some(Why::Own) => Some(JoinOutcome::NotAllowed(Refusal::OwnWorld)),
        Some(Why::Mode) => Some(JoinOutcome::NotAllowed(Refusal::Mode)),
        // OD-149: 409 `pool` both ways (committed like the refusals below).
        Some(Why::Pool) => {
            tx.commit().await?;
            return Err(MmError::Conflict("pool"));
        }
        Some(Why::Tier) => Some(JoinOutcome::NotAllowed(Refusal::Tier)),
        Some(Why::Host) => Some(JoinOutcome::NotAllowed(Refusal::Host)),
        Some(Why::Friends) if wrong_code => Some(JoinOutcome::Code),
        Some(Why::Friends) => Some(JoinOutcome::NotAllowed(Refusal::Friends)),
        Some(Why::Code) => Some(JoinOutcome::Code),
        Some(Why::Full) => Some(JoinOutcome::Full {
            players: u32::try_from(i64::from(w.players) + w.reserved + w.held - w.own_held)
                .unwrap_or(u32::MAX),
            max_players: u32::try_from(w.max_players).unwrap_or(0),
        }),
        Some(Why::Latency) => Some(JoinOutcome::Latency {
            rtt_ms: a.estimate.rtt_ms.unwrap_or(0),
            max_rtt_ms: w
                .max_rtt_ms
                .and_then(|m| u32::try_from(m).ok())
                .unwrap_or(0),
        }),
    };
    // OD-190 (§2.9 "Join friend"): an official world by id needs a friend of the caller seated
    // there (§3.1's order: before `full`).
    let group: bool = if w.host_vm_id.is_some() {
        sqlx::query_scalar("SELECT official_group FROM world WHERE id = $1")
            .bind(world_id)
            .fetch_one(&mut *tx)
            .await?
    } else {
        false
    };
    // CR-S96 (§2.10, §12.3): a new seat in a private official world takes the capability, and access
    // or a live invite with the friendship still holding, instead of the rule above. A live seat of
    // this session was answered above: it is no new join.
    let private_entry = if w.private {
        if !session_has_feature_in(&mut tx, caller.session_id, FEATURE_PRIVATE_OFFICIAL).await? {
            tx.commit().await?;
            return Err(MmError::Conflict("client_update"));
        }
        if let Some(region) = relay.region
            && w.region.as_deref().is_some_and(|r| r != region)
        {
            tx.commit().await?;
            return Err(MmError::Conflict("region"));
        }
        let access = private_access_in(&mut tx, world_id, caller.account_id).await?;
        private_may_enter_in(&mut tx, world_id, caller.account_id, access).await?
    } else {
        false
    };
    let refused = if w.private {
        if private_entry {
            refused
        } else if matches!(
            refused,
            None | Some(JoinOutcome::Full { .. }) | Some(JoinOutcome::Latency { .. })
        ) {
            Some(JoinOutcome::NotAllowed(Refusal::Friends))
        } else {
            refused
        }
    } else if w.host_vm_id.is_some()
        && (group || !w.friend_seated)
        && matches!(
            refused,
            None | Some(JoinOutcome::Full { .. }) | Some(JoinOutcome::Latency { .. })
        )
    {
        Some(JoinOutcome::NotAllowed(Refusal::Friends))
    } else {
        refused
    };
    // OD-181: any declared set may join any world (OD-112's equal-set rule is retired).
    // OD-150: a world that runs another activity than the one asked for (after every reason the
    // list would give, so it tells nothing the list does not).
    if refused.is_none() && input.activity.as_deref().is_some_and(|a| a != w.activity) {
        // Committed like the refusals below: the code attempt and the expiries above stay.
        tx.commit().await?;
        return Err(MmError::Conflict("activity"));
    }
    if let Some(outcome) = refused {
        if wrong_code && outcome == JoinOutcome::Code {
            wrong_code_in(&mut tx, caller.account_id).await?;
        }
        tx.commit().await?;
        return Ok(outcome);
    }
    // OD-189 (§2.9): an official world is direct, always; "always through the relay" is refused.
    if w.host_vm_id.is_some() && relay.relay_only {
        tx.commit().await?;
        return Err(MmError::Conflict("official_direct"));
    }
    let wr = world_relay_in(&mut tx, world_id).await?;
    let pair = if w.host_vm_id.is_some() {
        GrantRelay::none()
    } else {
        grant_relay(
            &wr.transport,
            wr.key.as_deref(),
            wr.nat.as_deref(),
            wr.relays.as_ref(),
            relay,
        )?
    };
    // OD-160: a Hardcore world's relayed seats are bounded by its seats alone.
    if pair.transport == "relay" && !wr.hardcore {
        relay_room_in(
            &mut tx,
            world_id,
            None,
            caller.account_id,
            relay.max_relayed,
        )
        .await?;
    }
    let (id, issued, expires, hold, udp_proof_key) = insert_grant_in(
        &mut tx,
        &caller.game,
        world_id,
        caller.profile_id,
        caller.account_id,
        caller.xuid,
        &caller.tier,
        caller.session_id,
        input.hold_secs,
        &input.mods,
        &pair,
    )
    .await?;
    end_offers_in(
        &mut tx,
        OfferScope::Seated {
            world: world_id,
            account: caller.account_id,
            grant: id,
        },
        "accepted",
    )
    .await?;
    if w.private {
        add_world_access_in(&mut tx, world_id, caller.account_id, "invite").await?;
    }
    let voice_blocked_until = crate::voice::blocked_until_in(&mut tx, caller.account_id).await?;
    tx.commit().await?;
    Ok(JoinOutcome::Granted(GrantIssued {
        id,
        world_id,
        host_xuid: w.host_xuid,
        host_vm_id: w.host_vm_id.clone(),
        issued_at: issued,
        expires_at: expires,
        hold_until: hold,
        udp_proof_key: Some(udp_proof_key),
        relay_key: pair.key,
        transport: pair.transport.to_owned(),
        voice_blocked_until,
        mods: input.mods.entries.clone(),
    }))
}

/// Gives the caller's reserved seat in its world 60 s more when the game's FreeRoam ticket
/// resolves the world through it (MATCHMAKING_API.md §4a, once: `arrived_at`), on the caller's
/// transaction: only in a world of the ticket's `mode` that its tier may enter (`vip` for a
/// Supporter-only one). Whether a seat was extended.
pub async fn arrive_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
    world: Uuid,
    mode: &str,
    vip: bool,
    caller_session: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query(
        "UPDATE mm_grant g SET expires_at = greatest(g.expires_at, date_trunc('seconds', clock_timestamp()) + make_interval(secs => $4)), \
           arrived_at = clock_timestamp() \
         FROM world w \
         WHERE g.game = $1 AND g.profile_id = $2 AND g.world_id = $3 AND g.state = 'issued' \
           AND g.expires_at > clock_timestamp() AND g.arrived_at IS NULL \
           AND g.session_id = $7 \
           AND w.id = g.world_id AND (w.mode = $5 OR w.any_mode) AND (w.audience <> 'supporters' OR $6) \
           AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
           AND EXISTS (SELECT 1 FROM play_session ps JOIN profile pp ON pp.id = ps.profile_id \
             WHERE ps.id = g.session_id AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id \
               AND ps.state = 'active' AND ps.game_until > clock_timestamp() AND (pp.mode = w.mode OR w.any_mode) AND pp.game = w.game) \
           AND (w.host_kind = 'official' OR EXISTS (SELECT 1 FROM play_session hs JOIN profile hp ON hp.id = hs.profile_id \
             WHERE hs.id = w.host_session_id AND hs.profile_id = w.host_profile_id AND hs.account_id = w.host_account_id \
               AND hs.state = 'active' AND hs.game_until > clock_timestamp() AND hp.mode = w.mode AND hp.game = w.game))",
    )
    .bind(game)
    .bind(profile)
    .bind(world)
    .bind(GRANT_TTL_SECS as f64)
    .bind(mode)
    .bind(vip)
    .bind(caller_session)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0)
}

/// `DELETE /mm/v1/join/{id}`: releases the caller's grant (idempotent: an unknown or ended id is
/// fine).
pub async fn release(db: &PgPool, caller: &Caller, id: Uuid) -> Result<(), IdentityError> {
    let mut tx = db.begin().await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE id = $1 AND profile_id = $2 AND session_id = $3 AND state IN ('issued', 'used') FOR UPDATE"
    )))
    .bind(id)
    .bind(caller.profile_id)
    .bind(caller.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = row {
        move_grant(&mut tx, &grant_row(&r)?, "released", None).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `DELETE /mm/v1/seat`: releases the caller's live seat, whatever its id (idempotent).
pub async fn leave_seat(db: &PgPool, caller: &Caller) -> Result<(), IdentityError> {
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, caller.profile_id).await?;
    if let Some(g) = live_grant_of(&mut tx, &caller.game, caller.profile_id).await?
        && g.session_id == Some(caller.session_id)
    {
        move_grant(&mut tx, &g, "released", None).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// OD-213 (WORLDS_OFFICIAL.md §11.4 "Life"): the caller's game left the MPSD session of the
/// official group world `world` (`members.me: null`: the player went back to Horizon Solo). Its
/// live seat of this play session in that world ends (`released`); the group world then closes on
/// its next announce once no seat in it is live, so the next lobby search places the player with
/// everyone else. `true` when a seat ended. Any other world, or no such seat: nothing (idempotent).
pub async fn official_group_left(
    db: &PgPool,
    game: &str,
    profile_id: Uuid,
    session_id: Uuid,
    world: Uuid,
) -> Result<bool, IdentityError> {
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, profile_id).await?;
    let Some(g) = live_grant_of(&mut tx, game, profile_id).await? else {
        return Ok(false);
    };
    if g.world_id != world || g.session_id != Some(session_id) {
        return Ok(false);
    }
    let group: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM world WHERE id = $1 AND official_group AND closed_at IS NULL)",
    )
    .bind(world)
    .fetch_one(&mut *tx)
    .await?;
    if !group {
        return Ok(false);
    }
    move_grant(&mut tx, &g, "released", None).await?;
    tx.commit().await?;
    Ok(true)
}

/// A live seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatGrant {
    /// Grant id.
    pub id: Uuid,
    /// `issued` (reserved) or `used` (seated).
    pub state: String,
    /// Host's XUID (0: an official world).
    pub host_xuid: i64,
    /// An official world's `vmId` (OD-188/189).
    pub host_vm_id: Option<String>,
    /// Expires at.
    pub expires_at: UnixMs,
    /// Hold.
    pub hold_until: UnixMs,
    /// This grant's private UDP proof credential; absent on legacy or kicked grants.
    pub udp_proof_key: Option<UdpProofKey>,
    /// The relay this pair meets at (CR-S57; `None`: no fallback).
    pub relay_key: Option<String>,
    /// `direct` | `relay`.
    pub transport: String,
    /// The caller's account's voice block in force (CR-S67), signed into the grant.
    pub voice_blocked_until: Option<UnixMs>,
    /// The seat's declared set (`mm_grant.mods`; CR-L53 signs its sync ids).
    pub mods: Vec<ModEntry>,
}

/// The caller's seat now.
// One per seat read; the grant stays unboxed (CR-L53 added its declared set) so callers keep
// matching it by value.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeatNow {
    /// It hosts this world.
    Hosting(Box<Assessed>),
    /// A reserved or used seat in this world.
    Seat(SeatGrant, Box<Assessed>),
}

/// How the caller's last seat ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatEnd {
    /// World.
    pub world_id: Uuid,
    /// Grant.
    pub grant_id: Uuid,
    /// When.
    pub at: UnixMs,
    /// `expired` | `released` | `kicked` | `world_closed` | `pool`.
    pub reason: String,
}

/// The seat read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatView {
    /// The effective leader of the private world the caller holds a live seat in (CR-S96).
    pub private_leader: Option<i64>,
    /// The game session linked to the private official world the caller holds a live seat in:
    /// scid, template, name (the inbox's `session`, [`private_session_in`]; CR-S102).
    pub private_session: Option<(String, String, String)>,
    /// The open private official world this account may rejoin (CR-S96).
    pub private_world: Option<Uuid>,
    /// The live seat or hosted world.
    pub now: Option<SeatNow>,
    /// The last seat, ended within [`ENDED_VISIBLE_SECS`], when none is live.
    pub ended: Option<SeatEnd>,
    /// The caller's account's latest request to join (pending, or ended within two minutes).
    pub request: Option<OfferRow>,
    /// The caller's cheat flag's end while it lasts (OD-149, `pool: {until}`; Hardcore only).
    pub pool_until: Option<UnixMs>,
    /// The database clock.
    pub at: UnixMs,
}

/// The game session linked to a private official world, as the inbox reads it ([`inbox_in`]'s
/// `session`): its oldest live `freeroam_session`, scid, template and name; none for any other world.
async fn private_session_in(
    conn: &mut PgConnection,
    world: Uuid,
) -> Result<Option<(String, String, String)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT f.scid, f.template, f.name FROM freeroam_session f JOIN world w ON w.id = f.private_world_id \
         WHERE f.private_world_id = $1 AND w.official_owner_account_id IS NOT NULL \
           AND f.expires_at > clock_timestamp() ORDER BY f.created_at LIMIT 1",
    )
    .bind(world)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn seat_in(
    conn: &mut PgConnection,
    caller: &Caller,
    ip: Option<String>,
    cell: Option<i32>,
) -> Result<SeatView, IdentityError> {
    let at = now_in(conn).await?;
    let viewer = caller.viewer(ip);
    let rules = caller.rules(cell);
    let request = latest_request_in(conn, caller.account_id).await?;
    let pool_until = if caller.mode == "hardcore" {
        crate::observations::flag_of_in(conn, caller.profile_id).await?
    } else {
        None
    };
    // Only the caller's mode and what its tier may hold (defence in depth: a session's end and a
    // mode change already released the rest).
    let private_world = private_rejoin_in(conn, caller).await?;
    let hosted: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM world WHERE game = $1 AND host_profile_id = $2 AND mode = $3 AND closed_at IS NULL \
           AND expires_at > clock_timestamp() AND host_session_id = $4",
    )
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(&caller.mode)
    .bind(caller.session_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = hosted
        && let Some(w) = world_in(conn, &viewer, id).await?
    {
        return Ok(SeatView {
            private_leader: None,
            private_session: None,
            now: Some(SeatNow::Hosting(Box::new(assess(&w, &rules, false)))),
            ended: None,
            request,
            pool_until,
            private_world,
            at,
        });
    }
    let live = sqlx::query(
        "SELECT g.id, g.state, g.world_id, g.expires_at, g.hold_until, g.udp_proof_key, g.relay_key, g.transport, g.mods, coalesce(p.xuid, 0) AS host_xuid, w.host_vm_id, \
           (SELECT CASE WHEN greatest(va.voice_banned_until, va.voice_community_until) > clock_timestamp() \
               THEN greatest(va.voice_banned_until, va.voice_community_until) END \
             FROM account va WHERE va.id = g.account_id) AS voice_blocked_until \
         FROM mm_grant g JOIN world w ON w.id = g.world_id LEFT JOIN profile p ON p.id = w.host_profile_id \
         WHERE g.game = $1 AND g.profile_id = $2 \
           AND ((g.state = 'issued' AND g.expires_at > clock_timestamp()) OR g.state = 'used') \
           AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
           AND (w.mode = $3 OR w.any_mode) AND (w.audience <> 'supporters' OR $4) \
           AND (w.host_kind = 'official' OR (EXISTS (SELECT 1 FROM play_session s WHERE s.id = w.host_session_id \
             AND s.profile_id = w.host_profile_id AND s.account_id = w.host_account_id \
             AND s.state = 'active' AND s.game_until > clock_timestamp()) \
           AND p.mode = w.mode AND p.game = w.game)) AND g.session_id = $5 \
           AND EXISTS (SELECT 1 FROM play_session ps JOIN profile pp ON pp.id = g.profile_id \
             WHERE ps.id = g.session_id AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id \
               AND ps.state = 'active' AND ps.game_until > clock_timestamp() AND (pp.mode = w.mode OR w.any_mode) AND pp.game = w.game)",
    )
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(&caller.mode)
    // OD-129: `supporters` worlds are everyone's.
    .bind(true)
    .bind(caller.session_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(r) = live {
        let world: Uuid = r.try_get("world_id")?;
        if let Some(w) = world_in(conn, &viewer, world).await? {
            let grant = SeatGrant {
                id: r.try_get("id")?,
                state: r.try_get("state")?,
                host_xuid: r.try_get("host_xuid")?,
                host_vm_id: r.try_get("host_vm_id")?,
                expires_at: ms(r.try_get("expires_at")?),
                hold_until: ms(r.try_get("hold_until")?),
                udp_proof_key: udp_proof_key(&r)?,
                relay_key: r.try_get("relay_key")?,
                transport: r.try_get("transport")?,
                voice_blocked_until: r
                    .try_get::<Option<OffsetDateTime>, _>("voice_blocked_until")?
                    .map(ms),
                mods: mods_from_json(r.try_get("mods")?),
            };
            // CR-S96: only here, to a reader seated in the world.
            let (private_leader, private_session) = if w.private {
                (
                    private_leader_row_in(conn, world)
                        .await?
                        .map(|(_, _, xuid)| xuid),
                    private_session_in(conn, world).await?,
                )
            } else {
                (None, None)
            };
            return Ok(SeatView {
                private_leader,
                private_session,
                now: Some(SeatNow::Seat(grant, Box::new(assess(&w, &rules, true)))),
                ended: None,
                request,
                pool_until,
                private_world,
                at,
            });
        }
    }
    // The last seat: an issued one past its time counts as expired now (the sweep writes it).
    let last = sqlx::query(
        "SELECT g.id, g.world_id, \
           CASE WHEN g.state = 'issued' AND g.expires_at <= clock_timestamp() THEN g.expires_at \
             ELSE coalesce(g.ended_at, clock_timestamp()) END AS at, \
           CASE WHEN g.state = 'issued' AND g.expires_at <= clock_timestamp() THEN 'expired' \
             WHEN g.state IN ('issued', 'used') THEN 'world_closed' ELSE coalesce(g.end_reason, g.state) END AS reason \
         FROM mm_grant g WHERE g.game = $1 AND g.profile_id = $2 \
           AND (g.ended_at > clock_timestamp() - make_interval(secs => $3) \
             OR (g.state = 'issued' AND g.expires_at > clock_timestamp() - make_interval(secs => $3)) \
             OR g.state = 'used') \
         ORDER BY coalesce(g.ended_at, g.expires_at) DESC, g.issued_at DESC LIMIT 1",
    )
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(ENDED_VISIBLE_SECS as f64)
    .fetch_optional(&mut *conn)
    .await?;
    let ended = last
        .map(|r| -> Result<SeatEnd, sqlx::Error> {
            Ok(SeatEnd {
                world_id: r.try_get("world_id")?,
                grant_id: r.try_get("id")?,
                at: ms(r.try_get("at")?),
                reason: r.try_get("reason")?,
            })
        })
        .transpose()?;
    Ok(SeatView {
        private_leader: None,
        private_session: None,
        now: None,
        ended,
        request,
        pool_until,
        private_world,
        at,
    })
}

/// `GET /mm/v1/seat`.
pub async fn seat(
    db: &PgPool,
    caller: &Caller,
    ip: Option<String>,
    cell: Option<i32>,
) -> Result<SeatView, IdentityError> {
    let mut conn = db.acquire().await?;
    seat_in(&mut conn, caller, ip, cell).await
}

/// Seat and its authorized roster from one database snapshot, using one pooled connection.
pub struct SeatSnapshot {
    /// The ordinary seat view.
    pub seat: SeatView,
    /// Its live world's roster; absent when there is no live seat or hosted world.
    pub members: Option<Vec<MemberRow>>,
}

/// Opt-in seat read. The roster shares the standalone endpoint's session bucket and SQL.
/// Reads never renew a seat or world. A clock-based TTL crossing aborts the response instead of
/// reporting a successful empty roster. Rate-limit refusals still commit their attempt count.
pub async fn seat_with_members(
    db: &PgPool,
    caller: &Caller,
    ip: Option<String>,
    cell: Option<i32>,
) -> Result<SeatSnapshot, MmError> {
    // Concurrent roster reads may change the shared rate bucket after this snapshot starts.
    // Aborted transactions roll their counter write back. Retry only this bounded read flow.
    let mut retries = 2;
    loop {
        let result = seat_with_members_once(db, caller, ip.clone(), cell).await;
        if matches!(&result, Err(MmError::Db(IdentityError::Busy))) && retries > 0 {
            retries -= 1;
            continue;
        }
        return result;
    }
}

async fn seat_with_members_once(
    db: &PgPool,
    caller: &Caller,
    ip: Option<String>,
    cell: Option<i32>,
) -> Result<SeatSnapshot, MmError> {
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let seat = seat_in(&mut tx, caller, ip, cell).await?;
    let world = match &seat.now {
        Some(SeatNow::Hosting(w) | SeatNow::Seat(_, w)) => Some(w.row.id),
        None => None,
    };
    let members = if let Some(world) = world {
        if let Hit::Limited { retry_after_secs } = limits::hit_in(
            &mut tx,
            &limit_key(&MEMBERS_LIMIT, caller.session_id),
            &MEMBERS_LIMIT,
        )
        .await?
        {
            tx.commit().await?;
            return Err(MmError::RateLimited(retry_after_secs));
        }
        let members = members_in(&mut tx, caller, world).await?;
        if members.is_none() {
            tx.commit().await?;
            return Err(MmError::SnapshotChanged);
        }
        members
    } else {
        None
    };
    tx.commit().await?;
    Ok(SeatSnapshot { seat, members })
}

/// `POST /mm/v1/seat/renew {grantId}`: a reserved seat gets `min(now + 60 s, hold_until)`; past
/// its hold it ends `expired`; in another mode's world, or a Supporter-only one for a free ticket,
/// it ends `released` (the ticket re-checks it). Answers the seat read.
pub async fn renew(
    db: &PgPool,
    caller: &Caller,
    grant_id: Uuid,
    ip: Option<String>,
    cell: Option<i32>,
) -> Result<SeatView, IdentityError> {
    let mut tx = db.begin().await?;
    lock_profile_in(&mut tx, caller.profile_id).await?;
    // As in join/host/close: world before grant. A close cannot race this reservation update,
    // and the UPDATE still rechecks TTL because wall-clock expiry needs no writer.
    sqlx::query(
        "SELECT w.id FROM world w JOIN mm_grant g ON g.world_id = w.id \
         WHERE g.id = $1 AND g.game = $2 AND g.profile_id = $3 AND g.session_id = $4 FOR UPDATE OF w",
    )
    .bind(grant_id)
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(caller.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let now = now_in(&mut tx).await?;
    expire_own(
        &mut tx,
        &caller.game,
        caller.profile_id,
        &caller.mode,
        true,
        now,
    )
    .await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GRANT_COLUMNS} FROM mm_grant WHERE id = $1 AND game = $2 AND profile_id = $3 AND session_id = $4 AND state = 'issued' FOR UPDATE"
    )))
    .bind(grant_id)
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(caller.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = row {
        let g = grant_row(&r)?;
        let renewed = sqlx::query(
            "UPDATE mm_grant g SET expires_at = least(date_trunc('seconds', clock_timestamp()) + make_interval(secs => $2), hold_until), \
               renewals = renewals + 1 \
             WHERE id = $1 AND expires_at > clock_timestamp() AND g.session_id = $4 \
               AND least(date_trunc('seconds', clock_timestamp()) + make_interval(secs => $2), hold_until) > clock_timestamp() \
               AND least(date_trunc('seconds', clock_timestamp()) + make_interval(secs => $2), hold_until) >= expires_at \
               AND EXISTS (SELECT 1 FROM world w LEFT JOIN play_session s ON s.id = w.host_session_id \
                 LEFT JOIN profile hp ON hp.id = w.host_profile_id WHERE w.id = g.world_id \
                   AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() AND (w.mode = $3 OR w.any_mode) \
                   AND (w.host_kind = 'official' OR (hp.mode = w.mode AND hp.game = w.game \
                   AND s.profile_id = w.host_profile_id AND s.account_id = w.host_account_id \
                   AND s.state = 'active' AND s.game_until > clock_timestamp()))) \
               AND EXISTS (SELECT 1 FROM play_session ps WHERE ps.id = g.session_id \
                 AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id \
                 AND ps.state = 'active' AND ps.game_until > clock_timestamp())",
        )
        .bind(g.id)
        .bind(GRANT_TTL_SECS as f64)
        .bind(&caller.mode)
        .bind(caller.session_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if renewed == 0 && g.expires_at <= now {
            move_grant(&mut tx, &g, "expired", None).await?;
        }
    }
    let view = seat_in(&mut tx, caller, ip, cell).await?;
    tx.commit().await?;
    Ok(view)
}

// ---- The world's roster (OD-63) -----------------------------------------------------------------

/// One player of a world's roster (never an address, an account id or an e-mail on the wire).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberRow {
    /// `host` | `seated` | `reserved`.
    pub role: String,
    /// XUID.
    pub xuid: i64,
    /// `Name#12345`.
    pub gamertag: String,
    /// `free` | `vip`: the world's host tier, or the seat's at the join.
    pub tier: String,
    /// `none` | `friends` | `outgoing` | `incoming` | `self`: the caller's account's relation to
    /// the member's ([`crate::friends::relation_sql`]).
    pub friend: String,
    /// The host: the world's opening; seated: the host's first announce listing it; reserved: the
    /// seat's issue.
    pub since: UnixMs,
    /// The member's account's voice block in force (CR-S67, `crate::voice`), else `None`.
    pub voice_blocked_until: Option<UnixMs>,
    /// The member's declared mod set (OD-181): the host's row the world's set, a seat's the set its
    /// join declared; empty: none.
    pub mods: Vec<ModEntry>,
}

/// The roster statement: `$1` the world, `$2` the caller's game, `$3` its mode, `$4` its profile,
/// `$5` a Supporter ticket, `$6` its account. The world (`cw`) is read only when it is open,
/// inside its TTL, of the caller's game and mode, and the caller hosts it or holds a live seat in
/// it (a `supporters` world only with a Supporter ticket, §3.2's defence in depth); then its host
/// (read always, with the world's `dedicated` flag: `members_in` drops a dedicated world's host
/// row, OD-109) and its live seats (`issued` and unexpired, or `used`), never the host's `peers` report. One
/// statement, so one snapshot of the world and its seats (tests EXPLAIN it).
pub fn members_sql() -> String {
    let friend = crate::friends::relation_sql("$6", "m.account_id");
    let voice = crate::voice::blocked_until_sql("m.account_id");
    format!(
        "WITH cw AS ( \
           SELECT w.id, w.host_profile_id, w.host_account_id, w.host_tier, w.opened_at, w.dedicated, w.mods FROM world w \
           WHERE w.id = $1 AND w.game = $2 AND (w.mode = $3 OR w.any_mode) \
             AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
             AND (w.host_profile_id = $4 \
               OR (EXISTS (SELECT 1 FROM mm_grant s WHERE s.world_id = w.id AND s.game = $2 AND s.profile_id = $4 \
                     AND (s.state = 'used' OR (s.state = 'issued' AND s.expires_at > clock_timestamp()))) \
                   AND (w.audience <> 'supporters' OR $5)))), \
         m AS ( \
           SELECT 0 AS rank, 'host' AS role, cw.host_profile_id AS profile_id, cw.host_account_id AS account_id, \
                  cw.host_tier AS tier, cw.opened_at AS since, NULL::uuid AS grant_id, cw.mods FROM cw \
           WHERE cw.host_profile_id IS NOT NULL \
           UNION ALL \
           SELECT CASE WHEN g.state = 'used' THEN 1 ELSE 2 END, \
                  CASE WHEN g.state = 'used' THEN 'seated' ELSE 'reserved' END, g.profile_id, g.account_id, g.tier, \
                  CASE WHEN g.state = 'used' THEN coalesce(g.used_at, g.issued_at) ELSE g.issued_at END, g.id, g.mods \
           FROM cw JOIN mm_grant g ON g.world_id = cw.id \
           WHERE g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())) \
         SELECT m.role, p.xuid, p.name AS gamertag, m.tier, \
                m.since, {friend} AS friend, {voice} AS voice_blocked_until, m.mods, \
                (SELECT cw.dedicated FROM cw) AS dedicated \
         FROM m JOIN profile p ON p.id = m.profile_id \
         ORDER BY m.rank, m.since, m.grant_id"
    )
}

/// `GET /mm/v1/worlds/{worldId}/members` (MATCHMAKING_API.md §3.7): the host, then the seated
/// peers by arrival, then the reserved seats by issue; `None` when the caller may not read it
/// (not its host, no live seat in it, another game or mode, closed, lapsed or unknown: one answer).
pub async fn members(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
) -> Result<Option<Vec<MemberRow>>, IdentityError> {
    let mut conn = db.acquire().await?;
    members_in(&mut conn, caller, world_id).await
}

/// The same authorized roster on a connection already held by the caller.
async fn members_in(
    conn: &mut PgConnection,
    caller: &Caller,
    world_id: Uuid,
) -> Result<Option<Vec<MemberRow>>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(members_sql()))
        .bind(world_id)
        .bind(&caller.game)
        .bind(&caller.mode)
        .bind(caller.profile_id)
        // OD-129: `supporters` worlds are everyone's.
        .bind(true)
        .bind(caller.account_id)
        .fetch_all(&mut *conn)
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    // The host row is always read (it is what makes an authorized empty roster non-empty); a
    // dedicated world's host is not a player and is left out here (OD-109).
    let mut dedicated = false;
    for r in &rows {
        dedicated |= r.try_get::<bool, _>("dedicated")?;
    }
    let members = rows
        .iter()
        .filter(|r| {
            !(dedicated
                && r.try_get::<String, _>("role")
                    .is_ok_and(|role| role == "host"))
        })
        .map(|r| -> Result<MemberRow, sqlx::Error> {
            Ok(MemberRow {
                role: r.try_get("role")?,
                xuid: r.try_get("xuid")?,
                gamertag: r
                    .try_get::<Option<String>, _>("gamertag")?
                    .unwrap_or_default(),
                tier: r.try_get("tier")?,
                friend: r.try_get("friend")?,
                since: ms(r.try_get("since")?),
                voice_blocked_until: r
                    .try_get::<Option<OffsetDateTime>, _>("voice_blocked_until")?
                    .map(ms),
                mods: mods_from_json(r.try_get("mods")?),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(members))
}

// ---- Invites and requests (OD-56, OD-59) --------------------------------------------------------

/// An invite or a request to join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferRow {
    /// Id.
    pub id: Uuid,
    /// `invite` | `request`.
    pub kind: String,
    /// Game.
    pub game: String,
    /// World.
    pub world_id: Uuid,
    /// Sender's account (the host for an invite, the asker for a request).
    pub from_account_id: Uuid,
    /// Sender's profile (the asker's gets the seat of a request).
    pub from_profile_id: Uuid,
    /// The account that answers (the invitee, or the host).
    pub to_account_id: Uuid,
    /// `pending` | `accepted` | `declined` | `expired` | `withdrawn` (a pending one past
    /// `expires_at` reads `expired`).
    pub state: String,
    /// Created.
    pub created_at: UnixMs,
    /// Lapses.
    pub expires_at: UnixMs,
    /// Ended.
    pub ended_at: Option<UnixMs>,
    /// The seat an accepted one led to.
    pub grant_id: Option<Uuid>,
}

const OFFER_COLUMNS: &str = "i.id, i.kind, i.game, i.world_id, i.from_account_id, i.from_profile_id, i.to_account_id, \
     CASE WHEN i.state = 'pending' AND i.expires_at <= clock_timestamp() THEN 'expired' ELSE i.state END AS state, \
     i.created_at, i.expires_at, i.ended_at, i.grant_id";

fn offer_row(r: &sqlx::postgres::PgRow) -> Result<OfferRow, sqlx::Error> {
    Ok(OfferRow {
        id: r.try_get("id")?,
        kind: r.try_get("kind")?,
        game: r.try_get("game")?,
        world_id: r.try_get("world_id")?,
        from_account_id: r.try_get("from_account_id")?,
        from_profile_id: r.try_get("from_profile_id")?,
        to_account_id: r.try_get("to_account_id")?,
        state: r.try_get("state")?,
        created_at: ms(r.try_get("created_at")?),
        expires_at: ms(r.try_get("expires_at")?),
        ended_at: r.try_get::<Option<OffsetDateTime>, _>("ended_at")?.map(ms),
        grant_id: r.try_get("grant_id")?,
    })
}

async fn latest_request_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Option<OfferRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {OFFER_COLUMNS} FROM world_invite i WHERE i.from_account_id = $1 AND i.kind = 'request' \
           AND (i.state = 'pending' OR i.ended_at > clock_timestamp() - make_interval(secs => $2)) \
           AND i.created_at > clock_timestamp() - interval '15 minutes' \
         ORDER BY i.created_at DESC LIMIT 1"
    )))
    .bind(account)
    .bind(OFFER_TTL_SECS as f64)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(offer_row).transpose()?)
}

/// Which pending offers [`end_offers_in`] ends.
#[derive(Clone, Copy, Debug)]
enum OfferScope {
    /// Every pending offer of a world (it closed: `withdrawn`).
    World(Uuid),
    /// The pending offers that give `account` a seat in `world` (it just got `grant`).
    Seated {
        world: Uuid,
        account: Uuid,
        grant: Uuid,
    },
    /// Pending offers past their time, of every world (the sweep; rows another transaction holds
    /// are left for the next pass).
    Lapsed,
    /// Pending offers of one world past their time (before a new one counts the world's).
    LapsedOf(Uuid),
    /// One offer.
    One(Uuid),
}

/// Ends pending offers as `outcome`.
async fn end_offers_in(
    conn: &mut PgConnection,
    scope: OfferScope,
    outcome: &str,
) -> Result<u64, IdentityError> {
    let lock = if matches!(scope, OfferScope::Lapsed) {
        "FOR UPDATE SKIP LOCKED"
    } else {
        "FOR UPDATE"
    };
    let (filter, a, b, grant): (&str, Option<Uuid>, Option<Uuid>, Option<Uuid>) = match scope {
        OfferScope::World(w) => ("world_id = $1", Some(w), None, None),
        OfferScope::Seated {
            world,
            account,
            grant,
        } => (
            "world_id = $1 AND expires_at > clock_timestamp() AND \
             ((kind = 'invite' AND to_account_id = $2) OR (kind = 'request' AND from_account_id = $2))",
            Some(world),
            Some(account),
            Some(grant),
        ),
        OfferScope::Lapsed => (
            "expires_at <= clock_timestamp() AND $1::uuid IS NULL AND $2::uuid IS NULL",
            None,
            None,
            None,
        ),
        OfferScope::LapsedOf(w) => (
            "world_id = $1 AND expires_at <= clock_timestamp() AND $2::uuid IS NULL",
            Some(w),
            None,
            None,
        ),
        OfferScope::One(id) => ("id = $1 AND $2::uuid IS NULL", Some(id), None, None),
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE world_invite SET state = $3, ended_at = clock_timestamp(), grant_id = $4 \
         WHERE id IN (SELECT id FROM world_invite WHERE state = 'pending' AND {filter} \
                      ORDER BY expires_at LIMIT 1000 {lock}) \
         RETURNING id, kind, game, world_id, from_account_id, to_account_id, created_at, ended_at"
    )))
    .bind(a)
    .bind(b)
    .bind(outcome)
    .bind(grant)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.len() as u64)
}

/// What sending an invite or a request did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    /// The offer.
    pub offer: OfferRow,
    /// The handle (CORE_API.md §4.1) of the account that answers.
    pub to_handle: i64,
    /// A new one (`false`: the live one, unchanged).
    pub created: bool,
}

/// The handle of an account: its first profile's XUID.
async fn handle_in(conn: &mut PgConnection, account: Uuid) -> Result<i64, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT xuid FROM profile WHERE account_id = $1 ORDER BY created_at, id LIMIT 1",
    )
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or(0))
}

/// The account is a Supporter now, active and not banned (the friend graph's `OPEN`).
async fn open_account_in(conn: &mut PgConnection, account: Uuid) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM account a WHERE a.id = $1 AND {})",
        crate::friends::OPEN
    )))
    .bind(account)
    .fetch_one(&mut *conn)
    .await?)
}

async fn mutual_friends_in(
    conn: &mut PgConnection,
    a: Uuid,
    b: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM account_friendship f JOIN account_friendship back \
           ON back.account_id = f.friend_id AND back.friend_id = f.account_id \
           WHERE f.account_id = $1 AND f.friend_id = $2)",
    )
    .bind(a)
    .bind(b)
    .fetch_one(&mut *conn)
    .await?)
}

async fn pending_offer_in(
    conn: &mut PgConnection,
    world: Uuid,
    kind: &str,
    from: Uuid,
    to: Uuid,
) -> Result<Option<OfferRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {OFFER_COLUMNS} FROM world_invite i WHERE i.world_id = $1 AND i.kind = $2 \
           AND i.from_account_id = $3 AND i.to_account_id = $4 AND i.state = 'pending' \
           AND i.expires_at > clock_timestamp()"
    )))
    .bind(world)
    .bind(kind)
    .bind(from)
    .bind(to)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(offer_row).transpose()?)
}

/// Seats and pending offers of a locked world: players, reserved, held.
async fn seats_in(conn: &mut PgConnection, world: Uuid) -> Result<(i64, i64, i64), IdentityError> {
    Ok(sqlx::query_as(
        "SELECT (CASE WHEN w.dedicated OR w.host_kind = 'official' THEN 0 ELSE 1 END) \
           + (SELECT count(*) FROM mm_grant s WHERE s.world_id = w.id AND s.state = 'used'), w.max_players::bigint, \
           (SELECT count(*) FROM mm_grant g WHERE g.world_id = w.id AND g.state = 'issued' AND g.expires_at > clock_timestamp()) \
           + (SELECT count(*) FROM world_invite i WHERE i.world_id = w.id AND i.state = 'pending' AND i.expires_at > clock_timestamp()) \
         FROM world w WHERE w.id = $1",
    )
    .bind(world)
    .fetch_one(&mut *conn)
    .await?)
}

async fn pending_count_in(conn: &mut PgConnection, world: Uuid) -> Result<i64, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM world_invite WHERE world_id = $1 AND state = 'pending' AND expires_at > clock_timestamp()",
    )
    .bind(world)
    .fetch_one(&mut *conn)
    .await?)
}

#[allow(clippy::too_many_arguments)]
async fn insert_offer_in(
    conn: &mut PgConnection,
    kind: &str,
    game: &str,
    world: Uuid,
    from_account: Uuid,
    from_profile: Uuid,
    to_account: Uuid,
    from_session: Option<Uuid>,
) -> Result<OfferRow, IdentityError> {
    let id = Uuid::now_v7();
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH i AS (INSERT INTO world_invite (id, game, kind, world_id, from_account_id, from_profile_id, to_account_id, expires_at, \
           from_session_id) \
           VALUES ($1, $2, $3, $4, $5, $6, $7, clock_timestamp() + make_interval(secs => $8), $9) RETURNING *) \
         SELECT {OFFER_COLUMNS} FROM i"
    )))
    .bind(id)
    .bind(game)
    .bind(kind)
    .bind(world)
    .bind(from_account)
    .bind(from_profile)
    .bind(to_account)
    .bind(OFFER_TTL_SECS as f64)
    .bind(from_session)
    .fetch_one(&mut *conn)
    .await?;
    let offer = offer_row(&row)?;
    Ok(offer)
}

/// Room for one more pending offer in a locked world: a seat to hold and fewer than
/// [`MAX_PENDING`] pending.
async fn room_for_offer_in(
    conn: &mut PgConnection,
    world: Uuid,
    limit_pending: bool,
) -> Result<(), MmError> {
    let (players, max, taken) = seats_in(conn, world).await?;
    if players + taken >= max {
        return Err(MmError::Conflict("full"));
    }
    if limit_pending && pending_count_in(conn, world).await? >= MAX_PENDING {
        return Err(MmError::Conflict("pending_limit"));
    }
    Ok(())
}

/// The caller's open world `world_id`, locked; `None` when unknown, closed or lapsed.
async fn lock_open_world_in(
    conn: &mut PgConnection,
    game: &str,
    world: Uuid,
) -> Result<Option<(Option<Uuid>, Option<Uuid>, String, String)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT host_profile_id, host_account_id, mode, \
           CASE WHEN join_mode IS NOT NULL THEN join_mode WHEN any_mode THEN 'any' ELSE mode END FROM world \
         WHERE id = $1 AND game = $2 AND closed_at IS NULL AND expires_at > clock_timestamp() FOR UPDATE",
    )
    .bind(world)
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?)
}

/// `POST /mm/v1/worlds/{id}/invites {xuid}`: the host (any ticket, OD-129/OD-141) invites a friend
/// account (OD-56) to its open world; the invite holds a seat for [`OFFER_TTL_SECS`].
/// [`MmError::NotFound`] for anyone but an accepted, open friend account.
pub async fn invite(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    target_xuid: i64,
) -> Result<Sent, MmError> {
    if let Hit::Limited { retry_after_secs } =
        rate_limit(db, &INVITE_LIMIT, caller.account_id).await?
    {
        return Err(MmError::RateLimited(retry_after_secs));
    }
    let mut tx = db.begin().await?;
    let host = match lock_open_world_in(&mut tx, &caller.game, world_id).await? {
        None => {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM world WHERE id = $1 AND game = $2 AND host_profile_id <> $3 \
                   AND closed_at IS NULL AND expires_at > clock_timestamp())",
            )
            .bind(world_id)
            .bind(&caller.game)
            .bind(caller.profile_id)
            .fetch_one(&mut *tx)
            .await?;
            return Err(if exists {
                MmError::NotHost
            } else {
                MmError::WorldNotFound
            });
        }
        Some((host, ..)) => host,
    };
    // CR-S96 (A3): a private official world's invites open to its current leader only, up to its
    // free seats (A6).
    let private_world: bool =
        sqlx::query_scalar("SELECT official_owner_account_id IS NOT NULL FROM world WHERE id = $1")
            .bind(world_id)
            .fetch_one(&mut *tx)
            .await?;
    if private_world {
        let leader = private_leader_in(&mut tx, world_id).await?;
        if leader.map(|(profile, _)| profile) != Some(caller.profile_id) {
            return Err(MmError::Conflict("not_leader"));
        }
    } else if host != Some(caller.profile_id) {
        return Err(MmError::NotHost);
    }
    let target: Option<Uuid> = sqlx::query_scalar("SELECT account_id FROM profile WHERE xuid = $1")
        .bind(target_xuid)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(target) = target.filter(|t| *t != caller.account_id) else {
        return Err(MmError::NotFound);
    };
    if !mutual_friends_in(&mut tx, caller.account_id, target).await?
        || !open_account_in(&mut tx, target).await?
    {
        return Err(MmError::NotFound);
    }
    // OD-149: an invite never crosses the pineapple pool: when the friend has profiles of this game
    // the world admits, one of them must be on the world's side of it (flagged for a pineapple
    // world, clean otherwise). Another mode alone is the join's refusal, as before.
    let same_side: bool = sqlx::query_scalar(
        "WITH c AS (SELECT w.pool, EXISTS (SELECT 1 FROM cheat_flag cf WHERE cf.profile_id = tp.id \
               AND cf.until > clock_timestamp() AND tp.mode = 'hardcore') AS flagged \
             FROM world w JOIN profile tp ON tp.account_id = $2 AND tp.game = w.game \
               AND (tp.mode = w.mode OR w.any_mode) AND (w.join_mode IS NULL OR tp.mode = w.join_mode) \
               AND tp.deleted_at IS NULL \
             WHERE w.id = $1) \
         SELECT NOT EXISTS (SELECT 1 FROM c) OR EXISTS (SELECT 1 FROM c WHERE c.pool = c.flagged)",
    )
    .bind(world_id)
    .bind(target)
    .fetch_one(&mut *tx)
    .await?;
    if !same_side {
        return Err(MmError::Conflict("pool"));
    }
    let to_handle = handle_in(&mut tx, target).await?;
    end_offers_in(&mut tx, OfferScope::LapsedOf(world_id), "expired").await?;
    if let Some(offer) =
        pending_offer_in(&mut tx, world_id, "invite", caller.account_id, target).await?
    {
        tx.commit().await?;
        return Ok(Sent {
            offer,
            to_handle,
            created: false,
        });
    }
    if let Hit::Limited { retry_after_secs } =
        limits::hit_in(&mut tx, &limit_key(&INVITED_LIMIT, target), &INVITED_LIMIT).await?
    {
        tx.commit().await?;
        return Err(MmError::RateLimited(retry_after_secs));
    }
    room_for_offer_in(&mut tx, world_id, !private_world).await?;
    let offer = insert_offer_in(
        &mut tx,
        "invite",
        &caller.game,
        world_id,
        caller.account_id,
        caller.profile_id,
        target,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(Sent {
        offer,
        to_handle,
        created: true,
    })
}

/// `POST /mm/v1/worlds/{id}/requests`: the caller (any ticket, OD-129/OD-141) asks the world's host to
/// let it in (OD-59), because a friend of its account hosts the world or is seated in it; the
/// request holds a seat for [`OFFER_TTL_SECS`] and goes to the host's account.
pub async fn ask(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    mods: &ModSet,
) -> Result<Sent, MmError> {
    ask_with_relay(db, caller, world_id, mods, None, &[]).await
}

/// [`ask`] with the asker's relay round trips (`RequestToJoin.relays`, OD-160): stored on the
/// request (trusted entries only), so its acceptance can seat the asker on a relay-born world's
/// relay instead of refusing it (`launcher_update_required`, CR-S57 review M1).
pub async fn ask_with_relay(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    mods: &ModSet,
    relays: Option<&RelayLatencies>,
    trusted: &[TrustedRelay],
) -> Result<Sent, MmError> {
    let relays_json = relays.map(|r| trusted_relays_json(r, trusted));
    if let Hit::Limited { retry_after_secs } =
        rate_limit(db, &INVITE_LIMIT, caller.account_id).await?
    {
        return Err(MmError::RateLimited(retry_after_secs));
    }
    let mut tx = db.begin().await?;
    let session: Option<(bool, bool)> =
        sqlx::query_as("SELECT dedicated, modded FROM play_session WHERE id = $1")
            .bind(caller.session_id)
            .fetch_optional(&mut *tx)
            .await?;
    if session.is_some_and(|(dedicated, _)| dedicated) {
        return Err(MmError::DedicatedHostOnly);
    }
    let mods_json = serde_json::to_value(&mods.entries).map_err(event_err)?;
    let Some((host_profile, host_account, mode, open_modes)) =
        lock_open_world_in(&mut tx, &caller.game, world_id).await?
    else {
        return Err(MmError::WorldNotFound);
    };
    if !admits(&mode, &open_modes, &caller.mode) {
        return Err(MmError::WorldNotFound);
    }
    // OD-188/189 (MATCHMAKING_API.md §2.9): no host answers a request to a public official world;
    // a private one's current leader does (CR-S102, OD-211, §2.10).
    let private_world: bool =
        sqlx::query_scalar("SELECT official_owner_account_id IS NOT NULL FROM world WHERE id = $1")
            .bind(world_id)
            .fetch_one(&mut *tx)
            .await?;
    let (host_profile, host_account) = match (host_profile, host_account) {
        (Some(p), Some(a)) => (p, a),
        _ if private_world => {
            let Some(leader) = private_leader_in(&mut tx, world_id).await? else {
                return Err(MmError::Conflict("not_leader"));
            };
            leader
        }
        _ => return Err(MmError::Conflict("official")),
    };
    if host_profile == caller.profile_id || (private_world && host_account == caller.account_id) {
        return Err(MmError::Conflict("own_world"));
    }
    if private_world
        && !session_has_feature_in(&mut tx, caller.session_id, FEATURE_PRIVATE_OFFICIAL).await?
    {
        return Err(MmError::Conflict("client_update"));
    }
    let w = world_in(&mut tx, &caller.viewer(None), world_id)
        .await?
        .ok_or(MmError::WorldNotFound)?;
    if w.barred {
        return Err(MmError::WorldNotFound);
    }
    // OD-149: a request never crosses the pineapple pool.
    if w.pool != w.viewer_flagged {
        return Err(MmError::Conflict("pool"));
    }
    // A private official world: a friend of the leader's account only (§2.10, as its invites).
    let friend_in: bool = if private_world {
        mutual_friends_in(&mut tx, host_account, caller.account_id).await?
    } else {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM account_friendship f WHERE f.account_id = $1 AND f.friend_id = $2) \
               OR EXISTS (SELECT 1 FROM mm_grant g JOIN account_friendship f ON f.account_id = $1 AND f.friend_id = g.account_id \
                    WHERE g.world_id = $3 AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())))",
        )
        .bind(caller.account_id)
        .bind(host_account)
        .bind(world_id)
        .fetch_one(&mut *tx)
        .await?
    };
    if !friend_in || host_account == caller.account_id {
        return Err(MmError::NotFound);
    }
    if !open_account_in(&mut tx, host_account).await? {
        return Err(MmError::Conflict("host_not_supporter"));
    }
    let seated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM mm_grant WHERE world_id = $1 AND profile_id = $2 \
           AND (state = 'used' OR (state = 'issued' AND expires_at > clock_timestamp())))",
    )
    .bind(world_id)
    .bind(caller.profile_id)
    .fetch_one(&mut *tx)
    .await?;
    if seated {
        return Err(MmError::Conflict("seated"));
    }
    let to_handle = handle_in(&mut tx, host_account).await?;
    end_offers_in(&mut tx, OfferScope::LapsedOf(world_id), "expired").await?;
    let pending = if private_world {
        // A private world's pending request follows its leader (§2.10): whoever it was addressed
        // to, asking again re-addresses it to the one leading now.
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {OFFER_COLUMNS} FROM world_invite i WHERE i.world_id = $1 AND i.kind = 'request' \
               AND i.from_account_id = $2 AND i.state = 'pending' AND i.expires_at > clock_timestamp() \
             ORDER BY i.created_at DESC LIMIT 1"
        )))
        .bind(world_id)
        .bind(caller.account_id)
        .fetch_optional(&mut *tx)
        .await?;
        row.as_ref().map(offer_row).transpose()?
    } else {
        pending_offer_in(
            &mut tx,
            world_id,
            "request",
            caller.account_id,
            host_account,
        )
        .await?
    };
    if let Some(mut offer) = pending {
        // The pending request now belongs to this session, which just passed the checks above
        // (its mod set declared again): an acceptance seats it with that set, never an earlier
        // session.
        sqlx::query(
            "UPDATE world_invite SET from_session_id = $2, from_profile_id = $3, relays = $4, mods = $5, \
               to_account_id = $6 WHERE id = $1",
        )
        .bind(offer.id)
        .bind(caller.session_id)
        .bind(caller.profile_id)
        .bind(&relays_json)
        .bind(&mods_json)
        .bind(host_account)
        .execute(&mut *tx)
        .await?;
        offer.to_account_id = host_account;
        tx.commit().await?;
        return Ok(Sent {
            offer,
            to_handle,
            created: false,
        });
    }
    if let Hit::Limited { retry_after_secs } = limits::hit_in(
        &mut tx,
        &limit_key(&INVITED_LIMIT, host_account),
        &INVITED_LIMIT,
    )
    .await?
    {
        tx.commit().await?;
        return Err(MmError::RateLimited(retry_after_secs));
    }
    // A private world's offers go up to its free seats (A6: no `pending_limit`), as its invites.
    room_for_offer_in(&mut tx, world_id, !private_world).await?;
    let offer = insert_offer_in(
        &mut tx,
        "request",
        &caller.game,
        world_id,
        caller.account_id,
        caller.profile_id,
        host_account,
        // The asking session: its acceptance seats exactly this session (OD-112 review fix).
        Some(caller.session_id),
    )
    .await?;
    // The asker's declared set (OD-181): its acceptance seats it with this set.
    sqlx::query("UPDATE world_invite SET relays = $2, mods = $3 WHERE id = $1")
        .bind(offer.id)
        .bind(&relays_json)
        .bind(&mods_json)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Sent {
        offer,
        to_handle,
        created: true,
    })
}

/// A request's asker as stored: profile, account, asking session, relay report, declared mods.
type AskerRow = (
    Uuid,
    Uuid,
    Option<Uuid>,
    Option<serde_json::Value>,
    serde_json::Value,
);

/// What accepting a request did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// The request (`accepted`, with its grant).
    pub offer: OfferRow,
    /// The host account's handle.
    pub to_handle: i64,
    /// The asker's seat.
    pub grant: GrantIssued,
}

/// `POST /mm/v1/worlds/{id}/requests/{requestId}/accept`: the host gives the asker's profile a
/// seat without the code (OD-59). Errors: [`MmError::NotFound`] (no such request of this world),
/// [`MmError::NotHost`] (not the current live hosting session and mode),
/// [`MmError::Conflict`] `ended` (no longer pending), `seated_elsewhere` (the asker holds a seat),
/// `not_allowed` (mode or the host's bans now refuse it), `full`.
pub async fn accept_request(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    request_id: Uuid,
    hold_secs: i64,
) -> Result<Accepted, MmError> {
    accept_request_with_relay(db, caller, world_id, request_id, hold_secs, &[], 0).await
}

/// [`accept_request`] with the server's trusted relays (CR-S57): a request carries no relay
/// report, so the asker's seat names no relay in a direct world, and a relay-born world refuses
/// it ([`MmError::Conflict`] `launcher_update_required`: the asker joins by code or invite with
/// its report instead).
pub async fn accept_request_with_relay(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    request_id: Uuid,
    hold_secs: i64,
    trusted: &[TrustedRelay],
    max_relayed: u32,
) -> Result<Accepted, MmError> {
    let asker: Option<AskerRow> = sqlx::query_as(
        "SELECT from_profile_id, from_account_id, from_session_id, relays, mods FROM world_invite \
         WHERE id = $1 AND world_id = $2 AND kind = 'request'",
    )
    .bind(request_id)
    .bind(world_id)
    .fetch_optional(db)
    .await?;
    let Some((asker_profile, asker_account, asked_from, asker_relays, asker_mods)) = asker else {
        return Err(MmError::NotFound);
    };
    // OD-181: the set the asker declared when it asked (validated then; stored as sent).
    let asker_mods = mod_set(Some(mods_from_json(asker_mods).as_slice()))?;
    // OD-160: the asker's report (2.0.1 launchers send it with the request), else none.
    let asker_relays = relays_from_json(asker_relays);
    let mut tx = db.begin().await?;
    // The seat holder's advisory lock, account/profile/session parents, then world: a join's order.
    lock_profile_in(&mut tx, asker_profile).await?;
    lock_holder_in(&mut tx, asker_account, asker_profile).await?;
    let asker_session: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT id, modded FROM play_session WHERE profile_id = $1 AND account_id = $2 \
           AND state = 'active' AND game_until > clock_timestamp() AND NOT dedicated FOR SHARE",
    )
    .bind(asker_profile)
    .bind(asker_account)
    .fetch_optional(&mut *tx)
    .await?;
    let (asker_session, asker_modded) = asker_session.ok_or(MmError::Conflict("not_allowed"))?;
    // OD-112 review fix: the request's checks (its mod set) were the asking session's; another
    // session of the asker (a relaunch, perhaps with other mods) is never seated by it.
    if asked_from != Some(asker_session) {
        return Err(MmError::Conflict("not_allowed"));
    }
    let now = now_in(&mut tx).await?;
    let host = match lock_open_world_in(&mut tx, &caller.game, world_id).await? {
        None => return Err(MmError::WorldNotFound),
        Some((host, ..)) => host,
    };
    // CR-S102 (§2.10): a private official world's requests are its current leader's to accept,
    // whoever led when they were asked.
    let private_world: bool =
        sqlx::query_scalar("SELECT official_owner_account_id IS NOT NULL FROM world WHERE id = $1")
            .bind(world_id)
            .fetch_one(&mut *tx)
            .await?;
    if private_world {
        let leader = private_leader_in(&mut tx, world_id).await?;
        if leader.map(|(profile, _)| profile) != Some(caller.profile_id) {
            return Err(MmError::Conflict("not_leader"));
        }
    } else if host != Some(caller.profile_id) {
        return Err(MmError::NotHost);
    }
    // Owning the profile alone is insufficient: a replacement session or mode cannot issue
    // credentials for the previous host's relay, even before its stale world is swept.
    // A private world's leader holds a live seat there ([`private_leader_in`]); it has no host session.
    let live_host: bool = if private_world {
        true
    } else {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM world w JOIN play_session s ON s.id = w.host_session_id \
               JOIN profile hp ON hp.id = w.host_profile_id \
               WHERE w.id = $1 AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
                 AND w.host_session_id = $2 AND w.mode = $3 AND hp.mode = w.mode AND hp.game = w.game \
                 AND s.profile_id = w.host_profile_id AND s.account_id = w.host_account_id \
                 AND s.state = 'active' AND s.game_until > clock_timestamp())",
        )
        .bind(world_id)
        .bind(caller.session_id)
        .bind(&caller.mode)
        .fetch_one(&mut *tx)
        .await?
    };
    if !live_host {
        return Err(MmError::NotHost);
    }
    let state: String = sqlx::query_scalar(
        "SELECT CASE WHEN state = 'pending' AND expires_at <= clock_timestamp() THEN 'expired' ELSE state END \
         FROM world_invite WHERE id = $1 FOR UPDATE",
    )
    .bind(request_id)
    .fetch_one(&mut *tx)
    .await?;
    if state != "pending" {
        if state == "expired" {
            end_offers_in(&mut tx, OfferScope::One(request_id), "expired").await?;
            tx.commit().await?;
        }
        return Err(MmError::Conflict("ended"));
    }
    let (game, xuid, mode): (String, i64, String) =
        sqlx::query_as("SELECT game, xuid, mode FROM profile WHERE id = $1")
            .bind(asker_profile)
            .fetch_one(&mut *tx)
            .await?;
    // The asker's tier now (it asked with a Supporter ticket; it may have lapsed since).
    let vip: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM entitlement e WHERE e.account_id = $1 AND e.product = 'vip' \
           AND e.status IN ('active', 'grace') AND e.access_until > clock_timestamp())",
    )
    .bind(asker_account)
    .fetch_one(&mut *tx)
    .await?;
    expire_own(&mut tx, &game, asker_profile, &mode, true, now).await?;
    if live_grant_of(&mut tx, &game, asker_profile)
        .await?
        .is_some()
    {
        return Err(MmError::Conflict("seated_elsewhere"));
    }
    let viewer = Viewer {
        account_id: asker_account,
        profile_id: asker_profile,
        xuid,
        ip: None,
    };
    let w = world_in(&mut tx, &viewer, world_id)
        .await?
        .ok_or(MmError::WorldNotFound)?;
    // OD-181, T-1110-NOMODMATCH: any declared set enters; only a session not admitted modded
    // never carries one (it could not have asked with one from this same session).
    if !w.admits(&mode) || w.barred || (!asker_modded && asker_mods.modded()) {
        return Err(MmError::Conflict("not_allowed"));
    }
    // §2.10: still a friend of the current leader (the caller), as an invitee at its join.
    if private_world && !mutual_friends_in(&mut tx, caller.account_id, asker_account).await? {
        return Err(MmError::Conflict("not_allowed"));
    }
    // OD-149: the asker's flag now, not when it asked.
    if w.pool != w.viewer_flagged {
        return Err(MmError::Conflict("pool"));
    }
    if i64::from(w.players) + w.reserved + w.held - w.own_held >= i64::from(w.max_players) {
        return Err(MmError::Conflict("full"));
    }
    let asker_live: bool = sqlx::query_scalar(
        "SELECT state = 'active' AND game_until > clock_timestamp() FROM play_session WHERE id = $1",
    )
    .bind(asker_session)
    .fetch_one(&mut *tx)
    .await?;
    if !asker_live {
        return Err(MmError::Conflict("not_allowed"));
    }
    // OD-189 (§2.9): an official world's seat is direct, always, as its join's.
    let pair = if w.host_vm_id.is_some() {
        GrantRelay::none()
    } else {
        let wr = world_relay_in(&mut tx, world_id).await?;
        let pair = grant_relay(
            &wr.transport,
            wr.key.as_deref(),
            wr.nat.as_deref(),
            wr.relays.as_ref(),
            &JoinRelay {
                region: None,
                nat: None,
                relays: asker_relays.as_ref(),
                trusted,
                max_relayed,
                relay_only: false,
            },
        )?;
        // CR-S57 review M4 for a relayed Noob seat (OD-160: never a Hardcore world's).
        if pair.transport == "relay" && !wr.hardcore {
            relay_room_in(&mut tx, world_id, None, asker_account, max_relayed).await?;
        }
        pair
    };
    let (id, issued, expires, hold, udp_proof_key) = insert_grant_in(
        &mut tx,
        &game,
        world_id,
        asker_profile,
        asker_account,
        xuid,
        if vip { "vip" } else { "free" },
        asker_session,
        hold_secs,
        // The set the asker declared (OD-181).
        &asker_mods,
        &pair,
    )
    .await?;
    // §2.10: an accepted request is an accepted invite (its rejoin, its linked session).
    if private_world {
        add_world_access_in(&mut tx, world_id, asker_account, "invite").await?;
    }
    end_offers_in(
        &mut tx,
        OfferScope::Seated {
            world: world_id,
            account: asker_account,
            grant: id,
        },
        "accepted",
    )
    .await?;
    let offer = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {OFFER_COLUMNS} FROM world_invite i WHERE i.id = $1"
    )))
    .bind(request_id)
    .fetch_one(&mut *tx)
    .await?;
    let offer = offer_row(&offer)?;
    let to_handle = handle_in(&mut tx, caller.account_id).await?;
    let asker_voice = crate::voice::blocked_until_in(&mut tx, asker_account).await?;
    tx.commit().await?;
    Ok(Accepted {
        offer,
        to_handle,
        grant: GrantIssued {
            id,
            world_id,
            host_xuid: if w.host_vm_id.is_some() {
                w.host_xuid
            } else {
                caller.xuid
            },
            host_vm_id: w.host_vm_id.clone(),
            issued_at: issued,
            expires_at: expires,
            hold_until: hold,
            udp_proof_key: Some(udp_proof_key),
            relay_key: pair.key,
            transport: pair.transport.to_owned(),
            voice_blocked_until: asker_voice,
            mods: asker_mods.entries.clone(),
        },
    })
}

/// The account that answers pending offer `i` of world `w` (SQL): its `to_account_id`, except a
/// request to a private official world, which its current leader answers ([`private_leader_row_in`]'s
/// rule; NULL while nobody leads), whoever it was addressed to (MATCHMAKING_API.md §2.10).
const OFFER_ANSWERER: &str = "CASE WHEN i.kind = 'request' AND w.official_owner_account_id IS NOT NULL THEN \
     (SELECT p.account_id FROM profile p WHERE p.game = w.game \
        AND (p.xuid = w.private_leader_xuid OR p.id = w.official_owner_profile_id) \
        AND EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id AND g.profile_id = p.id \
          AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) \
      ORDER BY (p.xuid IS NOT DISTINCT FROM w.private_leader_xuid) DESC LIMIT 1) \
   ELSE i.to_account_id END";

/// Declines the pending invite or request `offer` addressed to `account` (the invitee, the host,
/// or a private official world's current leader, §2.10): its seat is free at once. Whether it was
/// pending (idempotent).
pub async fn decline_in(
    conn: &mut PgConnection,
    account: Uuid,
    offer: Uuid,
) -> Result<bool, IdentityError> {
    let mine: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM world_invite i JOIN world w ON w.id = i.world_id WHERE i.id = $1 \
           AND ({OFFER_ANSWERER}) = $2 AND i.state = 'pending' AND i.expires_at > clock_timestamp())"
    )))
    .bind(offer)
    .bind(account)
    .fetch_one(&mut *conn)
    .await?;
    if !mine {
        return Ok(false);
    }
    Ok(end_offers_in(conn, OfferScope::One(offer), "declined").await? > 0)
}

/// A pending offer addressed to an account, with its sender's profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRow {
    /// The offer.
    pub offer: OfferRow,
    /// The sender's XUID.
    pub from_xuid: i64,
    /// Its gamertag.
    pub from_gamertag: String,
    /// Its mode.
    pub from_mode: String,
    /// The sender's account is a Supporter now.
    pub from_vip: bool,
    /// An invite to a private official world (CR-S96).
    pub private: bool,
    /// That world's linked game session: scid, template, name.
    pub session: Option<(String, String, String)>,
}

/// The most items of an inbox.
pub const INBOX_MAX: i64 = 20;

/// The pending invites and requests addressed to `account`, to open worlds, from a sender still
/// visible ([`crate::visibility::VISIBLE`]: its profile active, its account active and not banned
/// since it sent them), newest first, at most [`INBOX_MAX`] (CORE_API.md §4.2). One statement. A
/// request to a private official world is its current leader's ([`OFFER_ANSWERER`], §2.10): found
/// through the leader's live seat there.
pub async fn inbox_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Vec<InboxRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {OFFER_COLUMNS}, p.xuid AS from_xuid, \
           p.name AS from_gamertag, p.mode AS from_mode, \
           EXISTS (SELECT 1 FROM entitlement e WHERE e.account_id = i.from_account_id AND e.product = 'vip' \
             AND e.status IN ('active', 'grace') AND e.access_until > clock_timestamp()) AS from_vip, \
           w.official_owner_account_id IS NOT NULL AS private_world, fs.scid, fs.template, fs.name \
         FROM world_invite i JOIN profile p ON p.id = i.from_profile_id JOIN world w ON w.id = i.world_id \
           JOIN account a ON a.id = p.account_id \
           LEFT JOIN LATERAL (SELECT f.scid, f.template, f.name FROM freeroam_session f \
             WHERE f.private_world_id = w.id AND f.expires_at > clock_timestamp() \
             ORDER BY f.created_at LIMIT 1) fs ON true \
         WHERE (i.to_account_id = $1 OR i.world_id IN (SELECT g.world_id FROM mm_grant g WHERE g.account_id = $1 \
             AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())))) \
           AND ({OFFER_ANSWERER}) = $1 AND i.state = 'pending' AND i.expires_at > clock_timestamp() \
           AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() AND {visible} \
         ORDER BY i.created_at DESC LIMIT $2",
        visible = crate::visibility::VISIBLE,
    )))
    .bind(account)
    .bind(INBOX_MAX)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(InboxRow {
                offer: offer_row(r)?,
                from_xuid: r.try_get("from_xuid")?,
                from_gamertag: r
                    .try_get::<Option<String>, _>("from_gamertag")?
                    .unwrap_or_default(),
                from_mode: r.try_get("from_mode")?,
                from_vip: r.try_get("from_vip")?,
                private: r.try_get("private_world")?,
                session: match (
                    r.try_get::<Option<String>, _>("scid")?,
                    r.try_get::<Option<String>, _>("template")?,
                    r.try_get::<Option<String>, _>("name")?,
                ) {
                    (Some(scid), Some(template), Some(name)) => Some((scid, template, name)),
                    _ => None,
                },
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// An invite or request the caller sent, with the account it is addressed to now (CR-S106).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentRow {
    /// The offer (`state` reads `expired` for a pending one past `expires_at`).
    pub offer: OfferRow,
    /// The addressed account's handle (the friend address of `crate::friends`: its first
    /// profile's XUID, whatever that profile's state).
    pub to_handle: u64,
    /// The plain name of its profile in the world's game (the private world's leader profile,
    /// else one of the world's mode, else the oldest); empty when it has no live named one.
    pub to_gamertag: String,
}

/// The most items of the sent list.
pub const SENT_MAX: i64 = 20;

/// How long an ended offer stays in the sent list after it was made (CR-S106).
pub const SENT_WINDOW_SECS: i64 = 120;

/// The invites and requests `account` sent in the last [`SENT_WINDOW_SECS`] or still pending
/// (unexpired), whatever their state, newest first, at most [`SENT_MAX`] (CORE_API.md §4.2,
/// CR-S106). `to` is the account that answers now ([`OFFER_ANSWERER`]: a request to a private
/// official world is its current leader's), else the one it was addressed to (nobody leads now).
/// One statement on the caller's connection; `world_invite_sent (from_account_id, created_at)`
/// serves it (an offer lives at most 15 minutes, `world_invite_ttl`).
pub async fn sent_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Vec<SentRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {OFFER_COLUMNS}, h.xuid AS to_handle, tn.name AS to_gamertag \
         FROM world_invite i JOIN world w ON w.id = i.world_id \
           CROSS JOIN LATERAL (SELECT coalesce(({OFFER_ANSWERER}), i.to_account_id) AS id) t \
           CROSS JOIN LATERAL (SELECT h.xuid FROM profile h WHERE h.account_id = t.id \
             ORDER BY h.created_at, h.id LIMIT 1) h \
           LEFT JOIN LATERAL (SELECT tp.name FROM profile tp WHERE tp.account_id = t.id AND tp.game = w.game \
             AND tp.status = 'active' AND tp.name IS NOT NULL \
             ORDER BY (tp.xuid IS NOT DISTINCT FROM w.private_leader_xuid) DESC, (tp.mode = w.mode) DESC, \
               tp.created_at, tp.id LIMIT 1) tn ON true \
         WHERE i.from_account_id = $1 AND i.created_at > clock_timestamp() - interval '15 minutes' \
           AND (i.created_at > clock_timestamp() - make_interval(secs => $2) \
             OR (i.state = 'pending' AND i.expires_at > clock_timestamp())) \
         ORDER BY i.created_at DESC, i.id DESC LIMIT $3"
    )))
    .bind(account)
    .bind(SENT_WINDOW_SECS as f64)
    .bind(SENT_MAX)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(SentRow {
                offer: offer_row(r)?,
                to_handle: r.try_get::<i64, _>("to_handle")? as u64,
                to_gamertag: r
                    .try_get::<Option<String>, _>("to_gamertag")?
                    .unwrap_or_default(),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

// ---- Capacity and the sweep ---------------------------------------------------------------------

/// The public capacity of a game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityRow {
    /// Live free seats.
    pub online: i64,
    /// The configured free limit (informational since OD-58).
    pub limit: i32,
    /// The configured switch.
    pub queue_open: bool,
    /// Open free hosts' worlds.
    pub free_worlds: i64,
    /// Open Supporter hosts' worlds.
    pub supporter_worlds: i64,
    /// Database clock.
    pub at: UnixMs,
}

/// `GET /mm/v1/capacity?game=`; `None` for a game without a free limit row.
pub async fn capacity(db: &PgPool, game: &str) -> Result<Option<CapacityRow>, IdentityError> {
    let row = sqlx::query(
        "SELECT s.free_limit, s.queue_open, clock_timestamp() AS now, \
           (SELECT count(*) FROM mm_grant g JOIN world w ON w.id = g.world_id \
              WHERE g.game = s.game AND g.tier = 'free' AND g.state IN ('issued', 'used') \
              AND ((g.state = 'issued' AND g.expires_at > clock_timestamp()) \
                OR (g.state = 'used' AND w.closed_at IS NULL AND w.expires_at > clock_timestamp()))) AS online, \
           (SELECT count(*) FROM world w WHERE w.game = s.game AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
              AND w.kind = 'free') AS free_worlds, \
           (SELECT count(*) FROM world w WHERE w.game = s.game AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
              AND w.kind = 'supporter') AS supporter_worlds \
         FROM mm_free_slot s WHERE s.game = $1",
    )
    .bind(game)
    .fetch_optional(db)
    .await?;
    row.map(|r| {
        Ok(CapacityRow {
            online: r.try_get("online")?,
            limit: r.try_get("free_limit")?,
            queue_open: r.try_get("queue_open")?,
            free_worlds: r.try_get("free_worlds")?,
            supporter_worlds: r.try_get("supporter_worlds")?,
            at: ms(r.try_get("now")?),
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// What a sweep did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Issued grants past their time.
    pub grants_expired: u64,
    /// Worlds past their TTL.
    pub worlds_closed: u64,
    /// Invites and requests past their time.
    pub offers_expired: u64,
}

/// Expires seats, closes worlds past their TTL and expires invites and requests, in the caller's
/// transaction (`gate-sweep` runs it; also safe to run from any instance at any time).
pub async fn sweep_in(conn: &mut PgConnection) -> Result<Swept, IdentityError> {
    let now = now_in(conn).await?;
    let mut out = Swept::default();
    let worlds: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM world WHERE closed_at IS NULL AND expires_at <= clock_timestamp() ORDER BY expires_at LIMIT 500 FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *conn)
    .await?;
    for w in worlds {
        close_world_in(conn, w, "expired", now).await?;
        out.worlds_closed += 1;
    }
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT g.{} , w.game FROM mm_grant g JOIN world w ON w.id = g.world_id \
         WHERE g.state = 'issued' AND g.expires_at <= clock_timestamp() ORDER BY g.expires_at LIMIT 1000 FOR UPDATE OF g SKIP LOCKED",
        GRANT_COLUMNS.replace(", ", ", g.")
    )))
    .fetch_all(&mut *conn)
    .await?;
    for r in &rows {
        move_grant(conn, &grant_row(r)?, "expired", None).await?;
        out.grants_expired += 1;
    }
    out.offers_expired = end_offers_in(conn, OfferScope::Lapsed, "expired").await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, version: &str, fingerprint: &str) -> ModEntry {
        ModEntry {
            id: id.into(),
            version: version.into(),
            fingerprint: fingerprint.into(),
        }
    }

    #[test]
    fn an_empty_or_absent_set_is_vanilla() {
        assert_eq!(mod_set(None).unwrap(), ModSet::vanilla());
        assert_eq!(mod_set(Some(&[])).unwrap(), ModSet::vanilla());
        assert!(!ModSet::vanilla().modded());
        assert!(
            mod_set(Some(&[entry("a.mod", "1", &"a".repeat(64))]))
                .unwrap()
                .modded()
        );
    }

    #[test]
    fn mod_sets_are_validated_never_normalized() {
        let f = "a".repeat(64);
        let bad = |set: Vec<ModEntry>| {
            matches!(
                mod_set(Some(&set)),
                Err(MmError::Invalid { field: "mods", .. })
            )
        };
        // Order-dependent input is refused, not sorted; duplicates too.
        assert!(bad(vec![entry("b", "1", &f), entry("a", "1", &f)]));
        assert!(bad(vec![entry("a", "1", &f), entry("a", "2", &f)]));
        for id in ["", "A", "-a", ".a", "a b", "a/b", "é", &"a".repeat(65)] {
            assert!(bad(vec![entry(id, "1", &f)]), "{id}");
        }
        assert!(mod_set(Some(&[entry(&"a".repeat(64), "1", &f)])).is_ok());
        assert!(mod_set(Some(&[entry("0", "1", &f)])).is_ok());
        for version in ["", "\"", "a b", &"1".repeat(17)] {
            assert!(bad(vec![entry("a", version, &f)]), "{version}");
        }
        for fp in [
            "A".repeat(64),
            "a".repeat(63),
            "g".repeat(64),
            "a".repeat(65),
        ] {
            assert!(bad(vec![entry("a", "1", &fp)]), "{fp}");
        }
        let many: Vec<ModEntry> = (0..=MODS_MAX)
            .map(|i| entry(&format!("m{i:03}"), "1", &f))
            .collect();
        assert!(bad(many.clone()));
        assert!(mod_set(Some(&many[..MODS_MAX])).is_ok());
    }

    fn row() -> WorldRow {
        WorldRow {
            id: Uuid::from_u128(1),
            game: "fh2".into(),
            name: "n".into(),
            host_profile_id: Some(Uuid::from_u128(10)),
            host_account_id: Some(Uuid::from_u128(11)),
            host_vm_id: None,
            host_xuid: 1,
            host_gamertag: "H#00001".into(),
            host_tier: "vip".into(),
            kind: "supporter".into(),
            mode: "hardcore".into(),
            open_modes: "hardcore".into(),
            lobbies: Vec::new(),
            activity: "freeroam".into(),
            voice: "open".into(),
            host_away: false,
            players: 1,
            reported: 1,
            max_players: 4,
            dedicated: false,
            mods: Vec::new(),
            modded: false,
            members: None,
            message: None,
            races: true,
            opened_at: UnixMs::from_millis(1_000),
            audience: "everyone".into(),
            code: None,
            max_rtt_ms: None,
            geo_cell: None,
            region: None,
            private: false,
            open: true,
            reserved: 0,
            held: 0,
            own_held: 0,
            same_address: false,
            friend_host: false,
            friend_seated: false,
            invited: false,
            barred: false,
            transport: "direct".into(),
            relay_key: None,
            relay_name: None,
            pool: false,
            viewer_flagged: false,
        }
    }

    fn rules(vip: bool, mode: &str) -> Rules<'_> {
        Rules {
            profile_id: Uuid::from_u128(20),
            vip,
            mode: Some(mode),
            cell: None,
        }
    }

    #[test]
    fn why_is_decided_in_the_contract_order() {
        let r = rules(false, "hardcore");
        assert_eq!(assess(&row(), &r, false).why, None);
        let mut own = row();
        own.host_profile_id = Some(r.profile_id);
        assert_eq!(assess(&own, &r, false).why, Some(Why::Own));
        assert_eq!(
            assess(&row(), &rules(true, "noob"), false).why,
            Some(Why::Mode),
            "modes never meet"
        );
        // OD-141 (CR-S76): a world open to `any` admits both modes; the rest of the order holds.
        let mut mixed = row();
        mixed.open_modes = OPEN_MODES_ANY.into();
        assert_eq!(assess(&mixed, &rules(true, "noob"), false).why, None);
        assert_eq!(assess(&mixed, &r, false).why, None);
        mixed.barred = true;
        assert_eq!(
            assess(&mixed, &rules(true, "noob"), false).why,
            Some(Why::Host)
        );
        // OD-129: a `supporters` world admits every viewer; no world shows `why: "tier"`.
        let mut s = row();
        s.audience = "supporters".into();
        assert_eq!(assess(&s, &r, false).why, None, "a free viewer joins");
        s.barred = true;
        assert_eq!(assess(&s, &r, false).why, Some(Why::Host));
        assert_eq!(
            assess(&s, &rules(true, "hardcore"), false).why,
            Some(Why::Host)
        );
        let mut f = row();
        f.audience = "friends".into();
        let a = assess(&f, &rules(true, "hardcore"), false);
        assert_eq!((a.why, a.needs_code), (Some(Why::Friends), true));
        assert_eq!(
            assess(&f, &rules(true, "hardcore"), true).why,
            None,
            "the code"
        );
        f.friend_host = true;
        let a = assess(&f, &rules(true, "hardcore"), false);
        assert_eq!((a.why, a.needs_code), (None, false), "a friend");
        let mut p = row();
        p.audience = "private".into();
        assert_eq!(assess(&p, &r, false).why, Some(Why::Code));
        p.invited = true;
        let a = assess(&p, &r, false);
        assert_eq!((a.why, a.needs_code), (None, false), "an invite");
        let mut full = row();
        full.players = 2;
        full.reserved = 1;
        full.held = 1;
        let a = assess(&full, &r, false);
        assert_eq!((a.why, a.open_seats, a.full), (Some(Why::Full), 0, true));
        full.own_held = 1;
        let a = assess(&full, &r, false);
        assert_eq!((a.why, a.open_seats), (None, 1), "one's own held seat");
        let mut far = row();
        far.max_rtt_ms = Some(60);
        far.geo_cell = fi_domain::geo::cell(38.7, -9.1);
        let mut v = rules(false, "hardcore");
        v.cell = fi_domain::geo::cell(-23.5, -46.6);
        assert_eq!(assess(&far, &v, false).why, Some(Why::Latency));
        v.cell = None;
        assert_eq!(assess(&far, &v, false).why, None, "unknown is let in");
    }

    #[test]
    fn the_punches_sign_the_voice_block_of_the_joiners_account() {
        // CR-S67: the same expression as `crate::voice::blocked_until_sql`, over the grant's account.
        let expr = crate::voice::blocked_until_sql("g.account_id");
        let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            squash(PUNCHES_SQL).contains(&squash(&expr)),
            "{PUNCHES_SQL}"
        );
    }

    #[test]
    fn udp_proof_credentials_are_redacted_in_all_nested_debug_output() {
        let key = UdpProofKey::from_bytes([171; 32]);
        assert_eq!(key.as_bytes(), &[171; 32]);
        assert_eq!(format!("{key:?}"), "UdpProofKey([REDACTED])");
        let grant = GrantIssued {
            id: Uuid::nil(),
            world_id: Uuid::nil(),
            host_xuid: 1,
            host_vm_id: None,
            issued_at: UnixMs::from_millis(1),
            expires_at: UnixMs::from_millis(2),
            hold_until: UnixMs::from_millis(3),
            udp_proof_key: Some(key),
            relay_key: None,
            transport: "direct".into(),
            voice_blocked_until: None,
            mods: Vec::new(),
        };
        let text = format!("{:?}", JoinOutcome::Granted(grant));
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains("171"));
        assert!(!text.contains("q6ur"));
    }

    #[test]
    fn network_destinations_are_canonical_bounded_and_redacted() {
        let endpoint = Endpoint {
            ip: "::ffff:203.0.113.9".into(),
            cell: None,
            region: None,
            port: Some(62001),
            local_port: Some(5101),
            lan: Some(vec![
                "::ffff:192.168.1.2".into(),
                "192.168.1.2".into(),
                "fd00::2".into(),
            ]),
        };
        let canonical = canonical_network_endpoint(&endpoint).unwrap();
        assert_eq!(canonical.ip, "203.0.113.9");
        assert_eq!(canonical.lan, ["192.168.1.2", "fd00::2"]);
        let candidates = network_candidates(&canonical, "::ffff:203.0.113.9");
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].port, 62001);
        assert_eq!(candidates[1].port, 5101);
        assert_eq!(network_candidates(&canonical, "198.51.100.9").len(), 1);
        let debug = format!(
            "{:?}",
            NetworkHintsRow {
                revision: 1,
                candidates,
                generated_at: UnixMs::from_millis(1),
                expires_at: UnixMs::from_millis(2),
            }
        );
        assert!(debug.contains("[REDACTED]"));
        assert!(
            !debug.contains("203.0.113")
                && !debug.contains("192.168")
                && !debug.contains("fd00")
                && !debug.contains("62001")
        );
        for ip in [
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "ff02::1",
            "255.255.255.255",
            "::ffff:255.255.255.255",
        ] {
            assert!(
                canonical_network_endpoint(&Endpoint {
                    ip: ip.into(),
                    ..endpoint.clone()
                })
                .is_err()
            );
        }
        assert!(
            canonical_network_endpoint(&Endpoint {
                port: Some(0),
                ..endpoint.clone()
            })
            .is_err()
        );
        assert!(
            canonical_network_endpoint(&Endpoint {
                lan: Some(vec!["8.8.8.8".into()]),
                ..endpoint.clone()
            })
            .is_err()
        );
        assert!(
            canonical_network_endpoint(&Endpoint {
                lan: Some(vec!["192.168.1.2".into(); 5]),
                ..endpoint
            })
            .is_err()
        );
        assert_eq!((NETWORK_LIMIT.max, NETWORK_LIMIT.window_s), (12, 60));
    }

    #[test]
    fn the_selection_caps_and_orders_rows_and_locks_none() {
        let mut rows = Vec::new();
        for i in 0..9u128 {
            let mut w = row();
            w.id = Uuid::from_u128(100 + i);
            w.players = (i % 4) as i32 + 1;
            w.max_players = 12;
            w.opened_at = UnixMs::from_millis(i as i64);
            if i % 2 == 0 {
                w.audience = "supporters".into();
            }
            rows.push(w);
        }
        let assessed = |r: &Rules<'_>| rows.iter().map(|w| assess(w, r, false)).collect::<Vec<_>>();
        let (items, locked) = select(
            assessed(&rules(true, "hardcore")),
            Filter::Global,
            false,
            false,
        );
        assert_eq!((items.len(), locked.len()), (LIST_CAP, 0));
        let players: Vec<i32> = items.iter().map(|a| a.row.players).collect();
        assert!(players.windows(2).all(|p| p[0] >= p[1]), "{players:?}");
        // OD-129: a free viewer sees what a Supporter sees; nothing is locked.
        let (items, locked) = select(
            assessed(&rules(false, "hardcore")),
            Filter::Global,
            false,
            false,
        );
        assert_eq!((items.len(), locked.len()), (LIST_CAP, 0));
        assert!(items.iter().all(|a| a.why.is_none()));
        let (near, _) = select(
            assessed(&rules(true, "hardcore")),
            Filter::Near,
            false,
            false,
        );
        assert!(near.is_empty(), "unlocated: nothing is near");
        let (friends, _) = select(
            assessed(&rules(true, "hardcore")),
            Filter::Friends,
            false,
            false,
        );
        assert!(friends.is_empty());
        // CR-L40: `all` puts every visible world in `items`, `locked` empty, joinable ones first;
        // since OD-129 a free viewer's Supporter worlds are joinable (no `why: "tier"`).
        let (all, locked) = select(
            assessed(&rules(false, "hardcore")),
            Filter::All,
            false,
            false,
        );
        assert!(locked.is_empty());
        assert!(all.len() <= ALL_CAP && all.iter().all(|a| a.why != Some(Why::Tier)));
        assert_eq!(
            all.len(),
            rows.len(),
            "every world, the Supporters' included"
        );
        assert!(all.iter().all(|a| !a.row.barred));
        let first_blocked = all.iter().position(|a| !a.can_join()).unwrap_or(all.len());
        assert!(all[first_blocked..].iter().all(|a| !a.can_join()));
        assert_eq!(Filter::parse("global"), Some(Filter::Global));
        assert_eq!(Filter::parse("all"), Some(Filter::All));
        assert_eq!(Filter::parse("everything"), None);
    }

    #[test]
    fn codes_are_eight_uniform_digits() {
        for _ in 0..100 {
            let c = random_code().unwrap();
            assert!(
                c.len() == CODE_DIGITS && c.bytes().all(|b| b.is_ascii_digit()),
                "{c}"
            );
        }
        assert_ne!(random_code().unwrap(), random_code().unwrap());
        assert_ne!(
            limit_key(&CODE_LIMIT, Uuid::from_u128(1)),
            limit_key(&CODE_DAY_LIMIT, Uuid::from_u128(1))
        );
    }

    fn trusted(keys: &[&str]) -> Vec<TrustedRelay> {
        keys.iter()
            .map(|k| TrustedRelay {
                key: (*k).to_owned(),
                name: format!("relay {k}"),
            })
            .collect()
    }

    fn rtts(pairs: &[(&str, u32)]) -> RelayLatencies {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    /// CR-S57 (MATCHMAKING_API.md §5b): the pair's relay minimises the larger round trip among
    /// the trusted relays both sides reported; ties by the sum, then the list order.
    #[test]
    fn the_pair_relay_minimises_the_worse_side_among_trusted_relays_both_reported() {
        let t = trusted(&["sp", "mia", "fra"]);
        let host = rtts(&[("sp", 20), ("mia", 120), ("fra", 200)]);
        let joiner = rtts(&[("sp", 150), ("mia", 90), ("fra", 40)]);
        // max: sp 150, mia 120, fra 200 → mia.
        assert_eq!(
            choose_relay(Some(&host), Some(&joiner), &t).map(|r| r.key.as_str()),
            Some("mia")
        );
        // Only relays both reported: the joiner never measured mia or fra.
        let joiner_sp = rtts(&[("sp", 150)]);
        assert_eq!(
            choose_relay(Some(&host), Some(&joiner_sp), &t).map(|r| r.key.as_str()),
            Some("sp")
        );
        // Tie on the max → the smaller sum, then the list order.
        let a = rtts(&[("sp", 100), ("mia", 100)]);
        let b = rtts(&[("sp", 100), ("mia", 50)]);
        assert_eq!(
            choose_relay(Some(&a), Some(&b), &t).map(|r| r.key.as_str()),
            Some("mia")
        );
        let c = rtts(&[("sp", 100), ("mia", 100)]);
        assert_eq!(
            choose_relay(Some(&c), Some(&c), &t).map(|r| r.key.as_str()),
            Some("sp")
        );
        // No common relay, or no map at all: none.
        let only_fra = rtts(&[("fra", 10)]);
        assert_eq!(choose_relay(Some(&joiner_sp), Some(&only_fra), &t), None);
        assert_eq!(choose_relay(None, None, &t), None);
        assert_eq!(
            choose_relay(None, Some(&joiner), &t),
            None,
            "an older host (no map) never gets a relay claim"
        );
        // Both maps are required (review M1): an older joiner or host gets no relay claim.
        assert_eq!(choose_relay(Some(&host), None, &t), None);
        // A world's own relay: the host's lowest.
        assert_eq!(
            choose_relay(Some(&host), Some(&host), &t).map(|r| r.key.as_str()),
            Some("sp")
        );
    }

    #[test]
    fn an_untrusted_relay_is_never_chosen_however_fast() {
        let t = trusted(&["sp"]);
        let host = rtts(&[("evil", 1), ("sp", 300)]);
        let joiner = rtts(&[("evil", 1), ("sp", 280)]);
        assert_eq!(
            choose_relay(Some(&host), Some(&joiner), &t).map(|r| r.key.as_str()),
            Some("sp")
        );
        let only_evil = rtts(&[("evil", 1)]);
        assert_eq!(choose_relay(Some(&only_evil), Some(&only_evil), &t), None);
        assert_eq!(choose_relay(Some(&host), Some(&joiner), &[]), None);
        // Stored maps keep only trusted entries.
        assert_eq!(
            trusted_relays_json(&host, &t),
            serde_json::json!({"sp": 300})
        );
    }

    #[test]
    fn a_grant_is_relayed_when_its_world_is_or_a_symmetric_joiner_meets_a_restricted_host() {
        let t = trusted(&["sp", "mia"]);
        let host = rtts(&[("sp", 20), ("mia", 100)]);
        let joiner_rtts = rtts(&[("sp", 30), ("mia", 10)]);
        let joiner = |nat| JoinRelay {
            region: None,
            nat,
            relays: Some(&joiner_rtts),
            trusted: &t,
            max_relayed: 4,
            relay_only: false,
        };
        // OD-146: a joiner who always connects through the relay is relayed on a direct world
        // (the pair's relay), and refused rather than sent direct when no relay serves the pair.
        let only = JoinRelay {
            relay_only: true,
            ..joiner(Some("open"))
        };
        let g = grant_relay("direct", None, Some("open"), Some(&host), &only).unwrap();
        assert_eq!(g.transport, "relay");
        assert!(g.key.is_some());
        let none = JoinRelay {
            trusted: &[],
            ..only
        };
        assert!(matches!(
            grant_relay("direct", None, Some("open"), Some(&host), &none),
            Err(MmError::Conflict("no_relay"))
        ));
        // A relay-born world: its relay whatever the joiner measured, transport relay.
        let g = grant_relay(
            "relay",
            Some("mia"),
            Some("symmetric"),
            Some(&host),
            &joiner(None),
        )
        .unwrap();
        assert_eq!(
            g,
            GrantRelay {
                key: Some("mia".into()),
                transport: "relay"
            }
        );
        // ... unless the operator stopped trusting it: no relay, direct.
        let only_sp = trusted(&["sp"]);
        let g = grant_relay(
            "relay",
            Some("mia"),
            Some("symmetric"),
            Some(&host),
            &JoinRelay {
                region: None,
                nat: None,
                relays: Some(&joiner_rtts),
                trusted: &only_sp,
                max_relayed: 4,
                relay_only: false,
            },
        )
        .unwrap();
        assert_eq!(g, GrantRelay::none());
        // A direct world: the pair's choice as fallback, direct first.
        let g = grant_relay("direct", None, Some("open"), Some(&host), &joiner(None)).unwrap();
        assert_eq!(
            g,
            GrantRelay {
                key: Some("sp".into()),
                transport: "direct"
            }
        );
        // A symmetric joiner and a restricted host cannot punch: relayed at once.
        let g = grant_relay(
            "direct",
            None,
            Some("restricted"),
            Some(&host),
            &joiner(Some("symmetric")),
        )
        .unwrap();
        assert_eq!(g.transport, "relay");
        // A symmetric joiner and an open host: direct still works.
        let g = grant_relay(
            "direct",
            None,
            Some("open"),
            Some(&host),
            &joiner(Some("symmetric")),
        )
        .unwrap();
        assert_eq!(g.transport, "direct");
        // Nobody reported relays: no fallback.
        let g = grant_relay(
            "direct",
            None,
            Some("restricted"),
            None,
            &JoinRelay {
                region: None,
                nat: Some("symmetric"),
                relays: None,
                trusted: &t,
                max_relayed: 4,
                relay_only: false,
            },
        )
        .unwrap();
        assert_eq!(g, GrantRelay::none());
        // An older joiner (no map) of a relay-born world is refused, never handed a claim it
        // cannot verify (review M1).
        let refused = grant_relay(
            "relay",
            Some("mia"),
            Some("symmetric"),
            Some(&host),
            &JoinRelay {
                region: None,
                nat: None,
                relays: None,
                trusted: &t,
                max_relayed: 4,
                relay_only: false,
            },
        );
        assert!(matches!(
            refused,
            Err(MmError::Conflict("launcher_update_required"))
        ));
    }

    /// OD-160: who opens relay-born, on which relay, and when a Hardcore world opens direct.
    #[test]
    fn hardcore_worlds_open_on_an_available_relay_and_noob_worlds_keep_the_old_rule() {
        let t = trusted(&["sp", "mia"]);
        let host = rtts(&[("sp", 20), ("mia", 90)]);
        let report = |hardcore, nat, relay_only, family_mismatch, relays| OpenReport {
            hardcore,
            nat,
            relay_only,
            family_mismatch,
            relays,
        };
        let key = |r: Result<Option<&TrustedRelay>, MmError>| r.map(|o| o.map(|t| t.key.clone()));
        let none: &[String] = &[];
        // Hardcore: the host's lowest trusted relay, whatever it asked.
        for relay_only in [false, true] {
            assert_eq!(
                key(open_relay(
                    &report(true, None, relay_only, false, Some(&host)),
                    &t,
                    none
                ))
                .unwrap(),
                Some("sp".into())
            );
        }
        // Its lowest is down: the next available one.
        let sp_down = vec!["sp".to_owned()];
        assert_eq!(
            key(open_relay(
                &report(true, Some("open"), false, false, Some(&host)),
                &t,
                &sp_down
            ))
            .unwrap(),
            Some("mia".into())
        );
        // Every one down, no map (an older launcher), no trusted relay in it, none configured:
        // direct, never no_relay.
        let all_down = vec!["sp".to_owned(), "mia".to_owned()];
        let untrusted = rtts(&[("evil", 1)]);
        for (relays, trusted_now, down) in [
            (Some(&host), &t[..], &all_down[..]),
            (None, &t[..], none),
            (Some(&untrusted), &t[..], none),
            (Some(&host), &[][..], none),
        ] {
            assert_eq!(
                key(open_relay(
                    &report(true, Some("restricted"), true, false, relays),
                    trusted_now,
                    down
                ))
                .unwrap(),
                None
            );
        }
        // A Hardcore host that needs a relay keeps it (an available one first, else its lowest
        // trusted one), and no_relay without any.
        assert_eq!(
            key(open_relay(
                &report(true, Some("symmetric"), false, false, Some(&host)),
                &t,
                &all_down
            ))
            .unwrap(),
            Some("sp".into())
        );
        assert!(matches!(
            open_relay(&report(true, None, false, true, Some(&untrusted)), &t, none),
            Err(MmError::Conflict("no_relay"))
        ));
        // Noob: direct unless it needs or asks for a relay (OD-146), whatever is down.
        assert_eq!(
            key(open_relay(
                &report(false, Some("open"), false, false, Some(&host)),
                &t,
                none
            ))
            .unwrap(),
            None
        );
        assert_eq!(
            key(open_relay(
                &report(false, None, true, false, Some(&host)),
                &t,
                &sp_down
            ))
            .unwrap(),
            Some("mia".into())
        );
        assert!(matches!(
            open_relay(&report(false, None, true, false, None), &t, none),
            Err(MmError::Conflict("no_relay"))
        ));
    }
}
