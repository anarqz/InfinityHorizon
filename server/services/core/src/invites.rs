//! Invites to play and requests to join, the answering side (CORE_API.md §4.2; OD-56, OD-59;
//! MATCHMAKING_API.md §3.4): one inbox per account of the pending invites to it and the pending
//! requests to join the worlds it hosts, which the launcher polls about every 10 s with its access
//! token, and their decline. Matchmaking makes them (with the game ticket); accepting an invite is
//! matchmaking's join, accepting a request matchmaking's `…/requests/{id}/accept`.
//!
//! Same caller rules as the friend routes ([`crate::friends`]): every account (OD-129, OD-141: no
//! Supporter gate, `PORTAL.VIP_REQUIRED` is never answered), not pending deletion. The worlds are shown as the caller's current profile would see them: `?asXuid=`
//! (validated as a live profile of the caller's account), else the profile of the account's active
//! play session, else none (`sameMode` absent, no mode check). Core has no location: `pingClass`
//! is `unknown` (or `local` for one's own world). A request holds one pooled connection at a time.
//!
//! The inbox also names the friends who came online since this device's previous poll (CR-S102,
//! CORE_API.md §4.2a; [`fi_db::friends::came_online_in`]), on the same connection and bucket, and
//! the caller's own invites and requests with their state (CR-S106, [`fi_db::matchmaking::sent_in`]).

use crate::Core;
use crate::friends::{AsQuery, as_profile, as_query, open};
use crate::me::PRIVATE;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fi_db::friends;
use fi_db::matchmaking::{self, Rules, Viewer};
use fi_domain::UnixMs;
use fi_protocol::core::{
    FriendOnline, InviteFrom, InviteList, InviteReceived, InviteSent, InviteSession, InviteTo,
};
use fi_protocol::routes::launcher as lr;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::LauncherAuth;
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use serde::Deserialize;
use sqlx::Connection;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// The inbox routes (the launcher's alias only).
pub fn router() -> Router<AppState> {
    Router::new()
        .route(lr::INVITES, get(inbox))
        .route(lr::INVITE_DECLINE, post(decline))
}

/// The inbox's query: `asXuid` as the presence reads, and `first=true` on the launcher's first poll
/// after it starts (CR-S102 review #1: that poll is recorded and names nobody online).
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InboxQuery {
    /// A live profile of the caller's account (decimal XUID).
    pub as_xuid: Option<String>,
    /// The launcher's first poll since it started.
    pub first: Option<bool>,
}

/// `GET /launcher/v1/invites[?asXuid=][&first=true]` → 200 [`InviteList`].
pub async fn inbox(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    q: Result<Query<InboxQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(q) = q.map_err(|_| ApiError::invalid("query", "asXuid and first only"))?;
    let first = q.first.unwrap_or(false);
    let as_xuid = as_query(Ok(Query(AsQuery { as_xuid: q.as_xuid })))?;
    core.account_read("invites", auth.account_id)?;
    let mut conn = state.pool.acquire().await?;
    let caller = open(&mut conn, auth.account_id).await?;
    let me = match as_xuid {
        Some(x) => Some(as_profile(&mut conn, caller.account_id, x).await?),
        None => friends::playing_profile_in(&mut conn, caller.account_id).await?,
    };
    let rows = matchmaking::inbox_in(&mut conn, caller.account_id).await?;
    let viewer = Viewer {
        account_id: caller.account_id,
        profile_id: me.as_ref().map(|m| m.profile_id).unwrap_or_else(Uuid::nil),
        xuid: me.as_ref().map(|m| m.xuid).unwrap_or(0),
        ip: None,
    };
    let ids: Vec<Uuid> = rows.iter().map(|r| r.offer.world_id).collect();
    let worlds: HashMap<Uuid, matchmaking::WorldRow> =
        matchmaking::worlds_by_ids_in(&mut conn, &viewer, &ids)
            .await?
            .into_iter()
            .map(|w| (w.id, w))
            .collect();
    let rules = Rules {
        profile_id: viewer.profile_id,
        vip: true,
        mode: me.as_ref().map(|m| m.mode.as_str()),
        cell: None,
    };
    let items = rows
        .iter()
        .filter_map(|r| {
            let w = worlds.get(&r.offer.world_id)?;
            Some(InviteReceived {
                id: r.offer.id.to_string(),
                kind: r.offer.kind.clone(),
                world: fi_surfaces::worlds::view(&matchmaking::assess(w, &rules, false)),
                from: InviteFrom {
                    xuid: r.from_xuid.to_string(),
                    gamertag: r.from_gamertag.clone(),
                    mode: r.from_mode.clone(),
                    tier: if r.from_vip { "vip" } else { "free" }.to_owned(),
                },
                // OD-141: `sameMode` is "joinable with the current profile" (its mode, or `any`).
                same_mode: me.as_ref().map(|m| w.admits(&m.mode)),
                private: r.private,
                session: r
                    .session
                    .as_ref()
                    .map(|(scid, template, name)| InviteSession {
                        scid: scid.clone(),
                        template_name: template.clone(),
                        name: name.clone(),
                    }),
                created_at: views::at(r.offer.created_at),
                expires_at: views::at(r.offer.expires_at),
            })
        })
        .collect();
    // CR-S106: what the caller sent, with its state (the overlay drops its "Invited" mark on any
    // non-pending one).
    let sent = matchmaking::sent_in(&mut conn, caller.account_id)
        .await?
        .into_iter()
        .map(|r| InviteSent {
            id: r.offer.id.to_string(),
            kind: r.offer.kind,
            world_id: r.offer.world_id.to_string(),
            to: InviteTo {
                friend_id: r.to_handle.to_string(),
                gamertag: r.to_gamertag,
            },
            state: r.offer.state,
            expires_at: views::at(r.offer.expires_at),
        })
        .collect();
    // CR-S102: the friends who came online since this device's previous poll (this poll is
    // recorded; the first one after a launcher start answers none).
    let mut came = friends::came_online_in(&mut conn, caller.account_id, auth.family_id).await?;
    if first {
        came.clear();
    }
    let online = came
        .into_iter()
        .map(|f| FriendOnline {
            friend_id: f.handle.to_string(),
            gamertag: f.gamertag,
            game: f.game,
            mode: f.mode,
            since: views::at(f.since),
        })
        .collect();
    Ok((
        [(CACHE_CONTROL, PRIVATE)],
        Json(InviteList {
            items,
            generated_at: views::at(UnixMs::now_system()),
            online: Some(online),
            sent: Some(sent),
        }),
    )
        .into_response())
}

/// `POST /launcher/v1/invites/{inviteId}/decline` → 204: the caller declines a pending invite to
/// it or a request to join its world; its held seat is free at once (`invite.ended`). Idempotent
/// by state (an unknown, ended or another account's id is 204 too), so no `Idempotency-Key`.
pub async fn decline(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::invalid("inviteId", "an invite id"))?;
    core.account_write(auth.account_id)?;
    let mut conn = state.pool.acquire().await?;
    let caller = open(&mut conn, auth.account_id).await?;
    let mut tx = conn.begin().await?;
    matchmaking::decline_in(&mut tx, caller.account_id, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
