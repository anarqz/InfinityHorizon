//! Data retention (HANDOFF §5, `fi_db::retention`):
//!
//! - `data-retention`: deletes the guardrails' assessments older than 90 days, friend requests and
//!   in-game messages past their 30-day lapse, world invites ended over 7 days ago, peer
//!   observations after 30 days and ended cheat flags (OD-147), and a deleted
//!   profile's sessions and their summaries 90 days after its deletion (PD-Q1), in batches
//!   (`fi_db::retention::purge`). Needs a `fi_maintenance` login (`fi_jobs`). Nightly in production;
//!   a one-shot run locally. Logs counts only.
//! - `saves-retention`: keeps the save files of the newest 10 revisions of every save line plus
//!   the revisions the save rules or the save audit still read, and marks the others' bundles
//!   pruned (every revision row stays); deletes expired parked bundles nothing names
//!   (`fi_db::save_retention::prune`; SAVES_API.md §8). Never a blob row or an object. Exits
//!   non-zero when a line failed (the others are pruned; the next run retries it).
//! - `saves-gc`: the two-phase garbage collection of save bundles (CLOUD_SAVES.md §6): marks the
//!   blob rows no unpruned revision, parked bundle, seed or ghost references, then deletes the
//!   object of each one marked more than 24 h ago and still unreferenced (and its row when no
//!   revision row names it) (`fi_db::save_retention::{mark, sweep}`), through the
//!   instance's blob backend (`FI_BLOB_BACKEND`: the bucket, or `fs`).
//! - `account-erasure`: erases every account whose 14-day deletion grace has passed (ACCOUNTS.md
//!   §8; `fi_db::erasure::run`), one account per transaction; an account still playing is left for
//!   the next run. Rows only: its save blobs become unreferenced and `saves-gc` deletes them, so it
//!   runs before `saves-retention` and `saves-gc`. `FI_AUTH_BACKEND=local` erases every identity
//!   mapping (the instance's own credentials); otherwise an account whose identity is an Identity
//!   Platform user waits (counted, logged) until that user is deleted. Exits non-zero when an
//!   account failed (the others are erased; the next run retries it). Logs ids and counts only.

use super::Outcome;
use fi_db::{DbConfig, Workload, erasure, retention, save_retention};

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  data-retention   delete assessments older than 90 days, lapsed friend requests and in-game
                   messages (30 days), world invites ended over 7 days ago, peer observations
                   (30 days), cheat quorums (a day), ended cheat flags and the sessions of
                   profiles deleted over 90 days ago (DATABASE_URL, a fi_maintenance login such
                   as fi_jobs)
  saves-retention  keep the save files of the newest 10 revisions of each line and of those the save
                   rules or the save audit read; prune the others' (rows stay); delete expired
                   parked bundles nothing names (DATABASE_URL, fi_maintenance)
  saves-gc         mark save blobs nothing unpruned references; delete the objects marked over 24 h
                   ago and still unreferenced (DATABASE_URL, fi_maintenance; FI_BLOB_* or
                   CLOUD_SAVES_BUCKET)
  account-erasure  erase the accounts whose 14-day deletion grace has passed, one per transaction
                   (DATABASE_URL, fi_maintenance; FI_AUTH_BACKEND=local erases local identities,
                   otherwise an Identity Platform user waits for its deletion)
  content-retention delete at most100 expired startup checks (24h), with their scan pointers
                    (DATABASE_URL, fi_maintenance); counts only, no file inventory";

/// Runs `command` with `args`; `None` when it is not the retention job.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    match command {
        "data-retention" => Some(data_retention(args).await),
        "content-retention" => Some(content_retention(args).await),
        "saves-retention" => Some(saves_retention(args).await),
        "saves-gc" => Some(saves_gc(args).await),
        "account-erasure" => Some(account_erasure(args).await),
        _ => None,
    }
}

async fn content_retention(args: &[String]) -> Outcome {
    if !args.is_empty() {
        return Err("usage: content-retention".into());
    }
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-content-retention", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let mut tx = pool.begin().await?;
    let deleted = fi_db::content::cleanup_in(&mut tx).await?;
    tx.commit().await?;
    tracing::info!(deleted, "content-retention done");
    println!("content-retention: {deleted} expired startup checks deleted");
    Ok(())
}

async fn saves_retention(args: &[String]) -> Outcome {
    if !args.is_empty() {
        return Err("usage: saves-retention".into());
    }
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-saves-retention", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let p = save_retention::prune(&pool).await?;
    tracing::info!(
        lines = p.lines,
        bundles = p.bundles,
        parked = p.parked,
        busy = p.busy,
        failed = p.failed,
        "saves-retention done"
    );
    println!(
        "saves-retention: {} bundles of {} lines pruned, {} expired parked bundles deleted; {} busy and {} failed lines left for the next run",
        p.bundles, p.lines, p.parked, p.busy, p.failed
    );
    if p.failed > 0 {
        return Err(format!(
            "saves-retention: {} lines failed (logged; retried next run)",
            p.failed
        )
        .into());
    }
    Ok(())
}

async fn saves_gc(args: &[String]) -> Outcome {
    if !args.is_empty() {
        return Err("usage: saves-gc".into());
    }
    let blobs = fi_storage::Blobs::from_env()?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-saves-gc", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let m = save_retention::mark(&pool).await?;
    let s = save_retention::sweep(&pool, |digest| {
        let blobs = blobs.clone();
        async move { blobs.delete(digest).await.map_err(|e| e.to_string()) }
    })
    .await?;
    tracing::info!(
        marked = m.marked,
        unmarked = m.unmarked,
        deleted = s.deleted,
        rows = s.rows,
        kept = s.kept,
        failed = s.failed,
        backend = blobs.label(),
        "saves-gc done"
    );
    println!(
        "saves-gc: {} marked, {} unmarked, {} objects deleted ({} rows), {} kept, {} failed",
        m.marked, m.unmarked, s.deleted, s.rows, s.kept, s.failed
    );
    if s.failed > 0 {
        return Err(format!(
            "saves-gc: {} objects could not be deleted (kept, retried next run)",
            s.failed
        )
        .into());
    }
    Ok(())
}

async fn account_erasure(args: &[String]) -> Outcome {
    if !args.is_empty() {
        return Err("usage: account-erasure".into());
    }
    let local_identity = match std::env::var("FI_AUTH_BACKEND")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        None | Some("firebase") => false,
        Some("local") => true,
        Some(other) => return Err(format!("FI_AUTH_BACKEND: unknown backend {other:?}").into()),
    };
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-account-erasure", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let e = erasure::run(&pool, erasure::Options { local_identity }).await?;
    tracing::info!(
        accounts = e.accounts,
        profiles = e.profiles,
        busy = e.busy,
        idp_pending = e.idp_pending,
        failed = e.failed,
        rows = ?e.rows,
        "account-erasure done"
    );
    println!(
        "account-erasure: {} accounts ({} profiles) erased; {} busy, {} waiting for their Identity Platform user and {} failed left for the next run",
        e.accounts, e.profiles, e.busy, e.idp_pending, e.failed
    );
    if e.failed > 0 {
        return Err(format!(
            "account-erasure: {} accounts failed (logged; retried next run)",
            e.failed
        )
        .into());
    }
    Ok(())
}

async fn data_retention(args: &[String]) -> Outcome {
    if !args.is_empty() {
        return Err("usage: data-retention".into());
    }
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-data-retention", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let purged = retention::purge(&pool).await?;
    // MATCHMAKING_API.md §5d (CR-S80): the relay's reports and per-start counters after 30 days.
    let (relay_reports, relay_counters) = fi_db::relay_reports::purge(&pool).await?;
    tracing::info!(
        relay_reports,
        relay_counters,
        "data-retention relay reports"
    );
    println!(
        "data-retention: {relay_reports} relay reports and {relay_counters} relay start counters deleted"
    );
    tracing::info!(
        assessments = purged.assessments,
        friend_requests = purged.friend_requests,
        notifications = purged.notifications,
        invites = purged.invites,
        idle_observations = purged.idle_observations,
        mail_tokens = purged.mail_tokens,
        feed_events = purged.feed_events,
        link_codes = purged.link_codes,
        peer_observations = purged.peer_observations,
        cheat_quorums = purged.cheat_quorums,
        cheat_flags = purged.cheat_flags,
        voice_mute_events = purged.voice_mute_events,
        deleted_profile_sessions = purged.deleted_profile_sessions,
        deleted_profile_summaries = purged.deleted_profile_summaries,
        deleted_profile_worlds = purged.deleted_profile_worlds,
        "data-retention done"
    );
    println!(
        "data-retention: {} assessments, {} friend requests, {} in-game messages, {} world invites, {} idle observations, {} mail links, {} feed events, {} Discord link codes, {} peer observations, {} cheat quorums, {} cheat flags ended 30 days ago, {} voice mute events, {} sessions and {} summaries of deleted profiles deleted ({} closed worlds unlinked)",
        purged.assessments,
        purged.friend_requests,
        purged.notifications,
        purged.invites,
        purged.idle_observations,
        purged.mail_tokens,
        purged.feed_events,
        purged.link_codes,
        purged.peer_observations,
        purged.cheat_quorums,
        purged.cheat_flags,
        purged.voice_mute_events,
        purged.deleted_profile_sessions,
        purged.deleted_profile_summaries,
        purged.deleted_profile_worlds
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_takes_no_arguments_and_ignores_other_commands() {
        assert!(run("saves-audit", &[]).await.is_none());
        let refused = run("data-retention", &["--now".into()]).await.unwrap();
        assert_eq!(refused.unwrap_err().to_string(), "usage: data-retention");
        assert_eq!(
            run("content-retention", &["--all".into()])
                .await
                .unwrap()
                .unwrap_err()
                .to_string(),
            "usage: content-retention"
        );
        for job in ["saves-retention", "saves-gc", "account-erasure"] {
            assert!(USAGE.contains(job), "{job} is in the help");
            let refused = run(job, &["--all".into()]).await.unwrap();
            assert_eq!(refused.unwrap_err().to_string(), format!("usage: {job}"));
        }
    }
}
