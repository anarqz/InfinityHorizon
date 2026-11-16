//! The friend graph (SER-M3 step 7, OD-48 item 2; OD-55: friendships belong to the account;
//! migration `20261019160000_account_friends.sql`; routes CORE_API.md §4.1):
//! `account_friendship` (two rows per friendship, one per side, each with its own `favorite`) and
//! `account_friend_request` (one row from the asking account to the asked one). A friendship is
//! between two accounts, of every game, and survives every profile change (a Hardcore → Noob move,
//! a new profile, a deleted one).
//!
//! **The game plane** reads it with a profile as the viewer: a profile's friends are the profiles
//! of its friend accounts in the same game and mode, visible ([`crate::visibility::VISIBLE`];
//! [`friends_in`], [`friends_presence_in`], [`friends_compare_stats_in`], [`bucket_stats_in`],
//! [`in_friend_set_in`], [`are_friends_in`]); a friendship needs both its rows. Which tickets
//! reach those reads (Supporter Hardcore, OD-52) is the proxy's rule, not this module's.
//!
//! **Core** writes and lists it per account ([`lists_in`], [`request_in`], [`accept_in`],
//! [`decline_in`], [`remove_in`], [`set_favorite_in`]). A request names the other player by a
//! gamertag of any of their profiles that is listed and not hidden ([`find_by_gamertag_in`]); the
//! asked account must be [`OPEN`]: active, not banned, a Supporter now (the entitlement rule at the
//! database clock, ACCOUNTS.md §7). Anything else answers like an unknown gamertag, so the tier is
//! never revealed. A friend (or an asker) is addressed by its **handle**, the XUID of the
//! account's first profile (any status: an erased profile's row stays, ACCOUNTS.md §8), or by any
//! XUID of its profiles; an outgoing request by the XUID of the profile it asked, so the asked
//! account's handle and other profiles stay unknown to the asker. The account id is never on the
//! wire.
//!
//! Every write takes [`lock_in`] on the two accounts first (transaction-scoped advisory locks in
//! one global order), so two crossing requests make one friendship and the limits
//! ([`MAX_FRIENDS`], [`MAX_OUTGOING`]) hold under concurrency. Requests lapse after
//! [`LAPSE_DAYS`] (the Beta's rule): a lapsed request is neither listed, counted nor accepted, and
//! a new request replaces it. The statements composed with `format!` (`sqlx::AssertSqlSafe`) join
//! constants of this crate only; every value is a bind parameter.

use crate::identity::{IdentityError, ms};
use crate::limits::Limit;
use crate::visibility::VISIBLE;
use fi_domain::UnixMs;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

/// Listed friend accounts ([`FRIEND_ROWS`]) per account, checked on both sides when a friendship
/// is made.
pub const MAX_FRIENDS: i64 = 200;
/// Live outgoing requests per account.
pub const MAX_OUTGOING: i64 = 100;
/// A request lapses this many days after it was made (the Beta's `request_friend`).
pub const LAPSE_DAYS: i64 = 30;
/// The friend list's bound (the cap counts listed friends; a friend hidden when the cap was
/// checked, banned for a while, may show again later, so the list allows for more).
pub const FRIENDS_SHOWN: i64 = 500;
/// The incoming and outgoing lists' bound (newest first).
pub const REQUESTS_SHOWN: i64 = 100;
const _: () = assert!(FRIENDS_SHOWN > MAX_FRIENDS && REQUESTS_SHOWN >= MAX_OUTGOING);
/// Friend requests per asking account (the Beta's 20 per hour), counted in the shared
/// `rate_limit` table under the key `friends.request:<HMAC of account:<id>>`.
pub const REQUESTS: Limit = Limit {
    name: "friends.request",
    max: 20,
    window_s: 3_600,
};
/// First key of the two-key advisory locks of accounts ("FIF0"; [`crate::limits::KEY_LOCK_NAMESPACE`]
/// is "FIK0", pool admission's "FIP0").
pub const LOCK_NAMESPACE: i32 = 0x4649_4630;

/// The SQL condition of an account `a` that is visible: active (not pending deletion, not
/// deleted) and not banned (a timed ban that has passed no longer hides it); the account half of
/// [`VISIBLE`].
pub const ACCOUNT_VISIBLE: &str = "a.status = 'active' \
     AND (a.banned_at IS NULL OR (a.banned_until IS NOT NULL AND a.banned_until <= now()))";

/// The SQL condition of an account `a` that may be asked or accepted: [`ACCOUNT_VISIBLE`], whatever
/// its tier (OD-129 replaced OD-25's "a Supporter now").
pub const OPEN: &str = ACCOUNT_VISIBLE;

/// The SQL condition of a live request `r` (younger than [`LAPSE_DAYS`]).
const LIVE: &str = "r.created_at > clock_timestamp() - interval '30 days'";

/// The SQL `CASE` that names account `viewer`'s relation to account `other` as the friend list
/// shows it ([`lists_in`]; MATCHMAKING_API.md §3.7, the world's roster): `self` (the same
/// account), `friends` (`viewer`'s friendship row with a visible account, [`FRIEND_ROWS`]'s rule),
/// `outgoing` (a live request of `viewer` to an [`OPEN`] `other`), `incoming` (a live request of an
/// [`OPEN`] `other` to `viewer`), else `none`. `viewer` and `other` are SQL expressions of the
/// caller's statement (constants of this crate only), which must not name the aliases `f`, `r` or
/// `a` the subqueries use. Each branch is a primary-key probe.
pub fn relation_sql(viewer: &str, other: &str) -> String {
    format!(
        "CASE WHEN {other} = {viewer} THEN 'self' \
         WHEN EXISTS (SELECT 1 FROM account_friendship f JOIN account a ON a.id = f.friend_id \
           WHERE f.account_id = {viewer} AND f.friend_id = {other} AND {ACCOUNT_VISIBLE}) THEN 'friends' \
         WHEN EXISTS (SELECT 1 FROM account_friend_request r JOIN account a ON a.id = r.to_account \
           WHERE r.from_account = {viewer} AND r.to_account = {other} AND {LIVE} AND {OPEN}) THEN 'outgoing' \
         WHEN EXISTS (SELECT 1 FROM account_friend_request r JOIN account a ON a.id = r.from_account \
           WHERE r.to_account = {viewer} AND r.from_account = {other} AND {LIVE} AND {OPEN}) THEN 'incoming' \
         ELSE 'none' END"
    )
}

/// `online`: one of account `a`'s profiles has an active play session now (the public card's
/// rule, per account).
const ONLINE: &str = "EXISTS (SELECT 1 FROM profile op JOIN play_session s ON s.profile_id = op.id \
     WHERE op.account_id = a.id AND s.state = 'active' AND s.expires_at > clock_timestamp() AND NOT s.dedicated)";

/// The handle `h.xuid` of account `a`: its first profile's XUID, whatever that profile's state.
/// An account that never had a profile has none and is never listed.
const HANDLE: &str = "CROSS JOIN LATERAL (SELECT h.xuid FROM profile h WHERE h.account_id = a.id \
     ORDER BY h.created_at, h.id LIMIT 1) h";

// ---- The game plane's friend reads (SER-M3 step 7, game-plane friends lane; GAME_PLATFORM.md §3a
// "Supporter social"). One statement per request, whatever the number of friends: the friend set
// joined with what the method needs (gamertags, presence, compare stats). Modes never meet
// (ACCOUNTS.md: boards, friends, free roam), so every read is of the viewer's mode. The viewer is
// a profile; its friends are the profiles of its account's friend accounts (OD-55).

/// The most friend profiles one game-plane read returns (the game's friends list, presence and
/// compare stats, and the friend sets of the boards, drivatars and rivals); the oldest
/// friendships first.
pub const MAX_FRIENDS_READ: u32 = 1_000;

/// Whether the profiles `a` and `b` of `game` are friends: their accounts are, with both rows of
/// the friendship (one side alone is nobody's friend, as in [`friends_in`]).
pub async fn are_friends_in(
    conn: &mut PgConnection,
    game: &str,
    a: Uuid,
    b: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM profile pa \
           JOIN profile pb ON pb.id = $3 AND pb.game = $1 \
           JOIN account_friendship f ON f.account_id = pa.account_id AND f.friend_id = pb.account_id \
           JOIN account_friendship back ON back.account_id = f.friend_id AND back.friend_id = f.account_id \
           WHERE pa.id = $2 AND pa.game = $1)",
    )
    .bind(game)
    .bind(a)
    .bind(b)
    .fetch_one(&mut *conn)
    .await?)
}

/// Whether the profile `xuid` is in the viewer's friend set ([`friends_in`]'s rule: a profile of a
/// friend account, both rows of the friendship, of the viewer's game and mode, visible). One
/// statement of key lookups: what a single player's presence asks before it answers.
pub async fn in_friend_set_in(
    conn: &mut PgConnection,
    viewer: &crate::visibility::Viewer<'_>,
    xuid: i64,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM profile p JOIN account a ON a.id = p.account_id \
           JOIN account_friendship f ON f.account_id = $2 AND f.friend_id = p.account_id \
           JOIN account_friendship back ON back.account_id = f.friend_id AND back.friend_id = f.account_id \
           WHERE p.xuid = $4 AND p.game = $1 AND p.mode = $3 AND p.name IS NOT NULL AND {VISIBLE})"
    )))
    .bind(viewer.game)
    .bind(viewer.account_id)
    .bind(viewer.mode)
    .bind(xuid)
    .fetch_one(&mut *conn)
    .await?)
}

/// One of the viewer's friends as the game plane shows them: a profile of a friend account (the
/// friendship holds both its rows), of the viewer's game and mode and visible
/// ([`crate::visibility::VISIBLE`]: active, its account active and not banned).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Friend {
    /// The friend's profile.
    pub profile_id: Uuid,
    /// Its XUID.
    pub xuid: i64,
    /// Gamertag name.
    pub name: String,
    /// Whether the viewer's account marked the friend a favorite (the viewer's side).
    pub favorite: bool,
}

/// The friend set statement with `columns` and `joins` added (each a constant of this module;
/// the friend's profile is `p`): `$1` game, `$2` the viewer's account, `$3` the viewer's mode,
/// `$4` the limit. The viewer's rows are a range of the friendship's primary key (`account_id`);
/// the other side's row and the account are key lookups, the friend account's profiles a range of
/// `profile_account`: the cost is the viewer's friend count (accounts, at most core's
/// [`MAX_FRIENDS`] listed), bounded by `$4` profiles.
fn game_friends_sql(columns: &str, joins: &str) -> String {
    format!(
        "SELECT p.id, p.xuid, p.name, f.favorite{columns} \
         FROM account_friendship f \
         JOIN account_friendship back ON back.account_id = f.friend_id \
           AND back.friend_id = f.account_id \
         JOIN account a ON a.id = f.friend_id \
         JOIN profile p ON p.account_id = f.friend_id{joins} \
         WHERE f.account_id = $2 AND p.game = $1 AND p.mode = $3 \
           AND p.name IS NOT NULL AND {VISIBLE} \
         ORDER BY f.created_at, f.friend_id, p.created_at, p.id \
         LIMIT $4",
    )
}

type FriendColumns = (Uuid, i64, Option<String>, bool);

fn friend_of((profile_id, xuid, name, favorite): FriendColumns) -> Friend {
    Friend {
        profile_id,
        xuid,
        name: name.unwrap_or_default(),
        favorite,
    }
}

static FRIENDS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static FRIENDS_PRESENCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static FRIENDS_STATS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static FRIENDS_BUCKET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static ASKED_BUCKET: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The viewer's friends as the game plane shows them ([`Friend`]), oldest friendship first, at
/// most `limit` (and [`MAX_FRIENDS_READ`]). One statement.
pub async fn friends_in(
    conn: &mut PgConnection,
    viewer: &crate::visibility::Viewer<'_>,
    limit: u32,
) -> Result<Vec<Friend>, IdentityError> {
    // Built from this module's constants only, once.
    let sql = FRIENDS.get_or_init(|| game_friends_sql("", ""));
    let rows: Vec<FriendColumns> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(viewer.game)
        .bind(viewer.account_id)
        .bind(viewer.mode)
        .bind(i64::from(limit.min(MAX_FRIENDS_READ)))
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.into_iter().map(friend_of).collect())
}

/// [`friends_in`] with each friend's presence as their game last posted it (`None`: never), read
/// on the database clock. One statement.
pub async fn friends_presence_in(
    conn: &mut PgConnection,
    viewer: &crate::visibility::Viewer<'_>,
    limit: u32,
) -> Result<Vec<(Friend, Option<crate::gameplane::Presence>)>, IdentityError> {
    type Row = (
        Uuid,
        i64,
        Option<String>,
        bool,
        Option<String>,
        Option<String>,
        Option<time::OffsetDateTime>,
        time::OffsetDateTime,
    );
    let sql = FRIENDS_PRESENCE.get_or_init(|| {
        game_friends_sql(
            ", g.state, g.rich, g.updated_at, clock_timestamp() AS now",
            " LEFT JOIN game_player_presence g ON g.game = $1 AND g.profile_id = p.id",
        )
    });
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(viewer.game)
        .bind(viewer.account_id)
        .bind(viewer.mode)
        .bind(i64::from(limit.min(MAX_FRIENDS_READ)))
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, xuid, name, favorite, state, rich, updated, now)| {
            let presence = match (state, updated) {
                (Some(state), Some(updated)) => Some(crate::gameplane::Presence {
                    state,
                    rich: rich.unwrap_or_default(),
                    updated_at: crate::identity::ms(updated),
                    now: crate::identity::ms(now),
                }),
                _ => None,
            };
            (friend_of((id, xuid, name, favorite)), presence)
        })
        .collect())
}

/// [`friends_in`] with each friend's career compare stats (`compareStats[0]` of their merged
/// online profile, `game_profile`; `None` when they synced none). One statement.
pub async fn friends_compare_stats_in(
    conn: &mut PgConnection,
    viewer: &crate::visibility::Viewer<'_>,
    limit: u32,
) -> Result<Vec<(Friend, Option<serde_json::Value>)>, IdentityError> {
    type Row = (Uuid, i64, Option<String>, bool, Option<serde_json::Value>);
    let sql = FRIENDS_STATS.get_or_init(|| {
        game_friends_sql(
            ", gp.data #> '{compareStats,0}' AS stats",
            " LEFT JOIN game_profile gp ON gp.game = $1 AND gp.profile_id = p.id",
        )
    });
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(viewer.game)
        .bind(viewer.account_id)
        .bind(viewer.mode)
        .bind(i64::from(limit.min(MAX_FRIENDS_READ)))
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, xuid, name, favorite, stats)| (friend_of((id, xuid, name, favorite)), stats))
        .collect())
}

/// The most XUIDs of one per-bucket compare stats request looked up (the game asks for its
/// friends; a reply holds at most 100).
pub const MAX_ASKED: usize = 1_000;

/// The per-bucket compare stats (row `bucket` of the online-profile table `bucketedCompareStats`,
/// OnlineProfile.UpdateAllTables, whose rows are keyed by their `Id`; `None` when the profile
/// synced none) of the XUIDs `asked`, in the order asked (a repeated XUID once), or of the
/// viewer's friends ([`friends_in`]) when `asked` is `None`. An asked XUID is answered only when
/// it is the viewer's own profile or in the viewer's friend set (a friend account's profile, both
/// rows, the viewer's game and mode, visible): any other XUID is left out without a word, so the
/// population cannot be read by XUID. At most `limit` (and [`MAX_FRIENDS_READ`]), the first
/// [`MAX_ASKED`] distinct XUIDs looked at. One statement; a profile's row is one key lookup in
/// its table.
pub async fn bucket_stats_in(
    conn: &mut PgConnection,
    viewer: &crate::visibility::Viewer<'_>,
    bucket: i32,
    asked: Option<&[i64]>,
    limit: u32,
) -> Result<Vec<(i64, Option<serde_json::Value>)>, IdentityError> {
    type Row = (Uuid, i64, Option<String>, bool, Option<serde_json::Value>);
    let limit = i64::from(limit.min(MAX_FRIENDS_READ));
    let Some(asked) = asked else {
        let sql = FRIENDS_BUCKET.get_or_init(|| {
            game_friends_sql(
                ", t.data -> $5::text AS stats",
                " LEFT JOIN game_profile_table t ON t.game = $1 AND t.profile_id = p.id \
                   AND t.name = 'bucketedCompareStats'",
            )
        });
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(viewer.game)
            .bind(viewer.account_id)
            .bind(viewer.mode)
            .bind(limit)
            .bind(bucket.to_string())
            .fetch_all(&mut *conn)
            .await?;
        return Ok(rows
            .into_iter()
            .map(|(_, xuid, _, _, stats)| (xuid, stats))
            .collect());
    };
    let mut seen = std::collections::HashSet::new();
    let wanted: Vec<i64> = asked
        .iter()
        .copied()
        .filter(|x| seen.insert(*x))
        .take(MAX_ASKED)
        .collect();
    if wanted.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let sql = ASKED_BUCKET.get_or_init(|| {
        format!(
            "SELECT p.xuid, t.data -> $4::text AS stats \
             FROM unnest($3::bigint[]) WITH ORDINALITY AS w(xuid, ord) \
             JOIN profile p ON p.xuid = w.xuid \
             JOIN account a ON a.id = p.account_id \
             LEFT JOIN game_profile_table t ON t.game = $1 AND t.profile_id = p.id \
               AND t.name = 'bucketedCompareStats' \
             WHERE p.game = $1 AND p.mode = $2 AND p.name IS NOT NULL AND {VISIBLE} \
               AND (p.id = $6 OR EXISTS (SELECT 1 FROM account_friendship f \
                 JOIN account_friendship back ON back.account_id = f.friend_id \
                   AND back.friend_id = f.account_id \
                 WHERE f.account_id = $7 AND f.friend_id = p.account_id)) \
             ORDER BY w.ord \
             LIMIT $5",
        )
    });
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(&wanted)
        .bind(bucket.to_string())
        .bind(limit)
        .bind(viewer.profile_id)
        .bind(viewer.account_id)
        .fetch_all(&mut *conn)
        .await?)
}

// ---- Core's writes and lists, per account (CORE_API.md §4.1) ----------------------------------

/// The account a friend route acts for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The account.
    pub account_id: Uuid,
    /// The account is `active` (not pending deletion).
    pub active: bool,
    /// The account has (or had) a profile: it has a handle, so the other side can see and
    /// address it; an account without one cannot ask anyone ([`Asked::NoProfile`]).
    pub has_profile: bool,
}

/// `account_id`'s state for the friend routes; `None` when the account does not exist.
pub async fn caller_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<Caller>, IdentityError> {
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT a.status = 'active', EXISTS (SELECT 1 FROM profile p WHERE p.account_id = a.id) \
         FROM account a WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(active, has_profile)| Caller {
        account_id,
        active,
        has_profile,
    }))
}

/// The profile a request names and its account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The account asked.
    pub account_id: Uuid,
    /// The profile named (the asker's outgoing row shows it, and only it).
    pub profile_id: Uuid,
}

/// The listed, not hidden, live profile of `game` with the plain gamertag `name`, or with the
/// legacy `name#discriminator` when `discriminator` is given (OD-120; the name that went with the
/// number, whatever the profile is called now); names compared without regard to case, any mode;
/// and its [`OPEN`] account. `None` for anything else, whatever the reason.
pub async fn find_by_gamertag_in(
    conn: &mut PgConnection,
    game: &str,
    name: &str,
    discriminator: Option<u32>,
) -> Result<Option<Target>, IdentityError> {
    let disc = match discriminator.map(i32::try_from) {
        None => None,
        Some(Ok(d)) => Some(d),
        Some(Err(_)) => return Ok(None),
    };
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT a.id, p.id FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.game = $1 AND CASE WHEN $3::int IS NULL THEN lower(p.name) = lower($2) \
             ELSE lower(coalesce(p.legacy_name, p.name)) = lower($2) AND p.discriminator = $3 END \
           AND p.status = 'active' AND p.social_listed AND p.social_hidden_at IS NULL AND {OPEN}"
    )))
    .bind(game)
    .bind(name)
    .bind(disc)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(account_id, profile_id)| Target {
        account_id,
        profile_id,
    }))
}

/// The listed, not hidden, live profile of `game` with XUID `xuid` (a player the game shows, any
/// mode; OD-193, the game's own player menus) and its [`OPEN`] account, under
/// [`find_by_gamertag_in`]'s rule. `None` for anything else, whatever the reason.
pub async fn find_by_xuid_in(
    conn: &mut PgConnection,
    game: &str,
    xuid: u64,
) -> Result<Option<Target>, IdentityError> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(None);
    };
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT a.id, p.id FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.game = $1 AND p.xuid = $2 \
           AND p.status = 'active' AND p.social_listed AND p.social_hidden_at IS NULL AND {OPEN}"
    )))
    .bind(game)
    .bind(xuid)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(account_id, profile_id)| Target {
        account_id,
        profile_id,
    }))
}

/// The account of the profile `xuid`, of any game and state (a handle or any XUID of the
/// account's profiles; an erased profile's row stays, ACCOUNTS.md §8).
pub async fn account_of_xuid_in(
    conn: &mut PgConnection,
    xuid: u64,
) -> Result<Option<Uuid>, IdentityError> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(None);
    };
    Ok(
        sqlx::query_scalar("SELECT account_id FROM profile WHERE xuid = $1")
            .bind(xuid)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// A profile of a listed account, as the lists show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileCard {
    /// XUID.
    pub xuid: u64,
    /// `Name#12345`.
    pub gamertag: String,
    /// Game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The play dashboard lists it (CR-S83 `web_listed`, not hidden by the report rule; CR-S102).
    pub web_listed: bool,
}

/// A friend account's verified Discord link (`account.discord_*`, CORE_API.md §5a.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordCard {
    /// The Discord user's snowflake (never on the wire here: only the avatar URL built from it).
    pub user_id: i64,
    /// Its handle.
    pub username: String,
    /// Its display name.
    pub global_name: Option<String>,
    /// Its avatar hash (`None`: Discord's default avatar).
    pub avatar: Option<String>,
}

/// A friend account as listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendRow {
    /// The friend account (never on the wire).
    pub account_id: Uuid,
    /// Its handle: the XUID of its first profile, whatever that profile's state.
    pub handle: u64,
    /// Its live profiles, of every game and mode (empty when all were deleted).
    pub profiles: Vec<ProfileCard>,
    /// The caller's favorite mark.
    pub favorite: bool,
    /// One of its profiles has an active play session.
    pub online: bool,
    /// The friendship (the caller's row) was made.
    pub since: UnixMs,
    /// The friend account is a Supporter now (OD-56: its tier on the row).
    pub vip: bool,
    /// The profile it plays now, or played last ([`Playing`]); `None` when it never played or
    /// that profile was deleted.
    pub playing: Option<Playing>,
    /// Its newest pending invite to the caller's account, to an open world.
    pub invite: Option<FriendInviteRow>,
    /// When the friend account was created (CR-S102).
    pub account_since: UnixMs,
    /// Its verified Discord link (CR-S102).
    pub discord: Option<DiscordCard>,
}

/// What a friend is doing (CORE_API.md §4.1 "Presence", OD-56).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlayState {
    /// In a world: hosting an open one or holding a live seat in one.
    InWorld,
    /// In the game: an active play session.
    InGame,
    /// Neither (the profile is the one played last).
    Offline,
}

impl PlayState {
    /// The wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            PlayState::InWorld => "in_world",
            PlayState::InGame => "in_game",
            PlayState::Offline => "offline",
        }
    }
}

/// The profile a friend plays now (or played last).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playing {
    /// The profile.
    pub profile_id: Uuid,
    /// Its XUID.
    pub xuid: u64,
    /// `Name#12345`.
    pub gamertag: String,
    /// Game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// What it is doing.
    pub state: PlayState,
    /// `in_world`: the world it hosts or holds a seat in.
    pub world_id: Option<Uuid>,
    /// OD-190: that world is an official one (a seat), running this activity.
    pub official_activity: Option<String>,
    /// CR-S102: `in_world`, what it does there.
    pub doing: Option<Doing>,
}

/// What an `in_world` friend does in its world (CORE_API.md §4.2a, CR-S102).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doing {
    /// The world's activity (`world.activity`).
    pub activity: String,
    /// A private official world (an owner, MATCHMAKING_API.md §2.10).
    pub private: bool,
    /// FH2 only: its session's latest activity report, at most
    /// [`fi_domain::activity::REPORT_FRESH_SECS`] old, shows a race screen
    /// ([`fi_domain::activity::racing`]); `None` without such a report.
    pub racing: Option<bool>,
}

/// A friend's pending invite to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FriendInviteRow {
    /// The invite.
    pub id: Uuid,
    /// Its world.
    pub world_id: Uuid,
    /// When it lapses.
    pub expires_at: UnixMs,
}

/// A live request as listed (the other account).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    /// The other account (never on the wire).
    pub account_id: Uuid,
    /// Its address on the wire: an incoming row the asker's handle; an outgoing row the XUID of
    /// the profile the caller asked (never the asked account's handle).
    pub id: u64,
    /// An incoming row: all the asker's live profiles (the asker showed itself). An outgoing row:
    /// only the profile asked, while a public card shows it (live, listed, not hidden), else none.
    pub profiles: Vec<ProfileCard>,
    /// The request was made.
    pub since: UnixMs,
}

/// An account's friends and live requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lists {
    /// Listed friend accounts ([`FRIEND_ROWS`]) with their presence, in a world first, then in the
    /// game, then inviting the caller, then the rest (favorites first within each, then the
    /// oldest friendships), at most [`FRIENDS_SHOWN`].
    pub friends: Vec<FriendRow>,
    /// Live requests of [`OPEN`] accounts to this one, newest first, at most [`REQUESTS_SHOWN`].
    pub incoming: Vec<RequestRow>,
    /// Live requests of this account to [`OPEN`] accounts, newest first, at most
    /// [`REQUESTS_SHOWN`].
    pub outgoing: Vec<RequestRow>,
}

/// The friend accounts `a` of `$1` that are listed and counted: visible ([`ACCOUNT_VISIBLE`])
/// and with a handle (`h`).
pub const FRIEND_ROWS: &str = "FROM account_friendship f JOIN account a ON a.id = f.friend_id \
     CROSS JOIN LATERAL (SELECT h.xuid FROM profile h WHERE h.account_id = a.id \
       ORDER BY h.created_at, h.id LIMIT 1) h \
     WHERE f.account_id = $1 AND a.status = 'active' \
       AND (a.banned_at IS NULL OR (a.banned_until IS NOT NULL AND a.banned_until <= now()))";

/// A friend account `a`'s presence (OD-56, CORE_API.md §4.1), joined to [`FRIEND_ROWS`] in the
/// same statement: `cur` the profile of its active play session (`play_session_live_by_account`),
/// else `last` the profile of its latest one (`play_session_account_history`), dedicated lobbies'
/// host sessions never (OD-109: hosting one is not playing); `pp` that profile
/// when live; `pw` the open world that profile hosts or holds a live seat in, only while it plays
/// (`world_one_open_per_host`, `mm_grant_one_live`); `inv` the friend's newest pending invite to
/// `$1`, to an open world. Each lateral is a few index lookups per friend.
const PRESENCE: &str = "LEFT JOIN LATERAL (SELECT s.profile_id, \
         CASE WHEN s.activity_reported_at > clock_timestamp() - make_interval(secs => 90) THEN s.activity_rp END AS rp \
       FROM play_session s WHERE s.account_id = a.id \
       AND s.state = 'active' AND s.expires_at > clock_timestamp() AND NOT s.dedicated ORDER BY s.created_at DESC LIMIT 1) cur ON true \
     LEFT JOIN LATERAL (SELECT s.profile_id FROM play_session s WHERE s.account_id = a.id AND NOT s.dedicated \
       ORDER BY s.created_at DESC LIMIT 1) last ON cur.profile_id IS NULL \
     LEFT JOIN profile pp ON pp.id = coalesce(cur.profile_id, last.profile_id) \
       AND pp.status = 'active' AND pp.name IS NOT NULL \
     LEFT JOIN LATERAL (SELECT x.world_id, x.official_activity, x.activity, x.private FROM ( \
         SELECT w.id AS world_id, NULL::text AS official_activity, w.activity, false AS private FROM world w WHERE w.game = pp.game AND w.host_profile_id = pp.id \
           AND NOT w.dedicated AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
         UNION ALL \
         SELECT g.world_id, CASE WHEN w.host_kind = 'official' AND NOT w.official_group THEN w.activity END, \
           w.activity, w.official_owner_account_id IS NOT NULL FROM mm_grant g JOIN world w ON w.id = g.world_id \
           WHERE g.game = pp.game AND g.profile_id = pp.id \
             AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())) \
             AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
             AND (w.official_owner_account_id IS NULL OR EXISTS (SELECT 1 FROM world_access wa \
               WHERE wa.world_id = w.id AND wa.account_id = $1))) x \
       WHERE cur.profile_id IS NOT NULL LIMIT 1) pw ON true \
     LEFT JOIN LATERAL (SELECT i.id, i.world_id, i.expires_at FROM world_invite i JOIN world w ON w.id = i.world_id \
       WHERE i.from_account_id = a.id AND i.to_account_id = $1 AND i.kind = 'invite' AND i.state = 'pending' \
         AND i.expires_at > clock_timestamp() AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
       ORDER BY i.created_at DESC LIMIT 1) inv ON true";

/// The presence columns of [`PRESENCE`].
const PRESENCE_COLUMNS: &str = "EXISTS (SELECT 1 FROM entitlement e WHERE e.account_id = a.id AND e.product = 'vip' \
       AND e.status IN ('active', 'grace') AND e.access_until > clock_timestamp()) AS vip, \
     pp.id AS play_profile, pp.xuid AS play_xuid, pp.name AS play_name, \
     pp.game AS play_game, pp.mode AS play_mode, cur.profile_id IS NOT NULL AS play_live, pw.world_id AS play_world, pw.official_activity AS play_official_activity, \
     pw.activity AS play_activity, pw.private AS play_private, cur.rp AS play_rp, \
     a.created_at AS account_created, a.discord_user_id, a.discord_username, a.discord_global_name, a.discord_avatar, \
     inv.id AS invite_id, inv.world_id AS invite_world, inv.expires_at AS invite_expires";

/// The list's order (CORE_API.md §4.1): in a world, in the game, inviting the caller, the rest;
/// favorites first within each, then the oldest friendships.
const PRESENCE_ORDER: &str = "ORDER BY CASE WHEN pp.id IS NOT NULL AND pw.world_id IS NOT NULL THEN 0 \
       WHEN pp.id IS NOT NULL AND cur.profile_id IS NOT NULL THEN 1 WHEN inv.id IS NOT NULL THEN 2 ELSE 3 END, \
     f.favorite DESC, f.created_at, a.id";

fn friends_sql(filter: &str) -> String {
    // FRIEND_ROWS ends with its own WHERE (the last one; the handle's subquery has another): the
    // presence joins go before it.
    let (from, filter_rows) = FRIEND_ROWS
        .rsplit_once(" WHERE ")
        .expect("FRIEND_ROWS has a WHERE");
    format!(
        "SELECT a.id, h.xuid AS handle, f.favorite, f.created_at, {ONLINE} AS online, {PRESENCE_COLUMNS} \
         {from} {PRESENCE} WHERE {filter_rows} {filter}"
    )
}

/// One listed account's head, before its profiles are read.
struct Head {
    account_id: Uuid,
    handle: u64,
    favorite: bool,
    online: bool,
    since: UnixMs,
    vip: bool,
    playing: Option<Playing>,
    invite: Option<FriendInviteRow>,
    account_since: UnixMs,
    discord: Option<DiscordCard>,
}

fn head_of(r: &PgRow) -> Result<Head, sqlx::Error> {
    // Request rows carry no presence columns: those read as absent.
    let opt = |name: &str| r.try_get::<Option<Uuid>, _>(name).ok().flatten();
    let playing = match opt("play_profile") {
        None => None,
        Some(profile_id) => {
            let name: String = r.try_get("play_name")?;
            let world_id = opt("play_world");
            let live: bool = r.try_get("play_live")?;
            Some(Playing {
                profile_id,
                xuid: r.try_get::<i64, _>("play_xuid")? as u64,
                gamertag: name,
                game: r.try_get("play_game")?,
                mode: r.try_get("play_mode")?,
                state: match (world_id, live) {
                    (Some(_), _) => PlayState::InWorld,
                    (None, true) => PlayState::InGame,
                    (None, false) => PlayState::Offline,
                },
                world_id,
                official_activity: world_id.and(
                    r.try_get::<Option<String>, _>("play_official_activity")
                        .ok()
                        .flatten(),
                ),
                doing: match (world_id, r.try_get::<Option<String>, _>("play_activity")) {
                    (Some(_), Ok(Some(activity))) => Some(Doing {
                        activity,
                        private: r
                            .try_get::<Option<bool>, _>("play_private")?
                            .unwrap_or(false),
                        racing: r
                            .try_get::<Option<i16>, _>("play_rp")?
                            .and_then(|rp| u8::try_from(rp).ok())
                            .and_then(|rp| {
                                fi_domain::activity::racing(
                                    r.try_get::<&str, _>("play_game").unwrap_or_default(),
                                    rp,
                                )
                            }),
                    }),
                    _ => None,
                },
            })
        }
    };
    let invite = match (opt("invite_id"), opt("invite_world")) {
        (Some(id), Some(world_id)) => Some(FriendInviteRow {
            id,
            world_id,
            expires_at: ms(r.try_get::<OffsetDateTime, _>("invite_expires")?),
        }),
        _ => None,
    };
    // Request rows carry no account columns either: their creation reads as the epoch, unused.
    let discord = match r
        .try_get::<Option<i64>, _>("discord_user_id")
        .ok()
        .flatten()
    {
        Some(user_id) => Some(DiscordCard {
            user_id,
            username: r.try_get("discord_username")?,
            global_name: r.try_get("discord_global_name")?,
            avatar: r.try_get("discord_avatar")?,
        }),
        None => None,
    };
    Ok(Head {
        account_since: r
            .try_get::<OffsetDateTime, _>("account_created")
            .map(ms)
            .unwrap_or(UnixMs::from_millis(0)),
        discord,
        account_id: r.try_get("id")?,
        handle: r.try_get::<i64, _>("handle")? as u64,
        favorite: r.try_get("favorite").unwrap_or(false),
        online: r.try_get("online").unwrap_or(false),
        since: ms(r.try_get::<OffsetDateTime, _>("created_at")?),
        vip: r.try_get("vip").unwrap_or(false),
        playing,
        invite,
    })
}

/// The live, named profiles of `accounts` (friends and askers: they showed themselves).
async fn profiles_of_in(
    conn: &mut PgConnection,
    accounts: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ProfileCard>>, IdentityError> {
    let mut by_account: HashMap<Uuid, Vec<ProfileCard>> = HashMap::new();
    if accounts.is_empty() {
        return Ok(by_account);
    }
    let rows = sqlx::query(
        "SELECT p.account_id, p.xuid, p.name, p.game, p.mode, \
           (p.web_listed AND p.social_hidden_at IS NULL) AS web \
         FROM profile p WHERE p.account_id = ANY($1) AND p.status = 'active' AND p.name IS NOT NULL \
         ORDER BY p.account_id, p.game, p.created_at, p.id",
    )
    .bind(accounts)
    .fetch_all(&mut *conn)
    .await?;
    for r in &rows {
        let name: String = r.try_get("name")?;
        by_account
            .entry(r.try_get("account_id")?)
            .or_default()
            .push(ProfileCard {
                xuid: r.try_get::<i64, _>("xuid")? as u64,
                gamertag: name,
                game: r.try_get("game")?,
                mode: r.try_get("mode")?,
                web_listed: r.try_get("web")?,
            });
    }
    Ok(by_account)
}

fn cards(profiles: &HashMap<Uuid, Vec<ProfileCard>>, account: Uuid) -> Vec<ProfileCard> {
    profiles.get(&account).cloned().unwrap_or_default()
}

/// The live requests to `$1` of [`OPEN`] accounts `a`, with the asker's handle.
fn incoming_sql(filter: &str) -> String {
    format!(
        "SELECT a.id, h.xuid AS handle, r.created_at \
         FROM account_friend_request r JOIN account a ON a.id = r.from_account {HANDLE} \
         WHERE r.to_account = $1 AND {LIVE} AND {OPEN} {filter}"
    )
}

/// The live requests of `$1` to [`OPEN`] accounts `a`, with the profile asked (`ap`) and whether
/// a public card shows it.
fn outgoing_sql(filter: &str) -> String {
    format!(
        "SELECT a.id, ap.xuid, ap.name, ap.game, ap.mode, r.created_at, \
                (ap.status = 'active' AND ap.name IS NOT NULL AND ap.social_listed \
                 AND ap.social_hidden_at IS NULL) AS shown, \
                (ap.web_listed AND ap.social_hidden_at IS NULL) AS web \
         FROM account_friend_request r JOIN account a ON a.id = r.to_account \
         JOIN profile ap ON ap.id = r.asked_profile \
         WHERE r.from_account = $1 AND {LIVE} AND {OPEN} {filter}"
    )
}

fn outgoing_of(r: &PgRow) -> Result<RequestRow, sqlx::Error> {
    let shown: bool = r.try_get("shown")?;
    let xuid = r.try_get::<i64, _>("xuid")? as u64;
    let profiles = if shown {
        let name: String = r.try_get("name")?;
        vec![ProfileCard {
            xuid,
            gamertag: name,
            game: r.try_get("game")?,
            mode: r.try_get("mode")?,
            web_listed: r.try_get("web")?,
        }]
    } else {
        Vec::new()
    };
    Ok(RequestRow {
        account_id: r.try_get("id")?,
        id: xuid,
        profiles,
        since: ms(r.try_get::<OffsetDateTime, _>("created_at")?),
    })
}

/// `account_id`'s friends and live requests (see [`Lists`]).
pub async fn lists_in(conn: &mut PgConnection, account_id: Uuid) -> Result<Lists, IdentityError> {
    let friends: Vec<Head> = sqlx::query(sqlx::AssertSqlSafe(friends_sql(&format!(
        "{PRESENCE_ORDER} LIMIT $2"
    ))))
    .bind(account_id)
    .bind(FRIENDS_SHOWN)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(head_of)
    .collect::<Result<_, _>>()?;
    let incoming: Vec<Head> = sqlx::query(sqlx::AssertSqlSafe(incoming_sql(
        "ORDER BY r.created_at DESC, a.id LIMIT $2",
    )))
    .bind(account_id)
    .bind(REQUESTS_SHOWN)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(head_of)
    .collect::<Result<_, _>>()?;
    let outgoing: Vec<RequestRow> = sqlx::query(sqlx::AssertSqlSafe(outgoing_sql(
        "ORDER BY r.created_at DESC, a.id LIMIT $2",
    )))
    .bind(account_id)
    .bind(REQUESTS_SHOWN)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(outgoing_of)
    .collect::<Result<_, _>>()?;
    let accounts: Vec<Uuid> = friends
        .iter()
        .chain(&incoming)
        .map(|h| h.account_id)
        .collect();
    let profiles = profiles_of_in(conn, &accounts).await?;
    Ok(Lists {
        friends: friends
            .iter()
            .map(|h| friend_row(h, cards(&profiles, h.account_id)))
            .collect(),
        incoming: incoming
            .iter()
            .map(|h| RequestRow {
                account_id: h.account_id,
                id: h.handle,
                profiles: cards(&profiles, h.account_id),
                since: h.since,
            })
            .collect(),
        outgoing,
    })
}

fn friend_row(h: &Head, profiles: Vec<ProfileCard>) -> FriendRow {
    FriendRow {
        account_id: h.account_id,
        handle: h.handle,
        profiles,
        favorite: h.favorite,
        online: h.online,
        since: h.since,
        vip: h.vip,
        playing: h.playing.clone(),
        invite: h.invite,
        account_since: h.account_since,
        discord: h.discord.clone(),
    }
}

/// `friend_id` as `account_id`'s listed friend ([`FRIEND_ROWS`]), when it is one.
pub async fn friend_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    friend_id: Uuid,
) -> Result<Option<FriendRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(friends_sql("AND f.friend_id = $2")))
        .bind(account_id)
        .bind(friend_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(head) = row.as_ref().map(head_of).transpose()? else {
        return Ok(None);
    };
    let profiles = profiles_of_in(conn, &[friend_id]).await?;
    Ok(Some(friend_row(&head, cards(&profiles, friend_id))))
}

/// The live request of `from` to `to` when `to` is [`OPEN`], as `from` lists it (outgoing: the
/// profile it asked).
async fn outgoing_in(
    conn: &mut PgConnection,
    from: Uuid,
    to: Uuid,
) -> Result<Option<RequestRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(outgoing_sql("AND r.to_account = $2")))
        .bind(from)
        .bind(to)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.as_ref().map(outgoing_of).transpose()?)
}

/// The lock key of an account (its id folded to 32 bits).
fn lock_key(id: Uuid) -> i32 {
    let n = id.as_u128();
    let folded = (n ^ (n >> 32) ^ (n >> 64) ^ (n >> 96)) as u32;
    i32::from_ne_bytes(folded.to_ne_bytes())
}

/// Serializes the friend writes of accounts `a` and `b` until the caller's transaction ends: one
/// transaction-scoped advisory lock per account, taken in the order of the lock keys (so two
/// writes never wait on each other in a cycle; two accounts whose keys collide share a lock).
/// Every write of this module takes it first.
pub async fn lock_in(conn: &mut PgConnection, a: Uuid, b: Uuid) -> Result<(), IdentityError> {
    let mut keys = [lock_key(a), lock_key(b)];
    keys.sort_unstable();
    let keys: &[i32] = if keys[0] == keys[1] {
        &keys[..1]
    } else {
        &keys
    };
    for key in keys {
        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(LOCK_NAMESPACE)
            .bind(*key)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Whose limit refused a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The caller's account.
    Caller,
    /// The other account.
    Other,
}

/// A limit that refused a write (`SOCIAL.LIMIT_REACHED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Full {
    /// [`MAX_FRIENDS`] listed friends on that side.
    Friends(Side),
    /// [`MAX_OUTGOING`] live outgoing requests of the caller.
    Outgoing,
}

/// What a request by gamertag did.
#[derive(Clone, Debug, PartialEq, Eq)]
// One answer per request: the friend row's presence and sidebar data (CR-S102) make this variant
// larger than the others, which costs nothing here.
#[allow(clippy::large_enum_variant)]
pub enum Asked {
    /// A request is pending: just made, or made before (nothing written).
    Requested(RequestRow),
    /// The two are friends: just made (the target had asked), or before (nothing written).
    Friends(FriendRow),
    /// The target is the caller's own account: refused, nothing written.
    Own,
    /// The caller's account never had a profile, so the other side could neither see nor
    /// address it: refused, nothing written.
    NoProfile,
    /// Refused by a limit, nothing written.
    Full(Full),
    /// The target stopped being listed while the request ran: answer as unknown.
    Gone,
}

/// What an accept did.
#[derive(Clone, Debug, PartialEq, Eq)]
// Built once per request; the friend row carries its presence (OD-56).
#[allow(clippy::large_enum_variant)]
pub enum Accepted {
    /// The two are friends: just made, or before (nothing written).
    Friends(FriendRow),
    /// The other account is not [`OPEN`], or made no live request to the caller (and they are
    /// not friends): nothing written.
    NotFound,
    /// Refused by a limit, nothing written.
    Full(Full),
}

/// Listed friends of `account_id` (what [`MAX_FRIENDS`] counts: the same rows the list shows).
async fn listed_friends_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<i64, IdentityError> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) {FRIEND_ROWS}"
    )))
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// `None` when both sides have room for one more friend.
async fn room_in(
    conn: &mut PgConnection,
    caller: Uuid,
    other: Uuid,
) -> Result<Option<Full>, IdentityError> {
    for (account, side) in [(caller, Side::Caller), (other, Side::Other)] {
        if listed_friends_in(conn, account).await? >= MAX_FRIENDS {
            return Ok(Some(Full::Friends(side)));
        }
    }
    Ok(None)
}

/// Removes the lapsed requests between `a` and `b`, either way.
async fn drop_lapsed_in(conn: &mut PgConnection, a: Uuid, b: Uuid) -> Result<(), IdentityError> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM account_friend_request r \
         WHERE ((r.from_account = $1 AND r.to_account = $2) OR (r.from_account = $2 AND r.to_account = $1)) \
           AND NOT ({LIVE})"
    )))
    .bind(a)
    .bind(b)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Whether `from` made a live request to `to`.
async fn live_request_in(
    conn: &mut PgConnection,
    from: Uuid,
    to: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM account_friend_request r \
           WHERE r.from_account = $1 AND r.to_account = $2 AND {LIVE})"
    )))
    .bind(from)
    .bind(to)
    .fetch_one(&mut *conn)
    .await?)
}

async fn delete_requests_in(
    conn: &mut PgConnection,
    a: Uuid,
    b: Uuid,
) -> Result<u64, IdentityError> {
    Ok(sqlx::query(
        "DELETE FROM account_friend_request \
         WHERE (from_account = $1 AND to_account = $2) OR (from_account = $2 AND to_account = $1)",
    )
    .bind(a)
    .bind(b)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// What making a friendship did.
#[allow(clippy::large_enum_variant)]
enum Made {
    /// Made: the friend as the caller lists it.
    Friend(FriendRow),
    /// The other account stopped being listed while the write ran (answer as unknown; roll back).
    Gone,
    /// Refused by a limit, nothing written.
    Full(Full),
}

/// Makes the friendship of `me` and `them` when both sides have room: both rows (each side's
/// `favorite` false), no request left between the two.
async fn made_in(conn: &mut PgConnection, me: Uuid, them: Uuid) -> Result<Made, IdentityError> {
    if let Some(full) = room_in(conn, me, them).await? {
        return Ok(Made::Full(full));
    }
    sqlx::query(
        "INSERT INTO account_friendship (account_id, friend_id) VALUES ($1, $2), ($2, $1) \
         ON CONFLICT DO NOTHING",
    )
    .bind(me)
    .bind(them)
    .execute(&mut *conn)
    .await?;
    delete_requests_in(conn, me, them).await?;
    Ok(match friend_in(conn, me, them).await? {
        Some(f) => Made::Friend(f),
        None => Made::Gone,
    })
}

/// The caller asks `target` (found by [`find_by_gamertag_in`]) to be friends: already friends →
/// [`Asked::Friends`]; the target asked the caller (a live request) → the friendship (both rows,
/// every request between the two removed), [`Asked::Friends`]; the caller asked before →
/// [`Asked::Requested`] with the first request (and the profile it named then); else a request
/// row naming `target`'s profile. The outgoing limit counts the requests the caller's list shows
/// (live, to [`OPEN`] accounts), so it is never reached with fewer than [`MAX_OUTGOING`] shown.
/// Takes [`lock_in`]; run it in a transaction and roll back on anything but a success.
pub async fn request_in(
    conn: &mut PgConnection,
    caller: &Caller,
    target: Target,
) -> Result<Asked, IdentityError> {
    let me = caller.account_id;
    let (target, asked_profile) = (target.account_id, target.profile_id);
    if target == me {
        return Ok(Asked::Own);
    }
    if !caller.has_profile {
        return Ok(Asked::NoProfile);
    }
    lock_in(conn, me, target).await?;
    if let Some(f) = friend_in(conn, me, target).await? {
        return Ok(Asked::Friends(f));
    }
    drop_lapsed_in(conn, me, target).await?;
    if live_request_in(conn, target, me).await? {
        return Ok(match made_in(conn, me, target).await? {
            Made::Friend(f) => Asked::Friends(f),
            Made::Gone => Asked::Gone,
            Made::Full(full) => Asked::Full(full),
        });
    }
    if let Some(r) = outgoing_in(conn, me, target).await? {
        return Ok(Asked::Requested(r));
    }
    let outgoing: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM account_friend_request r JOIN account a ON a.id = r.to_account \
         WHERE r.from_account = $1 AND {LIVE} AND {OPEN}"
    )))
    .bind(me)
    .fetch_one(&mut *conn)
    .await?;
    if outgoing >= MAX_OUTGOING {
        return Ok(Asked::Full(Full::Outgoing));
    }
    sqlx::query(
        "INSERT INTO account_friend_request (from_account, to_account, asked_profile) \
         VALUES ($1, $2, $3)",
    )
    .bind(me)
    .bind(target)
    .bind(asked_profile)
    .execute(&mut *conn)
    .await?;
    Ok(match outgoing_in(conn, me, target).await? {
        Some(r) => Asked::Requested(r),
        None => Asked::Gone,
    })
}

/// The caller accepts `other`'s request (`other` any account; found by a handle or a XUID):
/// already friends → [`Accepted::Friends`]; `other` [`OPEN`] with a live request to the caller →
/// the friendship, [`Accepted::Friends`]; else [`Accepted::NotFound`]. Takes [`lock_in`]; run it
/// in a transaction and roll back on anything but a success.
pub async fn accept_in(
    conn: &mut PgConnection,
    caller: &Caller,
    other: Uuid,
) -> Result<Accepted, IdentityError> {
    let me = caller.account_id;
    if other == me {
        return Ok(Accepted::NotFound);
    }
    lock_in(conn, me, other).await?;
    if let Some(f) = friend_in(conn, me, other).await? {
        return Ok(Accepted::Friends(f));
    }
    let open: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM account a WHERE a.id = $1 AND {OPEN})"
    )))
    .bind(other)
    .fetch_one(&mut *conn)
    .await?;
    if !open || !live_request_in(conn, other, me).await? {
        return Ok(Accepted::NotFound);
    }
    Ok(match made_in(conn, me, other).await? {
        Made::Friend(f) => Accepted::Friends(f),
        Made::Gone => Accepted::NotFound,
        Made::Full(full) => Accepted::Full(full),
    })
}

/// The caller declines `other`'s request (any account, listed or not). Whether there was one.
pub async fn decline_in(
    conn: &mut PgConnection,
    caller: &Caller,
    other: Uuid,
) -> Result<bool, IdentityError> {
    lock_in(conn, caller.account_id, other).await?;
    let done = sqlx::query(
        "DELETE FROM account_friend_request WHERE from_account = $1 AND to_account = $2",
    )
    .bind(other)
    .bind(caller.account_id)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Whatever links the caller and `other` goes (any account, listed or not): the friendship (both
/// rows) and every request either way. Whether anything was there.
pub async fn remove_in(
    conn: &mut PgConnection,
    caller: &Caller,
    other: Uuid,
) -> Result<bool, IdentityError> {
    let me = caller.account_id;
    lock_in(conn, me, other).await?;
    let friendship = sqlx::query(
        "DELETE FROM account_friendship \
         WHERE (account_id = $1 AND friend_id = $2) OR (account_id = $2 AND friend_id = $1)",
    )
    .bind(me)
    .bind(other)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let requests = delete_requests_in(conn, me, other).await?;
    Ok(friendship + requests > 0)
}

/// Sets the caller's favorite mark on the listed friend `other`; `None` when it is not one
/// (nothing written). The other side never changes. Takes [`lock_in`], as every write here.
pub async fn set_favorite_in(
    conn: &mut PgConnection,
    caller: &Caller,
    other: Uuid,
    favorite: bool,
) -> Result<Option<FriendRow>, IdentityError> {
    lock_in(conn, caller.account_id, other).await?;
    let Some(mut row) = friend_in(conn, caller.account_id, other).await? else {
        return Ok(None);
    };
    if row.favorite != favorite {
        sqlx::query(
            "UPDATE account_friendship SET favorite = $3 WHERE account_id = $1 AND friend_id = $2",
        )
        .bind(caller.account_id)
        .bind(other)
        .bind(favorite)
        .execute(&mut *conn)
        .await?;
        row.favorite = favorite;
    }
    Ok(Some(row))
}

// ---- Presence: whose profile the caller looks from (CORE_API.md §4.1-§4.2, OD-56) ----------------

/// A live profile of the caller's own account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnProfile {
    /// Profile.
    pub profile_id: Uuid,
    /// XUID.
    pub xuid: i64,
    /// Game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
}

fn own_profile(r: (Uuid, i64, String, String)) -> OwnProfile {
    OwnProfile {
        profile_id: r.0,
        xuid: r.1,
        game: r.2,
        mode: r.3,
    }
}

/// The live profile `xuid` of `account_id` (`?asXuid=`); `None` for anything else.
pub async fn own_profile_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
) -> Result<Option<OwnProfile>, IdentityError> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(None);
    };
    let row: Option<(Uuid, i64, String, String)> = sqlx::query_as(
        "SELECT id, xuid, game, mode FROM profile WHERE xuid = $1 AND account_id = $2 \
           AND status = 'active' AND name IS NOT NULL ORDER BY created_at, id LIMIT 1",
    )
    .bind(xuid)
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(own_profile))
}

/// The profile of `account_id`'s active play session, the one it plays now (never a dedicated
/// lobby's host session, OD-109).
pub async fn playing_profile_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<OwnProfile>, IdentityError> {
    let row: Option<(Uuid, i64, String, String)> = sqlx::query_as(
        "SELECT p.id, p.xuid, p.game, p.mode FROM play_session s JOIN profile p ON p.id = s.profile_id \
         WHERE s.account_id = $1 AND s.state = 'active' AND s.expires_at > clock_timestamp() AND NOT s.dedicated \
           AND p.status = 'active' AND p.name IS NOT NULL \
         ORDER BY s.created_at DESC LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(own_profile))
}

/// For each of `worlds`, whether a join of `me` (a profile of the Supporter account
/// `account_id`) would be granted now, the code aside: matchmaking's own rule
/// (`fi_db::matchmaking::assess`: same mode, the audience lets it in, an invite included, the host
/// does not bar it, a seat is open; unlocated, so no latency refusal). One statement.
pub async fn joinable_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    me: &OwnProfile,
    worlds: &[Uuid],
) -> Result<HashMap<Uuid, bool>, IdentityError> {
    let viewer = crate::matchmaking::Viewer {
        account_id,
        profile_id: me.profile_id,
        xuid: me.xuid,
        ip: None,
    };
    let rules = crate::matchmaking::Rules {
        profile_id: me.profile_id,
        vip: true,
        mode: Some(me.mode.as_str()),
        cell: None,
    };
    Ok(crate::matchmaking::worlds_by_ids_in(conn, &viewer, worlds)
        .await?
        .iter()
        .map(|w| {
            let ok = w.open
                && w.game == me.game
                && crate::matchmaking::assess(w, &rules, false).why.is_none();
            (w.id, ok)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lapse_and_the_limits_are_the_documented_ones() {
        assert!(LIVE.contains(&format!("'{LAPSE_DAYS} days'")));
        assert_eq!((REQUESTS.max, REQUESTS.window_s), (20, 3_600));
        assert_eq!((MAX_FRIENDS, MAX_OUTGOING), (200, 100));
    }

    #[test]
    fn lock_keys_are_stable_and_spread() {
        let id = Uuid::from_u128(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
        assert_eq!(lock_key(id), lock_key(id));
        assert_ne!(lock_key(id), lock_key(Uuid::from_u128(1)));
    }

    #[test]
    fn the_presence_joins_go_before_the_friend_rows_filter() {
        let sql = friends_sql("AND f.friend_id = $2");
        let (head, filter) = sql.rsplit_once(" WHERE ").unwrap();
        assert!(head.contains("LEFT JOIN LATERAL") && head.contains(") inv ON true"));
        assert!(
            filter.starts_with("f.account_id = $1 AND a.status = 'active'"),
            "{filter}"
        );
        assert!(filter.ends_with("AND f.friend_id = $2"));
    }

    #[test]
    fn the_account_rules_are_the_visibility_rules() {
        // The account half of the one visibility rule, word for word.
        assert!(VISIBLE.ends_with(ACCOUNT_VISIBLE));
        assert!(FRIEND_ROWS.ends_with(ACCOUNT_VISIBLE));
        // OD-129: any visible account may be asked or accepted, whatever its tier.
        assert_eq!(OPEN, ACCOUNT_VISIBLE);
    }
}

// ---- Friends coming online (CORE_API.md §4.2a, OD-211, CR-S102) ---------------------------

/// The most friends one inbox poll names as come online.
pub const ONLINE_SHOWN: i64 = 20;

/// A friend whose account started an admitted play session since the caller's previous poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameOnline {
    /// The friend's handle ([`FriendRow::handle`]).
    pub handle: u64,
    /// The plain name of the profile of that session.
    pub gamertag: String,
    /// Its game.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// When the session was admitted.
    pub since: UnixMs,
}

/// Records this poll of `account_id`'s launcher sign-in `family_id` (its device binding: one row
/// per account and device, `friend_online_poll`, migration
/// `20261108120000_friend_online_poll.sql`) and answers the listed friends ([`FRIEND_ROWS`]) whose
/// account had a play session admitted after the device's previous poll and up to this one, still
/// active, oldest first, at most [`ONLINE_SHOWN`], one per friend. Nothing when the device has no
/// previous poll inside the last hour (the first poll after a launcher start or a sign-in), so a
/// restart does not replay everyone already online. A session is not named when the friend had
/// another (non-dedicated) session that was live within the 2 minutes before it was admitted (a
/// friend who left and came back, or was already online in another game). Two statements on the
/// caller's connection: the poll's upsert (which also drops the account's other devices' rows
/// older than an hour) and the read.
pub async fn came_online_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    family_id: Uuid,
) -> Result<Vec<CameOnline>, IdentityError> {
    let (prev, now): (Option<OffsetDateTime>, Option<OffsetDateTime>) = sqlx::query_as(
        "WITH t AS (SELECT clock_timestamp() AS now), \
           dev AS (SELECT r.device_binding_id AS id FROM refresh_token r \
             WHERE r.family_id = $2 AND r.account_id = $1 AND r.device_binding_id IS NOT NULL LIMIT 1), \
           prev AS (SELECT o.polled_at FROM friend_online_poll o, dev, t \
             WHERE o.account_id = $1 AND o.device_binding_id = dev.id AND o.polled_at > t.now - interval '1 hour'), \
           gc AS (DELETE FROM friend_online_poll o USING t WHERE o.account_id = $1 \
             AND o.polled_at <= t.now - interval '1 hour' AND o.device_binding_id NOT IN (SELECT id FROM dev)), \
           up AS (INSERT INTO friend_online_poll (account_id, device_binding_id, polled_at) \
             SELECT $1, dev.id, t.now FROM dev, t \
             ON CONFLICT (account_id, device_binding_id) DO UPDATE SET polled_at = EXCLUDED.polled_at \
             RETURNING polled_at) \
         SELECT (SELECT polled_at FROM prev), (SELECT polled_at FROM up)",
    )
    .bind(account_id)
    .bind(family_id)
    .fetch_one(&mut *conn)
    .await?;
    let (Some(prev), Some(now)) = (prev, now) else {
        return Ok(Vec::new());
    };
    let (from, filter_rows) = FRIEND_ROWS
        .rsplit_once(" WHERE ")
        .expect("FRIEND_ROWS has a WHERE");
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT handle, name, game, mode, admitted_at FROM ( \
           SELECT DISTINCT ON (a.id) a.id, h.xuid AS handle, p.name, p.game, p.mode, s.admitted_at \
           {from} JOIN play_session s ON s.account_id = a.id JOIN profile p ON p.id = s.profile_id \
           WHERE {filter_rows} AND s.state = 'active' AND s.expires_at > clock_timestamp() AND NOT s.dedicated \
             AND s.admitted_at > $2 AND s.admitted_at <= $3 \
             AND p.status = 'active' AND p.name IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM play_session e WHERE e.account_id = a.id AND e.id <> s.id \
               AND NOT e.dedicated AND e.admitted_at < s.admitted_at \
               AND coalesce(e.ended_at, e.expires_at) >= s.admitted_at - interval '2 minutes') \
           ORDER BY a.id, s.admitted_at) x \
         ORDER BY admitted_at, id LIMIT $4"
    )))
    .bind(account_id)
    .bind(prev)
    .bind(now)
    .bind(ONLINE_SHOWN)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(CameOnline {
                handle: r.try_get::<i64, _>("handle")? as u64,
                gamertag: r.try_get("name")?,
                game: r.try_get("game")?,
                mode: r.try_get("mode")?,
                since: ms(r.try_get::<OffsetDateTime, _>("admitted_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(IdentityError::from)
}
