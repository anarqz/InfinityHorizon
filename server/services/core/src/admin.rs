//! Admin and moderation (CORE_API.md §5): roles MOD (`moderator`) and ADM (`admin`),
//! `AUTH.ROLE_REQUIRED {role}` for anybody below. Every write takes an `Idempotency-Key` (a replay
//! answers the recorded response and writes nothing) and appends exactly one `admin_action` row in
//! its transaction. Nobody moderates their own account (`AUTH.FORBIDDEN`): no self-strike, review,
//! ban, role change or erasure.

use crate::me::{PRIVATE, strike};
use crate::session::CoreSession;
use crate::status::notice;
use crate::{cursor_id, next_id};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use fi_db::integrity::{self, AssessedBy, AssessmentRow};
use fi_db::limits::{self, Idempotency, Recorded};
use fi_db::moderation::{self, AuditEntry, AuditFilter, AuditRow, Lookup, ReportRow, Role};
use fi_db::{accounts, liveries, notices};
use fi_domain::UnixMs;
use fi_protocol::ErrorCode;
use fi_protocol::core::{
    AccountLookup, AccountLookupList, AdminAccount, AdminAccountView, AdminAction, AdminCheatFlag,
    AdminLivery, AdminLiveryList, AdminReport, AdminReportList, AdminSessions, AdminVoice,
    AuditActor, AuditList, Ban, BanApplied, CheatUnflag, ErasureScheduled, GameCapacity,
    GameCapacityUpdate, GuardAssessment, GuardAssessmentList, KickApplied, LiveSession, LiveryBulk,
    LiveryBulkApplied, LiveryDelete, LiveryUploads, LiveryUploadsUpdate, LiveryVisibility,
    Maintenance, MaintenanceUpdate, NewBan, NewKick, NewNotice, NewStrike, NewVoiceBan, NoticeList,
    ReportCounts, ReportDecision, ReportDecisionRequest, RoleChanged, RoleUpdate, SeedRevoke,
    SeedRevoked, SessionRecord, StrikeReviewRequest, VoiceBanApplied, VoiceMuteRecord,
};
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, IdempotencyKey};
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Rows of an audit or report page.
pub const PAGE: i64 = 50;

type Cached<T> = ([(axum::http::HeaderName, &'static str); 1], Json<T>);

fn private<T>(body: T) -> Cached<T> {
    ([(CACHE_CONTROL, PRIVATE)], Json(body))
}

fn key_reused() -> ApiError {
    ApiError::new(
        ErrorCode::RequestIdempotencyKeyReused,
        "this Idempotency-Key was used for another request",
    )
}

fn not_self(session: &CoreSession, target: Uuid) -> Result<(), ApiError> {
    if session.account_id == target {
        Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "nobody moderates their own account",
        ))
    } else {
        Ok(())
    }
}

fn account_param(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| ApiError::invalid("accountId", "a UUID"))
}

fn reason(field: &'static str, text: &str) -> Result<(), ApiError> {
    if text.trim().is_empty() || text.chars().count() > 500 || text.chars().any(char::is_control) {
        Err(ApiError::invalid(
            field,
            "1 to 500 characters, no control characters",
        ))
    } else {
        Ok(())
    }
}

fn instant(field: &'static str, text: &str) -> Result<UnixMs, ApiError> {
    fi_protocol::instant::parse_unix_micros(text)
        .map(|m| UnixMs::from_millis(m.div_euclid(1_000)))
        .ok_or_else(|| ApiError::invalid(field, "an RFC 3339 instant"))
}

fn future(field: &'static str, text: &str) -> Result<UnixMs, ApiError> {
    let t = instant(field, text)?;
    if t <= UnixMs::now_system() {
        return Err(ApiError::invalid(field, "an instant in the future"));
    }
    Ok(t)
}

/// One admin write: the transaction, its idempotency record and its single audit row.
struct Write {
    tx: Transaction<'static, Postgres>,
    scope: String,
    idem: Idempotency,
}

enum Begun {
    Replay(StatusCode, Option<Value>),
    Fresh(Write),
}

async fn begin<T: Serialize>(
    state: &AppState,
    session: &CoreSession,
    key: &IdempotencyKey,
    route: &'static str,
    body: &T,
) -> Result<Begun, ApiError> {
    let scope = format!("account:{}", session.account_id);
    let idem = key.of(route, body);
    let mut tx = state.pool.begin().await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        Recorded::Same(r) => Ok(Begun::Replay(
            StatusCode::from_u16(r.status as u16).unwrap_or(StatusCode::OK),
            r.response,
        )),
        Recorded::Different => Err(key_reused()),
        Recorded::None => Ok(Begun::Fresh(Write { tx, scope, idem })),
    }
}

fn replayed(status: StatusCode, body: Option<Value>) -> Response {
    match body {
        Some(b) => (status, Json(b)).into_response(),
        None => status.into_response(),
    }
}

impl Write {
    async fn audit(&mut self, session: &CoreSession, e: AuditEntry<'_>) -> Result<(), ApiError> {
        moderation::audit_in(&mut self.tx, &session.actor(), &e).await?;
        Ok(())
    }

    async fn finish<T: Serialize>(
        mut self,
        status: StatusCode,
        body: Option<&T>,
    ) -> Result<Response, ApiError> {
        let value = body
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| ApiError::unavailable())?;
        limits::record_with_response_in(
            &mut self.tx,
            &self.scope,
            &self.idem,
            status.as_u16() as i16,
            None,
            value.as_ref(),
        )
        .await?;
        self.tx.commit().await?;
        Ok(replayed(status, value))
    }
}

macro_rules! fresh_or_replay {
    ($begun:expr) => {
        match $begun {
            Begun::Replay(status, body) => return Ok(replayed(status, body)),
            Begun::Fresh(w) => w,
        }
    };
}

// Maintenance ---------------------------------------------------------------------------------

/// `?game=` of the maintenance switch.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameQuery {
    /// Game.
    pub game: Option<String>,
}

fn game_query(
    query: Result<Query<GameQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<String, ApiError> {
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "game only"))?;
    q.game
        .filter(|g| crate::valid_game(g))
        .ok_or_else(|| ApiError::invalid("game", "a game id such as fh2"))
}

/// `GET /core/v1/admin/maintenance?game=fh2` (ADM) → 200 [`Maintenance`].
pub async fn get_maintenance(
    State(state): State<AppState>,
    session: CoreSession,
    query: Result<Query<GameQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Cached<Maintenance>, ApiError> {
    session.require(Role::Admin)?;
    let game = game_query(query)?;
    let mut conn = state.pool.acquire().await?;
    let g = notices::game_in(&mut conn, &game)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "unknown game"))?;
    Ok(private(Maintenance {
        game: g.code,
        until: g.maintenance_until.map(views::at),
        message: g.maintenance_message,
    }))
}

/// `PUT /core/v1/admin/maintenance {game, until, message?}` (ADM) → 200 [`Maintenance`]; `until`
/// in the future sets it (the gate refuses admissions, the board shows a maintenance notice),
/// `null` lifts it.
pub async fn put_maintenance(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<MaintenanceUpdate>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    if !crate::valid_game(&req.game) {
        return Err(ApiError::invalid("game", "a game id such as fh2"));
    }
    let until = req
        .until
        .as_deref()
        .map(|t| future("until", t))
        .transpose()?;
    if let Some(m) = &req.message {
        reason("message", m)?;
    }
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "PUT /core/v1/admin/maintenance",
            &req
        )
        .await?
    );
    let g = notices::set_maintenance_in(&mut w.tx, &req.game, until, req.message.as_deref())
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => {
                ApiError::new(ErrorCode::PortalNotFound, "unknown game")
            }
            e => e.into(),
        })?;
    let target = format!("game:{}", g.code);
    w.audit(
        &session,
        AuditEntry {
            action: if until.is_some() {
                "game.maintenance_set"
            } else {
                "game.maintenance_lift"
            },
            account_id: None,
            target: &target,
            reason: req.message.as_deref(),
            detail: json!({"until": req.until}),
        },
    )
    .await?;
    let body = Maintenance {
        game: g.code,
        until: g.maintenance_until.map(views::at),
        message: g.maintenance_message,
    };
    w.finish(StatusCode::OK, Some(&body)).await
}

// Online capacity (OD-71) -------------------------------------------------------------------

/// The largest online capacity an admin may set (the column's CHECK).
pub const MAX_ONLINE_CAPACITY: u32 = 100_000;

async fn game_capacity(
    conn: &mut sqlx::PgConnection,
    game: &str,
) -> Result<GameCapacity, ApiError> {
    let load = fi_db::gate::online_load_in(conn, game)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "unknown game"))?;
    Ok(GameCapacity {
        game: game.to_owned(),
        online_capacity: load.capacity,
        active: load.active,
        queued: load.queued,
    })
}

/// `GET /core/v1/admin/capacity?game=fh2` (ADM) → 200 [`GameCapacity`] (OD-71).
pub async fn get_capacity(
    State(state): State<AppState>,
    session: CoreSession,
    query: Result<Query<GameQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Cached<GameCapacity>, ApiError> {
    session.require(Role::Admin)?;
    let game = game_query(query)?;
    let mut conn = state.pool.acquire().await?;
    Ok(private(game_capacity(&mut conn, &game).await?))
}

/// `PUT /core/v1/admin/capacity {game, onlineCapacity}` (ADM) → 200 [`GameCapacity`] (OD-71):
/// the game's online capacity, 1 to [`MAX_ONLINE_CAPACITY`] or `null` (unlimited). The gate reads
/// it on its next decision; lowering it ends no session. Audited as `game.capacity_set`.
pub async fn put_capacity(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<GameCapacityUpdate>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    if !crate::valid_game(&req.game) {
        return Err(ApiError::invalid("game", "a game id such as fh2"));
    }
    if req
        .online_capacity
        .is_some_and(|c| !(1..=MAX_ONLINE_CAPACITY).contains(&c))
    {
        return Err(ApiError::invalid(
            "onlineCapacity",
            "1 to 100000, or null for unlimited",
        ));
    }
    let mut w =
        fresh_or_replay!(begin(&state, &session, &key, "PUT /core/v1/admin/capacity", &req).await?);
    let previous = fi_db::gate::set_online_capacity_in(&mut w.tx, &req.game, req.online_capacity)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "unknown game"))?;
    let target = format!("game:{}", req.game);
    w.audit(
        &session,
        AuditEntry {
            action: "game.capacity_set",
            account_id: None,
            target: &target,
            reason: None,
            detail: json!({"from": previous, "to": req.online_capacity}),
        },
    )
    .await?;
    let body = game_capacity(&mut w.tx, &req.game).await?;
    w.finish(StatusCode::OK, Some(&body)).await
}

// Notices -------------------------------------------------------------------------------------

/// `GET /core/v1/admin/notices` (ADM) → 200 [`NoticeList`] (scheduled and expired included).
pub async fn list_notices(
    State(state): State<AppState>,
    session: CoreSession,
) -> Result<Cached<NoticeList>, ApiError> {
    session.require(Role::Admin)?;
    let mut conn = state.pool.acquire().await?;
    let rows = notices::admin_list_in(&mut conn).await?;
    Ok(private(NoticeList {
        items: rows.iter().map(notice).collect(),
    }))
}

/// `POST /core/v1/admin/notices` (ADM) → 201 `Notice`.
pub async fn create_notice(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<NewNotice>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    if !matches!(req.kind.as_str(), "news" | "warning" | "event") {
        return Err(ApiError::invalid(
            "kind",
            "news, warning or event (maintenance comes from the switch)",
        ));
    }
    if !matches!(req.level.as_str(), "info" | "warning" | "error") {
        return Err(ApiError::invalid("level", "info, warning or error"));
    }
    crate::status::lang(Some(&req.lang)).map_err(|_| ApiError::invalid("lang", "en or pt-BR"))?;
    if req.game.as_deref().is_some_and(|g| !crate::valid_game(g)) {
        return Err(ApiError::invalid("game", "a game id such as fh2, or null"));
    }
    if req.title.trim().is_empty()
        || req.title.chars().count() > 120
        || req.title.chars().any(char::is_control)
    {
        return Err(ApiError::invalid("title", "1 to 120 characters"));
    }
    if req.body.trim().is_empty() || req.body.len() > 4096 {
        return Err(ApiError::invalid("body", "1 byte to 4 KiB of Markdown"));
    }
    let since = req
        .since
        .as_deref()
        .map(|t| instant("since", t))
        .transpose()?;
    let until = req
        .until
        .as_deref()
        .map(|t| instant("until", t))
        .transpose()?;
    if let Some(u) = until
        && u <= since.unwrap_or_else(UnixMs::now_system)
    {
        return Err(ApiError::invalid("until", "after since"));
    }
    let mut w =
        fresh_or_replay!(begin(&state, &session, &key, "POST /core/v1/admin/notices", &req).await?);
    let row = notices::create_in(
        &mut w.tx,
        &notices::NewNotice {
            game: req.game.as_deref(),
            kind: &req.kind,
            level: &req.level,
            title: &req.title,
            body: &req.body,
            lang: &req.lang,
            since,
            until,
            created_by: session.account_id,
        },
    )
    .await
    .map_err(|e| match e {
        fi_db::identity::IdentityError::NotFound => ApiError::invalid("game", "an unknown game"),
        e => e.into(),
    })?;
    let target = format!("notice:{}", row.id);
    w.audit(
        &session,
        AuditEntry {
            action: "notice.create",
            account_id: None,
            target: &target,
            reason: None,
            detail: json!({"game": row.game, "kind": row.kind, "lang": row.lang}),
        },
    )
    .await?;
    w.finish(StatusCode::CREATED, Some(&notice(&row))).await
}

/// `DELETE /core/v1/admin/notices/{id}` (ADM) → 204; `PORTAL.NOT_FOUND` when unknown.
pub async fn delete_notice(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    let id =
        Uuid::parse_str(&id).map_err(|_| ApiError::new(ErrorCode::PortalNotFound, "not found"))?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "DELETE /core/v1/admin/notices/{id}",
            &id
        )
        .await?
    );
    if !notices::delete_in(&mut w.tx, id).await? {
        return Err(ApiError::new(ErrorCode::PortalNotFound, "not found"));
    }
    let target = format!("notice:{id}");
    w.audit(
        &session,
        AuditEntry {
            action: "notice.delete",
            account_id: None,
            target: &target,
            reason: None,
            detail: json!({}),
        },
    )
    .await?;
    w.finish::<()>(StatusCode::NO_CONTENT, None).await
}

// Accounts ------------------------------------------------------------------------------------

/// `?gamertag=|xuid=|email=` of the account lookup (exactly one).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupQuery {
    /// `Name#12345` or a name prefix.
    pub gamertag: Option<String>,
    /// A decimal XUID.
    pub xuid: Option<String>,
    /// An e-mail (ADM only).
    pub email: Option<String>,
}

/// `GET /core/v1/admin/accounts?gamertag=|xuid=|email=` (MOD; e-mail ADM) → 200
/// [`AccountLookupList`].
pub async fn lookup(
    State(state): State<AppState>,
    session: CoreSession,
    query: Result<Query<LookupQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Cached<AccountLookupList>, ApiError> {
    session.require(Role::Moderator)?;
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "gamertag, xuid or email"))?;
    let email;
    let by = match (q.gamertag.as_deref(), q.xuid.as_deref(), q.email.as_deref()) {
        (Some(tag), None, None) => {
            let (name, disc) = tag.split_once('#').unwrap_or((tag, ""));
            if name.is_empty()
                || name.len() > 20
                || !name.bytes().all(|b| b.is_ascii_alphanumeric())
                || !disc.bytes().all(|b| b.is_ascii_digit())
                || disc.len() > 5
            {
                return Err(ApiError::invalid("gamertag", "Name#12345 or a name prefix"));
            }
            Lookup::Gamertag(tag)
        }
        (None, Some(x), None) => Lookup::Xuid(
            x.parse::<u64>()
                .map_err(|_| ApiError::invalid("xuid", "a decimal XUID"))?,
        ),
        (None, None, Some(e)) => {
            session.require(Role::Admin)?;
            if !(3..=320).contains(&e.len()) || !e.contains('@') {
                return Err(ApiError::invalid("email", "an e-mail address"));
            }
            email = e.trim().to_lowercase();
            Lookup::Email(&email)
        }
        _ => {
            return Err(ApiError::invalid(
                "query",
                "exactly one of gamertag, xuid, email",
            ));
        }
    };
    let mut conn = state.pool.acquire().await?;
    let rows = moderation::lookup_in(&mut conn, &by).await?;
    Ok(private(AccountLookupList {
        items: rows
            .into_iter()
            .map(|r| AccountLookup {
                account_id: r.account_id.to_string(),
                gamertag: r.gamertag,
                game: r.game,
                xuid: r.xuid.to_string(),
            })
            .collect(),
    }))
}

fn mask(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let first: String = local.chars().take(1).collect();
            format!("{first}***@{domain}")
        }
        None => "***".into(),
    }
}

fn action(r: &AuditRow) -> AdminAction {
    AdminAction {
        id: r.id.to_string(),
        at: views::at(r.at),
        actor: AuditActor {
            account_id: r.actor_account_id.map(|a| a.to_string()),
            role: r.actor_role.clone(),
        },
        action: r.action.clone(),
        target: r.target.clone(),
        reason: r.reason.clone(),
        request_id: r.request_id.clone(),
    }
}

/// `GET /core/v1/admin/accounts/{accountId}` (MOD, ADM) → 200 [`AdminAccountView`]; the e-mail
/// (masked) for ADM only.
pub async fn account(
    State(state): State<AppState>,
    session: CoreSession,
    Path(account_id): Path<String>,
) -> Result<Cached<AdminAccountView>, ApiError> {
    session.require(Role::Moderator)?;
    let account_id = account_param(&account_id)?;
    let mut conn = state.pool.acquire().await?;
    let view = moderation::account_view_in(&mut conn, account_id)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::AccountsNotFound, "not found"))?;
    let profiles = accounts::profiles_in(&mut conn, account_id).await?;
    let access = accounts::access_in(&mut conn, account_id).await?;
    let strikes = moderation::strikes_of_account_in(&mut conn, account_id, None).await?;
    let sessions = moderation::sessions_of_account_in(&mut conn, account_id, 10).await?;
    let (made, received) = moderation::report_counts_in(&mut conn, account_id).await?;
    let audit = moderation::audit_feed_in(
        &mut conn,
        &AuditFilter {
            account_id: Some(account_id),
            ..Default::default()
        },
        20,
    )
    .await?;
    let discord_link = fi_db::discord::link_of_in(&mut conn, account_id)
        .await
        .map_err(fi_db::identity::IdentityError::from)?;
    // CR-S68: the voice blocks in force and the newest mute events about the account.
    let voice_state = fi_db::voice::state_in(&mut conn, account_id)
        .await
        .map_err(fi_db::identity::IdentityError::from)?;
    let mutes = fi_db::voice::mutes_about_in(&mut conn, account_id, VOICE_MUTES_SHOWN)
        .await
        .map_err(fi_db::identity::IdentityError::from)?;
    // MATCHMAKING_API.md §3.9: its profiles' cheat flags, live or within the repeat window.
    let cheat = fi_db::observations::flags_of_account_in(&mut conn, account_id).await?;
    let live = sessions
        .iter()
        .find(|s| s.state == "active" || s.state == "queued")
        .map(|s| LiveSession {
            game: s.game.clone(),
            pool: s.pool.clone(),
            since: views::at(s.created_at),
        });
    Ok(private(AdminAccountView {
        account: AdminAccount {
            id: account_id.to_string(),
            status: view.state.status.clone(),
            role: view.state.role.clone(),
            origin: view.state.origin.clone(),
            created_at: views::at(view.created_at),
            ban: view.ban.as_ref().map(|(since, until, reason)| Ban {
                since: views::at(*since),
                until: until.map(views::at),
                reason: reason.clone(),
                active: view.state.banned,
            }),
            deletion: views::deletion(&view.state),
            email_masked: (session.role == Role::Admin)
                .then(|| view.email.as_deref().map(mask))
                .flatten(),
            discord_link: discord_link.as_ref().map(crate::discord::link_view),
        },
        profiles: profiles.iter().map(views::profile).collect(),
        tier: if access.vip { "vip" } else { "free" }.into(),
        strikes: strikes.iter().map(strike).collect(),
        sessions: AdminSessions {
            live,
            recent: sessions
                .iter()
                .map(|s| SessionRecord {
                    id: s.id.to_string(),
                    game: s.game.clone(),
                    pool: s.pool.clone(),
                    state: s.state.clone(),
                    created_at: views::at(s.created_at),
                    ended_at: s.ended_at.map(views::at),
                    end_reason: s.end_reason.clone(),
                })
                .collect(),
        },
        reports: ReportCounts {
            made: made.max(0) as u64,
            received: received.max(0) as u64,
        },
        audit: audit.iter().map(action).collect(),
        voice: AdminVoice {
            banned_until: voice_state.banned_until.map(views::at),
            community_until: voice_state.community_until.map(views::at),
            mutes: mutes
                .iter()
                .map(|m| VoiceMuteRecord {
                    muter_account_id: m.muter_account_id.to_string(),
                    action: m.action.clone(),
                    world_id: m.world_id.to_string(),
                    game: m.game.clone(),
                    at: views::at(m.at),
                })
                .collect(),
        },
        cheat: cheat
            .iter()
            .map(|f| AdminCheatFlag {
                profile_id: f.profile_id.to_string(),
                until: views::at(f.until),
                first_at: views::at(f.first_at),
                episode: f.episode,
                kinds: f.kinds.clone(),
                in_force: f.in_force,
                lifted: f.lifted,
            })
            .collect(),
    }))
}

/// Voice mute events shown in the moderation view of an account (CR-S68).
pub const VOICE_MUTES_SHOWN: i64 = 50;

// Strikes -------------------------------------------------------------------------------------

/// `POST /core/v1/admin/strikes {xuid, source: "manual", reason}` (MOD, ADM) → 201 `Strike`.
pub async fn create_strike(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<NewStrike>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    if req.source != "manual" {
        return Err(ApiError::invalid("source", "manual"));
    }
    reason("reason", &req.reason)?;
    let xuid = req
        .xuid
        .parse::<u64>()
        .map_err(|_| ApiError::invalid("xuid", "a decimal XUID"))?;
    let mut w =
        fresh_or_replay!(begin(&state, &session, &key, "POST /core/v1/admin/strikes", &req).await?);
    let owner = moderation::lookup_in(&mut w.tx, &Lookup::Xuid(xuid))
        .await?
        .first()
        .map(|r| r.account_id)
        .ok_or_else(|| ApiError::new(ErrorCode::AccountsNotFound, "no such profile"))?;
    not_self(&session, owner)?;
    let row =
        moderation::create_strike_in(&mut w.tx, xuid, &req.reason, session.account_id).await?;
    let target = format!("strike:{}", row.id);
    w.audit(
        &session,
        AuditEntry {
            action: "strike.create",
            account_id: Some(row.account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({"xuid": req.xuid, "game": row.game}),
        },
    )
    .await?;
    w.finish(StatusCode::CREATED, Some(&strike(&row))).await
}

/// `POST /core/v1/admin/strikes/{id}/review {outcome, note?}` (MOD, ADM) → 200 `Strike`.
pub async fn review_strike(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
    Body(req): Body<StrikeReviewRequest>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let id = Uuid::parse_str(&id)
        .map_err(|_| ApiError::new(ErrorCode::AccountsNotFound, "not found"))?;
    if !matches!(req.outcome.as_str(), "upheld" | "overturned") {
        return Err(ApiError::invalid("outcome", "upheld or overturned"));
    }
    if let Some(n) = &req.note {
        reason("note", n)?;
    }
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "POST /core/v1/admin/strikes/{id}/review",
            &(id, &req)
        )
        .await?
    );
    let current = moderation::strike_in(&mut w.tx, id)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::AccountsNotFound, "no such strike"))?;
    not_self(&session, current.account_id)?;
    let row = moderation::review_strike_in(&mut w.tx, id, &req.outcome, session.account_id).await?;
    let target = format!("strike:{id}");
    w.audit(
        &session,
        AuditEntry {
            action: "strike.review",
            account_id: Some(row.account_id),
            target: &target,
            reason: req.note.as_deref(),
            detail: json!({"outcome": req.outcome}),
        },
    )
    .await?;
    w.finish(StatusCode::OK, Some(&strike(&row))).await
}

// Bans ----------------------------------------------------------------------------------------

/// `POST /core/v1/admin/bans {accountId, until, reason}` (ADM) → 201 [`BanApplied`]; revokes every
/// refresh family and portal session, marks live play sessions `revoked` (`game_until = now`) and
/// revokes MCP connections, in the same transaction as the audit row.
pub async fn ban(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<NewBan>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    let account_id = account_param(&req.account_id)?;
    not_self(&session, account_id)?;
    reason("reason", &req.reason)?;
    let until = req
        .until
        .as_deref()
        .map(|t| future("until", t))
        .transpose()?;
    let mut w =
        fresh_or_replay!(begin(&state, &session, &key, "POST /core/v1/admin/bans", &req).await?);
    let revoked = moderation::ban_in(&mut w.tx, account_id, until, &req.reason)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => {
                ApiError::new(ErrorCode::AccountsNotFound, "not found")
            }
            e => e.into(),
        })?;
    // CR-S63: #moderation-chat (a `staff` feed row, in this transaction).
    fi_db::discord::staff_event_in(
        &mut w.tx,
        fi_db::discord::StaffKind::Banned,
        account_id,
        None,
        Some(session.account_id),
        until,
    )
    .await
    .map_err(fi_db::identity::IdentityError::from)?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "account.ban",
            account_id: Some(account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({
                "until": req.until,
                "tokensRevoked": revoked.tokens_revoked,
                "sessionsRevoked": revoked.sessions_revoked,
                "connectionsRevoked": revoked.connections_revoked,
            }),
        },
    )
    .await?;
    let body = BanApplied {
        account_id: account_id.to_string(),
        until: until.map(views::at),
    };
    w.finish(StatusCode::CREATED, Some(&body)).await
}

// Kicks (CR-S62) ------------------------------------------------------------------------------

/// `POST /core/v1/admin/kicks {xuid | accountId, reason, cooldownMinutes?}` (MOD, ADM) → 200
/// [`KickApplied`]: the account's live play sessions end `revoked` / `kicked` (as a ban's revoke)
/// without a ban; refresh families, portal sessions and MCP connections stay. With a cooldown the
/// gate refuses its admissions until `until` (`SESSIONS.KICKED`). Nobody kicks themselves, and a
/// moderator kicks neither a moderator nor an admin (`AUTH.FORBIDDEN`); an unknown XUID or account
/// is 404 `PORTAL.NOT_FOUND`. Audited as `account.kick`.
pub async fn kick(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<NewKick>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let not_found = || ApiError::new(ErrorCode::PortalNotFound, "no such account or profile");
    let (xuid, account) = match (req.xuid.as_deref(), req.account_id.as_deref()) {
        (Some(x), None) => {
            let xuid = x
                .parse::<u64>()
                .ok()
                .filter(|v| *v > 0 && v.to_string() == x)
                .ok_or_else(|| ApiError::invalid("xuid", "a decimal XUID"))?;
            (Some(xuid), None)
        }
        (None, Some(a)) => {
            let id = account_param(a)?;
            not_self(&session, id)?;
            (None, Some(id))
        }
        (Some(_), Some(_)) => {
            return Err(ApiError::invalid(
                "accountId",
                "exactly one of xuid and accountId",
            ));
        }
        (None, None) => {
            return Err(ApiError::invalid(
                "xuid",
                "exactly one of xuid and accountId",
            ));
        }
    };
    reason("reason", &req.reason)?;
    let cooldown = req.cooldown_minutes.unwrap_or(0);
    if cooldown > moderation::KICK_COOLDOWN_MAX_MINUTES {
        return Err(ApiError::invalid("cooldownMinutes", "0 to 60"));
    }
    let mut w =
        fresh_or_replay!(begin(&state, &session, &key, "POST /core/v1/admin/kicks", &req).await?);
    let account_id = match (xuid, account) {
        (Some(x), _) => moderation::lookup_in(&mut w.tx, &Lookup::Xuid(x))
            .await?
            .first()
            .map(|r| r.account_id)
            .ok_or_else(not_found)?,
        (None, Some(id)) => id,
        (None, None) => return Err(not_found()),
    };
    not_self(&session, account_id)?;
    let role = moderation::kick_target_in(&mut w.tx, account_id)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => not_found(),
            e => e.into(),
        })?;
    if session.role < Role::Admin && role >= Role::Moderator {
        return Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "only an admin kicks a moderator or an admin",
        ));
    }
    let kicked = moderation::kick_in(&mut w.tx, account_id, cooldown)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => not_found(),
            e => e.into(),
        })?;
    // CR-S63: #moderation-chat (a `staff` feed row, in this transaction).
    fi_db::discord::staff_event_in(
        &mut w.tx,
        fi_db::discord::StaffKind::Kicked,
        account_id,
        xuid,
        Some(session.account_id),
        kicked.until,
    )
    .await
    .map_err(fi_db::identity::IdentityError::from)?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "account.kick",
            account_id: Some(account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({
                "sessionsEnded": kicked.sessions_ended,
                "cooldownMinutes": cooldown,
            }),
        },
    )
    .await?;
    let body = KickApplied {
        account_id: account_id.to_string(),
        sessions_ended: kicked.sessions_ended,
        until: kicked.until.map(views::at),
    };
    w.finish(StatusCode::OK, Some(&body)).await
}

/// The account a moderator action names (CR-S62's rules, shared by the voice bans): exactly one of
/// `xuid` and `accountId`, never one's own. Returns the account and the XUID when one was given.
fn one_target(
    session: &CoreSession,
    xuid: Option<&str>,
    account: Option<&str>,
) -> Result<(Option<u64>, Option<Uuid>), ApiError> {
    match (xuid, account) {
        (Some(x), None) => {
            let xuid = x
                .parse::<u64>()
                .ok()
                .filter(|v| *v > 0 && v.to_string() == x)
                .ok_or_else(|| ApiError::invalid("xuid", "a decimal XUID"))?;
            Ok((Some(xuid), None))
        }
        (None, Some(a)) => {
            let id = account_param(a)?;
            not_self(session, id)?;
            Ok((None, Some(id)))
        }
        (Some(_), Some(_)) => Err(ApiError::invalid(
            "accountId",
            "exactly one of xuid and accountId",
        )),
        (None, None) => Err(ApiError::invalid(
            "xuid",
            "exactly one of xuid and accountId",
        )),
    }
}

/// Locks the target account of a moderator action and applies CR-S62's role rules: nobody acts on
/// their own account, a moderator acts on neither a moderator nor an admin (`AUTH.FORBIDDEN`); an
/// unknown or deleted account is 404 `PORTAL.NOT_FOUND`.
async fn moderated_target(
    w: &mut Write,
    session: &CoreSession,
    account_id: Uuid,
) -> Result<(), ApiError> {
    let not_found = || ApiError::new(ErrorCode::PortalNotFound, "no such account or profile");
    not_self(session, account_id)?;
    let role = moderation::kick_target_in(&mut w.tx, account_id)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => not_found(),
            e => e.into(),
        })?;
    if session.role < Role::Admin && role >= Role::Moderator {
        return Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "only an admin acts on a moderator or an admin",
        ));
    }
    Ok(())
}

/// `POST /core/v1/admin/voice-bans {xuid? | accountId?, minutes, reason}` (MOD, ADM; CR-S68) → 200
/// [`VoiceBanApplied`]: the account's voice is blocked for `minutes` (replacing an earlier voice
/// ban); nothing else changes. CR-S62's role rules. Audited as `account.voice_ban`; a
/// `voice_banned` staff row.
pub async fn voice_ban(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Body(req): Body<NewVoiceBan>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let not_found = || ApiError::new(ErrorCode::PortalNotFound, "no such account or profile");
    let (xuid, account) = one_target(&session, req.xuid.as_deref(), req.account_id.as_deref())?;
    if !(1..=fi_db::voice::BAN_MAX_MINUTES).contains(&req.minutes) {
        return Err(ApiError::invalid("minutes", "1 to 43200"));
    }
    reason("reason", &req.reason)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "POST /core/v1/admin/voice-bans",
            &req
        )
        .await?
    );
    let account_id = match (xuid, account) {
        (Some(x), _) => moderation::lookup_in(&mut w.tx, &Lookup::Xuid(x))
            .await?
            .first()
            .map(|r| r.account_id)
            .ok_or_else(not_found)?,
        (None, Some(id)) => id,
        (None, None) => return Err(not_found()),
    };
    moderated_target(&mut w, &session, account_id).await?;
    let until = fi_db::voice::ban_in(&mut w.tx, account_id, req.minutes)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => not_found(),
            e => e.into(),
        })?;
    fi_db::discord::staff_event_in(
        &mut w.tx,
        fi_db::discord::StaffKind::VoiceBanned,
        account_id,
        xuid,
        Some(session.account_id),
        Some(until),
    )
    .await
    .map_err(fi_db::identity::IdentityError::from)?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "account.voice_ban",
            account_id: Some(account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({"minutes": req.minutes}),
        },
    )
    .await?;
    let body = VoiceBanApplied {
        account_id: account_id.to_string(),
        until: views::at(until),
    };
    w.finish(StatusCode::OK, Some(&body)).await
}

/// `DELETE /core/v1/admin/voice-bans/{accountId}` (MOD, ADM; CR-S68) → 204 (also when nothing
/// was in force): lifts a voice ban and a community mute in force. CR-S62's role rules. Audited as
/// `account.voice_unban`; a `voice_unbanned` staff row when something was lifted.
pub async fn lift_voice_ban(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(account_id): Path<String>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let account_id = account_param(&account_id)?;
    not_self(&session, account_id)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "DELETE /core/v1/admin/voice-bans/{accountId}",
            &account_id
        )
        .await?
    );
    moderated_target(&mut w, &session, account_id).await?;
    let lifted = fi_db::voice::lift_in(&mut w.tx, account_id).await?;
    if lifted.banned || lifted.community {
        fi_db::discord::staff_event_in(
            &mut w.tx,
            fi_db::discord::StaffKind::VoiceUnbanned,
            account_id,
            None,
            Some(session.account_id),
            None,
        )
        .await
        .map_err(fi_db::identity::IdentityError::from)?;
    }
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "account.voice_unban",
            account_id: Some(account_id),
            target: &target,
            reason: None,
            detail: json!({"wasBanned": lifted.banned, "wasCommunityMuted": lifted.community}),
        },
    )
    .await?;
    w.finish::<()>(StatusCode::NO_CONTENT, None).await
}

/// `DELETE /core/v1/admin/cheat-flags/{profileId} {reason}` (MOD, ADM; MATCHMAKING_API.md §3.9
/// "Lifting a flag") → 204 (also when no flag was in force): the profile's live cheat flag ends now
/// and is marked lifted, so it leaves the pineapple pool at its next list or join and the next flag
/// does not double on this one. CR-S62's role rules on the profile's account (404
/// `PORTAL.NOT_FOUND` for an unknown or deleted profile). Audited as `profile.cheat_unflag`.
pub async fn lift_cheat_flag(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(profile_id): Path<String>,
    Body(req): Body<CheatUnflag>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let profile_id =
        Uuid::parse_str(&profile_id).map_err(|_| ApiError::invalid("profileId", "a UUID"))?;
    reason("reason", &req.reason)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "DELETE /core/v1/admin/cheat-flags/{profileId}",
            &(profile_id, &req)
        )
        .await?
    );
    let account_id = fi_db::observations::profile_account_in(&mut w.tx, profile_id)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "no such account or profile"))?;
    moderated_target(&mut w, &session, account_id).await?;
    let lifted = fi_db::observations::unflag_in(&mut w.tx, profile_id).await?;
    let target = format!("profile:{profile_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "profile.cheat_unflag",
            account_id: Some(account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({"wasFlagged": lifted.was_flagged, "episode": lifted.episode}),
        },
    )
    .await?;
    w.finish::<()>(StatusCode::NO_CONTENT, None).await
}

/// `DELETE /core/v1/admin/bans/{accountId}` (ADM) → 204 (also when it was not banned).
pub async fn lift_ban(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(account_id): Path<String>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    let account_id = account_param(&account_id)?;
    not_self(&session, account_id)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "DELETE /core/v1/admin/bans/{accountId}",
            &account_id
        )
        .await?
    );
    let lifted = moderation::lift_ban_in(&mut w.tx, account_id)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => {
                ApiError::new(ErrorCode::AccountsNotFound, "not found")
            }
            e => e.into(),
        })?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "account.unban",
            account_id: Some(account_id),
            target: &target,
            reason: None,
            detail: json!({"wasBanned": lifted}),
        },
    )
    .await?;
    w.finish::<()>(StatusCode::NO_CONTENT, None).await
}

// Reports -------------------------------------------------------------------------------------

/// `?state=&cursor=` of the report queue.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportsQuery {
    /// `open` (default) | `dismissed` | `actioned`.
    pub state: Option<String>,
    /// The `next` of the previous page.
    pub cursor: Option<String>,
}

fn report(r: &ReportRow) -> AdminReport {
    AdminReport {
        id: r.id.to_string(),
        target: format!("{}:{}", r.target_kind, r.target_id),
        target_account_id: r.target_account_id.map(|a| a.to_string()),
        reporter_account_id: r.reporter_account_id.to_string(),
        reason: r.reason.clone(),
        text: r.text.clone(),
        state: r.state.clone(),
        created_at: views::at(r.created_at),
        decided: r.decided.as_ref().map(|(at, action, note)| ReportDecision {
            at: views::at(*at),
            action: action.clone(),
            note: note.clone(),
        }),
        source: r.source.clone(),
        world_id: r.world_id.map(|w| w.to_string()),
    }
}

/// `GET /core/v1/admin/reports?state=open&cursor=` (MOD, ADM) → 200 [`AdminReportList`].
pub async fn reports(
    State(state): State<AppState>,
    session: CoreSession,
    query: Result<Query<ReportsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Cached<AdminReportList>, ApiError> {
    session.require(Role::Moderator)?;
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "state and cursor only"))?;
    let st = q.state.as_deref().unwrap_or("open");
    if !matches!(st, "open" | "dismissed" | "actioned") {
        return Err(ApiError::invalid("state", "open, dismissed or actioned"));
    }
    let after = cursor_id(q.cursor.as_deref())?;
    let mut conn = state.pool.acquire().await?;
    let rows = moderation::reports_in(&mut conn, st, after, PAGE).await?;
    Ok(private(AdminReportList {
        next: next_id(&rows, PAGE, |r| r.id),
        items: rows.iter().map(report).collect(),
    }))
}

/// `POST /core/v1/admin/reports/{id} {action, note?}` (MOD, ADM; `ban` ADM) → 200 `AdminReport`:
/// closes every open report on the target; `dismiss` shows the target again, `hide` keeps it
/// hidden, `strike` adds a manual strike on the target's profile (a comment's author), `ban`
/// bans the target's account permanently. `PORTAL.CONFLICT {state}` when already decided.
pub async fn decide_report(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
    Body(req): Body<ReportDecisionRequest>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let id =
        Uuid::parse_str(&id).map_err(|_| ApiError::new(ErrorCode::PortalNotFound, "not found"))?;
    if !matches!(req.action.as_str(), "dismiss" | "hide" | "strike" | "ban") {
        return Err(ApiError::invalid("action", "dismiss, hide, strike or ban"));
    }
    if req.action == "ban" {
        session.require(Role::Admin)?;
    }
    if let Some(n) = &req.note {
        reason("note", n)?;
    }
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "POST /core/v1/admin/reports/{id}",
            &(id, &req)
        )
        .await?
    );
    let row = moderation::lock_report_in(&mut w.tx, id)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "not found"))?;
    if row.state != "open" {
        return Err(
            ApiError::new(ErrorCode::PortalConflict, "this report was decided")
                .with_details(json!({"state": row.state})),
        );
    }
    if let Some(target) = row.target_account_id {
        not_self(&session, target)?;
    }
    let note = req
        .note
        .clone()
        .unwrap_or_else(|| format!("report: {}", row.reason));
    let mut detail = json!({"action": req.action});
    match req.action.as_str() {
        "strike" => {
            let xuid = match row.target_kind.as_str() {
                "player" => row.target_id.parse::<u64>().ok(),
                _ => match Uuid::parse_str(&row.target_id) {
                    Ok(c) => moderation::comment_in(&mut w.tx, c)
                        .await?
                        .map(|c| c.author_xuid),
                    Err(_) => None,
                },
            }
            .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "the target is gone"))?;
            let s =
                moderation::create_strike_in(&mut w.tx, xuid, &note, session.account_id).await?;
            detail["strikeId"] = json!(s.id.to_string());
        }
        "ban" => {
            let target = row
                .target_account_id
                .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "the target is gone"))?;
            let revoked = moderation::ban_in(&mut w.tx, target, None, &note).await?;
            detail["sessionsRevoked"] = json!(revoked.sessions_revoked);
        }
        _ => {}
    }
    let closed = moderation::decide_reports_in(
        &mut w.tx,
        &row,
        &req.action,
        req.note.as_deref(),
        session.account_id,
    )
    .await?;
    detail["reportsClosed"] = json!(closed);
    let target = format!("report:{id}");
    w.audit(
        &session,
        AuditEntry {
            action: "report.decide",
            account_id: row.target_account_id,
            target: &target,
            reason: req.note.as_deref(),
            detail,
        },
    )
    .await?;
    let decided = moderation::lock_report_in(&mut w.tx, id)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    w.finish(StatusCode::OK, Some(&report(&decided))).await
}

// Roles and erasure ---------------------------------------------------------------------------

/// `PUT /core/v1/admin/accounts/{accountId}/role {role}` (ADM, never one's own) → 200
/// [`RoleChanged`]; a change revokes the account's MCP connections.
pub async fn set_role(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(account_id): Path<String>,
    Body(req): Body<RoleUpdate>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    let account_id = account_param(&account_id)?;
    not_self(&session, account_id)?;
    let role = Role::parse(&req.role)
        .ok_or_else(|| ApiError::invalid("role", "player, moderator or admin"))?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "PUT /core/v1/admin/accounts/{accountId}/role",
            &(account_id, &req)
        )
        .await?
    );
    let previous = moderation::set_role_in(&mut w.tx, account_id, role)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => {
                ApiError::new(ErrorCode::AccountsNotFound, "not found")
            }
            e => e.into(),
        })?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "account.role",
            account_id: Some(account_id),
            target: &target,
            reason: None,
            detail: json!({"role": role.as_str(), "previous": previous.as_str()}),
        },
    )
    .await?;
    let body = RoleChanged {
        account_id: account_id.to_string(),
        role: role.as_str().into(),
        previous: previous.as_str().into(),
    };
    w.finish(StatusCode::OK, Some(&body)).await
}

/// `POST /core/v1/admin/accounts/{accountId}/erase` (ADM, never one's own) → 202
/// [`ErasureScheduled`]: the 14-day deletion of ACCOUNTS.md §8, forced, every family revoked.
pub async fn erase(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(account_id): Path<String>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    let account_id = account_param(&account_id)?;
    not_self(&session, account_id)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "POST /core/v1/admin/accounts/{accountId}/erase",
            &account_id
        )
        .await?
    );
    let scheduled = accounts::force_deletion_in(&mut w.tx, account_id)
        .await
        .map_err(|e| match e {
            fi_db::identity::IdentityError::NotFound => {
                ApiError::new(ErrorCode::AccountsNotFound, "not found")
            }
            e => e.into(),
        })?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "account.erase",
            account_id: Some(account_id),
            target: &target,
            reason: None,
            detail: json!({"purgeAt": views::at(scheduled.purge_at)}),
        },
    )
    .await?;
    let body = ErasureScheduled {
        account_id: account_id.to_string(),
        requested_at: views::at(scheduled.requested_at),
        purge_at: views::at(scheduled.purge_at),
    };
    w.finish(StatusCode::ACCEPTED, Some(&body)).await
}

// Audit ---------------------------------------------------------------------------------------

/// `?cursor=&actor=&target=` of the audit feed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    /// The `next` of the previous page.
    pub cursor: Option<String>,
    /// An actor account id.
    pub actor: Option<String>,
    /// `account:<id>`, `strike:<id>`, ...
    pub target: Option<String>,
}

/// `GET /core/v1/admin/audit?cursor=&actor=&target=` (MOD, ADM) → 200 [`AuditList`].
pub async fn audit(
    State(state): State<AppState>,
    session: CoreSession,
    query: Result<Query<AuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Cached<AuditList>, ApiError> {
    session.require(Role::Moderator)?;
    let Query(q) =
        query.map_err(|_| ApiError::invalid("query", "cursor, actor and target only"))?;
    let actor = q
        .actor
        .as_deref()
        .map(|a| Uuid::parse_str(a).map_err(|_| ApiError::invalid("actor", "an account id")))
        .transpose()?;
    if q.target
        .as_deref()
        .is_some_and(|t| t.len() > 128 || !t.contains(':'))
    {
        return Err(ApiError::invalid("target", "<kind>:<id>"));
    }
    let before = cursor_id(q.cursor.as_deref())?;
    let mut conn = state.pool.acquire().await?;
    let rows = moderation::audit_feed_in(
        &mut conn,
        &AuditFilter {
            before,
            actor,
            target: q.target,
            account_id: None,
        },
        PAGE,
    )
    .await?;
    Ok(private(AuditList {
        next: next_id(&rows, PAGE, |r| r.id),
        items: rows.iter().map(action).collect(),
    }))
}

// Guardrails --------------------------------------------------------------------------------

/// `?game=&xuid=|accountId=&minScore=&cursor=` of the guardrails' assessments.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssessmentsQuery {
    /// The game (required).
    pub game: Option<String>,
    /// A decimal XUID: that profile of the game.
    pub xuid: Option<String>,
    /// An account id: every profile of the account in the game.
    pub account_id: Option<String>,
    /// The lowest score listed (0 to 100; default 0).
    pub min_score: Option<String>,
    /// The `next` of the previous page.
    pub cursor: Option<String>,
}

fn guard_assessment(r: &AssessmentRow) -> GuardAssessment {
    GuardAssessment {
        id: r.id.to_string(),
        at: views::at(r.at),
        game: r.game.clone(),
        xuid: r.xuid.to_string(),
        mode: r.mode.clone(),
        source: r.source.clone(),
        session_id: r.session_id.to_string(),
        hours: r.hours,
        // The stored documents have the DTOs' shape (`fi_db::integrity::deltas_json`).
        deltas: serde_json::from_value(r.deltas.clone()).unwrap_or_default(),
        rates: serde_json::from_value(r.rates.clone()).unwrap_or_default(),
        score: u8::try_from(r.score.clamp(0, 100)).unwrap_or(100),
        reasons: r.reasons.clone(),
        refusal: r.refusal.clone(),
        decision: r.decision.clone(),
        would_block: r.would_block,
        rules_version: r.rules_version.clone(),
        revision: r.revision,
        // `fi_gamedb::audit::Facts`, the DTO's shape.
        audit: r
            .audit
            .clone()
            .map(|a| serde_json::from_value(a).unwrap_or_default()),
        bundle_sha256: r
            .bundle_sha256
            .as_ref()
            .map(|b| b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        seed_target: r.seed_target.map(|x| x.to_string()),
    }
}

/// `GET /core/v1/admin/assessments?game=&xuid=|accountId=&minScore=&cursor=` (MOD, ADM) → 200
/// [`GuardAssessmentList`]: the Hardcore guardrails' assessments (OD-62), newest first, 50 a page;
/// one profile, one account's profiles, or the game's whole feed. Read only (no audit row).
pub async fn assessments(
    State(state): State<AppState>,
    session: CoreSession,
    query: Result<Query<AssessmentsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Cached<GuardAssessmentList>, ApiError> {
    session.require(Role::Moderator)?;
    let Query(q) = query.map_err(|_| {
        ApiError::invalid("query", "game, xuid or accountId, minScore and cursor only")
    })?;
    let game = q
        .game
        .as_deref()
        .filter(|g| crate::valid_game(g))
        .ok_or_else(|| ApiError::invalid("game", "a game code"))?;
    let who = match (q.xuid.as_deref(), q.account_id.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(ApiError::invalid(
                "query",
                "at most one of xuid and accountId",
            ));
        }
        (Some(x), None) => AssessedBy::Xuid(
            x.parse::<u64>()
                .ok()
                .and_then(|x| i64::try_from(x).ok())
                .ok_or_else(|| ApiError::invalid("xuid", "a decimal XUID"))?,
        ),
        (None, Some(a)) => AssessedBy::Account(account_param(a)?),
        (None, None) => AssessedBy::Everyone,
    };
    let min_score = match q.min_score.as_deref() {
        None => 0,
        Some(text) => text
            .parse::<i16>()
            .ok()
            .filter(|s| (0..=100).contains(s))
            .ok_or_else(|| ApiError::invalid("minScore", "0 to 100"))?,
    };
    let before = cursor_id(q.cursor.as_deref())?;
    let mut conn = state.pool.acquire().await?;
    let rows = integrity::assessments_in(&mut conn, game, who, min_score, before, PAGE).await?;
    Ok(private(GuardAssessmentList {
        next: next_id(&rows, PAGE, |r| r.id),
        items: rows.iter().map(guard_assessment).collect(),
    }))
}

// Liveries (CR-S59) -----------------------------------------------------------------------------

/// Every livery route's not-found answer (an unknown livery or account): 404 `PORTAL.NOT_FOUND`.
fn livery_not_found() -> ApiError {
    ApiError::new(ErrorCode::PortalNotFound, "not found")
}

fn livery_id_param(text: &str) -> Result<i64, ApiError> {
    text.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(livery_not_found)
}

async fn known_account(conn: &mut sqlx::PgConnection, account_id: Uuid) -> Result<(), ApiError> {
    match moderation::account_view_in(conn, account_id).await? {
        Some(_) => Ok(()),
        None => Err(livery_not_found()),
    }
}

fn admin_livery(r: &liveries::AdminRow) -> AdminLivery {
    AdminLivery {
        id: r.id.to_string(),
        account_id: r.account_id.to_string(),
        xuid: (r.xuid as u64).to_string(),
        game: r.game.clone(),
        car_id: r.car_id,
        name: r.name.clone(),
        number_of_layers: r.number_of_layers,
        size: r.size,
        sha256: (!r.sha256.is_empty())
            .then(|| r.sha256.iter().map(|b| format!("{b:02x}")).collect()),
        created_at: views::at(r.created_at),
        hidden_at: r.hidden_at.map(views::at),
    }
}

/// `?cursor=` of the livery list.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveriesQuery {
    /// The `next` of the previous page.
    pub cursor: Option<String>,
}

/// `GET /core/v1/admin/accounts/{accountId}/liveries?cursor=` (MOD, ADM) → 200
/// [`AdminLiveryList`]: the account's car liveries, newest first, 100 a page, and whether its
/// uploads are blocked. Read only (no audit row). Unknown account: 404 `PORTAL.NOT_FOUND`; a bad
/// cursor: 400 `REQUEST.INVALID {field: "cursor"}`.
pub async fn account_liveries(
    State(state): State<AppState>,
    session: CoreSession,
    Path(account_id): Path<String>,
    query: Result<Query<LiveriesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Cached<AdminLiveryList>, ApiError> {
    session.require(Role::Moderator)?;
    let account_id = account_param(&account_id)?;
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "cursor only"))?;
    let cursor = q
        .cursor
        .as_deref()
        .map(|c| {
            liveries::parse_cursor(c).ok_or_else(|| ApiError::invalid("cursor", "a page cursor"))
        })
        .transpose()?;
    let mut conn = state.pool.acquire().await?;
    known_account(&mut conn, account_id).await?;
    let rows = liveries::admin_list_in(&mut conn, account_id, cursor).await?;
    let uploads_blocked = liveries::upload_blocked_in(&mut conn, account_id).await?;
    let next = (rows.len() as i64 == liveries::ADMIN_PAGE)
        .then(|| rows.last().map(|r| r.cursor.clone()))
        .flatten();
    Ok(private(AdminLiveryList {
        items: rows.iter().map(admin_livery).collect(),
        next,
        uploads_blocked,
    }))
}

/// `PUT /core/v1/admin/liveries/{id} {hidden, reason}` (MOD, ADM) → 200 [`AdminLivery`]: a hidden
/// livery and its bytes are answered as missing to every game but its owner's, also when the same
/// design is applied again. Unknown id: 404 `PORTAL.NOT_FOUND`; one's own: 403 `AUTH.FORBIDDEN`.
/// Audit `livery.hide` / `livery.show`.
pub async fn set_livery(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
    Body(req): Body<LiveryVisibility>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let id = livery_id_param(&id)?;
    reason("reason", &req.reason)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "PUT /core/v1/admin/liveries/{id}",
            &(id, &req)
        )
        .await?
    );
    let owner = liveries::admin_get_in(&mut w.tx, id)
        .await?
        .ok_or_else(livery_not_found)?;
    not_self(&session, owner.account_id)?;
    let row = liveries::set_hidden_in(&mut w.tx, id, req.hidden)
        .await?
        .ok_or_else(livery_not_found)?;
    let target = format!("livery:{id}");
    w.audit(
        &session,
        AuditEntry {
            action: if req.hidden {
                "livery.hide"
            } else {
                "livery.show"
            },
            account_id: Some(row.account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({"carId": row.car_id, "wasHidden": owner.hidden_at.is_some()}),
        },
    )
    .await?;
    let body = admin_livery(&row);
    w.finish(StatusCode::OK, Some(&body)).await
}

/// `DELETE /core/v1/admin/liveries/{id} {reason}` (MOD, ADM) → 204: the livery and, when nothing
/// else of the profile names them and they are not hidden, its bytes. Unknown id: 404
/// `PORTAL.NOT_FOUND`; one's own: 403 `AUTH.FORBIDDEN`. Audit `livery.delete`.
pub async fn delete_livery(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(id): Path<String>,
    Body(req): Body<LiveryDelete>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let id = livery_id_param(&id)?;
    reason("reason", &req.reason)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "DELETE /core/v1/admin/liveries/{id}",
            &(id, &req)
        )
        .await?
    );
    let owner = liveries::admin_get_in(&mut w.tx, id)
        .await?
        .ok_or_else(livery_not_found)?;
    not_self(&session, owner.account_id)?;
    liveries::admin_delete_in(&mut w.tx, id).await?;
    let target = format!("livery:{id}");
    w.audit(
        &session,
        AuditEntry {
            action: "livery.delete",
            account_id: Some(owner.account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({"carId": owner.car_id, "size": owner.size}),
        },
    )
    .await?;
    w.finish::<()>(StatusCode::NO_CONTENT, None).await
}

/// `POST /core/v1/admin/accounts/{accountId}/liveries {action: "hide" | "delete", reason}` (MOD,
/// ADM) → 200 [`LiveryBulkApplied`]: every livery of the account's profiles hidden (their bytes
/// too) or deleted (with the drafts). Unknown account: 404 `PORTAL.NOT_FOUND`; one's own: 403
/// `AUTH.FORBIDDEN`; another action: 400 `REQUEST.INVALID {field: "action"}`. Audit
/// `livery.hide_all` / `livery.delete_all`.
pub async fn bulk_liveries(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(account_id): Path<String>,
    Body(req): Body<LiveryBulk>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let account_id = account_param(&account_id)?;
    not_self(&session, account_id)?;
    let action = match req.action.as_str() {
        "hide" => liveries::Bulk::Hide,
        "delete" => liveries::Bulk::Delete,
        _ => return Err(ApiError::invalid("action", "hide or delete")),
    };
    reason("reason", &req.reason)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "POST /core/v1/admin/accounts/{accountId}/liveries",
            &(account_id, &req)
        )
        .await?
    );
    known_account(&mut w.tx, account_id).await?;
    let affected = liveries::admin_bulk_in(&mut w.tx, account_id, action).await?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: if action == liveries::Bulk::Hide {
                "livery.hide_all"
            } else {
                "livery.delete_all"
            },
            account_id: Some(account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({"affected": affected}),
        },
    )
    .await?;
    let body = LiveryBulkApplied {
        account_id: account_id.to_string(),
        action: req.action.clone(),
        affected,
    };
    w.finish(StatusCode::OK, Some(&body)).await
}

/// `PUT /core/v1/admin/accounts/{accountId}/livery-uploads {blocked, reason}` (MOD, ADM) → 200
/// [`LiveryUploads`]: a blocked account's games keep no new livery (UpdateLivery acknowledged
/// and not stored, CreateVersionedLivery refused); its stored ones stay (hide or delete them).
/// Unknown account: 404 `PORTAL.NOT_FOUND`; one's own: 403 `AUTH.FORBIDDEN`. Audit
/// `livery.block` / `livery.unblock`.
pub async fn set_livery_uploads(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(account_id): Path<String>,
    Body(req): Body<LiveryUploadsUpdate>,
) -> Result<Response, ApiError> {
    session.require(Role::Moderator)?;
    let account_id = account_param(&account_id)?;
    not_self(&session, account_id)?;
    reason("reason", &req.reason)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "PUT /core/v1/admin/accounts/{accountId}/livery-uploads",
            &(account_id, &req)
        )
        .await?
    );
    known_account(&mut w.tx, account_id).await?;
    let before = liveries::set_upload_block_in(&mut w.tx, account_id, req.blocked).await?;
    let target = format!("account:{account_id}");
    w.audit(
        &session,
        AuditEntry {
            action: if req.blocked {
                "livery.block"
            } else {
                "livery.unblock"
            },
            account_id: Some(account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({"wasBlocked": before}),
        },
    )
    .await?;
    let body = LiveryUploads {
        account_id: account_id.to_string(),
        blocked: req.blocked,
    };
    w.finish(StatusCode::OK, Some(&body)).await
}

// Seed revoke (CR-S52) ------------------------------------------------------------------------

/// `POST /core/v1/admin/profiles/{profileId}/seed-revoke {reason}` (ADM; CR-S52, CORE_API.md §5)
/// → 200 [`SeedRevoked`]: the source of a Hardcore line seeded from another profile
/// (`seed_from_profile`) was found altered. The line is retired (deleted from the cloud: never
/// leased again, its bundles pruned by retention after 90 days) and the account is blocked pending
/// review (every refresh family, live play session and MCP connection revoked; it is answered
/// `ACCOUNTS.BANNED {review: "seed_from_profile"}` until an admin lifts the ban, which does not
/// bring the line back). Unknown profile: 404 `PORTAL.NOT_FOUND`; a line not seeded from a
/// profile: 409 `PORTAL.CONFLICT {reason: "not_seeded"}`; one's own: 403 `AUTH.FORBIDDEN`. Audit
/// `profile.seed_revoke`.
pub async fn seed_revoke(
    State(state): State<AppState>,
    session: CoreSession,
    key: IdempotencyKey,
    Path(profile_id): Path<String>,
    Body(req): Body<SeedRevoke>,
) -> Result<Response, ApiError> {
    session.require(Role::Admin)?;
    let profile_id =
        Uuid::parse_str(&profile_id).map_err(|_| ApiError::invalid("profileId", "a UUID"))?;
    reason("reason", &req.reason)?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT account_id FROM profile WHERE id = $1 AND status <> 'deleted'")
            .bind(profile_id)
            .fetch_optional(&state.pool)
            .await?;
    let not_found = || ApiError::new(ErrorCode::PortalNotFound, "not found");
    not_self(&session, owner.ok_or_else(not_found)?)?;
    let mut w = fresh_or_replay!(
        begin(
            &state,
            &session,
            &key,
            "POST /core/v1/admin/profiles/{profileId}/seed-revoke",
            &(profile_id, &req)
        )
        .await?
    );
    let revoked = fi_db::seed_profile::revoke_in(&mut w.tx, profile_id, &req.reason)
        .await
        .map_err(|e| match e {
            fi_db::seed_profile::RevokeError::NotFound => not_found(),
            fi_db::seed_profile::RevokeError::NotSeeded => ApiError::new(
                ErrorCode::PortalConflict,
                "the line was not seeded from a profile",
            )
            .with_details(json!({"reason": "not_seeded"})),
            fi_db::seed_profile::RevokeError::Db(e) => e.into(),
        })?;
    let target = format!("profile:{profile_id}");
    w.audit(
        &session,
        AuditEntry {
            action: "profile.seed_revoke",
            account_id: Some(revoked.account_id),
            target: &target,
            reason: Some(&req.reason),
            detail: json!({
                "sourceProfileId": revoked.source,
                "tokensRevoked": revoked.banned.tokens_revoked,
                "sessionsRevoked": revoked.banned.sessions_revoked,
                "connectionsRevoked": revoked.banned.connections_revoked,
            }),
        },
    )
    .await?;
    let body = SeedRevoked {
        profile_id: profile_id.to_string(),
        account_id: revoked.account_id.to_string(),
        source_profile_id: revoked.source.to_string(),
        revoked_at: views::at(revoked.retired_at),
    };
    w.finish(StatusCode::OK, Some(&body)).await
}
