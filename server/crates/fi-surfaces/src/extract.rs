//! Request extraction shared by the surfaces: strict JSON bodies (`REQUEST.INVALID` with
//! `details.field`), `Idempotency-Key`, the client network for rate limits, the launcher's bearer
//! access token and the portal's assertion and session token (ACCOUNTS.md §1, §6, §9).

use crate::config::PortalAssertion;
use crate::errors::ApiError;
use crate::state::{AppState, now_secs};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use fi_auth::access::{AccessError, Audience};
use fi_auth::opaque::{self, Kind};
use fi_db::accounts::{self, AccountState};
use fi_db::limits::Idempotency;
use fi_db::tokens;
use fi_protocol::{ErrorCode, headers};
use fi_runtime::http::ClientAddress;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use uuid::Uuid;

/// Largest JSON body the identity routes accept.
pub const MAX_BODY: usize = 64 * 1024;

/// A JSON body parsed strictly: unknown fields, missing fields and wrong types are
/// `REQUEST.INVALID` naming the field (never echoing a value).
pub struct Body<T>(pub T);

/// Parses `bytes` as `T` (see [`Body`]).
pub fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    if bytes.len() > MAX_BODY {
        return Err(ApiError::new(
            ErrorCode::RequestTooLarge,
            "request body too large",
        ));
    }
    let de = &mut serde_json::Deserializer::from_slice(bytes);
    serde_path_to_error::deserialize(de).map_err(|e| {
        let path = e.path().to_string();
        let message = e.into_inner().to_string();
        let named = |prefix: &str| {
            message
                .strip_prefix(prefix)
                .and_then(|rest| rest.split('`').next())
                .map(str::to_owned)
        };
        let name = named("unknown field `").or_else(|| named("missing field `"));
        let field = match (path.trim_matches('.'), name) {
            ("", Some(n)) => n,
            (p, Some(n)) if !p.ends_with(&n) => format!("{p}.{n}"),
            (p, Some(_)) => p.to_owned(),
            ("", None) => "body".to_owned(),
            (p, None) => p.to_owned(),
        };
        ApiError::invalid(field, "malformed request body")
    })
}

impl<S, T> FromRequest<S> for Body<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|_| ApiError::invalid("body", "unreadable request body"))?;
        parse(&bytes).map(Body)
    }
}

/// The `?game=` of a `/launcher/v1/profiles/{xuid}*` route (CR-S98, CAREERS.md §3, §6.1): the game
/// of the career it acts on. Absent (or empty), [`CareerGame::resolve_in`] means `fh2` while the
/// XUID has careers in one game only, so a launcher released before CR-S98 (it names no game)
/// keeps FH2's career; a game the profile has no career of answers as an unknown profile does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CareerGame(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for CareerGame {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let named =
            axum::extract::Query::<std::collections::HashMap<String, String>>::try_from_uri(
                &parts.uri,
            )
            .ok()
            .and_then(|q| q.0.get("game").map(|g| g.trim().to_owned()))
            .filter(|g| !g.is_empty());
        Ok(CareerGame(named))
    }
}

impl CareerGame {
    /// The named game, else the game of the account's one live career of `xuid` (`fh2` without any),
    /// unless its careers are in more than one game: then 400 `REQUEST.INVALID {field: "game"}` (no
    /// silent default once a XUID is shared, CAREERS.md §6.1). Read before anything is written.
    pub async fn resolve_in(
        &self,
        conn: &mut sqlx::PgConnection,
        account_id: Uuid,
        xuid: u64,
    ) -> Result<String, ApiError> {
        if let Some(game) = &self.0 {
            return Ok(game.clone());
        }
        let mut games = accounts::career_games_in(conn, account_id, xuid).await?;
        if games.len() > 1 {
            return Err(ApiError::invalid(
                "game",
                "this profile has careers in more than one game: name it with ?game=",
            ));
        }
        // Its one career's game (FH2's for every profile a released launcher holds), else FH2's.
        Ok(games.pop().unwrap_or_else(|| fi_domain::FH2.to_owned()))
    }

    /// [`Self::resolve_in`] for a write, inside the request's transaction (CR-S98 review): the
    /// account row, then the profile `xuid` (`FOR NO KEY UPDATE`, the order every profile write
    /// takes), so no career of another game is created between the answer and the write.
    pub async fn resolve_locked_in(
        &self,
        tx: &mut sqlx::PgConnection,
        account_id: Uuid,
        xuid: u64,
    ) -> Result<String, ApiError> {
        if self.0.is_none() {
            accounts::lock_profile_of_xuid_in(tx, account_id, xuid).await?;
        }
        self.resolve_in(tx, account_id, xuid).await
    }
}

/// The `Idempotency-Key` of a write (required: one UUID per user action, reused by its retries).
pub struct IdempotencyKey(pub Uuid);

impl<S: Send + Sync> FromRequestParts<S> for IdempotencyKey {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .headers
            .get(headers::IDEMPOTENCY_KEY)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Uuid::parse_str(v.trim()).ok())
            .filter(|u| !u.is_nil())
            .map(IdempotencyKey)
            .ok_or_else(|| {
                ApiError::invalid("Idempotency-Key", "a UUID Idempotency-Key is required")
            })
    }
}

impl IdempotencyKey {
    /// The idempotency identity of `body` on `route` (SHA-256 of the canonical body).
    pub fn of<T: Serialize>(&self, route: &str, body: &T) -> Idempotency {
        let mut bytes = route.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes.extend(serde_json::to_vec(body).unwrap_or_default());
        Idempotency {
            key: self.0,
            route: route.to_owned(),
            request_sha256: fi_auth::sha256(&bytes),
        }
    }
}

/// The client network: the client IP's /24 (IPv4) or /56 (IPv6), as a rate-limit subject
/// (`v4:203.0.113`, `v6:<14 hex>`). Never logged; stored only as an HMAC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientNetwork(pub String);

/// The subject of an IP.
pub fn network_of(ip: IpAddr) -> String {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("v4:{}.{}.{}", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let o = v6.octets();
            let hex: String = o[..7].iter().map(|b| format!("{b:02x}")).collect();
            format!("v6:{hex}")
        }
    }
}

/// The rightmost `X-Forwarded-For` entry (the one Google's front end appended, ACCOUNTS.md §9),
/// else the peer address; `unknown` when neither parses (one shared bucket: conservative). This is
/// [`ClientAddress::Forwarded`]; requests resolve through [`resolve_client_ip`].
pub fn client_network(headers: &HeaderMap, peer: Option<SocketAddr>) -> ClientNetwork {
    network_or_unknown(resolve_client_ip(
        &ClientAddress::Forwarded,
        false,
        headers,
        peer,
    ))
}

fn network_or_unknown(ip: Option<IpAddr>) -> ClientNetwork {
    ClientNetwork(ip.map(network_of).unwrap_or_else(|| "unknown".to_owned()))
}

fn rightmost_forwarded(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .rfind(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
}

/// The caller's exact address (Turnstile's `remoteip`, a world host's address; never logged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

fn header_ip(headers: &HeaderMap, name: &str) -> Option<IpAddr> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// The client address (ADR 0903 §1, ACCOUNTS.md §9). A request that came through the edge
/// (`edge`: [`fi_runtime::http::EdgeVerified`], the edge secret matched) is `CF-Connecting-IP`,
/// else the rightmost `X-Forwarded-For` entry, else the peer, whatever `policy` says. Otherwise
/// `policy` (`FI_CLIENT_ADDRESS`) decides: `forwarded` is the rightmost `X-Forwarded-For` entry,
/// else the peer; `peer` is the peer only; `trusted-proxy` reads the edge's chain only from a peer
/// inside `FI_TRUSTED_PROXIES` and is the peer for anyone else, so no other caller can choose its
/// address (review OD-122 M2.3).
pub fn resolve_client_ip(
    policy: &ClientAddress,
    edge: bool,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<IpAddr> {
    let peer = peer.map(|p| p.ip());
    let proxied = || {
        header_ip(headers, "cf-connecting-ip")
            .or_else(|| rightmost_forwarded(headers))
            .or(peer)
    };
    if edge {
        return proxied();
    }
    match policy {
        ClientAddress::Forwarded => rightmost_forwarded(headers).or(peer),
        ClientAddress::Peer => peer,
        ClientAddress::TrustedProxy(_) => match peer {
            Some(p) if policy.trusts(p) => proxied(),
            other => other,
        },
    }
}

/// The policy [`fi_runtime::http::serve`] put in the request; [`ClientAddress::Forwarded`] for a
/// router served in process (tests), as before the setting existed.
fn policy_of(extensions: &axum::http::Extensions) -> &ClientAddress {
    static FORWARDED: ClientAddress = ClientAddress::Forwarded;
    extensions
        .get::<Arc<ClientAddress>>()
        .map_or(&FORWARDED, |p| p.as_ref())
}

/// The client address of a request ([`resolve_client_ip`] with the request's policy).
pub fn client_ip(request: &Request) -> Option<IpAddr> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let edge = request
        .extensions()
        .get::<fi_runtime::http::EdgeVerified>()
        .is_some();
    resolve_client_ip(
        policy_of(request.extensions()),
        edge,
        request.headers(),
        peer,
    )
}

/// Middleware, inside `fi_runtime::http::edge_guard`: puts [`client_ip`] in the request as
/// [`ClientIp`] and its /24 or /56 network as the [`ClientNetwork`] the handlers and the rate
/// limits read.
pub async fn client_context(mut request: Request, next: Next) -> Response {
    let ip = client_ip(&request);
    let network = network_or_unknown(ip);
    if let Some(ip) = ip {
        request.extensions_mut().insert(ClientIp(ip));
    }
    request.extensions_mut().insert(network);
    next.run(request).await
}

/// The outer layers of every public deployable (ADR 0903 §1): the origin lock
/// (`fi_runtime::http::edge_guard`, `/health/*` exempt; nothing locked when `edge` is `None`, the
/// local stack) and, inside it, [`client_context`]. Applied last, so they run first.
pub fn origin_locked(
    router: axum::Router,
    edge: Option<std::sync::Arc<fi_runtime::http::EdgeSecret>>,
) -> axum::Router {
    router
        .layer(axum::middleware::from_fn(client_context))
        .layer(axum::middleware::from_fn_with_state(
            edge,
            fi_runtime::http::edge_guard,
        ))
}

impl<S: Send + Sync> FromRequestParts<S> for ClientNetwork {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        // The portal middleware put the trusted visitor IP here after checking the assertion.
        if let Some(trusted) = parts.extensions.get::<ClientNetwork>() {
            return Ok(trusted.clone());
        }
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        let edge = parts
            .extensions
            .get::<fi_runtime::http::EdgeVerified>()
            .is_some();
        Ok(network_or_unknown(resolve_client_ip(
            policy_of(&parts.extensions),
            edge,
            &parts.headers,
            peer,
        )))
    }
}

/// The web's session cookie (ADR 0207 §3, ADR 0014 §1).
pub const SESSION_COOKIE: &str = "__Host-fi_session";

/// The value of the `__Host-fi_session` cookie, when the request carries one.
pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.trim())
        .filter(|v| !v.is_empty())
}

/// The `Authorization: Bearer` credential, when there is one.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// A launcher request with a valid access token of a live family on a usable account.
#[derive(Clone, Debug)]
pub struct LauncherAuth {
    /// The account.
    pub account_id: Uuid,
    /// The refresh family the token belongs to.
    pub family_id: Uuid,
    /// The account's state at this request.
    pub account: AccountState,
}

impl LauncherAuth {
    /// Rechecks revocation in a session mutation's transaction, before any pool/session lock.
    /// Holds account KEY SHARE until commit so login replacement cannot race a queued poll,
    /// heartbeat resume or ticket issue authenticated before the replacement committed.
    pub async fn require_live_in(&self, conn: &mut sqlx::PgConnection) -> Result<(), ApiError> {
        if fi_db::admission::device_of_family_in(conn, self.account_id, self.family_id)
            .await?
            .is_none()
        {
            return Err(ApiError::new(
                ErrorCode::AuthTokenInvalid,
                "invalid access token",
            ));
        }
        Ok(())
    }
}

/// Marks a request already counted by [`account_rate`] (several extractors may authenticate it).
#[derive(Clone, Copy, Debug)]
struct AccountCounted;

/// The per-account buckets of `/launcher/v1` (ACCOUNTS.md §9, OD-215): every authenticated request
/// takes one token of the account's `any` bucket, a write one more of its `write` bucket, once per
/// request and before any database read. 429 `REQUEST.RATE_LIMITED` with `Retry-After` past either.
/// `/core/v1` keeps its own buckets (fi-core's guard), so it is not counted here.
pub fn account_rate(
    parts: &mut Parts,
    limiter: &crate::config::AccountLimiter,
    account_id: Uuid,
) -> Result<(), ApiError> {
    use fi_runtime::ratelimit::Decision;
    if !parts.uri.path().starts_with("/launcher/v1/")
        || parts.extensions.get::<AccountCounted>().is_some()
    {
        return Ok(());
    }
    parts.extensions.insert(AccountCounted);
    let write = !matches!(
        parts.method,
        axum::http::Method::GET | axum::http::Method::HEAD
    );
    let mut classes = vec![("launcher.account", limiter.rates.any)];
    if write {
        classes.push(("launcher.account_write", limiter.rates.write));
    }
    for (class, rate) in classes {
        if let Decision::Limited { retry_after_secs } =
            limiter.buckets.take(&format!("{class}:{account_id}"), rate)
        {
            tracing::info!(class, "rate limited");
            return Err(ApiError::new(
                ErrorCode::RequestRateLimited,
                "too many requests, retry later",
            )
            .with_details(serde_json::json!({ "limit": class }))
            .retry_after(retry_after_secs));
        }
    }
    Ok(())
}

impl FromRequestParts<AppState> for LauncherAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let invalid = || ApiError::new(ErrorCode::AuthTokenInvalid, "invalid access token");
        let token = bearer(&parts.headers).ok_or_else(invalid)?;
        let verified = state
            .id()
            .access_keys
            .verify(token, Audience::Launcher, now_secs())
            .map_err(|e| match e {
                AccessError::Expired => {
                    ApiError::new(ErrorCode::AuthTokenExpired, "access token expired")
                }
                AccessError::Invalid => invalid(),
            })?;
        let account_id = Uuid::parse_str(&verified.account_id).map_err(|_| invalid())?;
        let family_id = Uuid::parse_str(&verified.family_id).map_err(|_| invalid())?;
        account_rate(parts, &state.id().accounts, account_id)?;
        let account = accounts::account_state(&state.pool, account_id)
            .await?
            .filter(|a| a.status != "deleted")
            .ok_or_else(invalid)?;
        if account.banned {
            return Err(crate::errors::banned(account.ban_review.as_deref()));
        }
        if !tokens::family_live(&state.pool, account_id, family_id).await? {
            return Err(invalid());
        }
        Ok(LauncherAuth {
            account_id,
            family_id,
            account,
        })
    }
}

/// `X-Fi-Client-Assertion` on every `/portal/v1` call (ACCOUNTS.md §6): only the portal's server
/// may call the surface; with a valid assertion `X-Fi-Client-Ip` is the trusted visitor IP.
pub async fn portal_assertion(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let presented = request
        .headers()
        .get(headers::FI_CLIENT_ASSERTION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .as_bytes()
        .to_vec();
    let ok = match &state.id().portal_assertion {
        Some(PortalAssertion::LocalSharedSecret(secret)) => fi_auth::ct_eq(&presented, secret),
        None => false,
    };
    if !ok {
        return Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "the portal surface serves the portal's server only",
        ));
    }
    let visitor = request
        .headers()
        .get(headers::FI_CLIENT_IP)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    if let Some(ip) = visitor {
        request
            .extensions_mut()
            .insert(ClientNetwork(network_of(ip)));
    }
    Ok(next.run(request).await)
}

/// A portal request with a live portal session token: `Authorization: Bearer fi_ps_...` (the
/// deprecated BFF) or, without an `Authorization` header, the web's `__Host-fi_session` cookie.
#[derive(Clone, Debug)]
pub struct PortalAuth {
    /// The account.
    pub account_id: Uuid,
    /// The session's family.
    pub family_id: Uuid,
    /// The account's state at this request.
    pub account: AccountState,
}

impl FromRequestParts<AppState> for PortalAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let required =
            || ApiError::new(ErrorCode::PortalSessionRequired, "portal session required");
        let presented = match parts.headers.get(axum::http::header::AUTHORIZATION) {
            Some(_) => bearer(&parts.headers),
            None => session_cookie(&parts.headers),
        };
        let lookup = presented
            .and_then(|t| opaque::lookup(Kind::PortalSession, t))
            .ok_or_else(required)?;
        let session = tokens::portal_session(&state.pool, &lookup)
            .await?
            .ok_or_else(required)?;
        let account = accounts::account_state(&state.pool, session.account_id)
            .await?
            .filter(|a| a.status != "deleted")
            .ok_or_else(required)?;
        if account.banned {
            return Err(crate::errors::banned(account.ban_review.as_deref()));
        }
        Ok(PortalAuth {
            account_id: session.account_id,
            family_id: session.family_id,
            account,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_protocol::accounts::TokenGrant;

    #[test]
    fn bodies_name_the_bad_field_and_never_echo_values() {
        let e = parse::<TokenGrant>(br#"{"grantType":"id_token","idToken":"t","device":{"deviceId":"x","label":"l","platform":"p"},"extra":"secret-value"}"#).err().unwrap();
        assert_eq!(e.details.unwrap()["field"], "extra");
        assert!(!e.message.contains("secret-value"));
        let e = parse::<fi_protocol::accounts::Refresh>(br#"{}"#)
            .err()
            .unwrap();
        assert_eq!(e.details.unwrap()["field"], "refreshToken");
        let e = parse::<fi_protocol::accounts::LauncherAccountCreate>(
            br#"{"identityTicket":"t","device":{"deviceId":"x","label":"l","platform":"p","os":"w"},"profile":{"game":"fh2","name":"N","mode":"noob"}}"#,
        )
        .err()
        .unwrap();
        assert_eq!(e.details.unwrap()["field"], "device.os");
        let e = parse::<fi_protocol::accounts::Refresh>(br#"{"refreshToken": 5}"#)
            .err()
            .unwrap();
        assert_eq!(e.details.unwrap()["field"], "refreshToken");
        let e = parse::<fi_protocol::accounts::Refresh>(b"not json")
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::RequestInvalid);
        let big = vec![b' '; MAX_BODY + 1];
        assert_eq!(
            parse::<fi_protocol::accounts::Refresh>(&big)
                .err()
                .unwrap()
                .code,
            ErrorCode::RequestTooLarge
        );
    }

    #[test]
    fn the_session_cookie_is_found_by_its_exact_name() {
        let mut h = HeaderMap::new();
        h.append(
            "cookie",
            "theme=dark; x__Host-fi_session=evil; __Host-fi_session=fi_ps_abc"
                .parse()
                .unwrap(),
        );
        assert_eq!(session_cookie(&h), Some("fi_ps_abc"));
        let mut h = HeaderMap::new();
        h.append("cookie", "a=b".parse().unwrap());
        h.append("cookie", "__Host-fi_session=".parse().unwrap());
        assert_eq!(session_cookie(&h), None);
        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }

    #[test]
    fn the_client_network_is_the_rightmost_forwarded_entry() {
        let mut h = HeaderMap::new();
        h.append(
            "x-forwarded-for",
            "198.51.100.7, 203.0.113.9".parse().unwrap(),
        );
        assert_eq!(client_network(&h, None).0, "v4:203.0.113");
        h.append("x-forwarded-for", "192.0.2.200".parse().unwrap());
        assert_eq!(
            client_network(&h, None).0,
            "v4:192.0.2",
            "the last header's last entry"
        );
        let peer: SocketAddr = "10.1.2.3:5000".parse().unwrap();
        assert_eq!(client_network(&HeaderMap::new(), Some(peer)).0, "v4:10.1.2");
        assert_eq!(client_network(&HeaderMap::new(), None).0, "unknown");
        assert_eq!(
            network_of("2001:db8:abcd:12ff::1".parse().unwrap()),
            "v6:20010db8abcd12"
        );
        assert_eq!(
            network_of("::ffff:203.0.113.5".parse().unwrap()),
            "v4:203.0.113"
        );
        let mut bad = HeaderMap::new();
        bad.append("x-forwarded-for", "not-an-ip".parse().unwrap());
        assert_eq!(client_network(&bad, Some(peer)).0, "v4:10.1.2");
    }

    fn forged() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", "198.51.100.7".parse().unwrap());
        h.append("cf-connecting-ip", "203.0.113.77".parse().unwrap());
        h
    }

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn peer_mode_ignores_forged_forwarding_headers() {
        let peer: SocketAddr = "192.0.2.50:40000".parse().unwrap();
        let p = ClientAddress::Peer;
        assert_eq!(
            resolve_client_ip(&p, false, &forged(), Some(peer)),
            ip("192.0.2.50")
        );
        assert_eq!(resolve_client_ip(&p, false, &forged(), None), None);
        assert_eq!(
            resolve_client_ip(&ClientAddress::Forwarded, false, &forged(), Some(peer)),
            ip("198.51.100.7"),
            "forwarded (local default) keeps today's rule and ignores CF-Connecting-IP"
        );
    }

    #[test]
    fn trusted_proxy_reads_headers_only_from_a_listed_peer() {
        let p = ClientAddress::TrustedProxy(vec![
            fi_runtime::http::Cidr::parse("172.30.122.10/32").unwrap(),
        ]);
        let tunnel: SocketAddr = "172.30.122.10:51000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:172.30.122.10]:51000".parse().unwrap();
        let lan: SocketAddr = "172.30.122.1:51000".parse().unwrap();
        assert_eq!(
            resolve_client_ip(&p, false, &forged(), Some(tunnel)),
            ip("203.0.113.77")
        );
        assert_eq!(
            resolve_client_ip(&p, false, &forged(), Some(mapped)),
            ip("203.0.113.77")
        );
        let mut xff_only = HeaderMap::new();
        xff_only.append(
            "x-forwarded-for",
            "198.51.100.7, 198.51.100.9".parse().unwrap(),
        );
        assert_eq!(
            resolve_client_ip(&p, false, &xff_only, Some(tunnel)),
            ip("198.51.100.9")
        );
        assert_eq!(
            resolve_client_ip(&p, false, &HeaderMap::new(), Some(tunnel)),
            ip("172.30.122.10")
        );
        assert_eq!(
            resolve_client_ip(&p, false, &forged(), Some(lan)),
            ip("172.30.122.1"),
            "an unlisted peer is its own address, whatever it claims"
        );
        assert_eq!(resolve_client_ip(&p, false, &forged(), None), None);
    }

    #[test]
    fn edge_verified_requests_keep_the_edge_rule_under_every_mode() {
        let peer: SocketAddr = "192.0.2.50:40000".parse().unwrap();
        let mut xff_only = HeaderMap::new();
        xff_only.append("x-forwarded-for", "198.51.100.9".parse().unwrap());
        for p in [
            ClientAddress::Forwarded,
            ClientAddress::Peer,
            ClientAddress::TrustedProxy(vec![fi_runtime::http::Cidr::parse("10.0.0.0/8").unwrap()]),
        ] {
            assert_eq!(
                resolve_client_ip(&p, true, &forged(), Some(peer)),
                ip("203.0.113.77")
            );
            assert_eq!(
                resolve_client_ip(&p, true, &xff_only, Some(peer)),
                ip("198.51.100.9")
            );
            assert_eq!(
                resolve_client_ip(&p, true, &HeaderMap::new(), Some(peer)),
                ip("192.0.2.50")
            );
        }
    }

    #[tokio::test]
    async fn the_middleware_and_the_extractor_read_the_served_policy() {
        use axum::routing::get;
        use tower::ServiceExt;
        let app = |policy: Option<ClientAddress>| {
            let router = axum::Router::new()
                .route(
                    "/ctx",
                    get(|axum::Extension(n): axum::Extension<ClientNetwork>| async move { n.0 }),
                )
                .layer(axum::middleware::from_fn(client_context))
                .merge(
                    axum::Router::new().route("/bare", get(|n: ClientNetwork| async move { n.0 })),
                );
            match policy {
                Some(p) => router.layer(axum::Extension(Arc::new(p))),
                None => router,
            }
        };
        let call = |policy: Option<ClientAddress>, path: &'static str| async move {
            let mut req = Request::builder()
                .uri(path)
                .header("x-forwarded-for", "198.51.100.7")
                .body(axum::body::Body::empty())
                .unwrap();
            req.extensions_mut().insert(ConnectInfo::<SocketAddr>(
                "192.0.2.50:40000".parse().unwrap(),
            ));
            let res = app(policy).oneshot(req).await.unwrap();
            let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        };
        for path in ["/ctx", "/bare"] {
            assert_eq!(call(None, path).await, "v4:198.51.100", "{path}");
            assert_eq!(
                call(Some(ClientAddress::Peer), path).await,
                "v4:192.0.2",
                "{path}"
            );
        }
    }
}
