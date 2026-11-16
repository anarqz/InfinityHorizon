//! Platform commands (workstream A, docs/server/LOCAL_ENV.md §4.1, §5):
//!
//! - `local-seed`: pools from `FI_POOLS_JSON` (upsert as the migrator) and a check that
//!   monetization's `entitlement` table is migrated (the local VIP grants write it); refused when
//!   `APP_ENV` or `FI_ENV` is `prod`. With `FI_PROBE_DIR` set it also copies this executable there, so the
//!   compose healthchecks of the distroless fi-api images can run `fi-cli probe`;
//! - `pools-sync`: the pool upsert alone;
//! - `sessions-reap [--every <secs>]`: expire silent sessions, drop unpolled queued ones, end
//!   sessions past their maximum age, each with its session summary; with `--every`
//!   it loops (the local stand-in for the Scheduler job), logging and continuing on database
//!   errors, until SIGTERM;
//! - `probe <http://host:port/path>`: exits 0 when the URL answers 200 (a compose healthcheck;
//!   plain HTTP/1.0 over `std::net`, 2 s timeouts, nothing logged but the outcome).

use super::Outcome;
use fi_db::{DbConfig, Workload, admission};
use fi_runtime::config;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  local-seed       pools from FI_POOLS_JSON, entitlement table check (local only;
                   DATABASE_MIGRATOR_URL; FI_PROBE_DIR also installs this binary for `probe`)
  pools-sync       upsert the pool rows from FI_POOLS_JSON (DATABASE_MIGRATOR_URL)
  sessions-reap    expire, drop and end stale play sessions (DATABASE_URL); --every <secs> loops
  probe <url>      exit 0 when the URL answers HTTP 200 (compose healthchecks)";

/// Runs `command` with `args`; `None` when it is not a platform command.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    Some(match command {
        "local-seed" | "pools-sync" => match no_args(command, args) {
            Ok(()) => local_seed(command == "local-seed").await,
            Err(e) => Err(e),
        },
        "sessions-reap" => sessions_reap(args).await,
        "probe" => probe(args),
        _ => return None,
    })
}

fn no_args(command: &str, args: &[String]) -> Outcome {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("{command} takes no arguments").into())
    }
}

fn is_prod() -> bool {
    ["APP_ENV", "FI_ENV"]
        .into_iter()
        .any(|n| config::optional(n).is_some_and(|v| v.trim().eq_ignore_ascii_case("prod")))
}

/// `local-seed` (`local = true`) or `pools-sync`.
async fn local_seed(local: bool) -> Outcome {
    if local && is_prod() {
        return Err("local-seed is refused when APP_ENV or FI_ENV is prod".into());
    }
    let pools = match config::optional("FI_POOLS_JSON") {
        Some(text) => admission::parse_pools_json(&text)?,
        None => return Err("FI_POOLS_JSON is required".into()),
    };
    let name = if local {
        "fi-cli-local-seed"
    } else {
        "fi-cli-pools-sync"
    };
    let db = DbConfig::from_env_for("DATABASE_MIGRATOR_URL", name, Workload::Migrate)?;
    let pool = fi_db::connect_lazy(&db)?;
    let mut tx = pool.begin().await?;
    let changed = admission::upsert_pools_in(&mut tx, &pools).await?;
    if local {
        admission::ensure_local_entitlement_in(&mut tx).await?;
    }
    tx.commit().await?;
    pool.close().await;
    let names: Vec<&str> = pools.iter().map(|p| p.name.as_str()).collect();
    tracing::info!(pools = ?names, changed, entitlement_checked = local, "pools seeded");
    if local && let Some(dir) = config::optional("FI_PROBE_DIR") {
        install_probe(dir.trim())?;
    }
    Ok(())
}

/// Copies this executable to `<dir>/fi-cli` (write, then rename: a healthcheck never runs a half
/// written file).
fn install_probe(dir: &str) -> Outcome {
    let me = std::env::current_exe()?;
    let target = std::path::Path::new(dir).join("fi-cli");
    let partial = std::path::Path::new(dir).join(".fi-cli.partial");
    std::fs::copy(&me, &partial)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&partial, &target)?;
    tracing::info!(path = %target.display(), "healthcheck probe installed");
    Ok(())
}

/// `--every <secs>` (`5` or `5s`), when given.
fn every(args: &[String]) -> Result<Option<Duration>, Box<dyn std::error::Error>> {
    match args {
        [] => Ok(None),
        [flag, value] if flag == "--every" => {
            let secs: u64 = value
                .trim()
                .trim_end_matches('s')
                .parse()
                .ok()
                .filter(|s| (1..=3_600).contains(s))
                .ok_or("--every takes 1 to 3600 seconds")?;
            Ok(Some(Duration::from_secs(secs)))
        }
        _ => Err("usage: sessions-reap [--every <secs>]".into()),
    }
}

/// `sessions-reap [--every <secs>]`.
async fn sessions_reap(args: &[String]) -> Outcome {
    let every = every(args)?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-sessions-reap", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let Some(period) = every else {
        let reaped = admission::reap(&pool).await?;
        tracing::info!(
            expired = reaped.expired,
            queue_left = reaped.queue_left,
            max_age = reaped.max_age,
            "sessions reaped"
        );
        pool.close().await;
        return Ok(());
    };
    tracing::info!(every_secs = period.as_secs(), "sessions reaper started");
    let mut shutdown = std::pin::pin!(fi_runtime::http::shutdown_signal());
    loop {
        match admission::reap(&pool).await {
            Ok(r) if r.total() > 0 => tracing::info!(
                expired = r.expired,
                queue_left = r.queue_left,
                max_age = r.max_age,
                "sessions reaped"
            ),
            Ok(_) => tracing::debug!("nothing to reap"),
            // A restarting database, a lock timeout: the next pass retries.
            Err(e) => tracing::warn!(error = %e, "sessions reap failed; retrying next pass"),
        }
        tokio::select! {
            () = tokio::time::sleep(period) => {}
            () = &mut shutdown => break,
        }
    }
    pool.close().await;
    Ok(())
}

/// `probe <http://host:port/path>`: HTTP 200 or an error.
fn probe(args: &[String]) -> Outcome {
    let [url] = args else {
        return Err("usage: probe <http://host:port/path>".into());
    };
    let rest = url
        .strip_prefix("http://")
        .ok_or("probe takes an http:// URL")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let address = authority
        .to_socket_addrs()?
        .next()
        .ok_or("the probe's host does not resolve")?;
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {authority}\r\nUser-Agent: fi-cli-probe\r\n\r\n"
    )?;
    let mut head = [0u8; 64];
    let n = stream.read(&mut head)?;
    let status_line = String::from_utf8_lossy(&head[..n]);
    let status = status_line.split_whitespace().nth(1).unwrap_or("none");
    if status == "200" {
        Ok(())
    } else {
        Err(format!("{path} answered {status}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_parses_seconds() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(every(&args(&[])).unwrap(), None);
        assert_eq!(
            every(&args(&["--every", "5"])).unwrap(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            every(&args(&["--every", "5s"])).unwrap(),
            Some(Duration::from_secs(5))
        );
        assert!(every(&args(&["--every", "0"])).is_err());
        assert!(every(&args(&["--often"])).is_err());
    }

    #[test]
    fn probe_refuses_what_it_cannot_reach() {
        assert!(probe(&["https://x".into()]).is_err());
        assert!(probe(&[]).is_err());
        // Nothing listens on port 9.
        assert!(probe(&["http://127.0.0.1:9/health/ready".into()]).is_err());
    }
}
