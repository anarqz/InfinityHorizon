//! `/internal/dashboard/v1` (DASHBOARD_API.md, CR-S82, OD-168): the play dashboard's reads for
//! `apps/play`'s server. `fi-platform` mounts [`router`] only on its second listener
//! (`FI_INTERNAL_BIND`), never on the public port, and without the online policy or the origin
//! lock: the listener is reachable only on the private network and every request carries
//! `X-Fi-Dashboard-Key`.
//!
//! The key is checked before anything else, for every path and method under this router: a
//! missing or wrong key, or a process without `FI_DASHBOARD_KEY`, is 401 `AUTH.UNAUTHENTICATED`
//! and nothing else (fail closed). Every answer is `Cache-Control: no-store` (the site's LRU
//! caches). One pooled connection per request; the queries are `fi_db::dashboard`'s.
//!
//! The leaderboards (DASHBOARD_API.md §3, PD-2) are [`boards`]; the players, profiles and avatars
//! (§4, PD-3) are [`players`]; the hourly snapshot's content (§5, OD-173) is [`snapshot`], and
//! `GET /listed` the live allowlist the site applies over it; the clubs (§6, PD-4) are [`clubs`]; garages and shared creations (§7, PD-5) [`garages`].

pub mod boards;
pub mod clubs;
pub mod garages;
pub mod players;
pub mod snapshot;

use crate::errors::ApiError;
use crate::views::at;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, Request, State};
use axum::http::HeaderValue;
use axum::http::header::CACHE_CONTROL;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, middleware};
use fi_db::PgPool;
use fi_db::dashboard::{self, LobbyRow};
use fi_protocol::ErrorCode;
use fi_protocol::dashboard::{
    KEY_HEADER, LOBBIES_DEFAULT_LIMIT, LOBBIES_MAX_LIMIT, ListedClub, ListedProfile,
    ListedProfiles, Live, LiveLobbies, Lobby, LobbyHost, LobbyList, ModeCounts, REF_LENGTH,
};
use fi_protocol::routes::dashboard as r;
use serde::Deserialize;
use std::sync::Arc;

/// The shortest `FI_DASHBOARD_KEY` accepted (after trimming): the platform refuses to start and
/// the snapshot job refuses to run with a shorter one. The key's sub-keys (below) feed public
/// HMACs (creation ids of small storefront ids), which would otherwise be an offline oracle for
/// a weak key.
pub const MIN_KEY_CHARS: usize = 32;

/// Why `FI_DASHBOARD_KEY` was refused.
pub const SHORT_KEY: &str =
    "FI_DASHBOARD_KEY is shorter than 32 characters: generate a random key of at least 32";

/// A sub-key of `FI_DASHBOARD_KEY` (DASHBOARD_API.md §5.1 "As built", v2):
/// `HMAC-SHA256(key, "fi-dashboard-subkey-v2:" || purpose)`, so the request secret and the keys
/// behind public HMACs are never the same bytes.
fn subkey(key: &[u8], purpose: &str) -> [u8; 32] {
    fi_auth::hmac_sha256(key, format!("fi-dashboard-subkey-v2:{purpose}").as_bytes())
}

/// The trimmed key text when it is long enough; `Ok(None)` when blank (every request refused,
/// fail closed), `Err` when shorter than [`MIN_KEY_CHARS`].
pub fn checked_key(text: &str) -> Result<Option<&str>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        Ok(None)
    } else if text.chars().count() < MIN_KEY_CHARS {
        Err(SHORT_KEY)
    } else {
        Ok(Some(text))
    }
}

/// The shared key (`FI_DASHBOARD_KEY`): the request secret, compared as its `auth` sub-key on both
/// sides (constant time, whatever the presented key's length), and the [`RefKey`] of the public
/// HMACs.
#[derive(Clone)]
pub struct DashboardKey([u8; 32], RefKey);

impl DashboardKey {
    /// The key from its configured text (trimmed); `None` when empty (every request is then
    /// refused). The length rule ([`checked_key`]) is the configuration's (startup), so tests may
    /// build short keys.
    pub fn new(text: &str) -> Option<Self> {
        let refs = RefKey::new(text)?;
        Some(Self(subkey(text.trim().as_bytes(), "auth"), refs))
    }

    fn admits(&self, presented: &[u8]) -> bool {
        fi_auth::ct_eq(&subkey(presented, "auth"), &self.0)
    }

    /// The ref key of the same secret.
    pub fn refs(&self) -> &RefKey {
        &self.1
    }
}

/// The keys of the public HMACs (DASHBOARD_API.md §5.1, §7): sub-keys of `FI_DASHBOARD_KEY`, the
/// secret the site and the snapshot job share, one per purpose. A ref or a creation id is opaque,
/// stable, and cannot be turned back into its id.
#[derive(Clone)]
pub struct RefKey {
    refs: [u8; 32],
    creations: [u8; 32],
}

impl RefKey {
    /// The sub-keys of `FI_DASHBOARD_KEY`'s text (trimmed); `None` when blank.
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self {
            refs: subkey(text.as_bytes(), "ref"),
            creations: subkey(text.as_bytes(), "creation"),
        })
    }

    /// `base64url(HMAC-SHA256(ref_key, "fi-dashboard-ref-v2:" || profile id))`, the first 22
    /// characters ([`REF_LENGTH`]); `ref_key` = `HMAC-SHA256(key, "fi-dashboard-subkey-v2:ref")`.
    /// The profile id is its canonical lower-case hyphenated text.
    pub fn of(&self, profile: uuid::Uuid) -> String {
        tag(
            &self.refs,
            &format!("fi-dashboard-ref-v2:{}", profile.hyphenated()),
        )
    }

    /// A shared creation's opaque id (§7): `base64url(HMAC-SHA256(creation_key,
    /// "fi-dashboard-creation-v2:" || storefront id))`, the first 22 characters, the storefront id in
    /// decimal; `creation_key` = `HMAC-SHA256(key, "fi-dashboard-subkey-v2:creation")`.
    pub fn creation(&self, storefront_id: i32) -> String {
        tag(
            &self.creations,
            &format!("fi-dashboard-creation-v2:{storefront_id}"),
        )
    }
}

fn tag(key: &[u8; 32], message: &str) -> String {
    let mac = fi_auth::hmac_sha256(key, message.as_bytes());
    let mut text = fi_auth::b64::url_encode(&mac);
    text.truncate(REF_LENGTH);
    text
}

impl std::fmt::Debug for RefKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RefKey(..)")
    }
}

impl std::fmt::Debug for DashboardKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DashboardKey(..)")
    }
}

/// The surface's state.
#[derive(Clone)]
struct Dash {
    pool: PgPool,
    /// The ref key (`None` without `FI_DASHBOARD_KEY`: the guard refuses everything anyway).
    refs: Option<RefKey>,
    /// The avatar source (`None`: every avatar is 404).
    avatars: Option<players::Avatars>,
}

/// The dashboard routes over `pool`, behind `key` (`None`: every request is 401), with avatars
/// from the Discord CDN.
pub fn router(pool: PgPool, key: Option<DashboardKey>) -> Router {
    router_with(pool, key, players::Avatars::discord())
}

/// [`router`] with another avatar source (tests: a local stub CDN).
pub fn router_with(
    pool: PgPool,
    key: Option<DashboardKey>,
    avatars: Option<players::Avatars>,
) -> Router {
    Router::new()
        .route(r::LIVE, get(live))
        .route(r::LOBBIES, get(lobbies))
        .route(r::BOARDS, get(boards::index))
        .route(r::BOARD, get(boards::page))
        .route(r::PLAYERS, get(players::list))
        .route(r::PLAYER, get(players::profile))
        .route(r::MEDIA_AVATAR, get(players::avatar))
        .route(r::LISTED, get(listed))
        .route(r::CLUBS, get(clubs::list))
        .route(r::CLUB, get(clubs::page))
        .route(r::MEDIA_CREATION, get(garages::preview))
        .with_state(Dash {
            pool,
            avatars,
            refs: key.as_ref().map(|k| k.refs().clone()),
        })
        .layer(middleware::from_fn_with_state(key.map(Arc::new), guard))
        .layer(middleware::from_fn(no_store))
}

fn unauthenticated() -> ApiError {
    ApiError::new(ErrorCode::AuthUnauthenticated, "unauthenticated")
}

/// The key check, before routing, query parsing or the database.
async fn guard(
    State(key): State<Option<Arc<DashboardKey>>>,
    request: Request,
    next: Next,
) -> Response {
    let admitted = match (&key, request.headers().get(KEY_HEADER)) {
        (Some(key), Some(presented)) => key.admits(presented.as_bytes()),
        _ => false,
    };
    if !admitted {
        return unauthenticated().into_response();
    }
    next.run(request).await
}

/// `Cache-Control: no-store` on every answer, refusals included.
async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The common query (DASHBOARD_API.md §1); unknown parameters are ignored.
#[derive(Debug, Default, Deserialize)]
struct DashQuery {
    game: Option<String>,
    mode: Option<String>,
    limit: Option<String>,
    class: Option<String>,
    page: Option<String>,
    q: Option<String>,
    sort: Option<String>,
    file: Option<String>,
}

fn query(q: Result<Query<DashQuery>, QueryRejection>) -> Result<DashQuery, ApiError> {
    q.map(|Query(q)| q)
        .map_err(|_| ApiError::invalid("query", "malformed query string"))
}

fn unknown_game() -> ApiError {
    ApiError::invalid("game", "unknown game")
}

/// `game`, default `fh2`.
fn game(q: &DashQuery) -> String {
    q.game
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .unwrap_or("fh2")
        .to_owned()
}

/// `mode`, required: `hardcore` or `noob` (400 `REQUEST.INVALID {field: "mode"}`).
fn required_mode(q: &DashQuery) -> Result<String, ApiError> {
    match q.mode.as_deref().map(str::trim) {
        Some(m @ ("hardcore" | "noob")) => Ok(m.to_owned()),
        _ => Err(ApiError::invalid("mode", "mode is hardcore or noob")),
    }
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// `GET /internal/dashboard/v1/live?game`: 200 [`Live`]; 400 `REQUEST.INVALID {field: "game"}`
/// for an unknown game.
async fn live(
    State(s): State<Dash>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<Live>, ApiError> {
    let q = query(q)?;
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    let row = dashboard::live_in(&mut conn, &game)
        .await?
        .ok_or_else(unknown_game)?;
    Ok(Json(Live {
        at: at(row.at),
        game: row.game,
        online: count(row.online),
        per_mode: ModeCounts {
            hardcore: count(row.online_hardcore),
            noob: count(row.online_noob),
        },
        lobbies: LiveLobbies {
            open: count(row.lobbies),
            per_mode: ModeCounts {
                hardcore: count(row.lobbies_hardcore),
                noob: count(row.lobbies_noob),
            },
        },
        capacity: row.capacity,
    }))
}

fn lobby(row: LobbyRow) -> Lobby {
    Lobby {
        world_id: row.world_id.to_string(),
        name: row.name,
        host: row.host.map(|h| LobbyHost {
            name: h.name,
            discord_name: h.discord_name,
            avatar: h.avatar,
        }),
        players: row.players,
        max_players: row.max_players,
        full: row.full,
        activity: row.activity,
        region: row.region,
        mods: row.mods,
        dedicated: row.dedicated,
        any_mode: row.any_mode,
        created_at: at(row.created_at),
    }
}

/// `GET /internal/dashboard/v1/lobbies?game&mode&limit`: 200 [`LobbyList`]; 400 `REQUEST.INVALID`
/// with `field` `mode` (required, `hardcore` or `noob`), `limit` (1 to 100, default 50) or `game`.
async fn lobbies(
    State(s): State<Dash>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<LobbyList>, ApiError> {
    let q = query(q)?;
    let mode = required_mode(&q)?;
    let limit = match q.limit.as_deref().map(str::trim) {
        None => LOBBIES_DEFAULT_LIMIT,
        Some(text) => text
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=LOBBIES_MAX_LIMIT).contains(n))
            .ok_or_else(|| ApiError::invalid("limit", "limit is 1 to 100"))?,
    };
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    if !dashboard::game_known_in(&mut conn, &game).await? {
        return Err(unknown_game());
    }
    let rows = dashboard::lobbies_in(&mut conn, &game, &mode, i64::from(limit)).await?;
    Ok(Json(LobbyList {
        items: rows.into_iter().map(lobby).collect(),
    }))
}

/// `GET /internal/dashboard/v1/listed?game`: 200 [`ListedProfiles`], every profile of the game
/// listed on the website now, with its opaque ref and current lower-case name (DASHBOARD_API.md
/// §5.1), and every club of the game not private now (§5.2); 400 `REQUEST.INVALID {field:
/// "game"}` for an unknown game.
async fn listed(
    State(s): State<Dash>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<ListedProfiles>, ApiError> {
    let q = query(q)?;
    let refs = s.refs.as_ref().ok_or_else(unauthenticated)?;
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    if !dashboard::game_known_in(&mut conn, &game).await? {
        return Err(unknown_game());
    }
    let profiles = dashboard::players::listed_profiles_in(&mut conn, &game)
        .await?
        .into_iter()
        .map(|(id, name)| ListedProfile {
            ref_: refs.of(id),
            name,
        })
        .collect();
    let clubs = fi_db::clubs::public_club_tags_in(&mut conn, &game)
        .await?
        .into_iter()
        .map(|(mode, tag)| ListedClub { mode, tag })
        .collect();
    let creations = fi_db::storefront::shared_file_ids_in(&mut conn, &game)
        .await?
        .into_iter()
        .map(|f| refs.creation(f))
        .collect();
    Ok(Json(ListedProfiles {
        profiles,
        clubs,
        creations,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request as HttpRequest, StatusCode};
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    /// A pool that never connects: the refusals below must not need the database.
    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://unused:unused@127.0.0.1:9/unused")
            .unwrap()
    }

    async fn send(app: &Router, path: &str, key: Option<&str>) -> (StatusCode, String, String) {
        let mut req = HttpRequest::builder().uri(path);
        if let Some(k) = key {
            req = req.header(KEY_HEADER, k);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let cache = res
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = to_bytes(res.into_body(), 1 << 16).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let detail = format!(
            "{}{}",
            body["code"].as_str().unwrap_or_default(),
            body["details"]["field"]
                .as_str()
                .map(|f| format!(" {f}"))
                .unwrap_or_default()
        );
        (status, detail, cache)
    }

    #[test]
    fn an_empty_key_is_no_key() {
        assert!(DashboardKey::new("  ").is_none());
        let key = DashboardKey::new(" secret ").unwrap();
        assert!(key.admits(b"secret"));
        assert!(!key.admits(b"secret2") && !key.admits(b""));
        assert_eq!(format!("{key:?}"), "DashboardKey(..)");
    }

    #[test]
    fn a_key_shorter_than_32_characters_is_refused() {
        assert_eq!(checked_key("  "), Ok(None));
        assert_eq!(checked_key(&"a".repeat(31)), Err(SHORT_KEY));
        assert_eq!(
            checked_key(&format!(" {} ", "a".repeat(31))),
            Err(SHORT_KEY),
            "trimmed first"
        );
        let long = "a".repeat(32);
        assert_eq!(checked_key(&format!(" {long} ")), Ok(Some(long.as_str())));
    }

    #[test]
    fn each_purpose_has_its_own_subkey() {
        let key = "k".repeat(40);
        let auth = subkey(key.as_bytes(), "auth");
        let refs = subkey(key.as_bytes(), "ref");
        let creations = subkey(key.as_bytes(), "creation");
        assert!(auth != refs && refs != creations && auth != creations);
        // None is the raw key or its plain hash.
        for k in [auth, refs, creations] {
            assert_ne!(k, fi_auth::sha256(key.as_bytes()));
            assert_ne!(&k[..], &key.as_bytes()[..32]);
        }
        assert_eq!(
            refs,
            fi_auth::hmac_sha256(key.as_bytes(), b"fi-dashboard-subkey-v2:ref")
        );
        // The same number under both purposes gives unrelated outputs.
        let r = RefKey::new(&key).unwrap();
        assert_ne!(r.creation(7), tag(&refs, "fi-dashboard-creation-v2:7"));
        let d = DashboardKey::new(&key).unwrap();
        assert!(d.admits(key.as_bytes()));
        assert!(!d.admits(&refs), "a sub-key is not the request secret");
    }

    #[tokio::test]
    async fn a_missing_wrong_or_unconfigured_key_is_401_and_nothing_else() {
        let app = router(lazy_pool(), DashboardKey::new("k3y"));
        for path in [
            "/internal/dashboard/v1/live",
            "/internal/dashboard/v1/lobbies?mode=bogus",
            "/internal/dashboard/v1/unknown",
        ] {
            for key in [None, Some("wrong"), Some("")] {
                let (status, code, cache) = send(&app, path, key).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {key:?}");
                assert_eq!(code, "AUTH.UNAUTHENTICATED", "{path} {key:?}");
                assert_eq!(cache, "no-store");
            }
        }
        // FI_DASHBOARD_KEY unset: even the right-looking key is refused.
        let closed = router(lazy_pool(), None);
        let (status, code, _) = send(&closed, "/internal/dashboard/v1/live", Some("k3y")).await;
        assert_eq!(
            (status, code.as_str()),
            (StatusCode::UNAUTHORIZED, "AUTH.UNAUTHENTICATED")
        );
    }

    #[tokio::test]
    async fn bad_lobby_parameters_are_400_before_the_database() {
        let app = router(lazy_pool(), DashboardKey::new("k3y"));
        for (query, field) in [
            ("", "mode"),
            ("?mode=", "mode"),
            ("?mode=HARDCORE", "mode"),
            ("?mode=any", "mode"),
            ("?mode=noob&limit=0", "limit"),
            ("?mode=noob&limit=101", "limit"),
            ("?mode=noob&limit=-1", "limit"),
            ("?mode=noob&limit=ten", "limit"),
        ] {
            let path = format!("/internal/dashboard/v1/lobbies{query}");
            let (status, detail, cache) = send(&app, &path, Some("k3y")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
            assert_eq!(detail, format!("REQUEST.INVALID {field}"), "{path}");
            assert_eq!(cache, "no-store");
        }
    }

    #[tokio::test]
    async fn bad_board_requests_are_400_or_404_before_the_database() {
        let app = router(lazy_pool(), DashboardKey::new("k3y"));
        let b = "/internal/dashboard/v1/boards";
        for (path, status, detail) in [
            (
                b.to_string(),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID mode",
            ),
            (
                format!("{b}?mode=any"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID mode",
            ),
            (
                format!("{b}/rivals/560"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID mode",
            ),
            (
                format!("{b}/rivals/560?mode=noob&page=0"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID page",
            ),
            (
                format!("{b}/rivals/560?mode=noob&page=41"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID page",
            ),
            (
                format!("{b}/rivals/560?mode=noob&page=two"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID page",
            ),
            (
                format!("{b}/rivals/560?mode=noob&class=R"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID class",
            ),
            (
                format!("{b}/rivals/560?mode=noob&class=s1"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID class",
            ),
            (
                format!("{b}/speed-traps/1?mode=noob&class=A"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID class",
            ),
            (
                format!("{b}/xp/global?mode=noob&class=A"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID class",
            ),
            (
                format!("{b}/drift-zones/1?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
            (
                format!("{b}/speed-traps/36?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
            (
                format!("{b}/speed-traps/01?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
            (
                format!("{b}/rivals/99999?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
            (
                format!("{b}/showcases/578?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
            (
                format!("{b}/monthly-rivals/999?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
            (
                format!("{b}/stats/drift?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
            (
                format!("{b}/xp/club?mode=noob"),
                StatusCode::NOT_FOUND,
                "BOARDS.NOT_FOUND",
            ),
        ] {
            let (got, d, cache) = send(&app, &path, Some("k3y")).await;
            assert_eq!((got, d.as_str()), (status, detail), "{path}");
            assert_eq!(cache, "no-store");
        }
        // The key still comes first.
        let (got, d, _) = send(&app, &format!("{b}/drift-zones/1"), None).await;
        assert_eq!(
            (got, d.as_str()),
            (StatusCode::UNAUTHORIZED, "AUTH.UNAUTHENTICATED")
        );
    }

    #[tokio::test]
    async fn bad_player_requests_are_refused_before_the_database() {
        let app = router(lazy_pool(), DashboardKey::new("k3y"));
        let p = "/internal/dashboard/v1/players";
        for (path, status, detail) in [
            (
                p.to_string(),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID mode",
            ),
            (
                format!("{p}?mode=noob&q=a"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID q",
            ),
            (
                format!("{p}?mode=noob&q=a%25"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID q",
            ),
            (
                format!("{p}?mode=noob&q=abcdefghijklmnop"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID q",
            ),
            (
                format!("{p}?mode=noob&sort=xp"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID sort",
            ),
            (
                format!("{p}?mode=noob&page=0"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID page",
            ),
            (
                format!("{p}?mode=noob&page=101"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID page",
            ),
            (
                format!("{p}/7up"),
                StatusCode::NOT_FOUND,
                "PLAYERS.NOT_FOUND",
            ),
            (
                format!("{p}/a_b"),
                StatusCode::NOT_FOUND,
                "PLAYERS.NOT_FOUND",
            ),
            (
                format!("{p}/{}", "a".repeat(21)),
                StatusCode::NOT_FOUND,
                "PLAYERS.NOT_FOUND",
            ),
            (
                "/internal/dashboard/v1/media/avatar/7up".to_string(),
                StatusCode::NOT_FOUND,
                "",
            ),
        ] {
            let (got, d, cache) = send(&app, &path, Some("k3y")).await;
            assert_eq!((got, d.as_str()), (status, detail), "{path}");
            assert_eq!(cache, "no-store");
        }
        let (got, d, _) = send(&app, "/internal/dashboard/v1/media/avatar/Alex", None).await;
        assert_eq!(
            (got, d.as_str()),
            (StatusCode::UNAUTHORIZED, "AUTH.UNAUTHENTICATED")
        );
    }

    #[tokio::test]
    async fn bad_club_requests_are_refused_before_the_database() {
        let app = router(lazy_pool(), DashboardKey::new("k3y"));
        let c = "/internal/dashboard/v1/clubs";
        for (path, status, detail) in [
            (
                c.to_string(),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID mode",
            ),
            (
                format!("{c}/ABC"),
                StatusCode::BAD_REQUEST,
                "REQUEST.INVALID mode",
            ),
            (
                format!("{c}/A?mode=noob"),
                StatusCode::NOT_FOUND,
                "CLUBS.NOT_FOUND",
            ),
            (
                format!("{c}/ABCDE?mode=noob"),
                StatusCode::NOT_FOUND,
                "CLUBS.NOT_FOUND",
            ),
            (
                format!("{c}/A-B?mode=noob"),
                StatusCode::NOT_FOUND,
                "CLUBS.NOT_FOUND",
            ),
            // A creation preview's malformed id: a bare 404 before the database.
            (
                "/internal/dashboard/v1/media/creation/short".to_string(),
                StatusCode::NOT_FOUND,
                "",
            ),
        ] {
            let (got, d, cache) = send(&app, &path, Some("k3y")).await;
            assert_eq!((got, d.as_str()), (status, detail), "{path}");
            assert_eq!(cache, "no-store");
        }
    }
}
