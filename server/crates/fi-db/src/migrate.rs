//! Migrations and login roles (ADR 0004 §3, §4).
//!
//! The migrations in `crates/fi-db/migrations` are embedded at build time. sqlx records each file's
//! checksum in `_sqlx_migrations`, so an applied migration that was edited fails the next run
//! instead of drifting. Files are named `<UTC timestamp>_<area>.sql` and must sort after every file
//! on the main branch (`crates/fi-db/scripts/check-migration-order.sh`); other initiatives add
//! theirs here too (monetization's billing tables). A file whose first line is `-- no-transaction`
//! runs outside a transaction (sqlx), for `CREATE INDEX CONCURRENTLY`.
//!
//! Logins: `fi_app` (group `fi_runtime`) and `fi_jobs` (`fi_runtime`, `fi_maintenance`), created by
//! `fi-cli migrate`. In production they carry the credentials generation, `fi_app_g<N>`, so a
//! rotation creates new logins while the old ones keep working until every service has been
//! redeployed onto the new secret (docs/server/OBSERVABILITY.md §7).

use crate::pool::DbError;
use sqlx::migrate::Migrator;
use sqlx::{PgConnection, PgPool};
use std::collections::HashMap;

/// The embedded migrations.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Applies every pending migration (takes Postgres's advisory lock: concurrent runs wait).
pub async fn run(pool: &PgPool) -> Result<(), DbError> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// The newest migration this build embeds.
pub fn expected_version() -> i64 {
    MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Fails unless every migration this build embeds is applied, successfully, with the checksum it
/// has here: a new image must never serve against an old schema (readiness of every service, and
/// `fi-cli db-check`). Comparing only the newest version would pass a database that skipped an
/// older file merged late (four initiatives add files to one folder). Extra applied versions (a
/// newer image ran first) are fine: migrations only add.
pub async fn check_applied(pool: &PgPool) -> Result<i64, DbError> {
    let mut conn = pool.acquire().await?;
    check_applied_on(&mut conn).await
}

/// [`check_applied`] on one connection (inside a transaction in tests).
pub async fn check_applied_on(conn: &mut PgConnection) -> Result<i64, DbError> {
    let rows: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations WHERE success")
            .fetch_all(&mut *conn)
            .await?;
    let applied: HashMap<i64, Vec<u8>> = rows.into_iter().collect();
    for m in MIGRATOR.iter() {
        match applied.get(&m.version) {
            None => {
                return Err(DbError::Config(format!(
                    "migration {} ({}) is not applied; run fi-cli migrate",
                    m.version, m.description
                )));
            }
            Some(checksum) if checksum.as_slice() != &*m.checksum => {
                return Err(DbError::Config(format!(
                    "migration {} ({}) was applied with other contents",
                    m.version, m.description
                )));
            }
            Some(_) => {}
        }
    }
    Ok(expected_version())
}

/// Group roles a login may join (created by the first migration).
pub const GROUPS: [&str; 2] = ["fi_runtime", "fi_maintenance"];

fn valid_role(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The login names of credentials generation `generation` (`FI_LOGIN_GENERATION`): `fi_app` and
/// `fi_jobs` without one (local stack), `fi_app_g<N>` and `fi_jobs_g<N>` with one (production).
pub fn login_names(generation: Option<u32>) -> (String, String) {
    match generation {
        None => ("fi_app".into(), "fi_jobs".into()),
        Some(n) => (format!("fi_app_g{n}"), format!("fi_jobs_g{n}")),
    }
}

/// The generation a login name carries, if it is a generational runtime login.
pub fn login_generation(role: &str) -> Option<u32> {
    let digits = role
        .strip_prefix("fi_app_g")
        .or_else(|| role.strip_prefix("fi_jobs_g"))?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

mod scram {
    //! SCRAM-SHA-256 verifiers (RFC 5802, RFC 7677), the format Postgres stores in `pg_authid`.
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes keys of any length");
        mac.update(data);
        mac.finalize().into_bytes().into()
    }

    /// PBKDF2-HMAC-SHA-256 with one output block (Postgres' `scram_SaltedPassword`).
    fn salted_password(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
        let mut block = salt.to_vec();
        block.extend_from_slice(&1u32.to_be_bytes());
        let mut u = hmac(password, &block);
        let mut out = u;
        for _ in 1..iterations {
            u = hmac(password, &u);
            for (o, b) in out.iter_mut().zip(u) {
                *o ^= b;
            }
        }
        out
    }

    /// `(ClientKey, StoredKey, ServerKey)` of a password.
    pub fn keys(password: &str, salt: &[u8], iterations: u32) -> ([u8; 32], [u8; 32], [u8; 32]) {
        let salted = salted_password(password.as_bytes(), salt, iterations);
        let client_key = hmac(&salted, b"Client Key");
        let stored_key: [u8; 32] = Sha256::digest(client_key).into();
        (client_key, stored_key, hmac(&salted, b"Server Key"))
    }

    /// HMAC-SHA-256, for the RFC 7677 check of the tests.
    #[cfg(test)]
    pub fn sign(key: &[u8], data: &[u8]) -> [u8; 32] {
        hmac(key, data)
    }

    /// `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`. The password is ASCII
    /// (checked by the caller), for which SASLprep is the identity, as Postgres computes it.
    pub fn verifier(password: &str, salt: &[u8], iterations: u32) -> String {
        let (_, stored_key, server_key) = keys(password, salt, iterations);
        format!(
            "SCRAM-SHA-256${iterations}:{}${}:{}",
            STANDARD.encode(salt),
            STANDARD.encode(stored_key),
            STANDARD.encode(server_key)
        )
    }
}

/// Postgres' default SCRAM iteration count.
const SCRAM_ITERATIONS: u32 = 4096;

/// Creates or updates a login role with `password` and makes it a member of `groups` (and of
/// nothing else it should not be): `NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS`.
///
/// The password itself never reaches the server: the client computes its SCRAM-SHA-256 verifier
/// (random salt), and only the verifier is sent, as a bound parameter quoted by the server
/// (`format('%L')`). A statement log (`log_statement`, `log_min_duration_statement`) can print bind
/// parameters, so it may show the verifier, which does not let anyone sign in and cannot be
/// reversed for a 40-character random password.
pub async fn ensure_login(
    pool: &PgPool,
    role: &str,
    password: &str,
    groups: &[&str],
) -> Result<(), DbError> {
    if !valid_role(role) || groups.iter().any(|g| !GROUPS.contains(g)) {
        return Err(DbError::Config(format!(
            "refusing role {role:?} or its groups"
        )));
    }
    if password.len() < 16 || !password.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(DbError::Config(format!(
            "the password of {role} must have at least 16 printable ASCII characters"
        )));
    }
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt)
        .map_err(|_| DbError::Config("the system random source is unavailable".into()))?;
    let verifier = scram::verifier(password, &salt, SCRAM_ITERATIONS);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('fi.login_role', $1, true), set_config('fi.login_verifier', $2, true), set_config('fi.login_groups', $3, true)")
        .bind(role)
        .bind(&verifier)
        .bind(groups.join(","))
        .execute(&mut *tx)
        .await?;
    // Only a superuser may even restate NOSUPERUSER, so an existing role gets its password alone;
    // its attributes are then checked instead of set. A failing EXECUTE would echo its statement in
    // the error context: the handler re-raises with the message only.
    sqlx::query(
        r"DO $$
        DECLARE
          r text := current_setting('fi.login_role');
          p text := current_setting('fi.login_verifier');
          g text;
        BEGIN
          BEGIN
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r) THEN
              EXECUTE format('ALTER ROLE %I WITH LOGIN PASSWORD %L', r, p);
            ELSE
              EXECUTE format('CREATE ROLE %I WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION PASSWORD %L', r, p);
            END IF;
          EXCEPTION WHEN OTHERS THEN
            RAISE EXCEPTION 'could not create or update login role %: % (%)', r, SQLERRM, SQLSTATE;
          END;
          IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r
                     AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolbypassrls OR rolreplication)) THEN
            RAISE EXCEPTION 'login role % holds privileges a runtime login must not have', r;
          END IF;
          FOREACH g IN ARRAY string_to_array(nullif(current_setting('fi.login_groups'), ''), ',') LOOP
            EXECUTE format('GRANT %I TO %I', g, r);
          END LOOP;
        END
        $$",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Drops the generational runtime logins older than `keep_from` (`fi_app_g<N>`, `fi_jobs_g<N>`
/// with `N < keep_from`). `fi-cli migrate` of generation `G` keeps `G - 1`, whose instances may
/// still be running until every service is redeployed, and drops the rest. Returns the dropped
/// names.
pub async fn retire_logins(pool: &PgPool, keep_from: u32) -> Result<Vec<String>, DbError> {
    let candidates: Vec<String> = sqlx::query_scalar(
        "SELECT rolname::text FROM pg_roles WHERE rolname ~ '^fi_(app|jobs)_g[0-9]+$'",
    )
    .fetch_all(pool)
    .await?;
    let mut dropped = Vec::new();
    for role in candidates {
        if login_generation(&role).is_some_and(|g| g < keep_from) && valid_role(&role) {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP ROLE IF EXISTS \"{role}\""
            )))
            .execute(pool)
            .await?;
            dropped.push(role);
        }
    }
    Ok(dropped)
}

/// Attempts of [`apply`] when a migration waits too long for a lock (`lock_timeout`, 55P03).
pub const APPLY_ATTEMPTS: u32 = 3;

/// The logins [`apply`] creates or updates: a password per runtime login (`None` leaves it as it
/// is) and the credentials generation (`FI_LOGIN_GENERATION`, production).
#[derive(Clone, Default)]
pub struct Logins {
    /// `FI_LOGIN_GENERATION`: logins `fi_app_gN`/`fi_jobs_gN`, older than N-1 dropped.
    pub generation: Option<u32>,
    /// `FI_APP_PASSWORD` -> `fi_app` (`fi_runtime`).
    pub app_password: Option<String>,
    /// `FI_JOBS_PASSWORD` -> `fi_jobs` (`fi_runtime`, `fi_maintenance`).
    pub jobs_password: Option<String>,
}

impl std::fmt::Debug for Logins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logins")
            .field("generation", &self.generation)
            .field("app_password", &self.app_password.as_ref().map(|_| "***"))
            .field("jobs_password", &self.jobs_password.as_ref().map(|_| "***"))
            .finish()
    }
}

/// `fi-cli migrate` and `fi-platform migrate`: every pending migration (retried on a lock timeout,
/// 5 s then 10 s), the schema check, then the login roles whose passwords are set, then (with a
/// generation) the generations older than N-1 retired. Returns the applied version.
pub async fn apply(pool: &PgPool, logins: &Logins) -> Result<i64, DbError> {
    let mut attempt = 1;
    loop {
        match run(pool).await {
            Ok(()) => break,
            Err(e) if e.is_lock_timeout() && attempt < APPLY_ATTEMPTS => {
                tracing::warn!(attempt, error = %e, "a migration waited too long for a lock; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(5 * u64::from(attempt))).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
    let version = check_applied(pool).await?;
    tracing::info!(version, "migrations applied");
    let (app, jobs) = login_names(logins.generation);
    for (role, variable, password, groups) in [
        (
            app.as_str(),
            "FI_APP_PASSWORD",
            &logins.app_password,
            &["fi_runtime"][..],
        ),
        (
            jobs.as_str(),
            "FI_JOBS_PASSWORD",
            &logins.jobs_password,
            &["fi_runtime", "fi_maintenance"][..],
        ),
    ] {
        match password {
            Some(password) => {
                ensure_login(pool, role, password, groups).await?;
                tracing::info!(role, groups = ?groups, "login role ensured");
            }
            None => tracing::info!(role, variable, "password not set; login role left as is"),
        }
    }
    if let Some(generation) = logins.generation {
        // The previous generation stays until every service has been redeployed onto this one.
        let dropped = retire_logins(pool, generation.saturating_sub(1)).await?;
        tracing::info!(generation, dropped = ?dropped, "older login generations retired");
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_names_are_plain_identifiers() {
        assert!(valid_role("fi_app"));
        assert!(valid_role("fi_jobs2"));
        assert!(!valid_role("fi-app"));
        assert!(!valid_role("FI_APP"));
        assert!(!valid_role("x; DROP TABLE account"));
        assert!(!valid_role(""));
    }

    #[test]
    fn every_migration_is_embedded_in_order() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert!(versions.len() >= 5);
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
        assert!(MIGRATOR.iter().all(|m| !m.description.is_empty()));
    }

    #[test]
    fn logins_carry_their_generation() {
        assert_eq!(login_names(None), ("fi_app".into(), "fi_jobs".into()));
        assert_eq!(
            login_names(Some(3)),
            ("fi_app_g3".into(), "fi_jobs_g3".into())
        );
        assert_eq!(login_generation("fi_app_g12"), Some(12));
        assert_eq!(login_generation("fi_jobs_g1"), Some(1));
        assert_eq!(login_generation("fi_app"), None);
        assert_eq!(login_generation("fi_app_g"), None);
        assert_eq!(login_generation("fi_app_gx"), None);
    }

    #[test]
    fn scram_keys_reproduce_the_rfc_7677_exchange() {
        // RFC 7677 §3: user "user", password "pencil". The keys this module derives must produce
        // the client proof and the server signature printed in the RFC for that exchange.
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let (client_key, stored_key, server_key) = scram::keys("pencil", &salt, 4096);
        let auth_message = "n=user,r=rOprNGfwEbeRWgbNEkqO,\
            r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,\
            c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let client_signature = scram::sign(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_signature)
            .map(|(k, s)| k ^ s)
            .collect();
        assert_eq!(
            STANDARD.encode(proof),
            "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        assert_eq!(
            STANDARD.encode(scram::sign(&server_key, auth_message.as_bytes())),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
        let v = scram::verifier("pencil", &salt, 4096);
        assert!(
            v.starts_with("SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$"),
            "{v}"
        );
        assert!(!v.contains("pencil"));
    }
}
