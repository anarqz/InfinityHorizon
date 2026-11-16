//! The game's own proxied requests (GAME_PLATFORM.md §3a; SER-M3 steps 3-4): the hook rewrites
//! every request of the game to `{claims.server}/{host}/{path}[?query]` with the game access
//! ticket, and this catch-all answers them as the Beta's pipeline did
//! (`deprecated/beta/server/src/main.rs:574-909`).
//!
//! **Mounting.** [`serve`] is the router's **fallback** (`Router::fallback`, set before
//! `with_state`), not a `/{host}/{*path}` route: it never competes with a deployment's own routes
//! (`/game/v1`, `/saves/v1`, `/mm/v1`, `/health/*`, ...), and a path whose first segment is not a
//! host name (no dot) keeps the plain 404 of an unknown route. The deployment installs a [`Proxy`]
//! extension (its [`Features`] and hook minimum) next to `ticket::Tickets`.
//!
//! **What a request is served** is decided per request ([`Features::for_ticket`], OD-52): the
//! deployment's features narrowed by the ticket's `tier` and `mode` claims, right after the ticket
//! is verified, and used by every step below (the body limit, the foreign-XUID rule, the Forza
//! RPC). A Supporter's noob ticket and a lapsed Supporter's get the free behaviour on saves.
//!
//! **Checks, in the order of the §3a header table** (each refusal a code, never a 5xx): the ticket
//! ([`Ticket`]: signature 401 `AUTH.SESSION_EXPIRED`, pool and game 403 `GAME.WRONG_POOL`, `exp`
//! and liveness 401); `X-Fh2-Original-Url` present, `http(s)`, and agreeing with the rewritten
//! `{host}/{path}` (host case and port ignored, paths compared percent-decoded), else 400
//! `REQUEST.INVALID {field: "X-Fh2-Original-Url"}`; `X-Fh2-Request-Id` a UUID or the Beta hook's
//! decimal counter (1 to 20 digits), else 400 `{field: "X-Fh2-Request-Id"}` (facts derived from a
//! request take [`target::fact_id`]: `UUIDv5(session, "req:" + id)`); `X-Fi-Hook` at least the configured minimum (`FI_MIN_HOOK`;
//! none configured: any), else 403 `GAME.UPDATE_REQUIRED {minHook}`; `X-Fh2-CarDb` equal to the
//! ticket's `cdb` (case-insensitive) when that claim is not empty, else 403 `GAME.CAR_DB_REFUSED`;
//! then, from the headers alone ([`body_limit`]), a request the request's features do not serve is 404
//! `GAME.NOT_SERVED` and the body's limit is set (64 KiB for JSON and free-roam hosts, the method's
//! cap for a Forza RPC, 1 MiB unless listed); a body over it (declared or read) is 413
//! `REQUEST.TOO_LARGE` before anything parses it; then a request naming a XUID other than the
//! ticket's is 403 `GAME.FOREIGN_XUID {host}` ([`foreign`]: on a request served `Free` any such
//! request, on a Supporter's the writes and impersonation; OD-44, CR-S15).
//!
//! **Dispatch** on the original URL (host lower case, path percent-decoded, query as sent): (1)
//! [`crate::game::freeroam::handle`]; (2) Forza RPC ([`rpc`]) when `X-ClassName` and `X-Method`
//! are present, what the request's [`Features`] serve of it ([`forza::served`], the one
//! allowlist); (3) the Xbox Live JSON hosts ([`live`]); (4) [`stubs`]; anything else is 404
//! `GAME.NOT_SERVED {host, class?, method?}`, which the hook turns into its offline 503. Steps (1),
//! (3) and (4) are the same on both deployments; the Forza RPC differ. A ticket of Forza Motorsport
//! 5 takes none of this dispatch after the header checks: [`fm5`] is its own allowlist.

pub mod fm5;
pub mod foreign;
pub mod forza;
pub mod gamerpic;
pub mod live;
pub mod liveries;
pub mod rpc;
pub mod stubs;
pub mod target;
pub mod universe;

#[cfg(test)]
mod golden;

use crate::errors::ApiError;
use crate::extract::ClientIp;
use crate::game::request::GameRequest;
use crate::game::ticket::{Ticket, TicketClaims};
use crate::state::{AppState, Platform};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fi_protocol::{ErrorCode, headers};
use fi_runtime::config::ConfigError;
use forza::Lang;
use serde_json::json;
use std::sync::Arc;
use target::HookVersion;

/// The ceiling of every body the proxy reads (a Forza RPC upload, a livery or a ghost, stays far
/// below); each request has its own, smaller limit ([`body_limit`]).
pub const MAX_BODY: usize = 8 * 1024 * 1024;

/// The body limit of the Xbox Live JSON hosts, the gamerpic and stub hosts and the free-roam
/// hosts: the game's bodies there stay under 1 KiB (Beta captures: 825 bytes at most), and a JSON
/// body is parsed whole, so the cap bounds what one parse holds.
pub const JSON_BODY: usize = 64 * 1024;

/// The free-roam hosts, dispatch step (1): [`crate::game::freeroam::handle`] answers only these.
pub const FREE_ROAM_HOSTS: [&str; 4] = [
    "gameserverds.xboxlive.com",
    "gameserverms.xboxlive.com",
    "sessiondirectory.xboxlive.com",
    "momatch.xboxlive.com",
];

/// The body limit of a request, from what its headers name, or its refusal before the body is
/// read: a free-roam host [`JSON_BODY`]; a Forza RPC (`X-ClassName` and `X-Method`) the
/// deployment's `features` do not serve ([`forza::served`]) 404 `GAME.NOT_SERVED`, a served one
/// [`rpc::body_cap`] (the same cap on both deployments); the Xbox Live JSON, gamerpic and stub
/// hosts [`JSON_BODY`] (a path they do not answer is still 404 after the read); any other host 404.
pub fn body_limit(
    features: Features,
    host: &str,
    class: Option<&str>,
    method: Option<&str>,
) -> Result<usize, ApiError> {
    if FREE_ROAM_HOSTS.contains(&host) {
        return Ok(JSON_BODY);
    }
    match (class, method) {
        (Some(class), Some(method)) => rpc::lookup(features, class, method)
            .map(|r| rpc::body_cap(r.key, r.method))
            .ok_or_else(|| not_served(host, Some(rpc::class_key(class)), Some(method))),
        _ if live::HOSTS.contains(&host) || stubs::for_host(host).is_some() => Ok(JSON_BODY),
        _ => Err(not_served(host, None, None)),
    }
}

/// What is served of the proxied requests (GAME_PLATFORM.md §3a "Free tickets", "Cloud
/// eligibility"). A deployment declares what it can serve ([`ProxyConfig::features`]); each request
/// is served [`Features::for_ticket`] of it, which also reads the ticket. Both serve free roam, the
/// Xbox Live JSON hosts and the stubs; the Forza RPC differ ([`forza::served`]). The pool's
/// `gameEndpoint` is what sends a game to its deployment: a `vip` session's game reaches saves,
/// never matchmaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Features {
    /// Everything built, the profile stored (services/saves, pool `vip`): every ticket's on that
    /// deployment (OD-129).
    Supporter,
    /// The free game plane (services/matchmaking, pool `matchmaking`; PV1-Q11, OD-34): the free
    /// allowlist of the Forza RPC, nothing universe-shaped but the drivatars and Car Clubs
    /// (CR-S70), the profile sync
    /// stored (OD-128). A deployment property only: no ticket is narrowed to it (OD-129).
    Free,
}

impl Features {
    /// What one request is served: the deployment's features, whatever the ticket's tier, mode or
    /// save line (OD-129: everything free for everyone, Supporter a badge; it replaced OD-52's
    /// narrowing of a ticket without a line to [`Features::Free`]).
    pub fn for_ticket(self, _claims: &TicketClaims, _live_cloud: bool) -> Features {
        self
    }
}

/// The proxy's configuration, an `axum::Extension<Proxy>` of the deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    /// What is served.
    pub features: Features,
    /// The oldest hook allowed (`FI_MIN_HOOK`), as configured and parsed; `None`: any.
    pub min_hook: Option<(String, HookVersion)>,
}

impl ProxyConfig {
    /// The configuration of a deployment; `Err` names a `min_hook` that is not a version.
    pub fn new(features: Features, min_hook: Option<&str>) -> Result<Self, String> {
        let min_hook = match min_hook.map(str::trim).filter(|m| !m.is_empty()) {
            Some(text) => Some((
                text.to_owned(),
                HookVersion::parse(text)
                    .ok_or_else(|| format!("{text:?} is not a hook version"))?,
            )),
            None => None,
        };
        Ok(Self { features, min_hook })
    }
}

/// The shared configuration.
pub type Proxy = Arc<ProxyConfig>;

/// `FI_MIN_HOOK` (the oldest hook the proxied requests accept; unset: any), checked to be a hook
/// version. Read by the deployments that mount [`serve`].
pub fn min_hook_from_env(env: &dyn crate::config::Env) -> Result<Option<String>, ConfigError> {
    let value = env
        .get("FI_MIN_HOOK")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    if let Some(v) = &value
        && HookVersion::parse(v).is_none()
    {
        return Err(ConfigError::Invalid {
            name: "FI_MIN_HOOK",
            reason: "a hook version, like 0.3.0+fh2online.60".to_owned(),
        });
    }
    Ok(value)
}

/// 404 `GAME.NOT_SERVED` with `details: {host, class?, method?}`.
pub fn not_served(host: &str, class: Option<&str>, method: Option<&str>) -> ApiError {
    let clip = |s: &str| s.chars().take(128).collect::<String>();
    let mut details = json!({ "host": clip(host) });
    if let Some(c) = class {
        details["class"] = clip(c).into();
    }
    if let Some(m) = method {
        details["method"] = clip(m).into();
    }
    ApiError::new(
        ErrorCode::GameNotServed,
        "this deployment does not serve that request",
    )
    .with_details(details)
}

fn header_text<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The fallback of a deployment that serves the proxied game requests.
pub async fn serve(State(state): State<AppState>, request: Request) -> Response {
    let Some((segment, _)) = target::split_rewritten(request.uri().path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let host = segment.to_ascii_lowercase();
    // OD-121: the activity report's answer, added to a served response only.
    let mut activity = None;
    match proxy(&state, request, &mut activity).await {
        Ok(mut r) => {
            if let Some(v) = activity {
                r.headers_mut().insert(crate::game::activity::RESPONSE, v);
            }
            r
        }
        Err(e) => {
            tracing::debug!(host = %host, code = e.code.code(), "proxied request refused");
            e.into_response()
        }
    }
}

async fn proxy(
    state: &AppState,
    request: Request,
    activity: &mut Option<axum::http::HeaderValue>,
) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();
    let cfg = parts
        .extensions
        .get::<Proxy>()
        .cloned()
        .ok_or_else(ApiError::unavailable)?;
    let ticket = Ticket::from_request_parts(&mut parts, state).await?;
    // OD-52: what this request is served, from the deployment and the ticket's own claims.
    let features = cfg.features.for_ticket(&ticket.claims, ticket.live.cloud);
    let path = parts.uri.path().to_owned();
    let (seg_host, seg_path) = target::split_rewritten(&path)
        .ok_or_else(|| ApiError::invalid("X-Fh2-Original-Url", "the path is not /{host}/{path}"))?;
    let original = header_text(&parts.headers, headers::FH2_ORIGINAL_URL)
        .and_then(target::parse_original)
        .filter(|o| target::agrees(seg_host, seg_path, o))
        .ok_or_else(|| {
            ApiError::invalid(
                "X-Fh2-Original-Url",
                "an http(s) URL whose host and path are the request's {host}/{path}",
            )
        })?;
    let request_id = header_text(&parts.headers, headers::FH2_REQUEST_ID)
        .and_then(target::request_id)
        .ok_or_else(|| {
            ApiError::invalid(
                "X-Fh2-Request-Id",
                "a request id is required: a UUID or a decimal counter of 1 to 20 digits",
            )
        })?;
    if let Some((text, min)) = &cfg.min_hook {
        let hook = header_text(&parts.headers, headers::FI_HOOK).and_then(HookVersion::parse);
        if hook.is_none_or(|h| h < *min) {
            return Err(ApiError::new(
                ErrorCode::GameUpdateRequired,
                "this hook is too old for the game plane: update the launcher",
            )
            .with_details(json!({ "minHook": text })));
        }
    }
    let cdb = ticket.claims.cdb.trim();
    if !cdb.is_empty() {
        let sent = header_text(&parts.headers, headers::FH2_CAR_DB).unwrap_or_default();
        if !sent.eq_ignore_ascii_case(cdb) {
            return Err(ApiError::new(
                ErrorCode::GameCarDbRefused,
                "the car database is not the one this session was admitted with",
            ));
        }
    }
    // OD-121: the hook's activity report, applied whatever the request; it never refuses it.
    *activity = crate::game::activity::apply(&state.pool, &ticket.claims.sid, &parts.headers).await;
    // What is served, and how large its body may be, is decided from the headers: a request this
    // request's features do not serve, or a body over its limit, is refused before the body is
    // read.
    let class = header_text(&parts.headers, "x-classname");
    let method = header_text(&parts.headers, "x-method");
    // The ticket's game picks the table (GAME_PLATFORM.md "Per-game behaviour").
    let is_fm5 = fi_domain::GameRules::of(ticket.game()).proxy == fi_domain::game::Proxy::Fm5;
    let limit = if is_fm5 {
        fm5::body_limit(&original.host, class, method)
    } else {
        body_limit(features, &original.host, class, method)
    }?
    .min(MAX_BODY);
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let too_large = || ApiError::new(ErrorCode::RequestTooLarge, "the body is too large");
    if declared.is_some_and(|n| n > limit as u64) {
        return Err(too_large());
    }
    let body = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| too_large())?;
    let client_ip = parts.extensions.get::<ClientIp>().map(|c| c.0);
    // Handlers match paths as text: percent-decoded (kept as sent if that is not UTF-8).
    let path = String::from_utf8(target::percent_decode(&original.path))
        .unwrap_or_else(|_| original.path.clone());
    let req = GameRequest {
        ticket: &ticket,
        method: &parts.method,
        host: &original.host,
        path: &path,
        query: original.query.as_deref(),
        headers: &parts.headers,
        body: &body,
        client_ip,
    };
    // Only the ticket's profile talks to the server (OD-44): decided on what the request names,
    // before any handler sees it.
    foreign::check(is_fm5 || features == Features::Free, &req)?;
    if is_fm5 {
        let lang = Lang::from_header(header_text(&parts.headers, headers::FH2_LANG));
        return fm5::answer(state, &req, class, method, request_id, lang).await;
    }
    if let Some(r) = crate::game::freeroam::handle(state, &req).await {
        return Ok(r);
    }
    let lang = Lang::from_header(header_text(&parts.headers, headers::FH2_LANG));
    if let (Some(class), Some(method)) = (class, method) {
        // The deployment's bucket (ghost recordings), when it has one: saves does; and the
        // guardrails' enforcement switch (OD-62).
        let platform = parts.extensions.get::<Platform>();
        let deployment = rpc::Deployment {
            blobs: platform.and_then(|p| p.blobs.as_ref()),
            guard_enforce: platform.is_some_and(|p| p.guard_enforce),
        };
        let answer = rpc::answer(
            state, &req, features, class, method, request_id, lang, deployment,
        )
        .await;
        // OD-121: a skill score the server took is progress.
        if answer.is_ok() && crate::game::activity::is_skill_score(class, method) {
            crate::game::activity::skill_scored(&state.pool, &ticket.claims.sid).await;
        }
        return answer;
    }
    if let Some(r) = live::answer(state, &req, features, lang).await? {
        return Ok(r);
    }
    if let Some(stub) = stubs::for_host(&original.host) {
        return Ok(live::json_response(
            &serde_json::from_str(stub).unwrap_or_default(),
        ));
    }
    Err(not_served(&original.host, None, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_minimum_must_be_a_version() {
        let c = ProxyConfig::new(Features::Supporter, Some(" 0.3.0+fh2online.60 ")).unwrap();
        assert_eq!(c.min_hook.as_ref().unwrap().0, "0.3.0+fh2online.60");
        assert_eq!(
            ProxyConfig::new(Features::Free, Some("")).unwrap().min_hook,
            None
        );
        assert_eq!(
            ProxyConfig::new(Features::Free, None).unwrap().min_hook,
            None
        );
        assert!(ProxyConfig::new(Features::Supporter, Some("latest")).is_err());
    }

    #[test]
    fn body_limits_and_refusals_come_from_the_headers() {
        let rpc_host = "gameservices.fh2.forzamotorsport.net";
        let user = Some("Forza.WebServices.User, Forza.WebServices");
        let profile = Some("Forza.WebServices.OnlineProfile, Forza.WebServices");
        let scoreboard = Some("Forza.WebServices.Scoreboard, Forza.WebServices");
        let storefront = Some("Forza.WebServices.Storefront, Forza.WebServices");
        for features in [Features::Supporter, Features::Free] {
            for host in FREE_ROAM_HOSTS {
                assert_eq!(body_limit(features, host, None, None).unwrap(), JSON_BODY);
            }
            // Served RPCs by their cap, the same on both deployments.
            for (class, method) in [
                (user, "Logon"),
                (user, "GetPermissionsTable"),
                (profile, "UpdateAll"),
                (profile, "UpdateAllTables"),
            ] {
                assert_eq!(
                    body_limit(features, rpc_host, class, Some(method)).unwrap(),
                    rpc::RPC_BODY,
                    "{method}"
                );
            }
            // Unserved ones refused before the body, universe-shaped ones on both until served.
            for (class, method) in [
                (user, "NoSuchMethod"),
                (user, "GetVipState"),
                (scoreboard, "GetRollupRows"),
                (storefront, "SearchPhotoFiles"),
            ] {
                let e = body_limit(features, rpc_host, class, Some(method)).unwrap_err();
                assert_eq!(e.code, ErrorCode::GameNotServed, "{method}");
            }
            let e = body_limit(features, rpc_host, scoreboard, Some("GetRollupRows")).unwrap_err();
            assert_eq!(
                e.details,
                Some(
                    json!({"host": rpc_host, "class": "Forza.WebServices.Scoreboard", "method": "GetRollupRows"})
                )
            );
            // A player's own paint (OD-65): every tier; the design's upload has its own cap.
            assert_eq!(
                body_limit(features, rpc_host, profile, Some("UpdateLivery")).unwrap(),
                liveries::UPDATE_BODY
            );
            // The boards (SER-M3 step 6): Supporters only; a ghost upload has its own cap.
            let boards = body_limit(features, rpc_host, scoreboard, Some("GetRows"));
            let ghost = body_limit(features, rpc_host, scoreboard, Some("UploadFile"));
            match features {
                Features::Supporter => {
                    assert_eq!(boards.unwrap(), rpc::RPC_BODY);
                    assert_eq!(ghost.unwrap(), universe::ghosts::UPLOAD_BODY);
                }
                Features::Free => {
                    assert_eq!(boards.unwrap_err().code, ErrorCode::GameNotServed);
                    assert_eq!(ghost.unwrap_err().code, ErrorCode::GameNotServed);
                }
            }
            // The Xbox Live JSON, gamerpic and stub hosts: 64 KiB.
            for host in live::HOSTS.into_iter().chain(["achievements.xboxlive.com"]) {
                assert_eq!(
                    body_limit(features, host, None, None).unwrap(),
                    JSON_BODY,
                    "{host}"
                );
            }
            // A host nothing answers, or the RPC host without a method: refused before the body.
            for (host, class) in [("eds.xboxlive.com", None), (rpc_host, user)] {
                let e = body_limit(features, host, class, None).unwrap_err();
                assert_eq!(
                    (e.code, e.details),
                    (ErrorCode::GameNotServed, Some(json!({"host": host})))
                );
            }
        }
    }

    /// OD-129: a request is served what its deployment serves, whatever the ticket's tier, mode
    /// or save line: on saves every ticket gets the universe; matchmaking stays the free plane.
    #[test]
    fn a_request_is_served_by_the_deployment_only() {
        let ticket = |tier: &str, mode: &str, line: bool| {
            let mut c = crate::game::ticket::tests::claims("vip", Some("fh2"), 1_792_245_600);
            c.tier = tier.into();
            c.mode = mode.into();
            c.line = line;
            c
        };
        let rpc_host = "gameservices.fh2.forzamotorsport.net";
        let scoreboard = Some("Forza.WebServices.Scoreboard, Forza.WebServices");
        for (tier, mode) in [
            ("vip", "hardcore"),
            ("vip", "noob"),
            ("free", "hardcore"),
            ("free", "noob"),
            ("", ""),
        ] {
            for (line, live) in [(true, true), (true, false), (false, true)] {
                let t = ticket(tier, mode, line);
                let served = Features::Supporter.for_ticket(&t, live);
                assert_eq!(served, Features::Supporter, "{tier} {mode} {line} {live}");
                assert!(
                    body_limit(served, rpc_host, scoreboard, Some("GetRows")).is_ok(),
                    "the universe for every ticket"
                );
                assert_eq!(Features::Free.for_ticket(&t, live), Features::Free);
            }
        }
    }

    #[test]
    fn not_served_names_what_was_asked() {
        let e = not_served("eds.xboxlive.com", None, None);
        assert_eq!(e.code, ErrorCode::GameNotServed);
        assert_eq!(e.code.status(), 404);
        assert_eq!(e.details, Some(json!({"host": "eds.xboxlive.com"})));
        let e = not_served(
            "a.b",
            Some("Forza.WebServices.Hopper"),
            Some("GetPlayerSkills"),
        );
        assert_eq!(
            e.details,
            Some(
                json!({"host": "a.b", "class": "Forza.WebServices.Hopper", "method": "GetPlayerSkills"})
            )
        );
    }
}
