//! The save audit (OD-62 conservation step 2, OD-67, ADR 0021; CLOUD_SAVES.md §7.3):
//!
//! - `saves-audit [--limit N] [--days D]`: reads the accepted Hardcore revisions committed in the
//!   last `D` days (default 30, at most 90) that have no audit yet, oldest first, at most `N`
//!   (default 200, at most 1,000) per run (`fi_db::integrity::audit_due_in`). For each: fetches
//!   its bundle from the save bucket (`CLOUD_SAVES_BUCKET`; `STORAGE_EMULATOR_HOST` locally),
//!   unpacks it (`fi_saves::bundle::validate`), reads its PlayerDatabase (`fi_playerdb::read`:
//!   pure Rust, read-only, hard limits), judges it (`fi_gamedb::audit`: perks, parts, bought cars,
//!   vouchers) against the game's catalog, part prices and the game's active Hardcore seed, and
//!   records one shadow assessment (`fi_db::integrity::record_audit_in`: source `save_audit`,
//!   never a refusal; a rerun records nothing twice). As the jobs login (`DATABASE_URL`).
//!
//! Each line is judged against the seed it started from (its `fresh_career` revision 1's bundle,
//! `fi_db::seeds::by_blob_in`, active or retired); a line that did not start from a seed (a Beta
//! import) has none and its bought-car rule is off. A seed that exists but cannot be read stops the
//! run. No transaction spans the bucket read. A bundle the bucket cannot answer for is skipped this
//! run (a later run retries it); one it no longer has (`BLOB_MISSING`), one that does not match its
//! digest (`BLOB_DIGEST`) or no longer unpacks (`BUNDLE_INVALID`) is recorded without a score.
//! Logs carry counts and ids only, never a save's values.

use super::Outcome;
use fi_db::integrity::{self, AuditDue, AuditRecord};
use fi_db::{DbConfig, Workload, seeds};
use fi_gamedb::Catalog;
use fi_gamedb::audit::{self, Audit, Numbers, Reference, Seed};
use fi_gamedb::parts::PartPrices;
use fi_saves::{BlobStore, Digest, SaveSummary, bundle};
use fi_storage::Blobs;
use std::collections::BTreeMap;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  saves-audit      [--limit N] [--days D]: audit the PlayerDatabase of accepted Hardcore revisions
                   without an audit (the last D days, default 30; at most N, default 200) and record
                   each in shadow (DATABASE_URL, CLOUD_SAVES_BUCKET, STORAGE_EMULATOR_HOST locally)";

/// Runs `command` with `args`; `None` when it is not the audit.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    Some(match command {
        "saves-audit" => saves_audit(args).await,
        _ => return None,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    limit: i64,
    days: i64,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        limit: 200,
        days: 30,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let (slot, max) = match flag.as_str() {
            "--limit" => (&mut out.limit, 1_000),
            "--days" => (&mut out.days, 90),
            other => return Err(format!("unknown argument {other:?}")),
        };
        *slot = it
            .next()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| (1..=max).contains(v))
            .ok_or_else(|| format!("{flag} takes a number from 1 to {max}"))?;
    }
    Ok(out)
}

/// What one run did, logged at the end.
#[derive(Debug, Default)]
struct Tally {
    recorded: u32,
    already: u32,
    scored: u32,
    would_block: u32,
    unreadable: u32,
    skipped: u32,
}

async fn saves_audit(args: &[String]) -> Outcome {
    let args = parse(args)?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-saves-audit", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let store = Blobs::from_env()?;
    let due = {
        let mut conn = pool.acquire().await?;
        integrity::audit_due_in(&mut conn, args.days, args.limit).await?
    };
    // The seed of each seeded line, by its bundle (a line is judged against the seed it started
    // from, active or retired); lines that did not start from a seed have none (the bought-car
    // rule is off for them).
    let mut seeds_by_blob: BTreeMap<Vec<u8>, Option<Seed>> = BTreeMap::new();
    let mut tally = Tally::default();
    for item in &due {
        if let Some(blob) = &item.seed_blob
            && !seeds_by_blob.contains_key(blob)
        {
            let row = {
                let mut conn = pool.acquire().await?;
                seeds::by_blob_in(&mut conn, &item.game, blob).await?
            };
            let seed = match row {
                // A seed that exists but cannot be read stops the run: recording audits with the
                // rule off would never be redone (one audit per revision).
                Some(s) => Some(seed_of(&store, &s).await?),
                None => None,
            };
            seeds_by_blob.insert(blob.clone(), seed);
        }
        let seed = item
            .seed_blob
            .as_ref()
            .and_then(|b| seeds_by_blob.get(b))
            .and_then(Option::as_ref);
        let Some(result) = audit_one(&store, item, seed).await else {
            tally.skipped += 1;
            continue;
        };
        let facts = serde_json::to_value(&result.facts)?;
        let mut conn = pool.acquire().await?;
        let recorded = integrity::record_audit_in(
            &mut conn,
            &AuditRecord {
                due: item,
                score: result.score,
                reasons: &result.reasons,
                audit: &facts,
            },
        )
        .await?;
        if !recorded {
            tally.already += 1;
            continue;
        }
        tally.recorded += 1;
        tally.scored += u32::from(result.score > 0);
        tally.would_block += u32::from(result.would_block());
        tally.unreadable += u32::from(result.facts.unreadable.is_some());
        if result.score > 0 {
            let reasons: Vec<&str> = result.reasons.iter().map(|r| r.code()).collect();
            tracing::info!(
                profile = %item.profile_id,
                revision = item.revision,
                score = result.score,
                reasons = ?reasons,
                "save audit scored"
            );
        }
    }
    tracing::info!(
        due = due.len(),
        recorded = tally.recorded,
        already = tally.already,
        scored = tally.scored,
        would_block = tally.would_block,
        unreadable = tally.unreadable,
        skipped = tally.skipped,
        "saves-audit done"
    );
    println!(
        "saves-audit: {} due, {} recorded ({} scored, {} would block, {} unreadable), {} already, {} skipped",
        due.len(),
        tally.recorded,
        tally.scored,
        tally.would_block,
        tally.unreadable,
        tally.already,
        tally.skipped
    );
    Ok(())
}

/// A seed's numbers and garage (its cars were free, whatever their flags); an error when its
/// bundle cannot be fetched, does not match its digest or cannot be read.
async fn seed_of(store: &Blobs, seed: &seeds::Seed) -> Result<Seed, String> {
    let digest = seed.blob.sha256;
    let bytes = store
        .get(digest)
        .await
        .map_err(|e| format!("seed {}: {e}", digest.hex()))?;
    if Digest::of(&bytes) != digest {
        return Err(format!(
            "seed {}: the bundle does not match its digest",
            digest.hex()
        ));
    }
    let valid = bundle::validate(&bytes)
        .map_err(|e| format!("seed {}: the bundle does not verify: {e}", digest.hex()))?;
    let database = valid
        .files
        .iter()
        .find(|(name, _)| *name == "data/PlayerDatabase")
        .map(|(_, body)| body.as_slice())
        .unwrap_or_default();
    let db = fi_playerdb::read(database).map_err(|e| {
        format!(
            "seed {}: PlayerDatabase unreadable: {}",
            digest.hex(),
            e.code()
        )
    })?;
    Ok(Seed {
        credits: seed.summary.credits.unwrap_or(0),
        winnings: seed.winnings,
        // The driver level its XP reaches, never ProfileData `Main/Level` (fi_saves::gates::
        // with_driver_level; 2026-10-31): the tombola spins are counted between driver levels.
        level: fi_saves::gates::with_driver_level(&fi_gamedb::RULES, seed.summary)
            .level
            .unwrap_or(0),
        garage: db.garage.iter().map(|c| (c.id, c.car_id)).collect(),
    })
}

/// Audits one revision; `None` when the bucket could not answer (retried by a later run). A bundle
/// the bucket no longer has is recorded once (`BLOB_MISSING`, no score).
async fn audit_one(store: &Blobs, item: &AuditDue, seed: Option<&Seed>) -> Option<Audit> {
    let Some(digest) = Digest::from_slice(&item.blob_sha256) else {
        return Some(Audit::unreadable("BLOB_DIGEST", false));
    };
    let bytes = match store.get_if_present(digest).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Some(Audit::unreadable("BLOB_MISSING", false)),
        Err(e) => {
            tracing::warn!(
                profile = %item.profile_id,
                revision = item.revision,
                error = %e,
                "save audit: bundle not fetched, retried next run"
            );
            return None;
        }
    };
    if Digest::of(&bytes) != digest {
        return Some(Audit::unreadable("BLOB_DIGEST", false));
    }
    let Ok(valid) = bundle::validate(&bytes) else {
        return Some(Audit::unreadable("BUNDLE_INVALID", false));
    };
    let database = valid
        .files
        .iter()
        .find(|(name, _)| *name == "data/PlayerDatabase")
        .map(|(_, body)| body.as_slice())
        .unwrap_or_default();
    let summary: SaveSummary = fi_saves::gates::with_driver_level(
        &fi_gamedb::RULES,
        serde_json::from_value(item.summary.clone()).unwrap_or_default(),
    );
    let numbers = Numbers {
        credits: summary.credits,
        winnings: summary.winnings,
        tokens: summary.tokens,
        level: summary.level,
    };
    let read = fi_playerdb::read(database);
    Some(audit::audit(
        read.as_ref().map_err(|e| *e),
        numbers,
        seed,
        Reference {
            catalog: Catalog::fh2(),
            parts: Some(PartPrices::fh2()),
            rules: &fi_gamedb::RULES,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn arguments_are_bounded() {
        assert_eq!(
            parse(&[]).unwrap(),
            Args {
                limit: 200,
                days: 30
            }
        );
        assert_eq!(
            parse(&args(&["--limit", "5", "--days", "90"])).unwrap(),
            Args { limit: 5, days: 90 }
        );
        assert!(parse(&args(&["--limit", "0"])).is_err());
        assert!(parse(&args(&["--days", "91"])).is_err());
        assert!(parse(&args(&["--limit"])).is_err());
        assert!(parse(&args(&["--force"])).is_err());
    }

    #[test]
    fn the_stored_audit_is_the_moderators_dto() {
        // Review finding 7: fi_gamedb::audit::Facts is stored, fi_protocol::core::GuardAudit is
        // served; a renamed member would read back as null.
        let facts = fi_gamedb::audit::Facts {
            unreadable: Some("BLOB_MISSING".into()),
            cars: Some(1),
            bought: Some(2),
            unpriced: Some(3),
            bought_value: Some(4),
            parts: Some(5),
            parts_paid: Some(6),
            perks: Some(7),
            perks_cost: Some(8),
            tokens: Some(9),
            budget: Some(10),
            vouchers: Some(11),
        };
        let stored = serde_json::to_value(&facts).unwrap();
        let served: fi_protocol::core::GuardAudit = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(serde_json::to_value(&served).unwrap(), stored);
    }

    #[test]
    fn the_embedded_references_load() {
        assert!(Catalog::fh2().has_perks());
        assert_eq!(PartPrices::fh2().len(), 37_735);
    }
}
