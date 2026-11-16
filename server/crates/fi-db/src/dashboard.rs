//! The play dashboard's reads (DASHBOARD_API.md, CR-S82, OD-168; served by
//! `fi_surfaces::dashboard` on the platform's internal listener) and the profile's dashboard
//! opt-out (ACCOUNTS.md §5 `POST /profiles/{xuid}/website`, CR-S83, OD-168 follow-up DB-Q4).
//!
//! Aggregates and public cards only: no statement here selects an XUID, an account id, a Discord
//! user id, an e-mail or an address (DASHBOARD_API.md §1). The one exception is [`avatar`], whose
//! statement selects the Discord user id the CDN path needs; it reaches only the platform's avatar
//! fetch, never a response. (A player card's `profile_id` stays server-side, for the stats and
//! boards reads of the same request.)
//!
//! **Listed lobbies** ([`LISTED_WORLD`], PLAY_DASHBOARD.md §5.1): open and inside their TTL, audience
//! `everyone` (the schema's "public, no password": `friends` and `private` worlds need a code and
//! `supporters` worlds admit Supporters only), no host allow-list, and not a pineapple world
//! (OD-149). Full ones are listed too (DB-Q7). `GET /live` counts exactly these, so the count
//! always equals the list.
//!
//! **Host card**: the host profile's plain name when the host is listed on the dashboard
//! ([`crate::visibility::VISIBLE`], no `social_hidden_at`, `web_listed`); otherwise the lobby is
//! still listed, without a host.
//!
//! **Leaderboards** ([`boards`], DASHBOARD_API.md §3, PD-2): the game's boards with the game's own
//! ranking, website-hidden rows omitted after ranking.
//!
//! **Players** ([`players`], [`avatar`], DASHBOARD_API.md §4, PD-3): the list, a profile by its
//! plain name, and where its Discord avatar is.

pub mod avatar;
pub mod boards;
pub mod players;

use crate::accounts::{self, ProfileRow};
use crate::identity::{IdentityError, ms};
use fi_domain::UnixMs;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The condition on `world w` of a lobby the dashboard lists (see the module).
pub const LISTED_WORLD: &str = "w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
     AND w.audience = 'everyone' AND w.allow IS NULL AND NOT w.pool";

/// The condition on the joined host `profile p` and `account a` of a host the dashboard shows.
fn host_listed() -> String {
    format!(
        "({} AND p.social_hidden_at IS NULL AND p.web_listed)",
        crate::visibility::VISIBLE
    )
}

/// The largest page of [`lobbies_in`].
pub const MAX_LOBBIES: i64 = 100;

/// One game's live aggregates (`GET /live`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveRow {
    /// The database clock when read.
    pub at: UnixMs,
    /// Game.
    pub game: String,
    /// Admitted, unexpired, non-dedicated play sessions.
    pub online: i64,
    /// Of `online`, Hardcore profiles'.
    pub online_hardcore: i64,
    /// Of `online`, Noob profiles'.
    pub online_noob: i64,
    /// Listed lobbies ([`LISTED_WORLD`]).
    pub lobbies: i64,
    /// Of them, open to Hardcore (Hardcore or any-mode).
    pub lobbies_hardcore: i64,
    /// Of them, open to Noob (Noob or any-mode).
    pub lobbies_noob: i64,
    /// `game.online_capacity` (`None`: unlimited).
    pub capacity: Option<i32>,
}

/// `game`'s live aggregates in one statement; `None` when no such game.
pub async fn live_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Option<LiveRow>, IdentityError> {
    let sql = format!(
        "SELECT clock_timestamp() AS at, g.code AS game, g.online_capacity AS capacity, \
           s.online, s.hardcore, s.noob, l.lobbies, l.hardcore AS lobbies_hardcore, l.noob AS lobbies_noob \
         FROM game g \
         CROSS JOIN LATERAL ( \
           SELECT count(*) AS online, count(*) FILTER (WHERE p.mode = 'hardcore') AS hardcore, \
             count(*) FILTER (WHERE p.mode = 'noob') AS noob \
           FROM play_session s JOIN profile p ON p.id = s.profile_id \
           WHERE p.game = g.code AND s.state = 'active' AND s.expires_at > clock_timestamp() \
             AND NOT s.dedicated) s \
         CROSS JOIN LATERAL ( \
           SELECT count(*) AS lobbies, \
             count(*) FILTER (WHERE w.mode = 'hardcore' OR w.any_mode) AS hardcore, \
             count(*) FILTER (WHERE w.mode = 'noob' OR w.any_mode) AS noob \
           FROM world w WHERE w.game = g.code AND {LISTED_WORLD}) l \
         WHERE g.code = $1"
    );
    let Some(r) = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(LiveRow {
        at: ms(r.try_get("at")?),
        game: r.try_get("game")?,
        online: r.try_get("online")?,
        online_hardcore: r.try_get("hardcore")?,
        online_noob: r.try_get("noob")?,
        lobbies: r.try_get("lobbies")?,
        lobbies_hardcore: r.try_get("lobbies_hardcore")?,
        lobbies_noob: r.try_get("lobbies_noob")?,
        capacity: r.try_get("capacity")?,
    }))
}

/// Whether `game` is a game of this server.
pub async fn game_known_in(conn: &mut PgConnection, game: &str) -> Result<bool, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM game WHERE code = $1)")
            .bind(game)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// The host's public card on a lobby.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LobbyHostRow {
    /// The plain name.
    pub name: String,
    /// The linked Discord account's global name, else its username.
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
}

/// One listed lobby (`GET /lobbies`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LobbyRow {
    /// The world.
    pub world_id: Uuid,
    /// Its name.
    pub name: String,
    /// The host's card, when the host is listed.
    pub host: Option<LobbyHostRow>,
    /// Seated players: the host (unless dedicated) and every `used` seat, as matchmaking counts.
    pub players: i32,
    /// Seats.
    pub max_players: i32,
    /// Seated, reserved (`issued`, unexpired) and invited (pending, unexpired) seats fill it, as
    /// matchmaking's `full` for a stranger.
    pub full: bool,
    /// `freeroam` | `roadtrip`.
    pub activity: String,
    /// The host's coarse region.
    pub region: Option<String>,
    /// A modded world.
    pub mods: bool,
    /// A dedicated world.
    pub dedicated: bool,
    /// Open to Hardcore and Noob.
    pub any_mode: bool,
    /// Opened.
    pub created_at: UnixMs,
}

/// The listed lobbies of `game` open to `mode` (its own and any-mode worlds): not full first,
/// then most players, then newest; at most `limit` (clamped to 1..=[`MAX_LOBBIES`]).
pub async fn lobbies_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    limit: i64,
) -> Result<Vec<LobbyRow>, IdentityError> {
    let host = host_listed();
    let sql = format!(
        "SELECT * FROM ( \
           SELECT w.id, w.name, w.max_players, w.activity, w.region, jsonb_array_length(w.mods) > 0 AS mods, \
             w.dedicated, w.any_mode, w.opened_at, \
             ((CASE WHEN w.dedicated THEN 0 ELSE 1 END) \
               + (SELECT count(*) FROM mm_grant s WHERE s.world_id = w.id AND s.state = 'used'))::int AS players, \
             (SELECT count(*) FROM mm_grant g WHERE g.world_id = w.id AND g.state = 'issued' \
               AND g.expires_at > clock_timestamp()) AS reserved, \
             (SELECT count(*) FROM world_invite i WHERE i.world_id = w.id AND i.state = 'pending' \
               AND i.expires_at > clock_timestamp()) AS held, \
             CASE WHEN {host} THEN p.name END AS host_name, \
             CASE WHEN {host} AND a.discord_user_id IS NOT NULL \
               THEN coalesce(a.discord_global_name, a.discord_username) END AS host_discord_name, \
             ({host} AND a.discord_avatar IS NOT NULL) AS host_avatar \
           FROM world w \
           JOIN profile p ON p.id = w.host_profile_id \
           JOIN account a ON a.id = p.account_id \
           WHERE w.game = $1 AND (w.mode = $2 OR w.any_mode) AND {LISTED_WORLD} \
         ) l \
         ORDER BY (l.players + l.reserved + l.held >= l.max_players), l.players DESC, l.opened_at DESC, l.id DESC \
         LIMIT $3"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(mode)
        .bind(limit.clamp(1, MAX_LOBBIES))
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            let players: i32 = r.try_get("players")?;
            let max_players: i32 = r.try_get("max_players")?;
            let reserved: i64 = r.try_get("reserved")?;
            let held: i64 = r.try_get("held")?;
            let host = r
                .try_get::<Option<String>, _>("host_name")?
                .map(|name| -> Result<LobbyHostRow, sqlx::Error> {
                    Ok(LobbyHostRow {
                        name,
                        discord_name: r.try_get("host_discord_name")?,
                        avatar: r.try_get("host_avatar")?,
                    })
                })
                .transpose()?;
            Ok(LobbyRow {
                world_id: r.try_get("id")?,
                name: r.try_get("name")?,
                host,
                players,
                max_players,
                full: i64::from(players) + reserved + held >= i64::from(max_players),
                activity: r.try_get("activity")?,
                region: r.try_get("region")?,
                mods: r.try_get("mods")?,
                dedicated: r.try_get("dedicated")?,
                any_mode: r.try_get("any_mode")?,
                created_at: ms(r.try_get::<OffsetDateTime, _>("opened_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Why the dashboard opt-out was refused.
#[derive(Debug, thiserror::Error)]
pub enum WebsiteError {
    /// No such live profile of this account (`ACCOUNTS.NOT_FOUND`).
    #[error("not found")]
    NotFound,
    /// The account or the profile is pending deletion (`ACCOUNTS.DELETION_PENDING`).
    #[error("pending deletion")]
    DeletionPending,
    /// Database trouble.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for WebsiteError {
    fn from(e: sqlx::Error) -> Self {
        WebsiteError::Db(e.into())
    }
}

/// Sets whether the account's live profile `xuid` is listed on the dashboard, in the caller's
/// transaction (`POST /launcher/v1/profiles/{xuid}/website`). Setting the current value writes
/// nothing (idempotent without a key). Returns the profile as it is now.
pub async fn set_web_listed_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    listed: bool,
    game: &str,
) -> Result<ProfileRow, WebsiteError> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM account WHERE id = $1 FOR NO KEY UPDATE")
            .bind(account_id)
            .fetch_optional(&mut *conn)
            .await?;
    match status.as_deref() {
        None | Some("deleted") => return Err(WebsiteError::NotFound),
        Some("deletion_pending") => return Err(WebsiteError::DeletionPending),
        Some(_) => {}
    }
    let xuid = i64::try_from(xuid).map_err(|_| WebsiteError::NotFound)?;
    let row = sqlx::query(
        "SELECT id, status FROM profile WHERE account_id = $1 AND xuid = $2 AND game = $3 \
           AND status <> 'deleted' FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .bind(xuid)
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(WebsiteError::NotFound)?;
    let profile_id: Uuid = row.try_get("id")?;
    if row.try_get::<String, _>("status")? != "active" {
        return Err(WebsiteError::DeletionPending);
    }
    sqlx::query("UPDATE profile SET web_listed = $2 WHERE id = $1 AND web_listed <> $2")
        .bind(profile_id)
        .bind(listed)
        .execute(&mut *conn)
        .await?;
    accounts::profile_in(conn, profile_id)
        .await?
        .ok_or(WebsiteError::NotFound)
}
