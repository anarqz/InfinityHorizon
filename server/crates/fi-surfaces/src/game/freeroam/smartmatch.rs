//! momatch: SmartMatch (contract 103) as the game's XSAPI calls it. Every `FreeRoam_qos` ticket
//! is found at once: the caller gets a target session of its own, made from the ticket session's
//! `targetSessionConstants`, reserved in it, its server the caller's world; the ticket session's
//! `servers.matchmaking` says found. One target per player: a shared one made the second console
//! wait for a host and give up after about 14 s; the players meet in the world's relay.
//!
//! ```text
//! POST   /serviceconfigs/{scid}/hoppers/{hopper}              → 200 {ticketId, waitTime}
//! GET    /serviceconfigs/{scid}/hoppers/{hopper}/tickets/{id} → 200 details | 404 {}
//! DELETE /serviceconfigs/{scid}/hoppers/{hopper}/tickets/{id} → 204
//! GET    /serviceconfigs/{scid}/hoppers/{hopper}/stats        → 200 {name, waitTime, population}
//! ```
//!
//! Free roam and the car meet meet in a world. A `CarMeet_qos` ticket that names its car meet
//! (`ticketAttributes.Location`, the player's own request at a meet; provisional until a
//! background ticket is captured) is matched at the POST like a FreeRoam one, in the caller's
//! current world and without an automatic choice or an arrival, but its ticket session stays as
//! the game wrote it (no `servers.matchmaking`): the game learns the match from its GET of the
//! ticket, as other projects used as reference answer it. Its target is, as free roam's, a fresh session of the caller's own
//! (`fh2inf-carmeet_qos-<32 hex>`, the caller reserved and inactive in it, its server the caller's
//! world), in the game's `defaultTemplate`. A found ticket session made the game drop a car meet
//! ticket without ever polling it (hook logs, 2026-10-31); a shared, pre-activated meet session
//! made the host's game skip joining and the joiner's show "no longer exists" (live,
//! 2026-11-02). The world server tells the car meet apart by the lobby type of the game's
//! connection (OD-144). Online Road Trip (`RoadTrip_qos`) is free roam in the caller's world.
//! Other hoppers' tickets are `GAME.NOT_SERVED` (the hook's offline 503, the Beta's answer).

use super::doc::{self, CARMEET_HOPPER, CARMEET_TARGET_TEMPLATE, FREEROAM_HOPPER, ROADTRIP_HOPPER};
use super::{
    MAX_DOC, NO_WORLD_RETRY_SECS, Settings, check_storable, json_answer, me_for, missing,
    no_content, not_served, segments, stored, too_large,
};
use crate::errors::ApiError;
use crate::game::request::GameRequest;
use crate::state::AppState;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use fi_db::freeroam::{self, Caller, NewTicket, Route, SessionKey, Via};
use fi_db::matchmaking::{self, JoinOutcome, MmError};
use fi_protocol::ErrorCode;
use serde_json::{Value, json};
use uuid::Uuid;

/// One momatch request.
pub(super) async fn handle(
    state: &AppState,
    req: &GameRequest<'_>,
    caller: &Caller,
    settings: &Settings,
) -> Result<Response, ApiError> {
    let parts = segments(req.path);
    // serviceconfigs, scid, hoppers, hopper [, tickets [, id] | stats]
    if parts.len() < 4
        || parts.len() > 6
        || !parts[0].eq_ignore_ascii_case("serviceconfigs")
        || !parts[2].eq_ignore_ascii_case("hoppers")
    {
        return Err(not_served(req.host, None));
    }
    let (scid, hopper) = (parts[1], parts[3]);
    let what = parts.get(4).map(|p| p.to_ascii_lowercase());
    match (req.method.clone(), what.as_deref(), parts.get(5).copied()) {
        (Method::POST, None, None) => create(state, req, caller, settings, scid, hopper).await,
        (Method::GET, Some("stats"), None) => {
            let population = match freeroam::key_part(hopper) {
                Some(h) => freeroam::hopper_population(&state.pool, &caller.game, &h).await?,
                None => 0,
            };
            Ok(json_answer(
                StatusCode::OK,
                &json!({ "name": hopper, "waitTime": 0, "population": population }),
                1,
            ))
        }
        (Method::GET, Some("tickets"), Some(id)) => {
            let details = match Uuid::parse_str(id) {
                Ok(id) => {
                    freeroam::ticket_details(&state.pool, &caller.game, id, caller.profile_id)
                        .await?
                }
                Err(_) => None,
            };
            if let Some(suppressed) = crate::diagnostics::poll(crate::diagnostics::Poll::Ticket) {
                crate::network_info!(event = "freeroam_ticket", session_id = %caller.session_id,
                    found = details.is_some(), suppressed, "free-roam ticket read");
            }
            Ok(match details {
                Some(d) => json_answer(StatusCode::OK, &d, 1),
                None => missing(),
            })
        }
        (Method::DELETE, Some("tickets"), id) => {
            if let Some(Ok(id)) = id.map(Uuid::parse_str) {
                freeroam::end_ticket(&state.pool, &caller.game, id, caller.profile_id).await?;
            }
            Ok(no_content())
        }
        _ => Err(not_served(req.host, None)),
    }
}

/// No world for the caller: the Beta's 503 (the game shows its own connection error).
fn no_world() -> ApiError {
    ApiError::new(
        ErrorCode::ServiceUnavailable,
        "no world for this player yet: host one or join one from the launcher",
    )
    .retry_after(NO_WORLD_RETRY_SECS)
}

/// The caller's world for a ticket. An automatic choice takes a grant through matchmaking's join
/// first (the lock, the tier, bans, seats and the free limit hold); without one it is dropped
/// for the fallback, if any.
async fn route_for_ticket(
    state: &AppState,
    caller: &Caller,
    settings: &Settings,
) -> Result<Option<Route>, ApiError> {
    let options = settings.route();
    let route = freeroam::world_for(&state.pool, caller, &options).await?;
    let Some(r) = route.filter(|r| r.via == Via::Auto) else {
        return Ok(route);
    };
    let Some(world) = r.world_id else {
        return Ok(None);
    };
    let mm = caller.mm();
    // The game's ticket carries no mod set: the seat takes the play session's most recent
    // declared one (MATCHMAKING_API.md §4a step 3, OD-248 answer 1, CR-L53; vanilla when none),
    // for display and §5f `sync` only. Mods never decide the world (T-1110-NOMODMATCH, OD-235).
    let mods = match matchmaking::latest_session_mods(&state.pool, &mm).await {
        Ok(mods) => mods,
        Err(MmError::Db(e)) => return Err(e.into()),
        Err(_) => matchmaking::ModSet::vanilla(),
    };
    let input = matchmaking::JoinInput {
        world_id: Some(world),
        code: None,
        ip: caller.ip.map(|ip| ip.to_string()),
        cell: None,
        hold_secs: matchmaking::DEFAULT_SEAT_HOLD_SECS,
        mods,
        // OD-150: the automatic step takes free-roam worlds only.
        activity: Some("freeroam".into()),
    };
    let granted = match matchmaking::join(&state.pool, &mm, &input).await {
        Ok(JoinOutcome::Granted(_)) => true,
        Ok(_) | Err(MmError::WorldNotFound) => false,
        Err(MmError::Db(e)) => return Err(e.into()),
        Err(_) => false,
    };
    if granted {
        // The grant is now the caller's live grant for that world (the next request finds it
        // through the grant step); this ticket keeps how it was chosen.
        return Ok(Some(r));
    }
    // Only an automatic choice reaches this fallback, for any session, modded or not.
    Ok(options.fallback.map(|addr| Route {
        addr,
        world_id: None,
        via: Via::Fallback,
    }))
}

/// `POST …/hoppers/{hopper}`: a ticket, found at once.
async fn create(
    state: &AppState,
    req: &GameRequest<'_>,
    caller: &Caller,
    settings: &Settings,
    scid: &str,
    hopper: &str,
) -> Result<Response, ApiError> {
    let car_meet = hopper.eq_ignore_ascii_case(CARMEET_HOPPER);
    let road_trip = hopper.eq_ignore_ascii_case(ROADTRIP_HOPPER);
    // Car meets and Road Trips meet in the caller's current world (no automatic choice).
    let in_world = car_meet || road_trip;
    if !hopper.eq_ignore_ascii_case(FREEROAM_HOPPER) && !in_world {
        return Err(not_served(req.host, Some(("hopper", hopper))));
    }
    if req.body.len() > MAX_DOC {
        return Err(too_large());
    }
    let request: Value = serde_json::from_slice(req.body).unwrap_or(Value::Null);
    if car_meet && request["ticketAttributes"]["Location"].is_null() {
        return Err(not_served(req.host, Some(("hopper", hopper))));
    }
    let reference = &request["ticketSessionRef"];
    let (Some(template), Some(name)) = (
        reference["templateName"].as_str(),
        reference["name"].as_str(),
    ) else {
        return Err(ApiError::invalid(
            "ticketSessionRef",
            "the ticket session's templateName and name",
        ));
    };
    let ticket_scid = reference["scid"].as_str().unwrap_or(scid);
    let ticket_key = SessionKey::new(&caller.game, ticket_scid, template, name)
        .ok_or_else(|| ApiError::invalid("ticketSessionRef", "not a session of this directory"))?;
    let hopper_key = hopper.to_ascii_lowercase();
    // A car meet's target is in the game's target template; free roam's and the Road Trip's keep
    // the ticket session's.
    let target_template = if car_meet {
        CARMEET_TARGET_TEMPLATE
    } else {
        template
    };
    // One target per player, for every hopper: a shared car meet or Road Trip session was
    // refused by the joining games (SAURON V6 2026-10-30; live 2026-11-02).
    let target_name = format!("fh2inf-{hopper_key}-{}", Uuid::now_v7().simple());
    let target_key = SessionKey::new(&caller.game, ticket_scid, target_template, &target_name)
        .ok_or_else(|| ApiError::invalid("ticketSessionRef", "not a session of this directory"))?;
    let target_ref =
        json!({ "scid": ticket_scid, "templateName": target_template, "name": target_name });
    let details = json!({
        "ticketStatus": "found",
        "waitTime": 0,
        "preserveSession": request["preserveSession"].clone(),
        "ticketSessionRef": reference.clone(),
        "targetSessionRef": target_ref,
        "ticketAttributes": request["ticketAttributes"].clone(),
    });
    check_storable(&details)?;
    // Everything the request can be refused for comes before routing: an automatic choice takes
    // a grant (its own transaction), which a refused request must never hold.
    if !freeroam::is_live_session_of(&state.pool, &ticket_key, caller.profile_id).await? {
        return Ok(missing());
    }
    let route = if in_world {
        freeroam::world_for(&state.pool, caller, &settings.route_without_auto()).await?
    } else {
        route_for_ticket(state, caller, settings).await?
    };
    let Some(route) = route else {
        if let Some(suppressed) = crate::diagnostics::poll(crate::diagnostics::Poll::Refusal) {
            crate::network_warn!(event = "freeroam_route", session_id = %caller.session_id,
                outcome = "no_route", suppressed, "no live hosted world or usable guest seat");
        }
        return Err(no_world());
    };
    let mut tx = state.pool.begin().await?;
    let Some((mut ticket_session, _)) =
        freeroam::lock_live_session_in(&mut tx, &ticket_key, caller.profile_id).await?
    else {
        return Ok(missing());
    };
    let constants =
        ticket_session["properties"]["system"]["matchmaking"]["targetSessionConstants"].clone();
    // FreeRoam and Road Trip: the ticket session says found at once (their searches work so).
    // A car meet's stays as the game wrote it: the game reads the match from its GET of the ticket.
    // A reused ticket session may still carry an earlier FreeRoam ticket's `servers.matchmaking`
    // (a stale "found" at the old target, which made the game drop car meet tickets): removed,
    // and only then is the session written.
    let stale_found = car_meet
        && ticket_session
            .get_mut("servers")
            .and_then(Value::as_object_mut)
            .is_some_and(|servers| servers.remove("matchmaking").is_some());
    if !car_meet || stale_found {
        if !car_meet {
            doc::set_found(&mut ticket_session, &target_ref);
        }
        check_storable(&ticket_session)?;
        freeroam::save_session_in(
            &mut tx,
            &ticket_key,
            caller.profile_id,
            &ticket_session,
            false,
        )
        .await
        .map_err(stored)?;
    }
    // The target session (a fresh name: a session of the caller's own).
    let now = doc::now_iso();
    let profile = doc::SessionProfile::of(&caller.game);
    let mut target = doc::new_target_session(profile, Uuid::now_v7(), &now);
    if let (Value::Object(into), Value::Object(from)) = (&mut target["constants"], constants) {
        doc::merge(into, from);
    }
    let location = doc::location_of(&target, profile);
    doc::set_server(&mut target, doc::server_entry(Some(route.addr), &location));
    let me = me_for(&mut tx, caller).await?;
    doc::reserve(&mut target, &me, &now);
    // Arrival (MATCHMAKING_API.md §4a): the seat that routed this ticket gets 60 s more, once, so
    // the host's next announce finds it even when the launcher's renew loop is late.
    // A car meet ticket comes from a player already in that world: no second arrival.
    if let (false, Via::Grant | Via::Auto, Some(world)) = (in_world, route.via, route.world_id) {
        matchmaking::arrive_in(
            &mut tx,
            &caller.game,
            caller.profile_id,
            world,
            &caller.mode,
            // OD-129: `supporters` worlds are everyone's.
            true,
            caller.session_id,
        )
        .await?;
    }
    check_storable(&target)?;
    let existing = freeroam::lock_session_in(&mut tx, &target_key, caller.profile_id).await?;
    if existing.live {
        // A 128-bit fresh name already alive: refuse rather than overwrite anyone's session.
        return Err(ApiError::unavailable());
    }
    freeroam::save_session_in(&mut tx, &target_key, caller.profile_id, &target, true)
        .await
        .map_err(stored)?;
    let ticket_id = Uuid::now_v7();
    freeroam::insert_ticket_in(
        &mut tx,
        &NewTicket {
            id: ticket_id,
            game: caller.game.clone(),
            owner_profile_id: caller.profile_id,
            hopper: hopper_key,
            details,
            route,
        },
    )
    .await
    .map_err(stored)?;
    tx.commit().await?;
    let route_budget = if route.via == Via::Hosting {
        crate::diagnostics::Poll::RouteHost
    } else {
        crate::diagnostics::Poll::RouteGuest
    };
    if let Some(suppressed) = crate::diagnostics::poll(route_budget) {
        crate::network_info!(
            event = "freeroam_route", suppressed, session_id = %caller.session_id,
            ticket = %ticket_id, outcome = "resolved",
            destination_class = super::route_class(route.addr.ip()),
            via = route.via.as_str(),
            world = ?route.world_id,
            "free-roam ticket found"
        );
    }
    Ok(json_answer(
        StatusCode::OK,
        &json!({ "ticketId": ticket_id.to_string(), "waitTime": 0 }),
        1,
    ))
}
