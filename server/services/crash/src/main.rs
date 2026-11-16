//! Metadata-only service/diagnostic jobs and explicit HUMAN moderation; no dump/auto-freeze CLI.
use fi_surfaces::config::ProcessEnv;
use fi_surfaces::state::AppState;
use uuid::Uuid;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    if run().await.is_err() {
        eprintln!(
            "fi-crash-service: operation failed; check configuration and private operator evidence"
        );
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
fn digest(s: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid case-reference digest".into());
    }
    Ok((0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
        .collect::<Result<_, _>>()?)
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let db = fi_db::DbConfig::from_env("DATABASE_URL", "fi-crash")?;
    let pool = fi_db::connect_lazy(&db)?;
    fi_db::migrate::check_applied(&pool).await?;
    match a.first().map(String::as_str).unwrap_or("serve") {
        "serve" => {
            let config = fi_crash_service::config::Config::from_env(&ProcessEnv)?;
            let info = fi_runtime::ServiceInfo::from_env("fi-crash", env!("CARGO_PKG_VERSION"));
            fi_runtime::telemetry::init(&info);
            let router = fi_crash_service::router(
                AppState { pool, identity: Some(config.identity) },
                config.edge,
            );
            fi_runtime::http::serve(router, fi_runtime::http::port()?).await?;
        }
        "work" => {
            let destination = if a.get(1).map(String::as_str) == Some("--deliver") {
                Some(std::env::var("FI_CRASH_DISCORD_WEBHOOK")?)
            } else { None };
            let discord = fi_crash_service::Discord::new(destination.as_deref())?;
            let count = fi_crash_service::work(&pool, &discord).await?;
            println!("crash metadata: {count} reports triaged; no automatic sanctions");
        }
        "review" if a.len() == 8 && a[7] == "--human-reviewed" => {
            let proof = digest(&a[4])?;
            let review = fi_db::crash::ManualReview {
                id: Uuid::parse_str(&a[1])?,
                report: Uuid::parse_str(&a[2])?,
                moderator: Uuid::parse_str(&a[3])?,
                case_digest: &proof,
                subject: if a[5] == "none" { None } else { Some(Uuid::parse_str(&a[5])?) },
                duration_s: if a[6] == "none" { None } else { Some(a[6].parse()?) },
            };
            let mut tx = pool.begin().await?;
            let action = fi_db::crash::review_in(&mut tx, &review).await?;
            tx.commit().await?;
            println!("human review recorded; temporary restriction={}", action.is_some());
        }
        "release" if a.len() == 3 => {
            let mut tx = pool.begin().await?;
            fi_db::crash::release_in(&mut tx, Uuid::parse_str(&a[1])?, Uuid::parse_str(&a[2])?).await?;
            tx.commit().await?;
            println!("human restriction released; unrelated moderation unchanged");
        }
        _=>return Err("serve | work [--deliver] | review action-id report-id moderator-id case-sha256 subject-id-or-none duration-seconds-or-none --human-reviewed | release action-id moderator-id".into()),
    }
    Ok(())
}
