//! The caller's own views (CORE_API.md §2.3, §3): notices, strikes, support. Session only, never
//! cached (`Cache-Control: private, no-store`).

use crate::Core;
use crate::session::CoreSession;
use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::header::CACHE_CONTROL;
use fi_db::moderation::StrikeRow;
use fi_db::{accounts, moderation, notices};
use fi_domain::UnixMs;
use fi_protocol::core::{
    AccountNotice, AccountNoticeList, Strike, StrikeList, StrikeReview, SupportInfo,
};
use fi_surfaces::errors::ApiError;
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::Arc;

/// `Cache-Control` of every personal answer.
pub const PRIVATE: &str = "private, no-store";
/// Strikes younger than this make a `strike` notice (unless overturned).
const STRIKE_NOTICE_DAYS: i64 = 30;
/// A kick makes a `kicked` notice for this many hours (or while its cooldown lasts).
const KICK_NOTICE_HOURS: i64 = 24;
/// A Supporter's access ending within this many days without renewal makes a `vip_ending` notice.
const VIP_ENDING_DAYS: i64 = 7;

/// A strike on the wire.
pub fn strike(s: &StrikeRow) -> Strike {
    Strike {
        id: s.id.to_string(),
        game: s.game.clone(),
        xuid: s.xuid.to_string(),
        source: s.source.clone(),
        reason: s.reason.clone(),
        created_at: views::at(s.created_at),
        reviewed: s.reviewed.as_ref().map(|(at, outcome)| StrikeReview {
            at: views::at(*at),
            outcome: outcome.clone(),
        }),
    }
}

/// `GET /core/v1/me/notices` → 200 [`AccountNoticeList`]: deletion, strikes of the last 30 days
/// not overturned, bundles set aside in the last 7 days, the Supporter access ending soon, queued
/// sessions with their place in line, maintenance of the games the account plays.
pub async fn notices(
    State(state): State<AppState>,
    session: CoreSession,
) -> Result<
    (
        [(axum::http::HeaderName, &'static str); 1],
        Json<AccountNoticeList>,
    ),
    ApiError,
> {
    let now = UnixMs::now_system();
    let mut conn = state.pool.acquire().await?;
    let account = accounts::account_state_in(&mut conn, session.account_id)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    let mut items: Vec<AccountNotice> = views::notices(&account)
        .into_iter()
        .map(|n| AccountNotice {
            kind: n.kind,
            level: n.level,
            text: n.text,
            since: n.since,
            game: None,
            position: None,
            until: None,
        })
        .collect();
    for s in moderation::strikes_of_account_in(&mut conn, session.account_id, None).await? {
        let overturned = s.reviewed.as_ref().is_some_and(|(_, o)| o == "overturned");
        if overturned || now.since(s.created_at) > STRIKE_NOTICE_DAYS * 86_400_000 {
            continue;
        }
        items.push(AccountNotice {
            kind: "strike".into(),
            level: "warning".into(),
            text: format!("A strike was recorded on profile {}: {}", s.xuid, s.reason),
            since: views::at(s.created_at),
            game: Some(s.game.clone()),
            position: None,
            until: None,
        });
    }
    // CR-S62: a moderator's kick, for a day or while its cooldown lasts (never the reason).
    if let Some((at, until)) = moderation::last_kick_in(&mut conn, session.account_id).await?
        && (until.is_some() || now.since(at) <= KICK_NOTICE_HOURS * 3_600_000)
    {
        items.push(AccountNotice {
            kind: "kicked".into(),
            level: "warning".into(),
            text: match until {
                Some(u) => format!(
                    "A moderator removed you from online play. You can play online again after {}.",
                    views::at(u)
                ),
                None => "A moderator removed you from online play.".into(),
            },
            since: views::at(at),
            game: None,
            position: None,
            until: None,
        });
    }
    // CR-S68: a voice block in force (a moderator's voice ban or a community mute), its end;
    // never who muted or why.
    if let Some(until) = fi_db::voice::blocked_until_in(&mut conn, session.account_id)
        .await
        .map_err(fi_db::identity::IdentityError::from)?
    {
        items.push(AccountNotice {
            kind: "voice_blocked".into(),
            level: "warning".into(),
            text: format!("Your voice chat is blocked until {}.", views::at(until)),
            since: views::at(now),
            game: None,
            position: None,
            until: Some(views::at(until)),
        });
    }
    for p in notices::parked_recent_in(&mut conn, session.account_id).await? {
        items.push(AccountNotice {
            kind: "parked_save".into(),
            level: "info".into(),
            text: format!(
                "{} save upload(s) were set aside; nothing was lost. Open your saves to review them.",
                p.count
            ),
            since: views::at(p.latest),
            game: Some(p.game),
            position: None,
            until: None,
        });
    }
    let access = accounts::access_in(&mut conn, session.account_id).await?;
    if let (true, false, Some(until)) = (access.vip, access.auto_renew, access.access_until)
        && until.since(now) < VIP_ENDING_DAYS * 86_400_000
    {
        items.push(AccountNotice {
            kind: "vip_ending".into(),
            level: "info".into(),
            text: format!(
                "Your Infinity Supporter access ends on {}.",
                views::at(until)
            ),
            since: views::at(now),
            game: None,
            position: None,
            until: None,
        });
    }
    for q in notices::queue_positions_in(&mut conn, session.account_id).await? {
        items.push(AccountNotice {
            kind: "queue".into(),
            level: "info".into(),
            text: format!("You are number {} in the queue.", q.position),
            since: views::at(q.queued_at),
            game: Some(q.game),
            position: Some(q.position.max(1) as u64),
            until: None,
        });
    }
    let played: BTreeSet<String> = accounts::profiles_in(&mut conn, session.account_id)
        .await?
        .into_iter()
        .map(|p| p.game)
        .collect();
    for g in notices::games_in(&mut conn).await? {
        if let (true, Some(until)) = (played.contains(&g.code), g.maintenance_until) {
            items.push(AccountNotice {
                kind: "maintenance".into(),
                level: "warning".into(),
                text: g.maintenance_message.clone().unwrap_or_else(|| {
                    format!("Servers are under maintenance until {}.", views::at(until))
                }),
                since: views::at(now),
                game: Some(g.code),
                position: None,
                until: None,
            });
        }
    }
    Ok((
        [(CACHE_CONTROL, PRIVATE)],
        Json(AccountNoticeList { items }),
    ))
}

/// `?game=` of the strikes list.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrikesQuery {
    /// One game (default every game).
    pub game: Option<String>,
}

/// `GET /core/v1/me/strikes?game=fh2` → 200 [`StrikeList`].
pub async fn strikes(
    State(state): State<AppState>,
    session: CoreSession,
    query: Result<Query<StrikesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<
    (
        [(axum::http::HeaderName, &'static str); 1],
        Json<StrikeList>,
    ),
    ApiError,
> {
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "game only"))?;
    if q.game.as_deref().is_some_and(|g| !crate::valid_game(g)) {
        return Err(ApiError::invalid("game", "a game id such as fh2"));
    }
    let mut conn = state.pool.acquire().await?;
    let rows =
        moderation::strikes_of_account_in(&mut conn, session.account_id, q.game.as_deref()).await?;
    Ok((
        [(CACHE_CONTROL, PRIVATE)],
        Json(StrikeList {
            items: rows.iter().map(strike).collect(),
        }),
    ))
}

/// `GET /core/v1/me/support` → 200 [`SupportInfo`] (`reportToken` `null` until the report bundle
/// upload exists).
pub async fn support(
    Extension(core): Extension<Arc<Core>>,
    _session: CoreSession,
) -> (
    [(axum::http::HeaderName, &'static str); 1],
    Json<SupportInfo>,
) {
    (
        [(CACHE_CONTROL, PRIVATE)],
        Json(SupportInfo {
            links: core.config.support.clone(),
            report_token: None,
        }),
    )
}
