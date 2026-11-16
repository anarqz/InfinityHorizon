//! `schedule` (OD-122 M2, ADR 0023 §9): the scheduled jobs of a standalone instance in one
//! long-running process, the stand-in for Cloud Scheduler. Each job runs as a child `fi-cli
//! <command>` (its own pool, its own exit status; a failure is logged and the next pass retries),
//! one at a time, at a fixed interval from the process start (the first pass at start).
//!
//! Why not supercronic (study §8.2): it runs each crontab line through `/bin/sh -c`, and the
//! platform image is distroless (no shell), so it would need a shell and a third-party binary in
//! the image for a table of six lines; the `--every` loops of `gate-sweep` and `sessions-reap`
//! are the precedent. Overlapping runs cannot happen (one child at a time), and every job is
//! idempotent and batched, so a restart that runs a daily job again is harmless.
//!
//! **Lanes.** The jobs of [`JOBS`] share one queue. [`LANE`] (the dashboard snapshot, OD-173) runs
//! in its own lane, a second task beside the queue, one run at a time: it reads for minutes
//! (limit 15 min) and must never hold `gate-sweep`'s 60 s cadence behind it, and it is read-only
//! in one `REPEATABLE READ` transaction on its own connection, so running beside a queue job
//! changes nothing for either.
//!
//! The outbox, Pub/Sub and lake jobs are gone (owner 2026-10-26, ADR 0007 superseded):
//! `consolidate-nightly` also writes the totals snapshot core's status shows.

use super::Outcome;
use std::time::Duration;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  schedule         run the standalone instance's jobs forever, one at a time (DATABASE_URL, a
                   fi_maintenance login such as fi_jobs; FI_BLOB_* for saves-audit and saves-gc):
                   gate-sweep and worlds-ceiling (FI_ALERT_MAIL_TO, CR-S100) every 60 s;
                   freeroam-purge and saves-audit hourly;
                   dashboard-snapshot hourly in its own lane (FI_DASHBOARD_SNAPSHOT_DIR,
                   FI_DASHBOARD_KEY), beside the queue;
                   consolidate-nightly, data-retention, content-retention, account-erasure,
                   integrity-nightly, saves-retention and saves-gc daily";

/// The jobs, their intervals and their time limits in seconds. The limits are the Cloud Run
/// jobs' `timeout` (infra/terraform/modules/platform/main.tf): a child past it is killed, so a
/// stuck job cannot hold the queue (`gate-sweep` frees seats every minute).
pub const JOBS: [(&str, u64, u64); 11] = [
    ("gate-sweep", 60, 300),
    // CR-S100: the owner's mail when a Worlds VM passes 80% of its safety ceiling.
    ("worlds-ceiling", 60, 120),
    ("freeroam-purge", 3_600, 300),
    ("saves-audit", 3_600, 900),
    ("consolidate-nightly", 86_400, 3_600),
    ("data-retention", 86_400, 1_800),
    ("content-retention", 86_400, 1_800),
    // Erasure, then retention, before garbage collection: the save rows erasure deletes and the
    // bundles retention prunes leave objects the same pass marks and a later pass sweeps
    // (ACCOUNTS.md §8, CLOUD_SAVES.md §6).
    ("account-erasure", 86_400, 1_800),
    // The nightly guardrails pass before retention (CLOUD_SAVES.md §7.5): retention keeps every
    // revision it has not analyzed, so analyzing first lets the same night prune them.
    ("integrity-nightly", 86_400, 3_600),
    ("saves-retention", 86_400, 1_800),
    ("saves-gc", 86_400, 1_800),
];

/// The job of the second lane, its interval and its limit: the play dashboard's snapshot
/// (DASHBOARD_API.md section 5, OD-173), beside the queue so `gate-sweep` keeps its cadence.
pub const LANE: (&str, u64, u64) = ("dashboard-snapshot", 3_600, 900);

/// Runs `command` with `args`; `None` when it is not `schedule`.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    match command {
        "schedule" if args.is_empty() => Some(schedule().await),
        "schedule" => Some(Err("schedule takes no arguments".into())),
        _ => None,
    }
}

/// The index of the job due first and how long until it is due.
fn next(due: &[Duration], elapsed: Duration) -> (usize, Duration) {
    let (i, at) = due
        .iter()
        .enumerate()
        .min_by_key(|(_, at)| **at)
        .map(|(i, at)| (i, *at))
        .unwrap_or((0, Duration::ZERO));
    (i, at.saturating_sub(elapsed))
}

/// Runs `fi-cli <job>` as a child within `limit` seconds and logs how it ended. The child is
/// killed if this future is dropped (shutdown) or the limit passes.
async fn run_child(me: &std::path::Path, job: &'static str, limit: u64) {
    let started = std::time::Instant::now();
    // kill_on_drop: a child past its limit, or still running at shutdown, is killed.
    let child = tokio::process::Command::new(me)
        .arg(job)
        .kill_on_drop(true)
        .status();
    let status = tokio::time::timeout(Duration::from_secs(limit), child).await;
    let took_ms = started.elapsed().as_millis() as u64;
    match status {
        Ok(Ok(s)) if s.success() => tracing::info!(job, took_ms, "job done"),
        Ok(Ok(s)) => {
            tracing::warn!(job, took_ms, code = ?s.code(), "job failed; the next pass retries")
        }
        Ok(Err(e)) => tracing::warn!(job, error = %e, "job could not start"),
        Err(_) => tracing::warn!(
            job,
            limit_s = limit,
            "job killed at its time limit; the next pass retries"
        ),
    }
}

/// The next slot of a job due at `due`, every `every`, now `elapsed` (from the slot it was due
/// in, so a slow job does not drift the timetable; never in the past).
fn reschedule(due: Duration, every: u64, elapsed: Duration) -> Duration {
    let next = due + Duration::from_secs(every);
    if next < elapsed {
        elapsed + Duration::from_secs(every)
    } else {
        next
    }
}

/// The second lane: [`LANE`]'s job at start and every interval, one run at a time.
async fn lane(me: std::path::PathBuf, start: tokio::time::Instant) {
    let (job, every, limit) = LANE;
    let mut due = Duration::ZERO;
    loop {
        tokio::time::sleep(due.saturating_sub(start.elapsed())).await;
        run_child(&me, job, limit).await;
        due = reschedule(due, every, start.elapsed());
    }
}

async fn schedule() -> Outcome {
    let me = std::env::current_exe()?;
    let start = tokio::time::Instant::now();
    let mut due = vec![Duration::ZERO; JOBS.len()];
    let names: Vec<&str> = JOBS.iter().map(|(n, _, _)| *n).collect();
    tracing::info!(jobs = ?names, lane = LANE.0, "scheduler started");
    // Aborted at shutdown: dropping its future kills a running child.
    let second = tokio::spawn(lane(me.clone(), start));
    let mut shutdown = std::pin::pin!(fi_runtime::http::shutdown_signal());
    loop {
        let (i, wait) = next(&due, start.elapsed());
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = &mut shutdown => break,
        }
        let (job, every, limit) = JOBS[i];
        tokio::select! {
            () = run_child(&me, job, limit) => {}
            () = &mut shutdown => break,
        }
        due[i] = reschedule(due[i], every, start.elapsed());
    }
    second.abort();
    let _ = second.await;
    tracing::info!("scheduler stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_job_due_first_runs_first() {
        let due = [
            Duration::from_secs(60),
            Duration::from_secs(10),
            Duration::from_secs(30),
        ];
        assert_eq!(
            next(&due, Duration::from_secs(5)),
            (1, Duration::from_secs(5))
        );
        assert_eq!(next(&due, Duration::from_secs(20)), (1, Duration::ZERO));
    }

    #[test]
    fn a_slow_job_keeps_its_timetable_and_never_lands_in_the_past() {
        let s = Duration::from_secs;
        assert_eq!(reschedule(s(60), 60, s(70)), s(120));
        assert_eq!(reschedule(s(60), 60, s(500)), s(560));
    }

    #[test]
    fn the_snapshot_has_its_own_lane() {
        assert!(JOBS.iter().all(|(n, _, _)| *n != LANE.0));
        let (job, every, limit) = LANE;
        assert_eq!((job, every, limit), ("dashboard-snapshot", 3_600, 900));
        assert!(USAGE.contains(job));
    }

    #[test]
    fn every_scheduled_job_is_a_command() {
        for (job, every, limit) in JOBS {
            assert!((60..86_400).contains(&limit), "{job}");
            assert!(USAGE.contains(job) && every >= 60, "{job}");
        }
    }

    #[test]
    fn erasure_runs_before_retention_and_garbage_collection() {
        let at = |name: &str| JOBS.iter().position(|(n, _, _)| *n == name).unwrap();
        assert!(at("account-erasure") < at("saves-retention"));
        assert!(at("integrity-nightly") < at("saves-retention"));
        assert!(at("saves-retention") < at("saves-gc"));
    }
}
