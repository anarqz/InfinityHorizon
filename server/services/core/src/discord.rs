//! Discord (CORE_API.md §5a, register CR-S63): the bot's routes behind `X-Fi-Feed-Key` (the feed,
//! link previews and links, handle refreshes) and the player's routes (the link and each
//! profile's announce switch). The server never calls Discord.
//!
//! The bot key: `FI_FEED_KEYS` holds SHA-256 digests only ([`FeedKeys`]); the presented key's
//! digest is compared in constant time with every configured digest, and the header is never
//! logged. Link codes: 32 random bytes (43 base64url characters), only their SHA-256 is stored.

use crate::Core;
use crate::me::PRIVATE;
use crate::session::CoreSession;
use axum::Json;
use axum::extract::{Extension, FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use fi_db::accounts;
use fi_db::discord::{self, Cursor, DiscordIdentity, DiscordUserRow, FeedPerson, LinkError};
use fi_db::identity::IdentityError;
use fi_protocol::ErrorCode;
use fi_protocol::core::{
    DiscordLink, DiscordLinkCode, DiscordLinkCodeRequest, DiscordLinkPreview, DiscordLinkRequest,
    DiscordLinked, DiscordSettings, DiscordUser, DiscordUserUpdate, EventPerson, ProfileAnnounce,
    ProfileAnnounceUpdate, PublicEvent, PublicEventPage, PublicRecord,
};
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::Body;
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// The bot key's header.
pub const FEED_KEY_HEADER: &str = "x-fi-feed-key";

/// `FI_FEED_KEYS`: the SHA-256 digests of the accepted bot keys (never the keys).
#[derive(Clone, PartialEq, Eq)]
pub struct FeedKeys(Vec<[u8; 32]>);

impl std::fmt::Debug for FeedKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FeedKeys({} digests)", self.0.len())
    }
}

impl FeedKeys {
    /// Parses a comma-separated list of 64-hex-character digests (an optional `sha256:` prefix);
    /// at most 8, at least one.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let mut out = Vec::new();
        for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let hex = item.strip_prefix("sha256:").unwrap_or(item);
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("SHA-256 digests in hex (64 characters), comma-separated");
            }
            let mut d = [0u8; 32];
            for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
                let s = std::str::from_utf8(chunk).map_err(|_| "hex")?;
                d[i] = u8::from_str_radix(s, 16).map_err(|_| "hex")?;
            }
            out.push(d);
        }
        if out.is_empty() || out.len() > 8 {
            return Err("one to eight digests");
        }
        Ok(Self(out))
    }

    /// Whether `key` (32-256 printable ASCII characters) is one of the configured keys: its
    /// digest compared in constant time with every digest (no early exit).
    pub fn verify(&self, key: &str) -> bool {
        if !(32..=256).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_graphic()) {
            return false;
        }
        let digest = fi_auth::sha256(key.as_bytes());
        self.0
            .iter()
            .fold(false, |hit, d| hit | fi_auth::ct_eq(d, &digest))
    }
}

/// The bot, authenticated by `X-Fi-Feed-Key`: 503 `SERVICE.UNAVAILABLE` when no key is
/// configured, 401 `AUTH.TOKEN_INVALID` for a missing or wrong key.
pub struct Bot;

impl FromRequestParts<AppState> for Bot {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, ApiError> {
        let core = parts
            .extensions
            .get::<Arc<Core>>()
            .cloned()
            .ok_or_else(ApiError::unavailable)?;
        let Some(keys) = &core.config.feed_keys else {
            return Err(ApiError::new(
                ErrorCode::ServiceUnavailable,
                "the feed is not configured on this server",
            ));
        };
        let presented = parts
            .headers
            .get(FEED_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if keys.verify(presented) {
            Ok(Bot)
        } else {
            tracing::info!("feed key refused");
            Err(ApiError::new(
                ErrorCode::AuthTokenInvalid,
                "invalid feed key",
            ))
        }
    }
}

fn db(e: sqlx::Error) -> ApiError {
    IdentityError::from(e).into()
}

fn snowflake(field: &'static str, text: &str) -> Result<i64, ApiError> {
    text.parse::<i64>()
        .ok()
        .filter(|v| *v > 0 && v.to_string() == text)
        .ok_or_else(|| ApiError::invalid(field, "a Discord user id (decimal snowflake)"))
}

fn handle<'a>(
    field: &'static str,
    text: Option<&'a str>,
    min: usize,
) -> Result<Option<&'a str>, ApiError> {
    match text {
        None => Ok(None),
        Some(t) => {
            let n = t.chars().count();
            if n < min || n > 32 || t.chars().any(char::is_control) || t.trim() != t {
                Err(ApiError::invalid(
                    field,
                    "1-32 characters, no control characters",
                ))
            } else {
                Ok(Some(t))
            }
        }
    }
}

/// A link code from the wire: exactly 43 base64url characters (32 bytes); its SHA-256.
fn code_hash(code: &str) -> Result<[u8; 32], ApiError> {
    let ok = code.len() == 43
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(ApiError::invalid("code", "a link code"));
    }
    Ok(fi_auth::sha256(code.as_bytes()))
}

fn code_invalid() -> ApiError {
    ApiError::new(
        ErrorCode::AuthCodeInvalid,
        "unknown, expired or spent link code",
    )
}

/// The avatar hash's shape (`a_` = animated).
fn avatar(text: Option<&str>) -> Result<Option<&str>, ApiError> {
    match text {
        None => Ok(None),
        Some(t) => {
            let hex = t.strip_prefix("a_").unwrap_or(t);
            if hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                Ok(Some(t))
            } else {
                Err(ApiError::invalid("avatar", "a Discord avatar hash"))
            }
        }
    }
}

/// The avatar on Discord's CDN (CORE_API.md §5a.2): the user's, or Discord's default avatar.
pub fn avatar_url(id: i64, hash: Option<&str>) -> String {
    match hash {
        Some(h) => format!("https://cdn.discordapp.com/avatars/{id}/{h}.png?size=256"),
        None => format!(
            "https://cdn.discordapp.com/embed/avatars/{}.png",
            (id >> 22).rem_euclid(6)
        ),
    }
}

/// A link on the wire.
pub fn link_view(d: &DiscordUserRow) -> DiscordLink {
    DiscordLink {
        discord_user_id: d.id.to_string(),
        username: d.username.clone(),
        global_name: d.global_name.clone(),
        avatar_url: avatar_url(d.id, d.avatar.as_deref()),
        linked_at: views::at(d.linked_at),
        refreshed_at: views::at(d.refreshed_at),
    }
}

fn user_view(d: &DiscordUserRow) -> DiscordUser {
    DiscordUser {
        id: d.id.to_string(),
        username: d.username.clone(),
        global_name: d.global_name.clone(),
        avatar_url: avatar_url(d.id, d.avatar.as_deref()),
        refreshed_at: views::at(d.refreshed_at),
    }
}

fn person(p: &FeedPerson) -> EventPerson {
    EventPerson {
        name: p.name.clone(),
        discord: p.discord.as_ref().map(user_view),
    }
}

fn record_view(r: &discord::FeedRecord) -> PublicRecord {
    PublicRecord {
        board: r.board.clone(),
        mode: r.mode.clone(),
        unit: r.unit.clone(),
        value: r.value,
        previous_value: r.previous_value,
        car_id: r.car_id,
        car_name: r.car_name.clone(),
        board_id: r.board_id,
        route_id: r.route_id,
        route_name: r.route_name.clone(),
    }
}

// The bot ---------------------------------------------------------------------------------------

/// `?after=&limit=` of the feed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedQuery {
    after: Option<String>,
    limit: Option<String>,
}

/// `GET /core/v1/feed?after=&limit=` (bot) → 200 [`PublicEventPage`].
pub async fn feed(
    State(state): State<AppState>,
    _bot: Bot,
    query: Result<Query<FeedQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(q) = query.map_err(|_| ApiError::invalid("after", "after and limit only"))?;
    let after = q
        .after
        .as_deref()
        .map(|a| Cursor::parse(a).ok_or_else(|| ApiError::invalid("after", "a feed cursor")))
        .transpose()?;
    let limit = match q.limit.as_deref() {
        None => 100,
        Some(l) => l
            .parse::<i64>()
            .ok()
            .filter(|v| (1..=discord::FEED_MAX).contains(v))
            .ok_or_else(|| ApiError::invalid("limit", "1 to 200"))?,
    };
    let mut conn = state.pool.acquire().await?;
    let page = discord::feed_in(&mut conn, after, limit)
        .await
        .map_err(db)?;
    let body = PublicEventPage {
        items: page
            .items
            .iter()
            .map(|r| PublicEvent {
                id: r.cursor.to_string(),
                at: views::at(r.at),
                game: r.game.clone(),
                kind: r.kind.clone(),
                audience: r.audience.clone(),
                subject: Some(person(&r.subject)),
                actor: r.actor.as_ref().map(person),
                until: r.until.map(views::at),
                record: r.record.as_ref().map(record_view),
            })
            .collect(),
        next: page.next.map(|c| c.to_string()),
        gap: page.gap,
    };
    Ok(([(CACHE_CONTROL, PRIVATE)], Json(body)).into_response())
}

/// `POST /core/v1/discord/link-preview {code}` (bot) → 200 [`DiscordLinkPreview`].
pub async fn link_preview(
    State(state): State<AppState>,
    _bot: Bot,
    Body(req): Body<DiscordLinkCodeRequest>,
) -> Result<Response, ApiError> {
    let hash = code_hash(&req.code)?;
    let mut conn = state.pool.acquire().await?;
    let (names, expires) = discord::preview_in(&mut conn, &hash)
        .await
        .map_err(db)?
        .ok_or_else(code_invalid)?;
    let body = DiscordLinkPreview {
        names,
        expires_at: views::at(expires),
    };
    Ok(([(CACHE_CONTROL, PRIVATE)], Json(body)).into_response())
}

/// `PUT /core/v1/discord/links {code, discordUserId, username, globalName?}` (bot) → 200
/// [`DiscordLinked`].
pub async fn link(
    State(state): State<AppState>,
    _bot: Bot,
    Body(req): Body<DiscordLinkRequest>,
) -> Result<Response, ApiError> {
    let hash = code_hash(&req.code)?;
    let id = snowflake("discordUserId", &req.discord_user_id)?;
    let username = handle("username", Some(&req.username), 2)?.unwrap_or_default();
    let global_name = handle("globalName", req.global_name.as_deref(), 1)?;
    let avatar = avatar(req.avatar.as_deref())?;
    let mut tx = state.pool.begin().await?;
    let linked = discord::link_in(
        &mut tx,
        &hash,
        DiscordIdentity {
            id,
            username,
            global_name,
            avatar,
        },
    )
    .await
    .map_err(|e| match e {
        LinkError::CodeInvalid => code_invalid(),
        LinkError::Taken => ApiError::new(
            ErrorCode::AccountsAlreadyLinked,
            "this Discord user is linked to another account",
        )
        .with_details(json!({"reason": "discord_user_taken"})),
        LinkError::Db(e) => e.into(),
    })?;
    tx.commit().await?;
    let body = DiscordLinked {
        names: linked.names,
        linked_at: views::at(linked.linked_at),
    };
    Ok(([(CACHE_CONTROL, PRIVATE)], Json(body)).into_response())
}

/// `PUT /core/v1/discord/users/{discordUserId} {username, globalName?}` (bot) → 200
/// [`DiscordUser`]; 404 `ACCOUNTS.NOT_FOUND` when no account links that user.
pub async fn refresh_user(
    State(state): State<AppState>,
    _bot: Bot,
    Path(id): Path<String>,
    Body(req): Body<DiscordUserUpdate>,
) -> Result<Response, ApiError> {
    let id = snowflake("discordUserId", &id)?;
    let username = handle("username", Some(&req.username), 2)?.unwrap_or_default();
    let global_name = handle("globalName", req.global_name.as_deref(), 1)?;
    let avatar = avatar(req.avatar.as_deref())?;
    let mut conn = state.pool.acquire().await?;
    let row = discord::refresh_user_in(
        &mut conn,
        DiscordIdentity {
            id,
            username,
            global_name,
            avatar,
        },
    )
    .await
    .map_err(db)?
    .ok_or_else(|| ApiError::new(ErrorCode::AccountsNotFound, "no account links this user"))?;
    Ok(([(CACHE_CONTROL, PRIVATE)], Json(user_view(&row))).into_response())
}

// The player ------------------------------------------------------------------------------------

fn announce_view(r: &discord::AnnounceRow) -> ProfileAnnounce {
    ProfileAnnounce {
        xuid: r.xuid.to_string(),
        game: r.game.clone(),
        gamertag: r.name.clone(),
        announce: r.announce,
    }
}

/// `GET /core/v1/me/discord` → 200 [`DiscordSettings`].
pub async fn settings(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
) -> Result<Response, ApiError> {
    let mut conn = state.pool.acquire().await?;
    let link = discord::link_of_in(&mut conn, session.account_id)
        .await
        .map_err(db)?;
    let profiles = discord::announce_list_in(&mut conn, session.account_id)
        .await
        .map_err(db)?;
    let body = DiscordSettings {
        available: core.config.discord_link_url.is_some(),
        link: link.as_ref().map(link_view),
        profiles: profiles.iter().map(announce_view).collect(),
    };
    Ok(([(CACHE_CONTROL, PRIVATE)], Json(body)).into_response())
}

/// `POST /core/v1/me/discord/link` → 201 [`DiscordLinkCode`].
pub async fn new_code(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
) -> Result<Response, ApiError> {
    let Some(base) = core.config.discord_link_url.clone() else {
        return Err(ApiError::new(
            ErrorCode::ServiceUnavailable,
            "Discord linking is not configured on this server",
        ));
    };
    core.account_write(session.account_id)?;
    let bytes = fi_auth::random_bytes::<32>().map_err(|_| ApiError::unavailable())?;
    let code = fi_auth::b64::url_encode(&bytes);
    let hash = fi_auth::sha256(code.as_bytes());
    let mut tx = state.pool.begin().await?;
    let account = accounts::account_state_in(&mut tx, session.account_id)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    if account.status != "active" {
        return Err(ApiError::new(
            ErrorCode::AccountsDeletionPending,
            "the account is pending deletion",
        ));
    }
    let expires = discord::issue_code_in(&mut tx, session.account_id, &hash)
        .await
        .map_err(db)?;
    tx.commit().await?;
    let body = DiscordLinkCode {
        url: format!("{base}/link?code={code}"),
        expires_at: views::at(expires),
    };
    Ok((StatusCode::CREATED, [(CACHE_CONTROL, PRIVATE)], Json(body)).into_response())
}

/// `DELETE /core/v1/me/discord/link` → 204 (also when not linked).
pub async fn unlink(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
) -> Result<Response, ApiError> {
    core.account_write(session.account_id)?;
    let mut tx = state.pool.begin().await?;
    discord::unlink_in(&mut tx, session.account_id, true).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `PUT /core/v1/me/profiles/{xuid}/announce {announce}` → 200 [`ProfileAnnounce`].
pub async fn set_announce(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    session: CoreSession,
    Path(xuid): Path<String>,
    Body(req): Body<ProfileAnnounceUpdate>,
) -> Result<Response, ApiError> {
    let not_found = || ApiError::new(ErrorCode::AccountsNotFound, "no such profile");
    let xuid = xuid
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0 && v.to_string() == xuid)
        .ok_or_else(not_found)?;
    core.account_write(session.account_id)?;
    let mut conn = state.pool.acquire().await?;
    let row = discord::set_announce_in(&mut conn, session.account_id, xuid, req.announce)
        .await
        .map_err(db)?
        .ok_or_else(not_found)?;
    Ok(([(CACHE_CONTROL, PRIVATE)], Json(announce_view(&row))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_keys_hold_digests_and_compare_every_one() {
        let key = "k".repeat(40);
        let digest = fi_auth::hex(&fi_auth::sha256(key.as_bytes()));
        let other = fi_auth::hex(&fi_auth::sha256(b"another-key-another-key-another-key"));
        let keys = FeedKeys::parse(&format!("sha256:{other}, {digest}")).unwrap();
        assert!(keys.verify(&key));
        assert!(!keys.verify(&"k".repeat(41)));
        assert!(!keys.verify("short"));
        assert!(!keys.verify(&digest), "the digest itself is not a key");
        assert_eq!(format!("{keys:?}"), "FeedKeys(2 digests)");
        for bad in [
            "",
            "abc",
            &"g".repeat(64),
            &format!("{digest},{}", "0".repeat(63)),
        ] {
            assert!(FeedKeys::parse(bad).is_err(), "{bad}");
        }
        let nine = vec![digest.clone(); 9].join(",");
        assert!(FeedKeys::parse(&nine).is_err());
    }

    #[test]
    fn wire_values_are_checked() {
        assert_eq!(
            snowflake("f", "80351110224678912").unwrap(),
            80351110224678912
        );
        for bad in ["", "0", "-1", "01", "1.5", "9223372036854775808", " 1"] {
            assert!(snowflake("f", bad).is_err(), "{bad}");
        }
        assert!(code_hash(&"a".repeat(43)).is_ok());
        assert!(code_hash(&"a".repeat(42)).is_err());
        assert!(code_hash(&format!("{}=", "a".repeat(42))).is_err());
        assert_eq!(handle("u", Some("ab"), 2).unwrap(), Some("ab"));
        assert!(handle("u", Some("a"), 2).is_err());
        assert!(handle("u", Some(&"a".repeat(33)), 1).is_err());
        assert!(handle("u", Some("a\u{7}b"), 1).is_err());
        assert!(handle("u", Some(" ab"), 1).is_err());
        assert_eq!(handle("u", None, 1).unwrap(), None);
        let h = "0123456789abcdef0123456789abcdef";
        assert_eq!(avatar(Some(h)).unwrap(), Some(h));
        assert!(avatar(Some(&format!("a_{h}"))).is_ok());
        for bad in [
            "",
            "a_",
            &h[1..],
            &h.to_uppercase(),
            &format!("{h}.png"),
            "../x",
        ] {
            assert!(avatar(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(
            avatar_url(80351110224678912, Some(h)),
            format!("https://cdn.discordapp.com/avatars/80351110224678912/{h}.png?size=256")
        );
        assert_eq!(
            avatar_url(80351110224678912, None),
            format!(
                "https://cdn.discordapp.com/embed/avatars/{}.png",
                (80351110224678912_i64 >> 22) % 6
            )
        );
    }
}
