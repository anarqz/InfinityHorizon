//! Saves (SAVES_API.md §4): head, history, bundle download (re-hashed), commit (`If-Match`,
//! `Idempotency-Key`, `X-Fi-Session-Id`, `X-Fi-Fence`, `X-Fi-Commit-Kind`, `Repr-Digest`,
//! `X-Fi-Content-Sha256`) through `fi_saves::SaveService<PgSaveStore, fi_storage::Blobs>`;
//! restore and parked bundles are optional in v0 and not served. Built by workstream A of the
//! local environment v0.
//!
//! The service's upload order holds here unchanged: cheap refusals first (size, digest, head and
//! lease), then verification, then the create-only blob put, then the commit transaction (no
//! transaction spans the network call). The declared content digest is checked against the one
//! verification computes (`SAVES.DIGEST_MISMATCH`), so the launcher's sync mark and the server
//! agree on what was saved.
//!
//! Cloud eligibility (OD-52, SAVES_API.md §4.7; per server since CR-S51): a commit is refused 403
//! `SAVES.NOT_ELIGIBLE` before any verification unless the account's tier at the database clock and
//! the profile's mode are in the server's `FI_CLOUD_SAVES` ([`fi_db::cloud::keeps`]; OD-52's
//! Supporter Hardcore by default), whatever lease the caller holds; a refusal for a lapse marks the
//! line lapsed. Reads (head, history, download) stay the owner's. The head carries `lapsedSince`
//! (the renewal mark) while it is set, and `start: "fresh_start"` on a Hardcore line at revision 0
//! that starts from Azimuth's first save (CR-S51, SAVES_API.md §4.9, [`starts_fresh_in`]).
//!
//! The fresh start (CR-S51): a `fresh_start` commit (`If-Match: "0"`, a Hardcore line whose start
//! is `fresh_start`) is checked by `fi_saves::fresh` with the server's `FI_FRESH_START_MAX_*`
//! ceilings and the bundle's PlayerDatabase read by `fi-playerdb` ([`playerdb_facts`]); refused
//! 422 `SAVES.GATE_REFUSED {rule: "NOT_FRESH_START"}` and parked like every gate refusal.
//!
//! The Hardcore seed (OD-60, SAVES_API.md §4.8, [`fi_db::seeds`]): `POST .../save/seed` makes the
//! game's active seed an eligible Hardcore line's revision 1 (`fresh_career`, the same stored
//! object) and its RPC baseline; `GET /launcher/v1/games/{game}/seeds/hardcore` serves the seed's
//! bytes, re-hashed, to any launcher.
//!
//! Who may call: [`SaveCaller`], the launcher's platform session. On the account plane it is
//! [`LauncherAuth`] (the ring); on the platform v1 saves deployment, which installs
//! [`crate::game::ticket::Tickets`] and holds public keys only, the same access-token checks run
//! with `FI_PLATFORM_VERIFY_KEYS` (then the account and the refresh family in Postgres, as
//! `LauncherAuth` does), so the save line is served without the private key (ADR 0014 §2).

use crate::config::PlatformConfig;
use crate::errors::ApiError;
use crate::extract::{CareerGame, IdempotencyKey, LauncherAuth};
use crate::game::ticket::{self, Tickets};
use crate::state::{AppState, Platform};
use crate::views;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Extension, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fi_db::accounts::AccessRow;
use fi_db::admission::{self, ProfileRef};
use fi_db::{PgPool, PgSaveStore};
use fi_db::{cloud, seeds};
use fi_domain::{DeviceId, PlaySessionId};
use fi_protocol::routes::launcher as r;
use fi_protocol::saves::{
    CommitResponse, LeaseView, Revision as RevisionBody, RevisionPage, SaveHead, SaveStart,
    SaveSummary, SeedCheck, UploadCheck,
};
use fi_protocol::{ErrorCode, headers};
use fi_saves::bundle::{MAX_BUNDLE, Verified};
use fi_saves::fresh::{FreshStartCheck, HardcoreStart, PlayerDbFacts, PlayerDbUnreadable};
use fi_saves::{
    CommitId, CommitKind, Digest, FenceToken, HeadView, InlineVerifier, Revision, RevisionRecord,
    SaveError, SaveFormat, SavePolicy, SaveService, SaveStore, UploadRequest, UploadVerdict,
    Verifier,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// The launcher's platform session on a save route.
#[derive(Clone, Copy, Debug)]
pub struct SaveCaller {
    /// The account.
    pub account_id: Uuid,
    /// The refresh family of the access token.
    pub family_id: Uuid,
}

impl FromRequestParts<AppState> for SaveCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Some(tickets) = parts.extensions.get::<Tickets>().cloned() else {
            let auth = LauncherAuth::from_request_parts(parts, state).await?;
            return Ok(SaveCaller {
                account_id: auth.account_id,
                family_id: auth.family_id,
            });
        };
        let invalid = || ApiError::new(ErrorCode::AuthTokenInvalid, "invalid access token");
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(invalid)?;
        let verified = ticket::verify_access(
            &tickets.access_keys,
            token,
            fi_auth::access::Audience::Launcher,
            crate::state::now_secs(),
        )
        .map_err(|e| match e {
            fi_auth::access::AccessError::Expired => {
                ApiError::new(ErrorCode::AuthTokenExpired, "access token expired")
            }
            fi_auth::access::AccessError::Invalid => invalid(),
        })?;
        let account_id = Uuid::parse_str(&verified.account_id).map_err(|_| invalid())?;
        let family_id = Uuid::parse_str(&verified.family_id).map_err(|_| invalid())?;
        crate::extract::account_rate(parts, &tickets.accounts, account_id)?;
        let account = fi_db::accounts::account_state(&state.pool, account_id)
            .await?
            .filter(|a| a.status != "deleted")
            .ok_or_else(invalid)?;
        if account.banned {
            return Err(crate::errors::banned(account.ban_review.as_deref()));
        }
        if !fi_db::tokens::family_live(&state.pool, account_id, family_id).await? {
            return Err(invalid());
        }
        Ok(SaveCaller {
            account_id,
            family_id,
        })
    }
}

/// The save routes of the launcher surface.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::SAVE, get(head).put(commit))
        .route(r::SAVE_REVISIONS, get(revisions))
        .route(r::SAVE_REVISION_BUNDLE, get(bundle))
        .route(r::SAVE_SEED, post(seed))
        .route(r::SAVE_CHECK, post(seed_check))
        .route(r::SAVE_UPLOAD_CHECK, post(upload_check))
        .route(r::GAME_SEED_HARDCORE, get(seed_bundle))
}

fn invalid(field: &str, message: &'static str) -> ApiError {
    ApiError::new(ErrorCode::SavesInvalidArgument, message)
        .with_details(serde_json::json!({ "field": field }))
}

/// 403 `SAVES.NOT_ELIGIBLE {tier, mode}` (SAVES_API.md §4.7): the profile keeps no cloud save line
/// (OD-239: only a Hardcore profile does; an Azimuth profile plays its own Azimuth save).
pub fn not_eligible(access: &AccessRow, mode: &str) -> ApiError {
    ApiError::new(
        ErrorCode::SavesNotEligible,
        "an Azimuth profile keeps no cloud save: it plays your own Azimuth save",
    )
    .with_details(serde_json::json!({ "tier": cloud::tier(access).as_str(), "mode": mode }))
}

/// The save line's eligibility check on the caller's connection (OD-239, CR-S111): `Ok` when the
/// profile keeps a line (a Hardcore profile of a game with one, whatever the tier); else
/// `SAVES.NOT_ELIGIBLE` (a Noob/Azimuth profile: its old line, if any, is read-only, §4.2).
pub async fn check_eligible_in(
    conn: &mut sqlx::PgConnection,
    account: Uuid,
    profile: &ProfileRef,
) -> Result<(), ApiError> {
    if fi_domain::GameRules::of(&profile.game).line_mode(&profile.mode) {
        return Ok(());
    }
    let access = fi_db::accounts::access_in(conn, account).await?;
    Err(not_eligible(&access, &profile.mode))
}

/// A session's career (`POST /gate/v1/sessions`, `/launcher/v1/sessions`; CAREERS.md §3): the
/// body's `game` and `xuid`. A game the platform does not know is 400 `REQUEST.INVALID {field:
/// "game"}` as before CR-S98; a known game the profile has no career of is
/// `SAVES.PROFILE_NOT_FOUND`.
pub async fn session_profile_of(
    db: &PgPool,
    account: Uuid,
    xuid: &str,
    game: &str,
) -> Result<ProfileRef, ApiError> {
    match profile_of(db, account, xuid, &CareerGame(Some(game.to_owned()))).await {
        Err(e) if e.code == ErrorCode::SavesProfileNotFound => {
            let known: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM game WHERE code = $1)")
                    .bind(game)
                    .fetch_one(db)
                    .await?;
            Err(if known {
                e
            } else {
                ApiError::invalid("game", "a game code the platform serves")
            })
        }
        other => other,
    }
}

/// The signed-in account's live career of `game` of the profile `xuid` (CR-S98), else
/// `SAVES.PROFILE_NOT_FOUND`.
pub async fn profile_of(
    db: &PgPool,
    account: Uuid,
    xuid: &str,
    game: &CareerGame,
) -> Result<ProfileRef, ApiError> {
    let not_found = || ApiError::new(ErrorCode::SavesProfileNotFound, "no such profile");
    let xuid: u64 = xuid.trim().parse().map_err(|_| not_found())?;
    let mut conn = db.acquire().await?;
    let game = game.resolve_in(&mut conn, account, xuid).await?;
    admission::profile_by_xuid_in(&mut conn, account, xuid, &game)
        .await?
        .ok_or_else(not_found)
}

fn summary(s: &fi_saves::SaveSummary) -> SaveSummary {
    SaveSummary {
        credits: s.credits,
        xp: s.xp,
        level: s.level,
        cars: s.cars,
    }
}

/// Device labels of the account by install id, looked up once per request.
struct Labels<'a> {
    db: &'a PgPool,
    account: Uuid,
    seen: HashMap<DeviceId, Option<String>>,
}

impl<'a> Labels<'a> {
    fn new(db: &'a PgPool, account: Uuid) -> Self {
        Self {
            db,
            account,
            seen: HashMap::new(),
        }
    }

    async fn of(&mut self, device: Option<DeviceId>) -> Result<Option<String>, ApiError> {
        let Some(device) = device else {
            return Ok(None);
        };
        if let Some(label) = self.seen.get(&device) {
            return Ok(label.clone());
        }
        let mut conn = self.db.acquire().await?;
        let label = admission::device_label_in(&mut conn, self.account, device.as_uuid()).await?;
        self.seen.insert(device, label.clone());
        Ok(label)
    }
}

/// Whether a Hardcore line of `game` at revision 0 starts from Azimuth's first save (CR-S51,
/// SAVES_API.md §4.9): `FI_HARDCORE_START`, or unset and no active seed for the game.
pub async fn starts_fresh_in(
    conn: &mut sqlx::PgConnection,
    platform: &PlatformConfig,
    game: &str,
) -> Result<bool, ApiError> {
    let active_seed = match platform.hardcore_start {
        HardcoreStart::Auto => seeds::active_in(conn, game, seeds::HARDCORE)
            .await
            .map_err(|e| match e {
                seeds::SeedError::Db(e) => ApiError::from(e),
                _ => ApiError::unavailable(),
            })?
            .is_some(),
        // Not read: the setting decides alone.
        _ => true,
    };
    Ok(platform.hardcore_start.starts_fresh(active_seed))
}

/// `SaveHead` (§4.1) of a head read, with the line's renewal mark (`lapsedSince`, §4.7) when it
/// has a head, and its start (`start`, CR-S51 §4.9) when it is a Hardcore line at revision 0.
pub async fn head_body(
    db: &PgPool,
    platform: &PlatformConfig,
    game: &str,
    account: Uuid,
    head: &HeadView,
) -> Result<SaveHead, ApiError> {
    let mut labels = Labels::new(db, account);
    let lease = head.lease.as_ref().map(|l| LeaseView {
        device_label: l.holder.device_label.clone(),
        expires_at: views::at(l.expires_at),
        live: l.is_live(head.now),
    });
    Ok(match &head.current {
        None => {
            let fresh = head.mode == fi_domain::ProfileMode::Hardcore && {
                let mut conn = db.acquire().await?;
                starts_fresh_in(&mut conn, platform, game).await?
            };
            SaveHead {
                lease,
                start: if fresh {
                    SaveStart::FreshStart
                } else {
                    SaveStart::Seed
                },
                ..SaveHead::default()
            }
        }
        Some(c) => {
            let (lapsed_since, pruned) = {
                let mut conn = db.acquire().await?;
                (
                    cloud::lapsed_since_in(&mut conn, head.profile).await?,
                    fi_db::save_retention::bundle_pruned_in(
                        &mut conn,
                        head.profile.as_uuid(),
                        c.revision.get(),
                    )
                    .await?,
                )
            };
            SaveHead {
                revision: c.revision.get(),
                content_sha256: Some(c.content.hex()),
                blob_sha256: Some(c.blob.sha256.hex()),
                bytes: Some(c.blob.size),
                committed_at: Some(views::at(c.committed_at)),
                kind: Some(c.kind.as_str().to_owned()),
                device_label: labels.of(c.device).await?,
                summary: Some(summary(&c.summary)),
                lease,
                lapsed_since: lapsed_since.map(views::at),
                // Only at revision 0 (§4.1).
                start: SaveStart::Seed,
                // Retention (§8): said only when the head's file is gone.
                bundle: pruned.then_some(false),
            }
        }
    })
}

fn etag(revision: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{revision}\"")).unwrap_or(HeaderValue::from_static("\"0\""))
}

/// `GET .../save` → 200 `SaveHead`, `ETag: "<revision>"`.
async fn head(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: SaveCaller,
    game: CareerGame,
    Path(xuid): Path<String>,
) -> Result<Response, ApiError> {
    let profile = profile_of(&state.pool, auth.account_id, &xuid, &game).await?;
    let head = PgSaveStore::new(state.pool.clone())
        .head(profile.profile_id)
        .await?;
    let body = head_body(
        &state.pool,
        &platform,
        &profile.game,
        auth.account_id,
        &head,
    )
    .await?;
    let mut response = axum::Json(body).into_response();
    response
        .headers_mut()
        .insert(header::ETAG, etag(head.revision.get()));
    Ok(response)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Page {
    before: Option<u64>,
    limit: Option<u32>,
    /// The career's game (CR-S98), read by [`CareerGame`]; listed so `?game=` is not refused as an
    /// unknown field (found by CR-S105's FM5 suite: `.../revisions?game=` answered 400).
    #[serde(rename = "game")]
    _game: Option<String>,
}

async fn revision_body(
    labels: &mut Labels<'_>,
    r: &RevisionRecord,
) -> Result<RevisionBody, ApiError> {
    Ok(RevisionBody {
        revision: r.revision.get(),
        content_sha256: r.content.hex(),
        bytes: r.blob.size,
        committed_at: views::at(r.committed_at),
        kind: r.kind.as_str().to_owned(),
        device_label: labels.of(r.device).await?,
        summary: summary(&r.summary),
        restored_from: r.restored_from.map(Revision::get),
    })
}

/// `GET .../save/revisions?before=&limit=` → newest first.
async fn revisions(
    State(state): State<AppState>,
    auth: SaveCaller,
    game: CareerGame,
    Path(xuid): Path<String>,
    page: Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<axum::Json<RevisionPage>, ApiError> {
    let Query(page) = page.map_err(|_| invalid("query", "before and limit are integers"))?;
    let limit = page.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(invalid("limit", "limit is 1 to 100"));
    }
    let profile = profile_of(&state.pool, auth.account_id, &xuid, &game).await?;
    let records = PgSaveStore::new(state.pool.clone())
        .history_with_bundles(profile.profile_id, page.before.map(Revision::new), limit)
        .await?;
    let mut labels = Labels::new(&state.pool, auth.account_id);
    let mut items = Vec::with_capacity(records.len());
    for r in &records {
        items.push(revision_body(&mut labels, r).await?);
    }
    let next = match records.last() {
        Some(last) if records.len() == limit as usize && last.revision.get() > 1 => {
            Some(last.revision.get())
        }
        _ => None,
    };
    Ok(axum::Json(RevisionPage { items, next }))
}

fn service<V: Verifier>(
    state: &AppState,
    platform: &Platform,
    verifier: V,
) -> Result<SaveService<PgSaveStore, fi_storage::Blobs, V>, ApiError> {
    service_for(state, platform, verifier, None)
}

/// [`service`] with the Hardcore gates of `game` (the guardrails, OD-62): its economy (FH2's,
/// `fi_gamedb::RULES`; a game without rules checks nothing that needs them) and the deployment's
/// enforcement switch (`FI_GUARD_ENFORCE`).
fn service_for<V: Verifier>(
    state: &AppState,
    platform: &Platform,
    verifier: V,
    game: Option<&str>,
) -> Result<SaveService<PgSaveStore, fi_storage::Blobs, V>, ApiError> {
    let blobs = platform.blobs.clone().ok_or_else(|| {
        tracing::error!("the saves bucket is not configured on this deployment");
        ApiError::new(ErrorCode::SavesUnavailable, "save storage unavailable").retry_after(1)
    })?;
    let policy = save_policy(platform, game);
    Ok(SaveService::with_verifier(
        // OD-52 / CR-S51 again under the line's lock: a lapse or a mode change after the route's
        // check.
        PgSaveStore::new(state.pool.clone()).eligible_only(),
        blobs,
        verifier,
        policy,
    ))
}

/// The save policy of `game` (its economy, `FI_GUARD_ENFORCE`, its bundle format: CR-S105).
fn save_policy(platform: &Platform, game: Option<&str>) -> SavePolicy {
    let mut policy = SavePolicy::default();
    policy.gates.enforce = platform.guard_enforce;
    if game == Some("fh2") {
        policy.gates.rules = *fi_gamedb::RULES;
    }
    if let Some(game) = game {
        policy.format = SaveFormat::of_game(game);
    }
    policy
}

/// The bundle media type of a format (SAVES_API.md §2, CR-S105).
fn media_type(format: SaveFormat) -> &'static str {
    match format {
        SaveFormat::Fh2Save1 => headers::FH2SAVE1_MEDIA_TYPE,
        SaveFormat::Fm5Save1 => headers::FM5SAVE1_MEDIA_TYPE,
    }
}

/// The bundle format a commit's or dry run's `Content-Type` names: `application/x-fh2save1` or,
/// since CR-S105, `application/x-fm5save1`; else 400 `SAVES.INVALID_ARGUMENT {field:
/// "Content-Type"}`. Which one the line takes is checked once the profile is known
/// ([`format_of_line`]).
fn media_format(headers: &HeaderMap) -> Result<SaveFormat, ApiError> {
    let media = text(headers, header::CONTENT_TYPE.as_str())
        .and_then(|v| v.split(';').next())
        .map(str::trim);
    match media {
        Some(headers::FH2SAVE1_MEDIA_TYPE) => Ok(SaveFormat::Fh2Save1),
        Some(headers::FM5SAVE1_MEDIA_TYPE) => Ok(SaveFormat::Fm5Save1),
        _ => Err(invalid("Content-Type", "application/x-fh2save1")),
    }
}

/// The body of a commit or dry run, at most its format's bundle: 413 `REQUEST.TOO_LARGE` past it.
/// Read only once the profile, its line's format, the limits and eligibility passed (CR-S105
/// review 2), with no pooled connection held.
async fn bundle_body(body: Body, format: SaveFormat) -> Result<axum::body::Bytes, ApiError> {
    let max = format.max_bundle();
    let too_large = || {
        ApiError::new(
            ErrorCode::RequestTooLarge,
            match format {
                SaveFormat::Fh2Save1 => "a bundle is at most 384 KiB",
                SaveFormat::Fm5Save1 => "an FM5 bundle is at most 48 MiB",
            },
        )
    };
    let bytes = to_bytes(body, max + 1).await.map_err(|_| too_large())?;
    if bytes.len() > max {
        return Err(too_large());
    }
    Ok(bytes)
}

/// 400 `SAVES.INVALID_ARGUMENT {field: "Content-Type"}` unless `format` is the line's (FM5's
/// `application/x-fm5save1`, every other game's `application/x-fh2save1`; CR-S105).
fn format_of_line(profile: &ProfileRef, format: SaveFormat) -> Result<(), ApiError> {
    let line = SaveFormat::of_game(&profile.game);
    if line != format {
        return Err(invalid(
            "Content-Type",
            match line {
                SaveFormat::Fh2Save1 => "application/x-fh2save1",
                SaveFormat::Fm5Save1 => "application/x-fm5save1",
            },
        ));
    }
    Ok(())
}

/// The profile's last accepted game sync (the profile half of its Hardcore RPC baseline), which
/// a Hardcore upload is compared with; `None` before any (a line seeded before OD-60). Its level is
/// the driver level its XP reaches (CR-S81): a baseline row written before the fix holds the
/// game's in-band step, and the recorded guard input must carry the scale the save is measured on
/// (`fi_saves::gates::sync_with_driver_level` re-derives it again when measuring).
async fn game_sync_in(
    conn: &mut sqlx::PgConnection,
    profile: &ProfileRef,
) -> Result<Option<fi_guard::GameSync>, ApiError> {
    let base = fi_db::integrity::baselines_in(
        conn,
        fi_db::integrity::Subject {
            game: &profile.game,
            profile_id: profile.profile_id.as_uuid(),
            mode: "hardcore",
        },
    )
    .await?;
    Ok(base.profile.map(|b| fi_guard::GameSync {
        credits: b.numbers.credits,
        xp: b.numbers.xp,
        level: fi_gamedb::gates::driver_level(b.numbers.xp),
        hours_since: base.now.since(b.synced_at).max(0) as f64 / 3_600_000.0,
    }))
}

/// Records the assessment of an upload the fact checks refused (since the hybrid Hardcore checks,
/// CLOUD_SAVES.md §7.5, an accepted upload carries none: the nightly pass records its revision's,
/// source `save_nightly`), after the decision on its own connection: no transaction spans the save's network
/// calls. Best effort: a failure is logged and the answer stands (the decision is the commit's).
async fn record_upload_assessment(
    state: &AppState,
    profile: &ProfileRef,
    session: PlaySessionId,
    assessment: &fi_guard::Assessment,
    result: &Result<fi_saves::Committed, SaveError>,
) {
    let refusal = match result {
        Ok(_) => None,
        Err(SaveError::GateRefused { rule, .. }) => Some(*rule),
        Err(_) => return,
    };
    let recorded = async {
        let mut conn = state.pool.acquire().await?;
        fi_db::integrity::record_assessment_in(
            &mut conn,
            fi_db::integrity::Subject {
                game: &profile.game,
                profile_id: profile.profile_id.as_uuid(),
                mode: "hardcore",
            },
            session.as_uuid(),
            fi_db::integrity::AssessmentSource::SaveCommit,
            assessment,
            refusal,
        )
        .await
        .map_err(ApiError::from)
    }
    .await;
    if let Err(e) = recorded {
        tracing::warn!(error = ?e, %session, "the save's assessment was not recorded");
    }
    if assessment.would_block() || refusal.is_some() {
        tracing::info!(
            %session,
            score = assessment.score,
            reasons = ?assessment.reasons.iter().map(|r| r.code()).collect::<Vec<_>>(),
            refused = refusal,
            "a Hardcore save assessed at or above the block score, or refused"
        );
    }
}

/// `GET .../save/revisions/{revision}/bundle` → the bytes, re-hashed before they are served.
async fn bundle(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: SaveCaller,
    game: CareerGame,
    Path((xuid, revision)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let revision: u64 = revision
        .parse()
        .ok()
        .filter(|r| *r > 0)
        .ok_or_else(|| invalid("revision", "a revision is a positive integer"))?;
    let profile = profile_of(&state.pool, auth.account_id, &xuid, &game).await?;
    let (record, bytes) = service(&state, &platform, InlineVerifier)?
        .download(profile.profile_id, Revision::new(revision))
        .await?;
    let mut response = Response::new(Body::from(bytes));
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(media_type(SaveFormat::of_game(&profile.game))),
    );
    if let Ok(v) = HeaderValue::from_str(&headers::repr_digest(&record.blob.sha256.0)) {
        h.insert(headers::REPR_DIGEST, v);
    }
    if let Ok(v) = HeaderValue::from_str(&record.content.hex()) {
        h.insert(headers::FI_CONTENT_SHA256, v);
    }
    h.insert(header::ETAG, etag(record.revision.get()));
    Ok(response)
}

/// 503 `SERVICE.UNAVAILABLE` for a game without an active seed (logged: an operator imports one).
fn no_seed(game: &str) -> ApiError {
    // An operator's to-do, not a request's error: a warning at most once per period per instance.
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);
    let now = fi_domain::UnixMs::now_system().millis();
    let last = LAST.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) >= NO_SEED_LOG_EVERY_MS
        && LAST
            .compare_exchange(
                last,
                now,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    {
        tracing::warn!(
            game,
            "no active Hardcore seed: import one (fi-cli seed-import)"
        );
    }
    ApiError::new(
        ErrorCode::ServiceUnavailable,
        "no Hardcore seed is configured for this game yet",
    )
    .retry_after(60)
}

/// How often an instance logs that a game has no seed.
const NO_SEED_LOG_EVERY_MS: i64 = 5 * 60_000;

/// The active seed's verified bytes, by digest: a seed's object is immutable, so an instance keeps
/// the last one it served (one entry, a few hundred KiB; derived, never shared between requests'
/// decisions) instead of reading and re-hashing it from the bucket on every first launch.
static SEED_BYTES: std::sync::Mutex<Option<(Digest, axum::body::Bytes)>> =
    std::sync::Mutex::new(None);

/// The seed's bytes: the cached copy of `digest`, else read from the bucket, re-hashed, cached.
async fn seed_bytes(platform: &Platform, digest: Digest) -> Result<axum::body::Bytes, ApiError> {
    let cached = SEED_BYTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|(d, _)| *d == digest)
        .map(|(_, b)| b.clone());
    if let Some(bytes) = cached {
        return Ok(bytes);
    }
    let blobs = platform.blobs.clone().ok_or_else(|| {
        tracing::error!("the saves bucket is not configured on this deployment");
        ApiError::new(ErrorCode::SavesUnavailable, "save storage unavailable").retry_after(1)
    })?;
    let bytes = fi_saves::BlobStore::get(&blobs, digest).await?;
    if Digest::of(&bytes) != digest {
        tracing::error!(blob = %digest.hex(), "the seed's object failed its integrity check");
        return Err(ApiError::new(
            ErrorCode::SavesUnavailable,
            "save storage unavailable",
        ));
    }
    let bytes = axum::body::Bytes::from(bytes);
    *SEED_BYTES.lock().unwrap_or_else(|e| e.into_inner()) = Some((digest, bytes.clone()));
    Ok(bytes)
}

/// `POST .../save/seed` (SAVES_API.md §4.8, OD-60): the line's revision 1 from the game's active
/// seed. 201 seeded, 200 already seeded; 412 `SAVES.REVISION_CONFLICT` after later revisions;
/// 403 `SAVES.NOT_ELIGIBLE`; the lease rules' codes; 503 without a seed.
async fn seed(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: SaveCaller,
    game: CareerGame,
    key: IdempotencyKey,
    Path(xuid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let session = text(&headers, headers::FI_SESSION_ID);
    let fence = text(&headers, headers::FI_FENCE);
    let writer = match (session, fence) {
        (None, None) => None,
        (Some(s), Some(f)) => Some((
            Uuid::parse_str(s)
                .ok()
                .and_then(PlaySessionId::from_uuid)
                .ok_or_else(|| invalid("X-Fi-Session-Id", "the play session's id"))?,
            f.parse::<u64>()
                .ok()
                .and_then(FenceToken::new)
                .ok_or_else(|| invalid("X-Fi-Fence", "the session's current fence token"))?,
        )),
        _ => {
            return Err(invalid(
                "X-Fi-Session-Id",
                "X-Fi-Session-Id and X-Fi-Fence come together, or neither",
            ));
        }
    };
    let commit_id = CommitId::from_uuid(key.0).ok_or_else(|| {
        ApiError::invalid("Idempotency-Key", "a UUID Idempotency-Key is required")
    })?;
    let profile = profile_of(&state.pool, auth.account_id, &xuid, &game).await?;
    let device = {
        let mut conn = state.pool.acquire().await?;
        let device = admission::device_of_family_in(&mut conn, auth.account_id, auth.family_id)
            .await?
            .ok_or_else(|| ApiError::new(ErrorCode::AuthTokenInvalid, "invalid access token"))?;
        // OD-52 / CR-S51: only a profile the server keeps a line for has one to seed.
        check_eligible_in(&mut conn, auth.account_id, &profile).await?;
        device
    };
    let mut tx = state.pool.begin().await?;
    let outcome = seeds::seed_line_in(
        &mut tx,
        &seeds::SeedLine {
            profile: profile.profile_id,
            game: &profile.game,
            commit_id,
            writer,
            device: device.device_id,
            device_label: &device.label,
        },
    )
    .await;
    let status = match outcome {
        Ok(seeds::Seeded::Created(record)) => {
            tx.commit().await?;
            tracing::info!(profile = %profile.profile_id, revision = record.revision.get(), "line seeded");
            StatusCode::CREATED
        }
        Ok(seeds::Seeded::Already) => {
            tx.rollback().await?;
            StatusCode::OK
        }
        Err(e) => {
            tx.rollback().await?;
            return Err(match e {
                seeds::SeedError::NoSeed => no_seed(&profile.game),
                seeds::SeedError::HasRevisions { head } => ApiError::new(
                    ErrorCode::SavesRevisionConflict,
                    "the line already has revisions after its start",
                )
                .with_details(serde_json::json!({ "expected": 0, "head": head })),
                seeds::SeedError::Save(e) => e.into(),
                seeds::SeedError::Identity(e) => e.into(),
                seeds::SeedError::Db(e) => e.into(),
                seeds::SeedError::Unreadable => ApiError::unavailable(),
            });
        }
    };
    let head = PgSaveStore::new(state.pool.clone())
        .head(profile.profile_id)
        .await?;
    let body = head_body(
        &state.pool,
        &platform,
        &profile.game,
        auth.account_id,
        &head,
    )
    .await?;
    let mut response = (status, axum::Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::ETAG, etag(head.revision.get()));
    Ok(response)
}

/// `GET /launcher/v1/games/{game}/seeds/hardcore` (SAVES_API.md §4.8, OD-60): the active seed's
/// bundle, verified against its digest before it is first served ([`seed_bytes`]), to any
/// signed-in launcher; `If-None-Match` with its ETag (the bundle's digest) answers 304. 404
/// `GAME.NOT_SERVED {game}` for a game that does not exist, 503 without a seed.
async fn seed_bundle(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    _auth: SaveCaller,
    Path(game): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let unknown = || {
        ApiError::new(ErrorCode::GameNotServed, "no such game")
            .with_details(serde_json::json!({ "game": game }))
    };
    if !(2..=16).contains(&game.len()) || !game.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(unknown());
    }
    let seed = {
        let mut conn = state.pool.acquire().await?;
        let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM game WHERE code = $1)")
            .bind(&game)
            .fetch_one(&mut *conn)
            .await?;
        if !known {
            return Err(unknown());
        }
        seeds::active_in(&mut conn, &game, seeds::HARDCORE)
            .await
            .map_err(|e| match e {
                seeds::SeedError::Db(e) => ApiError::from(e),
                _ => ApiError::unavailable(),
            })?
            .ok_or_else(|| no_seed(&game))?
    };
    let tag = format!("\"{}\"", seed.blob.sha256.hex());
    let etag_value = HeaderValue::from_str(&tag).unwrap_or(HeaderValue::from_static("\"\""));
    let mut response = if text(&headers, header::IF_NONE_MATCH.as_str()) == Some(tag.as_str()) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let bytes = seed_bytes(&platform, seed.blob.sha256).await?;
        let mut response = Response::new(Body::from(bytes));
        let h = response.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(headers::FH2SAVE1_MEDIA_TYPE),
        );
        if let Ok(v) = HeaderValue::from_str(&headers::repr_digest(&seed.blob.sha256.0)) {
            h.insert(headers::REPR_DIGEST, v);
        }
        if let Ok(v) = HeaderValue::from_str(&seed.content.hex()) {
            h.insert(headers::FI_CONTENT_SHA256, v);
        }
        response
    };
    let h = response.headers_mut();
    h.insert(header::ETAG, etag_value);
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok(response)
}

/// Verification inline (the save service's order), then the declared content digest must be the
/// one the server computed.
struct DeclaredContent(Digest);

impl Verifier for DeclaredContent {
    async fn verify(&self, bytes: Arc<[u8]>) -> Result<Verified, SaveError> {
        let verified = InlineVerifier.verify(bytes).await?;
        if verified.content != self.0 {
            return Err(SaveError::DigestMismatch);
        }
        Ok(verified)
    }
}

/// The fresh-start check's PlayerDatabase facts of a bundle (CR-S51, CLOUD_SAVES.md §7.4), read by
/// `fi-playerdb`: `Err` when the bundle or its database does not read (the check fails closed).
/// It unpacks the bundle again (once per line, after verification passed, within the same
/// verification limits).
pub fn playerdb_facts(bytes: &[u8]) -> Result<PlayerDbFacts, PlayerDbUnreadable> {
    let valid = fi_saves::bundle::validate(bytes).map_err(|_| PlayerDbUnreadable)?;
    let database = valid
        .files
        .iter()
        .find(|(name, _)| *name == "data/PlayerDatabase")
        .map(|(_, body)| body.as_slice())
        .ok_or(PlayerDbUnreadable)?;
    let db = fi_playerdb::read(database).map_err(|_| PlayerDbUnreadable)?;
    Ok(facts_of(&db))
}

/// The fresh-start check's counts of a read PlayerDatabase. A NULL `PricePaid` or `BarnFinds.State`
/// counts as non-zero (review L3: an unknown value fails closed).
pub fn facts_of(db: &fi_playerdb::PlayerDb) -> PlayerDbFacts {
    let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    PlayerDbFacts {
        cars: count(db.garage.len()),
        perks: count(db.perks.len()),
        vouchers: count(db.free_cars.len()),
        event_cars: count(db.event_cars_claimed.len()),
        parts_paid: count(
            db.purchased_parts
                .iter()
                .filter(|p| p.price_paid.is_none_or(|v| v != 0))
                .count(),
        ),
        barn_finds: count(
            db.barn_finds
                .iter()
                .filter(|b| b.state.is_none_or(|v| v != 0))
                .count(),
        ),
    }
}

/// `X-Fi-Seed-Source` (CR-S52, §4.10): the source profile, a live profile of the caller's account
/// and of `target`'s game other than `target`. Missing or malformed: 400 `SAVES.INVALID_ARGUMENT
/// {field}`; another account's, or no such profile: 404 `SAVES.PROFILE_NOT_FOUND {field}`.
async fn seed_source_of(
    db: &PgPool,
    account: Uuid,
    target: &ProfileRef,
    headers: &HeaderMap,
) -> Result<ProfileRef, ApiError> {
    let field = "X-Fi-Seed-Source";
    let xuid = text(headers, headers::FI_SEED_SOURCE)
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| invalid(field, "the source profile's decimal XUID"))?;
    let source = profile_of(db, account, xuid, &CareerGame(Some(target.game.clone())))
        .await
        .map_err(|e| e.with_details(serde_json::json!({ "field": field })))?;
    if source.profile_id == target.profile_id || source.game != target.game {
        return Err(invalid(
            field,
            "another profile of the same game than the one it seeds",
        ));
    }
    Ok(source)
}

/// Whether `target` may start from another profile now (CR-S52, §4.10): the server allows it
/// (`FI_HARDCORE_FROM_PROFILE`), the profile is Hardcore and its line's start is `fresh_start`.
async fn seed_allowed_in(
    conn: &mut sqlx::PgConnection,
    platform: &PlatformConfig,
    target: &ProfileRef,
) -> Result<bool, ApiError> {
    Ok(platform.hardcore_from_profile
        && target.mode == fi_domain::ProfileMode::Hardcore.as_str()
        && starts_fresh_in(conn, platform, &target.game).await?)
}

/// 409 `SAVES.HARDCORE_NEEDS_FRESH_CAREER {reason: "source_used"}` when `source` already seeded a
/// live line other than `target`'s (review L7: one source, one seeded line at a time).
async fn source_unused_in(
    conn: &mut sqlx::PgConnection,
    source: &ProfileRef,
    target: &ProfileRef,
) -> Result<(), ApiError> {
    if fi_db::seed_profile::source_in_use_in(
        conn,
        source.profile_id.as_uuid(),
        target.profile_id.as_uuid(),
    )
    .await?
    {
        return Err(
            ApiError::from(SaveError::HardcoreNeedsFreshCareer).with_details(
                serde_json::json!({ "reason": "source_used", "field": "X-Fi-Seed-Source" }),
            ),
        );
    }
    Ok(())
}

/// Counts one seed check or seed commit of `account`: 429 `SAVES.RATE_LIMITED` past 5 a day.
async fn seed_check_allowed(state: &AppState, account: Uuid) -> Result<(), ApiError> {
    if let fi_db::limits::Hit::Limited { retry_after_secs } =
        fi_db::seed_profile::check_allowed(&state.pool, account).await?
    {
        return Err(ApiError::new(
            ErrorCode::SavesRateLimited,
            "too many seed checks today, wait Retry-After",
        )
        .retry_after(retry_after_secs.max(1) as u64));
    }
    Ok(())
}

/// Counts one commit of `profile`: 429 `SAVES.RATE_LIMITED` past 30 in the current clock hour
/// (SAVES_API.md §4.3), every kind included, right after the profile is found and before anything
/// else is read, verified or stored. A replay of a commit that already landed with these very
/// bytes is no new commit.
async fn commit_rate_allowed(
    state: &AppState,
    profile: &ProfileRef,
    commit_id: Uuid,
    declared: &Digest,
) -> Result<(), ApiError> {
    let limit = fi_db::limits::SAVE_COMMITS;
    let profile_id = profile.profile_id.as_uuid();
    let mut conn = state.pool.acquire().await?;
    if fi_db::seed_profile::commit_landed_with_in(&mut conn, profile_id, commit_id, &declared.0)
        .await?
    {
        return Ok(());
    }
    let key = fi_db::limits::profile_key(&limit, profile_id);
    if let fi_db::limits::Hit::Limited { retry_after_secs } =
        fi_db::limits::hit_in(&mut conn, &key, &limit).await?
    {
        return Err(ApiError::new(
            ErrorCode::SavesRateLimited,
            "too many commits this hour, wait Retry-After",
        )
        .retry_after(retry_after_secs.max(1) as u64));
    }
    Ok(())
}

/// One seed check to record (CLOUD_SAVES.md §7.6).
struct SeedCheckLog<'a> {
    game: &'a str,
    source: &'a ProfileRef,
    target: &'a ProfileRef,
    session: Option<Uuid>,
    blob_sha256: [u8; 32],
    assessment: Option<&'a fi_guard::Assessment>,
    hours: f64,
    reasons: &'a [&'a str],
}

/// Records a seed check on its own connection. Best effort for a commit's (the decision is the
/// commit's); the dry run uses [`record_seed_check_in`], which must succeed before it answers.
async fn record_seed_check(state: &AppState, log: SeedCheckLog<'_>) {
    let recorded = async {
        let mut conn = state.pool.acquire().await?;
        record_seed_check_in(&mut conn, &log).await
    }
    .await;
    if let Err(e) = recorded {
        tracing::warn!(error = ?e, "a seed check was not recorded");
    }
}

async fn record_seed_check_in(
    conn: &mut sqlx::PgConnection,
    log: &SeedCheckLog<'_>,
) -> Result<Uuid, ApiError> {
    let id = fi_db::seed_profile::record_check_in(
        conn,
        &fi_db::seed_profile::CheckRecord {
            game: log.game,
            source: log.source.profile_id.as_uuid(),
            source_mode: &log.source.mode,
            target: log.target.profile_id.as_uuid(),
            session: log.session,
            blob_sha256: log.blob_sha256,
            assessment: log.assessment,
            hours: log.hours,
            reasons: log.reasons,
        },
    )
    .await?;
    tracing::info!(
        check = %id,
        accepted = log.reasons.is_empty(),
        reasons = ?log.reasons,
        "a seed check recorded"
    );
    Ok(id)
}

/// `POST .../save/check` (CR-S52, SAVES_API.md §4.10): the dry run of the source profile's save
/// (`X-Fi-Seed-Source`) as this Hardcore line's start. 200 [`SeedCheck`]; nothing is committed or
/// parked; every check is recorded by digest (`integrity_assessment` source `seed_check`) and
/// counted against the account's 5 a day (429 `SAVES.RATE_LIMITED` with `Retry-After`).
async fn seed_check(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: SaveCaller,
    game: CareerGame,
    Path(xuid): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let media = text(&headers, header::CONTENT_TYPE.as_str())
        .and_then(|v| v.split(';').next())
        .map(str::trim);
    if media != Some(headers::FH2SAVE1_MEDIA_TYPE) {
        return Err(invalid("Content-Type", "application/x-fh2save1"));
    }
    let too_large = || ApiError::new(ErrorCode::RequestTooLarge, "a bundle is at most 384 KiB");
    let bytes = to_bytes(body, MAX_BUNDLE + 1)
        .await
        .map_err(|_| too_large())?;
    if bytes.len() > MAX_BUNDLE {
        return Err(too_large());
    }
    let profile = profile_of(&state.pool, auth.account_id, &xuid, &game).await?;
    let source = seed_source_of(&state.pool, auth.account_id, &profile, &headers).await?;
    let head = PgSaveStore::new(state.pool.clone())
        .head(profile.profile_id)
        .await?;
    let hours = {
        let mut conn = state.pool.acquire().await?;
        if profile.mode != fi_domain::ProfileMode::Hardcore.as_str() {
            return Err(invalid("xuid", "a seed starts a Hardcore line"));
        }
        // OD-52 / CR-S51: only a profile the server keeps a line for has one to seed.
        check_eligible_in(&mut conn, auth.account_id, &profile).await?;
        if !seed_allowed_in(&mut conn, &platform, &profile).await? {
            return Err(SaveError::HardcoreNeedsFreshCareer.into());
        }
        source_unused_in(&mut conn, &source, &profile).await?;
        fi_db::seed_profile::play_hours_in(&mut conn, source.profile_id.as_uuid()).await?
    };
    if !head.revision.is_none() {
        return Err(ApiError::new(
            ErrorCode::SavesRevisionConflict,
            "the line already has revisions",
        )
        .with_details(serde_json::json!({ "expected": 0, "head": head.revision.get() })));
    }
    seed_check_allowed(&state, auth.account_id).await?;
    let blob_sha256 = Digest::of(&bytes);
    let policy = save_policy(&platform, Some(&profile.game));
    // FH2SAVE1 only (a Hardcore seed check; CR-S105: `InlineVerifier` also reads FM5SAVE1).
    let (verdict, summary) =
        match fi_saves::bundle::verify(&bytes).map_err(SaveError::BundleInvalid) {
            Ok(verified) => {
                let facts = playerdb_facts(&bytes);
                let v = fi_saves::seed::check(
                    &policy.gates,
                    &verified.summary,
                    facts.as_ref().map_err(|e| *e),
                    hours,
                );
                let summary = v.summary;
                (v, Some(summary))
            }
            Err(SaveError::BundleInvalid(_)) | Err(SaveError::DigestMismatch) => (
                fi_saves::seed::SeedVerdict {
                    reasons: vec!["BUNDLE_INVALID"],
                    assessment: None,
                    summary: fi_saves::SaveSummary::default(),
                },
                None,
            ),
            Err(e) => return Err(e.into()),
        };
    {
        let mut conn = state.pool.acquire().await?;
        record_seed_check_in(
            &mut conn,
            &SeedCheckLog {
                game: &profile.game,
                source: &source,
                target: &profile,
                session: None,
                blob_sha256: blob_sha256.0,
                assessment: verdict.assessment.as_ref(),
                hours: verdict.assessment.as_ref().map_or(hours, |a| a.hours),
                reasons: &verdict.reasons,
            },
        )
        .await?;
    }
    let body = SeedCheck {
        accepted: verdict.accepted(),
        reasons: verdict.reasons.iter().map(|r| (*r).to_owned()).collect(),
        score: verdict.score(),
        hours: verdict.assessment.as_ref().map_or(hours, |a| a.hours),
        blob_sha256: blob_sha256.hex(),
        summary: summary.as_ref().map(self::summary).unwrap_or_default(),
    };
    let mut response = axum::Json(body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// `POST .../save/upload-check` (CR-S71, SAVES_API.md §4.11): the dry run of a player's `upload`
/// (or `resolve_keep_local`) of these bytes on top of `If-Match`, without a lease. The commit's
/// refusals before verification answer as the commit's (but the lease's: none is needed); then
/// the save core's own upload checks ([`SaveService::check_upload`], the commit's very steps)
/// answer 200 [`UploadCheck`]. Nothing is stored, parked, recorded or counted against the line's
/// commits; 10 per account per clock hour (429 `SAVES.RATE_LIMITED` with `Retry-After`).
async fn upload_check(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: SaveCaller,
    game: CareerGame,
    Path(xuid): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let expected = if_match(&headers)?;
    // `X-Fi-Session-Id` and `X-Fi-Fence` are not read: the dry run needs no lease.
    let kind = match text(&headers, headers::FI_COMMIT_KIND) {
        None => CommitKind::Upload,
        Some(k) => CommitKind::parse(k)
            .filter(|k| matches!(k, CommitKind::Upload | CommitKind::ResolveKeepLocal))
            .ok_or_else(|| invalid("X-Fi-Commit-Kind", "upload or resolve_keep_local"))?,
    };
    let declared = text(&headers, headers::REPR_DIGEST)
        .and_then(headers::parse_repr_digest)
        .map(Digest)
        .ok_or_else(|| invalid("Repr-Digest", "sha-256=:<base64>: of the bundle"))?;
    let content: Digest = text(&headers, headers::FI_CONTENT_SHA256)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| invalid("X-Fi-Content-Sha256", "the bundle's content digest (hex)"))?;
    let format = media_format(&headers)?;
    // CR-S105 review 2: the profile, its line's format, the limits and eligibility first (cheap
    // reads); only then the body, at most the line's format's bundle.
    let profile = profile_of(&state.pool, auth.account_id, &xuid, &game).await?;
    format_of_line(&profile, format)?;
    {
        let mut conn = state.pool.acquire().await?;
        // The line's hourly commits, read and not counted: a line the commit would answer 429.
        let commits = fi_db::limits::SAVE_COMMITS;
        let key = fi_db::limits::profile_key(&commits, profile.profile_id.as_uuid());
        if let fi_db::limits::Hit::Limited { retry_after_secs } =
            fi_db::limits::peek_in(&mut conn, &key, &commits).await?
        {
            return Err(ApiError::new(
                ErrorCode::SavesRateLimited,
                "too many commits this hour, wait Retry-After",
            )
            .retry_after(retry_after_secs.max(1) as u64));
        }
        // OD-52 / CR-S51, as the commit: before any verification.
        check_eligible_in(&mut conn, auth.account_id, &profile).await?;
        let limit = fi_db::limits::SAVE_UPLOAD_CHECKS;
        let key = fi_db::limits::account_key(&limit, auth.account_id);
        if let fi_db::limits::Hit::Limited { retry_after_secs } =
            fi_db::limits::hit_in(&mut conn, &key, &limit).await?
        {
            return Err(ApiError::new(
                ErrorCode::SavesRateLimited,
                "too many upload checks this hour, wait Retry-After",
            )
            .retry_after(retry_after_secs.max(1) as u64));
        }
    }
    // No pooled connection is held while the body arrives.
    let bytes = bundle_body(body, format).await?;
    let unused = || ApiError::new(ErrorCode::SavesUnavailable, "save checks unavailable");
    let verdict = service_for(
        &state,
        &platform,
        DeclaredContent(content),
        Some(&profile.game),
    )?
    .check_upload(UploadRequest {
        profile: profile.profile_id,
        // Not read by a dry run (no lease, no replay, no device, no guard input).
        session: PlaySessionId::new(),
        token: FenceToken::new(1).ok_or_else(unused)?,
        expected,
        commit_id: CommitId::from_uuid(Uuid::now_v7()).ok_or_else(unused)?,
        kind,
        device: DeviceId::new(),
        declared_sha256: Some(declared),
        // The body's buffer handed over (reused when unique), not a second copy of up to 48 MiB.
        bytes: Vec::from(bytes),
        game_sync: None,
        fresh_start: None,
        seed_from_profile: None,
    })
    .await?;
    let body = match verdict {
        UploadVerdict::Pass { summary: s } => UploadCheck {
            pass: true,
            code: None,
            rule: None,
            reason: None,
            score: None,
            reasons: Vec::new(),
            blob_sha256: declared.hex(),
            summary: Some(summary(&s)),
        },
        UploadVerdict::Refused {
            code,
            rule,
            reason,
            score,
            reasons,
        } => UploadCheck {
            pass: false,
            code: Some(code.to_owned()),
            rule: rule.map(str::to_owned),
            reason: Some(reason),
            score,
            reasons: reasons.iter().map(|r| (*r).to_owned()).collect(),
            blob_sha256: declared.hex(),
            summary: None,
        },
    };
    // Codes only: never the save's numbers or bytes.
    tracing::info!(
        pass = body.pass,
        code = body.code.as_deref(),
        rule = body.rule.as_deref(),
        "an upload check answered"
    );
    let mut response = axum::Json(body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// `If-Match: "<revision>"` (a bare number is accepted too).
fn if_match(headers: &HeaderMap) -> Result<Revision, ApiError> {
    let v = text(headers, header::IF_MATCH.as_str()).ok_or_else(|| {
        invalid(
            "If-Match",
            "If-Match names the revision the save is based on",
        )
    })?;
    let v = v.strip_prefix("W/").unwrap_or(v);
    let v = v
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(v);
    v.parse()
        .map(Revision::new)
        .map_err(|_| invalid("If-Match", "If-Match is \"<revision>\""))
}

/// `PUT .../save`: commit a bundle on top of `If-Match` (SAVES_API.md §4.3). 201, or 200 with
/// `X-Fi-Replayed: true` for a key that already committed.
#[allow(clippy::too_many_arguments)] // axum extractors, one per request part
async fn commit(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: SaveCaller,
    game: CareerGame,
    key: IdempotencyKey,
    Path(xuid): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let expected = if_match(&headers)?;
    let session = text(&headers, headers::FI_SESSION_ID)
        .and_then(|v| Uuid::parse_str(v).ok())
        .and_then(PlaySessionId::from_uuid)
        .ok_or_else(|| invalid("X-Fi-Session-Id", "the play session's id"))?;
    let token = text(&headers, headers::FI_FENCE)
        .and_then(|v| v.parse::<u64>().ok())
        .and_then(FenceToken::new)
        .ok_or_else(|| invalid("X-Fi-Fence", "the session's current fence token"))?;
    let kind = match text(&headers, headers::FI_COMMIT_KIND) {
        None => CommitKind::Upload,
        Some(k) => CommitKind::parse(k)
            .filter(|k| k.is_player_upload())
            .ok_or_else(|| {
                invalid(
                    "X-Fi-Commit-Kind",
                    "upload, resolve_keep_local, import_offline, fresh_start or seed_from_profile",
                )
            })?,
    };
    // CR-S51 (§4.9): Azimuth's first save is the line's revision 1, on nothing else; so is a seed
    // from a profile (CR-S52, §4.10).
    let first = matches!(kind, CommitKind::FreshStart | CommitKind::SeedFromProfile);
    if first && !expected.is_none() {
        return Err(invalid(
            "If-Match",
            "a fresh start is based on the empty line (If-Match: \"0\")",
        ));
    }
    let declared = text(&headers, headers::REPR_DIGEST)
        .and_then(headers::parse_repr_digest)
        .map(Digest)
        .ok_or_else(|| invalid("Repr-Digest", "sha-256=:<base64>: of the bundle"))?;
    let content: Digest = text(&headers, headers::FI_CONTENT_SHA256)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| invalid("X-Fi-Content-Sha256", "the bundle's content digest (hex)"))?;
    let format = media_format(&headers)?;
    // CR-S105 review 2: the profile, its line's format, the rate limit and eligibility first
    // (cheap reads); only then the body, at most the line's format's bundle.
    let profile = profile_of(&state.pool, auth.account_id, &xuid, &game).await?;
    // CR-S105: an FM5 line takes FM5SAVE1 bundles, every other line FH2SAVE1.
    format_of_line(&profile, format)?;
    commit_rate_allowed(&state, &profile, key.0, &declared).await?;
    let hardcore = profile.mode == fi_domain::ProfileMode::Hardcore.as_str();
    if first && !hardcore {
        return Err(invalid(
            "X-Fi-Commit-Kind",
            "a fresh start is a Hardcore line's first revision",
        ));
    }
    // CR-S52 (§4.10): the source profile, one of the caller's own, other than this one.
    let source = if kind == CommitKind::SeedFromProfile {
        Some(seed_source_of(&state.pool, auth.account_id, &profile, &headers).await?)
    } else {
        None
    };
    let (device, game_sync, fresh_start, seed_from_profile) = {
        let mut conn = state.pool.acquire().await?;
        let device = admission::device_of_family_in(&mut conn, auth.account_id, auth.family_id)
            .await?
            .ok_or_else(|| ApiError::new(ErrorCode::AuthTokenInvalid, "invalid access token"))?;
        // OD-52 / CR-S51, whatever lease the caller holds; before any verification.
        check_eligible_in(&mut conn, auth.account_id, &profile).await?;
        // The line's start (§4.9), read only for a fresh start: every other kind on a Hardcore
        // line at revision 0 is refused whatever the start.
        let fresh_start = if kind == CommitKind::FreshStart
            && starts_fresh_in(&mut conn, &platform, &profile.game).await?
        {
            Some(FreshStartCheck {
                limits: platform.fresh_start,
                facts: playerdb_facts,
            })
        } else {
            None
        };
        // CR-S52: only where the server allows it, on a line whose start is `fresh_start`.
        let seed_from_profile = match &source {
            Some(source) if seed_allowed_in(&mut conn, &platform, &profile).await? => {
                source_unused_in(&mut conn, source, &profile).await?;
                Some(fi_saves::seed::SeedFromProfileCheck {
                    source: source.profile_id,
                    hours: fi_db::seed_profile::play_hours_in(
                        &mut conn,
                        source.profile_id.as_uuid(),
                    )
                    .await?,
                    facts: playerdb_facts,
                })
            }
            _ => None,
        };
        // The guardrails compare the save with the game's last accepted sync (OD-62).
        (
            device,
            game_sync_in(&mut conn, &profile).await?,
            fresh_start,
            seed_from_profile,
        )
    };
    // No pooled connection is held while the body arrives.
    let bytes = bundle_body(body, format).await?;
    let commit_id = CommitId::from_uuid(key.0).ok_or_else(|| {
        ApiError::invalid("Idempotency-Key", "a UUID Idempotency-Key is required")
    })?;
    // Review M1: a seed commit is a check too, counted against the account's 5 a day, so the
    // commit is no unlimited oracle (its refusal says no more than the rule).
    // A replay of a seed commit that already landed is no new check: the save core answers it.
    if seed_from_profile.is_some() {
        let mut conn = state.pool.acquire().await?;
        let landed =
            fi_db::seed_profile::commit_landed_in(&mut conn, profile.profile_id.as_uuid(), key.0)
                .await?;
        drop(conn);
        if !landed {
            seed_check_allowed(&state, auth.account_id).await?;
        }
    }
    let outcome = service_for(
        &state,
        &platform,
        DeclaredContent(content),
        Some(&profile.game),
    )?
    .upload_assessed(UploadRequest {
        profile: profile.profile_id,
        session,
        token,
        expected,
        commit_id,
        kind,
        device: device.device_id,
        declared_sha256: Some(declared),
        // The body's buffer handed over (reused when unique), not a second copy of up to 48 MiB.
        bytes: Vec::from(bytes),
        game_sync,
        fresh_start,
        seed_from_profile,
    })
    .await;
    if let (Some(source), Some(check)) = (&source, seed_from_profile) {
        // Every authoritative seed check is recorded, accepted or refused (CLOUD_SAVES.md §7.6).
        let reasons: Option<Vec<&str>> = match &outcome.result {
            Ok(c) if !c.replayed => Some(Vec::new()),
            Err(SaveError::GateRefused { rule, reasons, .. })
                if *rule == fi_saves::seed::NOT_PLAUSIBLE_SEED =>
            {
                Some(reasons.clone())
            }
            _ => None,
        };
        // Review L5: the same refused bundle sent again (on the same, empty head) is not recorded
        // twice.
        let again = match &reasons {
            Some(r) if !r.is_empty() => {
                let mut conn = state.pool.acquire().await?;
                fi_db::seed_profile::refused_commit_recorded_in(
                    &mut conn,
                    profile.profile_id.as_uuid(),
                    &declared.0,
                )
                .await?
            }
            _ => false,
        };
        if let Some(reasons) = reasons.filter(|_| !again) {
            record_seed_check(
                &state,
                SeedCheckLog {
                    game: &profile.game,
                    source,
                    target: &profile,
                    session: Some(session.as_uuid()),
                    blob_sha256: declared.0,
                    assessment: outcome.assessment.as_ref(),
                    hours: outcome.assessment.as_ref().map_or(check.hours, |a| a.hours),
                    reasons: &reasons,
                },
            )
            .await;
        }
    } else if let Some(a) = &outcome.assessment {
        record_upload_assessment(&state, &profile, session, a, &outcome.result).await;
    }
    // Review M1: a refused seed commit answers only its rule and parked id; the reasons are the
    // dry run's (rate-limited) and the recorded check's.
    let result = match outcome.result {
        Err(SaveError::GateRefused { rule, parked, .. })
            if rule == fi_saves::seed::NOT_PLAUSIBLE_SEED =>
        {
            Err(SaveError::GateRefused {
                reason: "not a plausible seed".into(),
                parked,
                rule,
                score: None,
                reasons: Vec::new(),
            })
        }
        r => r,
    };
    let committed = result?;
    let r = &committed.record;
    let body = CommitResponse {
        revision: r.revision.get(),
        content_sha256: r.content.hex(),
        blob_sha256: r.blob.sha256.hex(),
        committed_at: views::at(r.committed_at),
        summary: summary(&r.summary),
    };
    let status = if committed.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let mut response = (status, axum::Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::ETAG, etag(r.revision.get()));
    if committed.replayed {
        response
            .headers_mut()
            .insert(headers::FI_REPLAYED, HeaderValue::from_static("true"));
    }
    Ok(response)
}

#[cfg(test)]
mod fresh_tests {
    use super::facts_of;
    use fi_playerdb::{BarnFind, PlayerDb, PurchasedPart};

    #[test]
    fn unknown_prices_and_barn_states_count_as_not_fresh() {
        let part = |price_paid| PurchasedPart {
            garage_id: Some(1),
            part_enum: Some(1),
            part_id: Some(1),
            price_paid,
        };
        let barn = |state| BarnFind {
            car_ordinal: 1,
            state,
        };
        let db = PlayerDb {
            purchased_parts: vec![part(Some(0)), part(None), part(Some(-3))],
            barn_finds: vec![barn(Some(0)), barn(None), barn(Some(2))],
            ..PlayerDb::default()
        };
        let f = facts_of(&db);
        assert_eq!((f.parts_paid, f.barn_finds), (2, 2));
        assert_eq!(facts_of(&PlayerDb::default()).parts_paid, 0);
    }
}
