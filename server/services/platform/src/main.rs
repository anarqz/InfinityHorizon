//! fi-platform: one binary for `fi-infinity`, `fi-game` and `fi-progression` (ADR 0023);
//! `FI_SURFACES` selects what this process mounts. One Cloud Run service per surface, min 0
//! instances; `all` for a standalone instance.
//!
//! Subcommands (ADR 0023 §1): `serve` (the default), `keys init [--dir <path>]`, `migrate`,
//! `accounts reset-password <email>`, `accounts import-firebase <file>` and `healthcheck`. Every one reads a variable it does not
//! find in the environment from `<FI_KEYS_DIR>/<VARIABLE>` (a standalone instance's keys volume),
//! so no secret has to be a compose default.

use fi_platform::{Config, Instance, announce, keys, ops};
use fi_runtime::{ServiceInfo, config, http, telemetry};
use fi_surfaces::config::{KeysDirEnv, ProcessEnv};
use std::process::ExitCode;

const USAGE: &str = "\
usage: fi-platform [command]

commands:
  serve                            the HTTP API (the default): FI_SURFACES=all|infinity|game|progression
  keys init [--dir <path>]         write every missing secret of a standalone instance, one file per
                                   variable (mode 0600) in <path> (default FI_KEYS_DIR); existing
                                   files are never changed
  migrate                          apply the migrations, create the runtime logins (FI_APP_PASSWORD,
                                   FI_JOBS_PASSWORD) and upsert the pools of FI_POOLS_JSON
                                   (DATABASE_MIGRATOR_URL)
  accounts reset-password <email>  FI_AUTH_BACKEND=local only: set a random temporary password
                                   (printed once on stdout) and sign the account out everywhere
  accounts import-firebase <file>  FI_AUTH_BACKEND=local only: import a `firebase auth:export` JSON
                                   file (password users keep their Firebase scrypt hash, rehashed to
                                   Argon2id at the first sign-in; others set a password by mail)
  healthcheck                      exit 0 when GET /health/ready on 127.0.0.1:$PORT answers 200
  help                             this text";

enum Command {
    Serve,
    KeysInit(Option<String>),
    Migrate,
    ResetPassword(String),
    ImportFirebase(String),
    Healthcheck,
    Help,
}

fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(match words.as_slice() {
        [] | ["serve"] => Command::Serve,
        ["keys", "init"] => Command::KeysInit(None),
        ["keys", "init", "--dir", dir] => Command::KeysInit(Some((*dir).to_owned())),
        ["migrate"] => Command::Migrate,
        ["accounts", "reset-password", email] => Command::ResetPassword((*email).to_owned()),
        ["accounts", "import-firebase", file] => Command::ImportFirebase((*file).to_owned()),
        ["healthcheck"] => Command::Healthcheck,
        ["help" | "--help" | "-h"] => Command::Help,
        _ => return Err(format!("unknown command {:?}", words.join(" "))),
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fi-platform: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // Synchronous commands first: no runtime, no log subscriber on stdout.
    match command {
        Command::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Command::KeysInit(dir) => return keys_init(dir),
        Command::Healthcheck => {
            return match ops::healthcheck() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("fi-platform healthcheck: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {}
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-platform: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        match command {
            Command::Migrate => {
                let info = ServiceInfo::from_env("fi-platform", env!("CARGO_PKG_VERSION"))
                    .with_label("command", "migrate");
                telemetry::init(&info);
                ops::migrate().await
            }
            Command::ResetPassword(email) => {
                // stdout carries the password alone: no log subscriber here.
                let env = KeysDirEnv::new(&ProcessEnv);
                ops::reset_password(&env, &email).await.map(|password| {
                    println!("{password}");
                })
            }
            Command::ImportFirebase(file) => {
                let env = KeysDirEnv::new(&ProcessEnv);
                ops::import_firebase(&env, &file).await.map(|r| {
                    eprintln!(
                        "fi-platform: imported {} with a password, {} without (set-password mail), \
                         {} FI account(s) re-pointed; {} already present, {} reclaimed from an \
                         account-less local credential, {} conflict(s), {} skipped",
                        r.with_password,
                        r.without_password,
                        r.remapped,
                        r.existing,
                        r.reclaimed,
                        r.conflicts.len(),
                        r.skipped
                    );
                    // Firebase uids only (no address): the operator resolves each by hand.
                    for uid in &r.conflicts {
                        println!("conflict firebase-uid {uid}");
                    }
                })
            }
            _ => serve().await,
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fi-platform stopped");
            eprintln!("fi-platform: {e}");
            ExitCode::FAILURE
        }
    }
}

fn keys_init(dir: Option<String>) -> ExitCode {
    let Some(dir) = dir.or_else(|| config::optional(config::KEYS_DIR)) else {
        eprintln!("fi-platform keys init: pass --dir <path> or set FI_KEYS_DIR");
        return ExitCode::from(2);
    };
    match keys::init(std::path::Path::new(dir.trim())) {
        Ok(report) => {
            eprintln!(
                "fi-platform keys init: {} written, {} kept (never overwritten){}{}",
                report.written.len(),
                report.kept.len(),
                if report.written.is_empty() { "" } else { ": " },
                report.written.join(", ")
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fi-platform keys init: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    // The game data (FI_GAME_DATA_DIR, T-1115): every file and table checked before anything else.
    let game_data = fi_surfaces::gamedata::load()?;
    let env = KeysDirEnv::new(&ProcessEnv);
    let instance = Instance::new(Config::from_env(&env)?);
    // Off unless FI_BEHOLDER_URL is set: only the instance behind the public URL sets it.
    let announce = announce::settings(&env)?;
    let info = instance.info();
    telemetry::init(&info);
    tracing::info!(
        dir = %game_data.root.display(),
        files = game_data.files,
        "game data loaded"
    );
    let db = fi_db::DbConfig::from_env("DATABASE_URL", instance.surfaces().service())?;
    let pool = fi_db::connect_lazy(&db)?;
    instance.spawn_background(&pool);
    let announcer = announce.map(|a| {
        tracing::info!(name = a.name(), "announcing to the beholder");
        tokio::spawn(a.run(pool.clone(), info.revision.clone(), http::shutdown_signal()))
    });
    // The public port and the private listener (the play dashboard's reads) stop together.
    let public = http::serve(instance.router(info, pool.clone()), http::port()?);
    let internal = http::serve_on(instance.internal_router(pool), instance.internal_bind());
    let served = tokio::try_join!(public, internal).map(|_| ());
    if let Some(task) = announcer {
        task.abort();
    }
    served?;
    Ok(())
}
