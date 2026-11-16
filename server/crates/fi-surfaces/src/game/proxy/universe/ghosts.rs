//! Ghost races and rivals (SER-M3 step 6; GAME_PLATFORM.md §3a "Supporter universe"):
//! `Scoreboard.UploadFile`, `DownloadFile`, `GetRival`, ported from the Beta's `upload_file`,
//! `download_file`, `pick_rival`, `challenge_rival`, `rival_ghost` and `rival_reply`
//! (`deprecated/beta/server/src/forza.rs`), over [`fi_db::scores`].
//!
//! - **UploadFile** (`{scoreId, data}`, `data` a blob of at most [`MAX_GHOST`] bytes, else Xls
//!   error 1): kept only when `scoreId` is the caller's current best on its event, else answered
//!   `{}` and discarded (the Beta's rule). The recording goes to the deployment's bucket under its
//!   SHA-256 (the ghost-blob rules of UNIVERSE_API.md §3.2 and CLOUD_SAVES.md §6) **before** the
//!   transaction that records it (`ghost_blob`), the ghost (`game_ghost`, its integer file id kept
//!   when a later ghost of the same event replaces it) and the `ghost` fact; a replayed request id
//!   writes nothing.
//! - **DownloadFile** (`{id, maxSize}`): a visible owner's ghost by its file id, re-hashed on the way
//!   out; Xls error 1 for an unknown or hidden one, or one over `maxSize` (or [`MAX_GHOST`]).
//! - **GetRival**: with `rivalXuid`, that visible player's best on the route; else the closest best
//!   on the route to the caller's own (friends first, [`super::social`]), in the car class
//!   `pivotId` names (catalog classes; -1 any). The reply carries the class and PI a board row
//!   shows (never below the car's stock class; the catalog's PI), the recording of the rival's very
//!   score, else the visible ghost nearest to the rival's score, else 0, and a bounty of
//!   [`BOUNTY`]. Nobody to race: 404 `GAME.NOT_SERVED`, which the
//!   hook turns into its offline 503, so the game shows no rival, as with the Beta's
//!   `_default.json`. The challenges of Meus Rivais ("Ir para") need the rivals sync of step 7.
//!
//! [`MAX_GHOST`]: fi_db::scores::MAX_GHOST

use super::super::forza::{Reply, Served, XlsError};
use super::super::rpc::{self, Ctx};
use super::boards::{int, score_row, shown_class, shown_pi};
use crate::errors::ApiError;
use crate::state::AppState;
use fi_db::scores::{self, GhostTarget, MAX_GHOST, RivalQuery, ScoreRow};
use fi_gamedb::Catalog;
use fi_saves::{BlobStore, Digest};
use fi_storage::Blobs;
use serde_json::{Map, Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// The class these methods are on.
pub const CLASS: &str = "Forza.WebServices.Scoreboard";

/// The body cap of `UploadFile`: the largest ghost kept and its 20 bytes of scoreId and length,
/// with room to spare (a larger body is 413 before it is decoded).
pub const UPLOAD_BODY: usize = MAX_GHOST + 1024;

/// The bounty a rival's time carries (the Beta's).
pub const BOUNTY: i64 = 9600;

/// How `(xls, class, method)` is answered; `None`: not this module's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    match (xls, short, method) {
        (false, "Scoreboard", "UploadFile" | "DownloadFile" | "GetRival") => Some(Served::Store),
        _ => None,
    }
}

/// Whether the method reaches the bucket ([`with_bucket`] answers it, outside any transaction).
pub(crate) fn uses_bucket(xls: bool, short: &str, method: &str) -> bool {
    matches!(
        (xls, short, method),
        (false, "Scoreboard", "UploadFile" | "DownloadFile")
    )
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
        "GetRival" => rival(conn, ctx, &params).await,
        _ => Err(ctx.not_served(short, method)),
    }
}

/// UploadFile and DownloadFile: each database step on its own pooled connection or transaction,
/// the bucket between them, never inside one.
pub(crate) async fn with_bucket(
    state: &AppState,
    ctx: &Ctx<'_>,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    let Some(blobs) = ctx.blobs else {
        tracing::error!(method, "no bucket for ghost recordings on this deployment");
        return Err(ApiError::unavailable());
    };
    match method {
        "UploadFile" => upload(state, ctx, blobs, &params).await,
        "DownloadFile" => download(state, ctx, blobs, &params).await,
        _ => Err(ctx.not_served(CLASS, method)),
    }
}

/// Xls error 1 for an upload without a recording or with one over [`MAX_GHOST`].
pub fn no_recording() -> XlsError {
    XlsError {
        code: 1,
        message: "ghost missing or too big",
    }
}

/// Xls error 1 for a download of an unknown or hidden ghost.
pub fn no_such_ghost() -> XlsError {
    XlsError {
        code: 1,
        message: "no such ghost",
    }
}

/// Xls error 1 for a download over the game's `maxSize`.
pub fn ghost_too_big() -> XlsError {
    XlsError {
        code: 1,
        message: "ghost too big",
    }
}

/// The bytes of a hex string (the decoded JSON of a `blob`); `None` for anything else.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let nibble = |c: u8| {
        char::from(c)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    pairs
        .iter()
        .map(|&[hi, lo]| Some(nibble(hi)? << 4 | nibble(lo)?))
        .collect()
}

/// The recording of an UploadFile, or its refusal.
pub fn recording(params: &Json) -> Result<(Uuid, Vec<u8>), XlsError> {
    let score_id = params["scoreId"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(no_recording)?;
    let hex = params["data"].as_str().unwrap_or("");
    if hex.is_empty() || hex.len() > 2 * MAX_GHOST {
        return Err(no_recording());
    }
    let bytes = unhex(hex).ok_or_else(no_recording)?;
    Ok((score_id, bytes))
}

/// The `ghost` fact of a kept upload (EVENTS.md §3.3): the event, the time it recorded (null for a
/// board that is not a time), the recording's digest and size.
pub fn ghost_fact(target: &GhostTarget, digest: &Digest, size: usize) -> Map<String, Json> {
    let time = scores::lower_is_better(target.key.score_type)
        .then(|| (target.score * 1000.0).round() as i64);
    let mut m = Map::new();
    m.insert("event".into(), target.key.event().into());
    m.insert("timeMs".into(), time.into());
    m.insert("sha256".into(), digest.hex().into());
    m.insert("size".into(), size.into());
    m
}

fn bucket_error(e: fi_saves::SaveError) -> ApiError {
    tracing::warn!(error = %e, "ghost bucket unavailable");
    ApiError::unavailable()
}

async fn upload(
    state: &AppState,
    ctx: &Ctx<'_>,
    blobs: &Blobs,
    params: &Json,
) -> Result<Reply, ApiError> {
    let (score_id, bytes) = match recording(params) {
        Ok(r) => r,
        Err(x) => return Ok(Err(x)),
    };
    // OD-112 follow-up: a modded session's recording is answered as uploaded and never stored.
    if crate::game::modded_result_skipped(&ctx.req.ticket.live, "ghost") {
        return Ok(Ok(json!({})));
    }
    let digest = Digest::of(&bytes);
    let size = i32::try_from(bytes.len()).map_err(|_| ApiError::unavailable())?;
    let viewer = ctx.viewer();
    // 1. Is it wanted? One pooled connection, released before the bucket.
    let target = {
        let mut conn = state.pool.acquire().await?;
        scores::ghost_target_in(&mut conn, &viewer, score_id, &digest.0).await?
    };
    // Not the caller's current best on that event: discarded, as the Beta's.
    let Some(target) = target else {
        return Ok(Ok(json!({})));
    };
    // 2. The recording, create-only under its digest, unless it is already there.
    if !target.blob_known {
        blobs.put(digest, &bytes).await.map_err(bucket_error)?;
    }
    // 3. The rows and the fact, in one transaction; a replayed request id writes nothing.
    let mut tx = state.pool.begin().await?;
    if rpc::record(
        &mut tx,
        ctx,
        "ghost",
        ghost_fact(&target, &digest, bytes.len()),
    )
    .await?
    {
        let saved =
            scores::save_ghost_in(&mut tx, &viewer, score_id, &digest.0, size, ctx.fact_id())
                .await
                .map_err(ApiError::from_stored)?;
        if saved.is_none() {
            // A better score arrived since step 1: discarded, the fact with it.
            tx.rollback().await?;
            return Ok(Ok(json!({})));
        }
    }
    tx.commit().await?;
    Ok(Ok(json!({})))
}

/// DownloadFile's reply.
pub fn downloaded(bytes: &[u8]) -> Json {
    json!({ "data": fi_xtsw::hex(bytes) })
}

async fn download(
    state: &AppState,
    ctx: &Ctx<'_>,
    blobs: &Blobs,
    params: &Json,
) -> Result<Reply, ApiError> {
    let id = params["id"].as_i64().unwrap_or(0);
    let max = params["maxSize"].as_i64().unwrap_or(0).max(0);
    let found = {
        let mut conn = state.pool.acquire().await?;
        scores::ghost_blob_in(&mut conn, &ctx.viewer(), id).await?
    };
    let Some((sha, size)) = found else {
        return Ok(Err(no_such_ghost()));
    };
    let size_ok = usize::try_from(size).is_ok_and(|s| s <= MAX_GHOST);
    if i64::from(size) > max || !size_ok {
        return Ok(Err(ghost_too_big()));
    }
    let bytes = blobs.get(Digest(sha)).await.map_err(bucket_error)?;
    if fi_auth::sha256(&bytes) != sha {
        tracing::error!(ghost = id, "a ghost recording does not match its digest");
        return Err(ApiError::unavailable());
    }
    Ok(Ok(downloaded(&bytes)))
}

/// GetRival's reply: the rival's best on the route as one row, with the class submitted (never
/// below the car's stock class, [`shown_class`]) and the catalog's PI of the car (the submitted
/// one for a car the catalog does not list), both within what the game holds; `ghost` (0: a time
/// without a car to chase) and the bounty.
pub fn rival_reply(catalog: &Catalog, row: &ScoreRow, ghost: Option<i64>) -> Json {
    rival_reply_with(catalog, row, ghost, BOUNTY)
}

/// The "no ghost" file id: the game downloads any other id, 0 included (SAURON RE 2026-10-30:
/// DownloadRivalsGhost FUN_140661b90 skips only -1; a 0 left "Loading Rivals" waiting).
pub const NO_GHOST: i32 = -1;

/// GetRival's reply when no real time exists (SAURON RE: the game waits on GetRival success or
/// failure, and a failure is its "rivals server error"; a success with ghost -1 races without a
/// ghost): an empty rival, nothing invented.
pub fn no_rival_reply() -> Json {
    json!({ "rival": { "scoreData": { "ghostFileId": NO_GHOST, "bounty": 0 }, "bounty": 0 } })
}

/// [`rival_reply`] with its `bounty` (0 for the player's own best).
pub fn rival_reply_with(
    catalog: &Catalog,
    row: &ScoreRow,
    ghost: Option<i64>,
    bounty: i64,
) -> Json {
    let car = catalog.car(i64::from(row.car_id));
    let class = shown_class(catalog, row.car_id, i64::from(row.car_class_id));
    let pi = shown_pi(car.map_or(f64::from(row.car_pi), |c| c.performance_index()));
    let mut data = score_row(row);
    data["position"] = 1.into();
    data["rank"] = 1.into();
    data["carClassId"] = class.into();
    data["carPerformanceIndex"] = pi.into();
    data["ghostFileId"] = ghost
        .and_then(|g| i32::try_from(g).ok())
        .unwrap_or(NO_GHOST)
        .into();
    data["bounty"] = bounty.into();
    json!({ "rival": { "scoreData": data, "bounty": bounty } })
}

/// The cars of a catalog class (`pivotId`), known car packs' cars included; `None` for -1 (any
/// car).
pub fn cars_of_class(catalog: &Catalog, class: i32) -> Option<Vec<i32>> {
    (class >= 0).then(|| {
        catalog
            .all_cars()
            .filter(|c| c.class_id == i64::from(class))
            .filter_map(|c| i32::try_from(c.id).ok())
            .collect()
    })
}

/// The track race boards' `scoreboardType` (festival and route races on a track).
pub const ROUTE_BOARD: i32 = 1;

async fn rival(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    // A pending challenge of the inbox (Meus Rivais → "Ir para"): that friend's time (step 7).
    if let Some(reply) = super::inbox::challenge(conn, ctx, params).await? {
        return Ok(Ok(reply));
    }
    let (board, score_type, track) = (
        int(params, "scoreboardType"),
        int(params, "scoreType"),
        int(params, "trackId"),
    );
    let viewer = ctx.viewer();
    let catalog = Catalog::fh2();
    // A Monthly Rivals event (S3; other projects used as reference): its own board and its class, whatever the
    // request's pivot says.
    let monthly = [int(params, "rivalEventId"), int(params, "pivotId")]
        .into_iter()
        .filter(|_| board == super::boards::MONTHLY_BOARD)
        .find_map(|id| super::rivals::event(catalog, i64::from(id)));
    let event = monthly.as_ref().and_then(|e| i32::try_from(e.id).ok());
    // The game asks for a Monthly event's rival before it read GetTimeTrials too, with
    // `trackId` -1 (SAURON V2, 2026-10-30): the event names its track.
    let track = monthly
        .as_ref()
        .and_then(|e| i32::try_from(e.track).ok())
        .unwrap_or(track);
    let class = monthly.as_ref().map_or(int(params, "pivotId"), |e| {
        i32::try_from(e.class).unwrap_or(-1)
    });
    // The caller's friends come first (the closest friend, then anyone visible).
    let friends = super::social::friend_ids(conn, ctx).await?;
    let query = |cars, xuid| RivalQuery {
        scoreboard_type: board,
        score_type,
        track,
        event,
        cars,
        friends: &friends,
        xuid,
    };
    let mut found = None;
    // An explicit rival: that player's time, whatever the car.
    if let Some(xuid) = params["rivalXuid"]
        .as_u64()
        .filter(|x| *x != 0)
        .and_then(|x| i64::try_from(x).ok())
    {
        found = scores::rival_in(conn, &viewer, &query(None, Some(xuid))).await?;
    }
    let cars = cars_of_class(catalog, class);
    if found.is_none() {
        found = scores::rival_in(conn, &viewer, &query(cars.as_deref(), None)).await?;
    }
    // A Monthly event nobody raced yet: the game cannot start it without a rival (owner
    // 2026-10-30: SERVER ERROR on accept; rivals only from real data). Real times of the event's
    // track, nearest the caller's: earlier Monthly events in its class, then the track's race
    // board in its class, then in any car, then earlier Monthly events in any car (Monthly lists
    // only events with such a time). Never an invented time.
    let mut fallback_board = None;
    if found.is_none() && monthly.is_some() {
        for (b, c) in [
            (board, cars.as_deref()),
            (ROUTE_BOARD, cars.as_deref()),
            (ROUTE_BOARD, None),
            (board, None),
        ] {
            let q = RivalQuery {
                scoreboard_type: b,
                event: None,
                cars: c,
                ..query(None, None)
            };
            found = scores::rival_in(conn, &viewer, &q).await?;
            if found.is_some() {
                fallback_board = Some(b);
                break;
            }
        }
    }
    // Nobody else: the caller's own best is the rival (the game supports it: IsRivalSelf), no
    // bounty; with no real time at all, an empty rival without a ghost. Never a failure: the game
    // shows "rivals server error" and retries (SAURON RE 2026-10-30).
    let Some(row) = found else {
        let own_q = RivalQuery {
            cars: None,
            ..query(None, None)
        };
        let Some(own) = scores::own_rival_in(conn, &viewer, &own_q).await? else {
            return Ok(Ok(no_rival_reply()));
        };
        let ghost = own.ghost_file_id;
        return Ok(Ok(rival_reply_with(catalog, &own, ghost, 0)));
    };
    // A time from another board is a real target, but its ghost is not: the game drops a ghost
    // recorded for another kind of event (WINDOWS V2 control, 2026-10-30: a race board ghost
    // downloaded and never shown). Only a ghost of this board races.
    let ghost = match (fallback_board, row.ghost_file_id) {
        (Some(_), _) => None,
        (None, Some(own)) => Some(own),
        (None, None) => {
            scores::nearest_ghost_in(conn, &viewer, board, score_type, track, event, row.score)
                .await?
        }
    };
    Ok(Ok(rival_reply(catalog, &row, ghost)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_db::gameplane::{self, Card};
    use fi_db::scores::{Assists, BoardKey};
    use fi_domain::UnixMs;
    use fi_xtsw::{Schema, decode_results, encode_results};

    fn row(car: i32, class: i16) -> ScoreRow {
        ScoreRow {
            position: 4,
            profile_id: Uuid::nil(),
            player: gameplane::Player {
                xuid: 2_535_100_000_000_003,
                name: Some("Rival".into()),
                card: Card::default(),
                tables_tran_id: None,
            },
            key: BoardKey {
                scoreboard_type: 1,
                score_type: 1,
                game_scoreboard_id: 70,
                track_id: 0,
            },
            score: 95.5,
            submitted_at: UnixMs::from_millis(1_792_000_000_000),
            car_id: car,
            car_class_id: class,
            car_pi: 0.7,
            assists: Assists::default(),
            score_id: Uuid::nil(),
            ghost_file_id: Some(5),
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

    /// Every reply of this module encodes with the game's schema.
    #[test]
    fn every_reply_encodes_with_the_schema() {
        // UploadFile has no _Results: `{}` encodes to nothing.
        assert!(
            encode_results(Schema::fh2(), CLASS, "UploadFile", &json!({}))
                .unwrap()
                .is_empty()
        );
        let back = encodes("DownloadFile", &downloaded(&[0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(back["data"], "deadbeef");
        let c = Catalog::fh2();
        // The Toyota 2000GT (class 0, PI 330): the catalog's class and PI.
        let back = encodes("GetRival", &rival_reply(c, &row(247, 0), Some(5)));
        let d = &back["rival"]["scoreData"];
        assert_eq!(back["rival"]["bounty"], BOUNTY);
        assert_eq!(
            (d["position"].clone(), d["rank"].clone()),
            (json!(1), json!(1))
        );
        assert_eq!(d["carClassId"], 0);
        assert!((d["carPerformanceIndex"].as_f64().unwrap() - 0.33).abs() < 1e-6);
        assert_eq!(d["ghostFileId"], 5);
        assert_eq!(d["bounty"], BOUNTY);
        assert_eq!(d["score"], 95.5);
        assert_eq!(d["user"]["wzGamerTag"], "Rival");
        // A car the catalog does not list keeps what was submitted; no ghost is -1 (the game
        // downloads any other id, 0 included).
        let back = encodes("GetRival", &rival_reply(c, &row(30_000, 4), None));
        let d = &back["rival"]["scoreData"];
        assert_eq!(d["carClassId"], 4);
        assert!((d["carPerformanceIndex"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        assert_eq!(d["ghostFileId"], NO_GHOST);
        // The player's own best: no bounty.
        let own = encodes("GetRival", &rival_reply_with(c, &row(461, 4), Some(5), 0));
        assert_eq!(own["rival"]["bounty"], 0);
        assert_eq!(own["rival"]["scoreData"]["bounty"], 0);
        // Nobody at all: an empty rival without a ghost, still a success.
        let none = encodes("GetRival", &no_rival_reply());
        assert_eq!(none["rival"]["scoreData"]["ghostFileId"], NO_GHOST);
        assert_eq!(none["rival"]["scoreData"]["score"], 0.0);
        assert_eq!(none["rival"]["bounty"], 0);
    }

    #[test]
    fn uploads_need_a_score_id_and_a_recording_within_the_limit() {
        let id = Uuid::now_v7();
        let (sid, bytes) =
            recording(&json!({"scoreId": id.to_string(), "data": "00ff10"})).unwrap();
        assert_eq!((sid, bytes), (id, vec![0, 255, 16]));
        for bad in [
            json!({"scoreId": id.to_string(), "data": ""}),
            json!({"scoreId": id.to_string(), "data": "abc"}),
            json!({"scoreId": id.to_string(), "data": "zz"}),
            json!({"scoreId": id.to_string(), "data": "00".repeat(MAX_GHOST + 1)}),
            json!({"scoreId": "nope", "data": "00"}),
        ] {
            assert_eq!(recording(&bad).unwrap_err().code, 1);
        }
        assert!(
            recording(&json!({"scoreId": id.to_string(), "data": "00".repeat(MAX_GHOST)})).is_ok()
        );
        assert_eq!(unhex("DEADbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        let target = GhostTarget {
            key: BoardKey {
                scoreboard_type: 1,
                score_type: 1,
                game_scoreboard_id: 70,
                track_id: 7,
            },
            score: 93.2504,
            blob_known: false,
        };
        let d = Digest::of(b"ghost");
        assert_eq!(
            Json::Object(ghost_fact(&target, &d, 5)),
            json!({"event": "1:1:70:7", "timeMs": 93250, "sha256": d.hex(), "size": 5})
        );
        let points = GhostTarget {
            key: BoardKey {
                score_type: 10,
                ..target.key
            },
            ..target
        };
        assert_eq!(ghost_fact(&points, &d, 5)["timeMs"], Json::Null);
        const _: () = assert!(
            UPLOAD_BODY > MAX_GHOST + 20 && UPLOAD_BODY < super::super::super::rpc::RPC_BODY
        );
    }

    #[test]
    fn classes_name_catalog_cars() {
        let c = Catalog::fh2();
        assert_eq!(cars_of_class(c, -1), None);
        let d = cars_of_class(c, 0).unwrap();
        assert!(d.contains(&247), "the Toyota 2000GT is class D");
        // Known car packs' cars race their own class's rivals.
        assert!(cars_of_class(c, 3).unwrap().contains(&255));
        assert!(cars_of_class(c, 6).unwrap().contains(&9674));
        assert!(cars_of_class(c, 99).unwrap().is_empty());
        assert!(served(false, "Scoreboard", "GetRival").is_some());
        assert!(uses_bucket(false, "Scoreboard", "UploadFile"));
        assert!(!uses_bucket(false, "Scoreboard", "GetRival"));
        assert!(!uses_bucket(true, "Scoreboard", "UploadFile"));
    }
}
