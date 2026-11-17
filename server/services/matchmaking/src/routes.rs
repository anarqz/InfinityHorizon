//! `/mm/v1` (MATCHMAKING_API.md §2-§6; worlds in the overlay, OD-54 to OD-59). Thin: parse and
//! check the request, map `fi_db::matchmaking`'s decision to the wire, sign relay grants. Per-account
//! rate limits (Postgres) come before any decision (§1). The caller's place (its address and, behind
//! the edge only, its coarse location) is [`Place`]: never logged. Private UDP rendezvous stores
//! a short-lived observed joiner address only for its exact authenticated host (CR-S26/27/28).

use crate::Mm;
use axum::Router;
use axum::extract::{Extension, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use fi_auth::relay_grant;
use fi_db::limits::{Hit, Limit};
use fi_db::matchmaking::{
    self as db, ANNOUNCE_LIMIT, Action, ByCode, Caller, Endpoint, Filter, GrantIssued, HOST_LIMIT,
    JOIN_LIMIT, JoinInput, JoinOutcome, LIST_LIMIT, MEMBERS_LIMIT, MmError, NETWORK_LIMIT,
    OfferRow, PUNCHES_LIMIT, PunchRow, SEAT_LIMIT, SETTINGS_LIMIT, SeatNow, SeatView, Sent,
    Settings,
};
use fi_db::observations::{self as seen, NewObservation, Observed};
use fi_domain::UnixMs;
use fi_domain::geo;
use fi_protocol::ErrorCode;
use fi_protocol::core::InviteSession;
use fi_protocol::matchmaking::{
    Announce, AnnouncePort, CallerPool, Capacity, FreeCapacity, GAME_MODE_SESSION_KINDS,
    GAME_MODE_SESSION_MAX_MEMBERS, GameModeSession, GameModeSessionRequest, Grant, Heartbeat,
    Invite, InviteRequest, JoinAnswer, JoinRequest, LatencyRefusal, ListViewer, Locked,
    MAX_PLAYERS, ModEntry, Moderation, ModerationResult, NetworkCandidate, NetworkCandidateKind,
    NetworkHints, PeerObservation, Punch, Punches, RelayGrant, RelayGrantClaims, RelayGrantHost,
    RelayGrantRelay, RelayLatencies, RequestToJoin, Seat, SeatAnswer, SeatEnded, SeatNetworkAnswer,
    SeatNetworkRequest, SeatQuery, SeatRenew, SeatRequest, WorldAnnounced, WorldCounts, WorldList,
    WorldMember, WorldMembers, WorldSeats, WorldSettings, normalize_code,
};
use fi_protocol::routes::mm as r;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, ClientIp};
use fi_surfaces::game::freeroam::invite;
use fi_surfaces::game::ticket::Ticket;
use fi_surfaces::state::{AppState, now_secs};
use fi_surfaces::views;
use fi_surfaces::worlds::view;
use serde::Deserialize;
use std::convert::Infallible;
use std::net::IpAddr;
use uuid::Uuid;

/// The `/mm/v1` routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::WORLDS, get(list).post(announce))
        .route(r::WORLD, get(one).put(put).delete(close))
        .route(r::WORLD_BY_CODE, get(by_code))
        .route(r::WORLD_ANNOUNCE, post(heartbeat))
        .route(r::WORLD_CODE, post(new_code))
        .route(r::WORLD_INVITES, post(invite))
        .route(r::WORLD_REQUESTS, post(ask))
        .route(r::WORLD_REQUEST_ACCEPT, post(accept))
        .route(r::WORLD_MODERATION, post(moderate))
        .route(r::WORLD_MEMBERS, get(members))
        .route(r::WORLD_JOIN_MODES, axum::routing::put(join_modes))
        .route(r::WORLD_GAME_SESSIONS, post(game_session))
        .route(r::WORLD_OBSERVATIONS, post(observe))
        .route(r::WATCH, get(watch))
        .route(r::JOIN, post(join))
        .route(r::GRANT, delete(release))
        .route(r::SEAT, get(seat).delete(leave))
        .route(r::SEAT_RENEW, post(renew))
        .route(r::SEAT_NETWORK, post(seat_network))
        .route(r::PUNCHES, get(punches))
        .route(r::CAPACITY, get(capacity))
}

/// A request's `transport` (OD-146): absent as before, `relay` always through the relay.
fn relay_only(transport: Option<&String>) -> Result<bool, ApiError> {
    match transport.map(String::as_str) {
        None => Ok(false),
        Some("relay") => Ok(true),
        Some(_) => Err(ApiError::invalid("transport", "relay, or absent")),
    }
}

/// OD-150: a world's activity, one of [`fi_protocol::matchmaking::ACTIVITY_KINDS`].
fn activity(activity: Option<&String>) -> Result<Option<&str>, ApiError> {
    match activity.map(String::as_str) {
        None => Ok(None),
        Some(a) if fi_protocol::matchmaking::ACTIVITY_KINDS.contains(&a) => Ok(Some(a)),
        Some(_) => Err(ApiError::invalid("activity", "freeroam or roadtrip")),
    }
}

/// CR-S67: a world's voice setting, one of [`fi_protocol::matchmaking::VOICE_MODES`].
fn voice(voice: Option<&String>) -> Result<Option<&str>, ApiError> {
    match voice.map(String::as_str) {
        None => Ok(None),
        Some(v) if fi_protocol::matchmaking::VOICE_MODES.contains(&v) => Ok(Some(v)),
        Some(_) => Err(ApiError::invalid("voice", "off, open or proximity")),
    }
}

fn caller(t: &Ticket) -> Caller {
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
    }
}

/// Where the caller is: its address as the platform sees it ([`ClientIp`], unmapped) and, only
/// when the origin lock verified the edge **and** the deployment says the edge sets them
/// (`FI_EDGE_LOCATION_HEADERS=true`: Cloudflare's visitor location transform is on, so a client
/// cannot send its own), the coarse location of `CF-IPLatitude`, `CF-IPLongitude` and
/// `CF-IPContinent` reduced to a cell and a region (MATCHMAKING_API.md §3.5). The coordinates are
/// dropped at once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Place {
    /// The caller's public IP (canonical).
    pub ip: Option<IpAddr>,
    /// Its 1°×1° cell.
    pub cell: Option<i32>,
    /// Its coarse region.
    pub region: Option<String>,
}

impl Place {
    /// The address as text, for the same-address rule.
    pub fn ip_text(&self) -> Option<String> {
        self.ip.map(|ip| ip.to_string())
    }
}

fn header_f64(parts: &Parts, name: &str) -> Option<f64> {
    parts
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
}

impl<S: Send + Sync> FromRequestParts<S> for Place {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let ip = host_ip(parts.extensions.get::<ClientIp>().map(|c| c.0));
        let trusted = parts
            .extensions
            .get::<Mm>()
            .is_some_and(|m| m.location_headers)
            && parts
                .extensions
                .get::<fi_runtime::http::EdgeVerified>()
                .is_some();
        if !trusted {
            return Ok(Self {
                ip,
                cell: None,
                region: None,
            });
        }
        let (lat, lon) = (
            header_f64(parts, "cf-iplatitude"),
            header_f64(parts, "cf-iplongitude"),
        );
        let continent = parts
            .headers
            .get("cf-ipcontinent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        Ok(Self {
            ip,
            cell: lat.zip(lon).and_then(|(la, lo)| geo::cell(la, lo)),
            region: geo::region(continent, lon).map(str::to_owned),
        })
    }
}

async fn limited(state: &AppState, limit: &Limit, account: Uuid) -> Result<Option<u64>, ApiError> {
    Ok(match db::rate_limit(&state.pool, limit, account).await? {
        Hit::Allowed => None,
        Hit::Limited { retry_after_secs } => {
            if let Some(suppressed) =
                fi_surfaces::diagnostics::poll(fi_surfaces::diagnostics::Poll::RateLimit)
            {
                fi_surfaces::network_warn!(
                    event = "mm_rate_limit",
                    bucket = limit.name,
                    retry_after_secs,
                    suppressed,
                    "matchmaking rate limited"
                );
            }
            Some(u64::try_from(retry_after_secs).unwrap_or(1))
        }
    })
}

// CR-S39: explicit scalar allowlists. Never log a request, endpoint or signed grant.
fn diagnostic_error(operation: &'static str, c: &Caller, error: ApiError) -> ApiError {
    let reason = error
        .details
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(|v| v.as_str());
    let reason = match reason {
        Some("network_revision") => "network_revision",
        Some("host_session") => "host_session",
        Some(_) => "other",
        None => "none",
    };
    fi_surfaces::network_warn!(event = "mm_decision", operation, session_id = %c.session_id,
        code = error.code.code(), status = error.code.status(), reason, "matchmaking refused");
    error
}

fn diagnostic_endpoint(c: &Caller, operation: &'static str, endpoint: Option<&AnnouncePort>) {
    fi_surfaces::network_info!(event = "mm_endpoint", operation, session_id = %c.session_id,
        endpoint_present = endpoint.is_some(),
        lan_candidates = endpoint.and_then(|v| v.lan.as_ref()).map_or(0, Vec::len),
        external_mapping = endpoint.and_then(|v| v.external.as_ref()).map_or("none", |v| match v.source {
            fi_protocol::matchmaking::AnnounceExternalSource::Stun => "stun",
            fi_protocol::matchmaking::AnnounceExternalSource::Upnp => "upnp",
        }),
        local_public_port_differ = endpoint.is_some_and(|v| v.external.as_ref().is_some_and(|e| e.port != v.port)),
        "typed endpoint observation");
}

fn diagnostic_punches(c: &Caller, rows: &[PunchRow]) {
    let Some(suppressed) = fi_surfaces::diagnostics::poll(fi_surfaces::diagnostics::Poll::Punches)
    else {
        return;
    };
    fi_surfaces::network_info!(event = "mm_punches", suppressed, session_id = %c.session_id,
        count = rows.len(), "host punch snapshot");
    for row in rows.iter().take(MAX_PLAYERS as usize - 1) {
        let hints = row.candidates.as_ref();
        fi_surfaces::network_info!(event = "mm_punch", session_id = %c.session_id,
            world_id = %row.world_id, grant_id = %row.grant_id,
            state = row.state.as_str(), proof_available = row.udp_proof_key.is_some(),
            revision = hints.map(|v| v.revision),
            public_candidates = hints.map_or(0, |v| v.candidates.iter().filter(|c| c.kind == "public").count()),
            lan_candidates = hints.map_or(0, |v| v.candidates.iter().filter(|c| c.kind == "lan").count()),
            hint_remaining_ms = hints.map(|v| (v.expires_at.millis() - v.generated_at.millis()).max(0)),
            "host punch authority");
    }
}

fn diagnostic_seat(c: &Caller, body: &SeatAnswer, operation: &'static str) {
    let sampled = fi_surfaces::diagnostics::poll(fi_surfaces::diagnostics::Poll::Seat);
    if operation == "read" && body.ended.is_none() && sampled.is_none() {
        return;
    }
    let seat = body.seat.as_ref();
    fi_surfaces::network_info!(event = "mm_seat", operation, suppressed = sampled.unwrap_or(0), session_id = %c.session_id,
        state = seat.map_or("none", |v| v.state.as_str()),
        world_id = seat.map(|v| v.world_id.as_str()), grant_id = seat.and_then(|v| v.grant_id.as_deref()),
        proof_available = seat.and_then(|v| v.relay.as_ref()).is_some_and(|v| v.udp_proof.is_some()),
        ended_reason = body.ended.as_ref().map(|v| v.reason.as_str()), "seat authority snapshot");
}

fn rate_limited(secs: u64) -> ApiError {
    ApiError::new(ErrorCode::RequestRateLimited, "too many requests").retry_after(secs)
}

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::MmWorldNotFound, "no such world")
}

pub(crate) fn mm_error(e: MmError) -> ApiError {
    match e {
        MmError::WorldNotFound => not_found(),
        MmError::NotHost => ApiError::new(ErrorCode::MmNotHost, "not the host of this world"),
        MmError::AlreadyHosting(id) => ApiError::new(
            ErrorCode::MmAlreadyHosting,
            "this profile hosts another world: close it first",
        )
        .with_details(serde_json::json!({"worldId": id.to_string()})),
        MmError::UnknownPlayer => ApiError::invalid("xuid", "no such player in this game"),
        MmError::NotFound => ApiError::new(ErrorCode::PortalNotFound, "not found"),
        MmError::Conflict(reason) => ApiError::new(ErrorCode::PortalConflict, "not now")
            .with_details(serde_json::json!({"reason": reason})),
        MmError::RateLimited(secs) => rate_limited(u64::try_from(secs).unwrap_or(1)),
        MmError::SnapshotChanged => ApiError::unavailable().retry_after(1),
        MmError::SessionEnded => {
            ApiError::new(ErrorCode::AuthSessionExpired, "the play session has ended")
        }
        MmError::Invalid { field, reason } => ApiError::invalid(field, reason),
        MmError::DedicatedHostOnly => fi_surfaces::game::ticket::dedicated_host_only(),
        MmError::NoOfficialRoom => no_official_room(),
        MmError::Db(e) => e.into(),
    }
}

pub(crate) fn world_id(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| not_found())
}

pub(crate) fn private(value: &'static str, mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ListQuery {
    filter: Option<String>,
    not_full: Option<bool>,
    modded: Option<String>,
    activity: Option<String>,
}

/// `GET /mm/v1/worlds?filter=&notFull=` (OD-58: a filter is required, five worlds per filter).
async fn list(
    State(state): State<AppState>,
    ticket: Ticket,
    place: Place,
    q: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(q) =
        q.map_err(|_| ApiError::invalid("query", "filter, notFull, modded and activity only"))?;
    // OD-150: only the worlds running that activity now.
    let wanted = activity(q.activity.as_ref())?.map(str::to_owned);
    let filter = q
        .filter
        .as_deref()
        .and_then(Filter::parse)
        .ok_or_else(|| ApiError::invalid("filter", "global, near, friends or all"))?;
    // T-1110-NOMODMATCH: mods never decide which worlds a caller sees; `modded` is accepted
    // (older launchers send it) and ignored.
    if !matches!(q.modded.as_deref(), None | Some("true" | "false")) {
        return Err(ApiError::invalid("modded", "true or false"));
    }
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &LIST_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    // OD-149: a flagged Hardcore caller is told until when (its scan below holds only pineapple
    // worlds); one connection at a time.
    let pool_until = if c.mode == "hardcore" {
        fi_db::observations::flag_of(&state.pool, c.profile_id).await?
    } else {
        None
    };
    let rows = db::list_worlds_running(&state.pool, &c, place.ip_text(), filter, wanted.as_deref())
        .await?;
    let rules = c.rules(place.cell);
    let assessed = rows.iter().map(|w| db::assess(w, &rules, false)).collect();
    let (mut items, mut locked) =
        // OD-129: no tier sees less; nothing is locked.
        db::select(assessed, filter, false, q.not_full.unwrap_or(false));
    db::embed_visible(
        &state.pool,
        items.iter_mut().chain(locked.iter_mut()),
        false,
    )
    .await?;
    let body = WorldList {
        filter: filter.as_str().to_owned(),
        items: items.iter().map(view).collect(),
        locked: locked.iter().map(view).collect(),
        generated_at: views::at(UnixMs::now_system()),
        viewer: ListViewer {
            located: place.cell.is_some(),
            region: place.region.clone(),
        },
        pool: pool_until.map(|until| CallerPool {
            until: views::at(until),
        }),
    };
    Ok(private(
        "private, max-age=5",
        axum::Json(body).into_response(),
    ))
}

/// `GET /mm/v1/worlds/{worldId}`.
async fn one(
    State(state): State<AppState>,
    ticket: Ticket,
    place: Place,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = world_id(&id)?;
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &LIST_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let w = db::world_for_caller(&state.pool, &c, place.ip_text(), id)
        .await?
        .ok_or_else(not_found)?;
    let mut a = db::assess(&w, &c.rules(place.cell), false);
    db::embed_visible(&state.pool, [&mut a], false).await?;
    let body = view(&a);
    Ok(private("no-store", axum::Json(body).into_response()))
}

/// `GET /mm/v1/worlds/{worldId}/members` (OD-63, MATCHMAKING_API.md §3.7): the roster, for the
/// world's host or a profile with a live seat in it; anyone else 404 `MM.WORLD_NOT_FOUND`. Bucket
/// `mm.members` per play session.
async fn members(
    State(state): State<AppState>,
    ticket: Ticket,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = world_id(&id)?;
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &MEMBERS_LIMIT, c.session_id).await? {
        return Err(rate_limited(secs));
    }
    let rows = db::members(&state.pool, &c, id)
        .await?
        .ok_or_else(not_found)?;
    let body = members_body(id.to_string(), &rows, UnixMs::now_system());
    Ok(private(
        "private, no-store",
        axum::Json(body).into_response(),
    ))
}

fn members_body(world_id: String, rows: &[db::MemberRow], at: UnixMs) -> WorldMembers {
    WorldMembers {
        world_id,
        items: rows
            .iter()
            .map(|m| WorldMember {
                xuid: m.xuid.to_string(),
                gamertag: m.gamertag.clone(),
                tier: m.tier.clone(),
                role: m.role.clone(),
                friend: m.friend.clone(),
                since: views::at(m.since),
                voice_blocked_until: m.voice_blocked_until.map(views::at),
                mods: m.mods.clone(),
            })
            .collect(),
        generated_at: views::at(at),
    }
}

/// `POST /mm/v1/worlds/{worldId}/game-sessions` (host; MATCHMAKING_API.md §3.8, S9;
/// other projects used as reference): the free-roam session a car meet invite's joiners go to. Every XUID is the host or holds
/// a live seat in the world (the roster's members); the host owns the session.
async fn game_session(
    State(state): State<AppState>,
    ticket: Ticket,
    Path(world): Path<String>,
    Body(req): Body<GameModeSessionRequest>,
) -> Result<Response, ApiError> {
    let world = world_id(&world)?;
    if !GAME_MODE_SESSION_KINDS.contains(&req.kind.as_str()) {
        return Err(ApiError::invalid("kind", "freeroam"));
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
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &HOST_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let rows = db::members(&state.pool, &c, world)
        .await?
        .ok_or_else(not_found)?;
    if !rows.iter().any(|m| m.role == "host" && m.xuid == c.xuid) {
        return Err(mm_error(MmError::NotHost));
    }
    let members: Vec<invite::Invited> = xuids
        .iter()
        .map(|x| {
            rows.iter().find(|m| m.xuid == *x).map(|m| invite::Invited {
                xuid: *x,
                gamertag: Some(m.gamertag.clone()),
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| ApiError::invalid("xuids", "the host or players seated in this world"))?;
    // CR-S92: the session is the invitation's starter's (`xuids[0]`, as on the official route):
    // the game's own flow has the starter own it, and a host that only joined dropped out of a
    // session made its own while the starter rewrote its entry for ever (live test 2026-11-04)
    let owner = match xuids.first().filter(|x| **x != c.xuid) {
        Some(first) => {
            let seat = db::seated_player(&state.pool, world, *first)
                .await?
                .ok_or_else(|| {
                    ApiError::invalid("xuids", "the host or players seated in this world")
                })?;
            fi_db::freeroam::Caller {
                game: c.game.clone(),
                profile_id: seat.profile_id,
                account_id: seat.account_id,
                session_id: seat.session_id,
                xuid: seat.xuid,
                tier: seat.tier,
                mode: seat.mode,
                ip: None,
            }
        }
        None => fi_db::freeroam::Caller {
            game: c.game.clone(),
            profile_id: c.profile_id,
            account_id: c.account_id,
            session_id: c.session_id,
            xuid: c.xuid,
            tier: c.tier.clone(),
            mode: c.mode.clone(),
            ip: None,
        },
    };
    let key = invite::create(&state.pool, &owner, world, &members).await?;
    let body = GameModeSession {
        kind: req.kind,
        scid: key.scid,
        template_name: key.template,
        name: key.name,
        members: xuids.iter().map(i64::to_string).collect(),
        world: None,
    };
    Ok((StatusCode::CREATED, axum::Json(body)).into_response())
}

/// `GET /mm/v1/watch` (OD-147, MATCHMAKING_API.md §3.9): the thresholds a hook measures the other
/// players' cars against. The blocked-car list stays here.
async fn watch(_ticket: Ticket) -> Response {
    let body = crate::relay::current_rules();
    private("private, max-age=3600", axum::Json(body).into_response())
}

/// `POST /mm/v1/worlds/{worldId}/observations` (OD-147, MATCHMAKING_API.md §3.9): what the
/// caller's hook saw of another player's car in a Hardcore world it hosts or is seated in. 204,
/// also for a repeated id; the quorum and the flag are settled in the same transaction. Bucket
/// `mm.observe` per play session.
async fn observe(
    State(state): State<AppState>,
    ticket: Ticket,
    Path(world): Path<String>,
    Body(req): Body<PeerObservation>,
) -> Result<Response, ApiError> {
    let world = world_id(&world)?;
    let id = Uuid::parse_str(&req.id).map_err(|_| ApiError::invalid("id", "a UUID"))?;
    let xuid = req
        .xuid
        .parse::<i64>()
        .ok()
        .filter(|x| *x > 0)
        .ok_or_else(|| ApiError::invalid("xuid", "a player's XUID, decimal"))?;
    let o = NewObservation {
        id,
        xuid,
        kind: req.kind,
        value: req.value,
        sustain_ms: req.sustain_ms,
        car_id: req.car_id,
        held_kmh: req.held_kmh,
        rules_version: req.rules_version,
    };
    let rules = seen::validate(&o)
        .map_err(|field| ApiError::invalid(field, "see MATCHMAKING_API.md §3.9"))?;
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &seen::OBSERVE_LIMIT, c.session_id).await? {
        return Err(rate_limited(secs));
    }
    match seen::observe(&state.pool, &c, world, &o, rules).await? {
        Observed::WorldNotFound => Err(not_found()),
        Observed::NotMember => Err(ApiError::invalid(
            "xuid",
            "the host or a player seated in this world, not the caller",
        )),
        Observed::Repeat => Ok(StatusCode::NO_CONTENT.into_response()),
        Observed::Stored {
            counts,
            quorum,
            flagged,
        } => {
            if quorum {
                tracing::info!(
                    event = "peer_quorum",
                    world_id = %world,
                    kind = o.kind,
                    flagged,
                    "peer observation quorum"
                );
            } else {
                tracing::debug!(
                    event = "peer_observation",
                    kind = o.kind,
                    counts,
                    "peer observation"
                );
            }
            Ok(StatusCode::NO_CONTENT.into_response())
        }
    }
}

/// `GET /mm/v1/worlds/by-code/{code}` (the exact lookup outside the list, OD-58).
async fn by_code(
    State(state): State<AppState>,
    ticket: Ticket,
    place: Place,
    Path(code): Path<String>,
) -> Result<Response, ApiError> {
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &LIST_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    // A malformed code reads like a wrong one (and counts like one).
    let code = normalize_code(&code).unwrap_or_else(|| "x".repeat(8));
    match db::world_by_code(&state.pool, &c, place.ip_text(), &code).await? {
        ByCode::Found(w) => {
            let mut a = db::assess(&w, &c.rules(place.cell), true);
            db::embed_visible(&state.pool, [&mut a], true).await?;
            let body = view(&a);
            Ok(private("no-store", axum::Json(body).into_response()))
        }
        ByCode::Wrong => Err(not_found()),
        ByCode::Limited(secs) => Err(rate_limited(u64::try_from(secs).unwrap_or(1))),
    }
}

/// The host's public IP: fi-api's [`ClientIp`] (`CF-Connecting-IP` when the request came through
/// the edge, else the rightmost `X-Forwarded-For` entry, else the peer; ADR 0903 §1), with an
/// IPv4-mapped IPv6 address unmapped. Never logged.
fn host_ip(ip: Option<IpAddr>) -> Option<IpAddr> {
    ip.map(|ip| match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    })
}

/// Most `endpoint.lan` addresses (MATCHMAKING_API.md §2.5).
const MAX_LAN: usize = 4;

/// The `endpoint.lan`: at most [`MAX_LAN`] private addresses (RFC 1918, link-local, `fc00::/7`,
/// `fe80::/10`), canonical text, duplicates dropped, in the host's order.
fn lan_addresses(list: &[String]) -> Result<Vec<String>, ApiError> {
    let bad = || ApiError::invalid("endpoint.lan", "up to 4 private addresses of the host's PC");
    if list.len() > MAX_LAN {
        return Err(bad());
    }
    let mut out: Vec<String> = Vec::new();
    for text in list {
        let ip = text
            .trim()
            .parse::<IpAddr>()
            .map(|ip| ip.to_canonical())
            .ok()
            .filter(fi_db::freeroam::is_lan)
            .ok_or_else(bad)?;
        let ip = ip.to_string();
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    Ok(out)
}

pub(crate) fn xuids(
    field: &'static str,
    list: &[String],
    max: usize,
) -> Result<Vec<i64>, ApiError> {
    if list.len() > max {
        return Err(ApiError::invalid(field, "too many entries"));
    }
    list.iter()
        .map(|x| {
            x.parse::<i64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| ApiError::invalid(field, "XUIDs, decimal"))
        })
        .collect()
}

fn text_ok(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
}

/// A checked `nat` (CR-S57, MATCHMAKING_API.md §5b).
fn nat(value: Option<&String>) -> Result<Option<&str>, ApiError> {
    match value.map(String::as_str) {
        None => Ok(None),
        Some(n) if fi_protocol::matchmaking::NAT_TYPES.contains(&n) => Ok(Some(n)),
        Some(_) => Err(ApiError::invalid("nat", "open, restricted or symmetric")),
    }
}

/// A checked `relays` map (CR-S57): at most 16 entries, keys the standard base64 of 32 bytes,
/// round trips 0 to 5000 ms. Keys the server does not trust are accepted and ignored.
fn relays(value: Option<&RelayLatencies>) -> Result<Option<&RelayLatencies>, ApiError> {
    let Some(map) = value else { return Ok(None) };
    if map.len() > fi_protocol::matchmaking::RELAYS_MAX {
        return Err(ApiError::invalid("relays", "at most 16 relays"));
    }
    let ok = map.iter().all(|(k, ms)| {
        k.len() == 44
            && fi_auth::b64::std_decode(k).is_some_and(|b| b.len() == 32)
            && *ms <= fi_protocol::matchmaking::RELAY_RTT_MAX_MS
    });
    if !ok {
        return Err(ApiError::invalid(
            "relays",
            "relay public key (base64 of 32 bytes) to 0-5000 ms",
        ));
    }
    Ok(Some(map))
}

/// The checked settings of a `PUT` (or the combined `POST`).
#[allow(clippy::too_many_arguments)]
fn settings(
    ticket: &Ticket,
    name: &str,
    mode: &str,
    modes: Option<&String>,
    max_players: u32,
    audience: &str,
    max_rtt_ms: Option<u32>,
    races: bool,
    message: Option<&String>,
    allow: Option<&Vec<String>>,
    mods: Option<&Vec<ModEntry>>,
    lobbies: Option<&Vec<String>>,
) -> Result<Settings, ApiError> {
    if !text_ok(name, 48) {
        return Err(ApiError::invalid("name", "1 to 48 characters"));
    }
    if mode != ticket.claims.mode {
        return Err(ApiError::new(
            ErrorCode::PortalModeMismatch,
            "a world has its host's mode",
        ));
    }
    // OD-141 (CR-S76): the host's own mode or `any`, chosen at creation.
    if modes.is_some_and(|m| !fi_protocol::matchmaking::WORLD_MODES.contains(&m.as_str())) {
        return Err(ApiError::invalid("modes", "hardcore, noob or any"));
    }
    if modes.is_some_and(|m| m != "any" && *m != ticket.claims.mode) {
        return Err(ApiError::new(
            ErrorCode::PortalModeMismatch,
            "a world is of its host's mode or open to any",
        ));
    }
    if !(1..=MAX_PLAYERS).contains(&max_players) {
        return Err(ApiError::invalid("maxPlayers", "1 to 12"));
    }
    if !fi_protocol::matchmaking::AUDIENCES.contains(&audience) {
        return Err(ApiError::invalid(
            "audience",
            "everyone, supporters, friends or private",
        ));
    }
    if max_rtt_ms.is_some_and(|m| !geo::MAX_RTT_STEPS.contains(&m)) {
        return Err(ApiError::invalid("maxRttMs", "60, 100 or 150"));
    }
    if message.is_some_and(|m| !text_ok(m, 140)) {
        return Err(ApiError::invalid("message", "up to 140 characters"));
    }
    let allow = allow.map(|l| xuids("allow", l, 256)).transpose()?;
    let mods = db::mod_set(mods.map(Vec::as_slice)).map_err(mm_error)?;
    // The lobby engines the host's world server runs (other projects used as reference): distinct known kinds.
    let lobbies = lobbies.cloned().unwrap_or_default();
    if lobbies.len() > fi_protocol::matchmaking::LOBBY_KINDS.len()
        || lobbies.iter().enumerate().any(|(i, l)| {
            !fi_protocol::matchmaking::LOBBY_KINDS.contains(&l.as_str()) || lobbies[..i].contains(l)
        })
    {
        return Err(ApiError::invalid(
            "lobbies",
            "distinct values of carmeet, roadtrip",
        ));
    }
    Ok(Settings {
        name: name.trim().to_owned(),
        mode: mode.to_owned(),
        open_modes: modes.cloned(),
        max_players: max_players as i32,
        audience: audience.to_owned(),
        max_rtt_ms: max_rtt_ms.map(|m| m as i32),
        races,
        message: message.cloned(),
        allow,
        mods,
        lobbies,
    })
}

/// A host's endpoint (PUT, combined POST, heartbeat): as [`endpoint`], except that an IPv6
/// observation with an IPv4 external measurement (CR-S40) is not refused. It comes back without
/// the external port and `true`: the host was reached over IPv6 (Cloudflare serves play on both
/// families) while STUN/UPnP measured IPv4, so its direct route cannot be told. A new world is
/// then born on a relay, an open one keeps its stored route (its heartbeat still counts).
fn host_endpoint(place: &Place, port: Option<&AnnouncePort>) -> Result<(Endpoint, bool), ApiError> {
    let mismatch =
        port.is_some_and(|p| p.external.is_some()) && place.ip.is_some_and(|ip| !ip.is_ipv4());
    if !mismatch {
        return Ok((endpoint(place, port)?, false));
    }
    let mut local = port.cloned();
    if let Some(p) = local.as_mut() {
        p.external = None;
    }
    Ok((endpoint(place, local.as_ref())?, true))
}

fn endpoint(place: &Place, port: Option<&AnnouncePort>) -> Result<Endpoint, ApiError> {
    let ip = place
        .ip
        .ok_or_else(|| ApiError::invalid("endpoint", "the caller's address is unknown"))?;
    let (port, local_port, lan) = match port {
        None => (None, None, None),
        Some(p) => {
            // CR-S40: current STUN/UPnP external ports are IPv4 measurements.
            // The trusted HTTPS observation must belong to that same family.
            if p.external.is_some() && !ip.is_ipv4() {
                return Err(ApiError::invalid(
                    "endpoint.external",
                    "an IPv4 observation is required for an external port",
                ));
            }
            if p.port == 0 {
                return Err(ApiError::invalid("endpoint.port", "1 to 65535"));
            }
            let external = p.external.as_ref().map_or(p.port, |e| e.port);
            if external == 0 {
                return Err(ApiError::invalid("endpoint.external.port", "1 to 65535"));
            }
            (
                Some(i32::from(external)),
                Some(i32::from(p.port)),
                Some(lan_addresses(p.lan.as_deref().unwrap_or_default())?),
            )
        }
    };
    Ok(Endpoint {
        ip: ip.to_string(),
        cell: place.cell,
        region: place.region.clone(),
        port,
        local_port,
        lan,
    })
}

fn announced(mm: &Mm, c: &Caller, hosted: &db::Hosted) -> Result<WorldAnnounced, ApiError> {
    fi_surfaces::network_info!(event = "mm_world", session_id = %c.session_id,
        world_id = %hosted.world.row.id, opened = hosted.opened,
        players = hosted.world.row.players, punch_count = hosted.punches.len(), "world announced");
    diagnostic_punches(c, &hosted.punches);
    Ok(WorldAnnounced {
        world: view(&hosted.world),
        punches: hosted
            .punches
            .iter()
            .map(|p| punch(mm, &RelayGrantHost::Xuid(c.xuid.to_string()), p))
            .collect::<Result<Vec<_>, _>>()?,
        // CR-L53 (§5f, DEDICATED_SERVER.md §4.3): the host holds no grant; its set's sync ids
        // reach its world through `world.context` `world.hostSync`.
        host_sync: crate::sync::SyncRegistry::builtin().ids(&hosted.world.row.mods),
    })
}

/// `PUT /mm/v1/worlds/{worldId}`: open or change the host's world (201 when opened).
async fn put(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    place: Place,
    Path(id): Path<String>,
    Body(s): Body<WorldSettings>,
) -> Result<Response, ApiError> {
    let world_id = Uuid::parse_str(&id)
        .ok()
        .filter(|u| !u.is_nil())
        .ok_or_else(|| ApiError::invalid("worldId", "a UUID minted by the host"))?;
    let c = caller(&ticket);
    let settings = settings(
        &ticket,
        &s.name,
        &s.mode,
        s.modes.as_ref(),
        s.max_players,
        &s.audience,
        s.max_rtt_ms,
        s.races,
        s.message.as_ref(),
        s.allow.as_ref(),
        s.mods.as_ref(),
        s.lobbies.as_ref(),
    )?;
    let (endpoint, family_mismatch) = host_endpoint(&place, Some(&s.endpoint))?;
    let relay = db::HostRelay {
        nat: nat(s.nat.as_ref())?,
        relays: relays(s.relays.as_ref())?,
        trusted: &mm.trusted,
        relay_only: relay_only(s.transport.as_ref())?,
        family_mismatch,
        activity: activity(s.activity.as_ref())?,
        voice: voice(s.voice.as_ref())?,
        host_away: None,
    };
    if let Some(secs) = limited(&state, &SETTINGS_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let hosted = db::host_with_relay(
        &state.pool,
        &c,
        world_id,
        Some(&settings),
        &endpoint,
        None,
        mm.world_idle_secs,
        &relay,
    )
    .await
    .map_err(|error| diagnostic_error("settings", &c, mm_error(error)))?;
    let status = if hosted.opened {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    diagnostic_endpoint(&c, "settings", Some(&s.endpoint));
    let body = announced(&mm, &c, &hosted)?;
    Ok(private(
        "no-store",
        (status, axum::Json(body)).into_response(),
    ))
}

/// `POST /mm/v1/worlds/{worldId}/announce`: the host's heartbeat.
async fn heartbeat(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    place: Place,
    Path(id): Path<String>,
    Body(h): Body<Heartbeat>,
) -> Result<Response, ApiError> {
    let world_id = world_id(&id)?;
    let c = caller(&ticket);
    let peers = xuids("peers", &h.peers, db::MAX_PEERS_DEDICATED)?;
    let relay = db::HostRelay {
        activity: activity(h.activity.as_ref())?,
        voice: voice(h.voice.as_ref())?,
        host_away: Some(h.host_away),
        ..db::HostRelay::default()
    };
    // A heartbeat whose route family cannot be told keeps the stored route and still counts.
    let endpoint = match host_endpoint(&place, h.endpoint.as_ref())? {
        (_, true) => endpoint(&place, None)?,
        (e, false) => e,
    };
    if let Some(secs) = limited(&state, &ANNOUNCE_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let hosted = db::host_with_relay(
        &state.pool,
        &c,
        world_id,
        None,
        &endpoint,
        Some(&peers),
        mm.world_idle_secs,
        &relay,
    )
    .await
    .map_err(|error| diagnostic_error("heartbeat", &c, mm_error(error)))?;
    diagnostic_endpoint(&c, "heartbeat", h.endpoint.as_ref());
    let body = announced(&mm, &c, &hosted)?;
    Ok(private("no-store", axum::Json(body).into_response()))
}

/// `POST /mm/v1/worlds`: the settings and the heartbeat in one call (the first version's route).
async fn announce(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    place: Place,
    Body(a): Body<Announce>,
) -> Result<Response, ApiError> {
    let world_id = Uuid::parse_str(&a.world_id)
        .ok()
        .filter(|u| !u.is_nil())
        .ok_or_else(|| ApiError::invalid("worldId", "a UUID minted by the host"))?;
    let c = caller(&ticket);
    let settings = settings(
        &ticket,
        &a.name,
        &a.mode,
        a.modes.as_ref(),
        a.max_players,
        &a.audience,
        a.max_rtt_ms,
        a.races,
        a.message.as_ref(),
        a.allow.as_ref(),
        a.mods.as_ref(),
        a.lobbies.as_ref(),
    )?;
    let peers = xuids("peers", &a.peers, db::MAX_PEERS_DEDICATED)?;
    let (endpoint, family_mismatch) = host_endpoint(&place, Some(&a.endpoint))?;
    let relay = db::HostRelay {
        nat: nat(a.nat.as_ref())?,
        relays: relays(a.relays.as_ref())?,
        trusted: &mm.trusted,
        relay_only: relay_only(a.transport.as_ref())?,
        family_mismatch,
        activity: activity(a.activity.as_ref())?,
        voice: voice(a.voice.as_ref())?,
        host_away: Some(a.host_away),
    };
    if let Some(secs) = limited(&state, &ANNOUNCE_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let hosted = db::host_with_relay(
        &state.pool,
        &c,
        world_id,
        Some(&settings),
        &endpoint,
        Some(&peers),
        mm.world_idle_secs,
        &relay,
    )
    .await
    .map_err(|error| diagnostic_error("combined", &c, mm_error(error)))?;
    diagnostic_endpoint(&c, "combined", Some(&a.endpoint));
    let body = announced(&mm, &c, &hosted)?;
    Ok(private("no-store", axum::Json(body).into_response()))
}

/// `POST /mm/v1/worlds/{worldId}/code`: a new code (host).
async fn new_code(
    State(state): State<AppState>,
    ticket: Ticket,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let world_id = world_id(&id)?;
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &SETTINGS_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let a = db::new_code(&state.pool, &c, world_id)
        .await
        .map_err(mm_error)?;
    Ok(private("no-store", axum::Json(view(&a)).into_response()))
}

/// `PUT /mm/v1/worlds/{worldId}/modes {modes}` (OD-210, CR-S101, §2.11): a private official world's
/// current leader picks who may join it from now on; seats already granted stay. Bucket
/// `mm.settings`, as a host's other world settings.
async fn join_modes(
    State(state): State<AppState>,
    ticket: Ticket,
    Path(id): Path<String>,
    Body(m): Body<fi_protocol::matchmaking::WorldJoinModes>,
) -> Result<Response, ApiError> {
    let world_id = world_id(&id)?;
    if !fi_protocol::matchmaking::WORLD_MODES.contains(&m.modes.as_str()) {
        return Err(ApiError::invalid("modes", "any, hardcore or noob"));
    }
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &SETTINGS_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let a = db::set_private_modes(&state.pool, &c, world_id, &m.modes)
        .await
        .map_err(mm_error)?;
    fi_surfaces::network_info!(event = "mm_join_modes", session_id = %c.session_id,
        world_id = %world_id, modes = m.modes.as_str(), "private world join modes set");
    Ok(private("no-store", axum::Json(view(&a)).into_response()))
}

/// `DELETE /mm/v1/worlds/{worldId}` (the host left free roam).
async fn close(
    State(state): State<AppState>,
    ticket: fi_surfaces::game::ticket::CleanupTicket,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let world = world_id(&id)?;
    let c = caller(&ticket.0);
    if let Some(secs) = limited(&state, &HOST_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    db::close_world(&state.pool, &c, world)
        .await
        .map_err(|error| diagnostic_error("close", &c, mm_error(error)))?;
    fi_surfaces::network_info!(event = "mm_close", session_id = %c.session_id,
        world_id = %world, outcome = "closed", "host closed world");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /mm/v1/worlds/{worldId}/moderation`.
async fn moderate(
    State(state): State<AppState>,
    ticket: Ticket,
    Path(world): Path<String>,
    Body(m): Body<Moderation>,
) -> Result<Response, ApiError> {
    let world = world_id(&world)?;
    let action = match m.action.as_str() {
        "kick" => Action::Kick,
        "ban" => Action::Ban,
        "unban" => Action::Unban,
        _ => return Err(ApiError::invalid("action", "kick, ban or unban")),
    };
    let target: i64 = m
        .xuid
        .parse()
        .ok()
        .filter(|v: &i64| *v > 0)
        .ok_or_else(|| ApiError::invalid("xuid", "a XUID, decimal"))?;
    let until = match (&m.until, action) {
        (None, _) => None,
        (Some(t), Action::Ban) => {
            let ms = fi_protocol::instant::parse_unix_micros(t)
                .map(|us| us.div_euclid(1_000))
                .filter(|ms| *ms > UnixMs::now_system().millis())
                .ok_or_else(|| ApiError::invalid("until", "a future instant"))?;
            Some(UnixMs::from_millis(ms))
        }
        (Some(_), _) => return Err(ApiError::invalid("until", "only a ban has an end")),
    };
    if m.reason.as_deref().is_some_and(|r| !text_ok(r, 200)) {
        return Err(ApiError::invalid("reason", "up to 200 characters"));
    }
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &HOST_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    db::moderate(
        &state.pool,
        &c,
        world,
        action,
        target,
        until,
        m.reason.as_deref(),
    )
    .await
    .map_err(mm_error)?;
    Ok(axum::Json(ModerationResult {
        action: m.action.clone(),
        xuid: m.xuid.clone(),
        until: until.map(views::at),
    })
    .into_response())
}

fn offer(o: &OfferRow, to_handle: i64) -> Invite {
    Invite {
        id: o.id.to_string(),
        kind: o.kind.clone(),
        world_id: o.world_id.to_string(),
        to: to_handle.to_string(),
        state: o.state.clone(),
        created_at: views::at(o.created_at),
        expires_at: views::at(o.expires_at),
        grant_id: o.grant_id.map(|g| g.to_string()),
    }
}

fn sent(s: &Sent) -> Response {
    let status = if s.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    private(
        "no-store",
        (status, axum::Json(offer(&s.offer, s.to_handle))).into_response(),
    )
}

/// `POST /mm/v1/worlds/{worldId}/invites {xuid}` (host, Supporter; OD-56).
async fn invite(
    State(state): State<AppState>,
    ticket: Ticket,
    Path(id): Path<String>,
    Body(i): Body<InviteRequest>,
) -> Result<Response, ApiError> {
    let world_id = world_id(&id)?;
    let target: i64 = i
        .xuid
        .parse()
        .ok()
        .filter(|v: &i64| *v > 0)
        .ok_or_else(|| ApiError::invalid("xuid", "a XUID, decimal"))?;
    let s = db::invite(&state.pool, &caller(&ticket), world_id, target)
        .await
        .map_err(mm_error)?;
    Ok(sent(&s))
}

/// `POST /mm/v1/worlds/{worldId}/requests` (Supporter; OD-59: ask the host to let the caller in).
async fn ask(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let world_id = world_id(&id)?;
    // OD-112: an optional `{mods?}` body (none, `{}` or `[]`: vanilla).
    let asked: RequestToJoin = if body.is_empty() {
        RequestToJoin::default()
    } else {
        fi_surfaces::extract::parse(&body)?
    };
    let mods = db::mod_set(asked.mods.as_deref()).map_err(mm_error)?;
    // OD-160: the asker's relay report, for an acceptance onto a relay-born world's relay.
    let reported = relays(asked.relays.as_ref())?;
    let s = db::ask_with_relay(
        &state.pool,
        &caller(&ticket),
        world_id,
        &mods,
        reported,
        &mm.trusted,
    )
    .await
    .map_err(mm_error)?;
    Ok(sent(&s))
}

/// `POST /mm/v1/worlds/{worldId}/requests/{requestId}/accept` (host; OD-59).
async fn accept(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    Path((id, request)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let world_id = world_id(&id)?;
    let request = Uuid::parse_str(&request)
        .map_err(|_| ApiError::new(ErrorCode::PortalNotFound, "not found"))?;
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &HOST_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let accepted = db::accept_request_with_relay(
        &state.pool,
        &c,
        world_id,
        request,
        mm.seat_hold_secs,
        &mm.trusted,
        mm.relay_seats,
    )
    .await
    .map_err(mm_error)?;
    Ok(private(
        "no-store",
        axum::Json(offer(&accepted.offer, accepted.to_handle)).into_response(),
    ))
}

fn sign(
    mm: &Mm,
    claims: RelayGrantClaims,
    proof: Option<&db::UdpProofKey>,
) -> Result<RelayGrant, ApiError> {
    let mut grant = relay_grant::sign(&mm.grant_keys, claims).map_err(|e| {
        tracing::error!(error = %e, "relay grant out of bounds");
        ApiError::unavailable()
    })?;
    grant.udp_proof = proof.map(|key| fi_protocol::matchmaking::RelayUdpProof {
        version: 1,
        key: fi_auth::b64::std_encode(key.as_bytes()),
    });
    Ok(grant)
}

#[allow(clippy::too_many_arguments)]
fn claims(
    grant: Uuid,
    world: Uuid,
    profile: Uuid,
    xuid: i64,
    tier: &str,
    host: RelayGrantHost,
    role: &str,
    iat: i64,
    exp: i64,
    relay: Option<&crate::config::Relay>,
    voice_blocked_until: Option<UnixMs>,
    mods: &[ModEntry],
) -> RelayGrantClaims {
    RelayGrantClaims {
        v: 1,
        kid: String::new(),
        grant: grant.to_string(),
        world: world.to_string(),
        sub: profile.to_string(),
        xuid: xuid.to_string(),
        tier: tier.to_owned(),
        // OD-189 (§2.9): an official world's grant names no relay.
        relay: relay
            .filter(|_| host.official().is_none())
            .map(|r| RelayGrantRelay {
                key: r.key.clone(),
                addr: r.addr.clone(),
                binding: r.binding,
                port: r.port,
            }),
        host,
        role: role.to_owned(),
        iat,
        exp,
        // CR-L53 (§5f): the registry ids of this seat's declared set (`mm_grant.mods`), never
        // the other side's; omitted when none.
        sync: crate::sync::SyncRegistry::builtin().ids(mods),
        // CR-S67: Unix seconds, the stored millisecond end rounded up to the second.
        voice_blocked_until: voice_blocked_until.map(|u| (u.millis() + 999).div_euclid(1000)),
    }
}

/// The host claim of a grant: the official world's `vmId`, else the host's XUID.
fn grant_host(xuid: i64, vm_id: Option<&str>) -> RelayGrantHost {
    match vm_id {
        Some(vm) => RelayGrantHost::Official(vm.to_owned()),
        None => RelayGrantHost::Xuid(xuid.to_string()),
    }
}

/// The trusted relay a stored seat names (CR-S57; `None`: none, or no longer trusted) and the
/// seat's transport as signed: never `relay` without a relay in the claims.
fn pair_relay<'m>(
    mm: &'m Mm,
    key: Option<&str>,
    transport: &str,
) -> (Option<&'m crate::config::Relay>, String) {
    let relay = key.and_then(|k| mm.relay(k));
    let transport = if relay.is_some() && transport == "relay" {
        "relay"
    } else {
        "direct"
    };
    (relay, transport.to_owned())
}

fn network_candidates(rows: &[db::NetworkCandidateRow]) -> Vec<NetworkCandidate> {
    rows.iter()
        .filter_map(|row| {
            let kind = match row.kind {
                "public" => NetworkCandidateKind::Public,
                "lan" => NetworkCandidateKind::Lan,
                _ => return None,
            };
            Some(NetworkCandidate {
                kind,
                ip: row.ip.clone(),
                port: row.port,
            })
        })
        .collect()
}

/// A punch for the host: issued grants keep their times; a live used seat is re-signed for the
/// next60s. Routing candidates never substitute for authenticated FIUP socket ownership.
/// A punch for `host`'s announce: a player host's carries the host-role copy of the grant, an
/// official world's the joiner's own (no host-role grant exists for it, OD-189, §2.9).
pub(crate) fn punch(mm: &Mm, host: &RelayGrantHost, p: &PunchRow) -> Result<Punch, ApiError> {
    let (iat, exp) = fresh(p.state == "used", p.issued_at, p.expires_at);
    let (relay, transport) = pair_relay(mm, p.relay_key.as_deref(), &p.transport);
    let role = if host.official().is_some() {
        "joiner"
    } else {
        "host"
    };
    Ok(Punch {
        transport,
        grant_id: p.grant_id.to_string(),
        xuid: p.xuid.to_string(),
        endpoint: None,
        candidates: if p.state == "kicked" {
            None
        } else {
            p.candidates.as_ref().map(|h| NetworkHints {
                revision: h.revision,
                candidates: network_candidates(&h.candidates),
                generated_at: views::at(h.generated_at),
                expires_at: views::at(h.expires_at),
            })
        },
        relay: sign(
            mm,
            claims(
                p.grant_id,
                p.world_id,
                p.profile_id,
                p.xuid,
                &p.tier,
                host.clone(),
                role,
                iat,
                exp,
                relay,
                p.voice_blocked_until,
                &p.mods,
            ),
            if p.state == "kicked" {
                None
            } else {
                p.udp_proof_key.as_ref()
            },
        )?,
        kicked: p.state == "kicked",
    })
}

/// A relay grant's times: a used seat now + 60 s; a reserved one from its issue (or now, once it
/// is older than a renew) to its expiry, at most 60 s.
fn fresh(used: bool, issued: UnixMs, expires: UnixMs) -> (i64, i64) {
    let now = now_secs();
    let ttl = fi_protocol::matchmaking::GRANT_TTL_SECS;
    if used {
        return (now, now + ttl);
    }
    let exp = expires.millis() / 1000;
    let iat = (issued.millis() / 1000).max(exp - ttl);
    (iat, exp.max(iat + 1))
}

fn grant_body(mm: &Mm, c: &Caller, g: &GrantIssued) -> Result<Grant, ApiError> {
    let (iat, exp) = fresh(false, g.issued_at, g.expires_at);
    let (relay, transport) = pair_relay(mm, g.relay_key.as_deref(), &g.transport);
    Ok(Grant {
        transport,
        id: g.id.to_string(),
        world_id: g.world_id.to_string(),
        expires_at: views::at(g.expires_at),
        hold_until: views::at(g.hold_until),
        relay: sign(
            mm,
            claims(
                g.id,
                g.world_id,
                c.profile_id,
                c.xuid,
                &c.tier,
                grant_host(g.host_xuid, g.host_vm_id.as_deref()),
                "joiner",
                iat,
                exp,
                relay,
                g.voice_blocked_until,
                &g.mods,
            ),
            g.udp_proof_key.as_ref(),
        )?,
    })
}

fn join_rate_limited(secs: u64) -> Response {
    let mut answer = JoinAnswer::status("rate_limited");
    answer.retry_after_seconds = Some(u32::try_from(secs).unwrap_or(60));
    let mut response = (StatusCode::TOO_MANY_REQUESTS, axum::Json(answer)).into_response();
    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, v);
    }
    response
}

/// Private UDP address rendezvous. The HTTP edge supplies the public IP, not the request body.
/// An exact revision retry never refreshes hint freshness or renews world/seat authority.
async fn seat_network(
    State(state): State<AppState>,
    ticket: Ticket,
    place: Place,
    Body(input): Body<SeatNetworkRequest>,
) -> Result<Response, ApiError> {
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &NETWORK_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let grant = Uuid::parse_str(&input.grant_id)
        .map_err(|_| ApiError::invalid("grantId", "a grant UUID"))?;
    if input.revision == 0 {
        return Err(ApiError::invalid("revision", "1 to 4294967295"));
    }
    let e = endpoint(&place, Some(&input.endpoint))?;
    let answer = db::seat_network(&state.pool, &c, grant, input.revision, &e)
        .await
        .map_err(|error| diagnostic_error("seat_network", &c, mm_error(error)))?;
    fi_surfaces::network_info!(event = "mm_network", session_id = %c.session_id,
        world_id = %answer.world_id, grant_id = %grant, revision = answer.revision,
        external_mapping = input.endpoint.external.as_ref().map_or("none", |v| match v.source {
            fi_protocol::matchmaking::AnnounceExternalSource::Stun => "stun",
            fi_protocol::matchmaking::AnnounceExternalSource::Upnp => "upnp",
        }),
        public_candidates = answer.candidates.iter().filter(|v| v.kind == "public").count(),
        lan_candidates = answer.candidates.iter().filter(|v| v.kind == "lan").count(),
        hint_remaining_ms = (answer.expires_at.millis() - answer.generated_at.millis()).max(0),
        "network hints exchanged");
    let body = SeatNetworkAnswer {
        grant_id: answer.grant_id.to_string(),
        world_id: answer.world_id.to_string(),
        revision: answer.revision,
        candidates: network_candidates(&answer.candidates),
        generated_at: views::at(answer.generated_at),
        expires_at: views::at(answer.expires_at),
    };
    Ok(private("no-store", axum::Json(body).into_response()))
}

/// `Retry-After` of a private create's 409 `lobbies_full` (its own private world is full;
/// MATCHMAKING_API.md §2.10).
pub const LOBBIES_FULL_RETRY_SECS: u64 = 2;

/// `Retry-After` of the 503 a quick join or private create answers when no official world took the
/// caller within the bounded wait (CR-S100, WORLDS_OFFICIAL.md §13): at least the wait itself, so a
/// launcher honouring it once cannot spend `mm.join` (6 a minute) on an incident.
pub const NO_OFFICIAL_ROOM_RETRY_SECS: u64 = 15;

/// How often the bounded wait reads whether room appeared (each read its own pooled connection,
/// released before the next sleep; never a transaction across the wait).
pub const OFFICIAL_WAIT_POLL_MS: u64 = 500;

/// CR-S100's incident answer: 503 `SERVICE.UNAVAILABLE` with `Retry-After`.
fn no_official_room() -> ApiError {
    ApiError::new(
        ErrorCode::ServiceUnavailable,
        "no official lobby has room right now",
    )
    .retry_after(NO_OFFICIAL_ROOM_RETRY_SECS)
}

/// `POST /mm/v1/join {worldId?, code?}`: reserve a seat (never queued, OD-58).
async fn join(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    place: Place,
    Body(j): Body<JoinRequest>,
) -> Result<Response, ApiError> {
    let c = caller(&ticket);
    let mut world = j.world_id.as_deref().map(world_id).transpose()?;
    let mods = db::mod_set(j.mods.as_deref()).map_err(mm_error)?;
    let relay = db::JoinRelay {
        nat: nat(j.nat.as_ref())?,
        relays: relays(j.relays.as_ref())?,
        trusted: &mm.trusted,
        max_relayed: mm.relay_seats,
        relay_only: relay_only(j.transport.as_ref())?,
        region: j.region.as_deref(),
    };
    // OD-205: only a region a Worlds host declares.
    if let Some(region) = j.region.as_deref()
        && !mm
            .worlds
            .iter()
            .any(|w| w.region.as_deref() == Some(region))
    {
        return Err(ApiError::invalid("region", "a region of this server"));
    }
    // CR-S99 (FM5_LOBBIES.md §1): the ticket's game decides its activities; another game's is 400.
    let rules = fi_domain::GameRules::of(&c.game);
    let foreign_activity = j.activity.as_deref().is_some_and(|a| {
        fi_protocol::matchmaking::OFFICIAL_ACTIVITY_KINDS.contains(&a)
            && !rules.official_activity(a)
    });
    // CR-S96 (§2.10): private create is by activity alone, and a car meet is public only.
    if j.private {
        if world.is_some() || j.code.is_some() {
            return Err(ApiError::invalid("private", "without worldId and code"));
        }
        if !foreign_activity
            && !j
                .activity
                .as_deref()
                .is_some_and(|a| rules.private_activity(a))
        {
            return Err(mm_error(MmError::Conflict("activity")));
        }
    }
    // OD-190: `carmeet` is an official activity (quick join, join friend); a player world never
    // runs it, so asking for it there is the `activity` conflict.
    let asked = match j.activity.as_deref() {
        None => None,
        Some(a)
            if !foreign_activity
                && fi_protocol::matchmaking::OFFICIAL_ACTIVITY_KINDS.contains(&a) =>
        {
            Some(a.to_owned())
        }
        Some(_) => {
            return Err(ApiError::invalid(
                "activity",
                format!("one of {}", rules.official_activities.join(", ")),
            ));
        }
    };

    // A malformed code reads like a wrong one.
    let code = j
        .code
        .as_deref()
        .map(|t| normalize_code(t).unwrap_or_else(|| "x".repeat(8)));
    if let Some(secs) = limited(&state, &JOIN_LIMIT, c.account_id).await? {
        return Ok(join_rate_limited(secs));
    }
    let quick = world.is_none() && j.code.is_none();
    let input = JoinInput {
        world_id: world,
        code,
        ip: place.ip_text(),
        cell: place.cell,
        hold_secs: mm.seat_hold_secs,
        mods,
        activity: asked,
    };
    // OD-190 (§2.9): neither a world nor a code is the quick join: leave the old seat, take the
    // oldest open official world of that activity with a seat. CR-S100 (WORLDS_OFFICIAL.md §13):
    // with no room (every world of the placement full, or no idle spare for a private create),
    // wait up to `FI_MM_OFFICIAL_WAIT_MS` for the agent to open one, polling short reads; each
    // attempt is its own transaction and nothing is held across a sleep. Still none: 503.
    let outcome = if j.private || quick {
        let activity = input.activity.clone().unwrap_or_else(|| {
            if j.private {
                String::new()
            } else {
                "freeroam".into()
            }
        });
        let started = tokio::time::Instant::now();
        let deadline = started + std::time::Duration::from_millis(mm.official_wait_ms);
        let poll = std::time::Duration::from_millis(OFFICIAL_WAIT_POLL_MS);
        loop {
            let outcome = if j.private {
                db::official_private_create(&state.pool, &c, &activity, &input, &relay).await
            } else {
                db::official_quick_join(&state.pool, &c, &activity, &input, &relay).await
            };
            if !matches!(outcome, Err(MmError::NoOfficialRoom)) {
                break outcome;
            }
            // Wait for room, reading every `poll`, until the deadline.
            let mut room = false;
            while !room {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::time::sleep(poll.min(deadline - now)).await;
                room =
                    db::official_room(&state.pool, &c, &activity, j.private, relay.region).await?;
            }
            if !room {
                fi_surfaces::network_warn!(event = "mm_official_no_room",
                    session_id = %c.session_id, activity = activity.as_str(), private = j.private,
                    waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "no official world took the caller within the wait: an incident (CR-S100)");
                break outcome;
            }
        }
    } else {
        db::join_with_relay(&state.pool, &c, &input, &relay).await
    };
    // A10: a flagged Hardcore caller's refusal says until when (MATCHMAKING_API.md §2.10).
    let pool_until = if matches!(outcome, Err(MmError::Conflict("pool"))) {
        db::pool_until(&state.pool, &c).await?
    } else {
        None
    };
    let outcome = outcome.map_err(|error| {
        let full = matches!(error, MmError::Conflict("lobbies_full"));
        let e = diagnostic_error("join", &c, mm_error(error));
        if full {
            e.retry_after(LOBBIES_FULL_RETRY_SECS)
        } else if let Some(until) = pool_until {
            e.with_details(serde_json::json!({"reason": "pool", "until": views::at(until)}))
        } else {
            e
        }
    })?;
    if let JoinOutcome::Granted(g) = &outcome {
        world = Some(g.world_id);
    }
    let answer = match outcome {
        JoinOutcome::Granted(g) => {
            let mut a = JoinAnswer::status("granted");
            a.grant = Some(grant_body(&mm, &c, &g)?);
            a
        }
        JoinOutcome::Full {
            players,
            max_players,
        } => {
            let mut a = JoinAnswer::status("full");
            a.world = Some(WorldSeats {
                players,
                max_players,
            });
            a
        }
        JoinOutcome::Locked {
            world_id,
            grant_id,
            expires_at,
        } => {
            let mut a = JoinAnswer::status("locked");
            a.locked = Some(Locked {
                world_id: world_id.to_string(),
                grant_id: grant_id.to_string(),
                expires_at: views::at(expires_at),
            });
            a
        }
        JoinOutcome::NotAllowed(reason) => {
            let mut a = JoinAnswer::status("not_allowed");
            a.reason = Some(reason.as_str().to_owned());
            a
        }
        JoinOutcome::Latency { rtt_ms, max_rtt_ms } => {
            let mut a = JoinAnswer::status("not_allowed");
            a.reason = Some("latency".into());
            a.latency = Some(LatencyRefusal { rtt_ms, max_rtt_ms });
            a
        }
        JoinOutcome::Code => JoinAnswer::status("code"),
        JoinOutcome::Closed => JoinAnswer::status("closed"),
        JoinOutcome::RateLimited(secs) => {
            fi_surfaces::network_warn!(event = "mm_join", session_id = %c.session_id,
                status = "rate_limited", "code attempts limited");
            return Ok(join_rate_limited(u64::try_from(secs).unwrap_or(1)));
        }
    };
    fi_surfaces::network_info!(event = "mm_join", session_id = %c.session_id,
        requested_world_id = ?world, status = answer.status.as_str(), reason = answer.reason.as_deref(),
        world_id = answer.grant.as_ref().map(|v| v.world_id.as_str()),
        grant_id = answer.grant.as_ref().map(|v| v.id.as_str()),
        locked_world_id = answer.locked.as_ref().map(|v| v.world_id.as_str()),
        proof_available = answer.grant.as_ref().is_some_and(|v| v.relay.udp_proof.is_some()),
        "join decision");
    Ok(private("no-store", axum::Json(answer).into_response()))
}

/// `DELETE /mm/v1/join/{grantId}`.
async fn release(
    State(state): State<AppState>,
    ticket: fi_surfaces::game::ticket::CleanupTicket,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let c = caller(&ticket.0);
    if let Some(secs) = limited(&state, &SEAT_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    if let Ok(id) = Uuid::parse_str(&id) {
        db::release(&state.pool, &c, id).await?;
        fi_surfaces::network_info!(event = "mm_leave", session_id = %c.session_id, grant_id = %id, "grant released");
    }
    Ok(StatusCode::NO_CONTENT)
}

fn seat_answer(mm: &Mm, c: &Caller, v: &SeatView) -> Result<SeatAnswer, ApiError> {
    let seat = match &v.now {
        None => None,
        Some(SeatNow::Hosting(w)) => Some(Seat {
            state: "hosting".into(),
            world_id: w.row.id.to_string(),
            world: view(w),
            grant_id: None,
            expires_at: None,
            hold_until: None,
            relay: None,
            transport: None,
            session: None,
        }),
        Some(SeatNow::Seat(g, w)) => {
            let used = g.state == "used";
            let (iat, exp) = fresh(used, v.at, g.expires_at);
            let (pair, transport) = pair_relay(mm, g.relay_key.as_deref(), &g.transport);
            let relay = sign(
                mm,
                claims(
                    g.id,
                    w.row.id,
                    c.profile_id,
                    c.xuid,
                    &c.tier,
                    grant_host(g.host_xuid, g.host_vm_id.as_deref()),
                    "joiner",
                    iat,
                    exp,
                    pair,
                    g.voice_blocked_until,
                    &g.mods,
                ),
                g.udp_proof_key.as_ref(),
            )?;
            Some(Seat {
                state: if used { "seated" } else { "reserved" }.into(),
                world_id: w.row.id.to_string(),
                world: {
                    let mut world = view(w);
                    world.leader = v.private_leader.map(|x| x.to_string());
                    world
                },
                grant_id: Some(g.id.to_string()),
                expires_at: (!used).then(|| views::at(g.expires_at)),
                hold_until: (!used).then(|| views::at(g.hold_until)),
                relay: Some(relay),
                transport: Some(transport),
                session: v
                    .private_session
                    .as_ref()
                    .map(|(scid, template, name)| InviteSession {
                        scid: scid.clone(),
                        template_name: template.clone(),
                        name: name.clone(),
                    }),
            })
        }
    };
    Ok(SeatAnswer {
        private_world_id: v.private_world.map(|w| w.to_string()),
        seat,
        ended: v.ended.as_ref().map(|e| SeatEnded {
            world_id: e.world_id.to_string(),
            grant_id: e.grant_id.to_string(),
            at: views::at(e.at),
            reason: e.reason.clone(),
        }),
        request: v.request.as_ref().map(|o| SeatRequest {
            id: o.id.to_string(),
            world_id: o.world_id.to_string(),
            state: o.state.clone(),
            expires_at: views::at(o.expires_at),
        }),
        members: None,
        pool: v.pool_until.map(|until| CallerPool {
            until: views::at(until),
        }),
        generated_at: views::at(v.at),
    })
}

/// `GET /mm/v1/seat`: the launcher's seat status and the hook's relay grant.
async fn seat(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    place: Place,
    Query(query): Query<SeatQuery>,
) -> Result<Response, ApiError> {
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &SEAT_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let body = if query.include_members {
        let snapshot = db::seat_with_members(&state.pool, &c, place.ip_text(), place.cell)
            .await
            .map_err(|error| diagnostic_error("seat_read", &c, mm_error(error)))?;
        let mut body = seat_answer(&mm, &c, &snapshot.seat)?;
        if let (Some(seat), Some(rows)) = (&body.seat, &snapshot.members) {
            body.members = Some(members_body(seat.world_id.clone(), rows, snapshot.seat.at));
        }
        body
    } else {
        let v = db::seat(&state.pool, &c, place.ip_text(), place.cell)
            .await
            .map_err(|error| diagnostic_error("seat_read", &c, error.into()))?;
        seat_answer(&mm, &c, &v)?
    };
    diagnostic_seat(&c, &body, "read");
    Ok(private("no-store", axum::Json(body).into_response()))
}

/// `POST /mm/v1/seat/renew {grantId, transport?}`: `relay` (CR-S57) or `direct` (OD-160).
async fn renew(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
    place: Place,
    Body(b): Body<SeatRenew>,
) -> Result<Response, ApiError> {
    let c = caller(&ticket);
    let grant =
        Uuid::parse_str(&b.grant_id).map_err(|_| ApiError::invalid("grantId", "a grant id"))?;
    let (fall_back, go_direct) = match b.transport.as_deref() {
        None => (false, false),
        Some("relay") => (true, false),
        Some("direct") => (false, true),
        Some(_) => return Err(ApiError::invalid("transport", "relay or direct")),
    };
    if let Some(secs) = limited(&state, &SEAT_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    // CR-S57: the joiner's punch failed; record it before the renew reads the seat (one pooled
    // connection at a time: the fallback's statement ends before the renew begins).
    if fall_back {
        match db::fall_back(&state.pool, &c, grant, &mm.trusted, mm.relay_seats).await? {
            db::FallBack::NoRelay => {
                return Err(mm_error(MmError::Conflict("no_relay")));
            }
            db::FallBack::Busy => {
                return Err(mm_error(MmError::Conflict("relay_busy")));
            }
            db::FallBack::OfficialDirect => {
                return Err(mm_error(MmError::Conflict("official_direct")));
            }
            db::FallBack::Relay | db::FallBack::NotFound => {}
        }
        fi_surfaces::network_info!(event = "mm_fallback", session_id = %c.session_id,
            grant_id = %grant, "seat moved to its relay");
    }
    // OD-160: a Hardcore joiner who could not reach the world's relay reached the host directly.
    if go_direct {
        match db::go_direct(&state.pool, &c, grant).await? {
            db::GoDirect::Refused => {
                return Err(mm_error(MmError::Conflict("no_direct")));
            }
            // Logged only when the seat moved (the direct fallback's count, OD-160).
            db::GoDirect::Direct => {
                fi_surfaces::network_info!(event = "mm_direct", session_id = %c.session_id,
                    grant_id = %grant, "seat moved to the host directly");
            }
            db::GoDirect::AlreadyDirect | db::GoDirect::NotFound => {}
        }
    }
    let v = db::renew(&state.pool, &c, grant, place.ip_text(), place.cell)
        .await
        .map_err(|error| diagnostic_error("seat_renew", &c, error.into()))?;
    let body = seat_answer(&mm, &c, &v)?;
    diagnostic_seat(&c, &body, "renew");
    Ok(private("no-store", axum::Json(body).into_response()))
}

/// `DELETE /mm/v1/seat`.
async fn leave(
    State(state): State<AppState>,
    ticket: fi_surfaces::game::ticket::CleanupTicket,
) -> Result<StatusCode, ApiError> {
    let c = caller(&ticket.0);
    if let Some(secs) = limited(&state, &SEAT_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    db::leave_seat(&state.pool, &c).await?;
    fi_surfaces::network_info!(event = "mm_leave", session_id = %c.session_id, "seat released");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /mm/v1/punches`.
async fn punches(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    ticket: Ticket,
) -> Result<Response, ApiError> {
    let c = caller(&ticket);
    if let Some(secs) = limited(&state, &PUNCHES_LIMIT, c.account_id).await? {
        return Err(rate_limited(secs));
    }
    let rows = db::punches(&state.pool, &c)
        .await
        .map_err(|error| diagnostic_error("punches", &c, error.into()))?;
    diagnostic_punches(&c, &rows);
    let items = rows
        .iter()
        .map(|p| punch(&mm, &RelayGrantHost::Xuid(c.xuid.to_string()), p))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(private(
        "no-store",
        axum::Json(Punches { items }).into_response(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityQuery {
    game: String,
}

/// `GET /mm/v1/capacity?game=` (public; cached 10 s in process and at the edge).
async fn capacity(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    q: Result<Query<CapacityQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(q) = q.map_err(|_| ApiError::invalid("game", "the game, e.g. fh2"))?;
    if !mm.games.contains(&q.game) {
        return Err(ApiError::invalid("game", "a game this deployment serves"));
    }
    let body = match mm.capacity.get(&q.game) {
        Some(b) => b,
        None => {
            let row = db::capacity(&state.pool, &q.game)
                .await?
                .ok_or_else(|| ApiError::invalid("game", "a game this deployment serves"))?;
            let count = |v: i64| u32::try_from(v).unwrap_or(u32::MAX);
            let body = Capacity {
                game: q.game.clone(),
                free: FreeCapacity {
                    online: count(row.online),
                    limit: u32::try_from(row.limit).unwrap_or(0),
                    queued: 0,
                    queue_open: row.queue_open,
                },
                worlds: WorldCounts {
                    free: count(row.free_worlds),
                    supporter: count(row.supporter_worlds),
                },
                generated_at: views::at(row.at),
            };
            mm.capacity.put(q.game.clone(), body.clone());
            body
        }
    };
    Ok(private(
        "public, max-age=10",
        axum::Json(body).into_response(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_port_family_is_not_inferred_from_numeric_port_equality() {
        let place = Place {
            ip: "2001:db8::7".parse().ok(),
            ..Place::default()
        };
        let local: AnnouncePort = serde_json::from_value(serde_json::json!({"port":5101})).unwrap();
        assert!(endpoint(&place, Some(&local)).is_ok());
        for source in ["stun", "upnp"] {
            let mapped: AnnouncePort = serde_json::from_value(serde_json::json!({
                "port":5101,"external":{"port":5101,"source":source}
            }))
            .unwrap();
            assert!(endpoint(&place, Some(&mapped)).is_err());
        }
    }

    /// CR-L53 (§5f): a grant's `sync` is the registry ids of that seat's declared set; a seat
    /// with none keeps today's bytes.
    #[test]
    fn a_grant_signs_the_sync_ids_of_its_seat_s_declared_set() {
        let entry = |id: &str| ModEntry {
            id: id.into(),
            version: "1".into(),
            fingerprint: "a".repeat(64),
        };
        let at = |mods: &[ModEntry]| {
            claims(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
                2_535_100_000_000_002,
                "free",
                RelayGrantHost::Xuid("2535100000000001".into()),
                "joiner",
                10,
                70,
                None,
                None,
                mods,
            )
        };
        assert_eq!(at(&[]).sync, None);
        assert!(
            !serde_json::to_string(&at(&[entry("other")]))
                .unwrap()
                .contains("sync")
        );
        assert_eq!(
            at(&[entry("fh2-anna"), entry("nitro"), entry("underglow")]).sync,
            Some(vec![1, 2])
        );
        assert_eq!(at(&[entry("underglow")]).sync, Some(vec![1]));
    }

    #[test]
    fn host_ip_text_and_relay_times() {
        assert_eq!(
            host_ip("198.51.100.7".parse().ok()),
            "198.51.100.7".parse().ok()
        );
        assert_eq!(
            host_ip("::ffff:203.0.113.5".parse().ok()),
            "203.0.113.5".parse().ok()
        );
        assert_eq!(host_ip(None), None);
        assert!(text_ok("Evening cruise", 48) && !text_ok(" ", 48) && !text_ok("a\nb", 48));
        let now = now_secs();
        let (iat, exp) = fresh(true, UnixMs::from_millis(0), UnixMs::from_millis(0));
        assert!(iat >= now && exp - iat == 60);
        // A seat renewed for its fifth minute: the grant starts 60 s before its expiry.
        let issued = UnixMs::from_millis((now - 300) * 1000);
        let expires = UnixMs::from_millis((now + 50) * 1000);
        let (iat, exp) = fresh(false, issued, expires);
        assert_eq!((exp - iat, exp), (60, now + 50));
        let (iat, exp) = fresh(false, UnixMs::from_millis(now * 1000), expires);
        assert_eq!((iat, exp), (now, now + 50));
    }

    #[test]
    fn lan_addresses_are_private_few_and_canonical() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            lan_addresses(&s(&[
                "192.168.1.40",
                " 10.0.0.2 ",
                "192.168.1.40",
                "fe80::1"
            ]))
            .unwrap(),
            s(&["192.168.1.40", "10.0.0.2", "fe80::1"])
        );
        assert_eq!(
            lan_addresses(&s(&["::ffff:192.168.1.2"])).unwrap(),
            s(&["192.168.1.2"])
        );
        assert!(lan_addresses(&[]).unwrap().is_empty());
        for bad in [
            &["203.0.113.5"][..],
            &["127.0.0.1"],
            &["nope"],
            &["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4", "10.0.0.5"],
        ] {
            let e = lan_addresses(&s(bad)).unwrap_err();
            assert_eq!(e.code, ErrorCode::RequestInvalid, "{bad:?}");
        }
    }

    #[test]
    fn the_limits_and_caps_are_the_contract_s() {
        assert_eq!(db::LIST_CAP, fi_protocol::matchmaking::LIST_CAP);
        assert_eq!(db::LOCKED_CAP, fi_protocol::matchmaking::LOCKED_CAP);
        assert_eq!(db::CODE_DIGITS, fi_protocol::matchmaking::CODE_DIGITS);
        assert_eq!(
            db::OFFER_TTL_SECS,
            fi_protocol::matchmaking::INVITE_TTL_SECS
        );
        assert_eq!(
            db::MAX_PENDING,
            i64::from(fi_protocol::matchmaking::MAX_PENDING)
        );
        assert_eq!(db::GRANT_TTL_SECS, fi_protocol::matchmaking::GRANT_TTL_SECS);
        assert_eq!(db::WORLD_TTL_SECS, fi_protocol::matchmaking::WORLD_TTL_SECS);
        assert_eq!(geo::MAX_RTT_STEPS, fi_protocol::matchmaking::MAX_RTT_STEPS);
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cloud_diagnostics_keep_authority_counts_but_never_credentials_or_addresses() {
        let output = Capture(Arc::new(Mutex::new(Vec::new())));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .fmt_fields(tracing_subscriber::fmt::format::JsonFields::new())
            .event_format(fi_runtime::telemetry::CloudLogging::new("synthetic".into()))
            .with_writer(move || writer.clone())
            .finish();
        let c = Caller {
            game: "synthetic".into(),
            profile_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            session_id: Uuid::from_u128(3),
            xuid: 2_535_100_000_000_001,
            tier: "free".into(),
            mode: "noob".into(),
        };
        let row = PunchRow {
            grant_id: Uuid::from_u128(4),
            world_id: Uuid::from_u128(5),
            profile_id: c.profile_id,
            xuid: c.xuid,
            tier: c.tier.clone(),
            state: "issued".into(),
            issued_at: UnixMs::from_millis(1),
            expires_at: UnixMs::from_millis(60_001),
            udp_proof_key: Some(db::UdpProofKey::from_bytes([69; 32])),
            candidates: Some(db::NetworkHintsRow {
                revision: 7,
                generated_at: UnixMs::from_millis(1),
                expires_at: UnixMs::from_millis(30_001),
                candidates: vec![db::NetworkCandidateRow {
                    kind: "public",
                    ip: "198.51.100.123".into(),
                    port: 62001,
                }],
            }),
            relay_key: None,
            transport: "direct".into(),
            voice_blocked_until: None,
            mods: Vec::new(),
        };
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "request",
                path = "SECRET_PATH",
                method = "SECRET_METHOD",
                request_id = "SECRET_HEADER"
            );
            let _entered = span.enter();
            diagnostic_punches(&c, &[row]);
            diagnostic_error(
                "seat_network",
                &c,
                ApiError::new(ErrorCode::PortalConflict, "SECRET_MESSAGE").with_details(
                    serde_json::json!({"reason":"SECRET_REASON", "key":"SECRET_KEY"}),
                ),
            );
        });
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        for forbidden in [
            "198.51.100.123",
            "62001",
            "2535100000000001",
            "SECRET_MESSAGE",
            "SECRET_REASON",
            "SECRET_KEY",
            "SECRET_PATH",
            "SECRET_METHOD",
            "SECRET_HEADER",
            "RUVFRUVF",
            "UdpProofKey",
            "claims",
            "signature",
        ] {
            assert!(
                !text.contains(forbidden),
                "private field leaked: {forbidden}"
            );
        }
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let punch = lines
            .iter()
            .find(|line| line["event"] == "mm_punch")
            .expect("INFO survives successful-request exclusion");
        assert_eq!(punch["proof_available"], true);
        assert_eq!(punch["public_candidates"], 1);
        assert_eq!(punch["revision"], 7);
        assert_eq!(punch["grant_id"], Uuid::from_u128(4).to_string());
        let refusal = lines
            .iter()
            .find(|line| line["event"] == "mm_decision")
            .unwrap();
        assert_eq!(refusal["reason"], "other");
        assert_eq!(refusal["code"], "PORTAL.CONFLICT");
    }
}
