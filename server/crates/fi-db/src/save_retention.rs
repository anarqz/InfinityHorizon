//! Save retention and the two-phase garbage collection of save bundles (SAVES_API.md §8,
//! CLOUD_SAVES.md §6 and §8; owner 2026-10-26 "keep 10 save revisions per line").
//!
//! **Retention prunes bundles, never rows.** Every revision row stays (its numbers, digests, kind
//! and play time, ~400 bytes): the guardrails' rate replay, the gates' base and windows, the
//! replaced-career check and the history's contiguity read rows, so pruning changes none of them.
//! What retention gives up is a revision's **save file**: `save_revision.bundle_pruned_at` is set,
//! the revision can no longer be downloaded or restored (`SAVES.REVISION_NOT_FOUND`), and its
//! object becomes garbage once no unpruned revision, parked bundle, seed or ghost names it.
//!
//! - [`prune`] (`fi-cli saves-retention`, nightly): deletes expired parked bundles nothing names,
//!   then, line by line, marks the bundle of every revision outside [`kept_ctes`] pruned: the
//!   newest [`KEEP_NEWEST`] revisions plus the revisions the save rules or the save audit still
//!   read keep theirs. One transaction per line, holding the line's head row (`FOR UPDATE SKIP
//!   LOCKED`: a line in the middle of a commit is left for the next run), so no commit or restore
//!   lands between the keep set's computation and the update. Batches of [`BATCH`] rows.
//!   Idempotent. One `save_event` of kind `pruned` per line it pruned (no outbox event). A line
//!   that fails is logged and counted ([`Pruned::failed`]); the run goes on.
//! - [`mark`] and [`sweep`] (`fi-cli saves-gc`, nightly): phase 1 marks every `save_blob` row no
//!   unpruned revision, parked bundle, seed (active or retired) or ghost recording names
//!   ([`UNREFERENCED`]; ghosts share the bucket's `blobs/sha256/<hex>` names) and clears the mark
//!   of a marked row referenced again; phase 2, per row marked longer than [`GC_GRACE_HOURS`],
//!   under the row's lock and only while it is still marked and unreferenced, deletes the object,
//!   then the row when no revision row names it at all, else records `object_deleted_at` on the
//!   row (it stays marked: the pruned revisions' foreign key keeps it). A commit, park or seed
//!   import that needs a marked row clears the mark and puts the object again
//!   (`saves::insert_blob`).
//!
//! Both run as a `fi_maintenance` login (`fi_jobs`): `SELECT, DELETE` on `save_revision`,
//! `save_parked`, `save_blob`; `UPDATE (bundle_pruned_at)` on `save_revision`; `UPDATE
//! (gc_marked_at, object_deleted_at)` on `save_blob`; `UPDATE (revision, reset_revision)` on
//! `save_head` (what `FOR UPDATE` needs); `INSERT` on `save_event`; `SELECT` on `save_seed`,
//! `ghost_blob`, `profile`, `play_session`, `session_summary`, `integrity_assessment`; `SELECT
//! (parked_id)` on `integrity_strike` (migration `20261026170000_saves_retention.sql`).

use fi_saves::Digest;
use sqlx::PgPool;
use std::future::Future;

/// The newest revisions every line keeps the save file of (owner 2026-10-26).
pub const KEEP_NEWEST: i64 = 10;
/// Rows per update, delete or mark statement.
pub const BATCH: i64 = 1_000;
/// Lines read per page.
const LINES_PER_PAGE: i64 = 200;
/// A blob row is swept no sooner than this many hours after it was marked.
pub const GC_GRACE_HOURS: i32 = 24;
/// At most this many marked rows are swept per run (the next run continues).
pub const SWEEP_LIMIT: i64 = 20_000;
/// The save audit's queue looks back this many days (`integrity::audit_due_in`'s maximum).
pub const AUDIT_DAYS: i32 = 90;
/// A retired line (`save_head.retired_at`), or a lapsed line (`lapsed_at`) whose profile does not
/// keep a line now, keeps the rule this many days, then every bundle is pruned and only the records
/// stay (owner 2026-10-26).
pub const LAPSE_DAYS: i32 = 90;

/// The guardrails' rate anchors (`fi_saves::CommitKind::is_rate_anchor`; the
/// `kept_anchors_match_the_model` test keeps them equal, as `saves::RATE_ANCHORS`).
const ANCHORS: &str = "'fresh_career', 'beta_import', 'moderation_reset', 'season_reset', \
    'career_reset', 'parked_accept', 'fresh_start', 'seed_from_profile'";

/// The guardrails' two windows (`fi_saves::service::RATE_WINDOW_MS`, `SLOW_WINDOW_MS`).
const WINDOWS_MS: [u64; 2] = [
    fi_saves::service::RATE_WINDOW_MS,
    fi_saves::service::SLOW_WINDOW_MS,
];

/// The revisions of one line (`$1`, the profile) that keep their save file, as common table
/// expressions ending in `kept (revision)`: the one place the retention rule lives (SAVES_API.md
/// §8). A revision outside it has its bundle pruned (its row stays).
///
/// | CTE | Kept | Why |
/// | --- | --- | --- |
/// | `newest` | the newest 10 | owner 2026-10-26 |
/// | `rules` (`h`) | the head; the reset revision and the one before it | the current save; a restore behind the reset is support's, the replaced career is the PC's |
/// | `base` | the newest readable revision at or below the head | the gates' base (`SaveService::gate_base`) |
/// | `anchor`, `head_anchor` | the newest rate anchor at or below the base, and at or below the head | where the gates' rates restart |
/// | `ws` | each window's start at the base's play time, and the oldest readable revision since the anchor | `play_window_start` (6 h and 72 h of play; its fallback) |
/// | `restarts`, `high` | for the base and each window start: the newest anchor or restore at or below it, and the revision with the highest level since then | `level_high_water` |
/// | `rules` (`r`) | revision 1; pinned; `parked_accept` | the line's root (seed or fresh start); the pin; a moderator's decision |
/// | `rules` (`save_parked`) | `based_on_revision` of every parked bundle still stored | the parked bundle's context |
/// | `sessions` | `start_revision` of a queued or active session; start and end of an ended session without its summary | the running session; `consolidate-nightly` |
/// | `unaudited` | a Hardcore revision of the save audit's queue not assessed yet | `fi-cli saves-audit` reads its bundle (`integrity::audit_due_in`) |
/// | `closure` | the source (`restored_from`) of every kept restore, recursively | the restore's lineage |
///
/// A line retired more than [`LAPSE_DAYS`] ago, or lapsed more than [`LAPSE_DAYS`] ago **and whose
/// profile does not keep a line now** (`meets`: since OD-239 every existing profile's line counts
/// as kept, a Noob (Azimuth) line too: no new Noob line is minted, and an old one stays whole for
/// its export, never pruned as "unkept"),
/// is `h.ended`: it keeps only what something outside it needs (`refs`: parked bundles' bases,
/// sessions, the save audit's queue, and their restore sources). A line whose profile keeps a line
/// now always keeps its head's file (`refs`), whatever `lapsed_at` says: the mark is cleared only by
/// the next player commit, so a renewed Supporter may still carry an old one (review N1).
///
/// The rows themselves are never deleted, so the gates read the same history whatever this keeps;
/// the gates' entries keep the bundles of the revisions they measure from, for support.
///
/// The hybrid Hardcore checks (owner 2026-10-26 "no outbox or BigQuery; hybrid Hardcore checks",
/// CLOUD_SAVES.md §7.5) add two entries to `refs`, kept on every line, an ended one too:
///
/// | CTE | Kept | Why |
/// | --- | --- | --- |
/// | `unanalyzed` | a revision queued for the nightly guardrails pass and not analyzed yet (`guard_input` set, `guard_analyzed_at` NULL) | retention never outruns the pass: the evidence of a revision nobody measured stays whole |
/// | `seed_review` | a `seed_from_profile` revision 1 while its account is blocked pending review (`account.ban_review`, CR-S52's seed revoke), until the ban is lifted or the account erased | the reviewed seed's evidence, even past a retired line's 90 days |
/// | `flagged` | a revision the pass flagged (`save_nightly` assessment with decision `flagged`), while that row exists (90 days, `data-retention`) | the moderation queue's evidence |
pub fn kept_ctes() -> String {
    let [fast, slow] = WINDOWS_MS;
    format!(
        "WITH RECURSIVE \
         meets AS (SELECT EXISTS (SELECT 1 FROM profile p WHERE p.id = $1) AS now), \
         h AS (SELECT s.revision AS head, s.reset_revision, s.retired_at IS NOT NULL AS retired, \
                 COALESCE(s.retired_at < now() - make_interval(days => {LAPSE_DAYS}), false) \
                 OR (COALESCE(s.lapsed_at < now() - make_interval(days => {LAPSE_DAYS}), false) \
                     AND NOT m.now) AS ended \
               FROM save_head s, meets m WHERE s.profile_id = $1), \
         r AS (SELECT revision, kind, play_ms, summary, pinned, restored_from, parked_from, committed_at, \
                 guard_input IS NOT NULL AND guard_analyzed_at IS NULL AS unanalyzed, \
                 (jsonb_typeof(summary -> 'credits') = 'number' AND jsonb_typeof(summary -> 'xp') = 'number' \
                  AND jsonb_typeof(summary -> 'level') = 'number') AS readable \
               FROM save_revision WHERE profile_id = $1), \
         newest AS (SELECT revision FROM r ORDER BY revision DESC LIMIT {KEEP_NEWEST}), \
         base AS (SELECT r.revision, r.play_ms FROM r, h WHERE r.readable AND r.revision <= h.head \
                  ORDER BY r.revision DESC LIMIT 1), \
         anchor AS (SELECT COALESCE(max(r.revision), 0) AS revision FROM r, base \
                    WHERE r.kind IN ({ANCHORS}) AND r.revision <= base.revision), \
         head_anchor AS (SELECT max(r.revision) AS revision FROM r, h \
                         WHERE r.kind IN ({ANCHORS}) AND r.revision <= h.head), \
         ws AS ( \
           SELECT (SELECT c.revision FROM r c WHERE c.readable AND c.revision <= b.revision \
                     AND c.revision >= a.revision AND c.play_ms < b.play_ms - w.ms \
                   ORDER BY c.play_ms DESC, c.revision DESC LIMIT 1) AS revision \
             FROM base b, anchor a, (VALUES ({fast}::bigint), ({slow}::bigint)) AS w (ms) \
           UNION ALL \
           SELECT (SELECT c.revision FROM r c WHERE c.readable AND c.revision <= b.revision \
                     AND c.revision >= a.revision ORDER BY c.revision LIMIT 1) \
             FROM base b, anchor a), \
         bounds AS (SELECT revision FROM base UNION SELECT revision FROM ws WHERE revision IS NOT NULL), \
         restarts AS (SELECT x.revision AS x, \
                        COALESCE((SELECT max(r.revision) FROM r WHERE r.revision <= x.revision \
                                  AND r.kind IN ({ANCHORS}, 'restore')), 0) AS restart \
                      FROM bounds x), \
         high AS ( \
           SELECT (SELECT r.revision FROM r WHERE r.revision BETWEEN s.restart AND s.x \
                     AND jsonb_typeof(r.summary -> 'level') = 'number' \
                   ORDER BY (r.summary ->> 'level')::numeric DESC, r.revision DESC LIMIT 1) AS revision \
             FROM restarts s \
           UNION ALL SELECT restart FROM restarts), \
         sessions AS ( \
           SELECT s.start_revision AS revision FROM play_session s \
            WHERE s.profile_id = $1 AND s.state IN ('queued', 'active') \
           UNION ALL \
           SELECT unnest(ARRAY[s.start_revision, s.end_revision]) FROM play_session s \
            WHERE s.profile_id = $1 AND s.ended_at IS NOT NULL \
              AND NOT EXISTS (SELECT 1 FROM session_summary m WHERE m.play_session_id = s.id)), \
         unaudited AS ( \
           SELECT r.revision FROM r JOIN profile p ON p.id = $1 \
            WHERE p.mode = 'hardcore' AND p.status = 'active' \
              AND r.kind NOT IN ('fresh_career', 'fresh_start') \
              AND r.committed_at > now() - make_interval(days => {AUDIT_DAYS}) \
              AND NOT EXISTS (SELECT 1 FROM integrity_assessment a WHERE a.source = 'save_audit' \
                                AND a.profile_id = $1 AND a.revision = r.revision)), \
         unanalyzed AS (SELECT revision FROM r WHERE unanalyzed), \
         seed_review AS (SELECT r.revision FROM r JOIN profile p ON p.id = $1 \
            JOIN account a ON a.id = p.account_id \
            WHERE r.kind = 'seed_from_profile' AND a.ban_review IS NOT NULL \
              AND a.banned_at IS NOT NULL \
              AND (a.banned_until IS NULL OR a.banned_until > clock_timestamp())), \
         flagged AS (SELECT a.revision FROM integrity_assessment a \
            WHERE a.source = 'save_nightly' AND a.profile_id = $1 AND a.decision = 'flagged'), \
         line_rules AS ( \
           SELECT revision FROM newest \
           UNION SELECT head FROM h \
           UNION SELECT reset_revision FROM h \
           UNION SELECT reset_revision - 1 FROM h \
           UNION SELECT revision FROM base \
           UNION SELECT revision FROM anchor \
           UNION SELECT revision FROM head_anchor \
           UNION SELECT revision FROM ws \
           UNION SELECT revision FROM high \
           UNION SELECT 1 \
           UNION SELECT revision FROM r WHERE pinned OR parked_from IS NOT NULL), \
         refs AS ( \
           SELECT h.head AS revision FROM h, meets m WHERE m.now AND NOT h.retired \
           UNION SELECT based_on_revision FROM save_parked WHERE profile_id = $1 \
           UNION SELECT revision FROM sessions \
           UNION SELECT revision FROM unaudited \
           UNION SELECT revision FROM unanalyzed \
           UNION SELECT revision FROM flagged \
           UNION SELECT revision FROM seed_review), \
         rules AS ( \
           SELECT l.revision FROM line_rules l, h WHERE NOT h.ended \
           UNION SELECT revision FROM refs), \
         closure AS ( \
           SELECT revision FROM rules WHERE revision > 0 \
           UNION \
           SELECT r.restored_from FROM closure c JOIN r ON r.revision = c.revision \
            WHERE r.restored_from IS NOT NULL), \
         kept AS (SELECT revision FROM closure) "
    )
}

/// The revisions of `profile`'s line the rule keeps the bundle of (ascending; only those that
/// exist).
pub async fn kept_in(
    conn: &mut sqlx::PgConnection,
    profile: uuid::Uuid,
) -> Result<Vec<i64>, sqlx::Error> {
    let sql = format!(
        "{} SELECT r.revision FROM save_revision r WHERE r.profile_id = $1 \
           AND r.revision IN (SELECT revision FROM kept) ORDER BY r.revision",
        kept_ctes()
    );
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(profile)
        .fetch_all(conn)
        .await
}

/// Whether retention pruned the save file of `profile`'s `revision` (`SaveHead.bundle`, SAVES_API.md
/// §4.1); `false` for a revision that does not exist.
pub async fn bundle_pruned_in(
    conn: &mut sqlx::PgConnection,
    profile: uuid::Uuid,
    revision: u64,
) -> Result<bool, sqlx::Error> {
    let pruned: Option<bool> = sqlx::query_scalar(
        "SELECT bundle_pruned_at IS NOT NULL FROM save_revision WHERE profile_id = $1 AND revision = $2",
    )
    .bind(profile)
    .bind(i64::try_from(revision).unwrap_or(i64::MAX))
    .fetch_optional(conn)
    .await?;
    Ok(pruned.unwrap_or(false))
}

/// What one retention run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Lines that had at least one bundle pruned.
    pub lines: u64,
    /// Revisions whose bundle was pruned (their rows stay).
    pub bundles: u64,
    /// Expired parked bundles deleted.
    pub parked: u64,
    /// Lines skipped because a commit held their head (the next run takes them).
    pub busy: u64,
    /// Lines whose pruning failed (logged; the next run retries them).
    pub failed: u64,
}

/// Expired parked bundles no revision (`parked_accept`) and no integrity strike names.
const PARKED_GONE: &str = "p.expires_at <= clock_timestamp() \
    AND NOT EXISTS (SELECT 1 FROM save_revision r WHERE r.parked_from = p.id) \
    AND NOT EXISTS (SELECT 1 FROM integrity_strike s WHERE s.parked_id = p.id)";

/// Runs retention once over every line. See the
/// module docs.
pub async fn prune(db: &PgPool) -> Result<Pruned, sqlx::Error> {
    let mut out = Pruned::default();
    // 1. Parked bundles past their 90 days first, so a deleted one keeps no bundle below.
    let sql = format!(
        "DELETE FROM save_parked p WHERE p.id IN \
           (SELECT p.id FROM save_parked p WHERE {PARKED_GONE} LIMIT $1) AND {PARKED_GONE}"
    );
    loop {
        let n = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(BATCH)
            .execute(db)
            .await?
            .rows_affected();
        out.parked += n;
        if n < BATCH as u64 {
            break;
        }
    }
    // 2. Lines holding more than 10 bundles.
    let update = format!(
        "{} UPDATE save_revision d SET bundle_pruned_at = clock_timestamp() \
           WHERE d.profile_id = $1 AND d.revision IN \
           (SELECT x.revision FROM save_revision x WHERE x.profile_id = $1 \
              AND x.bundle_pruned_at IS NULL AND x.revision NOT IN (SELECT revision FROM kept) \
            ORDER BY x.revision LIMIT $2)",
        kept_ctes()
    );
    let mut cursor = uuid::Uuid::nil();
    loop {
        let page: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT h.profile_id FROM save_head h WHERE h.profile_id > $1 \
               AND (SELECT count(*) FROM save_revision r \
                     WHERE r.profile_id = h.profile_id AND r.bundle_pruned_at IS NULL) > \
                   CASE WHEN h.retired_at < now() - make_interval(days => $4) \
                          OR h.lapsed_at < now() - make_interval(days => $4) THEN 0 ELSE $2 END \
             ORDER BY h.profile_id LIMIT $3",
        )
        .bind(cursor)
        .bind(KEEP_NEWEST)
        .bind(LINES_PER_PAGE)
        .bind(LAPSE_DAYS)
        .fetch_all(db)
        .await?;
        let Some(last) = page.last() else { break };
        cursor = *last;
        for profile in &page {
            match prune_line(db, &update, *profile).await {
                Ok(None) => out.busy += 1,
                Ok(Some(0)) => {}
                Ok(Some(n)) => {
                    out.lines += 1;
                    out.bundles += n;
                }
                Err(e) => {
                    tracing::warn!(profile = %profile, error = %e, "line not pruned; the next run retries");
                    out.failed += 1;
                }
            }
        }
        if (page.len() as i64) < LINES_PER_PAGE {
            break;
        }
    }
    Ok(out)
}

/// Prunes one line in one transaction; `None` when a commit held its head.
async fn prune_line(
    db: &PgPool,
    update: &str,
    profile: uuid::Uuid,
) -> Result<Option<u64>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let head: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM save_head WHERE profile_id = $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(profile)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(head) = head else {
        return Ok(None);
    };
    let mut pruned = 0u64;
    loop {
        let n = sqlx::query(sqlx::AssertSqlSafe(update))
            .bind(profile)
            .bind(BATCH)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        pruned += n;
        if n < BATCH as u64 {
            break;
        }
    }
    if pruned > 0 {
        sqlx::query(
            "INSERT INTO save_event (profile_id, kind, revision, detail) \
             VALUES ($1, 'pruned', $2, jsonb_build_object('bundles', $3::bigint))",
        )
        .bind(profile)
        .bind(head)
        .bind(pruned as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(pruned))
}

/// No unpruned revision, parked bundle, seed (active or retired) or ghost recording names the
/// blob row `b`.
pub const UNREFERENCED: &str = "NOT EXISTS (SELECT 1 FROM save_revision r \
      WHERE r.blob_sha256 = b.sha256 AND r.bundle_pruned_at IS NULL) \
    AND NOT EXISTS (SELECT 1 FROM save_parked p WHERE p.blob_sha256 = b.sha256) \
    AND NOT EXISTS (SELECT 1 FROM save_seed s WHERE s.blob_sha256 = b.sha256) \
    AND NOT EXISTS (SELECT 1 FROM ghost_blob g WHERE g.sha256 = b.sha256)";

/// What phase 1 did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marked {
    /// Rows newly marked.
    pub marked: u64,
    /// Marked rows referenced again, unmarked.
    pub unmarked: u64,
}

/// Phase 1: marks every unreferenced, unmarked blob row; unmarks a marked row referenced again
/// whose object is still there (a row whose object was deleted is unmarked only by the writer that
/// puts the object back).
pub async fn mark(db: &PgPool) -> Result<Marked, sqlx::Error> {
    let unmarked = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE save_blob b SET gc_marked_at = NULL \
          WHERE b.gc_marked_at IS NOT NULL AND b.object_deleted_at IS NULL AND NOT ({UNREFERENCED})"
    )))
    .execute(db)
    .await?
    .rows_affected();
    let mut out = Marked {
        marked: 0,
        unmarked,
    };
    let sql = format!(
        "UPDATE save_blob b SET gc_marked_at = clock_timestamp() WHERE b.sha256 IN \
           (SELECT b.sha256 FROM save_blob b WHERE b.gc_marked_at IS NULL AND {UNREFERENCED} LIMIT $1) \
         AND b.gc_marked_at IS NULL AND {UNREFERENCED}"
    );
    loop {
        let n = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(BATCH)
            .execute(db)
            .await?
            .rows_affected();
        out.marked += n;
        if n < BATCH as u64 {
            break;
        }
    }
    Ok(out)
}

/// What phase 2 did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Objects deleted.
    pub deleted: u64,
    /// Blob rows deleted (with their object, or tombstones no revision row names any more).
    pub rows: u64,
    /// Rows referenced again or unmarked since the candidate list was read: kept.
    pub kept: u64,
    /// Rows whose object could not be deleted: kept, marked, for the next run.
    pub failed: u64,
}

/// Phase 2: per row marked more than [`GC_GRACE_HOURS`] ago whose object is still there, in its
/// own transaction holding the row's lock, and only while it is still marked past the grace and
/// unreferenced: `delete_object` (the bucket's object; absent is success), then the row when no
/// revision row names it, else `object_deleted_at` (the row stays marked). At most
/// [`SWEEP_LIMIT`] rows.
pub async fn sweep<F, Fut>(db: &PgPool, delete_object: F) -> Result<Swept, sqlx::Error>
where
    F: Fn(Digest) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut out = Swept::default();
    let candidates: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT sha256 FROM save_blob \
          WHERE gc_marked_at < clock_timestamp() - make_interval(hours => $1) \
            AND object_deleted_at IS NULL \
          ORDER BY gc_marked_at LIMIT $2",
    )
    .bind(GC_GRACE_HOURS)
    .bind(SWEEP_LIMIT)
    .fetch_all(db)
    .await?;
    let lock = format!(
        "SELECT b.sha256 FROM save_blob b WHERE b.sha256 = $1 AND b.object_deleted_at IS NULL \
           AND b.gc_marked_at < clock_timestamp() - make_interval(hours => $2) AND {UNREFERENCED} \
         FOR UPDATE OF b"
    );
    for sha in candidates {
        let Ok(bytes) = <[u8; 32]>::try_from(sha.as_slice()) else {
            out.kept += 1;
            continue;
        };
        let mut tx = db.begin().await?;
        let still: Option<Vec<u8>> = sqlx::query_scalar(sqlx::AssertSqlSafe(lock.as_str()))
            .bind(&sha)
            .bind(GC_GRACE_HOURS)
            .fetch_optional(&mut *tx)
            .await?;
        if still.is_none() {
            out.kept += 1;
            continue;
        }
        // The one sanctioned network call inside a transaction (CLOUD_SAVES.md §6): one row lock.
        if let Err(e) = delete_object(Digest(bytes)).await {
            tracing::warn!(blob = %Digest(bytes).hex(), error = %e, "object not deleted; kept for the next run");
            out.failed += 1;
            continue;
        }
        let gone = sqlx::query(
            "DELETE FROM save_blob b WHERE b.sha256 = $1 \
               AND NOT EXISTS (SELECT 1 FROM save_revision r WHERE r.blob_sha256 = b.sha256)",
        )
        .bind(&sha)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if gone == 0 {
            sqlx::query(
                "UPDATE save_blob SET object_deleted_at = clock_timestamp() WHERE sha256 = $1",
            )
            .bind(&sha)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        out.deleted += 1;
        out.rows += gone;
    }
    // Tombstones (object already deleted) no revision row names any more (an erasure deleted
    // them): the row goes too. Deleting takes the row's lock, as an upsert needing it would.
    let sql = format!(
        "DELETE FROM save_blob b WHERE b.sha256 IN \
           (SELECT b.sha256 FROM save_blob b WHERE b.object_deleted_at IS NOT NULL \
              AND NOT EXISTS (SELECT 1 FROM save_revision r WHERE r.blob_sha256 = b.sha256) \
              AND {UNREFERENCED} LIMIT $1) \
         AND b.object_deleted_at IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM save_revision r WHERE r.blob_sha256 = b.sha256) \
         AND {UNREFERENCED}"
    );
    loop {
        let n = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(BATCH)
            .execute(db)
            .await?
            .rows_affected();
        out.rows += n;
        if n < BATCH as u64 {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_saves::CommitKind;

    #[test]
    fn kept_anchors_match_the_model() {
        let listed: Vec<&str> = ANCHORS
            .split(',')
            .map(|s| s.trim().trim_matches('\''))
            .collect();
        let model: Vec<&str> = CommitKind::ALL
            .into_iter()
            .filter(|k| k.is_rate_anchor())
            .map(CommitKind::as_str)
            .collect();
        assert_eq!(
            listed.iter().collect::<std::collections::BTreeSet<_>>(),
            model.iter().collect()
        );
    }

    #[test]
    fn the_rule_keeps_ten_and_names_its_windows() {
        assert_eq!(KEEP_NEWEST, 10);
        let sql = kept_ctes();
        assert!(
            sql.contains("meets AS (SELECT EXISTS"),
            "OD-239: every existing line counts as kept"
        );
        assert!(sql.contains(&format!("LIMIT {KEEP_NEWEST}")));
        assert!(sql.contains(&format!("days => {AUDIT_DAYS}")));
        assert!(sql.contains(&format!("days => {LAPSE_DAYS}")));
        // The hybrid checks: unanalyzed and flagged revisions keep their bundles on every line.
        assert!(sql.contains("guard_input IS NOT NULL AND guard_analyzed_at IS NULL"));
        assert!(sql.contains("UNION SELECT revision FROM unanalyzed"));
        assert!(sql.contains("UNION SELECT revision FROM flagged"));
        for w in WINDOWS_MS {
            assert!(sql.contains(&format!("({w}::bigint)")));
        }
        const { assert!(GC_GRACE_HOURS >= 24) };
    }
}
