//! The leaderboards and the XP ledger (SER-M3 step 6; GAME_PLATFORM.md §3a "Supporter universe"):
//! `Scoreboard.SubmitScore`, `SubmitXpScore`, `GetBestScore`, `GetRows`, `GetRowsAnthem`,
//! `GetTimeTrials`, ported from the Beta's `submit_score`, `submit_xp`, `best_score`,
//! `rows_anthem`, `club_xp_rows` and `score_rows` (`deprecated/beta/server/src/forza.rs`), over
//! [`fi_db::scores`]. The reply builders are pure (each in the schema test below); the handlers
//! read and write on the request's transaction.
//!
//! - **SubmitScore**: a score that is not finite, not above 0 or above 1e9 is Xls error 1 ("invalid
//!   score"), nothing recorded. On a gated ticket ([`rpc::gated`]: anything but noob) a score the
//!   gate refuses ([`fi_gamedb::gates::score`]: a car id the game cannot have; a time the car,
//!   or for a car-pack car the game's fastest, could not have done on the track; a speed over the
//!   game's fastest stock car with the margin; points over their type's plausibility ceiling) is
//!   refused the same way, recorded as a `score` fact with `accepted: false` and struck
//!   (`race_time`), once. Otherwise the `score` fact is recorded and the board keeps the profile's
//!   best; `{scoreId, uploadFiles}`: `scoreId` is the fact id, `uploadFiles` whether this score is
//!   now the profile's best on its event (the game then uploads its ghost). A replayed request id
//!   writes nothing and answers the same.
//!   A stored best on a record board (speed traps and zones, race times on a track) that takes
//!   the board's #1 from another player appends a `speed_record` / `route_record` feed row on
//!   the same transaction ([`fi_db::discord::record_in`], CORE_API.md §5a "Records", CR-S64); an
//!   ungated (Noob) score only when the gate would have accepted it.
//! - **SubmitXpScore**: the report clamped ([`fi_gamedb::gates::xp_of_report`]); on a gated
//!   ticket over the hourly allowance ([`fi_gamedb::gates::xp_rate`]) it is struck (`xp_rate`) and
//!   not kept; else the ledger row (none for 0 XP; the fact is kept, so a replay stays one).
//!   Always `{weeklyTierAchieved: false, weeklyTierInfo: {}}`.
//! - **GetBestScore**: the profile's best on exactly that event and track.
//! - **GetRows**, **GetRowsAnthem**: track boards are the whole route (any event of the track,
//!   each profile's best), boards without a track name their event in `pivotId`; views 0 global,
//!   1 near me, 2 friends, 3 club, 4 friends and club, 5 after the event (global); rows keep their
//!   real position; views 2 and 4 show the caller and their friends ([`super::social`], read only
//!   for those views), views 3 and 4 the caller's Car Club's visible members (CR-S70, read only
//!   for those views and club boards), and club boards (`isClubBoard`) keep only the club's
//!   members of the view (the Beta's). The club XP board (scoreboard type 8, score type 4,
//!   `isClubBoard`) is [`super::clubs::xp_board`]. `maxResults` is clamped to 0..500, `startAt`
//!   from 1.
//! - **GetTimeTrials**: this month's Monthly Rivals events ([`super::rivals`], S2),
//!   answered without a database step ([`without_db`]).
//!
//! Rows show others a class and PI the game can hold ([`shown_class`], [`shown_pi`]).

use super::super::forza::{self, Lang, Player, Reply, Served, XlsError};
use super::super::rpc::{self, Ctx};
use crate::errors::ApiError;
use fi_db::scores::{
    self, Assists, BoardFilter, BoardKey, BoardPage, BoardQuery, ScoreRow, Submission,
};
use fi_domain::UnixMs;
use fi_gamedb::{Catalog, gates};
use serde_json::{Map, Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// The leaderboard views, as the game labels them (IDS_Global, IDS_NearMe, IDS_Friends,
/// IDS_Club, IDS_FriendsAndClub; 5 is the board shown right after an event).
pub const VIEW_NEAR_ME: i64 = 1;
/// Friends only.
pub const VIEW_FRIENDS: i64 = 2;
/// The caller's club only.
pub const VIEW_CLUB: i64 = 3;
/// Friends and club.
pub const VIEW_FRIENDS_AND_CLUB: i64 = 4;

/// How `(xls, class, method)` is answered; `None`: not this module's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    match (xls, short, method) {
        (
            false,
            "Scoreboard",
            "SubmitScore" | "SubmitXpScore" | "GetBestScore" | "GetRows" | "GetRowsAnthem"
            | "GetTimeTrials",
        ) => Some(Served::Store),
        _ => None,
    }
}

/// A stored method of this module, on the request's transaction.
pub(crate) async fn stored(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    short: &str,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    match method {
        "SubmitScore" => submit_score(conn, ctx, &params).await,
        "SubmitXpScore" => submit_xp(conn, ctx, &params).await,
        "GetBestScore" => best_score(conn, ctx, &params).await,
        "GetRows" => rows(conn, ctx, &params, false).await,
        "GetRowsAnthem" => rows(conn, ctx, &params, true).await,
        "GetTimeTrials" => {
            let catalog = Catalog::fh2();
            let now = UnixMs::now_system();
            let tracks: Vec<i32> =
                super::rivals::month_events(catalog, super::rivals::month_of(now))
                    .iter()
                    .filter_map(|e| i32::try_from(e.track).ok())
                    .collect();
            let raced = scores::tracks_with_rivals_in(
                conn,
                &ctx.viewer(),
                &[MONTHLY_BOARD, super::ghosts::ROUTE_BOARD],
                &tracks,
            )
            .await?;
            Ok(Ok(time_trials(ctx.lang, &params, |t| {
                i32::try_from(t).is_ok_and(|t| raced.contains(&t))
            })))
        }
        _ => Err(ctx.not_served(short, method)),
    }
}

/// A method of this module that reads and writes nothing (`rpc::answer` opens no transaction for
/// it): its reply.
pub(crate) fn without_db(
    _xls: bool,
    _short: &str,
    _method: &str,
    _lang: Lang,
    _params: &Json,
) -> Option<Reply> {
    // GetTimeTrials reads which tracks have real rivals: no method of this module is pure now.
    None
}

/// An `i32` field of the parameters (the schema's integers fit; -1 when absent).
pub(crate) fn int(v: &Json, field: &str) -> i32 {
    v[field]
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(-1)
}

/// Xls error 1 "invalid score" (the Beta's answer to a refused score).
pub fn invalid_score() -> XlsError {
    XlsError {
        code: 1,
        message: "invalid score",
    }
}

/// A SubmitScore as stored, `None` when its score is not one a board takes
/// ([`scores::valid_score`]). A known car pack's car sent with FM's R class (7) or a raw PI above 1
/// (an FM5 race car before the game recalculates it) is stored as X with a raw PI of 1.0, the class
/// and PI the game gives it ([`pack_class`]).
pub fn submission(params: &Json) -> Option<Submission> {
    let mut s = submission_as_sent(params)?;
    (s.car_class_id, s.car_pi) = pack_class(Catalog::fh2(), s.car_id, s.car_class_id, s.car_pi);
    Some(s)
}

/// FH2's X class id and its highest raw PI.
const X_CLASS: i16 = 6;

/// The class and raw PI a score keeps: a known car pack's car reported as R (7) or above a raw PI
/// of 1 is X at 1.0 (the pack catalog's X 999, `tools/gamedata/extract_fh2_packs.py`); any other
/// car or report as sent.
pub fn pack_class(catalog: &Catalog, car: i32, class: i16, pi: f32) -> (i16, f32) {
    let pack = catalog
        .car(i64::from(car))
        .is_some_and(|c| c.pack.is_some());
    if pack && (class == X_CLASS + 1 || pi > 1.0) {
        (X_CLASS, pi.min(1.0))
    } else {
        (class, pi)
    }
}

fn submission_as_sent(params: &Json) -> Option<Submission> {
    let data = &params["data"];
    let score = data["score"].as_f64().filter(|s| scores::valid_score(*s))?;
    let flag = |k: &str| data[k].as_bool().unwrap_or(false);
    let key = BoardKey {
        scoreboard_type: int(data, "scoreboardType"),
        score_type: int(params, "type"),
        game_scoreboard_id: int(data, "gameScoreboardId"),
        track_id: int(data, "trackId"),
    };
    Some(Submission {
        key,
        score,
        car_id: int(data, "carId"),
        car_class_id: data["carClassId"]
            .as_i64()
            .and_then(|c| i16::try_from(c).ok())
            .unwrap_or(0),
        car_pi: data["carPerformanceIndex"]
            .as_f64()
            .filter(|v| v.is_finite())
            .unwrap_or(0.0) as f32,
        assists: Assists {
            is_clean: flag("isClean"),
            used_stm: flag("usedSTM"),
            used_tcs: flag("usedTCS"),
            used_abs: flag("usedABS"),
            used_auto_brake: flag("usedAutoBrake"),
            used_auto_shifting: flag("usedAutoShifting"),
            used_clutch: flag("usedClutch"),
        },
        data: data.clone(),
    })
}

/// The `scoreType` of a `score` fact (EVENTS.md §3.3): type 1 a time, 5 and 6 speeds (speed traps
/// and zones, metres per second), anything else points.
pub fn score_unit(score_type: i32) -> &'static str {
    match score_type {
        1 => "time_ms",
        5 | 6 => "speed_mps",
        _ => "points",
    }
}

/// The `score` fact of a submission (EVENTS.md §3.3): `board` is `scoreboardType:scoreType`,
/// `eventKey` the `gameScoreboardId`, `value` milliseconds for a time, else the score.
pub fn score_fact(s: &Submission, accepted: bool) -> Map<String, Json> {
    let value = if scores::lower_is_better(s.key.score_type) {
        Json::from((s.score * 1000.0).round() as i64)
    } else {
        Json::from(s.score)
    };
    let mut m = Map::new();
    m.insert(
        "board".into(),
        format!("{}:{}", s.key.scoreboard_type, s.key.score_type).into(),
    );
    m.insert("eventKey".into(), s.key.game_scoreboard_id.into());
    m.insert("trackId".into(), s.key.track_id.into());
    m.insert("carId".into(), s.car_id.into());
    m.insert("scoreType".into(), score_unit(s.key.score_type).into());
    m.insert("value".into(), value);
    m.insert("accepted".into(), accepted.into());
    m
}

/// SubmitScore's reply.
pub fn submitted(score_id: Uuid, upload_files: bool) -> Json {
    json!({ "scoreId": score_id.to_string(), "uploadFiles": upload_files })
}

async fn submit_score(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
) -> Result<Reply, ApiError> {
    let Some(s) = submission(params) else {
        return Ok(Err(invalid_score()));
    };
    // OD-112 follow-up: a modded session's score is answered as posted and never recorded.
    if crate::game::modded_result_skipped(&ctx.req.ticket.live, "score") {
        return Ok(Ok(submitted(ctx.fact_id(), false)));
    }
    let refusal = if rpc::gated(ctx) {
        gate(&s).err()
    } else {
        None
    };
    let viewer = ctx.viewer();
    let score_id = ctx.fact_id();
    let mut fact = score_fact(&s, refusal.is_none());
    fact.insert(
        "refusal".into(),
        refusal.as_ref().map_or(Json::Null, |r| r.rule.into()),
    );
    // OD-178 follow-up: a stats board's value over its ceiling is refused without a strike and
    // recorded once per profile and board (the boards send running totals: every later activity
    // would send it again).
    if let Some(r) = refusal
        .as_ref()
        .filter(|r| r.rule == gates::STATS_CEILING_RULE)
    {
        let board = format!("{}:{}", s.key.scoreboard_type, s.key.score_type);
        if !fi_db::game::refused_score_recorded_in(
            conn,
            viewer.profile_id,
            r.rule,
            &board,
            i64::from(s.key.game_scoreboard_id),
            i64::from(s.key.track_id),
        )
        .await?
        {
            rpc::record(conn, ctx, "score", fact).await?;
        }
        return Ok(Err(invalid_score()));
    }
    // Recorded first: a replayed request id writes nothing (no second strike, no second row).
    let new = rpc::record(conn, ctx, "score", fact).await?;
    if let Some(r) = refusal {
        if new {
            fi_db::integrity::strike_rpc_in(conn, viewer.profile_id, r.rule, &r.reason).await?;
        }
        return Ok(Err(invalid_score()));
    }
    let best = if new {
        // CR-S64: a record board's #1 before the score (the board stays locked to this request).
        let scope = fi_db::discord::record_scope(&s.key);
        let before = match &scope {
            Some(scope) => fi_db::discord::record_leader_in(conn, &viewer, scope).await?,
            None => None,
        };
        let best = scores::submit_in(conn, &viewer, &s, score_id)
            .await
            .map_err(ApiError::from_stored)?;
        let catalog = Catalog::fh2();
        if let Some(scope) = scope.filter(|sc| {
            best && sc.real(|t| catalog.track(i64::from(t)).is_some()) && announceable(ctx, &s)
        }) {
            record(conn, &viewer, &scope, before, &s).await?;
        }
        best
    } else {
        scores::is_best_in(conn, &viewer, score_id).await?
    };
    Ok(Ok(submitted(score_id, best)))
}

/// Whether an accepted score may be announced (CR-S64): a gated (Hardcore) score passed the gate
/// already; an ungated one only when the same gate would have accepted it (it is kept either way).
fn announceable(ctx: &Ctx<'_>, s: &Submission) -> bool {
    rpc::gated(ctx) || gate(s).is_ok()
}

/// The score gate of a gated (Hardcore) ticket (GAME_PLATFORM.md §3a "Supporter universe"):
/// [`gates::score`] (the car, the time, speed or points), then [`gates::score_class`] (OD-178: the
/// class and PI, with a Monthly Rivals event's own class on its board). Every refusal is rule
/// `race_time`.
pub fn gate(s: &Submission) -> Result<(), gates::Refusal> {
    let catalog = Catalog::fh2();
    let car = i64::from(s.car_id);
    gates::score(
        catalog,
        i64::from(s.key.scoreboard_type),
        i64::from(s.key.score_type),
        car,
        i64::from(s.key.track_id),
        s.score,
    )?;
    let required = (s.key.scoreboard_type == MONTHLY_BOARD)
        .then(|| super::rivals::event_class(catalog, i64::from(s.key.game_scoreboard_id)))
        .flatten();
    gates::score_class(
        catalog,
        i64::from(s.car_class_id),
        f64::from(s.car_pi),
        required,
    )
}

/// The record row of a stored best (CORE_API.md §5a "Records"), with the catalog's car and route
/// names.
async fn record(
    conn: &mut PgConnection,
    viewer: &fi_db::visibility::Viewer<'_>,
    scope: &fi_db::discord::RecordScope,
    before: Option<fi_db::discord::Leader>,
    s: &Submission,
) -> Result<(), ApiError> {
    let catalog = Catalog::fh2();
    let car_name = catalog.car(i64::from(s.car_id)).map(|c| c.name());
    let route_name = (scope.board == fi_db::discord::RecordBoard::Route)
        .then(|| catalog.track(i64::from(s.key.track_id)))
        .flatten()
        .map(|t| t.name("en").to_owned());
    fi_db::discord::record_in(
        conn,
        viewer,
        scope,
        before,
        s.score,
        fi_db::discord::RecordNames {
            car_id: s.car_id,
            car_name: car_name.as_deref(),
            route_name: route_name.as_deref(),
        },
    )
    .await?;
    Ok(())
}

/// SubmitXpScore's reply: no weekly tiers.
pub fn xp_reply() -> Json {
    json!({ "weeklyTierAchieved": false, "weeklyTierInfo": {} })
}

/// The `xp` fact of a report (EVENTS.md §3.3).
pub fn xp_fact(xp: f64, accepted: bool) -> Map<String, Json> {
    let mut m = Map::new();
    m.insert("amount".into(), xp.into());
    m.insert("source".into(), "Scoreboard.SubmitXpScore".into());
    m.insert("accepted".into(), accepted.into());
    m
}

async fn submit_xp(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
) -> Result<Reply, ApiError> {
    let xp = gates::xp_of_report(params["xp"].as_f64().unwrap_or(0.0));
    // OD-112 follow-up: a modded session's XP is answered as posted and never recorded.
    if crate::game::modded_result_skipped(&ctx.req.ticket.live, "xp") {
        return Ok(Ok(xp_reply()));
    }
    let viewer = ctx.viewer();
    let refusal = if rpc::gated(ctx) {
        gates::xp_rate(scores::xp_last_hour_in(conn, &viewer).await?, xp).err()
    } else {
        None
    };
    // The fact is kept for every report, 0 XP included, so a replay stays a replay.
    if rpc::record(conn, ctx, "xp", xp_fact(xp, refusal.is_none())).await? {
        match refusal {
            Some(r) => {
                fi_db::integrity::strike_rpc_in(conn, viewer.profile_id, r.rule, &r.reason).await?;
            }
            // 0 XP adds nothing to the ledger: no row.
            None if xp > 0.0 => scores::add_xp_in(conn, &viewer, ctx.fact_id(), xp).await?,
            None => {}
        }
    }
    Ok(Ok(xp_reply()))
}

/// GetBestScore's reply.
pub fn best_reply(best: Option<(f64, bool)>) -> Json {
    match best {
        Some((score, clean)) => json!({
            "bestScore": { "score": score, "isClean": clean },
            "scoreFound": true,
        }),
        None => json!({ "scoreFound": false }),
    }
}

async fn best_score(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
) -> Result<Reply, ApiError> {
    let mut key = BoardKey {
        scoreboard_type: int(params, "scoreboardType"),
        score_type: int(params, "scoreType"),
        game_scoreboard_id: int(params, "gameScoreboardId"),
        track_id: int(params, "trackId"),
    };
    // A Monthly event's best asked without its track (`trackId` -1): the event names it.
    if key.scoreboard_type == MONTHLY_BOARD
        && key.track_id < 0
        && let Some(e) = super::rivals::event(Catalog::fh2(), i64::from(key.game_scoreboard_id))
    {
        key.track_id = i32::try_from(e.track).unwrap_or(key.track_id);
    }
    let best = scores::best_in(conn, &ctx.viewer(), key).await?;
    Ok(Ok(best_reply(best)))
}

/// An instant as the game's reader parses it (`2026-10-19T12:00:00.123Z`); `null` (the codec's
/// default datetime) for one it would not.
pub fn game_time(at: UnixMs) -> Json {
    let wire = fi_protocol::instant::format_unix_ms(at.millis()).unwrap_or_default();
    // `…T12:00:00.123000Z` → `…T12:00:00.123Z`
    let text = match wire.get(..23) {
        Some(head) => format!("{head}Z"),
        None => wire,
    };
    if fi_xtsw::valid_datetime(&text) {
        Json::String(text)
    } else {
        Json::Null
    }
}

/// The lowest and highest PI the rows show others (the game's 0.100 to 0.999).
pub const SHOWN_PI: (f64, f64) = (0.1, 0.999);

/// The class a row shows others: the class submitted when the catalog has it (else the car's
/// stock class, else 0), and never below the stock class of a car the catalog lists.
pub fn shown_class(catalog: &Catalog, car: i32, submitted: i64) -> i64 {
    let stock = catalog.car(i64::from(car)).map(|c| c.class_id);
    let class = if catalog.class(submitted).is_some() {
        submitted
    } else {
        stock.unwrap_or(0)
    };
    stock.map_or(class, |s| class.max(s))
}

/// The PI a row shows others, within [`SHOWN_PI`] (not finite: the lowest).
pub fn shown_pi(pi: f64) -> f64 {
    if pi.is_finite() {
        pi.clamp(SHOWN_PI.0, SHOWN_PI.1)
    } else {
        SHOWN_PI.0
    }
}

/// A `ScoreboardRow` of a board.
pub fn score_row(row: &ScoreRow) -> Json {
    let a = row.assists;
    let catalog = Catalog::fh2();
    json!({
        "position": row.position,
        "rank": row.position,
        "user": forza::user_data(&rpc::player_of(row.player.clone())),
        "score": row.score,
        "submittedTime": game_time(row.submitted_at),
        "carId": row.car_id,
        "carClassId": shown_class(catalog, row.car_id, i64::from(row.car_class_id)),
        "carPerformanceIndex": shown_pi(f64::from(row.car_pi)),
        "usedSTM": a.used_stm,
        "usedTCS": a.used_tcs,
        "usedABS": a.used_abs,
        "usedAutoBrake": a.used_auto_brake,
        "usedAutoShifting": a.used_auto_shifting,
        "usedClutch": a.used_clutch,
        "isClean": a.is_clean,
        "ghostFileId": row
            .ghost_file_id
            .and_then(|id| i32::try_from(id).ok())
            .unwrap_or(super::ghosts::NO_GHOST),
    })
}

/// GetRows' reply (GetRowsAnthem's with `totalRowCount`): the page, the caller's own row (only
/// their user when they have no score there) and the view's row count.
pub fn rows_reply(page: &BoardPage, caller: Option<&Player>, anthem: bool) -> Json {
    let rows: Vec<Json> = page.rows.iter().map(score_row).collect();
    let user_row = match (&page.me, caller) {
        (Some(me), _) => score_row(me),
        (None, Some(p)) => json!({ "user": forza::user_data(p) }),
        (None, None) => json!({}),
    };
    let mut out = json!({ "rows": rows, "userRow": user_row, "available": page.available });
    if anthem {
        out["totalRowCount"] = page.available.into();
    }
    out
}

/// GetTimeTrials' reply: this month's Monthly Rivals events ([`super::rivals`]) whose track
/// `raced`, at most `maxTimeTrials`.
pub fn time_trials(lang: Lang, params: &Json, raced: impl Fn(i64) -> bool) -> Json {
    let max = params["maxTimeTrials"].as_i64().unwrap_or(0);
    super::rivals::time_trials(Catalog::fh2(), UnixMs::now_system(), lang, max, raced)
}

/// Whether a board read is the car club XP board (CLUB LEADERBOARD).
pub fn is_club_xp_board(params: &Json) -> bool {
    int(params, "scoreboardType") == 8
        && int(params, "scoreType") == 4
        && params["isClubBoard"].as_bool() == Some(true)
}

/// Whether a board read shows the caller's friends (views 2 and 4): the friend set is read only
/// then.
pub fn wants_friends(params: &Json) -> bool {
    matches!(
        params["view"].as_i64().unwrap_or(0),
        VIEW_FRIENDS | VIEW_FRIENDS_AND_CLUB
    )
}

/// Whether a board read shows the caller's club (views 3 and 4, and club boards): the club's
/// members are read only then.
pub fn wants_club(params: &Json) -> bool {
    params["isClubBoard"].as_bool() == Some(true)
        || matches!(
            params["view"].as_i64().unwrap_or(0),
            VIEW_CLUB | VIEW_FRIENDS_AND_CLUB
        )
}

/// The profiles a view keeps besides the caller (the Beta's `score_rows`): view 2 the friends,
/// view 3 the club, view 4 both; a club board keeps the club's members of that (all of them for
/// another view); `None`: everyone.
pub fn circle_of(params: &Json, friends: &[Uuid], club: &[Uuid]) -> Option<Vec<Uuid>> {
    let view = params["view"].as_i64().unwrap_or(0);
    let circle: Option<Vec<Uuid>> = match view {
        VIEW_FRIENDS => Some(friends.to_vec()),
        VIEW_CLUB => Some(club.to_vec()),
        VIEW_FRIENDS_AND_CLUB => Some(friends.iter().chain(club).copied().collect()),
        _ => None,
    };
    if params["isClubBoard"].as_bool() == Some(true) {
        return Some(match circle {
            Some(c) => c.into_iter().filter(|p| club.contains(p)).collect(),
            None => club.to_vec(),
        });
    }
    circle
}

/// The Monthly Rivals boards' `scoreboardType`: their `pivotId` and the scores'
/// `gameScoreboardId` are the event id ([`super::rivals`]).
pub const MONTHLY_BOARD: i32 = scores::MONTHLY_BOARD;

/// The board a GetRows names: a Monthly Rivals event's board is that event, other track boards
/// are the route, trackless ones their event (`pivotId`); `circle` ([`circle_of`]) keeps the
/// caller and those profiles only, each at its real position.
pub fn board_query<'a>(params: &Json, circle: Option<&'a [Uuid]>) -> BoardQuery<'a> {
    let pivot = int(params, "pivotId");
    let monthly = int(params, "scoreboardType") == MONTHLY_BOARD
        && super::rivals::event(Catalog::fh2(), i64::from(pivot)).is_some();
    let filter = match params["trackId"]
        .as_i64()
        .filter(|t| *t >= 0 && !monthly)
        .and_then(|t| i32::try_from(t).ok())
    {
        Some(track) => BoardFilter::Route(track),
        None => BoardFilter::Event(pivot),
    };
    let view = params["view"].as_i64().unwrap_or(0);
    BoardQuery {
        scoreboard_type: int(params, "scoreboardType"),
        score_type: int(params, "scoreType"),
        filter,
        circle,
        start: params["startAt"].as_i64().unwrap_or(1).max(1),
        max: params["maxResults"]
            .as_i64()
            .unwrap_or(50)
            .clamp(0, scores::MAX_PAGE),
        near_me: view == VIEW_NEAR_ME,
    }
}

async fn caller(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<Player, ApiError> {
    let row =
        fi_db::gameplane::player_in(conn, ctx.req.ticket.game(), ctx.req.ticket.live.profile_id)
            .await?
            .ok_or_else(ApiError::unavailable)?;
    Ok(rpc::player_of(row))
}

async fn rows(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
    anthem: bool,
) -> Result<Reply, ApiError> {
    // The car club XP board (CR-S70): clubs by XP, or the caller's club's ladder.
    if is_club_xp_board(params) {
        return super::clubs::xp_board(conn, ctx, params, anthem).await;
    }
    // The caller's friends and club, read only for the views that show them.
    let friends = if wants_friends(params) {
        super::social::friend_ids(conn, ctx).await?
    } else {
        Vec::new()
    };
    let club = if wants_club(params) {
        super::clubs::member_ids(conn, ctx).await?
    } else {
        Vec::new()
    };
    let circle = circle_of(params, &friends, &club);
    let page =
        scores::board_in(conn, &ctx.viewer(), &board_query(params, circle.as_deref())).await?;
    let me = match page.me {
        Some(_) => None,
        None => Some(caller(conn, ctx).await?),
    };
    Ok(Ok(rows_reply(&page, me.as_ref(), anthem)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_db::gameplane::{self, Card};
    use fi_xtsw::{Schema, decode_results, encode_results};

    const CLASS: &str = "Forza.WebServices.Scoreboard";

    fn player(xuid: i64) -> gameplane::Player {
        gameplane::Player {
            xuid,
            name: Some("Tester".into()),
            card: Card {
                career_level: 7,
                badge_id: 3,
                player_title_id: 9,
            },
            tables_tran_id: None,
        }
    }

    fn row(position: i64, ghost: Option<i64>) -> ScoreRow {
        ScoreRow {
            position,
            profile_id: Uuid::nil(),
            player: player(2_535_100_000_000_001),
            key: BoardKey {
                scoreboard_type: 1,
                score_type: 1,
                game_scoreboard_id: 70,
                track_id: 7,
            },
            score: 93.25,
            submitted_at: UnixMs::from_millis(1_792_000_000_123),
            car_id: 461,
            car_class_id: 3,
            car_pi: 0.608,
            assists: Assists {
                is_clean: true,
                used_abs: true,
                ..Assists::default()
            },
            score_id: Uuid::nil(),
            ghost_file_id: ghost,
        }
    }

    fn encodes(method: &str, reply: &Json) -> Json {
        let schema = Schema::fh2();
        let bytes = encode_results(schema, CLASS, method, reply)
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        decode_results(schema, CLASS, method, &bytes)
            .unwrap()
            .to_json()
    }

    /// Known car packs' cars (255 Ferrari 512 TR, A; 9674 Dallara DW12, FM's R stored as X 999) pass the Hardcore score
    /// gate on every board kind, held to their own stats, and show their own class and name.
    #[test]
    fn pack_car_scores_pass_the_gate_on_every_board() {
        let c = Catalog::fh2();
        let track = c.tracks().find(|t| t.length_m > 0).expect("a track");
        for (id, class) in [(255, 3), (9674, 6)] {
            let raw = c.car(id).unwrap().performance_index().min(1.0);
            for (board, ty, score) in [(1, 1, 9_999.0), (1, 5, 60.0), (1, 6, 60.0), (2, 2, 5e4)] {
                let s = submission(&json!({
                    "type": ty,
                    "data": {
                        "score": score, "scoreboardType": board, "gameScoreboardId": 1,
                        "trackId": track.id, "carId": id, "carClassId": class,
                        "carPerformanceIndex": raw,
                    }
                }))
                .expect("a submission");
                assert!(gate(&s).is_ok(), "{id} {board}/{ty}: {:?}", gate(&s).err());
            }
            // Faster than the pack car itself can cover the track: refused like a stock car.
            let floor = track.length_m as f64 / (c.car(id).unwrap().top_speed_mps * 1.5);
            let s = submission(&json!({ "type": 1, "data": {
                "score": floor, "scoreboardType": 1, "gameScoreboardId": 1, "trackId": track.id,
                "carId": id, "carClassId": class, "carPerformanceIndex": raw,
            }}))
            .unwrap();
            assert!(gate(&s).is_err());
            let i = i32::try_from(id).unwrap();
            assert_eq!(shown_class(c, i, class), class);
            // As an FM5 race car may report itself: R, raw PI 1.195 → X at 1.0, accepted.
            let s = submission(&json!({ "type": 1, "data": {
                "score": 9_999.0, "scoreboardType": 1, "gameScoreboardId": 1, "trackId": track.id,
                "carId": id, "carClassId": 7, "carPerformanceIndex": 1.195,
            }}))
            .unwrap();
            if id == 9674 {
                assert_eq!((s.car_class_id, s.car_pi), (6, 1.0));
                assert!(gate(&s).is_ok());
            }
            let mut r = row(1, None);
            r.car_id = i;
            assert_eq!(score_row(&r)["carClassId"], class);
        }
    }

    /// Every reply of this module encodes with the game's schema (an encode failure is a 503).
    #[test]
    fn every_reply_encodes_with_the_schema() {
        let id = Uuid::now_v7();
        let back = encodes("SubmitScore", &submitted(id, true));
        assert_eq!(back["scoreId"], id.to_string(), "the guid round-trips");
        assert_eq!(back["uploadFiles"], true);
        let back = encodes("SubmitXpScore", &xp_reply());
        assert_eq!(back["weeklyTierAchieved"], false);
        let back = encodes("GetBestScore", &best_reply(Some((93.25, true))));
        assert_eq!(back["bestScore"]["score"], 93.25);
        assert_eq!(back["scoreFound"], true);
        assert_eq!(
            encodes("GetBestScore", &best_reply(None))["scoreFound"],
            false
        );
        let caller = rpc::player_of(player(2_535_100_000_000_002));
        let page = BoardPage {
            rows: vec![row(1, Some(12)), row(2, None)],
            me: Some(row(2, None)),
            available: 2,
        };
        for (method, anthem) in [("GetRows", false), ("GetRowsAnthem", true)] {
            let back = encodes(method, &rows_reply(&page, None, anthem));
            assert_eq!(back["rows"].as_array().unwrap().len(), 2);
            assert_eq!(back["rows"][0]["ghostFileId"], 12);
            assert_eq!(back["rows"][0]["user"]["wzGamerTag"], "Tester");
            assert_eq!(back["rows"][0]["submittedTime"], "2026-10-14T17:46:40.123Z");
            assert_eq!(back["userRow"]["position"], 2);
            assert_eq!(back["available"], 2);
            if anthem {
                assert_eq!(back["totalRowCount"], 2);
            }
            // An empty board: the caller's user only.
            let back = encodes(
                method,
                &rows_reply(&BoardPage::default(), Some(&caller), anthem),
            );
            assert_eq!(back["rows"], json!([]));
            assert_eq!(back["userRow"]["user"]["qwXuid"], 2_535_100_000_000_002u64);
            assert_eq!(back["userRow"]["position"], 0);
            assert_eq!(back["available"], 0);
        }
        let trials = encodes(
            "GetTimeTrials",
            &time_trials(Lang::En, &json!({}), |_| true),
        );
        assert_eq!(
            trials["timeTrials"].as_array().map(Vec::len),
            Some(super::super::rivals::EVENTS_PER_MONTH)
        );
    }

    #[test]
    fn submissions_take_valid_scores_and_name_their_board() {
        let params = |score: Json| {
            json!({"type": 1, "data": {"scoreboardType": 1, "gameScoreboardId": 70, "score": score, "trackId": 7,
                "carId": 461, "carClassId": 3, "carPerformanceIndex": 0.608, "isClean": true, "usedABS": true}})
        };
        let s = submission(&params(json!(93.25))).unwrap();
        assert_eq!(
            s.key,
            BoardKey {
                scoreboard_type: 1,
                score_type: 1,
                game_scoreboard_id: 70,
                track_id: 7
            }
        );
        assert!(s.assists.is_clean && s.assists.used_abs && !s.assists.used_tcs);
        for bad in [
            json!(0.0),
            json!(-3.0),
            json!(2e9),
            json!("NaN"),
            json!("Infinity"),
            Json::Null,
        ] {
            assert_eq!(submission(&params(bad.clone())), None, "{bad}");
        }
        let fact = score_fact(&s, true);
        assert_eq!(
            Json::Object(fact),
            json!({"board": "1:1", "eventKey": 70, "trackId": 7, "carId": 461, "scoreType": "time_ms",
                "value": 93250, "accepted": true})
        );
        let mut speed = s.clone();
        speed.key.score_type = 5;
        assert_eq!(score_fact(&speed, false)["scoreType"], "speed_mps");
        assert_eq!(score_fact(&speed, false)["value"], 93.25);
        assert_eq!(score_unit(10), "points");
        assert_eq!(
            Json::Object(xp_fact(1200.0, true)),
            json!({"amount": 1200.0, "source": "Scoreboard.SubmitXpScore", "accepted": true})
        );
    }

    #[test]
    fn board_reads_pick_the_route_or_the_event_and_the_view() {
        // A Monthly Rivals board is its event, whatever track the game names.
        let monthly = super::super::rivals::ID_BASE + 9 * 16;
        let m = board_query(
            &json!({"scoreboardType": MONTHLY_BOARD, "pivotId": monthly, "trackId": 560}),
            None,
        );
        assert!(matches!(m.filter, BoardFilter::Event(p) if i64::from(p) == monthly));
        let other = board_query(
            &json!({"scoreboardType": MONTHLY_BOARD, "pivotId": 7, "trackId": 560}),
            None,
        );
        assert!(matches!(other.filter, BoardFilter::Route(560)));
        let q = board_query(
            &json!({"scoreboardType": 1, "scoreType": 1, "view": 0, "trackId": 7, "pivotId": 3,
                "isClubBoard": false, "startAt": 0, "maxResults": 900}),
            None,
        );
        assert_eq!(
            (q.filter, q.circle, q.start, q.max, q.near_me),
            (BoardFilter::Route(7), None, 1, 500, false)
        );
        let q = board_query(
            &json!({"scoreboardType": 9, "scoreType": 5, "view": 1, "trackId": -1, "pivotId": 1234,
                "startAt": 3, "maxResults": 10}),
            None,
        );
        assert_eq!(
            (q.filter, q.start, q.max, q.near_me),
            (BoardFilter::Event(1234), 3, 10, true)
        );
        // Views 2, 3 and 4: friends, club, both; a club board keeps the club's members of it.
        let (f1, f2, c1, c2) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let friends = [f1, f2, c1];
        let club = [c1, c2];
        let circle = |p: Json| circle_of(&p, &friends, &club);
        assert_eq!(circle(json!({"view": 2})), Some(vec![f1, f2, c1]));
        assert_eq!(circle(json!({"view": 3})), Some(vec![c1, c2]));
        assert_eq!(circle(json!({"view": 4})), Some(vec![f1, f2, c1, c1, c2]));
        assert_eq!(
            circle(json!({"view": 2, "isClubBoard": true})),
            Some(vec![c1])
        );
        assert_eq!(
            circle(json!({"view": 0, "isClubBoard": true})),
            Some(vec![c1, c2])
        );
        for view in [0, 1, 5] {
            assert_eq!(circle(json!({"view": view})), None, "view {view}");
            assert!(!wants_friends(&json!({"view": view})));
            assert!(!wants_club(&json!({"view": view})));
        }
        assert!(wants_friends(&json!({"view": 4})) && wants_club(&json!({"view": 4})));
        assert!(wants_club(&json!({"view": 0, "isClubBoard": true})));
        let c = circle(json!({"view": 3})).unwrap();
        assert_eq!(
            board_query(&json!({"view": 3, "trackId": 7}), Some(&c)).circle,
            Some(&c[..])
        );
        assert!(is_club_xp_board(
            &json!({"scoreboardType": 8, "scoreType": 4, "isClubBoard": true})
        ));
        assert!(!is_club_xp_board(
            &json!({"scoreboardType": 8, "scoreType": 4, "isClubBoard": false})
        ));
    }

    #[test]
    fn rows_show_a_class_and_pi_the_game_holds() {
        let c = Catalog::fh2();
        // The Toyota 2000GT is class D (0): never shown below it; a catalog class above it stays.
        assert_eq!(shown_class(c, 247, 0), 0);
        assert_eq!(shown_class(c, 247, 4), 4);
        assert_eq!(shown_class(c, 247, 200), 0, "not a class: the stock one");
        let s1 = c.cars().find(|car| car.class_id == 4).unwrap();
        let s1_id = i32::try_from(s1.id).unwrap();
        assert_eq!(shown_class(c, s1_id, 0), 4, "never below the stock class");
        assert_eq!(shown_class(c, s1_id, 5), 5);
        // A car the catalog does not list: a catalog class as submitted, else 0.
        assert_eq!(shown_class(c, 3000, 3), 3);
        assert_eq!(shown_class(c, 3000, -1), 0);
        assert_eq!(shown_pi(0.608), 0.608);
        assert_eq!(shown_pi(7.0), 0.999);
        assert_eq!(shown_pi(-1.0), 0.1);
        assert_eq!(shown_pi(f64::NAN), 0.1);
        let mut r = row(1, None);
        r.car_class_id = 120;
        r.car_pi = 55.0;
        let shown = score_row(&r);
        assert_eq!(shown["carClassId"], c.car(461).map_or(0, |k| k.class_id));
        assert_eq!(shown["carPerformanceIndex"], 0.999);
        let none = json!({});
        assert_eq!(
            without_db(false, "Scoreboard", "GetTimeTrials", Lang::En, &none),
            None
        );
        assert_eq!(
            without_db(false, "Scoreboard", "GetRows", Lang::En, &none),
            None
        );
    }

    #[test]
    fn times_are_ones_the_game_parses() {
        let t = game_time(UnixMs::from_millis(1_792_000_000_123));
        assert_eq!(t, "2026-10-14T17:46:40.123Z");
        assert!(fi_xtsw::valid_datetime(t.as_str().unwrap()));
        assert_eq!(
            game_time(UnixMs::from_millis(0)),
            "1970-01-01T00:00:00.000Z"
        );
    }
}
