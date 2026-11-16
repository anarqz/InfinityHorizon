//! The play dashboard's players (DASHBOARD_API.md §4, PD-3; served by `fi_surfaces::dashboard`):
//! the player list of a mode and one profile by its plain name.
//!
//! **Listed** ([`listed`]): the profile is visible as the game sees it
//! ([`crate::visibility::VISIBLE`]: active profile, active account, not banned), named, not
//! `social_hidden_at` and `web_listed`. A profile that is not listed is never in the list and its
//! name answers like a missing one.
//!
//! **The card**: the plain name, the Discord display name and whether an avatar exists (as the
//! boards' rows), the XP total and the driver level the game shows for it (the XP from
//! [`crate::gameplane::xp_total_sql`]: the online profile's `playerRaceHistory[0].xp`, else the
//! Logon's `career_level`, which holds the XP total, not a level; the level is
//! `fi_gamedb::gates::shown_level`, 1 to 999; neither without XP), whether an active play session exists now, the last time seen (the
//! latest of the last logon, the presence update and the end of the logon's last session) and the
//! club of the profile's mode unless private. No XUID, account id or Discord user id is selected.
//!
//! **Search** is a case-insensitive prefix of the plain name through `profile_name_prefix`
//! (`(game, lower(name) text_pattern_ops) WHERE status = 'active'`): the bound is written with the
//! `text_pattern_ops` operators (`~>=~`, `~<~`), which a generic plan still matches to the index
//! (a `LIKE $n || '%'` only does under a custom plan).

use crate::identity::{IdentityError, ms};
use fi_domain::UnixMs;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The condition on the joined `profile p` and `account a` of a profile the dashboard lists.
pub fn listed() -> String {
    format!(
        "({} AND p.name IS NOT NULL AND p.social_hidden_at IS NULL AND p.web_listed)",
        crate::visibility::VISIBLE
    )
}

/// The largest page of [`players_in`].
pub const MAX_PLAYERS: i64 = 100;

/// One player's card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardRow {
    /// The profile (server-side only: the stats and boards reads; never on the wire).
    pub profile_id: Uuid,
    /// The plain name.
    pub name: String,
    /// The linked Discord account's global name, else its username.
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The XP total (`None`: none stored, or 0).
    pub xp: Option<i64>,
    /// The driver level the game shows for `xp`, 1 to 999 (`None` without XP).
    pub level: Option<i64>,
    /// An active, unexpired, non-dedicated play session now.
    pub online: bool,
    /// The latest of the last logon, presence and the last session's end.
    pub last_seen: Option<UnixMs>,
    /// The club of its mode, unless private: `(tag, name)`.
    pub club: Option<(String, String)>,
    /// Profile created.
    pub since: UnixMs,
}

/// The order of [`players_in`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    /// Most XP first (the level's order, finer; none last), then by name. The XP is read from the
    /// joined online profile of every matching profile before the page is cut: no index serves
    /// it (a mode's listed profiles per request).
    Level,
    /// Most recently seen first (never seen last), then by name.
    Recent,
    /// By name (case-insensitive).
    Name,
}

impl Sort {
    fn order(self) -> &'static str {
        match self {
            Sort::Level => "xp DESC NULLS LAST, lower(p.name), p.id",
            Sort::Recent => "last_seen DESC NULLS LAST, lower(p.name), p.id",
            Sort::Name => "lower(p.name), p.id",
        }
    }
}

/// The card's columns over `profile p`, `account a`, `game_logon l`, `game_profile gp`,
/// `game_player_presence g`, the logon's last session `ls` and the club `c` ([`card_joins`]).
fn card_columns() -> String {
    format!(
        "p.id, p.name, p.mode, p.created_at, \
         CASE WHEN a.discord_user_id IS NOT NULL THEN coalesce(a.discord_global_name, a.discord_username) END AS discord_name, \
         (a.discord_avatar IS NOT NULL) AS avatar, \
         NULLIF(LEAST(trunc(COALESCE({xp}, 0)), 9223372036854775807), 0)::bigint AS xp, \
         EXISTS (SELECT 1 FROM play_session s WHERE s.profile_id = p.id AND s.state = 'active' \
           AND s.expires_at > clock_timestamp() AND NOT s.dedicated) AS online, \
         GREATEST(l.last_logon_at, g.updated_at, ls.ended_at) AS last_seen, \
         c.tag AS club_tag, c.name AS club_name",
        xp = crate::gameplane::xp_total_sql("gp", "l"),
    )
}

/// The card's joins (`p` and `a` joined by the caller).
fn card_joins() -> String {
    format!(
        "LEFT JOIN game_logon l ON l.game = p.game AND l.profile_id = p.id \
         LEFT JOIN game_profile gp ON gp.game = p.game AND gp.profile_id = p.id \
         LEFT JOIN play_session ls ON ls.id = l.last_session_id \
         LEFT JOIN game_player_presence g ON g.game = p.game AND g.profile_id = p.id \
         LEFT JOIN game_club_member m ON m.game = p.game AND m.profile_id = p.id \
         LEFT JOIN game_club c ON c.game = m.game AND c.id = m.club_id AND c.mode = p.mode \
           AND c.privacy <> {}",
        crate::clubs::PRIVACY_PRIVATE
    )
}

fn card(r: &PgRow) -> Result<CardRow, sqlx::Error> {
    let xp: Option<i64> = r.try_get::<Option<i64>, _>("xp")?.filter(|x| *x > 0);
    let tag: Option<String> = r.try_get("club_tag")?;
    let club_name: Option<String> = r.try_get("club_name")?;
    Ok(CardRow {
        profile_id: r.try_get("id")?,
        name: r.try_get("name")?,
        discord_name: r.try_get("discord_name")?,
        avatar: r.try_get("avatar")?,
        mode: r.try_get("mode")?,
        xp,
        level: xp.and_then(fi_gamedb::gates::shown_level),
        online: r.try_get("online")?,
        last_seen: r.try_get::<Option<OffsetDateTime>, _>("last_seen")?.map(ms),
        club: tag.zip(club_name),
        since: ms(r.try_get::<OffsetDateTime, _>("created_at")?),
    })
}

/// The listed players of `game` in `mode` whose name starts with `prefix` (lower case letters and
/// digits, as the caller validated it; `None`: all), in `sort` order, skipping `offset`, at most
/// `limit` (clamped to 0..=[`MAX_PLAYERS`]), and how many match in all. One statement.
pub async fn players_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    prefix: Option<&str>,
    sort: Sort,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<CardRow>), IdentityError> {
    let listed = listed();
    let joins = card_joins();
    let columns = card_columns();
    let order = sort.order();
    // Two statement texts, so a generic plan of the search still bounds the index: '{' is the byte
    // after 'z', so every lower-cased name with the prefix sorts in [prefix, prefix || '{').
    let search = if prefix.is_some() {
        "AND lower(p.name) ~>=~ $3 AND lower(p.name) ~<~ ($3 || '{') AND lower(p.name) LIKE $3 || '%'"
    } else {
        "AND $3::text IS NULL"
    };
    let sql = format!(
        "WITH hit AS ( \
           SELECT p.id FROM profile p JOIN account a ON a.id = p.account_id \
           WHERE p.game = $1 AND p.mode = $2 AND {listed} {search} \
         ), \
         total AS (SELECT count(*) AS total FROM hit) \
         SELECT t.total, x.* FROM total t \
         LEFT JOIN LATERAL ( \
           SELECT {columns} FROM hit h \
           JOIN profile p ON p.id = h.id JOIN account a ON a.id = p.account_id {joins} \
           ORDER BY {order} OFFSET $4 LIMIT $5 \
         ) x ON true"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(mode)
        .bind(prefix)
        .bind(offset.max(0))
        .bind(limit.clamp(0, MAX_PLAYERS))
        .fetch_all(&mut *conn)
        .await?;
    let mut total = 0;
    let mut out = Vec::new();
    for r in &rows {
        total = r.try_get("total")?;
        if r.try_get::<Option<Uuid>, _>("id")?.is_none() {
            continue;
        }
        out.push(card(r)?);
    }
    Ok((total, out))
}

/// The listed profile of `game` named `name` (case-insensitive, any mode); `None` for a missing
/// name and a profile that is not listed alike.
pub async fn player_in(
    conn: &mut PgConnection,
    game: &str,
    name: &str,
) -> Result<Option<CardRow>, IdentityError> {
    let listed = listed();
    let joins = card_joins();
    let columns = card_columns();
    let sql = format!(
        "SELECT {columns} FROM profile p JOIN account a ON a.id = p.account_id {joins} \
         WHERE p.game = $1 AND lower(p.name) = lower($2) AND {listed}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.as_ref().map(card).transpose()?)
}

/// Every listed player of `game` in `mode`, by name (the hourly snapshot, DASHBOARD_API.md §5).
/// One statement.
pub async fn every_player_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
) -> Result<Vec<CardRow>, IdentityError> {
    let listed = listed();
    let joins = card_joins();
    let columns = card_columns();
    let sql = format!(
        "SELECT {columns} FROM profile p JOIN account a ON a.id = p.account_id {joins} \
         WHERE p.game = $1 AND p.mode = $2 AND {listed} ORDER BY lower(p.name), p.id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(mode)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.iter().map(card).collect::<Result<_, _>>()?)
}

/// A Forza Motorsport 5 career's stored `profile_progress` numbers (GAME_PLATFORM.md §3a "Career
/// and scores (M7)"), each `None` when absent or out of range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// As the game sent it.
    pub level: Option<i32>,
    /// The XP total.
    pub xp: Option<i64>,
    /// Credits.
    pub credits: Option<i64>,
    /// The stored garage rows.
    pub cars: Option<i32>,
}

/// Every listed career of `game` in any mode (DASHBOARD_API.md §5.3, FM5) with its card and its
/// `profile_progress` row, by name. The card's `xp` and `level` are FH2's derivation; the caller
/// replaces them with the progress numbers. One statement.
pub async fn every_career_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Vec<(CardRow, Progress)>, IdentityError> {
    let listed = listed();
    let joins = card_joins();
    let columns = card_columns();
    let sql = format!(
        "SELECT {columns}, pp.level AS pp_level, pp.xp AS pp_xp, pp.credits AS pp_credits, \
           pp.cars AS pp_cars \
         FROM profile p JOIN account a ON a.id = p.account_id {joins} \
         LEFT JOIN profile_progress pp ON pp.profile_id = p.id \
         WHERE p.game = $1 AND {listed} ORDER BY lower(p.name), p.id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .fetch_all(&mut *conn)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let progress = Progress {
            level: r.try_get("pp_level")?,
            xp: r.try_get("pp_xp")?,
            credits: r.try_get("pp_credits")?,
            cars: r.try_get("pp_cars")?,
        };
        out.push((card(r)?, progress));
    }
    Ok(out)
}

/// One stored FM5 best of [`fm5_bests_in`].
#[derive(Clone, Debug, PartialEq)]
pub struct Fm5BestRow {
    /// The career (server-side only).
    pub profile_id: Uuid,
    /// 0 time, 1 drift.
    pub score_type: i32,
    /// `scoreboardType`.
    pub scoreboard_type: i32,
    /// `gameScoreboardId`.
    pub game_scoreboard_id: i32,
    /// `trackId`.
    pub track_id: i32,
    /// `carId`.
    pub car_id: i32,
    /// `carClassId`.
    pub car_class_id: i16,
    /// `carDriveTypeId`.
    pub car_drive_type_id: i16,
    /// The score.
    pub score: f32,
    /// `isClean`.
    pub is_clean: bool,
    /// Stored.
    pub submitted_at: UnixMs,
}

/// The stored `fm5_score` bests of `profiles`, by profile then track, car, class, drive type,
/// scoreboard and type (a stable order). One statement.
pub async fn fm5_bests_in(
    conn: &mut PgConnection,
    profiles: &[Uuid],
) -> Result<Vec<Fm5BestRow>, IdentityError> {
    let rows = sqlx::query(
        "SELECT profile_id, score_type, scoreboard_type, game_scoreboard_id, track_id, car_id, \
           car_class_id, car_drive_type_id, score, is_clean, submitted_at FROM fm5_score \
         WHERE game = 'fm5' AND profile_id = ANY($1::uuid[]) \
         ORDER BY profile_id, track_id, car_id, car_class_id, car_drive_type_id, scoreboard_type, \
           game_scoreboard_id, score_type",
    )
    .bind(profiles)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(Fm5BestRow {
                profile_id: r.try_get("profile_id")?,
                score_type: r.try_get("score_type")?,
                scoreboard_type: r.try_get("scoreboard_type")?,
                game_scoreboard_id: r.try_get("game_scoreboard_id")?,
                track_id: r.try_get("track_id")?,
                car_id: r.try_get("car_id")?,
                car_class_id: r.try_get("car_class_id")?,
                car_drive_type_id: r.try_get("car_drive_type_id")?,
                score: r.try_get("score")?,
                is_clean: r.try_get("is_clean")?,
                submitted_at: ms(r.try_get::<OffsetDateTime, _>("submitted_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// The stored online profiles (`game_profile.data`) of `profiles` in `game`, by profile. One
/// statement.
pub async fn profile_data_in(
    conn: &mut PgConnection,
    game: &str,
    profiles: &[Uuid],
) -> Result<Vec<(Uuid, serde_json::Value)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT profile_id, data FROM game_profile WHERE game = $1 AND profile_id = ANY($2::uuid[])",
    )
    .bind(game)
    .bind(profiles)
    .fetch_all(&mut *conn)
    .await?)
}

/// Every profile of `game` listed on the website now ([`listed`]) with its current lower-case
/// plain name, by name (DASHBOARD_API.md §5.1 `GET /listed`: the site's allowlist over the
/// snapshot; the profile id stays server-side, the route sends its ref). One statement.
pub async fn listed_profiles_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Vec<(Uuid, String)>, IdentityError> {
    let listed = listed();
    let sql = format!(
        "SELECT p.id, lower(p.name) FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.game = $1 AND {listed} ORDER BY lower(p.name)"
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .fetch_all(&mut *conn)
        .await?)
}

/// The most garage entries kept per profile (after the catalog filter and the per-car merge,
/// FH2 has 354 cars; the cap holds whatever the stored table grew to).
pub const MAX_GARAGE: i64 = 512;

/// The garages (`game_profile_table` `garageCars`: each row's `carId` and `numRaces`) of
/// `profiles` in `game`, as `(profile, car id, races)`: only car ids in `cars` (the game's catalog),
/// one entry per car with its races summed (each row's races held to 0 and above, the sum to
/// 2^31-1), at most [`MAX_GARAGE`] per profile, by car id. The table is merged upload after upload
/// and a Noob upload is not assessed, so its size is the client's choice: the filter, the merge and
/// the cap run in the statement, before anything reaches memory. One statement.
pub async fn garages_in(
    conn: &mut PgConnection,
    game: &str,
    profiles: &[Uuid],
    cars: &[i32],
) -> Result<Vec<(Uuid, i64, i64)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT profile_id, car_id, races FROM ( \
           SELECT t.profile_id, n.car::bigint AS car_id, \
             LEAST(sum(GREATEST(n.races, 0)), 2147483647)::bigint AS races, \
             row_number() OVER (PARTITION BY t.profile_id ORDER BY n.car) AS k \
           FROM game_profile_table t CROSS JOIN LATERAL jsonb_each(t.data) AS e \
           CROSS JOIN LATERAL (SELECT \
             CASE WHEN jsonb_typeof(e.value -> 'carId') = 'number' THEN \
               CASE WHEN (e.value ->> 'carId')::numeric BETWEEN 1 AND 2147483647 \
                 THEN trunc((e.value ->> 'carId')::numeric)::integer END END AS car, \
             COALESCE(CASE WHEN jsonb_typeof(e.value -> 'numRaces') = 'number' \
               THEN trunc(LEAST((e.value ->> 'numRaces')::numeric, 2147483647)) END, 0) AS races) n \
           WHERE t.game = $1 AND t.name = 'garageCars' AND t.profile_id = ANY($2::uuid[]) \
             AND n.car = ANY($3::integer[]) \
           GROUP BY t.profile_id, n.car \
         ) g WHERE k <= $4 ORDER BY profile_id, car_id",
    )
    .bind(game)
    .bind(profiles)
    .bind(cars)
    .bind(MAX_GARAGE)
    .fetch_all(&mut *conn)
    .await?)
}
