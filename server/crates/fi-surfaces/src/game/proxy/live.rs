//! The Xbox Live JSON hosts the game needs answered with data (GAME_PLATFORM.md §3a (3)), ported
//! from the Beta's `forza::live_service` (`deprecated/beta/server/src/forza.rs:305-398`) and its
//! gamerpic host (`main.rs:663-676`):
//!
//! - `profile.xboxlive.com` `…/profile/settings` (the batch, and one user by `xuid(…)`): gamertags
//!   of real profiles, a generated name for anyone else (AI drivatars), and an **absolute**
//!   picture URL (an empty one crashes the game); `…/people/People`: the friends list, each friend
//!   with the settings the query names;
//! - `social.xboxlive.com` `…/people`: the friends, with the caller's favorite flag, at most
//!   `maxItems`;
//! - `client-strings.xboxlive.com` `…/strings/validate`: everything passes, `resultCode` a JSON
//!   number (XSAPI throws on any other type and the game reports the text check as off);
//! - `userpresence.xboxlive.com`: the caller's posted state and rich presence stored
//!   (`fi_db::gameplane::set_presence_in`), `DELETE` offline; `…/groups/People` the friends'
//!   presence ([`presence_record`]); one player's presence by `xuid(…)`, real only for the
//!   caller's own profile or a profile of its friend set (`fi_db::friends::in_friend_set_in`: the
//!   ticket's game and mode, both rows of the friendship), anyone else `Offline`, so presence
//!   cannot be read by XUID;
//! - [`super::gamerpic::HOST`]: the generated picture.
//!
//! **Friends** (SER-M3 step 7, GAME_PLATFORM.md §3a "Supporter social"): the three friend lists
//! answer only a request served `Supporter` (the request's effective features, which also read
//! the ticket's `tier` and `mode`, OD-52: [`super::foreign::is_free`]) and only for the caller's own
//! graph (a path of `me` or the ticket's own `xuid(…)`): a free, noob or lapsed ticket, or another
//! player's list (a Supporter's GET of another XUID passes
//! the foreign check), is answered with an empty list, never someone else's friends. The friends
//! are those of `fi_db::friends::friends_in` (the caller's game and mode, visible), oldest
//! friendship first, read in one statement.
//!
//! Only the caller is written: a write path names `me` or the ticket's own `xuid(…)` (another
//! XUID is refused before dispatch, [`super::foreign`]), both mean the ticket's profile, and only
//! `PUT`, `POST` and `DELETE` write. On a free ticket every XUID a request names is the caller's
//! (OD-44), so the settings and the picture answered here are the caller's own, except a name
//! lookup: every ticket's reads of other players' gamertags are answered (boards, rivals, ghosts
//! and, for every ticket, the drivatars need names; OD-128 replaced OD-64's world-mates-only rule
//! for free tickets), at most
//! [`LOOKUPS`] other XUIDs a minute per session (counted in Postgres; past it, the others get the
//! generated name, nothing real). Bodies are parsed leniently (anything unreadable is an
//! empty request), as the Beta did, and bounded: at most [`super::JSON_BODY`] bytes (the
//! dispatcher refuses larger ones), [`MAX_USERS`] users, [`KNOWN_SETTINGS`] deduped,
//! [`MAX_STRINGS`] strings.

use super::Features;
use super::forza::{self, Lang};
use super::gamerpic;
use crate::errors::ApiError;
use crate::game::request::GameRequest;
use crate::state::AppState;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fi_db::friends::{self, MAX_FRIENDS_READ};
use fi_db::gameplane::{self, Owner, Presence};
use fi_db::limits::{self, Hit, Limit};
use fi_db::visibility::Viewer;
use serde_json::{Value as Json, json};

/// Presence older than this reads as offline (the game posts every few seconds while it runs).
pub const PRESENCE_TTL_MS: i64 = 5 * 60 * 1000;

/// The hosts [`answer`] may answer (by path); any other host is not one of these.
pub const HOSTS: [&str; 5] = [
    "profile.xboxlive.com",
    "social.xboxlive.com",
    "client-strings.xboxlive.com",
    "userpresence.xboxlive.com",
    gamerpic::HOST,
];

/// The profile settings answered, the seven the game asks for (every request of the Beta's
/// captures, batch and People alike) and the Beta's `profile_user` knows; any other name is left
/// out of the answer, and each is answered once however often it is asked.
pub const KNOWN_SETTINGS: [&str; 7] = [
    "Gamertag",
    "GameDisplayName",
    "AppDisplayName",
    "AccountTier",
    "Gamerscore",
    "GameDisplayPicRaw",
    "AppDisplayPicRaw",
];

/// Users answered per profile-settings request (XSAPI's batch limit; the game asks for 1 to 7).
pub const MAX_USERS: usize = 100;

/// Other players' XUIDs one session's profile-settings requests look up at most per minute
/// (`rate_limit` key `settings.lookup:<SHA-256 of the session id>`): enough for every board and
/// rival the game shows, too few to walk the sequential XUIDs of the population.
pub const LOOKUPS: Limit = Limit {
    name: "settings.lookup",
    max: 120,
    window_s: 60,
};

/// Strings validated per client-strings request (the game sends 2).
pub const MAX_STRINGS: usize = 100;

/// The settings asked for, in the order asked, known ones only, each once.
fn wanted_settings<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for name in names {
        if let Some(known) = KNOWN_SETTINGS.iter().find(|k| **k == name)
            && !out.contains(known)
        {
            out.push(known);
        }
        if out.len() == KNOWN_SETTINGS.len() {
            break;
        }
    }
    out
}

/// A JSON answer, 200.
pub fn json_response(body: &Json) -> Response {
    let mut r = (StatusCode::OK, body.to_string()).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    r
}

fn path_xuid(path: &str) -> Option<u64> {
    let start = path.find("xuid(")? + 5;
    let end = path[start..].find(')')? + start;
    path[start..end].parse().ok()
}

fn query_param<'q>(query: Option<&'q str>, name: &str) -> Option<&'q str> {
    query?
        .split('&')
        .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
}

/// The text of a rich presence id (the console read it from the title's service config).
pub fn rich_presence_text(id: &str, lang: Lang) -> String {
    match (id, lang) {
        ("RP_SPLASH", Lang::En) => "At the title screen".into(),
        ("RP_SPLASH", Lang::PtBr) => "Na tela inicial".into(),
        ("RP_FREEROAM", Lang::En) => "Exploring southern Europe".into(),
        ("RP_FREEROAM", Lang::PtBr) => "Explorando o sul da Europa".into(),
        ("", Lang::En) => "Playing".into(),
        ("", Lang::PtBr) => "Jogando".into(),
        (other, _) => other
            .trim_start_matches("RP_")
            .replace('_', " ")
            .to_lowercase(),
    }
}

/// Whether a presence reads as online: posted active or inactive within [`PRESENCE_TTL_MS`] of
/// the database clock (the Beta's `is_online`).
pub fn online(p: &Presence) -> bool {
    p.state != "offline" && p.now.millis() - p.updated_at.millis() < PRESENCE_TTL_MS
}

/// One player's userpresence record (the shapes of the Beta's `presence_record` and of its
/// captures): online with the title and its rich presence, else `Offline`.
pub fn presence_record(xuid: u64, presence: Option<&Presence>, lang: Lang) -> Json {
    match presence.filter(|p| online(p)) {
        Some(p) => json!({
            "xuid": xuid.to_string(),
            "state": "Online",
            "devices": [{
                "type": "XboxOne",
                "titles": [{
                    "id": "446059611",
                    "name": "Forza Horizon 2",
                    "placement": "Full",
                    "state": "Active",
                    "lastModified": fi_protocol::instant::format_unix_ms(p.updated_at.millis()).unwrap_or_default(),
                    "activity": { "richPresence": rich_presence_text(&p.rich, lang) },
                }],
            }],
        }),
        None => json!({ "xuid": xuid.to_string(), "state": "Offline" }),
    }
}

/// One social.xboxlive.com person: a friend, the caller's favorite flag, following back (a
/// friendship is mutual).
pub fn social_person(f: &friends::Friend) -> Json {
    json!({
        "xuid": f.xuid.to_string(),
        "isFavorite": f.favorite,
        "isFollowingCaller": true,
        "socialNetworks": [],
    })
}

/// The social.xboxlive.com people reply (`totalCount` the people answered, as the Beta's).
pub fn social_people(people: &[friends::Friend]) -> Json {
    let list: Vec<Json> = people.iter().map(social_person).collect();
    json!({ "totalCount": list.len(), "people": list })
}

/// The friends a social.xboxlive.com people request takes: its `maxItems` (the game sends 100)
/// when above 0, at most [`MAX_FRIENDS_READ`].
pub fn max_people(query: Option<&str>) -> u32 {
    query_param(query, "maxItems")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .map_or(MAX_FRIENDS_READ, |n| n.min(MAX_FRIENDS_READ))
}

/// Whether a friend list request is about the caller's own graph: `me` or the ticket's own XUID
/// in the path.
fn own_graph(path: &str, me: u64) -> bool {
    path.contains("/users/me/") || path_xuid(path) == Some(me)
}

/// The ticket's profile as the friend reads see it.
fn viewer<'a>(req: &'a GameRequest<'_>) -> Viewer<'a> {
    let t = req.ticket;
    Viewer {
        game: t.game(),
        mode: &t.claims.mode,
        profile_id: t.live.profile_id,
        account_id: t.live.account_id,
        xuid: t.xuid,
        session_id: t.live.session_id,
    }
}

/// One profile.xboxlive.com user with the requested settings (already [`wanted_settings`]).
fn profile_user(xuid: u64, known: Option<&str>, wanted: &[&str]) -> Json {
    let tag = known
        .map(str::to_owned)
        .unwrap_or_else(|| forza::generated_gamertag(xuid));
    let settings: Vec<Json> = wanted
        .iter()
        .map(|name| {
            let value = match *name {
                "Gamertag" | "GameDisplayName" | "AppDisplayName" => tag.clone(),
                "AccountTier" => "Gold".into(),
                "Gamerscore" => "0".into(),
                "GameDisplayPicRaw" | "AppDisplayPicRaw" => gamerpic::url_for(xuid),
                _ => String::new(),
            };
            json!({ "id": name, "value": value })
        })
        .collect();
    let id = xuid.to_string();
    json!({ "id": id, "hostId": id, "settings": settings, "isSponsoredUser": false })
}

/// The profile settings of `ids` (at most [`MAX_USERS`]), real gamertags where a profile of the
/// game has that XUID, for every ticket (OD-128: the drivatars of the grid are real players, free
/// tickets included; OD-64's world-mates-only rule is gone). Only XUIDs of the online range are
/// looked up: below it (the game's offline AI drivers) the generated name, no
/// read. The XUIDs other than the caller's count on the session's [`LOOKUPS`]; past it only the
/// caller's own is looked up and the others get the generated name.
async fn profile_users(
    state: &AppState,
    req: &GameRequest<'_>,
    ids: &[u64],
    wanted: &[&str],
) -> Result<Json, ApiError> {
    let me = req.ticket.xuid;
    let mut xuids: Vec<i64> = ids
        .iter()
        .take(MAX_USERS)
        .filter(|x| fi_domain::Xuid::new(**x).is_ok())
        .filter_map(|x| i64::try_from(*x).ok())
        .collect();
    let known = if xuids.is_empty() {
        Vec::new()
    } else {
        let mut conn = state.pool.acquire().await?;
        let others = xuids.iter().filter(|x| **x != me).count();
        if others > 0 {
            let key = format!(
                "{}:{}",
                LOOKUPS.name,
                fi_auth::hex(&fi_auth::sha256(
                    req.ticket.live.session_id.as_bytes().as_slice()
                ))
            );
            let n = i32::try_from(others).unwrap_or(i32::MAX);
            if let Hit::Limited { .. } = limits::hit_n_in(&mut conn, &key, &LOOKUPS, n).await? {
                tracing::info!(session = %req.ticket.live.session_id, "gamertag lookups limited");
                xuids.retain(|x| *x == me);
            }
        }
        if xuids.is_empty() {
            Vec::new()
        } else {
            gameplane::gamertags_in(&mut conn, req.ticket.game(), &xuids).await?
        }
    };
    let users: Vec<Json> = ids
        .iter()
        .take(MAX_USERS)
        .map(|&x| {
            let tag = known
                .iter()
                .find(|(k, _)| u64::try_from(*k).ok() == Some(x))
                .map(|(_, name)| forza::game_tag(name));
            profile_user(x, tag.as_deref(), wanted)
        })
        .collect();
    Ok(json!({ "profileUsers": users }))
}

/// Answers an Xbox Live JSON request on a deployment serving `features`, or `None` when the host
/// and path are not one of these.
pub async fn answer(
    state: &AppState,
    req: &GameRequest<'_>,
    features: Features,
    lang: Lang,
) -> Result<Option<Response>, ApiError> {
    let (host, path, method) = (req.host, req.path, req.method);
    let game = req.ticket.game();
    let me = u64::try_from(req.ticket.xuid).unwrap_or(0);
    // The friend lists: a Supporter's own graph only (the module's "Friends").
    let friends_served = !super::foreign::is_free(features, req.ticket) && own_graph(path, me);
    // The dispatcher already refused a body over JSON_BODY; never parse a larger one.
    let body = || {
        if req.body.len() > super::JSON_BODY {
            return Json::Null;
        }
        serde_json::from_slice::<Json>(req.body).unwrap_or(Json::Null)
    };
    let reply = match host {
        "profile.xboxlive.com" if path.ends_with("/profile/settings/people/People") => {
            // Who the player follows, with the settings in the query.
            let users: Vec<Json> = if friends_served {
                let wanted = wanted_settings(
                    query_param(req.query, "settings")
                        .unwrap_or_default()
                        .split(','),
                );
                let mut conn = state.pool.acquire().await?;
                friends::friends_in(&mut conn, &viewer(req), MAX_FRIENDS_READ)
                    .await?
                    .iter()
                    .map(|f| {
                        let tag = forza::game_tag(&f.name);
                        profile_user(u64::try_from(f.xuid).unwrap_or(0), Some(&tag), &wanted)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            json!({ "profileUsers": users })
        }
        "profile.xboxlive.com" if path.ends_with("/profile/settings") => {
            let (ids, wanted) = if method == Method::POST {
                let b = body();
                let ids: Vec<u64> = b["userIds"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .take(MAX_USERS)
                            .filter_map(|v| match v {
                                Json::String(s) => s.parse().ok(),
                                other => other.as_u64(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let names = b["settings"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                (ids, wanted_settings(names.iter().filter_map(Json::as_str)))
            } else {
                let id = if path.contains("/users/me/") {
                    Some(me)
                } else {
                    path_xuid(path)
                };
                let names = query_param(req.query, "settings").unwrap_or_default();
                (id.into_iter().collect(), wanted_settings(names.split(',')))
            };
            profile_users(state, req, &ids, &wanted).await?
        }
        "social.xboxlive.com" if path.ends_with("/people") => {
            let people = if friends_served {
                let mut conn = state.pool.acquire().await?;
                friends::friends_in(&mut conn, &viewer(req), max_people(req.query)).await?
            } else {
                Vec::new()
            };
            social_people(&people)
        }
        "client-strings.xboxlive.com" if path.ends_with("/strings/validate") => {
            let n = body()["stringstoVerify"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
                .min(MAX_STRINGS);
            let results: Vec<Json> = (0..n)
                .map(|_| json!({ "resultCode": 0, "offendingString": "" }))
                .collect();
            json!({ "verifyStringResult": results })
        }
        // The caller's own presence: only a write (the game POSTs it) changes it; a GET of this
        // path is not served.
        "userpresence.xboxlive.com"
            if path.ends_with("/devices/current/titles/current")
                && matches!(*method, Method::POST | Method::PUT | Method::DELETE) =>
        {
            let owner = Owner {
                game,
                profile_id: req.ticket.live.profile_id,
                session_id: req.ticket.live.session_id,
            };
            let (state_text, rich) = if method == Method::DELETE {
                ("offline", String::new())
            } else {
                let b = body();
                let s = if b["state"].as_str() == Some("inactive") {
                    "inactive"
                } else {
                    "active"
                };
                // Presence ids are the game's own RP_* names.
                let rich: String = b["activity"]["richPresence"]["id"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(40)
                    .collect();
                (s, rich)
            };
            let mut conn = state.pool.acquire().await?;
            gameplane::set_presence_in(&mut conn, owner, state_text, &rich)
                .await
                .map_err(ApiError::from_stored)?;
            json!({})
        }
        // Presence of everyone the player follows (a JSON array).
        "userpresence.xboxlive.com" if path.ends_with("/groups/People") => {
            let records: Vec<Json> = if friends_served {
                let mut conn = state.pool.acquire().await?;
                friends::friends_presence_in(&mut conn, &viewer(req), MAX_FRIENDS_READ)
                    .await?
                    .iter()
                    .map(|(f, p)| {
                        presence_record(u64::try_from(f.xuid).unwrap_or(0), p.as_ref(), lang)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Json::Array(records)
        }
        "userpresence.xboxlive.com" if method == Method::GET && path.ends_with(')') => {
            let Some(x) = path_xuid(path) else {
                return Ok(None);
            };
            // Real presence only for the caller's own profile or one of its friend set; anyone
            // else reads offline, whoever they are.
            let found = match i64::try_from(x) {
                Ok(xuid) => {
                    let mut conn = state.pool.acquire().await?;
                    let shown = x == me
                        || (!super::foreign::is_free(features, req.ticket)
                            && friends::in_friend_set_in(&mut conn, &viewer(req), xuid).await?);
                    if shown {
                        gameplane::presence_in(&mut conn, game, &req.ticket.claims.mode, xuid)
                            .await?
                    } else {
                        None
                    }
                }
                Err(_) => None,
            };
            presence_record(x, found.as_ref(), lang)
        }
        // The picture of the XUID the URL names (profile settings always name one): the answer
        // of a ticket-authenticated request, so caches keep it private.
        gamerpic::HOST if method == Method::GET => {
            let xuid: u64 = query_param(req.query, "xuid")
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| ApiError::invalid("xuid", "the picture's xuid is required"))?;
            let mut r = (StatusCode::OK, gamerpic::identicon(xuid, gamerpic::SIZE)).into_response();
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=86400"),
            );
            return Ok(Some(r));
        }
        _ => return Ok(None),
    };
    Ok(Some(json_response(&reply)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_users_always_carry_an_absolute_picture() {
        let wanted = wanted_settings(["Gamertag", "GameDisplayPicRaw", "Other"].into_iter());
        let u = profile_user(2_535_100_000_000_001, Some("Tester#00042"), &wanted);
        assert_eq!(u["id"], "2535100000000001");
        assert_eq!(u["settings"][0]["value"], "Tester#00042");
        assert!(
            u["settings"][1]["value"]
                .as_str()
                .unwrap()
                .starts_with("http://gamerpic.fh2online/pic?xuid=")
        );
        assert_eq!(
            u["settings"].as_array().unwrap().len(),
            2,
            "an unknown setting is left out"
        );
        let ai = profile_user(2_535_100_000_000_009, None, &["Gamertag"]);
        assert_eq!(
            ai["settings"][0]["value"],
            forza::generated_gamertag(2_535_100_000_000_009)
        );
    }

    /// The review's 20,000 empty names (or any flood of names) answer at most the seven known
    /// settings, each once, in the order first asked.
    #[test]
    fn wanted_settings_are_known_and_deduplicated() {
        let flood = std::iter::repeat_n("", 20_000)
            .chain(std::iter::repeat_n("Gamertag", 20_000))
            .chain(["AccountTier", "Nope", "Gamertag"]);
        assert_eq!(wanted_settings(flood), ["Gamertag", "AccountTier"]);
        let all = wanted_settings(
            KNOWN_SETTINGS
                .iter()
                .rev()
                .chain(KNOWN_SETTINGS.iter())
                .copied(),
        );
        assert_eq!(all.len(), 7);
        assert_eq!(all[0], "AppDisplayPicRaw");
        assert!(wanted_settings("".split(',')).is_empty());
        assert_eq!(
            wanted_settings("Gamertag,,Gamertag,GameDisplayPicRaw".split(',')),
            ["Gamertag", "GameDisplayPicRaw"]
        );
    }

    #[test]
    fn paths_queries_and_presence_texts() {
        assert_eq!(
            path_xuid("/users/xuid(2535100000000001)/groups/People"),
            Some(2_535_100_000_000_001)
        );
        assert_eq!(path_xuid("/users/me/people"), None);
        assert_eq!(
            query_param(Some("a=1&settings=Gamertag,GameDisplayPicRaw"), "settings"),
            Some("Gamertag,GameDisplayPicRaw")
        );
        assert_eq!(query_param(None, "settings"), None);
        assert_eq!(
            rich_presence_text("RP_SPLASH", Lang::PtBr),
            "Na tela inicial"
        );
        assert_eq!(rich_presence_text("RP_CAR_MEET", Lang::En), "car meet");
    }

    /// The friend lists' shapes, as the Beta's captures show them: social people
    /// (`{totalCount, people: [{xuid, isFavorite, isFollowingCaller, socialNetworks}]}`) and the
    /// presence records (online with one device and one title, or `{xuid, state: Offline}`).
    #[test]
    fn friend_lists_have_the_shapes_the_game_reads() {
        let friend = |n: i64, favorite: bool| friends::Friend {
            profile_id: uuid::Uuid::now_v7(),
            xuid: 2_535_100_000_000_000 + n,
            name: format!("Friend{n}"),
            favorite,
        };
        let v = social_people(&[friend(1, true), friend(2, false)]);
        assert_eq!(v["totalCount"], 2);
        assert_eq!(
            v["people"][0],
            json!({"xuid": "2535100000000001", "isFavorite": true, "isFollowingCaller": true,
                "socialNetworks": []})
        );
        assert_eq!(v["people"][1]["isFavorite"], false);
        assert_eq!(social_people(&[]), json!({"totalCount": 0, "people": []}));
        assert_eq!(max_people(Some("maxItems=100")), 100);
        assert_eq!(max_people(Some("maxItems=0")), MAX_FRIENDS_READ);
        assert_eq!(max_people(Some("maxItems=99999")), MAX_FRIENDS_READ);
        assert_eq!(max_people(Some("maxItems=x")), MAX_FRIENDS_READ);
        assert_eq!(max_people(None), MAX_FRIENDS_READ);
        let at = |ms: i64| fi_domain::UnixMs::from_millis(ms);
        let posted = |state: &str, ago: i64| Presence {
            state: state.into(),
            rich: "RP_FREEROAM".into(),
            updated_at: at(1_792_000_000_000),
            now: at(1_792_000_000_000 + ago),
        };
        let on = presence_record(
            2_535_100_000_000_001,
            Some(&posted("active", 1000)),
            Lang::En,
        );
        assert_eq!(on["state"], "Online");
        assert_eq!(on["xuid"], "2535100000000001");
        let title = &on["devices"][0]["titles"][0];
        assert_eq!(
            title["activity"]["richPresence"],
            "Exploring southern Europe"
        );
        assert_eq!(title["lastModified"], "2026-10-14T17:46:40.000000Z");
        assert!(online(&posted("inactive", PRESENCE_TTL_MS - 1)));
        for p in [posted("active", PRESENCE_TTL_MS), posted("offline", 1000)] {
            assert!(!online(&p));
            assert_eq!(
                presence_record(2_535_100_000_000_001, Some(&p), Lang::PtBr),
                json!({"xuid": "2535100000000001", "state": "Offline"})
            );
        }
        assert_eq!(
            presence_record(2_535_100_000_000_002, None, Lang::En),
            json!({"xuid": "2535100000000002", "state": "Offline"})
        );
        // Only the caller's own graph.
        let me = 2_535_100_000_000_001;
        assert!(own_graph("/users/me/profile/settings/people/People", me));
        assert!(own_graph("/users/xuid(2535100000000001)/people", me));
        assert!(own_graph("/users/xuid(2535100000000001)/groups/People", me));
        assert!(!own_graph("/users/xuid(2535100000000002)/people", me));
        assert!(!own_graph(
            "/users/xuid(2535100000000002)/groups/People",
            me
        ));
        assert!(!own_graph("/users/people", me));
    }
}
