//! The drivatars (SER-M3 step 6, OD-48; GAME_PLATFORM.md §3a "Supporter universe", BUG-0001):
//! `Drivatar.SaveDrivatarInformation`, `GetDrivatarInformationForRace`, `GetDrivatarsForCache`,
//! ported from the Beta's `save_drivatar`, `drivatar_car`, `racing_info`, `drivatars_for_race`
//! and the `GetDrivatarsForCache` arm (`deprecated/beta/server/src/forza.rs`), with BUG-0001
//! fixed: others get only **trained** drivatars ([`trained`]) whose `wristbandLevel` is
//! **derived from the owner's online profile** (its XP, `fi_db::drivatars`), and a drivatar
//! without one, or without a car, is left out instead of defaulted.
//!
//! - **Save**: the upload (`info`, `curveTypes`, `segmentInformation`) is checked before it is
//!   stored, because it reaches other players' games ([`upload_of`]: every number within the
//!   bounds of [`MAX_TIME`], [`MAX_VARIANCE`], [`MAX_MEAN`], [`MAX_OTHER`]; a segment on a track the
//!   catalog does not list, or an id out of range, dropped; at most [`MAX_CURVES`] and
//!   [`MAX_SEGMENTS`] valid ones kept), [`trained`] decided, and stored in the game's own encoding
//!   (the SaveDrivatarInformation parameters, [`encode_model`]) in place of the profile's
//!   drivatar in the ticket's mode, after the request's `drivatar` fact (`bytes`: the request
//!   body's size; `accepted`: true), so a replayed request id writes nothing. An untrained upload
//!   is stored too: its owner's races keep training from it.
//! - **Race**: up to [`MAX_FOR_RACE`] other players' drivatars for `bucketId` (above 0: the
//!   owner's most-raced garage car of that bucket; 0 or below, as the game sends for "any": the
//!   most-raced bucketed car; else the game's own pick, see "The car" below), `cachedDrivatars` left out
//!   unless also in `cachedDrivatarsNewCars`, each at the **caller's** `skillSelection` (clamped
//!   to `0..=`[`MAX_SKILL_LEVEL`]: the player's chosen difficulty governs the AI they race);
//!   `playerData` the caller's own stored drivatar, trained or not (so the game keeps training
//!   it), else the Beta's minimal one, with `UserSkillLevel` = `skillSelection` and
//!   `wristbandLevel` = the caller's own XP.
//! - **Cache**: up to [`MAX_FOR_CACHE`] drivatars, any bucket, `BucketId` 0, at the caller's own
//!   difficulty (`assists[0].aiDiff`, clamped; [`DEFAULT_SKILL`] when it synced none).
//! - Replies stay within a byte budget ([`RACE_BUDGET`], [`CACHE_BUDGET`]): the store stops at
//!   it (each drivatar counted as its stored model plus [`RACING_OVERHEAD`]) and [`race_reply`] /
//!   [`cache_reply`] check the exact encoded size; trailing drivatars are dropped, none is cut.
//!
//! **The car (BUG-0003, 2026-10-31).** `CarId` is the owner's real car when its garage holds one
//! of the race's bucket (it fits the event); otherwise [`GAME_PICKS_CAR`] (`-1`) with `BucketId`
//! 0, the pair every drivatar of the game's shipped cache (`DefaultCache.bin`) carries and other projects
//! used as reference serve: the game then chooses a car for the event itself, from the event's own class, PI and
//! restrictions, which the request does not carry (it sends only `bucketId`). The former fallback,
//! the bucket's first hero car, put every such drivatar in the same car (all eleven in a
//! Lamborghini Diablo SV, bucket 12, in the report). Two owners racing the same real car still
//! bring it, as they would in the game. `LiveryId` and `TuneId` are 0 (the car's own paint):
//! liveries come with the storefront (SER-M3 step 9). The caller's friends come first, `IsFriend` true (SER-M3 step 7: the friend set of
//! [`super::social`], read once per request), then everyone else as above.
//!
//! `UserInfo` is the drivatar's uploaded `info` (DrivatarInfo: the behaviour model the game
//! learned; the online profile has nothing of its shape), never a progression.
//!
//! **Every ticket, its own mode (OD-128, OD-130).** The three methods are served to every ticket,
//! free and Noob included (`forza::served`); the candidates are the caller's mode only
//! (`fi_db::drivatars`: Hardcore never races Noob drivatars).
//! When the real ones are fewer than asked, the reply is shorter: nothing is invented (OD-130,
//! which reverted OD-128's filler), and the game fills the rest with its own AI.

use super::super::forza::{self, Reply, Served};
use super::super::rpc::{Ctx, record};
use crate::errors::ApiError;
use fi_db::drivatars::{self as store, Candidate, GAME_PICKS_CAR, Own, Upload, Wanted};
use fi_gamedb::Catalog;
use fi_xtsw::{Limits, Schema, decode_params, encode_params, encode_results, ok_envelope};
use serde_json::{Map, Value as Json, json};
use sqlx::PgConnection;
use std::collections::HashSet;
use std::sync::OnceLock;

pub use fi_db::drivatars::{MAX_CURVES, MAX_MODEL_BYTES, MAX_SEGMENTS};

const CLASS: &str = "Forza.WebServices.Drivatar";
const SAVE: &str = "SaveDrivatarInformation";
const RACE: &str = "GetDrivatarInformationForRace";
const CACHE: &str = "GetDrivatarsForCache";

/// The most drivatars of a race grid (`numberOfDrivatars` is clamped to it).
pub const MAX_FOR_RACE: usize = 64;

/// The most drivatars of one cache request.
pub const MAX_FOR_CACHE: usize = 200;

/// The largest GetDrivatarInformationForRace reply, envelope included.
pub const RACE_BUDGET: usize = 512 << 10;

/// The largest GetDrivatarsForCache reply, envelope included.
pub const CACHE_BUDGET: usize = 1 << 20;

/// What a served DrivatarRacingInfo takes beyond its stored model (gamertag of up to 15
/// characters, ids, levels): an upper bound, checked against the encoder by a test.
pub const RACING_OVERHEAD: usize = 64;

/// The highest `UserSkillLevel` served for another player's drivatar: the game's drivatar
/// difficulties (`skillSelection`, `assists.aiDiff`) are fewer than 8.
pub const MAX_SKILL_LEVEL: i64 = 7;

/// The cache's `UserSkillLevel` when the caller never synced a difficulty: the game's default
/// (`aiDiff` and `skillSelection` are 1 in every captured sync and race).
pub const DEFAULT_SKILL: i64 = 1;

/// The largest observation time or clock (`*Time` fields) stored, in the game's seconds; the
/// captures' are below 10^4.
pub const MAX_TIME: f64 = 1.0e7;

/// The largest variance (`*Variance` fields) stored; the captures' are 0.
pub const MAX_VARIANCE: f64 = 100.0;

/// The largest magnitude of a mean (`*Mean` fields) stored; the captures' are within 0 to 1.
pub const MAX_MEAN: f64 = 10.0;

/// The largest magnitude of any other number of a model (the `lineSelector*` weights); the
/// captures' are within 0 to 10.
pub const MAX_OTHER: f64 = 100.0;

/// The largest `localSegmentId` stored (no captured upload carried a segment: an assumption).
pub const MAX_SEGMENT_ID: i64 = 9_999;

/// The largest `curveTypeId` stored (no captured upload carried a curve: an assumption).
pub const MAX_CURVE_TYPE: i64 = 255;

/// The observation times of a BaseDrivatarInfo (the `base` of DrivatarInfo, CurveInfo and
/// SegmentInfo): how long each learned quantity has been watched.
pub const BASE_TIMES: [&str; 4] = [
    "timeRatioTime",
    "cornerCuttingTime",
    "goingWideTime",
    "goingOffTime",
];

/// DrivatarInfo's own observation times.
pub const INFO_TIMES: [&str; 4] = [
    "blockingTime",
    "sideswipeTime",
    "temperamentTime",
    "racingLineOffsetTime",
];

/// The numbers of a BaseDrivatarInfo, in the schema's order.
pub const BASE_FIELDS: [&str; 9] = [
    "timeRatioMean",
    "timeRatioVariance",
    "timeRatioTime",
    "cornerCuttingMean",
    "cornerCuttingTime",
    "goingWideMean",
    "goingWideTime",
    "goingOffMean",
    "goingOffTime",
];

/// The numbers of a DrivatarInfo besides its `base`, in the schema's order.
pub const INFO_FIELDS: [&str; 20] = [
    "blockingMean",
    "blockingTime",
    "sideswipeMean",
    "sideswipeTime",
    "temperamentMean",
    "temperamentTime",
    "lastUpdateTime",
    "lineSelectorRelativeSpeed",
    "lineSelectorTimeToCollision",
    "lineSelectorDistanceToCollision",
    "lineSelectorDistanceToGrass",
    "lineSelectorOutOfBounds",
    "lineSelectorOffTrack",
    "lineSelectorCar",
    "lineSelectorOffLine",
    "lineSelectorOutsidePass",
    "lineSelectorChangePassSides",
    "racingLineOffsetMean",
    "racingLineOffsetVariance",
    "racingLineOffsetTime",
];

/// How `(xls, class, method)` is answered; `None`: not this module's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    match (xls, short, method) {
        (false, "Drivatar", SAVE | RACE | CACHE) => Some(Served::Store),
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
        SAVE => save(conn, ctx, params).await,
        RACE => race(conn, ctx, &params).await,
        CACHE => cache(conn, ctx, &params).await,
        _ => Err(ctx.not_served(short, method)),
    }
}

async fn save(conn: &mut PgConnection, ctx: &Ctx<'_>, params: Json) -> Result<Reply, ApiError> {
    let upload = upload_of(params).ok_or_else(|| {
        // The checked model always encodes: this is a server bug, not the game's.
        tracing::error!("a checked drivatar model does not encode");
        ApiError::unavailable()
    })?;
    // OD-112 follow-up: a modded session's Drivatar is answered as saved and never stored (its
    // vanilla Drivatar stays as it was).
    if crate::game::modded_result_skipped(&ctx.req.ticket.live, "drivatar") {
        return Ok(Ok(json!({})));
    }
    let mut fact = Map::new();
    fact.insert("bytes".into(), ctx.req.body.len().into());
    fact.insert("accepted".into(), true.into());
    // Recorded first: a replayed request id writes nothing.
    if record(conn, ctx, "drivatar", fact).await? {
        store::save_in(
            conn,
            ctx.owner(),
            &ctx.req.ticket.claims.mode,
            &upload,
            ctx.fact_id(),
        )
        .await
        .map_err(ApiError::from_stored)?;
    }
    Ok(Ok(json!({})))
}

async fn race(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let bucket = params["bucketId"].as_i64().unwrap_or(0);
    let limit = count(params, MAX_FOR_RACE);
    let skill_selection = params["skillSelection"].as_i64().unwrap_or(0);
    let viewer = ctx.viewer();
    let own = store::own_in(conn, &viewer)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    let empty = race_reply(
        player_data(&own, bucket, skill_selection),
        Vec::new(),
        own.now.millis(),
    );
    let cars = race_cars(bucket);
    let skip = skipped(params);
    let friends = friends_for(conn, ctx, limit).await?;
    let others = store::candidates_in(
        conn,
        &Wanted {
            viewer,
            friends: &friends,
            cars: &cars,
            skip: &skip,
            limit,
            budget: RACE_BUDGET.saturating_sub(encoded_size(RACE, &empty)),
            overhead: RACING_OVERHEAD,
        },
    )
    .await?;
    let skill = skill_level(skill_selection);
    let infos: Vec<Json> = others
        .iter()
        .filter_map(|c| racing_info(c, bucket, skill))
        .collect();
    let real = others.len();
    let mut reply = empty;
    reply["drivatarInfos"] = Json::Array(fit(RACE, &reply, infos, RACE_BUDGET));
    // Counts only: the reads write nothing, so this line is their trace.
    tracing::info!(
        method = RACE,
        asked = limit,
        real,
        served = reply["drivatarInfos"].as_array().map_or(0, Vec::len),
        "drivatars served"
    );
    Ok(Ok(reply))
}

async fn cache(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let limit = count(params, MAX_FOR_CACHE);
    let viewer = ctx.viewer();
    let own = store::own_in(conn, &viewer)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    let friends = friends_for(conn, ctx, limit).await?;
    let others = store::candidates_in(
        conn,
        &Wanted {
            viewer,
            friends: &friends,
            cars: bucketed_cars(Catalog::fh2()),
            skip: &[],
            limit,
            budget: CACHE_BUDGET.saturating_sub(encoded_size(CACHE, &cache_reply(Vec::new()))),
            overhead: RACING_OVERHEAD,
        },
    )
    .await?;
    let skill = own
        .difficulty
        .map_or(DEFAULT_SKILL, |d| skill_level(i64::from(d)));
    let infos: Vec<Json> = others
        .iter()
        .filter_map(|c| racing_info(c, 0, skill))
        .collect();
    let reply = cache_reply(infos);
    // Counts only: the reads write nothing, so this line is their trace.
    tracing::info!(
        method = CACHE,
        asked = limit,
        real = others.len(),
        served = reply["drivatarInfos"].as_array().map_or(0, Vec::len),
        "drivatars served"
    );
    Ok(Ok(reply))
}

/// The caller's friends, offered first (none read when no drivatar is asked for).
async fn friends_for(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    limit: usize,
) -> Result<Vec<uuid::Uuid>, ApiError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    super::social::friend_ids(conn, ctx).await
}

/// `numberOfDrivatars` clamped to `0..=max`.
fn count(params: &Json, max: usize) -> usize {
    let n = params["numberOfDrivatars"].as_i64().unwrap_or(0);
    usize::try_from(n.max(0)).unwrap_or(0).min(max)
}

/// A difficulty as another player's drivatar is served it: `0..=`[`MAX_SKILL_LEVEL`].
pub fn skill_level(difficulty: i64) -> i64 {
    difficulty.clamp(0, MAX_SKILL_LEVEL)
}

/// [`cars_for`] in the game's own catalog (`fi_gamedb::Catalog::fh2`).
pub fn race_cars(bucket: i64) -> Vec<i64> {
    cars_for(Catalog::fh2(), bucket)
}

/// The garage cars a drivatar may bring to a race of `bucket`: above 0, the bucket's cars (none
/// for a bucket the catalog does not list); 0 or below ("any"), every bucketed car. One whose
/// garage holds none of them brings [`GAME_PICKS_CAR`] (the game's pick), never a car chosen
/// here: the request carries no class or PI to choose by.
pub fn cars_for(catalog: &Catalog, bucket: i64) -> Vec<i64> {
    if bucket > 0 {
        catalog.bucket_cars(bucket).to_vec()
    } else {
        bucketed(catalog)
    }
}

fn bucketed(catalog: &Catalog) -> Vec<i64> {
    catalog
        .cars()
        .map(|c| c.id)
        .filter(|id| catalog.bucket_of(*id).is_some())
        .collect()
}

/// Every car of the FH2 catalog that races in a bucket (the cache's cars), computed once.
fn bucketed_cars(catalog: &'static Catalog) -> &'static [i64] {
    static CARS: OnceLock<Vec<i64>> = OnceLock::new();
    CARS.get_or_init(|| bucketed(catalog))
}

/// The XUIDs a race leaves out: `cachedDrivatars` not in `cachedDrivatarsNewCars` (the game
/// already holds them and their car), at most [`store::MAX_SKIPPED`].
pub fn skipped(params: &Json) -> Vec<i64> {
    let ids = |k: &str| {
        params[k]
            .as_array()
            .map(|a| a.iter().filter_map(Json::as_u64).collect::<Vec<u64>>())
            .unwrap_or_default()
    };
    let new_cars: HashSet<u64> = ids("cachedDrivatarsNewCars").into_iter().collect();
    ids("cachedDrivatars")
        .into_iter()
        .filter(|x| !new_cars.contains(x))
        .filter_map(|x| i64::try_from(x).ok())
        .take(store::MAX_SKIPPED)
        .collect()
}

fn positive(v: &Json) -> bool {
    v.as_f64().is_some_and(|x| x.is_finite() && x > 0.0)
}

fn base_trained(base: &Json) -> bool {
    BASE_TIMES.iter().any(|k| positive(&base[*k]))
}

/// Whether a drivatar holds trained data: any observation time above 0, of the model
/// (`info.base`'s [`BASE_TIMES`], `info`'s [`INFO_TIMES`]) or of any curve or segment (their
/// `base`'s). The game's defaults (`lineSelector*`, the means and variances it starts from) and
/// `lastUpdateTime` (a clock) say nothing about training; an untrained model, like the Beta's
/// test fixture `{"info": {}, "curveTypes": [], "segmentInformation": []}`, has every time at 0.
pub fn trained(info: &Json, curves: &[Json], segments: &[Json]) -> bool {
    base_trained(&info["base"])
        || INFO_TIMES.iter().any(|k| positive(&info[*k]))
        || curves
            .iter()
            .chain(segments)
            .any(|e| base_trained(&e["base"]))
}

/// One number of a model within its field's bounds: a `*Time` within `0..=`[`MAX_TIME`], a
/// `*Variance` within `0..=`[`MAX_VARIANCE`], a `*Mean` within ±[`MAX_MEAN`], anything else
/// within ±[`MAX_OTHER`]; absent or non-finite (`"NaN"`, `"Infinity"` in fi-xtsw's JSON): 0.
pub fn bounded_number(field: &str, v: &Json) -> f64 {
    let x = v.as_f64().filter(|x| x.is_finite()).unwrap_or(0.0);
    let (low, high) = if field.ends_with("Time") {
        (0.0, MAX_TIME)
    } else if field.ends_with("Variance") {
        (0.0, MAX_VARIANCE)
    } else if field.ends_with("Mean") {
        (-MAX_MEAN, MAX_MEAN)
    } else {
        (-MAX_OTHER, MAX_OTHER)
    };
    x.clamp(low, high)
}

fn numbers(fields: &[&str], v: &Json) -> Map<String, Json> {
    fields
        .iter()
        .map(|k| ((*k).to_owned(), json!(bounded_number(k, &v[*k]))))
        .collect()
}

/// A BaseDrivatarInfo with exactly its schema fields, each bounded.
fn checked_base(v: &Json) -> Json {
    Json::Object(numbers(&BASE_FIELDS, v))
}

/// A DrivatarInfo with exactly its schema fields, each bounded.
pub fn checked_info(v: &Json) -> Json {
    let mut out = numbers(&INFO_FIELDS, v);
    out.insert("base".into(), checked_base(&v["base"]));
    Json::Object(out)
}

/// An integer id within `0..=max`, else `None`.
fn id(v: &Json, max: i64) -> Option<i64> {
    v.as_i64().filter(|n| (0..=max).contains(n))
}

/// A SegmentInfo as stored: on a track of the catalog, `localSegmentId` within
/// `0..=`[`MAX_SEGMENT_ID`], its `base` bounded; else `None` (dropped).
pub fn checked_segment(catalog: &Catalog, v: &Json) -> Option<Json> {
    let track = v["trackId"]
        .as_i64()
        .filter(|t| catalog.track(*t).is_some())?;
    let segment = id(&v["localSegmentId"], MAX_SEGMENT_ID)?;
    Some(json!({"base": checked_base(&v["base"]), "localSegmentId": segment, "trackId": track}))
}

/// A CurveInfo as stored: `curveTypeId` within `0..=`[`MAX_CURVE_TYPE`], its `base` bounded;
/// else `None` (dropped).
pub fn checked_curve(v: &Json) -> Option<Json> {
    let curve = id(&v["curveTypeId"], MAX_CURVE_TYPE)?;
    Some(json!({"base": checked_base(&v["base"]), "curveTypeId": curve}))
}

fn list(v: Option<Json>) -> Vec<Json> {
    match v {
        Some(Json::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// A drivatar model as the replies use it (DrivatarInfo, CurveInfo and SegmentInfo lists).
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    /// DrivatarInfo.
    pub info: Json,
    /// CurveInfo list.
    pub curves: Json,
    /// SegmentInfo list.
    pub segments: Json,
}

/// A model in the game's encoding, as stored: the SaveDrivatarInformation parameters it came in.
pub fn encode_model(m: &Model) -> Option<Vec<u8>> {
    let params = json!({"info": m.info, "curveTypes": m.curves, "segmentInformation": m.segments});
    encode_params(Schema::fh2(), CLASS, SAVE, &params).ok()
}

/// A stored model back; `None` when the bytes do not decode (the drivatar is then left out).
pub fn decode_model(bytes: &[u8]) -> Option<Model> {
    let mut p = decode_params(Schema::fh2(), CLASS, SAVE, bytes)
        .ok()?
        .into_json();
    Some(Model {
        info: p["info"].take(),
        curves: p["curveTypes"].take(),
        segments: p["segmentInformation"].take(),
    })
}

/// A SaveDrivatarInformation as stored (see the module): the model checked against the bounds,
/// invalid curves and segments dropped, the first valid [`MAX_CURVES`] and [`MAX_SEGMENTS`]
/// kept, whether it is [`trained`], and the game's encoding of it; `None` only if that encoding
/// fails (a server bug).
pub fn upload_of(params: Json) -> Option<Upload> {
    let mut p = match params {
        Json::Object(map) => map,
        _ => Map::new(),
    };
    let info = checked_info(&p.remove("info").unwrap_or(Json::Null));
    let catalog = Catalog::fh2();
    let sent_curves = list(p.remove("curveTypes"));
    let sent_segments = list(p.remove("segmentInformation"));
    let valid_curves: Vec<Json> = sent_curves.iter().filter_map(checked_curve).collect();
    let valid_segments: Vec<Json> = sent_segments
        .iter()
        .filter_map(|s| checked_segment(catalog, s))
        .collect();
    let dropped = (
        sent_curves.len() - valid_curves.len(),
        sent_segments.len() - valid_segments.len(),
    );
    if dropped != (0, 0) {
        // Counts only: the bounds of ids are assumptions until a device shows the game's.
        tracing::debug!(
            dropped_curves = dropped.0,
            dropped_segments = dropped.1,
            "drivatar upload: invalid curves or segments left out"
        );
    }
    let curves: Vec<Json> = valid_curves.into_iter().take(MAX_CURVES).collect();
    let segments: Vec<Json> = valid_segments.into_iter().take(MAX_SEGMENTS).collect();
    let trained = trained(&info, &curves, &segments);
    let model = encode_model(&Model {
        info,
        curves: Json::Array(curves),
        segments: Json::Array(segments),
    })?;
    Some(Upload { model, trained })
}

fn bounded(list: &Json, max: usize) -> Json {
    Json::Array(
        list.as_array()
            .map(|a| a.iter().take(max).cloned().collect())
            .unwrap_or_default(),
    )
}

/// A DrivatarRacingInfo.
#[allow(clippy::too_many_arguments)]
fn info(
    gamertag: String,
    friend: bool,
    xuid: i64,
    car: i64,
    bucket: i64,
    model: &Model,
    skill: i64,
    wristband: u32,
) -> Json {
    json!({
        "GamerTag": gamertag,
        "IsFriend": friend,
        "Xuid": xuid,
        "CarId": car,
        // The car's own paint and tune until the storefront (SER-M3 step 9).
        "LiveryId": 0,
        "TuneId": 0,
        // The game's pick goes with BucketId 0, as in its shipped cache.
        "BucketId": if car == GAME_PICKS_CAR { 0 } else { bucket.max(0) },
        "Segments": bounded(&model.segments, MAX_SEGMENTS),
        "Curves": bounded(&model.curves, MAX_CURVES),
        "UserInfo": model.info,
        "UserSkillLevel": skill,
        "wristbandLevel": wristband,
    })
}

/// Another player's drivatar in a race of `bucket` (0 for the cache) at `skill` (the caller's
/// difficulty, already within `0..=`[`MAX_SKILL_LEVEL`]); `None` when its stored model does not
/// decode.
pub fn racing_info(c: &Candidate, bucket: i64, skill: i64) -> Option<Json> {
    let Some(model) = decode_model(&c.model) else {
        tracing::warn!(profile_id = %c.profile_id, "a stored drivatar model does not decode");
        return None;
    };
    Some(info(
        forza::game_tag(&c.name),
        c.friend,
        c.xuid,
        c.car_id,
        bucket,
        &model,
        skill,
        c.wristband,
    ))
}

/// The race's `playerData`: the caller's own drivatar with no car (as the Beta's); without a
/// stored drivatar (or one that does not decode), the Beta's minimal `{GamerTag, Xuid,
/// UserSkillLevel}`. Either way `UserSkillLevel` = `skillSelection` and `wristbandLevel` = the
/// caller's own XP (0 without).
pub fn player_data(own: &Own, bucket: i64, skill_selection: i64) -> Json {
    let gamertag = match own.name.as_deref() {
        Some(name) => forza::game_tag(name),
        None => forza::generated_gamertag(u64::try_from(own.xuid).unwrap_or(0)),
    };
    let wristband = own.wristband.unwrap_or(0);
    match own.model.as_deref().and_then(decode_model) {
        Some(model) => info(
            gamertag,
            false,
            own.xuid,
            0,
            bucket,
            &model,
            skill_selection,
            wristband,
        ),
        None => json!({
            "GamerTag": gamertag,
            "Xuid": own.xuid,
            "UserSkillLevel": skill_selection,
            "wristbandLevel": wristband,
        }),
    }
}

/// `currentTime` as the game's reader parses it (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub fn current_time(millis: i64) -> String {
    fi_protocol::instant::format_unix_ms(millis)
        .and_then(|t| t.get(..23).map(|t| format!("{t}Z")))
        .filter(|t| fi_xtsw::valid_datetime(t))
        .unwrap_or_else(|| fi_xtsw::DEFAULT_DATETIME.to_owned())
}

/// The size of `reply` as answered (envelope included); `usize::MAX` if it does not encode.
fn encoded_size(method: &str, reply: &Json) -> usize {
    encode_results(Schema::fh2(), CLASS, method, reply)
        .map_or(usize::MAX, |b| ok_envelope(&b).len())
}

/// The leading `infos` whose reply, `empty` (the reply without them) plus each one's encoded
/// DrivatarRacingInfo and the envelope, stays within `budget` bytes. The xtsw encoding is
/// positional with a fixed-width count, so the reply's size is exactly that sum. An info that
/// does not encode is left out.
fn fit(method: &str, empty: &Json, infos: Vec<Json>, budget: usize) -> Vec<Json> {
    let schema = Schema::fh2();
    let mut used = encoded_size(method, empty);
    let mut out = Vec::with_capacity(infos.len());
    for info in infos {
        let Ok(bytes) = schema.encode("DrivatarRacingInfo", &info, &Limits::RESULTS) else {
            continue;
        };
        if used.saturating_add(bytes.len()) > budget {
            break;
        }
        used += bytes.len();
        out.push(info);
    }
    out
}

/// GetDrivatarInformationForRace, within [`RACE_BUDGET`].
pub fn race_reply(player: Json, infos: Vec<Json>, now_millis: i64) -> Json {
    let mut reply = json!({
        "playerData": player,
        "drivatarInfos": [],
        "currentTime": current_time(now_millis),
    });
    reply["drivatarInfos"] = Json::Array(fit(RACE, &reply, infos, RACE_BUDGET));
    reply
}

/// GetDrivatarsForCache, within [`CACHE_BUDGET`].
pub fn cache_reply(infos: Vec<Json>) -> Json {
    let empty = json!({ "drivatarInfos": [] });
    json!({ "drivatarInfos": fit(CACHE, &empty, infos, CACHE_BUDGET) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_domain::UnixMs;
    use fi_xtsw::decode_results;
    use uuid::Uuid;

    fn base(time: f64) -> Json {
        json!({"timeRatioMean": 1.25, "timeRatioVariance": 0.0, "timeRatioTime": time,
               "cornerCuttingMean": 0.0, "cornerCuttingTime": 0.0, "goingWideMean": 0.0,
               "goingWideTime": 0.0, "goingOffMean": 0.0, "goingOffTime": 0.0})
    }

    fn model(segments: usize, curves: usize) -> Model {
        Model {
            info: checked_info(&json!({"base": base(12.5), "lastUpdateTime": 33.0,
                "lineSelectorCar": 1.0, "sideswipeTime": 4.0, "racingLineOffsetMean": 0.25})),
            curves: (0..curves)
                .map(|i| json!({"base": base(1.0), "curveTypeId": i}))
                .collect(),
            segments: (0..segments)
                .map(|i| json!({"base": base(2.0), "localSegmentId": i, "trackId": 31}))
                .collect(),
        }
    }

    fn bytes(m: &Model) -> Vec<u8> {
        encode_model(m).unwrap()
    }

    fn candidate(n: i64) -> Candidate {
        Candidate {
            profile_id: Uuid::now_v7(),
            xuid: 2_535_100_000_000_100 + n,
            name: "Driver".into(),
            friend: n == 0,
            car_id: 289,
            wristband: 125_000,
            model: bytes(&model(MAX_SEGMENTS, MAX_CURVES)),
        }
    }

    fn own(model: Option<Model>, wristband: Option<u32>) -> Own {
        Own {
            xuid: 2_535_100_000_000_001,
            name: Some("Tester".into()),
            model: model.as_ref().map(bytes),
            wristband,
            difficulty: None,
            now: UnixMs::from_millis(1_792_000_000_123),
        }
    }

    fn round_trip(method: &str, reply: &Json) -> (usize, Json) {
        let schema = Schema::fh2();
        let bytes = encode_results(schema, CLASS, method, reply)
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        let size = ok_envelope(&bytes).len();
        let json = decode_results(schema, CLASS, method, &bytes)
            .unwrap()
            .to_json();
        (size, json)
    }

    /// Every reply of the module encodes with the game's schema, at the largest sizes served
    /// (within its byte budget).
    #[test]
    fn every_reply_encodes_with_the_schema() {
        let schema = Schema::fh2();
        for method in [SAVE, RACE, CACHE] {
            assert_eq!(served(false, "Drivatar", method), Some(Served::Store));
            assert!(schema.method(CLASS, method).is_some(), "{method}");
        }
        encode_results(schema, CLASS, SAVE, &json!({})).unwrap();
        let full: Vec<Json> = (0..i64::try_from(MAX_FOR_RACE).unwrap())
            .map(|n| racing_info(&candidate(n), 17, 3).unwrap())
            .collect();
        for player in [
            player_data(&own(Some(model(MAX_SEGMENTS, MAX_CURVES)), None), 17, 1),
            player_data(&own(None, None), 0, 1),
            player_data(&own(None, Some(9)), -1, 2),
        ] {
            let (size, r) = round_trip(RACE, &race_reply(player, full.clone(), 1_792_000_000_123));
            assert!(size <= RACE_BUDGET, "{size}");
            assert!(!r["drivatarInfos"].as_array().unwrap().is_empty());
            assert_eq!(r["currentTime"], "2026-10-14T17:46:40.123Z");
            assert_eq!(
                r["drivatarInfos"][1]["Segments"].as_array().unwrap().len(),
                MAX_SEGMENTS
            );
        }
        let cache: Vec<Json> = (0..i64::try_from(MAX_FOR_CACHE).unwrap())
            .map(|n| racing_info(&candidate(n), 0, 1).unwrap())
            .collect();
        let (size, _) = round_trip(CACHE, &cache_reply(cache));
        assert!(size <= CACHE_BUDGET, "{size}");
        assert_eq!(
            round_trip(CACHE, &cache_reply(Vec::new())).1["drivatarInfos"],
            json!([])
        );
        // Not served: the fourth Drivatar method and anything Xls.
        assert_eq!(
            served(false, "Drivatar", "GetDrivatarInformationForTrack"),
            None
        );
        assert_eq!(served(true, "Drivatar", CACHE), None);
    }

    /// The stored encoding round-trips the checked model, a full one stays under the column's
    /// ceiling, and [`RACING_OVERHEAD`] bounds what a served drivatar adds to it (the store's
    /// byte budget relies on it).
    #[test]
    fn the_stored_encoding_and_the_overhead_of_a_served_drivatar() {
        let schema = Schema::fh2();
        let mut overheads = Vec::new();
        for (segments, curves) in [(0, 0), (1, 0), (0, 1), (17, 3), (MAX_SEGMENTS, MAX_CURVES)] {
            let m = model(segments, curves);
            let stored = bytes(&m);
            assert_eq!(decode_model(&stored), Some(m.clone()));
            assert!(stored.len() <= MAX_MODEL_BYTES);
            let mut c = candidate(1);
            c.name = "Abcdefghij".into();
            c.model = stored.clone();
            let served = schema
                .encode(
                    "DrivatarRacingInfo",
                    &racing_info(&c, 17, 7).unwrap(),
                    &Limits::RESULTS,
                )
                .unwrap();
            overheads.push(served.len() - stored.len());
        }
        let full = bytes(&model(MAX_SEGMENTS, MAX_CURVES)).len();
        println!("full model {full} bytes, overheads {overheads:?}");
        assert!(full < 10_000, "{full}");
        assert!(
            overheads.iter().all(|o| *o <= RACING_OVERHEAD),
            "{overheads:?}"
        );
        assert!(
            overheads.iter().all(|o| *o + 32 > RACING_OVERHEAD),
            "the bound is tight: {overheads:?}"
        );
        // Bytes that are no model are left out, never served.
        assert_eq!(decode_model(b"\x00\x01"), None);
        let mut c = candidate(2);
        c.model = vec![1, 2, 3];
        assert_eq!(racing_info(&c, 0, 1), None);
        let mut o = own(None, Some(5));
        o.model = Some(vec![9; 7]);
        assert_eq!(player_data(&o, 0, 1)["Segments"], Json::Null);
    }

    /// Full drivatars (200 segments, 20 curves) are about 9.8 KB each: a race keeps the leading
    /// ones that fit 512 KiB, the cache those that fit 1 MiB; small ones all fit. None is cut.
    #[test]
    fn replies_keep_the_leading_drivatars_within_their_budget() {
        let schema = Schema::fh2();
        let one = schema
            .encode(
                "DrivatarRacingInfo",
                &racing_info(&candidate(1), 17, 3).unwrap(),
                &Limits::RESULTS,
            )
            .unwrap()
            .len();
        assert!((9_000..10_500).contains(&one), "{one}");
        let full: Vec<Json> = (0..i64::try_from(MAX_FOR_RACE).unwrap())
            .map(|n| racing_info(&candidate(n), 17, 3).unwrap())
            .collect();
        let (size, r) = round_trip(
            RACE,
            &race_reply(
                player_data(&own(Some(model(MAX_SEGMENTS, MAX_CURVES)), None), 17, 1),
                full.clone(),
                0,
            ),
        );
        let kept = r["drivatarInfos"].as_array().unwrap();
        assert!(size <= RACE_BUDGET && size + one > RACE_BUDGET, "{size}");
        assert!(kept.len() < MAX_FOR_RACE);
        // The leading ones, in order, whole.
        for (i, info) in kept.iter().enumerate() {
            assert_eq!(info["Xuid"], full[i]["Xuid"]);
            assert_eq!(info["Segments"].as_array().unwrap().len(), MAX_SEGMENTS);
        }
        let cache: Vec<Json> = (0..i64::try_from(MAX_FOR_CACHE).unwrap())
            .map(|n| racing_info(&candidate(n), 0, 1).unwrap())
            .collect();
        let (size, r) = round_trip(CACHE, &cache_reply(cache));
        let kept = r["drivatarInfos"].as_array().unwrap().len();
        assert!(size <= CACHE_BUDGET && size + one > CACHE_BUDGET, "{size}");
        assert!((100..MAX_FOR_CACHE).contains(&kept), "{kept}");
        // Drivatars without segments (what the game uploads today): all of them fit.
        let mut small = candidate(3);
        small.model = bytes(&model(0, 0));
        let all: Vec<Json> = (0..MAX_FOR_CACHE)
            .map(|_| racing_info(&small, 0, 1).unwrap())
            .collect();
        assert_eq!(
            cache_reply(all)["drivatarInfos"].as_array().unwrap().len(),
            MAX_FOR_CACHE
        );
    }

    #[test]
    fn a_racing_info_carries_the_owners_xp_and_the_callers_skill() {
        let c = candidate(7);
        let (_, r) = round_trip(CACHE, &cache_reply(vec![racing_info(&c, 0, 4).unwrap()]));
        let i = &r["drivatarInfos"][0];
        assert_eq!(i["GamerTag"], "Driver");
        assert_eq!(i["Xuid"], 2_535_100_000_000_107u64);
        assert_eq!(i["IsFriend"], false);
        assert_eq!(
            (&i["CarId"], &i["LiveryId"], &i["TuneId"], &i["BucketId"]),
            (&json!(289), &json!(0), &json!(0), &json!(0))
        );
        assert_eq!(
            (&i["UserSkillLevel"], &i["wristbandLevel"]),
            (&json!(4), &json!(125_000))
        );
        assert_eq!(i["UserInfo"]["sideswipeTime"], 4.0);
        assert_eq!(i["Curves"].as_array().unwrap().len(), MAX_CURVES);
        // A race echoes its bucket; "any" (0 or below) is 0.
        assert_eq!(racing_info(&c, 17, 1).unwrap()["BucketId"], 17);
        assert_eq!(racing_info(&c, -1, 1).unwrap()["BucketId"], 0);
        // The caller's difficulty, bounded.
        assert_eq!([-5, 0, 3, 7, 8, 9_999].map(skill_level), [0, 0, 3, 7, 7, 7]);
    }

    #[test]
    fn player_data_is_the_callers_own_drivatar_or_the_minimal_one() {
        let p = player_data(&own(Some(model(3, 1)), Some(900)), 17, 2);
        assert_eq!(
            (&p["GamerTag"], &p["CarId"], &p["BucketId"]),
            (&json!("Tester"), &json!(0), &json!(17))
        );
        assert_eq!(
            (&p["UserSkillLevel"], &p["wristbandLevel"]),
            (&json!(2), &json!(900))
        );
        assert_eq!(p["Segments"].as_array().unwrap().len(), 3);
        // Skill and XP are independent: no XP yet is 0, the skill is still the selection.
        let p = player_data(&own(Some(model(0, 0)), None), 0, 5);
        assert_eq!(
            (&p["UserSkillLevel"], &p["wristbandLevel"]),
            (&json!(5), &json!(0))
        );
        // Without a stored drivatar: the Beta's minimal struct, with the caller's XP.
        assert_eq!(
            player_data(&own(None, Some(900)), 0, 1),
            json!({"GamerTag": "Tester", "Xuid": 2_535_100_000_000_001u64,
                   "UserSkillLevel": 1, "wristbandLevel": 900})
        );
        // An erased profile keeps a generated name.
        let mut erased = own(None, None);
        erased.name = None;
        assert_eq!(
            player_data(&erased, 0, 1)["GamerTag"],
            forza::generated_gamertag(2_535_100_000_000_001)
        );
    }

    fn stored(params: Json) -> (Model, bool) {
        let up = upload_of(params).unwrap();
        (decode_model(&up.model).unwrap(), up.trained)
    }

    #[test]
    fn trained_means_an_observation_time_above_zero() {
        // The Beta's fixture and a model of defaults only.
        assert!(!stored(json!({"info": {}, "curveTypes": [], "segmentInformation": []})).1);
        assert!(!stored(json!({})).1);
        let defaults = json!({"info": {"base": base(0.0), "lastUpdateTime": 812.0,
            "lineSelectorCar": 1.0, "lineSelectorOffLine": 0.5, "racingLineOffsetMean": 0.3}});
        assert!(!stored(defaults.clone()).1);
        // Each observation time alone trains it.
        for k in INFO_TIMES {
            let mut u = defaults.clone();
            u["info"][k] = json!(0.5);
            assert!(stored(u).1, "{k}");
        }
        for k in BASE_TIMES {
            let mut u = defaults.clone();
            u["info"]["base"][k] = json!(0.5);
            assert!(stored(u).1, "{k}");
        }
        let mut u = defaults.clone();
        u["segmentInformation"] = json!([{"base": base(3.0), "localSegmentId": 1, "trackId": 31}]);
        assert!(stored(u).1);
        let mut u = defaults.clone();
        u["curveTypes"] = json!([{"base": base(3.0), "curveTypeId": 1}]);
        assert!(stored(u).1);
        // A trained segment on a track the catalog does not list is dropped: not training.
        let mut u = defaults.clone();
        u["segmentInformation"] =
            json!([{"base": base(3.0), "localSegmentId": 1, "trackId": 99_999}]);
        assert!(!stored(u).1);
        // Negative or non-finite times are not training, and non-finite numbers are stored as 0.
        let mut u = defaults;
        u["info"]["sideswipeTime"] = json!(-2.0);
        u["info"]["temperamentTime"] = json!("Infinity");
        u["info"]["base"]["timeRatioMean"] = json!("NaN");
        let (m, trained) = stored(u);
        assert!(!trained);
        assert_eq!(m.info["temperamentTime"], 0.0);
        assert_eq!(m.info["sideswipeTime"], 0.0);
        assert_eq!(m.info["base"]["timeRatioMean"], 0.0);
    }

    /// What reaches other players' games is within bounds: numbers clamped by field kind, the
    /// model rebuilt with exactly its schema fields, invalid segments and curves dropped.
    #[test]
    fn uploads_are_checked_before_they_are_stored() {
        let extreme = 3.402_823_466e38;
        let mut info = json!({"base": {}, "lineSelectorCar": -extreme, "unknownField": 5});
        for k in BASE_FIELDS {
            info["base"][k] = json!(extreme);
        }
        for k in INFO_FIELDS
            .iter()
            .filter(|k| !k.starts_with("lineSelector"))
        {
            info[*k] = json!(-extreme);
        }
        let checked = checked_info(&info);
        assert_eq!(checked.as_object().unwrap().len(), INFO_FIELDS.len() + 1);
        assert_eq!(checked["unknownField"], Json::Null);
        let (m, trained) = stored(json!({"info": info}));
        let i = &m.info;
        assert_eq!(i["base"].as_object().unwrap().len(), BASE_FIELDS.len());
        assert_eq!(
            (&i["base"]["timeRatioTime"], &i["base"]["timeRatioVariance"]),
            (&json!(MAX_TIME), &json!(MAX_VARIANCE))
        );
        assert_eq!(i["base"]["timeRatioMean"], json!(MAX_MEAN));
        assert_eq!(
            (&i["sideswipeTime"], &i["racingLineOffsetVariance"]),
            (&json!(0.0), &json!(0.0)),
            "times and variances are never negative"
        );
        assert_eq!(i["temperamentMean"], json!(-MAX_MEAN));
        assert_eq!(i["lineSelectorCar"], json!(-MAX_OTHER));
        assert!(trained, "a clamped time above 0 is still training");
        // Missing fields are 0; values within bounds are kept as sent.
        let (m, _) = stored(json!({"info": {"sideswipeMean": 0.25, "lastUpdateTime": 4096.5}}));
        assert_eq!(
            (
                &m.info["sideswipeMean"],
                &m.info["lastUpdateTime"],
                &m.info["blockingMean"]
            ),
            (&json!(0.25), &json!(4096.5), &json!(0.0))
        );
        // Segments: a catalog track, an id in range; curves: a type in range.
        let seg = |track: Json, id: Json| json!({"base": base(1.0), "localSegmentId": id, "trackId": track});
        let (m, _) = stored(json!({
            "segmentInformation": [
                seg(json!(31), json!(5)), seg(json!(99_999), json!(5)), seg(json!(-1), json!(5)),
                seg(json!(31), json!(-1)), seg(json!(31), json!(MAX_SEGMENT_ID + 1)),
                seg(json!(31), json!(MAX_SEGMENT_ID)), seg(json!("31"), json!(1)),
            ],
            "curveTypes": [
                {"base": base(1.0), "curveTypeId": 0}, {"base": base(1.0), "curveTypeId": -3},
                {"base": base(1.0), "curveTypeId": MAX_CURVE_TYPE + 1},
                {"base": {"timeRatioTime": -1.0}, "curveTypeId": MAX_CURVE_TYPE},
            ],
        }));
        let ids: Vec<&Json> = m
            .segments
            .as_array()
            .unwrap()
            .iter()
            .map(|s| &s["localSegmentId"])
            .collect();
        assert_eq!(ids, [&json!(5), &json!(MAX_SEGMENT_ID)]);
        let curves = m.curves.as_array().unwrap();
        assert_eq!(curves.len(), 2);
        assert_eq!(curves[1]["base"]["timeRatioTime"], 0.0);
        // The field lists are the schema's.
        let schema = Schema::fh2();
        let names = |ty: &str| -> Vec<String> {
            schema
                .type_def(ty)
                .unwrap()
                .fields
                .iter()
                .map(|f| f.name.clone())
                .collect()
        };
        assert_eq!(names("BaseDrivatarInfo"), BASE_FIELDS);
        let mut info = vec!["base".to_owned()];
        info.extend(INFO_FIELDS.iter().map(|s| (*s).to_owned()));
        assert_eq!(names("DrivatarInfo"), info);
        assert_eq!(names("SegmentInfo"), ["base", "localSegmentId", "trackId"]);
        assert_eq!(names("CurveInfo"), ["base", "curveTypeId"]);
    }

    #[test]
    fn an_upload_keeps_the_first_valid_curves_and_segments_the_game_can_take_back() {
        let m = model(MAX_SEGMENTS + 50, MAX_CURVES + 5);
        let mut segments = vec![json!({"base": base(2.0), "localSegmentId": 1, "trackId": 99_999})];
        segments.extend(m.segments.as_array().unwrap().iter().cloned());
        let (s, trained) =
            stored(json!({"info": m.info, "curveTypes": m.curves, "segmentInformation": segments}));
        assert!(trained);
        assert_eq!(s.curves.as_array().unwrap().len(), MAX_CURVES);
        assert_eq!(s.segments.as_array().unwrap().len(), MAX_SEGMENTS);
        assert_eq!(s.segments[0]["localSegmentId"], 0);
        assert_eq!(s.curves[MAX_CURVES - 1]["curveTypeId"], MAX_CURVES - 1);
    }

    #[test]
    fn race_parameters_are_bounded() {
        assert_eq!(count(&json!({"numberOfDrivatars": 11}), MAX_FOR_RACE), 11);
        assert_eq!(count(&json!({"numberOfDrivatars": 900}), MAX_FOR_RACE), 64);
        assert_eq!(
            count(&json!({"numberOfDrivatars": 900}), MAX_FOR_CACHE),
            200
        );
        assert_eq!(count(&json!({"numberOfDrivatars": -3}), MAX_FOR_RACE), 0);
        assert_eq!(count(&json!({}), MAX_FOR_RACE), 0);
        let skip = skipped(&json!({
            "cachedDrivatars": [2_535_100_000_000_010u64, 2_535_100_000_000_011u64, 2_535_100_000_000_012u64],
            "cachedDrivatarsNewCars": [2_535_100_000_000_011u64],
        }));
        assert_eq!(skip, [2_535_100_000_000_010, 2_535_100_000_000_012]);
        let many: Vec<u64> = (0..5000).map(|i| 2_535_100_000_000_000 + i).collect();
        assert_eq!(
            skipped(&json!({"cachedDrivatars": many})).len(),
            store::MAX_SKIPPED
        );
    }

    #[test]
    fn cars_by_bucket_and_any_bucket() {
        let c = Catalog::fh2();
        let cars = cars_for(c, 17);
        assert!(!cars.is_empty());
        assert_eq!(Some(cars.as_slice()), Some(c.bucket_cars(17)));
        for bucket in [0, -1] {
            let any = cars_for(c, bucket);
            assert_eq!(any.len(), 336, "every bucketed car");
            assert!(any.iter().all(|car| c.bucket_of(*car).is_some()));
        }
        assert_eq!(bucketed_cars(c).len(), 336);
        assert_eq!(cars_for(c, 9_999), Vec::<i64>::new());
    }

    /// BUG-0003 (2026-10-31): an S1 830 race of bucket 12 (the report's: the player in a RUF CTR
    /// 2, every drivatar in the bucket's first hero car, the Lamborghini Diablo SV). Owners whose
    /// garage holds a car of the bucket bring it; the others carry the game's pick (`-1`,
    /// `BucketId` 0, as every drivatar of the game's shipped cache), so the game chooses each a
    /// car for the event: never the hero car for everyone. The reply encodes and decodes with the
    /// game's schema (`CarId` is an i32: `-1` survives).
    #[test]
    fn a_bucket_race_reply_carries_real_cars_and_the_games_pick() {
        let c = Catalog::fh2();
        let bucket = 12;
        let cars = race_cars(bucket);
        let (diablo, ctr2, f40) = (325, 1515, 340);
        assert_eq!(cars.first(), Some(&diablo), "the bucket's first hero car");
        assert!(cars.contains(&ctr2) && cars.contains(&f40));
        let s1 = c.class(4).unwrap();
        assert_eq!((s1.name.as_str(), c.car(ctr2).unwrap().class_id), ("S1", 4));
        // What the store answers for eleven drivatars: two garages with a bucket-12 car, nine
        // without (the seeded drivatars of OD-131 among them).
        let pool: Vec<Candidate> = (0..11)
            .map(|n| {
                let mut x = candidate(n);
                x.car_id = match n {
                    0 => ctr2,
                    1 => f40,
                    _ => GAME_PICKS_CAR,
                };
                x
            })
            .collect();
        let infos: Vec<Json> = pool
            .iter()
            .filter_map(|x| racing_info(x, bucket, 1))
            .collect();
        let (_, r) = round_trip(
            RACE,
            &race_reply(player_data(&own(None, None), bucket, 1), infos, 0),
        );
        let served = r["drivatarInfos"].as_array().unwrap();
        assert_eq!(served.len(), 11);
        let pairs: Vec<(i64, i64)> = served
            .iter()
            .map(|i| {
                (
                    i["CarId"].as_i64().unwrap(),
                    i["BucketId"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(pairs[0], (ctr2, bucket), "a real car echoes the bucket");
        assert_eq!(pairs[1], (f40, bucket));
        assert!(pairs[2..].iter().all(|p| *p == (GAME_PICKS_CAR, 0)));
        // No server-chosen car repeated across the grid, and never the hero car by default.
        assert!(pairs.iter().all(|(car, _)| *car != diablo));
        let real: HashSet<i64> = pairs
            .iter()
            .map(|(car, _)| *car)
            .filter(|car| *car != GAME_PICKS_CAR)
            .collect();
        assert_eq!(real.len(), 2);
        // Every real car served is a car of the race's bucket.
        assert!(real.iter().all(|car| c.bucket_of(*car) == Some(bucket)));
        // The cache answers the same way (BucketId 0 either way).
        let i = racing_info(&pool[5], 0, 1).unwrap();
        assert_eq!((&i["CarId"], &i["BucketId"]), (&json!(-1), &json!(0)));
    }

    #[test]
    fn current_time_is_one_the_game_reads() {
        for ms in [0, 1_792_000_000_123, 253_402_300_799_999] {
            assert!(fi_xtsw::valid_datetime(&current_time(ms)), "{ms}");
        }
        assert_eq!(current_time(1_792_000_000_123), "2026-10-14T17:46:40.123Z");
        assert_eq!(current_time(i64::MAX), fi_xtsw::DEFAULT_DATETIME);
    }

    /// Whether every number of `checked` equals the one at its place in `original` (absent: 0),
    /// whatever its JSON spelling (`0` or `0.0`).
    fn same_numbers(checked: &Json, original: &Json) -> bool {
        match checked {
            Json::Object(map) => map.iter().all(|(k, v)| same_numbers(v, &original[k])),
            Json::Array(items) => items
                .iter()
                .enumerate()
                .all(|(i, v)| same_numbers(v, &original[i])),
            v => v.as_f64() == Some(original.as_f64().unwrap_or(0.0)),
        }
    }

    fn keys(v: &Json) -> Vec<String> {
        let mut k: Vec<String> = v
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        k.sort();
        k
    }

    #[derive(Default)]
    struct Tally {
        records: usize,
        decoded: usize,
        trained: usize,
        with_curves_or_segments: usize,
        models: usize,
        unchanged: usize,
        encoded: usize,
        same_fields: usize,
        beta_infos: usize,
        beta_info_shape_equal: usize,
        beta_bucket_echo: usize,
        failures: usize,
    }

    /// Golden decode (opt-in) over the Beta's captures: every captured Drivatar request decodes
    /// with the schema; every upload is trained by [`trained`] (the evidence for the threshold)
    /// and every captured model (the uploads and the Beta's served `UserInfo`) is within the
    /// bounds, so checking changes none of them; the replies built from the captured parameters
    /// (synthetic players) encode, decode, carry the Beta's top-level fields, and each
    /// DrivatarRacingInfo has the fields the Beta's had, with `BucketId` echoing the request as
    /// the Beta's did. The captures hold real identities: only counts are printed.
    ///
    /// `FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-api drivatars::tests::captured -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the Beta's captures from FI_FH2_CAPTURES_DIR (outside the repository)"]
    fn captured_drivatar_requests_decode_and_replies_keep_the_betas_shape() {
        let dir = std::env::var("FI_FH2_CAPTURES_DIR")
            .expect("set FI_FH2_CAPTURES_DIR to a directory of Beta capture .jsonl files");
        let schema = Schema::fh2();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read FI_FH2_CAPTURES_DIR")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        files.sort();
        let mut per: std::collections::BTreeMap<String, Tally> = Default::default();
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read a capture file");
            for line in text.lines() {
                let Ok(r) = serde_json::from_str::<Json>(line) else {
                    continue;
                };
                let (Some(class), Some(method)) =
                    (r["forza_class"].as_str(), r["forza_method"].as_str())
                else {
                    continue;
                };
                let key = crate::game::proxy::rpc::class_key(class);
                if key != CLASS || served(false, "Drivatar", method).is_none() {
                    continue;
                }
                let t = per.entry(method.to_owned()).or_default();
                t.records += 1;
                let body = fi_auth::b64::std_decode(r["request_body_b64"].as_str().unwrap_or(""))
                    .unwrap_or_default();
                let Ok(params) = fi_xtsw::decode_params(schema, key, method, &body) else {
                    t.failures += 1;
                    continue;
                };
                let params = params.into_json();
                t.decoded += 1;
                let bucket = params["bucketId"].as_i64().unwrap_or(0);
                let reply = match method {
                    SAVE => {
                        let (m, trained) = stored(params.clone());
                        t.trained += usize::from(trained);
                        let has = |v: &Json| v.as_array().is_some_and(|a| !a.is_empty());
                        t.with_curves_or_segments +=
                            usize::from(has(&m.curves) || has(&m.segments));
                        t.models += 1;
                        t.unchanged += usize::from(same_numbers(&m.info, &params["info"]));
                        json!({})
                    }
                    RACE => {
                        let n = count(&params, MAX_FOR_RACE).min(3);
                        let _ = skipped(&params);
                        let selection = params["skillSelection"].as_i64().unwrap_or(0);
                        race_reply(
                            player_data(&own(None, None), bucket, selection),
                            (0..n)
                                .filter_map(|i| {
                                    racing_info(
                                        &candidate(i64::try_from(i).unwrap()),
                                        bucket,
                                        skill_level(selection),
                                    )
                                })
                                .collect(),
                            1_792_000_000_123,
                        )
                    }
                    _ => cache_reply(
                        (0..count(&params, MAX_FOR_CACHE).min(3))
                            .filter_map(|i| {
                                racing_info(&candidate(i64::try_from(i).unwrap()), 0, 1)
                            })
                            .collect(),
                    ),
                };
                let Ok(bytes) = encode_results(schema, key, method, &reply) else {
                    t.failures += 1;
                    continue;
                };
                let Ok(ours) = decode_results(schema, key, method, &bytes) else {
                    t.failures += 1;
                    continue;
                };
                let ours = ours.to_json();
                t.encoded += 1;
                let beta = &r["response_xtsw"];
                if keys(beta) == keys(&reply) {
                    t.same_fields += 1;
                } else {
                    t.failures += 1;
                }
                let our_info = ours["drivatarInfos"][0].clone();
                for info in beta["drivatarInfos"].as_array().into_iter().flatten() {
                    t.beta_infos += 1;
                    if our_info.is_object() && keys(info) == keys(&our_info) {
                        t.beta_info_shape_equal += 1;
                    }
                    if info["BucketId"].as_i64() == Some(bucket.max(0)) {
                        t.beta_bucket_echo += 1;
                    }
                    t.models += 1;
                    t.unchanged += usize::from(same_numbers(
                        &checked_info(&info["UserInfo"]),
                        &info["UserInfo"],
                    ));
                }
            }
        }
        println!(
            "{:<32} {:>7} {:>7} {:>7} {:>9} {:>7} {:>9} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
            "method",
            "records",
            "decoded",
            "trained",
            "curv/seg",
            "models",
            "unchanged",
            "encoded",
            "fields",
            "infos",
            "shape",
            "bucket",
            "fail"
        );
        for (m, t) in &per {
            println!(
                "{m:<32} {:>7} {:>7} {:>7} {:>9} {:>7} {:>9} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
                t.records,
                t.decoded,
                t.trained,
                t.with_curves_or_segments,
                t.models,
                t.unchanged,
                t.encoded,
                t.same_fields,
                t.beta_infos,
                t.beta_info_shape_equal,
                t.beta_bucket_echo,
                t.failures
            );
        }
        assert!(!per.is_empty(), "no Drivatar request in the captures");
        for (m, t) in &per {
            assert_eq!(t.failures, 0, "{m}");
            assert_eq!(t.decoded, t.records, "{m}");
            assert_eq!(t.same_fields, t.records, "{m}");
            assert_eq!(
                t.unchanged, t.models,
                "{m}: the bounds change no captured model"
            );
            if m == SAVE {
                assert_eq!(t.trained, t.decoded, "every captured upload is trained");
            }
            // Where the Beta answered drivatars, ours have its fields and the same bucket echo.
            if t.beta_infos > 0 && t.encoded > 0 {
                assert_eq!(t.beta_info_shape_equal, t.beta_infos, "{m}");
                assert_eq!(t.beta_bucket_echo, t.beta_infos, "{m}");
            }
        }
    }
}
