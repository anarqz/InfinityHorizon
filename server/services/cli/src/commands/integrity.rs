//! `integrity-nightly` (hybrid Hardcore checks, owner 2026-10-26 "no outbox or BigQuery; hybrid
//! Hardcore checks"; CLOUD_SAVES.md §7.5): the guardrails' rate replay, ceilings and anomaly score
//! over every gated Hardcore revision not analyzed yet (`fi_db::guard_nightly::analyze`), with the
//! same gate policy the upload path builds (FH2's rules for `fh2`, none otherwise; the deployment's
//! `FI_GUARD_ENFORCE`, `true` or `false`, unset `false`). Flags, never refusals: a revision the
//! inline guardrails would have refused is recorded `flagged` for moderation, nothing is rolled
//! back. Daily, before `saves-retention` (which keeps every revision not analyzed yet). A
//! `fi_maintenance` login (`fi_jobs`). Logs counts only; exits non-zero when a line failed (the
//! others are analyzed; the next run retries it).

use super::Outcome;
use fi_db::{DbConfig, Workload, guard_nightly};
use fi_saves::gates::GatePolicy;
use std::time::Instant;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  integrity-nightly assess every gated Hardcore revision not analyzed yet with the full
                   guardrails (rates, ceilings, score); flag, never refuse (DATABASE_URL,
                   fi_maintenance; FI_GUARD_ENFORCE)";

/// Runs `command` with `args`; `None` when it is not this module's command.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    if command != "integrity-nightly" {
        return None;
    }
    if !args.is_empty() {
        return Some(Err(format!("{command} takes no arguments").into()));
    }
    Some(integrity_nightly().await)
}

/// `FI_GUARD_ENFORCE` as the services read it (`fi_surfaces::config::guard_enforce_from_env`).
fn enforce(value: Option<&str>) -> Result<bool, String> {
    match value.map(str::trim) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err("FI_GUARD_ENFORCE must be true or false".into()),
    }
}

/// The gate policy of `game`, as the upload path builds it (`fi_surfaces::launcher::saves`).
fn policy_for(game: &str, enforce: bool) -> GatePolicy {
    let mut policy = GatePolicy {
        enforce,
        ..GatePolicy::default()
    };
    if game == "fh2" {
        policy.rules = *fi_gamedb::RULES;
    }
    policy
}

async fn integrity_nightly() -> Outcome {
    let enforce = enforce(std::env::var("FI_GUARD_ENFORCE").ok().as_deref())?;
    let config = DbConfig::from_env_for("DATABASE_URL", "fi-cli-integrity-nightly", Workload::Job)?;
    let db = fi_db::connect_lazy(&config)?;
    let started = Instant::now();
    let report = guard_nightly::analyze(&db, |game| Some(policy_for(game, enforce))).await?;
    db.close().await;
    tracing::info!(
        enforce,
        lines = report.lines,
        revisions = report.revisions,
        recorded = report.recorded,
        flagged = report.flagged,
        would_block = report.would_block,
        skipped = report.skipped,
        failed = report.failed,
        took_ms = started.elapsed().as_millis() as u64,
        "integrity.nightly.summary"
    );
    if report.failed > 0 {
        return Err(format!(
            "{} line(s) failed the nightly integrity pass (integrity-nightly warnings)",
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
    fn the_switch_reads_as_the_services_read_it() {
        assert_eq!(enforce(None), Ok(false));
        assert_eq!(enforce(Some(" true ")), Ok(true));
        assert_eq!(enforce(Some("false")), Ok(false));
        assert!(enforce(Some("1")).is_err());
    }

    #[test]
    fn fh2_gets_its_rules_and_others_none() {
        assert_eq!(policy_for("fh2", true).rules, *fi_gamedb::RULES);
        assert!(policy_for("fh2", true).enforce);
        assert_eq!(policy_for("other", false), GatePolicy::default());
    }

    #[tokio::test]
    async fn other_commands_are_not_ours_and_arguments_are_refused() {
        assert!(run("saves-retention", &[]).await.is_none());
        let refused = run("integrity-nightly", &["--now".to_owned()])
            .await
            .unwrap();
        assert!(refused.unwrap_err().to_string().contains("no arguments"));
    }
}
