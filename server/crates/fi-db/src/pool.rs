//! Connection pool settings (ADR 0004 §4).
//!
//! Connection budget: Cloud SQL's `max_connections` must cover every instance of every service at
//! its pool maximum, the instances of a second revision of the service being deployed, the jobs and
//! an operator session, minus the connections Postgres reserves (infra/terraform/modules/database
//! `main.tf`, a plan-time precondition). Each connection is opened with timeouts so a stuck query or
//! an abandoned transaction cannot hold row locks (and therefore save leases) hostage; jobs and
//! migrations get longer limits than services ([`Workload`]).

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use std::time::Duration;

/// A database failure, or a bad setting.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A setting is missing or malformed (never echoes a URL: it holds a password).
    #[error("database configuration: {0}")]
    Config(String),
    /// A query or connection failed.
    #[error("database: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A migration failed or an applied one was edited.
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

impl DbError {
    /// Whether a statement gave up waiting for a lock (`lock_timeout`, SQLSTATE 55P03): retrying
    /// later can succeed.
    pub fn is_lock_timeout(&self) -> bool {
        let lock = |e: &sqlx::Error| matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03"));
        match self {
            DbError::Sqlx(e) => lock(e),
            DbError::Migrate(sqlx::migrate::MigrateError::Execute(e)) => lock(e),
            _ => false,
        }
    }
}

/// What kind of process the pool serves; each gets its own session limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workload {
    /// A Cloud Run service: statements 5 s, lock waits 2 s (a save commit waits for one profile's
    /// row; anything longer is an incident), idle in a transaction 10 s, whole transactions 30 s.
    /// The transaction cap is load-bearing: the progression watermark (`profile_progress.
    /// consolidated_through`, 60 s behind) relies on no service transaction living longer.
    Service,
    /// A Cloud Run job (retention, garbage collection, erasure, nightly
    /// consolidation): statements 15 min (deletes are batched), lock waits 5 s, idle in a
    /// transaction 60 s, no transaction cap.
    Job,
    /// `fi-cli migrate`: no statement cap (index builds, constraint validation, backfills), lock
    /// waits 5 s (the CLI retries on `lock_not_available`), idle in a transaction 60 s.
    Migrate,
}

impl Workload {
    /// `(statement_timeout, lock_timeout, idle_in_transaction_session_timeout,
    /// transaction_timeout)` in milliseconds; 0 disables one.
    pub const fn limits_ms(self) -> (u32, u32, u32, u32) {
        match self {
            Workload::Service => (5_000, 2_000, 10_000, 30_000),
            Workload::Job => (900_000, 5_000, 60_000, 0),
            Workload::Migrate => (0, 5_000, 60_000, 0),
        }
    }
}

/// Pool settings.
#[derive(Clone)]
pub struct DbConfig {
    /// `postgres://user:password@host/db` or `?host=/cloudsql/<connection>` on Cloud Run.
    pub url: String,
    /// Connections per instance (`DB_POOL_MAX`, default 5; Terraform sets it per deployment).
    pub max_connections: u32,
    /// How long a request waits for a connection (`DB_ACQUIRE_TIMEOUT_MS`, default 3000).
    pub acquire_timeout: Duration,
    /// Server-side cap per statement (`DB_STATEMENT_TIMEOUT_MS`, default from the workload).
    pub statement_timeout_ms: u32,
    /// The workload's other limits.
    pub workload: Workload,
    /// Shown in `pg_stat_activity` and Query Insights.
    pub application_name: String,
}

impl std::fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbConfig")
            .field("max_connections", &self.max_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("statement_timeout_ms", &self.statement_timeout_ms)
            .field("workload", &self.workload)
            .field("application_name", &self.application_name)
            .finish_non_exhaustive()
    }
}

fn env_number<T: FromStr>(name: &str, default: T) -> Result<T, DbError> {
    match process_env(name) {
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| DbError::Config(format!("{name} is not a number"))),
        None => Ok(default),
    }
}

/// A variable of the process environment, else `<FI_KEYS_DIR>/<name>` (a standalone instance's
/// keys directory, ADR 0023 §9; the same rule as `fi_runtime::config::secret`, which this crate
/// does not depend on): blank is unset, a set variable always wins.
fn process_env(name: &str) -> Option<String> {
    lookup_with_keys_dir(&|n| std::env::var(n).ok(), name)
}

fn lookup_with_keys_dir(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    let set = |n: &str| env(n).filter(|v| !v.trim().is_empty());
    set(name).or_else(|| {
        let dir = set("FI_KEYS_DIR")?;
        if !name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return None;
        }
        let path = std::path::Path::new(dir.trim()).join(name);
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > 64 * 1024 {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    })
}

/// The password variable of a login a standalone instance composes a URL for.
fn password_variable(user: &str) -> Option<&'static str> {
    match user {
        "fi_migrator" => Some("FI_MIGRATOR_PASSWORD"),
        "fi_app" => Some("FI_APP_PASSWORD"),
        "fi_jobs" => Some("FI_JOBS_PASSWORD"),
        _ => None,
    }
}

/// `url_var` itself, or, when it is unset and `FI_DB_HOST` is set (a standalone instance), the URL
/// composed from `FI_DB_HOST`, `FI_DB_PORT` (5432), `FI_DB_NAME` (`forza_infinity`), the login
/// (`fi_migrator` for `DATABASE_MIGRATOR_URL`, else `FI_DB_USER`, default `fi_app`) and its
/// password variable (`FI_MIGRATOR_PASSWORD`, `FI_APP_PASSWORD`, `FI_JOBS_PASSWORD`; usually the
/// keys directory's file). Errors name variables, never a value.
fn database_url(env: &dyn Fn(&str) -> Option<String>, url_var: &str) -> Result<String, DbError> {
    let get = |n: &str| lookup_with_keys_dir(env, n);
    if let Some(url) = get(url_var) {
        return Ok(url);
    }
    let Some(host) = get("FI_DB_HOST") else {
        return Err(DbError::Config(format!("{url_var} is required")));
    };
    let user = if url_var == "DATABASE_MIGRATOR_URL" {
        "fi_migrator".to_owned()
    } else {
        get("FI_DB_USER").unwrap_or_else(|| "fi_app".to_owned())
    };
    let user = user.trim();
    let variable = password_variable(user)
        .ok_or_else(|| DbError::Config("FI_DB_USER must be fi_app or fi_jobs".into()))?;
    let password = get(variable)
        .ok_or_else(|| DbError::Config(format!("{url_var} or {variable} is required")))?;
    // Generated passwords are hex; anything else must not need URL encoding.
    if !password
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(DbError::Config(format!(
            "{variable} may hold only letters, digits, - and _ (set {url_var} instead)"
        )));
    }
    let port: u16 = match get("FI_DB_PORT") {
        Some(p) => p
            .trim()
            .parse()
            .map_err(|_| DbError::Config("FI_DB_PORT is not a port".into()))?,
        None => 5432,
    };
    let name = get("FI_DB_NAME").unwrap_or_else(|| "forza_infinity".to_owned());
    let host = host.trim();
    let name = name.trim();
    let plain = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if !plain(host) || !plain(name) {
        return Err(DbError::Config(
            "FI_DB_HOST and FI_DB_NAME are plain host and database names".into(),
        ));
    }
    Ok(format!("postgres://{user}:{password}@{host}:{port}/{name}"))
}

impl DbConfig {
    /// A service's settings: reads `url_var` (`DATABASE_URL`) and the pool settings.
    pub fn from_env(url_var: &str, application_name: &str) -> Result<Self, DbError> {
        Self::from_env_for(url_var, application_name, Workload::Service)
    }

    /// The settings of `workload`: reads `url_var` (`DATABASE_URL`, or `DATABASE_MIGRATOR_URL` for
    /// migrations) and the pool settings.
    pub fn from_env_for(
        url_var: &str,
        application_name: &str,
        workload: Workload,
    ) -> Result<Self, DbError> {
        let url = database_url(&|n| std::env::var(n).ok(), url_var)?;
        Ok(Self {
            url,
            max_connections: env_number("DB_POOL_MAX", 5)?,
            acquire_timeout: Duration::from_millis(env_number("DB_ACQUIRE_TIMEOUT_MS", 3_000)?),
            statement_timeout_ms: env_number("DB_STATEMENT_TIMEOUT_MS", workload.limits_ms().0)?,
            workload,
            application_name: application_name.to_owned(),
        })
    }

    /// Connect options with the session settings every connection of the workload gets.
    pub fn connect_options(&self) -> Result<PgConnectOptions, DbError> {
        let options = PgConnectOptions::from_str(&self.url)
            .map_err(|_| DbError::Config("database URL does not parse".into()))?;
        let (_, lock, idle, transaction) = self.workload.limits_ms();
        Ok(options.application_name(&self.application_name).options([
            ("statement_timeout", self.statement_timeout_ms.to_string()),
            ("lock_timeout", lock.to_string()),
            ("idle_in_transaction_session_timeout", idle.to_string()),
            ("transaction_timeout", transaction.to_string()),
            ("timezone", "UTC".to_owned()),
        ]))
    }
}

/// A pool that connects on first use, so a revision starts even while the database is briefly
/// unreachable; `/health/ready` reports it until it answers.
pub fn connect_lazy(config: &DbConfig) -> Result<PgPool, DbError> {
    Ok(PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .idle_timeout(Duration::from_secs(300))
        .max_lifetime(Duration::from_secs(1800))
        .connect_lazy_with(config.connect_options()?))
}

/// Readiness: one round trip.
pub async fn ping(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standalone_instance_composes_its_urls_from_the_keys_directory() {
        let dir = std::env::temp_dir().join(format!("fi-db-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("FI_APP_PASSWORD"), "a1b2\n").unwrap();
        std::fs::write(dir.join("FI_JOBS_PASSWORD"), "c3d4").unwrap();
        std::fs::write(dir.join("FI_MIGRATOR_PASSWORD"), "e5f6").unwrap();
        let dir_text = dir.to_string_lossy().into_owned();
        let env = |pairs: Vec<(&'static str, String)>| {
            move |n: &str| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone())
        };
        let base = || {
            vec![
                ("FI_KEYS_DIR", dir_text.clone()),
                ("FI_DB_HOST", "postgres".to_owned()),
            ]
        };
        assert_eq!(
            database_url(&env(base()), "DATABASE_URL").unwrap(),
            "postgres://fi_app:a1b2@postgres:5432/forza_infinity"
        );
        assert_eq!(
            database_url(&env(base()), "DATABASE_MIGRATOR_URL").unwrap(),
            "postgres://fi_migrator:e5f6@postgres:5432/forza_infinity"
        );
        let mut jobs = base();
        jobs.push(("FI_DB_USER", "fi_jobs".to_owned()));
        assert_eq!(
            database_url(&env(jobs), "DATABASE_URL").unwrap(),
            "postgres://fi_jobs:c3d4@postgres:5432/forza_infinity"
        );
        // A set URL wins; without FI_DB_HOST nothing is composed.
        let mut set = base();
        set.push(("DATABASE_URL", "postgres://x:y@h/d".to_owned()));
        assert_eq!(
            database_url(&env(set), "DATABASE_URL").unwrap(),
            "postgres://x:y@h/d"
        );
        let none = vec![("FI_KEYS_DIR", dir_text.clone())];
        assert!(database_url(&env(none), "DATABASE_URL").is_err());
        // An unknown login, and an error never echoes a password.
        let mut other = base();
        other.push(("FI_DB_USER", "postgres".to_owned()));
        assert!(database_url(&env(other), "DATABASE_URL").is_err());
        let mut odd = base();
        odd.push(("FI_APP_PASSWORD", "p@ss/word".to_owned()));
        let e = database_url(&env(odd), "DATABASE_URL")
            .unwrap_err()
            .to_string();
        assert!(!e.contains("p@ss"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jobs_and_migrations_outlive_service_limits() {
        let (statement, lock, _, transaction) = Workload::Service.limits_ms();
        assert_eq!((statement, lock, transaction), (5_000, 2_000, 30_000));
        let (statement, lock, _, transaction) = Workload::Job.limits_ms();
        assert!(statement >= 600_000 && lock >= 5_000 && transaction == 0);
        let (statement, _, _, transaction) = Workload::Migrate.limits_ms();
        assert_eq!(
            (statement, transaction),
            (0, 0),
            "an index build is never cancelled"
        );
    }
}
