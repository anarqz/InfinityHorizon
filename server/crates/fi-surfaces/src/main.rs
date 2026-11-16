//! fi-api: the HTTP service of the account plane and the game plane (ADR 0002).
//!
//! One image, several Cloud Run services that differ only in configuration (infra/terraform
//! `run.tf`): `fi-account` serves the launcher, portal and internal surfaces; `fi-game-vip` (and
//! later `fi-game-free`) serves the game surface for one pool. `FI_SURFACES` and `FI_POOL` say which,
//! and `/meta` reports them so every deployment describes itself. The routers live in this
//! package's library (`lib.rs`), which the platform v1 deployables (ADR 0013) mount too.

use fi_runtime::{ServiceInfo, http, telemetry};
use fi_surfaces::{app, config, state};
use std::sync::Arc;

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-api: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fi-api stopped");
            eprintln!("fi-api: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = app::Config::from_env()?;
    let identity = if config.serves_identity() {
        Some(Arc::new(config::IdentityConfig::from_env(
            &config::ProcessEnv,
            config.serves(app::Surface::Portal),
        )?))
    } else {
        None
    };
    let mut info = ServiceInfo::from_env("fi-api", env!("CARGO_PKG_VERSION"))
        .with_label("surfaces", config.surfaces_label())
        .with_label(
            "pool",
            config.pool.clone().unwrap_or_else(|| "none".to_owned()),
        );
    if config.serves(app::Surface::Game) {
        info = info.with_label(
            "liveness_cache_ms",
            config.platform.liveness.ttl_ms().to_string(),
        );
    }
    if let Some(id) = &identity {
        info = info.with_label(
            "idp",
            if id.verifier.as_ref().is_some_and(|v| v.is_emulator()) {
                "emulator"
            } else {
                "rs256"
            },
        );
    }
    telemetry::init(&info);
    let db = fi_db::DbConfig::from_env("DATABASE_URL", "fi-api")?;
    let pool = fi_db::connect_lazy(&db)?;
    let state = state::AppState { pool, identity };
    let router = app::router(info, state, &config);
    http::serve(router, http::port()?).await?;
    Ok(())
}
