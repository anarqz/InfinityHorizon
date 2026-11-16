//! Operator subcommands of `fi-platform` (ADR 0023 §1, §9): `migrate`, `accounts
//! reset-password` and `healthcheck`. Secrets come from the environment or the keys directory
//! (`FI_KEYS_DIR`); nothing here logs a password, an e-mail address or a URL.

use fi_db::{DbConfig, Workload, admission, credential, migrate, tokens};
use fi_runtime::config;
use fi_surfaces::config::Env;
use std::io::{Read, Write};
use std::time::Duration;

/// A subcommand's outcome.
pub type Outcome = Result<(), Box<dyn std::error::Error>>;

/// `migrate`: the embedded migrations and the runtime logins, exactly as `fi-cli migrate`
/// ([`migrate::apply`]; `DATABASE_MIGRATOR_URL`, `FI_APP_PASSWORD`, `FI_JOBS_PASSWORD`,
/// `FI_LOGIN_GENERATION`), then the pool rows of `FI_POOLS_JSON` when it is set (as `fi-cli
/// pools-sync`), so a standalone instance's pools name its own origin.
pub async fn migrate() -> Outcome {
    let generation: Option<u32> = match config::optional("FI_LOGIN_GENERATION") {
        Some(v) => Some(
            v.trim()
                .parse()
                .ok()
                .filter(|g| *g > 0)
                .ok_or("FI_LOGIN_GENERATION is a positive integer")?,
        ),
        None => None,
    };
    let pools = config::optional("FI_POOLS_JSON")
        .map(|text| admission::parse_pools_json(&text))
        .transpose()?;
    let db = DbConfig::from_env_for(
        "DATABASE_MIGRATOR_URL",
        "fi-platform-migrate",
        Workload::Migrate,
    )?;
    let pool = fi_db::connect_lazy(&db)?;
    let logins = migrate::Logins {
        generation,
        app_password: config::secret("FI_APP_PASSWORD"),
        jobs_password: config::secret("FI_JOBS_PASSWORD"),
    };
    migrate::apply(&pool, &logins).await?;
    if let Some(pools) = pools {
        let mut tx = pool.begin().await?;
        let changed = admission::upsert_pools_in(&mut tx, &pools).await?;
        tx.commit().await?;
        let names: Vec<&str> = pools.iter().map(|p| p.name.as_str()).collect();
        tracing::info!(pools = ?names, changed, "pools synced");
    }
    pool.close().await;
    Ok(())
}

/// Characters of a temporary password (hex of 12 random bytes: 24, inside the 8-128 rule).
const TEMPORARY_BYTES: usize = 12;

/// `accounts reset-password <email>` (a `local` instance only): a random temporary password
/// replaces the credential's hash (also "password not set"), the address counts as confirmed,
/// and every refresh family of its account is revoked
/// (`password_changed`), in one transaction as the migrator (`DATABASE_MIGRATOR_URL`). Returns
/// the temporary password, which the caller prints once; ID tokens issued before stop working
/// because the credential's `updated_at` moves.
pub async fn reset_password(
    env: &dyn Env,
    email: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let backend = env.get("FI_AUTH_BACKEND").unwrap_or_default();
    if backend.trim() != "local" {
        return Err(
            "accounts reset-password works only with FI_AUTH_BACKEND=local (a firebase instance \
             resets passwords in Firebase Authentication)"
                .into(),
        );
    }
    let email = fi_surfaces::launcher::local::normalize_email(email)
        .ok_or("not an e-mail address (3-254 characters with one @)")?;
    let temporary = fi_auth::hex(&fi_auth::random_bytes::<TEMPORARY_BYTES>()?);
    let phc = fi_auth::password::hash(&temporary)?;
    let db = DbConfig::from_env_for(
        "DATABASE_MIGRATOR_URL",
        "fi-platform-accounts",
        Workload::Job,
    )?;
    let pool = fi_db::connect_lazy(&db)?;
    let mut tx = pool.begin().await?;
    let Some(found) = credential::find_by_email_in(&mut tx, &email).await? else {
        return Err("no local credential has that address".into());
    };
    if !credential::set_password_hash_in(&mut tx, found.id, &phc).await? {
        return Err("the credential disappeared; nothing changed".into());
    }
    // The operator vouches for the address (support checked it out of band): an unconfirmed
    // credential is confirmed too, as the password mail would (CR-S53).
    credential::confirm_email_in(&mut tx, found.id).await?;
    let revoked = match credential::account_of_in(&mut tx, &found.id.to_string()).await? {
        Some(account) => tokens::revoke_account_in(&mut tx, account, "password_changed").await?,
        None => 0,
    };
    tx.commit().await?;
    pool.close().await;
    tracing::info!(credential = %found.id, revoked, "password reset by the operator");
    eprintln!(
        "fi-platform: the password of credential {} was reset; {revoked} refresh token(s) revoked. \
         The temporary password follows on stdout (shown once): sign in with it, then change it.",
        found.id
    );
    Ok(temporary)
}

/// What `accounts import-firebase` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// New credentials with a Firebase password hash (rehashed at their first sign-in).
    pub with_password: usize,
    /// New credentials without one ("password not set": the set-password mail only).
    pub without_password: usize,
    /// New credentials whose FI account (mapped to the Firebase uid) now names them.
    pub remapped: usize,
    /// Addresses that already had a credential and nothing orphaned (a rerun): untouched.
    pub existing: usize,
    /// Addresses whose local credential had no FI account while the Firebase uid had one: the
    /// imported account took the credential over (security review H2).
    pub reclaimed: usize,
    /// Firebase uids whose address belongs to another FI account here: nothing changed; the
    /// operator resolves them (uids only, never addresses).
    pub conflicts: Vec<String>,
    /// Export entries without a usable address: skipped.
    pub skipped: usize,
}

/// The users of a `firebase auth:export --format=json` file (`{"users": [{localId, email,
/// emailVerified, passwordHash, salt, …}]}`): entries without a usable e-mail are counted in the
/// second value and skipped; a hash without a salt (or either not base64) imports as "password not
/// set". Pure, so the shape is tested without a database.
pub fn parse_firebase_export(
    text: &str,
) -> Result<(Vec<credential::ImportedUser>, usize), Box<dyn std::error::Error>> {
    let doc: serde_json::Value = serde_json::from_str(text)?;
    let users = doc
        .get("users")
        .and_then(serde_json::Value::as_array)
        .ok_or("not a Firebase auth export (no `users` array)")?;
    let mut out = Vec::with_capacity(users.len());
    let mut skipped = 0;
    let field = |u: &serde_json::Value, name: &str| {
        u.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .filter(|v| !v.trim().is_empty())
    };
    for user in users {
        let (Some(uid), Some(email)) = (
            field(user, "localId"),
            field(user, "email").and_then(|e| fi_surfaces::launcher::local::normalize_email(&e)),
        ) else {
            skipped += 1;
            continue;
        };
        let password_hash = match (field(user, "passwordHash"), field(user, "salt")) {
            (Some(hash), Some(salt)) => fi_auth::firebase_scrypt::stored(&salt, &hash),
            _ => None,
        };
        out.push(credential::ImportedUser {
            firebase_uid: uid,
            email_lower: email,
            password_hash,
            email_verified: user
                .get("emailVerified")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        });
    }
    Ok((out, skipped))
}

/// `accounts import-firebase <export.json>` (a `local` instance only; the migration of OD-122
/// follow-up 2026-10-26; run it with registration closed, `FI_REGISTRATION_ENABLED=false`): every user of a Firebase Authentication export becomes a credential in
/// one transaction as the migrator, idempotent per address. Password users keep Firebase's scrypt
/// hash (checked with `FI_FIREBASE_SCRYPT_*` at sign-in, then rehashed to Argon2id); the others
/// are "password not set" and use the set-password mail (`POST /launcher/v1/auth/password-reset`).
/// The FI account mapped to a user's Firebase uid is re-pointed at its credential. Prints counts
/// only, never an address or a hash.
pub async fn import_firebase(
    env: &dyn Env,
    path: &str,
) -> Result<ImportReport, Box<dyn std::error::Error>> {
    let backend = env.get("FI_AUTH_BACKEND").unwrap_or_default();
    if backend.trim() != "local" {
        return Err("accounts import-firebase works only with FI_AUTH_BACKEND=local".into());
    }
    let text = std::fs::read_to_string(path)?;
    let (users, skipped) = parse_firebase_export(&text)?;
    let db = DbConfig::from_env_for(
        "DATABASE_MIGRATOR_URL",
        "fi-platform-accounts",
        Workload::Job,
    )?;
    let pool = fi_db::connect_lazy(&db)?;
    let mut report = ImportReport {
        skipped,
        ..ImportReport::default()
    };
    let mut tx = pool.begin().await?;
    for user in &users {
        match credential::import_in(&mut tx, user).await? {
            credential::Imported::Exists => report.existing += 1,
            credential::Imported::Reclaimed { .. } => report.reclaimed += 1,
            credential::Imported::Conflict => report.conflicts.push(user.firebase_uid.clone()),
            credential::Imported::New { remapped, .. } => {
                if user.password_hash.is_some() {
                    report.with_password += 1;
                } else {
                    report.without_password += 1;
                }
                if remapped {
                    report.remapped += 1;
                }
            }
        }
    }
    tx.commit().await?;
    pool.close().await;
    tracing::info!(
        with_password = report.with_password,
        without_password = report.without_password,
        remapped = report.remapped,
        existing = report.existing,
        reclaimed = report.reclaimed,
        conflicts = report.conflicts.len(),
        skipped = report.skipped,
        "firebase users imported"
    );
    Ok(report)
}

/// `healthcheck`: exit 0 when `GET /health/ready` on `127.0.0.1:$PORT` answers 200 (the compose
/// healthcheck of a distroless image without a shell or curl).
pub fn healthcheck() -> Outcome {
    let port = fi_runtime::http::port()?;
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        b"GET /health/ready HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: fi-platform-healthcheck\r\nConnection: close\r\n\r\n",
    )?;
    let mut head = [0u8; 12];
    stream.read_exact(&mut head)?;
    if head.starts_with(b"HTTP/1.1 200") {
        Ok(())
    } else {
        Err(format!(
            "/health/ready answered {}",
            String::from_utf8_lossy(&head[9..]).trim()
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reset_password_refuses_a_firebase_instance_before_any_work() {
        for backend in [None, Some("firebase"), Some("Local")] {
            let mut env: Vec<(&'static str, String)> = Vec::new();
            if let Some(b) = backend {
                env.push(("FI_AUTH_BACKEND", b.to_owned()));
            }
            let e = reset_password(&env, "ana@example.test").await.unwrap_err();
            assert!(e.to_string().contains("FI_AUTH_BACKEND=local"), "{e}");
        }
        let local = vec![("FI_AUTH_BACKEND", "local".to_owned())];
        let e = reset_password(&local, "not-an-address").await.unwrap_err();
        assert!(e.to_string().contains("e-mail"), "{e}");
        let e = import_firebase(&Vec::new(), "/nonexistent")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("FI_AUTH_BACKEND=local"), "{e}");
    }

    /// The shape of `firebase auth:export` (the sample user of Firebase's own scrypt README, plus
    /// synthetic ones): a password user keeps its hash in the stored form, a Google-only user has
    /// none, entries without a usable address are skipped.
    #[test]
    fn firebase_exports_parse() {
        let text = r#"{"users": [
          {"localId": "uid-password", "email": "User1@Test.com", "emailVerified": true,
           "passwordHash": "lSrfV15cpx95/sZS2W9c9Kp6i/LVgQNDNC/qzrCnh1SAyZvqmZqAjTdn3aoItz+VHjoZilo78198JAdRuid5lQ==",
           "salt": "42xEC+ixf3L2lw==", "createdAt": "1508893925000"},
          {"localId": "uid-google", "email": "google-only@example.test", "emailVerified": true,
           "providerUserInfo": [{"providerId": "google.com", "rawId": "1"}]},
          {"localId": "uid-phone", "phoneNumber": "+10000000000"},
          {"localId": "uid-bad", "email": "not-an-address"}
        ]}"#;
        let (users, skipped) = parse_firebase_export(text).unwrap();
        assert_eq!(skipped, 2);
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].email_lower, "user1@test.com");
        assert!(users[0].email_verified);
        assert!(
            users[0]
                .password_hash
                .as_deref()
                .is_some_and(fi_auth::firebase_scrypt::is_firebase)
        );
        assert_eq!(users[1].password_hash, None);
        assert!(parse_firebase_export("{}").is_err());
        assert!(!format!("{:?}", users[0]).contains("test.com"));
    }
}
