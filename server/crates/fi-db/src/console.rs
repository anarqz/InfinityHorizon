//! Reads and writes of the local admin console (apps/admin-console), run as fi_maintenance.

use crate::admission::Stale;
use crate::moderation::{self, Actor, AuditEntry, Role};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// An account allowed into the console.
#[derive(Clone, Debug)]
pub struct Staff {
    /// Account.
    pub account_id: Uuid,
    /// `admin` only, today.
    pub role: String,
}

/// The active account of an Identity Platform uid, with its role.
pub async fn staff_by_uid(db: &PgPool, idp_uid: &str) -> Result<Option<Staff>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT a.id, a.role FROM account_identity i JOIN account a ON a.id = i.account_id \
         WHERE i.idp_uid = $1 AND a.status = 'active' AND a.banned_at IS NULL",
    )
    .bind(idp_uid)
    .fetch_optional(db)
    .await?;
    row.map(|r| {
        Ok(Staff {
            account_id: r.try_get("id")?,
            role: r.try_get("role")?,
        })
    })
    .transpose()
}

/// A player in a live play session.
#[derive(Clone, Debug)]
pub struct LivePlayer {
    /// Play session.
    pub session_id: Uuid,
    /// Account e-mail, when the identity has one.
    pub email: Option<String>,
    /// Discord handle.
    pub discord: Option<String>,
    /// `Name#12345`.
    pub profile: Option<String>,
    /// Pool.
    pub pool: String,
    /// `free` | `vip`.
    pub tier: String,
    /// Admission, epoch ms.
    pub admitted_at_ms: Option<i64>,
}

/// Every live active session of `game`, oldest admission first.
pub async fn live_players(db: &PgPool, game: &str) -> Result<Vec<LivePlayer>, sqlx::Error> {
    let sql = format!(
        "SELECT s.id, i.email_lower, a.discord::text AS discord, \
         p.name AS profile, s.pool, pl.tier, \
         (extract(epoch FROM s.admitted_at) * 1000)::bigint AS admitted_ms \
         FROM play_session s \
         JOIN account a ON a.id = s.account_id \
         JOIN profile p ON p.id = s.profile_id \
         JOIN pool pl ON pl.name = s.pool \
         LEFT JOIN account_identity i ON i.account_id = s.account_id \
         WHERE s.game = $1 AND {} ORDER BY s.admitted_at",
        Stale::live_active()
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .fetch_all(db)
        .await?
        .iter()
        .map(|r| {
            Ok(LivePlayer {
                session_id: r.try_get("id")?,
                email: r.try_get("email_lower")?,
                discord: r.try_get("discord")?,
                profile: r.try_get("profile")?,
                pool: r.try_get("pool")?,
                tier: r.try_get("tier")?,
                admitted_at_ms: r.try_get("admitted_ms")?,
            })
        })
        .collect()
}

/// An open world and who holds a seat in it.
#[derive(Clone, Debug)]
pub struct Lobby {
    /// World.
    pub id: Uuid,
    /// Name.
    pub name: String,
    /// `free` | `supporter`.
    pub kind: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// `public` | `friends` | `secret`.
    pub audience: String,
    /// Host `Name#12345`.
    pub host: Option<String>,
    /// Count the host last announced.
    pub players: i32,
    /// Seats.
    pub max_players: i32,
    /// Seated or joining profiles.
    pub members: Vec<String>,
    /// Opened, epoch ms.
    pub opened_at_ms: i64,
}

/// Open, unexpired worlds of `game`, newest first.
pub async fn lobbies(db: &PgPool, game: &str) -> Result<Vec<Lobby>, sqlx::Error> {
    sqlx::query(
        "SELECT w.id, w.name, w.kind, w.mode, w.audience, w.players, w.max_players, \
         h.name AS host, \
         (extract(epoch FROM w.opened_at) * 1000)::bigint AS opened_ms, \
         coalesce(array_agg(m.name ORDER BY g.issued_at) \
           FILTER (WHERE g.id IS NOT NULL), '{}') AS members \
         FROM world w \
         JOIN profile h ON h.id = w.host_profile_id \
         LEFT JOIN mm_grant g ON g.world_id = w.id AND g.state IN ('issued', 'used') \
         LEFT JOIN profile m ON m.id = g.profile_id \
         WHERE w.game = $1 AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
         GROUP BY w.id, h.name ORDER BY w.opened_at DESC",
    )
    .bind(game)
    .fetch_all(db)
    .await?
    .iter()
    .map(|r| {
        Ok(Lobby {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            kind: r.try_get("kind")?,
            mode: r.try_get("mode")?,
            audience: r.try_get("audience")?,
            host: r.try_get("host")?,
            players: r.try_get("players")?,
            max_players: r.try_get("max_players")?,
            members: r
                .try_get::<Vec<Option<String>>, _>("members")?
                .into_iter()
                .flatten()
                .collect(),
            opened_at_ms: r.try_get("opened_ms")?,
        })
    })
    .collect()
}

/// Sets a pool's capacity (`None`: unlimited) and queue length.
pub async fn set_pool_limits(
    db: &PgPool,
    game: &str,
    pool: &str,
    capacity: Option<i32>,
    queue_limit: i32,
) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE pool SET capacity = $3, queue_limit = $4 WHERE game = $1 AND name = $2",
    )
    .bind(game)
    .bind(pool)
    .bind(capacity)
    .bind(queue_limit)
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The per-game client floor; the effective launcher minimum is the higher of this and the
/// service's `FI_MIN_LAUNCHER`.
#[derive(Clone, Debug)]
pub struct ClientFloor {
    /// `game.min_launcher`.
    pub min_launcher: Option<String>,
    /// `game.min_hook`.
    pub min_hook: Option<String>,
}

/// Why a client floor was refused.
#[derive(Debug, thiserror::Error)]
pub enum FloorError {
    /// Not a launcher version core can compare (it would fail closed for every player).
    #[error("minimum launcher must be a version such as 1.0.7")]
    Launcher,
    /// Outside the column's 1 to 32 characters, or with control characters.
    #[error("minimum hook is 1 to 32 characters")]
    Hook,
    /// No such game.
    #[error("unknown game")]
    UnknownGame,
    /// Storage.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Reads the client floor of `game`.
pub async fn client_floor(db: &PgPool, game: &str) -> Result<Option<ClientFloor>, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT min_launcher, min_hook FROM game WHERE code = $1")
            .bind(game)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(min_launcher, min_hook)| ClientFloor {
        min_launcher,
        min_hook,
    }))
}

/// Sets the client floor of `game` (`None` clears a value).
pub async fn set_client_floor(
    db: &PgPool,
    game: &str,
    floor: &ClientFloor,
) -> Result<(), FloorError> {
    if let Some(v) = &floor.min_launcher {
        fi_domain::version::LauncherVersion::parse(v).map_err(|_| FloorError::Launcher)?;
    }
    if floor
        .min_hook
        .as_deref()
        .is_some_and(|h| !(1..=32).contains(&h.chars().count()) || h.chars().any(char::is_control))
    {
        return Err(FloorError::Hook);
    }
    let done = sqlx::query("UPDATE game SET min_launcher = $2, min_hook = $3 WHERE code = $1")
        .bind(game)
        .bind(&floor.min_launcher)
        .bind(&floor.min_hook)
        .execute(db)
        .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(FloorError::UnknownGame)
    }
}

/// Appends one `admin_action` row for a console change.
pub async fn audit(
    db: &PgPool,
    actor: Uuid,
    action: &str,
    target: &str,
    detail: Value,
) -> Result<(), crate::identity::IdentityError> {
    let mut conn = db
        .acquire()
        .await
        .map_err(crate::identity::IdentityError::Db)?;
    let actor = Actor {
        account_id: actor,
        role: Role::Admin,
        request_id: None,
    };
    let entry = AuditEntry {
        action,
        account_id: None,
        target,
        reason: Some("admin console"),
        detail,
    };
    moderation::audit_in(&mut conn, &actor, &entry).await?;
    Ok(())
}
