//! The social hub (CORE_API.md §4; OD-25): public basic cards of the server's players for
//! everyone, full profiles and interaction for every signed-in account (OD-129, OD-141: no
//! Supporter gate), comments and reports behind Turnstile, the three-report rule (ADR
//! 0204 §3). Interaction never crosses Hardcore and Noob (`PORTAL.MODE_MISMATCH`).

use crate::guard::ClientIp;
use crate::session::{CoreSession, MaybeSession};
use crate::{Core, cursor_id, next_id, turnstile};
use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use fi_db::accounts::{self, CardRow};
use fi_db::limits::{self, Recorded};
use fi_db::moderation::{
    self, AuditEntry, CommentRow, Lookup, NewComment as NewCommentRow, NewReport as NewReportRow,
    Role,
};
use fi_protocol::ErrorCode;
use fi_protocol::core::{
    Comment, CommentAuthor, CommentList, FullProfile, NewComment, NewReport, PlayerCard,
    PlayerCardList, PlayerProfile, ProgressSummary, ReportAccepted,
};
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, IdempotencyKey};
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

/// Cards and comments per page.
pub const PAGE: i64 = 20;
const PUBLIC_CACHE: &str = "public, max-age=30, s-maxage=30";
const COMMENTS_CACHE: &str = "public, max-age=15, s-maxage=15";
const PRIVATE: &str = "private, no-store";
const COMMENTS_ROUTE: &str = "POST /core/v1/social/comments";
const REPORTS_ROUTE: &str = "POST /core/v1/social/reports";

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::PortalNotFound, "not found")
}

fn key_reused() -> ApiError {
    ApiError::new(
        ErrorCode::RequestIdempotencyKeyReused,
        "this Idempotency-Key was used for another request",
    )
}

/// A card on the wire (never the account id).
pub fn card(c: &CardRow) -> PlayerCard {
    PlayerCard {
        xuid: c.xuid.to_string(),
        gamertag: c.gamertag.clone(),
        game: c.game.clone(),
        mode: c.mode.clone(),
        level: c.level,
        tier: if c.vip { "vip" } else { "free" }.into(),
        club: None,
        online: c.online,
        since: views::at(c.since),
    }
}

fn comment(c: &CommentRow) -> Comment {
    Comment {
        id: c.id.to_string(),
        target: format!("{}:{}", c.target_kind, c.target_id),
        author: CommentAuthor {
            xuid: c.author_xuid.to_string(),
            gamertag: c.author_gamertag.clone(),
        },
        body: c.body.clone(),
        created_at: views::at(c.created_at),
        hidden: false,
    }
}

/// `player:<xuid>` | `club:<id>` | `news:<uuid>` | `comment:<uuid>` with the kinds `allowed`.
fn target<'a>(text: &'a str, allowed: &[&str]) -> Result<(&'a str, &'a str), ApiError> {
    let bad = || {
        ApiError::invalid(
            "target",
            "player:<xuid>, club:<id>, news:<id> or comment:<id>",
        )
    };
    let (kind, id) = text.split_once(':').ok_or_else(bad)?;
    if !allowed.contains(&kind) {
        return Err(bad());
    }
    let ok = match kind {
        "player" => id
            .parse::<u64>()
            .ok()
            .and_then(|x| fi_domain::Xuid::new(x).ok())
            .is_some(),
        "news" | "comment" => Uuid::parse_str(id).is_ok(),
        "club" => {
            (1..=64).contains(&id.len())
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }
        _ => false,
    };
    if ok { Ok((kind, id)) } else { Err(bad()) }
}

fn xuid_param(text: &str) -> Result<u64, ApiError> {
    text.parse::<u64>()
        .ok()
        .filter(|x| fi_domain::Xuid::new(*x).is_ok())
        .ok_or_else(|| ApiError::invalid("xuid", "a decimal XUID"))
}

/// `?game=&q=&cursor=` of the directory.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayersQuery {
    /// Game (required).
    pub game: Option<String>,
    /// A name prefix.
    pub q: Option<String>,
    /// `<lowercase name>.<xuid>` of the last card of the previous page.
    pub cursor: Option<String>,
}

/// `GET /core/v1/social/players?game=fh2&q=<name>&cursor=` → 200 [`PlayerCardList`] (public).
pub async fn players(
    State(state): State<AppState>,
    query: Result<Query<PlayersQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<([(HeaderName, &'static str); 1], Json<PlayerCardList>), ApiError> {
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "game, q and cursor only"))?;
    let game = q
        .game
        .filter(|g| crate::valid_game(g))
        .ok_or_else(|| ApiError::invalid("game", "a game id such as fh2"))?;
    if q.q
        .as_deref()
        .is_some_and(|p| p.len() > 20 || !p.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return Err(ApiError::invalid("q", "up to 20 letters and digits"));
    }
    let after = match q.cursor.as_deref() {
        None => None,
        Some(c) => {
            let (name, xuid) = c
                .rsplit_once('.')
                .and_then(|(n, x)| Some((n, x.parse::<u64>().ok()?)))
                .filter(|(n, _)| {
                    n.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                })
                .ok_or_else(|| ApiError::invalid("cursor", "the next value of a previous page"))?;
            Some((name.to_owned(), xuid))
        }
    };
    let mut conn = state.pool.acquire().await?;
    let rows = accounts::directory_in(
        &mut conn,
        &game,
        q.q.as_deref(),
        after.as_ref().map(|(n, x)| (n.as_str(), *x)),
        PAGE,
    )
    .await?;
    let next = (rows.len() as i64 == PAGE)
        .then(|| rows.last())
        .flatten()
        .map(|c| {
            let name = c
                .gamertag
                .split('#')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            format!("{name}.{}", c.xuid)
        });
    Ok((
        [(CACHE_CONTROL, PUBLIC_CACHE)],
        Json(PlayerCardList {
            items: rows.iter().map(card).collect(),
            next,
        }),
    ))
}

/// `GET /core/v1/social/players/{xuid}` → 200 [`PlayerCard`] for everyone, [`PlayerProfile`]
/// for any signed-in viewer (OD-129; `Cache-Control: private` then). `PORTAL.NOT_FOUND` for unknown,
/// unlisted, hidden or deleted profiles alike.
pub async fn player(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    Path(xuid): Path<String>,
) -> Result<Response, ApiError> {
    let xuid = xuid_param(&xuid)?;
    let mut conn = state.pool.acquire().await?;
    let row = accounts::card_in(&mut conn, xuid)
        .await?
        .ok_or_else(not_found)?;
    // OD-129: every signed-in viewer sees the full card (it was a Supporter's); anonymous
    // readers keep the public, cached one.
    if session.is_none() {
        return Ok(([(CACHE_CONTROL, PUBLIC_CACHE)], Json(card(&row))).into_response());
    }
    let progress = accounts::progress_in(&mut conn, row.profile_id)
        .await?
        .map(|p| ProgressSummary {
            level: p.level,
            xp: p.xp,
            credits: p.credits,
            cars: p.cars,
            races: p.races,
            play_seconds: p.play_seconds,
        });
    Ok((
        [(CACHE_CONTROL, PRIVATE)],
        Json(PlayerProfile {
            card: card(&row),
            full: FullProfile {
                progress,
                boards: None,
                creations: None,
                friends: None,
            },
        }),
    )
        .into_response())
}

/// `?target=&cursor=` of a comment list.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentsQuery {
    /// `player:<xuid>` | `club:<id>` | `news:<id>`.
    pub target: Option<String>,
    /// The `next` of the previous page.
    pub cursor: Option<String>,
}

/// `GET /core/v1/social/comments?target=&cursor=` → 200 [`CommentList`] (public).
pub async fn comments(
    State(state): State<AppState>,
    query: Result<Query<CommentsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<([(HeaderName, &'static str); 1], Json<CommentList>), ApiError> {
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "target and cursor only"))?;
    let text = q
        .target
        .ok_or_else(|| ApiError::invalid("target", "a target is required"))?;
    let (kind, id) = target(&text, &["player", "club", "news"])?;
    let before = cursor_id(q.cursor.as_deref())?;
    let mut conn = state.pool.acquire().await?;
    let rows = moderation::comments_in(&mut conn, kind, id, before, PAGE).await?;
    Ok((
        [(CACHE_CONTROL, COMMENTS_CACHE)],
        Json(CommentList {
            next: next_id(&rows, PAGE, |c| c.id),
            items: rows.iter().map(comment).collect(),
        }),
    ))
}

fn plain_text(body: &str, max: usize) -> bool {
    !body.trim().is_empty()
        && body.len() <= max
        && !body.chars().any(|c| c.is_control() && c != '\n')
}

/// `POST /core/v1/social/comments {target, body}` (session + `X-Fi-Challenge`; every account, OD-141) →
/// 201 [`Comment`]; 10 per minute per account; the author is the caller's oldest live profile of
/// the target's game and mode (`PORTAL.MODE_MISMATCH` without one). Idempotent by key.
pub async fn post_comment(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    ip: Option<Extension<ClientIp>>,
    session: CoreSession,
    request_headers: HeaderMap,
    key: IdempotencyKey,
    Body(req): Body<NewComment>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let (kind, id) = target(&req.target, &["player", "club", "news"])?;
    if !plain_text(&req.body, 1024) {
        return Err(ApiError::invalid("body", "1 byte to 1 KiB of plain text"));
    }
    core.account_write(session.account_id)?;
    core.comment(session.account_id)?;
    turnstile::require(
        &core.turnstile,
        &state.pool,
        &request_headers,
        ip.map(|Extension(ClientIp(ip))| ip),
    )
    .await?;
    let scope = format!("account:{}", session.account_id);
    let idem = key.of(COMMENTS_ROUTE, &req);
    let mut tx = state.pool.begin().await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(r) => {
            return Ok((StatusCode::CREATED, Json(r.response.unwrap_or_default())));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    // The target's game and mode; `None` mode: any mode of that game may comment.
    let (game, mode): (String, Option<String>) = match kind {
        "player" => {
            let xuid = id.parse::<u64>().map_err(|_| not_found())?;
            let c = accounts::card_in(&mut tx, xuid)
                .await?
                .ok_or_else(not_found)?;
            (c.game, Some(c.mode))
        }
        "news" => {
            let n =
                fi_db::notices::notice_in(&mut tx, Uuid::parse_str(id).map_err(|_| not_found())?)
                    .await?
                    .ok_or_else(not_found)?;
            match n.game {
                Some(g) => (g, None),
                None => {
                    // A platform-wide notice: the caller's first game.
                    let first = accounts::profiles_in(&mut tx, session.account_id)
                        .await?
                        .into_iter()
                        .find(|p| p.status == "active")
                        .ok_or_else(|| {
                            ApiError::new(
                                ErrorCode::PortalModeMismatch,
                                "no profile to comment with",
                            )
                        })?;
                    (first.game, None)
                }
            }
        }
        // Clubs come with POR-M5.
        _ => return Err(not_found()),
    };
    let author = accounts::profiles_in(&mut tx, session.account_id)
        .await?
        .into_iter()
        .filter(|p| p.status == "active" && p.game == game)
        .find(|p| mode.as_deref().is_none_or(|m| m == p.mode))
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::PortalModeMismatch,
                "comments stay within a game and mode (Hardcore or Azimuth)",
            )
        })?;
    let row = moderation::create_comment_in(
        &mut tx,
        &NewCommentRow {
            game: &game,
            target_kind: kind,
            target_id: id,
            author_account_id: session.account_id,
            author_profile_id: author.profile_id,
            body: &req.body,
        },
    )
    .await?;
    let body = serde_json::to_value(comment(&row)).map_err(|_| ApiError::unavailable())?;
    limits::record_with_response_in(&mut tx, &scope, &idem, 201, Some(row.id), Some(&body)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// `DELETE /core/v1/social/comments/{id}` → 204: by its author, or by a moderator (audited,
/// one `admin_action` row). `PORTAL.NOT_FOUND` for unknown or deleted comments and for anybody
/// else.
pub async fn delete_comment(
    State(state): State<AppState>,
    session: CoreSession,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = Uuid::parse_str(&id).map_err(|_| not_found())?;
    let mut tx = state.pool.begin().await?;
    let row = moderation::comment_in(&mut tx, id)
        .await?
        .ok_or_else(not_found)?;
    let own = row.author_account_id == session.account_id;
    if !own && session.role < Role::Moderator {
        return Err(not_found());
    }
    moderation::delete_comment_in(&mut tx, id).await?;
    if !own {
        let target = format!("comment:{id}");
        moderation::audit_in(
            &mut tx,
            &session.actor(),
            &AuditEntry {
                action: "comment.delete",
                account_id: Some(row.author_account_id),
                target: &target,
                reason: None,
                detail: json!({}),
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /core/v1/social/reports {target, reason, text?}` (session + `X-Fi-Challenge`) → 202
/// `{id}`; a repeat by the same reporter counts once; the third distinct reporter hides the
/// target pending review. Reporting oneself is `REQUEST.INVALID`. Idempotent by key.
pub async fn post_report(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    ip: Option<Extension<ClientIp>>,
    session: CoreSession,
    request_headers: HeaderMap,
    key: IdempotencyKey,
    Body(req): Body<NewReport>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let (kind, id) = target(&req.target, &["player", "comment"])?;
    if !matches!(req.reason.as_str(), "cheating" | "abuse" | "other") {
        return Err(ApiError::invalid("reason", "cheating, abuse or other"));
    }
    if req
        .text
        .as_deref()
        .is_some_and(|t| !plain_text(t, 1000) || t.chars().count() > 1000)
    {
        return Err(ApiError::invalid(
            "text",
            "up to 1000 characters of plain text",
        ));
    }
    core.account_write(session.account_id)?;
    turnstile::require(
        &core.turnstile,
        &state.pool,
        &request_headers,
        ip.map(|Extension(ClientIp(ip))| ip),
    )
    .await?;
    let scope = format!("account:{}", session.account_id);
    let idem = key.of(REPORTS_ROUTE, &req);
    let mut tx = state.pool.begin().await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(r) => {
            return Ok((StatusCode::ACCEPTED, Json(r.response.unwrap_or_default())));
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    // The target is stored in its canonical form (the XUID in decimal, the comment id
    // hyphenated), so one target is one key for the repeat, the dedupe and the three-report rule,
    // whoever spelled it and whichever route filed it (`POST /launcher/v1/reports` too).
    let (target_account, target_id) = match kind {
        "player" => {
            let xuid = id.parse::<u64>().map_err(|_| not_found())?;
            let account = moderation::lookup_in(&mut tx, &Lookup::Xuid(xuid))
                .await?
                .first()
                .map(|r| r.account_id)
                .ok_or_else(not_found)?;
            (account, xuid.to_string())
        }
        _ => {
            let comment = Uuid::parse_str(id).map_err(|_| not_found())?;
            let account = moderation::comment_in(&mut tx, comment)
                .await?
                .ok_or_else(not_found)?
                .author_account_id;
            (account, comment.to_string())
        }
    };
    if target_account == session.account_id {
        return Err(ApiError::invalid("target", "one's own profile or comment"));
    }
    let reported = moderation::report_in(
        &mut tx,
        &NewReportRow {
            reporter: session.account_id,
            target_kind: kind,
            target_id: &target_id,
            target_account: Some(target_account),
            reason: &req.reason,
            text: req.text.as_deref(),
            source: "web",
            world_id: None,
        },
    )
    .await?;
    let body = serde_json::to_value(ReportAccepted {
        id: reported.id.to_string(),
    })
    .map_err(|_| ApiError::unavailable())?;
    limits::record_with_response_in(&mut tx, &scope, &idem, 202, Some(reported.id), Some(&body))
        .await?;
    tx.commit().await?;
    if reported.hidden {
        tracing::info!(
            kind,
            "a target reached the report threshold and is hidden pending review"
        );
    }
    Ok((StatusCode::ACCEPTED, Json(body)))
}
