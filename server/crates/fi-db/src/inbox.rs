//! The game's in-game inbox (SER-M3 step 7, OD-48 item 2; GAME_PLATFORM.md §3a "Supporter social";
//! migration `20261019140200_inbox.sql`): `game_notification` and `game_inbox`, ported from the
//! Beta's `notifications` (`deprecated/beta/server/src/db/notifications.rs`: `notify`,
//! `notifications`, `mark_notification_read`, `clear_notification`, `rival_notifications`,
//! `pending_challenge`; `db/news.rs`), keyed by `(game, profile)` instead of the XUID.
//!
//! A notification is its type (the game's `notificationType`), its parameter struct's schema name
//! and JSON (encoded on delivery by the caller), when it was sent and whether it was read or
//! cleared. A cleared row is a tombstone: never delivered again, and its `dedup_key` keeps a news
//! item or a rival time the player deleted (`cleared_by` player) from coming back; the rivals sync
//! revives only what it cleared itself (`cleared_by` server). Every function takes the caller's
//! connection (the request's one transaction) and reads the database clock.
//!
//! Bounds: a row expires [`TTL_DAYS`] after it was sent (never delivered after, deleted by
//! [`prune_in`]); a profile keeps its newest [`MAX_ROWS`] rows, server-cleared tombstones dropped
//! first (a prune skips rows another transaction holds and never waits for them, so an inbox may
//! stay a few rows over its bound until the next prune); a sync ([`begin_sync_in`]) runs at most
//! every [`SYNC_EVERY_S`] seconds per profile and delivers at most [`NEWS_PER_SYNC`] news items
//! (every live notice of the last [`TTL_DAYS`] days the inbox does not hold yet, cleared or not:
//! [`news_in`]); the rivals sync reads at most [`RIVAL_FRIENDS`] friends ×
//! [`RIVAL_TIMES_PER_FRIEND`] route times in one statement, keeps at most [`MAX_RIVALS`] rival
//! times and writes only what changed ([`sync_rivals_in`]); a sender sends at most
//! [`MESSAGES_PER_HOUR`] messages an hour.
//!
//! The statements composed with `format!` (`sqlx::AssertSqlSafe`) join constants of this crate
//! only ([`VISIBLE`]); every value is a bind parameter.

use crate::gameplane::{Card, Player};
use crate::identity::{IdentityError, ms};
use crate::scores::{self, Assists, BoardKey, ScoreRow};
use crate::visibility::{VISIBLE, Viewer};
use fi_domain::UnixMs;
use serde_json::Value as Json;
use sqlx::postgres::PgRow;
use sqlx::types::Json as Jsonb;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// `Message` (0x0200, MessageNotificationParameters): a friend's message, and the platform's news
/// (the Beta's admin news used it too). Tab 2, "Messages".
pub const MESSAGE: i32 = 0x0200;
/// `TimeTrialTimeBeaten` (0x8403, TimeTrialTimeBeatenNotificationParameters): a friend's route
/// time to beat (Meus Rivais). Tab 4, "Rivals".
pub const TIME_BEATEN: i32 = 0x8403;
/// The parameter struct of [`MESSAGE`].
pub const MESSAGE_STRUCT: &str = "MessageNotificationParameters";
/// The parameter struct of [`TIME_BEATEN`].
pub const TIME_BEATEN_STRUCT: &str = "TimeTrialTimeBeatenNotificationParameters";
/// `TimeTrialRecommend` (0x8402): a Monthly Rivals event recommended once per month (S4; other
/// projects used as reference).
pub const RECOMMEND: i32 = 0x8402;
/// Its parameter struct.
pub const RECOMMEND_STRUCT: &str = "TimeTrialRecommendNotificationParameters";

/// A notification lives this many days after it was sent (the Beta's `TTL_MS`).
pub const TTL_DAYS: i64 = 30;
/// [`TTL_DAYS`] in milliseconds (`expirationDate`).
pub const TTL_MS: i64 = TTL_DAYS * 24 * 3600 * 1000;
/// Rows a profile keeps, tombstones included (the newest; server-cleared tombstones go first).
pub const MAX_ROWS: i64 = 500;
/// A sync runs at most this often per profile (the game polls every 30 s; under it, so a poll a
/// little early still syncs).
pub const SYNC_EVERY_S: i32 = 25;
/// News items one sync delivers at most (the next sync takes the next ones).
pub const NEWS_PER_SYNC: i64 = 20;
/// Friends the rivals sync reads at most (favorites first, then the oldest friendships).
pub const RIVAL_FRIENDS: i64 = 100;
/// Route times read per friend at most (their newest).
pub const RIVAL_TIMES_PER_FRIEND: i64 = 20;
/// Rival times an inbox holds at most (the friends' newest times first).
pub const MAX_RIVALS: i64 = 100;
/// Messages one sender sends at most per hour (the Beta's).
pub const MESSAGES_PER_HOUR: i64 = 30;
/// A rival time still wanted has its `sent_at` moved to now once it is this old, so it never
/// expires ([`TTL_DAYS`]) while the friend's time stands.
pub const RIVAL_REFRESH_DAYS: i64 = TTL_DAYS / 2;

/// A notification as delivered.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    /// The wire's notification id.
    pub id: i64,
    /// `notificationType`.
    pub kind: i32,
    /// The parameter struct's schema name.
    pub structure: String,
    /// The parameter struct as JSON.
    pub params: Json,
    /// When it was sent.
    pub sent_at: UnixMs,
    /// Whether the player read it.
    pub read: bool,
}

/// Claims the profile's sync: `true` when the last one ran [`SYNC_EVERY_S`] seconds ago or more
/// (or never), and the profile's sync row stays locked until the caller's transaction ends, so
/// concurrent polls sync once; `false`: not due. (The row's `news_since` / `news_id` mark is no
/// longer read: news delivery follows the inbox's own `news:<id>` rows, [`news_in`].)
pub async fn begin_sync_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<bool, IdentityError> {
    let row: Option<i32> = sqlx::query_scalar(
        "INSERT INTO game_inbox (game, profile_id, synced_at) VALUES ($1, $2, clock_timestamp()) \
         ON CONFLICT (game, profile_id) DO UPDATE SET synced_at = clock_timestamp() \
         WHERE game_inbox.synced_at IS NULL \
            OR game_inbox.synced_at <= clock_timestamp() - make_interval(secs => $3) \
         RETURNING 1",
    )
    .bind(game)
    .bind(profile_id)
    .bind(f64::from(SYNC_EVERY_S))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.is_some())
}

/// A notice to deliver as news.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewsItem {
    /// The notice's id.
    pub id: Uuid,
    /// Its title.
    pub title: String,
    /// Its body (Markdown).
    pub body: String,
    /// Shown from.
    pub since: OffsetDateTime,
}

/// The notices of `game` (and platform-wide ones) in `lang` that are live now, were shown from
/// within the last [`TTL_DAYS`] days, and that the profile's inbox does not hold yet (no
/// `news:<id>` row, cleared or not: a news item the player deleted never comes back): oldest
/// first, at most [`NEWS_PER_SYNC`]. Delivery follows the inbox, not a mark, so a notice
/// published with an earlier `since` than one already delivered still arrives, and a language
/// switch delivers the other language's notices (each language is its own notice).
pub async fn news_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    lang: &str,
) -> Result<Vec<NewsItem>, IdentityError> {
    let rows = sqlx::query(
        "SELECT x.id, x.title, x.body, x.since FROM notice x \
         WHERE x.deleted_at IS NULL AND x.lang = $2 AND (x.game IS NULL OR x.game = $1) \
           AND x.since <= clock_timestamp() AND (x.until IS NULL OR x.until > clock_timestamp()) \
           AND x.since > clock_timestamp() - make_interval(days => $4) \
           AND NOT EXISTS (SELECT 1 FROM game_notification n WHERE n.game = $1 \
             AND n.profile_id = $3 AND n.dedup_key = 'news:' || x.id::text) \
         ORDER BY x.since, x.id LIMIT $5",
    )
    .bind(game)
    .bind(lang)
    .bind(profile_id)
    .bind(i32::try_from(TTL_DAYS).unwrap_or(30))
    .bind(NEWS_PER_SYNC)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(NewsItem {
                id: r.try_get("id")?,
                title: r.try_get("title")?,
                body: r.try_get("body")?,
                since: r.try_get("since")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Delivers news to a profile: each `(notice, MessageNotificationParameters)` once (a notice
/// already in the inbox, cleared or not, is not added again). Returns how many were added.
pub async fn deliver_news_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    items: &[(Uuid, Json)],
) -> Result<u64, IdentityError> {
    if items.is_empty() {
        return Ok(0);
    }
    let keys: Vec<String> = items.iter().map(|(id, _)| format!("news:{id}")).collect();
    let params: Vec<Jsonb<&Json>> = items.iter().map(|(_, p)| Jsonb(p)).collect();
    let done = sqlx::query(
        "INSERT INTO game_notification (game, profile_id, type, struct, params, dedup_key) \
         SELECT $1, $2, $3, $4, w.p, w.k FROM unnest($5::jsonb[], $6::text[]) WITH ORDINALITY AS w(p, k, n) \
         ORDER BY w.n \
         ON CONFLICT (game, profile_id, dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING",
    )
    .bind(game)
    .bind(profile_id)
    .bind(MESSAGE)
    .bind(MESSAGE_STRUCT)
    .bind(&params)
    .bind(&keys)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// Delivers one notification of `kind` (`structure` its parameter struct) to a profile once per
/// `dedup_key` (already in the inbox, cleared or not: nothing). Returns whether it was added.
pub async fn deliver_once_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    kind: i32,
    structure: &str,
    dedup_key: &str,
    params: &Json,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "INSERT INTO game_notification (game, profile_id, type, struct, params, dedup_key) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (game, profile_id, dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING",
    )
    .bind(game)
    .bind(profile_id)
    .bind(kind)
    .bind(structure)
    .bind(Jsonb(params))
    .bind(dedup_key)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Clears (server) the news items of a profile's inbox whose notice was deleted or has ended, as
/// the Beta's `delete_news` took a send back. Returns how many.
pub async fn retract_news_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "UPDATE game_notification n SET cleared = true, cleared_by = 'server' FROM notice x \
         WHERE n.game = $1 AND n.profile_id = $2 AND n.type = $3 AND NOT n.cleared \
           AND n.params ? 'noticeId' AND x.id = (n.params ->> 'noticeId')::uuid \
           AND (x.deleted_at IS NOT NULL OR (x.until IS NOT NULL AND x.until <= clock_timestamp()))",
    )
    .bind(game)
    .bind(profile_id)
    .bind(MESSAGE)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// A friend's route time the viewer has not beaten.
#[derive(Clone, Debug, PartialEq)]
pub struct RivalTime {
    /// The friend's profile.
    pub sender: Uuid,
    /// The friend's player (gamertag parts; the card is not read).
    pub player: Player,
    /// The board key of the time (score type 1, a track).
    pub key: BoardKey,
    /// The time, seconds.
    pub score: f64,
    /// `carId`.
    pub car_id: i32,
    /// `carClassId` as submitted.
    pub car_class_id: i16,
    /// When it was submitted.
    pub submitted_at: UnixMs,
}

/// The rivals sync's one read (the Beta's `sync_rivals`): for the viewer's visible friends of its
/// game and mode (the profiles of its account's friend accounts, OD-55; at most
/// [`RIVAL_FRIENDS`], favorites first, then the oldest friendships), each
/// friend's newest route times (score type 1, a track; at most [`RIVAL_TIMES_PER_FRIEND`] each)
/// that the viewer has not beaten (no time on that event, or a slower one), the friends' newest
/// times first, at most [`MAX_RIVALS`]. Monthly Rivals boards ([`scores::MONTHLY_BOARD`]) are
/// [`event_rival_times_in`]'s, for this month only.
pub async fn rival_times_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
) -> Result<Vec<RivalTime>, IdentityError> {
    let sql = format!(
        "WITH friends AS ( \
           SELECT p.id, p.xuid, p.name FROM account_friendship f \
           JOIN account_friendship back ON back.account_id = f.friend_id \
             AND back.friend_id = f.account_id \
           JOIN account a ON a.id = f.friend_id \
           JOIN profile p ON p.account_id = f.friend_id \
           WHERE f.account_id = $7 AND p.game = $1 AND p.mode = $2 AND {VISIBLE} \
           ORDER BY f.favorite DESC, f.created_at, f.friend_id, p.created_at, p.id LIMIT $4 \
         ) \
         SELECT fr.id AS sender, fr.xuid, fr.name, t.scoreboard_type, t.game_scoreboard_id, \
           t.track_id, t.score, t.car_id, t.car_class_id, t.submitted_at \
         FROM friends fr \
         CROSS JOIN LATERAL ( \
           SELECT s.scoreboard_type, s.game_scoreboard_id, s.track_id, s.score, s.car_id, s.car_class_id, s.submitted_at \
           FROM game_score s \
           WHERE s.game = $1 AND s.mode = $2 AND s.profile_id = fr.id AND s.score_type = 1 AND s.track_id >= 0 \
             AND s.scoreboard_type <> $8 \
           ORDER BY s.submitted_at DESC LIMIT $5 \
         ) t \
         WHERE NOT EXISTS ( \
           SELECT 1 FROM game_score m WHERE m.game = $1 AND m.mode = $2 AND m.profile_id = $3 \
             AND m.scoreboard_type = t.scoreboard_type AND m.score_type = 1 \
             AND m.game_scoreboard_id = t.game_scoreboard_id AND m.track_id = t.track_id AND m.score <= t.score \
         ) \
         ORDER BY t.submitted_at DESC, fr.id, t.scoreboard_type, t.game_scoreboard_id, t.track_id \
         LIMIT $6"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(viewer.profile_id)
        .bind(RIVAL_FRIENDS)
        .bind(RIVAL_TIMES_PER_FRIEND)
        .bind(MAX_RIVALS)
        .bind(viewer.account_id)
        .bind(scores::MONTHLY_BOARD)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(RivalTime {
                sender: r.try_get("sender")?,
                player: Player {
                    xuid: r.try_get("xuid")?,
                    name: r.try_get("name")?,
                    card: Card::default(),
                    tables_tran_id: None,
                },
                key: BoardKey {
                    scoreboard_type: r.try_get("scoreboard_type")?,
                    score_type: 1,
                    game_scoreboard_id: r.try_get("game_scoreboard_id")?,
                    track_id: r.try_get("track_id")?,
                },
                score: r.try_get("score")?,
                car_id: r.try_get("car_id")?,
                car_class_id: r.try_get("car_class_id")?,
                submitted_at: ms(r.try_get("submitted_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// The Monthly Rivals beaten-time read (S4; other projects used as reference): for each of `events` (board
/// `scoreboard_type`, score type 1) where the viewer has a time, the visible player whose best
/// there is the nearest faster one (any player, not only friends), at most one per event. The
/// rivals sync turns them into TimeTrialTimeBeaten notifications; an event outside `events` (a
/// past month) or a time the viewer has since beaten drops out and its notification is cleared.
pub async fn event_rival_times_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    scoreboard_type: i32,
    events: &[i32],
) -> Result<Vec<RivalTime>, IdentityError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "WITH best AS ( \
           SELECT DISTINCT ON (s.game_scoreboard_id, s.profile_id) s.profile_id, s.game_scoreboard_id, \
             s.track_id, s.score, s.car_id, s.car_class_id, s.submitted_at \
           FROM game_score s \
           WHERE s.game = $1 AND s.mode = $2 AND s.scoreboard_type = $4 AND s.score_type = 1 \
             AND s.game_scoreboard_id IN ( \
               SELECT v.game_scoreboard_id FROM game_score v \
               WHERE v.game = $1 AND v.mode = $2 AND v.profile_id = $3 AND v.scoreboard_type = $4 \
                 AND v.score_type = 1 AND v.game_scoreboard_id = ANY($5::integer[])) \
           ORDER BY s.game_scoreboard_id, s.profile_id, s.score, s.submitted_at \
         ), mine AS (SELECT game_scoreboard_id, score FROM best WHERE profile_id = $3) \
         SELECT DISTINCT ON (b.game_scoreboard_id) p.id AS sender, p.xuid, p.name, \
           b.game_scoreboard_id, b.track_id, b.score, b.car_id, b.car_class_id, b.submitted_at \
         FROM mine m \
         JOIN best b ON b.game_scoreboard_id = m.game_scoreboard_id AND b.score < m.score \
           AND b.profile_id <> $3 \
         JOIN profile p ON p.id = b.profile_id \
         JOIN account a ON a.id = p.account_id \
         WHERE p.game = $1 AND p.mode = $2 AND {VISIBLE} \
         ORDER BY b.game_scoreboard_id, b.score DESC, b.submitted_at, p.id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(viewer.profile_id)
        .bind(scoreboard_type)
        .bind(events)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(RivalTime {
                sender: r.try_get("sender")?,
                player: Player {
                    xuid: r.try_get("xuid")?,
                    name: r.try_get("name")?,
                    card: Card::default(),
                    tables_tran_id: None,
                },
                key: BoardKey {
                    scoreboard_type,
                    score_type: 1,
                    game_scoreboard_id: r.try_get("game_scoreboard_id")?,
                    track_id: r.try_get("track_id")?,
                },
                score: r.try_get("score")?,
                car_id: r.try_get("car_id")?,
                car_class_id: r.try_get("car_class_id")?,
                submitted_at: ms(r.try_get("submitted_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// A rival time the inbox should hold.
#[derive(Clone, Debug, PartialEq)]
pub struct WantedRival {
    /// `rival:<mode>:<sender>:<board>:<event>:<track>:<score bits>` (unique per rival time).
    pub dedup_key: String,
    /// The friend.
    pub sender: Uuid,
    /// TimeTrialTimeBeatenNotificationParameters (with `rivalBoard` and `rivalEvent`).
    pub params: Json,
}

/// What a rivals sync changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RivalSync {
    /// Rival times cleared: beaten, improved on, from someone no longer a friend (or no longer
    /// among the bounded set).
    pub cleared: u64,
    /// Rival times added, or revived after the sync had cleared them.
    pub added: u64,
}

/// Makes the profile's rival times exactly `wanted`: every other live one is cleared (server), each
/// wanted one is added unless the inbox holds its key already (a tombstone the player cleared
/// stays; one the sync cleared is revived). Writes only what changed: the live keys are read
/// first, and when they are exactly the wanted ones nothing is cleared, inserted or revived; a key
/// with a row is never inserted again (no conflict, no row lock, no sequence value spent). A wanted
/// time older than [`RIVAL_REFRESH_DAYS`] has its `sent_at` moved to now, so it never expires while
/// it stands.
pub async fn sync_rivals_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    wanted: &[WantedRival],
) -> Result<RivalSync, IdentityError> {
    let mut keys: Vec<&str> = Vec::with_capacity(wanted.len());
    let mut senders: Vec<Uuid> = Vec::with_capacity(wanted.len());
    let mut params: Vec<Jsonb<&Json>> = Vec::with_capacity(wanted.len());
    for w in wanted {
        // One row per key.
        if keys.contains(&w.dedup_key.as_str()) {
            continue;
        }
        keys.push(&w.dedup_key);
        senders.push(w.sender);
        params.push(Jsonb(&w.params));
    }
    let live: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT dedup_key FROM game_notification \
         WHERE game = $1 AND profile_id = $2 AND type = $3 AND NOT cleared",
    )
    .bind(game)
    .bind(profile_id)
    .bind(TIME_BEATEN)
    .fetch_all(&mut *conn)
    .await?;
    let same = live.len() == keys.len()
        && live
            .iter()
            .all(|k| k.as_deref().is_some_and(|k| keys.contains(&k)));
    let mut done = RivalSync::default();
    if !same {
        done.cleared = sqlx::query(
            "UPDATE game_notification SET cleared = true, cleared_by = 'server' \
             WHERE game = $1 AND profile_id = $2 AND type = $3 AND NOT cleared \
               AND (dedup_key IS NULL OR dedup_key <> ALL($4::text[]))",
        )
        .bind(game)
        .bind(profile_id)
        .bind(TIME_BEATEN)
        .bind(&keys)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        let inserted = sqlx::query(
            "INSERT INTO game_notification (game, profile_id, type, struct, params, dedup_key, sender_id) \
             SELECT $1, $2, $3, $4, w.p, w.k, w.s \
             FROM unnest($5::jsonb[], $6::text[], $7::uuid[]) WITH ORDINALITY AS w(p, k, s, n) \
             WHERE NOT EXISTS (SELECT 1 FROM game_notification x \
               WHERE x.game = $1 AND x.profile_id = $2 AND x.dedup_key = w.k) \
             ORDER BY w.n \
             ON CONFLICT (game, profile_id, dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING",
        )
        .bind(game)
        .bind(profile_id)
        .bind(TIME_BEATEN)
        .bind(TIME_BEATEN_STRUCT)
        .bind(&params)
        .bind(&keys)
        .bind(&senders)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        let revived = sqlx::query(
            "UPDATE game_notification SET cleared = false, cleared_by = NULL \
             WHERE game = $1 AND profile_id = $2 AND type = $3 AND cleared_by = 'server' \
               AND dedup_key = ANY($4::text[])",
        )
        .bind(game)
        .bind(profile_id)
        .bind(TIME_BEATEN)
        .bind(&keys)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        done.added = inserted + revived;
    }
    if !keys.is_empty() {
        sqlx::query(
            "UPDATE game_notification SET sent_at = clock_timestamp() \
             WHERE game = $1 AND profile_id = $2 AND type = $3 AND NOT cleared \
               AND dedup_key = ANY($4::text[]) AND sent_at < clock_timestamp() - make_interval(days => $5)",
        )
        .bind(game)
        .bind(profile_id)
        .bind(TIME_BEATEN)
        .bind(&keys)
        .bind(i32::try_from(RIVAL_REFRESH_DAYS).unwrap_or(15))
        .execute(&mut *conn)
        .await?;
    }
    Ok(done)
}

/// Deletes a profile's expired rows and every row beyond its newest [`MAX_ROWS`] (server-cleared
/// tombstones go first, then the oldest). The rows to delete are locked `SKIP LOCKED`: a row
/// another transaction holds (the recipient's own sync, a friend's message) is left for the next
/// prune, so two prunes of one inbox never wait on each other (no deadlock). Returns how many.
pub async fn prune_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "DELETE FROM game_notification WHERE id IN ( \
           SELECT id FROM game_notification WHERE game = $1 AND profile_id = $2 AND ( \
             sent_at < clock_timestamp() - make_interval(days => $3) \
             OR id IN (SELECT id FROM game_notification WHERE game = $1 AND profile_id = $2 \
                         AND sent_at >= clock_timestamp() - make_interval(days => $3) \
                       ORDER BY (cleared_by IS DISTINCT FROM 'server') DESC, id DESC OFFSET $4)) \
           FOR UPDATE SKIP LOCKED)",
    )
    .bind(game)
    .bind(profile_id)
    .bind(i32::try_from(TTL_DAYS).unwrap_or(30))
    .bind(MAX_ROWS)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// The profile's live notifications (not cleared, not expired), newest first, at most `max`.
pub async fn live_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    max: i64,
) -> Result<Vec<Notification>, IdentityError> {
    let rows = sqlx::query(
        "SELECT id, type, struct, params, sent_at, read FROM game_notification \
         WHERE game = $1 AND profile_id = $2 AND NOT cleared \
           AND sent_at >= clock_timestamp() - make_interval(days => $3) \
         ORDER BY id DESC LIMIT $4",
    )
    .bind(game)
    .bind(profile_id)
    .bind(i32::try_from(TTL_DAYS).unwrap_or(30))
    .bind(max.max(0))
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            let params: Jsonb<Json> = r.try_get("params")?;
            Ok(Notification {
                id: r.try_get("id")?,
                kind: r.try_get("type")?,
                structure: r.try_get("struct")?,
                params: params.0,
                sent_at: ms(r.try_get("sent_at")?),
                read: r.try_get("read")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// MarkRead: the profile's own notification `id` only. Whether one was marked.
pub async fn mark_read_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    id: i64,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE game_notification SET read = true WHERE game = $1 AND id = $3 AND profile_id = $2 AND NOT read",
    )
    .bind(game)
    .bind(profile_id)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Clear: the profile's own notification `id` only, cleared by the player (a tombstone the syncs
/// never revive). Whether one was cleared.
pub async fn clear_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    id: i64,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE game_notification SET cleared = true, cleared_by = 'player' \
         WHERE game = $1 AND id = $3 AND profile_id = $2 AND cleared_by IS DISTINCT FROM 'player'",
    )
    .bind(game)
    .bind(profile_id)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The notification a message request already sent (`source_id`: the request's fact id), if any:
/// a replayed SendTextNotification answers its id and sends nothing.
pub async fn sent_in(
    conn: &mut PgConnection,
    game: &str,
    source_id: Uuid,
) -> Result<Option<i64>, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT id FROM game_notification WHERE game = $1 AND source_id = $2")
            .bind(game)
            .bind(source_id)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// The profile of `xuid` when it is visible to the viewer (its game and mode, active, its account
/// active and not banned) and is not the viewer: who a message may go to, friendship aside.
pub async fn recipient_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    xuid: i64,
) -> Result<Option<Uuid>, IdentityError> {
    let sql = format!(
        "SELECT p.id FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.xuid = $1 AND p.game = $2 AND p.mode = $3 AND p.id <> $4 AND {VISIBLE}"
    );
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(xuid)
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(viewer.profile_id)
        .fetch_optional(&mut *conn)
        .await?)
}

/// A message to send.
#[derive(Clone, Debug, PartialEq)]
pub struct NewMessage<'a> {
    /// The game.
    pub game: &'a str,
    /// The recipient's profile.
    pub recipient: Uuid,
    /// The sender's profile.
    pub sender: Uuid,
    /// The request's fact id.
    pub source_id: Uuid,
    /// MessageNotificationParameters.
    pub params: &'a Json,
}

/// What [`send_message_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// Sent: the recipient's new notification id.
    Sent(i64),
    /// The same request sent it already: that notification's id, nothing new.
    Replayed(i64),
    /// The sender reached [`MESSAGES_PER_HOUR`] in the last hour: nothing sent.
    Limited,
}

/// Sends a message (a [`MESSAGE`] notification in the recipient's inbox), once per request fact,
/// at most [`MESSAGES_PER_HOUR`] per sender in any hour: the sender's messages are counted under a
/// transaction-scoped advisory lock on the sender, so concurrent sends count one after the other.
/// The recipient's inbox is pruned to its bound. The caller checked friendship and visibility.
pub async fn send_message_in(
    conn: &mut PgConnection,
    m: &NewMessage<'_>,
) -> Result<Sent, IdentityError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('fi.game_notification.send'), \
           hashtext($1 || ':' || $2::text))",
    )
    .bind(m.game)
    .bind(m.sender)
    .execute(&mut *conn)
    .await?;
    if let Some(id) = sent_in(conn, m.game, m.source_id).await? {
        return Ok(Sent::Replayed(id));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM game_notification WHERE game = $1 AND sender_id = $2 AND type = 512 \
           AND sent_at > clock_timestamp() - interval '1 hour'",
    )
    .bind(m.game)
    .bind(m.sender)
    .fetch_one(&mut *conn)
    .await?;
    if count >= MESSAGES_PER_HOUR {
        return Ok(Sent::Limited);
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO game_notification (game, profile_id, type, struct, params, sender_id, source_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(m.game)
    .bind(m.recipient)
    .bind(MESSAGE)
    .bind(MESSAGE_STRUCT)
    .bind(Jsonb(m.params))
    .bind(m.sender)
    .bind(m.source_id)
    .fetch_one(&mut *conn)
    .await?;
    prune_in(conn, m.game, m.recipient).await?;
    Ok(Sent::Sent(id))
}

/// GetRival's pending challenge (the Beta's `pending_challenge` and `challenge_rival`): the
/// sender's time on the event of the viewer's newest live rival notification of `board` on `track`
/// (the notified time when the sender still has it, else their best there)
/// (and `event` as its `timeTrialId` when given, and from `sender_xuid` when given), when the
/// sender is visible to the viewer, as one row with its player, card and own ghost. `None`: no
/// such challenge.
pub async fn pending_challenge_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    board: i32,
    track: i32,
    event: Option<i32>,
    sender_xuid: Option<i64>,
) -> Result<Option<ScoreRow>, IdentityError> {
    let sql = format!(
        "SELECT s.profile_id, s.scoreboard_type, s.score_type, s.game_scoreboard_id, s.track_id, s.score, \
           s.score_id, s.submitted_at, s.car_id, s.car_class_id, s.car_pi, s.is_clean, s.used_stm, s.used_tcs, \
           s.used_abs, s.used_auto_brake, s.used_auto_shifting, s.used_clutch, p.xuid, p.name, \
           l.career_level, l.badge_id, l.player_title_id, g.file_id \
         FROM game_notification n \
         JOIN profile p ON p.id = n.sender_id \
         JOIN account a ON a.id = p.account_id \
         JOIN game_score s ON s.game = n.game AND s.mode = $2 AND s.profile_id = n.sender_id \
           AND s.scoreboard_type = (n.params ->> 'rivalBoard')::integer AND s.score_type = 1 \
           AND s.game_scoreboard_id = (n.params ->> 'rivalEvent')::integer AND s.track_id = $4 \
         LEFT JOIN game_logon l ON l.game = $1 AND l.profile_id = p.id \
         LEFT JOIN game_ghost g ON g.game = $1 AND g.score_id = s.score_id \
         WHERE n.game = $1 AND n.profile_id = $3 AND n.type = $7 AND NOT n.cleared \
           AND n.sent_at >= clock_timestamp() - make_interval(days => $8) \
           AND (n.params ->> 'trackId')::integer = $4 AND (n.params ->> 'rivalBoard')::integer = $9 \
           AND ($5::integer IS NULL OR (n.params ->> 'timeTrialId')::integer = $5) \
           AND ($6::bigint IS NULL OR p.xuid = $6) \
           AND p.game = $1 AND p.mode = $2 AND p.id <> $3 AND {VISIBLE} \
         ORDER BY n.id DESC, (s.score = (n.params ->> 'score')::double precision) DESC, s.score, s.score_id \
         LIMIT 1"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(viewer.profile_id)
        .bind(track)
        .bind(event)
        .bind(sender_xuid)
        .bind(TIME_BEATEN)
        .bind(i32::try_from(TTL_DAYS).unwrap_or(30))
        .bind(board)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.as_ref().map(challenge_row).transpose()?)
}

fn challenge_row(r: &PgRow) -> Result<ScoreRow, sqlx::Error> {
    let level: Option<i64> = r.try_get("career_level")?;
    let badge: Option<i32> = r.try_get("badge_id")?;
    let title: Option<i32> = r.try_get("player_title_id")?;
    let submitted: OffsetDateTime = r.try_get("submitted_at")?;
    Ok(ScoreRow {
        position: 1,
        profile_id: r.try_get("profile_id")?,
        player: Player {
            xuid: r.try_get("xuid")?,
            name: r.try_get("name")?,
            card: Card {
                career_level: level.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
                badge_id: badge.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
                player_title_id: title.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
            },
            tables_tran_id: None,
        },
        key: BoardKey {
            scoreboard_type: r.try_get("scoreboard_type")?,
            score_type: r.try_get("score_type")?,
            game_scoreboard_id: r.try_get("game_scoreboard_id")?,
            track_id: r.try_get("track_id")?,
        },
        score: r.try_get("score")?,
        submitted_at: ms(submitted),
        car_id: r.try_get("car_id")?,
        car_class_id: r.try_get("car_class_id")?,
        car_pi: r.try_get("car_pi")?,
        assists: Assists {
            is_clean: r.try_get("is_clean")?,
            used_stm: r.try_get("used_stm")?,
            used_tcs: r.try_get("used_tcs")?,
            used_abs: r.try_get("used_abs")?,
            used_auto_brake: r.try_get("used_auto_brake")?,
            used_auto_shifting: r.try_get("used_auto_shifting")?,
            used_clutch: r.try_get("used_clutch")?,
        },
        score_id: r.try_get("score_id")?,
        ghost_file_id: r.try_get("file_id")?,
    })
}
