//! A `local` instance's own e-mail and password sign-in (ACCOUNTS.md §4.1 "Two identity
//! backends", §5; ADR 0023 §9; OD-122 M2.3): `POST /launcher/v1/auth/sign-up`, `/auth/sign-in`
//! and `/auth/password`, mounted only with `FI_AUTH_BACKEND=local` (404 on a `firebase` instance).
//! Their ID tokens then go to `POST /auth/token` like Identity Platform's.
//!
//! - Passwords are hashed and checked with Argon2id ([`fi_auth::password`]) on the blocking pool,
//!   never while this request holds a pooled connection (services/AGENTS.md).
//! - Limits (ACCOUNTS.md §9): the exchange's per-client-network limit (`signin.net`) and one per
//!   e-mail address (`local.email`), both counted before any password work and whether or not the
//!   address has a credential. The per-identity limit (`signin.uid`) counts at the exchange that
//!   follows.
//! - Sign-in answers one `AUTH.INVALID_CREDENTIALS` for an unknown address and a wrong password,
//!   after the same statements and the same Argon2 work.
//! - Nothing here logs an address, a password, a hash or a token. The sign-up's idempotency record
//!   holds a pepper-keyed MAC of the request, never a plain digest of a password.

use super::local_mail::{confirmation_in, send_confirmation};
use crate::errors::ApiError;
use crate::extract::{Body, ClientNetwork, IdempotencyKey};
use crate::signin;
use crate::state::{AppState, now_secs};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use fi_auth::firebase_scrypt;
use fi_auth::idtoken;
use fi_auth::localidp::LocalIdp;
use fi_auth::password;
use fi_db::credential::{self, Created};
use fi_db::identity::IdentityClaim;
use fi_db::limits::{self, Idempotency, Recorded};
use fi_db::tokens;
use fi_protocol::ErrorCode;
use fi_protocol::accounts::{LocalCredentials, LocalIdToken, LocalPasswordChange};
use sqlx::PgConnection;
use uuid::Uuid;

const SIGN_UP: &str = "POST /launcher/v1/auth/sign-up";

/// The normalized address: trimmed, lower-cased, 3-254 characters with exactly one `@` and no
/// control character (ACCOUNTS.md §4.1); `None` otherwise.
pub fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    let ok = (3..=254).contains(&email.chars().count())
        && email.matches('@').count() == 1
        && !email.chars().any(char::is_control)
        && email.trim() == email;
    ok.then_some(email)
}

pub(super) fn provider(state: &AppState) -> Result<&LocalIdp, ApiError> {
    // Mounted only with the local provider; a mis-mount answers like an unknown route would.
    state
        .id()
        .local_idp
        .as_ref()
        .ok_or_else(|| ApiError::new(ErrorCode::ServiceUnavailable, "service unavailable"))
}

/// Argon2id jobs running at once in this process: each holds 19 MiB, and the per-network
/// Postgres limits bound their rate, not how many arrive together from many networks.
const PASSWORD_WORK_AT_ONCE: usize = 4;

/// The process's password-work slots.
static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(PASSWORD_WORK_AT_ONCE);

/// Runs CPU-heavy password work on the blocking pool, at most [`PASSWORD_WORK_AT_ONCE`] at a time;
/// a caller that waits more than 10 s gets 503 (`SERVICE.UNAVAILABLE`, retry later). The slot moves
/// into the job: a blocking job outlives a cancelled request (a client that disconnects), and the
/// slot is released only when the work is done (security review H1, 2026-10-25).
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    let slot = tokio::time::timeout(std::time::Duration::from_secs(10), SLOTS.acquire())
        .await
        .map_err(|_| ApiError::unavailable())?
        .map_err(|_| ApiError::unavailable())?;
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        work()
    })
    .await
    .map_err(|_| ApiError::unavailable())
}

pub(super) async fn hash(password: String) -> Result<String, ApiError> {
    blocking(move || password::hash(&password))
        .await?
        .map_err(|_| ApiError::unavailable())
}

/// The per-network and per-address limits, each in its own statement (attempts count even when
/// the request then fails).
async fn count(state: &AppState, net: &ClientNetwork, email: Option<&str>) -> Result<(), ApiError> {
    signin::limit(
        state,
        &limits::SIGNIN_NET,
        &net.0,
        ErrorCode::AuthRateLimited,
    )
    .await?;
    match email {
        Some(email) => {
            signin::limit(
                state,
                &limits::LOCAL_EMAIL,
                &format!("email:{email}"),
                ErrorCode::AuthRateLimited,
            )
            .await
        }
        None => Ok(()),
    }
}

fn email_taken() -> ApiError {
    ApiError::new(
        ErrorCode::AccountsEmailTaken,
        "this e-mail already has a sign-in here: sign in instead",
    )
}

fn id_token_invalid() -> ApiError {
    ApiError::new(ErrorCode::AuthIdTokenInvalid, "ID token refused")
}

/// On a `local` instance, refuses an ID token minted before its credential's last password change
/// (or whose credential is gone), so a password change also ends the 5 minutes of the tokens
/// signed before it. Firebase instances: nothing to check. Runs on the caller's connection.
pub async fn require_current_in(
    conn: &mut PgConnection,
    state: &AppState,
    claim: &IdentityClaim,
) -> Result<(), ApiError> {
    if !state.id().is_local() {
        return Ok(());
    }
    let id = Uuid::parse_str(&claim.idp_uid).map_err(|_| id_token_invalid())?;
    let credential = credential::find_by_id_in(conn, id)
        .await?
        .ok_or_else(id_token_invalid)?;
    // Milliseconds (security review M2): a token whose `auth_time` second started before the
    // change dies with it; sign-in waits into the next second after a change (`after_change`).
    if credential.updated_at != credential.created_at
        && credential.updated_at.millis() > claim.auth_time.millis()
    {
        return Err(id_token_invalid());
    }
    Ok(())
}

/// Whether sign-ups start confirmed: an instance that delivers no mail (`FI_MAIL_BACKEND=log`)
/// requires no confirmation nobody could receive.
fn confirms(state: &AppState) -> bool {
    state
        .id()
        .mail
        .as_ref()
        .is_some_and(|m| m.requires_confirmation())
}

/// Whether the ID tokens of `c` say its address is confirmed.
fn verified(state: &AppState, c: &credential::Credential) -> bool {
    c.email_verified_at.is_some() || !confirms(state)
}

fn issued_for(idp: &LocalIdp, id: Uuid, email_lower: &str, email_verified: bool) -> LocalIdToken {
    let now = now_secs();
    let t = idp.issue_with(&id.to_string(), email_lower, email_verified, now, now);
    LocalIdToken {
        id_token: t.token.into_inner(),
        expires_in: t.expires_in,
    }
}

/// *public* `POST /launcher/v1/auth/sign-up` (`local` only) `{email, password}` → 201
/// `{idToken, expiresIn}`. Creates the credential only; a replay of the same key and body answers
/// a fresh ID token for the same credential. When the instance sends mail the credential starts
/// unconfirmed: its ID token says `email_verified: false` (the exchange answers
/// `AUTH.EMAIL_UNVERIFIED`) and the confirmation link is mailed after the commit (CR-S53).
pub async fn sign_up(
    State(state): State<AppState>,
    key: IdempotencyKey,
    net: ClientNetwork,
    headers: HeaderMap,
    Body(req): Body<LocalCredentials>,
) -> Result<(StatusCode, Json<LocalIdToken>), ApiError> {
    let idp = provider(&state)?;
    let email = normalize_email(&req.email)
        .ok_or_else(|| ApiError::invalid("email", "3 to 254 characters with one @"))?;
    if !password::acceptable(&req.password) {
        return Err(ApiError::invalid("password", "8 to 128 characters"));
    }
    count(&state, &net, Some(&email)).await?;
    // Closed registration costs no password work.
    {
        let mut conn = state.pool.acquire().await?;
        signin::require_registration(&state, &mut conn).await?;
    }
    let pepper = state.id().pepper()?;
    let scope = pepper.signup_scope(&email);
    let idem = Idempotency {
        key: key.0,
        route: SIGN_UP.to_owned(),
        request_sha256: pepper.signup_request(SIGN_UP, &email, &req.password),
    };
    let phc = hash(req.password).await?;
    let confirms = confirms(&state);
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    signin::require_registration(&state, &mut tx).await?;
    let mut link = None;
    let found = match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(record) => {
            let first = record.resource_id.ok_or_else(ApiError::unavailable)?;
            match credential::find_by_id_in(&mut tx, first).await? {
                // A password changed since is not this request's any more (a taken-over
                // credential stays this request's while it is unconfirmed).
                Some(c)
                    if c.updated_at == c.created_at
                        || (confirms && c.email_verified_at.is_none()) =>
                {
                    c
                }
                Some(_) => return Err(email_taken()),
                None => {
                    return Err(ApiError::new(
                        ErrorCode::RequestIdempotencyKeyReused,
                        "this Idempotency-Key was used for another request",
                    ));
                }
            }
        }
        Recorded::Different => {
            return Err(ApiError::new(
                ErrorCode::RequestIdempotencyKeyReused,
                "this Idempotency-Key was used for another request",
            ));
        }
        // Never stored confirmed: with `FI_MAIL_BACKEND=log` the token says confirmed
        // (`verified`), the row does not, so the address must confirm once mail is on (H1).
        Recorded::None => match credential::create_in(&mut tx, &email, &phc, false).await? {
            Created::New(c) => {
                limits::record_in(&mut tx, &scope, &idem, 201, Some(c.id)).await?;
                if confirms {
                    link = Some(confirmation_in(&mut tx, &state, c.id).await?);
                }
                c
            }
            // An unconfirmed credential without an FI account proved nothing: a new sign-up of
            // the address takes it over when the instance mails (security review M1).
            Created::EmailTaken if confirms => {
                let held = credential::find_by_email_in(&mut tx, &email)
                    .await?
                    .ok_or_else(email_taken)?;
                if !credential::retake_unconfirmed_in(&mut tx, held.id, &phc).await? {
                    return Err(email_taken());
                }
                limits::record_in(&mut tx, &scope, &idem, 201, Some(held.id)).await?;
                link = Some(confirmation_in(&mut tx, &state, held.id).await?);
                tracing::info!(credential = %held.id, "unconfirmed credential taken over by a new sign-up");
                held
            }
            Created::EmailTaken => return Err(email_taken()),
        },
    };
    tx.commit().await?;
    if let Some(confirmation) = link {
        send_confirmation(&state, &email, &confirmation, &headers);
    }
    let ok = verified(&state, &found);
    Ok((
        StatusCode::CREATED,
        Json(issued_for(idp, found.id, &email, ok)),
    ))
}

/// Waits into the second after `c`'s last password change, so the ID token minted next has an
/// `auth_time` (whole seconds) later than the change and outlives [`require_current_in`]'s
/// millisecond check (at most a second, only right after a change).
async fn after_change(c: &credential::Credential) {
    if c.updated_at == c.created_at {
        return;
    }
    let next = (c.updated_at.millis().div_euclid(1_000) + 1) * 1_000;
    let now = fi_domain::UnixMs::now_system().millis();
    if next > now {
        tokio::time::sleep(std::time::Duration::from_millis(
            (next - now).min(1_100) as u64
        ))
        .await;
    }
}

/// What a sign-in's password check found.
pub(super) enum Checked {
    /// The password matched an Argon2id hash.
    Matched,
    /// It matched an imported Firebase hash: rehash it (`previous`).
    MatchedImported(String),
    /// No match, an unknown address, or no password set.
    Refused,
}

/// Checks `typed` against a stored hash (Argon2id, an imported Firebase hash, or none: the same
/// work, never a match) on the password-work slots.
pub(super) async fn check(
    state: &AppState,
    stored: Option<String>,
    typed: String,
) -> Result<Checked, ApiError> {
    let imported = state.id().firebase_hash.clone();
    blocking(move || match stored {
        Some(phc) if firebase_scrypt::is_firebase(&phc) => match imported {
            Some(config) if config.verify(&typed, &phc) => Checked::MatchedImported(phc),
            Some(_) => Checked::Refused,
            // Not configured: the same work, never a match (the set-password mail still works).
            None => {
                password::verify_unknown(&typed);
                Checked::Refused
            }
        },
        Some(phc) if password::verify(&typed, &phc) => Checked::Matched,
        Some(_) => Checked::Refused,
        None => {
            password::verify_unknown(&typed);
            Checked::Refused
        }
    })
    .await
}

/// *public* `POST /launcher/v1/auth/sign-in` (`local` only) `{email, password}` → 200
/// `{idToken, expiresIn}`; one `AUTH.INVALID_CREDENTIALS` for an unknown address, a wrong password
/// and an account whose password is not set. An unconfirmed address signs in with
/// `email_verified: false` (refused at the exchange, as Identity Platform's tokens). An imported
/// Firebase hash that matches is replaced by its Argon2id rehash before the token is minted.
pub async fn sign_in(
    State(state): State<AppState>,
    net: ClientNetwork,
    Body(req): Body<LocalCredentials>,
) -> Result<Json<LocalIdToken>, ApiError> {
    let idp = provider(&state)?;
    let wrong = || {
        ApiError::new(
            ErrorCode::AuthInvalidCredentials,
            "the e-mail or the password is incorrect",
        )
    };
    let email = normalize_email(&req.email);
    count(&state, &net, email.as_deref()).await?;
    // Any stored password may be tried (imported Firebase users had 6 characters); the 8-128
    // policy applies when a password is set. The upper bound caps the hashing work.
    let tried = !req.password.is_empty() && req.password.chars().count() <= password::MAX_CHARS;
    let Some(email) = email.filter(|_| tried) else {
        return Err(wrong());
    };
    let found = {
        let mut conn = state.pool.acquire().await?;
        credential::find_by_email_in(&mut conn, &email).await?
    };
    let presented = req.password;
    let stored = found.as_ref().and_then(|c| c.password_hash.clone());
    let checked = check(&state, stored, presented.clone()).await?;
    let Some(c) = found else {
        return Err(wrong());
    };
    match checked {
        Checked::Refused => return Err(wrong()),
        Checked::Matched => {}
        Checked::MatchedImported(previous) => {
            let phc = hash(presented).await?;
            let mut conn = state.pool.acquire().await?;
            if !credential::rehash_in(&mut conn, c.id, &previous, &phc).await? {
                // The hash changed while it was checked (a reset): this password is not it.
                return Err(wrong());
            }
            tracing::info!(credential = %c.id, "imported password rehashed to Argon2id");
        }
    }
    after_change(&c).await;
    // The password checked must still be the credential's (security review M2): a reset or
    // change that committed during the check refuses this sign-in. Minted right after the read.
    let now = {
        let mut conn = state.pool.acquire().await?;
        credential::find_by_id_in(&mut conn, c.id).await?
    };
    match now {
        Some(n) if n.updated_at == c.updated_at => {
            Ok(Json(issued_for(idp, n.id, &email, verified(&state, &n))))
        }
        _ => Err(wrong()),
    }
}

/// *public* `POST /launcher/v1/auth/password` (`local` only) `{idToken, newPassword}` → 204: a
/// fresh ID token of a recent sign-in sets the new password, and every refresh family and portal
/// session of the credential's account is revoked (`password_changed`). Setting the same password
/// twice ends in the same state, so no Idempotency-Key is needed.
pub async fn change_password(
    State(state): State<AppState>,
    net: ClientNetwork,
    Body(req): Body<LocalPasswordChange>,
) -> Result<StatusCode, ApiError> {
    provider(&state)?;
    count(&state, &net, None).await?;
    let claim = signin::verify_id_token(&state, &req.id_token).await?;
    if !idtoken::is_recent(claim.auth_time.millis().div_euclid(1_000), now_secs()) {
        return Err(ApiError::new(
            ErrorCode::AuthReauthRequired,
            "sign in again (the last sign-in is older than 5 minutes)",
        ));
    }
    if !password::acceptable(&req.new_password) {
        return Err(ApiError::invalid("newPassword", "8 to 128 characters"));
    }
    let email = claim.email_lower.clone().ok_or_else(id_token_invalid)?;
    signin::limit(
        &state,
        &limits::LOCAL_EMAIL,
        &format!("email:{email}"),
        ErrorCode::AuthRateLimited,
    )
    .await?;
    let id = Uuid::parse_str(&claim.idp_uid).map_err(|_| id_token_invalid())?;
    let phc = hash(req.new_password).await?;
    let mut tx = state.pool.begin().await?;
    require_current_in(&mut tx, &state, &claim).await?;
    if !credential::set_password_hash_in(&mut tx, id, &phc).await? {
        return Err(id_token_invalid());
    }
    // Older mail links stop working too (security review L1).
    credential::spend_all_mail_tokens_in(&mut tx, id).await?;
    if let Some(account_id) = credential::account_of_in(&mut tx, &claim.idp_uid).await? {
        tokens::revoke_account_in(&mut tx, account_id, "password_changed").await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e_mails_are_trimmed_lower_cased_and_bounded() {
        assert_eq!(
            normalize_email("  Ana@Example.TEST ").as_deref(),
            Some("ana@example.test")
        );
        assert_eq!(normalize_email("a@b").as_deref(), Some("a@b"));
        for bad in [
            "",
            "ab",
            "no-at.example",
            "two@@example.test",
            "a@b@c",
            "line\nbreak@example.test",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?}");
        }
        let long = format!("{}@example.test", "x".repeat(241));
        assert_eq!(long.len(), 254);
        assert!(normalize_email(&long).is_some());
        assert!(normalize_email(&format!("x{long}")).is_none());
    }
}

#[cfg(test)]
mod slot_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Requests cancelled while their password work runs never let more than
    /// [`PASSWORD_WORK_AT_ONCE`] jobs run together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_requests_keep_their_slot_until_the_work_ends() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut calls = Vec::new();
        for _ in 0..32 {
            let (running, peak) = (running.clone(), peak.clone());
            calls.push(tokio::spawn(async move {
                let work = blocking(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    running.fetch_sub(1, Ordering::SeqCst);
                });
                // The client goes away 5 ms in; its job keeps running on the blocking pool.
                let _ = tokio::time::timeout(std::time::Duration::from_millis(5), work).await;
            }));
            // A new request every 3 ms: each finds the slots its cancelled predecessors left.
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        for c in calls {
            c.await.unwrap();
        }
        // Let every job that started finish.
        while running.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            peak.load(Ordering::SeqCst) <= PASSWORD_WORK_AT_ONCE,
            "{peak:?}"
        );
    }
}
