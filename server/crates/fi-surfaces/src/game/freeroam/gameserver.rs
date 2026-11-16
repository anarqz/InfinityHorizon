//! gameserverds (the game server directory: variants, QoS servers; no state) and gameserverms
//! (`POST …/clusters`: fulfilled at once with the caller's world).

use super::doc::{self, SessionProfile};
use super::{MAX_DOC, Settings, check_storable, json_answer, not_served, stored, too_large};
use crate::errors::ApiError;
use crate::game::request::GameRequest;
use crate::state::AppState;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use fi_db::freeroam::{self, Caller};
use serde_json::Value;

/// `POST /titles/{titleId}/variants` and `GET /qosservers` (any method, as the Beta answered).
pub(super) fn directory(req: &GameRequest<'_>, p: &SessionProfile) -> Result<Response, ApiError> {
    if req.path.ends_with("/variants") {
        return Ok(json_answer(StatusCode::OK, &doc::variants(p), 1));
    }
    if req.path.eq_ignore_ascii_case("/qosservers") {
        return Ok(json_answer(StatusCode::OK, &doc::qos_servers(p), 1));
    }
    Err(not_served(req.host, None))
}

/// `POST …/clusters {sessionId}` (any path ending so, as the Beta): the caller's own session
/// named by the last segment of `sessionId` gets its server entry; fulfilled at once.
pub(super) async fn clusters(
    state: &AppState,
    req: &GameRequest<'_>,
    caller: &Caller,
    settings: &Settings,
    p: &SessionProfile,
) -> Result<Response, ApiError> {
    if req.method != Method::POST || !req.path.ends_with("/clusters") {
        return Err(not_served(req.host, None));
    }
    if req.body.len() > MAX_DOC {
        return Err(too_large());
    }
    let body: Value = serde_json::from_slice(req.body).unwrap_or(Value::Null);
    let session = body["sessionId"].as_str().unwrap_or("");
    let last = session.rsplit('/').next().unwrap_or("");
    let mut tx = state.pool.begin().await?;
    // Never an automatic choice here: only a FreeRoam ticket takes one, with its grant.
    let route = freeroam::world_for_in(&mut tx, caller, &settings.route_without_auto()).await?;
    let world = route.map(|r| r.addr);
    if let Some(name) = freeroam::key_part(last)
        && let Some((key, mut session, _)) =
            freeroam::lock_own_session_named_in(&mut tx, &caller.game, caller.profile_id, &name)
                .await?
    {
        let location = doc::location_of(&session, p);
        doc::set_server(&mut session, doc::server_entry(world, &location));
        check_storable(&session)?;
        freeroam::save_session_in(&mut tx, &key, caller.profile_id, &session, false)
            .await
            .map_err(stored)?;
    }
    tx.commit().await?;
    Ok(json_answer(StatusCode::OK, &doc::cluster(world, p), 1))
}
