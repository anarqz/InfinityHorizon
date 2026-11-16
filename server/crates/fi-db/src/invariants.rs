//! The invariant checker's queries (docs/server/LOCAL_ENV.md §7.2), built by workstream C of the
//! local environment v0, re-cut for platform v1 (PV1-I2: core, gate, saves, matchmaking) and run by
//! `fi-sim check`: I1 no lost save revision, I2 never two writers, I3 every fact consolidated
//! exactly once (the read models `session_summary` and `profile_progress` agree with the game
//! tables), I4 Supporters and free players each on their own pool and service (free sessions never
//! served by saves, VIP never in pool `matchmaking`), I5 queue and admission honoured (the gate's
//! slot semantics). I6 (outbox drained) and I3's lake part left with the outbox (owner
//! 2026-10-26, ADR 0007 superseded). Each query reports counts and the offending ids, never bytes or personal data.
//!
//! Every function reads only (`SELECT`), so it runs as any login that can read the tables (the
//! simulator uses the migrator locally). The parts that need client-side evidence take it as
//! arguments (the simulator's journal): acknowledged commits, `SAVES.LEASE_LOST` answers, the
//! sessions a run played and the distinct facts it sent. The fake-gcs part of I1 is the caller's
//! (it reads [`blob_refs`] and hashes each object).

use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};

/// Offending ids kept per report (the total is counted beyond it).
pub const MAX_OFFENDERS: usize = 20;

/// The outcome of one invariant (or one part of it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// `I1` .. `I6`.
    pub invariant: &'static str,
    /// What it checks, in a few words.
    pub title: &'static str,
    /// No offender.
    pub ok: bool,
    /// Named counts, in insertion order.
    pub counts: Vec<(String, i64)>,
    /// The first [`MAX_OFFENDERS`] offenders (`what: id ...`).
    pub offenders: Vec<String>,
    /// Every offender, including those not listed.
    pub offender_total: usize,
}

impl Report {
    /// An empty, passing report.
    pub fn new(invariant: &'static str, title: &'static str) -> Self {
        Self {
            invariant,
            title,
            ok: true,
            counts: Vec::new(),
            offenders: Vec::new(),
            offender_total: 0,
        }
    }

    /// Records a count.
    pub fn count(&mut self, name: impl Into<String>, value: i64) {
        self.counts.push((name.into(), value));
    }

    /// Records an offender; the report fails.
    pub fn offend(&mut self, what: impl Into<String>) {
        self.ok = false;
        self.offender_total += 1;
        if self.offenders.len() < MAX_OFFENDERS {
            self.offenders.push(what.into());
        }
    }

    /// Folds another part of the same invariant into this one.
    pub fn merge(&mut self, other: Report) {
        self.ok &= other.ok;
        self.counts.extend(other.counts);
        self.offender_total += other.offender_total;
        for o in other.offenders {
            if self.offenders.len() < MAX_OFFENDERS {
                self.offenders.push(o);
            }
        }
    }

    /// A count by name.
    pub fn get(&self, name: &str) -> Option<i64> {
        self.counts.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }

    /// One line: `I1 PASS no lost save revision: profiles=16 revisions=48`, then the offenders.
    pub fn line(&self) -> String {
        let counts: Vec<String> = self
            .counts
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect();
        let mut out = format!(
            "{} {} {}: {}",
            self.invariant,
            if self.ok { "PASS" } else { "FAIL" },
            self.title,
            counts.join(" ")
        );
        if !self.ok {
            out.push_str(&format!(
                " | {} offender(s): {}",
                self.offender_total,
                self.offenders.join("; ")
            ));
            if self.offender_total > self.offenders.len() {
                out.push_str(" ...");
            }
        }
        out
    }
}

/// A revision and the object fake-gcs must hold for it (I1's storage part, checked by the caller).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRef {
    /// Profile id.
    pub profile_id: String,
    /// Revision number.
    pub revision: i64,
    /// The bundle's SHA-256 (the object `blobs/sha256/<hex>`).
    pub blob_sha256: [u8; 32],
}

/// A commit the client saw acknowledged (201, or 200 replayed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ack {
    /// Profile id.
    pub profile_id: String,
    /// The revision the answer named.
    pub revision: i64,
    /// The `Idempotency-Key` (= `save_revision.commit_id`).
    pub commit_id: String,
}

/// A `SAVES.LEASE_LOST` answer the client got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LostLease {
    /// Profile id.
    pub profile_id: String,
    /// The session that lost the lease.
    pub session_id: String,
}

async fn table_exists(pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(pool)
        .await
}

/// I1 (record part): per profile `save_revision.revision` is exactly `1..save_head.revision`, the
/// head names the highest revision, and every `commit_id` is unique.
pub async fn revisions(pool: &PgPool) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I1", "no lost save revision");
    let rows = sqlx::query(
        "WITH rev AS ( \
           SELECT profile_id, count(*) AS n, min(revision) AS lo, max(revision) AS hi \
           FROM save_revision GROUP BY profile_id) \
         SELECT coalesce(h.profile_id, rev.profile_id)::text AS profile, \
                coalesce(h.revision, -1) AS head, coalesce(rev.n, 0) AS n, \
                coalesce(rev.lo, 0) AS lo, coalesce(rev.hi, 0) AS hi \
         FROM save_head h FULL JOIN rev ON rev.profile_id = h.profile_id \
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    let (mut lines, mut total) = (0i64, 0i64);
    for row in &rows {
        let profile: String = row.try_get("profile")?;
        let head: i64 = row.try_get("head")?;
        let n: i64 = row.try_get("n")?;
        let lo: i64 = row.try_get("lo")?;
        let hi: i64 = row.try_get("hi")?;
        total += n;
        if n > 0 {
            lines += 1;
        }
        let contiguous = n == hi && (n == 0 || lo == 1);
        if head < 0 {
            r.offend(format!("revisions without a head: profile {profile}"));
        } else if !contiguous {
            r.offend(format!(
                "gap: profile {profile} has {n} revisions in {lo}..{hi}"
            ));
        } else if head != hi {
            r.offend(format!(
                "head {head} is not the newest revision {hi}: profile {profile}"
            ));
        }
    }
    r.count("profiles", rows.len() as i64);
    r.count("lines", lines);
    r.count("revisions", total);
    let dups: Vec<String> = sqlx::query_scalar(
        "SELECT commit_id::text FROM save_revision GROUP BY commit_id HAVING count(*) > 1",
    )
    .fetch_all(pool)
    .await?;
    for d in dups {
        r.offend(format!("commit_id used twice: {d}"));
    }
    Ok(r)
}

/// Every revision that keeps its save file with its blob digest, for the fake-gcs check of I1
/// (retention prunes older bundles and the garbage collection deletes their objects, SAVES_API.md
/// §8; the rows stay).
pub async fn blob_refs(pool: &PgPool) -> Result<Vec<BlobRef>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT profile_id::text AS profile, revision, blob_sha256 FROM save_revision \
          WHERE bundle_pruned_at IS NULL ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let digest: Vec<u8> = row.try_get("blob_sha256")?;
        let Ok(blob_sha256) = <[u8; 32]>::try_from(digest.as_slice()) else {
            continue;
        };
        out.push(BlobRef {
            profile_id: row.try_get("profile")?,
            revision: row.try_get("revision")?,
            blob_sha256,
        });
    }
    Ok(out)
}

/// I1 (journal part): every commit the client saw acknowledged exists, with its key as
/// `commit_id` and the revision the answer named.
pub async fn commits_acknowledged(pool: &PgPool, acks: &[Ack]) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I1", "no lost save revision");
    let keys: Vec<String> = acks.iter().map(|a| a.commit_id.clone()).collect();
    let rows = sqlx::query(
        "SELECT commit_id::text AS key, profile_id::text AS profile, revision FROM save_revision \
         WHERE commit_id = ANY($1::text[]::uuid[])",
    )
    .bind(&keys)
    .fetch_all(pool)
    .await?;
    let mut found: BTreeMap<String, (String, i64)> = BTreeMap::new();
    for row in rows {
        found.insert(
            row.try_get("key")?,
            (row.try_get("profile")?, row.try_get("revision")?),
        );
    }
    let distinct: BTreeSet<&str> = acks.iter().map(|a| a.commit_id.as_str()).collect();
    r.count("acknowledged", distinct.len() as i64);
    for a in acks {
        match found.get(&a.commit_id) {
            None => r.offend(format!(
                "acknowledged commit missing: key {} profile {}",
                a.commit_id, a.profile_id
            )),
            Some((p, rev)) if *p != a.profile_id || *rev != a.revision => r.offend(format!(
                "acknowledged commit {} answered revision {} of {} but is revision {rev} of {p}",
                a.commit_id, a.revision, a.profile_id
            )),
            Some(_) => {}
        }
    }
    Ok(r)
}

/// I2 (record part): per profile the grant tokens (`lease_granted`, `lease_taken_over`,
/// `lease_resumed`) strictly increase; every revision carries the token (and session) of the grant
/// in force when it was committed, so no revision was written under a token older than a later
/// grant. "In force" is read from the audit order: grants and commits both write `save_event`
/// under the profile's head lock, so their ids are their order.
pub async fn writers(pool: &PgPool) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I2", "never two writers");
    let grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM save_event WHERE kind IN ('lease_granted', 'lease_taken_over', 'lease_resumed')",
    )
    .fetch_one(pool)
    .await?;
    r.count("grants", grants);
    let takeovers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM save_event WHERE kind = 'lease_taken_over'")
            .fetch_one(pool)
            .await?;
    r.count("takeovers", takeovers);
    let back = sqlx::query(
        "SELECT profile_id::text AS profile, id, fence_token, prev FROM ( \
           SELECT profile_id, id, fence_token, \
                  lag(fence_token) OVER (PARTITION BY profile_id ORDER BY id) AS prev \
           FROM save_event WHERE kind IN ('lease_granted', 'lease_taken_over', 'lease_resumed')) g \
         WHERE prev IS NOT NULL AND fence_token <= prev ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    for row in back {
        let profile: String = row.try_get("profile")?;
        let token: i64 = row.try_get("fence_token")?;
        let prev: i64 = row.try_get("prev")?;
        r.offend(format!(
            "token went back {prev} -> {token}: profile {profile}"
        ));
    }
    let rows = sqlx::query(
        "SELECT v.profile_id::text AS profile, v.revision, v.fence_token, v.session_id::text AS session, \
                g.fence_token AS in_force, g.session_id::text AS in_force_session, c.id IS NOT NULL AS audited \
         FROM save_revision v \
         LEFT JOIN LATERAL ( \
           SELECT id FROM save_event e \
           WHERE e.profile_id = v.profile_id AND e.kind = 'commit' AND e.revision = v.revision \
           ORDER BY id LIMIT 1) c ON true \
         LEFT JOIN LATERAL ( \
           SELECT fence_token, session_id FROM save_event e \
           WHERE e.profile_id = v.profile_id \
             AND e.kind IN ('lease_granted', 'lease_taken_over', 'lease_resumed') \
             AND (CASE WHEN c.id IS NOT NULL THEN e.id < c.id ELSE e.occurred_at <= v.committed_at END) \
           ORDER BY e.id DESC LIMIT 1) g ON true \
         ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?;
    let mut checked = 0i64;
    for row in rows {
        let profile: String = row.try_get("profile")?;
        let revision: i64 = row.try_get("revision")?;
        let token: i64 = row.try_get("fence_token")?;
        let session: String = row.try_get("session")?;
        let in_force: Option<i64> = row.try_get("in_force")?;
        let in_force_session: Option<String> = row.try_get("in_force_session")?;
        checked += 1;
        match in_force {
            None => r.offend(format!(
                "revision {revision} of {profile} has no grant before it"
            )),
            Some(t) if t != token => r.offend(format!(
                "revision {revision} of {profile} written with token {token} while {t} was in force"
            )),
            Some(_) if in_force_session.as_deref() != Some(session.as_str()) => r.offend(format!(
                "revision {revision} of {profile} written by session {session}, lease held by {}",
                in_force_session.unwrap_or_default()
            )),
            Some(_) => {}
        }
    }
    r.count("revisions_checked", checked);
    Ok(r)
}

/// I2 (journal part): every `SAVES.LEASE_LOST` the client got is explained by a grant of the
/// profile to another session after this session's own first grant.
pub async fn lease_lost_explained(
    pool: &PgPool,
    lost: &[LostLease],
) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I2", "never two writers");
    r.count("lease_lost_answers", lost.len() as i64);
    for l in lost {
        let explained: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
               SELECT 1 FROM save_event o \
               WHERE o.profile_id = $1::uuid \
                 AND o.kind IN ('lease_granted', 'lease_taken_over', 'lease_resumed') \
                 AND o.session_id IS DISTINCT FROM $2::uuid \
                 AND o.id > coalesce((SELECT min(id) FROM save_event s WHERE s.profile_id = $1::uuid \
                                      AND s.session_id = $2::uuid \
                                      AND s.kind IN ('lease_granted', 'lease_taken_over', 'lease_resumed')), 0))",
        )
        .bind(&l.profile_id)
        .bind(&l.session_id)
        .fetch_one(pool)
        .await?;
        if !explained {
            r.offend(format!(
                "LEASE_LOST without a later grant to another session: session {} profile {}",
                l.session_id, l.profile_id
            ));
        }
    }
    Ok(r)
}

/// I3 (record part): one `session_summary` per closed session; `profile_progress.races` = the
/// race facts per profile (`fi_db::progression::race_fact`).
pub async fn consolidation(pool: &PgPool) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I3", "every fact consolidated exactly once");
    let closed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM play_session WHERE ended_at IS NOT NULL")
            .fetch_one(pool)
            .await?;
    let summaries: i64 = sqlx::query_scalar("SELECT count(*) FROM session_summary")
        .fetch_one(pool)
        .await?;
    r.count("closed_sessions", closed);
    r.count("session_summaries", summaries);
    let unsummarized: Vec<String> = sqlx::query_scalar(
        // A deleted profile's session is never summarized after its erasure (the nightly skips it).
        "SELECT s.id::text FROM play_session s WHERE s.ended_at IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM session_summary m WHERE m.play_session_id = s.id) \
         AND NOT EXISTS (SELECT 1 FROM profile p WHERE p.id = s.profile_id AND p.status = 'deleted') \
         ORDER BY s.ended_at",
    )
    .fetch_all(pool)
    .await?;
    for s in unsummarized {
        r.offend(format!("closed session without a summary: {s}"));
    }
    let early: Vec<String> = sqlx::query_scalar(
        "SELECT m.play_session_id::text FROM session_summary m JOIN play_session s ON s.id = m.play_session_id \
         WHERE s.ended_at IS NULL",
    )
    .fetch_all(pool)
    .await?;
    for s in early {
        r.offend(format!("summary of a session that is still open: {s}"));
    }
    let races = sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH f AS ( \
           SELECT e.profile_id, count(*) AS n FROM progression_event e WHERE {race} \
           GROUP BY e.profile_id) \
         SELECT coalesce(f.profile_id, p.profile_id)::text AS profile, coalesce(f.n, 0) AS facts, \
                coalesce(p.races, 0)::bigint AS races \
         FROM f FULL JOIN profile_progress p ON p.profile_id = f.profile_id \
         WHERE coalesce(f.n, 0) <> coalesce(p.races, 0) ORDER BY 1",
        race = crate::progression::race_fact()
    )))
    .fetch_all(pool)
    .await?;
    for row in races {
        let profile: String = row.try_get("profile")?;
        let facts: i64 = row.try_get("facts")?;
        let races: i64 = row.try_get("races")?;
        r.offend(format!(
            "profile_progress.races {races} != {facts} race facts: {profile}"
        ));
    }
    let accepted: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM progression_event e WHERE {}",
        crate::progression::race_fact()
    )))
    .fetch_one(pool)
    .await?;
    r.count("accepted_races", accepted);
    Ok(r)
}

/// I3 (journal part): the facts recorded for the sessions a run played equal the distinct
/// `X-Fh2-Request-Id` values it sent and saw answered (201 or 200), retries and duplicates included.
pub async fn facts_recorded(
    pool: &PgPool,
    sessions: &[String],
    distinct_sent: i64,
) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I3", "every fact consolidated exactly once");
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM progression_event WHERE play_session_id = ANY($1::text[]::uuid[])",
    )
    .bind(sessions)
    .fetch_one(pool)
    .await?;
    r.count("facts_sent", distinct_sent);
    r.count("facts_recorded", rows);
    if rows != distinct_sent {
        r.offend(format!(
            "{rows} progression_event rows for the run's sessions, {distinct_sent} distinct request ids sent"
        ));
    }
    Ok(r)
}

/// I4 (platform v1): Supporters and free players each on their own pool and service.
///
/// - Every admitted session's pool serves the tier its account had when the session was created
///   (the gate reads the tier at create, ADR 0015 §2; the local `entitlement` stand-in: VIP iff
///   `status IN ('active', 'grace')` and `access_until > created_at`): no VIP session in a free
///   pool (`matchmaking`), no free session in `vip`.
/// - Free sessions are never served by saves (OD-24: no save line, no game plane there): no save
///   line on the row, no progression fact, no revision and no `save_event` of any kind (lease,
///   commit, refusal, download, ...) of the session — the durable traces only the Supporters'
///   plane and the save line write. `saves_instances` (the game replicas that stamped
///   `X-Fi-Pool: vip`) is only counted: since ADR 0023 one fi-game instance serves both pools.
/// - `game_presence.pool` equals the session's pool: the plane a request was served on follows
///   the ticket's pool, so an instance serving two pools is not an offence.
pub async fn pools(pool: &PgPool, saves_instances: &[String]) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I4", "VIP and free on their own services");
    let entitlement = table_exists(pool, "public.entitlement").await?;
    let vip_at = if entitlement {
        "EXISTS (SELECT 1 FROM entitlement e WHERE e.account_id = s.account_id AND e.product = 'vip' \
           AND e.status IN ('active', 'grace') AND e.access_until > s.created_at)"
    } else {
        "false"
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT s.id::text AS session, s.pool, p.tier AS pool_tier, \
                CASE WHEN {vip_at} THEN 'vip' ELSE 'free' END AS tier \
         FROM play_session s JOIN pool p ON p.name = s.pool \
         WHERE s.admitted_at IS NOT NULL ORDER BY s.admitted_at"
    )))
    .fetch_all(pool)
    .await?;
    let mut by_pool: BTreeMap<String, i64> = BTreeMap::new();
    for row in &rows {
        let session: String = row.try_get("session")?;
        let pool_name: String = row.try_get("pool")?;
        let pool_tier: String = row.try_get("pool_tier")?;
        let tier: String = row.try_get("tier")?;
        *by_pool.entry(pool_name.clone()).or_default() += 1;
        // A game with its own pool takes every tier there (CR-S97).
        let own = fi_domain::GameRules::of(&pool_name).pools == fi_domain::game::Pools::Own;
        if pool_tier != tier && !own {
            r.offend(format!(
                "{tier} session {session} admitted to pool {pool_name} ({pool_tier})"
            ));
        }
    }
    for (p, n) in by_pool {
        r.count(format!("admitted_{p}"), n);
    }
    if !entitlement {
        r.count("entitlement_table", 0);
    }

    // Free sessions never served by saves: what only saves writes, per session of a free pool.
    let free =
        "SELECT s.id FROM play_session s JOIN pool p ON p.name = s.pool WHERE p.tier <> 'vip'";
    let free_sessions: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM ({free}) f"
    )))
    .fetch_one(pool)
    .await?;
    r.count("free_sessions", free_sessions);
    let checks: [(&str, String); 4] = [
        (
            "free session with a save line",
            format!("SELECT s.id::text FROM play_session s WHERE s.save_line AND s.id IN ({free})"),
        ),
        (
            "progression fact of a free session",
            format!(
                "SELECT DISTINCT e.play_session_id::text FROM progression_event e WHERE e.play_session_id IN ({free})"
            ),
        ),
        (
            "save revision of a free session",
            format!(
                "SELECT DISTINCT v.session_id::text FROM save_revision v WHERE v.session_id IN ({free})"
            ),
        ),
        (
            "save event of a free session",
            format!(
                "SELECT e.session_id::text || ' (' || string_agg(DISTINCT e.kind, ',') || ')' FROM save_event e \
                 WHERE e.session_id IN ({free}) GROUP BY e.session_id"
            ),
        ),
    ];
    for (what, sql) in checks {
        let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await?;
        for id in ids {
            r.offend(format!("{what}: {id}"));
        }
    }

    if table_exists(pool, "public.game_presence").await? {
        let presence: i64 = sqlx::query_scalar("SELECT count(*) FROM game_presence")
            .fetch_one(pool)
            .await?;
        r.count("presence", presence);
        let wrong = sqlx::query(
            "SELECT g.session_id::text AS session, g.pool AS seen, s.pool AS admitted, g.instance \
             FROM game_presence g JOIN play_session s ON s.id = g.session_id WHERE g.pool <> s.pool",
        )
        .fetch_all(pool)
        .await?;
        for row in wrong {
            let session: String = row.try_get("session")?;
            let seen: String = row.try_get("seen")?;
            let admitted: String = row.try_get("admitted")?;
            let instance: String = row.try_get("instance")?;
            r.offend(format!(
                "session {session} of pool {admitted} seen on {seen} instance {instance}"
            ));
        }
        r.count("saves_instances", saves_instances.len() as i64);
        // ADR 0023 (OD-122): one fi-game instance serves both pools and picks the plane by the
        // ticket's pool, so the separation is the pool each request was served on (the `wrong`
        // rows above) and the free sessions' absent save data, not which instance answered.
        let instances = sqlx::query(
            "SELECT pool, count(DISTINCT instance) AS n FROM game_presence GROUP BY pool ORDER BY pool",
        )
        .fetch_all(pool)
        .await?;
        for row in instances {
            let p: String = row.try_get("pool")?;
            let n: i64 = row.try_get("n")?;
            r.count(format!("instances_{p}"), n);
        }
    } else {
        r.count("presence_table", 0);
    }
    Ok(r)
}

/// The slot length of a session row `{s}`, as the gate reads it (`lease_ttl_ms`: the lease of a
/// Supporter, the free liveness window of a free session, OD-40; else the default lease).
fn ttl(alias: &str) -> String {
    format!(
        "make_interval(secs => COALESCE({alias}.lease_ttl_ms, {}) / 1000.0)",
        fi_saves::lease::DEFAULT_TTL_MS
    )
}

/// When an admitted session stopped holding its pool slot, by the gate's semantics (ADR 0015;
/// `fi_db::admission`): a slot is held while the row is `active`, and the admission that reuses a
/// lapsed slot closes the stale row inside its own transaction, after reading the clock it
/// stamps as the new `admitted_at`. So the row's `ended_at` lies a moment after that admission,
/// and the slot end is when the slot really became free:
///
/// - `expired`: the later of its two liveness signals, `expires_at` (heartbeat) and
///   `last_seen_at + lease` (a game request; GATE_API.md §4; for a free session the lease is its
///   liveness window, OD-40), capped by `ended_at`. A slot kept alive past `expires_at` by game
///   requests is counted until then, so a reuse while it was alive is seen.
/// - `takeover`: the replacing session's admission (or queueing) instant, capped by `ended_at`
///   (the replacement is the account's first session created at or after `ended_at`).
/// - `queue_left` (never held a slot, but waited ahead in the line): its lapse, `expires_at`.
/// - `max_age`: `max_until`.
/// - otherwise (`game_exit`, `player`, `revoked`): `ended_at`, written in its own transaction.
///
/// `NULL` while it holds it. `least` and `greatest` ignore `NULL`s.
fn slot_end(alias: &str) -> String {
    let s = alias;
    format!(
        "CASE WHEN {s}.ended_at IS NULL THEN NULL \
              WHEN {s}.state = 'expired' THEN least({s}.ended_at, greatest({s}.expires_at, {s}.last_seen_at + {ttl})) \
              WHEN {s}.end_reason = 'takeover' THEN least({s}.ended_at, \
                (SELECT COALESCE(n.admitted_at, n.queued_at, n.created_at) FROM play_session n \
                 WHERE n.account_id = {s}.account_id AND n.id <> {s}.id AND n.created_at >= {s}.ended_at \
                   AND n.dedicated = {s}.dedicated \
                 ORDER BY n.created_at LIMIT 1)) \
              WHEN {s}.end_reason = 'queue_left' THEN least({s}.ended_at, {s}.expires_at) \
              WHEN {s}.end_reason = 'max_age' THEN least({s}.ended_at, {s}.max_until) \
              ELSE {s}.ended_at END",
        ttl = ttl(s)
    )
}

/// Seconds past the end of its live window (both signals, a live save lease, `max_until`) after
/// which an `active` row is late: the local gate sweeper runs every 5 s (`gate-sweep --every 5`).
pub const LATE_AFTER_SECS: i64 = 15;

/// I5: for every pool and every instant the admitted sessions holding a slot never exceed
/// `capacity` (sweep over admission and [`slot_end`], a slot freed and reused at the same instant
/// counted freed first); nobody was admitted while sessions queued ahead of it waited for the slots
/// it took (FIFO by `queued_at`); no session is still `active` [`LATE_AFTER_SECS`] after its slot
/// lapsed on every signal the gate counts (`expires_at`, `last_seen_at + lease`, a live save
/// lease of the session) or after `max_until`.
pub async fn admission(pool: &PgPool) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I5", "queue and admission honoured");
    let peaks = sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH ends AS (SELECT s.id, s.pool, s.admitted_at, {end} AS slot_end FROM play_session s \
                       WHERE s.admitted_at IS NOT NULL AND NOT s.dedicated), \
         ev AS ( \
           SELECT pool, admitted_at AS t, 1 AS d, id FROM ends \
           UNION ALL \
           SELECT pool, slot_end AS t, -1 AS d, id FROM ends WHERE slot_end IS NOT NULL), \
         run AS ( \
           SELECT pool, t, sum(d) OVER (PARTITION BY pool ORDER BY t, d ROWS UNBOUNDED PRECEDING) AS live \
           FROM ev) \
         SELECT p.name AS pool, p.capacity, coalesce(max(run.live), 0)::bigint AS peak, \
                (array_agg(run.t::text ORDER BY run.live DESC, run.t))[1] AS at \
         FROM pool p LEFT JOIN run ON run.pool = p.name GROUP BY p.name, p.capacity ORDER BY p.name",
        end = slot_end("s")
    )))
    .fetch_all(pool)
    .await?;
    for row in peaks {
        let pool_name: String = row.try_get("pool")?;
        let capacity: Option<i32> = row.try_get("capacity")?;
        let peak: i64 = row.try_get("peak")?;
        let at: Option<String> = row.try_get("at")?;
        r.count(format!("peak_{pool_name}"), peak);
        if let Some(c) = capacity {
            r.count(format!("capacity_{pool_name}"), i64::from(c));
            if peak > i64::from(c) {
                r.offend(format!(
                    "pool {pool_name}: {peak} sessions held slots at {} (capacity {c})",
                    at.unwrap_or_default()
                ));
            }
        }
    }
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM play_session WHERE queued_at IS NOT NULL")
            .fetch_one(pool)
            .await?;
    let queued_admitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM play_session WHERE queued_at IS NOT NULL AND admitted_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    r.count("queued", queued);
    r.count("queued_then_admitted", queued_admitted);
    // A session b admitted at T took a slot from the sessions queued ahead of it (queued before b
    // joined the line, still waiting at T) when they were at least as many as the free slots at T.
    let jumps = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT b.id::text AS session, b.pool, b.admitted_at::text AS at, p.capacity, \
           (SELECT count(*) FROM play_session a WHERE a.pool = b.pool AND a.id <> b.id AND NOT a.dedicated \
              AND a.queued_at IS NOT NULL AND a.queued_at < coalesce(b.queued_at, b.admitted_at) \
              AND (a.admitted_at IS NULL OR a.admitted_at > b.admitted_at) \
              AND ({end_a} IS NULL OR {end_a} > b.admitted_at)) AS ahead, \
           (SELECT count(*) FROM play_session c WHERE c.pool = b.pool AND c.id <> b.id AND NOT c.dedicated \
              AND c.admitted_at IS NOT NULL AND c.admitted_at <= b.admitted_at \
              AND ({end_c} IS NULL OR {end_c} > b.admitted_at)) AS holding \
         FROM play_session b JOIN pool p ON p.name = b.pool \
         WHERE b.admitted_at IS NOT NULL AND NOT b.dedicated AND p.capacity IS NOT NULL ORDER BY b.admitted_at",
        end_a = slot_end("a"),
        end_c = slot_end("c")
    )))
    .fetch_all(pool)
    .await?;
    for row in jumps {
        let session: String = row.try_get("session")?;
        let pool_name: String = row.try_get("pool")?;
        let at: String = row.try_get("at")?;
        let capacity: i32 = row.try_get("capacity")?;
        let ahead: i64 = row.try_get("ahead")?;
        let holding: i64 = row.try_get("holding")?;
        let free = i64::from(capacity) - holding;
        if ahead > 0 && ahead >= free {
            r.offend(format!(
                "session {session} admitted to {pool_name} at {at} past {ahead} waiting ahead ({free} free)"
            ));
        }
    }
    let late = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT s.id::text AS session, s.pool, s.expires_at::text AS exp, s.last_seen_at::text AS seen, \
                s.max_until::text AS max FROM play_session s \
         WHERE s.state = 'active' AND ( \
           s.max_until + interval '{late} seconds' < clock_timestamp() \
           OR (s.expires_at + interval '{late} seconds' < clock_timestamp() \
               AND (s.last_seen_at IS NULL OR s.last_seen_at + {ttl} + interval '{late} seconds' < clock_timestamp()) \
               AND NOT EXISTS (SELECT 1 FROM save_head h WHERE h.profile_id = s.profile_id AND h.lease_session_id = s.id \
                               AND h.lease_expires_at + interval '{late} seconds' >= clock_timestamp())))",
        late = LATE_AFTER_SECS,
        ttl = ttl("s")
    )))
    .fetch_all(pool)
    .await?;
    for row in late {
        let session: String = row.try_get("session")?;
        let pool_name: String = row.try_get("pool")?;
        let exp: String = row.try_get("exp")?;
        let seen: Option<String> = row.try_get("seen")?;
        let max: Option<String> = row.try_get("max")?;
        r.offend(format!(
            "session {session} of {pool_name} still active {LATE_AFTER_SECS} s past its slot (expires_at {exp}, last_seen_at {}, max_until {})",
            seen.unwrap_or_default(),
            max.unwrap_or_default()
        ));
    }
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM play_session WHERE state = 'active'")
            .fetch_one(pool)
            .await?;
    r.count("active_now", active);
    Ok(r)
}

/// I5 (journal part): every session the client left normally (`leave` answered 204, and no
/// answer of the run said the profile was taken) was closed by that leave: `state = 'ended'`
/// with `end_reason` `game_exit` or `player`. A leave is idempotent, so its 204 alone does not
/// prove the session was live until then; a session the sweeper expired (its heartbeats lost) or
/// another launcher took over shows here.
pub async fn left(pool: &PgPool, sessions: &[String]) -> Result<Report, sqlx::Error> {
    let mut r = Report::new("I5", "queue and admission honoured");
    r.count("left", sessions.len() as i64);
    let rows = sqlx::query(
        "SELECT q.id AS session, s.state, s.end_reason FROM unnest($1::text[]) AS q (id) \
         LEFT JOIN play_session s ON s.id = q.id::uuid ORDER BY 1",
    )
    .bind(sessions)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let session: String = row.try_get("session")?;
        let state: Option<String> = row.try_get("state")?;
        let reason: Option<String> = row.try_get("end_reason")?;
        let by_leave = state.as_deref() == Some("ended")
            && matches!(reason.as_deref(), Some("game_exit" | "player"));
        if !by_leave {
            r.offend(format!(
                "session {session} left by its launcher is {} ({})",
                state.as_deref().unwrap_or("missing"),
                reason.as_deref().unwrap_or("-")
            ));
        }
    }
    Ok(r)
}

/// Database work counters for a load measurement (`chaos game-load`): reads of `play_session`
/// (index plus sequential scans, what the game plane's liveness check costs) and transactions of
/// the current database. Postgres flushes these statistics within about a second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkCounters {
    /// `pg_stat_user_tables.idx_scan + seq_scan` of `play_session`.
    pub play_session_reads: i64,
    /// `pg_stat_database.xact_commit + xact_rollback` of this database.
    pub transactions: i64,
    /// Connections open to this database now (`numbackends`).
    pub connections: i64,
}

/// Reads [`WorkCounters`].
pub async fn work_counters(pool: &PgPool) -> Result<WorkCounters, sqlx::Error> {
    let reads: Option<i64> = sqlx::query_scalar(
        "SELECT (coalesce(idx_scan, 0) + coalesce(seq_scan, 0))::bigint FROM pg_stat_user_tables          WHERE relname = 'play_session'",
    )
    .fetch_optional(pool)
    .await?;
    let row = sqlx::query(
        "SELECT (xact_commit + xact_rollback)::bigint AS tx, numbackends::bigint AS conns          FROM pg_stat_database WHERE datname = current_database()",
    )
    .fetch_one(pool)
    .await?;
    Ok(WorkCounters {
        play_session_reads: reads.unwrap_or(0),
        transactions: row.try_get("tx")?,
        connections: row.try_get("conns")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_fails_with_its_first_offenders_listed_and_all_counted() {
        let mut r = Report::new("I1", "no lost save revision");
        r.count("revisions", 3);
        assert!(r.ok);
        assert_eq!(r.line(), "I1 PASS no lost save revision: revisions=3");
        for i in 0..(MAX_OFFENDERS + 5) {
            r.offend(format!("gap {i}"));
        }
        assert!(!r.ok);
        assert_eq!(r.offenders.len(), MAX_OFFENDERS);
        assert_eq!(r.offender_total, MAX_OFFENDERS + 5);
        assert!(r.line().starts_with(
            "I1 FAIL no lost save revision: revisions=3 | 25 offender(s): gap 0; gap 1"
        ));
        assert!(r.line().ends_with(" ..."));
        let mut other = Report::new("I1", "no lost save revision");
        other.count("acknowledged", 2);
        let mut ok = Report::new("I1", "x");
        ok.merge(other);
        assert!(ok.ok);
        assert_eq!(ok.get("acknowledged"), Some(2));
        ok.merge(r);
        assert!(!ok.ok);
        assert_eq!(ok.offender_total, MAX_OFFENDERS + 5);
    }

    #[test]
    fn the_slot_end_follows_the_gates_signals() {
        let c = slot_end("c");
        assert!(c.contains(
            "WHEN c.state = 'expired' THEN least(c.ended_at, greatest(c.expires_at, c.last_seen_at + make_interval(secs => COALESCE(c.lease_ttl_ms, 300000) / 1000.0)))"
        ), "{c}");
        assert!(c.contains("WHEN c.end_reason = 'takeover' THEN least(c.ended_at,"));
        assert!(c.contains(
            "n.account_id = c.account_id AND n.id <> c.id AND n.created_at >= c.ended_at"
        ));
        // OD-109: a dedicated session never stands for an ordinary one's replacement.
        assert!(c.contains("AND n.dedicated = c.dedicated"));
        assert!(
            c.contains("WHEN c.end_reason = 'queue_left' THEN least(c.ended_at, c.expires_at)")
        );
        assert!(c.ends_with("ELSE c.ended_at END"));
        assert!(!c.contains("{s}") && !slot_end("a").contains("c."));
    }
}
