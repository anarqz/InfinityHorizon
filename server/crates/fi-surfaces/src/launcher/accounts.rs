//! `/launcher/v1/accounts`, `/legacy/link`, `/me`, `/profiles`, `/profiles/{xuid}` (`DELETE`),
//! `/profiles/{xuid}/mode`, `/me/deletion` (ACCOUNTS.md §4.1, §5, §8, §10).

use crate::errors::ApiError;
use crate::extract::{Body, CareerGame, ClientNetwork, IdempotencyKey, LauncherAuth};
use crate::signin::{self, LauncherTokens};
use crate::state::AppState;
use crate::views;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use fi_auth::opaque::{self, Kind};
use fi_auth::{ct_eq, idtoken};
use fi_db::accounts::{self, NewProfile as ProfileInput};
use fi_db::identity::{self, Client};
use fi_db::limits::{self, Recorded};
use fi_db::purge::{self, ModeChanged};
use fi_domain::{Gamertag, GamertagInput, ProfileMode, SiteKey};
use fi_protocol::ErrorCode;
use fi_protocol::accounts::{
    CareerCreate, Deletion, LauncherAccountCreate, LauncherAccountCreated, LauncherDeletionRequest,
    LegacyLink, Me, ModeChange, ModeChangeWarning, NameChange, NewProfile, PresenceClub, Profile,
    ProfilePresence, SignInResult, WebsiteVisibility,
};
use sqlx::PgConnection;
use uuid::Uuid;

const ACCOUNTS: &str = "POST /launcher/v1/accounts";
const LEGACY_LINK: &str = "POST /launcher/v1/legacy/link";
const PROFILES: &str = "POST /launcher/v1/profiles";
const PROFILE_MODE: &str = "POST /launcher/v1/profiles/{xuid}/mode";
const PROFILE_NAME: &str = "POST /launcher/v1/profiles/{xuid}/name";
const PROFILE_DELETE: &str = "DELETE /launcher/v1/profiles/{xuid}";
const PLAYER_CAREERS: &str = "POST /launcher/v1/players/{xuid}/careers";
const PLAYER_DELETE: &str = "DELETE /launcher/v1/players/{xuid}";
const ME_DELETION: &str = "POST /launcher/v1/me/deletion";

fn key_reused() -> ApiError {
    ApiError::new(
        ErrorCode::RequestIdempotencyKeyReused,
        "this Idempotency-Key was used for another request",
    )
}

fn ticket_invalid() -> ApiError {
    ApiError::new(
        ErrorCode::AuthIdentityTicketInvalid,
        "identity ticket unknown, used or expired: sign in again",
    )
}

/// Checks a profile to create.
pub fn new_profile(p: &NewProfile) -> Result<ProfileInput, ApiError> {
    let Some(rules) = fi_domain::GameRules::known(&p.game) else {
        return Err(ApiError::invalid("profile.game", "fh2"));
    };
    if !fi_domain::gamertag::valid_name(&p.name) {
        return Err(ApiError::new(
            ErrorCode::AccountsInvalidName,
            "a name is 1 to 20 letters or digits and starts with a letter",
        )
        .with_details(serde_json::json!({"field": "profile.name"})));
    }
    let mode: ProfileMode = p
        .mode
        .parse()
        .map_err(|_| ApiError::invalid("profile.mode", "hardcore or noob"))?;
    // CR-S97: a game's own modes (FM5 has no Hardcore regime for now).
    if !rules.allows(mode) {
        return Err(ApiError::invalid("profile.mode", "noob"));
    }
    // `starterCar` left the contract (OD-60): a launcher that still sends it is not refused, and
    // the value is ignored; nothing is written at creation (the line is seeded at first launch).
    Ok(ProfileInput {
        game: p.game.clone(),
        name: p.name.clone(),
        mode,
    })
}

/// A game open only with a pool (`GameRules::open_with_pool`, CR-S97: FM5) takes profiles only
/// while a pool of it accepts sessions: a server without one (production) answers as if the game
/// did not exist.
async fn require_open_game(state: &AppState, game: &str) -> Result<(), ApiError> {
    if !fi_domain::GameRules::of(game).open_with_pool {
        return Ok(());
    }
    let open: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pool WHERE game = $1 AND accepting)")
            .bind(game)
            .fetch_one(&state.pool)
            .await?;
    if open {
        Ok(())
    } else {
        Err(ApiError::invalid("profile.game", "fh2"))
    }
}

/// The idempotency identity of a `/profiles/{xuid}/*` write's path and body: the path is part of
/// the request (one key on another profile or career is another request). The default career's
/// (FH2's, no `?game=`) hashes as before CR-S98, so a released launcher's retry still matches.
fn path_of<T: serde::Serialize>(
    xuid: u64,
    game: Option<&str>,
    body: Option<&T>,
) -> serde_json::Value {
    let mut v = serde_json::json!({ "xuid": xuid.to_string() });
    if let Some(b) = body {
        v["body"] = serde_json::to_value(b).unwrap_or_default();
    }
    if let Some(game) = game.filter(|g| *g != fi_domain::FH2) {
        v["game"] = game.into();
    }
    v
}

fn created_body(t: LauncherTokens, profile: Profile) -> LauncherAccountCreated {
    LauncherAccountCreated {
        account_id: t.account_id.to_string(),
        refresh_token: t.refresh.into_inner(),
        access_token: t.access.into_inner(),
        access_expires_at: t.access_expires_at,
        profile,
    }
}

async fn first_profile(
    tx: &mut PgConnection,
    state: &AppState,
    account_id: Uuid,
) -> Result<Profile, ApiError> {
    let first = accounts::profiles_in(tx, account_id)
        .await?
        .into_iter()
        .next()
        .ok_or_else(ApiError::unavailable)?;
    profile_in(tx, state, &first).await
}

/// `Profile` with `cloudLine` and `contentRequired` (ACCOUNTS.md §2, CR-S51; OD-239: Hardcore
/// only, whatever the tier).
async fn profile_in(
    _conn: &mut PgConnection,
    _state: &AppState,
    row: &accounts::ProfileRow,
) -> Result<Profile, ApiError> {
    Ok(views::profile_lines(row))
}

/// *public* `POST /launcher/v1/accounts`: the account and its first profile, from a launcher
/// identity ticket. Limited per device (3 a day) and per client network (10 a day), counted in
/// the transaction so every instance sees the same count. A replay with the same key returns the
/// same account with new tokens (the first family revoked: CR-L4).
pub async fn create(
    State(state): State<AppState>,
    key: IdempotencyKey,
    net: ClientNetwork,
    Body(req): Body<LauncherAccountCreate>,
) -> Result<(StatusCode, Json<LauncherAccountCreated>), ApiError> {
    signin::require_registration(&state, &mut *state.pool.acquire().await?).await?;
    let device = signin::device(&state, &req.device)?;
    let profile = new_profile(&req.profile)?;
    require_open_game(&state, &profile.game).await?;
    let ticket =
        opaque::lookup(Kind::IdentityTicket, &req.identity_ticket).ok_or_else(ticket_invalid)?;
    let idem = key.of(ACCOUNTS, &req);
    let scope = format!("device:{}", device.device_id);
    // Twice at most: a concurrent first request with this key may spend the ticket and commit
    // while this one waits for the ticket's row; a fresh transaction then sees its record.
    for attempt in 0..2 {
        let mut tx = state.pool.begin().await?;
        limits::lock_key_in(&mut tx, &scope, &idem).await?;
        match limits::recorded_in(&mut tx, &scope, &idem).await? {
            // CR-L4: re-issued while the family the record names is live; once it is signed out
            // or revoked, the ticket's first use is all there was (AUTH.IDENTITY_TICKET_INVALID).
            Recorded::Same(record) => {
                let first = record.resource_id.ok_or_else(ApiError::unavailable)?;
                let t = signin::reissue_launcher_in(&mut tx, &state, &scope, &idem, first)
                    .await?
                    .ok_or_else(ticket_invalid)?;
                let profile = first_profile(&mut tx, &state, t.account_id).await?;
                tx.commit().await?;
                return Ok((StatusCode::CREATED, Json(created_body(t, profile))));
            }
            Recorded::Different => return Err(key_reused()),
            Recorded::None => {}
        }
        signin::limit_in(
            &mut tx,
            &state,
            &limits::CREATE_DEVICE,
            &format!("device:{}", device.device_id),
            ErrorCode::RequestRateLimited,
        )
        .await?;
        signin::limit_in(
            &mut tx,
            &state,
            &limits::CREATE_NET,
            &net.0,
            ErrorCode::RequestRateLimited,
        )
        .await?;
        match identity::consume_ticket_in(&mut tx, &ticket, Client::Launcher).await? {
            Some(t) => {
                let (account_id, created) =
                    accounts::create_account_in(&mut tx, &t.claim, None, Some(&profile)).await?;
                let created = created.ok_or_else(ApiError::unavailable)?;
                accounts::audit_in(&mut tx, account_id, "account.created").await?;
                let tokens =
                    signin::issue_launcher_in(&mut tx, &state, account_id, "player", &device)
                        .await?;
                limits::record_with_response_in(
                    &mut tx,
                    &scope,
                    &idem,
                    201,
                    Some(tokens.issued.token_id),
                    Some(&signin::outcome("account_created")),
                )
                .await?;
                let body = profile_in(&mut tx, &state, &created).await?;
                tx.commit().await?;
                return Ok((StatusCode::CREATED, Json(created_body(tokens, body))));
            }
            // Rolled back (the limit counts with it); the second attempt answers the replay.
            None if attempt == 0 => continue,
            None => return Err(ticket_invalid()),
        }
    }
    Err(ticket_invalid())
}

/// *public* `POST /launcher/v1/legacy/link`: an identity ticket plus a Beta account's gamertag and
/// site key (ACCOUNTS.md §4.1, §10). Unknown gamertag and wrong key answer alike; the key is spent.
/// The device-token form (`legacyToken`) arrives with SER-1.2.
pub async fn legacy_link(
    State(state): State<AppState>,
    key: IdempotencyKey,
    net: ClientNetwork,
    Body(req): Body<LegacyLink>,
) -> Result<Json<SignInResult>, ApiError> {
    let device = signin::device(&state, &req.device)?;
    let ticket =
        opaque::lookup(Kind::IdentityTicket, &req.identity_ticket).ok_or_else(ticket_invalid)?;
    let (gamertag, site_key) = match (&req.gamertag, &req.site_key, &req.legacy_token) {
        (Some(g), Some(k), None) => (g.clone(), k.clone()),
        (None, None, Some(_)) => {
            return Err(ApiError::new(
                ErrorCode::AuthLegacyTokenUnknown,
                "legacy device-token links are not available yet: use gamertag and site key",
            ));
        }
        _ => {
            return Err(ApiError::invalid(
                "siteKey",
                "gamertag and siteKey, or legacyToken",
            ));
        }
    };
    let idem = key.of(LEGACY_LINK, &req);
    let scope = format!("device:{}", device.device_id);
    signin::limit(
        &state,
        &limits::LEGACY_GAMERTAG,
        &format!("tag:{}", gamertag.trim().to_ascii_lowercase()),
        ErrorCode::AuthRateLimited,
    )
    .await?;
    signin::limit(
        &state,
        &limits::LEGACY_NET,
        &net.0,
        ErrorCode::AuthRateLimited,
    )
    .await?;
    let wrong = || {
        ApiError::new(
            ErrorCode::AuthInvalidCredentials,
            "gamertag or site key wrong",
        )
    };
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        // CR-L4, as for `POST /accounts`: only while the family the record names is live.
        Recorded::Same(record) => {
            let first = record.resource_id.ok_or_else(ApiError::unavailable)?;
            let t = signin::reissue_launcher_in(&mut tx, &state, &scope, &idem, first)
                .await?
                .ok_or_else(ticket_invalid)?;
            tx.commit().await?;
            return Ok(Json(signed_in(t)));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    let t = identity::consume_ticket_in(&mut tx, &ticket, Client::Launcher)
        .await?
        .ok_or_else(ticket_invalid)?;
    let now = identity::now_in(&mut tx).await?;
    if !idtoken::is_recent(
        t.claim.auth_time.millis().div_euclid(1_000),
        now.millis().div_euclid(1_000),
    ) {
        return Err(ApiError::new(
            ErrorCode::AuthReauthRequired,
            "sign in again (the last sign-in is older than 5 minutes)",
        ));
    }
    let tag: Gamertag = gamertag.parse().map_err(|_| wrong())?;
    let typed: SiteKey = site_key.parse().map_err(|_| wrong())?;
    let found = identity::find_legacy_account_in(&mut tx, "fh2", tag.name(), tag.discriminator())
        .await?
        .ok_or_else(wrong)?;
    let pepper = state.id().pepper()?;
    let matches = match (found.legacy_wrapped, found.key_hmac) {
        (Some(stored), _) => pepper
            .legacy_wrapped(found.pepper_version, typed.canonical())
            .is_some_and(|c| ct_eq(&c, &stored)),
        (None, Some(stored)) => pepper
            .key_hmac(found.pepper_version, typed.canonical())
            .is_some_and(|c| ct_eq(&c, &stored)),
        (None, None) => false,
    };
    if !matches {
        return Err(wrong());
    }
    if found.banned {
        return Err(crate::errors::banned(found.ban_review.as_deref()));
    }
    if found.linked {
        return Err(ApiError::new(
            ErrorCode::AccountsAlreadyLinked,
            "the Beta account is linked to another identity",
        ));
    }
    accounts::link_identity_in(&mut tx, found.account_id, &t.claim).await?;
    identity::spend_legacy_credentials_in(&mut tx, found.account_id).await?;
    accounts::audit_in(&mut tx, found.account_id, "account.legacy_linked").await?;
    let role = accounts::lock_account_in(&mut tx, found.account_id)
        .await?
        .role;
    let tokens =
        signin::issue_launcher_in(&mut tx, &state, found.account_id, &role, &device).await?;
    limits::record_with_response_in(
        &mut tx,
        &scope,
        &idem,
        200,
        Some(tokens.issued.token_id),
        Some(&signin::outcome("signed_in")),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(signed_in(tokens)))
}

fn signed_in(t: LauncherTokens) -> SignInResult {
    SignInResult::SignedIn {
        account_id: t.account_id.to_string(),
        refresh_token: t.refresh.into_inner(),
        access_token: t.access.into_inner(),
        access_expires_at: t.access_expires_at,
        profile: None,
    }
}

/// `GET /launcher/v1/me`.
pub async fn me(State(state): State<AppState>, auth: LauncherAuth) -> Result<Json<Me>, ApiError> {
    let me = accounts::me(&state.pool, auth.account_id)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::AuthTokenInvalid, "account gone"))?;
    Ok(Json(views::me(&me)))
}

/// `GET /launcher/v1/profiles/{xuid}/presence` (ACCOUNTS.md §5 "Presence", CR-S72, OD-136): the
/// caller's own profile's club of its mode and its last accepted profile sync (level, credits),
/// for the launcher's Discord presence. Another account's profile, an unknown or an erased one is
/// 404 `ACCOUNTS.NOT_FOUND`; past 60 reads an hour, 429 `REQUEST.RATE_LIMITED`. With `?car=`
/// (CR-S77), `liveryImage` names the profile's own servable Storefront preview of that car; a
/// missing or malformed `car` (or any other query) only leaves it out.
pub async fn presence(
    State(state): State<AppState>,
    auth: LauncherAuth,
    named: CareerGame,
    Path(xuid): Path<String>,
    query: Result<
        Query<std::collections::HashMap<String, String>>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Result<Json<ProfilePresence>, ApiError> {
    let not_found = || ApiError::new(ErrorCode::AccountsNotFound, "no such profile");
    let xuid: u64 = xuid.trim().parse().map_err(|_| not_found())?;
    let mut conn = state.pool.acquire().await?;
    let game = named.resolve_in(&mut conn, auth.account_id, xuid).await?;
    let key = limits::account_key(&limits::PRESENCE_READS, auth.account_id);
    if let limits::Hit::Limited { retry_after_secs } =
        limits::hit_in(&mut conn, &key, &limits::PRESENCE_READS).await?
    {
        return Err(ApiError::new(
            ErrorCode::RequestRateLimited,
            "too many presence reads this hour, wait Retry-After",
        )
        .retry_after(retry_after_secs.max(1) as u64));
    }
    let row = accounts::presence_in(&mut conn, auth.account_id, xuid, &game)
        .await?
        .ok_or_else(not_found)?;
    let (level, credits, synced_at) = match row.synced {
        Some((level, credits, at)) => (Some(level), Some(credits), Some(views::at(at))),
        None => (None, None, None),
    };
    let car = query
        .as_ref()
        .ok()
        .and_then(|Query(q)| q.get("car"))
        .and_then(|c| c.trim().parse::<i32>().ok())
        .filter(|c| *c > 0);
    let origin = state.id().info.services.core.trim_end_matches('/');
    let livery_image = match car {
        Some(car) if origin.starts_with("https://") => {
            fi_db::storefront::own_preview_token_in(&mut conn, auth.account_id, xuid, car, &game)
                .await?
                .map(|t| {
                    format!(
                        "{origin}{}",
                        fi_protocol::routes::media::preview_path(&fi_db::storefront::token_hex(&t))
                    )
                })
        }
        _ => None,
    };
    Ok(Json(ProfilePresence {
        club: row.club.map(|(tag, name)| PresenceClub { tag, name }),
        level,
        credits,
        synced_at,
        livery_image,
    }))
}

/// `POST /launcher/v1/players/{xuid}/careers` (CR-S98, CAREERS.md §3): the profile's empty
/// career of `game` → 201 `Profile` (the career); a replay with the key returns it again. 409
/// `ACCOUNTS.CAREER_EXISTS` when the profile has one live; 400 `profile.game` for an
/// unknown or closed game; 400 `profile.mode` when the game does not allow the profile's mode;
/// 404 `ACCOUNTS.NOT_FOUND` for another account's, an unknown or an erased profile.
pub async fn add_career(
    State(state): State<AppState>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Path(xuid): Path<String>,
    Body(req): Body<CareerCreate>,
) -> Result<(StatusCode, Json<Profile>), ApiError> {
    use fi_db::accounts::CareerError as E;
    let not_found = || ApiError::new(ErrorCode::AccountsNotFound, "no such profile");
    let xuid: u64 = xuid.trim().parse().map_err(|_| not_found())?;
    if fi_domain::GameRules::known(&req.game).is_none() {
        return Err(ApiError::invalid("profile.game", "fh2"));
    }
    require_open_game(&state, &req.game).await?;
    let idem = key.of(
        PLAYER_CAREERS,
        &serde_json::json!({"xuid": xuid.to_string(), "body": &req}),
    );
    let scope = format!("account:{}", auth.account_id);
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(record) => {
            let body: Profile = record
                .response
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or_else(ApiError::unavailable)?;
            return Ok((StatusCode::CREATED, Json(body)));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    let created = accounts::add_career_in(&mut tx, auth.account_id, xuid, &req.game)
        .await
        .map_err(|e| match e {
            E::NotFound => not_found(),
            E::DeletionPending => ApiError::new(
                ErrorCode::AccountsDeletionPending,
                "the account or the profile is pending deletion",
            ),
            E::Exists => ApiError::new(
                ErrorCode::AccountsCareerExists,
                "the profile already has a career of this game",
            ),
            E::ModeNotAllowed => ApiError::invalid("profile.mode", "noob"),
            E::Banned { review } => crate::errors::banned(review.as_deref()),
            E::Db(e) => e.into(),
        })?;
    let body = profile_in(&mut tx, &state, &created).await?;
    let stored = serde_json::to_value(&body).map_err(|_| ApiError::unavailable())?;
    limits::record_with_response_in(
        &mut tx,
        &scope,
        &idem,
        201,
        Some(created.profile_id),
        Some(&stored),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// `POST /launcher/v1/profiles`: another profile and its first career (at most one live profile
/// per mode, CR-S98); a replay returns it again.
pub async fn add_profile(
    State(state): State<AppState>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Body(req): Body<NewProfile>,
) -> Result<(StatusCode, Json<Profile>), ApiError> {
    if auth.account.status == "deletion_pending" {
        return Err(ApiError::new(
            ErrorCode::AccountsDeletionPending,
            "the account is pending deletion",
        ));
    }
    let profile = new_profile(&req)?;
    require_open_game(&state, &profile.game).await?;
    let idem = key.of(PROFILES, &req);
    let scope = format!("account:{}", auth.account_id);
    let mut tx = state.pool.begin().await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(record) => {
            let body: Profile = record
                .response
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or_else(ApiError::unavailable)?;
            return Ok((StatusCode::CREATED, Json(body)));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    let created = accounts::add_profile_in(&mut tx, auth.account_id, &profile).await?;
    let body = profile_in(&mut tx, &state, &created).await?;
    let stored = serde_json::to_value(&body).map_err(|_| ApiError::unavailable())?;
    limits::record_with_response_in(
        &mut tx,
        &scope,
        &idem,
        201,
        Some(created.profile_id),
        Some(&stored),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// `POST /launcher/v1/profiles/{xuid}/mode` (ACCOUNTS.md §5 "Mode change", OD-52): Hardcore →
/// Noob in two steps. Without `confirm: true` nothing is written or recorded and the answer is 409
/// `ACCOUNTS.MODE_CHANGE_CONFIRM` with what would be deleted; confirmed, the profile becomes Noob,
/// its universe rows are deleted and its save line retired, in one transaction with the
/// idempotency record (a retry with the key replays the same `Profile`). Noob → Noob is a no-op.
pub async fn change_mode(
    State(state): State<AppState>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    named: CareerGame,
    Path(xuid): Path<String>,
    Body(req): Body<ModeChange>,
) -> Result<Json<Profile>, ApiError> {
    let to: ProfileMode = req
        .mode
        .parse()
        .map_err(|_| ApiError::invalid("mode", "hardcore or noob"))?;
    let xuid: u64 = xuid
        .trim()
        .parse()
        .map_err(|_| ApiError::new(ErrorCode::AccountsNotFound, "no such profile"))?;
    let confirmed = req.confirm == Some(true);
    // The path is part of the request: one key on another profile is another request.
    let idem = key.of(PROFILE_MODE, &path_of(xuid, named.0.as_deref(), Some(&req)));
    let scope = format!("account:{}", auth.account_id);
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(record) => {
            let body: Profile = record
                .response
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or_else(ApiError::unavailable)?;
            return Ok(Json(body));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    let game = named
        .resolve_locked_in(&mut tx, auth.account_id, xuid)
        .await?;
    match purge::change_mode_in(&mut tx, auth.account_id, xuid, to, confirmed, &game)
        .await
        .map_err(|e| ApiError::from(e).in_game(&game))?
    {
        ModeChanged::Unchanged(profile) => {
            let body = profile_in(&mut tx, &state, &profile).await?;
            tx.rollback().await?;
            Ok(Json(body))
        }
        ModeChanged::Confirm(warning) => {
            tx.rollback().await?;
            let details = serde_json::to_value(ModeChangeWarning {
                save_revision: warning.save_revision,
                deletes: warning.deletes,
            })
            .map_err(|_| ApiError::unavailable())?;
            Err(ApiError::new(
                ErrorCode::AccountsModeChangeConfirm,
                "Hardcore to Azimuth deletes the profile's cloud data: confirm to proceed",
            )
            .with_details(details))
        }
        ModeChanged::Changed { profile, .. } => {
            let body = profile_in(&mut tx, &state, &profile).await?;
            let stored = serde_json::to_value(&body).map_err(|_| ApiError::unavailable())?;
            limits::record_with_response_in(
                &mut tx,
                &scope,
                &idem,
                200,
                Some(profile.profile_id),
                Some(&stored),
            )
            .await?;
            tx.commit().await?;
            Ok(Json(body))
        }
    }
}

/// `POST /launcher/v1/profiles/{xuid}/name` (ACCOUNTS.md §5 "Plain names and renames", OD-120):
/// the free rename after a clash rename. The current name keeps it, another free name takes it;
/// either clears the notice, in one transaction with the idempotency record (a retry with the key
/// replays the same `Profile`).
pub async fn rename_profile(
    State(state): State<AppState>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    named: CareerGame,
    Path(xuid): Path<String>,
    Body(req): Body<NameChange>,
) -> Result<Json<Profile>, ApiError> {
    let name = req.name.trim();
    if !fi_domain::gamertag::valid_name(name) {
        return Err(ApiError::new(
            ErrorCode::AccountsInvalidName,
            "a name is 1 to 20 letters or digits and starts with a letter",
        )
        .with_details(serde_json::json!({"field": "name"})));
    }
    let xuid: u64 = xuid
        .trim()
        .parse()
        .map_err(|_| ApiError::new(ErrorCode::AccountsNotFound, "no such profile"))?;
    let idem = key.of(PROFILE_NAME, &path_of(xuid, named.0.as_deref(), Some(&req)));
    let scope = format!("account:{}", auth.account_id);
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(record) => {
            let body: Profile = record
                .response
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or_else(ApiError::unavailable)?;
            return Ok(Json(body));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    let game = named
        .resolve_locked_in(&mut tx, auth.account_id, xuid)
        .await?;
    match fi_db::rename::rename_in(&mut tx, auth.account_id, xuid, name, &game).await? {
        fi_db::rename::Renamed::Unchanged(profile) => {
            let body = profile_in(&mut tx, &state, &profile).await?;
            tx.rollback().await?;
            Ok(Json(body))
        }
        fi_db::rename::Renamed::Done { profile, .. } => {
            let body = profile_in(&mut tx, &state, &profile).await?;
            let stored = serde_json::to_value(&body).map_err(|_| ApiError::unavailable())?;
            limits::record_with_response_in(
                &mut tx,
                &scope,
                &idem,
                200,
                Some(profile.profile_id),
                Some(&stored),
            )
            .await?;
            tx.commit().await?;
            Ok(Json(body))
        }
    }
}

/// `POST /launcher/v1/profiles/{xuid}/website` (ACCOUNTS.md §5, CR-S83, OD-168 follow-up DB-Q4):
/// whether the caller's own profile appears on the play dashboard. Setting the current value is a
/// no-op 200, so it needs no idempotency key; 200 `Profile` either way.
pub async fn set_website(
    State(state): State<AppState>,
    auth: LauncherAuth,
    named: CareerGame,
    Path(xuid): Path<String>,
    Body(req): Body<WebsiteVisibility>,
) -> Result<Json<Profile>, ApiError> {
    use fi_db::dashboard::WebsiteError as W;
    let xuid: u64 = xuid
        .trim()
        .parse()
        .map_err(|_| ApiError::new(ErrorCode::AccountsNotFound, "no such profile"))?;
    let mut tx = state.pool.begin().await?;
    let game = named
        .resolve_locked_in(&mut tx, auth.account_id, xuid)
        .await?;
    let profile =
        fi_db::dashboard::set_web_listed_in(&mut tx, auth.account_id, xuid, req.listed, &game)
            .await
            .map_err(|e| match e {
                W::NotFound => ApiError::new(ErrorCode::AccountsNotFound, "no such profile"),
                W::DeletionPending => ApiError::new(
                    ErrorCode::AccountsDeletionPending,
                    "the account or the profile is pending deletion",
                ),
                W::Db(e) => e.into(),
            })?;
    let body = profile_in(&mut tx, &state, &profile).await?;
    tx.commit().await?;
    Ok(Json(body))
}

/// `DELETE /launcher/v1/profiles/{xuid}` (ACCOUNTS.md §5, owner 2026-10-27, CR-S61): the profile's
/// cloud data erased at once, in one transaction with the idempotency record (`fi_db::erasure::
/// delete_profile_in`); 204, no body. A retry with the key replays the 204; without it, a deleted
/// profile is 404 `ACCOUNTS.NOT_FOUND` (the launcher treats both as done). The record names no
/// profile (no resource, no body), so nothing of it outlives the deletion.
pub async fn delete_profile(
    State(state): State<AppState>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    named: CareerGame,
    Path(xuid): Path<String>,
) -> Result<StatusCode, ApiError> {
    let xuid: u64 = xuid
        .trim()
        .parse()
        .map_err(|_| ApiError::new(ErrorCode::AccountsNotFound, "no such profile"))?;
    // The path is the request: one key on another profile is another request.
    let idem = key.of(
        PROFILE_DELETE,
        &path_of::<()>(xuid, named.0.as_deref(), None),
    );
    let scope = format!("account:{}", auth.account_id);
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(_) => return Ok(StatusCode::NO_CONTENT),
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    let game = named
        .resolve_locked_in(&mut tx, auth.account_id, xuid)
        .await?;
    fi_db::erasure::delete_profile_in(&mut tx, auth.account_id, xuid, &game)
        .await
        .map_err(|e| ApiError::from(e).in_game(&game))?;
    limits::record_with_response_in(&mut tx, &scope, &idem, 204, None, None).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /launcher/v1/players/{xuid}` (CR-S98, CAREERS.md §3 "delete profile"): every career
/// deleted as `DELETE /profiles/{xuid}` does, then the profile, in one transaction with the
/// idempotency record (`fi_db::erasure::delete_player_in`); 204, no body; the same refusals and
/// rolling hour. A retry with the key replays the 204; without it, a deleted profile is 404
/// `ACCOUNTS.NOT_FOUND`.
pub async fn delete_player(
    State(state): State<AppState>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Path(xuid): Path<String>,
) -> Result<StatusCode, ApiError> {
    let xuid: u64 = xuid
        .trim()
        .parse()
        .map_err(|_| ApiError::new(ErrorCode::AccountsNotFound, "no such profile"))?;
    let idem = key.of(
        PLAYER_DELETE,
        &serde_json::json!({"xuid": xuid.to_string()}),
    );
    let scope = format!("account:{}", auth.account_id);
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(_) => return Ok(StatusCode::NO_CONTENT),
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    fi_db::erasure::delete_player_in(&mut tx, auth.account_id, xuid).await?;
    limits::record_with_response_in(&mut tx, &scope, &idem, 204, None, None).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /launcher/v1/me/deletion`: typed gamertag plus a fresh ID token of the same
/// identity (`auth_time` within 5 minutes) → 202 `{requestedAt, purgeAt}` (14 days).
pub async fn request_deletion(
    State(state): State<AppState>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Body(req): Body<LauncherDeletionRequest>,
) -> Result<(StatusCode, Json<Deletion>), ApiError> {
    // Verification can refresh the key set: never hold a transaction during it.
    let claim = signin::verify_id_token(&state, &req.reauth.id_token).await?;
    let idem = key.of(ME_DELETION, &req);
    let scope = format!("account:{}", auth.account_id);
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    super::local::require_current_in(&mut tx, &state, &claim).await?;
    let uid = accounts::uid_of_in(&mut tx, auth.account_id).await?;
    if uid.as_deref() != Some(claim.idp_uid.as_str()) {
        return Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "the sign-in belongs to another identity",
        ));
    }
    let now = identity::now_in(&mut tx).await?;
    if !idtoken::is_recent(
        claim.auth_time.millis().div_euclid(1_000),
        now.millis().div_euclid(1_000),
    ) {
        return Err(ApiError::new(
            ErrorCode::AuthReauthRequired,
            "sign in again (the last sign-in is older than 5 minutes)",
        ));
    }
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(record) => {
            let body: Deletion = record
                .response
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or_else(ApiError::unavailable)?;
            return Ok((StatusCode::ACCEPTED, Json(body)));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    let mismatch = || {
        ApiError::new(
            ErrorCode::AccountsConfirmationMismatch,
            "type one of the account's gamertags to confirm",
        )
    };
    // The plain name, case-insensitive (OD-120); a legacy `Name#12345` names the live profile
    // that carried it (its name then, its number exact).
    let typed: GamertagInput = req.confirm_gamertag.parse().map_err(|_| mismatch())?;
    let matches = match &typed {
        GamertagInput::Plain(name) => accounts::profiles_in(&mut tx, auth.account_id)
            .await?
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name)),
        GamertagInput::Legacy(legacy) => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM profile WHERE account_id = $1 AND status <> 'deleted' \
               AND lower(coalesce(legacy_name, name)) = lower($2) AND discriminator = $3)",
        )
        .bind(auth.account_id)
        .bind(legacy.name())
        .bind(legacy.discriminator() as i32)
        .fetch_one(&mut *tx)
        .await?,
    };
    if !matches {
        return Err(mismatch());
    }
    let scheduled = accounts::request_deletion_in(&mut tx, auth.account_id, auth.family_id).await?;
    let body = Deletion {
        requested_at: views::at(scheduled.requested_at),
        purge_at: views::at(scheduled.purge_at),
    };
    let stored = serde_json::to_value(&body).map_err(|_| ApiError::unavailable())?;
    limits::record_with_response_in(&mut tx, &scope, &idem, 202, None, Some(&stored)).await?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(body)))
}

/// `DELETE /launcher/v1/me/deletion`: cancels a pending deletion.
pub async fn cancel_deletion(
    State(state): State<AppState>,
    auth: LauncherAuth,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool.begin().await?;
    accounts::cancel_deletion_in(&mut tx, auth.account_id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
