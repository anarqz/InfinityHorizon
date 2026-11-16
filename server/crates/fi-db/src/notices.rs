//! What the launcher's hub and the web's home show (CORE_API.md §2): the games and their switches
//! (maintenance, version floors, accepting), the notice board, and the queue as Postgres sees it.
//!
//! The maintenance notice is never a row: it is derived from `game.maintenance_until` on read, so
//! lifting maintenance removes it at once. The queue summary reads `play_session` the way the
//! gate's admission counts it (live = `queued` or `active` and not past `expires_at`); the gate
//! lane's `fi_db::gate::queue_summary` replaces it when the lanes merge.

use crate::identity::{IdentityError, ms, ts};
use fi_domain::UnixMs;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Notices of one board answer.
pub const BOARD_LIMIT: i64 = 20;
/// Notices of one admin listing.
pub const ADMIN_LIMIT: i64 = 100;

/// A game and its switches (`game` row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameRow {
    /// `fh2`.
    pub code: String,
    /// Display title.
    pub title: String,
    /// Oldest hook build allowed online (the deployment's floor when unset).
    pub min_hook: Option<String>,
    /// Oldest launcher allowed.
    pub min_launcher: Option<String>,
    /// Whether the game takes players at all.
    pub accepting: bool,
    /// Maintenance end while it is in force (`maintenance_until > now`).
    pub maintenance_until: Option<UnixMs>,
    /// The maintenance message, while in force.
    pub maintenance_message: Option<String>,
}

const GAME_COLUMNS: &str = "code, title, min_hook, min_launcher, accepting, \
     CASE WHEN maintenance_until > clock_timestamp() THEN maintenance_until END AS maintenance_until, \
     CASE WHEN maintenance_until > clock_timestamp() THEN maintenance_message END AS maintenance_message";

fn game_from_row(r: &PgRow) -> Result<GameRow, sqlx::Error> {
    Ok(GameRow {
        code: r.try_get("code")?,
        title: r.try_get("title")?,
        min_hook: r.try_get("min_hook")?,
        min_launcher: r.try_get("min_launcher")?,
        accepting: r.try_get("accepting")?,
        maintenance_until: r
            .try_get::<Option<OffsetDateTime>, _>("maintenance_until")?
            .map(ms),
        maintenance_message: r.try_get("maintenance_message")?,
    })
}

/// Every game, by code.
pub async fn games_in(conn: &mut PgConnection) -> Result<Vec<GameRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GAME_COLUMNS} FROM game ORDER BY code"
    )))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(game_from_row).collect::<Result<_, _>>()?)
}

/// The games a player is shown (`/launcher/v1/info`, `/core/v1/status`): every game but one closed
/// without an accepting pool (`GameRules::open_with_pool`, CR-S97), so a server where FM5 is closed
/// lists FH2 alone, as before it.
pub async fn listed_games_in(conn: &mut PgConnection) -> Result<Vec<GameRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GAME_COLUMNS} FROM game g WHERE g.code <> ALL($1) \
           OR EXISTS (SELECT 1 FROM pool p WHERE p.game = g.code AND p.accepting) ORDER BY code"
    )))
    .bind(fi_domain::GameRules::open_with_pool_codes())
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(game_from_row).collect::<Result<_, _>>()?)
}

/// One game.
pub async fn game_in(
    conn: &mut PgConnection,
    code: &str,
) -> Result<Option<GameRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {GAME_COLUMNS} FROM game WHERE code = $1"
    )))
    .bind(code)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(game_from_row).transpose()?)
}

/// Sets (or, with `until = None`, lifts) a game's maintenance. `NotFound` for an unknown game; an
/// `until` in the past lifts it too (readers compare with the clock).
pub async fn set_maintenance_in(
    conn: &mut PgConnection,
    code: &str,
    until: Option<UnixMs>,
    message: Option<&str>,
) -> Result<GameRow, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE game SET maintenance_until = $2, maintenance_message = CASE WHEN $2::timestamptz IS NULL THEN NULL ELSE $3 END \
         WHERE code = $1 RETURNING {GAME_COLUMNS}"
    )))
    .bind(code)
    .bind(until.map(ts))
    .bind(message)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(IdentityError::NotFound)?;
    Ok(game_from_row(&row)?)
}

/// A notice row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeRow {
    /// Id.
    pub id: Uuid,
    /// The game, or `None` for every game.
    pub game: Option<String>,
    /// `news` | `maintenance` | `warning` | `event`.
    pub kind: String,
    /// `info` | `warning` | `error`.
    pub level: String,
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// `en` | `pt-BR`.
    pub lang: String,
    /// Shown from.
    pub since: UnixMs,
    /// Shown until.
    pub until: Option<UnixMs>,
}

/// A notice to publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewNotice<'a> {
    /// The game, or `None` for every game.
    pub game: Option<&'a str>,
    /// Kind.
    pub kind: &'a str,
    /// Level.
    pub level: &'a str,
    /// Title.
    pub title: &'a str,
    /// Body.
    pub body: &'a str,
    /// Language.
    pub lang: &'a str,
    /// From (`None`: now).
    pub since: Option<UnixMs>,
    /// Until.
    pub until: Option<UnixMs>,
    /// The admin publishing it.
    pub created_by: Uuid,
}

const NOTICE_COLUMNS: &str = "id, game, kind, level, title, body, lang, since, until";

fn notice_from_row(r: &PgRow) -> Result<NoticeRow, sqlx::Error> {
    Ok(NoticeRow {
        id: r.try_get("id")?,
        game: r.try_get("game")?,
        kind: r.try_get("kind")?,
        level: r.try_get("level")?,
        title: r.try_get("title")?,
        body: r.try_get("body")?,
        lang: r.try_get("lang")?,
        since: ms(r.try_get("since")?),
        until: r.try_get::<Option<OffsetDateTime>, _>("until")?.map(ms),
    })
}

/// The board of `game` in `lang`: notices of that game and platform-wide ones, in their window
/// now, newest `since` first, at most [`BOARD_LIMIT`].
pub async fn board_in(
    conn: &mut PgConnection,
    game: &str,
    lang: &str,
) -> Result<Vec<NoticeRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {NOTICE_COLUMNS} FROM notice \
         WHERE deleted_at IS NULL AND lang = $2 AND (game IS NULL OR game = $1) \
           AND since <= clock_timestamp() AND (until IS NULL OR until > clock_timestamp()) \
         ORDER BY since DESC, id DESC LIMIT $3"
    )))
    .bind(game)
    .bind(lang)
    .bind(BOARD_LIMIT)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(notice_from_row).collect::<Result<_, _>>()?)
}

/// Every notice not deleted (scheduled and expired included), newest first, for the admin console.
pub async fn admin_list_in(conn: &mut PgConnection) -> Result<Vec<NoticeRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {NOTICE_COLUMNS} FROM notice WHERE deleted_at IS NULL ORDER BY since DESC, id DESC LIMIT $1"
    )))
    .bind(ADMIN_LIMIT)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(notice_from_row).collect::<Result<_, _>>()?)
}

/// One notice (deleted ones included: `NoticeRow` does not say).
pub async fn notice_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<NoticeRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {NOTICE_COLUMNS} FROM notice WHERE id = $1 AND deleted_at IS NULL"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(notice_from_row).transpose()?)
}

/// Publishes a notice. `NotFound` for an unknown game.
pub async fn create_in(
    conn: &mut PgConnection,
    n: &NewNotice<'_>,
) -> Result<NoticeRow, IdentityError> {
    if let Some(game) = n.game {
        let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM game WHERE code = $1)")
            .bind(game)
            .fetch_one(&mut *conn)
            .await?;
        if !known {
            return Err(IdentityError::NotFound);
        }
    }
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO notice (game, kind, level, title, body, lang, since, until, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, clock_timestamp()), $8, $9) RETURNING {NOTICE_COLUMNS}"
    )))
    .bind(n.game)
    .bind(n.kind)
    .bind(n.level)
    .bind(n.title)
    .bind(n.body)
    .bind(n.lang)
    .bind(n.since.map(ts))
    .bind(n.until.map(ts))
    .bind(n.created_by)
    .fetch_one(&mut *conn)
    .await?;
    Ok(notice_from_row(&row)?)
}

/// Deletes (soft) a notice; `false` when it does not exist or was deleted already.
pub async fn delete_in(conn: &mut PgConnection, id: Uuid) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE notice SET deleted_at = clock_timestamp() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Live sessions of one game in one pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueRow {
    /// Game.
    pub game: String,
    /// Pool name (`vip`, `matchmaking`, ...).
    pub pool: String,
    /// Admitted and not lapsed.
    pub active: i64,
    /// Waiting and not stale.
    pub queued: i64,
    /// The pool's capacity (`None`: unlimited).
    pub capacity: Option<i32>,
}

/// The public queue summary: per game and pool, active and queued sessions not past `expires_at`
/// (dedicated lobbies' host sessions are not counted, OD-109).
pub async fn queue_summary_in(conn: &mut PgConnection) -> Result<Vec<QueueRow>, IdentityError> {
    let rows = sqlx::query(
        "SELECT p.game, s.pool, max(pl.capacity) AS capacity, \
           count(*) FILTER (WHERE s.state = 'active') AS active, \
           count(*) FILTER (WHERE s.state = 'queued') AS queued \
         FROM play_session s JOIN profile p ON p.id = s.profile_id JOIN pool pl ON pl.name = s.pool \
         WHERE s.state IN ('queued', 'active') AND s.expires_at > clock_timestamp() AND NOT s.dedicated \
         GROUP BY p.game, s.pool ORDER BY p.game, s.pool",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(QueueRow {
                game: r.try_get("game")?,
                pool: r.try_get("pool")?,
                active: r.try_get("active")?,
                queued: r.try_get("queued")?,
                capacity: r.try_get("capacity")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// One game's live aggregates (CR-L40, CORE_API.md §2.2 `games[].live`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveRow {
    /// Game.
    pub game: String,
    /// Admitted, unexpired, non-dedicated sessions.
    pub online: i64,
    /// `game.online_capacity` (`None`: unlimited).
    pub capacity: Option<i32>,
    /// Of `online`, Hardcore profiles'.
    pub hardcore: i64,
    /// Of `online`, Noob profiles'.
    pub noob: i64,
    /// Open worlds inside their TTL, every mode, modded or not, dedicated included.
    pub lobbies: i64,
    /// Non-deleted Hardcore profiles.
    pub profiles_hardcore: i64,
    /// Non-deleted Noob profiles.
    pub profiles_noob: i64,
}

/// Every game's live aggregates in one statement (CR-L40): aggregates only, no identities.
pub async fn live_in(conn: &mut PgConnection) -> Result<Vec<LiveRow>, IdentityError> {
    let rows = sqlx::query(
        "SELECT g.code AS game, g.online_capacity AS capacity, \
           coalesce(s.online, 0) AS online, coalesce(s.hardcore, 0) AS hardcore, coalesce(s.noob, 0) AS noob, \
           (SELECT count(*) FROM world w WHERE w.game = g.code AND w.closed_at IS NULL \
              AND w.expires_at > clock_timestamp()) AS lobbies, \
           (SELECT count(*) FROM profile p WHERE p.game = g.code AND p.status <> 'deleted' \
              AND p.mode = 'hardcore') AS profiles_hardcore, \
           (SELECT count(*) FROM profile p WHERE p.game = g.code AND p.status <> 'deleted' \
              AND p.mode = 'noob') AS profiles_noob \
         FROM game g \
         LEFT JOIN LATERAL ( \
           SELECT count(*) AS online, count(*) FILTER (WHERE p.mode = 'hardcore') AS hardcore, \
             count(*) FILTER (WHERE p.mode = 'noob') AS noob \
           FROM play_session s JOIN profile p ON p.id = s.profile_id \
           WHERE p.game = g.code AND s.state = 'active' AND s.expires_at > clock_timestamp() \
             AND NOT s.dedicated) s ON true \
         ORDER BY g.code",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(LiveRow {
                game: r.try_get("game")?,
                online: r.try_get("online")?,
                capacity: r.try_get("capacity")?,
                hardcore: r.try_get("hardcore")?,
                noob: r.try_get("noob")?,
                lobbies: r.try_get("lobbies")?,
                profiles_hardcore: r.try_get("profiles_hardcore")?,
                profiles_noob: r.try_get("profiles_noob")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// A queued session of the caller and its place in line (1 = next).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePosition {
    /// The session.
    pub session_id: Uuid,
    /// Game.
    pub game: String,
    /// Pool.
    pub pool: String,
    /// 1-based position among live queued sessions of the pool.
    pub position: i64,
    /// When it queued.
    pub queued_at: UnixMs,
}

/// The account's queued sessions with their positions.
pub async fn queue_positions_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Vec<QueuePosition>, IdentityError> {
    let rows = sqlx::query(
        "SELECT s.id, p.game, s.pool, s.queued_at, \
           1 + (SELECT count(*) FROM play_session o WHERE o.pool = s.pool AND o.state = 'queued' \
                AND o.expires_at > clock_timestamp() AND (o.queued_at, o.id) < (s.queued_at, s.id)) AS position \
         FROM play_session s JOIN profile p ON p.id = s.profile_id \
         WHERE s.account_id = $1 AND s.state = 'queued' AND s.expires_at > clock_timestamp() \
         ORDER BY s.queued_at",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(QueuePosition {
                session_id: r.try_get("id")?,
                game: r.try_get("game")?,
                pool: r.try_get("pool")?,
                position: r.try_get("position")?,
                queued_at: ms(r.try_get("queued_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Bundles set aside in the last 7 days for one game of the account (gate refusals, conflict
/// losers, unreadable uploads): what the `parked_save` account notice says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedRecent {
    /// Game.
    pub game: String,
    /// How many.
    pub count: i64,
    /// The newest one's time.
    pub latest: UnixMs,
}

/// [`ParkedRecent`] per game of `account_id`.
pub async fn parked_recent_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Vec<ParkedRecent>, IdentityError> {
    let rows = sqlx::query(
        "SELECT p.game, count(*) AS count, max(sp.parked_at) AS latest \
         FROM save_parked sp JOIN profile p ON p.id = sp.profile_id \
         WHERE p.account_id = $1 AND sp.parked_at > clock_timestamp() - interval '7 days' \
           AND sp.reason IN ('conflict_loser', 'gate_refused', 'unreadable') \
         GROUP BY p.game ORDER BY p.game",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(ParkedRecent {
                game: r.try_get("game")?,
                count: r.try_get("count")?,
                latest: ms(r.try_get("latest")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}
