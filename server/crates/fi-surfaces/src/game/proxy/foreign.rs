//! Foreign XUIDs (GAME_PLATFORM.md §3a "Foreign XUIDs"; OD-44, register CR-S15): the ticket names
//! one profile, and a proxied request that names another player's XUID is refused with 403
//! `GAME.FOREIGN_XUID` (`details: {host}`, never the XUIDs) after the body's capped read and before
//! dispatch, the same on both deployments.
//!
//! **What a request names** ([`Named::of`]); nothing else of it is read:
//!
//! - the path (percent-decoded, any host): every `xuid(N)`; `/users/me` names the caller and no
//!   XUID;
//! - the query (any host): every `xuid=N` parameter (name in any case, value percent-decoded), the
//!   MPSD `…/sessions?xuid=N` and the gamerpic's `?xuid=N`;
//! - `profile.xboxlive.com` `POST …/profile/settings` (the batch): the body's `userIds`, read as
//!   the handler reads them (decimal strings or numbers), every one of them;
//! - `sessiondirectory.xboxlive.com` `PUT`/`POST …/sessions/{name}` (the session write): each key
//!   of the body's `members` (`me`, or the caller's XUID in decimal, is the caller; any other key,
//!   a reservation `reserve_N` or a member index, is someone else) and each member's
//!   `constants.system.xuid`; and a member path's key (`…/sessions/{name}/members/{key}`).
//!
//! A value that is not a `u64` as the handlers parse it names no XUID (the handlers ignore it
//! too), except a member key, which always names a member: anything the check skips, the handler
//! cannot read as a XUID either. Bodies are only parsed for those two writes, as JSON, within
//! [`super::JSON_BODY`] (the dispatcher's cap for these hosts); an unreadable one names nothing (the
//! handler then answers it as before). Forza RPC bodies (`bin/xtsw`) are not read here: their path
//! names no XUID and their class and method are the deployment's allowlist.
//!
//! **The rule** ([`Named::refused`]): on a **free** request (its effective features are
//! [`super::Features::Free`]: the deployment's, or a ticket that is not `vip` and `hardcore`,
//! [`is_free`], OD-52) any foreign XUID is refused but a name lookup's (the settings batch's
//! `userIds`, a `GET` of `…/users/xuid(N)/profile/settings` and a `GET` of the generated picture
//! `?xuid=N`): those are answered, real gamertags for every ticket (OD-128, which replaced OD-64's
//! world-mates-only rule; `super::live`). On a
//! **Supporter** request, impersonation and writes are refused: a foreign member key or member
//! XUID in a session, and a foreign XUID in the path or query of any method but `GET` and `HEAD`
//! (a presence write for another player); the reads stay served (a `GET` of another player's
//! profile settings, presence, achievements or picture, and the settings batch, which the game
//! `POST`s), as leaderboards, rivals and ghosts need other players' names.

use super::{Features, JSON_BODY};
use crate::errors::ApiError;
use crate::game::request::GameRequest;
use crate::game::ticket::Ticket;
use axum::http::Method;
use fi_protocol::ErrorCode;
use serde_json::{Value as Json, json};

/// The XUIDs one request names, by where they are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Named {
    /// `xuid(N)` in the path and `xuid=N` in the query: the player the request is about.
    pub addressed: Vec<u64>,
    /// The profile-settings batch's `userIds`: identity lookups.
    pub looked_up: Vec<u64>,
    /// MPSD members: keys that are decimal XUIDs (indices included) and members'
    /// `constants.system.xuid`.
    pub members: Vec<u64>,
    /// MPSD member keys that are neither `me` nor a decimal number (`reserve_N`, ...): another
    /// member whoever the caller is.
    pub other_members: usize,
}

/// Whether a request is a free one: its effective features ([`Features::for_ticket`]: the
/// deployment's, narrowed by the ticket's `tier` and `mode`, OD-52).
pub fn is_free(features: Features, ticket: &Ticket) -> bool {
    features.for_ticket(&ticket.claims, ticket.live.cloud) == Features::Free
}

/// 403 `GAME.FOREIGN_XUID` with `details: {host}`.
pub fn refusal(host: &str) -> ApiError {
    ApiError::new(
        ErrorCode::GameForeignXuid,
        "this request names a player other than the session's profile",
    )
    .with_details(json!({ "host": host.chars().take(128).collect::<String>() }))
}

/// The rule for one request, before dispatch.
pub fn check(free: bool, req: &GameRequest<'_>) -> Result<(), ApiError> {
    let named = Named::of(req.method, req.host, req.path, req.query, req.body);
    if named.refused(u64::try_from(req.ticket.xuid).ok(), free, req.method) {
        return Err(refusal(req.host));
    }
    Ok(())
}

/// A XUID as the handlers parse one (`u64::from_str`, so a leading `+` too), surrounding
/// whitespace ignored (stricter than the handlers: more is named, never less).
fn decimal(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

fn json_xuid(v: &Json) -> Option<u64> {
    match v {
        Json::String(s) => decimal(s),
        other => other.as_u64(),
    }
}

/// Every `xuid(N)` of a path, the prefix in any case.
fn path_xuids(path: &str) -> Vec<u64> {
    let lower = path.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut rest = lower.as_str();
    while let Some(i) = rest.find("xuid(") {
        rest = &rest[i + 5..];
        if let Some(end) = rest.find(')')
            && let Some(x) = decimal(&rest[..end])
        {
            out.push(x);
        }
    }
    out
}

/// Every `xuid=N` of a query (as sent), the name in any case, the value percent-decoded.
fn query_xuids(query: Option<&str>) -> Vec<u64> {
    query
        .unwrap_or_default()
        .split('&')
        .filter_map(|kv| {
            let (name, value) = kv.split_once('=')?;
            if !name.eq_ignore_ascii_case("xuid") {
                return None;
            }
            decimal(std::str::from_utf8(&super::target::percent_decode(value)).ok()?)
        })
        .collect()
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// `serviceconfigs/{scid}/sessiontemplates/{t}/sessions/{name}[/…]`.
fn is_session_path(parts: &[&str]) -> bool {
    parts.len() >= 6
        && parts[0].eq_ignore_ascii_case("serviceconfigs")
        && parts[2].eq_ignore_ascii_case("sessiontemplates")
        && parts[4].eq_ignore_ascii_case("sessions")
}

/// A session write of the directory, as the handler matches it (exactly a session path, `PUT`
/// or `POST`).
fn is_session_write(method: &Method, parts: &[&str]) -> bool {
    matches!(*method, Method::PUT | Method::POST) && parts.len() == 6 && is_session_path(parts)
}

fn body_json(body: &[u8]) -> Json {
    if body.is_empty() || body.len() > JSON_BODY {
        return Json::Null;
    }
    serde_json::from_slice(body).unwrap_or(Json::Null)
}

impl Named {
    /// What a request names: `host` lower case, `path` percent-decoded, `query` as sent.
    pub fn of(method: &Method, host: &str, path: &str, query: Option<&str>, body: &[u8]) -> Self {
        let mut named = Named {
            addressed: path_xuids(path),
            ..Named::default()
        };
        named.addressed.extend(query_xuids(query));
        match host {
            "profile.xboxlive.com"
                if *method == Method::POST && path.ends_with("/profile/settings") =>
            {
                if let Some(ids) = body_json(body)["userIds"].as_array() {
                    named.looked_up = ids.iter().filter_map(json_xuid).collect();
                }
            }
            // One user's settings by `xuid(N)`: a name lookup like the batch, not an address.
            "profile.xboxlive.com"
                if matches!(*method, Method::GET | Method::HEAD)
                    && path.ends_with("/profile/settings") =>
            {
                named.looked_up = path_xuids(path);
                named.addressed = query_xuids(query);
            }
            // A grid driver's generated picture (OD-128: every drivatar's settings point at it):
            // a lookup, not an address; the picture is derived from the XUID alone.
            super::gamerpic::HOST if matches!(*method, Method::GET | Method::HEAD) => {
                named.looked_up = std::mem::take(&mut named.addressed);
            }
            "sessiondirectory.xboxlive.com" => {
                let parts = segments(path);
                if parts.len() >= 8
                    && is_session_path(&parts)
                    && parts[6].eq_ignore_ascii_case("members")
                {
                    named.member_key(parts[7]);
                }
                if is_session_write(method, &parts)
                    && let Json::Object(members) = &body_json(body)["members"]
                {
                    for (key, member) in members {
                        named.member_key(key);
                        if let Some(x) = json_xuid(&member["constants"]["system"]["xuid"]) {
                            named.members.push(x);
                        }
                    }
                }
            }
            _ => {}
        }
        named
    }

    fn member_key(&mut self, key: &str) {
        if key == "me" {
            return;
        }
        match decimal(key) {
            Some(x) => self.members.push(x),
            None => self.other_members += 1,
        }
    }

    /// Whether the request is refused for the caller `own` (`None`: a XUID that is not a `u64`,
    /// so every XUID is foreign): free, any foreign XUID; Supporter, a foreign member, or a
    /// foreign XUID addressed by a write (any method but `GET` and `HEAD`).
    pub fn refused(&self, own: Option<u64>, free: bool, method: &Method) -> bool {
        let foreign = |xs: &[u64]| xs.iter().any(|x| Some(*x) != own);
        if self.other_members > 0 || foreign(&self.members) {
            return true;
        }
        let read = matches!(*method, Method::GET | Method::HEAD);
        if free {
            // A name or picture lookup is answered, never refused: real gamertags for every
            // ticket since OD-128 (`live::profile_users`).
            foreign(&self.addressed)
        } else {
            !read && foreign(&self.addressed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u64 = 2_535_100_000_000_001;
    const OTHER: u64 = 2_535_100_000_000_002;
    const PROFILE: &str = "profile.xboxlive.com";
    const PRESENCE: &str = "userpresence.xboxlive.com";
    const MPSD: &str = "sessiondirectory.xboxlive.com";
    const SESSION: &str = "/serviceconfigs/00000000-0000-0000-0000-0000000000aa/sessionTemplates/defaultTemplate_qos/sessions/s1";

    fn named(method: Method, host: &str, path: &str, query: Option<&str>, body: &Json) -> Named {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        Named::of(&method, host, path, query, &bytes)
    }

    /// (free refused, Supporter refused) for the caller `ME`.
    fn verdicts(
        method: Method,
        host: &str,
        path: &str,
        query: Option<&str>,
        body: &Json,
    ) -> (bool, bool) {
        let n = named(method.clone(), host, path, query, body);
        (
            n.refused(Some(ME), true, &method),
            n.refused(Some(ME), false, &method),
        )
    }

    #[test]
    fn paths_queries_and_bodies_name_their_xuids() {
        let n = named(
            Method::GET,
            PRESENCE,
            &format!(
                "/users/XUID({ME})/x/xuid(+{OTHER})/xuid(abc)/xuid(99999999999999999999)/xuid(xuid(7))"
            ),
            Some(&format!(
                "a=1&Xuid={OTHER}&xuid=%32&xuid=&xuid=-1&notxuid=5&xuid=%2B9"
            )),
            &Json::Null,
        );
        // `+N` is a XUID to the handlers' parse, so to the check too.
        assert_eq!(n.addressed, [ME, OTHER, 7, OTHER, 2, 9]);
        assert!(n.looked_up.is_empty() && n.members.is_empty());
        assert_eq!(
            named(
                Method::GET,
                PROFILE,
                "/users/me/profile/settings",
                None,
                &Json::Null
            ),
            Named::default()
        );
        // The batch: strings and numbers, unreadable entries skipped, every entry read.
        let ids: Vec<Json> = std::iter::repeat_n(json!(ME.to_string()), 150)
            .chain([
                json!(OTHER),
                json!("x"),
                json!(null),
                json!(-3),
                json!(" +5 "),
            ])
            .collect();
        let n = named(
            Method::POST,
            PROFILE,
            "/users/batch/profile/settings",
            None,
            &json!({"userIds": ids, "settings": ["Gamertag"]}),
        );
        assert_eq!(n.looked_up.len(), 152);
        assert_eq!(n.looked_up[150..], [OTHER, 5]);
        // Only the batch's body is read: a People GET or another host's body names nothing.
        for (method, host, path) in [
            (Method::GET, PROFILE, "/users/batch/profile/settings"),
            (
                Method::POST,
                PROFILE,
                "/users/me/profile/settings/people/People",
            ),
            (
                Method::POST,
                "client-strings.xboxlive.com",
                "/system/strings/validate",
            ),
            (
                Method::POST,
                PRESENCE,
                "/users/me/devices/current/titles/current",
            ),
        ] {
            let n = named(method, host, path, None, &json!({"userIds": [OTHER]}));
            assert_eq!(n, Named::default(), "{host}{path}");
        }
        // Unreadable or non-object bodies name nothing.
        let n = Named::of(
            &Method::POST,
            PROFILE,
            "/users/batch/profile/settings",
            None,
            b"{oops",
        );
        assert_eq!(n, Named::default());
        let n = named(
            Method::PUT,
            MPSD,
            SESSION,
            None,
            &json!({"members": [1, 2]}),
        );
        assert_eq!(n, Named::default());
    }

    #[test]
    fn mpsd_members_name_the_caller_only_as_me() {
        let n = named(
            Method::PUT,
            MPSD,
            SESSION,
            None,
            &json!({"members": {
                "me": {"constants": {"system": {"xuid": ME.to_string(), "initialize": true}}},
                "reserve_1": {"constants": {"system": {"xuid": OTHER}}},
                "3": null,
                ME.to_string(): {},
            }}),
        );
        assert_eq!(n.other_members, 1);
        let mut members = n.members.clone();
        members.sort_unstable();
        assert_eq!(members, [3, ME, ME, OTHER]);
        // A member path, any method; only a session write's body is read.
        let n = named(
            Method::DELETE,
            MPSD,
            &format!("{SESSION}/members/reserve_2"),
            None,
            &Json::Null,
        );
        assert_eq!(n.other_members, 1);
        let n = named(
            Method::DELETE,
            MPSD,
            &format!("{SESSION}/members/me"),
            None,
            &Json::Null,
        );
        assert_eq!(n, Named::default());
        let n = named(
            Method::GET,
            MPSD,
            SESSION,
            None,
            &json!({"members": {"reserve_1": {}}}),
        );
        assert_eq!(n, Named::default());
        // Only a session's own `members/{key}`: a template or session called "members" is not one.
        let n = named(
            Method::PUT,
            MPSD,
            "/serviceconfigs/s/sessiontemplates/members/sessions/members",
            None,
            &json!({"members": {"me": {}}}),
        );
        assert_eq!(n, Named::default());
    }

    #[test]
    fn free_refuses_any_foreign_xuid_supporters_writes_and_impersonation() {
        let presence = |x: u64| format!("/users/xuid({x})/devices/current/titles/current");
        let batch =
            |ids: &[u64]| json!({"userIds": ids.iter().map(u64::to_string).collect::<Vec<_>>()});
        let sessions = "/serviceconfigs/00000000-0000-0000-0000-0000000000aa/sessiontemplates/defaultTemplate_qos/sessions";
        for (what, got, want) in [
            // Own and `me`: served on both.
            (
                "own batch",
                verdicts(
                    Method::POST,
                    PROFILE,
                    "/users/batch/profile/settings",
                    None,
                    &batch(&[ME]),
                ),
                (false, false),
            ),
            (
                "me settings",
                verdicts(
                    Method::GET,
                    PROFILE,
                    "/users/me/profile/settings",
                    Some("settings=Gamertag"),
                    &Json::Null,
                ),
                (false, false),
            ),
            (
                "own presence write",
                verdicts(
                    Method::POST,
                    PRESENCE,
                    &presence(ME),
                    None,
                    &json!({"state": "active"}),
                ),
                (false, false),
            ),
            (
                "own sessions query",
                verdicts(
                    Method::GET,
                    MPSD,
                    sessions,
                    Some(&format!("xuid={ME}&private=true")),
                    &Json::Null,
                ),
                (false, false),
            ),
            (
                "me member",
                verdicts(
                    Method::PUT,
                    MPSD,
                    SESSION,
                    None,
                    &json!({"members": {"me": {"constants": {"system": {"xuid": ME.to_string()}}}}}),
                ),
                (false, false),
            ),
            (
                "own member key",
                verdicts(
                    Method::PUT,
                    MPSD,
                    SESSION,
                    None,
                    &json!({"members": {ME.to_string(): {}}}),
                ),
                (false, false),
            ),
            (
                "nothing named",
                verdicts(
                    Method::POST,
                    "gameserverds.xboxlive.com",
                    "/titles/1/variants",
                    None,
                    &json!({"locale": "en-US"}),
                ),
                (false, false),
            ),
            // Name lookups are answered on both (OD-64); other reads of another player: free
            // refused, Supporter served.
            (
                "foreign batch",
                verdicts(
                    Method::POST,
                    PROFILE,
                    "/users/batch/profile/settings",
                    None,
                    &batch(&[ME, OTHER]),
                ),
                (false, false),
            ),
            (
                "foreign settings",
                verdicts(
                    Method::GET,
                    PROFILE,
                    &format!("/users/xuid({OTHER})/profile/settings"),
                    None,
                    &Json::Null,
                ),
                (false, false),
            ),
            (
                "foreign presence read",
                verdicts(
                    Method::GET,
                    PRESENCE,
                    &format!("/users/xuid({OTHER})"),
                    None,
                    &Json::Null,
                ),
                (true, false),
            ),
            (
                "foreign achievements",
                verdicts(
                    Method::GET,
                    "achievements.xboxlive.com",
                    &format!("/users/xuid({OTHER})/achievements"),
                    None,
                    &Json::Null,
                ),
                (true, false),
            ),
            // A grid driver's picture is a lookup for every ticket (OD-128).
            (
                "foreign picture",
                verdicts(
                    Method::GET,
                    "gamerpic.fh2online",
                    "/pic",
                    Some(&format!("xuid={OTHER}")),
                    &Json::Null,
                ),
                (false, false),
            ),
            (
                "foreign sessions query",
                verdicts(
                    Method::GET,
                    MPSD,
                    sessions,
                    Some(&format!("xuid={OTHER}")),
                    &Json::Null,
                ),
                (true, false),
            ),
            // Writes and impersonation: refused on both.
            (
                "foreign presence write",
                verdicts(
                    Method::POST,
                    PRESENCE,
                    &presence(OTHER),
                    None,
                    &json!({"state": "active"}),
                ),
                (true, true),
            ),
            (
                "foreign presence delete",
                verdicts(
                    Method::DELETE,
                    PRESENCE,
                    &presence(OTHER),
                    None,
                    &Json::Null,
                ),
                (true, true),
            ),
            (
                "foreign me member",
                verdicts(
                    Method::PUT,
                    MPSD,
                    SESSION,
                    None,
                    &json!({"members": {"me": {"constants": {"system": {"xuid": OTHER}}}}}),
                ),
                (true, true),
            ),
            (
                "reservation",
                verdicts(
                    Method::PUT,
                    MPSD,
                    SESSION,
                    None,
                    &json!({"members": {"me": {}, "reserve_1": {"constants": {"system": {"xuid": OTHER.to_string()}}}}}),
                ),
                (true, true),
            ),
            (
                "member index",
                verdicts(
                    Method::POST,
                    MPSD,
                    SESSION,
                    None,
                    &json!({"members": {"0": null}}),
                ),
                (true, true),
            ),
            (
                "foreign member path",
                verdicts(
                    Method::DELETE,
                    MPSD,
                    &format!("{SESSION}/members/{OTHER}"),
                    None,
                    &Json::Null,
                ),
                (true, true),
            ),
        ] {
            assert_eq!(got, want, "{what}");
        }
        // A caller whose XUID is not a u64 has no own XUID: everything named is foreign.
        let n = named(
            Method::GET,
            PRESENCE,
            &format!("/users/xuid({ME})"),
            None,
            &Json::Null,
        );
        assert!(n.refused(None, true, &Method::GET));
        assert!(!n.refused(None, false, &Method::GET));
    }

    #[test]
    fn the_refusal_names_the_host_only() {
        let e = refusal("profile.xboxlive.com");
        assert_eq!(e.code, ErrorCode::GameForeignXuid);
        assert_eq!(e.code.status(), 403);
        assert_eq!(e.details, Some(json!({"host": "profile.xboxlive.com"})));
        assert!(!e.message.contains(&ME.to_string()));
    }
}
