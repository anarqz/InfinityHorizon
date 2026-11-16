//! Car Clubs on the Forza RPC (SER-M3 step 8; GAME_PLATFORM.md §3a "Car Clubs", register CR-S70;
//! OD-129 everyone, OD-130 one mode per club): `Forza.WebServices.CarClub` (details, searches,
//! recommendations, keywords), `Xls.WebServices.Club` (create, edit, roles, members, invites,
//! requests, leaving, removal, the notice board, history), `Scoreboard.GetLadderRows` (the club
//! ladder) and Xls `User.AcceptClubInvites`, ported from the Beta's `forza/clubs.rs` over
//! [`fi_db::clubs`]. Served to every ticket (`forza::served`, before the Supporter check, as the
//! drivatars).
//!
//! **One mode.** Every read and write is of the ticket's game and mode: a club, its members,
//! invites, requests, notices, ladder and boards are never seen from the other mode, and the same
//! tag may live in both. Players named by XUID or name are looked up visible in that mode
//! ([`fi_db::visibility`]); an unknown one is the Beta's Xls error 1.
//!
//! **Writes** take the mode's club lock ([`clubs::lock_in`]), then record the request's `club`
//! fact ([`record`]): a replayed request id writes nothing and answers what the first one did
//! (CreateClub its club, RequestJoiningClub whether the caller is now a member, the notice board
//! the same notice id). Notifications go to the members' in-game inboxes (tab 7, "Club") with the
//! Beta's types and parameter structs, `sender_id` the player whose identity they carry.
//!
//! **Text** the game posts (name, description, notices) is user content: cleaned as messages are
//! ([`inbox::clean_text`]: single spaces, no control or invisible characters), then bounded by the
//! schema's UTF-16 capacity: tag 2 to 4 letters or digits (stored in capitals), name 1 to 24,
//! description up to 200, a notice 1 to 200 characters. Never logged.
//!
//! **The ladder** (FH2-S18) ranks the club's members by the XP they earned this week (since Monday
//! 00:00 UTC, never from before they joined; `fi_db::clubs::Ranking::Weekly`). Its reward tiers
//! ([`LADDER_TIERS`]) have weekly thresholds (owner, 2026-11-02) and the Beta's credits (owner: "go
//! with everything the BETA have"), sent **highest first**: the client fills Tier 1 from the first
//! threshold. Every row's `topTierCount` is the
//! number of members in Tier 1 ([`top_tier_count`]); as in the Beta, nothing pays the tiers.
//!
//! **Club XP** goes to the game in units of [`CLUB_XP_UNIT`] ([`club_xp`]): the pause menu reads
//! `clubXP × 10000` and shows level `min(9999, XP / 1,000,000 + 1)`, so a raw value read 9999 for
//! any large club. The club boards' `score` stays raw XP: the board passes it to the same level
//! function without the ×10000.

use super::super::forza::{self, Reply, Served, XlsError};
use super::super::rpc::{self, Ctx, record};
use super::boards::game_time;
use super::inbox::clean_text;
use crate::errors::ApiError;
use fi_db::clubs::{
    self, Badge, Club, ClubFields, ClubPlayer, ClubQuery, Created, Invited, Joined, Left,
    ROLE_COLEADER, ROLE_LEADER, ROLE_MEMBER, Ranking, Requested,
};
use fi_db::gameplane::Player;
use fi_db::limits::{self, Hit};
use fi_xtsw::truncate_utf16;
use serde_json::{Map, Value as Json, json};
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

/// `Forza.WebServices.CarClub`.
pub const CAR_CLUB: &str = "Forza.WebServices.CarClub";
/// `Xls.WebServices.Club`.
pub const CLUB: &str = "Xls.WebServices.Club";

const READS: [&str; 8] = [
    "GetClubDetails",
    "GetClubFullInfo",
    "GetClubKeywords",
    "GetFriendsClubs",
    "GetRecommendedClubs",
    "SearchForClubByTag",
    "SearchForClubs",
    "SearchForClubsByName",
];

const ACTIONS: [&str; 19] = [
    "AcceptClubInvite",
    "ApproveRequestToJoin",
    "ChangeClubDetails",
    "ChangeMemberRole",
    "CreateClub",
    "CreateNoticeBoardNotification",
    "DeleteNoticeBoardNotification",
    "GetClubHistory",
    "GetClubMembers",
    "GetClubStatus",
    "GetUsersToInvite",
    "GetUsersToJoin",
    "InviteToClub",
    "InviteToClubByName",
    "LeaveClub",
    "RecommendMemberForRemoval",
    "RemoveFromClub",
    "RequestJoiningClub",
    "RequestJoiningClubByName",
];

/// A club list's length when the game asks for none (the Beta's).
pub const DEFAULT_LIST: i64 = 25;
/// GetClubMembers' length when the game asks for none (the Beta's).
pub const DEFAULT_MEMBERS: i64 = 100;
/// The tag's length, characters.
pub const TAG_CHARS: (usize, usize) = (2, 4);
/// The name's capacity (ClubData.clubName, UTF-16 units).
pub const NAME_CHARS: usize = 24;
/// The description's capacity (ClubData.clubDescription).
pub const DESCRIPTION_CHARS: usize = 200;
/// A notice's length, characters (the Beta's).
pub const NOTICE_CHARS: usize = 200;
/// `ClubNoticeBoardMessageParameters.noticeBoardMessage`, UTF-16 units.
pub const NOTICE_FIELD: usize = 280;
/// A member is "recently changed" (GetClubMembers) this long after joining (the Beta's).
pub const RECENT_MS: i64 = 7 * 24 * 3600 * 1000;

/// `ForzaCarClubKeyword.Category`: region.
pub const CATEGORY_REGION: i64 = 0;
/// Attitude.
pub const CATEGORY_ATTITUDE: i64 = 1;
/// Interest.
pub const CATEGORY_INTEREST: i64 = 2;

/// The keywords offered (id, category, English, pt-BR): the Beta's list. The ids are what
/// CreateClub and ChangeClubDetails send back.
pub const KEYWORDS: &[(i64, i64, &str, &str)] = &[
    (1, CATEGORY_REGION, "South America", "América do Sul"),
    (2, CATEGORY_REGION, "North America", "América do Norte"),
    (3, CATEGORY_REGION, "Europe", "Europa"),
    (4, CATEGORY_REGION, "Asia", "Ásia"),
    (5, CATEGORY_REGION, "Oceania", "Oceania"),
    (6, CATEGORY_REGION, "Africa", "África"),
    (11, CATEGORY_ATTITUDE, "Casual", "Casual"),
    (12, CATEGORY_ATTITUDE, "Competitive", "Competitivo"),
    (13, CATEGORY_ATTITUDE, "Friendly", "Amigável"),
    (14, CATEGORY_ATTITUDE, "Hardcore", "Hardcore"),
    (21, CATEGORY_INTEREST, "Racing", "Corridas"),
    (22, CATEGORY_INTEREST, "Drift", "Drift"),
    (23, CATEGORY_INTEREST, "Off-road", "Off-road"),
    (24, CATEGORY_INTEREST, "Classics", "Clássicos"),
    (25, CATEGORY_INTEREST, "Supercars", "Superesportivos"),
    (26, CATEGORY_INTEREST, "Liveries", "Pinturas"),
    (27, CATEGORY_INTEREST, "Photography", "Fotografia"),
    (28, CATEGORY_INTEREST, "Tuning", "Ajustes"),
];

/// How `(xls, class, method)` is answered; `None`: not this module's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    match (xls, short, method) {
        (false, "CarClub", m) if READS.contains(&m) => Some(Served::Store),
        (true, "Club", m) if ACTIONS.contains(&m) => Some(Served::Store),
        (false, "Scoreboard", "GetLadderRows") | (true, "User", "AcceptClubInvites") => {
            Some(Served::Store)
        }
        _ => None,
    }
}

/// Xls error 1 with the Beta's message.
fn err(message: &'static str) -> XlsError {
    XlsError { code: 1, message }
}

/// The caller's own club fields for UserData (Logon, GetUserData).
pub(crate) async fn own_badge(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<Badge, ApiError> {
    let v = ctx.viewer();
    Ok(clubs::badges_in(conn, v.game, v.mode, &[v.profile_id])
        .await?
        .pop()
        .map(|b| b.1)
        .unwrap_or_else(|| Badge {
            club: None,
            accepts_invites: true,
        }))
}

/// A keyword as the game reads it (`{}` for none or an unknown id), in the reader's language.
pub fn keyword(id: i64, lang: forza::Lang) -> Json {
    match KEYWORDS.iter().find(|k| k.0 == id) {
        Some(&(id, category, en, pt)) => json!({
            "Keyword": if lang == forza::Lang::PtBr { pt } else { en },
            "KeywordId": id,
            "Category": category,
        }),
        None => json!({}),
    }
}

/// A keyword id a club keeps: one of [`KEYWORDS`] of `category`, else 0.
pub fn keyword_id(v: &Json, category: i64) -> i16 {
    let id = v.as_i64().unwrap_or(0);
    if KEYWORDS.iter().any(|k| k.0 == id && k.1 == category) {
        i16::try_from(id).unwrap_or(0)
    } else {
        0
    }
}

/// A tag as typed: 2 to 4 ASCII letters or digits, in capitals; `None` otherwise.
pub fn tag_of(text: &str) -> Option<String> {
    let t = text.trim();
    ((TAG_CHARS.0..=TAG_CHARS.1).contains(&t.chars().count())
        && t.chars().all(|c| c.is_ascii_alphanumeric()))
    .then(|| t.to_ascii_uppercase())
}

/// Player text kept and shown to others: cleaned ([`clean_text`]) to at most `chars` characters
/// and `units` UTF-16 units.
pub fn text_of(text: &str, chars: usize, units: usize) -> String {
    truncate_utf16(&clean_text(text, chars), units).to_owned()
}

/// The fields of a CreateClub or ChangeClubDetails: `None` for a name that cleans to nothing or a
/// privacy outside 1 to 4.
pub fn fields_of(
    name: &str,
    description: &str,
    privacy: &Json,
    keywords: [&Json; 3],
) -> Option<ClubFields> {
    let name = text_of(name, NAME_CHARS, NAME_CHARS);
    let privacy = privacy
        .as_i64()
        .filter(|p| (1..=4).contains(p))
        .and_then(|p| i16::try_from(p).ok())?;
    (!name.is_empty()).then(|| ClubFields {
        name,
        description: text_of(description, DESCRIPTION_CHARS, DESCRIPTION_CHARS),
        privacy,
        region: keyword_id(keywords[0], CATEGORY_REGION),
        attitude: keyword_id(keywords[1], CATEGORY_ATTITUDE),
        interest: keyword_id(keywords[2], CATEGORY_INTEREST),
    })
}

/// Whether the club takes members without approval (ClubData.isOpenForMembership).
pub fn open_for_membership(privacy: i16) -> bool {
    privacy == clubs::PRIVACY_OPEN || privacy == clubs::PRIVACY_FRIENDS
}

/// A UserData of a club member or any other player, with its badge (none known: no club).
pub fn user(player: &Player, badge: Option<&Badge>) -> Json {
    let p = rpc::player_of(player.clone());
    match badge {
        Some(b) => forza::user_data_in(&p, b),
        None => forza::user_data(&p),
    }
}

/// A member's UserData in its club (its badge is the club's; `accepts` from the batch when read).
fn member_user(m: &ClubPlayer, club: &Club, badges: &Badges) -> Json {
    let accepts = badges.get(&m.profile_id).is_none_or(|b| b.accepts_invites);
    user(
        &m.player,
        Some(&Badge {
            club: Some((club.id, club.tag.clone(), m.role)),
            accepts_invites: accepts,
        }),
    )
}

type Badges = HashMap<Uuid, Badge>;

/// The ClubData of a club.
pub fn club_data(c: &Club, badges: &Badges, lang: forza::Lang) -> Json {
    let some = |m: &Option<ClubPlayer>| match m {
        Some(m) => member_user(m, c, badges),
        None => json!({}),
    };
    json!({
        "clubId": c.id,
        "clubTag": c.tag,
        "clubName": c.name,
        "clubDescription": c.description,
        "isOpenForMembership": open_for_membership(c.privacy),
        "creationDate": game_time(c.created_at),
        "carCount": 0,
        "memberCount": c.members.clamp(0, i64::from(i16::MAX)),
        "clubLeader": some(&c.leader),
        "friendInClub": some(&c.friend),
        "clubChampion": some(&c.champion),
        "regionKeyword": keyword(i64::from(c.region), lang),
        "attitudeKeyword": keyword(i64::from(c.attitude), lang),
        "interestKeyword": keyword(i64::from(c.interest), lang),
        "clubPrivacy": c.privacy,
        "clubXP": club_xp(c.xp),
        "clubXPRank": c.xp_rank.clamp(0, i64::from(i32::MAX)),
    })
}

/// ClubShortData (a scoreboard row's club).
pub fn club_short(c: &Club) -> Json {
    json!({
        "clubId": c.id,
        "clubTag": c.tag,
        "clubName": c.name,
        "memberCount": c.members.clamp(0, i64::from(i16::MAX)),
        "clubPrivacy": c.privacy,
    })
}

/// The unit of `ClubData.clubXP` (FH2-S18): the game multiplies the value by 10000 before its
/// level function (exe c18640, then c16a70: level = min(9999, XP / 1,000,000 + 1)).
pub const CLUB_XP_UNIT: f64 = 10_000.0;

/// `ClubData.clubXP`: the club's XP in units of [`CLUB_XP_UNIT`], rounded down, saturating at
/// the i32 (0 for a non-finite or negative value).
pub fn club_xp(xp: f64) -> i64 {
    if xp.is_finite() && xp > 0.0 {
        ((xp / CLUB_XP_UNIT).floor() as i64).clamp(0, i64::from(i32::MAX))
    } else {
        0
    }
}

/// A list's length: the asked one above 0, else `default`, at most [`clubs::MAX_LIST`].
fn wanted(v: &Json, default: i64) -> i64 {
    v.as_i64()
        .filter(|n| *n > 0)
        .unwrap_or(default)
        .min(clubs::MAX_LIST)
}

fn int(v: &Json) -> i32 {
    v.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(0)
}

fn xuid(v: &Json) -> i64 {
    v.as_u64().and_then(|n| i64::try_from(n).ok()).unwrap_or(0)
}

fn is_officer(role: i16) -> bool {
    role == ROLE_LEADER || role == ROLE_COLEADER
}

/// The badges of the players a reply shows, in one statement.
async fn badges_of(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    profiles: Vec<Uuid>,
) -> Result<Badges, ApiError> {
    let v = ctx.viewer();
    Ok(clubs::badges_in(conn, v.game, v.mode, &profiles)
        .await?
        .into_iter()
        .collect())
}

fn club_profiles(list: &[Club]) -> Vec<Uuid> {
    list.iter()
        .flat_map(|c| [&c.leader, &c.champion, &c.friend])
        .filter_map(|m| m.as_ref().map(|m| m.profile_id))
        .collect()
}

async fn friends(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<Vec<Uuid>, ApiError> {
    super::social::friend_ids(conn, ctx).await
}

/// A stored method of this module, on the request's transaction.
pub(crate) async fn stored(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    short: &str,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    match (short, method) {
        ("CarClub", "GetClubKeywords") => {
            let max = params["maxKeywordResults"]
                .as_u64()
                .filter(|n| *n > 0)
                .map_or(KEYWORDS.len(), |n| usize::try_from(n).unwrap_or(usize::MAX));
            let list: Vec<Json> = KEYWORDS
                .iter()
                .take(max)
                .map(|k| keyword(k.0, ctx.lang))
                .collect();
            Ok(Ok(json!({ "keywords": list })))
        }
        ("CarClub", _) => read(conn, ctx, method, &params).await,
        ("Scoreboard", "GetLadderRows") => ladder(conn, ctx, &params).await,
        ("User", "AcceptClubInvites") => {
            let accepts = params["acceptingClubInvites"].as_bool().unwrap_or(true);
            if record(conn, ctx, "club", fact("AcceptClubInvites", 0)).await? {
                let v = ctx.viewer();
                clubs::set_accepts_in(conn, v.game, v.profile_id, accepts).await?;
            }
            Ok(Ok(json!({})))
        }
        ("Club", "GetClubMembers") => members(conn, ctx, &params).await,
        ("Club", "GetClubStatus") => status(conn, ctx, &params).await,
        ("Club", "GetClubHistory") => history(conn, ctx, &params).await,
        ("Club", "GetUsersToInvite") => to_invite(conn, ctx, &params).await,
        ("Club", "GetUsersToJoin") => to_join(conn, ctx, &params).await,
        ("Club", _) => {
            let v = ctx.viewer();
            clubs::lock_in(conn, v.game, v.mode).await?;
            write(conn, ctx, method, &params).await
        }
        _ => Err(ctx.not_served(short, method)),
    }
}

/// The `club` fact of a write (no player text, no other player's id).
fn fact(method: &str, club: i32) -> Map<String, Json> {
    let mut m = Map::new();
    m.insert("method".into(), method.into());
    m.insert("clubId".into(), club.into());
    m
}

/// A list reply of `field` with its total.
fn list_reply(field: &str, list: &[Club], badges: &Badges, lang: forza::Lang) -> Json {
    let rows: Vec<Json> = list.iter().map(|c| club_data(c, badges, lang)).collect();
    let mut out = Map::new();
    out.insert("totalClubs".into(), rows.len().into());
    out.insert(field.into(), rows.into());
    Json::Object(out)
}

async fn read(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    method: &str,
    params: &Json,
) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let friends = friends(conn, ctx).await?;
    let own = clubs::own_in(conn, v.game, v.mode, v.profile_id).await?;
    let (field, list) = match method {
        "GetRecommendedClubs" => {
            let q = ClubQuery {
                public_only: true,
                exclude: own.as_ref().map(|o| o.club_id),
                limit: wanted(&params["maxResults"], DEFAULT_LIST),
                ..ClubQuery::default()
            };
            (
                "clubDetailsList",
                clubs::clubs_in(conn, &v, &friends, &q).await?,
            )
        }
        "GetFriendsClubs" => {
            let mut ids: Vec<i32> = clubs::badges_in(conn, v.game, v.mode, &friends)
                .await?
                .into_iter()
                .filter_map(|(_, b)| b.club.map(|c| c.0))
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let q = ClubQuery {
                ids: Some(&ids),
                limit: wanted(&params["maxResults"], DEFAULT_LIST),
                ..ClubQuery::default()
            };
            (
                "friendsClubDetailsList",
                clubs::clubs_in(conn, &v, &friends, &q).await?,
            )
        }
        "SearchForClubsByName" => {
            let name = params["clubName"].as_str().unwrap_or("").trim();
            let q = ClubQuery {
                name: Some(name),
                public_only: true,
                limit: wanted(&params["maxResults"], DEFAULT_LIST),
                ..ClubQuery::default()
            };
            (
                "clubDetailsList",
                clubs::clubs_in(conn, &v, &friends, &q).await?,
            )
        }
        "SearchForClubs" => {
            let p = &params["clubSearchParams"];
            // "WithNone" enums: anything outside the four privacy values (0) means "any".
            let privacy = p["Privacy"]
                .as_i64()
                .filter(|n| (1..=4).contains(n))
                .and_then(|n| i16::try_from(n).ok());
            let k = |f: &str| i16::try_from(p[f].as_i64().unwrap_or(0)).unwrap_or(0);
            let q = ClubQuery {
                privacy,
                region: k("RegionKeywordId"),
                attitude: k("AttitudeKeywordId"),
                interest: k("InterestKeywordId"),
                public_only: true,
                limit: wanted(&params["maxResults"], DEFAULT_LIST),
                ..ClubQuery::default()
            };
            (
                "clubDetailsList",
                clubs::clubs_in(conn, &v, &friends, &q).await?,
            )
        }
        "SearchForClubByTag" => {
            let tag = params["clubTag"].as_str().unwrap_or("");
            let found = match tag_of(tag) {
                Some(t) => clubs::club_by_tag_in(conn, &v, &friends, &t).await?,
                None => None,
            };
            // An error, not empty details: the game lists empty details as a blank club (id 0)
            // and crashes when it is opened (the Beta's finding).
            let Some(c) = found else {
                return Ok(Err(err("no club with that tag")));
            };
            let badges = badges_of(conn, ctx, club_profiles(std::slice::from_ref(&c))).await?;
            return Ok(Ok(
                json!({ "clubDetails": club_data(&c, &badges, ctx.lang) }),
            ));
        }
        "GetClubDetails" | "GetClubFullInfo" => {
            let id = int(&params["clubId"]);
            let Some(c) = clubs::club_in(conn, &v, &friends, id).await? else {
                return Ok(Ok(if method == "GetClubDetails" {
                    json!({ "clubDetails": {} })
                } else {
                    json!({ "clubDetails": {}, "leaders": [] })
                }));
            };
            let mut profiles = club_profiles(std::slice::from_ref(&c));
            let officers = if method == "GetClubFullInfo" {
                let max = wanted(&params["maxLeaders"], DEFAULT_LIST);
                let (_, rows) =
                    clubs::members_in(conn, &v, c.id, Ranking::Role, clubs::MAX_MEMBERS).await?;
                let list: Vec<ClubPlayer> = rows
                    .into_iter()
                    .map(|r| r.member)
                    .filter(|m| is_officer(m.role))
                    .take(usize::try_from(max).unwrap_or(0))
                    .collect();
                profiles.extend(list.iter().map(|m| m.profile_id));
                Some(list)
            } else {
                None
            };
            let badges = badges_of(conn, ctx, profiles).await?;
            let details = club_data(&c, &badges, ctx.lang);
            return Ok(Ok(match officers {
                Some(list) => {
                    let leaders: Vec<Json> =
                        list.iter().map(|m| member_user(m, &c, &badges)).collect();
                    json!({ "clubDetails": details, "leaders": leaders })
                }
                None => json!({ "clubDetails": details }),
            }));
        }
        _ => return Err(ctx.not_served("CarClub", method)),
    };
    let badges = badges_of(conn, ctx, club_profiles(&list)).await?;
    Ok(Ok(list_reply(field, &list, &badges, ctx.lang)))
}

/// The club ladder's reward tiers: (weekly XP to reach, credits shown), **Tier 1 first**
/// (FH2-S18): the client sorts the rows into its tier tabs by walking this list in order (a row
/// goes to the first tier whose threshold it reaches), so the highest threshold leads and the
/// lowest tab takes everyone under the last one. The thresholds are weekly (owner, 2026-11-02:
/// the ladder counts a week's XP); the credits are the Beta's. Nothing pays them.
pub const LADDER_TIERS: [(f32, i32); 5] = [
    (50_000.0, 100_000),
    (25_000.0, 50_000),
    (10_000.0, 25_000),
    (2_500.0, 10_000),
    (500.0, 5_000),
];

/// `topTierCount` of a GetLadderRows (FH2-S18, the field read as other projects used as reference read it): the members whose
/// weekly score reaches Tier 1, the same on every row.
pub fn top_tier_count(scores: impl IntoIterator<Item = f64>) -> usize {
    let top = f64::from(LADDER_TIERS[0].0);
    scores.into_iter().filter(|s| *s >= top).count()
}

/// The `ladderTiers` of a GetLadderRows: the first `max` tiers, Tier 1 first (5 when absent or
/// not above 0).
pub fn ladder_tiers(max: Option<i64>) -> Json {
    let n = max
        .filter(|n| *n > 0)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(LADDER_TIERS.len());
    Json::Array(
        LADDER_TIERS
            .iter()
            .take(n)
            .enumerate()
            .map(|(i, (xp, credits))| {
                json!({ "xpThreshold": xp, "creditRewardAmount": credits, "tierNumber": i + 1 })
            })
            .collect(),
    )
}

/// A ClubLadderRow; `top` is the ladder's [`top_tier_count`].
pub fn ladder_row(rank: usize, user: Json, score: f64, top: usize) -> Json {
    json!({ "rank": rank, "user": user, "score": score, "topTierCount": top })
}

/// Scoreboard.GetLadderRows: the club's visible members by the XP they earned in it this week,
/// with the Beta's reward tiers, Tier 1 first.
async fn ladder(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let friends = friends(conn, ctx).await?;
    let club = clubs::club_in(conn, &v, &friends, int(&params["clubId"])).await?;
    let (total, rows) = match &club {
        Some(c) => clubs::members_in(conn, &v, c.id, Ranking::Weekly, clubs::MAX_MEMBERS).await?,
        None => (0, Vec::new()),
    };
    let top = top_tier_count(rows.iter().map(|r| r.xp));
    let start = params["startAt"].as_i64().unwrap_or(1).max(1);
    let max = params["maxResults"]
        .as_i64()
        .filter(|n| *n > 0)
        .unwrap_or(100)
        .min(clubs::MAX_LIST);
    let mut profiles: Vec<Uuid> = rows.iter().map(|r| r.member.profile_id).collect();
    profiles.push(v.profile_id);
    let badges = badges_of(conn, ctx, profiles).await?;
    let mut page = Vec::new();
    let mut mine = None;
    for (i, r) in rows.iter().enumerate() {
        let rank = i + 1;
        let row = || match &club {
            Some(c) => ladder_row(rank, member_user(&r.member, c, &badges), r.xp, top),
            None => json!({}),
        };
        if r.member.profile_id == v.profile_id {
            mine = Some(row());
        }
        let n = i64::try_from(rank).unwrap_or(i64::MAX);
        if n >= start && n < start.saturating_add(max) {
            page.push(row());
        }
    }
    let user_row = match mine {
        Some(r) => r,
        None => {
            let me = rpc::player_of(caller(conn, ctx).await?);
            let badge = badges.get(&v.profile_id).cloned().unwrap_or_default();
            ladder_row(0, forza::user_data_in(&me, &badge), 0.0, top)
        }
    };
    Ok(Ok(json!({
        "rows": page,
        "ladderTiers": ladder_tiers(params["maxTiers"].as_i64()),
        "userRow": user_row,
        "available": total.clamp(0, i64::from(i32::MAX)),
    })))
}

/// The visible members of the caller's club (the boards' club circle; none outside a club).
pub(crate) async fn member_ids(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
) -> Result<Vec<Uuid>, ApiError> {
    let v = ctx.viewer();
    Ok(
        match clubs::own_in(conn, v.game, v.mode, v.profile_id).await? {
            Some(o) => clubs::member_ids_in(conn, &v, o.club_id).await?,
            None => Vec::new(),
        },
    )
}

/// A ScoreboardRow of the club XP board: a player, its score, its club.
pub fn xp_row(position: usize, user: Json, score: f64, club: &Club) -> Json {
    json!({
        "position": position,
        "rank": position,
        "user": user,
        "score": score,
        "submittedTime": game_time(club.created_at),
        "isClean": true,
        "clubData": club_short(club),
    })
}

/// The car club XP board (CLUB LEADERBOARD: scoreboard type 8, score type 4, `isClubBoard`; the
/// Beta's `club_xp_rows`): view 3 is the caller's club, its visible members by the XP they earned
/// in it this week (the ladder); every other view is the mode's clubs the caller may see by club XP (at most
/// [`clubs::MAX_LIST`]), each shown by its visible leader. `startAt` from 1, `maxResults` 0..500.
pub(crate) async fn xp_board(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
    anthem: bool,
) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let friends = friends(conn, ctx).await?;
    let own = clubs::own_in(conn, v.game, v.mode, v.profile_id).await?;
    // (user, score, the caller's own row, its club)
    let mut ranked: Vec<(ClubPlayer, f64, bool, Club)> = Vec::new();
    if params["view"].as_i64() == Some(super::boards::VIEW_CLUB) {
        if let Some(o) = &own
            && let Some(c) = clubs::club_in(conn, &v, &friends, o.club_id).await?
        {
            let (_, rows) =
                clubs::members_in(conn, &v, c.id, Ranking::Weekly, clubs::MAX_MEMBERS).await?;
            for r in rows {
                let mine = r.member.profile_id == v.profile_id;
                ranked.push((r.member, r.xp, mine, c.clone()));
            }
        }
    } else {
        let q = ClubQuery {
            order: clubs::Order::Xp,
            ..ClubQuery::default()
        };
        for c in clubs::clubs_in(conn, &v, &friends, &q).await? {
            if let Some(l) = c.leader.clone() {
                let mine = own.as_ref().is_some_and(|o| o.club_id == c.id);
                ranked.push((l, c.xp, mine, c));
            }
        }
    }
    let mut profiles: Vec<Uuid> = ranked.iter().map(|r| r.0.profile_id).collect();
    profiles.push(v.profile_id);
    let badges = badges_of(conn, ctx, profiles).await?;
    let start = usize::try_from(params["startAt"].as_i64().unwrap_or(1).max(1)).unwrap_or(1);
    let max =
        usize::try_from(params["maxResults"].as_i64().unwrap_or(50).clamp(0, 500)).unwrap_or(0);
    let row = |i: usize, r: &(ClubPlayer, f64, bool, Club)| {
        xp_row(i + 1, member_user(&r.0, &r.3, &badges), r.1, &r.3)
    };
    let page: Vec<Json> = ranked
        .iter()
        .enumerate()
        .skip(start - 1)
        .take(max)
        .map(|(i, r)| row(i, r))
        .collect();
    let user_row = match ranked.iter().position(|r| r.2) {
        Some(i) => row(i, &ranked[i]),
        None => {
            let me = rpc::player_of(caller(conn, ctx).await?);
            let badge = badges.get(&v.profile_id).cloned().unwrap_or_default();
            json!({ "user": forza::user_data_in(&me, &badge) })
        }
    };
    let available = i64::try_from(ranked.len()).unwrap_or(i64::MAX);
    let mut out = json!({ "rows": page, "userRow": user_row, "available": available });
    if anthem {
        out["totalRowCount"] = available.into();
    }
    Ok(Ok(out))
}

async fn caller(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<Player, ApiError> {
    fi_db::gameplane::player_in(conn, ctx.req.ticket.game(), ctx.req.ticket.live.profile_id)
        .await?
        .ok_or_else(ApiError::unavailable)
}

async fn members(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let friends = friends(conn, ctx).await?;
    let Some(c) = clubs::club_in(conn, &v, &friends, int(&params["clubId"])).await? else {
        return Ok(Ok(json!({ "totalMembers": 0, "clubMembers": [] })));
    };
    let max = params["maxMembers"]
        .as_i64()
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MEMBERS)
        .min(clubs::MAX_MEMBERS);
    let (total, rows) = clubs::members_in(conn, &v, c.id, Ranking::Role, max).await?;
    let badges = badges_of(
        conn,
        ctx,
        rows.iter().map(|r| r.member.profile_id).collect(),
    )
    .await?;
    let now = fi_domain::UnixMs::now_system().millis();
    let list: Vec<Json> = rows
        .iter()
        .map(|r| {
            json!({
                "user": member_user(&r.member, &c, &badges),
                "recentClubStatusChange": now - r.joined_at.millis() < RECENT_MS,
            })
        })
        .collect();
    Ok(Ok(json!({
        "totalMembers": total.clamp(0, i64::from(u32::MAX)),
        "clubMembers": list,
    })))
}

/// The player a read names by XUID: that profile when visible in the caller's mode, else the
/// caller (the Beta's `meets`).
async fn named_or_me(conn: &mut PgConnection, ctx: &Ctx<'_>, x: i64) -> Result<Uuid, ApiError> {
    let v = ctx.viewer();
    if x == 0 || x == v.xuid {
        return Ok(v.profile_id);
    }
    Ok(clubs::profile_by_xuid_in(conn, &v, x)
        .await?
        .unwrap_or(v.profile_id))
}

async fn status(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let who = named_or_me(conn, ctx, xuid(&params["xuid"])).await?;
    let badges = badges_of(conn, ctx, vec![who]).await?;
    let b = badges.get(&who).cloned().unwrap_or(Badge {
        club: None,
        accepts_invites: true,
    });
    Ok(Ok(json!({ "clubStatus": {
        "acceptsClubInvites": b.accepts_invites,
        "clubId": b.club.map_or(0, |c| c.0),
    }})))
}

async fn history(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let who = named_or_me(conn, ctx, xuid(&params["userXuid"])).await?;
    let max = params["maxEntries"].as_i64().unwrap_or(20);
    let rows = clubs::history_in(conn, v.game, v.mode, who, max).await?;
    let player = clubs::players_in(conn, v.game, &[who]).await?.pop();
    let badges = badges_of(conn, ctx, vec![who]).await?;
    let user_json = match &player {
        Some((_, p, _)) => user(p, badges.get(&who)),
        None => json!({}),
    };
    let entries: Vec<Json> = rows
        .iter()
        .map(|h| {
            json!({
                "clubName": h.club_name,
                "date": game_time(h.at),
                "user": user_json,
                "role": h.role,
                "action": h.action,
            })
        })
        .collect();
    Ok(Ok(json!({ "userClubHistory": entries })))
}

/// The caller's friends (the module's friend set) as players, with their badges and clubs.
async fn friend_players(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
) -> Result<(Vec<(Uuid, Player, bool)>, Badges), ApiError> {
    let v = ctx.viewer();
    let ids = friends(conn, ctx).await?;
    let players = clubs::players_in(conn, v.game, &ids).await?;
    let badges = badges_of(conn, ctx, ids).await?;
    Ok((players, badges))
}

/// GetUsersToInvite: the caller's friends not in its club, with the club they are in.
async fn to_invite(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let own = clubs::own_in(conn, v.game, v.mode, v.profile_id).await?;
    let (players, badges) = friend_players(conn, ctx).await?;
    let max = usize::try_from(wanted(&params["maxResults"], 50)).unwrap_or(50);
    let mut ids: Vec<i32> = badges
        .values()
        .filter_map(|b| b.club.as_ref().map(|c| c.0))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let names: HashMap<i32, String> = club_names(conn, ctx, &ids).await?;
    let list: Vec<Json> = players
        .iter()
        .filter(|(id, _, _)| {
            let theirs = badges.get(id).and_then(|b| b.club.as_ref().map(|c| c.0));
            own.is_none() || theirs != own.as_ref().map(|o| o.club_id)
        })
        .take(max)
        .map(|(id, p, logged)| {
            let b = badges.get(id);
            json!({
                "user": user(p, b),
                "acceptsClubInvites": b.is_none_or(|b| b.accepts_invites),
                "hasEverLoggedIn": logged,
                "clubName": b.and_then(|b| b.club.as_ref()).and_then(|c| names.get(&c.0)).cloned().unwrap_or_default(),
            })
        })
        .collect();
    Ok(Ok(json!({ "candidatesForInvite": list })))
}

/// The names of clubs of the caller's mode (whatever their visibility: a friend's club name).
async fn club_names(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    ids: &[i32],
) -> Result<HashMap<i32, String>, ApiError> {
    let v = ctx.viewer();
    let mut out = HashMap::new();
    for id in ids
        .iter()
        .take(usize::try_from(clubs::MAX_LIST).unwrap_or(200))
    {
        if let Some((_, name, _)) = clubs::stored_in(conn, v.game, v.mode, *id).await? {
            out.insert(*id, name);
        }
    }
    Ok(out)
}

/// GetUsersToJoin: the caller's friends who are in a club the caller may see.
async fn to_join(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let (players, badges) = friend_players(conn, ctx).await?;
    let mut ids: Vec<i32> = badges
        .values()
        .filter_map(|b| b.club.as_ref().map(|c| c.0))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let friend_ids: Vec<Uuid> = players.iter().map(|p| p.0).collect();
    let q = ClubQuery {
        ids: Some(&ids),
        ..ClubQuery::default()
    };
    let list = clubs::clubs_in(conn, &v, &friend_ids, &q).await?;
    let by_id: HashMap<i32, &Club> = list.iter().map(|c| (c.id, c)).collect();
    let max = usize::try_from(wanted(&params["maxResults"], 50)).unwrap_or(50);
    let rows: Vec<Json> = players
        .iter()
        .filter_map(|(id, p, _)| {
            let b = badges.get(id)?;
            let c = by_id.get(&b.club.as_ref()?.0)?;
            Some(json!({
                "user": user(p, Some(b)),
                "clubMemberCount": c.members.clamp(0, i64::from(i32::MAX)),
                "clubName": c.name,
                "clubIsOpenForMembership": open_for_membership(c.privacy),
            }))
        })
        .take(max)
        .collect();
    Ok(Ok(json!({ "usersInClubs": rows })))
}

/// The caller's own club (under the lock), or Xls error 1 "not in a club".
async fn my_club(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
) -> Result<Result<clubs::Own, XlsError>, ApiError> {
    let v = ctx.viewer();
    Ok(clubs::own_in(conn, v.game, v.mode, v.profile_id)
        .await?
        .ok_or_else(|| err("not in a club")))
}

/// Counts one use of `limit` by the caller; `false` past it.
async fn allowed(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    limit: &limits::Limit,
) -> Result<bool, ApiError> {
    let key = limits::profile_key(limit, ctx.viewer().profile_id);
    Ok(limits::hit_in(conn, &key, limit).await? == Hit::Allowed)
}

/// The caller's gamertag as notifications carry it.
async fn my_tag(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<String, ApiError> {
    Ok(rpc::player_of(caller(conn, ctx).await?).gamertag)
}

/// A player's gamertag (the game's 15 characters) by profile.
async fn tag_of_profile(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    profile: Uuid,
) -> Result<(i64, String), ApiError> {
    let v = ctx.viewer();
    let p = clubs::players_in(conn, v.game, &[profile]).await?.pop();
    Ok(match p {
        Some((_, p, _)) => {
            let shown = rpc::player_of(p.clone());
            (p.xuid, shown.gamertag)
        }
        None => (0, String::new()),
    })
}

/// Tells the members of the club a member left (`ClubMemberQuit`, the leaver's identity) and,
/// when the leader left, every member that the heir leads now (`ClubNewPromotion`).
async fn after_leave(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    who: Uuid,
    left: &Left,
) -> Result<(), ApiError> {
    if left.deleted {
        return Ok(());
    }
    let game = ctx.viewer().game;
    let (x, tag) = tag_of_profile(conn, ctx, who).await?;
    let others = clubs::recipients_in(conn, game, left.club_id, false, Some(who)).await?;
    clubs::notify_in(
        conn,
        game,
        &others,
        clubs::NOTE_QUIT,
        &json!({ "exMemberXuid": x, "exMemberGamertag": tag, "clubName": left.name, "clubTag": left.tag }),
        Some(who),
    )
    .await?;
    if let Some(heir) = left.heir {
        promoted(
            conn,
            ctx,
            left.club_id,
            &left.name,
            &left.tag,
            heir,
            ROLE_LEADER,
        )
        .await?;
    }
    Ok(())
}

/// `ClubNewPromotion` to every member of the club.
async fn promoted(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    club: i32,
    name: &str,
    tag: &str,
    who: Uuid,
    role: i16,
) -> Result<(), ApiError> {
    let game = ctx.viewer().game;
    let (x, gamertag) = tag_of_profile(conn, ctx, who).await?;
    let all = clubs::recipients_in(conn, game, club, false, None).await?;
    clubs::notify_in(
        conn,
        game,
        &all,
        clubs::NOTE_PROMOTION,
        &json!({ "newRole": role, "newPromotedXuid": x, "newPromotedGamertag": gamertag,
            "clubName": name, "clubTag": tag }),
        Some(who),
    )
    .await?;
    Ok(())
}

/// Tells a player who joined (`ClubMemberAccepted`) and the other members (`ClubNewMember`).
async fn after_join(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    who: Uuid,
    club: i32,
    left: Option<&Left>,
) -> Result<(), ApiError> {
    if let Some(l) = left {
        after_leave(conn, ctx, who, l).await?;
    }
    let v = ctx.viewer();
    let Some((_, name, tag)) = clubs::stored_in(conn, v.game, v.mode, club).await? else {
        return Ok(());
    };
    clubs::notify_in(
        conn,
        v.game,
        &[who],
        clubs::NOTE_ACCEPTED,
        &json!({ "clubName": name, "clubTag": tag }),
        None,
    )
    .await?;
    let (x, gamertag) = tag_of_profile(conn, ctx, who).await?;
    let others = clubs::recipients_in(conn, v.game, club, false, Some(who)).await?;
    clubs::notify_in(
        conn,
        v.game,
        &others,
        clubs::NOTE_NEW_MEMBER,
        &json!({ "newMemberXuid": x, "newMemberGamertag": gamertag, "clubName": name, "clubTag": tag }),
        Some(who),
    )
    .await?;
    Ok(())
}

/// The club writes, under the mode's club lock.
async fn write(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    method: &str,
    params: &Json,
) -> Result<Reply, ApiError> {
    let v = ctx.viewer();
    let (game, mode, me) = (v.game, v.mode, v.profile_id);
    match method {
        "CreateClub" => {
            let Some(tag) = tag_of(params["clubTag"].as_str().unwrap_or("")) else {
                return Ok(Err(err("tag: 2 to 4 letters or digits")));
            };
            let Some(f) = fields_of(
                params["clubName"].as_str().unwrap_or(""),
                params["clubDescription"].as_str().unwrap_or(""),
                &params["privacy"],
                [
                    &params["regionKeywordId"],
                    &params["attitudeKeywordId"],
                    &params["interestKeywordId"],
                ],
            ) else {
                return Ok(Err(err("name: 1 to 24 characters; privacy: 1 to 4")));
            };
            if !record(conn, ctx, "club", fact(method, 0)).await? {
                // A replay: the club it created, if the caller still leads one.
                return Ok(match clubs::own_in(conn, game, mode, me).await? {
                    Some(o) => Ok(json!({ "clubId": o.club_id })),
                    None => Err(err("the player is not in a club")),
                });
            }
            if !allowed(conn, ctx, &clubs::CLUB_CHANGES).await? {
                return Ok(Err(err("too many club changes")));
            }
            Ok(
                match clubs::create_in(conn, game, mode, me, &tag, &f).await? {
                    Created::Created(id) => Ok(json!({ "clubId": id })),
                    Created::InClub => Err(err("the player is already in a club")),
                    Created::TagTaken => Err(err("that tag already exists")),
                },
            )
        }
        "ChangeClubDetails" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) if is_officer(o.role) => o,
                Ok(_) => return Ok(Err(err("not a club leader"))),
                Err(e) => return Ok(Err(e)),
            };
            let name = params["newClubName"].as_str().unwrap_or(&own.name);
            let Some(f) = fields_of(
                name,
                params["newClubDescription"].as_str().unwrap_or(""),
                &params["privacyLevel"],
                [
                    &params["regionKeyword"],
                    &params["attitudeKeyword"],
                    &params["interestKeyword"],
                ],
            ) else {
                return Ok(Err(err("name: 1 to 24 characters; privacy: 1 to 4")));
            };
            if record(conn, ctx, "club", fact(method, own.club_id)).await? {
                if !allowed(conn, ctx, &clubs::CLUB_CHANGES).await? {
                    return Ok(Err(err("too many club changes")));
                }
                clubs::update_in(conn, game, own.club_id, &f).await?;
            }
            Ok(Ok(json!({})))
        }
        "ChangeMemberRole" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) if o.role == ROLE_LEADER => o,
                Ok(_) => return Ok(Err(err("only the leader changes roles"))),
                Err(e) => return Ok(Err(e)),
            };
            let role = params["newRole"]
                .as_i64()
                .and_then(|r| i16::try_from(r).ok())
                .filter(|r| (ROLE_LEADER..=ROLE_MEMBER).contains(r));
            let who = clubs::profile_by_xuid_in(conn, &v, xuid(&params["userToChange"])).await?;
            // The leader hands over by promoting someone else, never demotes self.
            let (Some(role), Some(who)) = (role, who.filter(|w| *w != me)) else {
                return Ok(Err(err("not a member")));
            };
            if !record(conn, ctx, "club", fact(method, own.club_id)).await? {
                return Ok(Ok(json!({})));
            }
            let Some(before) = clubs::set_role_in(conn, game, mode, own.club_id, who, role).await?
            else {
                return Ok(Err(err("not a member")));
            };
            if role < before {
                promoted(conn, ctx, own.club_id, &own.name, &own.tag, who, role).await?;
            } else if role > before {
                clubs::notify_in(
                    conn,
                    game,
                    &[who],
                    clubs::NOTE_DEMOTION,
                    &json!({ "clubName": own.name, "clubTag": own.tag }),
                    None,
                )
                .await?;
            }
            Ok(Ok(json!({})))
        }
        "InviteToClub" | "InviteToClubByName" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) if is_officer(o.role) || o.privacy == clubs::PRIVACY_OPEN => o,
                Ok(_) => return Ok(Err(err("not allowed to invite"))),
                Err(e) => return Ok(Err(e)),
            };
            let who = match params["usernameToInvite"].as_str() {
                Some(name) => clubs::profile_by_name_in(conn, &v, name)
                    .await?
                    .map(|p| p.0),
                None => clubs::profile_by_xuid_in(conn, &v, xuid(&params["userToInvite"])).await?,
            };
            let Some(who) = who.filter(|w| *w != me) else {
                return Ok(Err(err("unknown player")));
            };
            if !record(conn, ctx, "club", fact(method, own.club_id)).await? {
                return Ok(Ok(json!({})));
            }
            if !allowed(conn, ctx, &clubs::CLUB_ASKS).await? {
                return Ok(Err(err("too many invites")));
            }
            match clubs::invite_in(conn, game, own.club_id, me, who).await? {
                Invited::Sent(_) => {
                    let tag = my_tag(conn, ctx).await?;
                    clubs::notify_in(
                        conn,
                        game,
                        &[who],
                        clubs::NOTE_INVITATION,
                        &json!({ "sponsorXuid": v.xuid, "clubId": own.club_id, "clubName": own.name,
                            "clubTag": own.tag, "sponsorGamertag": tag }),
                        Some(me),
                    )
                    .await?;
                    Ok(Ok(json!({})))
                }
                Invited::Member => Ok(Err(err("the player is already in the club"))),
                Invited::Declines => Ok(Err(err("the player does not accept club invites"))),
                Invited::TooMany => Ok(Err(err("too many pending invites"))),
            }
        }
        "AcceptClubInvite" => {
            let club = int(&params["clubId"]);
            let invitation = params["invitationId"].as_i64().unwrap_or(0);
            if !record(conn, ctx, "club", fact(method, club)).await? {
                return Ok(Ok(json!({})));
            }
            if clubs::invite_of_in(conn, game, club, me, invitation)
                .await?
                .is_none()
            {
                return Ok(Err(err("invite not found")));
            }
            join(conn, ctx, club, me)
                .await
                .map(|r| r.map(|_| json!({})))
        }
        "RequestJoiningClub" | "RequestJoiningClubByName" => {
            let by_name = method == "RequestJoiningClubByName";
            let club = match params["clubTag"].as_str() {
                Some(tag) => clubs::id_by_tag_in(conn, game, mode, tag).await?,
                None => Some(int(&params["clubId"])),
            };
            let answer = |approved: bool| {
                if by_name {
                    json!({})
                } else {
                    json!({ "requestApproved": approved })
                }
            };
            let Some(club) = club else {
                return Ok(Err(err("unknown club")));
            };
            if !record(conn, ctx, "club", fact(method, club)).await? {
                let member = clubs::own_in(conn, game, mode, me)
                    .await?
                    .is_some_and(|o| o.club_id == club);
                return Ok(Ok(answer(member)));
            }
            let friends = friends(conn, ctx).await?;
            let invited = clubs::invite_of_in(conn, game, club, me, 0)
                .await?
                .is_some();
            let shown = clubs::club_in(conn, &v, &friends, club).await?;
            // A club the caller may see, or one that invited it.
            let Some((privacy, _, _)) = clubs::stored_in(conn, game, mode, club).await? else {
                return Ok(Err(err("unknown club")));
            };
            if shown.is_none() && !invited {
                return Ok(Err(err("unknown club")));
            }
            // The Beta's rule: a private club always asks; an invite opens the others.
            let direct = privacy != clubs::PRIVACY_PRIVATE
                && (invited
                    || privacy == clubs::PRIVACY_OPEN
                    || (privacy == clubs::PRIVACY_FRIENDS
                        && shown.is_some_and(|c| c.friend.is_some())));
            if direct {
                return Ok(match join(conn, ctx, club, me).await? {
                    Ok(()) => Ok(answer(true)),
                    Err(e) => Err(e),
                });
            }
            if !allowed(conn, ctx, &clubs::CLUB_ASKS).await? {
                return Ok(Err(err("too many requests")));
            }
            match clubs::request_in(conn, game, club, me).await? {
                Requested::New => {
                    let tag = my_tag(conn, ctx).await?;
                    let officers = clubs::recipients_in(conn, game, club, true, Some(me)).await?;
                    clubs::notify_in(
                        conn,
                        game,
                        &officers,
                        clubs::NOTE_REQUEST,
                        &json!({ "requestorXuid": v.xuid, "requestorGamertag": tag, "clubId": club }),
                        Some(me),
                    )
                    .await?;
                }
                Requested::Again => {}
                Requested::TooMany => return Ok(Err(err("too many pending requests"))),
            }
            Ok(Ok(answer(false)))
        }
        "ApproveRequestToJoin" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) if is_officer(o.role) => o,
                Ok(_) => return Ok(Err(err("not a club leader"))),
                Err(e) => return Ok(Err(e)),
            };
            let Some(who) =
                clubs::profile_by_xuid_in(conn, &v, xuid(&params["approvedUser"])).await?
            else {
                return Ok(Err(err("no such request")));
            };
            if !record(conn, ctx, "club", fact(method, own.club_id)).await? {
                return Ok(Ok(json!({})));
            }
            // Only someone who asked to join.
            if !clubs::take_request_in(conn, game, own.club_id, who).await? {
                return Ok(Err(err("no such request")));
            }
            join(conn, ctx, own.club_id, who)
                .await
                .map(|r| r.map(|_| json!({})))
        }
        "LeaveClub" => {
            let club = clubs::own_in(conn, game, mode, me)
                .await?
                .map_or(0, |o| o.club_id);
            if record(conn, ctx, "club", fact(method, club)).await?
                && let Some(left) =
                    clubs::leave_in(conn, game, me, clubs::ACTION_LEFT, true).await?
            {
                after_leave(conn, ctx, me, &left).await?;
            }
            Ok(Ok(json!({})))
        }
        "RemoveFromClub" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) if is_officer(o.role) => o,
                Ok(_) => return Ok(Err(err("not a club leader"))),
                Err(e) => return Ok(Err(e)),
            };
            let who = clubs::profile_by_xuid_in(conn, &v, xuid(&params["userToRemove"])).await?;
            let theirs = match who {
                Some(w) if w != me => clubs::own_in(conn, game, mode, w)
                    .await?
                    .filter(|t| t.club_id == own.club_id)
                    .map(|t| (w, t.role)),
                _ => None,
            };
            // The leader removes anyone; a co-leader, members only.
            let Some((who, _)) =
                theirs.filter(|(_, r)| own.role == ROLE_LEADER || *r == ROLE_MEMBER)
            else {
                return Ok(Err(err("cannot remove that member")));
            };
            if !record(conn, ctx, "club", fact(method, own.club_id)).await? {
                return Ok(Ok(json!({})));
            }
            if let Some(left) =
                clubs::leave_in(conn, game, who, clubs::ACTION_REMOVED, true).await?
            {
                let (x, tag) = tag_of_profile(conn, ctx, who).await?;
                clubs::notify_in(
                    conn,
                    game,
                    &[who],
                    clubs::NOTE_KICK,
                    &json!({ "kickedXuid": x, "kickedGamertag": tag, "clubName": left.name,
                        "clubTag": left.tag }),
                    None,
                )
                .await?;
                after_leave(conn, ctx, who, &left).await?;
            }
            Ok(Ok(json!({})))
        }
        "RecommendMemberForRemoval" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) => o,
                Err(e) => return Ok(Err(e)),
            };
            let who = clubs::profile_by_xuid_in(conn, &v, xuid(&params["userToRemove"])).await?;
            let member = match who {
                Some(w) if w != me => clubs::own_in(conn, game, mode, w)
                    .await?
                    .is_some_and(|t| t.club_id == own.club_id),
                _ => false,
            };
            let (Some(who), true) = (who, member) else {
                return Ok(Err(err("not a member")));
            };
            if !record(conn, ctx, "club", fact(method, own.club_id)).await? {
                return Ok(Ok(json!({})));
            }
            if !allowed(conn, ctx, &clubs::CLUB_REPORTS).await? {
                return Ok(Err(err("too many reports")));
            }
            let mine = my_tag(conn, ctx).await?;
            let (x, theirs) = tag_of_profile(conn, ctx, who).await?;
            let officers = clubs::recipients_in(conn, game, own.club_id, true, Some(me)).await?;
            clubs::notify_in(
                conn,
                game,
                &officers,
                clubs::NOTE_REMOVE_RECOMMENDATION,
                &json!({ "recommendingRemovalXuid": v.xuid, "recommendingRemovalGamertag": mine,
                    "recommendedForRemovalXuid": x, "recommendedForRemovalGamertag": theirs,
                    "clubId": own.club_id }),
                Some(me),
            )
            .await?;
            Ok(Ok(json!({})))
        }
        "CreateNoticeBoardNotification" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) => o,
                Err(e) => return Ok(Err(e)),
            };
            let text = text_of(
                params["noticeBoardMessage"].as_str().unwrap_or(""),
                NOTICE_CHARS,
                NOTICE_FIELD,
            );
            if text.is_empty() {
                return Ok(Err(err("empty notice")));
            }
            let source = ctx.fact_id();
            if !record(conn, ctx, "club", fact(method, own.club_id)).await? {
                return Ok(
                    match clubs::notice_by_source_in(conn, game, source).await? {
                        Some(id) => Ok(json!({ "notificationId": id })),
                        None => Err(err("too many notices")),
                    },
                );
            }
            if !allowed(conn, ctx, &clubs::CLUB_NOTICES).await? {
                return Ok(Err(err("too many notices")));
            }
            let id = clubs::add_notice_in(conn, game, own.club_id, me, &text, source).await?;
            let tag = my_tag(conn, ctx).await?;
            let others = clubs::recipients_in(conn, game, own.club_id, false, Some(me)).await?;
            clubs::notify_in(
                conn,
                game,
                &others,
                clubs::NOTE_NOTICE,
                &json!({ "senderXuid": v.xuid, "senderClubRole": own.role, "senderGamertag": tag,
                    "noticeBoardMessage": text }),
                Some(me),
            )
            .await?;
            Ok(Ok(json!({ "notificationId": id })))
        }
        // The author, or an officer of the club, takes a notice down.
        "DeleteNoticeBoardNotification" => {
            let own = match my_club(conn, ctx).await? {
                Ok(o) => o,
                Err(e) => return Ok(Err(e)),
            };
            let id = params["notificationId"].as_i64().unwrap_or(0);
            if record(conn, ctx, "club", fact(method, own.club_id)).await? {
                let author = (!is_officer(own.role)).then_some(me);
                clubs::delete_notice_in(conn, game, own.club_id, id, author).await?;
            }
            Ok(Ok(json!({})))
        }
        _ => Err(ctx.not_served("Club", method)),
    }
}

/// Puts `who` in the club of the caller's mode (leaving its current club) and tells everyone.
async fn join(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    club: i32,
    who: Uuid,
) -> Result<Result<(), XlsError>, ApiError> {
    let v = ctx.viewer();
    let (joined, left) = clubs::join_in(conn, v.game, v.mode, club, who).await?;
    Ok(match joined {
        Joined::Joined => {
            after_join(conn, ctx, who, club, left.as_ref()).await?;
            Ok(())
        }
        Joined::Already => Ok(()),
        Joined::Full => Err(err("the club is full")),
        Joined::Unknown => Err(err("unknown club")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_db::gameplane::Card;
    use fi_domain::UnixMs;
    use fi_xtsw::{Schema, decode_results, encode_results};

    fn member(n: i64, role: i16) -> ClubPlayer {
        ClubPlayer {
            profile_id: Uuid::from_u128(u128::try_from(n).unwrap()),
            player: Player {
                xuid: 2_535_100_000_000_000 + n,
                name: Some(format!("Member{n}")),
                card: Card {
                    career_level: 40,
                    badge_id: 2,
                    player_title_id: 5,
                },
                tables_tran_id: None,
            },
            role,
        }
    }

    fn club() -> Club {
        Club {
            id: 12,
            tag: "ABCD".into(),
            // 24 UTF-16 units, the field's capacity.
            name: "Clube dos Testes Ágeis 1".into(),
            description: "d".repeat(200),
            privacy: clubs::PRIVACY_FRIENDS,
            region: 3,
            attitude: 11,
            interest: 99,
            created_at: UnixMs::from_millis(1_792_000_000_123),
            members: 40_000,
            xp: 1e15,
            xp_rank: 2,
            leader: Some(member(1, ROLE_LEADER)),
            champion: Some(member(2, ROLE_COLEADER)),
            friend: None,
        }
    }

    fn encodes(class: &str, method: &str, reply: &Json) -> Json {
        let schema = Schema::fh2();
        let bytes = encode_results(schema, class, method, reply)
            .unwrap_or_else(|e| panic!("{class}.{method}: {e}"));
        decode_results(schema, class, method, &bytes)
            .unwrap()
            .to_json()
    }

    /// FH2-S18: `clubXP` in units of 10000, so the pause menu's level (min(9999, clubXP × 10000 /
    /// 1,000,000 + 1)) matches the club board's level of the raw XP.
    #[test]
    fn club_xp_is_sent_in_units_of_ten_thousand() {
        assert_eq!(club_xp(0.0), 0);
        assert_eq!(club_xp(9_999.9), 0);
        assert_eq!(club_xp(10_000.0), 1);
        // 45,558,854 XP: clubXP 4555, the game's level 46, as the board shows it.
        assert_eq!(club_xp(45_558_854.0), 4555);
        let level = |units: i64| (units * 10_000 / 1_000_000 + 1).min(9999);
        assert_eq!(level(club_xp(45_558_854.0)), 46);
        assert_eq!(club_xp(1e15), i64::from(i32::MAX), "saturates at the i32");
        assert_eq!(club_xp(-5.0), 0);
        assert_eq!(club_xp(f64::NAN), 0);
        assert_eq!(club_xp(f64::INFINITY), 0);
    }

    /// FH2-S18: the tiers run highest first (the client fills Tier 1 from the first threshold),
    /// and `topTierCount` counts the members in Tier 1.
    #[test]
    fn ladder_tiers_run_highest_first() {
        assert!(LADDER_TIERS.windows(2).all(|w| w[0].0 > w[1].0));
        let tiers = ladder_tiers(None);
        for (i, t) in tiers.as_array().unwrap().iter().enumerate() {
            assert_eq!(t["tierNumber"], i + 1);
            assert_eq!(t["xpThreshold"], f64::from(LADDER_TIERS[i].0));
        }
        assert_eq!(top_tier_count([]), 0);
        assert_eq!(top_tier_count([49_999.0, 300_000.0, 50_000.0, 0.0]), 2);
        // Owner, 2026-11-02: weekly thresholds, Tier 1 first; the credits shown stay the Beta's.
        assert_eq!(
            LADDER_TIERS,
            [
                (50_000.0, 100_000),
                (25_000.0, 50_000),
                (10_000.0, 25_000),
                (2_500.0, 10_000),
                (500.0, 5_000),
            ]
        );
        let u = json!({});
        assert_eq!(ladder_row(3, u, 10.0, 2)["topTierCount"], 2);
    }

    /// Every reply of this module encodes with the game's schema (an encode failure is a 503),
    /// at the fields' capacities.
    #[test]
    fn every_reply_encodes_with_the_schema() {
        let c = club();
        let badges = Badges::new();
        for lang in [forza::Lang::En, forza::Lang::PtBr] {
            let keywords: Vec<Json> = KEYWORDS.iter().map(|k| keyword(k.0, lang)).collect();
            let back = encodes(
                CAR_CLUB,
                "GetClubKeywords",
                &json!({ "keywords": keywords }),
            );
            assert_eq!(back["keywords"].as_array().unwrap().len(), KEYWORDS.len());
            let data = club_data(&c, &badges, lang);
            for method in ["GetClubDetails", "SearchForClubByTag"] {
                let back = encodes(CAR_CLUB, method, &json!({ "clubDetails": data }));
                let d = &back["clubDetails"];
                assert_eq!(
                    (d["clubId"].as_i64(), d["clubTag"].as_str()),
                    (Some(12), Some("ABCD"))
                );
                assert_eq!(d["memberCount"], i64::from(i16::MAX), "clamped to the i16");
                assert_eq!(d["clubXP"], i64::from(i32::MAX), "clamped to the i32");
                assert_eq!(d["isOpenForMembership"], true);
                assert_eq!(d["clubLeader"]["clubTag"], "ABCD");
                assert_eq!(d["clubLeader"]["roleInClub"], 1);
                assert_eq!(
                    d["interestKeyword"]["KeywordId"], 0,
                    "an unknown keyword is none"
                );
                assert_eq!(d["creationDate"], "2026-10-14T17:46:40.123Z");
            }
            let leaders = vec![member_user(&member(1, ROLE_LEADER), &c, &badges)];
            encodes(
                CAR_CLUB,
                "GetClubFullInfo",
                &json!({ "clubDetails": data, "leaders": leaders }),
            );
            for (method, field) in [
                ("GetRecommendedClubs", "clubDetailsList"),
                ("SearchForClubs", "clubDetailsList"),
                ("SearchForClubsByName", "clubDetailsList"),
                ("GetFriendsClubs", "friendsClubDetailsList"),
            ] {
                let back = encodes(
                    CAR_CLUB,
                    method,
                    &list_reply(field, std::slice::from_ref(&c), &badges, lang),
                );
                assert_eq!(back["totalClubs"], 1);
            }
        }
        // Not found: empty details (GetClubDetails), never for the tag search.
        encodes(CAR_CLUB, "GetClubDetails", &json!({ "clubDetails": {} }));
        encodes(
            CAR_CLUB,
            "GetClubFullInfo",
            &json!({ "clubDetails": {}, "leaders": [] }),
        );
        let u = member_user(&member(3, ROLE_MEMBER), &c, &badges);
        let back = encodes(
            "Forza.WebServices.Scoreboard",
            "GetLadderRows",
            &json!({ "rows": [ladder_row(1, u.clone(), 1234.5, 0)], "ladderTiers": ladder_tiers(None),
                "userRow": ladder_row(1, u.clone(), 1234.5, 0), "available": 1 }),
        );
        assert_eq!(back["rows"][0]["score"], 1234.5);
        assert_eq!(back["ladderTiers"].as_array().unwrap().len(), 5);
        assert_eq!(back["ladderTiers"][0]["tierNumber"], 1);
        assert_eq!(back["ladderTiers"][0]["creditRewardAmount"], 100_000);
        assert_eq!(back["ladderTiers"][4]["creditRewardAmount"], 5_000);
        let two = ladder_tiers(Some(2));
        assert_eq!(two.as_array().unwrap().len(), 2);
        assert_eq!(two[0]["xpThreshold"], 50_000.0, "Tier 1 is the highest");
        for (method, anthem) in [("GetRows", false), ("GetRowsAnthem", true)] {
            let mut reply = json!({ "rows": [xp_row(1, u.clone(), 5e9, &c)],
                "userRow": xp_row(1, u.clone(), 5e9, &c), "available": 1 });
            if anthem {
                reply["totalRowCount"] = 1.into();
            }
            let back = encodes("Forza.WebServices.Scoreboard", method, &reply);
            assert_eq!(back["rows"][0]["clubData"]["clubTag"], "ABCD");
            assert_eq!(
                back["rows"][0]["clubData"]["memberCount"],
                i64::from(i16::MAX)
            );
        }
        assert_eq!(
            encodes(CLUB, "CreateClub", &json!({ "clubId": 12 }))["clubId"],
            12
        );
        encodes(
            CLUB,
            "GetClubMembers",
            &json!({ "totalMembers": 1, "clubMembers": [{ "user": u, "recentClubStatusChange": true }] }),
        );
        encodes(
            CLUB,
            "GetClubStatus",
            &json!({ "clubStatus": { "acceptsClubInvites": true, "clubId": 12 } }),
        );
        encodes(
            CLUB,
            "GetClubHistory",
            &json!({ "userClubHistory": [{ "clubName": c.name, "date": game_time(c.created_at),
                "user": u, "role": 3, "action": 0 }] }),
        );
        encodes(
            CLUB,
            "GetUsersToInvite",
            &json!({ "candidatesForInvite": [{ "user": u, "acceptsClubInvites": true,
                "hasEverLoggedIn": true, "clubName": c.name }] }),
        );
        encodes(
            CLUB,
            "GetUsersToJoin",
            &json!({ "usersInClubs": [{ "user": u, "clubMemberCount": 3, "clubName": c.name,
                "clubIsOpenForMembership": false }] }),
        );
        for approved in [true, false] {
            encodes(
                CLUB,
                "RequestJoiningClub",
                &json!({ "requestApproved": approved }),
            );
        }
        encodes(
            CLUB,
            "CreateNoticeBoardNotification",
            &json!({ "notificationId": 9 }),
        );
        for method in [
            "AcceptClubInvite",
            "ApproveRequestToJoin",
            "ChangeClubDetails",
            "ChangeMemberRole",
            "DeleteNoticeBoardNotification",
            "InviteToClub",
            "InviteToClubByName",
            "LeaveClub",
            "RecommendMemberForRemoval",
            "RemoveFromClub",
            "RequestJoiningClubByName",
        ] {
            encodes(CLUB, method, &json!({}));
        }
        encodes("Xls.WebServices.User", "AcceptClubInvites", &json!({}));
        // Every club notification's struct encodes at the fields' capacities.
        let tag15 = "G".repeat(15);
        for (kind, params) in [
            (
                clubs::NOTE_REQUEST,
                json!({"requestorXuid": 1u64, "requestorGamertag": tag15, "clubId": 12}),
            ),
            (
                clubs::NOTE_INVITATION,
                json!({"sponsorXuid": 1u64, "clubId": 12, "clubName": c.name, "clubTag": c.tag, "sponsorGamertag": tag15}),
            ),
            (
                clubs::NOTE_REMOVE_RECOMMENDATION,
                json!({"recommendingRemovalXuid": 1u64, "recommendingRemovalGamertag": tag15, "recommendedForRemovalXuid": 2u64, "recommendedForRemovalGamertag": tag15, "clubId": 12}),
            ),
            (
                clubs::NOTE_KICK,
                json!({"kickedXuid": 1u64, "kickedGamertag": tag15, "clubName": c.name, "clubTag": c.tag}),
            ),
            (
                clubs::NOTE_DEMOTION,
                json!({"clubName": c.name, "clubTag": c.tag}),
            ),
            (
                clubs::NOTE_ACCEPTED,
                json!({"clubName": c.name, "clubTag": c.tag}),
            ),
            (
                clubs::NOTE_QUIT,
                json!({"exMemberXuid": 1u64, "exMemberGamertag": tag15, "clubName": c.name, "clubTag": c.tag}),
            ),
            (
                clubs::NOTE_NEW_MEMBER,
                json!({"newMemberXuid": 1u64, "newMemberGamertag": tag15, "clubName": c.name, "clubTag": c.tag}),
            ),
            (
                clubs::NOTE_PROMOTION,
                json!({"newRole": 1, "newPromotedXuid": 1u64, "newPromotedGamertag": tag15, "clubName": c.name, "clubTag": c.tag}),
            ),
            (
                clubs::NOTE_NOTICE,
                json!({"senderXuid": 1u64, "senderClubRole": 2, "senderGamertag": tag15,
                "noticeBoardMessage": text_of(&"😀".repeat(300), NOTICE_CHARS, NOTICE_FIELD)}),
            ),
        ] {
            let ty = clubs::note_struct(kind);
            assert_eq!(super::super::inbox::struct_for(i64::from(kind)), Some(ty));
            Schema::fh2()
                .encode(ty, &params, &fi_xtsw::Limits::RESULTS)
                .unwrap_or_else(|e| panic!("{ty}: {e}"));
        }
    }

    #[test]
    fn typed_text_is_cleaned_and_bounded() {
        assert_eq!(tag_of(" abc "), Some("ABC".into()));
        assert_eq!(tag_of("A1b2"), Some("A1B2".into()));
        for bad in ["a", "abcde", "a-b", "ÁBC", ""] {
            assert_eq!(tag_of(bad), None, "{bad}");
        }
        let f = fields_of(
            "  Night\u{200b}  Riders\u{0007} of the very long name club ",
            "line one\nline two",
            &json!(3),
            [&json!(3), &json!(12), &json!(1)],
        )
        .unwrap();
        assert_eq!(f.name, "Night Riders of the very");
        assert_eq!(f.description, "line one line two");
        assert_eq!((f.privacy, f.region, f.attitude, f.interest), (3, 3, 12, 0));
        assert!(fields_of("\u{200b} ", "", &json!(1), [&json!(0); 3]).is_none());
        assert!(fields_of("Ok", "", &json!(5), [&json!(0); 3]).is_none());
        assert!(fields_of("Ok", "", &json!(0), [&json!(0); 3]).is_none());
        // Astral characters count two UTF-16 units: the name stays within its 24.
        let wide = text_of(&"😀".repeat(20), NAME_CHARS, NAME_CHARS);
        assert_eq!(wide.encode_utf16().count(), 24);
        assert!(open_for_membership(1) && open_for_membership(2));
        assert!(!open_for_membership(3) && !open_for_membership(4));
        assert_eq!(keyword(4, forza::Lang::PtBr)["Keyword"], "Ásia");
        assert_eq!(keyword(0, forza::Lang::En), json!({}));
    }

    #[test]
    fn every_club_method_of_the_schema_is_served() {
        let schema = Schema::fh2();
        let mut n = 0;
        for (class, service) in schema.services() {
            let short = class.rsplit('.').next().unwrap();
            let xls = class.starts_with("Xls.");
            for method in service.methods.keys() {
                let ours = served(xls, short, method).is_some();
                let club = matches!(short, "CarClub" | "Club")
                    || (short, method.as_str()) == ("Scoreboard", "GetLadderRows")
                    || (xls, short, method.as_str()) == (true, "User", "AcceptClubInvites");
                assert_eq!(ours, club, "{class}.{method}");
                n += usize::from(ours);
            }
        }
        assert_eq!(n, 29);
    }
}
