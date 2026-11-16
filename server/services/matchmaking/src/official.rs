//! The world agent → platform routes of official worlds (OD-188/189; MATCHMAKING_API.md §2.9,
//! WORLDS_OFFICIAL.md §4). Outside §1's ticket rule: a world agent is authenticated only by its
//! `FI-World-Agent` signature with a node key pinned in `FI_WORLDS`.
//!
//! Order: the body (at most [`MAX_AGENT_BODY`], else 413), the signature (every failure 401
//! `MM.AGENT_UNAUTHORIZED`, one answer: no oracle), then the fields (400) and one transaction
//! (`fi_db::matchmaking`). Logs carry the `vmId` and ids, never the header, a key or a body.

use crate::Mm;
use crate::config::WorldsHost;
use axum::Router;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use ed25519_dalek::{Signature, VerifyingKey};
use fi_db::matchmaking::{self as db, Hosted, OfficialHost, OfficialSettings};
use fi_protocol::ErrorCode;
use fi_protocol::matchmaking::{
    GAME_MODE_SESSION_MAX_MEMBERS, GameModeSession, GameModeSessionRequest, GameModeSessionWorld,
    Heartbeat, Moderation, ModerationResult, OFFICIAL_ACTIVITY_KINDS,
    OFFICIAL_GAME_MODE_SESSION_KINDS, OfficialGrantKey, OfficialHello, OfficialWorldSettings,
    RelayGrantHost, VOICE_MODES, WORLD_AGENT_HEADER, WORLD_AGENT_PREFIX, WORLD_AGENT_SKEW_SECS,
    WorldAnnounced,
};
use fi_protocol::routes::mm as r;
use fi_surfaces::errors::ApiError;
use fi_surfaces::game::freeroam::invite;
use fi_surfaces::state::{AppState, now_secs};
use sha2::{Digest, Sha256};

/// The largest agent body read (16 KiB).
pub const MAX_AGENT_BODY: usize = 16 * 1024;

/// The routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::OFFICIAL_HELLO, get(hello))
        .route(r::OFFICIAL_WORLD, put(open).delete(close))
        .route(r::OFFICIAL_WORLD_ANNOUNCE, post(announce))
        .route(r::OFFICIAL_WORLD_MODERATION, post(moderate))
        .route(r::OFFICIAL_WORLD_GAME_SESSIONS, post(game_session))
}

/// The signed message (WORLDS_OFFICIAL.md §4.1): `"fi-world-agent-v1\n" ‖ method ‖ "\n" ‖
/// path-and-query ‖ "\n" ‖ seconds ‖ "\n" ‖ hex(SHA-256(body))`, no trailing newline.
pub fn agent_message(method: &str, path_and_query: &str, secs: &str, body: &[u8]) -> Vec<u8> {
    let digest = Sha256::digest(body);
    let mut m = Vec::with_capacity(WORLD_AGENT_PREFIX.len() + path_and_query.len() + 96);
    m.extend_from_slice(WORLD_AGENT_PREFIX);
    m.extend_from_slice(method.as_bytes());
    m.push(b'\n');
    m.extend_from_slice(path_and_query.as_bytes());
    m.push(b'\n');
    m.extend_from_slice(secs.as_bytes());
    m.push(b'\n');
    for b in digest.iter() {
        m.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    m
}

/// The Worlds host a header proves (`None`: refused). `header` is the `FI-World-Agent` value.
pub fn verify_agent<'w>(
    worlds: &'w [WorldsHost],
    header: Option<&str>,
    method: &str,
    path_and_query: &str,
    body: &[u8],
    now: i64,
) -> Option<&'w WorldsHost> {
    let mut fields = header?.split(' ');
    let (Some("v1"), Some(vm_id), Some(secs), Some(sig), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return None;
    };
    let host = worlds.iter().find(|w| w.vm_id == vm_id)?;
    let at: i64 = secs.parse().ok()?;
    // The canonical decimal only (no sign, no leading zeros): the signed text is the header's.
    if at < 0 || at.to_string() != secs || (now - at).abs() > WORLD_AGENT_SKEW_SECS {
        return None;
    }
    if sig.len() != 88 {
        return None;
    }
    let sig: [u8; 64] = fi_auth::b64::std_decode(sig)?.try_into().ok()?;
    let key = VerifyingKey::from_bytes(&host.key).ok()?;
    key.verify_strict(
        &agent_message(method, path_and_query, secs, body),
        &Signature::from_bytes(&sig),
    )
    .ok()?;
    Some(host)
}

/// The caller's Worlds host, or 401 `MM.AGENT_UNAUTHORIZED` (413 past [`MAX_AGENT_BODY`]).
pub fn agent<'m>(
    mm: &'m Mm,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<&'m WorldsHost, ApiError> {
    if body.len() > MAX_AGENT_BODY {
        return Err(ApiError::new(
            ErrorCode::RequestTooLarge,
            "request body too large",
        ));
    }
    let header = headers
        .get(WORLD_AGENT_HEADER)
        .and_then(|v| v.to_str().ok());
    let target = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path());
    verify_agent(
        &mm.worlds,
        header,
        method.as_str(),
        target,
        body,
        now_secs(),
    )
    .ok_or_else(|| {
        fi_surfaces::network_warn!(
            event = "mm_official_auth",
            status = "refused",
            "world agent call refused"
        );
        ApiError::new(ErrorCode::MmAgentUnauthorized, "world agent unauthorized")
    })
}

/// `GET /mm/v1/official/hello`.
async fn hello(
    State(_state): State<AppState>,
    Extension(mm): Extension<Mm>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let host = agent(&mm, &method, &uri, &headers, &body)?;
    fi_surfaces::network_info!(
        event = "mm_official_hello",
        vm_id = host.vm_id.as_str(),
        "world agent hello"
    );
    let answer = OfficialHello {
        server_time_ms: fi_domain::UnixMs::now_system().millis(),
        grant_verify_keys: mm
            .grant_keys
            .public_keys()
            .into_iter()
            .map(|(kid, key)| OfficialGrantKey {
                kid,
                public_key: fi_auth::b64::std_encode(&key),
            })
            .collect(),
        // CR-S99: what the served games' official worlds run (FH2 alone: as before).
        activities: OFFICIAL_ACTIVITY_KINDS
            .iter()
            .filter(|a| {
                mm.games
                    .iter()
                    .any(|g| fi_domain::GameRules::of(g).official_activity(a))
            })
            .map(|a| (*a).to_owned())
            .collect(),
    };
    Ok(crate::routes::private(
        "no-store",
        axum::Json(answer).into_response(),
    ))
}

fn store_host(h: &WorldsHost) -> OfficialHost<'_> {
    OfficialHost {
        vm_id: &h.vm_id,
        name: &h.name,
        ip: &h.ip,
        port: i32::from(h.port),
        max_worlds: i64::from(h.max_worlds),
        region: h.region.as_deref(),
    }
}

fn announced(mm: &Mm, host: &WorldsHost, hosted: &Hosted) -> Result<WorldAnnounced, ApiError> {
    let claim = RelayGrantHost::Official(host.vm_id.clone());
    Ok(WorldAnnounced {
        world: fi_surfaces::worlds::view(&hosted.world),
        punches: hosted
            .punches
            .iter()
            .map(|p| crate::routes::punch(mm, &claim, p))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

/// `PUT /mm/v1/official/worlds/{worldId}`.
async fn open(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let host = agent(&mm, &method, &uri, &headers, &body)?;
    let world = uuid::Uuid::parse_str(&id).map_err(|_| ApiError::invalid("worldId", "a UUID"))?;
    let s: OfficialWorldSettings = fi_surfaces::extract::parse(&body)?;
    if !mm.games.contains(&s.game) {
        return Err(ApiError::invalid("game", "a game this server serves"));
    }
    // CR-S101 (§2.11): `any` opens a mixed world (both modes); an older agent's mode still works.
    if !fi_protocol::matchmaking::WORLD_MODES.contains(&s.mode.as_str()) {
        return Err(ApiError::invalid("mode", "hardcore, noob or any"));
    }
    // CR-S99 (FM5_LOBBIES.md §1): the activities and seats of the world's game.
    let rules = fi_domain::GameRules::of(&s.game);
    let activity = match s.activity.as_deref() {
        None => "freeroam",
        Some(a) if OFFICIAL_ACTIVITY_KINDS.contains(&a) => a,
        Some(_) => {
            return Err(ApiError::invalid(
                "activity",
                "freeroam, carmeet, roadtrip or race",
            ));
        }
    };
    if !rules.official_activity(activity) {
        return Err(ApiError::invalid(
            "activity",
            format!("one of {}", rules.official_activities.join(", ")),
        ));
    }
    if !(2..=rules.max_players).contains(&u32::from(s.max_players)) {
        return Err(ApiError::invalid(
            "maxPlayers",
            format!("2 to {} guests", rules.max_players),
        ));
    }
    // CR-S103 (WORLDS_OFFICIAL.md §14): pool worlds are FH2's only.
    if s.pool && !rules.pool_worlds {
        return Err(ApiError::invalid(
            "pool",
            "absent: this game keeps no pool worlds",
        ));
    }
    let settings = OfficialSettings {
        game: s.game,
        mode: s.mode,
        activity: activity.to_owned(),
        max_players: i32::from(s.max_players),
        pool: s.pool,
    };
    let hosted = db::official_put(&state.pool, &store_host(host), world, &settings)
        .await
        .map_err(|e| {
            // CR-S100: the ceiling is a safety limit; reaching it is an incident (the alert job
            // mails the owner past 80%).
            if matches!(e, db::MmError::Conflict("max_worlds")) {
                fi_surfaces::network_warn!(
                    event = "mm_official_ceiling",
                    vm_id = host.vm_id.as_str(),
                    max_worlds = host.max_worlds,
                    "a Worlds VM reached its maxWorlds safety ceiling (CR-S100)"
                );
            }
            crate::routes::mm_error(e)
        })?;
    fi_surfaces::network_info!(event = "mm_official_world", vm_id = host.vm_id.as_str(),
        world_id = %world, opened = hosted.opened, "official world put");
    let status = if hosted.opened {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let body = announced(&mm, host, &hosted)?;
    Ok(crate::routes::private(
        "no-store",
        (status, axum::Json(body)).into_response(),
    ))
}

/// `POST /mm/v1/official/worlds/{worldId}/announce`.
async fn announce(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let host = agent(&mm, &method, &uri, &headers, &body)?;
    let world = crate::routes::world_id(&id)?;
    let h: Heartbeat = fi_surfaces::extract::parse(&body)?;
    // CR-S99: up to the largest game's seats here; the world's own game's in the store.
    let peers = crate::routes::xuids("peers", &h.peers, fi_domain::game::MAX_SEATS as usize)?;

    // OD-190: an official world's activity is fixed at `PUT`; the announce's is ignored.
    let voice = match h.voice.as_deref() {
        None => None,
        Some(v) if VOICE_MODES.contains(&v) => Some(v),
        Some(_) => return Err(ApiError::invalid("voice", "off, open or proximity")),
    };
    // §12.8b: a malformed leader is dropped and logged, never failing the announce.
    let leader = h.leader.as_deref().and_then(|x| {
        let parsed = x.parse::<i64>().ok();
        if parsed.is_none() {
            fi_surfaces::network_warn!(event = "mm_official_announce", vm_id = host.vm_id.as_str(),
                world_id = %world, "malformed leader dropped");
        }
        parsed
    });
    // OD-194: a Road Trip past its vote takes no newcomers
    let hosted = db::official_announce_with(
        &state.pool,
        &store_host(host),
        world,
        &peers,
        voice,
        h.in_progress,
        leader,
    )
    .await
    .map_err(crate::routes::mm_error)?;
    fi_surfaces::network_info!(event = "mm_official_announce", vm_id = host.vm_id.as_str(),
        world_id = %world, players = hosted.world.row.players,
        punch_count = hosted.punches.len(), "official world announced");
    let body = announced(&mm, host, &hosted)?;
    Ok(crate::routes::private(
        "no-store",
        axum::Json(body).into_response(),
    ))
}

/// `DELETE /mm/v1/official/worlds/{worldId}`.
async fn close(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let host = agent(&mm, &method, &uri, &headers, &body)?;
    let world = crate::routes::world_id(&id)?;
    db::official_close(&state.pool, &store_host(host), world)
        .await
        .map_err(crate::routes::mm_error)?;
    fi_surfaces::network_info!(event = "mm_official_close", vm_id = host.vm_id.as_str(),
        world_id = %world, "official world closed");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /mm/v1/official/worlds/{worldId}/game-sessions` (OD-190/OD-191, MATCHMAKING_API.md
/// §2.9): a session started in a `carmeet` world opens a group world for its players (their car
/// meet seats move there) and the MPSD session they go to, owned by `xuids[0]`.
async fn game_session(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let host = agent(&mm, &method, &uri, &headers, &body)?;
    let world = crate::routes::world_id(&id)?;
    let req: GameModeSessionRequest = fi_surfaces::extract::parse(&body)?;
    if !OFFICIAL_GAME_MODE_SESSION_KINDS.contains(&req.kind.as_str()) {
        return Err(ApiError::invalid("kind", "freeroam or roadtrip"));
    }
    let xuids: Vec<i64> = req
        .xuids
        .iter()
        .map(|x| x.parse::<i64>().ok().filter(|v| *v > 0))
        .collect::<Option<_>>()
        .filter(|v: &Vec<i64>| {
            (1..=GAME_MODE_SESSION_MAX_MEMBERS).contains(&v.len())
                && v.iter().enumerate().all(|(i, x)| !v[..i].contains(x))
        })
        .ok_or_else(|| ApiError::invalid("xuids", "1 to 12 distinct XUIDs, decimal"))?;
    let opened = db::official_group_open(
        &state.pool,
        &store_host(host),
        world,
        &xuids,
        &req.kind,
        mm.seat_hold_secs,
    )
    .await
    .map_err(crate::routes::mm_error)?;
    let first = &opened.members[0];
    let owner = fi_db::freeroam::Caller {
        game: opened.game.clone(),
        profile_id: first.profile_id,
        account_id: first.account_id,
        session_id: first.session_id,
        xuid: first.xuid,
        tier: first.tier.clone(),
        mode: first.mode.clone(),
        ip: None,
    };
    let members: Vec<invite::Invited> = opened
        .members
        .iter()
        .map(|m| invite::Invited {
            xuid: m.xuid,
            gamertag: m.gamertag.clone(),
        })
        .collect();
    let key = invite::create_official(&state.pool, &owner, opened.world_id, &members).await?;
    fi_surfaces::network_info!(event = "mm_official_game_session", vm_id = host.vm_id.as_str(),
        world_id = %world, group_world_id = %opened.world_id, members = members.len(),
        "official group world and game-mode session");
    let answer = GameModeSession {
        kind: req.kind,
        scid: key.scid,
        template_name: key.template,
        name: key.name,
        members: xuids.iter().map(i64::to_string).collect(),
        world: Some(GameModeSessionWorld {
            world_id: opened.world_id.to_string(),
            activity: opened.activity,
        }),
    };
    Ok(crate::routes::private(
        "no-store",
        (StatusCode::CREATED, axum::Json(answer)).into_response(),
    ))
}

/// `POST /mm/v1/official/worlds/{worldId}/moderation`: `kick` only.
async fn moderate(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let host = agent(&mm, &method, &uri, &headers, &body)?;
    let world = crate::routes::world_id(&id)?;
    let m: Moderation = fi_surfaces::extract::parse(&body)?;
    if m.action != "kick" || m.until.is_some() || m.reason.is_some() {
        return Err(ApiError::invalid(
            "action",
            "kick (bans stay with moderators)",
        ));
    }
    let xuid = m
        .xuid
        .parse::<i64>()
        .ok()
        .filter(|x| *x > 0)
        .ok_or_else(|| ApiError::invalid("xuid", "an XUID, decimal"))?;
    db::official_kick(&state.pool, &store_host(host), world, xuid)
        .await
        .map_err(crate::routes::mm_error)?;
    fi_surfaces::network_info!(event = "mm_official_kick", vm_id = host.vm_id.as_str(),
        world_id = %world, "official world kick");
    Ok(crate::routes::private(
        "no-store",
        axum::Json(ModerationResult {
            action: m.action,
            xuid: m.xuid,
            until: None,
        })
        .into_response(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// WORLDS_OFFICIAL.md §4.2.
    fn vector_host() -> WorldsHost {
        let seed: [u8; 32] = std::array::from_fn(|i| i as u8);
        WorldsHost {
            region: None,
            vm_id: "local-mac".into(),
            key: SigningKey::from_bytes(&seed).verifying_key().to_bytes(),
            ip: "192.168.1.53".into(),
            port: 5200,
            name: "Local".into(),
            max_worlds: 4,
        }
    }

    const V1_HEX: &str = "66692d776f726c642d6167656e742d76310a4745540a2f6d6d2f76312f6f6666696369616c2f68656c6c6f0a313739333735303430300a65336230633434323938666331633134396166626634633839393666623932343237616534316534363439623933346361343935393931623738353262383535";
    const V1_HEADER: &str = "v1 local-mac 1793750400 mAV+0LE5h39r4oF4WEhXo9Cpwv0L784vSs05hM2/vc9CuE/MBIrkFpzlH6xd04nUOaIrplacH2/sv5P+L547Cg==";
    const V2_PATH: &str = "/mm/v1/official/worlds/01900000-0000-7000-8000-000000000001";
    const V2_BODY: &str = r#"{"game":"fh2","mode":"noob","activity":"freeroam","maxPlayers":12}"#;
    const V2_HEX: &str = "66692d776f726c642d6167656e742d76310a5055540a2f6d6d2f76312f6f6666696369616c2f776f726c64732f30313930303030302d303030302d373030302d383030302d3030303030303030303030310a313739333735303431350a37363938663830613539373764636532306466376664656632346562363462306330373066623137303639346662303039663433376332383431323132356636";
    const V2_HEADER: &str = "v1 local-mac 1793750415 OR7d/J4aZ1ksGXMhO2+rikYTJ360YjooL5e/q7ZC91Fd+/lvVTuUVhiighFKYiIWcZsoGy7lt5lcsnhlXxBBBw==";

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn the_test_vectors_of_worlds_official_4_2() {
        let seed: [u8; 32] = std::array::from_fn(|i| i as u8);
        let key = SigningKey::from_bytes(&seed);
        assert_eq!(
            fi_auth::b64::std_encode(key.verifying_key().as_bytes()),
            "A6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg="
        );
        let worlds = [vector_host()];
        for (method, path, secs, body, want, header) in [
            (
                "GET",
                "/mm/v1/official/hello",
                "1793750400",
                "",
                V1_HEX,
                V1_HEADER,
            ),
            ("PUT", V2_PATH, "1793750415", V2_BODY, V2_HEX, V2_HEADER),
        ] {
            let message = agent_message(method, path, secs, body.as_bytes());
            assert_eq!(hex(&message), want);
            let sig = fi_auth::b64::std_encode(&key.sign(&message).to_bytes());
            assert_eq!(format!("v1 local-mac {secs} {sig}"), header);
            let now: i64 = secs.parse().unwrap();
            for skew in [-30, 0, 30] {
                assert!(
                    verify_agent(
                        &worlds,
                        Some(header),
                        method,
                        path,
                        body.as_bytes(),
                        now + skew
                    )
                    .is_some()
                );
            }
            for skew in [-31, 31] {
                assert!(
                    verify_agent(
                        &worlds,
                        Some(header),
                        method,
                        path,
                        body.as_bytes(),
                        now + skew
                    )
                    .is_none()
                );
            }
        }
    }

    #[test]
    fn every_other_header_is_refused() {
        let worlds = [vector_host()];
        let now = 1_793_750_400;
        let ok = |h: &str, m: &str, p: &str, b: &[u8]| verify_agent(&worlds, Some(h), m, p, b, now);
        let path = "/mm/v1/official/hello";
        assert!(ok(V1_HEADER, "GET", path, b"").is_some());
        assert!(verify_agent(&worlds, None, "GET", path, b"", now).is_none());
        let sig = V1_HEADER.rsplit(' ').next().unwrap();
        for bad in [
            format!("v2 local-mac 1793750400 {sig}"),
            format!("v1 other 1793750400 {sig}"),
            format!("v1 local-mac 01793750400 {sig}"),
            format!("v1 local-mac +1793750400 {sig}"),
            format!("v1  local-mac 1793750400 {sig}"),
            format!("v1 local-mac 1793750400 {sig} x"),
            format!("v1 local-mac 1793750400 {}", sig.trim_end_matches('=')),
            format!("v1 local-mac 1793750401 {sig}"),
            "v1 local-mac 1793750400".to_owned(),
            String::new(),
        ] {
            assert!(ok(&bad, "GET", path, b"").is_none(), "{bad}");
        }
        assert!(ok(V1_HEADER, "POST", path, b"").is_none());
        assert!(ok(V1_HEADER, "GET", "/mm/v1/official/hello?x=1", b"").is_none());
        assert!(ok(V1_HEADER, "GET", path, b"{}").is_none());
        // Another node key under the same vmId.
        let mut other = vector_host();
        other.key = SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(verify_agent(&[other], Some(V1_HEADER), "GET", path, b"", now).is_none());
    }
}
