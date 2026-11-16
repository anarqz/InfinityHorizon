//! The game's free-roam hosts on the game plane (docs/contracts/MATCHMAKING_API.md §4a; SER-M3
//! step 5): gameserverds, gameserverms, sessiondirectory (MPSD) and momatch (SmartMatch), the
//! Xbox Live calls that take the game into online free roam and to a world's relay. Ported from
//! the Beta's `freeroam.rs` (itself from fh2-mods' `fh2srv.py`, MIT), with everything the Beta
//! kept in memory in Postgres (`fi_db::freeroam`), so any instance answers any request.
//!
//! The proxy dispatcher (GAME_PLATFORM.md §3a, dispatch step 1) calls [`handle`] with a request
//! whose ticket and headers it already checked; `None` means the host is not one of these four.
//! Both `vip` (saves) and `matchmaking` (free) tickets are served; which worlds a caller may be
//! sent to follows matchmaking's rules (the hosted world, the launcher's grant). Answers:
//!
//! - the emulated services' own statuses (200, 201, 204, and 404 `{}` for a session or ticket
//!   that does not exist for the caller), JSON with an `ETag`, passed to the game as they are;
//! - refusals as the contract's envelope: 404 `GAME.NOT_SERVED` (another path or method of these
//!   hosts, a ticket for a hopper other than `FreeRoam_qos` and an explicit `CarMeet_qos`), 400 `REQUEST.INVALID`, 413
//!   `REQUEST.TOO_LARGE`, and 503 `SERVICE.UNAVAILABLE` with `Retry-After: 5` for a FreeRoam
//!   ticket with no world for the caller (the Beta's 503: the game shows its connection error).
//!
//! Settings (read once from the environment, [`Settings::from_env`]): `FI_FREEROAM_AUTO`
//! (`on`|`off`, default off) and `FI_FREEROAM_WORLD` (`<ip>:<port>`, default unset).

mod doc;
mod gameserver;
pub mod invite;
mod mpsd;
mod smartmatch;
#[cfg(test)]
mod tests;

use crate::config::{Env, ProcessEnv};
use crate::errors::ApiError;
use crate::game::request::GameRequest;
use crate::state::AppState;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fi_db::freeroam::{Caller, RouteOptions};
use fi_protocol::ErrorCode;
use fi_runtime::config::ConfigError;
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::net::SocketAddr;
use std::sync::OnceLock;

pub use doc::{ADDRESS_PREFIX, FH2_SESSION, FM5_SESSION, SessionProfile, secure_device_address};

/// The game server directory.
pub const GAMESERVERDS: &str = "gameserverds.xboxlive.com";
/// The game server manager (clusters).
pub const GAMESERVERMS: &str = "gameserverms.xboxlive.com";
/// The multiplayer session directory.
pub const SESSIONDIRECTORY: &str = "sessiondirectory.xboxlive.com";
/// SmartMatch.
pub const MOMATCH: &str = "momatch.xboxlive.com";
/// The four hosts this module answers.
pub const HOSTS: [&str; 4] = [GAMESERVERDS, GAMESERVERMS, SESSIONDIRECTORY, MOMATCH];
/// Largest body, and largest session document once merged (serialized).
pub const MAX_DOC: usize = 128 << 10;
/// `Retry-After` of a FreeRoam ticket with no world for the caller.
pub const NO_WORLD_RETRY_SECS: u64 = 5;

/// The deployment's free-roam settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// `FI_FREEROAM_AUTO=on`: send a caller with no hosted world and no grant to the best open
    /// world, taking a grant for it (off by default: the joiner's hook gets no relay grant that
    /// way, MATCHMAKING_API.md §4a).
    pub auto: bool,
    /// `FI_FREEROAM_WORLD=<ip>:<port>`: a fixed world for a caller with no other.
    pub fallback: Option<SocketAddr>,
}

impl Settings {
    /// Reads `FI_FREEROAM_AUTO` and `FI_FREEROAM_WORLD`.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        let auto = match env.get("FI_FREEROAM_AUTO").as_deref().map(str::trim) {
            None | Some("off") => false,
            Some("on") => true,
            Some(_) => {
                return Err(ConfigError::Invalid {
                    name: "FI_FREEROAM_AUTO",
                    reason: "on or off".into(),
                });
            }
        };
        let fallback = env
            .get("FI_FREEROAM_WORLD")
            .map(|v| {
                v.trim()
                    .parse::<SocketAddr>()
                    .ok()
                    .filter(|a| a.port() != 0 && !a.ip().is_unspecified())
                    .ok_or(ConfigError::Invalid {
                        name: "FI_FREEROAM_WORLD",
                        reason: "<ip>:<port>, IPv6 in brackets".into(),
                    })
            })
            .transpose()?;
        Ok(Self { auto, fallback })
    }

    /// The routing options of `fi_db::freeroam` for a FreeRoam ticket, the one request that may
    /// choose a world automatically (it takes a grant for it first).
    pub fn route(&self) -> RouteOptions {
        RouteOptions {
            auto: self.auto,
            fallback: self.fallback,
        }
    }

    /// The routing options of sessions and clusters: never an automatic choice, so no address
    /// goes out without a grant (the ticket's grant is then found by the grant step).
    pub fn route_without_auto(&self) -> RouteOptions {
        RouteOptions {
            auto: false,
            fallback: self.fallback,
        }
    }

    /// The process's settings, read once (an invalid value is logged and treated as unset;
    /// deployables call [`Settings::from_env`] at startup to refuse it instead).
    pub fn current() -> &'static Settings {
        static CURRENT: OnceLock<Settings> = OnceLock::new();
        CURRENT.get_or_init(|| {
            Settings::from_env(&ProcessEnv).unwrap_or_else(|e| {
                tracing::error!(error = %e, "free-roam settings ignored: automatic and fallback worlds off");
                Settings::default()
            })
        })
    }
}

/// Whether `host` (lower case) is one of the four free-roam hosts.
pub fn is_freeroam_host(host: &str) -> bool {
    HOSTS.contains(&host)
}

/// One proxied game request for a free-roam host; `None` when `req.host` is not one.
pub async fn handle(state: &AppState, req: &GameRequest<'_>) -> Option<Response> {
    if !is_freeroam_host(req.host) {
        return None;
    }
    handle_with(state, req, Settings::current()).await
}

/// [`handle`] with explicit settings (tests, and deployables that read them at startup).
pub async fn handle_with(
    state: &AppState,
    req: &GameRequest<'_>,
    settings: &Settings,
) -> Option<Response> {
    let caller = caller(req);
    // The ticket's game picks the session values (FM5_LOBBIES.md §3).
    let profile = SessionProfile::of(req.ticket.game());
    let answer = match req.host {
        GAMESERVERDS => gameserver::directory(req, profile),
        GAMESERVERMS => gameserver::clusters(state, req, &caller, settings, profile).await,
        SESSIONDIRECTORY => mpsd::handle(state, req, &caller, settings, profile).await,
        // SmartMatch is FH2's only (car meet, Road Trip, its hoppers).
        MOMATCH if profile.fh2_lobbies => smartmatch::handle(state, req, &caller, settings).await,
        MOMATCH => Err(not_served(req.host, None)),
        _ => return None,
    };
    if let Err(error) = &answer
        && let Some(suppressed) = crate::diagnostics::poll(crate::diagnostics::Poll::Refusal)
    {
        crate::network_warn!(event = "freeroam_refused", session_id = %caller.session_id,
            service_host = req.host, operation = diagnostic_method(req.method), suppressed,
            code = error.code.code(), status = error.code.status(), "free-roam request refused");
    }
    Some(answer.unwrap_or_else(IntoResponse::into_response))
}

/// The ticket's player and the request's address.
fn caller(req: &GameRequest<'_>) -> Caller {
    let t = req.ticket;
    Caller {
        game: t.game().to_owned(),
        profile_id: t.live.profile_id,
        account_id: t.live.account_id,
        session_id: t.live.session_id,
        xuid: t.xuid,
        tier: if t.claims.tier == "vip" {
            "vip"
        } else {
            "free"
        }
        .to_owned(),
        mode: t.claims.mode.clone(),
        ip: req.client_ip.map(|ip| ip.to_canonical()),
    }
}

/// The caller as a session member (its gamertag read on the request's connection).
async fn me_for(conn: &mut PgConnection, caller: &Caller) -> Result<doc::Me, ApiError> {
    let tag = fi_db::freeroam::gamertag_in(conn, caller.profile_id).await?;
    Ok(doc::Me {
        xuid: caller.xuid,
        gamertag: doc::game_tag(tag.as_deref(), caller.xuid),
        device_token: doc::device_token(caller.session_id, caller.profile_id),
        title_id: SessionProfile::of(&caller.game).title_id,
    })
}

/// The path's non-empty segments.
fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// A JSON answer of the emulated service, with its `ETag`.
fn json_answer(status: StatusCode, body: &Value, etag: i64) -> Response {
    let bytes =
        if body.get(doc::FI_OFFICIAL).is_some() || body.get(doc::FI_HOST_CLAIMS).is_some() {
            let mut shown = body.clone();
            doc::strip_server_fields(&mut shown);
            serde_json::to_vec(&shown)
        } else {
            serde_json::to_vec(body)
        }
        .unwrap_or_else(|_| b"{}".to_vec());
    let mut response = (status, bytes).into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("\"{etag}\"")) {
        h.insert(header::ETAG, v);
    }
    response
}

/// The service's own 404: no such session or ticket for the caller.
fn missing() -> Response {
    json_answer(StatusCode::NOT_FOUND, &json!({}), 0)
}

/// 204, no body.
fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// 404 `GAME.NOT_SERVED {host, …}`.
fn not_served(host: &str, extra: Option<(&str, &str)>) -> ApiError {
    let mut details = json!({ "host": host });
    if let Some((k, v)) = extra {
        details[k] = json!(v);
    }
    ApiError::new(
        ErrorCode::GameNotServed,
        "this free-roam request is not served",
    )
    .with_details(details)
}

/// 413 `REQUEST.TOO_LARGE`.
fn too_large() -> ApiError {
    ApiError::new(ErrorCode::RequestTooLarge, "over 128 KiB")
}

/// A document Postgres would refuse or grow past the limit: 400 or 413, before any write.
fn check_storable(doc: &Value) -> Result<(), ApiError> {
    doc::storable(doc, MAX_DOC).map_err(|e| match e {
        doc::Unstorable::Nul => ApiError::invalid("body", "no U+0000 characters"),
        doc::Unstorable::TooLarge => too_large(),
    })
}

/// A write's database error: a value Postgres refuses (a check, an untranslatable character) is
/// the caller's 400 or 413, never a 5xx; anything else is the usual mapping.
fn stored(e: fi_db::identity::IdentityError) -> ApiError {
    if let fi_db::identity::IdentityError::Db(err) = &e {
        match err.as_database_error().and_then(|d| d.code()).as_deref() {
            Some("23514") => return too_large(),
            Some("22P05" | "22P02") => {
                return ApiError::invalid("body", "a value the store refuses");
            }
            _ => {}
        }
    }
    e.into()
}

fn diagnostic_method(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        _ => "OTHER",
    }
}

/// Only a fixed route class is observable, never its address or port.
fn route_class(ip: std::net::IpAddr) -> &'static str {
    if ip.is_loopback() {
        return "loopback";
    }
    match ip {
        std::net::IpAddr::V4(v) if v.is_private() || v.is_link_local() => "lan",
        std::net::IpAddr::V6(v) if v.is_unique_local() || v.is_unicast_link_local() => "lan",
        _ => "public",
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn settings_are_off_by_default_and_refuse_nonsense() {
        let none: Vec<(&'static str, String)> = vec![];
        assert_eq!(Settings::from_env(&none).unwrap(), Settings::default());
        let on = vec![
            ("FI_FREEROAM_AUTO", "on".to_owned()),
            ("FI_FREEROAM_WORLD", "[2001:db8::1]:5101".to_owned()),
        ];
        let s = Settings::from_env(&on).unwrap();
        assert!(s.auto);
        assert_eq!(s.fallback, "[2001:db8::1]:5101".parse().ok());
        assert_eq!(s.route().fallback, s.fallback);
        for (k, v) in [
            ("FI_FREEROAM_AUTO", "yes"),
            ("FI_FREEROAM_WORLD", "192.0.2.1"),
            ("FI_FREEROAM_WORLD", "192.0.2.1:0"),
            ("FI_FREEROAM_WORLD", "0.0.0.0:5101"),
            ("FI_FREEROAM_WORLD", "host.example:5101"),
        ] {
            assert!(
                Settings::from_env(&vec![(k, v.to_owned())]).is_err(),
                "{k}={v}"
            );
        }
        assert!(is_freeroam_host(MOMATCH) && !is_freeroam_host("profile.xboxlive.com"));
        assert_eq!(segments("/a//b/"), vec!["a", "b"]);
    }
}
