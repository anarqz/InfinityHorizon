//! The game access ticket on the platform v1 game surfaces (ADR 0014 §2, GATE_API.md §3,
//! UNIVERSE_API.md §1, MATCHMAKING_API.md §1), verify-only: the deployment holds the public keys
//! (`FI_TOKEN_VERIFY_KEYS`, `kid:base64(32-byte Ed25519 public key)`), never the gate's private
//! ring, and never mints.
//!
//! Order of the checks (GAME_PLATFORM.md §1): signature and shape (anything wrong, a missing
//! header, or a platform session presented instead: 401 `AUTH.SESSION_EXPIRED`), then `pool`
//! against the deployment's pools (403 `GAME.WRONG_POOL`, before any other check; a free ticket
//! on saves), then `game` against `FI_GAMES` (403 `GAME.WRONG_POOL {game}`), then `exp`, then the
//! session's liveness through the per-instance [`LivenessCache`] (≤ 60 s). Every admitted request
//! marks the session in the instance's [`SeenBatch`], which writes `last_seen_at` for many
//! sessions in one statement, at most once per session per minute.
//!
//! [`verify`] and [`verify_ring`] both go through the gate's `fi_auth::session::verify_signature`
//! (a verify-only ring and the full ring); the gate reads tokens without a `game` claim as `fh2`.
//!
//! OD-239 (CR-S111): the universe and the results (`/saves/v1` universe routes, `/game/v1`
//! progression and presence, every proxied universe RPC) serve every live ticket, with or
//! without the `line` claim; OD-236's rule (a modded session stores no results) is unchanged. The
//! proxied profile sync (`OnlineProfile.UpdateAll`, `UpdateAllTables`) is not the save file and is
//! stored for every ticket (OD-128, owner 2026-11-11: Azimuth players keep feeding drivatars).
//! Only the save line needs `line`: the lease, revisions and bundles are the launcher's routes
//! (eligibility in admission and on commit).

use crate::errors::ApiError;
use crate::game::cache::{LivenessCache, SeenBatch};
use crate::state::{AppState, now_secs};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use fi_auth::access::{self, AccessClaims, AccessError, Audience};
use fi_auth::keys::SigningKeys;
use fi_auth::paseto;
use fi_auth::relay_grant::PublicKeys;
use fi_auth::session::{self, SessionError};
use fi_db::game::{self, Liveness, Seen};
use fi_protocol::{ErrorCode, headers, instant};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// The game of a ticket without a `game` claim (the local environment v0 mints only `fh2`).
pub const LEGACY_GAME: &str = "fh2";

/// The ticket's claims as the gate mints them (`game` added by platform v1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TicketClaims {
    /// Play session id.
    pub sid: String,
    /// Profile id.
    pub sub: String,
    /// Account id.
    pub acc: String,
    /// XUID, decimal.
    pub xuid: String,
    /// `fh2` (ADR 0902); absent on v0 tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    /// `vip` | `matchmaking` (`free` on v0 tokens).
    pub pool: String,
    /// `vip` | `free`.
    pub tier: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// Car database digest or empty.
    pub cdb: String,
    /// CR-S51: the session holds its save line (absent: `false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub line: bool,
    /// `game`.
    pub aud: String,
    /// Wire instant.
    pub iat: String,
    /// Wire instant.
    pub exp: String,
}

impl TicketClaims {
    /// The ticket's game.
    pub fn game(&self) -> &str {
        self.game.as_deref().unwrap_or(LEGACY_GAME)
    }
}

/// A ticket whose signature verified (not yet checked against the clock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedTicket {
    /// Claims.
    pub claims: TicketClaims,
    /// `iat` (Unix seconds).
    pub issued_at: i64,
    /// `exp` (Unix seconds).
    pub expires_at: i64,
}

impl VerifiedTicket {
    /// `Expired` at or past `exp`, `Invalid` when issued in the future.
    pub fn check_live(&self, now: i64) -> Result<(), SessionError> {
        if self.issued_at > now + session::SKEW_SECS {
            return Err(SessionError::Invalid);
        }
        if self.expires_at <= now {
            return Err(SessionError::Expired);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Footer {
    kid: String,
}

fn secs(t: &str) -> Option<i64> {
    instant::parse_unix_micros(t).map(|m| m.div_euclid(1_000_000))
}

impl From<session::Verified> for VerifiedTicket {
    fn from(v: session::Verified) -> Self {
        let c = v.claims;
        VerifiedTicket {
            claims: TicketClaims {
                sid: c.sid,
                sub: c.sub,
                acc: c.acc,
                xuid: c.xuid,
                game: Some(c.game),
                pool: c.pool,
                tier: c.tier,
                mode: c.mode,
                cdb: c.cdb,
                line: c.line,
                aud: c.aud,
                iat: c.iat,
                exp: c.exp,
            },
            issued_at: v.issued_at,
            expires_at: v.expires_at,
        }
    }
}

/// Verifies a ticket's signature (by the footer's `kid`), audience and shape with public keys,
/// through the gate's `fi_auth::session::verify_signature`.
pub fn verify(keys: &PublicKeys, token: &str) -> Result<VerifiedTicket, SessionError> {
    session::verify_signature(keys, token).map(VerifiedTicket::from)
}

/// The same check with a full ring (the local environment v0's game planes, which hold
/// `FI_TOKEN_SIGNING_KEYS`).
pub fn verify_ring(keys: &SigningKeys, token: &str) -> Result<VerifiedTicket, SessionError> {
    session::verify_signature(keys, token).map(VerifiedTicket::from)
}

/// A launcher access token (the platform session) verified with public keys, as
/// `fi_auth::access::verify` does with the ring: the saves deployment serves the save line
/// without the private key.
pub fn verify_access(
    keys: &PublicKeys,
    token: &str,
    audience: Audience,
    now: i64,
) -> Result<access::Verified, AccessError> {
    let footer = paseto::untrusted_footer(token).ok_or(AccessError::Invalid)?;
    let kid = serde_json::from_slice::<Footer>(&footer)
        .map_err(|_| AccessError::Invalid)?
        .kid;
    let key = keys.get(&kid).ok_or(AccessError::Invalid)?;
    let message = paseto::verify(key, token, &footer, b"").map_err(|_| AccessError::Invalid)?;
    let claims: AccessClaims =
        serde_json::from_slice(&message).map_err(|_| AccessError::Invalid)?;
    if claims.aud != audience.as_str() {
        return Err(AccessError::Invalid);
    }
    let (iat, exp) = (
        secs(&claims.iat).ok_or(AccessError::Invalid)?,
        secs(&claims.exp).ok_or(AccessError::Invalid)?,
    );
    if exp <= iat || exp - iat > access::TTL_SECS || iat > now + access::SKEW_SECS {
        return Err(AccessError::Invalid);
    }
    if exp <= now {
        return Err(AccessError::Expired);
    }
    Ok(access::Verified {
        account_id: claims.sub,
        family_id: claims.fam,
        role: claims.role,
        expires_at: exp,
    })
}

/// What a verify-only game deployment accepts (an `axum::Extension<Tickets>`).
#[derive(Debug)]
pub struct TicketConfig {
    /// Strict global launcher floor, validated at process startup.
    pub min_launcher: String,
    /// CR-L29 required unless an explicitly local process selects synthetic-test bypass.
    pub content_required: bool,
    /// `FI_TOKEN_VERIFY_KEYS`: the game access ticket's public keys (the gate's ring).
    pub keys: PublicKeys,
    /// `FI_PLATFORM_VERIFY_KEYS`: launcher access tokens on the save line (core's ring; ADR 0014).
    /// Locally the same keys as [`Self::keys`] (compose runs one ring).
    pub access_keys: PublicKeys,
    /// The pools served (`vip` on saves; `vip` and `matchmaking` on matchmaking).
    pub pools: Vec<String>,
    /// OD-239 (CR-S111): the saves deployment (the universe) admits a ticket of any pool, so an
    /// Azimuth session reaches its boards, records, clubs and liveries; every other deployment
    /// (matchmaking) keeps the plain pool check.
    pub any_pool: bool,
    /// `FI_GAMES`.
    pub games: Vec<String>,
    /// Positive liveness answers of this instance.
    pub liveness: Arc<LivenessCache>,
    /// Sessions seen since the last liveness flush.
    pub seen: Arc<SeenBatch>,
    /// The per-account buckets of `/launcher/v1` for the save line's launcher tokens (OD-215).
    pub accounts: Arc<crate::config::AccountLimiter>,
}

/// The shared ticket configuration.
pub type Tickets = Arc<TicketConfig>;

fn expired() -> ApiError {
    ApiError::new(
        ErrorCode::AuthSessionExpired,
        "no live session for this ticket: play offline until the launcher refreshes it",
    )
}

/// A request with a valid ticket of a served pool and game, for a session live now.
#[derive(Clone, Debug)]
pub struct Ticket {
    /// The verified claims.
    pub claims: TicketClaims,
    /// The session as Postgres saw it (at most the cache TTL ago).
    pub live: Liveness,
    /// The profile's XUID.
    pub xuid: i64,
}

impl Ticket {
    /// The ticket's game.
    pub fn game(&self) -> &str {
        self.claims.game()
    }
}

/// Only authenticated own-seat cleanup, not multiplayer permission.
/// Bypasses the online restriction alone; never bypasses signature, expiry or session ownership.
#[derive(Clone, Debug)]
pub struct CleanupTicket(pub Ticket);

/// The ticket's checks in the contract's order, then liveness and the seen mark.
pub async fn admit(
    db: &fi_db::PgPool,
    cfg: &TicketConfig,
    verified: Result<VerifiedTicket, SessionError>,
) -> Result<Ticket, ApiError> {
    admit_checked(
        db,
        verified,
        &cfg.pools,
        Some(&cfg.games),
        &cfg.liveness,
        Some(&cfg.seen),
        Policy {
            cleanup: false,
            online: OnlinePolicy {
                content_required: cfg.content_required,
                min_launcher: &cfg.min_launcher,
            },
            host_route: false,
            any_pool: cfg.any_pool,
        },
    )
    .await
}

/// [`admit`] with its parts: the pools served, the games served (`None`: any, the local
/// environment v0's game planes), the liveness cache and, when the deployment batches liveness
/// writes, the seen batch.
pub async fn admit_with(
    db: &fi_db::PgPool,
    verified: Result<VerifiedTicket, SessionError>,
    pools: &[String],
    games: Option<&[String]>,
    liveness: &LivenessCache,
    seen: Option<&SeenBatch>,
) -> Result<Ticket, ApiError> {
    admit_with_policy(
        db,
        verified,
        pools,
        games,
        liveness,
        seen,
        OnlinePolicy {
            content_required: false,
            min_launcher: "0.1.0",
        },
    )
    .await
}

/// Online policy shared by canonical ticket and legacy game-proxy authentication.
#[derive(Clone, Copy)]
pub struct OnlinePolicy<'a> {
    /// Original-file policy; explicit local synthetic bypass only.
    pub content_required: bool,
    /// Validated deployment minimum, never inferred from ticket/client input.
    pub min_launcher: &'a str,
}

/// Process-configured policy, including legacy game proxies; no cache bypasses policy checks.
pub async fn admit_with_policy(
    db: &fi_db::PgPool,
    verified: Result<VerifiedTicket, SessionError>,
    pools: &[String],
    games: Option<&[String]>,
    liveness: &LivenessCache,
    seen: Option<&SeenBatch>,
    online: OnlinePolicy<'_>,
) -> Result<Ticket, ApiError> {
    admit_checked(
        db,
        verified,
        pools,
        games,
        liveness,
        seen,
        Policy {
            cleanup: false,
            online,
            host_route: false,
            any_pool: false,
        },
    )
    .await
}
struct Policy<'a> {
    cleanup: bool,
    online: OnlinePolicy<'a>,
    /// The request is one of matchmaking's host routes ([`dedicated_host_route`]): the only
    /// requests a dedicated session's ticket may make (OD-109).
    host_route: bool,
    /// [`TicketConfig::any_pool`].
    any_pool: bool,
}

/// 403 `AUTH.FORBIDDEN {reason: "dedicated_host_only"}`: a dedicated lobby's ticket (OD-109,
/// GATE_API.md §2) on anything but matchmaking's host routes.
pub fn dedicated_host_only() -> ApiError {
    ApiError::new(
        ErrorCode::AuthForbidden,
        "a dedicated lobby's ticket only hosts its world",
    )
    .with_details(serde_json::json!({ "reason": "dedicated_host_only" }))
}

/// Whether `method route` (the matched route) is one of matchmaking's host routes, the only ones
/// a dedicated session's ticket may call (OD-109, MATCHMAKING_API.md §2.6): open, change, close and
/// heartbeat the world, its code, invites, accepting a request, moderation, the roster and the
/// punches. Everything else on a game plane (joins, seats, requests to join, free roam, the
/// universe, the RPC proxy) refuses it with [`dedicated_host_only`].
pub fn dedicated_host_route(method: &axum::http::Method, route: Option<&str>) -> bool {
    use axum::http::Method;
    use fi_protocol::routes::mm as r;
    let Some(route) = route else { return false };
    matches!(
        (method, route),
        (&Method::POST, r::WORLDS)
            | (&Method::PUT | &Method::DELETE, r::WORLD)
            | (&Method::POST, r::WORLD_ANNOUNCE)
            | (&Method::POST, r::WORLD_CODE)
            | (&Method::POST, r::WORLD_INVITES)
            | (&Method::POST, r::WORLD_REQUEST_ACCEPT)
            | (&Method::POST, r::WORLD_MODERATION)
            | (&Method::GET, r::WORLD_MEMBERS)
            | (&Method::GET, r::PUNCHES)
    )
}

fn host_route_of(parts: &Parts) -> bool {
    dedicated_host_route(
        &parts.method,
        parts
            .extensions
            .get::<axum::extract::MatchedPath>()
            .map(|m| m.as_str()),
    )
}

async fn admit_checked(
    db: &fi_db::PgPool,
    verified: Result<VerifiedTicket, SessionError>,
    pools: &[String],
    games: Option<&[String]>,
    liveness: &LivenessCache,
    seen: Option<&SeenBatch>,
    policy: Policy<'_>,
) -> Result<Ticket, ApiError> {
    let cleanup = policy.cleanup;
    let verified = verified.map_err(|_| expired())?;
    // OD-239 (CR-S111, CR-L16 and CR-S51 refined): the saves deployment (the universe,
    // `TicketConfig::any_pool`) admits a ticket of any pool, with or without a save line, so an
    // Azimuth session reaches its boards, records, clubs and liveries; matchmaking keeps the plain
    // pool check.
    if !policy.any_pool && !pools.iter().any(|p| p == &verified.claims.pool) {
        return Err(ApiError::new(
            ErrorCode::GameWrongPool,
            "this ticket belongs to another pool",
        ));
    }
    let game = verified.claims.game().to_owned();
    if games.is_some_and(|g| !g.contains(&game)) {
        return Err(ApiError::new(
            ErrorCode::GameWrongPool,
            "this deployment does not serve that game",
        )
        .with_details(serde_json::json!({ "game": game })));
    }
    verified.check_live(now_secs()).map_err(|_| expired())?;
    let claims = verified.claims;
    let sid = Uuid::parse_str(&claims.sid).map_err(|_| expired())?;
    let xuid: i64 = claims.xuid.parse().map_err(|_| expired())?;
    let pool = claims.pool.clone();
    let admits = |l: &Liveness| {
        l.serves(&pool)
            && l.profile_id.to_string() == claims.sub
            && l.account_id.to_string() == claims.acc
    };
    let cached = (!cleanup).then(|| liveness.get(sid)).flatten().filter(|l| {
        let ok = admits(l);
        if !ok {
            liveness.forget(sid);
        }
        ok
    });
    let live = match cached {
        Some(l) => l,
        None => {
            let l = if cleanup {
                game::liveness_for_cleanup(db, sid).await?
            } else {
                game::liveness(db, sid).await?
            }
            .filter(admits)
            .ok_or_else(expired)?;
            if !cleanup {
                liveness.put(&l, &pool);
            }
            l
        }
    };
    // OD-109: a dedicated session's ticket hosts its world and nothing else.
    if live.dedicated && !policy.host_route {
        return Err(dedicated_host_only());
    }
    if !cleanup {
        // The launcher floor applies only when a session is created (OD-121 follow-up "Admission
        // only"): an admitted session runs to its natural end below a raised floor.
        let mut conn = db.acquire().await.map_err(|_| ApiError::unavailable())?;
        // OD-113: only a Supporter + Hardcore session is bound to a content scan and its `cdb`
        // (never a dedicated host, OD-109, or a modded session); every other session skips both.
        if policy.online.content_required && live.integrity {
            let b = fi_db::content::require_session_in(&mut conn, sid)
                .await
                .map_err(crate::content::error)?;
            if claims.cdb != b {
                return Err(crate::content::error(fi_db::content::Error::Denied(
                    fi_protocol::content::ContentReason::IncompatibleFiles,
                )));
            }
        }
    }
    if let Some(seen) = seen {
        seen.mark(Seen {
            session_id: live.session_id,
            profile_id: live.profile_id,
            pool: live.pool.clone(),
        });
    }
    Ok(Ticket { claims, live, xuid })
}

/// The `X-Fi-Session` header, trimmed.
pub fn header_token(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(headers::FI_SESSION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

impl FromRequestParts<AppState> for Ticket {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let cfg = parts
            .extensions
            .get::<Tickets>()
            .cloned()
            .ok_or_else(ApiError::unavailable)?;
        let verified = header_token(parts)
            .ok_or(SessionError::Invalid)
            .and_then(|t| verify(&cfg.keys, t));
        admit_checked(
            &state.pool,
            verified,
            &cfg.pools,
            Some(&cfg.games),
            &cfg.liveness,
            Some(&cfg.seen),
            Policy {
                cleanup: false,
                online: OnlinePolicy {
                    content_required: cfg.content_required,
                    min_launcher: &cfg.min_launcher,
                },
                host_route: host_route_of(parts),
                any_pool: cfg.any_pool,
            },
        )
        .await
    }
}

impl FromRequestParts<AppState> for CleanupTicket {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let cfg = parts
            .extensions
            .get::<Tickets>()
            .cloned()
            .ok_or_else(ApiError::unavailable)?;
        let verified = header_token(parts)
            .ok_or(SessionError::Invalid)
            .and_then(|t| verify(&cfg.keys, t));
        // No seen mark and no shared liveness cache: cleanup cannot extend multiplayer access.
        admit_checked(
            &state.pool,
            verified,
            &cfg.pools,
            Some(&cfg.games),
            &cfg.liveness,
            None,
            Policy {
                cleanup: true,
                online: OnlinePolicy {
                    content_required: cfg.content_required,
                    min_launcher: &cfg.min_launcher,
                },
                host_route: host_route_of(parts),
                any_pool: cfg.any_pool,
            },
        )
        .await
        .map(CleanupTicket)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fi_auth::b64;
    use fi_auth::session::Grant;

    /// OD-109: a dedicated ticket's routes are matchmaking's host routes only.
    #[test]
    fn a_dedicated_ticket_reaches_the_host_routes_only() {
        use axum::http::Method;
        use fi_protocol::routes::mm as r;
        for (m, route) in [
            (Method::POST, r::WORLDS),
            (Method::PUT, r::WORLD),
            (Method::DELETE, r::WORLD),
            (Method::POST, r::WORLD_ANNOUNCE),
            (Method::POST, r::WORLD_CODE),
            (Method::POST, r::WORLD_INVITES),
            (Method::POST, r::WORLD_REQUEST_ACCEPT),
            (Method::POST, r::WORLD_MODERATION),
            (Method::GET, r::WORLD_MEMBERS),
            (Method::GET, r::PUNCHES),
        ] {
            assert!(dedicated_host_route(&m, Some(route)), "{m} {route}");
        }
        for (m, route) in [
            (Method::GET, Some(r::WORLDS)),
            (Method::GET, Some(r::WORLD)),
            (Method::GET, Some(r::WORLD_BY_CODE)),
            (Method::POST, Some(r::WORLD_REQUESTS)),
            (Method::POST, Some(r::JOIN)),
            (Method::DELETE, Some(r::GRANT)),
            (Method::GET, Some(r::SEAT)),
            (Method::DELETE, Some(r::SEAT)),
            (Method::POST, Some(r::SEAT_RENEW)),
            (Method::POST, Some(r::SEAT_NETWORK)),
            (Method::GET, Some(r::CAPACITY)),
            (Method::POST, Some("/game/v1/rpc")),
            (Method::POST, None),
        ] {
            assert!(!dedicated_host_route(&m, route), "{m} {route:?}");
        }
        let e = dedicated_host_only();
        assert_eq!(e.code, ErrorCode::AuthForbidden);
    }

    const NOW: i64 = 1_792_245_600;

    pub(crate) fn ring() -> SigningKeys {
        SigningKeys::parse(
            "FI_TOKEN_SIGNING_KEYS",
            &format!("st-1:{}", b64::std_encode(&[3u8; 32])),
        )
        .unwrap()
    }

    /// A ticket with a `game` claim, signed like the gate will (tests only: services never mint).
    pub(crate) fn mint(keys: &SigningKeys, claims: &TicketClaims) -> String {
        let (kid, key) = keys.current();
        let footer = format!(r#"{{"kid":"{kid}"}}"#);
        paseto::sign(
            key,
            &serde_json::to_vec(claims).unwrap(),
            footer.as_bytes(),
            b"",
        )
    }

    pub(crate) fn claims(pool: &str, game: Option<&str>, now: i64) -> TicketClaims {
        TicketClaims {
            sid: Uuid::now_v7().to_string(),
            sub: Uuid::now_v7().to_string(),
            acc: Uuid::now_v7().to_string(),
            xuid: "2535100000000001".into(),
            game: game.map(str::to_owned),
            pool: pool.into(),
            tier: "vip".into(),
            mode: "noob".into(),
            cdb: String::new(),
            line: false,
            aud: "game".into(),
            iat: instant::format_unix_secs(now).unwrap(),
            exp: instant::format_unix_secs(now + 600).unwrap(),
        }
    }

    #[test]
    fn the_public_and_the_ring_verifier_agree_on_v0_tokens() {
        let keys = ring();
        let public = PublicKeys::of(&keys);
        let issued = session::issue(
            &keys,
            &Grant {
                session_id: "0192f0c4-0000-7000-8000-00000000000a",
                profile_id: "0192f0c4-0000-7000-8000-00000000000c",
                account_id: "0192f0c4-0000-7000-8000-00000000000b",
                xuid: 2_535_100_000_000_001,
                pool: "vip",
                tier: "vip",
                mode: "noob",
                car_db: "",
                line: false,
                game_until: NOW + 3_600,
            },
            NOW,
        );
        let a = verify(&public, issued.token.expose()).unwrap();
        let b = verify_ring(&keys, issued.token.expose()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.claims.game(), LEGACY_GAME);
        assert_eq!(a.check_live(NOW + session::TTL_SECS - 1), Ok(()));
        assert_eq!(
            a.check_live(NOW + session::TTL_SECS),
            Err(SessionError::Expired)
        );
    }

    #[test]
    fn game_claims_forgeries_and_platform_sessions() {
        let keys = ring();
        let public = PublicKeys::of(&keys);
        let t = mint(&keys, &claims("vip", Some("fh3"), NOW));
        assert_eq!(verify(&public, &t).unwrap().claims.game(), "fh3");
        let other = PublicKeys::of(
            &SigningKeys::parse("X", &format!("st-1:{}", b64::std_encode(&[4u8; 32]))).unwrap(),
        );
        assert_eq!(verify(&other, &t).err(), Some(SessionError::Invalid));
        assert_eq!(
            verify(&public, &format!("{t}x")).err(),
            Some(SessionError::Invalid)
        );
        // A launcher access token (the platform session) is not a ticket, and the reverse.
        let platform = access::issue(&keys, "acc", "fam", Audience::Launcher, "player", NOW);
        assert_eq!(
            verify(&public, platform.token.expose()).err(),
            Some(SessionError::Invalid)
        );
        assert!(verify_access(&public, &t, Audience::Launcher, NOW).is_err());
        let v = verify_access(&public, platform.token.expose(), Audience::Launcher, NOW).unwrap();
        assert_eq!(
            v,
            access::verify(&keys, platform.token.expose(), Audience::Launcher, NOW).unwrap(),
            "the public check equals fi_auth::access::verify"
        );
        assert_eq!(
            verify_access(
                &public,
                platform.token.expose(),
                Audience::Launcher,
                NOW + 900
            ),
            Err(AccessError::Expired)
        );
        let mut long = claims("vip", None, NOW);
        long.exp = instant::format_unix_secs(NOW + session::TTL_SECS + 1).unwrap();
        assert_eq!(
            verify(&public, &mint(&keys, &long)).err(),
            Some(SessionError::Invalid),
            "longer than a ticket lives"
        );
    }
}
