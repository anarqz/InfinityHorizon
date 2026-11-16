//! Friends (CORE_API.md §4.1; SER-M3 step 7, OD-48 item 2, OD-50; OD-55: friendships belong to
//! the account): the friend graph's routes over [`fi_db::friends`], one implementation mounted
//! twice: the web's `/core/v1/me/friends*` (any session) and the launcher's alias
//! `/launcher/v1/friends*` (access token only), which the overlay's Supporter sidebar reaches
//! through the launcher. No profile is named: the caller is the session's account, whatever
//! profile it plays. The list carries each friend's presence (OD-56: tier, the profile it plays,
//! in a world or in the game, its invite to the caller); `?asXuid=` names the caller's current
//! profile, for whom `joinable` is answered.
//!
//! Every route: any account (OD-129, OD-141: every player has friends; `PORTAL.VIP_REQUIRED` is
//! never answered, its code stays in fi-protocol for older clients), not pending deletion
//! (`ACCOUNTS.DELETION_PENDING`). A friend is named by `{friendId}`: its handle (a row's `id`) or
//! any XUID of its profiles. Every write takes `Idempotency-Key` (a replay answers the first
//! outcome) and the per-account write bucket; a request also counts on the per-account Postgres
//! limit ([`fi_db::friends::REQUESTS`]) before the gamertag is looked up, so unknown gamertags
//! count too. A request holds one pooled connection: the checks and the limit run on it, then its
//! transaction.

use crate::Core;
use crate::me::PRIVATE;
use crate::session::CoreSession;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use fi_db::friends::{
    self, Accepted, Asked, Caller, FriendRow, Full, OwnProfile, ProfileCard, RequestRow, Side,
};
use fi_db::limits::{self, Hit, Idempotency, Recorded};
use fi_domain::GamertagInput;
use fi_protocol::ErrorCode;
use fi_protocol::core::{
    Friend, FriendAdd, FriendAddResult, FriendDiscord, FriendDoing, FriendFavorite, FriendInvite,
    FriendList, FriendPlaying, FriendProfile, FriendRequest,
};
use fi_protocol::routes::{core as cr, launcher as lr};
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, IdempotencyKey, LauncherAuth};
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, Postgres, Transaction};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// The capability older clients knew from `PORTAL.VIP_REQUIRED` (no longer answered, OD-141).
pub const CAPABILITY: &str = "social.friends";

/// Both mountings of the friend routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(cr::ME_FRIENDS, get(web_list).post(web_add))
        .route(cr::ME_FRIEND, delete(web_remove))
        .route(cr::ME_FRIEND_ACCEPT, post(web_accept))
        .route(cr::ME_FRIEND_DECLINE, post(web_decline))
        .route(cr::ME_FRIEND_FAVORITE, put(web_favorite))
        .route(lr::FRIENDS, get(launcher_list).post(launcher_add))
        .route(lr::FRIEND, delete(launcher_remove))
        .route(lr::FRIEND_ACCEPT, post(launcher_accept))
        .route(lr::FRIEND_DECLINE, post(launcher_decline))
        .route(lr::FRIEND_FAVORITE, put(launcher_favorite))
}

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::PortalNotFound, "not found")
}

fn key_reused() -> ApiError {
    ApiError::new(
        ErrorCode::RequestIdempotencyKeyReused,
        "this Idempotency-Key was used for another request",
    )
}

/// `SOCIAL.LIMIT_REACHED {limit, max, side}`.
fn full(f: Full) -> ApiError {
    let (limit, max, side) = match f {
        Full::Friends(side) => ("friends", friends::MAX_FRIENDS, side),
        Full::Outgoing => ("outgoing", friends::MAX_OUTGOING, Side::Caller),
    };
    let side = match side {
        Side::Caller => "caller",
        Side::Other => "other",
    };
    ApiError::new(ErrorCode::SocialLimitReached, "a friends limit is reached")
        .with_details(json!({"limit": limit, "max": max, "side": side}))
}

/// `{friendId}`: a decimal XUID (a handle or any XUID of the friend's profiles).
fn friend_id(text: &str) -> Result<u64, ApiError> {
    text.parse::<u64>()
        .ok()
        .filter(|x| fi_domain::Xuid::new(*x).is_ok())
        .ok_or_else(|| ApiError::invalid("friendId", "a decimal XUID"))
}

/// A profile on the wire; `site` is the play dashboard's origin (`FI_PUBLIC_CORE_URL`: the site
/// shares the platform's public host, DASHBOARD_API.md §1), which `profileUrl` starts with
/// (CR-S102), only for a profile the website lists.
fn card(c: &ProfileCard, site: &str) -> FriendProfile {
    FriendProfile {
        xuid: c.xuid.to_string(),
        gamertag: c.gamertag.clone(),
        game: c.game.clone(),
        mode: c.mode.clone(),
        profile_url: c
            .web_listed
            .then(|| profile_url(site, &c.game, &c.mode, &c.gamertag)),
    }
}

/// `{site}/{game}/{mode}/profiles/{name}` (CORE_API.md §4.2a), every byte of the name but ASCII
/// letters and digits percent-encoded (plain names are letters and digits, OD-120).
pub fn profile_url(site: &str, game: &str, mode: &str, name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{site}/{game}/{mode}/profiles/{encoded}")
}

/// A friend row; `joinable` (with `?asXuid=`) answers for the world of an `in_world` friend.
fn friend_with(f: &FriendRow, joinable: Option<&HashMap<Uuid, bool>>, site: &str) -> Friend {
    Friend {
        id: f.handle.to_string(),
        profiles: f.profiles.iter().map(|c| card(c, site)).collect(),
        favorite: f.favorite,
        online: f.online,
        since: views::at(f.since),
        tier: if f.vip { "vip" } else { "free" }.to_owned(),
        playing: f.playing.as_ref().map(|p| FriendPlaying {
            xuid: p.xuid.to_string(),
            gamertag: p.gamertag.clone(),
            game: p.game.clone(),
            mode: p.mode.clone(),
            state: p.state.as_str().to_owned(),
            world_id: p.world_id.map(|w| w.to_string()),
            joinable: p
                .world_id
                .and_then(|w| joinable.map(|j| j.get(&w).copied().unwrap_or(false))),
            official_activity: p.official_activity.clone(),
            doing: p.doing.as_ref().map(|d| FriendDoing {
                activity: d.activity.clone(),
                private: d.private,
                racing: d.racing,
            }),
        }),
        invite: f.invite.map(|i| FriendInvite {
            id: i.id.to_string(),
            world_id: i.world_id.to_string(),
            expires_at: views::at(i.expires_at),
        }),
        account_since: Some(views::at(f.account_since)),
        discord: f.discord.as_ref().map(|d| FriendDiscord {
            username: d.username.clone(),
            global_name: d.global_name.clone(),
            avatar_url: crate::discord::avatar_url(d.user_id, d.avatar.as_deref()),
        }),
    }
}

fn friend(f: &FriendRow, site: &str) -> Friend {
    friend_with(f, None, site)
}

/// `?asXuid=`: the caller's current profile, which `joinable` is computed for (CORE_API.md §4.1).
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AsQuery {
    /// A live profile of the caller's account (decimal XUID).
    pub as_xuid: Option<String>,
}

/// The query of a presence read: `None` without `asXuid`; 400 `REQUEST.INVALID {field: "query"}`
/// for any other parameter.
pub fn as_query(
    q: Result<Query<AsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Option<u64>, ApiError> {
    let Query(q) = q.map_err(|_| ApiError::invalid("query", "asXuid only"))?;
    q.as_xuid
        .map(|x| {
            x.parse::<u64>()
                .ok()
                .filter(|v| fi_domain::Xuid::new(*v).is_ok())
                .ok_or_else(|| ApiError::invalid("asXuid", "a live profile of your account"))
        })
        .transpose()
}

/// The live profile `xuid` of the caller's account, else 400 `REQUEST.INVALID {field: "asXuid"}`.
pub async fn as_profile(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
) -> Result<OwnProfile, ApiError> {
    friends::own_profile_in(conn, account_id, xuid)
        .await?
        .ok_or_else(|| ApiError::invalid("asXuid", "a live profile of your account"))
}

fn request(r: &RequestRow, site: &str) -> FriendRequest {
    FriendRequest {
        id: r.id.to_string(),
        profiles: r.profiles.iter().map(|c| card(c, site)).collect(),
        since: views::at(r.since),
    }
}

fn to_value<T: serde::Serialize>(body: &T) -> Result<Value, ApiError> {
    serde_json::to_value(body).map_err(|_| ApiError::unavailable())
}

fn answer(status: StatusCode, body: Option<Value>) -> Response {
    match body {
        Some(b) => (status, Json(b)).into_response(),
        None => status.into_response(),
    }
}

/// The caller's account on the request's connection (every account's since OD-129).
pub async fn open(conn: &mut PgConnection, account_id: Uuid) -> Result<Caller, ApiError> {
    let caller = friends::caller_in(conn, account_id)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    if !caller.active {
        return Err(ApiError::new(
            ErrorCode::AccountsDeletionPending,
            "the account is pending deletion",
        ));
    }
    Ok(caller)
}

/// A write in progress: its transaction and idempotency identity.
struct Write<'c> {
    tx: Transaction<'c, Postgres>,
    scope: String,
    idem: Idempotency,
}

/// Opens the write's transaction on `conn`; `Err(response)` inside replays the first outcome.
async fn begin<'c>(
    conn: &'c mut PgConnection,
    account_id: Uuid,
    idem: Idempotency,
) -> Result<Result<Write<'c>, Response>, ApiError> {
    let scope = format!("account:{account_id}");
    let mut tx = conn.begin().await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(r) => Ok(Err(answer(
            StatusCode::from_u16(r.status as u16).unwrap_or(StatusCode::OK),
            r.response,
        ))),
        Recorded::Different => Err(key_reused()),
        Recorded::None => Ok(Ok(Write { tx, scope, idem })),
    }
}

impl Write<'_> {
    /// Records the outcome and commits.
    async fn finish(
        mut self,
        status: StatusCode,
        body: Option<Value>,
    ) -> Result<Response, ApiError> {
        limits::record_with_response_in(
            &mut self.tx,
            &self.scope,
            &self.idem,
            status.as_u16() as i16,
            None,
            body.as_ref(),
        )
        .await?;
        self.tx.commit().await?;
        Ok(answer(status, body))
    }
}

macro_rules! fresh_or_replay {
    ($begun:expr) => {
        match $begun? {
            Ok(w) => w,
            Err(replay) => return Ok(replay),
        }
    };
}

/// What an addressed write does with the friend.
#[derive(Clone, Copy)]
enum Act {
    Accept,
    Decline,
    Remove,
    Favorite(bool),
}

impl Act {
    fn route(self) -> &'static str {
        match self {
            Act::Accept => "POST friends/accept",
            Act::Decline => "POST friends/decline",
            Act::Remove => "DELETE friends",
            Act::Favorite(_) => "PUT friends/favorite",
        }
    }
}

// The operations, shared by both mountings ---------------------------------------------------

async fn list_for(
    state: &AppState,
    core: &Core,
    account_id: Uuid,
    as_xuid: Option<u64>,
) -> Result<Response, ApiError> {
    core.account_read("friends", account_id)?;
    let mut conn = state.pool.acquire().await?;
    let caller = open(&mut conn, account_id).await?;
    let me = match as_xuid {
        Some(x) => Some(as_profile(&mut conn, caller.account_id, x).await?),
        None => None,
    };
    let lists = friends::lists_in(&mut conn, caller.account_id).await?;
    let site = core.config.public_origin.as_str();
    let joinable = match &me {
        None => None,
        Some(me) => {
            let worlds: Vec<Uuid> = lists
                .friends
                .iter()
                .filter_map(|f| f.playing.as_ref().and_then(|p| p.world_id))
                .collect();
            Some(friends::joinable_in(&mut conn, caller.account_id, me, &worlds).await?)
        }
    };
    Ok((
        [(CACHE_CONTROL, PRIVATE)],
        Json(FriendList {
            friends: lists
                .friends
                .iter()
                .map(|f| friend_with(f, joinable.as_ref(), site))
                .collect(),
            incoming: lists.incoming.iter().map(|r| request(r, site)).collect(),
            outgoing: lists.outgoing.iter().map(|r| request(r, site)).collect(),
        }),
    )
        .into_response())
}

/// The player a friend request names.
enum Named {
    Gamertag(GamertagInput),
    Xuid(u64),
}

async fn add_for(
    state: &AppState,
    core: &Core,
    account_id: Uuid,
    key: &IdempotencyKey,
    req: &FriendAdd,
) -> Result<Response, ApiError> {
    if !crate::valid_game(&req.game) {
        return Err(ApiError::invalid("game", "a game id such as fh2"));
    }
    // A gamertag or a XUID (the game's own player menus, OD-193), exactly one.
    let named = match (&req.gamertag, &req.xuid) {
        (Some(gamertag), None) => {
            // The plain name (OD-120), or a legacy `Name#12345` during the transition.
            let tag: GamertagInput = gamertag.parse().map_err(|_| {
                ApiError::new(
                    ErrorCode::AccountsInvalidGamertag,
                    "a gamertag is a name of 1 to 20 letters or digits",
                )
                .with_details(json!({"field": "gamertag"}))
            })?;
            Named::Gamertag(tag)
        }
        (None, Some(xuid)) => Named::Xuid(
            xuid.parse::<u64>()
                .ok()
                .filter(|x| fi_domain::Xuid::new(*x).is_ok())
                .ok_or_else(|| ApiError::invalid("xuid", "a decimal XUID"))?,
        ),
        _ => {
            return Err(ApiError::invalid(
                "gamertag",
                "exactly one of gamertag and xuid",
            ));
        }
    };
    core.account_write(account_id)?;
    let mut conn = state.pool.acquire().await?;
    let caller = open(&mut conn, account_id).await?;
    // Counted in its own statement before the lookup: unknown gamertags count too.
    let subject = format!("account:{account_id}");
    let rate_key = state
        .id()
        .pepper()?
        .rate_limit_key(friends::REQUESTS.name, &subject);
    if let Hit::Limited { retry_after_secs } =
        limits::hit_in(&mut conn, &rate_key, &friends::REQUESTS).await?
    {
        return Err(ApiError::new(
            ErrorCode::RequestRateLimited,
            "too many friend requests, retry later",
        )
        .retry_after(retry_after_secs.max(1) as u64));
    }
    let idem = match &named {
        Named::Gamertag(tag) => {
            let canonical = match tag {
                GamertagInput::Plain(name) => name.clone(),
                GamertagInput::Legacy(legacy) => legacy.to_string(),
            };
            key.of(
                "POST friends",
                &json!({"game": req.game, "gamertag": canonical}),
            )
        }
        Named::Xuid(xuid) => key.of(
            "POST friends",
            &json!({"game": req.game, "xuid": xuid.to_string()}),
        ),
    };
    let mut w = fresh_or_replay!(begin(&mut conn, account_id, idem).await);
    let target = match &named {
        Named::Gamertag(GamertagInput::Plain(name)) => {
            friends::find_by_gamertag_in(&mut w.tx, &req.game, name, None).await?
        }
        Named::Gamertag(GamertagInput::Legacy(legacy)) => {
            friends::find_by_gamertag_in(
                &mut w.tx,
                &req.game,
                legacy.name(),
                Some(legacy.discriminator()),
            )
            .await?
        }
        Named::Xuid(xuid) => friends::find_by_xuid_in(&mut w.tx, &req.game, *xuid).await?,
    }
    .ok_or_else(not_found)?;
    let body = match friends::request_in(&mut w.tx, &caller, target).await? {
        Asked::Requested(r) => FriendAddResult::Requested {
            request: request(&r, &core.config.public_origin),
        },
        Asked::Friends(f) => FriendAddResult::Friends {
            friend: friend(&f, &core.config.public_origin),
        },
        Asked::Own => {
            let field = match named {
                Named::Gamertag(_) => "gamertag",
                Named::Xuid(_) => "xuid",
            };
            return Err(ApiError::invalid(field, "a profile of your own account"));
        }
        Asked::NoProfile => {
            return Err(ApiError::new(
                ErrorCode::PortalConflict,
                "create a profile first: the other player sees your profiles",
            )
            .with_details(json!({"reason": "no_profile"})));
        }
        Asked::Full(f) => return Err(full(f)),
        Asked::Gone => return Err(not_found()),
    };
    let body = to_value(&body)?;
    w.finish(StatusCode::OK, Some(body)).await
}

/// Accept, decline, remove or favorite the friend named by `id`.
async fn act_for(
    state: &AppState,
    core: &Core,
    account_id: Uuid,
    key: &IdempotencyKey,
    id: &str,
    act: Act,
) -> Result<Response, ApiError> {
    let xuid = friend_id(id)?;
    core.account_write(account_id)?;
    let mut conn = state.pool.acquire().await?;
    let caller = open(&mut conn, account_id).await?;
    let body = match act {
        Act::Favorite(favorite) => json!({"friend": xuid.to_string(), "favorite": favorite}),
        _ => json!({"friend": xuid.to_string()}),
    };
    let idem = key.of(act.route(), &body);
    let mut w = fresh_or_replay!(begin(&mut conn, account_id, idem).await);
    let other = friends::account_of_xuid_in(&mut w.tx, xuid).await?;
    match (act, other) {
        (Act::Accept, Some(other)) => match friends::accept_in(&mut w.tx, &caller, other).await? {
            Accepted::Friends(f) => {
                let body = to_value(&friend(&f, &core.config.public_origin))?;
                w.finish(StatusCode::OK, Some(body)).await
            }
            Accepted::NotFound => Err(not_found()),
            Accepted::Full(f) => Err(full(f)),
        },
        (Act::Favorite(favorite), Some(other)) => {
            let row = friends::set_favorite_in(&mut w.tx, &caller, other, favorite)
                .await?
                .ok_or_else(not_found)?;
            let body = to_value(&friend(&row, &core.config.public_origin))?;
            w.finish(StatusCode::OK, Some(body)).await
        }
        (Act::Accept | Act::Favorite(_), None) => Err(not_found()),
        (Act::Decline | Act::Remove, other) => {
            if let Some(other) = other {
                if matches!(act, Act::Remove) {
                    friends::remove_in(&mut w.tx, &caller, other).await?;
                } else {
                    friends::decline_in(&mut w.tx, &caller, other).await?;
                }
            }
            w.finish(StatusCode::NO_CONTENT, None).await
        }
    }
}

// The web's mounting: any session ----------------------------------------------------------------

/// `GET /core/v1/me/friends[?asXuid=]` → 200 [`FriendList`].
pub async fn web_list(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
    q: Result<Query<AsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let as_xuid = as_query(q)?;
    list_for(&state, &core, session.account_id, as_xuid).await
}

/// `POST /core/v1/me/friends {game, gamertag | xuid}` → 200 [`FriendAddResult`].
pub async fn web_add(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<FriendAdd>,
) -> Result<Response, ApiError> {
    add_for(&state, &core, session.account_id, &key, &req).await
}

/// `POST /core/v1/me/friends/{friendId}/accept` → 200 [`Friend`].
pub async fn web_accept(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    act_for(&state, &core, session.account_id, &key, &id, Act::Accept).await
}

/// `POST /core/v1/me/friends/{friendId}/decline` → 204.
pub async fn web_decline(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    act_for(&state, &core, session.account_id, &key, &id, Act::Decline).await
}

/// `PUT /core/v1/me/friends/{friendId}/favorite {favorite}` → 200 [`Friend`].
pub async fn web_favorite(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
    Body(req): Body<FriendFavorite>,
) -> Result<Response, ApiError> {
    let act = Act::Favorite(req.favorite);
    act_for(&state, &core, session.account_id, &key, &id, act).await
}

/// `DELETE /core/v1/me/friends/{friendId}` → 204.
pub async fn web_remove(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    act_for(&state, &core, session.account_id, &key, &id, Act::Remove).await
}

// The launcher's alias: the access token -----------------------------------------------------

/// `GET /launcher/v1/friends[?asXuid=]` → 200 [`FriendList`].
pub async fn launcher_list(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    q: Result<Query<AsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let as_xuid = as_query(q)?;
    list_for(&state, &core, auth.account_id, as_xuid).await
}

/// `POST /launcher/v1/friends {game, gamertag | xuid}` → 200 [`FriendAddResult`].
pub async fn launcher_add(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Body(req): Body<FriendAdd>,
) -> Result<Response, ApiError> {
    add_for(&state, &core, auth.account_id, &key, &req).await
}

/// `POST /launcher/v1/friends/{friendId}/accept` → 200 [`Friend`].
pub async fn launcher_accept(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    act_for(&state, &core, auth.account_id, &key, &id, Act::Accept).await
}

/// `POST /launcher/v1/friends/{friendId}/decline` → 204.
pub async fn launcher_decline(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    act_for(&state, &core, auth.account_id, &key, &id, Act::Decline).await
}

/// `PUT /launcher/v1/friends/{friendId}/favorite {favorite}` → 200 [`Friend`].
pub async fn launcher_favorite(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Path(id): Path<String>,
    Body(req): Body<FriendFavorite>,
) -> Result<Response, ApiError> {
    let act = Act::Favorite(req.favorite);
    act_for(&state, &core, auth.account_id, &key, &id, act).await
}

/// `DELETE /launcher/v1/friends/{friendId}` → 204.
pub async fn launcher_remove(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    act_for(&state, &core, auth.account_id, &key, &id, Act::Remove).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_refusals_name_the_limit_and_the_side() {
        let e = full(Full::Friends(Side::Other));
        assert_eq!(e.code, ErrorCode::SocialLimitReached);
        assert_eq!(
            e.details.unwrap(),
            json!({"limit": "friends", "max": 200, "side": "other"})
        );
        let e = full(Full::Outgoing);
        assert_eq!(
            e.details.unwrap(),
            json!({"limit": "outgoing", "max": 100, "side": "caller"})
        );
        assert_eq!(friend_id("2535100000000001").unwrap(), 2535100000000001);
        for bad in ["", "abc", "0", "-1", "2535000000000000"] {
            assert!(friend_id(bad).is_err(), "{bad:?}");
        }
    }
}
