//! Peer observations and the cheater pool (OD-147, MATCHMAKING_API.md §3.9, register CR-S79;
//! migration `20261030200000_peer_observations.sql`).
//!
//! In a Hardcore world (`mode = 'hardcore'`, not open to both modes) each player's hook measures
//! the other players' cars and reports what it saw ([`observe`]). The hook punishes nothing; the
//! server stores the observation and, in the same transaction, checks the quorum:
//!
//! - **what counts** ([`counts`]): `speed` over the observed car's limit ([`speed_limit_kmh`]:
//!   its upgraded top-speed bound plus 10%, NR-5) and `climb` over the rules' limit, for the rules'
//!   time,
//!   `car` with an ordinal on [`BLOCKED_CARS`] (the traffic cars SAURON mapped, V9; any other
//!   ordinal is no proof: the base catalogue leaves out car-pack cars), `parts`;
//!   `jump` and `clock` never count (calibration first);
//! - **the quorum**, per world, observed profile and counting kind over the last
//!   [`QUORUM_WINDOW_SECS`]: at least [`QUORUM_MIN`] distinct reporting **accounts** (an account's
//!   profiles count once; the observed one's own account never) and more than half of the world's
//!   other members (host and seated, minus the observed one) now. One player never flags another
//!   alone: a two-player world never flags;
//! - **the flag** ([`pool_hours`]): a quorum about the world's host flags it at once (its
//!   car data is its own); a quorum about anyone else flags only when quorums about that profile
//!   were reached under two different hosts (accounts) within [`HOSTS_WINDOW_HOURS`] (a host
//!   relays every joiner's view of the others and could forge it). Either way the counting
//!   observations about the profile in that window must come from at least
//!   [`FLAG_MIN_REPORTERS`] distinct accounts, so two colluding accounts can flag nobody. A flag
//!   lasts [`CHEAT_POOL_BASE_HOURS`], doubling with each new flag that starts within
//!   [`CHEAT_POOL_REPEAT_DAYS`] of the previous one's end, at most [`CHEAT_POOL_MAX_HOURS`]. A new
//!   quorum during a flag moves its end, never shortens it; a flag that had ended starts afresh
//!   (new kinds, new first moment, the next episode).
//! - **lifting** ([`unflag_in`], CORE_API.md §5 `DELETE /core/v1/admin/cheat-flags/{profileId}`,
//!   `fi-cli cheat-unflag`): staff end a live flag now and mark it `lifted`; the next flag then
//!   repeats the lifted one's episode instead of doubling on it (a lift forgives that flag only; the
//!   30-day window then runs from the lift, for the earlier episodes too). Nothing moves: the profile leaves the pool at its next
//!   list or join, and a pineapple world it hosts stays one for its life.
//! - **the pineapple pool** (OD-149): in the flag's transaction, the profile's live seat in a clean
//!   world ends (`pool`) and a clean world it hosts closes (`pool`)
//!   ([`crate::matchmaking::pool_in`]); while flagged, matchmaking offers it only pineapple worlds.
//!
//! Rows carry ids, never a gamertag, an address or a payload beyond the observation's fields.
//! Concurrent counting observations about one profile serialise on an advisory lock keyed by that
//! profile, so two reporters landing together still see each other; they also lock the observed
//! profile's row (`FOR NO KEY UPDATE`, before any row of theirs), so a flag and that profile's own
//! join or hosting never interleave: whichever commits second sees the other.

use crate::identity::{IdentityError, ms};
use crate::limits::Limit;
use crate::matchmaking::Caller;
use fi_domain::UnixMs;
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The kinds an observation may carry (§3.9).
pub const KINDS: [&str; 6] = fi_protocol::matchmaking::OBSERVATION_KINDS;
/// Observations per play session (`mm.observe`, 20 per minute; [`crate::matchmaking::rate_limit`]
/// hashes whichever id it is given).
pub const OBSERVE_LIMIT: Limit = Limit {
    name: "mm.observe",
    max: 20,
    window_s: 60,
};
/// The kinds that may count toward a quorum (`jump` and `clock` are stored only).
pub const COUNTING_KINDS: [&str; 4] = ["speed", "climb", "car", "parts"];
/// Distinct reporters a quorum needs at least.
pub const QUORUM_MIN: i64 = 2;
/// The quorum's window.
pub const QUORUM_WINDOW_SECS: i64 = 600;
/// Distinct accounts whose counting observations about a profile a flag needs, within
/// [`HOSTS_WINDOW_HOURS`] (more than any two colluders).
pub const FLAG_MIN_REPORTERS: i64 = 3;
/// Car ordinals no player may drive: the 17 traffic cars SAURON mapped (V9, 2026-10-31), one list
/// with the creation gate's ([`fi_gamedb::gates::BLOCKED_CARS`]). A `car` observation of any other
/// ordinal is stored and never counts (1215, a placeholder, included).
pub use fi_gamedb::gates::BLOCKED_CARS;
/// A non-host is flagged on quorums under two different hosts within this window.
pub const HOSTS_WINDOW_HOURS: i64 = 24;
/// How long a first flag lasts after the quorum that set it (OD-149: 2 hours, not 24).
pub const CHEAT_POOL_BASE_HOURS: i64 = 2;
/// The longest a flag lasts, however often it repeats.
pub const CHEAT_POOL_MAX_HOURS: i64 = 48;
/// A new flag that starts within this many days of the previous one's end doubles its length; the
/// flag's row is kept that long after it ends (fi_db::retention).
pub const CHEAT_POOL_REPEAT_DAYS: i32 = 30;

/// The length of a flag's `episode` (1 = first, or the first after a clean
/// [`CHEAT_POOL_REPEAT_DAYS`]): 2, 4, 8, 16, 32 hours, then [`CHEAT_POOL_MAX_HOURS`].
pub fn pool_hours(episode: i32) -> i64 {
    let doublings = u32::try_from(episode.saturating_sub(1))
        .unwrap_or(0)
        .min(16);
    (CHEAT_POOL_BASE_HOURS << doublings).min(CHEAT_POOL_MAX_HOURS)
}
/// Days an observation is kept (fi_db::retention).
pub const OBSERVATION_DAYS: i32 = 30;

/// One version of the thresholds the hook measures against (`WatchRules` on the wire).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rules {
    /// `WatchRules.version`.
    pub version: i64,
    /// Speed over this, km/h …
    pub speed_kmh: f64,
    /// … for at least this long.
    pub speed_ms: i64,
    /// Upward speed over this, km/h …
    pub climb_kmh: f64,
    /// … for at least this long.
    pub climb_ms: i64,
    /// One sample-to-sample move past this, metres (log only).
    pub jump_m: f64,
}

/// Every version, oldest first; the last is current. Version 1 (owner, OD-163, 2026-10-31):
/// thresholds taken from other projects as reference, 451 km/h and 150 km/h for 3 s, jumps of 200 m (log only); it
/// reverses OD-158's more conservative amendment, edited in place before any client sent an
/// observation.
pub const RULES: &[Rules] = &[Rules {
    version: 1,
    speed_kmh: 451.0,
    speed_ms: 3000,
    climb_kmh: 150.0,
    climb_ms: 3000,
    jump_m: 200.0,
}];

/// The per-car speed limit's margin over a car's upgraded top-speed bound, in tenths (110% = the
/// bound × 1.10; OD-163, NR-5). The margin and the table belong to the `WatchRules` version, but
/// [`Rules`] does not carry them yet: changing either needs a new version that also selects its
/// table, or the previous version's observations would be judged with the new one.
pub const PER_CAR_MARGIN_TENTHS: u32 = 11;
/// A car ordinal that names no real car (the catalogue's placeholder): treated as unknown.
pub const PLACEHOLDER_CAR: i64 = fi_gamedb::gates::PLACEHOLDER_CAR;

/// The speed limit of car `car` under `rules` (MATCHMAKING_API.md §3.9 "The per-car speed limit",
/// NR-5): its bound from [`fi_gamedb::topspeed`] × 1.10 for a `high` or `medium` row, the larger of
/// that and the flat `rules.speed_kmh` for a `low` row, and the flat limit for an absent car, the
/// placeholder or an ordinal the table does not list (a car-pack car). It replaces the flat limit
/// for that car, below it or above it. The relay detector's reports (§5d) are judged with it too
/// ([`crate::relay_reports::judge`]).
pub fn speed_limit_kmh(rules: &Rules, car: Option<i64>) -> f64 {
    use fi_gamedb::topspeed::{Confidence, TopSpeeds};
    let Some(row) = car
        .filter(|c| *c != PLACEHOLDER_CAR)
        .and_then(|c| TopSpeeds::fh2().get(c))
    else {
        return rules.speed_kmh;
    };
    let own = f64::from(row.max_kmh * PER_CAR_MARGIN_TENTHS) / 10.0;
    match row.confidence {
        Confidence::High | Confidence::Medium => own,
        Confidence::Low => own.max(rules.speed_kmh),
    }
}

/// `WatchRules.speed.perCarFloorKmh`: the lowest [`speed_limit_kmh`] any player's car can have under
/// `rules` (the flat limit at most), so a hook that cannot know the observed car's limit reports
/// every speed episode that could count. The traffic cars ([`BLOCKED_CARS`]: driving one counts as
/// `car` already) and the placeholder are left out.
pub fn per_car_floor_kmh(rules: &Rules) -> f64 {
    fi_gamedb::topspeed::TopSpeeds::fh2()
        .iter()
        .filter(|(o, _)| *o != PLACEHOLDER_CAR && !BLOCKED_CARS.contains(o))
        .map(|(o, _)| speed_limit_kmh(rules, Some(o)))
        .fold(rules.speed_kmh, f64::min)
}

/// The current rules.
pub fn current() -> &'static Rules {
    &RULES[RULES.len() - 1]
}

/// The rules of `version` when it is the current one or the one before it (§3.9).
pub fn accepted(version: i64) -> Option<&'static Rules> {
    let n = RULES.len();
    RULES[n.saturating_sub(2)..]
        .iter()
        .find(|r| r.version == version)
}

/// An observation as the hook sent it (the wire's `PeerObservation`, the XUID parsed).
#[derive(Clone, Debug, PartialEq)]
pub struct NewObservation {
    /// The client-minted id (the idempotency key).
    pub id: Uuid,
    /// The observed player.
    pub xuid: i64,
    /// One of [`KINDS`].
    pub kind: String,
    /// speed/climb: the peak km/h; jump: metres; car: the ordinal; clock: the clock ratio.
    pub value: Option<f64>,
    /// speed/climb: how long it stayed over the limit (speed: over `perCarFloorKmh` when
    /// `held_kmh` is given).
    pub sustain_ms: Option<i64>,
    /// speed: the observed car's ordinal, when the hook knows it (NR-5).
    pub car_id: Option<i64>,
    /// speed: the highest speed held without a break for at least the rules' `speed.sustainMs`.
    pub held_kmh: Option<f64>,
    /// The rules version measured against.
    pub rules_version: i64,
}

/// Checks an observation's fields (§3.9): `Err(field)` names the first wrong one
/// (`REQUEST.INVALID {field}`).
pub fn validate(o: &NewObservation) -> Result<&'static Rules, &'static str> {
    if !KINDS.contains(&o.kind.as_str()) {
        return Err("kind");
    }
    let rules = accepted(o.rules_version).ok_or("rulesVersion")?;
    let value = o.value.filter(|v| v.is_finite());
    if o.value.is_some() && value.is_none() {
        return Err("value");
    }
    let value_ok = match o.kind.as_str() {
        "speed" | "climb" => value.is_some_and(|v| v > 0.0 && v <= 5_000.0),
        "jump" => value.is_some_and(|v| (0.0..=1_000_000.0).contains(&v)),
        "car" => value.is_some_and(|v| v.fract() == 0.0 && (0.0..=1_000_000.0).contains(&v)),
        "clock" => value.is_some_and(|v| v > 0.0 && v <= 1_000.0),
        // `parts` (NR-6): the offending part row id, the first in slot order.
        "parts" => value.is_some_and(|v| v.fract() == 0.0 && v.abs() < 1e9),
        _ => false,
    };
    if !value_ok {
        return Err("value");
    }
    let sustain = matches!(o.kind.as_str(), "speed" | "climb");
    match o.sustain_ms {
        Some(ms) if sustain && (0..=3_600_000).contains(&ms) => {}
        None if !sustain => {}
        _ => return Err("sustainMs"),
    }
    let speed = o.kind == "speed";
    match o.car_id {
        Some(car) if !speed || !(0..=1_000_000).contains(&car) => return Err("carId"),
        _ => {}
    }
    match o.held_kmh {
        Some(held)
            if !speed || !(held.is_finite() && held > 0.0 && held <= value.unwrap_or(0.0)) =>
        {
            return Err("heldKmh");
        }
        _ => {}
    }
    Ok(rules)
}

/// Whether a valid observation counts toward a quorum under `rules` (§3.9 "What counts"). `speed`
/// is judged against the observed car's own limit ([`speed_limit_kmh`]): the held speed over it
/// when the hook sent `heldKmh`, else the peak over the larger of it and the flat limit (a hook
/// measuring against the flat limit: its `sustainMs` is time over that). `car` counts only for an ordinal on [`BLOCKED_CARS`]:
/// an ordinal outside the base catalogue may be a car-pack car (fi-gamedb treats an unknown id as
/// "no reference", never as a refusal).
pub fn counts(o: &NewObservation, rules: &Rules) -> bool {
    let value = o.value.unwrap_or(0.0);
    let sustain = o.sustain_ms.unwrap_or(0);
    match o.kind.as_str() {
        "speed" => {
            let limit = speed_limit_kmh(rules, o.car_id);
            // Without a held speed only the peak is known: never below the flat limit.
            let over = match o.held_kmh {
                Some(held) => held > limit,
                None => value > limit.max(rules.speed_kmh),
            };
            over && sustain >= rules.speed_ms
        }
        "climb" => value > rules.climb_kmh && sustain >= rules.climb_ms,
        "car" => BLOCKED_CARS.contains(&(value as i64)),
        "parts" => true,
        _ => false,
    }
}

/// What [`observe`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observed {
    /// Not a Hardcore world the caller hosts or is seated in (404 `MM.WORLD_NOT_FOUND`).
    WorldNotFound,
    /// The observed XUID is not the host or a seated member, or is the caller's own
    /// (400 `REQUEST.INVALID {field: "xuid"}`).
    NotMember,
    /// The id was stored before: nothing changed.
    Repeat,
    /// Stored.
    Stored {
        /// It counts toward a quorum.
        counts: bool,
        /// It completed a quorum.
        quorum: bool,
        /// The observed profile is flagged (newly or again).
        flagged: bool,
    },
}

/// Stores one observation and settles its quorum and flag, in one transaction on one pooled
/// connection (§3.9). The caller must have validated it ([`validate`]).
pub async fn observe(
    db: &PgPool,
    caller: &Caller,
    world_id: Uuid,
    o: &NewObservation,
    rules: &Rules,
) -> Result<Observed, IdentityError> {
    let mut tx = db.begin().await?;
    let out = observe_in(&mut tx, caller, world_id, o, rules).await?;
    tx.commit().await?;
    Ok(out)
}

async fn observe_in(
    conn: &mut PgConnection,
    caller: &Caller,
    world_id: Uuid,
    o: &NewObservation,
    rules: &Rules,
) -> Result<Observed, IdentityError> {
    // A repeated id changes nothing, whoever sends it and whatever happened to the world since.
    let seen: Option<bool> = sqlx::query_scalar("SELECT true FROM peer_observation WHERE id = $1")
        .bind(o.id)
        .fetch_optional(&mut *conn)
        .await?;
    if seen.is_some() {
        return Ok(Observed::Repeat);
    }
    // The reporter: the host, or a used seat of the ticket's game, in an open Hardcore world of
    // the ticket's mode (Hardcore only, never `any`); never an official world (OD-189: its engine is
    // the detector, MATCHMAKING_API.md §2.9).
    let host: Option<Uuid> = sqlx::query_scalar(
        "SELECT w.host_profile_id FROM world w \
         WHERE w.id = $1 AND w.game = $2 AND $3 = 'hardcore' AND w.mode = 'hardcore' AND NOT w.any_mode \
           AND w.host_kind = 'player' AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
           AND ((w.host_profile_id = $4 AND NOT w.dedicated) \
             OR EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id AND g.game = $2 \
                          AND g.profile_id = $4 AND g.state = 'used'))",
    )
    .bind(world_id)
    .bind(&caller.game)
    .bind(&caller.mode)
    .bind(caller.profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(host) = host else {
        return Ok(Observed::WorldNotFound);
    };
    // The members now: the host (a dedicated world's is not a player) and the used seats.
    let members: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT p.id, p.xuid FROM profile p WHERE p.id IN ( \
           SELECT w.host_profile_id FROM world w WHERE w.id = $1 AND NOT w.dedicated \
           UNION SELECT g.profile_id FROM mm_grant g WHERE g.world_id = $1 AND g.state = 'used')",
    )
    .bind(world_id)
    .fetch_all(&mut *conn)
    .await?;
    let Some(observed) = members
        .iter()
        .find(|(p, x)| *x == o.xuid && *p != caller.profile_id)
        .map(|(p, _)| *p)
    else {
        return Ok(Observed::NotMember);
    };
    let counting = counts(o, rules);
    // Counting observations about one profile settle one at a time (each sees the others' rows);
    // log-only ones never wait. The observed profile's row next, before this transaction holds any
    // other row: its own join and hosting take it `FOR SHARE` before their world, so a flag below
    // and those never interleave (the pineapple pool's moves, §3.9).
    if counting {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('peer_observation:' || $1::text, 0))",
        )
        .bind(observed)
        .execute(&mut *conn)
        .await?;
        sqlx::query("SELECT 1 FROM profile WHERE id = $1 FOR NO KEY UPDATE")
            .bind(observed)
            .execute(&mut *conn)
            .await?;
    }
    let stored = sqlx::query(
        "INSERT INTO peer_observation (id, world_id, reporter_profile_id, observed_profile_id, kind, \
           value, sustain_ms, rules_version, counts, car_id, held_kmh) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(o.id)
    .bind(world_id)
    .bind(caller.profile_id)
    .bind(observed)
    .bind(&o.kind)
    .bind(o.value)
    .bind(o.sustain_ms.map(|v| v as i32))
    .bind(o.rules_version as i32)
    .bind(counting)
    .bind(o.car_id.map(|c| c as i32))
    .bind(o.held_kmh)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if stored == 0 {
        return Ok(Observed::Repeat);
    }
    if !counting {
        return Ok(Observed::Stored {
            counts: false,
            quorum: false,
            flagged: false,
        });
    }
    let reporters: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT r.account_id) FROM peer_observation o \
         JOIN profile r ON r.id = o.reporter_profile_id \
         WHERE o.world_id = $1 AND o.observed_profile_id = $2 AND o.kind = $3 AND o.counts \
           AND o.created_at > clock_timestamp() - make_interval(secs => $4) \
           AND r.account_id <> (SELECT account_id FROM profile WHERE id = $2)",
    )
    .bind(world_id)
    .bind(observed)
    .bind(&o.kind)
    .bind(QUORUM_WINDOW_SECS as f64)
    .fetch_one(&mut *conn)
    .await?;
    let others = members.len() as i64 - 1;
    if !reached(reporters, others) {
        return Ok(Observed::Stored {
            counts: true,
            quorum: false,
            flagged: false,
        });
    }
    sqlx::query(
        "INSERT INTO cheat_quorum (world_id, observed_profile_id, kind, host_profile_id) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (world_id, observed_profile_id, kind) \
         DO UPDATE SET host_profile_id = excluded.host_profile_id, reached_at = clock_timestamp()",
    )
    .bind(world_id)
    .bind(observed)
    .bind(&o.kind)
    .bind(host)
    .execute(&mut *conn)
    .await?;
    let flag = if observed == host {
        true
    } else {
        let hosts: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT h.account_id) FROM cheat_quorum q \
             JOIN profile h ON h.id = q.host_profile_id \
             WHERE q.observed_profile_id = $1 AND q.reached_at > clock_timestamp() - make_interval(hours => $2)",
        )
        .bind(observed)
        .bind(HOSTS_WINDOW_HOURS as i32)
        .fetch_one(&mut *conn)
        .await?;
        hosts >= 2
    } && {
        let accounts: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT r.account_id) FROM peer_observation o \
             JOIN profile r ON r.id = o.reporter_profile_id \
             WHERE o.observed_profile_id = $1 AND o.counts \
               AND o.created_at > clock_timestamp() - make_interval(hours => $2) \
               AND r.account_id <> (SELECT account_id FROM profile WHERE id = $1)",
        )
        .bind(observed)
        .bind(HOSTS_WINDOW_HOURS as i32)
        .fetch_one(&mut *conn)
        .await?;
        accounts >= FLAG_MIN_REPORTERS
    };
    if flag {
        flag_in(conn, observed, &o.kind).await?;
        // OD-149: out of every clean world, in this transaction (idempotent on a repeat quorum).
        crate::matchmaking::pool_in(conn, observed).await?;
    }
    Ok(Observed::Stored {
        counts: true,
        quorum: true,
        flagged: flag,
    })
}

/// Sets or moves the flag of `profile` for a quorum of `kind` (§3.9 "The flag"); the caller holds
/// the profile's row lock. A live flag keeps its episode and moves its end to at least now plus
/// that episode's length; an ended one (or none) starts the next episode, or the first when the
/// previous one ended more than [`CHEAT_POOL_REPEAT_DAYS`] ago.
async fn flag_in(conn: &mut PgConnection, profile: Uuid, kind: &str) -> Result<(), IdentityError> {
    let row: Option<(bool, bool, i32, bool)> = sqlx::query_as(
        "SELECT until > clock_timestamp(), \
           until > clock_timestamp() - make_interval(days => $2), episode, lifted \
         FROM cheat_flag WHERE profile_id = $1 FOR UPDATE",
    )
    .bind(profile)
    .bind(CHEAT_POOL_REPEAT_DAYS)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some((true, _, episode, _)) => {
            sqlx::query(
                "UPDATE cheat_flag SET last_at = t.at, lifted = false, \
                   until = greatest(until, t.at + make_interval(hours => $2)), \
                   kinds = ARRAY(SELECT DISTINCT k FROM unnest(kinds || ARRAY[$3]) k ORDER BY k) \
                 FROM (SELECT clock_timestamp() AS at) t WHERE profile_id = $1",
            )
            .bind(profile)
            .bind(pool_hours(episode) as i32)
            .bind(kind)
            .execute(&mut *conn)
            .await?;
        }
        Some((false, recent, episode, lifted)) => {
            let next = next_episode(episode, recent, lifted);
            sqlx::query(
                "UPDATE cheat_flag SET first_at = t.at, last_at = t.at, lifted = false, \
                   until = t.at + make_interval(hours => $2), kinds = ARRAY[$3], episode = $4 \
                 FROM (SELECT clock_timestamp() AS at) t WHERE profile_id = $1",
            )
            .bind(profile)
            .bind(pool_hours(next) as i32)
            .bind(kind)
            .bind(next)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO cheat_flag (profile_id, first_at, last_at, until, kinds, episode) \
                 SELECT $1, t.at, t.at, t.at + make_interval(hours => $2), ARRAY[$3], 1 \
                 FROM (SELECT clock_timestamp() AS at) t",
            )
            .bind(profile)
            .bind(pool_hours(1) as i32)
            .bind(kind)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// The episode of a new flag after an ended one (`episode`, ended within
/// [`CHEAT_POOL_REPEAT_DAYS`] when `recent`): the next one doubles, a lifted one does not count
/// (its episode repeats), and after a clean window the count starts over.
pub fn next_episode(episode: i32, recent: bool, lifted: bool) -> i32 {
    match (recent, lifted) {
        (false, _) => 1,
        (true, true) => episode.max(1),
        (true, false) => episode.saturating_add(1),
    }
}

/// What [`unflag_in`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unflagged {
    /// A flag was in force and is now lifted (`false`: nothing changed).
    pub was_flagged: bool,
    /// The lifted flag's episode.
    pub episode: Option<i32>,
    /// When the lifted flag would have ended.
    pub until: Option<UnixMs>,
}

/// Lifts the live flag of `profile` (§3.9 "Lifting a flag"): it ends now (`until` = the lift) and
/// is marked `lifted`, so the next flag repeats its episode instead of doubling on it. Nothing
/// else moves: the profile leaves the pool at its next list or join; a pineapple world it hosts
/// stays one. Idempotent: no live flag (none, ended, or lifted before) changes nothing. Runs in the
/// caller's transaction (which writes the audit row); the flag's row lock serialises it with a
/// concurrent quorum's [`flag_in`] on an existing row (a profile's very first flag inserted while
/// the lift runs is not seen: the lift answers `was_flagged: false` and staff lift again).
pub async fn unflag_in(conn: &mut PgConnection, profile: Uuid) -> Result<Unflagged, IdentityError> {
    // The row lock first, whatever the flag's state, so a quorum's flag that committed while this
    // waited is seen (the next statement reads a fresh snapshot) and lifted too.
    sqlx::query("SELECT 1 FROM cheat_flag WHERE profile_id = $1 FOR UPDATE")
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    let row: Option<(i32, OffsetDateTime)> = sqlx::query_as(
        "WITH old AS (SELECT until FROM cheat_flag WHERE profile_id = $1 \
           AND until > clock_timestamp()) \
         UPDATE cheat_flag f SET lifted = true, \
           until = greatest(clock_timestamp(), f.last_at + interval '1 microsecond') \
         FROM old WHERE f.profile_id = $1 RETURNING f.episode, old.until",
    )
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match row {
        Some((episode, until)) => Unflagged {
            was_flagged: true,
            episode: Some(episode),
            until: Some(ms(until)),
        },
        None => Unflagged {
            was_flagged: false,
            episode: None,
            until: None,
        },
    })
}

/// The live (not deleted) profile `profile` and its account, for a lift (`None`: unknown).
pub async fn profile_account_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<Option<Uuid>, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT account_id FROM profile WHERE id = $1 AND deleted_at IS NULL")
            .bind(profile)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// The operator's lift (`fi-cli cheat-unflag`): [`unflag_in`] and an operator `admin_action` row
/// `profile.cheat_unflag` (actor `admin:<actor>`, the reason, target `profile:<id>`, detail
/// `{wasFlagged, episode}`) in one transaction. `NotFound` for an unknown or deleted profile.
pub async fn unflag_as_operator(
    db: &PgPool,
    profile: Uuid,
    actor: &str,
    reason: &str,
) -> Result<Unflagged, IdentityError> {
    let mut tx = db.begin().await?;
    let account = profile_account_in(&mut tx, profile)
        .await?
        .ok_or(IdentityError::NotFound)?;
    let lifted = unflag_in(&mut tx, profile).await?;
    sqlx::query(
        "INSERT INTO admin_action (actor, action, account_id, reason, detail, target) \
         VALUES ('admin:' || $1, 'profile.cheat_unflag', $2, $3, $4, 'profile:' || $5::text)",
    )
    .bind(actor)
    .bind(account)
    .bind(reason)
    .bind(serde_json::json!({"wasFlagged": lifted.was_flagged, "episode": lifted.episode}))
    .bind(profile)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(lifted)
}

/// One profile's flag row as staff see it (CORE_API.md §5 `AdminAccountView.cheat`): live, or
/// ended within [`CHEAT_POOL_REPEAT_DAYS`] (it still sets the next flag's length).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagState {
    /// The profile.
    pub profile_id: Uuid,
    /// The flag's end (in the past once ended or lifted).
    pub until: UnixMs,
    /// The flag's first moment.
    pub first_at: UnixMs,
    /// Its repeat count (1 = first).
    pub episode: i32,
    /// The kinds of its quorums.
    pub kinds: Vec<String>,
    /// Staff lifted it.
    pub lifted: bool,
    /// In force now (a live flag of a Hardcore profile: the pool applies).
    pub in_force: bool,
}

/// The flag rows of the account's live profiles, the latest first.
pub async fn flags_of_account_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Vec<FlagState>, IdentityError> {
    let rows = sqlx::query(
        "SELECT f.profile_id, f.until, f.first_at, f.episode, f.kinds, f.lifted, \
           f.until > clock_timestamp() AND p.mode = 'hardcore' AS in_force \
         FROM cheat_flag f JOIN profile p ON p.id = f.profile_id \
         WHERE p.account_id = $1 AND p.deleted_at IS NULL \
           AND f.until > clock_timestamp() - make_interval(days => $2) \
         ORDER BY f.last_at DESC, f.profile_id",
    )
    .bind(account)
    .bind(CHEAT_POOL_REPEAT_DAYS)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(|r| -> Result<FlagState, sqlx::Error> {
            Ok(FlagState {
                profile_id: r.try_get("profile_id")?,
                until: ms(r.try_get("until")?),
                first_at: ms(r.try_get("first_at")?),
                episode: r.try_get("episode")?,
                kinds: r.try_get("kinds")?,
                lifted: r.try_get("lifted")?,
                in_force: r.try_get("in_force")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?)
}

/// The quorum rule: at least [`QUORUM_MIN`] reporters and more than half of the `others`.
pub fn reached(reporters: i64, others: i64) -> bool {
    reporters >= QUORUM_MIN && reporters * 2 > others
}

/// The flag of a profile while it lasts (`None`: clean): the end `pool: {until}` shows.
pub async fn flag_of(db: &PgPool, profile: Uuid) -> Result<Option<UnixMs>, IdentityError> {
    let mut conn = db.acquire().await?;
    flag_of_in(&mut conn, profile).await
}

/// [`flag_of`] on the caller's connection. Only a Hardcore profile is ever in the pool: a profile
/// whose mode changed since keeps no flag that counts.
pub async fn flag_of_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<Option<UnixMs>, IdentityError> {
    let until: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT f.until FROM cheat_flag f JOIN profile p ON p.id = f.profile_id \
         WHERE f.profile_id = $1 AND f.until > clock_timestamp() AND p.mode = 'hardcore'",
    )
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(until.map(ms))
}

/// One kind's numbers in [`report`].
#[derive(Clone, Debug, PartialEq)]
pub struct KindStats {
    /// The kind.
    pub kind: String,
    /// Observations.
    pub observations: i64,
    /// Of them, counting.
    pub counting: i64,
    /// Distinct reporting profiles.
    pub reporters: i64,
    /// Distinct observed profiles.
    pub observed: i64,
    /// The value's median, 95th and 99th percentiles and maximum (`None` without values).
    pub p50: Option<f64>,
    /// 95th percentile.
    pub p95: Option<f64>,
    /// 99th percentile.
    pub p99: Option<f64>,
    /// Maximum.
    pub max: Option<f64>,
}

/// A live flag, or one lifted within the report's days, in [`report`] (ids only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagRow {
    /// The profile.
    pub profile_id: Uuid,
    /// Until (the lift, for a lifted flag).
    pub until: UnixMs,
    /// The kinds of its quorums.
    pub kinds: Vec<String>,
    /// Its repeat count (1 = first).
    pub episode: i32,
    /// Its first moment.
    pub first_at: UnixMs,
    /// Staff lifted it.
    pub lifted: bool,
}

/// One observed car's speed outcomes in [`report`] (NR-5): the per-car limit applied.
#[derive(Clone, Debug, PartialEq)]
pub struct CarSpeed {
    /// The car ordinal reported (`None`: observations that named no car).
    pub car_id: Option<i64>,
    /// The current rules' limit for it ([`speed_limit_kmh`]).
    pub limit_kmh: f64,
    /// Speed observations.
    pub observations: i64,
    /// Of them, counting (judged with the rules of their version when stored).
    pub counting: i64,
    /// The largest held speed (or peak, without `heldKmh`).
    pub max_kmh: Option<f64>,
}

/// `fi-cli cheat-report`: the calibration data of the last `days` (§3.9 "Order of delivery").
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Per kind, every kind listed (zeros included).
    pub kinds: Vec<KindStats>,
    /// Speed observations per observed car, the most observed first (at most [`REPORT_CARS`]).
    pub speed_cars: Vec<CarSpeed>,
    /// World and kind pairs whose quorum was last reached in the last 24 hours (`cheat_quorum` keeps
    /// a day, one row per world, profile and kind).
    pub quorums: i64,
    /// Live flags and flags lifted within the report's days, the latest first.
    pub flags: Vec<FlagRow>,
}

/// The most cars [`report`] lists.
pub const REPORT_CARS: i64 = 50;

/// Reads [`Report`] for the last `days`.
pub async fn report(db: &PgPool, days: i32) -> Result<Report, IdentityError> {
    let rows = sqlx::query(
        "SELECT k.kind, count(o.id) AS observations, count(o.id) FILTER (WHERE o.counts) AS counting, \
           count(DISTINCT o.reporter_profile_id) AS reporters, count(DISTINCT o.observed_profile_id) AS observed, \
           percentile_cont(0.5) WITHIN GROUP (ORDER BY o.value) AS p50, \
           percentile_cont(0.95) WITHIN GROUP (ORDER BY o.value) AS p95, \
           percentile_cont(0.99) WITHIN GROUP (ORDER BY o.value) AS p99, max(o.value) AS max \
         FROM unnest($1::text[]) WITH ORDINALITY AS k(kind, n) \
         LEFT JOIN peer_observation o ON o.kind = k.kind \
           AND o.created_at > clock_timestamp() - make_interval(days => $2) \
         GROUP BY k.kind, k.n ORDER BY k.n",
    )
    .bind(KINDS.to_vec())
    .bind(days)
    .fetch_all(db)
    .await?;
    let kinds = rows
        .iter()
        .map(|r| -> Result<KindStats, sqlx::Error> {
            Ok(KindStats {
                kind: r.try_get("kind")?,
                observations: r.try_get("observations")?,
                counting: r.try_get("counting")?,
                reporters: r.try_get("reporters")?,
                observed: r.try_get("observed")?,
                p50: r.try_get("p50")?,
                p95: r.try_get("p95")?,
                p99: r.try_get("p99")?,
                max: r.try_get("max")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let speed_cars = sqlx::query_as::<_, (Option<i32>, i64, i64, Option<f64>)>(
        "SELECT car_id, count(*), count(*) FILTER (WHERE counts), max(coalesce(held_kmh, value)) \
         FROM peer_observation WHERE kind = 'speed' \
           AND created_at > clock_timestamp() - make_interval(days => $1) \
         GROUP BY car_id ORDER BY count(*) DESC, car_id NULLS FIRST LIMIT $2",
    )
    .bind(days)
    .bind(REPORT_CARS)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|(car, observations, counting, max_kmh)| {
        let car_id = car.map(i64::from);
        CarSpeed {
            car_id,
            limit_kmh: speed_limit_kmh(current(), car_id),
            observations,
            counting,
            max_kmh,
        }
    })
    .collect();
    let quorums: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cheat_quorum WHERE reached_at > clock_timestamp() - interval '24 hours'",
    )
    .fetch_one(db)
    .await?;
    let flags = sqlx::query(
        "SELECT profile_id, until, kinds, episode, first_at, lifted FROM cheat_flag \
         WHERE until > clock_timestamp() \
           OR (lifted AND until > clock_timestamp() - make_interval(days => $1)) \
         ORDER BY until > clock_timestamp() DESC, last_at DESC, profile_id",
    )
    .bind(days)
    .fetch_all(db)
    .await?
    .iter()
    .map(|r| -> Result<FlagRow, sqlx::Error> {
        Ok(FlagRow {
            profile_id: r.try_get("profile_id")?,
            until: ms(r.try_get("until")?),
            kinds: r.try_get("kinds")?,
            episode: r.try_get("episode")?,
            first_at: ms(r.try_get("first_at")?),
            lifted: r.try_get("lifted")?,
        })
    })
    .collect::<Result<Vec<_>, _>>()?;
    Ok(Report {
        kinds,
        speed_cars,
        quorums,
        flags,
    })
}

/// A profile's rows erased with it (fi_db::erasure).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfileErased {
    /// `peer_observation` rows it reported or that observed it.
    pub observations: u64,
    /// `cheat_quorum` rows about it or under its worlds.
    pub quorums: u64,
    /// Its `cheat_flag` row.
    pub flags: u64,
}

/// Deletes every row naming `profile` (a player's profile deletion and the account erasure).
pub async fn erase_profile_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<ProfileErased, sqlx::Error> {
    let observations = sqlx::query(
        "DELETE FROM peer_observation WHERE reporter_profile_id = $1 OR observed_profile_id = $1",
    )
    .bind(profile)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let quorums = sqlx::query(
        "DELETE FROM cheat_quorum WHERE observed_profile_id = $1 OR host_profile_id = $1",
    )
    .bind(profile)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let flags = sqlx::query("DELETE FROM cheat_flag WHERE profile_id = $1")
        .bind(profile)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(ProfileErased {
        observations,
        quorums,
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(kind: &str, value: Option<f64>, sustain_ms: Option<i64>) -> NewObservation {
        NewObservation {
            id: Uuid::nil(),
            xuid: 1,
            kind: kind.into(),
            value,
            sustain_ms,
            car_id: None,
            held_kmh: None,
            rules_version: current().version,
        }
    }

    fn speed(value: f64, held: Option<f64>, car: Option<i64>) -> NewObservation {
        NewObservation {
            car_id: car,
            held_kmh: held,
            ..obs("speed", Some(value), Some(current().speed_ms))
        }
    }

    #[test]
    fn speed_counts_over_the_observed_cars_own_limit() {
        let r = current();
        let flat = r.speed_kmh;
        // A slow car (2176: bound 283, high): its limit is 311.3, far under the flat one.
        assert_eq!(speed_limit_kmh(r, Some(2176)), 311.3);
        assert!(counts(&speed(405.0, Some(400.0), Some(2176)), r));
        assert!(counts(&speed(320.0, Some(312.0), Some(2176)), r));
        // Without a held speed, a peak never counts below the flat limit.
        assert!(!counts(&speed(400.0, None, Some(2176)), r));
        assert!(counts(&speed(flat + 1.0, None, Some(2176)), r));
        assert!(!counts(&speed(320.0, Some(311.3), Some(2176)), r));
        // The same speed without a car is judged by the flat limit.
        assert!(!counts(&speed(400.0, None, None), r));
        // A swapped car (2045: bound 591, medium): 650.1, above the flat one; 560 is legitimate.
        assert_eq!(speed_limit_kmh(r, Some(2045)), 650.1);
        assert!(!counts(&speed(560.0, None, Some(2045)), r));
        assert!(!counts(&speed(650.0, None, Some(2045)), r));
        assert!(counts(&speed(651.0, None, Some(2045)), r));
        assert!(!counts(&speed(700.0, Some(650.0), Some(2045)), r));
        assert!(counts(&speed(700.0, Some(651.0), Some(2045)), r));
        // A low row never goes under the flat limit (1674: bound 221) but may go over it (1558: 613).
        assert_eq!(speed_limit_kmh(r, Some(1674)), flat);
        assert!(!counts(&speed(flat - 1.0, None, Some(1674)), r));
        assert!(counts(&speed(flat + 1.0, None, Some(1674)), r));
        assert!((speed_limit_kmh(r, Some(1558)) - 674.3).abs() < 1e-9);
        // An unknown car, the placeholder and no car: the flat limit.
        for car in [None, Some(3000), Some(PLACEHOLDER_CAR)] {
            assert_eq!(speed_limit_kmh(r, car), flat, "{car:?}");
        }
        // The held speed decides even with a high peak (a one-sample spike proves nothing), and the
        // sustain stays the rules'.
        assert!(!counts(&speed(900.0, Some(300.0), Some(2176)), r));
        let mut short = speed(400.0, None, Some(2176));
        short.sustain_ms = Some(r.speed_ms - 1);
        assert!(!counts(&short, r));
    }

    #[test]
    fn the_floor_is_the_lowest_player_car_limit() {
        let r = current();
        // 2176's 311.3: the traffic cars (631: 220) and the placeholder are left out.
        assert_eq!(per_car_floor_kmh(r), 311.3_f64.min(r.speed_kmh));
        assert_eq!(speed_limit_kmh(r, Some(631)), 220.0);
    }

    #[test]
    fn car_id_and_held_kmh_are_speeds_only_and_in_range() {
        assert!(validate(&speed(400.0, Some(400.0), Some(0))).is_ok());
        assert!(validate(&speed(400.0, None, Some(1_000_000))).is_ok());
        assert_eq!(validate(&speed(400.0, None, Some(-1))), Err("carId"));
        assert_eq!(validate(&speed(400.0, None, Some(1_000_001))), Err("carId"));
        let mut climb = obs("climb", Some(200.0), Some(4000));
        climb.car_id = Some(247);
        assert_eq!(validate(&climb), Err("carId"));
        assert_eq!(validate(&speed(400.0, Some(401.0), None)), Err("heldKmh"));
        assert_eq!(validate(&speed(400.0, Some(0.0), None)), Err("heldKmh"));
        assert_eq!(
            validate(&speed(400.0, Some(f64::NAN), None)),
            Err("heldKmh")
        );
        climb.car_id = None;
        climb.held_kmh = Some(100.0);
        assert_eq!(validate(&climb), Err("heldKmh"));
    }

    #[test]
    fn the_quorum_needs_two_reporters_and_a_majority_of_the_others() {
        // A two-player world never flags: one other, one possible reporter.
        assert!(!reached(1, 1));
        assert!(reached(2, 2));
        assert!(reached(2, 3));
        assert!(!reached(2, 4));
        assert!(reached(3, 4));
        assert!(!reached(5, 10));
        assert!(reached(6, 10));
    }

    #[test]
    fn each_kind_checks_its_own_fields() {
        assert!(validate(&obs("speed", Some(500.0), Some(4000))).is_ok());
        assert_eq!(validate(&obs("speed", Some(500.0), None)), Err("sustainMs"));
        assert_eq!(validate(&obs("speed", None, Some(4000))), Err("value"));
        assert_eq!(
            validate(&obs("speed", Some(f64::NAN), Some(1))),
            Err("value")
        );
        assert_eq!(validate(&obs("climb", Some(-3.0), Some(1))), Err("value"));
        assert!(validate(&obs("jump", Some(250.0), None)).is_ok());
        assert_eq!(
            validate(&obs("jump", Some(250.0), Some(1))),
            Err("sustainMs")
        );
        assert!(validate(&obs("car", Some(247.0), None)).is_ok());
        assert_eq!(validate(&obs("car", Some(247.5), None)), Err("value"));
        // NR-6: a part carries the offending part row id, a whole number under 1e9 in size.
        assert!(validate(&obs("parts", Some(4_021.0), None)).is_ok());
        assert!(validate(&obs("parts", Some(-7.0), None)).is_ok());
        assert_eq!(validate(&obs("parts", None, None)), Err("value"));
        assert_eq!(validate(&obs("parts", Some(12.5), None)), Err("value"));
        assert_eq!(validate(&obs("parts", Some(1e9), None)), Err("value"));
        assert_eq!(
            validate(&obs("parts", Some(4_021.0), Some(1))),
            Err("sustainMs")
        );
        assert!(validate(&obs("clock", Some(1.4), None)).is_ok());
        assert_eq!(validate(&obs("clock", Some(0.0), None)), Err("value"));
        assert_eq!(validate(&obs("teleport", None, None)), Err("kind"));
        let mut old = obs("parts", Some(1.0), None);
        old.rules_version = 0;
        assert_eq!(validate(&old), Err("rulesVersion"));
    }

    #[test]
    fn only_numbers_past_the_rules_count_and_jump_and_clock_never_do() {
        let r = current();
        assert!(counts(&obs("speed", Some(452.0), Some(3000)), r));
        assert!(!counts(&obs("speed", Some(452.0), Some(2999)), r));
        assert!(!counts(&obs("speed", Some(451.0), Some(9000)), r));
        assert!(!counts(&obs("speed", Some(440.0), Some(9000)), r));
        assert!(counts(&obs("climb", Some(151.0), Some(3000)), r));
        assert!(!counts(&obs("climb", Some(151.0), Some(2999)), r));
        assert!(!counts(&obs("climb", Some(150.0), Some(9000)), r));
        assert!(!counts(&obs("jump", Some(10_000.0), None), r));
        assert!(!counts(&obs("clock", Some(4.0), None), r));
        assert!(counts(&obs("parts", Some(4_021.0), None), r));
        // Only a traffic car counts (V9): any other ordinal may be a car-pack car, and 1215 is a
        // placeholder.
        assert!(counts(&obs("car", Some(283.0), None), r));
        assert!(counts(&obs("car", Some(1674.0), None), r));
        assert!(!counts(&obs("car", Some(1215.0), None), r));
        assert!(!counts(&obs("car", Some(500.0), None), r));
        assert!(!counts(&obs("car", Some(247.0), None), r));
        assert!(!counts(&obs("car", Some(999_999.0), None), r));
        assert_eq!(BLOCKED_CARS.len(), 17);
        assert!(BLOCKED_CARS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_flag_lasts_two_hours_doubling_to_at_most_48() {
        let steps: Vec<i64> = (1..=8).map(pool_hours).collect();
        assert_eq!(steps, [2, 4, 8, 16, 32, 48, 48, 48]);
        assert_eq!(pool_hours(0), 2, "never below the base");
        assert_eq!(pool_hours(i32::MAX), CHEAT_POOL_MAX_HOURS);
    }

    #[test]
    fn a_lifted_flag_does_not_double_the_next_one() {
        assert_eq!(next_episode(1, true, false), 2);
        assert_eq!(next_episode(3, true, false), 4);
        assert_eq!(next_episode(1, true, true), 1);
        assert_eq!(next_episode(3, true, true), 3);
        assert_eq!(next_episode(3, false, true), 1);
        assert_eq!(next_episode(3, false, false), 1);
        assert_eq!(next_episode(i32::MAX, true, false), i32::MAX);
    }

    #[test]
    fn version_one_is_reference_thresholds() {
        let r = &RULES[0];
        assert_eq!(
            (r.speed_kmh, r.speed_ms, r.climb_kmh, r.climb_ms, r.jump_m),
            (451.0, 3000, 150.0, 3000, 200.0)
        );
    }

    #[test]
    fn the_current_and_the_previous_versions_are_accepted() {
        assert_eq!(accepted(current().version), Some(current()));
        assert_eq!(accepted(current().version + 1), None);
        assert!(RULES.windows(2).all(|w| w[0].version < w[1].version));
    }
}
