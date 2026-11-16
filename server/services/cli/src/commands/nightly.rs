//! `consolidate-nightly` (daily; owner-decisions, OD-122 follow-ups 2026-10-26 "no outbox or
//! BigQuery" and "no outbox, no queue"): the read models from the game tables, then the public
//! totals.
//!
//! 1. `fi_db::progression::consolidate_nightly`: `profile_progress` up to a watermark behind the
//!    clock, the `session_summary` of every ended session without one, the head's numbers of every
//!    profile behind its save head (the writers keep them current in their own transaction; this
//!    pass rebuilds whatever they missed);
//!    a session or profile that fails is logged, skipped and makes the job exit non-zero at the
//!    end;
//! 2. every game's `totals` gold snapshot (`fi_db::universe::local_totals`, what core's status
//!    shows), formerly `gold-build --local`;
//! 3. every game's server summary of yesterday (UTC) as its `summary` gold snapshot
//!    (`fi_db::summary`, what core's `GET /core/v1/summary` shows; a rerun the same day replaces
//!    it with the same numbers).
//!
//! The outbox sweeper, `pubsub-init`, the lake commands (`lake-migrate`, `lake-writer`,
//! `lake-build`, `lake-clone`) and `gold-build` left with ADR 0007. The pool runs with the job
//! limits (`fi_db::Workload::Job`) as a `fi_maintenance` login (the snapshot write needs it). Log
//! lines carry counts, never payloads.
//!
//! `FI_CONSOLIDATE_LAG_MS` (default 60000, the watermark's lag) is local only: refused when
//! `APP_ENV` or `FI_ENV` is `prod`.

use super::Outcome;
use fi_db::{DbConfig, Workload, progression, summary, universe};
use std::time::Instant;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  consolidate-nightly  recompute profile_progress and missing session_summary rows, then
                       every game's totals and server summary snapshots";

/// Runs `command` with `args`; `None` when it is not this module's command.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    if command != "consolidate-nightly" {
        return None;
    }
    if !args.is_empty() {
        return Some(Err(format!("{command} takes no arguments").into()));
    }
    Some(consolidate_nightly().await)
}

fn is_prod() -> bool {
    ["APP_ENV", "FI_ENV"]
        .iter()
        .any(|v| std::env::var(v).is_ok_and(|x| x.trim().eq_ignore_ascii_case("prod")))
}

/// A local-only knob: unset gives `default`; set in prod is refused.
fn local_knob<T: std::str::FromStr>(name: &str, default: T, prod: bool) -> Result<T, String> {
    match std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
    {
        None => Ok(default),
        Some(_) if prod => Err(format!("{name} is local only (APP_ENV or FI_ENV is prod)")),
        Some(v) => v.parse().map_err(|_| format!("{name} does not parse")),
    }
}

async fn consolidate_nightly() -> Outcome {
    let lag_ms: u64 = local_knob("FI_CONSOLIDATE_LAG_MS", 60_000, is_prod())?;
    let config =
        DbConfig::from_env_for("DATABASE_URL", "fi-cli-consolidate-nightly", Workload::Job)?;
    let db = fi_db::connect_lazy(&config)?;
    let started = Instant::now();
    let report = progression::consolidate_nightly(&db, lag_ms).await?;
    tracing::info!(
        watermark = %report.watermark,
        lag_ms,
        summaries_built = report.summaries_built,
        profiles_refreshed = report.profiles_refreshed,
        heads_copied = report.heads_copied,
        failed = report.failed,
        took_ms = started.elapsed().as_millis() as u64,
        "progression.nightly.summary"
    );
    for game in universe::games(&db).await? {
        let totals = universe::local_totals(&db, &game).await?;
        universe::put_gold_snapshot(&db, &game, "totals", &totals).await?;
        tracing::info!(game = %game, name = "totals", "gold snapshot written");
    }
    let day = summary::last_day();
    for game in universe::games(&db).await? {
        let figures = summary::compute(&db, &game, day).await?;
        summary::store(&db, &game, &figures).await?;
        tracing::info!(game = %game, name = summary::SNAPSHOT, day = %figures.day, "gold snapshot written");
    }
    db.close().await;
    if report.failed > 0 {
        // Every other session and profile was consolidated and the totals written; the job
        // still fails so an operator reads the warnings (ids only).
        return Err(format!(
            "{} session(s) or profile(s) failed to consolidate (progression.nightly.*_failed)",
            report.failed
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_knobs_are_refused_in_prod_only_when_set() {
        // A name no environment sets: the default, prod or not.
        const UNSET: &str = "FI_CLI_TEST_SURELY_UNSET_9C1E";
        assert_eq!(local_knob(UNSET, 30_000u64, true), Ok(30_000));
        assert_eq!(local_knob(UNSET, 0.0f64, false), Ok(0.0));
        // PATH is always set: as a "knob" it is refused in prod and fails to parse elsewhere.
        assert!(
            local_knob("PATH", 0u64, true)
                .unwrap_err()
                .contains("local only")
        );
        assert!(
            local_knob("PATH", 0u64, false)
                .unwrap_err()
                .contains("does not parse")
        );
    }

    #[tokio::test]
    async fn other_commands_are_not_ours_and_arguments_are_refused() {
        assert!(run("local-seed", &[]).await.is_none());
        assert!(run("outbox-sweep", &[]).await.is_none());
        assert!(run("lake-build", &[]).await.is_none());
        let refused = run("consolidate-nightly", &["--now".to_owned()])
            .await
            .unwrap();
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("takes no arguments")
        );
    }
}
