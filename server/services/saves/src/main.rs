//! fi-saves-service: the save line, the game plane and the Supporters' universe (`/saves/v1`)
//! behind the game access ticket (library docs in `lib.rs`).

use fi_runtime::{ServiceInfo, http, telemetry};
use fi_saves_service::{Parts, background, config::Settings, router};
use fi_surfaces::config::ProcessEnv;
use fi_surfaces::state::AppState;

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-saves-service: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fi-saves-service stopped");
            eprintln!("fi-saves-service: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::from_env(&ProcessEnv)?;
    // The game data (FI_GAME_DATA_DIR, T-1115): every file and table checked before serving.
    fi_surfaces::gamedata::load()?;
    let info = ServiceInfo::from_env("fi-saves-service", env!("CARGO_PKG_VERSION"))
        .with_label("pool", fi_saves_service::POOL)
        .with_label("games", settings.games.join(","))
        .with_label("liveness_cache_ms", settings.liveness_ms.to_string())
        .with_label("edge_lock", settings.edge.is_some().to_string())
        .with_label("storage", settings.storage_label());
    telemetry::init(&info);
    let db = fi_db::DbConfig::from_env("DATABASE_URL", "fi-saves-service")?;
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
