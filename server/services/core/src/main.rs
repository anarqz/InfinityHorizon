//! fi-core: the core deployable of platform v1 (ADR 0013): identity and `/me`, the web's cookie
//! session, `/core/v1` (status, notices, social hub, moderation), `/internal/v1` and the MCP
//! skeleton. One Cloud Run service, min 0 instances.

use fi_core::config::CoreConfig;
use fi_runtime::{ServiceInfo, http, telemetry};
use fi_surfaces::config::{IdentityConfig, IdentityNeeds, PlatformConfig, ProcessEnv};
use fi_surfaces::state::AppState;
use std::sync::Arc;

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-core: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fi-core stopped");
            eprintln!("fi-core: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // The game data (FI_GAME_DATA_DIR, T-1115): every file and table checked before serving.
    fi_surfaces::gamedata::load()?;
    let identity = Arc::new(IdentityConfig::from_env_for(
        &ProcessEnv,
        IdentityNeeds::CORE,
    )?);
    let platform = PlatformConfig::from_env(&ProcessEnv, None, false, false)?;
    let core = Arc::new(fi_core::Core::new(CoreConfig::from_env(&ProcessEnv)?));
    let info = ServiceInfo::from_env("fi-core", env!("CARGO_PKG_VERSION"))
        .with_label(
            "idp",
            if identity.verifier.as_ref().is_some_and(|v| v.is_emulator()) {
                "emulator"
            } else {
                "rs256"
            },
        )
        .with_label("turnstile", core.turnstile.mode())
        .with_label("edge_lock", core.config.edge.is_some().to_string())
        .with_label("mcp", core.config.mcp.enabled.to_string());
    telemetry::init(&info);
    let db = fi_db::DbConfig::from_env("DATABASE_URL", "fi-core")?;
    let pool = fi_db::connect_lazy(&db)?;
    let state = AppState {
        pool,
        identity: Some(identity),
    };
    let router = fi_core::router(info, state, platform, core);
    http::serve(router, http::port()?).await?;
    Ok(())
}
