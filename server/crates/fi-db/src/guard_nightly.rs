//! The nightly guardrails pass (`fi-cli integrity-nightly`; hybrid Hardcore checks, owner
//! 2026-10-26 "no outbox or BigQuery; hybrid Hardcore checks", CLOUD_SAVES.md §7.5).
//!
//! The upload path refuses only what is cheap and deterministic (unreadable numbers, a career a
//! reset replaced, the game's limits, the fresh-start check, the fact checks) and records, with
//! every gated upload's revision, what the full guardrails need beyond the line's history
//! (`save_revision.guard_input`: the game sync it was compared with). This pass assesses every such
//! revision not analyzed yet with [`fi_saves::nightly_assessment`], the same function the upload
//! path ran inline before (rates replayed from the line's history, ceilings, the anomaly score),
//! records one `integrity_assessment` row per revision (source `save_nightly`, with `revision`)
//! and marks the revision analyzed (`guard_analyzed_at`, set once).
//!
//! **Flags, never refusals.** The revision already is the line's: nothing is deleted, rolled back
//! or restored, the head never moves and no strike is written (the save path never wrote one; the
//! row is the moderator's record, as a parked refusal's was). A revision the inline guardrails
//! would have refused (a ceiling, whatever the switch; a score of 60 or more with
//! `FI_GUARD_ENFORCE` on) is recorded `decision = 'flagged'` with that rule in `refusal`: the
//! moderation queue's entry (`GET /core/v1/admin/assessments`). With the switch off a score of 60
//! or more is `accepted` with `would_block`, as in shadow before. Retention keeps a flagged
//! revision's bundle while its row exists (`save_retention::kept_ctes`).
//!
//! **Batches, resumable.** Lines are paged by profile id from the queue's partial index; a line's
//! assessments read only immutable rows (the revisions at or below each one's base, the parked
//! bundles), outside any transaction, then one transaction per line (per [`REVISIONS_PER_TX`]
//! revisions) inserts the rows and sets the marks: a crash leaves a line wholly analyzed or not,
//! and a rerun skips what is marked (the insert is idempotent too). A line that fails is logged
//! by id and counted, and the next run retries it. Log lines carry counts and ids, never numbers.
//! A line that fails every night stays queued (retention keeps its bundles) and the job exits
//! non-zero every night, so it is never silent: an operator fixes the cause by hand. The pass
//! never marks a revision analyzed without its assessment (no evidence is dropped unread).
//!
//! Runs as a `fi_maintenance` login (`fi_jobs`): `SELECT` on the save tables and `profile`,
//! `UPDATE (guard_analyzed_at)` on `save_revision`, `INSERT` on `integrity_assessment` (migration
//! `20261027160000_integrity_nightly.sql`).

use crate::integrity::{self, NightlyRecord};
use crate::saves::{PgSaveStore, guard_input_from_json};
use fi_domain::ProfileId;
use fi_saves::gates::GatePolicy;
use fi_saves::{Revision, SaveStore};
use sqlx::PgPool;
use uuid::Uuid;

/// Lines read per page.
pub const LINES_PER_PAGE: i64 = 200;
/// Revisions of one line assessed and recorded per transaction (a longer queue takes several).
pub const REVISIONS_PER_TX: i64 = 500;

/// What one pass did (counts only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Analyzed {
    /// Lines with at least one revision analyzed.
    pub lines: u64,
    /// Revisions analyzed (marked).
    pub revisions: u64,
    /// Assessment rows written (a revision whose row a concurrent pass wrote is marked only).
    pub recorded: u64,
    /// Revisions flagged (what the inline guardrails would have refused).
    pub flagged: u64,
    /// Revisions scored 60 or more.
    pub would_block: u64,
    /// Revisions marked without an assessment (not a gated upload after revision 1).
    pub skipped: u64,
    /// Lines whose game has no rules here, left queued.
    pub no_rules: u64,
    /// Lines that failed (logged; the next run retries them).
    pub failed: u64,
}

/// Runs the pass once over every line with a revision not analyzed yet. `policy_for` gives a
/// game's gate policy (its rules and the deployment's `FI_GUARD_ENFORCE`); a game without one is
/// left queued and counted.
pub async fn analyze(
    db: &PgPool,
    policy_for: impl Fn(&str) -> Option<GatePolicy>,
) -> Result<Analyzed, sqlx::Error> {
    let store = PgSaveStore::new(db.clone());
    let mut out = Analyzed::default();
    let mut cursor = Uuid::nil();
    loop {
        // The page is the queue's ids; the game joins without dropping a row, so a short page
        // means the queue's end.
        let page: Vec<(Uuid, Option<String>)> = sqlx::query_as(
            "SELECT q.profile_id, p.game FROM ( \
               SELECT DISTINCT r.profile_id FROM save_revision r \
                WHERE r.guard_input IS NOT NULL AND r.guard_analyzed_at IS NULL \
                  AND r.profile_id > $1 \
                ORDER BY r.profile_id LIMIT $2) q \
             LEFT JOIN profile p ON p.id = q.profile_id ORDER BY q.profile_id",
        )
        .bind(cursor)
        .bind(LINES_PER_PAGE)
        .fetch_all(db)
        .await?;
        let Some((last, _)) = page.last() else { break };
        cursor = *last;
        for (profile, game) in &page {
            let Some(game) = game else {
                tracing::warn!(profile = %profile, "queued line without a profile row");
                out.failed += 1;
                continue;
            };
            let Some(policy) = policy_for(game) else {
                out.no_rules += 1;
                continue;
            };
            match analyze_line(db, &store, &policy, game, *profile).await {
                Ok(line) => {
                    if line.revisions > 0 {
                        out.lines += 1;
                    }
                    out.revisions += line.revisions;
                    out.recorded += line.recorded;
                    out.flagged += line.flagged;
                    out.would_block += line.would_block;
                    out.skipped += line.skipped;
                }
                Err(e) => {
                    tracing::warn!(profile = %profile, error = %e, "line not analyzed; the next run retries");
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

#[derive(Debug, thiserror::Error)]
enum LineError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Store(#[from] fi_saves::SaveError),
    #[error(transparent)]
    Record(#[from] crate::identity::IdentityError),
    #[error("{0}")]
    Corrupt(&'static str),
}

/// Analyzes one line's queue, oldest first, one transaction per [`REVISIONS_PER_TX`] revisions.
async fn analyze_line(
    db: &PgPool,
    store: &PgSaveStore,
    policy: &GatePolicy,
    game: &str,
    profile: Uuid,
) -> Result<Analyzed, LineError> {
    let line = ProfileId::from_uuid(profile).ok_or(LineError::Corrupt("profile id"))?;
    let mut out = Analyzed::default();
    let mut after = 0_i64;
    loop {
        let queued: Vec<(i64, serde_json::Value)> = sqlx::query_as(
            "SELECT revision, guard_input FROM save_revision \
              WHERE profile_id = $1 AND guard_input IS NOT NULL AND guard_analyzed_at IS NULL \
                AND revision > $2 \
              ORDER BY revision LIMIT $3",
        )
        .bind(profile)
        .bind(after)
        .bind(REVISIONS_PER_TX)
        .fetch_all(db)
        .await?;
        let (Some(&(first, _)), Some(&(last, _))) = (queued.first(), queued.last()) else {
            break;
        };
        // The queued revisions' records in one read (newest first, at most the span).
        let span = u32::try_from(last - first + 1).map_err(|_| LineError::Corrupt("revision"))?;
        let records = store
            .history(line, Some(Revision::new(last as u64).next()), span)
            .await?;
        let mut rows = Vec::with_capacity(queued.len());
        for (revision, input) in &queued {
            let record = records
                .iter()
                .find(|r| r.revision.get() == *revision as u64)
                .ok_or(LineError::Corrupt("queued revision missing"))?;
            let input =
                guard_input_from_json(input).ok_or(LineError::Corrupt("guard_input shape"))?;
            let assessment = fi_saves::nightly_assessment(store, policy, record, input).await?;
            rows.push((*revision, record.session.as_uuid(), assessment));
        }
        let mut tx = db.begin().await?;
        let mut marked = Vec::with_capacity(rows.len());
        for (revision, session, assessment) in &rows {
            marked.push(*revision);
            let Some(a) = assessment else {
                out.skipped += 1;
                continue;
            };
            let flag = a.refusal(policy.enforce).map(|r| r.code());
            let recorded = integrity::record_nightly_in(
                &mut tx,
                &NightlyRecord {
                    game,
                    profile_id: profile,
                    revision: *revision,
                    session_id: *session,
                    assessment: a,
                    flag,
                },
            )
            .await?;
            if recorded {
                out.recorded += 1;
                out.flagged += u64::from(flag.is_some());
                out.would_block += u64::from(a.would_block());
            }
        }
        let n = sqlx::query(
            "UPDATE save_revision SET guard_analyzed_at = clock_timestamp() \
              WHERE profile_id = $1 AND revision = ANY($2) AND guard_analyzed_at IS NULL",
        )
        .bind(profile)
        .bind(&marked)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        out.revisions += n;
        after = last;
        if (queued.len() as i64) < REVISIONS_PER_TX {
            break;
        }
    }
    Ok(out)
}
