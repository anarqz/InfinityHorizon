//! The proxied game requests' storage (migration `20261019000400_gameplane.sql`; SER-M3 steps 3-4,
//! docs/contracts/GAME_PLATFORM.md §3a): what the Forza RPC and Xbox Live handlers of
//! `fi_surfaces::game::proxy` keep of a profile, ported from the Beta's SQLite store
//! (`deprecated/beta/server/src/db.rs`: `users`, `profile`, `profile_tables`, `transactions`,
//! `presence`), keyed by `(game, profile)` instead of the XUID.
//!
//! Every function takes the caller's connection (`*_in`, usually the request's one transaction)
//! and reads the database clock. Writes are upserts, so a retried request writes the same rows:
//!
//! - [`record_logon_in`], [`set_card_in`], [`player_in`]: the logon state (career level, badge,
//!   title) served back in UserData, and the tables checkpoint the Logon reply carries;
//! - [`merge_profile_in`]: OnlineProfile.UpdateAll, sections the game marks unchanged (an empty
//!   list) kept from the stored profile, in one statement (no read-modify-write race);
//! - [`apply_table_in`], [`set_tables_checkpoint_in`]: OnlineProfile.UpdateAllTables, rows merged by
//!   key and `delete<Table>` keys removed, in one statement per table;
//! - [`keep_reply_in`], [`reply_in`]: replies of `X-TransactionId` requests for Reconcile, a ring
//!   of [`REPLY_SLOTS`] per profile;
//! - [`set_presence_in`], [`presence_in`]: the Xbox Live presence the game posts;
//! - [`gamertags_in`]: the gamertags of the profiles behind a list of XUIDs (Xbox Live profile
//!   settings), banned and deleted profiles left out.
//!
//! Postgres text and jsonb cannot hold NUL: strings and keys of the game's JSON lose their NUL
//! characters before they are stored (a refusal would make the game resend the same data forever).

use crate::identity::{IdentityError, ms};
use fi_domain::UnixMs;
use serde_json::{Map, Value as Json};
use sqlx::types::Json as Jsonb;
use sqlx::{PgConnection, Row};
use std::borrow::Cow;
use time::OffsetDateTime;
use uuid::Uuid;

/// The largest reply kept for Reconcile: the game's blob limit in replies (0x80000), which
/// `TransactionReconcile_Results.WebRequestPayload` must fit.
pub const MAX_REPLY: usize = 0x80000;

/// Kept replies per profile (the `game_transaction.slot` CHECK): the ring [`keep_reply_in`]
/// overwrites, oldest first. The game reconciles the transaction it just missed, so 8 is ample.
pub const REPLY_SLOTS: i16 = 8;

fn has_nul(v: &Json) -> bool {
    match v {
        Json::String(s) => s.contains('\0'),
        Json::Array(items) => items.iter().any(has_nul),
        Json::Object(map) => map_has_nul(map),
        _ => false,
    }
}

fn map_has_nul(map: &Map<String, Json>) -> bool {
    map.iter().any(|(k, v)| k.contains('\0') || has_nul(v))
}

fn without_nul(v: &Json) -> Json {
    match v {
        Json::String(s) => Json::String(s.replace('\0', "")),
        Json::Array(items) => Json::Array(items.iter().map(without_nul).collect()),
        Json::Object(map) => Json::Object(map_without_nul(map)),
        other => other.clone(),
    }
}

fn map_without_nul(map: &Map<String, Json>) -> Map<String, Json> {
    map.iter()
        .map(|(k, v)| (k.replace('\0', ""), without_nul(v)))
        .collect()
}

/// `map` as Postgres can store it: borrowed when it has no NUL (the usual case, no copy).
fn storable(map: &Map<String, Json>) -> Cow<'_, Map<String, Json>> {
    if map_has_nul(map) {
        Cow::Owned(map_without_nul(map))
    } else {
        Cow::Borrowed(map)
    }
}

/// The profile a request writes for: the ticket's game, profile and session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner<'a> {
    /// The game (`fh2`).
    pub game: &'a str,
    /// The profile.
    pub profile_id: Uuid,
    /// The play session of the request.
    pub session_id: Uuid,
}

/// The player card the game reports (Logon, SetPlayerCardInfo) and others see in UserData.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Card {
    /// `currentCareerLevel` (stored from 0; the game shows level + 1).
    pub career_level: u32,
    /// `currentBadgeId`.
    pub badge_id: u16,
    /// `currentPlayerTitleId`.
    pub player_title_id: u16,
}

/// A profile as the game plane serves it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    /// XUID.
    pub xuid: i64,
    /// Gamertag name (`None` once the profile is erased).
    pub name: Option<String>,
    /// The card last reported (zeros before the first Logon).
    pub card: Card,
    /// The `transactionId` of the last UpdateAllTables stored.
    pub tables_tran_id: Option<i32>,
}

/// One profile's presence as its game last posted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    /// `active`, `inactive` or `offline`.
    pub state: String,
    /// The rich presence id (`RP_FREEROAM`, ...), possibly empty.
    pub rich: String,
    /// When it was posted.
    pub updated_at: UnixMs,
    /// The database clock of the read.
    pub now: UnixMs,
}

fn card_of(r: &sqlx::postgres::PgRow) -> Result<Card, sqlx::Error> {
    let level: Option<i64> = r.try_get("career_level")?;
    let badge: Option<i32> = r.try_get("badge_id")?;
    let title: Option<i32> = r.try_get("player_title_id")?;
    Ok(Card {
        career_level: level.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        badge_id: badge.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
        player_title_id: title.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
    })
}

/// User.Logon: records the card the game reports and the logon, and returns the stored tables
/// checkpoint (`None` until the first UpdateAllTables).
pub async fn record_logon_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    card: Card,
    title_version: Option<i32>,
) -> Result<Option<i32>, IdentityError> {
    let tran: Option<i32> = sqlx::query_scalar(
        "INSERT INTO game_logon (game, profile_id, career_level, badge_id, player_title_id, title_version, \
           last_session_id, first_logon_at, last_logon_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, clock_timestamp(), clock_timestamp()) \
         ON CONFLICT (game, profile_id) DO UPDATE SET career_level = EXCLUDED.career_level, \
           badge_id = EXCLUDED.badge_id, player_title_id = EXCLUDED.player_title_id, \
           title_version = EXCLUDED.title_version, last_session_id = EXCLUDED.last_session_id, \
           first_logon_at = COALESCE(game_logon.first_logon_at, EXCLUDED.first_logon_at), \
           last_logon_at = EXCLUDED.last_logon_at, updated_at = clock_timestamp() \
         RETURNING tables_tran_id",
    )
    .bind(owner.game)
    .bind(owner.profile_id)
    .bind(i64::from(card.career_level))
    .bind(i32::from(card.badge_id))
    .bind(i32::from(card.player_title_id))
    .bind(title_version)
    .bind(owner.session_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(tran)
}

/// User.SetPlayerCardInfo: the card others see, without a logon.
pub async fn set_card_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    card: Card,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO game_logon (game, profile_id, career_level, badge_id, player_title_id, last_session_id) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (game, profile_id) DO UPDATE SET career_level = EXCLUDED.career_level, \
           badge_id = EXCLUDED.badge_id, player_title_id = EXCLUDED.player_title_id, \
           last_session_id = EXCLUDED.last_session_id, updated_at = clock_timestamp()",
    )
    .bind(owner.game)
    .bind(owner.profile_id)
    .bind(i64::from(card.career_level))
    .bind(i32::from(card.badge_id))
    .bind(i32::from(card.player_title_id))
    .bind(owner.session_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The profile with its logon state, or `None` when it does not exist.
pub async fn player_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<Option<Player>, IdentityError> {
    let row = sqlx::query(
        "SELECT p.xuid, p.name, l.career_level, l.badge_id, l.player_title_id, l.tables_tran_id \
         FROM profile p LEFT JOIN game_logon l ON l.game = $1 AND l.profile_id = p.id \
         WHERE p.id = $2",
    )
    .bind(game)
    .bind(profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(Player {
        xuid: r.try_get("xuid")?,
        name: r.try_get("name")?,
        card: card_of(&r)?,
        tables_tran_id: r.try_get("tables_tran_id")?,
    }))
}

/// OnlineProfile.UpdateAll: merges `update` into the stored profile (a section whose value is an
/// empty list did not change and keeps the stored one, unless there is none yet) and returns the
/// merged profile. One statement: concurrent uploads serialize on the row. NULs are dropped.
pub async fn merge_profile_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    update: &Map<String, Json>,
) -> Result<Json, IdentityError> {
    let merged: Json = sqlx::query_scalar(
        "INSERT INTO game_profile (game, profile_id, data) VALUES ($1, $2, $3) \
         ON CONFLICT (game, profile_id) DO UPDATE SET \
           data = game_profile.data || COALESCE(( \
             SELECT jsonb_object_agg(e.key, e.value) FROM jsonb_each(EXCLUDED.data) AS e \
             WHERE NOT (jsonb_typeof(e.value) = 'array' AND jsonb_array_length(e.value) = 0) \
                OR NOT (game_profile.data ? e.key)), '{}'::jsonb), \
           updated_at = clock_timestamp(), row_version = game_profile.row_version + 1 \
         RETURNING data",
    )
    .bind(owner.game)
    .bind(owner.profile_id)
    .bind(Jsonb(storable(update).as_ref()))
    .fetch_one(&mut *conn)
    .await?;
    Ok(merged)
}

/// A number of the merged online profile `{alias}.data` (a joined `game_profile`) at `path`
/// (`{section,0,field}`), NULL when it is absent or not a number. Statement text only.
pub(crate) fn profile_number_sql(alias: &str, path: &str) -> String {
    format!(
        "(CASE WHEN jsonb_typeof({alias}.data #> '{path}') = 'number' THEN ({alias}.data #>> '{path}')::numeric END)"
    )
}

/// A profile's XP total from the game's own stored state (OD-131), as numeric SQL over a joined
/// `game_profile` `gp` and `game_logon` `gl`: the online profile's `playerRaceHistory[0].xp` when
/// above 0, else the Logon's `career_level` (the game reports its XP total there) when above 0,
/// else NULL. The drivatars' Noob XP and the dashboard's player card both read it, so they agree.
pub(crate) fn xp_total_sql(gp: &str, gl: &str) -> String {
    format!(
        "COALESCE(NULLIF({}, 0), NULLIF({gl}.career_level, 0)::numeric)",
        profile_number_sql(gp, "{playerRaceHistory,0,xp}")
    )
}

/// The stored online profile (`None` before the first UpdateAll).
pub async fn profile_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<Option<Json>, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT data FROM game_profile WHERE game = $1 AND profile_id = $2")
            .bind(game)
            .bind(profile_id)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// OnlineProfile.UpdateAllTables, one table: `rows` (by key) merged over the stored rows, then the
/// `deletes` keys removed. NULs are dropped from keys and strings.
pub async fn apply_table_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    name: &str,
    rows: &Map<String, Json>,
    deletes: &[String],
) -> Result<(), IdentityError> {
    let deletes: Cow<'_, [String]> = if deletes.iter().any(|d| d.contains('\0')) {
        Cow::Owned(deletes.iter().map(|d| d.replace('\0', "")).collect())
    } else {
        Cow::Borrowed(deletes)
    };
    sqlx::query(
        "INSERT INTO game_profile_table (game, profile_id, name, data) VALUES ($1, $2, $3, $4::jsonb - $5::text[]) \
         ON CONFLICT (game, profile_id, name) DO UPDATE SET \
           data = (game_profile_table.data || $4::jsonb) - $5::text[], updated_at = clock_timestamp()",
    )
    .bind(owner.game)
    .bind(owner.profile_id)
    .bind(name)
    .bind(Jsonb(storable(rows).as_ref()))
    .bind(deletes.as_ref())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The stored tables of a profile, `{name: [rows]}` in key order.
pub async fn tables_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<Map<String, Json>, IdentityError> {
    let rows = sqlx::query(
        "SELECT name, data FROM game_profile_table WHERE game = $1 AND profile_id = $2 ORDER BY name",
    )
    .bind(game)
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Map::new();
    for r in rows {
        let name: String = r.try_get("name")?;
        let data: Json = r.try_get("data")?;
        let list = match data {
            Json::Object(by_key) => by_key.into_iter().map(|(_, v)| v).collect(),
            _ => Vec::new(),
        };
        out.insert(name, Json::Array(list));
    }
    Ok(out)
}

/// The checkpoint of the last UpdateAllTables (its `transactionId`), which the next Logon reply
/// carries so the game resends nothing it already sent.
pub async fn set_tables_checkpoint_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    transaction_id: i32,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO game_logon (game, profile_id, tables_tran_id, last_session_id) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (game, profile_id) DO UPDATE SET tables_tran_id = EXCLUDED.tables_tran_id, \
           updated_at = clock_timestamp()",
    )
    .bind(owner.game)
    .bind(owner.profile_id)
    .bind(transaction_id)
    .bind(owner.session_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Keeps the reply of a transactional request (`X-TransactionId`) in the profile's ring of
/// [`REPLY_SLOTS`]: the slot already holding `transaction_id` (a retry: the latest reply wins), else
/// a free slot, else the oldest one, upserted by slot in one statement (the runtime role never
/// deletes). Returns `false` (nothing kept) when the reply is larger than [`MAX_REPLY`] or shorter
/// than an envelope.
pub async fn keep_reply_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    transaction_id: Uuid,
    method: &str,
    reply: &[u8],
) -> Result<bool, IdentityError> {
    if !(12..=MAX_REPLY).contains(&reply.len()) {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO game_transaction (game, profile_id, slot, transaction_id, method, reply, session_id) \
         VALUES ($1, $2, COALESCE( \
           (SELECT t.slot FROM game_transaction t \
              WHERE t.game = $1 AND t.profile_id = $2 AND t.transaction_id = $3), \
           (SELECT s.slot::smallint FROM generate_series(0, $7 - 1) AS s (slot) \
              WHERE NOT EXISTS (SELECT 1 FROM game_transaction t \
                WHERE t.game = $1 AND t.profile_id = $2 AND t.slot = s.slot) \
              ORDER BY s.slot LIMIT 1), \
           (SELECT t.slot FROM game_transaction t WHERE t.game = $1 AND t.profile_id = $2 \
              ORDER BY t.created_at, t.slot LIMIT 1)), \
           $3, $4, $5, $6) \
         ON CONFLICT (game, profile_id, slot) DO UPDATE SET transaction_id = EXCLUDED.transaction_id, \
           method = EXCLUDED.method, reply = EXCLUDED.reply, session_id = EXCLUDED.session_id, \
           created_at = clock_timestamp()",
    )
    .bind(owner.game)
    .bind(owner.profile_id)
    .bind(transaction_id)
    .bind(method)
    .bind(reply)
    .bind(owner.session_id)
    .bind(i32::from(REPLY_SLOTS))
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// The kept reply of a transaction of this profile.
pub async fn reply_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    transaction_id: Uuid,
) -> Result<Option<Vec<u8>>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT reply FROM game_transaction WHERE game = $1 AND profile_id = $2 AND transaction_id = $3",
    )
    .bind(game)
    .bind(profile_id)
    .bind(transaction_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Records the presence the profile's game posted (`state` one of `active`, `inactive`,
/// `offline`; `rich` already reduced to `[A-Za-z0-9_]{0,40}`).
pub async fn set_presence_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    state: &str,
    rich: &str,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO game_player_presence (game, profile_id, state, rich, session_id) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (game, profile_id) DO UPDATE SET state = EXCLUDED.state, rich = EXCLUDED.rich, \
           session_id = EXCLUDED.session_id, updated_at = clock_timestamp()",
    )
    .bind(owner.game)
    .bind(owner.profile_id)
    .bind(state)
    .bind(rich)
    .bind(owner.session_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The presence of the profile behind `xuid` in `game`, if its game ever posted one and the
/// profile is one others may see: active, named, its account not banned (as [`gamertags_in`]),
/// and of the viewer's `mode`: modes never meet (OD-52), so a Hardcore player never sees a Noob
/// one's presence and the reverse. Presence is transient, written for every tier, and stays
/// across a mode change (it is not cloud data: GAME_PLATFORM.md §3a "free state").
pub async fn presence_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    xuid: i64,
) -> Result<Option<Presence>, IdentityError> {
    let row = sqlx::query(
        "SELECT g.state, g.rich, g.updated_at, clock_timestamp() AS now \
         FROM game_player_presence g JOIN profile p ON p.id = g.profile_id \
           JOIN account a ON a.id = p.account_id \
         WHERE g.game = $1 AND p.game = $1 AND p.xuid = $2 AND p.mode = $3 AND p.status = 'active' \
           AND p.name IS NOT NULL \
           AND (a.banned_at IS NULL OR (a.banned_until IS NOT NULL AND a.banned_until <= clock_timestamp()))",
    )
    .bind(game)
    .bind(xuid)
    .bind(mode)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(Presence {
        state: r.try_get("state")?,
        rich: r.try_get("rich")?,
        updated_at: ms(r.try_get::<OffsetDateTime, _>("updated_at")?),
        now: ms(r.try_get::<OffsetDateTime, _>("now")?),
    }))
}

/// The XUIDs of the profiles sharing `profile_id`'s world in `game` (OD-64): the open worlds it
/// hosts or holds a live seat in (`issued` or `used`, MATCHMAKING_API.md §3.2), and in them the
/// host and every live seat; the caller itself excluded. A closed or lapsed world shares nothing.
/// At most [`MAX_WORLD_MATES`].
pub async fn world_mates_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<Vec<i64>, IdentityError> {
    let rows: Vec<i64> = sqlx::query_scalar(
        "WITH mine AS ( \
           SELECT w.id FROM world w \
            WHERE w.game = $1 AND w.host_profile_id = $2 AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
           UNION \
           SELECT w.id FROM mm_grant g JOIN world w ON w.id = g.world_id \
            WHERE g.game = $1 AND g.profile_id = $2 AND g.state IN ('issued', 'used') \
              AND w.closed_at IS NULL AND w.expires_at > clock_timestamp()) \
         SELECT x FROM ( \
           SELECT p.xuid AS x FROM world w JOIN profile p ON p.id = w.host_profile_id WHERE w.id IN (SELECT id FROM mine) \
           UNION \
           SELECT g.xuid AS x FROM mm_grant g WHERE g.world_id IN (SELECT id FROM mine) AND g.state IN ('issued', 'used')) m \
         WHERE x <> (SELECT xuid FROM profile WHERE id = $2) \
         LIMIT $3",
    )
    .bind(game)
    .bind(profile_id)
    .bind(MAX_WORLD_MATES)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// The most world mates [`world_mates_in`] reads (a world holds at most 12 players).
pub const MAX_WORLD_MATES: i64 = 16;

/// `(xuid, name)` of the active, unbanned profiles of `game` among `xuids` (at most
/// 100 are looked up).
pub async fn gamertags_in(
    conn: &mut PgConnection,
    game: &str,
    xuids: &[i64],
) -> Result<Vec<(i64, String)>, IdentityError> {
    if xuids.is_empty() {
        return Ok(Vec::new());
    }
    let wanted: Vec<i64> = xuids.iter().copied().take(100).collect();
    let rows = sqlx::query(
        "SELECT p.xuid, p.name FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.game = $1 AND p.xuid = ANY($2) AND p.status = 'active' AND p.name IS NOT NULL \
           AND (a.banned_at IS NULL OR (a.banned_until IS NOT NULL AND a.banned_until <= clock_timestamp()))",
    )
    .bind(game)
    .bind(&wanted)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push((r.try_get("xuid")?, r.try_get("name")?));
    }
    Ok(out)
}
