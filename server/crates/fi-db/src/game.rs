//! The game plane's platform surface storage (docs/contracts/GAME_PLATFORM.md §2), built by
//! workstream A of the local environment v0 (docs/server/LOCAL_ENV.md §9):
//!
//! - [`liveness`]: the session's state for the token check (`play_session` state, `game_until`,
//!   pool), read by the database clock. v0 reads it on every request; the contract allows a
//!   per-instance cache of at most 60 s (ADR 0008 §4);
//! - [`record_progression_in`]: a `progression_event` insert idempotent by `event_id` (the caller
//!   derives it: `UUIDv5(profile_id, X-Fh2-Request-Id)` on `/game/v1`, `UUIDv5(session, "req:" +
//!   Class.Method + ":" + X-Fh2-Request-Id)` on the proxied requests), with a race fact's
//!   `profile_progress` recount in the same transaction; [`Recorded::recorded`] tells a caller whose write must happen once whether
//!   this copy is the first;
//! - [`presence_in`]: the `game_presence` upsert with the deployment's pool and instance
//!   (migration `20261018000100_platform_local_v0.sql`; `game` is the profile's since
//!   `20261019000300_universe.sql`);
//! - [`touch_seen`]: the batched presence write of saves and matchmaking: one `game_presence`
//!   upsert for many sessions (which instance served them last), `last_seen_at` by the database
//!   clock. The liveness signal the sweeper reads is `play_session.last_seen_at`, written in the
//!   same flush by `fi_db::gate::touch_seen` (ADR 0015 §3; review 2026-10-18).

use crate::identity::{IdentityError, ms};
use fi_domain::UnixMs;
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The progression kinds (the former EVENTS.md §3.3) plus `ghost` (the game's
/// `Scoreboard.UploadFile`, GAME_PLATFORM.md §3a) and `club` (the Car Clubs' writes, CR-S70; the
/// `progression_event.kind` CHECK lists the same), and `creation` (a gated creation refused,
/// OD-178). `/game/v1` accepts the first seven only.
pub const KINDS: [&str; 10] = [
    "race_result",
    "score",
    "xp",
    "profile_sync",
    "tables_sync",
    "drivatar",
    "livery",
    "ghost",
    "club",
    "creation",
];

/// A session as the game plane checks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Liveness {
    /// The session.
    pub session_id: Uuid,
    /// Its account.
    pub account_id: Uuid,
    /// Its profile.
    pub profile_id: Uuid,
    /// The pool it was admitted to.
    pub pool: String,
    /// `play_session.state`.
    pub state: String,
    /// Until when the game plane serves it.
    pub game_until: Option<UnixMs>,
    /// Whether the session keeps cloud data now (OD-52, CR-S51): it holds the profile's save line
    /// (`play_session.save_line`: granted by admission to a profile the server's `FI_CLOUD_SAVES`
    /// lists, OD-52's Supporter Hardcore by default; dropped by the heartbeat that finds the
    /// profile no longer listed). The game plane serves the cloud features only when this and the
    /// ticket's `line` claim both say so, so a claim minted earlier drifts at most a heartbeat and
    /// the liveness cache's TTL. (A mode change waits for the profile's sessions to end, so a live
    /// line never changes mode.)
    pub cloud: bool,
    /// A dedicated lobby's host session (OD-109): the game plane's content and `cdb` checks do
    /// not apply to it (it runs no game; its ticket's `cdb` is empty).
    pub dedicated: bool,
    /// A modded session (OD-112 follow-up, GATE_API.md §2 "Modded sessions"): boards and the
    /// universe record none of its results.
    pub modded: bool,
    /// Whether the game plane's content and `cdb` checks apply (OD-113, CONTENT_API.md §3; CR-S51):
    /// a session with a save line whose profile is `hardcore` now, neither dedicated nor modded.
    pub integrity: bool,
    /// The database clock of the read.
    pub now: UnixMs,
}

impl Liveness {
    /// Whether the game plane of `pool` may serve the session now: active, in that pool, before
    /// `game_until`.
    pub fn serves(&self, pool: &str) -> bool {
        self.state == "active" && self.pool == pool && self.game_until.is_some_and(|u| self.now < u)
    }
}

/// The session's liveness, or `None` when it does not exist.
pub async fn liveness(db: &PgPool, session: Uuid) -> Result<Option<Liveness>, IdentityError> {
    read_liveness(db, session, false).await
}

/// Own-seat cleanup preserves natural session expiry under an online
/// restriction. Callers MUST still authenticate the ticket, pool, game and account/profile.
pub async fn liveness_for_cleanup(
    db: &PgPool,
    session: Uuid,
) -> Result<Option<Liveness>, IdentityError> {
    read_liveness(db, session, true).await
}

async fn read_liveness(
    db: &PgPool,
    session: Uuid,
    cleanup: bool,
) -> Result<Option<Liveness>, IdentityError> {
    let row = sqlx::query(
        "SELECT s.id, s.account_id, s.profile_id, s.pool, s.state, \
           CASE WHEN NOT $2 AND EXISTS(SELECT 1 FROM crash_restriction f WHERE f.account_id=s.account_id AND f.released_at IS NULL AND f.until_at>clock_timestamp()) THEN least(s.game_until,clock_timestamp()) ELSE s.game_until END AS game_until, \
           s.save_line AS cloud, s.dedicated, s.modded, \
           NOT s.dedicated AND NOT s.modded AND s.save_line AND p.mode = 'hardcore' AS integrity, \
           clock_timestamp() AS now \
         FROM play_session s JOIN profile p ON p.id = s.profile_id \
         WHERE s.id = $1",
    )
    .bind(session)
    .bind(cleanup)
    .fetch_optional(db)
    .await?;
    row.map(|r| {
        Ok(Liveness {
            session_id: r.try_get("id")?,
            account_id: r.try_get("account_id")?,
            profile_id: r.try_get("profile_id")?,
            pool: r.try_get("pool")?,
            state: r.try_get("state")?,
            game_until: r
                .try_get::<Option<OffsetDateTime>, _>("game_until")?
                .map(ms),
            cloud: r.try_get("cloud")?,
            dedicated: r.try_get("dedicated")?,
            modded: r.try_get("modded")?,
            integrity: r.try_get("integrity")?,
            now: ms(r.try_get("now")?),
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// One progression fact from the game plane.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    /// Derived from the request (`UUIDv5(profile_id, X-Fh2-Request-Id)` on `/game/v1`; GAME_PLATFORM.md
    /// §3a for the proxied requests): the same request is the same fact.
    pub event_id: Uuid,
    /// The session's account.
    pub account_id: Uuid,
    /// The session's profile.
    pub profile_id: Uuid,
    /// The session it happened in.
    pub session_id: Option<Uuid>,
    /// One of [`KINDS`].
    pub kind: String,
    /// Kind-specific facts (EVENTS.md §3.3).
    pub data: serde_json::Map<String, serde_json::Value>,
}

/// What [`record_progression_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The fact's id.
    pub event_id: Uuid,
    /// `false` when the fact already existed (a retry): nothing was written.
    pub recorded: bool,
    /// When the first copy was received.
    pub recorded_at: UnixMs,
}

/// Why a fact was not recorded.
#[derive(Debug, thiserror::Error)]
pub enum FactError {
    /// The event id already names another profile's fact (the caller's derivation guarantees this
    /// never happens for a v5 id; refused rather than answered as a replay).
    #[error("the event id belongs to another fact")]
    Conflict,
    /// Not one of [`KINDS`].
    #[error("unknown progression kind")]
    Kind,
    /// A number of the fact is out of range (`field`, `reason`): see [`check_data`].
    #[error("{0}: {1}")]
    Data(&'static str, &'static str),
    /// Database failure.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for FactError {
    fn from(e: sqlx::Error) -> Self {
        FactError::Db(e.into())
    }
}

/// The largest magnitude a number of a fact may have: JavaScript's safe integers, so every
/// aggregate over facts stays far inside `bigint`.
pub const MAX_FACT_NUMBER: f64 = 9_007_199_254_740_991.0;

/// Refuses a fact whose numbers would poison the read models (CR-S55 review: the aggregates over
/// `progression_event` run in every session close): every number of the fact object (the kind's
/// `data`, or its `data.data` when the whole payload was sent) is at most [`MAX_FACT_NUMBER`] in
/// magnitude, and an `xp` fact's `amount` is a number in `0..=`[`fi_gamedb::gates::XP_PER_REPORT`]
/// (the game's per-report bound). Refused as `REQUEST.INVALID` by the routes.
pub fn check_data(
    kind: &str,
    data: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), FactError> {
    let fact = match data.get("data") {
        Some(serde_json::Value::Object(inner)) => inner,
        _ => data,
    };
    fn numbers_ok(v: &serde_json::Value, depth: u8) -> bool {
        match v {
            serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f.abs() <= MAX_FACT_NUMBER),
            serde_json::Value::Array(a) if depth < 8 => a.iter().all(|v| numbers_ok(v, depth + 1)),
            serde_json::Value::Object(o) if depth < 8 => {
                o.values().all(|v| numbers_ok(v, depth + 1))
            }
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => false,
            _ => true,
        }
    }
    if !data.values().all(|v| numbers_ok(v, 0)) {
        return Err(FactError::Data("data", "a number out of range"));
    }
    if kind == "xp"
        && let Some(amount) = fact.get("amount")
        && !amount
            .as_f64()
            .is_some_and(|a| (0.0..=fi_gamedb::gates::XP_PER_REPORT).contains(&a))
    {
        return Err(FactError::Data(
            "data.amount",
            "an XP amount within the per-report bound",
        ));
    }
    Ok(())
}

/// Records `fact` (and recounts the profile's races for a race fact) in the caller's
/// transaction;
/// a replay (same `event_id`) writes nothing and answers the first copy's time.
pub async fn record_progression_in(
    conn: &mut PgConnection,
    fact: &Fact,
) -> Result<Recorded, FactError> {
    if !KINDS.contains(&fact.kind.as_str()) {
        return Err(FactError::Kind);
    }
    check_data(&fact.kind, &fact.data)?;
    let inserted: Option<OffsetDateTime> = sqlx::query_scalar(
        "INSERT INTO progression_event (event_id, profile_id, play_session_id, kind, payload, occurred_at) \
         VALUES ($1, $2, $3, $4, $5, clock_timestamp()) ON CONFLICT (event_id) DO NOTHING RETURNING received_at",
    )
    .bind(fact.event_id)
    .bind(fact.profile_id)
    .bind(fact.session_id)
    .bind(&fact.kind)
    .bind(sqlx::types::Json(&fact.data))
    .fetch_optional(&mut *conn)
    .await?;
    let Some(received) = inserted else {
        let row = sqlx::query(
            "SELECT profile_id, received_at FROM progression_event WHERE event_id = $1",
        )
        .bind(fact.event_id)
        .fetch_one(&mut *conn)
        .await?;
        if row.try_get::<Uuid, _>("profile_id")? != fact.profile_id {
            return Err(FactError::Conflict);
        }
        return Ok(Recorded {
            event_id: fact.event_id,
            recorded: false,
            recorded_at: ms(row.try_get("received_at")?),
        });
    };
    let received = ms(received);
    // `profile_progress.races` counts finished races (a race-board `score`, a `race_result`;
    // `fi_db::progression::race_fact`): recounted synchronously (no outbox, owner 2026-10-26); the
    // other facts change no column of it.
    if crate::progression::is_race_fact(&fact.kind, &fact.data) {
        crate::progression::refresh_profile_guarded(conn, fact.profile_id).await?;
    }
    Ok(Recorded {
        event_id: fact.event_id,
        recorded: true,
        recorded_at: received,
    })
}

/// Whether `profile_id` already has a refused `score` fact with rule `refusal` on the board key
/// (`board` `"<scoreboardType>:<scoreType>"`, `eventKey`, `trackId`): a stats board's ceiling
/// refusal is recorded once per profile and board (OD-178 follow-up).
pub async fn refused_score_recorded_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    refusal: &str,
    board: &str,
    event_key: i64,
    track_id: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM progression_event \
           WHERE profile_id = $1 AND kind = 'score' AND payload->>'refusal' = $2 \
             AND payload->>'board' = $3 AND payload->>'eventKey' = $4::text \
             AND payload->>'trackId' = $5::text)",
    )
    .bind(profile_id)
    .bind(refusal)
    .bind(board)
    .bind(event_key)
    .bind(track_id)
    .fetch_one(&mut *conn)
    .await
}

/// Upserts the session's presence on this deployment's `pool` and `instance` (database clock).
pub async fn presence_in(
    conn: &mut PgConnection,
    session: Uuid,
    profile: Uuid,
    pool: &str,
    instance: &str,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO game_presence (session_id, profile_id, pool, instance, game) \
         SELECT $1, $2, $3, $4, p.game FROM profile p WHERE p.id = $2 \
         ON CONFLICT (session_id) DO UPDATE SET instance = EXCLUDED.instance, last_seen_at = clock_timestamp()",
    )
    .bind(session)
    .bind(profile)
    .bind(pool)
    .bind(instance)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One session seen by this instance since its last flush.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The session.
    pub session_id: Uuid,
    /// Its profile.
    pub profile_id: Uuid,
    /// Its pool.
    pub pool: String,
}

/// Records `last_seen_at = now` for every session of `seen` in one statement, as this
/// deployment's `instance` (the profile's game). Returns the rows written.
pub async fn touch_seen(db: &PgPool, seen: &[Seen], instance: &str) -> Result<u64, IdentityError> {
    if seen.is_empty() {
        return Ok(0);
    }
    let sessions: Vec<Uuid> = seen.iter().map(|s| s.session_id).collect();
    let profiles: Vec<Uuid> = seen.iter().map(|s| s.profile_id).collect();
    let pools: Vec<String> = seen.iter().map(|s| s.pool.clone()).collect();
    let done = sqlx::query(
        "INSERT INTO game_presence (session_id, profile_id, pool, instance, game) \
         SELECT s.session_id, s.profile_id, s.pool, $4, p.game \
         FROM UNNEST($1::uuid[], $2::uuid[], $3::text[]) AS s (session_id, profile_id, pool) \
         JOIN profile p ON p.id = s.profile_id \
         ON CONFLICT (session_id) DO UPDATE SET instance = EXCLUDED.instance, last_seen_at = clock_timestamp()",
    )
    .bind(&sessions)
    .bind(&profiles)
    .bind(&pools)
    .bind(instance)
    .execute(db)
    .await?;
    Ok(done.rows_affected())
}
