//! Forza Motorsport 5's proxied requests (GAME_PLATFORM.md §3a "Forza Motorsport 5"; OD-206,
//! CR-S97): its own allowlist over its own schema, so nothing of FH2's Forza RPC, catalog, gates or
//! free roam can answer it, and FH2's path is unchanged.
//!
//! Every RPC here answers from its name alone but `User.Logon2`, which records the logon and reads
//! the stored checkpoint (`deprecated/beta/notes/local-docs/re/fm5-response-spec.md` §1), and the
//! career and scores of M7 (CR-S104, GAME_PLATFORM.md "Career and scores (M7)"):
//! `OnlineProfile.UpdateAll` and `UpdateAllTables` stored per career under game `fm5` with the
//! tables checkpoint and the career's `profile_progress`, `Scoreboard.SubmitScore` keeping the
//! career's best per key and `GetBestScores` answering it. None of FH2's integrity gates, guard
//! scores or progression facts.
//! The Xbox Live JSON hosts and the stubs are the game-neutral [`super::live`] and [`super::stubs`]
//! as a free request sees them. `sessiondirectory`, `gameserverds` and `gameserverms` are the
//! free-roam handler with FM5's session profile (CR-S99, FM5_LOBBIES.md §3): private races on
//! official Worlds. `momatch` is not served (no SmartMatch, car meet or Road Trip for FM5), and
//! `Hopper.GetList` answers the one private-race hopper.

use super::forza::{self, Lang, Player, Reply, XlsError};
use super::rpc::{self, class_key};
use super::{FREE_ROAM_HOSTS, Features, JSON_BODY, live, not_served, stubs, target};
use crate::errors::ApiError;
use crate::game::freeroam::{self, FM5_SESSION, MOMATCH};
use crate::game::request::GameRequest;
use crate::state::AppState;
use axum::response::Response;
use fi_db::fm5::{self as store, Best, ScoreKey, Submitted};
use fi_db::gameplane::{self, Owner};
use fi_xtsw::{Schema, decode_params};
use serde_json::{Value as Json, json};
use sqlx::PgConnection;

/// The game's code.
pub const GAME: &str = fi_domain::FM5;

/// The Xbox media catalog host, asked for its field list at logon.
const EDS_HOST: &str = "eds.xboxlive.com";

/// How a served method is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// Records the logon ([`logon2`]).
    Logon,
    /// Stores the online profile ([`stored`]).
    Profile,
    /// Stores the profile's tables and their checkpoint ([`stored`]).
    Tables,
    /// Keeps the career's best score ([`stored`]).
    Submit,
    /// [`forza::permissions_table`].
    Permissions,
    /// The inbox, empty.
    Notifications,
    /// Xls error 6: no transaction reply is kept yet.
    Reconcile,
    /// The private-race hopper ([`hoppers`]).
    Hoppers,
    /// The career's stored best for the key, else the game's own reset ([`best_scores`]).
    BestScores,
    /// The `_Results` type with every field at its default.
    Empty,
}

/// The allowlist: `(class key, method, answer)`, every method the LAN lab saw the game call. A 404
/// here is the hook's 503, which the lab never answered, so nothing it called is left out; the empty
/// replies are what the lab's own server sent. `GetFieldsString` and `GetUnlockedTitleAchievements`
/// are JSON requests of the game ([`EDS_HOST`], the achievements stub), never this path.
const SERVED: &[(&str, &str, Answer)] = &[
    ("Forza.WebServices.User", "Logon2", Answer::Logon),
    (
        "Forza.WebServices.User",
        "GetPermissionsTable",
        Answer::Permissions,
    ),
    ("Forza.WebServices.User", "GetContentAd", Answer::Empty),
    ("Forza.WebServices.User", "GetOwnedProducts", Answer::Empty),
    (
        "Forza.WebServices.User",
        "RetrieveOnlineCredits",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.User",
        "GetForzaAssetCounts",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.Drivatar",
        "GetDrivatarsForCache",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.Drivatar",
        "GetDrivatarInformationForTrack",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.Drivatar",
        "SaveDrivatarInformation",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.DailyCreditBonus",
        "GetPayout",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.OnlineProfile",
        "UpdateAll",
        Answer::Profile,
    ),
    (
        "Forza.WebServices.OnlineProfile",
        "UpdateAllTables",
        Answer::Tables,
    ),
    ("Forza.WebServices.GameplayLog", "Write", Answer::Empty),
    (
        "Forza.WebServices.Storefront",
        "GetFileKeywords",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.Storefront",
        "GetMyFileList",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.Scoreboard",
        "GetBestScores",
        Answer::BestScores,
    ),
    (
        "Forza.WebServices.Scoreboard",
        "GetTimeTrials",
        Answer::Empty,
    ),
    ("Forza.WebServices.Scoreboard", "GetRival", Answer::Empty),
    (
        "Forza.WebServices.Scoreboard",
        "SubmitScore",
        Answer::Submit,
    ),
    (
        "Forza.WebServices.Scoreboard",
        "DownloadFile",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.ContentVersioning",
        "CreateVersionedTuneFromStructs",
        Answer::Empty,
    ),
    ("Forza.WebServices.Hopper", "GetList", Answer::Hoppers),
    ("Forza.WebServices.Hopper", "GetPlayerSkills", Answer::Empty),
    // R11, a fresh career (2026-11-07): the first car's livery, the race's drivatar read and the
    // player card; acknowledged, nothing stored. `CreateVersionedLivery` stays a 404, which the game
    // took: a default id 0 would name a livery no one holds (FM5 liveries are not kept).
    (
        "Forza.WebServices.OnlineProfile",
        "UpdateLivery",
        Answer::Empty,
    ),
    (
        "Forza.WebServices.Drivatar",
        "GetDrivatarInformationForRace",
        Answer::Empty,
    ),
    ("Forza.WebServices.User", "SetPlayerCardInfo", Answer::Empty),
    ("Xls.WebServices.User", "SetFriends", Answer::Empty),
    (
        "Xls.WebServices.Notifications",
        "RetrieveForUser",
        Answer::Notifications,
    ),
    (
        "Xls.WebServices.Notifications",
        "StartNewSession",
        Answer::Empty,
    ),
    (
        "Xls.WebServices.Transaction",
        "Reconcile",
        Answer::Reconcile,
    ),
];

fn served(class: &str, method: &str) -> Option<(&'static str, Answer)> {
    let key = class_key(class);
    SERVED
        .iter()
        .find(|(c, m, _)| *c == key && *m == method)
        .map(|(c, _, answer)| (*c, *answer))
}

/// The body limit of a request, from what its headers name, or its refusal before the body is read
/// (as [`super::body_limit`], over this module's allowlist).
pub fn body_limit(
    host: &str,
    class: Option<&str>,
    method: Option<&str>,
) -> Result<usize, ApiError> {
    match (class, method) {
        (Some(class), Some(method)) => served(class, method)
            .map(|(key, _)| rpc::body_cap(key, method))
            .ok_or_else(|| not_served(host, Some(class_key(class)), Some(method))),
        // No SmartMatch for FM5 (FM5_LOBBIES.md §3).
        _ if host == MOMATCH => Err(not_served(host, None, None)),
        _ if FREE_ROAM_HOSTS.contains(&host)
            || live::HOSTS.contains(&host)
            || stubs::for_host(host).is_some()
            || host == EDS_HOST =>
        {
            Ok(JSON_BODY)
        }
        _ => Err(not_served(host, None, None)),
    }
}

/// `User.Logon2`'s reply (response spec §1). Three things the game's code punishes silently: every
/// permission present (an empty table fails each gated feature later), valid discount datetimes
/// (the codec's default; an empty one fails the reply and the game retries forever) and at most 6
/// notification categories (FH2's 7 exceed the capacity). The checkpoint is the stored one, else 0,
/// never the client's `transactionCount`: above 0 the game deletes local change tracking rows.
fn logon2(player: &Player, tables_tran_id: Option<i32>) -> Json {
    json!({
        "responseData": {
            "isProfileValid": true,
            "lastOnlineProfileUpdateAllTablesTranId": tables_tran_id.unwrap_or(0),
            "permissions": forza::permissions(),
            "featureAreasBanMask": 0,
            "userFlags": 0,
            "upsellOfferId": 0,
            "upsellText": "",
            "vipOfferIds": [],
            "userData": {
                "qwXuid": player.xuid,
                "wzGamerTag": player.gamertag,
                "flags": 0,
                "clanId": 0,
                "clanTag": "",
                "roleInClan": 0,
                "currentCareerLevel": player.card.career_level,
                "currentBadgeId": player.card.badge_id,
                "currentPlayerTitleId": player.card.player_title_id,
            },
            "notificationNonce": 1,
            "notificationPollIntervalSeconds": 30,
            "defaultFriendsPollIntervalSeconds": 60,
            "maxFriendsPollIntervalSeconds": 300,
            "notificationCategories": [],
            "gameplayLogConfig": { "Enabled": false, "FlushInterval": 0, "Throttle": "" },
            "carsToUnshareFromGame": [],
            "defaultTimeoutMs": 30000,
            "defaultContentTimeoutMs": 40000,
            "defaultInGameTimeoutMs": 30000,
            "makeDiscounts": [],
            "carDiscounts": [],
            "makePartsDiscounts": [],
            "carPartsDiscounts": [],
        }
    })
}

/// `Hopper.GetList` (FM5_LOBBIES.md §3): one category with one hopper, the lab's working reply
/// (`tools/fm5-probe/mocks/xtsw/Hopper.GetList.json`) with FM5's GSI set as `cloudGsiSetPattern`,
/// named in the player's language. It fills the hopper screen; private races are made from there.
fn hoppers(lang: Lang) -> Json {
    let (hopper, category_text, hopper_text) = match lang {
        Lang::En => (
            "Private race",
            "Races on Infinity",
            "Invite your friends to a private race.",
        ),
        Lang::PtBr => (
            "Corrida privada",
            "Corridas no Infinity",
            "Convide seus amigos para uma corrida privada.",
        ),
    };
    json!({
        "hopperCategories": [{
            "id": 1, "name": "INFINITY", "description": category_text, "imageId": 0, "hopperStartIndex": 0,
        }],
        "hoppers": [{
            "id": 1,
            "name": hopper,
            "description": hopper_text,
            "cloudGsiSetPattern": FM5_SESSION.gsi_set,
            "matchHopperMapping": "defaultTemplate",
            "playerCount": FM5_SESSION.max_members,
            "payoutModifier": 1.0,
            "hideTrueSkill": true,
            "maxPartySize": 1,
            "gameOptionCount": 1,
            "intermissionTimeSeconds": 30,
            "votingTimeSeconds": 20,
            "launchTimeSeconds": 15,
            "minPlayersToLaunch": 1,
        }],
    })
}

/// The reply of a method answered from its name alone (and the request's language).
fn pure(answer: Answer, lang: Lang) -> Option<Reply> {
    match answer {
        Answer::Logon | Answer::Profile | Answer::Tables | Answer::Submit | Answer::BestScores => {
            None
        }
        Answer::Hoppers => Some(Ok(hoppers(lang))),
        Answer::Permissions => Some(Ok(forza::permissions_table())),
        Answer::Notifications => Some(Ok(json!({ "results": [], "nextRetrieveSeconds": 300 }))),
        Answer::Reconcile => Some(Err(XlsError {
            code: 6,
            message: "unknown transaction",
        })),
        Answer::Empty => Some(Ok(json!({}))),
    }
}

/// The game's own per-race reset of a time best (response spec §11.1, R10): `f32::MAX`, not clean,
/// so any finish, clean or dirty, submits. An all-zero time would be a best no finish can lower,
/// silently blocking every dirty finish.
const NO_TIME: Best = Best {
    score: f32::MAX,
    is_clean: false,
};
/// The reset of a drift best: 0.0, not clean.
const NO_DRIFT: Best = Best {
    score: 0.0,
    is_clean: false,
};

/// `Scoreboard.GetBestScores` (response spec §11.6): the career's stored best time and drift score
/// for the key, each else the game's own reset ([`NO_TIME`], [`NO_DRIFT`]).
fn best_scores(time: Option<Best>, drift: Option<Best>) -> Json {
    let one = |b: Best| json!({ "score": b.score, "isClean": b.is_clean });
    json!({
        "timeScore": one(time.unwrap_or(NO_TIME)),
        "driftScore": one(drift.unwrap_or(NO_DRIFT)),
    })
}

/// The key GetBestScores' parameters name, or SubmitScore's `data` (the same six fields); `None`
/// when one is out of its wire range.
fn score_key(p: &Json) -> Option<ScoreKey> {
    let int = |f: &str| p[f].as_i64();
    Some(ScoreKey {
        scoreboard_type: i32::try_from(int("scoreboardType")?).ok()?,
        game_scoreboard_id: i32::try_from(int("gameScoreboardId")?).ok()?,
        track_id: i16::try_from(int("trackId")?).ok()?,
        car_id: i16::try_from(int("carId")?).ok()?,
        car_class_id: u8::try_from(int("carClassId")?).ok()?,
        car_drive_type_id: u8::try_from(int("carDriveTypeId")?).ok()?,
    })
}

/// SubmitScore's parameters as a score to keep; `None` when the type is neither a time nor a drift
/// score, the key is out of range or the score is not finite and not negative (nothing stored).
fn submitted(params: &Json, profile_id: uuid::Uuid, score_id: uuid::Uuid) -> Option<Submitted<'_>> {
    let score_type = i32::try_from(params["type"].as_i64()?).ok()?;
    if score_type != store::TIME && score_type != store::DRIFT {
        return None;
    }
    let d = &params["data"];
    let score = d["score"].as_f64()? as f32;
    // A time of 0 would be a best no later dirty time could lower (response spec §11.2): never kept.
    if !score.is_finite() || score < 0.0 || (score_type == store::TIME && score <= 0.0) {
        return None;
    }
    Some(Submitted {
        profile_id,
        score_type,
        key: score_key(d)?,
        score,
        is_clean: d["isClean"].as_bool().unwrap_or(false),
        score_id,
        data: d.as_object()?,
    })
}

/// `Scoreboard.SubmitScore`'s reply: the request's `scoreId` and no file upload (response spec
/// §11.5: `uploadFiles` true would ask for a ghost, which FM5 does not keep).
fn submit_reply(score_id: uuid::Uuid) -> Json {
    json!({ "scoreId": score_id.to_string(), "uploadFiles": false })
}

/// A method answered from the database, in the request's one transaction.
async fn stored(
    conn: &mut PgConnection,
    req: &GameRequest<'_>,
    how: Answer,
    params: Json,
    score_id: uuid::Uuid,
) -> Result<Json, ApiError> {
    let owner = Owner {
        game: req.ticket.game(),
        profile_id: req.ticket.live.profile_id,
        session_id: req.ticket.live.session_id,
    };
    Ok(match how {
        Answer::Logon => {
            let tables = gameplane::record_logon_in(
                conn,
                owner,
                forza::logon_card(&params),
                forza::logon_title_version(&params),
            )
            .await?;
            let row = gameplane::player_in(conn, owner.game, owner.profile_id)
                .await?
                .ok_or_else(ApiError::unavailable)?;
            logon2(&rpc::player_of(row), tables)
        }
        // The merged profile (a section the game marks unchanged keeps the stored one), then the
        // career's numbers from it.
        Answer::Profile => {
            let update = match params {
                Json::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            gameplane::merge_profile_in(conn, owner, &update)
                .await
                .map_err(ApiError::from_stored)?;
            store::sync_progress_in(conn, owner.profile_id).await?;
            forza::updated()
        }
        // Rows merged by key per table, `delete<Table>` keys removed, then the checkpoint the next
        // Logon2 answers and the career's car count.
        Answer::Tables => {
            let checkpoint = params["transactionId"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok());
            for t in forza::table_updates(params) {
                gameplane::apply_table_in(conn, owner, &t.name, &t.rows, &t.deletes)
                    .await
                    .map_err(ApiError::from_stored)?;
            }
            if let Some(id) = checkpoint {
                gameplane::set_tables_checkpoint_in(conn, owner, id).await?;
            }
            store::sync_progress_in(conn, owner.profile_id).await?;
            forza::updated()
        }
        Answer::Submit => {
            match submitted(&params, owner.profile_id, score_id) {
                Some(s) => {
                    store::submit_score_in(conn, &s)
                        .await
                        .map_err(ApiError::from_stored)?;
                }
                None => tracing::warn!(
                    session = %owner.session_id,
                    "FM5 SubmitScore out of range, not stored"
                ),
            }
            submit_reply(score_id)
        }
        Answer::BestScores => match score_key(&params) {
            Some(key) => {
                let (time, drift) = store::best_scores_in(conn, owner.profile_id, key).await?;
                best_scores(time, drift)
            }
            None => best_scores(None, None),
        },
        Answer::Permissions
        | Answer::Notifications
        | Answer::Reconcile
        | Answer::Hoppers
        | Answer::Empty => return Err(ApiError::unavailable()),
    })
}

/// Answers one proxied request of an FM5 ticket; the caller checked the ticket, the headers, the
/// body against [`body_limit`] and the foreign XUIDs.
pub async fn answer(
    state: &AppState,
    req: &GameRequest<'_>,
    class: Option<&str>,
    method: Option<&str>,
    request_id: &str,
    lang: Lang,
) -> Result<Response, ApiError> {
    if let (Some(class), Some(method)) = (class, method) {
        let (key, how) = served(class, method)
            .ok_or_else(|| not_served(req.host, Some(class_key(class)), Some(method)))?;
        let schema = Schema::fm5();
        let reply = match pure(how, lang) {
            Some(reply) => reply,
            None => {
                let params = match decode_params(schema, key, method, req.body) {
                    Ok(v) => v.into_json(),
                    // M7's stored methods worked answered by name before (T1-T3): a body that does
                    // not decode keeps that answer (the no-best reset, an empty acknowledgement)
                    // instead of a 400 the game never saw, and nothing is stored.
                    Err(e) if how != Answer::Logon => {
                        tracing::warn!(rpc = %format!("{key}.{method}"), error = %e, "FM5 body does not decode; answered by name");
                        let fallback = if how == Answer::BestScores {
                            best_scores(None, None)
                        } else {
                            json!({})
                        };
                        return Ok(rpc::xtsw_response(rpc::envelope(
                            schema,
                            key,
                            method,
                            Ok(fallback),
                        )?));
                    }
                    Err(e) => {
                        return Err(ApiError::invalid(
                            "body",
                            format!("the body does not decode as {key}.{method}: {e}"),
                        ));
                    }
                };
                // The request's id, stable across a replay (GAME_PLATFORM.md §3a), as FH2's.
                let score_id = target::fact_id(
                    req.ticket.live.session_id,
                    &format!("{key}.{method}"),
                    request_id,
                );
                let mut tx = state.pool.begin().await?;
                let reply = stored(&mut tx, req, how, params, score_id).await?;
                tx.commit().await?;
                Ok(reply)
            }
        };
        return Ok(rpc::xtsw_response(rpc::envelope(
            schema, key, method, reply,
        )?));
    }
    // FM5's session profile: the free-roam handler picks it by the ticket's game.
    if let Some(r) = freeroam::handle(state, req).await {
        return Ok(r);
    }
    if let Some(r) = live::answer(state, req, Features::Free, lang).await? {
        return Ok(r);
    }
    if req.host == EDS_HOST && req.path.starts_with("/media/") && req.path.ends_with("/fields") {
        return Ok(live::json_response(&json!({
            "ImpressionGuid": "00000000-0000-0000-0000-000000000000",
            "Fields": "id.name.Availabilities",
        })));
    }
    if let Some(stub) = stubs::for_host(req.host) {
        return Ok(live::json_response(
            &serde_json::from_str(stub).unwrap_or_default(),
        ));
    }
    Err(not_served(req.host, None, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_db::gameplane::Card;
    use fi_xtsw::{decode_results, encode_results};

    const USER: &str = "Forza.WebServices.User, Forza.WebServices";

    fn player() -> Player {
        Player {
            xuid: 2_535_000_000_000_001,
            gamertag: "Tester".into(),
            card: Card {
                career_level: 7,
                badge_id: 2,
                player_title_id: 3,
            },
        }
    }

    /// Encoded and decoded with FM5's schema: the field names are the schema's, no field is
    /// misspelled away into a default.
    fn round_trip(reply: &Json) -> Json {
        let s = Schema::fm5();
        let bytes = encode_results(s, "Forza.WebServices.User", "Logon2", reply).unwrap();
        decode_results(s, "Forza.WebServices.User", "Logon2", &bytes)
            .unwrap()
            .into_json()
    }

    #[test]
    fn every_served_method_is_in_the_schema_and_encodes() {
        let s = Schema::fm5();
        for (class, method, answer) in SERVED {
            assert!(s.method(class, method).is_some(), "{class}.{method}");
            for lang in [Lang::En, Lang::PtBr] {
                if let Some(Ok(results)) = pure(*answer, lang) {
                    encode_results(s, class, method, &results)
                        .unwrap_or_else(|e| panic!("{class}.{method}: {e}"));
                }
            }
        }
    }

    #[test]
    fn logon2_carries_what_the_game_needs() {
        let r = round_trip(&logon2(&player(), None))["responseData"].clone();
        assert_eq!(r["isProfileValid"], true);
        let permissions = r["permissions"].as_array().unwrap();
        assert_eq!(permissions.len(), 256);
        assert!(
            permissions
                .iter()
                .enumerate()
                .all(|(i, p)| p["methodId"] == i as u64
                    && p["isEnabled"] == true
                    && p["access"] == 0)
        );
        assert_eq!(r["featureAreasBanMask"], 0);
        assert_eq!(r["upsellOfferId"], 0);
        assert_eq!(r["notificationNonce"], 1);
        assert_eq!(r["notificationCategories"], json!([]));
        assert_eq!(r["userData"]["qwXuid"], 2_535_000_000_000_001_u64);
        assert_eq!(r["userData"]["wzGamerTag"], "Tester");
        assert_eq!(r["userData"]["currentCareerLevel"], 7);
        assert_eq!(r["userData"]["currentBadgeId"], 2);
        assert_eq!(r["userData"]["currentPlayerTitleId"], 3);
        for all in ["allCarDiscount", "allPartsDiscount"] {
            let dates: Vec<&str> = r[all]
                .as_object()
                .unwrap()
                .values()
                .filter_map(Json::as_str)
                .collect();
            assert_eq!(dates.len(), 2, "{all}: {}", r[all]);
            assert!(dates.iter().all(|d| fi_xtsw::valid_datetime(d)), "{all}");
        }
    }

    /// Encoded and decoded with FM5's schema: every field the lab sent survives, by name.
    #[test]
    fn the_hopper_list_is_one_private_race_hopper_in_the_players_language() {
        let s = Schema::fm5();
        let (class, method) = ("Forza.WebServices.Hopper", "GetList");
        for (lang, name) in [(Lang::En, "Private race"), (Lang::PtBr, "Corrida privada")] {
            let sent = hoppers(lang);
            let bytes = encode_results(s, class, method, &sent).unwrap();
            let back = decode_results(s, class, method, &bytes)
                .unwrap()
                .into_json();
            assert_eq!(back["hopperCategories"].as_array().unwrap().len(), 1);
            assert_eq!(back["hopperCategories"][0]["name"], "INFINITY");
            assert_eq!(back["hopperCategories"][0]["hopperStartIndex"], 0);
            let hoppers = back["hoppers"].as_array().unwrap();
            assert_eq!(hoppers.len(), 1);
            let h = &hoppers[0];
            assert_eq!(h["name"], name);
            assert_eq!(
                h["cloudGsiSetPattern"],
                "00000000-0000-0000-0000-00000000f503"
            );
            assert_eq!(h["matchHopperMapping"], "defaultTemplate");
            assert_eq!(h["playerCount"], 16);
            assert_eq!(h["minPlayersToLaunch"], 1);
            for (field, value) in sent["hoppers"][0].as_object().unwrap() {
                assert_eq!(&h[field], value, "{field}");
            }
        }
    }

    #[test]
    fn best_scores_answer_the_games_own_reset_not_zero() {
        let s = Schema::fm5();
        let (class, method) = ("Forza.WebServices.Scoreboard", "GetBestScores");
        let bytes = encode_results(s, class, method, &best_scores(None, None)).unwrap();
        let back = decode_results(s, class, method, &bytes)
            .unwrap()
            .into_json();
        assert_eq!(
            back["timeScore"]["score"].as_f64().unwrap() as f32,
            f32::MAX
        );
        assert_eq!(back["timeScore"]["isClean"], false);
        assert_eq!(back["driftScore"]["score"], 0.0);
        assert!(
            SERVED
                .iter()
                .any(|(c, m, a)| *c == class && *m == method && *a == Answer::BestScores)
        );
    }

    /// The race OD-206 observed (synthetic numbers of its shape), encoded and decoded with FM5's
    /// schema as the game sends it.
    fn submit_params(score_type: i32, score: f32, clean: bool) -> Json {
        let s = Schema::fm5();
        let (class, method) = ("Forza.WebServices.Scoreboard", "SubmitScore");
        let sent = json!({"type": score_type, "data": {
            "scoreboardType": 3, "gameScoreboardId": -1, "score": score, "trackId": 222,
            "carId": 1426, "garageId": 1, "carDriveTypeId": 2, "carClassId": 1,
            "carPerformanceIndex": 0.5, "isClean": clean, "hadPenalty": !clean,
            "numberOfCarsPassed": 0, "versionedTuneId": 0,
        }});
        let bytes = fi_xtsw::encode_params(s, class, method, &sent).unwrap();
        decode_params(s, class, method, &bytes).unwrap().into_json()
    }

    #[test]
    fn submit_score_decodes_to_its_key_and_the_reply_encodes() {
        let profile = uuid::Uuid::from_u128(1);
        let id = uuid::Uuid::from_u128(2);
        let params = submit_params(0, 42.29, false);
        let s = submitted(&params, profile, id).unwrap();
        assert_eq!(s.score_type, store::TIME);
        assert_eq!(
            s.key,
            ScoreKey {
                scoreboard_type: 3,
                game_scoreboard_id: -1,
                track_id: 222,
                car_id: 1426,
                car_class_id: 1,
                car_drive_type_id: 2,
            }
        );
        assert_eq!(s.score, 42.29_f32);
        assert!(!s.is_clean);
        assert_eq!(s.data["hadPenalty"], true);
        // A drift score; a type the game never sends and a non-finite score are not stored.
        assert_eq!(
            submitted(&submit_params(1, 1500.0, true), profile, id)
                .unwrap()
                .score_type,
            store::DRIFT
        );
        assert!(submitted(&submit_params(2, 1.0, true), profile, id).is_none());
        let mut nan = params.clone();
        nan["data"]["score"] = Json::Null;
        assert!(submitted(&nan, profile, id).is_none());
        let mut negative = params.clone();
        negative["data"]["score"] = json!(-1.0);
        assert!(submitted(&negative, profile, id).is_none());

        let (class, method) = ("Forza.WebServices.Scoreboard", "SubmitScore");
        let s = Schema::fm5();
        let bytes = encode_results(s, class, method, &submit_reply(id)).unwrap();
        let back = decode_results(s, class, method, &bytes)
            .unwrap()
            .into_json();
        assert_eq!(back["scoreId"], id.to_string());
        assert_eq!(back["uploadFiles"], false);
    }

    #[test]
    fn best_scores_answer_the_stored_best_by_the_requests_key() {
        let s = Schema::fm5();
        let (class, method) = ("Forza.WebServices.Scoreboard", "GetBestScores");
        let sent = json!({"scoreboardType": 3, "gameScoreboardId": -1, "trackId": -5,
            "carId": 1426, "carClassId": 255, "carDriveTypeId": 2});
        let bytes = fi_xtsw::encode_params(s, class, method, &sent).unwrap();
        let params = decode_params(s, class, method, &bytes).unwrap().into_json();
        let key = score_key(&params).unwrap();
        assert_eq!((key.track_id, key.car_class_id), (-5, 255));
        let reply = best_scores(
            Some(Best {
                score: 41.5,
                is_clean: true,
            }),
            None,
        );
        let bytes = encode_results(s, class, method, &reply).unwrap();
        let back = decode_results(s, class, method, &bytes)
            .unwrap()
            .into_json();
        assert_eq!(back["timeScore"]["score"].as_f64().unwrap() as f32, 41.5);
        assert_eq!(back["timeScore"]["isClean"], true);
        // The drift half has no best: the game's own reset.
        assert_eq!(back["driftScore"]["score"], 0.0);
        assert_eq!(back["driftScore"]["isClean"], false);
        assert!(score_key(&json!({"trackId": 40000})).is_none());
    }

    #[test]
    fn the_sync_and_scores_are_stored_not_answered_by_name() {
        for (class, method, answer) in [
            (
                "Forza.WebServices.OnlineProfile",
                "UpdateAll",
                Answer::Profile,
            ),
            (
                "Forza.WebServices.OnlineProfile",
                "UpdateAllTables",
                Answer::Tables,
            ),
            (
                "Forza.WebServices.Scoreboard",
                "SubmitScore",
                Answer::Submit,
            ),
            (
                "Forza.WebServices.Scoreboard",
                "GetBestScores",
                Answer::BestScores,
            ),
        ] {
            assert_eq!(served(class, method).map(|s| s.1), Some(answer), "{method}");
            assert!(pure(answer, Lang::En).is_none(), "{method}");
        }
    }

    #[test]
    fn the_checkpoint_is_the_stored_one_or_zero() {
        let id = "lastOnlineProfileUpdateAllTablesTranId";
        assert_eq!(logon2(&player(), None)["responseData"][id], 0);
        assert_eq!(logon2(&player(), Some(9))["responseData"][id], 9);
    }

    #[test]
    fn only_the_allowlist_is_served_and_fh2_methods_are_not() {
        assert_eq!(
            body_limit("h", Some(USER), Some("Logon2")).unwrap(),
            rpc::RPC_BODY
        );
        for (class, method) in [
            (USER, "Logon"),
            ("Forza.WebServices.Scoreboard", "GetRows"),
            (
                "Forza.WebServices.LiveServices",
                "GetUnlockedTitleAchievements",
            ),
            ("No.Such.Class", "Logon2"),
        ] {
            assert_eq!(
                body_limit("h", Some(class), Some(method))
                    .unwrap_err()
                    .code
                    .code(),
                "GAME.NOT_SERVED",
                "{class}.{method}"
            );
        }
        assert_eq!(
            body_limit("sessiondirectory.xboxlive.com", None, None).unwrap(),
            JSON_BODY
        );
        assert_eq!(body_limit(EDS_HOST, None, None).unwrap(), JSON_BODY);
        for host in [
            "gameserverds.xboxlive.com",
            "gameserverms.xboxlive.com",
            "sessiondirectory.xboxlive.com",
        ] {
            assert_eq!(body_limit(host, None, None).unwrap(), JSON_BODY, "{host}");
        }
        assert_eq!(
            body_limit(MOMATCH, None, None).unwrap_err().code.code(),
            "GAME.NOT_SERVED"
        );

        assert!(body_limit("example.com", None, None).is_err());
    }
}
