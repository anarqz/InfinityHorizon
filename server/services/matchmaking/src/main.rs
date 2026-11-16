//! fi-matchmaking: worlds, join grants, the free limit and queue, host moderation and punches
//! behind the game access ticket (library docs in `lib.rs`).

use fi_matchmaking::{Parts, background, config::Settings, router};
use fi_runtime::{ServiceInfo, http, telemetry};
use fi_surfaces::config::ProcessEnv;
use fi_surfaces::state::AppState;

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-matchmaking: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fi-matchmaking stopped");
            eprintln!("fi-matchmaking: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // The game data (FI_GAME_DATA_DIR, T-1115): every file and table checked before serving.
    fi_surfaces::gamedata::load()?;
    let settings = Settings::from_env(&ProcessEnv)?;
    let info = ServiceInfo::from_env("fi-matchmaking", env!("CARGO_PKG_VERSION"))
        .with_label("pools", fi_matchmaking::POOLS.join(","))
        .with_label("games", settings.games.join(","))
        .with_label("liveness_cache_ms", settings.liveness_ms.to_string())
        .with_label("edge_lock", settings.edge.is_some().to_string());
    telemetry::init(&info);
    let db = fi_db::DbConfig::from_env("DATABASE_URL", "fi-matchmaking")?;
    let pool = fi_db::connect_lazy(&db)?;
    let parts = Parts::new(&settings);
    tokio::spawn(background(
        pool.clone(),
        parts.clone(),
        settings.instance_id.clone(),
    ));
    let state = AppState {
        pool,
        identity: None,
    };
    http::serve(router(info, state, &parts), http::port()?).await?;
    Ok(())
}
