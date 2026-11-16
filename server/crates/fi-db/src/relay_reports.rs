//! The relay's anti-cheat reports (MATCHMAKING_API.md §5d, OD-159/OD-160, register CR-S80;
//! migration `20261031230000_relay_reports.sql`).
//!
//! The relay detects; matchmaking decides. Each report the relay pushes is stored once (its id is
//! the idempotency key), in its own transaction, with an outcome:
//!
//! 1. **Standing** ([`store`]): the world exists, `hostXuid` is its host, the subject is the host
//!    or held a seat in it (live, or ended within [`STANDING_SECS`]), the reporting relay carried
//!    the subject (the world's relay, or the seat's grant names this relay; a joiner's seat must be
//!    `transport = 'relay'`: a seat that fell back to direct is never judged, OD-160), `to` is at
//!    most [`STANDING_SECS`] old, the rules version is the current or the previous. Else
//!    `rejected` with the reason.
//! 2. **Re-judging** ([`judge`]) with the platform's own `WatchRules` of that version; a report
//!    they do not support is `rejected` (`rules`); a speed report about a car whose per-car limit
//!    is above the flat one is `recorded` (`car_limit`, NR-5, [`over_car_limit`]).
//! 3. **Scope:** only a Hardcore world (`mode = 'hardcore'` or open to both modes) and a subject
//!    profile in the Hardcore mode (violation: the measured profile; forgery: the host) can be
//!    flagged; anything else is `recorded` (`scope`); `jump` is always `recorded` (`jump`); a
//!    forgery in a dedicated world is `recorded` (`dedicated`).
//! 4. **Effect:** while [`RELAY_REPORTS_RECORD_ONLY`] holds (§5d rollout step 1, rule
//!    `relay-reports-record-only`), `would_flag` (violation) or `would_flag_host` (forgery), and
//!    nobody is flagged; peer reports (§3.9) keep deciding. Step 2 is a release that deletes the
//!    rule and calls §3.9's flag instead.
//!
//! Rows carry ids and numbers, never a position, an address or a gamertag; the relay's counters
//! are summed per node key and start ([`add_counters`]).

use crate::identity::IdentityError;
use crate::observations::{self, Rules};
use fi_domain::UnixMs;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// §5d rollout step 1 (rule `relay-reports-record-only`): every report is verified, re-judged and
/// stored as `would_flag*`, and flags nothing. Not a setting (OD-144): step 2 is a release that
/// deletes this rule.
pub const RELAY_REPORTS_RECORD_ONLY: bool = true;
/// A report's `to`, and a seat's end, may be at most this old (seconds).
pub const STANDING_SECS: i64 = 15 * 60;
/// Days a report and a relay start's counters are kept (fi_db::retention).
pub const REPORT_DAYS: i32 = 30;

/// One report as the relay sent it (validated by the route).
#[derive(Clone, Debug, PartialEq)]
pub struct NewRelayReport {
    /// The relay-minted id.
    pub id: Uuid,
    /// The episode.
    pub episode: Uuid,
    /// The reporting relay's node key (verified).
    pub relay_key: String,
    /// Its start.
    pub start_id: Uuid,
    /// `violation` or `forgery`.
    pub kind_type: String,
    /// `speed`, `climb` or `jump`.
    pub kind: String,
    /// The world.
    pub world_id: Uuid,
    /// The measured (violation) or forged (forgery) player.
    pub xuid: i64,
    /// The grant's host.
    pub host_xuid: i64,
    /// Unix ms.
    pub from_ms: i64,
    /// Unix ms.
    pub to_ms: i64,
    /// The version evaluated against.
    pub rules_version: i64,
    /// Speed (NR-5): the measured car's ordinal, when the relay decodes it.
    pub car_id: Option<i64>,
    /// km/h.
    pub peak_kmh: f64,
    /// ms.
    pub over_limit_ms: i64,
    /// Forgery: own maximum.
    pub own_peak_kmh: Option<f64>,
    /// Forgery: legs.
    pub legs: Option<i64>,
    /// Jump: metres.
    pub max_jump_m: Option<f64>,
}

/// Checks a report's fields (the route answers 400 `REQUEST.INVALID {field}` naming the first).
pub fn validate(r: &NewRelayReport) -> Result<(), &'static str> {
    if !matches!(r.kind_type.as_str(), "violation" | "forgery") {
        return Err("type");
    }
    if !matches!(r.kind.as_str(), "speed" | "climb" | "jump")
        || (r.kind_type == "forgery" && r.kind == "jump")
    {
        return Err("kind");
    }
    if r.xuid <= 0 {
        return Err("xuid");
    }
    if r.host_xuid <= 0 {
        return Err("hostXuid");
    }
    if r.from_ms <= 0 || r.from_ms > r.to_ms {
        return Err("from");
    }
    if r.rules_version <= 0 || r.rules_version > i64::from(i32::MAX) {
        return Err("rulesVersion");
    }
    if r.car_id
        .is_some_and(|c| r.kind != "speed" || !(0..=1_000_000).contains(&c))
    {
        return Err("carId");
    }
    let num = |v: f64, max: f64| v.is_finite() && (0.0..=max).contains(&v);
    if !num(r.peak_kmh, 10_000.0) {
        return Err("peakKmh");
    }
    if !(0..=3_600_000).contains(&r.over_limit_ms) {
        return Err("overLimitMs");
    }
    if r.own_peak_kmh.is_some_and(|v| !num(v, 10_000.0)) {
        return Err("ownPeakKmh");
    }
    if r.legs.is_some_and(|v| !(0..=1000).contains(&v)) {
        return Err("legs");
    }
    if r.max_jump_m.is_some_and(|v| !num(v, 1e7)) {
        return Err("maxJumpM");
    }
    Ok(())
}

/// Whether `rules` support the report's verdict (§5d step 3), with the flat limits (the relay
/// detects against them). The car's own limit is [`over_car_limit`]'s question.
pub fn judge(r: &NewRelayReport, rules: &Rules) -> bool {
    let (max, sustain) = match r.kind.as_str() {
        "speed" => (rules.speed_kmh, rules.speed_ms),
        "climb" => (rules.climb_kmh, rules.climb_ms),
        _ => return r.max_jump_m.is_some_and(|m| m > rules.jump_m),
    };
    let over = r.peak_kmh > max && r.over_limit_ms >= sustain;
    match r.kind_type.as_str() {
        "forgery" => over && r.own_peak_kmh.is_some_and(|own| own <= max),
        _ => over,
    }
}

/// Whether a speed report the flat rules support names a car whose per-car limit
/// ([`observations::speed_limit_kmh`], NR-5) is above the flat one: a run over the flat limit
/// proves no held speed over that car's limit, so it is `recorded` (`car_limit`), never flagged,
/// and listed. `carId` is the subject's own car (the measured player's, or the forged one's), so a
/// spoofed car can hide a report from the flag but never from `fi-cli cheat-report`.
pub fn over_car_limit(r: &NewRelayReport, rules: &Rules) -> bool {
    r.kind == "speed" && observations::speed_limit_kmh(rules, r.car_id) > rules.speed_kmh
}

/// What [`store`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// The id was stored before: nothing changed.
    Repeat,
    /// Stored now with this outcome and reason.
    New {
        /// `rejected`, `recorded`, `would_flag` or `would_flag_host`.
        outcome: &'static str,
        /// Why (rejected or recorded).
        reason: Option<&'static str>,
    },
}

struct Standing {
    measured: Option<Uuid>,
    host: Option<Uuid>,
    outcome: &'static str,
    reason: Option<&'static str>,
}

fn rejected(reason: &'static str, measured: Option<Uuid>, host: Option<Uuid>) -> Standing {
    Standing {
        measured,
        host,
        outcome: "rejected",
        reason: Some(reason),
    }
}

async fn standing_in(
    conn: &mut PgConnection,
    r: &NewRelayReport,
    now_ms: i64,
) -> Result<Standing, IdentityError> {
    let world = sqlx::query(
        "SELECT w.host_profile_id, hp.xuid AS host_xuid, hp.mode AS host_mode, w.relay_key, \
           (w.mode = 'hardcore' OR w.any_mode) AS hardcore, w.dedicated, \
           EXISTS (SELECT 1 FROM mm_grant g WHERE g.world_id = w.id AND g.relay_key = $2 \
             AND g.transport = 'relay') AS relayed_here \
         FROM world w JOIN profile hp ON hp.id = w.host_profile_id WHERE w.id = $1",
    )
    .bind(r.world_id)
    .bind(&r.relay_key)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(w) = world else {
        return Ok(rejected("world", None, None));
    };
    let host: Uuid = w.try_get("host_profile_id")?;
    let host_xuid: i64 = w.try_get("host_xuid")?;
    let host_mode: String = w.try_get("host_mode")?;
    let world_relay: Option<String> = w.try_get("relay_key")?;
    let hardcore: bool = w.try_get("hardcore")?;
    let dedicated: bool = w.try_get("dedicated")?;
    let relayed_here: bool = w.try_get("relayed_here")?;
    if host_xuid != r.host_xuid {
        return Ok(rejected("host", None, Some(host)));
    }
    let (measured, subject_mode) = if r.xuid == host_xuid {
        if r.kind_type == "forgery" {
            // The host authors its own car: a forgery of the host's data is no proof.
            return Ok(rejected("rules", Some(host), Some(host)));
        }
        if world_relay.as_deref() != Some(r.relay_key.as_str()) && !relayed_here {
            return Ok(rejected("relay", Some(host), Some(host)));
        }
        (host, host_mode.clone())
    } else {
        let seat = sqlx::query(
            "SELECT g.profile_id, p.mode, g.relay_key, g.transport FROM mm_grant g \
             JOIN profile p ON p.id = g.profile_id \
             WHERE g.world_id = $1 AND g.xuid = $2 \
               AND (g.state IN ('issued', 'used') \
                 OR g.ended_at > clock_timestamp() - make_interval(secs => $3)) \
             ORDER BY g.issued_at DESC LIMIT 1",
        )
        .bind(r.world_id)
        .bind(r.xuid)
        .bind(STANDING_SECS as f64)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(seat) = seat else {
            return Ok(rejected("member", None, Some(host)));
        };
        let profile: Uuid = seat.try_get("profile_id")?;
        let relay_key: Option<String> = seat.try_get("relay_key")?;
        let transport: String = seat.try_get("transport")?;
        if relay_key.as_deref() != Some(r.relay_key.as_str()) {
            return Ok(rejected("relay", Some(profile), Some(host)));
        }
        if transport != "relay" {
            return Ok(rejected("transport", Some(profile), Some(host)));
        }
        (profile, seat.try_get::<String, _>("mode")?)
    };
    if r.to_ms < now_ms - STANDING_SECS * 1000 || r.to_ms > now_ms + 60_000 {
        return Ok(rejected("stale", Some(measured), Some(host)));
    }
    let Some(rules) = observations::accepted(r.rules_version) else {
        return Ok(rejected("rules", Some(measured), Some(host)));
    };
    if !judge(r, rules) {
        return Ok(rejected("rules", Some(measured), Some(host)));
    }
    let recorded = |reason| Standing {
        measured: Some(measured),
        host: Some(host),
        outcome: "recorded",
        reason: Some(reason),
    };
    if r.kind == "jump" {
        return Ok(recorded("jump"));
    }
    if over_car_limit(r, rules) {
        return Ok(recorded("car_limit"));
    }
    let scoped_mode = if r.kind_type == "forgery" {
        host_mode.as_str()
    } else {
        subject_mode.as_str()
    };
    if !hardcore || scoped_mode != "hardcore" {
        return Ok(recorded("scope"));
    }
    if r.kind_type == "forgery" && dedicated {
        return Ok(recorded("dedicated"));
    }
    let outcome = match (RELAY_REPORTS_RECORD_ONLY, r.kind_type.as_str()) {
        (true, "forgery") => "would_flag_host",
        (true, _) => "would_flag",
        // Step 2 (§5d) replaces this arm with §3.9's flag of the measured or host profile.
        (false, "forgery") => "flagged_host",
        (false, _) => "flagged",
    };
    Ok(Standing {
        measured: Some(measured),
        host: Some(host),
        outcome,
        reason: None,
    })
}

/// Stores one report with its outcome, in one transaction on one pooled connection.
pub async fn store(db: &PgPool, r: &NewRelayReport) -> Result<Stored, IdentityError> {
    let mut tx = db.begin().await?;
    let seen: Option<bool> = sqlx::query_scalar("SELECT true FROM relay_report WHERE id = $1")
        .bind(r.id)
        .fetch_optional(&mut *tx)
        .await?;
    if seen.is_some() {
        return Ok(Stored::Repeat);
    }
    let s = standing_in(&mut tx, r, UnixMs::now_system().millis()).await?;
    let n = sqlx::query(
        "INSERT INTO relay_report (id, episode, relay_key, start_id, type, kind, world_id, \
           measured_profile_id, host_profile_id, from_at, to_at, rules_version, peak_kmh, \
           over_limit_ms, own_peak_kmh, legs, max_jump_m, outcome, reason, car_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, to_timestamp($10::double precision / 1000), \
           to_timestamp($11::double precision / 1000), $12, $13, $14, $15, $16, $17, $18, $19, $20) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(r.id)
    .bind(r.episode)
    .bind(&r.relay_key)
    .bind(r.start_id)
    .bind(&r.kind_type)
    .bind(&r.kind)
    .bind(r.world_id)
    .bind(s.measured)
    .bind(s.host)
    .bind(r.from_ms as f64)
    .bind(r.to_ms as f64)
    .bind(r.rules_version as i32)
    .bind(r.peak_kmh)
    .bind(r.over_limit_ms.min(i64::from(i32::MAX)) as i32)
    .bind(r.own_peak_kmh)
    .bind(r.legs.map(|v| v as i32))
    .bind(r.max_jump_m)
    .bind(s.outcome)
    .bind(s.reason)
    .bind(r.car_id.map(|c| c as i32))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(if n == 0 {
        Stored::Repeat
    } else {
        Stored::New {
            outcome: s.outcome,
            reason: s.reason,
        }
    })
}

/// A batch's counters: totals since the relay's start (the row keeps the largest of each, so a
/// retried or replayed batch never counts twice).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Cars decoded.
    pub decoded: i64,
    /// Undecodable packets and positions.
    pub undecodable: i64,
    /// Foreign handles on joiners' own legs.
    pub foreign_owner: i64,
    /// XUID mismatches.
    pub xuid_mismatch: i64,
    /// Subjects without a ring.
    pub witness_full: i64,
    /// Steps without rules.
    pub rules_missing: i64,
    /// Kinds not evaluated.
    pub rule_unevaluable: i64,
    /// Reports dropped.
    pub report_dropped: i64,
    /// Rings in use (a gauge: the latest is kept).
    pub rings_in_use: i64,
}

/// Records a batch's counters on its relay start's row (the largest total of each).
pub async fn add_counters(
    db: &PgPool,
    relay_key: &str,
    start_id: Uuid,
    c: &Counters,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO relay_report_counters (relay_key, start_id, decoded, undecodable, foreign_owner, \
           xuid_mismatch, witness_full, rules_missing, rule_unevaluable, report_dropped, rings_in_use) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (relay_key, start_id) DO UPDATE SET batches = relay_report_counters.batches + 1, \
           decoded = greatest(relay_report_counters.decoded, excluded.decoded), \
           undecodable = greatest(relay_report_counters.undecodable, excluded.undecodable), \
           foreign_owner = greatest(relay_report_counters.foreign_owner, excluded.foreign_owner), \
           xuid_mismatch = greatest(relay_report_counters.xuid_mismatch, excluded.xuid_mismatch), \
           witness_full = greatest(relay_report_counters.witness_full, excluded.witness_full), \
           rules_missing = greatest(relay_report_counters.rules_missing, excluded.rules_missing), \
           rule_unevaluable = greatest(relay_report_counters.rule_unevaluable, excluded.rule_unevaluable), \
           report_dropped = greatest(relay_report_counters.report_dropped, excluded.report_dropped), \
           rings_in_use = excluded.rings_in_use, updated_at = clock_timestamp()",
    )
    .bind(relay_key)
    .bind(start_id)
    .bind(c.decoded)
    .bind(c.undecodable)
    .bind(c.foreign_owner)
    .bind(c.xuid_mismatch)
    .bind(c.witness_full)
    .bind(c.rules_missing)
    .bind(c.rule_unevaluable)
    .bind(c.report_dropped)
    .bind(c.rings_in_use)
    .execute(db)
    .await?;
    Ok(())
}

/// One line of `fi-cli cheat-report`'s relay section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportCount {
    /// The relay's node key.
    pub relay_key: String,
    /// `violation` or `forgery`.
    pub kind_type: String,
    /// `speed`, `climb` or `jump`.
    pub kind: String,
    /// The outcome.
    pub outcome: String,
    /// The reason, if any.
    pub reason: Option<String>,
    /// Reports.
    pub reports: i64,
    /// Distinct measured profiles.
    pub profiles: i64,
}

/// A relay start's summed counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartCounters {
    /// The relay's node key.
    pub relay_key: String,
    /// Batches received.
    pub batches: i64,
    /// The sums (rings: the latest).
    pub counters: Counters,
    /// Last batch.
    pub updated_at: UnixMs,
}

/// The relay section of `fi-cli cheat-report` for the last `days`.
pub async fn report(
    db: &PgPool,
    days: i32,
) -> Result<(Vec<ReportCount>, Vec<StartCounters>), IdentityError> {
    let rows = sqlx::query(
        "SELECT relay_key, type, kind, outcome, reason, count(*) AS reports, \
           count(DISTINCT measured_profile_id) AS profiles FROM relay_report \
         WHERE created_at > clock_timestamp() - make_interval(days => $1) \
         GROUP BY relay_key, type, kind, outcome, reason ORDER BY relay_key, type, kind, outcome, reason",
    )
    .bind(days)
    .fetch_all(db)
    .await?;
    let counts = rows
        .iter()
        .map(|r| -> Result<ReportCount, sqlx::Error> {
            Ok(ReportCount {
                relay_key: r.try_get("relay_key")?,
                kind_type: r.try_get("type")?,
                kind: r.try_get("kind")?,
                outcome: r.try_get("outcome")?,
                reason: r.try_get("reason")?,
                reports: r.try_get("reports")?,
                profiles: r.try_get("profiles")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let rows = sqlx::query(
        "SELECT relay_key, batches, decoded, undecodable, foreign_owner, xuid_mismatch, witness_full, \
           rules_missing, rule_unevaluable, report_dropped, rings_in_use, \
           (extract(epoch FROM updated_at) * 1000)::bigint AS at FROM relay_report_counters \
         WHERE updated_at > clock_timestamp() - make_interval(days => $1) ORDER BY updated_at DESC LIMIT 50",
    )
    .bind(days)
    .fetch_all(db)
    .await?;
    let starts = rows
        .iter()
        .map(|r| -> Result<StartCounters, sqlx::Error> {
            Ok(StartCounters {
                relay_key: r.try_get("relay_key")?,
                batches: r.try_get("batches")?,
                counters: Counters {
                    decoded: r.try_get("decoded")?,
                    undecodable: r.try_get("undecodable")?,
                    foreign_owner: r.try_get("foreign_owner")?,
                    xuid_mismatch: r.try_get("xuid_mismatch")?,
                    witness_full: r.try_get("witness_full")?,
                    rules_missing: r.try_get("rules_missing")?,
                    rule_unevaluable: r.try_get("rule_unevaluable")?,
                    report_dropped: r.try_get("report_dropped")?,
                    rings_in_use: r.try_get("rings_in_use")?,
                },
                updated_at: UnixMs::from_millis(r.try_get("at")?),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((counts, starts))
}

/// Retention (`fi-cli data-retention`, as `fi_maintenance`): reports and relay starts' counters
/// older than [`REPORT_DAYS`], in batches of [`crate::retention::BATCH`]; the two counts.
pub async fn purge(db: &PgPool) -> Result<(u64, u64), IdentityError> {
    let mut out = [0u64; 2];
    for (i, (table, column)) in [
        ("relay_report", "created_at"),
        ("relay_report_counters", "updated_at"),
    ]
    .iter()
    .enumerate()
    {
        let past = format!("{column} < clock_timestamp() - make_interval(days => {REPORT_DAYS})");
        let sql = format!(
            "DELETE FROM {table} WHERE ctid IN (SELECT ctid FROM {table} WHERE {past} LIMIT $1) AND {past}"
        );
        loop {
            let n = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(crate::retention::BATCH)
                .execute(db)
                .await?
                .rows_affected();
            out[i] += n;
            if n < crate::retention::BATCH as u64 {
                break;
            }
        }
    }
    Ok((out[0], out[1]))
}

/// Deletes every report naming `profile` (measured or host); the count.
pub async fn erase_profile_in(conn: &mut PgConnection, profile: Uuid) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM relay_report WHERE measured_profile_id = $1 OR host_profile_id = $1",
    )
    .bind(profile)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(kind_type: &str, kind: &str, peak: f64, over: i64) -> NewRelayReport {
        NewRelayReport {
            id: Uuid::nil(),
            episode: Uuid::nil(),
            relay_key: "k".into(),
            start_id: Uuid::nil(),
            kind_type: kind_type.into(),
            kind: kind.into(),
            world_id: Uuid::nil(),
            xuid: 1,
            host_xuid: 2,
            from_ms: 1,
            to_ms: 2,
            rules_version: 1,
            car_id: None,
            peak_kmh: peak,
            over_limit_ms: over,
            own_peak_kmh: (kind_type == "forgery").then_some(100.0),
            legs: None,
            max_jump_m: None,
        }
    }

    #[test]
    fn the_platform_re_judges_with_its_own_rules() {
        let rules = observations::current();
        let (max, ms) = (rules.speed_kmh, rules.speed_ms);
        assert!(judge(&r("violation", "speed", max + 1.0, ms), rules));
        assert!(
            !judge(&r("violation", "speed", max, ms), rules),
            "over, not at"
        );
        assert!(
            !judge(&r("violation", "speed", max + 1.0, ms - 1), rules),
            "long enough"
        );
        assert!(judge(
            &r("violation", "climb", rules.climb_kmh + 1.0, rules.climb_ms),
            rules
        ));
        assert!(judge(&r("forgery", "speed", max + 1.0, ms), rules));
        let mut fast_own = r("forgery", "speed", max + 50.0, ms);
        fast_own.own_peak_kmh = Some(max + 1.0);
        assert!(
            !judge(&fast_own, rules),
            "the player's own data was over too"
        );
        fast_own.own_peak_kmh = None;
        assert!(!judge(&fast_own, rules));
        let mut jump = r("violation", "jump", 0.0, 0);
        jump.max_jump_m = Some(rules.jump_m + 1.0);
        assert!(judge(&jump, rules));
        jump.max_jump_m = Some(rules.jump_m);
        assert!(!judge(&jump, rules));
        // NR-5: judged flat; a car whose limit is above the flat one (2045: 650.1) is then
        // recorded, never flagged; a car under it (2176: 311.3) or unknown is not.
        let mut swapped = r("violation", "speed", 700.0, ms);
        swapped.car_id = Some(2045);
        assert!(judge(&swapped, rules) && over_car_limit(&swapped, rules));
        swapped.kind = "climb".into();
        assert!(!over_car_limit(&swapped, rules));
        let mut slow = r("violation", "speed", max + 1.0, ms);
        slow.car_id = Some(2176);
        assert!(judge(&slow, rules) && !over_car_limit(&slow, rules));
        slow.car_id = Some(3000);
        assert!(judge(&slow, rules) && !over_car_limit(&slow, rules));
        slow.peak_kmh = max;
        assert!(!judge(&slow, rules));
        let mut climb = r("violation", "climb", rules.climb_kmh + 1.0, rules.climb_ms);
        climb.car_id = Some(2176);
        assert_eq!(validate(&climb), Err("carId"));
        slow.car_id = Some(-1);
        assert_eq!(validate(&slow), Err("carId"));
    }

    #[test]
    fn fields_are_checked_and_named() {
        assert_eq!(validate(&r("violation", "speed", 500.0, 3000)), Ok(()));
        assert_eq!(validate(&r("ban", "speed", 500.0, 3000)), Err("type"));
        assert_eq!(validate(&r("forgery", "jump", 0.0, 0)), Err("kind"));
        assert_eq!(validate(&r("violation", "car", 0.0, 0)), Err("kind"));
        assert_eq!(
            validate(&r("violation", "speed", f64::NAN, 0)),
            Err("peakKmh")
        );
        assert_eq!(
            validate(&r("violation", "speed", 1.0, -1)),
            Err("overLimitMs")
        );
        let mut x = r("violation", "speed", 1.0, 1);
        x.from_ms = 3;
        assert_eq!(validate(&x), Err("from"));
        let mut x = r("violation", "speed", 1.0, 1);
        x.host_xuid = 0;
        assert_eq!(validate(&x), Err("hostXuid"));
    }
}
