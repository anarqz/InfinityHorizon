//! Moderation and the social hub's writes (CORE_API.md §4-§5): the `admin_action` audit of account
//! actors, strikes, bans, roles, reports with the three-report rule (ADR 0204 §3) from the web and
//! from the game ([`game_report_in`], OD-63; OD-193: any player card, a named world shared in the
//! last 30 minutes), comments, and
//! the moderation view of an account.
//!
//! Every admin write runs in one transaction with its `admin_action` row ([`audit_in`]), so an
//! action without its audit row (or the reverse) never commits. Lock order: the account row first
//! (`accounts::lock_account_in`) when the write touches an account, then its rows.

use crate::accounts;
use crate::identity::{IdentityError, ms, ts};
use crate::limits::Limit;
use fi_domain::UnixMs;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Rows of one audit, report or comment page.
pub const PAGE_LIMIT: i64 = 50;
/// Distinct open reports that hide a target pending review.
pub const HIDE_AFTER_REPORTS: i64 = 3;

/// `player` < `moderator` < `admin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Everyone.
    Player,
    /// MOD: strikes, reviews, reports, the moderation view.
    Moderator,
    /// ADM: everything, bans and roles included.
    Admin,
}

impl Role {
    /// The column value.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "player" => Some(Role::Player),
            "moderator" => Some(Role::Moderator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    /// The column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Player => "player",
            Role::Moderator => "moderator",
            Role::Admin => "admin",
        }
    }
}

/// Who acts in an admin write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    /// The moderator's or admin's account.
    pub account_id: Uuid,
    /// Their role at this request (`moderator` | `admin`).
    pub role: Role,
    /// The request's `X-Request-Id`.
    pub request_id: Option<String>,
}

/// One audit row to append.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditEntry<'a> {
    /// `strike.create`, `account.ban`, ... (dotted lower-case).
    pub action: &'a str,
    /// The account the action is about, when there is one.
    pub account_id: Option<Uuid>,
    /// `account:<id>`, `strike:<id>`, `notice:<id>`, `game:<code>`, `report:<id>`, `comment:<id>`.
    pub target: &'a str,
    /// The moderator's reason or note.
    pub reason: Option<&'a str>,
    /// Ids and enum values only (never free text beyond `reason`, never e-mail).
    pub detail: serde_json::Value,
}

/// Appends an `admin_action` row (actor kind `account`) in the caller's transaction.
pub async fn audit_in(
    conn: &mut PgConnection,
    actor: &Actor,
    e: &AuditEntry<'_>,
) -> Result<Uuid, IdentityError> {
    Ok(sqlx::query_scalar(
        "INSERT INTO admin_action (actor, action, account_id, reason, detail, actor_kind, actor_account_id, actor_role, target, request_id) \
         VALUES ('account:' || $1::text, $2, $3, $4, $5, 'account', $1, $6, $7, $8) RETURNING id",
    )
    .bind(actor.account_id)
    .bind(e.action)
    .bind(e.account_id)
    .bind(e.reason)
    .bind(&e.detail)
    .bind(actor.role.as_str())
    .bind(e.target)
    .bind(actor.request_id.as_deref())
    .fetch_one(&mut *conn)
    .await?)
}

/// An audit row as the feed shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    /// Id.
    pub id: Uuid,
    /// When.
    pub at: UnixMs,
    /// The acting account (`None` for an operator row of fi-billing's CLI).
    pub actor_account_id: Option<Uuid>,
    /// `moderator` | `admin` | `operator`.
    pub actor_role: String,
    /// Action.
    pub action: String,
    /// Target (`account:<id>` for operator rows about an account).
    pub target: Option<String>,
    /// Reason or note.
    pub reason: Option<String>,
    /// Request id.
    pub request_id: Option<String>,
}

/// A filter of the audit feed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// Rows strictly older than this id (uuidv7, time-ordered).
    pub before: Option<Uuid>,
    /// Only this actor.
    pub actor: Option<Uuid>,
    /// Only this target (`account:<id>` also matches operator rows about that account).
    pub target: Option<String>,
    /// Only rows about this account.
    pub account_id: Option<Uuid>,
}

/// The audit feed, newest first, at most `limit` rows.
pub async fn audit_feed_in(
    conn: &mut PgConnection,
    f: &AuditFilter,
    limit: i64,
) -> Result<Vec<AuditRow>, IdentityError> {
    let rows = sqlx::query(
        "SELECT id, at, actor_account_id, COALESCE(actor_role, 'operator') AS actor_role, action, \
           COALESCE(target, CASE WHEN account_id IS NOT NULL THEN 'account:' || account_id::text END) AS target, \
           reason, request_id \
         FROM admin_action \
         WHERE ($1::uuid IS NULL OR id < $1) \
           AND ($2::uuid IS NULL OR actor_account_id = $2) \
           AND ($3::text IS NULL OR target = $3 OR ('account:' || account_id::text) = $3) \
           AND ($4::uuid IS NULL OR account_id = $4) \
         ORDER BY id DESC LIMIT $5",
    )
    .bind(f.before)
    .bind(f.actor)
    .bind(f.target.as_deref())
    .bind(f.account_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(AuditRow {
                id: r.try_get("id")?,
                at: ms(r.try_get("at")?),
                actor_account_id: r.try_get("actor_account_id")?,
                actor_role: r.try_get("actor_role")?,
                action: r.try_get("action")?,
                target: r.try_get("target")?,
                reason: r.try_get("reason")?,
                request_id: r.try_get("request_id")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// A strike (`integrity_strike`) with its profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StrikeRow {
    /// Id.
    pub id: Uuid,
    /// Game.
    pub game: String,
    /// The profile's XUID.
    pub xuid: u64,
    /// The profile's account.
    pub account_id: Uuid,
    /// `save_gate` | `profile_gate` | `race_time` | `top_speed` | `manual`.
    pub source: String,
    /// Reason.
    pub reason: String,
    /// Created.
    pub created_at: UnixMs,
    /// The review, when there was one: `(at, outcome)`.
    pub reviewed: Option<(UnixMs, String)>,
}

const STRIKE_SQL: &str = "SELECT s.id, s.game, p.xuid, p.account_id, s.source, s.reason, s.created_at, s.reviewed_at, s.outcome \
     FROM integrity_strike s JOIN profile p ON p.id = s.profile_id";

fn strike_from_row(r: &PgRow) -> Result<StrikeRow, sqlx::Error> {
    let reviewed_at: Option<OffsetDateTime> = r.try_get("reviewed_at")?;
    let outcome: Option<String> = r.try_get("outcome")?;
    Ok(StrikeRow {
        id: r.try_get("id")?,
        game: r.try_get("game")?,
        xuid: r.try_get::<i64, _>("xuid")? as u64,
        account_id: r.try_get("account_id")?,
        source: r.try_get("source")?,
        reason: r.try_get("reason")?,
        created_at: ms(r.try_get("created_at")?),
        reviewed: reviewed_at.zip(outcome).map(|(at, o)| (ms(at), o)),
    })
}

/// The strikes of every profile of an account (optionally of one game), newest first.
pub async fn strikes_of_account_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    game: Option<&str>,
) -> Result<Vec<StrikeRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{STRIKE_SQL} WHERE p.account_id = $1 AND ($2::text IS NULL OR s.game = $2) \
         ORDER BY s.created_at DESC, s.id DESC LIMIT 200"
    )))
    .bind(account_id)
    .bind(game)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(strike_from_row).collect::<Result<_, _>>()?)
}

/// One strike.
pub async fn strike_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<StrikeRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!("{STRIKE_SQL} WHERE s.id = $1")))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.as_ref().map(strike_from_row).transpose()?)
}

/// A manual strike on the live profile `xuid` by `by`. `NotFound` for an unknown or deleted
/// profile.
pub async fn create_strike_in(
    conn: &mut PgConnection,
    xuid: u64,
    reason: &str,
    by: Uuid,
) -> Result<StrikeRow, IdentityError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO integrity_strike (profile_id, source, reason, created_by) \
         SELECT id, 'manual', $2, $3 FROM profile WHERE xuid = $1 AND status <> 'deleted' \
         ORDER BY created_at, id LIMIT 1 RETURNING id",
    )
    .bind(xuid as i64)
    .bind(reason)
    .bind(by)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(IdentityError::NotFound)?;
    strike_in(conn, id).await?.ok_or(IdentityError::NotFound)
}

/// Reviews a strike (`upheld` | `overturned`); a later review replaces an earlier one (an appeal).
/// `NotFound` for an unknown strike.
pub async fn review_strike_in(
    conn: &mut PgConnection,
    id: Uuid,
    outcome: &str,
    by: Uuid,
) -> Result<StrikeRow, IdentityError> {
    let done = sqlx::query(
        "UPDATE integrity_strike SET reviewed_at = clock_timestamp(), reviewed_by = 'account:' || $3::text, outcome = $2 \
         WHERE id = $1",
    )
    .bind(id)
    .bind(outcome)
    .bind(by)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 0 {
        return Err(IdentityError::NotFound);
    }
    strike_in(conn, id).await?.ok_or(IdentityError::NotFound)
}

/// What a ban revoked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Banned {
    /// Refresh tokens (launcher families and portal sessions) revoked.
    pub tokens_revoked: u64,
    /// Live play sessions marked `revoked` (`game_until = now`).
    pub sessions_revoked: u64,
    /// MCP connections revoked.
    pub connections_revoked: u64,
}

/// Bans the account (`until = None`: permanent) and revokes everything that lets it in: every
/// refresh token (launcher families and portal sessions), every live play session (`revoked`,
/// `game_until = now`, with its session summary), every MCP connection. A ban of a banned account
/// replaces its end and reason. `NotFound` for an unknown or deleted account.
pub async fn ban_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    until: Option<UnixMs>,
    reason: &str,
) -> Result<Banned, IdentityError> {
    ban_with_review_in(conn, account_id, until, reason, None).await
}

/// [`ban_in`] that is a block pending review the player is told about (`account.ban_review`,
/// answered as `ACCOUNTS.BANNED {review}`; CR-S52's `seed_from_profile`). `None`: an ordinary ban
/// (which replaces a review block's reason).
pub async fn ban_with_review_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    until: Option<UnixMs>,
    reason: &str,
    review: Option<&str>,
) -> Result<Banned, IdentityError> {
    let state = accounts::lock_account_in(conn, account_id).await?;
    if state.status == "deleted" {
        return Err(IdentityError::NotFound);
    }
    // A review block on an account already banned keeps the ban in force's reason (review L3):
    // the new one is appended, and the ban stays from its first instant.
    sqlx::query(
        "UPDATE account SET \
           banned_at = CASE WHEN $4::text IS NOT NULL AND banned_at IS NOT NULL \
             AND (banned_until IS NULL OR banned_until > clock_timestamp()) \
             THEN banned_at ELSE clock_timestamp() END, \
           ban_reason = CASE WHEN $4::text IS NOT NULL AND banned_at IS NOT NULL \
             AND (banned_until IS NULL OR banned_until > clock_timestamp()) AND ban_reason IS NOT NULL \
             AND ban_reason <> $3 THEN left(ban_reason || ' | ' || $3, 500) ELSE $3 END, \
           banned_until = $2, ban_review = $4 WHERE id = $1",
    )
    .bind(account_id)
    .bind(until.map(ts))
    .bind(reason)
    .bind(review)
    .execute(&mut *conn)
    .await?;
    let tokens = sqlx::query(
        "UPDATE refresh_token SET revoked_at = clock_timestamp(), revoke_reason = 'admin' \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let sessions: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE play_session s SET state = 'revoked', ended_at = clock_timestamp(), end_reason = 'revoked', \
           game_until = CASE WHEN s.max_until IS NULL THEN clock_timestamp() ELSE LEAST(clock_timestamp(), s.max_until) END, \
           end_revision = (SELECT h.revision FROM save_head h WHERE h.profile_id = s.profile_id) \
         WHERE s.account_id = $1 AND s.state IN ('queued', 'active') RETURNING s.id",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    for session in &sessions {
        session_closed_in(conn, *session).await?;
        // The worlds lane (MATCHMAKING_API.md §3.3): its worlds close and its seats end.
        crate::matchmaking::end_session_in(conn, *session).await?;
    }
    let connections = revoke_mcp_connections_in(conn, account_id, "banned").await?;
    accounts::audit_as_in(conn, account_id, "account.banned", "moderator").await?;
    Ok(Banned {
        tokens_revoked: tokens,
        sessions_revoked: sessions.len() as u64,
        connections_revoked: connections,
    })
}

/// The longest re-admission cooldown of a kick, in minutes (CORE_API.md §5).
pub const KICK_COOLDOWN_MAX_MINUTES: u32 = 60;

/// What a kick did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kicked {
    /// The kicked account's role before the kick (`player` | `moderator` | `admin`).
    pub role: Role,
    /// Live play sessions (queued or active) ended `revoked` / `kicked`.
    pub sessions_ended: u64,
    /// Admission is refused until then (`SESSIONS.KICKED`); `None` without a cooldown in force.
    pub until: Option<UnixMs>,
}

/// The account's state for a kick, locked (`FOR UPDATE`): its role, so the caller can refuse a
/// moderator kicking a moderator or an admin before [`kick_in`]. `NotFound` for an unknown or
/// deleted account.
pub async fn kick_target_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Role, IdentityError> {
    let state = accounts::lock_account_in(conn, account_id).await?;
    if state.status == "deleted" {
        return Err(IdentityError::NotFound);
    }
    Ok(Role::parse(&state.role).unwrap_or(Role::Player))
}

/// Kicks the account (CORE_API.md §5, register CR-S62): every live play session (queued or
/// active) ends `revoked` with `end_reason = 'kicked'` (`game_until = now`, its read models, its
/// seats ended and the worlds it hosts closed), exactly as a ban's revoke does, but nothing else:
/// no ban, refresh families, portal sessions and MCP connections untouched. `kicked_at` is set (the
/// player's `kicked` notice; a lapsed session created before it is not resumed by a heartbeat).
/// With `cooldown_minutes > 0` admission is refused until `now + cooldown` (`SESSIONS.KICKED`); a
/// longer cooldown already in force is kept, and a kick without a cooldown never shortens one.
/// `NotFound` for an unknown or deleted account.
pub async fn kick_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    cooldown_minutes: u32,
) -> Result<Kicked, IdentityError> {
    let role = kick_target_in(conn, account_id).await?;
    let sessions: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE play_session s SET state = 'revoked', ended_at = clock_timestamp(), end_reason = 'kicked', \
           game_until = CASE WHEN s.max_until IS NULL THEN clock_timestamp() ELSE LEAST(clock_timestamp(), s.max_until) END, \
           end_revision = (SELECT h.revision FROM save_head h WHERE h.profile_id = s.profile_id) \
         WHERE s.account_id = $1 AND s.state IN ('queued', 'active') RETURNING s.id",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    for session in &sessions {
        session_closed_in(conn, *session).await?;
        crate::matchmaking::end_session_in(conn, *session).await?;
    }
    let until: Option<OffsetDateTime> = sqlx::query_scalar(
        "UPDATE account SET kicked_at = clock_timestamp(), \
           kicked_until = CASE WHEN $2::int > 0 \
             THEN GREATEST(kicked_until, clock_timestamp() + make_interval(mins => $2::int)) \
             ELSE kicked_until END \
         WHERE id = $1 \
         RETURNING CASE WHEN kicked_until > clock_timestamp() THEN kicked_until END",
    )
    .bind(account_id)
    .bind(cooldown_minutes.min(KICK_COOLDOWN_MAX_MINUTES) as i32)
    .fetch_one(&mut *conn)
    .await?;
    accounts::audit_as_in(conn, account_id, "account.kicked", "moderator").await?;
    Ok(Kicked {
        role,
        sessions_ended: sessions.len() as u64,
        until: until.map(ms),
    })
}

/// The account's latest kick and the cooldown in force (`None` until when it has run out), for the
/// player's `kicked` notice. `None` when it was never kicked.
pub async fn last_kick_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<(UnixMs, Option<UnixMs>)>, IdentityError> {
    let row: Option<(Option<OffsetDateTime>, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT kicked_at, CASE WHEN kicked_until > clock_timestamp() THEN kicked_until END \
         FROM account WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.and_then(|(at, until)| at.map(|at| (ms(at), until.map(ms)))))
}

/// Lifts a ban; `false` when the account was not banned. `NotFound` for an unknown account.
pub async fn lift_ban_in(conn: &mut PgConnection, account_id: Uuid) -> Result<bool, IdentityError> {
    accounts::lock_account_in(conn, account_id).await?;
    let done = sqlx::query(
        "UPDATE account SET banned_at = NULL, banned_until = NULL, ban_reason = NULL, \
           ban_review = NULL WHERE id = $1 AND banned_at IS NOT NULL",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 1 {
        accounts::audit_as_in(conn, account_id, "account.unbanned", "moderator").await?;
    }
    Ok(done.rows_affected() == 1)
}

/// Revokes the account's live MCP connections (and their refresh tokens' use).
pub async fn revoke_mcp_connections_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    reason: &str,
) -> Result<u64, IdentityError> {
    Ok(sqlx::query(
        "UPDATE mcp_connection SET revoked_at = clock_timestamp(), revoke_reason = $2 \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .bind(reason)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Sets an account's role; returns the previous one. A change revokes the account's MCP
/// connections (they are bound to the role at consent, ADR 0018 §2). `NotFound` for an unknown or
/// deleted account.
pub async fn set_role_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    role: Role,
) -> Result<Role, IdentityError> {
    let state = accounts::lock_account_in(conn, account_id).await?;
    if state.status == "deleted" {
        return Err(IdentityError::NotFound);
    }
    let previous = Role::parse(&state.role).unwrap_or(Role::Player);
    if previous != role {
        sqlx::query("UPDATE account SET role = $2 WHERE id = $1")
            .bind(account_id)
            .bind(role.as_str())
            .execute(&mut *conn)
            .await?;
        revoke_mcp_connections_in(conn, account_id, "role_changed").await?;
        accounts::audit_as_in(conn, account_id, "account.role_changed", "moderator").await?;
    }
    Ok(previous)
}

/// The read models of a session the ban just closed (`fi_db::progression::session_ended_guarded`, as
/// `admission`'s closes; no outbox, owner 2026-10-26).
async fn session_closed_in(conn: &mut PgConnection, session: Uuid) -> Result<(), IdentityError> {
    let r = sqlx::query(
        "SELECT profile_id, end_revision FROM play_session WHERE id = $1 AND ended_at IS NOT NULL",
    )
    .bind(session)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(r) = r {
        let profile: Uuid = r.try_get("profile_id")?;
        let end: Option<i64> = r.try_get("end_revision")?;
        crate::progression::session_ended_guarded(conn, session, profile, end).await?;
    }
    Ok(())
}

/// A report to file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewReport<'a> {
    /// The reporting account.
    pub reporter: Uuid,
    /// `player` | `comment`.
    pub target_kind: &'a str,
    /// XUID (decimal) or comment id.
    pub target_id: &'a str,
    /// The account behind the target.
    pub target_account: Option<Uuid>,
    /// `cheating` | `abuse` | `other`.
    pub reason: &'a str,
    /// Free text.
    pub text: Option<&'a str>,
    /// `web` (`POST /core/v1/social/reports`) | `game` (`POST /launcher/v1/reports`).
    pub source: &'a str,
    /// A report from the game: the world the two shared, when the overlay named it.
    pub world_id: Option<Uuid>,
}

/// What filing a report did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reported {
    /// The report (the reporter's open report on that target when one existed: it counts once).
    pub id: Uuid,
    /// The target is now hidden pending review (three distinct open reports).
    pub hidden: bool,
}

/// Files a report and applies the three-report rule: at [`HIDE_AFTER_REPORTS`] distinct open
/// reporters the target (a player's card or a comment) is hidden until a moderator decides.
pub async fn report_in(
    conn: &mut PgConnection,
    r: &NewReport<'_>,
) -> Result<Reported, IdentityError> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO report (reporter_account_id, target_kind, target_id, target_account_id, reason, text, source, world_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (reporter_account_id, target_kind, target_id) WHERE state = 'open' DO NOTHING RETURNING id",
    )
    .bind(r.reporter)
    .bind(r.target_kind)
    .bind(r.target_id)
    .bind(r.target_account)
    .bind(r.reason)
    .bind(r.text)
    .bind(r.source)
    .bind(r.world_id)
    .fetch_optional(&mut *conn)
    .await?;
    let id = match inserted {
        Some(id) => id,
        None => {
            sqlx::query_scalar(
                "SELECT id FROM report WHERE reporter_account_id = $1 AND target_kind = $2 AND target_id = $3 AND state = 'open'",
            )
            .bind(r.reporter)
            .bind(r.target_kind)
            .bind(r.target_id)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    let open: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT reporter_account_id) FROM report WHERE target_kind = $1 AND target_id = $2 AND state = 'open'",
    )
    .bind(r.target_kind)
    .bind(r.target_id)
    .fetch_one(&mut *conn)
    .await?;
    let hidden = open >= HIDE_AFTER_REPORTS;
    if hidden {
        set_target_hidden_in(conn, r.target_kind, r.target_id, true).await?;
    }
    Ok(Reported { id, hidden })
}

/// Reports from the game per account (CORE_API.md §4.3): 10 an hour, counted for every request
/// past the field checks, keyed by the HMAC of `account:<id>`.
pub const GAME_REPORTS: Limit = Limit {
    name: "report.game",
    max: 10,
    window_s: 3_600,
};
/// A report from the game that names a world needs it shared within this many seconds (30
/// minutes).
pub const SHARED_WITHIN_SECS: i64 = 30 * 60;
/// One report per reporter account and target profile within this many seconds (24 hours).
pub const REPEAT_WITHIN_SECS: i64 = 24 * 3_600;

/// A report from the game (`POST /launcher/v1/reports`; OD-63).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameReport<'a> {
    /// The reporting account (the launcher session's).
    pub reporter: Uuid,
    /// The game.
    pub game: &'a str,
    /// The target profile's XUID.
    pub xuid: u64,
    /// `cheating` | `abuse` | `other`.
    pub reason: &'a str,
    /// The world the overlay names: it must be one the two shared. `None` (the game's own player
    /// menus, any card, OD-193): no world is checked.
    pub world_id: Option<Uuid>,
}

/// What a report from the game did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameReported {
    /// No profile of the game with that XUID (or a deleted one): `PORTAL.NOT_FOUND`.
    Unknown,
    /// A profile of the reporter's own account: `REQUEST.INVALID {field: "xuid"}`.
    Own,
    /// The reporter reported that profile within [`REPEAT_WITHIN_SECS`], or holds an open report
    /// on it: that report, nothing written.
    Repeat(Uuid),
    /// A world was named and the two did not share it within [`SHARED_WITHIN_SECS`]:
    /// `PORTAL.NOT_FOUND`, nothing written.
    NotShared,
    /// Filed (`source = game`) under the three-report rule.
    Filed(Reported),
}

/// The SQL of the shared-world check (tests EXPLAIN it): `$1` the game, `$2` the target profile,
/// `$3` the reporter account, `$4` the named world (or NULL), `$5` the window (seconds). `t` is
/// the target's spans in the game's worlds that reach into the window (its seats of any state,
/// from issue to end, and the worlds it hosted, from opening to close: `mm_grant_one_live`,
/// `mm_grant_profile_ended`, `world_one_open_per_host`, `world_host_closed`); `r` the reporter
/// account's spans in those worlds (`mm_grant_world_account`, the world row); an unended seat or
/// world ends now, or at its expiry when it lapsed unswept; every span is bounded by its world's
/// life. Shared: two spans of one world overlap and the overlap reaches into the window.
pub const SHARED_WORLD_SQL: &str = "WITH t AS ( \
       SELECT g.world_id, g.issued_at AS since, \
              CASE WHEN g.state = 'issued' THEN least(g.expires_at, clock_timestamp()) ELSE clock_timestamp() END AS until \
         FROM mm_grant g WHERE g.game = $1 AND g.profile_id = $2 AND g.state IN ('issued', 'used') \
       UNION ALL \
       SELECT g.world_id, g.issued_at, g.ended_at FROM mm_grant g \
         WHERE g.game = $1 AND g.profile_id = $2 AND g.ended_at IS NOT NULL \
           AND g.ended_at > clock_timestamp() - make_interval(secs => $5) \
       UNION ALL \
       SELECT w.id, w.opened_at, least(w.expires_at, clock_timestamp()) FROM world w \
         WHERE w.game = $1 AND w.host_profile_id = $2 AND w.closed_at IS NULL \
       UNION ALL \
       SELECT w.id, w.opened_at, w.closed_at FROM world w \
         WHERE w.game = $1 AND w.host_profile_id = $2 AND w.closed_at IS NOT NULL \
           AND w.closed_at > clock_timestamp() - make_interval(secs => $5)), \
     r AS ( \
       SELECT g.world_id, g.issued_at AS since, \
              coalesce(g.ended_at, CASE WHEN g.state = 'issued' THEN least(g.expires_at, clock_timestamp()) \
                ELSE clock_timestamp() END) AS until \
         FROM mm_grant g WHERE g.world_id IN (SELECT world_id FROM t) AND g.account_id = $3 \
       UNION ALL \
       SELECT w.id, w.opened_at, coalesce(w.closed_at, least(w.expires_at, clock_timestamp())) FROM world w \
         WHERE w.id IN (SELECT world_id FROM t) AND w.host_account_id = $3) \
     SELECT EXISTS (SELECT 1 FROM t JOIN r ON r.world_id = t.world_id JOIN world w ON w.id = t.world_id \
       WHERE w.game = $1 AND ($4::uuid IS NULL OR w.id = $4) \
         AND greatest(t.since, r.since) <= least(t.until, r.until, coalesce(w.closed_at, w.expires_at)) \
         AND least(t.until, r.until, coalesce(w.closed_at, w.expires_at)) \
           > clock_timestamp() - make_interval(secs => $5))";

/// Files a report from the game (CORE_API.md §4.3), the first that applies: an unknown target;
/// one's own account's profile; with a world named, a target the reporter did not share it with
/// within [`SHARED_WITHIN_SECS`] (without one, OD-193: any player the game shows); a repeat (the reporter's newest report on
/// that profile within [`REPEAT_WITHIN_SECS`], from the game or the web, or its open one); else
/// the report, `source = game`, through [`report_in`] (one open report per reporter and target,
/// the three-report rule unchanged). Every 202 is an eligible report. The target is stored as its
/// canonical XUID, as the web's reports are. Run in the caller's transaction.
pub async fn game_report_in(
    conn: &mut PgConnection,
    r: &GameReport<'_>,
) -> Result<GameReported, IdentityError> {
    let Ok(xuid) = i64::try_from(r.xuid) else {
        return Ok(GameReported::Unknown);
    };
    let target: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, account_id FROM profile WHERE game = $1 AND xuid = $2 AND status <> 'deleted'",
    )
    .bind(r.game)
    .bind(xuid)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((profile, account)) = target else {
        return Ok(GameReported::Unknown);
    };
    if account == r.reporter {
        return Ok(GameReported::Own);
    }
    if let Some(world) = r.world_id {
        let shared: bool = sqlx::query_scalar(SHARED_WORLD_SQL)
            .bind(r.game)
            .bind(profile)
            .bind(r.reporter)
            .bind(world)
            .bind(SHARED_WITHIN_SECS as f64)
            .fetch_one(&mut *conn)
            .await?;
        if !shared {
            return Ok(GameReported::NotShared);
        }
    }
    let target_id = r.xuid.to_string();
    let earlier: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM report WHERE reporter_account_id = $1 AND target_kind = 'player' AND target_id = $2 \
           AND (state = 'open' OR created_at > clock_timestamp() - make_interval(secs => $3)) \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(r.reporter)
    .bind(&target_id)
    .bind(REPEAT_WITHIN_SECS as f64)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = earlier {
        return Ok(GameReported::Repeat(id));
    }
    let filed = report_in(
        conn,
        &NewReport {
            reporter: r.reporter,
            target_kind: "player",
            target_id: &target_id,
            target_account: Some(account),
            reason: r.reason,
            text: None,
            source: "game",
            world_id: r.world_id,
        },
    )
    .await?;
    Ok(GameReported::Filed(filed))
}

async fn set_target_hidden_in(
    conn: &mut PgConnection,
    kind: &str,
    id: &str,
    hidden: bool,
) -> Result<(), IdentityError> {
    match kind {
        "player" => {
            let Ok(xuid) = id.parse::<i64>() else {
                return Ok(());
            };
            sqlx::query(
                "UPDATE profile SET social_hidden_at = CASE WHEN $2 THEN COALESCE(social_hidden_at, clock_timestamp()) END \
                 WHERE xuid = $1",
            )
            .bind(xuid)
            .bind(hidden)
            .execute(&mut *conn)
            .await?;
        }
        "comment" => {
            let Ok(comment) = Uuid::parse_str(id) else {
                return Ok(());
            };
            sqlx::query(
                "UPDATE social_comment SET hidden_at = CASE WHEN $2 THEN COALESCE(hidden_at, clock_timestamp()) END \
                 WHERE id = $1",
            )
            .bind(comment)
            .bind(hidden)
            .execute(&mut *conn)
            .await?;
        }
        _ => {}
    }
    Ok(())
}

/// A report as the moderation queue shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportRow {
    /// Id.
    pub id: Uuid,
    /// Reporter.
    pub reporter_account_id: Uuid,
    /// `player` | `comment`.
    pub target_kind: String,
    /// Target id.
    pub target_id: String,
    /// The account behind the target.
    pub target_account_id: Option<Uuid>,
    /// Reason.
    pub reason: String,
    /// Free text.
    pub text: Option<String>,
    /// `open` | `dismissed` | `actioned`.
    pub state: String,
    /// Filed.
    pub created_at: UnixMs,
    /// The decision: `(at, decision, note)`.
    pub decided: Option<(UnixMs, String, Option<String>)>,
    /// `web` | `game`.
    pub source: String,
    /// A report from the game: the world the two shared, when named.
    pub world_id: Option<Uuid>,
}

const REPORT_COLUMNS: &str = "id, reporter_account_id, target_kind, target_id, target_account_id, reason, text, state, \
     created_at, decided_at, decision, note, source, world_id";

fn report_from_row(r: &PgRow) -> Result<ReportRow, sqlx::Error> {
    let decided_at: Option<OffsetDateTime> = r.try_get("decided_at")?;
    let decision: Option<String> = r.try_get("decision")?;
    let note: Option<String> = r.try_get("note")?;
    Ok(ReportRow {
        id: r.try_get("id")?,
        reporter_account_id: r.try_get("reporter_account_id")?,
        target_kind: r.try_get("target_kind")?,
        target_id: r.try_get("target_id")?,
        target_account_id: r.try_get("target_account_id")?,
        reason: r.try_get("reason")?,
        text: r.try_get("text")?,
        state: r.try_get("state")?,
        created_at: ms(r.try_get("created_at")?),
        decided: decided_at.zip(decision).map(|(at, d)| (ms(at), d, note)),
        source: r.try_get("source")?,
        world_id: r.try_get("world_id")?,
    })
}

/// Reports in `state` (`open` | `dismissed` | `actioned`), oldest first for `open` (a queue),
/// newest first otherwise, after the cursor id (uuidv7).
pub async fn reports_in(
    conn: &mut PgConnection,
    state: &str,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<ReportRow>, IdentityError> {
    let sql = if state == "open" {
        format!(
            "SELECT {REPORT_COLUMNS} FROM report WHERE state = $1 \
             AND ($2::uuid IS NULL OR id > $2) ORDER BY id LIMIT $3"
        )
    } else {
        format!(
            "SELECT {REPORT_COLUMNS} FROM report WHERE state = $1 \
             AND ($2::uuid IS NULL OR id < $2) ORDER BY id DESC LIMIT $3"
        )
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(state)
        .bind(after)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.iter().map(report_from_row).collect::<Result<_, _>>()?)
}

/// One report, locked (`FOR UPDATE`).
pub async fn lock_report_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<ReportRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {REPORT_COLUMNS} FROM report WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(report_from_row).transpose()?)
}

/// Closes every open report on the target of `report` with `decision` (`dismiss` → `dismissed`
/// and the target shown again; `hide`, `strike`, `ban` → `actioned`, the target stays hidden for
/// `hide`); the caller applies a strike or a ban itself. Returns the reports closed.
pub async fn decide_reports_in(
    conn: &mut PgConnection,
    report: &ReportRow,
    decision: &str,
    note: Option<&str>,
    by: Uuid,
) -> Result<u64, IdentityError> {
    let state = if decision == "dismiss" {
        "dismissed"
    } else {
        "actioned"
    };
    let closed = sqlx::query(
        "UPDATE report SET state = $3, decided_at = clock_timestamp(), decided_by = $4, decision = $5, note = $6 \
         WHERE target_kind = $1 AND target_id = $2 AND state = 'open'",
    )
    .bind(&report.target_kind)
    .bind(&report.target_id)
    .bind(state)
    .bind(by)
    .bind(decision)
    .bind(note)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    match decision {
        "dismiss" => {
            set_target_hidden_in(conn, &report.target_kind, &report.target_id, false).await?
        }
        "hide" => set_target_hidden_in(conn, &report.target_kind, &report.target_id, true).await?,
        _ => {}
    }
    Ok(closed)
}

/// Reports an account filed and reports about it.
pub async fn report_counts_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(i64, i64), IdentityError> {
    let r = sqlx::query(
        "SELECT (SELECT count(*) FROM report WHERE reporter_account_id = $1) AS made, \
                (SELECT count(*) FROM report WHERE target_account_id = $1) AS received",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok((r.try_get("made")?, r.try_get("received")?))
}

/// A comment with its author's live profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentRow {
    /// Id.
    pub id: Uuid,
    /// Game.
    pub game: String,
    /// `player` | `club` | `news`.
    pub target_kind: String,
    /// Target id.
    pub target_id: String,
    /// The author's account.
    pub author_account_id: Uuid,
    /// The author's XUID.
    pub author_xuid: u64,
    /// The author's `Name#12345`.
    pub author_gamertag: String,
    /// Plain text.
    pub body: String,
    /// Posted.
    pub created_at: UnixMs,
}

const COMMENT_SQL: &str = "SELECT c.id, c.game, c.target_kind, c.target_id, c.author_account_id, p.xuid, p.name, c.body, c.created_at \
     FROM social_comment c JOIN profile p ON p.id = c.author_profile_id";

fn comment_from_row(r: &PgRow) -> Result<CommentRow, sqlx::Error> {
    let name: Option<String> = r.try_get("name")?;
    Ok(CommentRow {
        id: r.try_get("id")?,
        game: r.try_get("game")?,
        target_kind: r.try_get("target_kind")?,
        target_id: r.try_get("target_id")?,
        author_account_id: r.try_get("author_account_id")?,
        author_xuid: r.try_get::<i64, _>("xuid")? as u64,
        author_gamertag: name.unwrap_or_default(),
        body: r.try_get("body")?,
        created_at: ms(r.try_get("created_at")?),
    })
}

/// Visible comments on a target, newest first, strictly older than the cursor id (uuidv7).
pub async fn comments_in(
    conn: &mut PgConnection,
    target_kind: &str,
    target_id: &str,
    before: Option<Uuid>,
    limit: i64,
) -> Result<Vec<CommentRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{COMMENT_SQL} WHERE c.target_kind = $1 AND c.target_id = $2 AND c.deleted_at IS NULL AND c.hidden_at IS NULL \
           AND p.status <> 'deleted' AND ($3::uuid IS NULL OR c.id < $3) \
         ORDER BY c.id DESC LIMIT $4"
    )))
    .bind(target_kind)
    .bind(target_id)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(comment_from_row)
        .collect::<Result<_, _>>()?)
}

/// One comment, not deleted (hidden ones included, for its author and moderators).
pub async fn comment_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<CommentRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{COMMENT_SQL} WHERE c.id = $1 AND c.deleted_at IS NULL"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(comment_from_row).transpose()?)
}

/// A comment to post.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewComment<'a> {
    /// Game.
    pub game: &'a str,
    /// `player` | `club` | `news`.
    pub target_kind: &'a str,
    /// Target id.
    pub target_id: &'a str,
    /// Author account.
    pub author_account_id: Uuid,
    /// Author profile.
    pub author_profile_id: Uuid,
    /// Plain text.
    pub body: &'a str,
}

/// Posts a comment.
pub async fn create_comment_in(
    conn: &mut PgConnection,
    c: &NewComment<'_>,
) -> Result<CommentRow, IdentityError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO social_comment (game, target_kind, target_id, author_account_id, author_profile_id, body) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(c.game)
    .bind(c.target_kind)
    .bind(c.target_id)
    .bind(c.author_account_id)
    .bind(c.author_profile_id)
    .bind(c.body)
    .fetch_one(&mut *conn)
    .await?;
    comment_in(conn, id).await?.ok_or(IdentityError::NotFound)
}

/// Deletes (soft) a comment; `false` when it was deleted already or never existed.
pub async fn delete_comment_in(conn: &mut PgConnection, id: Uuid) -> Result<bool, IdentityError> {
    Ok(sqlx::query(
        "UPDATE social_comment SET deleted_at = clock_timestamp() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1)
}

/// Comments the account posted in the last minute (the per-account comment limit is the
/// in-process bucket; this is the exact floor across instances).
pub async fn recent_comments_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<i64, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM social_comment WHERE author_account_id = $1 AND created_at > clock_timestamp() - interval '1 minute'",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// The account part of the moderation view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountView {
    /// Lifecycle and role.
    pub state: accounts::AccountState,
    /// Created.
    pub created_at: UnixMs,
    /// A ban in force or a past one still recorded: `(banned_at, until, reason)`.
    pub ban: Option<(UnixMs, Option<UnixMs>, Option<String>)>,
    /// The identity's e-mail (lower case), for the ADM's masked view only.
    pub email: Option<String>,
}

/// The account's moderation facts. `None` for an unknown account.
pub async fn account_view_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<AccountView>, IdentityError> {
    let Some(state) = sqlx::query(
        "SELECT a.created_at, a.banned_at, a.banned_until, a.ban_reason, i.email_lower \
         FROM account a LEFT JOIN account_identity i ON i.account_id = a.id WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let banned_at: Option<OffsetDateTime> = state.try_get("banned_at")?;
    let until: Option<OffsetDateTime> = state.try_get("banned_until")?;
    let reason: Option<String> = state.try_get("ban_reason")?;
    let created_at = ms(state.try_get("created_at")?);
    let email: Option<String> = state.try_get("email_lower")?;
    let account = accounts::account_state_in(conn, account_id)
        .await?
        .ok_or(IdentityError::NotFound)?;
    Ok(Some(AccountView {
        state: account,
        created_at,
        ban: banned_at.map(|b| (ms(b), until.map(ms), reason)),
        email,
    }))
}

/// A play session as the moderation view lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    /// Id.
    pub id: Uuid,
    /// Game.
    pub game: String,
    /// Pool.
    pub pool: String,
    /// `queued` | `active` | `ended` | `expired` | `revoked`.
    pub state: String,
    /// Created.
    pub created_at: UnixMs,
    /// Ended.
    pub ended_at: Option<UnixMs>,
    /// End reason.
    pub end_reason: Option<String>,
}

/// The account's last `limit` play sessions, newest first.
pub async fn sessions_of_account_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    limit: i64,
) -> Result<Vec<SessionRow>, IdentityError> {
    let rows = sqlx::query(
        "SELECT s.id, p.game, s.pool, s.state, s.created_at, s.ended_at, s.end_reason \
         FROM play_session s JOIN profile p ON p.id = s.profile_id WHERE s.account_id = $1 \
         ORDER BY s.created_at DESC, s.id DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(SessionRow {
                id: r.try_get("id")?,
                game: r.try_get("game")?,
                pool: r.try_get("pool")?,
                state: r.try_get("state")?,
                created_at: ms(r.try_get("created_at")?),
                ended_at: r.try_get::<Option<OffsetDateTime>, _>("ended_at")?.map(ms),
                end_reason: r.try_get("end_reason")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// One row of an account lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupRow {
    /// Account.
    pub account_id: Uuid,
    /// `Name#12345`.
    pub gamertag: String,
    /// Game.
    pub game: String,
    /// XUID.
    pub xuid: u64,
}

/// How to find accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// A legacy `Name#12345` exactly (OD-120), or a name prefix (case-insensitive) without `#`.
    Gamertag(&'a str),
    /// A profile's XUID.
    Xuid(u64),
    /// The identity's e-mail, exactly (lower-cased by the caller).
    Email(&'a str),
}

/// Profiles matching `by` (deleted profiles excluded), at most 50.
pub async fn lookup_in(
    conn: &mut PgConnection,
    by: &Lookup<'_>,
) -> Result<Vec<LookupRow>, IdentityError> {
    let base = "SELECT p.account_id, p.name, p.game, p.xuid FROM profile p";
    let q = match by {
        Lookup::Gamertag(tag) => match tag.split_once('#') {
            Some((name, disc)) => {
                let Ok(disc) = disc.parse::<i32>() else {
                    return Ok(Vec::new());
                };
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "{base} WHERE lower(coalesce(p.legacy_name, p.name)) = lower($1) AND p.discriminator = $2 AND p.status <> 'deleted' ORDER BY p.game, p.xuid LIMIT 50"
                )))
                .bind(name.to_owned())
                .bind(disc)
            }
            None => sqlx::query(sqlx::AssertSqlSafe(format!(
                "{base} WHERE lower(p.name) LIKE lower($1) || '%' AND p.status <> 'deleted' ORDER BY lower(p.name), p.xuid LIMIT 50"
            )))
            .bind(tag.replace(['%', '_', '\\'], "")),
        },
        Lookup::Xuid(xuid) => sqlx::query(sqlx::AssertSqlSafe(format!(
            "{base} WHERE p.xuid = $1 AND p.status <> 'deleted'"
        )))
        .bind(*xuid as i64),
        Lookup::Email(email) => sqlx::query(sqlx::AssertSqlSafe(format!(
            "{base} JOIN account_identity i ON i.account_id = p.account_id WHERE i.email_lower = $1 AND p.status <> 'deleted' ORDER BY p.game, p.xuid LIMIT 50"
        )))
        .bind(email.to_owned()),
    };
    let rows = q.fetch_all(&mut *conn).await?;
    rows.iter()
        .map(|r| {
            let name: Option<String> = r.try_get("name")?;
            Ok(LookupRow {
                account_id: r.try_get("account_id")?,
                gamertag: name.unwrap_or_default(),
                game: r.try_get("game")?,
                xuid: r.try_get::<i64, _>("xuid")? as u64,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}
