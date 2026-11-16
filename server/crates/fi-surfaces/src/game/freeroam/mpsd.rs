//! sessiondirectory: the multiplayer session directory (MPSD, contract 105) over
//! `freeroam_session`. Owner-only: another player's live session does not exist for the caller
//! (the Beta's 2026-10-15 review C1: a player could rewrite another's world address), except a car
//! meet invite session (MATCHMAKING_API.md §3.8): its members read it, with the server entry of
//! their own route, and write their own member entry only.
//!
//! ```text
//! GET    /serviceconfigs/{scid}/sessiontemplates/{t}/sessions[?…]  → 200 {"results": []}
//! PUT    /serviceconfigs/{scid}/sessiontemplates/{t}/sessions/{n}  → 201 created | 200 merged
//! POST   (same)                                                    → as PUT
//! GET    (same)                                                    → 200 | 404 {}
//! DELETE (same)                                                    → 204 | 404 {} (another's)
//! ```

use super::doc::{self, Me, SessionProfile};
use super::{
    MAX_DOC, Settings, check_storable, json_answer, me_for, missing, no_content, not_served,
    segments, stored, too_large,
};
use crate::errors::ApiError;
use crate::game::request::GameRequest;
use crate::state::AppState;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use fi_db::freeroam::{self, Caller, Ended, SessionKey};
use serde_json::{Value, json};
use uuid::Uuid;

/// One sessiondirectory request.
pub(super) async fn handle(
    state: &AppState,
    req: &GameRequest<'_>,
    caller: &Caller,
    settings: &Settings,
    p: &SessionProfile,
) -> Result<Response, ApiError> {
    let parts = segments(req.path);
    if !parts
        .first()
        .is_some_and(|p| p.eq_ignore_ascii_case("serviceconfigs"))
    {
        return Err(not_served(req.host, None));
    }
    // FM5 names its own service config only (FM5_LOBBIES.md §3); FH2 any, as the Beta.
    if let Some(scid) = parts.get(1)
        && !p.accepts_scid(scid)
    {
        return Err(not_served(req.host, None));
    }
    // FM5's leave (`DELETE …/sessions/{name}/members/me`): the caller's `members.me` null.
    if p.member_leave_path
        && *req.method == Method::DELETE
        && parts.len() == 8
        && parts[2].eq_ignore_ascii_case("sessiontemplates")
        && parts[4].eq_ignore_ascii_case("sessions")
        && parts[6].eq_ignore_ascii_case("members")
        && parts[7].eq_ignore_ascii_case("me")
    {
        let key = SessionKey::new(&caller.game, parts[1], parts[3], parts[5])
            .ok_or_else(|| ApiError::invalid("path", "not a session of this directory"))?;
        let leave = br#"{"members":{"me":null}}"#;
        return write(state, caller, settings, p, &key, leave, true)
            .await
            .map(|_| no_content());
    }
    // The sessions a user is in: always none (an earlier run's sessions made the game try to
    // rejoin them at start).
    if *req.method == Method::GET
        && parts
            .last()
            .is_some_and(|p| p.eq_ignore_ascii_case("sessions"))
    {
        return Ok(json_answer(StatusCode::OK, &json!({ "results": [] }), 1));
    }
    if parts.len() != 6
        || !parts[2].eq_ignore_ascii_case("sessiontemplates")
        || !parts[4].eq_ignore_ascii_case("sessions")
    {
        return Err(not_served(req.host, None));
    }
    let key = SessionKey::new(&caller.game, parts[1], parts[3], parts[5])
        .ok_or_else(|| ApiError::invalid("path", "not a session of this directory"))?;
    match *req.method {
        Method::GET => {
            if let Some((doc, revision)) =
                freeroam::read_session(&state.pool, &key, caller.profile_id).await?
            {
                return Ok(json_answer(StatusCode::OK, &doc, revision));
            }
            Ok(
                match freeroam::read_invite_session(&state.pool, &key, caller.xuid).await? {
                    Some((mut doc, revision)) => {
                        as_seen_by(state, caller, settings, p, &mut doc).await?;
                        json_answer(StatusCode::OK, &doc, revision)
                    }
                    None => private_read(state, caller, settings, p, &key).await?,
                },
            )
        }
        Method::DELETE => Ok(
            match freeroam::end_session(&state.pool, &key, caller.profile_id).await? {
                Ended::Foreign => missing(),
                Ended::Ended | Ended::Missing => no_content(),
            },
        ),
        Method::PUT | Method::POST => {
            if req.body.len() > MAX_DOC {
                return Err(too_large());
            }
            write(state, caller, settings, p, &key, req.body, false).await
        }
        _ => Err(not_served(req.host, None)),
    }
}

/// `PUT`/`POST …/sessions/{name}`: merge into the caller's session (created when it does not
/// exist), fill a game session's server, count the members; one transaction on one connection.
/// `leave_only`: FM5's member leave, which never creates a session.
async fn write(
    state: &AppState,
    caller: &Caller,
    settings: &Settings,
    p: &SessionProfile,
    key: &SessionKey,
    body: &[u8],
    leave_only: bool,
) -> Result<Response, ApiError> {
    let raw: &[u8] = if body.is_empty() { b"{}" } else { body };
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(raw) else {
        return Err(ApiError::invalid("body", "a JSON object"));
    };
    let wants_me = body
        .get("members")
        .and_then(|m| m.get("me"))
        .is_some_and(Value::is_object);
    let mut tx = state.pool.begin().await?;
    let mut existing = freeroam::lock_session_in(&mut tx, key, caller.profile_id).await?;
    if leave_only && !existing.live {
        // Rolled back on drop: a leave never makes a session.
        return Ok(no_content());
    }
    if existing.live && existing.owner_profile_id != caller.profile_id {
        if freeroam::is_invite_name(&key.name)
            && doc::member_of(&existing.doc, caller.xuid).is_some()
        {
            return member_write(
                state, caller, settings, p, key, tx, existing, body, wants_me,
            )
            .await;
        }
        // CR-S96 (WORLDS_OFFICIAL.md §12.4): an account with access to the linked private world
        // writes as a member; an invitee who has not accepted, or anyone else, sees no session.
        if existing.private_world_id.is_some()
            && freeroam::private_reader_in(&mut tx, key, caller.account_id, caller.profile_id)
                .await?
                == Some(freeroam::PrivateReader::Access)
        {
            if wants_me {
                let me = me_for(&mut tx, caller).await?;
                doc::invite(&mut existing.doc, &me, &doc::now_iso());
            }
            return member_write(
                state, caller, settings, p, key, tx, existing, body, wants_me,
            )
            .await;
        }
        // Rolled back on drop: nothing of the caller's is written.
        return Ok(missing());
    }
    let created = !existing.live;
    // OD-213: the owner's game leaving its own session (`members.me: null`)
    let leaving = body
        .get("members")
        .and_then(|m| m.get("me"))
        .is_some_and(Value::is_null);
    let now = doc::now_iso();
    let mut session = if created {
        doc::new_session(p, Uuid::now_v7(), &now)
    } else {
        existing.doc
    };
    let me: Me = if wants_me {
        me_for(&mut tx, caller).await?
    } else {
        Me {
            xuid: caller.xuid,
            gamertag: String::new(),
            device_token: String::new(),
            title_id: p.title_id,
        }
    };
    if freeroam::is_invite_name(&key.name) {
        let sent = body.get("members").and_then(|m| m.get("me")).cloned();
        let shape = match &sent {
            Some(Value::Null) => "null",
            Some(Value::Object(_)) => "entry",
            _ => "absent",
        };
        let active = sent
            .as_ref()
            .and_then(|s| s["properties"]["system"]["active"].as_bool());
        let body_keys: Vec<String> = body.keys().cloned().collect();
        crate::network_info!(event = "invite_owner_write", session_id = %caller.session_id,
            created, me = shape, active = ?active, body_keys = ?body_keys,
            "invite session owner write");
    }
    let private_create = created && doc::is_private_create(p, &key.template, &key.name, &body);
    doc::apply_write(&mut session, body, &me, &now);
    let mut resolved = None;
    let mut filled_server = false;
    if p.fh2_lobbies && doc::is_car_meet_ticket_session(&key.template, &session) {
        doc::clear_server(&mut session);
    } else if let Some(location) = doc::needs_server(&session, p) {
        filled_server = true;
        let route = freeroam::world_for_in(&mut tx, caller, &settings.route_without_auto()).await?;
        resolved = route;
        doc::set_server(
            &mut session,
            doc::server_entry(route.map(|r| r.addr), &location),
        );
    }
    doc::count_members(&mut session);
    check_storable(&session)?;
    let revision = freeroam::save_session_in(&mut tx, key, caller.profile_id, &session, created)
        .await
        .map_err(stored)?;
    // CR-S96 (§12.2 item 4): never allocates. Only the launcher's create route seats an owner; a
    // write without such a seat stays a plain session, as for an old client.
    if created {
        let world = if private_create {
            freeroam::owned_private_seat_in(&mut tx, caller).await?
        } else {
            None
        };
        freeroam::set_private_world_in(&mut tx, key, world).await?;
    }
    tx.commit().await?;
    if leaving {
        left_group(state, caller, key).await;
    }
    if let Some(suppressed) = crate::diagnostics::poll(crate::diagnostics::Poll::Session) {
        crate::network_info!(event = "freeroam_session", suppressed, session_id = %caller.session_id,
        created, filled_server, revision,
        route = resolved.map(|v| v.via.as_str()), world_id = ?resolved.and_then(|v| v.world_id),
        destination_class = resolved.map(|v| super::route_class(v.addr.ip())),
        placeholder = filled_server && resolved.is_none(), "game session written");
    }
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(json_answer(status, &session, revision))
}

/// `GET` of a private session by someone else than its owner (WORLDS_OFFICIAL.md §12.4): 200 for an
/// account with access, added as an inactive member, and for a pending invitee; 404 `{}` otherwise.
/// The reader's own route is its server entry.
async fn private_read(
    state: &AppState,
    caller: &Caller,
    settings: &Settings,
    p: &SessionProfile,
    key: &SessionKey,
) -> Result<Response, ApiError> {
    let mut tx = state.pool.begin().await?;
    let Some(reader) =
        freeroam::private_reader_in(&mut tx, key, caller.account_id, caller.profile_id).await?
    else {
        return Ok(missing());
    };
    let mut existing = freeroam::lock_session_in(&mut tx, key, caller.profile_id).await?;
    let mut revision = existing.revision;
    if reader == freeroam::PrivateReader::Access
        && doc::member_of(&existing.doc, caller.xuid).is_none()
    {
        let me = me_for(&mut tx, caller).await?;
        doc::invite(&mut existing.doc, &me, &doc::now_iso());
        check_storable(&existing.doc)?;
        revision = freeroam::save_session_in(
            &mut tx,
            key,
            existing.owner_profile_id,
            &existing.doc,
            false,
        )
        .await
        .map_err(stored)?;
    }
    tx.commit().await?;
    let mut session = existing.doc;
    as_seen_by(state, caller, settings, p, &mut session).await?;
    Ok(json_answer(StatusCode::OK, &session, revision))
}

/// Most bytes of a member's own `properties.custom` in an invite session.
const MEMBER_CUSTOM_MAX: usize = 4096;

/// A member's write to a car meet invite session it does not own (§3.8): from its own
/// `members.me`, only `properties.system.active` and `ready` and a small `properties.custom`; its
/// gamertag and device token are the server's. The owner keeps the session.
#[allow(clippy::too_many_arguments)]
async fn member_write(
    state: &AppState,
    caller: &Caller,
    settings: &Settings,
    p: &SessionProfile,
    key: &SessionKey,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    existing: freeroam::StoredSession,
    body: serde_json::Map<String, Value>,
    _wants_me: bool,
) -> Result<Response, ApiError> {
    let sent = body
        .get("members")
        .and_then(|m| m.get("me"))
        .cloned()
        .unwrap_or(Value::Null);
    let me = me_for(&mut tx, caller).await?;
    let mut session = existing.doc;
    let now = doc::now_iso();
    let leaving = body
        .get("members")
        .and_then(|m| m.get("me"))
        .is_some_and(Value::is_null);
    // Which way a car meet invitee went (local test 2026-11-03: joiners leave): the shape of its
    // own entry and the body's other parts, never an identity or a value.
    let members_before = session["members"].as_object().map_or(0, |m| m.len());
    crate::network_info!(event = "invite_member_write", session_id = %caller.session_id,
        me = if leaving { "null" } else if sent.is_object() { "entry" } else { "absent" },
        active = ?sent["properties"]["system"]["active"].as_bool(),
        me_keys = ?sent.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
        body_keys = ?body.keys().cloned().collect::<Vec<_>>(),
        members_before, "invite session member write");
    if leaving {
        // The member leaves: its own entry goes.
        let mut only_me = serde_json::Map::new();
        only_me.insert("members".into(), json!({ "me": null }));
        doc::apply_write(&mut session, only_me, &me, &now);
    } else if sent.is_object() {
        // The member's own entry, as the game writes it (other projects used as reference merge a member's whole entry): its
        // `properties.system` (its secureDeviceAddress among them; without it a joiner reported
        // "the session no longer exists", live 2026-11-02) and `custom`, and its
        // `constants.system` except the identity the server owns. Each part is bounded.
        let mut system = serde_json::Map::new();
        if let Some(sent_system) = sent["properties"]["system"].as_object()
            && Value::Object(sent_system.clone()).to_string().len() <= MEMBER_CUSTOM_MAX
        {
            system = sent_system.clone();
        }
        system.retain(|field, v| match field.as_str() {
            "active" | "ready" => v.is_boolean(),
            _ => true,
        });
        let mut properties = json!({ "system": system });
        let custom = &sent["properties"]["custom"];
        if custom.is_object() && custom.to_string().len() <= MEMBER_CUSTOM_MAX {
            properties["custom"] = custom.clone();
        }
        let mut entry = json!({ "properties": properties });
        if let Some(constants) = sent["constants"]["system"].as_object()
            && Value::Object(constants.clone()).to_string().len() <= MEMBER_CUSTOM_MAX
        {
            let mut constants = constants.clone();
            constants.remove("xuid");
            constants.remove("index");
            entry["constants"] = json!({ "system": constants });
        }
        let mut only_me = serde_json::Map::new();
        only_me.insert("members".into(), json!({ "me": entry }));
        doc::apply_write(&mut session, only_me, &me, &now);
        // The member is shown as the server knows it, whatever was stored before.
        if let Some(index) = doc::member_of(&session, caller.xuid)
            && let Some(slot) = session["members"].get_mut(&index)
        {
            slot["gamertag"] = json!(me.gamertag);
            slot["deviceToken"] = json!(me.device_token);
        }
    }
    // A member claiming the host role: kept as that member's own claim, shown back only in its own
    // view (the game retried an unseen claim ~9/s, local test 2026-11-03; a private car meet's
    // joiner ~20/s, CR-S93, 2026-11-04); the world server hosts the session.
    if let Some(claim) = body
        .get("properties")
        .and_then(|p| p.get("system"))
        .and_then(|s| s.get("host"))
        .and_then(Value::as_str)
        .filter(|h| h.len() <= 64)
    {
        session[doc::FI_HOST_CLAIMS][caller.xuid.to_string()] = json!(claim);
    }
    doc::count_members(&mut session);
    check_storable(&session)?;
    let revision =
        freeroam::save_session_in(&mut tx, key, existing.owner_profile_id, &session, false)
            .await
            .map_err(stored)?;
    let route = freeroam::world_for_in(&mut tx, caller, &settings.route_without_auto()).await?;
    tx.commit().await?;
    if leaving {
        left_group(state, caller, key).await;
    }
    let location = doc::location_of(&session, p);
    doc::set_server(
        &mut session,
        doc::server_entry(route.map(|r| r.addr), &location),
    );
    doc::reader_first(&mut session, caller.xuid);
    doc::fill_member_addresses(&mut session, Some(caller.xuid));
    Ok(json_answer(StatusCode::OK, &session, revision))
}

/// OD-213 (WORLDS_OFFICIAL.md §11.4 "Life"): a game that left the session of an official group
/// world (Horizon Solo) gives up its seat there, in its own transaction after the session's (never
/// the session row's lock while a grant row is taken: the announce takes them the other way). The
/// group world closes on its next announce once nobody is left in it. Best effort: the session
/// write stands; a failure leaves the seat to lapse on its own. `true`: a seat ended.
async fn left_group(state: &AppState, caller: &Caller, key: &SessionKey) -> bool {
    let Some(world) = freeroam::invite_world(&key.name) else {
        return false;
    };
    match fi_db::matchmaking::official_group_left(
        &state.pool,
        &caller.game,
        caller.profile_id,
        caller.session_id,
        world,
    )
    .await
    {
        Ok(true) => {
            crate::network_info!(event = "official_group_left", session_id = %caller.session_id,
                world_id = %world, "group seat released: the game left the group session");
            true
        }
        Ok(false) => false,
        Err(_) => {
            crate::network_warn!(event = "official_group_left", session_id = %caller.session_id,
                world_id = %world, "group seat release failed");
            false
        }
    }
}

/// An invite session as a member reads it: the server entry names the member's own route to the
/// host's world (its seat), as after a match.
async fn as_seen_by(
    state: &AppState,
    caller: &Caller,
    settings: &Settings,
    p: &SessionProfile,
    session: &mut Value,
) -> Result<(), ApiError> {
    let route = freeroam::world_for(&state.pool, caller, &settings.route_without_auto()).await?;
    let location = doc::location_of(session, p);

    doc::set_server(session, doc::server_entry(route.map(|r| r.addr), &location));
    doc::reader_first(session, caller.xuid);
    doc::fill_member_addresses(session, Some(caller.xuid));
    Ok(())
}
