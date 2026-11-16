//! The gate (ADR 0015, GATE_API.md; platform v1): admission per game, the game access ticket's
//! bookkeeping, liveness from game requests, manual leave, the batched sweeper and the public
//! queue summary, over the play sessions of [`crate::admission`].
//!
//! Every function runs in the caller's transaction or takes one connection at a time from the
//! pool, never two (services/AGENTS.md). Lock order is admission's: the pool's advisory lock, the
//! session row, the profile's save head, then the session rows and read models. The sweeper locks rows
//! only (`FOR UPDATE SKIP LOCKED`) and re-reads its predicate in the closing statement, so a
//! heartbeat, a game request or a leave that committed first always wins and a session ends once,
//! with one `session.ended`.
//!
//! - [`admit_in`]: `POST /gate/v1/sessions`: the game exists and is the profile's, nobody new is
//!   admitted during maintenance, then admission's transaction with the save lease for `vip`
//!   pools only, and there only for a Supporter's Hardcore profile (OD-52, [`crate::cloud`]; free
//!   sessions play on the game's [`MATCHMAKING_POOL`] with no save line, OD-24).
//! - [`poll_in`], [`heartbeat_in`], [`ticket_in`], [`leave_in`]: the session routes; a ticket is
//!   minted by the caller after commit, and its `kid` and `exp` recorded with
//!   [`record_ticket_in`] in the same transaction.
//! - [`touch_seen`]: the liveness signal saves and matchmaking record for authenticated game
//!   requests (batched, at most once per session per instance per minute).
//! - [`sweep`]: `fi-cli gate-sweep` (hourly; every 5 s locally).
//! - [`queue_summary`]: `GET /gate/v1/queue?game=`.
//! - [`rate_limit`]: the per-account limits of admission and ticket refresh ([`ADMIT_LIMIT`],
//!   [`TICKET_LIMIT`]; ADR 0903 §3), exact across instances.

use crate::admission::{
    self, AdmissionError, Admitted, CreateRequest, Device, Outcome, ProfileRef, Reaped,
    SaveLinePolicy, SlotTtl, Stale,
};
use crate::identity::{ms, ts};
use fi_domain::{PlaySessionId, PoolRouting, UnixMs};
use fi_saves::{FenceToken, SaveError};
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The pool free admissions of every game go to (tier `free`, no save lease; CR-L16).
pub const MATCHMAKING_POOL: &str = "matchmaking";
/// Rows one sweeper transaction closes at most.
pub const SWEEP_BATCH: i64 = 500;
/// What a queued launcher waits between polls (GATE_API.md §2: 10 to 30 s).
pub const RETRY_AFTER_SECS: u32 = 10;

/// Canonical queue polling: stay responsive near admission, bound idle work farther back.
/// The legacy admission API retains its own interval.
pub fn queue_retry_after_secs(position: u32, eta_seconds: Option<u32>) -> u32 {
    if position <= 5 || eta_seconds.is_some_and(|eta| eta <= 30) {
        RETRY_AFTER_SECS
    } else if position <= 20 {
        20
    } else {
        30
    }
}
/// The admissions window queue ETAs are estimated from.
pub const ETA_WINDOW_MS: i64 = 15 * 60_000;
/// Most session ids one [`touch_seen`] call records.
pub const TOUCH_BATCH: usize = 1_000;
/// Admissions (`POST /gate/v1/sessions`, replays included) per account: 6 per minute (ADR 0903
/// §3). Each one takes the pool's advisory lock, so one account cannot serialize a pool.
pub const ADMIT_LIMIT: crate::limits::Limit = crate::limits::Limit {
    name: "gate.admit",
    max: 6,
    window_s: 60,
};
/// Ticket refreshes without a heartbeat (`POST .../ticket`) per account: 6 per minute (the
/// launcher refreshes with its heartbeat every 60 s; this route is its fallback).
pub const TICKET_LIMIT: crate::limits::Limit = crate::limits::Limit {
    name: "gate.ticket",
    max: 6,
    window_s: 60,
};

/// Counts one request of `account` against `limit` (its own statement, one connection; the key
/// is the limit's name and a SHA-256 of the account, never the id in clear).
pub async fn rate_limit(
    db: &PgPool,
    limit: &crate::limits::Limit,
    account: Uuid,
) -> Result<crate::limits::Hit, crate::identity::IdentityError> {
    crate::limits::hit(db, &rate_limit_key(limit, account), limit).await
}

/// The `rate_limit` key of `account` under `limit`.
pub fn rate_limit_key(limit: &crate::limits::Limit, account: Uuid) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("gate:{account}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{hex}", limit.name)
}

/// Why the gate refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// Admission's refusals and failures.
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    /// No such game (`REQUEST.INVALID {field: game}`).
    #[error("unknown game")]
    UnknownGame,
    /// The XUID is a profile of another game (`REQUEST.INVALID {field: game}`).
    #[error("the profile belongs to another game")]
    WrongGame,
    /// The game is in maintenance until `until` (`SERVICE.MAINTENANCE`).
    #[error("the game is in maintenance")]
    Maintenance {
        /// Core's switch.
        until: UnixMs,
        /// The database clock of the refusal.
        now: UnixMs,
    },
    /// A heartbeat of a session with a save line carried no fencing token
    /// (`REQUEST.INVALID {field: fenceToken}`).
    #[error("a fenceToken is required for a session with a save line")]
    FenceRequired,
}

impl From<sqlx::Error> for GateError {
    fn from(e: sqlx::Error) -> Self {
        GateError::Admission(e.into())
    }
}

/// A game row as admission reads it, by the database clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameGate {
    /// `maintenance_until` while it is in the future.
    pub maintenance_until: Option<UnixMs>,
    /// The database clock.
    pub now: UnixMs,
}

/// The game's maintenance state; `None` for an unknown game.
pub async fn game_gate_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Option<GameGate>, AdmissionError> {
    let row = sqlx::query(
        "SELECT CASE WHEN maintenance_until > clock_timestamp() THEN maintenance_until END AS until, \
         clock_timestamp() AS now FROM game WHERE code = $1",
    )
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(GameGate {
            maintenance_until: r.try_get::<Option<OffsetDateTime>, _>("until")?.map(ms),
            now: ms(r.try_get("now")?),
        })
    })
    .transpose()
}

/// What `POST /gate/v1/sessions` asks for.
#[derive(Clone, Debug)]
pub struct AdmitRequest<'a> {
    /// Minted by the launcher (also the `Idempotency-Key`).
    pub session_id: PlaySessionId,
    /// The signed-in account.
    pub account_id: Uuid,
    /// The game the body names.
    pub game: &'a str,
    /// The account's profile with the body's XUID.
    pub profile: &'a ProfileRef,
    /// The launcher install.
    pub device: &'a Device,
    /// Which pool serves each tier for this game (`vip` and [`MATCHMAKING_POOL`]).
    pub routing: &'a PoolRouting,
    /// Launcher version.
    pub launcher_version: &'a str,
    /// The handoff nonce (32 lower-case hex digits).
    pub nonce: &'a str,
    /// SHA-256 of the car database, when declared.
    pub car_db: Option<[u8; 32]>,
    /// Revoke a session open on another PC.
    pub take_over: bool,
    /// The slot's length per pool tier: the lease (`FI_LEASE_TTL_MS`) on `vip`, the free
    /// liveness window (`FI_FREE_LIVENESS_MS`, OD-40) on free pools.
    pub ttl: SlotTtl,
    /// [`SaveLinePolicy::VipPools`] on `/gate/v1`; [`SaveLinePolicy::Always`] on the
    /// `/launcher/v1/sessions` aliases, whose body always carries a lease (OD-52: refused for a
    /// profile without a save line).
    pub save_line: SaveLinePolicy,
    /// A dedicated lobby's host session (OD-109; `/gate/v1` only).
    pub dedicated: bool,
    /// A modded session (OD-112 follow-up; `/gate/v1` only): self-declared, stored as
    /// `play_session.modded`.
    pub modded: bool,
}

/// `POST /gate/v1/sessions` (GATE_API.md §2, ADR 0015 §2) and its `/launcher/v1` alias. Idempotent by the session id: a
/// replay answers as [`poll_in`] does (during maintenance too, without admitting).
pub async fn admit_in(
    conn: &mut PgConnection,
    req: &AdmitRequest<'_>,
) -> Result<Outcome, GateError> {
    let gate = game_gate_in(conn, req.game)
        .await?
        .ok_or(GateError::UnknownGame)?;
    if req.profile.game != req.game {
        return Err(GateError::WrongGame);
    }
    let existing: Option<(Uuid, Uuid, bool, bool)> = sqlx::query_as(
        "SELECT account_id, profile_id, dedicated, modded FROM play_session WHERE id = $1",
    )
    .bind(req.session_id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((account, profile, dedicated, modded)) = existing {
        if account != req.account_id
            || profile != req.profile.profile_id.as_uuid()
            || dedicated != req.dedicated
            || modded != req.modded
        {
            return Err(AdmissionError::KeyReused.into());
        }
        let admit = gate.maintenance_until.is_none();
        return Ok(
            admission::poll_with_in(conn, req.session_id, req.account_id, req.ttl, admit).await?,
        );
    }
    // OD-112 follow-up: Hardcore allows no multiplayer mod, so a Hardcore profile is never
    // admitted modded (read in the admission's transaction; the only mode change is to noob).
    if req.modded {
        let hardcore: bool =
            sqlx::query_scalar("SELECT mode = 'hardcore' FROM profile WHERE id = $1")
                .bind(req.profile.profile_id.as_uuid())
                .fetch_one(&mut *conn)
                .await?;
        if hardcore {
            return Err(AdmissionError::ModdedHardcore.into());
        }
    }
    if let Some(until) = gate.maintenance_until {
        return Err(GateError::Maintenance {
            until,
            now: gate.now,
        });
    }
    let outcome = admission::create_in(
        conn,
        &CreateRequest {
            session_id: req.session_id,
            account_id: req.account_id,
            profile_id: req.profile.profile_id,
            device: req.device,
            routing: req.routing,
            launcher_version: req.launcher_version,
            nonce: req.nonce,
            car_db: req.car_db,
            take_over: req.take_over,
            ttl: req.ttl,
            game: req.game,
            save_line: req.save_line,
            dedicated: req.dedicated,
            modded: req.modded,
        },
    )
    .await?;
    Ok(outcome)
}

/// Stores what a client declares it can do (MATCHMAKING_API.md §2.10): only names the server knows,
/// at most [`fi_protocol::gate::MAX_SESSION_FEATURES`] looked at; unknown ones are ignored.
pub async fn record_features_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    features: &[String],
) -> Result<(), crate::identity::IdentityError> {
    let known: Vec<&str> = features
        .iter()
        .take(fi_protocol::gate::MAX_SESSION_FEATURES)
        .map(String::as_str)
        .filter(|f| *f == fi_protocol::gate::FEATURE_PRIVATE_OFFICIAL)
        .collect();
    sqlx::query("UPDATE play_session SET features = $2 WHERE id = $1")
        .bind(session.as_uuid())
        .bind(&known)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `GET /gate/v1/sessions/{id}`: the poll; during the game's maintenance a queued session keeps
/// its place and is not admitted.
pub async fn poll_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    ttl: SlotTtl,
) -> Result<Outcome, GateError> {
    let in_maintenance: Option<bool> = sqlx::query_scalar(
        "SELECT COALESCE(g.maintenance_until > clock_timestamp(), false) FROM play_session s \
         JOIN game g ON g.code = s.game WHERE s.id = $1 AND s.account_id = $2",
    )
    .bind(session.as_uuid())
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    let in_maintenance = in_maintenance.ok_or(AdmissionError::NotFound)?;
    Ok(admission::poll_with_in(conn, session, account_id, ttl, !in_maintenance).await?)
}

/// `POST /gate/v1/sessions/{id}/heartbeat` (GATE_API.md §4): renews the slot (and the lease, with
/// the fencing token, for a session with a save line), resumes a lapse when nobody took the line
/// or the queue. A free session's slot then lasts the free liveness window (OD-40). Maintenance
/// does not stop live sessions.
pub async fn heartbeat_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    fence: Option<FenceToken>,
    ttl: SlotTtl,
) -> Result<Admitted, GateError> {
    match admission::heartbeat_any_in(conn, session, account_id, fence, ttl).await {
        Err(AdmissionError::Save(SaveError::InvalidArgument("fenceToken"))) => {
            Err(GateError::FenceRequired)
        }
        other => Ok(other?),
    }
}

/// `POST /gate/v1/sessions/{id}/ticket`: the session for a fresh ticket, without renewing the
/// lease or the slot.
pub async fn ticket_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
) -> Result<Admitted, GateError> {
    Ok(admission::token_in(conn, session, account_id).await?)
}

/// Records the ticket just decided for `session` (`play_session.ticket_kid`, `ticket_exp`): the
/// table shows every player's current ticket without storing it (ADR 0015 §1). Only an active
/// session gets one: `SAVES.LEASE_LOST` when it was closed meanwhile (the callers hold its row, so
/// that means a bug, not a race).
pub async fn record_ticket_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    kid: &str,
    expires_at: UnixMs,
) -> Result<(), AdmissionError> {
    let recorded = sqlx::query(
        "UPDATE play_session SET ticket_kid = $2, ticket_exp = $3 WHERE id = $1 AND state = 'active'",
    )
    .bind(session.as_uuid())
    .bind(kid)
    .bind(ts(expires_at))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if recorded == 0 {
        return Err(SaveError::LeaseLost { current: None }.into());
    }
    Ok(())
}

/// Where a queued session waits, as its 202 body shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePlace {
    /// The game.
    pub game: String,
    /// The pool's name.
    pub pool: String,
    /// The pool's tier.
    pub tier: String,
    /// The pool's endpoint.
    pub server: String,
    /// [`eta_in`] for its position.
    pub eta_seconds: Option<u32>,
}

/// The pool and ETA of queued `session` at `position`, in the caller's transaction.
pub async fn queue_place_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    position: u32,
) -> Result<QueuePlace, AdmissionError> {
    let (game, pool, tier, server): (String, String, String, String) = sqlx::query_as(
        "SELECT s.game, s.pool, p.tier, p.game_endpoint FROM play_session s JOIN pool p ON p.name = s.pool WHERE s.id = $1",
    )
    .bind(session.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AdmissionError::NotFound)?;
    let eta_seconds = eta_in(conn, &game, &pool, position).await?;
    Ok(QueuePlace {
        game,
        pool,
        tier,
        server,
        eta_seconds,
    })
}

/// `POST /gate/v1/sessions/{id}/leave` (GATE_API.md §5): frees the slot and the lease at once and
/// writes `session.ended`; idempotent (a closed session answers `Ok`). Without `fence` the lease
/// the session still holds is released by its stored token.
pub async fn leave_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    account_id: Uuid,
    reason: &str,
    end_revision: Option<u64>,
    fence: Option<FenceToken>,
) -> Result<(), GateError> {
    Ok(admission::end_any_in(conn, session, account_id, fence, reason, end_revision).await?)
}

/// Records an authenticated game request of each active session in `sessions` as its liveness
/// (`last_seen_at`, ADR 0015 §3): one statement, rows another transaction holds are skipped (that
/// transaction is a heartbeat, itself a signal, or the sweeper). Never renews the save lease and
/// mints nothing. At most [`TOUCH_BATCH`] ids; returns how many rows were touched.
///
/// For a free session this is how a game request extends its slot (OD-40, GATE_API.md §4): the
/// slot lapses only when `expires_at` and `last_seen_at + lease_ttl_ms` (the free window the
/// decision recorded) have both passed ([`Stale`]), the same effect as moving `expires_at` to now
/// + the window, without a second column write on this hot path.
pub async fn touch_seen(db: &PgPool, sessions: &[Uuid]) -> Result<u64, AdmissionError> {
    let mut conn = db.acquire().await?;
    touch_seen_in(&mut conn, sessions).await
}

/// [`touch_seen`] on the caller's connection.
pub async fn touch_seen_in(
    conn: &mut PgConnection,
    sessions: &[Uuid],
) -> Result<u64, AdmissionError> {
    if sessions.is_empty() {
        return Ok(0);
    }
    if sessions.len() > TOUCH_BATCH {
        return Err(AdmissionError::Event(format!(
            "touch_seen takes at most {TOUCH_BATCH} sessions"
        )));
    }
    Ok(sqlx::query(
        "UPDATE play_session SET last_seen_at = clock_timestamp() WHERE id IN \
         (SELECT id FROM play_session WHERE id = ANY($1) AND state = 'active' FOR UPDATE SKIP LOCKED)",
    )
    .bind(sessions)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// One sweeper pass (`fi-cli gate-sweep`, ADR 0015 §4): every stale session of every game and
/// pool ([`admission::Reaped`]: `max_age`, silent past the 60-minute ceiling, lapsed on both
/// signals, unpolled queue), in transactions of at most [`SWEEP_BATCH`] rows locked with
/// `FOR UPDATE SKIP LOCKED`, each closed by a statement that re-reads its predicate.
pub async fn sweep(db: &PgPool) -> Result<Reaped, AdmissionError> {
    let mut total = Reaped::default();
    for stale in Stale::ALL {
        let select = format!(
            "SELECT s.id FROM play_session s WHERE {} ORDER BY s.expires_at, s.id LIMIT {SWEEP_BATCH} \
             FOR UPDATE OF s SKIP LOCKED",
            stale.predicate()
        );
        // A batch shorter than SWEEP_BATCH means nothing unlocked is left; rows skipped as locked
        // belong to a transaction that is deciding them now, or to the next pass.
        for _ in 0..10_000 {
            let mut tx = db.begin().await?;
            let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(select.as_str()))
                .fetch_all(&mut *tx)
                .await?;
            let mut batch = Reaped::default();
            for id in &ids {
                if admission::close_if_stale_in(&mut tx, *id, stale).await? {
                    batch.count(stale);
                }
            }
            tx.commit().await?;
            total.add(batch);
            if (ids.len() as i64) < SWEEP_BATCH {
                break;
            }
        }
    }
    Ok(total)
}

/// One pool in the public summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolQueue {
    /// Pool name (`vip`, `matchmaking`, ...).
    pub name: String,
    /// `vip` | `free`.
    pub tier: String,
    /// Live active sessions.
    pub active: u32,
    /// Queued sessions polled within 90 s.
    pub queued: u32,
    /// Admission capacity (`None`: unlimited).
    pub capacity: Option<u32>,
    /// Whether a launch now may be admitted or queued (the pool accepts and the game is not in
    /// maintenance).
    pub accepting: bool,
    /// Estimated wait of a launch now: 0 with a free slot and nobody queued, else from the last
    /// 15 minutes of admissions; `None` without admissions to estimate from.
    pub eta_seconds: Option<u32>,
}

/// A game's online capacity and its live sessions over every pool (OD-71).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OnlineLoad {
    /// `game.online_capacity` (`None`: unlimited).
    pub capacity: Option<u32>,
    /// Live active sessions of the game.
    pub active: u32,
    /// Queued sessions polled within 90 s.
    pub queued: u32,
}

/// `GET /gate/v1/queue?game=` (no identities).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSummary {
    /// The game.
    pub game: String,
    /// Its pools, by name.
    pub pools: Vec<PoolQueue>,
    /// The game's online capacity and totals.
    pub online: OnlineLoad,
    /// `maintenance_until` while in the future.
    pub maintenance_until: Option<UnixMs>,
    /// The database clock.
    pub now: UnixMs,
}

/// The estimated wait of queue position `position` (1 = next) on `pool`: position x the mean
/// time between the last 15 minutes' admissions. `None` without admissions in the window.
pub async fn eta_in(
    conn: &mut PgConnection,
    game: &str,
    pool: &str,
    position: u32,
) -> Result<Option<u32>, AdmissionError> {
    let admitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM play_session WHERE game = $1 AND pool = $2 AND NOT dedicated \
         AND admitted_at > clock_timestamp() - make_interval(secs => $3)",
    )
    .bind(game)
    .bind(pool)
    .bind((ETA_WINDOW_MS / 1000) as f64)
    .fetch_one(&mut *conn)
    .await?;
    Ok(eta(admitted, position))
}

fn eta(admitted_in_window: i64, position: u32) -> Option<u32> {
    if admitted_in_window <= 0 {
        return None;
    }
    let window = ETA_WINDOW_MS / 1000;
    let total = i64::from(position) * window;
    let secs = (total + admitted_in_window - 1) / admitted_in_window;
    Some(u32::try_from(secs).unwrap_or(u32::MAX))
}

/// The public queue summary of `game`; `None` for an unknown game. One connection, four reads.
pub async fn queue_summary(
    db: &PgPool,
    game: &str,
) -> Result<Option<QueueSummary>, AdmissionError> {
    let mut conn = db.acquire().await?;
    let Some(gate) = game_gate_in(&mut conn, game).await? else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT p.name, p.tier, p.capacity, p.accepting, \
         count(s.id) FILTER (WHERE {}) AS active, \
         count(s.id) FILTER (WHERE s.state = 'queued' AND s.expires_at > clock_timestamp()) AS queued, \
         (SELECT count(*) FROM play_session a WHERE a.game = p.game AND a.pool = p.name AND NOT a.dedicated \
            AND a.admitted_at > clock_timestamp() - make_interval(secs => $2)) AS admitted \
         FROM pool p LEFT JOIN play_session s ON s.game = p.game AND s.pool = p.name AND s.state IN ('active', 'queued') \
         WHERE p.game = $1 GROUP BY p.name ORDER BY p.name",
        Stale::counted_active()
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind((ETA_WINDOW_MS / 1000) as f64)
        .fetch_all(&mut *conn)
        .await?;
    // The game's online capacity (OD-71) bounds every pool's "free now": a full game has no free
    // slot whatever a pool's own room (OD-129: no tier is served first).
    let online_capacity = online_capacity_in(&mut conn, game).await?.flatten();
    let mut counted = Vec::with_capacity(rows.len());
    for r in &rows {
        let tier: String = r.try_get("tier")?;
        counted.push((
            tier,
            r.try_get::<i64, _>("active")?,
            r.try_get::<i64, _>("queued")?,
        ));
    }
    let game_active: i64 = counted.iter().map(|(_, a, _)| a).sum();
    // OD-228: so does the global online capacity across every game (counted only when set).
    let global_room = match global_online_capacity_in(&mut conn).await? {
        None => true,
        Some(c) => c > global_online_load_in(&mut conn).await?.active,
    };
    let game_room = online_capacity.is_none_or(|c| i64::from(c) - game_active > 0) && global_room;
    let mut pools = Vec::with_capacity(rows.len());
    for (r, (tier, active, queued)) in rows.into_iter().zip(counted) {
        let capacity = r.try_get::<Option<i32>, _>("capacity")?.map(i64::from);
        let free_now = capacity.is_none_or(|c| active < c) && queued == 0 && game_room;
        pools.push(PoolQueue {
            name: r.try_get("name")?,
            tier,
            active: active as u32,
            queued: queued as u32,
            capacity: capacity.map(|c| c as u32),
            accepting: r.try_get::<bool, _>("accepting")? && gate.maintenance_until.is_none(),
            eta_seconds: if free_now {
                Some(0)
            } else {
                eta(r.try_get("admitted")?, queued as u32 + 1)
            },
        });
    }
    let online = OnlineLoad {
        capacity: online_capacity,
        active: pools.iter().map(|p| p.active).sum(),
        queued: pools.iter().map(|p| p.queued).sum(),
    };
    Ok(Some(QueueSummary {
        game: game.to_owned(),
        pools,
        online,
        maintenance_until: gate.maintenance_until,
        now: gate.now,
    }))
}

/// The online capacity of `game` (OD-71): `None` for an unknown game, `Some(None)` unlimited.
pub async fn online_capacity_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Option<Option<u32>>, AdmissionError> {
    let row: Option<Option<i32>> =
        sqlx::query_scalar("SELECT online_capacity FROM game WHERE code = $1")
            .bind(game)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row.map(|c| c.map(|c| c as u32)))
}

/// Sets the online capacity of `game` (`None`: unlimited; 1 to 100,000, the column's CHECK);
/// returns the previous value, or `None` for an unknown game. Takes effect on the next admission
/// decision; lowering it ends no session.
pub async fn set_online_capacity_in(
    conn: &mut PgConnection,
    game: &str,
    capacity: Option<u32>,
) -> Result<Option<Option<u32>>, AdmissionError> {
    let previous: Option<Option<i32>> = sqlx::query_scalar(
        "UPDATE game g SET online_capacity = $2 FROM (SELECT online_capacity FROM game WHERE code = $1 FOR UPDATE) old \
         WHERE g.code = $1 RETURNING old.online_capacity",
    )
    .bind(game)
    .bind(capacity.map(|c| i32::try_from(c).unwrap_or(i32::MAX)))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(previous.map(|c| c.map(|c| c as u32)))
}

/// The global online capacity across every game (OD-228, `global_online_capacity`): `None`
/// unlimited.
pub async fn global_online_capacity_in(
    conn: &mut PgConnection,
) -> Result<Option<u32>, AdmissionError> {
    let row: Option<Option<i32>> =
        sqlx::query_scalar("SELECT capacity FROM global_online_capacity WHERE singleton")
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row.flatten().map(|c| c as u32))
}

/// Sets the global online capacity (`None`: unlimited; 1 to 100,000, the column's CHECK) and
/// returns the previous value. Takes effect on the next admission decision of any game; lowering
/// it ends no session. Idempotent: setting the current value changes nothing.
pub async fn set_global_online_capacity_in(
    conn: &mut PgConnection,
    capacity: Option<u32>,
) -> Result<Option<u32>, AdmissionError> {
    let previous: Option<Option<i32>> = sqlx::query_scalar(
        "UPDATE global_online_capacity g SET capacity = $1          FROM (SELECT capacity FROM global_online_capacity WHERE singleton FOR UPDATE) old          WHERE g.singleton RETURNING old.capacity",
    )
    .bind(capacity.map(|c| i32::try_from(c).unwrap_or(i32::MAX)))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(previous.flatten().map(|c| c as u32))
}

/// The global online load now (OD-228): the global capacity and the live active and queued
/// sessions of every game (dedicated ones are never counted).
pub async fn global_online_load_in(conn: &mut PgConnection) -> Result<OnlineLoad, AdmissionError> {
    let capacity = global_online_capacity_in(conn).await?;
    let sql = format!(
        "SELECT count(*) FILTER (WHERE {}) AS active, \
           count(*) FILTER (WHERE s.state = 'queued' AND s.expires_at > clock_timestamp()) AS queued \
         FROM play_session s WHERE s.state IN ('active', 'queued') AND NOT s.dedicated",
        Stale::counted_active()
    );
    let r = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(&mut *conn)
        .await?;
    Ok(OnlineLoad {
        capacity,
        active: r.try_get::<i64, _>("active")? as u32,
        queued: r.try_get::<i64, _>("queued")? as u32,
    })
}

/// The online load of `game` now (core's admin read; the public summary computes the same).
pub async fn online_load_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Option<OnlineLoad>, AdmissionError> {
    let Some(capacity) = online_capacity_in(conn, game).await? else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT count(*) FILTER (WHERE {}) AS active, \
           count(*) FILTER (WHERE s.state = 'queued' AND s.expires_at > clock_timestamp()) AS queued \
         FROM play_session s WHERE s.game = $1 AND s.state IN ('active', 'queued')",
        Stale::counted_active()
    );
    let r = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .fetch_one(&mut *conn)
        .await?;
    Ok(Some(OnlineLoad {
        capacity,
        active: r.try_get::<i64, _>("active")? as u32,
        queued: r.try_get::<i64, _>("queued")? as u32,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_polling_is_bounded_and_responsive_near_admission() {
        for position in [0, 1, 5] {
            assert_eq!(queue_retry_after_secs(position, None), 10);
            assert_eq!(queue_retry_after_secs(position, Some(900)), 10);
        }
        for position in [6, 20] {
            assert_eq!(queue_retry_after_secs(position, None), 20);
            assert_eq!(queue_retry_after_secs(position, Some(31)), 20);
        }
        for position in [21, u32::MAX] {
            assert_eq!(queue_retry_after_secs(position, None), 30);
            assert_eq!(queue_retry_after_secs(position, Some(u32::MAX)), 30);
        }
        for position in [6, 20, 21, u32::MAX] {
            for eta in [0, 1, 30] {
                assert_eq!(queue_retry_after_secs(position, Some(eta)), 10);
            }
        }
    }

    #[test]
    fn etas_follow_the_admission_rate() {
        assert_eq!(eta(0, 3), None);
        // 30 admissions in 15 minutes = one every 30 s: position 3 waits 90 s.
        assert_eq!(eta(30, 3), Some(90));
        assert_eq!(eta(7, 1), Some(129));
        assert_eq!(eta(1, u32::MAX), Some(u32::MAX));
    }
}
