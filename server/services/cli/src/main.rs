//! fi-cli: one binary for every Cloud Run job and the compose `init` service (ADR 0002 §3).
//!
//! Jobs run the same image with a fixed command each (infra/terraform `jobs.tf`); `max_retries = 0`,
//! so an operator reads the log before anything runs twice. Commands by milestone
//! (docs/milestones/server.md): `migrate`, `db-check` (SER-M0); `saves-retention`, `saves-gc`,
//! `saves-verify`, `account-erasure` (SER-M2/M1); `saves-audit` (OD-67); `data-retention`
//! (HANDOFF §5); `consolidate-nightly` (SER-M5; the outbox, Pub/Sub and lake commands left with
//! ADR 0007, owner 2026-10-26); `beta-import` (SER-M6);
//! monetization adds `billing ...` and `entitlement ...` (its ADR 0305 §9). Every command runs with
//! the job session limits (`fi_db::Workload::Job`), migrate with the migration ones.
//!
//! The local environment v0 (docs/server/LOCAL_ENV.md) adds its commands in [`commands`], one
//! module per workstream; this file only dispatches to them.

mod commands;

use commands::Outcome;
use fi_db::{DbConfig, Workload, migrate};
use fi_runtime::{ServiceInfo, config, telemetry};
use std::process::ExitCode;

const USAGE_HEAD: &str = "\
usage: fi-cli <command> [args...]

commands:
  migrate    apply pending migrations (DATABASE_MIGRATOR_URL; no statement timeout, lock waits of
             5 s retried 3 times), then create or update the login roles whose passwords are set:
             FI_APP_PASSWORD -> fi_app (fi_runtime), FI_JOBS_PASSWORD -> fi_jobs (fi_runtime,
             fi_maintenance). With FI_LOGIN_GENERATION=N (production) the logins are fi_app_gN and
             fi_jobs_gN, and generations older than N-1 are dropped (credential rotation,
             docs/server/OBSERVABILITY.md section 7)
  db-check   connect with DATABASE_URL and fail unless every migration of this build is applied
             with the checksum it has in this build
  help       this text";

fn usage() -> String {
    format!(
        "{USAGE_HEAD}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        commands::platform::USAGE,
        commands::nightly::USAGE,
        commands::gate::USAGE,
        commands::saves::USAGE,
        commands::audit::USAGE,
        commands::integrity::USAGE,
        commands::retention::USAGE,
        commands::online_control::USAGE,
        commands::dashboard::USAGE,
        commands::schedule::USAGE,
        commands::profiles::USAGE,
        commands::worlds::USAGE
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };
    let command = command.as_str();
    if matches!(command, "help" | "--help" | "-h") {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }
    if matches!(command, "migrate" | "db-check") && !rest.is_empty() {
        eprintln!("fi-cli {command}: takes no arguments\n\n{}", usage());
        return ExitCode::from(2);
    }
    // The game data (FI_GAME_DATA_DIR, T-1115) is checked before any command that may read it;
    // only the database, probe and seat-sweep commands never do.
    if !matches!(command, "migrate" | "db-check" | "probe" | "gate-sweep")
        && let Err(e) = fi_surfaces::gamedata::load()
    {
        eprintln!("fi-cli {command}: {e}");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-cli: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let info =
        ServiceInfo::from_env("fi-cli", env!("CARGO_PKG_VERSION")).with_label("command", command);
    telemetry::init(&info);
    let result = runtime.block_on(async {
        match command {
            "migrate" => run_migrate().await,
            "db-check" => run_db_check().await,
            other => {
                if let Some(outcome) = commands::platform::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::nightly::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::gate::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::saves::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::audit::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::integrity::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::retention::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::online_control::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::dashboard::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::schedule::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::profiles::run(other, rest).await {
                    outcome
                } else if let Some(outcome) = commands::worlds::run(other, rest).await {
                    outcome
                } else {
                    Err(format!("unknown command {other:?}\n\n{}", usage()).into())
                }
            }
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(command, error = %e, "fi-cli failed");
            eprintln!("fi-cli {command}: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_migrate() -> Outcome {
    let db = DbConfig::from_env_for("DATABASE_MIGRATOR_URL", "fi-cli-migrate", Workload::Migrate)?;
    let pool = fi_db::connect_lazy(&db)?;
    let generation: Option<u32> = match config::optional("FI_LOGIN_GENERATION") {
        Some(v) => Some(
            v.trim()
                .parse()
                .ok()
                .filter(|g| *g > 0)
                .ok_or("FI_LOGIN_GENERATION is a positive integer")?,
        ),
        None => None,
    };
    let logins = migrate::Logins {
        generation,
        // A standalone instance's passwords come from its keys directory (FI_KEYS_DIR).
        app_password: config::secret("FI_APP_PASSWORD"),
        jobs_password: config::secret("FI_JOBS_PASSWORD"),
    };
    migrate::apply(&pool, &logins).await?;
    pool.close().await;
    Ok(())
}

async fn run_db_check() -> Outcome {
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-db-check", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    fi_db::ping(&pool).await?;
    let applied = migrate::check_applied(&pool).await?;
    tracing::info!(
        applied,
        expected = migrate::expected_version(),
        "database schema is current"
    );
    pool.close().await;
    Ok(())
}
