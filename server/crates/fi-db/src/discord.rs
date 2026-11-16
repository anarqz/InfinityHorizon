//! The Discord bot's server side (CORE_API.md §5a, register CR-S63; owner-decisions 2026-10-27
//! "Discord bot: channels, language and opt-out" and its refinement): the `public_event` feed, the
//! verified Discord link of an account and each profile's announce switch. Migration
//! `20261027220000_discord_feed.sql`.
//!
//! - **Writers** append one `public_event` row on the caller's transaction, next to the action
//!   that causes it ([`first_admission_in`], [`lobby_opened_in`], [`staff_event_in`]). The server
//!   never calls Discord.
//! - **Opt-out:** a profile whose `profile_feed.announce` is false gets no `public` row, and
//!   [`feed_in`] drops kept `public` rows whose subject turned it off or is gone. `staff` rows
//!   always name.
//! - **Order:** [`feed_in`] orders by `(tx, id)` and returns only rows whose transaction is older
//!   than every running one (`pg_snapshot_xmin`), so a late commit is never skipped.
//! - **Link:** one verified Discord user per account, at most one account per Discord user
//!   ([`LinkError::Taken`]); codes are kept as SHA-256 only.
//! - **Records** (phase 2, CR-S64): [`record_scope`] says whether a score's board is a record
//!   board, [`record_leader_in`] locks the board and reads its #1 before the score is stored, and
//!   [`record_in`] writes `speed_record` / `route_record` when the score took #1 from another
//!   player (the previous holder is the row's actor).

use crate::identity::{IdentityError, ms, ts};
use crate::scores::{BoardKey, lower_is_better};
use crate::visibility::{VISIBLE, Viewer};
use fi_domain::UnixMs;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// A link code lives 10 minutes.
pub const LINK_CODE_TTL_SECS: i64 = 600;
/// At most one `lobby_opened` per host profile per 10 minutes.
pub const LOBBY_EVENT_GAP_SECS: i64 = 600;
/// Feed rows are kept 7 days (`fi_db::retention`).
pub const FEED_DAYS: i32 = 7;
/// The largest feed page.
pub const FEED_MAX: i64 = 200;

/// At most one record row per subject per board per 10 minutes.
pub const RECORD_GAP_SECS: i64 = 600;
/// At most this many record rows per account (all its profiles) per hour.
pub const RECORD_HOURLY_MAX: i64 = 6;
/// A subject whose account has an integrity strike (not overturned) or a refused or flagged
/// assessment, on any of its profiles, in this many days is not announced.
pub const RECORD_CLEAN_DAYS: i64 = 30;

/// The SQL of an account's display profile (`$1` the account): its live profile with the latest
/// admitted play session, else its oldest live profile (CORE_API.md §5a "Display profile").
const DISPLAY_PROFILE: &str = "SELECT p.id FROM profile p WHERE p.account_id = $1 \
       AND p.status <> 'deleted' AND p.name IS NOT NULL \
     ORDER BY (SELECT max(s.admitted_at) FROM play_session s WHERE s.profile_id = p.id) DESC NULLS LAST, \
       p.created_at, p.id LIMIT 1";

/// A `staff` kind (#moderation-chat).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaffKind {
    /// A moderator's kick (CR-S62).
    Kicked,
    /// An admin's ban (`POST /core/v1/admin/bans`).
    Banned,
    /// A community voice mute started (CR-S68, CORE_API.md §4.4); no actor.
    VoiceCommunityMuted,
    /// A moderator's voice ban (`POST /core/v1/admin/voice-bans`, CR-S68).
    VoiceBanned,
    /// A moderator lifted a voice block (`DELETE /core/v1/admin/voice-bans/{accountId}`, CR-S68).
    VoiceUnbanned,
}

impl StaffKind {
    fn as_str(self) -> &'static str {
        match self {
            StaffKind::Kicked => "player_kicked",
            StaffKind::Banned => "player_banned",
            StaffKind::VoiceCommunityMuted => "voice_community_muted",
            StaffKind::VoiceBanned => "voice_banned",
            StaffKind::VoiceUnbanned => "voice_unbanned",
        }
    }
}

/// A profile's first non-dedicated admission (`player_joined`): marks it once and, when the
/// profile announces, writes the `public` row. Call it on the admission's transaction right after
/// the session became `active`. `true` when a row was written.
pub async fn first_admission_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<bool, sqlx::Error> {
    let written: Option<i64> = sqlx::query_scalar(
        "WITH f AS ( \
           INSERT INTO profile_feed (profile_id, first_admitted_at) VALUES ($1, clock_timestamp()) \
           ON CONFLICT (profile_id) DO UPDATE SET first_admitted_at = clock_timestamp() \
             WHERE profile_feed.first_admitted_at IS NULL \
           RETURNING announce) \
         INSERT INTO public_event (game, kind, audience, subject_account, subject_profile, subject_name) \
         SELECT p.game, 'player_joined', 'public', p.account_id, p.id, p.name \
           FROM f, profile p WHERE p.id = $1 AND f.announce AND p.name IS NOT NULL \
         RETURNING id",
    )
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(written.is_some())
}

/// A public (`everyone`) world opened by `profile` (`lobby_opened`): written when the profile
/// announces and it has no such row in the last [`LOBBY_EVENT_GAP_SECS`]. `true` when written.
pub async fn lobby_opened_in(conn: &mut PgConnection, profile: Uuid) -> Result<bool, sqlx::Error> {
    let written: Option<i64> = sqlx::query_scalar(
        "INSERT INTO public_event (game, kind, audience, subject_account, subject_profile, subject_name) \
         SELECT p.game, 'lobby_opened', 'public', p.account_id, p.id, p.name FROM profile p \
         WHERE p.id = $1 AND p.name IS NOT NULL \
           AND COALESCE((SELECT f.announce FROM profile_feed f WHERE f.profile_id = p.id), true) \
           AND NOT EXISTS (SELECT 1 FROM public_event e WHERE e.subject_profile = p.id \
             AND e.kind = 'lobby_opened' AND e.at > clock_timestamp() - make_interval(secs => $2)) \
         RETURNING id",
    )
    .bind(profile)
    .bind(LOBBY_EVENT_GAP_SECS as f64)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(written.is_some())
}

/// The account's display profile (CORE_API.md §5a), if any.
pub async fn display_profile_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(DISPLAY_PROFILE)
        .bind(account)
        .fetch_optional(&mut *conn)
        .await
}

/// A `staff` row (a kick, a ban, or a voice block, CR-S68): the subject is the account's live
/// profile `subject_xuid` (an action by XUID) or else the account's display profile; the actor is
/// the moderator's display profile (`None`: no actor, a community mute). Always written, whatever
/// `announce` says (#moderation-chat names its subjects).
pub async fn staff_event_in(
    conn: &mut PgConnection,
    kind: StaffKind,
    subject_account: Uuid,
    subject_xuid: Option<u64>,
    actor_account: Option<Uuid>,
    until: Option<UnixMs>,
) -> Result<(), sqlx::Error> {
    let by_xuid: Option<Uuid> = match subject_xuid.and_then(|x| i64::try_from(x).ok()) {
        Some(x) => sqlx::query_scalar(
            "SELECT id FROM profile WHERE xuid = $1 AND account_id = $2 AND status <> 'deleted' \
               AND name IS NOT NULL ORDER BY created_at, id LIMIT 1",
        )
        .bind(x)
        .bind(subject_account)
        .fetch_optional(&mut *conn)
        .await?,
        None => None,
    };
    let subject = match by_xuid {
        Some(p) => Some(p),
        None => display_profile_in(conn, subject_account).await?,
    };
    let actor = match actor_account {
        Some(a) => display_profile_in(conn, a).await?,
        None => None,
    };
    sqlx::query(
        "INSERT INTO public_event (game, kind, audience, subject_account, subject_profile, subject_name, \
           actor_account, actor_profile, actor_name, until) \
         SELECT sp.game, $1, 'staff', $2, sp.id, sp.name, $4, ap.id, ap.name, $6 \
           FROM (SELECT 1) one \
           LEFT JOIN profile sp ON sp.id = $3 \
           LEFT JOIN profile ap ON ap.id = $5",
    )
    .bind(kind.as_str())
    .bind(subject_account)
    .bind(subject)
    .bind(actor_account)
    .bind(actor)
    .bind(until.map(ts))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// Records (CR-S64) ----------------------------------------------------------------------------

/// What a record board is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordBoard {
    /// `scoreboardType` 9, `scoreType` 5: one speed trap camera.
    SpeedTrap,
    /// `scoreboardType` 9, `scoreType` 6: one speed zone camera.
    SpeedZone,
    /// `scoreType` 1 on a track: the route (every event on the track).
    Route,
}

impl RecordBoard {
    /// The `record.board` value.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordBoard::SpeedTrap => "speed_trap",
            RecordBoard::SpeedZone => "speed_zone",
            RecordBoard::Route => "route",
        }
    }

    /// The row's kind.
    pub fn kind(self) -> &'static str {
        match self {
            RecordBoard::Route => "route_record",
            _ => "speed_record",
        }
    }
}

/// A record board: the board the game shows for a score (`GetRows`; [`crate::scores`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordScope {
    /// What it is.
    pub board: RecordBoard,
    /// `scoreboardType`.
    pub scoreboard_type: i32,
    /// `scoreType`.
    pub score_type: i32,
    /// The camera (`gameScoreboardId`) of a speed board, the track of a route.
    pub id: i32,
    /// The score's own event (`gameScoreboardId`).
    pub board_id: i32,
}

impl RecordScope {
    /// The throttle's and the lock's board key.
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.board.as_str(),
            self.scoreboard_type,
            self.score_type,
            self.id
        )
    }

    /// A score in the unit the record carries: whole milliseconds for a time, m/s for a speed.
    pub fn value(&self, score: f64) -> serde_json::Value {
        if self.board == RecordBoard::Route {
            serde_json::Value::from((score * 1000.0).round() as i64)
        } else {
            serde_json::Value::from(score)
        }
    }

    /// Whether `new` beats `old` by at least what the bot shows: 1 ms for a time, 0.1 km/h for a
    /// speed (the bot rounds `m/s × 36` to whole tenths the same way).
    pub fn shown_better(&self, new: f64, old: f64) -> bool {
        if self.board == RecordBoard::Route {
            ((new * 1000.0).round() as i64) < ((old * 1000.0).round() as i64)
        } else {
            ((new * 36.0).round() as i64) > ((old * 36.0).round() as i64)
        }
    }

    /// Whether the board is one the game has: a speed trap camera 1-35 or a speed zone 36-55
    /// (`media/EntityModel.zip` speed_cameras*.xml, Storm Island included), or a route on a track
    /// `known` names (the catalog's). A made-up board is stored as usual, never announced.
    pub fn real(&self, known: impl FnOnce(i32) -> bool) -> bool {
        match self.board {
            RecordBoard::SpeedTrap => (1..=35).contains(&self.id),
            RecordBoard::SpeedZone => (36..=55).contains(&self.id),
            RecordBoard::Route => known(self.id),
        }
    }
}

/// The record board of a score's key, `None` when its board has no records: speed boards
/// (`scoreboardType` 9, `scoreType` 5 or 6) and race times on a track (`scoreType` 1,
/// `trackId` ≥ 0).
pub fn record_scope(key: &BoardKey) -> Option<RecordScope> {
    let board = match (key.scoreboard_type, key.score_type) {
        (9, 5) => RecordBoard::SpeedTrap,
        (9, 6) => RecordBoard::SpeedZone,
        (_, 1) if key.track_id >= 0 => RecordBoard::Route,
        _ => return None,
    };
    Some(RecordScope {
        board,
        scoreboard_type: key.scoreboard_type,
        score_type: key.score_type,
        id: if board == RecordBoard::Route {
            key.track_id
        } else {
            key.game_scoreboard_id
        },
        board_id: key.game_scoreboard_id,
    })
}

/// A board's #1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leader {
    /// The holder.
    pub profile_id: Uuid,
    /// Their score (the game's unit: seconds or m/s).
    pub score: f64,
}

async fn leader_of(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    scope: &RecordScope,
) -> Result<Option<Leader>, sqlx::Error> {
    // The visible board's #1 in the board's own order (`scores::best_cte`): the best score, the
    // earlier one on a tie. Two statement texts, each a plain equality on its index.
    let on = if scope.board == RecordBoard::Route {
        "s.track_id = $5"
    } else {
        "s.game_scoreboard_id = $5"
    };
    let sql = format!(
        "SELECT s.profile_id, s.score FROM game_score s \
         JOIN profile p ON p.id = s.profile_id \
         JOIN account a ON a.id = p.account_id \
         WHERE s.game = $1 AND s.mode = $2 AND s.scoreboard_type = $3 AND s.score_type = $4 AND {on} \
           AND p.mode = s.mode AND {VISIBLE} \
         ORDER BY CASE WHEN $6 THEN s.score ELSE -s.score END, s.submitted_at, s.profile_id LIMIT 1"
    );
    let row: Option<(Uuid, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(scope.scoreboard_type)
        .bind(scope.score_type)
        .bind(scope.id)
        .bind(lower_is_better(scope.score_type))
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.map(|(profile_id, score)| Leader { profile_id, score }))
}

/// Locks the record board for the rest of the transaction (two scores on one board are decided
/// one after the other) and reads its #1 before the score is stored.
pub async fn record_leader_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    scope: &RecordScope,
) -> Result<Option<Leader>, sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "fi.record:{}:{}:{}",
            viewer.game,
            viewer.mode,
            scope.key()
        ))
        .execute(&mut *conn)
        .await?;
    leader_of(conn, viewer, scope).await
}

/// The record's catalog names and car.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordNames<'a> {
    /// `carId`.
    pub car_id: i32,
    /// The car's name (`None`: not in the catalog).
    pub car_name: Option<&'a str>,
    /// The route's name (`None`: a speed board, or not in the catalog).
    pub route_name: Option<&'a str>,
}

/// After the viewer's `score` was stored as their best on `scope`: writes the record row when the
/// board's #1 was `before`, another profile, and is now the viewer with this score, strictly
/// better in the shown unit; the subject announces, has a clean integrity record
/// ([`RECORD_CLEAN_DAYS`]) and is within the throttle ([`RECORD_GAP_SECS`],
/// [`RECORD_HOURLY_MAX`]). The previous holder is the actor, named only when they announce. The
/// caller holds the board's lock ([`record_leader_in`]). `true` when written.
pub async fn record_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    scope: &RecordScope,
    before: Option<Leader>,
    score: f64,
    names: RecordNames<'_>,
) -> Result<bool, sqlx::Error> {
    let Some(before) = before else {
        return Ok(false);
    };
    if before.profile_id == viewer.profile_id || !scope.shown_better(score, before.score) {
        return Ok(false);
    }
    match leader_of(conn, viewer, scope).await? {
        Some(now) if now.profile_id == viewer.profile_id && now.score == score => {}
        _ => return Ok(false),
    }
    // The hourly cap is per account: its record rows are counted one request at a time (after the
    // board's lock, always in this order).
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("fi.record.account:{}", viewer.account_id))
        .execute(&mut *conn)
        .await?;
    let route = scope.board == RecordBoard::Route;
    let record = serde_json::json!({
        "key": scope.key(),
        "board": scope.board.as_str(),
        "mode": viewer.mode,
        "unit": if route { "time_ms" } else { "speed_mps" },
        "value": scope.value(score),
        "previousValue": scope.value(before.score),
        "carId": names.car_id,
        "carName": names.car_name,
        "boardId": scope.board_id,
        "routeId": if route { Some(scope.id) } else { None },
        "routeName": if route { names.route_name } else { None },
    });
    let written: Option<i64> = sqlx::query_scalar(
        "INSERT INTO public_event (game, kind, audience, subject_account, subject_profile, subject_name, \
           actor_account, actor_profile, actor_name, record) \
         SELECT p.game, $3, 'public', p.account_id, p.id, p.name, r.account_id, r.id, \
           CASE WHEN r.status = 'active' AND r.name IS NOT NULL \
             AND COALESCE((SELECT f.announce FROM profile_feed f WHERE f.profile_id = r.id), true) \
             THEN r.name END, $4 \
         FROM profile p JOIN profile r ON r.id = $2 \
         WHERE p.id = $1 AND p.status = 'active' AND p.name IS NOT NULL AND r.account_id <> p.account_id \
           AND COALESCE((SELECT f.announce FROM profile_feed f WHERE f.profile_id = p.id), true) \
           AND NOT EXISTS (SELECT 1 FROM integrity_strike k JOIN profile kp ON kp.id = k.profile_id \
             WHERE kp.account_id = p.account_id \
             AND k.created_at > clock_timestamp() - make_interval(days => $7) \
             AND k.outcome IS DISTINCT FROM 'overturned') \
           AND NOT EXISTS (SELECT 1 FROM integrity_assessment x JOIN profile xp ON xp.id = x.profile_id \
             WHERE xp.account_id = p.account_id AND x.game = p.game \
             AND x.decision IN ('refused', 'flagged') \
             AND x.at > clock_timestamp() - make_interval(days => $7)) \
           AND NOT EXISTS (SELECT 1 FROM public_event e WHERE e.subject_profile = p.id AND e.kind = $3 \
             AND e.at > clock_timestamp() - make_interval(secs => $5) AND e.record ->> 'key' = $8) \
           AND (SELECT count(*) FROM public_event e WHERE e.subject_account = p.account_id \
             AND e.kind IN ('speed_record', 'route_record') \
             AND e.at > clock_timestamp() - interval '1 hour') < $6 \
         RETURNING id",
    )
    .bind(viewer.profile_id)
    .bind(before.profile_id)
    .bind(scope.board.kind())
    .bind(sqlx::types::Json(&record))
    .bind(RECORD_GAP_SECS as f64)
    .bind(RECORD_HOURLY_MAX)
    .bind(RECORD_CLEAN_DAYS as i32)
    .bind(scope.key())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(written.is_some())
}

// The feed ------------------------------------------------------------------------------------

/// A feed position: the row's transaction id and row id (on the wire `"<tx>.<id>"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    /// `public_event.tx` (xid8).
    pub tx: u64,
    /// `public_event.id`.
    pub id: i64,
}

impl Cursor {
    /// Parses `"<tx>.<id>"` (both decimal); `None` otherwise.
    pub fn parse(text: &str) -> Option<Self> {
        let (tx, id) = text.split_once('.')?;
        let canonical =
            |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
        if !canonical(tx) || !canonical(id) {
            return None;
        }
        Some(Self {
            tx: tx.parse().ok()?,
            id: id.parse().ok().filter(|v: &i64| *v > 0)?,
        })
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.tx, self.id)
    }
}

/// A verified Discord user of an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordUserRow {
    /// The snowflake.
    pub id: i64,
    /// The unique handle.
    pub username: String,
    /// The display name.
    pub global_name: Option<String>,
    /// The avatar hash (`None`: Discord's default avatar).
    pub avatar: Option<String>,
    /// When linked.
    pub linked_at: UnixMs,
    /// When the handle was last read from Discord.
    pub refreshed_at: UnixMs,
}

/// A person of a feed row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedPerson {
    /// The gamertag at that moment (`None`: the account had no live profile).
    pub name: Option<String>,
    /// The account's verified Discord user now.
    pub discord: Option<DiscordUserRow>,
}

/// A feed row as the bot reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedRow {
    /// Its position.
    pub cursor: Cursor,
    /// When written.
    pub at: UnixMs,
    /// The game (`None` for an account-level row without a display profile).
    pub game: Option<String>,
    /// `player_joined` | `lobby_opened` | `player_kicked` | `player_banned` | `speed_record` |
    /// `route_record`.
    pub kind: String,
    /// `public` | `staff`.
    pub audience: String,
    /// Who it is about.
    pub subject: FeedPerson,
    /// Who did it (staff rows), or the previous holder of a record (`None` when hidden).
    pub actor: Option<FeedPerson>,
    /// A ban's end or a kick's cooldown.
    pub until: Option<UnixMs>,
    /// A record's values (`speed_record`, `route_record`).
    pub record: Option<FeedRecord>,
}

/// A record row's values (CORE_API.md §5a "Records").
#[derive(Clone, Debug, PartialEq)]
pub struct FeedRecord {
    /// `speed_trap` | `speed_zone` | `route`.
    pub board: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// `speed_mps` | `time_ms`.
    pub unit: String,
    /// The new record.
    pub value: f64,
    /// The record it beat.
    pub previous_value: f64,
    /// `carId`.
    pub car_id: i32,
    /// The car's catalog name.
    pub car_name: Option<String>,
    /// `gameScoreboardId`.
    pub board_id: i32,
    /// The track of a route.
    pub route_id: Option<i32>,
    /// The route's catalog name.
    pub route_name: Option<String>,
}

impl FeedRecord {
    /// The record as written by [`record_in`].
    fn of(v: &serde_json::Value) -> Result<Self, sqlx::Error> {
        let bad = || sqlx::Error::Decode("public_event.record".into());
        let text = |k: &str| v[k].as_str().map(str::to_owned).ok_or_else(bad);
        let num = |k: &str| v[k].as_f64().filter(|f| f.is_finite()).ok_or_else(bad);
        let int = |k: &str| v[k].as_i64().and_then(|n| i32::try_from(n).ok());
        Ok(Self {
            board: text("board")?,
            mode: text("mode")?,
            unit: text("unit")?,
            value: num("value")?,
            previous_value: num("previousValue")?,
            car_id: int("carId").ok_or_else(bad)?,
            car_name: v["carName"].as_str().map(str::to_owned),
            board_id: int("boardId").ok_or_else(bad)?,
            route_id: int("routeId"),
            route_name: v["routeName"].as_str().map(str::to_owned),
        })
    }
}

/// One page of the feed.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedPage {
    /// Visible rows, oldest first.
    pub items: Vec<FeedRow>,
    /// The last row examined (or `after` on an empty page).
    pub next: Option<Cursor>,
    /// `after` names a row no longer kept.
    pub gap: bool,
}

fn discord_of(r: &PgRow, p: &str) -> Result<Option<DiscordUserRow>, sqlx::Error> {
    let id: Option<i64> = r.try_get(format!("{p}_id").as_str())?;
    let Some(id) = id else { return Ok(None) };
    Ok(Some(DiscordUserRow {
        id,
        username: r.try_get(format!("{p}_user").as_str())?,
        global_name: r.try_get(format!("{p}_global").as_str())?,
        avatar: r.try_get(format!("{p}_avatar").as_str())?,
        linked_at: ms(r.try_get(format!("{p}_linked").as_str())?),
        refreshed_at: ms(r.try_get(format!("{p}_refreshed").as_str())?),
    }))
}

/// The feed after `after` (CORE_API.md §5a.1): at most `limit` rows ([`FEED_MAX`]) in `(tx, id)`
/// order, only rows whose transaction precedes every running one; `public` rows whose subject no
/// longer announces (or is gone) are examined and skipped. Discord users are read now.
pub async fn feed_in(
    conn: &mut PgConnection,
    after: Option<Cursor>,
    limit: i64,
) -> Result<FeedPage, sqlx::Error> {
    let limit = limit.clamp(1, FEED_MAX);
    let gap =
        match after {
            Some(c) => !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM public_event WHERE tx = $1::text::xid8 AND id = $2)",
            )
            .bind(c.tx.to_string())
            .bind(c.id)
            .fetch_one(&mut *conn)
            .await?,
            None => false,
        };
    let rows = sqlx::query(
        "SELECT e.id, e.tx::text AS tx, e.at, e.game, e.kind, e.audience, e.subject_name, e.actor_name, \
           e.until, e.record, \
           (e.actor_account IS NOT NULL AND (e.audience = 'staff' OR (ap.status = 'active' \
             AND ap.name IS NOT NULL AND e.actor_name IS NOT NULL AND aa.status = 'active' \
             AND (aa.banned_at IS NULL OR (aa.banned_until IS NOT NULL AND aa.banned_until <= now())) \
             AND COALESCE(apf.announce, true)))) AS has_actor, \
           (e.audience = 'staff' OR (sp.status = 'active' AND sp.name IS NOT NULL \
             AND COALESCE(pf.announce, true) \
             AND (e.record IS NULL OR (sa.status = 'active' AND (sa.banned_at IS NULL \
               OR (sa.banned_until IS NOT NULL AND sa.banned_until <= now())))))) AS visible, \
           sa.discord_user_id AS s_id, sa.discord_username AS s_user, sa.discord_global_name AS s_global, \
           sa.discord_avatar AS s_avatar, sa.discord_linked_at AS s_linked, sa.discord_refreshed_at AS s_refreshed, \
           aa.discord_user_id AS a_id, aa.discord_username AS a_user, aa.discord_global_name AS a_global, \
           aa.discord_avatar AS a_avatar, aa.discord_linked_at AS a_linked, aa.discord_refreshed_at AS a_refreshed \
         FROM public_event e \
         LEFT JOIN profile sp ON sp.id = e.subject_profile \
         LEFT JOIN profile_feed pf ON pf.profile_id = e.subject_profile \
         LEFT JOIN account sa ON sa.id = e.subject_account \
         LEFT JOIN account aa ON aa.id = e.actor_account \
         LEFT JOIN profile ap ON ap.id = e.actor_profile \
         LEFT JOIN profile_feed apf ON apf.profile_id = e.actor_profile \
         WHERE ($1::text IS NULL OR (e.tx, e.id) > ($1::text::xid8, $2::bigint)) \
           AND e.tx < pg_snapshot_xmin(pg_current_snapshot()) \
         ORDER BY e.tx, e.id LIMIT $3",
    )
    .bind(after.map(|c| c.tx.to_string()))
    .bind(after.map(|c| c.id).unwrap_or(0))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    let mut items = Vec::with_capacity(rows.len());
    let mut next = after;
    for r in &rows {
        let tx: String = r.try_get("tx")?;
        let cursor = Cursor {
            tx: tx.parse().map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            id: r.try_get("id")?,
        };
        next = Some(cursor);
        if !r.try_get::<bool, _>("visible")? {
            continue;
        }
        items.push(FeedRow {
            cursor,
            at: ms(r.try_get("at")?),
            game: r.try_get("game")?,
            kind: r.try_get("kind")?,
            audience: r.try_get("audience")?,
            subject: FeedPerson {
                name: r.try_get("subject_name")?,
                discord: discord_of(r, "s")?,
            },
            actor: if r.try_get::<bool, _>("has_actor")? {
                Some(FeedPerson {
                    name: r.try_get("actor_name")?,
                    discord: discord_of(r, "a")?,
                })
            } else {
                None
            },
            until: r.try_get::<Option<OffsetDateTime>, _>("until")?.map(ms),
            record: r
                .try_get::<Option<serde_json::Value>, _>("record")?
                .map(|v| FeedRecord::of(&v))
                .transpose()?,
        });
    }
    Ok(FeedPage { items, next, gap })
}

// The link ------------------------------------------------------------------------------------

/// The Discord user a bot read from `GET /users/@me` or `GET /users/{id}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscordIdentity<'a> {
    /// The snowflake (1 to 2^63-1).
    pub id: i64,
    /// The unique handle (2-32 characters).
    pub username: &'a str,
    /// The display name (1-32 characters).
    pub global_name: Option<&'a str>,
    /// The avatar hash (`^(a_)?[0-9a-f]{32}$`).
    pub avatar: Option<&'a str>,
}

/// Why a link was refused.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// Unknown, expired or spent code, or its account is not active (`AUTH.CODE_INVALID`).
    #[error("link code invalid")]
    CodeInvalid,
    /// The Discord user is linked to another account (`ACCOUNTS.ALREADY_LINKED`).
    #[error("discord user linked to another account")]
    Taken,
    /// The database.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for LinkError {
    fn from(e: sqlx::Error) -> Self {
        LinkError::Db(e.into())
    }
}

/// A link made (or replayed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linked {
    /// The account.
    pub account_id: Uuid,
    /// Its live profiles' gamertags.
    pub names: Vec<String>,
    /// When linked.
    pub linked_at: UnixMs,
}

/// The gamertags of the account's live profiles, oldest first.
pub async fn names_in(conn: &mut PgConnection, account: Uuid) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT name FROM profile WHERE account_id = $1 AND status <> 'deleted' AND name IS NOT NULL \
         ORDER BY created_at, id",
    )
    .bind(account)
    .fetch_all(&mut *conn)
    .await
}

/// A new link code for the account (its SHA-256 `hash`), replacing its unused one; the expiry.
pub async fn issue_code_in(
    conn: &mut PgConnection,
    account: Uuid,
    hash: &[u8; 32],
) -> Result<UnixMs, sqlx::Error> {
    sqlx::query("DELETE FROM discord_link_code WHERE account_id = $1 AND used_at IS NULL")
        .bind(account)
        .execute(&mut *conn)
        .await?;
    let expires: OffsetDateTime = sqlx::query_scalar(
        "INSERT INTO discord_link_code (code_hash, account_id, expires_at) \
         VALUES ($1, $2, clock_timestamp() + make_interval(secs => $3)) RETURNING expires_at",
    )
    .bind(hash.as_slice())
    .bind(account)
    .bind(LINK_CODE_TTL_SECS as f64)
    .fetch_one(&mut *conn)
    .await?;
    Ok(ms(expires))
}

/// What an unspent, unexpired code of an active account would link: the names and the expiry.
pub async fn preview_in(
    conn: &mut PgConnection,
    hash: &[u8; 32],
) -> Result<Option<(Vec<String>, UnixMs)>, sqlx::Error> {
    let row: Option<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT c.account_id, c.expires_at FROM discord_link_code c JOIN account a ON a.id = c.account_id \
         WHERE c.code_hash = $1 AND c.used_at IS NULL AND c.expires_at > clock_timestamp() \
           AND a.status = 'active'",
    )
    .bind(hash.as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some((account, expires)) => Ok(Some((names_in(conn, account).await?, ms(expires)))),
        None => Ok(None),
    }
}

fn unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

/// Spends the code and links `who` to its account (CORE_API.md §5a.1 `PUT /core/v1/discord/links`):
/// the account's previous link is replaced, the same user again refreshes the handle (keeping
/// `linked_at`), a user linked to another account is [`LinkError::Taken`] (the code stays
/// unspent). The same code with the same user again, while that link stands, is a replay.
pub async fn link_in(
    conn: &mut PgConnection,
    hash: &[u8; 32],
    who: DiscordIdentity<'_>,
) -> Result<Linked, LinkError> {
    let code: Option<(Uuid, bool, Option<i64>)> = sqlx::query_as(
        "SELECT account_id, expires_at > clock_timestamp(), discord_user_id FROM discord_link_code \
         WHERE code_hash = $1 FOR UPDATE",
    )
    .bind(hash.as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((account, live, spent_by)) = code else {
        return Err(LinkError::CodeInvalid);
    };
    let state: Option<(String, Option<i64>, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT status, discord_user_id, discord_linked_at FROM account WHERE id = $1 FOR UPDATE",
    )
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((status, current, linked_at)) = state else {
        return Err(LinkError::CodeInvalid);
    };
    if status != "active" {
        return Err(LinkError::CodeInvalid);
    }
    if let Some(spent_by) = spent_by {
        return match (current, linked_at) {
            (Some(c), Some(at)) if c == spent_by && c == who.id => Ok(Linked {
                account_id: account,
                names: names_in(conn, account).await?,
                linked_at: ms(at),
            }),
            _ => Err(LinkError::CodeInvalid),
        };
    }
    if !live {
        return Err(LinkError::CodeInvalid);
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM account WHERE discord_user_id = $1 AND id <> $2)",
    )
    .bind(who.id)
    .bind(account)
    .fetch_one(&mut *conn)
    .await?;
    if taken {
        return Err(LinkError::Taken);
    }
    let linked: OffsetDateTime = match sqlx::query_scalar(
        "UPDATE account SET \
           discord_linked_at = CASE WHEN discord_user_id = $2 THEN discord_linked_at ELSE clock_timestamp() END, \
           discord_user_id = $2, discord_username = $3, discord_global_name = $4, discord_avatar = $5, \
           discord_refreshed_at = clock_timestamp() \
         WHERE id = $1 RETURNING discord_linked_at",
    )
    .bind(account)
    .bind(who.id)
    .bind(who.username)
    .bind(who.global_name)
    .bind(who.avatar)
    .fetch_one(&mut *conn)
    .await
    {
        Ok(at) => at,
        // Another account linked the same user meanwhile (the partial unique index).
        Err(e) if unique_violation(&e) => return Err(LinkError::Taken),
        Err(e) => return Err(e.into()),
    };
    sqlx::query(
        "UPDATE discord_link_code SET used_at = clock_timestamp(), discord_user_id = $2 WHERE code_hash = $1",
    )
    .bind(hash.as_slice())
    .bind(who.id)
    .execute(&mut *conn)
    .await?;
    crate::accounts::audit_in(conn, account, "discord.linked").await?;
    Ok(Linked {
        account_id: account,
        names: names_in(conn, account).await?,
        linked_at: ms(linked),
    })
}

/// Clears the account's link and unused codes; `true` when a link was cleared. `audit` writes
/// `discord.unlinked` (the player's unlink). The link is the account's: a profile's deletion never
/// calls this (owner 2026-10-27).
pub async fn unlink_in(
    conn: &mut PgConnection,
    account: Uuid,
    audit: bool,
) -> Result<bool, IdentityError> {
    sqlx::query("DELETE FROM discord_link_code WHERE account_id = $1 AND used_at IS NULL")
        .bind(account)
        .execute(&mut *conn)
        .await?;
    let cleared = sqlx::query(
        "UPDATE account SET discord_user_id = NULL, discord_username = NULL, discord_global_name = NULL, \
           discord_avatar = NULL, discord_linked_at = NULL, discord_refreshed_at = NULL \
         WHERE id = $1 AND discord_user_id IS NOT NULL",
    )
    .bind(account)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0;
    if cleared && audit {
        crate::accounts::audit_in(conn, account, "discord.unlinked").await?;
    }
    Ok(cleared)
}

const LINK_COLUMNS: &str = "discord_user_id AS d_id, discord_username AS d_user, \
    discord_global_name AS d_global, discord_avatar AS d_avatar, discord_linked_at AS d_linked, \
    discord_refreshed_at AS d_refreshed";

/// The account's verified Discord user.
pub async fn link_of_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Option<DiscordUserRow>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {LINK_COLUMNS} FROM account WHERE id = $1"
    )))
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(r) => discord_of(&r, "d"),
        None => Ok(None),
    }
}

/// The bot's refresh of a linked user's handle; `None` when no account links `who.id`.
pub async fn refresh_user_in(
    conn: &mut PgConnection,
    who: DiscordIdentity<'_>,
) -> Result<Option<DiscordUserRow>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE account SET discord_username = $2, discord_global_name = $3, discord_avatar = $4, \
           discord_refreshed_at = clock_timestamp() \
         WHERE discord_user_id = $1 RETURNING {LINK_COLUMNS}"
    )))
    .bind(who.id)
    .bind(who.username)
    .bind(who.global_name)
    .bind(who.avatar)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(r) => discord_of(&r, "d"),
        None => Ok(None),
    }
}

/// A profile's announce switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnounceRow {
    /// XUID.
    pub xuid: u64,
    /// Game.
    pub game: String,
    /// Gamertag.
    pub name: String,
    /// The switch.
    pub announce: bool,
}

const ANNOUNCE_SQL: &str = "SELECT p.xuid, p.game, p.name, COALESCE(f.announce, true) AS announce \
    FROM profile p LEFT JOIN profile_feed f ON f.profile_id = p.id \
    WHERE p.account_id = $1 AND p.status <> 'deleted' AND p.name IS NOT NULL";

fn announce_of(r: &PgRow) -> Result<AnnounceRow, sqlx::Error> {
    Ok(AnnounceRow {
        xuid: r.try_get::<i64, _>("xuid")? as u64,
        game: r.try_get("game")?,
        name: r.try_get("name")?,
        announce: r.try_get("announce")?,
    })
}

/// The account's live profiles with their switch, oldest first.
pub async fn announce_list_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Vec<AnnounceRow>, sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "{ANNOUNCE_SQL} ORDER BY p.created_at, p.id"
    )))
    .bind(account)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(announce_of)
    .collect()
}

/// Sets the switch of the account's live profile `xuid`; `None` when it is not one.
pub async fn set_announce_in(
    conn: &mut PgConnection,
    account: Uuid,
    xuid: u64,
    announce: bool,
) -> Result<Option<AnnounceRow>, sqlx::Error> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(None);
    };
    let profile: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM profile WHERE account_id = $1 AND xuid = $2 AND status <> 'deleted' \
           AND name IS NOT NULL ORDER BY created_at, id LIMIT 1",
    )
    .bind(account)
    .bind(xuid)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(profile) = profile else {
        return Ok(None);
    };
    sqlx::query(
        "INSERT INTO profile_feed (profile_id, announce) VALUES ($1, $2) \
         ON CONFLICT (profile_id) DO UPDATE SET announce = $2, changed_at = clock_timestamp() \
           WHERE profile_feed.announce <> $2",
    )
    .bind(profile)
    .bind(announce)
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!("{ANNOUNCE_SQL} AND p.id = $2")))
        .bind(account)
        .bind(profile)
        .fetch_one(&mut *conn)
        .await?;
    announce_of(&row).map(Some)
}

/// What a profile's erasure did to the feed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfileErased {
    /// `public_event` rows about it, deleted.
    pub events: u64,
    /// Rows about other players it acted on (a moderator's kick or ban) or was the previous
    /// record holder of: its profile and name removed, the row kept ("a moderator", "a rival").
    pub acted: u64,
    /// Its `profile_feed` row.
    pub feed: u64,
}

/// A profile's feed rows and switch, for its erasure (`fi_db::erasure`).
pub async fn erase_profile_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<ProfileErased, sqlx::Error> {
    let events = sqlx::query("DELETE FROM public_event WHERE subject_profile = $1")
        .bind(profile)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    // A record's previous holder leaves the row whole (the row reads "a rival"); a moderator's
    // account stays on a staff row.
    let acted = sqlx::query(
        "UPDATE public_event SET actor_profile = NULL, actor_name = NULL, \
           actor_account = CASE WHEN audience = 'public' THEN NULL ELSE actor_account END \
         WHERE actor_profile = $1",
    )
    .bind(profile)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let feed = sqlx::query("DELETE FROM profile_feed WHERE profile_id = $1")
        .bind(profile)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(ProfileErased {
        events,
        acted,
        feed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_boards_are_the_games() {
        let key = |sbt, st, id, track| BoardKey {
            scoreboard_type: sbt,
            score_type: st,
            game_scoreboard_id: id,
            track_id: track,
        };
        let trap = record_scope(&key(9, 5, 11, -1)).unwrap();
        assert_eq!((trap.board, trap.id), (RecordBoard::SpeedTrap, 11));
        assert!(trap.real(|_| false));
        assert!(!record_scope(&key(9, 5, 36, -1)).unwrap().real(|_| true));
        assert!(record_scope(&key(9, 6, 36, -1)).unwrap().real(|_| false));
        assert!(!record_scope(&key(9, 6, 56, -1)).unwrap().real(|_| true));
        let route = record_scope(&key(1, 1, 70, 898)).unwrap();
        assert_eq!(
            (route.board, route.id, route.board_id),
            (RecordBoard::Route, 898, 70)
        );
        assert!(route.real(|t| t == 898) && !route.real(|_| false));
        assert_eq!(
            record_scope(&key(1, 1, 70, -1)),
            None,
            "a time without a track"
        );
        assert_eq!(record_scope(&key(9, 10, 1, -1)), None, "points");
        assert_eq!(
            record_scope(&key(4, 5, 1, -1)),
            None,
            "a speed off the camera boards"
        );
        // Shown units: 1 ms, 0.1 km/h.
        assert!(!route.shown_better(149.8798, 149.88));
        assert!(route.shown_better(149.879, 149.88));
        assert!(!trap.shown_better(68.01, 68.0));
        assert!(trap.shown_better(68.02, 68.0));
    }

    #[test]
    fn cursors_round_trip_and_refuse_noise() {
        let c = Cursor { tx: 812, id: 41 };
        assert_eq!(c.to_string(), "812.41");
        assert_eq!(Cursor::parse("812.41"), Some(c));
        for bad in [
            "", "1", "1.", ".1", "1.0", "-1.2", "1.-2", "a.1", "1.2.3", "1 .2", "+1.2",
        ] {
            assert_eq!(Cursor::parse(bad), None, "{bad}");
        }
        assert_eq!(Cursor::parse("99999999999999999999999.1"), None);
    }
}
