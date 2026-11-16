//! A `local` instance's e-mail confirmation and password reset (ACCOUNTS.md §4.1 "E-mail
//! confirmation and password reset", §5; register CR-S53; OD-122 follow-ups 2026-10-26):
//!
//! - `POST /launcher/v1/auth/confirmation {idToken}` → 202: the confirmation link again, for the
//!   ID token of a sign-in whose exchange answered `AUTH.EMAIL_UNVERIFIED` (the same token the
//!   launcher already holds, as Identity Platform's `sendOobCode VERIFY_EMAIL`). Limited per
//!   credential (3 per hour) and per client network (10 per hour).
//! - `POST /launcher/v1/auth/password-reset {email}` → 202 whatever the address (no enumeration:
//!   same status, same body; the mail leaves from a task after the answer, so its latency does not
//!   tell either). Limited per address (3 per hour, keyed by the pepper's HMAC) and per network.
//!   A new password link spends the older ones. The per-network limit counts each action (resend,
//!   reset, each page's `POST`) on its own, so one user cannot block another action for the network.
//! - `GET`/`POST /launcher/v1/auth/email/confirm` and `/auth/email/password`: the links' pages,
//!   plain server-rendered HTML in English and Portuguese with no script or external asset. The
//!   `GET` only renders a form (mail scanners that prefetch links spend nothing); the `POST` spends
//!   the single-use token. Confirming asks for the account's current password (a wrong one keeps
//!   the token) and sets `email_verified_at`; the password page sets the new
//!   password (Argon2id, 8-128 characters), confirms the address, spends the credential's other
//!   links and revokes every refresh family of its account (`password_reset`): signed out
//!   everywhere, and every ID token minted before dies (`updated_at` moves).
//!
//! Tokens: `fi_mt_` + 43 base64url characters, only the SHA-256 stored
//! (`credential_mail_token`), confirmation 24 hours, password 1 hour. Nothing here logs an
//! address, a link or a token: the mailer's failures log the kind and a short address hash.

use super::local::{Checked, check, hash, normalize_email, provider};
use crate::errors::ApiError;
use crate::extract::{Body, ClientNetwork};
use crate::signin;
use crate::state::{AppState, now_secs};
use axum::Json;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fi_auth::opaque::{self, Kind};
use fi_auth::password;
use fi_db::credential::{self, MailPurpose};
use fi_db::limits;
use fi_db::tokens;
use fi_mail::{Lang, Message};
use fi_protocol::ErrorCode;
use fi_protocol::accounts::{
    LocalConfirmationCode, LocalConfirmationRequest, LocalIdToken, LocalPasswordResetRequest,
};
use fi_protocol::routes::launcher as r;
use serde::Deserialize;
use sqlx::PgConnection;
use uuid::Uuid;

/// Mints a mail token of `purpose` for credential `id` and stores its SHA-256 in the caller's
/// transaction; the token goes only into the mail.
pub(super) async fn mail_token_in(
    conn: &mut PgConnection,
    id: Uuid,
    purpose: MailPurpose,
) -> Result<fi_auth::Secret, ApiError> {
    let minted = opaque::mint(Kind::MailToken).map_err(|_| ApiError::unavailable())?;
    credential::insert_mail_token_in(conn, id, purpose, &minted.lookup).await?;
    Ok(minted.token)
}

/// A confirmation mail's secrets: the link's token and the 6-digit code (one row).
pub(super) struct Confirmation {
    token: fi_auth::Secret,
    code: fi_auth::Secret,
}

/// Replaces the credential's open confirmation (its older links and code are spent: a new mail
/// replaces the open code) with a new token and code, stored as SHA-256 and pepper MAC in the
/// caller's transaction.
pub(super) async fn confirmation_in(
    conn: &mut PgConnection,
    state: &AppState,
    id: Uuid,
) -> Result<Confirmation, ApiError> {
    // One open confirmation per credential: a concurrent resend and takeover serialize here.
    credential::lock_in(conn, id).await?;
    let minted = opaque::mint(Kind::MailToken).map_err(|_| ApiError::unavailable())?;
    let code = fi_auth::codes::confirmation_code().map_err(|_| ApiError::unavailable())?;
    let mac = state
        .id()
        .pepper()?
        .confirmation_code(&minted.lookup, code.expose());
    credential::spend_mail_tokens_of_in(conn, id, MailPurpose::Confirm).await?;
    credential::insert_confirm_token_in(conn, id, &minted.lookup, &mac).await?;
    Ok(Confirmation {
        token: minted.token,
        code,
    })
}

fn lang(headers: &HeaderMap) -> Lang {
    Lang::from_accept_language(
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
    )
}

fn link(state: &AppState, path: &str, token: &fi_auth::Secret) -> Option<String> {
    let mail = state.id().mail.as_ref()?;
    Some(format!("{}{path}?token={}", mail.link_base, token.expose()))
}

/// Sends `message` from a task, after the caller's answer: no request waits on SMTP, and a failure
/// is one warning with the kind and the address hash (the confirmation can be resent; a reset asked
/// again).
fn dispatch(state: &AppState, message: Message) {
    let Some(mail) = state.id().mail.clone() else {
        return;
    };
    tokio::spawn(async move {
        let kind = message.kind.as_str();
        let to = fi_mail::address_hash(&message.to);
        if let Err(e) = mail.mailer.send(message).await {
            tracing::warn!(kind, to = %to, error = %e, "mail not sent");
        }
    });
}

/// Mails the confirmation code and link of `c` to `email`.
pub(super) fn send_confirmation(
    state: &AppState,
    email: &str,
    c: &Confirmation,
    headers: &HeaderMap,
) {
    if let Some(url) = link(state, r::AUTH_EMAIL_CONFIRM, &c.token) {
        dispatch(
            state,
            fi_mail::confirm_mail(email, c.code.expose(), &url, lang(headers)),
        );
    }
}

async fn limit(state: &AppState, which: &limits::Limit, subject: &str) -> Result<(), ApiError> {
    signin::limit(state, which, subject, ErrorCode::AuthRateLimited).await
}

/// *public* `POST /launcher/v1/auth/confirmation` (`local` only) `{idToken}` → 202: mails the
/// confirmation link again to the token's address while it is unconfirmed (nothing when it is
/// confirmed already, or when the instance sends no mail).
pub async fn resend_confirmation(
    State(state): State<AppState>,
    net: ClientNetwork,
    headers: HeaderMap,
    Body(req): Body<LocalConfirmationRequest>,
) -> Result<StatusCode, ApiError> {
    let idp = provider(&state)?;
    limit(
        &state,
        &limits::LOCAL_MAIL_NET,
        &format!("resend:{}", net.0),
    )
    .await?;
    let identity = idp
        .verify_unconfirmed(&req.id_token, now_secs())
        .map_err(|e| match e {
            fi_auth::idtoken::IdTokenError::ProviderNotAllowed => ApiError::new(
                ErrorCode::AuthProviderNotAllowed,
                "sign-in provider not allowed",
            ),
            _ => ApiError::new(ErrorCode::AuthIdTokenInvalid, "ID token refused"),
        })?;
    let id = Uuid::parse_str(&identity.uid)
        .map_err(|_| ApiError::new(ErrorCode::AuthIdTokenInvalid, "ID token refused"))?;
    limit(
        &state,
        &limits::LOCAL_MAIL_CREDENTIAL,
        &format!("credential:{id}"),
    )
    .await?;
    let requires = state
        .id()
        .mail
        .as_ref()
        .is_some_and(|m| m.requires_confirmation());
    if !requires {
        return Ok(StatusCode::ACCEPTED);
    }
    let mut tx = state.pool.begin().await?;
    let Some(found) = credential::find_by_id_in(&mut tx, id).await? else {
        return Err(ApiError::new(
            ErrorCode::AuthIdTokenInvalid,
            "ID token refused",
        ));
    };
    if found.email_verified_at.is_some() {
        return Ok(StatusCode::ACCEPTED);
    }
    let confirmation = confirmation_in(&mut tx, &state, id).await?;
    tx.commit().await?;
    send_confirmation(&state, &found.email_lower, &confirmation, &headers);
    Ok(StatusCode::ACCEPTED)
}

/// *public* `POST /launcher/v1/auth/confirmation/code` (`local` only) `{idToken, code}` → 200
/// `{idToken, expiresIn}` (CR-S53 follow-up "6-digit code"): the ID token of a sign-in (an
/// unconfirmed one is accepted) that is current for its credential proves the password; the code
/// must match the credential's open confirmation (pepper MAC, constant-time). Success spends it
/// and the credential's other confirmation links, confirms the address and answers a confirmed ID
/// token with the same `auth_time`, for `/auth/token` straight away. A wrong code is 400
/// `AUTH.CONFIRMATION_CODE_INVALID` and counts: the fifth spends the code. Limited per credential
/// and per network.
pub async fn confirm_code(
    State(state): State<AppState>,
    net: ClientNetwork,
    Body(req): Body<LocalConfirmationCode>,
) -> Result<Json<LocalIdToken>, ApiError> {
    let idp = provider(&state)?;
    limit(&state, &limits::LOCAL_CODE_NET, &net.0).await?;
    let refused = || ApiError::new(ErrorCode::AuthIdTokenInvalid, "ID token refused");
    let identity = idp
        .verify_unconfirmed(&req.id_token, now_secs())
        .map_err(|e| match e {
            fi_auth::idtoken::IdTokenError::ProviderNotAllowed => ApiError::new(
                ErrorCode::AuthProviderNotAllowed,
                "sign-in provider not allowed",
            ),
            _ => refused(),
        })?;
    let id = Uuid::parse_str(&identity.uid).map_err(|_| refused())?;
    let code = fi_auth::codes::normalize_confirmation_code(&req.code)
        .ok_or_else(|| ApiError::invalid("code", "6 digits"))?;
    limit(
        &state,
        &limits::LOCAL_CODE_CREDENTIAL,
        &format!("credential:{id}"),
    )
    .await?;
    let claim = fi_db::identity::IdentityClaim {
        email_lower: identity.email_lower(),
        idp_uid: identity.uid.clone(),
        provider_id: identity.provider_id.clone(),
        email_verified: identity.email_verified,
        auth_time: fi_domain::UnixMs::from_millis(identity.auth_time.saturating_mul(1_000)),
    };
    let pepper = state.id().pepper()?;
    let mut tx = state.pool.begin().await?;
    // The token must be current: a password change or takeover since its sign-in refuses it.
    super::local::require_current_in(&mut tx, &state, &claim).await?;
    if !credential::lock_in(&mut tx, id).await? {
        return Err(refused());
    }
    let found = credential::find_by_id_in(&mut tx, id)
        .await?
        .ok_or_else(refused)?;
    let invalid = || {
        ApiError::new(
            ErrorCode::AuthConfirmationCodeInvalid,
            "the code is wrong, used or expired: check the newest e-mail or send a new one",
        )
    };
    // Already confirmed: no code to check, and no token minted for any 6 digits (L-C1).
    if found.email_verified_at.is_some() {
        return Err(ApiError::invalid(
            "code",
            "this address is already confirmed: sign in again",
        ));
    }
    // Totals across codes (M-C1): 30 wrong codes over the credential's life, 10 per 24 hours.
    let limited = |secs: i64, why: &'static str| {
        ApiError::new(ErrorCode::AuthRateLimited, why).retry_after(secs.max(1) as u64)
    };
    if found.code_failures >= credential::CODE_FAILURES_LIFETIME {
        return Err(limited(
            86_400,
            "too many wrong codes: confirm with the e-mail's link and your password, or reset the password",
        ));
    }
    let wrong_key =
        pepper.rate_limit_key(limits::LOCAL_CODE_WRONG.name, &format!("credential:{id}"));
    if let limits::Hit::Limited { retry_after_secs } =
        limits::peek_in(&mut tx, &wrong_key, &limits::LOCAL_CODE_WRONG).await?
    {
        return Err(limited(
            retry_after_secs,
            "too many wrong codes today: use the e-mail's link with your password, or wait",
        ));
    }
    let Some((row, stored)) = credential::open_code_in(&mut tx, id).await? else {
        return Err(invalid());
    };
    if !fi_auth::ct_eq(&pepper.confirmation_code(&row, &code), &stored) {
        credential::wrong_code_in(&mut tx, &row).await?;
        credential::code_failure_in(&mut tx, id).await?;
        limits::hit_in(&mut tx, &wrong_key, &limits::LOCAL_CODE_WRONG).await?;
        tx.commit().await?;
        return Err(invalid());
    }
    credential::spend_mail_tokens_of_in(&mut tx, id, MailPurpose::Confirm).await?;
    credential::confirm_email_in(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(credential = %id, "e-mail confirmed by code");
    let now = now_secs();
    let t = idp.issue_with(
        &identity.uid,
        &found.email_lower,
        true,
        identity.auth_time,
        now,
    );
    Ok(Json(LocalIdToken {
        id_token: t.token.into_inner(),
        expires_in: t.expires_in,
    }))
}

/// *public* `POST /launcher/v1/auth/password-reset` (`local` only) `{email}` → 202 whatever the
/// address: a credential gets a password link (also the "set your password" mail of an account
/// imported without one). A malformed address is 400 `REQUEST.INVALID` (`email`), which says
/// nothing about any account.
pub async fn request_password_reset(
    State(state): State<AppState>,
    net: ClientNetwork,
    headers: HeaderMap,
    Body(req): Body<LocalPasswordResetRequest>,
) -> Result<StatusCode, ApiError> {
    provider(&state)?;
    let email = normalize_email(&req.email)
        .ok_or_else(|| ApiError::invalid("email", "3 to 254 characters with one @"))?;
    limit(&state, &limits::LOCAL_MAIL_NET, &format!("reset:{}", net.0)).await?;
    limit(
        &state,
        &limits::LOCAL_MAIL_ADDRESS,
        &format!("email:{email}"),
    )
    .await?;
    let mut tx = state.pool.begin().await?;
    let token = match credential::find_by_email_in(&mut tx, &email).await? {
        Some(c) => {
            // One live password link at a time (security review L1).
            credential::spend_mail_tokens_of_in(&mut tx, c.id, MailPurpose::Reset).await?;
            Some(mail_token_in(&mut tx, c.id, MailPurpose::Reset).await?)
        }
        None => None,
    };
    tx.commit().await?;
    if let Some(token) = token
        && let Some(url) = link(&state, r::AUTH_EMAIL_PASSWORD, &token)
    {
        dispatch(&state, fi_mail::reset_mail(&email, &url, lang(&headers)));
    }
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------------------------------------------
// The links' pages.
// ---------------------------------------------------------------------------------------------

/// `?token=` of a link. Never `Debug` (it holds the token).
#[derive(Default, Deserialize)]
pub struct LinkQuery {
    #[serde(default)]
    token: String,
}

/// The confirmation page's form: the token and the account's current password (security review
/// M1: a link alone never confirms a password someone else set). Never `Debug`.
#[derive(Default, Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    password: String,
}

/// The password page's form. Never `Debug`.
#[derive(Default, Deserialize)]
pub struct PasswordForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    repeat: String,
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:32rem;margin:2rem auto;padding:0 1rem;\
color:#1b1f24;background:#f6f7f9}main{background:#fff;border-radius:8px;padding:1.5rem;\
box-shadow:0 1px 3px #0002}h1{font-size:1.25rem}p.pt{color:#444}label{display:block;margin:.75rem 0 .25rem}\
input[type=password]{width:100%;padding:.5rem;box-sizing:border-box}button{margin-top:1rem;padding:.6rem 1.2rem;\
font-size:1rem}.error{color:#a40e26}@media (prefers-color-scheme:dark){body{background:#0d1117;color:#e6edf3}\
main{background:#161b22}p.pt{color:#aab}}";

/// One page: `title` in both languages, then `body` (already escaped HTML).
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"pt-BR\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{title}</title><style>{STYLE}</style></head>\
         <body><main><h1>{title}</h1>{body}</main></body></html>"
    );
    let mut response = (status, html).into_response();
    let h = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
        ),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    response
}

fn both(en: &str, pt: &str) -> String {
    format!("<p>{en}</p><p class=\"pt\">{pt}</p>")
}

fn invalid_link() -> Response {
    page(
        StatusCode::BAD_REQUEST,
        "Infinity Horizon",
        &both(
            "This link is invalid, was already used or has expired. Sign in to the launcher to get a new one.",
            "Este link é inválido, já foi usado ou expirou. Entre no launcher para receber um novo.",
        ),
    )
}

fn too_many(e: &ApiError) -> Option<Response> {
    (e.code == ErrorCode::AuthRateLimited).then(|| {
        page(
            StatusCode::TOO_MANY_REQUESTS,
            "Infinity Horizon",
            &both(
                "Too many attempts from this network. Wait an hour and try again.",
                "Tentativas demais desta rede. Espere uma hora e tente de novo.",
            ),
        )
    })
}

fn lookup(token: &str) -> Option<[u8; 32]> {
    opaque::lookup(Kind::MailToken, token.trim())
}

fn confirm_form(status: StatusCode, token: &str, error: Option<(&str, &str)>) -> Response {
    let error = error
        .map(|(en, pt)| format!("<p class=\"error\">{en}<br>{pt}</p>"))
        .unwrap_or_default();
    let body = format!(
        "{}{error}<form method=\"post\" action=\"{action}\"><input type=\"hidden\" name=\"token\" value=\"{token}\">\
         <label for=\"password\">Password / Senha</label>\
         <input id=\"password\" name=\"password\" type=\"password\" maxlength=\"128\" autocomplete=\"current-password\" required>\
         <button type=\"submit\">Confirm / Confirmar</button></form>",
        both(
            "Enter the password of your account and press the button to confirm its e-mail address.",
            "Digite a senha da sua conta e clique no botão para confirmar o endereço de e-mail.",
        ),
        action = r::AUTH_EMAIL_CONFIRM,
        token = escape(token.trim()),
    );
    page(status, "Confirm your e-mail / Confirme seu e-mail", &body)
}

/// *public* `GET /launcher/v1/auth/email/confirm?token=` (`local` only): the confirmation form.
/// Spends nothing (mail scanners prefetch links).
pub async fn confirm_page(Query(q): Query<LinkQuery>) -> Response {
    if lookup(&q.token).is_none() {
        return invalid_link();
    }
    confirm_form(StatusCode::OK, &q.token, None)
}

/// *public* `POST /launcher/v1/auth/email/confirm` (`local` only, form `token`, `password`):
/// with the account's current password (checked like a sign-in), spends the confirmation token
/// and confirms the address. A wrong password keeps the token (security review M1: after a
/// takeover the newest link confirms only for whoever knows the password now set).
pub async fn confirm_submit(
    State(state): State<AppState>,
    net: ClientNetwork,
    Form(form): Form<ConfirmForm>,
) -> Result<Response, ApiError> {
    provider(&state)?;
    if let Err(e) = limit(
        &state,
        &limits::LOCAL_MAIL_NET,
        &format!("confirm:{}", net.0),
    )
    .await
    {
        return too_many(&e).map(Ok).unwrap_or(Err(e));
    }
    let Some(lookup) = lookup(&form.token) else {
        return Ok(invalid_link());
    };
    let wrong = || {
        confirm_form(
            StatusCode::BAD_REQUEST,
            &form.token,
            Some(("The password is incorrect.", "A senha está incorreta.")),
        )
    };
    if form.password.is_empty() || form.password.chars().count() > password::MAX_CHARS {
        return Ok(wrong());
    }
    let found = {
        let mut conn = state.pool.acquire().await?;
        match credential::open_mail_token_in(&mut conn, MailPurpose::Confirm, &lookup).await? {
            Some(id) => credential::find_by_id_in(&mut conn, id).await?,
            None => return Ok(invalid_link()),
        }
    };
    let Some(c) = found else {
        return Ok(invalid_link());
    };
    if matches!(
        check(&state, c.password_hash.clone(), form.password.clone()).await?,
        Checked::Refused
    ) {
        return Ok(wrong());
    }
    let mut tx = state.pool.begin().await?;
    let Some(id) = credential::spend_mail_token_in(&mut tx, MailPurpose::Confirm, &lookup).await?
    else {
        return Ok(invalid_link());
    };
    // The password checked must still be the credential's (a takeover or reset meanwhile).
    match credential::find_by_id_in(&mut tx, id).await? {
        Some(now) if id == c.id && now.updated_at == c.updated_at => {}
        _ => return Ok(invalid_link()),
    }
    credential::confirm_email_in(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(credential = %id, "e-mail confirmed");
    Ok(page(
        StatusCode::OK,
        "E-mail confirmed / E-mail confirmado",
        &both(
            "Your e-mail address is confirmed. Go back to the launcher and sign in.",
            "Seu endereço de e-mail está confirmado. Volte ao launcher e entre.",
        ),
    ))
}

fn password_form(status: StatusCode, token: &str, error: Option<(&str, &str)>) -> Response {
    let error = error
        .map(|(en, pt)| format!("<p class=\"error\">{en}<br>{pt}</p>"))
        .unwrap_or_default();
    let body = format!(
        "{}{error}<form method=\"post\" action=\"{action}\"><input type=\"hidden\" name=\"token\" value=\"{token}\">\
         <label for=\"password\">New password / Nova senha (8-128)</label>\
         <input id=\"password\" name=\"password\" type=\"password\" minlength=\"8\" maxlength=\"128\" autocomplete=\"new-password\" required>\
         <label for=\"repeat\">Repeat / Repita</label>\
         <input id=\"repeat\" name=\"repeat\" type=\"password\" minlength=\"8\" maxlength=\"128\" autocomplete=\"new-password\" required>\
         <button type=\"submit\">Save / Salvar</button></form>",
        both(
            "Choose the password of your account. Saving it signs the account out everywhere.",
            "Escolha a senha da sua conta. Salvar desconecta a conta em todos os lugares.",
        ),
        action = r::AUTH_EMAIL_PASSWORD,
        token = escape(token.trim()),
    );
    page(status, "Set your password / Defina sua senha", &body)
}

/// *public* `GET /launcher/v1/auth/email/password?token=` (`local` only): the password form.
/// Spends nothing.
pub async fn password_page(Query(q): Query<LinkQuery>) -> Response {
    if lookup(&q.token).is_none() {
        return invalid_link();
    }
    password_form(StatusCode::OK, &q.token, None)
}

/// *public* `POST /launcher/v1/auth/email/password` (`local` only, form `token`, `password`,
/// `repeat`): spends the token, sets the password, confirms the address, spends the credential's
/// other links and signs its account out everywhere.
pub async fn password_submit(
    State(state): State<AppState>,
    net: ClientNetwork,
    Form(form): Form<PasswordForm>,
) -> Result<Response, ApiError> {
    provider(&state)?;
    if let Err(e) = limit(
        &state,
        &limits::LOCAL_MAIL_NET,
        &format!("password:{}", net.0),
    )
    .await
    {
        return too_many(&e).map(Ok).unwrap_or(Err(e));
    }
    let Some(lookup) = lookup(&form.token) else {
        return Ok(invalid_link());
    };
    if !password::acceptable(&form.password) {
        return Ok(password_form(
            StatusCode::BAD_REQUEST,
            &form.token,
            Some((
                "The password needs 8 to 128 characters.",
                "A senha precisa ter de 8 a 128 caracteres.",
            )),
        ));
    }
    if form.password != form.repeat {
        return Ok(password_form(
            StatusCode::BAD_REQUEST,
            &form.token,
            Some((
                "The two passwords differ.",
                "As duas senhas são diferentes.",
            )),
        ));
    }
    // No password work for a token that cannot be spent.
    {
        let mut conn = state.pool.acquire().await?;
        if credential::open_mail_token_in(&mut conn, MailPurpose::Reset, &lookup)
            .await?
            .is_none()
        {
            return Ok(invalid_link());
        }
    }
    let phc = hash(form.password).await?;
    let mut tx = state.pool.begin().await?;
    let Some(id) = credential::spend_mail_token_in(&mut tx, MailPurpose::Reset, &lookup).await?
    else {
        return Ok(invalid_link());
    };
    if !credential::set_password_hash_in(&mut tx, id, &phc).await? {
        return Ok(invalid_link());
    }
    credential::confirm_email_in(&mut tx, id).await?;
    credential::spend_all_mail_tokens_in(&mut tx, id).await?;
    let revoked = match credential::account_of_in(&mut tx, &id.to_string()).await? {
        Some(account) => tokens::revoke_account_in(&mut tx, account, "password_reset").await?,
        None => 0,
    };
    tx.commit().await?;
    tracing::info!(credential = %id, revoked, "password set by mail link");
    Ok(page(
        StatusCode::OK,
        "Password saved / Senha salva",
        &both(
            "Your new password is saved and the account was signed out everywhere. Go back to the launcher and sign in with it.",
            "Sua nova senha foi salva e a conta foi desconectada em todos os lugares. Volte ao launcher e entre com ela.",
        ),
    ))
}
