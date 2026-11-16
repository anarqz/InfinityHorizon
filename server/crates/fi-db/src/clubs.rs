//! Car Clubs (SER-M3 step 8; GAME_PLATFORM.md §3a "Car Clubs", register CR-S70; OD-129, OD-130):
//! `game_club`, `game_club_member`, `game_club_invite`, `game_club_request`, `game_club_notice`,
//! `game_club_history` and `game_club_setting` (migration `20261028200000_car_clubs.sql`), ported
//! from the Beta's `deprecated/beta/server/src/db/clubs.rs`, keyed by game, mode and profile.
//!
//! **One mode.** A club belongs to the mode of the profile that created it (`game_club.mode`):
//! every read here takes the viewer's game and mode and sees only that mode's clubs, and of their
//! members only the visible ones ([`VISIBLE`], the member's profile still of the club's mode). A
//! profile is in at most one club (the member row is keyed by the profile).
//!
//! **One writer.** Every write of a game and mode's clubs runs under [`lock_in`], a
//! transaction-scoped advisory lock: the Beta relied on SQLite's single writer for "one club per
//! player", the tag's uniqueness, leader succession and "the last member out deletes the club";
//! here those are decided one write at a time (the unique indexes hold them anyway). Club writes
//! are human actions (create, join, leave, invite), so serialising them per mode costs nothing.
//!
//! **Rules** (the Beta's): roles [`ROLE_LEADER`], [`ROLE_COLEADER`], [`ROLE_MEMBER`]; privacy
//! [`PRIVACY_OPEN`], [`PRIVACY_FRIENDS`], [`PRIVACY_INVITE_ONLY`], [`PRIVACY_PRIVATE`]; a leader
//! who leaves hands over to the oldest co-leader, else the oldest member (visible members first);
//! a club with no member left is deleted with its invites, requests and notices (history stays);
//! a new leader turns the old one into a co-leader. A private club shows only to its members and
//! their friends.
//!
//! **Bounds.** [`MAX_MEMBERS`] members a club, [`MAX_INVITES`] pending invites and
//! [`MAX_REQUESTS`] pending requests a club, the newest [`MAX_NOTICES`] notices a club, the newest
//! [`MAX_HISTORY`] history entries a profile, at most [`MAX_LIST`] clubs a list.
//!
//! The statements composed with `format!` (`sqlx::AssertSqlSafe`) join constants of this crate
//! only ([`VISIBLE`]); every value is a bind parameter.

use crate::gameplane::{Card, Player};
use crate::identity::{IdentityError, ms};
use crate::visibility::{VISIBLE, Viewer};
use fi_domain::UnixMs;
use serde_json::Value as Json;
use sqlx::postgres::PgRow;
use sqlx::types::Json as Jsonb;
use sqlx::{PgConnection, Row};
use std::sync::OnceLock;
use uuid::Uuid;

/// `roleInClub` of the club's leader (0 is "not in a club").
pub const ROLE_LEADER: i16 = 1;
/// A co-leader (an officer).
pub const ROLE_COLEADER: i16 = 2;
/// A member.
pub const ROLE_MEMBER: i16 = 3;

/// Anyone joins.
pub const PRIVACY_OPEN: i16 = 1;
/// A player with a friend in the club joins; anyone else asks.
pub const PRIVACY_FRIENDS: i16 = 2;
/// Invited players join; anyone else asks.
pub const PRIVACY_INVITE_ONLY: i16 = 3;
/// Shown only to its members and their friends; everyone asks.
pub const PRIVACY_PRIVATE: i16 = 4;

/// `ClubHistoryEntryData.action`: joined (the Beta's numbering; the game only shows it).
pub const ACTION_JOINED: i16 = 0;
/// Left.
pub const ACTION_LEFT: i16 = 1;
/// Removed (kicked, or the club deleted around it).
pub const ACTION_REMOVED: i16 = 2;
/// Its role changed.
pub const ACTION_ROLE: i16 = 3;
/// Created the club.
pub const ACTION_CREATED: i16 = 4;

/// Members a club holds at most.
pub const MAX_MEMBERS: i64 = 1_000;
/// Pending invites a club holds at most.
pub const MAX_INVITES: i64 = 200;
/// Pending requests to join a club holds at most.
pub const MAX_REQUESTS: i64 = 200;
/// Notices a club keeps (the newest; the Beta showed 50).
pub const MAX_NOTICES: i64 = 50;
/// History entries a profile keeps (the newest).
pub const MAX_HISTORY: i64 = 100;
/// Clubs one list answers at most.
pub const MAX_LIST: i64 = 200;

/// `ClubMembershipRequest` (0x0700, ClubMembershipRequestParameters): to the officers. Tab 7.
pub const NOTE_REQUEST: i32 = 0x0700;
/// `ClubInvitation` (0x0701).
pub const NOTE_INVITATION: i32 = 0x0701;
/// `ClubMemberRemoveRecommendation` (0x0702): to the officers.
pub const NOTE_REMOVE_RECOMMENDATION: i32 = 0x0702;
/// `ClubKick` (0x0703): to the removed player.
pub const NOTE_KICK: i32 = 0x0703;
/// `ClubDemotion` (0x0704).
pub const NOTE_DEMOTION: i32 = 0x0704;
/// `ClubMemberAccepted` (0x0705): to the player who joined.
pub const NOTE_ACCEPTED: i32 = 0x0705;
/// `ClubMemberQuit` (0x0706): to the other members.
pub const NOTE_QUIT: i32 = 0x0706;
/// `ClubNewMember` (0x0707): to the other members.
pub const NOTE_NEW_MEMBER: i32 = 0x0707;
/// `ClubNewPromotion` (0x0708): to every member.
pub const NOTE_PROMOTION: i32 = 0x0708;
/// `ClubNoticeBoardMessage` (0x0709): to the other members.
pub const NOTE_NOTICE: i32 = 0x0709;

/// The parameter struct of a club notification type.
pub fn note_struct(kind: i32) -> &'static str {
    match kind {
        NOTE_REQUEST => "ClubMembershipRequestParameters",
        NOTE_INVITATION => "ClubInvitationParameters",
        NOTE_REMOVE_RECOMMENDATION => "ClubMemberRemoveRecommendationParameters",
        NOTE_KICK => "ClubKickParameters",
        NOTE_DEMOTION => "ClubDemotionParameters",
        NOTE_ACCEPTED => "ClubMemberAcceptedParameters",
        NOTE_QUIT => "ClubMemberQuitParameters",
        NOTE_NEW_MEMBER => "ClubNewMemberParameters",
        NOTE_PROMOTION => "ClubNewPromotionParameters",
        _ => "ClubNoticeBoardMessageParameters",
    }
}

/// Creating or editing a club (the Beta's 10 club changes an hour).
pub const CLUB_CHANGES: crate::limits::Limit = crate::limits::Limit {
    name: "club.change",
    max: 10,
    window_s: 3600,
};
/// Notices one player posts (the Beta's 20 an hour).
pub const CLUB_NOTICES: crate::limits::Limit = crate::limits::Limit {
    name: "club.notice",
    max: 20,
    window_s: 3600,
};
/// Removal recommendations one player sends (the Beta's 5 an hour).
pub const CLUB_REPORTS: crate::limits::Limit = crate::limits::Limit {
    name: "club.report",
    max: 5,
    window_s: 3600,
};
/// Invites and requests to join one player sends (each notifies someone).
pub const CLUB_ASKS: crate::limits::Limit = crate::limits::Limit {
    name: "club.ask",
    max: 60,
    window_s: 3600,
};

/// A member as the replies show it: its profile, player card and role.
#[derive(Clone, Debug, PartialEq)]
pub struct ClubPlayer {
    /// The member's profile.
    pub profile_id: Uuid,
    /// Its XUID, name and card.
    pub player: Player,
    /// Its role.
    pub role: i16,
}

/// A club as a reader of its mode sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Club {
    /// The wire's clubId.
    pub id: i32,
    /// The tag, in capitals.
    pub tag: String,
    /// The name.
    pub name: String,
    /// The description.
    pub description: String,
    /// The privacy (1 to 4).
    pub privacy: i16,
    /// Keyword ids (0 none).
    pub region: i16,
    /// Attitude keyword.
    pub attitude: i16,
    /// Interest keyword.
    pub interest: i16,
    /// When it was created.
    pub created_at: UnixMs,
    /// Visible members.
    pub members: i64,
    /// What its visible members earned while in it.
    pub xp: f64,
    /// Its place among the mode's clubs by XP (1 = most; ties share it).
    pub xp_rank: i64,
    /// The leader, when visible.
    pub leader: Option<ClubPlayer>,
    /// The visible member with the highest career level (the Beta's stand-in for a champion).
    pub champion: Option<ClubPlayer>,
    /// The longest-standing member among the viewer's friends.
    pub friend: Option<ClubPlayer>,
}

/// The fields a club is created or edited with (already cleaned and bounded by the caller).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClubFields {
    /// The name (1 to 24 characters).
    pub name: String,
    /// The description (at most 200 characters).
    pub description: String,
    /// The privacy (1 to 4).
    pub privacy: i16,
    /// Region keyword (0 to 255).
    pub region: i16,
    /// Attitude keyword.
    pub attitude: i16,
    /// Interest keyword.
    pub interest: i16,
}

/// The order of a club list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Most members first, then most XP (the Beta's).
    Members,
    /// Most XP first (the club XP board).
    Xp,
}

/// A club list (lookups, searches and boards), within the viewer's game and mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClubQuery<'a> {
    /// Only these clubs.
    pub ids: Option<&'a [i32]>,
    /// Only the club of this tag (capitals).
    pub tag: Option<&'a str>,
    /// Only clubs whose name contains this (any case) or whose tag is this.
    pub name: Option<&'a str>,
    /// Only clubs of this privacy.
    pub privacy: Option<i16>,
    /// Region keyword (0 any).
    pub region: i16,
    /// Attitude keyword (0 any).
    pub attitude: i16,
    /// Interest keyword (0 any).
    pub interest: i16,
    /// Leave private clubs out (lists and searches; a private club is still found by id or tag
    /// by its members and their friends).
    pub public_only: bool,
    /// Leave this club out.
    pub exclude: Option<i32>,
    /// The order.
    pub order: Order,
    /// At most this many (clamped to [`MAX_LIST`]).
    pub limit: i64,
}

impl Default for ClubQuery<'_> {
    fn default() -> Self {
        ClubQuery {
            ids: None,
            tag: None,
            name: None,
            privacy: None,
            region: 0,
            attitude: 0,
            interest: 0,
            public_only: false,
            exclude: None,
            order: Order::Members,
            limit: MAX_LIST,
        }
    }
}

/// A profile's own club, as stored (not filtered by visibility).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Own {
    /// The club.
    pub club_id: i32,
    /// Its tag.
    pub tag: String,
    /// Its name.
    pub name: String,
    /// Its privacy.
    pub privacy: i16,
    /// The profile's role.
    pub role: i16,
}

/// What UserData shows of a profile's club: its club (of the asked mode) and whether it takes
/// invites.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Badge {
    /// `(clubId, clubTag, roleInClub)`; `None` outside a club.
    pub club: Option<(i32, String, i16)>,
    /// `acceptsClubInvites`.
    pub accepts_invites: bool,
}

/// One member of a club's list.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberRow {
    /// The member.
    pub member: ClubPlayer,
    /// When it joined.
    pub joined_at: UnixMs,
    /// [`Ranking::Weekly`]: what it earned in the club this week ([`WEEK_START_SQL`]);
    /// [`Ranking::Role`]: what it earned while in the club.
    pub xp: f64,
}

/// How [`members_in`] orders a club's members.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ranking {
    /// Leader first, then by role and joining (member lists).
    Role,
    /// The ladder (FH2-S18): by the XP accepted this week while in the club, from the XP ledger
    /// (`game_xp`), so the tiers start over every week.
    Weekly,
}

/// The start of the club ladder's week: Monday 00:00 UTC (ISO week, Postgres `date_trunc`),
/// whatever the session's TimeZone, on the database clock. The game's own reset time is unknown;
/// Monday 00:00 UTC is a guess, as in other projects used as reference.
///
/// `now()` (the transaction's start), not `clock_timestamp()`: the expression must be stable for
/// the planner to use it as an index condition, so `x.at >= GREATEST(m.joined_at, week)` range-
/// scans `game_xp_recent` (game, profile_id, at) instead of reading a member's whole XP history.
pub const WEEK_START_SQL: &str =
    "(date_trunc('week', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC')";

/// One history entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    /// The club's name as it was.
    pub club_name: String,
    /// The role.
    pub role: i16,
    /// The action.
    pub action: i16,
    /// When.
    pub at: UnixMs,
}

/// One notice of the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The wire's notificationId.
    pub id: i64,
    /// Its author.
    pub profile_id: Uuid,
    /// The text.
    pub message: String,
}

/// Takes the club writes' lock of a game and mode until the caller's transaction ends.
pub async fn lock_in(conn: &mut PgConnection, game: &str, mode: &str) -> Result<(), IdentityError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('fi.game_club'), hashtext($1 || ':' || $2))",
    )
    .bind(game)
    .bind(mode)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The visible members of the mode's clubs (`$1` game, `$2` mode), one row each with its card.
fn visible_members() -> String {
    format!(
        "vm AS MATERIALIZED ( \
           SELECT m.club_id, m.profile_id, m.role, m.joined_at, m.xp, p.xuid, p.name, \
             l.career_level, l.badge_id, l.player_title_id \
           FROM game_club c \
           JOIN game_club_member m ON m.game = c.game AND m.club_id = c.id \
           JOIN profile p ON p.id = m.profile_id \
           JOIN account a ON a.id = p.account_id \
           LEFT JOIN game_logon l ON l.game = m.game AND l.profile_id = m.profile_id \
           WHERE c.game = $1 AND c.mode = $2 AND p.mode = c.mode AND {VISIBLE} \
         )"
    )
}

/// The member columns of a lateral `x`, prefixed `pre`.
fn member_columns(x: &str, pre: &str) -> String {
    [
        "profile_id",
        "role",
        "xuid",
        "name",
        "career_level",
        "badge_id",
        "player_title_id",
    ]
    .iter()
    .map(|c| format!("{x}.{c} AS {pre}{c}"))
    .collect::<Vec<_>>()
    .join(", ")
}

fn card(r: &PgRow, pre: &str) -> Result<Card, sqlx::Error> {
    let level: Option<i64> = r.try_get(format!("{pre}career_level").as_str())?;
    let badge: Option<i32> = r.try_get(format!("{pre}badge_id").as_str())?;
    let title: Option<i32> = r.try_get(format!("{pre}player_title_id").as_str())?;
    Ok(Card {
        career_level: level.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        badge_id: badge.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
        player_title_id: title.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
    })
}

fn club_player(r: &PgRow, pre: &str) -> Result<Option<ClubPlayer>, sqlx::Error> {
    let Some(profile_id) = r.try_get::<Option<Uuid>, _>(format!("{pre}profile_id").as_str())?
    else {
        return Ok(None);
    };
    Ok(Some(ClubPlayer {
        profile_id,
        player: Player {
            xuid: r.try_get(format!("{pre}xuid").as_str())?,
            name: r.try_get(format!("{pre}name").as_str())?,
            card: card(r, pre)?,
            tables_tran_id: None,
        },
        role: r.try_get(format!("{pre}role").as_str())?,
    }))
}

static LIST_SQL: OnceLock<String> = OnceLock::new();

fn list_sql() -> &'static str {
    LIST_SQL.get_or_init(|| {
        format!(
            "WITH {vm}, \
             totals AS ( \
               SELECT club_id, count(*) AS members, sum(xp)::double precision AS xp, \
                 rank() OVER (ORDER BY sum(xp) DESC) AS xp_rank \
               FROM vm GROUP BY club_id \
             ) \
             SELECT c.id, c.tag, c.name, c.description, c.privacy, c.region, c.attitude, c.interest, \
               c.created_at, t.members, t.xp, t.xp_rank, {ld}, {ch}, {fr} \
             FROM game_club c \
             JOIN totals t ON t.club_id = c.id \
             LEFT JOIN LATERAL (SELECT * FROM vm WHERE vm.club_id = c.id AND vm.role = 1 LIMIT 1) ld ON true \
             LEFT JOIN LATERAL (SELECT * FROM vm WHERE vm.club_id = c.id \
               ORDER BY vm.career_level DESC NULLS LAST, vm.joined_at, vm.profile_id LIMIT 1) ch ON true \
             LEFT JOIN LATERAL (SELECT * FROM vm WHERE vm.club_id = c.id AND vm.profile_id = ANY($3::uuid[]) \
               ORDER BY vm.joined_at, vm.profile_id LIMIT 1) fr ON true \
             WHERE c.game = $1 AND c.mode = $2 \
               AND (c.privacy <> 4 OR EXISTS (SELECT 1 FROM vm k WHERE k.club_id = c.id \
                    AND (k.profile_id = $4 OR k.profile_id = ANY($3::uuid[])))) \
               AND ($5::int[] IS NULL OR c.id = ANY($5::int[])) \
               AND ($6::text IS NULL OR c.tag = $6) \
               AND ($7::text IS NULL OR strpos(lower(c.name), lower($7)) > 0 OR c.tag = upper($7)) \
               AND ($8::smallint IS NULL OR c.privacy = $8) \
               AND ($9::smallint = 0 OR c.region = $9) \
               AND ($10::smallint = 0 OR c.attitude = $10) \
               AND ($11::smallint = 0 OR c.interest = $11) \
               AND (NOT $12 OR c.privacy <> 4) \
               AND ($13::int IS NULL OR c.id <> $13) \
             ORDER BY CASE WHEN $14 THEN t.xp ELSE t.members::double precision END DESC, t.xp DESC, c.id \
             LIMIT $15",
            vm = visible_members(),
            ld = member_columns("ld", "l_"),
            ch = member_columns("ch", "c_"),
            fr = member_columns("fr", "f_"),
        )
    })
}

/// The clubs `q` selects that the viewer may see (its game and mode; a private club only when the
/// viewer or one of `friends` is a member), with their visible member count, XP and rank, leader,
/// champion and the viewer's longest-standing friend among the members. A club whose members are
/// all hidden (banned, deleted, moved) is left out. One statement; the members of the mode's
/// clubs are read once.
pub async fn clubs_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    friends: &[Uuid],
    q: &ClubQuery<'_>,
) -> Result<Vec<Club>, IdentityError> {
    list_in(conn, viewer, friends, q, q.limit.clamp(0, MAX_LIST)).await
}

/// Every non-private club of `game` and `mode` (or only the one of `tag`, capitals) as
/// [`clubs_in`] computes them (the same statement: visible members, XP, the XP rank among all the
/// mode's clubs, private ones included, leader and champion), most XP first, without the game's
/// list cap: the play dashboard's clubs (DASHBOARD_API.md §6). Read as no one in particular, so a
/// private club never shows.
pub async fn public_clubs_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    tag: Option<&str>,
) -> Result<Vec<Club>, IdentityError> {
    let nobody = Viewer {
        game,
        mode,
        profile_id: Uuid::nil(),
        account_id: Uuid::nil(),
        xuid: 0,
        session_id: Uuid::nil(),
    };
    let q = ClubQuery {
        tag,
        public_only: true,
        order: Order::Xp,
        ..ClubQuery::default()
    };
    list_in(conn, &nobody, &[], &q, i64::MAX).await
}

async fn list_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    friends: &[Uuid],
    q: &ClubQuery<'_>,
    limit: i64,
) -> Result<Vec<Club>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(list_sql()))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(friends)
        .bind(viewer.profile_id)
        .bind(q.ids)
        .bind(q.tag)
        .bind(q.name)
        .bind(q.privacy)
        .bind(q.region)
        .bind(q.attitude)
        .bind(q.interest)
        .bind(q.public_only)
        .bind(q.exclude)
        .bind(q.order == Order::Xp)
        .bind(limit.max(0))
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(Club {
                id: r.try_get("id")?,
                tag: r.try_get("tag")?,
                name: r.try_get("name")?,
                description: r.try_get("description")?,
                privacy: r.try_get("privacy")?,
                region: r.try_get("region")?,
                attitude: r.try_get("attitude")?,
                interest: r.try_get("interest")?,
                created_at: ms(r.try_get("created_at")?),
                members: r.try_get("members")?,
                xp: r.try_get("xp")?,
                xp_rank: r.try_get("xp_rank")?,
                leader: club_player(r, "l_")?,
                champion: club_player(r, "c_")?,
                friend: club_player(r, "f_")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// One club by id as [`clubs_in`] shows it; `None` when the viewer may not see it.
pub async fn club_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    friends: &[Uuid],
    id: i32,
) -> Result<Option<Club>, IdentityError> {
    let ids = [id];
    let q = ClubQuery {
        ids: Some(&ids),
        limit: 1,
        ..ClubQuery::default()
    };
    Ok(clubs_in(conn, viewer, friends, &q).await?.pop())
}

/// The viewer's game and mode club of `tag` (any case) as [`clubs_in`] shows it.
pub async fn club_by_tag_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    friends: &[Uuid],
    tag: &str,
) -> Result<Option<Club>, IdentityError> {
    let tag = tag.trim().to_ascii_uppercase();
    let q = ClubQuery {
        tag: Some(&tag),
        limit: 1,
        ..ClubQuery::default()
    };
    Ok(clubs_in(conn, viewer, friends, &q).await?.pop())
}

/// The id of the club of `tag` (any case) in the game and mode, whatever its visibility (a club
/// that invited the caller is joinable by tag even when private).
pub async fn id_by_tag_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    tag: &str,
) -> Result<Option<i32>, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT id FROM game_club WHERE game = $1 AND mode = $2 AND tag = $3")
            .bind(game)
            .bind(mode)
            .bind(tag.trim().to_ascii_uppercase())
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// The club's privacy and name when it is a club of the game and mode, whatever its visibility.
pub async fn stored_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    id: i32,
) -> Result<Option<(i16, String, String)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT privacy, name, tag FROM game_club WHERE game = $1 AND mode = $2 AND id = $3",
    )
    .bind(game)
    .bind(mode)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The visible members of a club of the viewer's game and mode, with the total: in role order
/// (leader first, then by joining; [`Ranking::Role`]), or by the XP earned in the club this week
/// ([`Ranking::Weekly`], the ladder: accepted `game_xp` reports of the club's mode since
/// [`WEEK_START_SQL`] and since joining). At most `limit` ([`MAX_MEMBERS`]). One statement.
pub async fn members_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    club: i32,
    ranking: Ranking,
    limit: i64,
) -> Result<(i64, Vec<MemberRow>), IdentityError> {
    let by_xp = ranking == Ranking::Weekly;
    // Weekly: the ledger's rows of the member's profile in the club's mode since the later of the
    // week's start and its joining (index `game_xp_recent`); computed only for the ladder.
    let sql = format!(
        "SELECT * FROM ( \
           SELECT m.profile_id, m.role, m.joined_at, \
             CASE WHEN $4 THEN COALESCE((SELECT sum(x.xp) FROM game_xp x \
               WHERE x.game = m.game AND x.profile_id = m.profile_id AND x.mode = c.mode \
                 AND x.at >= GREATEST(m.joined_at, {WEEK_START_SQL})), 0)::double precision \
             ELSE m.xp END AS xp, \
             p.xuid, p.name, l.career_level, l.badge_id, l.player_title_id, count(*) OVER () AS total \
           FROM game_club c \
           JOIN game_club_member m ON m.game = c.game AND m.club_id = c.id \
           JOIN profile p ON p.id = m.profile_id \
           JOIN account a ON a.id = p.account_id \
           LEFT JOIN game_logon l ON l.game = m.game AND l.profile_id = m.profile_id \
           WHERE c.game = $1 AND c.mode = $2 AND c.id = $3 AND p.mode = c.mode AND {VISIBLE} \
         ) t \
         ORDER BY CASE WHEN $4 THEN t.xp ELSE 0 END DESC, t.role, t.joined_at, t.profile_id \
         LIMIT $5"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(club)
        .bind(by_xp)
        .bind(limit.clamp(1, MAX_MEMBERS))
        .fetch_all(&mut *conn)
        .await?;
    let mut total = 0;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        total = r.try_get("total")?;
        out.push(MemberRow {
            member: club_player(r, "")?.ok_or(IdentityError::NotFound)?,
            joined_at: ms(r.try_get("joined_at")?),
            xp: r.try_get("xp")?,
        });
    }
    if limit < 1 {
        out.clear();
    }
    Ok((total, out))
}

/// Every club of `game` that is not private now, as `(mode, tag)` by mode then tag: the play
/// dashboard's live allowlist of clubs (DASHBOARD_API.md §5.2). One statement.
pub async fn public_club_tags_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Vec<(String, String)>, IdentityError> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT mode, tag FROM game_club WHERE game = $1 AND privacy <> {PRIVACY_PRIVATE} ORDER BY mode, tag"
    )))
    .bind(game)
    .fetch_all(&mut *conn)
    .await?)
}

/// A visible member of one of a mode's clubs, as the play dashboard shows it (DASHBOARD_API.md §6).
#[derive(Clone, Debug, PartialEq)]
pub struct ModeMember {
    /// The club.
    pub club_id: i32,
    /// The member's profile (server-side only).
    pub profile_id: Uuid,
    /// Its role.
    pub role: i16,
    /// When it joined.
    pub joined_at: UnixMs,
    /// What it earned while in the club.
    pub club_xp: f64,
    /// The plain name.
    pub name: String,
    /// The linked Discord account's global name, else its username.
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
    /// Listed on the website now (`crate::dashboard::players::listed`).
    pub listed: bool,
    /// Its XP total ([`crate::gameplane::xp_total_sql`]).
    pub xp: Option<i64>,
}

/// The visible members of every club of `game` and `mode` (or of club `club` only), from the same
/// member set the clubs' totals count ([`clubs_in`]'s `vm`): by club, then role, then XP earned in
/// the club, then joining. One statement.
pub async fn mode_members_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    club: Option<i32>,
) -> Result<Vec<ModeMember>, IdentityError> {
    let sql = format!(
        "WITH {vm} \
         SELECT vm.club_id, vm.profile_id, vm.role, vm.joined_at, vm.xp AS club_xp, vm.name, \
           CASE WHEN a.discord_user_id IS NOT NULL THEN coalesce(a.discord_global_name, a.discord_username) END \
             AS discord_name, \
           (a.discord_avatar IS NOT NULL) AS avatar, {listed} AS listed, \
           NULLIF(LEAST(trunc(COALESCE({xp}, 0)), 9223372036854775807), 0)::bigint AS xp_total \
         FROM vm \
         JOIN profile p ON p.id = vm.profile_id \
         JOIN account a ON a.id = p.account_id \
         LEFT JOIN game_logon l ON l.game = $1 AND l.profile_id = vm.profile_id \
         LEFT JOIN game_profile gp ON gp.game = $1 AND gp.profile_id = vm.profile_id \
         WHERE ($3::int IS NULL OR vm.club_id = $3) \
         ORDER BY vm.club_id, vm.role, vm.xp DESC, vm.joined_at, vm.profile_id",
        vm = visible_members(),
        listed = crate::dashboard::players::listed(),
        xp = crate::gameplane::xp_total_sql("gp", "l"),
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(mode)
        .bind(club)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(ModeMember {
                club_id: r.try_get("club_id")?,
                profile_id: r.try_get("profile_id")?,
                role: r.try_get("role")?,
                joined_at: ms(r.try_get("joined_at")?),
                club_xp: r.try_get("club_xp")?,
                name: r.try_get::<Option<String>, _>("name")?.unwrap_or_default(),
                discord_name: r.try_get("discord_name")?,
                avatar: r.try_get("avatar")?,
                listed: r.try_get("listed")?,
                xp: r.try_get::<Option<i64>, _>("xp_total")?.filter(|x| *x > 0),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// The visible members' profiles of a club of the viewer's mode (the boards' club circle), at
/// most [`MAX_MEMBERS`].
pub async fn member_ids_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    club: i32,
) -> Result<Vec<Uuid>, IdentityError> {
    Ok(members_in(conn, viewer, club, Ranking::Role, MAX_MEMBERS)
        .await?
        .1
        .into_iter()
        .map(|m| m.member.profile_id)
        .collect())
}

/// A profile's own club of the game and mode, as stored.
pub async fn own_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    profile: Uuid,
) -> Result<Option<Own>, IdentityError> {
    let row = sqlx::query(
        "SELECT m.club_id, c.tag, c.name, c.privacy, m.role FROM game_club_member m \
         JOIN game_club c ON c.game = m.game AND c.id = m.club_id \
         WHERE m.game = $1 AND m.profile_id = $2 AND c.mode = $3",
    )
    .bind(game)
    .bind(profile)
    .bind(mode)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(Own {
        club_id: r.try_get("club_id")?,
        tag: r.try_get("tag")?,
        name: r.try_get("name")?,
        privacy: r.try_get("privacy")?,
        role: r.try_get("role")?,
    }))
}

/// UserData's club fields of each profile among `profiles` (its club of `mode`, whether it takes
/// invites), in one statement; a profile not found is left out.
pub async fn badges_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    profiles: &[Uuid],
) -> Result<Vec<(Uuid, Badge)>, IdentityError> {
    if profiles.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT u.id, c.id AS club_id, c.tag, m.role, COALESCE(s.accepts_invites, true) AS accepts \
         FROM unnest($3::uuid[]) AS u(id) \
         LEFT JOIN game_club_member m ON m.game = $1 AND m.profile_id = u.id \
         LEFT JOIN game_club c ON c.game = m.game AND c.id = m.club_id AND c.mode = $2 \
         LEFT JOIN game_club_setting s ON s.game = $1 AND s.profile_id = u.id",
    )
    .bind(game)
    .bind(mode)
    .bind(profiles)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            let club: Option<i32> = r.try_get("club_id")?;
            let badge = Badge {
                club: match club {
                    Some(id) => Some((id, r.try_get("tag")?, r.try_get("role")?)),
                    None => None,
                },
                accepts_invites: r.try_get("accepts")?,
            };
            Ok((r.try_get("id")?, badge))
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Each of `profiles` of the game as the replies show it (XUID, name, card) and whether it ever
/// logged on (a `game_logon` row), in one statement; an unknown profile is left out.
pub async fn players_in(
    conn: &mut PgConnection,
    game: &str,
    profiles: &[Uuid],
) -> Result<Vec<(Uuid, Player, bool)>, IdentityError> {
    if profiles.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT p.id AS profile_id, 0::smallint AS role, p.xuid, p.name, l.career_level, l.badge_id, \
           l.player_title_id, l.profile_id IS NOT NULL AS logged \
         FROM profile p LEFT JOIN game_logon l ON l.game = p.game AND l.profile_id = p.id \
         WHERE p.game = $1 AND p.id = ANY($2::uuid[])",
    )
    .bind(game)
    .bind(profiles)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        if let Some(p) = club_player(r, "")? {
            out.push((p.profile_id, p.player, r.try_get("logged")?));
        }
    }
    Ok(out)
}

/// The profile of `xuid` visible in the viewer's game and mode (any profile but erased ones), or
/// `None`.
pub async fn profile_by_xuid_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    xuid: i64,
) -> Result<Option<Uuid>, IdentityError> {
    let sql = format!(
        "SELECT p.id FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.xuid = $1 AND p.game = $2 AND p.mode = $3 AND {VISIBLE}"
    );
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(xuid)
        .bind(viewer.game)
        .bind(viewer.mode)
        .fetch_optional(&mut *conn)
        .await?)
}

/// The profile named `name` (any case) visible in the viewer's game and mode, or `None`.
pub async fn profile_by_name_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    name: &str,
) -> Result<Option<(Uuid, i64)>, IdentityError> {
    let sql = format!(
        "SELECT p.id, p.xuid FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.game = $1 AND lower(p.name) = lower($2) AND p.mode = $3 AND {VISIBLE}"
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(name.trim())
        .bind(viewer.mode)
        .fetch_optional(&mut *conn)
        .await?)
}

/// What [`create_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Created {
    /// The new club's id; the creator is its leader.
    Created(i32),
    /// The creator is in a club already.
    InClub,
    /// The tag is taken in the game and mode.
    TagTaken,
}

/// Creates a club of the owner's game and mode with the owner as its leader (and a `created`
/// history entry). The caller holds [`lock_in`] and checked the fields.
pub async fn create_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    owner: Uuid,
    tag: &str,
    f: &ClubFields,
) -> Result<Created, IdentityError> {
    if any_club_in(conn, game, owner).await? {
        return Ok(Created::InClub);
    }
    if id_by_tag_in(conn, game, mode, tag).await?.is_some() {
        return Ok(Created::TagTaken);
    }
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO game_club (game, mode, tag, name, description, privacy, region, attitude, interest) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(game)
    .bind(mode)
    .bind(tag.trim().to_ascii_uppercase())
    .bind(&f.name)
    .bind(&f.description)
    .bind(f.privacy)
    .bind(f.region)
    .bind(f.attitude)
    .bind(f.interest)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO game_club_member (game, profile_id, club_id, role) VALUES ($1, $2, $3, 1)",
    )
    .bind(game)
    .bind(owner)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    clear_asks_in(conn, game, owner).await?;
    history_add_in(
        conn,
        game,
        mode,
        owner,
        &f.name,
        ROLE_LEADER,
        ACTION_CREATED,
    )
    .await?;
    Ok(Created::Created(id))
}

/// Whether the profile has a member row at all (a club of another mode it was left in).
async fn any_club_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM game_club_member WHERE game = $1 AND profile_id = $2)",
    )
    .bind(game)
    .bind(profile)
    .fetch_one(&mut *conn)
    .await?)
}

/// Edits a club's name, description, privacy and keywords (the tag stays). The caller holds
/// [`lock_in`] and checked the fields.
pub async fn update_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    f: &ClubFields,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE game_club SET name = $3, description = $4, privacy = $5, region = $6, attitude = $7, \
           interest = $8 WHERE game = $1 AND id = $2",
    )
    .bind(game)
    .bind(club)
    .bind(&f.name)
    .bind(&f.description)
    .bind(f.privacy)
    .bind(f.region)
    .bind(f.attitude)
    .bind(f.interest)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The profile's pending invites and requests go once it is in a club.
async fn clear_asks_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query("DELETE FROM game_club_invite WHERE game = $1 AND profile_id = $2")
        .bind(game)
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM game_club_request WHERE game = $1 AND profile_id = $2")
        .bind(game)
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn history_add_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    profile: Uuid,
    club_name: &str,
    role: i16,
    action: i16,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO game_club_history (game, mode, profile_id, club_name, role, action) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(game)
    .bind(mode)
    .bind(profile)
    .bind(club_name)
    .bind(role)
    .bind(action)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM game_club_history WHERE game = $1 AND profile_id = $2 AND id < ( \
           SELECT id FROM game_club_history WHERE game = $1 AND profile_id = $2 \
           ORDER BY id DESC OFFSET $3 LIMIT 1)",
    )
    .bind(game)
    .bind(profile)
    .bind(MAX_HISTORY - 1)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What [`join_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Joined {
    /// A member now (its invites and requests gone, a `joined` history entry).
    Joined,
    /// Already a member of that club: nothing changed.
    Already,
    /// The club holds [`MAX_MEMBERS`].
    Full,
    /// No such club in the game and mode.
    Unknown,
}

/// Puts the profile in the club as a member, leaving any other club first (the Beta's
/// `add_member`). The caller holds [`lock_in`] and decided the profile may join.
pub async fn join_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    club: i32,
    profile: Uuid,
) -> Result<(Joined, Option<Left>), IdentityError> {
    let Some((_, name, _)) = stored_in(conn, game, mode, club).await? else {
        return Ok((Joined::Unknown, None));
    };
    if let Some(own) = own_in(conn, game, mode, profile).await?
        && own.club_id == club
    {
        return Ok((Joined::Already, None));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM game_club_member WHERE game = $1 AND club_id = $2",
    )
    .bind(game)
    .bind(club)
    .fetch_one(&mut *conn)
    .await?;
    if count >= MAX_MEMBERS {
        return Ok((Joined::Full, None));
    }
    let left = leave_in(conn, game, profile, ACTION_LEFT, true).await?;
    sqlx::query(
        "INSERT INTO game_club_member (game, profile_id, club_id, role) VALUES ($1, $2, $3, 3)",
    )
    .bind(game)
    .bind(profile)
    .bind(club)
    .execute(&mut *conn)
    .await?;
    clear_asks_in(conn, game, profile).await?;
    history_add_in(conn, game, mode, profile, &name, ROLE_MEMBER, ACTION_JOINED).await?;
    Ok((Joined::Joined, left))
}

/// A profile that left a club.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Left {
    /// The club.
    pub club_id: i32,
    /// Its tag.
    pub tag: String,
    /// Its name.
    pub name: String,
    /// The role it had.
    pub role: i16,
    /// The new leader, when the leader left and someone stayed.
    pub heir: Option<Uuid>,
    /// The club was deleted (nobody stayed).
    pub deleted: bool,
}

/// The profile leaves its club (any mode's: a profile has one), with `action` in its history when
/// `history` (left, removed). A leader hands over to the oldest co-leader, else the oldest member
/// (visible members of the club's mode first), who becomes leader; the last member out deletes
/// the club with its invites, requests and notices. `None`: not in a club. The caller holds
/// [`lock_in`].
pub async fn leave_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
    action: i16,
    history: bool,
) -> Result<Option<Left>, IdentityError> {
    let row = sqlx::query(
        "DELETE FROM game_club_member m USING game_club c \
         WHERE m.game = $1 AND m.profile_id = $2 AND c.game = m.game AND c.id = m.club_id \
         RETURNING m.club_id, m.role, c.tag, c.name, c.mode",
    )
    .bind(game)
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let club: i32 = r.try_get("club_id")?;
    let role: i16 = r.try_get("role")?;
    let name: String = r.try_get("name")?;
    let club_mode: String = r.try_get("mode")?;
    if history {
        history_add_in(conn, game, &club_mode, profile, &name, role, action).await?;
    }
    let sql = format!(
        "SELECT m.profile_id FROM game_club_member m \
         JOIN profile p ON p.id = m.profile_id JOIN account a ON a.id = p.account_id \
         WHERE m.game = $1 AND m.club_id = $2 \
         ORDER BY (p.mode = $3 AND {VISIBLE}) DESC, m.role, m.joined_at, m.profile_id LIMIT 1"
    );
    let heir: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(club)
        .bind(&club_mode)
        .fetch_optional(&mut *conn)
        .await?;
    let mut left = Left {
        club_id: club,
        tag: r.try_get("tag")?,
        name: name.clone(),
        role,
        heir: None,
        deleted: false,
    };
    match heir {
        None => {
            sqlx::query("DELETE FROM game_club WHERE game = $1 AND id = $2")
                .bind(game)
                .bind(club)
                .execute(&mut *conn)
                .await?;
            left.deleted = true;
        }
        Some(heir) if role == ROLE_LEADER => {
            sqlx::query("UPDATE game_club_member SET role = 1 WHERE game = $1 AND profile_id = $2")
                .bind(game)
                .bind(heir)
                .execute(&mut *conn)
                .await?;
            history_add_in(
                conn,
                game,
                &club_mode,
                heir,
                &name,
                ROLE_LEADER,
                ACTION_ROLE,
            )
            .await?;
            left.heir = Some(heir);
        }
        Some(_) => {}
    }
    Ok(Some(left))
}

/// Changes a member's role; a new leader turns the old one into a co-leader. Returns the role it
/// had, or `None` when it is not a member of the club (or the role is not 1 to 3). The caller
/// holds [`lock_in`].
pub async fn set_role_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    club: i32,
    profile: Uuid,
    role: i16,
) -> Result<Option<i16>, IdentityError> {
    if !(ROLE_LEADER..=ROLE_MEMBER).contains(&role) {
        return Ok(None);
    }
    let before: Option<i16> = sqlx::query_scalar(
        "SELECT role FROM game_club_member WHERE game = $1 AND profile_id = $2 AND club_id = $3",
    )
    .bind(game)
    .bind(profile)
    .bind(club)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(before) = before else {
        return Ok(None);
    };
    if role == ROLE_LEADER {
        sqlx::query(
            "UPDATE game_club_member SET role = 2 WHERE game = $1 AND club_id = $2 AND role = 1 \
               AND profile_id <> $3",
        )
        .bind(game)
        .bind(club)
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("UPDATE game_club_member SET role = $3 WHERE game = $1 AND profile_id = $2")
        .bind(game)
        .bind(profile)
        .bind(role)
        .execute(&mut *conn)
        .await?;
    if let Some((_, name, _)) = stored_in(conn, game, mode, club).await? {
        history_add_in(conn, game, mode, profile, &name, role, ACTION_ROLE).await?;
    }
    Ok(Some(before))
}

/// What [`invite_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invited {
    /// The invite's id (a repeat refreshes the same invite).
    Sent(i64),
    /// The player is in that club already.
    Member,
    /// The player takes no club invites.
    Declines,
    /// The club holds [`MAX_INVITES`] pending invites.
    TooMany,
}

/// Invites a profile to a club (one pending invite per club and player). The caller holds
/// [`lock_in`] and checked the sender may invite and the invitee is visible.
pub async fn invite_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    sender: Uuid,
    invitee: Uuid,
) -> Result<Invited, IdentityError> {
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM game_club_member WHERE game = $1 AND profile_id = $2 AND club_id = $3)",
    )
    .bind(game)
    .bind(invitee)
    .bind(club)
    .fetch_one(&mut *conn)
    .await?;
    if member {
        return Ok(Invited::Member);
    }
    if !accepts_in(conn, game, invitee).await? {
        return Ok(Invited::Declines);
    }
    let (pending, mine): (i64, bool) = sqlx::query_as(
        "SELECT count(*), bool_or(profile_id = $3) IS TRUE FROM game_club_invite WHERE game = $1 AND club_id = $2",
    )
    .bind(game)
    .bind(club)
    .bind(invitee)
    .fetch_one(&mut *conn)
    .await?;
    if pending >= MAX_INVITES && !mine {
        return Ok(Invited::TooMany);
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO game_club_invite (game, club_id, profile_id, sender_id) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (game, club_id, profile_id) DO UPDATE SET sender_id = EXCLUDED.sender_id, \
           created_at = clock_timestamp() \
         RETURNING id",
    )
    .bind(game)
    .bind(club)
    .bind(invitee)
    .bind(sender)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Invited::Sent(id))
}

/// The id of the profile's pending invite from `club` (`invitation` when it names that invite,
/// any invite from the club when it is 0 or below), or `None`.
pub async fn invite_of_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    profile: Uuid,
    invitation: i64,
) -> Result<Option<i64>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM game_club_invite WHERE game = $1 AND club_id = $2 AND profile_id = $3 \
           AND ($4 <= 0 OR id = $4)",
    )
    .bind(game)
    .bind(club)
    .bind(profile)
    .bind(invitation)
    .fetch_optional(&mut *conn)
    .await?)
}

/// What [`request_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requested {
    /// A new request (the officers are told).
    New,
    /// The profile asked already: the first request stays, nobody is told again.
    Again,
    /// The club holds [`MAX_REQUESTS`] pending requests.
    TooMany,
}

/// The profile asks to join the club. The caller holds [`lock_in`].
pub async fn request_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    profile: Uuid,
) -> Result<Requested, IdentityError> {
    let (pending, mine): (i64, bool) = sqlx::query_as(
        "SELECT count(*), bool_or(profile_id = $3) IS TRUE FROM game_club_request WHERE game = $1 AND club_id = $2",
    )
    .bind(game)
    .bind(club)
    .bind(profile)
    .fetch_one(&mut *conn)
    .await?;
    if mine {
        return Ok(Requested::Again);
    }
    if pending >= MAX_REQUESTS {
        return Ok(Requested::TooMany);
    }
    sqlx::query("INSERT INTO game_club_request (game, club_id, profile_id) VALUES ($1, $2, $3)")
        .bind(game)
        .bind(club)
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    Ok(Requested::New)
}

/// Takes the profile's request to join the club (deleted); whether there was one.
pub async fn take_request_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    profile: Uuid,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "DELETE FROM game_club_request WHERE game = $1 AND club_id = $2 AND profile_id = $3",
    )
    .bind(game)
    .bind(club)
    .bind(profile)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The notice a request already posted (`source`: the request's fact id), if any.
pub async fn notice_by_source_in(
    conn: &mut PgConnection,
    game: &str,
    source: Uuid,
) -> Result<Option<i64>, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT id FROM game_club_notice WHERE game = $1 AND source_id = $2")
            .bind(game)
            .bind(source)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// Posts a notice (cleaned and bounded by the caller) and keeps the club's newest
/// [`MAX_NOTICES`]. Its id.
pub async fn add_notice_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    author: Uuid,
    message: &str,
    source: Uuid,
) -> Result<i64, IdentityError> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO game_club_notice (game, club_id, profile_id, message, source_id) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(game)
    .bind(club)
    .bind(author)
    .bind(message)
    .bind(source)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM game_club_notice WHERE game = $1 AND club_id = $2 AND id < ( \
           SELECT id FROM game_club_notice WHERE game = $1 AND club_id = $2 \
           ORDER BY id DESC OFFSET $3 LIMIT 1)",
    )
    .bind(game)
    .bind(club)
    .bind(MAX_NOTICES - 1)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Takes a notice of the club down: `author` only its own (`None`: any, an officer). Whether one
/// was deleted.
pub async fn delete_notice_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    id: i64,
    author: Option<Uuid>,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "DELETE FROM game_club_notice WHERE game = $1 AND club_id = $2 AND id = $3 \
           AND ($4::uuid IS NULL OR profile_id = $4)",
    )
    .bind(game)
    .bind(club)
    .bind(id)
    .bind(author)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The club's notices, newest first.
pub async fn notices_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
) -> Result<Vec<Notice>, IdentityError> {
    let rows: Vec<(i64, Uuid, String)> = sqlx::query_as(
        "SELECT id, profile_id, message FROM game_club_notice WHERE game = $1 AND club_id = $2 \
         ORDER BY id DESC LIMIT $3",
    )
    .bind(game)
    .bind(club)
    .bind(MAX_NOTICES)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, profile_id, message)| Notice {
            id,
            profile_id,
            message,
        })
        .collect())
}

/// A profile's club history in a mode, newest first, at most `max`.
pub async fn history_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    profile: Uuid,
    max: i64,
) -> Result<Vec<HistoryRow>, IdentityError> {
    let rows = sqlx::query(
        "SELECT club_name, role, action, at FROM game_club_history \
         WHERE game = $1 AND profile_id = $2 AND mode = $3 ORDER BY id DESC LIMIT $4",
    )
    .bind(game)
    .bind(profile)
    .bind(mode)
    .bind(max.clamp(0, MAX_HISTORY))
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(HistoryRow {
                club_name: r.try_get("club_name")?,
                role: r.try_get("role")?,
                action: r.try_get("action")?,
                at: ms(r.try_get("at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Whether the profile takes club invites (no setting: it does).
pub async fn accepts_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
) -> Result<bool, IdentityError> {
    let v: Option<bool> = sqlx::query_scalar(
        "SELECT accepts_invites FROM game_club_setting WHERE game = $1 AND profile_id = $2",
    )
    .bind(game)
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(v.unwrap_or(true))
}

/// User.AcceptClubInvites.
pub async fn set_accepts_in(
    conn: &mut PgConnection,
    game: &str,
    profile: Uuid,
    accepts: bool,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO game_club_setting (game, profile_id, accepts_invites) VALUES ($1, $2, $3) \
         ON CONFLICT (game, profile_id) DO UPDATE SET accepts_invites = EXCLUDED.accepts_invites",
    )
    .bind(game)
    .bind(profile)
    .bind(accepts)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The members of a club (every stored row): all, or the officers only, but `except`.
pub async fn recipients_in(
    conn: &mut PgConnection,
    game: &str,
    club: i32,
    officers: bool,
    except: Option<Uuid>,
) -> Result<Vec<Uuid>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT profile_id FROM game_club_member WHERE game = $1 AND club_id = $2 \
           AND (NOT $3 OR role <= 2) AND ($4::uuid IS NULL OR profile_id <> $4) \
         ORDER BY profile_id",
    )
    .bind(game)
    .bind(club)
    .bind(officers)
    .bind(except)
    .fetch_all(&mut *conn)
    .await?)
}

/// Puts one club notification of `kind` in each recipient's in-game inbox (`fi_db::inbox`'s
/// `game_notification`), one statement, then prunes those inboxes to their bound in one more.
/// `sender` is the player whose identity `params` carry (so an erasure of that player deletes the
/// rows, `fi_erase_profile_data`).
pub async fn notify_in(
    conn: &mut PgConnection,
    game: &str,
    recipients: &[Uuid],
    kind: i32,
    params: &Json,
    sender: Option<Uuid>,
) -> Result<(), IdentityError> {
    if recipients.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO game_notification (game, profile_id, type, struct, params, sender_id) \
         SELECT $1, r, $3, $4, $5, $6 FROM unnest($2::uuid[]) AS r",
    )
    .bind(game)
    .bind(recipients)
    .bind(kind)
    .bind(note_struct(kind))
    .bind(Jsonb(params))
    .bind(sender)
    .execute(&mut *conn)
    .await?;
    // `fi_db::inbox::prune_in`'s rule for several inboxes at once: expired rows and each inbox's
    // rows beyond its newest MAX_ROWS (server-cleared tombstones first), SKIP LOCKED.
    sqlx::query(
        "DELETE FROM game_notification WHERE game = $1 AND id IN ( \
           SELECT n.id FROM game_notification n WHERE n.game = $1 AND n.id IN ( \
             SELECT x.id FROM ( \
               SELECT id, sent_at, row_number() OVER (PARTITION BY profile_id \
                 ORDER BY (cleared_by IS DISTINCT FROM 'server') DESC, id DESC) AS k \
               FROM game_notification WHERE game = $1 AND profile_id = ANY($2::uuid[]) \
             ) x WHERE x.k > $4 OR x.sent_at < clock_timestamp() - make_interval(days => $3)) \
           FOR UPDATE SKIP LOCKED)",
    )
    .bind(game)
    .bind(recipients)
    .bind(i32::try_from(crate::inbox::TTL_DAYS).unwrap_or(30))
    .bind(crate::inbox::MAX_ROWS)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Adds an accepted XP report to the profile's club membership of `mode` (the ladder and the
/// club's XP): called by [`crate::scores::add_xp_in`] when it stores a new report.
pub async fn add_member_xp_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    profile: Uuid,
    xp: f64,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE game_club_member m SET xp = LEAST(m.xp + $4, 1e15) FROM game_club c \
         WHERE m.game = $1 AND m.profile_id = $2 AND c.game = m.game AND c.id = m.club_id AND c.mode = $3",
    )
    .bind(game)
    .bind(profile)
    .bind(mode)
    .bind(xp.max(0.0))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A mode change (fi_db::purge::change_mode_in, before the purge): the profile leaves its club of
/// the old mode (succession as [`leave_in`], a `left` history entry, no notification) and its
/// pending invites and requests go. Whether it was in a club.
pub async fn leave_mode_in(
    conn: &mut PgConnection,
    game: &str,
    old_mode: &str,
    profile: Uuid,
) -> Result<bool, IdentityError> {
    lock_in(conn, game, old_mode).await?;
    let left = leave_in(conn, game, profile, ACTION_LEFT, true).await?;
    clear_asks_in(conn, game, profile).await?;
    Ok(left.is_some())
}

/// What [`erase_profile_in`] deleted, per table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfileErased {
    /// Its membership (0 or 1).
    pub member: u64,
    /// Its club, deleted because it was the last member (0 or 1).
    pub club: u64,
    /// Invites to it or sent by it.
    pub invites: u64,
    /// Its requests to join.
    pub requests: u64,
    /// Its notices.
    pub notices: u64,
    /// Its history.
    pub history: u64,
    /// Its setting.
    pub setting: u64,
    /// Removal recommendations naming it in officers' inboxes.
    pub notifications: u64,
}

/// A profile's erasure (fi_db::erasure, both the account erasure and a player's profile deletion):
/// it leaves its club silently (succession as [`leave_in`], no notification, no history), and its
/// invites (both ways), requests, notices, history and setting go, with the removal
/// recommendations that name it in other members' inboxes (every other club notification naming
/// it carries it as `sender_id` and goes with `fi_erase_profile_data`).
pub async fn erase_profile_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<ProfileErased, sqlx::Error> {
    let at: Option<(String, i64)> =
        sqlx::query_as("SELECT p.game, p.xuid FROM profile p WHERE p.id = $1")
            .bind(profile)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((game, xuid)) = at else {
        return Ok(ProfileErased::default());
    };
    let mut out = ProfileErased::default();
    // Both modes' locks, in a fixed order, before anything is read or deleted: an officer's
    // invite, approval or join of this profile (which takes its mode's lock) then either ran
    // before (its rows are deleted below) or runs after the erasure committed (the profile is no
    // longer active), so no club row of an erased profile survives (review finding 1).
    for mode in ["hardcore", "noob"] {
        lock_in(conn, &game, mode).await.map_err(db_error)?;
    }
    let club_mode: Option<String> = sqlx::query_scalar(
        "SELECT c.mode FROM game_club_member m JOIN game_club c ON c.game = m.game AND c.id = m.club_id \
         WHERE m.game = $1 AND m.profile_id = $2",
    )
    .bind(&game)
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    if club_mode.is_some()
        && let Some(left) = leave_in(conn, &game, profile, ACTION_REMOVED, false)
            .await
            .map_err(db_error)?
    {
        out.member = 1;
        out.club = u64::from(left.deleted);
    }
    let run = |sql: &'static str| sqlx::query(sql).bind(game.clone()).bind(profile);
    out.invites =
        run("DELETE FROM game_club_invite WHERE game = $1 AND (profile_id = $2 OR sender_id = $2)")
            .execute(&mut *conn)
            .await?
            .rows_affected();
    out.requests = run("DELETE FROM game_club_request WHERE game = $1 AND profile_id = $2")
        .execute(&mut *conn)
        .await?
        .rows_affected();
    out.notices = run("DELETE FROM game_club_notice WHERE game = $1 AND profile_id = $2")
        .execute(&mut *conn)
        .await?
        .rows_affected();
    out.history = run("DELETE FROM game_club_history WHERE game = $1 AND profile_id = $2")
        .execute(&mut *conn)
        .await?
        .rows_affected();
    out.setting = run("DELETE FROM game_club_setting WHERE game = $1 AND profile_id = $2")
        .execute(&mut *conn)
        .await?
        .rows_affected();
    out.notifications = sqlx::query(
        "DELETE FROM game_notification WHERE game = $1 AND type = $2 \
           AND params->>'recommendedForRemovalXuid' = $3",
    )
    .bind(&game)
    .bind(NOTE_REMOVE_RECOMMENDATION)
    .bind(xuid.to_string())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(out)
}

fn db_error(e: IdentityError) -> sqlx::Error {
    match e {
        IdentityError::Db(e) => e,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_statement_names_every_bind() {
        let sql = list_sql();
        for n in 1..=15 {
            assert!(sql.contains(&format!("${n}")), "${n}");
        }
        assert!(!sql.contains("$16"));
        assert_eq!(note_struct(NOTE_NOTICE), "ClubNoticeBoardMessageParameters");
        assert_eq!(note_struct(NOTE_REQUEST), "ClubMembershipRequestParameters");
    }
}
