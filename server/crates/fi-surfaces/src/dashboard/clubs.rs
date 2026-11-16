//! `GET /internal/dashboard/v1/clubs` and `/clubs/{tag}` (DASHBOARD_API.md §6, PD-4, OD-169): the
//! play dashboard's clubs, as the in-game club screens, and their assembly for the hourly
//! snapshot ([`super::snapshot`]).
//!
//! Where each field comes from (no second definition of anything the game computes):
//!
//! - `members`, `xp`, `rank`, the leader and the champion: `fi_db::clubs::public_clubs_in`, the
//!   statement behind the game's ClubData (`clubs_in`): visible members, the XP they earned in the
//!   club, the XP rank among all the mode's clubs (private ones included; ties share it), the
//!   visible leader, and the champion = the visible member with the highest career level;
//! - the roster: `fi_db::clubs::mode_members_in`, the same visible-member set, with each member's
//!   Discord display, website listing and XP total (`fi_db::gameplane::xp_total_sql`; its level is
//!   `fi_gamedb::gates::shown_level`, as a player card's);
//! - `region`, `attitude`, `interest`: the game's keyword ids, named from the game plane's keyword
//!   table ([`crate::game::proxy::universe::clubs::KEYWORDS`]: "South America" → `south-america`);
//!   an id outside it is sent as its number;
//! - `role`: 1 `leader`, 2 `officer` (the game's CoLeader), 3 `member`; `privacy`: 1 Open →
//!   `open`, 2 OpenToFriends → `request` (friends join at once, everyone else asks), 3 InviteOnly
//!   → `invite`; 4 Private never appears;
//! - `records`: the boards (every §3 family) whose game #1 is a member, from the boards' grouped
//!   ranking;
//! - not derivable from stored data, so absent: the club `level` and the `weekly` ladder (the game
//!   plane keeps the XP earned while in the club, not per week).
//!
//! Persons (leader, champion, members, record holders) appear only when listed on the website
//! (§1); the club's totals stay the game's. Refs (§5.1) only in snapshot files.

use super::boards::{self, FAMILIES, Fam, family_read, in_unit};
use super::{Dash, DashQuery, RefKey, game, query, required_mode, unknown_game};
use crate::errors::ApiError;
use crate::game::proxy::universe::clubs::{
    CATEGORY_ATTITUDE, CATEGORY_INTEREST, CATEGORY_REGION, KEYWORDS,
};
use crate::views::at;
use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use fi_db::clubs::{self, Club, ModeMember};
use fi_db::dashboard::{self, boards as db_boards};
use fi_db::identity::IdentityError;
use fi_gamedb::gates::shown_level;
use fi_protocol::ErrorCode;
use fi_protocol::dashboard::{
    ClubCard, ClubChampion, ClubList, ClubMember, ClubPage, ClubPerson, ClubRecord,
};
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

/// A board's game #1, listed on the website (a club record's candidate).
#[derive(Clone, Debug)]
pub struct BoardTopRow {
    /// The family.
    pub fam: Fam,
    /// The catalog board.
    pub board: boards::Board,
    /// The holder's profile.
    pub profile_id: Uuid,
    /// The holder's plain name.
    pub name: String,
    /// The score as stored.
    pub score: f64,
}

/// `role` of a member's role id.
pub fn role_id(role: i16) -> String {
    match role {
        clubs::ROLE_LEADER => "leader".into(),
        clubs::ROLE_COLEADER => "officer".into(),
        clubs::ROLE_MEMBER => "member".into(),
        other => other.to_string(),
    }
}

/// `privacy` of a club's privacy; `None` for a private club (never shown).
pub fn privacy_id(privacy: i16) -> Option<String> {
    match privacy {
        clubs::PRIVACY_OPEN => Some("open".into()),
        clubs::PRIVACY_FRIENDS => Some("request".into()),
        clubs::PRIVACY_INVITE_ONLY => Some("invite".into()),
        clubs::PRIVACY_PRIVATE => None,
        other => Some(other.to_string()),
    }
}

/// A keyword's stable id ("South America" → `south-america`), from the game plane's table; an id
/// outside the table's `category` is its number; 0 (none) is absent.
pub fn keyword_id(id: i16, category: i64) -> Option<String> {
    if id == 0 {
        return None;
    }
    Some(
        KEYWORDS
            .iter()
            .find(|k| k.0 == i64::from(id) && k.1 == category)
            .map_or_else(
                || id.to_string(),
                |k| k.2.to_ascii_lowercase().replace(' ', "-"),
            ),
    )
}

fn level_of(xp: Option<i64>) -> Option<u32> {
    xp.and_then(shown_level).and_then(|l| u32::try_from(l).ok())
}

fn ref_of(refs: Option<&RefKey>, profile: Uuid) -> Option<String> {
    refs.map(|k| k.of(profile))
}

/// A club's card; `members` its roster by profile (listing and Discord display of the leader).
pub fn club_card(
    club: &Club,
    members: &HashMap<Uuid, &ModeMember>,
    refs: Option<&RefKey>,
) -> ClubCard {
    let leader = club
        .leader
        .as_ref()
        .and_then(|l| members.get(&l.profile_id))
        .filter(|m| m.listed)
        .map(|m| ClubPerson {
            name: m.name.clone(),
            discord_name: m.discord_name.clone(),
            avatar: m.avatar,
            ref_: ref_of(refs, m.profile_id),
        });
    ClubCard {
        tag: club.tag.clone(),
        name: club.name.clone(),
        members: u64::try_from(club.members).unwrap_or(0),
        xp: club.xp,
        rank: u64::try_from(club.xp_rank).unwrap_or(0),
        level: None,
        region: keyword_id(club.region, CATEGORY_REGION),
        attitude: keyword_id(club.attitude, CATEGORY_ATTITUDE),
        interest: keyword_id(club.interest, CATEGORY_INTEREST),
        created_at: at(club.created_at),
        leader,
    }
}

/// A club's page: its card, roster (`roster`, the club's visible members in role-then-XP order),
/// champion and the records among `tops`. `None` for a private club.
pub fn club_page(
    club: &Club,
    roster: &[ModeMember],
    tops: &[BoardTopRow],
    refs: Option<&RefKey>,
) -> Option<ClubPage> {
    let privacy = privacy_id(club.privacy)?;
    let by_profile: HashMap<Uuid, &ModeMember> = roster.iter().map(|m| (m.profile_id, m)).collect();
    let champion = club
        .champion
        .as_ref()
        .and_then(|c| by_profile.get(&c.profile_id))
        .filter(|m| m.listed)
        .and_then(|m| {
            Some(ClubChampion {
                name: m.name.clone(),
                level: level_of(m.xp)?,
                ref_: ref_of(refs, m.profile_id),
            })
        });
    let members = roster
        .iter()
        .filter(|m| m.listed)
        .map(|m| ClubMember {
            name: m.name.clone(),
            discord_name: m.discord_name.clone(),
            avatar: m.avatar,
            ref_: ref_of(refs, m.profile_id),
            role: role_id(m.role),
            xp: m.club_xp,
            joined_at: at(m.joined_at),
            level: level_of(m.xp),
        })
        .collect();
    let records = tops
        .iter()
        .filter(|t| by_profile.get(&t.profile_id).is_some_and(|m| m.listed))
        .map(|t| ClubRecord {
            family: t.fam.slug().to_owned(),
            board: t.board.summary.board.clone(),
            title: t.board.summary.title.clone(),
            holder: t.name.clone(),
            score: in_unit(t.board.unit, t.score),
            unit: t.board.unit,
            ref_: ref_of(refs, t.profile_id),
        })
        .collect();
    Some(ClubPage {
        card: club_card(club, &by_profile, refs),
        description: (!club.description.is_empty()).then(|| club.description.clone()),
        champion,
        privacy,
        members,
        weekly: None,
        records,
    })
}

/// Every board's listed #1 of `game` and `mode` (one grouped statement per family).
pub async fn board_tops_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    month: i64,
) -> Result<Vec<BoardTopRow>, IdentityError> {
    let mut out = Vec::new();
    for fam in FAMILIES {
        let catalog = boards::family_boards(fam, month);
        let keys: Vec<i32> = catalog.iter().map(|b| b.key).collect();
        let tops = db_boards::family_tops_in(conn, game, mode, &family_read(fam, &keys)).await?;
        for (key, profile_id, name, score) in tops {
            if let Some(b) = catalog.iter().find(|b| b.key == key) {
                out.push(BoardTopRow {
                    fam,
                    board: b.clone(),
                    profile_id,
                    name,
                    score,
                });
            }
        }
    }
    Ok(out)
}

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::ClubsNotFound, "no such club")
}

/// `GET /internal/dashboard/v1/clubs?game&mode`: 200 [`ClubList`] (every non-private club of the
/// mode, by XP rank); 400 `REQUEST.INVALID` `mode` or `game`.
pub(super) async fn list(
    State(s): State<Dash>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<ClubList>, ApiError> {
    let q = query(q)?;
    let mode = required_mode(&q)?;
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    if !dashboard::game_known_in(&mut conn, &game).await? {
        return Err(unknown_game());
    }
    let list = clubs::public_clubs_in(&mut conn, &game, &mode, None).await?;
    let roster = clubs::mode_members_in(&mut conn, &game, &mode, None).await?;
    let by_profile: HashMap<Uuid, &ModeMember> = roster.iter().map(|m| (m.profile_id, m)).collect();
    Ok(Json(ClubList {
        rows: list
            .iter()
            .map(|c| club_card(c, &by_profile, None))
            .collect(),
    }))
}

/// A tag as the URL may carry one (2 to 4 ASCII letters or digits, any case).
fn tag_of(tag: &str) -> Option<String> {
    ((2..=4).contains(&tag.len()) && tag.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| tag.to_ascii_uppercase())
}

/// `GET /internal/dashboard/v1/clubs/{tag}?game&mode`: 200 [`ClubPage`]; 400 `REQUEST.INVALID`
/// `mode` or `game`; 404 `CLUBS.NOT_FOUND` for an unknown, malformed or private club alike.
pub(super) async fn page(
    State(s): State<Dash>,
    Path(tag): Path<String>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<ClubPage>, ApiError> {
    let q = query(q)?;
    let mode = required_mode(&q)?;
    let tag = tag_of(&tag).ok_or_else(not_found)?;
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    if !dashboard::game_known_in(&mut conn, &game).await? {
        return Err(unknown_game());
    }
    let club = clubs::public_clubs_in(&mut conn, &game, &mode, Some(&tag))
        .await?
        .pop()
        .ok_or_else(not_found)?;
    let roster = clubs::mode_members_in(&mut conn, &game, &mode, Some(club.id)).await?;
    let tops = board_tops_in(&mut conn, &game, &mode, boards::current_month()).await?;
    club_page(&club, &roster, &tops, None)
        .map(Json)
        .ok_or_else(not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_unknown_ones_stay_numbers() {
        assert_eq!(role_id(1), "leader");
        assert_eq!(role_id(2), "officer");
        assert_eq!(role_id(3), "member");
        assert_eq!(role_id(9), "9");
        assert_eq!(privacy_id(1).as_deref(), Some("open"));
        assert_eq!(privacy_id(2).as_deref(), Some("request"));
        assert_eq!(privacy_id(3).as_deref(), Some("invite"));
        assert_eq!(privacy_id(4), None);
        assert_eq!(
            keyword_id(1, CATEGORY_REGION).as_deref(),
            Some("south-america")
        );
        assert_eq!(keyword_id(6, CATEGORY_REGION).as_deref(), Some("africa"));
        assert_eq!(
            keyword_id(12, CATEGORY_ATTITUDE).as_deref(),
            Some("competitive")
        );
        assert_eq!(
            keyword_id(23, CATEGORY_INTEREST).as_deref(),
            Some("off-road")
        );
        assert_eq!(keyword_id(0, CATEGORY_REGION), None);
        assert_eq!(keyword_id(77, CATEGORY_REGION).as_deref(), Some("77"));
        // An id of another category is not that category's word.
        assert_eq!(keyword_id(12, CATEGORY_REGION).as_deref(), Some("12"));
        // Every keyword id is a URL-safe slug.
        for k in KEYWORDS {
            let id = keyword_id(k.0 as i16, k.1).unwrap();
            assert!(
                id.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                "{id}"
            );
        }
        assert_eq!(tag_of("ab1").as_deref(), Some("AB1"));
        for bad in ["a", "abcde", "a-b", "", "AB%"] {
            assert!(tag_of(bad).is_none(), "{bad}");
        }
    }
}
