//! The players' drivatars (SER-M3 step 6; GAME_PLATFORM.md §3a "Supporter universe", BUG-0001):
//! `game_drivatar` (migrations `20261019120100_drivatars.sql`, `20261019120300_drivatar_model.sql`),
//! ported from the Beta's `save_drivatar`, `drivatar_owners` and `drivatar`
//! (`deprecated/beta/server/src/db.rs`), keyed by game, mode and profile. The model is opaque
//! here: the caller's checked model in the game's encoding (`model`, bytes).
//!
//! - [`save_in`]: Drivatar.SaveDrivatarInformation, the latest upload of a profile in a mode
//!   replacing the previous one (the caller records the request's `drivatar` fact first and
//!   writes only a new one).
//! - [`candidates_in`]: other players' drivatars for a race grid or the game's cache, in one
//!   query per group (friends, then everyone else), whatever the number of candidates.
//! - [`own_in`]: the caller's own drivatar, XP and difficulty (its race's `playerData`).
//!
//! **Served to others only when trained and with real progression (BUG-0001).** A candidate is
//! a drivatar of the viewer's game and **mode** (OD-130 restored it after OD-128 mixed them: a
//! Hardcore player never races Noob drivatars), the row of its profile's current mode (a profile
//! that moved leaves the other mode's row behind, unseen), visible as [`crate::visibility`] says, `trained` (decided by the
//! writer), whose owner's merged online profile (`game_profile`, OnlineProfile.UpdateAll) yields
//! an XP total above 0, and for which a car is found; anything else is left out, never
//! defaulted:
//!
//! - `wristbandLevel`, **Hardcore** (OD-142): the XP of the profile's last accepted
//!   `OnlineProfile.UpdateAll` of the mode (`integrity_baseline.xp`), nothing else: a Hardcore
//!   profile without an accepted sync is not served (its game profile and Logon claim are
//!   unchecked). **Noob**: the profile's `compareStats[0].wristbandLevel` (the game's XP total:
//!   it equals `playerRaceHistory[0].xp` in 163 of 165 captured syncs), or
//!   `playerRaceHistory[0].xp` when that is 0 or absent, else the Logon's career level (OD-131).
//!   Either way it must be above 0, capped at `u32::MAX`; the caller's own (`own_in`) follows
//!   the same rule;
//! - the car = the owner's garage car (`game_profile_table` `garageCars`) among the caller's
//!   `cars` with the most `numRaces` (ties: the lowest car id, then the garage key), else
//!   [`GAME_PICKS_CAR`] (`-1`): the game chooses the drivatar's car for the event itself, as it
//!   does for every drivatar of its shipped cache (`DefaultCache.bin`: all 125 carry `CarId -1`).
//!   Never one server-chosen car for everyone: the former fallback, the bucket's first hero car,
//!   put every drivatar without a car of the bucket in that same car (BUG-0003, 2026-10-31).
//!
//! **Bounded work.** A query looks at no more than the [`SCAN`] most recently trained drivatars
//! of the game and mode (the partial index `game_drivatar_recent`, walked in order and left
//! after the window: the statement is planned for its parameters on every call, never from a
//! cached generic plan, and the constant bound holds whatever the plan), keeps the first that pass
//! the cheap checks (visibility, skip, XP): a window of `limit`, cut where the models' bytes
//! (plus the caller's per-drivatar `overhead`) pass `budget`. Only that window's garages are
//! read (every drivatar of it has a car, its own or [`GAME_PICKS_CAR`]), and only its models.
//! A model's size costs no read (`octet_length` of a stored bytea). The asked cars go in as a
//! jsonb object, so a garage entry costs one key lookup, never a pass over the list. Serving fewer than asked is the price of the bound.

use crate::gameplane::Owner;
use crate::identity::{IdentityError, ms};
use crate::visibility::{VISIBLE, Viewer};
use fi_domain::UnixMs;
use serde_json::Value as Json;
use sqlx::types::Json as Jsonb;
use sqlx::{PgConnection, Row};
use std::sync::OnceLock;
use time::OffsetDateTime;
use uuid::Uuid;

/// The most curves a drivatar holds (`DrivatarRacingInfo.Curves`).
pub const MAX_CURVES: usize = 20;

/// The most segments a drivatar holds (`DrivatarRacingInfo.Segments`).
pub const MAX_SEGMENTS: usize = 200;

/// The largest stored model (the `game_drivatar_model_size` CHECK); a full one is under 10 KB.
pub const MAX_MODEL_BYTES: usize = 16_384;

/// The most XUIDs of a request's `cachedDrivatars` honoured (the game caches the 120 it asks
/// for; any beyond these are not excluded).
pub const MAX_SKIPPED: usize = 1024;

/// The most recently trained drivatars of a game and mode one candidates query looks at.
pub const SCAN: usize = 1024;

/// `DrivatarRacingInfo.CarId` of a drivatar whose owner's garage holds none of the asked cars:
/// the game picks the car for the event (its class, PI and restrictions, which the request does
/// not carry), as for the drivatars of its own shipped cache, which all carry it.
pub const GAME_PICKS_CAR: i64 = -1;

static CANDIDATES: OnceLock<String> = OnceLock::new();
static OWN: OnceLock<String> = OnceLock::new();

/// One upload as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    /// The checked model in the game's encoding (1 to [`MAX_MODEL_BYTES`] bytes).
    pub model: Vec<u8>,
    /// Whether the model holds trained data (served to others only then).
    pub trained: bool,
}

/// Another player's drivatar for a race or the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The owner's profile.
    pub profile_id: Uuid,
    /// The owner's XUID.
    pub xuid: i64,
    /// Gamertag name.
    pub name: String,
    /// Whether the owner is one of the viewer's friends.
    pub friend: bool,
    /// The car it brings: its owner's garage car among the asked ones, else [`GAME_PICKS_CAR`].
    pub car_id: i64,
    /// `wristbandLevel`: the owner's XP total (above 0): Hardcore, its accepted baseline only
    /// (OD-142); Noob, its online profile, else its Logon level.
    pub wristband: u32,
    /// The trained model, as stored.
    pub model: Vec<u8>,
}

/// What a race or the cache asks for.
#[derive(Clone, Copy, Debug)]
pub struct Wanted<'a> {
    /// Whose grid (game and mode, the caller left out; OD-130).
    pub viewer: Viewer<'a>,
    /// The viewer's friends' profiles, offered first (none until the friends graph, SER-M3 step
    /// 7).
    pub friends: &'a [Uuid],
    /// The garage cars a drivatar may bring (a race bucket's, or every bucketed car); one whose
    /// garage holds none of them brings [`GAME_PICKS_CAR`].
    pub cars: &'a [i64],
    /// XUIDs left out (the game's cached drivatars without a new car); at most [`MAX_SKIPPED`].
    pub skip: &'a [i64],
    /// At most this many.
    pub limit: usize,
    /// At most this many bytes of models, each counted with `overhead`.
    pub budget: usize,
    /// What a served drivatar takes beyond its stored model (its gamertag, ids, levels).
    pub overhead: usize,
}

/// Drivatar.SaveDrivatarInformation: replaces the profile's drivatar in `mode` with `upload`,
/// written by the fact `event_id`. One statement: concurrent uploads serialize on the row.
pub async fn save_in(
    conn: &mut PgConnection,
    owner: Owner<'_>,
    mode: &str,
    upload: &Upload,
    event_id: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO game_drivatar (game, mode, profile_id, model, trained, event_id, session_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (game, mode, profile_id) DO UPDATE SET model = EXCLUDED.model, \
           trained = EXCLUDED.trained, saves = game_drivatar.saves + 1, \
           trained_at = clock_timestamp(), event_id = EXCLUDED.event_id, session_id = EXCLUDED.session_id",
    )
    .bind(owner.game)
    .bind(mode)
    .bind(owner.profile_id)
    .bind(&upload.model)
    .bind(upload.trained)
    .bind(event_id)
    .bind(owner.session_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A number of the merged online profile `gp.data` at `path` (`{section,0,field}`), NULL when it
/// is absent or not a number.
fn number(path: &str) -> String {
    crate::gameplane::profile_number_sql("gp", path)
}

/// The owner's XP total in mode `$2`. **Hardcore** (OD-142, "HC só com nível verificado"): only
/// the XP of the last accepted `OnlineProfile.UpdateAll` of that mode (`ib`, the profile's
/// `integrity_baseline`), never the merged online profile or the Logon's claim; NULL without
/// one. **Noob**: over `gp` (the module's `wristbandLevel` rule), else the game's own career level
/// from its last Logon (`gl.career_level`, OD-131: a player who never synced still shows a real
/// level), NULL without either.
fn xp_total() -> String {
    format!(
        "(CASE WHEN $2 = 'hardcore' THEN NULLIF(ib.xp, 0)::numeric \
           ELSE COALESCE(NULLIF({wristband}, 0), {stored}) END)",
        wristband = number("{compareStats,0,wristbandLevel}"),
        stored = crate::gameplane::xp_total_sql("gp", "gl"),
    )
}

/// The candidates statement. `win`: the partial index walked (at most [`SCAN`] entries) through
/// the cheap checks, up to `$8` rows, cut where the running bytes (`size + $9` each) pass `$10`;
/// `picked`: the car of each, from its garage (`$5` the asked cars as a jsonb object), else
/// [`GAME_PICKS_CAR`]; then only their models read.
fn candidates_sql() -> String {
    format!(
        "WITH win AS MATERIALIZED ( \
           SELECT x.* FROM ( \
             SELECT d.profile_id, d.mode, d.trained_at, d.size, p.xuid, p.name, \
               LEAST(trunc(g.xp_total), 4294967295)::bigint AS wristband, \
               sum(d.size + $9) OVER (ORDER BY d.trained_at DESC, d.profile_id) AS upto \
             FROM (SELECT r.game, r.mode, r.profile_id, r.trained_at, octet_length(r.model)::bigint AS size \
                   FROM game_drivatar r \
                   WHERE r.game = $1 AND r.mode = $2 AND r.trained \
                   ORDER BY r.trained_at DESC, r.profile_id LIMIT {SCAN}) d \
             JOIN profile p ON p.id = d.profile_id \
             JOIN account a ON a.id = p.account_id \
             LEFT JOIN game_profile gp ON gp.game = d.game AND gp.profile_id = d.profile_id \
             LEFT JOIN game_logon gl ON gl.game = d.game AND gl.profile_id = d.profile_id \
             LEFT JOIN integrity_baseline ib ON ib.game = d.game AND ib.profile_id = d.profile_id \
               AND ib.mode = $2 AND ib.synced_at IS NOT NULL \
             CROSS JOIN LATERAL (SELECT {xp_total} AS xp_total) g \
             WHERE p.game = $1 AND p.mode = $2 AND {VISIBLE} AND d.profile_id <> $3 \
               AND NOT (p.xuid = ANY($6)) AND (p.id = ANY($4)) = $7 AND g.xp_total > 0 \
             ORDER BY d.trained_at DESC, d.profile_id \
             LIMIT $8) x \
           WHERE x.upto <= $10), \
         picked AS ( \
           SELECT w.profile_id, w.mode, w.trained_at, w.xuid, w.name, w.wristband, \
             COALESCE(c.car_id, {GAME_PICKS_CAR}) AS car_id \
           FROM win w \
           LEFT JOIN LATERAL ( \
             SELECT (e.value ->> 'carId')::bigint AS car_id \
             FROM game_profile_table t CROSS JOIN LATERAL jsonb_each(t.data) AS e \
             WHERE t.game = $1 AND t.profile_id = w.profile_id AND t.name = 'garageCars' \
               AND $5::jsonb ? (e.value ->> 'carId') AND jsonb_typeof(e.value -> 'carId') = 'number' \
             ORDER BY (CASE WHEN jsonb_typeof(e.value -> 'numRaces') = 'number' \
                         THEN (e.value ->> 'numRaces')::numeric ELSE 0 END) DESC, \
               (e.value ->> 'carId')::bigint, e.key \
             LIMIT 1) c ON true) \
         SELECT f.profile_id, f.xuid, f.name, f.wristband, f.car_id, d.model \
         FROM picked f \
         JOIN game_drivatar d ON d.game = $1 AND d.mode = f.mode AND d.profile_id = f.profile_id \
         ORDER BY f.trained_at DESC, f.profile_id",
        xp_total = xp_total(),
    )
}

/// Other players' drivatars for `wanted`: the viewer's friends first, then everyone else, each
/// group most recently trained first (ties by profile id), at most `wanted.limit` in all and
/// their models' bytes (each with `wanted.overhead`) within `wanted.budget`. Only trained
/// drivatars of visible profiles of the viewer's game and mode (OD-130), never the viewer's own, none in
/// `wanted.skip`, each with an XP total and a car (the module's rules), within the module's
/// bounded work.
pub async fn candidates_in(
    conn: &mut PgConnection,
    wanted: &Wanted<'_>,
) -> Result<Vec<Candidate>, IdentityError> {
    let mut out: Vec<Candidate> = Vec::new();
    if wanted.limit == 0 {
        return Ok(out);
    }
    let skip: Vec<i64> = wanted.skip.iter().copied().take(MAX_SKIPPED).collect();
    let cars = Json::Object(
        wanted
            .cars
            .iter()
            .map(|c| (c.to_string(), Json::Bool(true)))
            .collect(),
    );
    let sql = CANDIDATES.get_or_init(candidates_sql);
    let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    for friends in [true, false] {
        let room = wanted.limit.saturating_sub(out.len());
        let used: usize = out.iter().map(|c| c.model.len() + wanted.overhead).sum();
        let bytes = wanted.budget.saturating_sub(used);
        if room == 0 || bytes == 0 || (friends && wanted.friends.is_empty()) {
            continue;
        }
        // Built from the module's constants only, once. Planned for its parameters every time
        // (an unnamed statement, about 2 ms of planning): the plan then walks the partial index
        // and stops after the window, where a cached generic plan, estimating the game and mode
        // without their values, may scan and sort every drivatar of them first.
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .persistent(false)
            .bind(wanted.viewer.game)
            .bind(wanted.viewer.mode)
            .bind(wanted.viewer.profile_id)
            .bind(wanted.friends)
            .bind(Jsonb(&cars))
            .bind(&skip)
            .bind(friends)
            .bind(count(room))
            .bind(count(wanted.overhead))
            .bind(count(bytes))
            .fetch_all(&mut *conn)
            .await?;
        for r in rows {
            let wristband: i64 = r.try_get("wristband")?;
            out.push(Candidate {
                profile_id: r.try_get("profile_id")?,
                xuid: r.try_get("xuid")?,
                name: r.try_get("name")?,
                friend: friends,
                car_id: r.try_get("car_id")?,
                wristband: u32::try_from(wristband).unwrap_or(u32::MAX),
                model: r.try_get("model")?,
            });
        }
    }
    Ok(out)
}

/// The caller's side of a race grid or of the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Own {
    /// XUID.
    pub xuid: i64,
    /// Gamertag name (`None` once erased).
    pub name: Option<String>,
    /// Its stored model in the viewer's mode, trained or not.
    pub model: Option<Vec<u8>>,
    /// Its XP total by the module's `wristbandLevel` rule, when above 0.
    pub wristband: Option<u32>,
    /// The drivatar difficulty it races at (`assists[0].aiDiff`), when it synced an integer one
    /// (the caller bounds it).
    pub difficulty: Option<i32>,
    /// The database clock.
    pub now: UnixMs,
}

/// The viewer's own profile, drivatar (in its mode), XP and difficulty; `None` when the profile
/// does not exist.
pub async fn own_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
) -> Result<Option<Own>, IdentityError> {
    let sql = OWN.get_or_init(|| {
        format!(
            "SELECT p.xuid, p.name, clock_timestamp() AS now, d.model, \
               CASE WHEN g.xp_total > 0 THEN LEAST(trunc(g.xp_total), 4294967295)::bigint END AS wristband, \
               CASE WHEN g.ai = trunc(g.ai) AND g.ai BETWEEN -2147483648 AND 2147483647 \
                 THEN g.ai::integer END AS difficulty \
             FROM profile p \
             LEFT JOIN game_drivatar d ON d.game = $1 AND d.mode = $2 AND d.profile_id = p.id \
             LEFT JOIN game_profile gp ON gp.game = $1 AND gp.profile_id = p.id \
             LEFT JOIN game_logon gl ON gl.game = $1 AND gl.profile_id = p.id \
             LEFT JOIN integrity_baseline ib ON ib.game = $1 AND ib.profile_id = p.id \
               AND ib.mode = $2 AND ib.synced_at IS NOT NULL \
             LEFT JOIN LATERAL (SELECT {xp_total} AS xp_total, {ai} AS ai) g ON true \
             WHERE p.id = $3",
            xp_total = xp_total(),
            ai = number("{assists,0,aiDiff}"),
        )
    });
    // Built from the module's constants only, once.
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(viewer.profile_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(r) = row else { return Ok(None) };
    let wristband: Option<i64> = r.try_get("wristband")?;
    Ok(Some(Own {
        xuid: r.try_get("xuid")?,
        name: r.try_get("name")?,
        model: r.try_get("model")?,
        wristband: wristband.map(|w| u32::try_from(w).unwrap_or(u32::MAX)),
        difficulty: r.try_get("difficulty")?,
        now: ms(r.try_get::<OffsetDateTime, _>("now")?),
    }))
}

/// A profile's stored model in `mode`, with whether it is trained and how many uploads it took
/// (support and tests).
pub async fn drivatar_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    profile_id: Uuid,
) -> Result<Option<(Vec<u8>, bool, i64)>, IdentityError> {
    let row = sqlx::query(
        "SELECT model, trained, saves FROM game_drivatar \
         WHERE game = $1 AND mode = $2 AND profile_id = $3",
    )
    .bind(game)
    .bind(mode)
    .bind(profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some((
        r.try_get("model")?,
        r.try_get("trained")?,
        r.try_get("saves")?,
    )))
}
