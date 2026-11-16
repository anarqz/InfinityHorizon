//! fi-gate: the gate deployable (Cloud Run `fi-gate`, min 0; locally its routes run in fi-platform's
//! `infinity` service). The router lives in this package's library (`lib.rs`) so the HTTP tests
//! drive the same one.

use fi_gate::{config::GateConfig, router, service_info};
use fi_runtime::{http, telemetry};

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-gate: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fi-gate stopped");
            eprintln!("fi-gate: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = GateConfig::from_env(&fi_surfaces::config::ProcessEnv)?;
    let info = service_info(&config);
    telemetry::init(&info);
    let db = fi_db::DbConfig::from_env("DATABASE_URL", "fi-gate")?;
    let pool = fi_db::connect_lazy(&db)?;
    let state = fi_surfaces::state::AppState {
        pool,
        identity: Some(config.identity.clone()),
    };
    http::serve(router(info, state, &config), http::port()?).await?;
    Ok(())
}
