//! The Storefront on the Forza RPC (owner decision OD-138, 2026-10-29; GAME_PLATFORM.md §3a
//! "Storefront", register CR-S74): sharing designs, vinyl groups and tunes
//! (`StorefrontLivery` / `StorefrontLayerGroup` / `StorefrontTuning.Create`), browsing and
//! downloading them (`Storefront.*`), the libraries the game syncs (`OnlineProfile` vinyl groups and
//! car setups) and the versioned content behind a shared file (`ContentVersioning` groups and
//! tunes; liveries are [`super::super::liveries`]), ported from the Beta's `forza/storefront.rs`
//! over [`fi_db::storefront`]. Served to every ticket (`forza::served`, before the Supporter check,
//! as the clubs).
//!
//! **Keywords** come from the server ([`FILE_KEYWORDS`], the Beta's list: the game's own texts);
//! the share screen offers only these, and a file keeps their ids. **Every player of both
//! modes** sees a shared file (owner: "Everything should be shared"; OD-141, CR-S75: the Storefront
//! is "Comum aos dois", Hardcore and Noob alike); its owner always. Club badges stay those of the
//! reader's mode. A downloaded design
//! or tune is not copied: the game puts the shared versioned id on its own car (as the Beta saw),
//! so a shared item is kept while its file lives. A **download** is counted on the full retrieve
//! of a shared item (`RetrieveVersionedLayerGroupById`, `RetrieveVersionedTuneById`, and
//! `RetrieveVersionedLiveryById` without its thumbnail, [`super::super::liveries`]; FH2-S17: the
//! game was never seen calling `AddReferenceTo*`) and on `AddReferenceTo*`, once per account
//! through the same mark, never by the owner's account.
//!
//! **Text** the game posts (titles, descriptions, library names) is user content: cleaned as
//! messages are ([`inbox::clean_text`]) and bounded (title 16, description 256, names 64
//! characters). Never logged.

use super::super::forza::{self, Reply, Served, XlsError};
use super::super::liveries::livery_id;
use super::super::rpc::{self, Ctx};
use super::boards::game_time;
use super::ghosts::unhex;
use super::inbox::clean_text;
use crate::errors::ApiError;
use fi_db::clubs::{self, Badge};
use fi_db::limits::{self, Hit, Limit};
use fi_db::storefront::{
    self as sf, Edit, File, Kind, Mark, Marked, NewFile, Query, Setup, Shared, Tune, Versioned,
    Viewer,
};
use fi_gamedb::Catalog;
use fi_protocol::ErrorCode;
use fi_xtsw::truncate_utf16;
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

/// `Storefront.*` methods served.
const STOREFRONT: [&str; 20] = [
    "AddFavoriteUser",
    "BatchIncrementViews",
    "ChangeFileDescription",
    "ChangeFileTitle",
    "GetCuratedContent",
    "GetFileDataIfShared",
    "GetFileHeavyWeightData",
    "GetFileKeywords",
    "GetMyFileList",
    "IncrementLikes",
    "IncrementUsage",
    "IncrementViews",
    "RemoveFavoriteUser",
    "RemoveFile",
    "ReportContent",
    "SearchLayerGroupFiles",
    "SearchLiveryFiles",
    "SearchTuningFiles",
    "SynchronizeSharedFiles",
    "ToggleFollowedCreator",
];

/// `ContentVersioning.*` methods served here (the livery ones are `liveries`', but
/// `AddReferenceToVersionedLivery`).
const VERSIONING: [&str; 10] = [
    "AddReferenceToVersionedLayerGroup",
    "AddReferenceToVersionedLivery",
    "AddReferenceToVersionedTune",
    "CreateVersionedLayerGroup",
    "CreateVersionedTune",
    "CreateVersionedTuneFromStructs",
    "RemoveReferenceToVersionedLayerGroup",
    "RemoveReferenceToVersionedTune",
    "RetrieveVersionedLayerGroupById",
    "RetrieveVersionedTuneById",
];

/// `OnlineProfile.*` library methods served.
const LIBRARY: [&str; 8] = [
    "DeleteCarSetup",
    "DeleteLivery",
    "DeleteLiveryGroup",
    "RenameCarSetup",
    "RenameLivery",
    "RenameLiveryGroup",
    "UpdateCarSetup",
    "UpdateLiveryGroup",
];

/// `fileHeader.Type` of a design (seen in `StorefrontLivery.Create`).
pub const FILE_TYPE_LIVERY: i32 = 2;
/// A title's length, characters (the Beta's).
pub const TITLE_CHARS: usize = 16;
/// A description's length, characters (the Beta's).
pub const DESCRIPTION_CHARS: usize = 256;
/// A search's length when the game asks for none (the Beta's).
pub const DEFAULT_SEARCH: i64 = 50;
/// The most files one search, list or detail answers.
pub const MAX_RESULTS: i64 = 200;
/// The most files one `GetFileHeavyWeightData` answers (each carries its preview).
pub const MAX_HEAVY: usize = 100;
/// The files of one `GetFileHeavyWeightData` that carry their preview (the rest answer an empty
/// `PreviewPayload`): 12, a page of the game's grid.
pub const MAX_PREVIEWS: usize = 12;

/// The game data file of the Storefront keywords.
pub const KEYWORDS_FILE: &str = "fh2/storefront-keywords.json";

/// Storefront keywords: (text, bucket), the Beta's list. Bucket 0 fills KEYWORD 1 (what the file
/// is), 1 fills KEYWORD 2 (its style). Ids are the position + 1, kept stable: shared files store
/// them. The texts are the game's own (Storefront.str), so the list is the game data's
/// [`KEYWORDS_FILE`] (`{"keywords": [[text, bucket], ...]}`, in id order), read once; every file
/// type gets the same list.
pub static FILE_KEYWORDS: fi_gamedata::Lazy<Vec<(String, i32)>> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(KEYWORDS_FILE, keywords_from_json));

/// The keywords of a keywords JSON: a non-empty list of `[text, bucket]`, bucket 0 or 1.
pub fn keywords_from_json(text: &str) -> Result<Vec<(String, i32)>, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        keywords: Vec<(String, i32)>,
    }
    let raw: Raw = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if raw.keywords.is_empty()
        || raw
            .keywords
            .iter()
            .any(|(k, b)| k.is_empty() || !(0..=1).contains(b))
    {
        return Err("keywords must be a non-empty list of [text, 0 or 1]".to_owned());
    }
    Ok(raw.keywords)
}

/// Shares per profile: 30 an hour.
pub const SHARE_LIMIT: Limit = Limit {
    name: "storefront.share",
    max: 30,
    window_s: 3600,
};
/// Library and versioned writes per profile (groups, setups, tunes): 240 an hour.
pub const WRITE_LIMIT: Limit = Limit {
    name: "storefront.write",
    max: 240,
    window_s: 3600,
};
/// Reports per profile: 20 an hour (the Beta's); past it a report is answered and not counted.
pub const REPORT_LIMIT: Limit = Limit {
    name: "storefront.report",
    max: 20,
    window_s: 3600,
};
/// Reads per profile (searches, curated content, my files, if-shared, view counts): 120 a minute,
/// well above play (the game asks a few per screen); past it answered empty.
pub const READ_LIMIT: Limit = Limit {
    name: "storefront.read",
    max: 120,
    window_s: 60,
};
/// `GetFileHeavyWeightData` per profile (each carries previews): 30 a minute; past it answered
/// empty.
pub const HEAVY_LIMIT: Limit = Limit {
    name: "storefront.heavy",
    max: 30,
    window_s: 60,
};

/// How `(xls, class, method)` is answered; `None`: not this module's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    match (xls, short, method) {
        (false, "Storefront", m) if STOREFRONT.contains(&m) => Some(Served::Store),
        (false, "StorefrontLivery" | "StorefrontLayerGroup" | "StorefrontTuning", "Create") => {
            Some(Served::Store)
        }
        (false, "ContentVersioning", m) if VERSIONING.contains(&m) => Some(Served::Store),
        (false, "OnlineProfile", m) if LIBRARY.contains(&m) => Some(Served::Store),
        _ => None,
    }
}

fn err(message: &'static str) -> XlsError {
    XlsError { code: 1, message }
}

/// The caller as the Storefront sees it.
pub(crate) fn viewer<'a>(ctx: &'a Ctx<'a>) -> Viewer<'a> {
    let v = ctx.viewer();
    Viewer {
        game: v.game,
        mode: v.mode,
        profile_id: v.profile_id,
        account_id: v.account_id,
    }
}

/// Counts the full retrieve of a shared item as a download of the file sharing it (FH2-S17), in a
/// savepoint: a database error is logged (ids only) and rolled back to before the count, and the
/// content already read is still answered and the request's transaction still commits.
pub(crate) async fn count_download(conn: &mut PgConnection, v: Viewer<'_>, kind: Kind, id: i64) {
    let kind_str = kind.as_str();
    match sqlx::Connection::begin(&mut *conn).await {
        Ok(mut sp) => match sf::referenced_in(&mut sp, v, kind, id).await {
            Ok(_) => {
                if let Err(e) = sp.commit().await {
                    tracing::warn!(error = %e, kind = kind_str, item_id = id,
                        "a storefront download not counted (savepoint release)");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, kind = kind_str, item_id = id,
                    "a storefront download not counted (database)");
                if let Err(e) = sp.rollback().await {
                    tracing::warn!(error = %e, kind = kind_str, item_id = id,
                        "a storefront download savepoint not rolled back");
                }
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, kind = kind_str, item_id = id,
                "a storefront download not counted (savepoint)");
        }
    }
}

/// Whether a moderator blocked the caller's account from uploading liveries (`livery_upload_block`):
/// its Storefront uploads are refused as its livery uploads are (OD-138 follow-up).
async fn blocked(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<bool, ApiError> {
    Ok(fi_db::liveries::upload_blocked_in(conn, ctx.req.ticket.live.account_id).await?)
}

/// A read past its bucket: answered empty (`None`), never a failed call the hook would retry.
async fn allowed(conn: &mut PgConnection, ctx: &Ctx<'_>, limit: &Limit) -> Result<bool, ApiError> {
    let over = bucket(conn, ctx, limit).await?.is_some();
    if over {
        tracing::info!(
            session = %ctx.req.ticket.live.session_id,
            limit = limit.name,
            "a storefront read past its bucket"
        );
    }
    Ok(!over)
}

fn i32_of(v: &Json) -> i32 {
    v.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(-1)
}

fn i64_of(v: &Json) -> i64 {
    v.as_i64().unwrap_or(-1)
}

/// A positive filter value, else `None` ("any").
fn filter(v: &Json) -> Option<i64> {
    v.as_i64().filter(|n| *n > 0)
}

/// A list's length: the asked one above 0, else `default`, at most `max`.
fn wanted(v: &Json, default: i64, max: i64) -> i64 {
    v.as_i64().filter(|n| *n > 0).unwrap_or(default).min(max)
}

/// Player text kept and shown to others: cleaned to at most `chars` characters and UTF-16 units.
fn text(v: &Json, chars: usize) -> String {
    truncate_utf16(&clean_text(v.as_str().unwrap_or(""), chars), chars).to_owned()
}

/// A blob of the call (hex in the decoded tree), or empty.
fn blob(v: &Json) -> Vec<u8> {
    v.as_str().and_then(unhex).unwrap_or_default()
}

/// A keyword id a file keeps: one of [`FILE_KEYWORDS`] of `bucket`, else 0.
pub fn keyword_id(v: &Json, bucket: i32) -> i16 {
    let id = v.as_i64().unwrap_or(0);
    let ok = usize::try_from(id - 1)
        .ok()
        .and_then(|i| FILE_KEYWORDS.get(i))
        .is_some_and(|k| k.1 == bucket);
    if ok {
        i16::try_from(id).unwrap_or(0)
    } else {
        0
    }
}

/// `GetFileKeywords`'s list.
pub fn keywords(max: usize) -> Json {
    let list: Vec<Json> = FILE_KEYWORDS
        .iter()
        .enumerate()
        .take(max)
        .map(|(i, (k, bucket))| json!({ "Keyword": k, "KeywordId": i + 1, "Bucket": bucket }))
        .collect();
    json!({ "keywords": list })
}

async fn bucket(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    limit: &Limit,
) -> Result<Option<u64>, ApiError> {
    let key = limits::profile_key(limit, ctx.req.ticket.live.profile_id);
    Ok(match limits::hit_in(conn, &key, limit).await? {
        Hit::Allowed => None,
        Hit::Limited { retry_after_secs } => Some(retry_after_secs.max(1) as u64),
    })
}

/// A write past its bucket: 429 `REQUEST.RATE_LIMITED` with `Retry-After`, as the liveries'.
async fn spend(conn: &mut PgConnection, ctx: &Ctx<'_>, limit: &Limit) -> Result<(), ApiError> {
    match bucket(conn, ctx, limit).await? {
        None => Ok(()),
        Some(secs) => Err(ApiError::new(
            ErrorCode::RequestRateLimited,
            "too many storefront writes, wait Retry-After",
        )
        .retry_after(secs)),
    }
}

/// The creators of `files`: their UserData (with their club of the reader's mode) and whether the
/// reader follows them.
struct Creators {
    users: HashMap<Uuid, Json>,
    followed: Vec<Uuid>,
}

async fn creators(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    files: &[File],
) -> Result<Creators, ApiError> {
    let mut ids: Vec<Uuid> = files.iter().map(|f| f.profile_id).collect();
    ids.sort();
    ids.dedup();
    let v = ctx.viewer();
    let badges: HashMap<Uuid, Badge> = clubs::badges_in(conn, v.game, v.mode, &ids)
        .await?
        .into_iter()
        .collect();
    let mut users = HashMap::new();
    for (id, row) in sf::creators_in(conn, v.game, &ids).await? {
        let p = rpc::player_of(row);
        let user = match badges.get(&id) {
            Some(b) => forza::user_data_in(&p, b),
            None => forza::user_data(&p),
        };
        users.insert(id, user);
    }
    let followed = sf::followed_in(conn, viewer(ctx), &ids).await?;
    Ok(Creators { users, followed })
}

impl Creators {
    fn user(&self, f: &File) -> Json {
        self.users
            .get(&f.profile_id)
            .cloned()
            .unwrap_or_else(|| json!({}))
    }

    fn follows(&self, f: &File) -> bool {
        self.followed.contains(&f.profile_id)
    }
}

/// `PopularityRating`: 0 to 5 stars from likes, uses and downloads ([`sf::popularity_rating`]).
fn rating(f: &File) -> i64 {
    sf::popularity_rating(f.likes, f.uses, f.downloads)
}

/// ForzaStorefrontFileLightweightData (search results, curated content).
fn lightweight(f: &File, c: &Creators) -> Json {
    json!({
        "Id": f.id,
        "PopularityRating": rating(f),
        "Creator": c.user(f),
        "IsCreatorFollowed": c.follows(f),
        "SubmissionTime": game_time(f.created_at),
        "Ordinal": 0,
        "MethodObtained": 0,
    })
}

/// ForzaStorefrontFile (details, preview and the content to download).
fn heavyweight(f: &File, c: &Creators, preview: Option<&[u8]>) -> Json {
    json!({
        "Id": f.id,
        "Type": f.file_type,
        "Title": f.title,
        "Description": f.description,
        "SubmissionTime": game_time(f.created_at),
        "Owner": c.user(f),
        "PayloadSize": 0,
        "PreviewPayload": preview.map(fi_xtsw::hex).unwrap_or_default(),
        "Ordinal": 0,
        "IsUnderReview": f.removed,
        "VersionedContentId": f.item_id,
        "PopularityRating": rating(f),
        "IsCreatorFollowed": c.follows(f),
    })
}

/// ForzaStorefrontFileInfoForOwner (GetMyFileList).
fn for_owner(f: &File) -> Json {
    let n = |v: i32| u32::try_from(v).unwrap_or(0);
    json!({
        "Id": f.id,
        "Title": f.title,
        "Description": f.description,
        "SubmissionTime": game_time(f.created_at),
        "IsUnderReview": f.removed,
        "VersionedContentId": f.item_id,
        "PopularityRating": rating(f),
        "DownloadCount": n(f.downloads),
        "LikeCount": n(f.likes),
        "UseCount": n(f.uses),
    })
}

/// The cars a search's make and model name, from the game's catalog with the known car packs'
/// cars (`None`: any car).
pub fn cars_of(catalog: &Catalog, make: Option<i64>, model: Option<i64>) -> Option<Vec<i32>> {
    let to = |id: i64| i32::try_from(id).ok();
    match (make, model) {
        (None, None) => None,
        (None, Some(model)) => Some(to(model).into_iter().collect()),
        (Some(make), model) => Some(
            catalog
                .all_cars()
                .filter(|c| c.make_id == make && model.is_none_or(|m| m == c.id))
                .filter_map(|c| to(c.id))
                .collect(),
        ),
    }
}

/// The cars of a class, from the game's catalog (known car packs' cars included).
pub fn class_cars(catalog: &Catalog, class: i64) -> Vec<i32> {
    catalog
        .all_cars()
        .filter(|c| c.class_id == class)
        .filter_map(|c| i32::try_from(c.id).ok())
        .collect()
}

/// Search*Files: the visible files of `kind` that match the search (values <= 0 or empty mean
/// "any"), newest first.
async fn search(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    kind: Kind,
    params: &Json,
) -> Result<Reply, ApiError> {
    if !allowed(conn, ctx, &READ_LIMIT).await? {
        return Ok(Ok(json!({ "searchResults": [] })));
    }
    let s = &params["searchParameters"];
    let catalog = Catalog::fh2();
    let class_id = filter(&s["ClassId"]);
    let text_query = s["ManualSearchKeywords"].as_str().unwrap_or("");
    let gamertag = s["GamerTag"].as_str().unwrap_or("");
    let q = Query {
        kind: Some(kind),
        cars: cars_of(catalog, filter(&s["MakeId"]), filter(&s["ModelId"])),
        class_id,
        class_cars: class_id.map_or_else(Vec::new, |c| class_cars(catalog, c)),
        keyword_one: filter(&s["KeywordOneId"]).and_then(|k| i16::try_from(k).ok()),
        keyword_two: filter(&s["KeywordTwoId"]).and_then(|k| i16::try_from(k).ok()),
        track_id: filter(&s["TrackId"]),
        max_layers: filter(&s["NumberOfLayers"]).and_then(|k| i16::try_from(k).ok()),
        text: text_query,
        gamertag,
        limit: wanted(&params["maxFiles"], DEFAULT_SEARCH, MAX_RESULTS),
        ..Query::default()
    };
    let files = sf::files_in(conn, viewer(ctx), &q).await?;
    let c = creators(conn, ctx, &files).await?;
    let results: Vec<Json> = files.iter().map(|f| lightweight(f, &c)).collect();
    Ok(Ok(json!({ "searchResults": results })))
}

/// The file ids of a call (`fileIds`), at most `max`.
fn file_ids(params: &Json, max: usize) -> Vec<i32> {
    params["fileIds"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_i64().and_then(|n| i32::try_from(n).ok()))
                .take(max)
                .collect()
        })
        .unwrap_or_default()
}

/// A `Storefront*.Create`: the header and the caller's versioned item it shares.
/// A gated shared tune's search fields (OD-178 and its follow-up): `MakeId` and `ModelId` (the car
/// id, as the search's [`cars_of`] reads it) from the catalog by the tune's own car (a known car
/// pack's car has its own make; an unknown one: `ModelId` only, `MakeId` null); `ClassId` (already checked as a catalog class),
/// `EnvironmentId`, `TrackId` and `PerformanceIndex` as the game sent them.
pub fn tune_meta(catalog: &Catalog, car: i64, sent: &Json) -> Json {
    let known = catalog.car(car);
    json!({
        "MakeId": known.map(|c| c.make_id),
        "ModelId": car,
        "ClassId": sent["ClassId"],
        "EnvironmentId": sent["EnvironmentId"],
        "TrackId": sent["TrackId"],
        "PerformanceIndex": sent["PerformanceIndex"],
    })
}

async fn share(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    kind: Kind,
    params: &Json,
    item_id: i64,
    meta: &Json,
) -> Result<Reply, ApiError> {
    let missing = match kind {
        Kind::Livery => "no such livery",
        Kind::Group => "no such vinyl group",
        Kind::Tune => "no such tune",
    };
    // A blocked account's share is refused as its CreateVersionedLivery is.
    if blocked(conn, ctx).await? {
        return Ok(Err(err(missing)));
    }
    // OD-178: the item's own car (a group has none): a gated share of an item for a car the game
    // cannot have or a traffic car (one kept before the rule) is refused as a missing item.
    let car = sf::own_item_in(conn, viewer(ctx), kind, item_id)
        .await?
        .map(|(_, car)| i64::from(car));
    let method = match kind {
        Kind::Livery => "StorefrontLivery.Create",
        Kind::Group => "StorefrontLayerGroup.Create",
        Kind::Tune => "StorefrontTuning.Create",
    };
    if let Some(car) = car.filter(|_| kind != Kind::Group)
        && rpc::creation_refused(conn, ctx, method, car).await?
    {
        return Ok(Err(err(missing)));
    }
    // OD-178 follow-up, gated tickets only: a tune's make and model are the catalog's for its car;
    // its class is the client's, refused when not a catalog class. A noob listing is as sent.
    let meta = match (kind, car) {
        (Kind::Tune, Some(car)) if rpc::gated(ctx) => {
            let class = meta["ClassId"].as_i64().unwrap_or(-1);
            if let Err(r) = fi_gamedb::gates::tune_class(Catalog::fh2(), class) {
                rpc::refuse_creation(conn, ctx, method, car, &r).await?;
                return Ok(Err(err(missing)));
            }
            tune_meta(Catalog::fh2(), car, meta)
        }
        _ => meta.clone(),
    };
    let meta = &meta;
    spend(conn, ctx, &SHARE_LIMIT).await?;
    let header = &params["fileHeader"];
    let title = text(&header["Title"], TITLE_CHARS);
    let description = text(&header["Description"], DESCRIPTION_CHARS);
    let preview = blob(&params["previewData"]);
    let file = NewFile {
        kind,
        item_id,
        file_type: i32_of(&header["Type"]),
        title: &title,
        description: &description,
        keyword_one: keyword_id(&header["KeywordOne"], 0),
        keyword_two: keyword_id(&header["KeywordTwo"], 1),
        meta,
        preview: &preview,
    };
    Ok(match sf::share_in(conn, viewer(ctx), &file).await? {
        Shared::File(id) => {
            tracing::info!(
                session = %ctx.req.ticket.live.session_id,
                kind = kind.as_str(),
                file = id,
                "a storefront file shared"
            );
            Ok(json!({ "contentFileId": id }))
        }
        Shared::NoItem => Err(err(missing)),
        Shared::TooMany => Err(err("too many shared files")),
        Shared::Removed => Err(err("this file was taken off the storefront after reports")),
    })
}

/// A stored method, on the request's transaction.
pub(crate) async fn stored(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    short: &str,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    let v = viewer(ctx);
    let me = v.profile_id;
    match (short, method) {
        // ------------------------------------------------------------------------- sharing
        ("StorefrontLivery", "Create") => {
            let d = &params["liveryDescription"];
            let id = i64_of(&d["base"]["VersionedLiveryId"]);
            share(conn, ctx, Kind::Livery, &params, id, &json!({})).await
        }
        ("StorefrontLayerGroup", "Create") => {
            let id = i64_of(&params["layerGroupDescription"]["base"]["VersionedLayerGroupId"]);
            share(conn, ctx, Kind::Group, &params, id, &json!({})).await
        }
        ("StorefrontTuning", "Create") => {
            let d = &params["tuningDescription"]["base"];
            let meta = json!({
                "MakeId": d["MakeId"], "ModelId": d["ModelId"], "ClassId": d["ClassId"],
                "EnvironmentId": d["EnvironmentId"], "TrackId": d["TrackId"],
                "PerformanceIndex": d["PerformanceIndex"],
            });
            let id = i64_of(&d["VersionedTuneId"]);
            share(conn, ctx, Kind::Tune, &params, id, &meta).await
        }

        // ------------------------------------------------------------------------ browsing
        ("Storefront", "GetFileKeywords") => {
            let max = params["maxKeywordResults"]
                .as_u64()
                .filter(|n| *n > 0)
                .map_or(FILE_KEYWORDS.len(), |n| {
                    usize::try_from(n).unwrap_or(usize::MAX)
                });
            Ok(Ok(keywords(max)))
        }
        ("Storefront", "SearchLiveryFiles") => search(conn, ctx, Kind::Livery, &params).await,
        ("Storefront", "SearchLayerGroupFiles") => search(conn, ctx, Kind::Group, &params).await,
        ("Storefront", "SearchTuningFiles") => search(conn, ctx, Kind::Tune, &params).await,
        // RECOMMENDED DESIGNS (ContentType = the file type, 2 = designs): every visible file of
        // that type for the car, the most used first (the player's own last).
        ("Storefront", "GetCuratedContent") => {
            if !allowed(conn, ctx, &READ_LIMIT).await? {
                return Ok(Ok(json!({ "curatedContent": [] })));
            }
            let c = &params["curationParameters"];
            let q = Query {
                file_type: Some(i32_of(&c["ContentType"])),
                cars: filter(&c["CarId"])
                    .and_then(|car| i32::try_from(car).ok())
                    .map(|car| vec![car]),
                track_id: filter(&c["TrackId"]),
                popular: true,
                limit: wanted(&params["maxContentResults"], 20, MAX_RESULTS),
                ..Query::default()
            };
            let files = sf::files_in(conn, v, &q).await?;
            let cr = creators(conn, ctx, &files).await?;
            let list: Vec<Json> = files.iter().map(|f| lightweight(f, &cr)).collect();
            Ok(Ok(json!({ "curatedContent": list })))
        }
        ("Storefront", "GetFileHeavyWeightData") => {
            if !allowed(conn, ctx, &HEAVY_LIMIT).await? {
                return Ok(Ok(json!({ "fileData": [] })));
            }
            let ids = file_ids(&params, MAX_HEAVY);
            let files = sf::files_by_id_in(conn, v, &ids).await?;
            // Previews only for the first MAX_PREVIEWS files shown.
            let shown: Vec<i32> = files.iter().take(MAX_PREVIEWS).map(|f| f.id).collect();
            let previews: HashMap<i32, Vec<u8>> =
                sf::previews_in(conn, &shown).await?.into_iter().collect();
            let cr = creators(conn, ctx, &files).await?;
            let list: Vec<Json> = files
                .iter()
                .map(|f| heavyweight(f, &cr, previews.get(&f.id).map(Vec::as_slice)))
                .collect();
            Ok(Ok(json!({ "fileData": list })))
        }
        ("Storefront", "GetMyFileList") => {
            if !allowed(conn, ctx, &READ_LIMIT).await? {
                return Ok(Ok(json!({ "files": [] })));
            }
            let q = Query {
                file_type: Some(i32_of(&params["fileType"])),
                own: true,
                limit: wanted(&params["maxFiles"], 100, 500),
                ..Query::default()
            };
            let files = sf::files_in(conn, v, &q).await?;
            let list: Vec<Json> = files.iter().map(for_owner).collect();
            Ok(Ok(json!({ "files": list })))
        }
        // Is this content shared? The share screen asks it.
        ("Storefront", "GetFileDataIfShared") => {
            if !allowed(conn, ctx, &READ_LIMIT).await? {
                return Ok(Ok(json!({ "found": false })));
            }
            let (item, file_type) = (
                i64_of(&params["versionedItemId"]),
                i32_of(&params["fileType"]),
            );
            let mut found = None;
            for kind in [Kind::Livery, Kind::Group, Kind::Tune] {
                if let Some(f) = sf::file_of_item_in(conn, v, kind, item).await?
                    && f.file_type == file_type
                {
                    found = Some(f);
                    break;
                }
            }
            Ok(Ok(match found {
                Some(f) => {
                    let cr = creators(conn, ctx, std::slice::from_ref(&f)).await?;
                    json!({ "found": true, "storefrontData": lightweight(&f, &cr) })
                }
                None => json!({ "found": false }),
            }))
        }

        // ------------------------------------------------------------------- owner's edits
        ("Storefront", "RemoveFile") => {
            sf::edit_in(conn, me, i32_of(&params["fileId"]), Edit::Remove).await?;
            Ok(Ok(json!({})))
        }
        ("Storefront", "ChangeFileTitle") => {
            let t = text(&params["newTitle"], TITLE_CHARS);
            sf::edit_in(conn, me, i32_of(&params["fileId"]), Edit::Title(&t)).await?;
            Ok(Ok(json!({})))
        }
        ("Storefront", "ChangeFileDescription") => {
            let d = text(&params["newDescription"], DESCRIPTION_CHARS);
            sf::edit_in(conn, me, i32_of(&params["fileId"]), Edit::Description(&d)).await?;
            Ok(Ok(json!({})))
        }
        // Shared files are kept until their owner removes them: nothing to reconcile.
        ("Storefront", "SynchronizeSharedFiles") => Ok(Ok(json!({}))),

        // ------------------------------------------------------------- counters, follows
        ("Storefront", "IncrementLikes" | "IncrementUsage" | "IncrementViews") => {
            let mark = match method {
                "IncrementLikes" => Mark::Like,
                "IncrementUsage" => Mark::Use,
                _ => Mark::View,
            };
            sf::mark_in(conn, v, i32_of(&params["fileId"]), mark).await?;
            Ok(Ok(json!({})))
        }
        ("Storefront", "BatchIncrementViews") => {
            if !allowed(conn, ctx, &READ_LIMIT).await? {
                return Ok(Ok(json!({})));
            }
            for id in file_ids(&params, MAX_RESULTS as usize) {
                sf::mark_in(conn, v, id, Mark::View).await?;
            }
            Ok(Ok(json!({})))
        }
        // A report is a strike; `STRIKES_TO_REMOVE` from different players take the file off
        // until a moderator decides. Past the report bucket: answered, not counted.
        ("Storefront", "ReportContent") => {
            if bucket(conn, ctx, &REPORT_LIMIT).await?.is_none()
                && sf::mark_in(conn, v, i32_of(&params["fileId"]), Mark::Report).await?
                    == Marked::Removed
            {
                tracing::info!(
                    file = i32_of(&params["fileId"]),
                    "a storefront file taken off by reports"
                );
            }
            Ok(Ok(json!({})))
        }
        ("Storefront", "AddFavoriteUser") => {
            sf::follow_in(conn, v, i64_of(&params["favoriteXuid"]), true).await?;
            Ok(Ok(json!({})))
        }
        ("Storefront", "RemoveFavoriteUser") => {
            sf::follow_in(conn, v, i64_of(&params["notSoFavoriteXuid"]), false).await?;
            Ok(Ok(json!({})))
        }
        ("Storefront", "ToggleFollowedCreator") => {
            let on = params["followCreator"].as_bool().unwrap_or(false);
            sf::follow_in(conn, v, i64_of(&params["creatorXuid"]), on).await?;
            Ok(Ok(json!({})))
        }

        // ------------------------------------------------------------------------ libraries
        ("OnlineProfile", "UpdateLiveryGroup") => {
            // A blocked account's upload is acknowledged and not kept, as its UpdateLivery is.
            if blocked(conn, ctx).await? {
                return Ok(Ok(json!({})));
            }
            spend(conn, ctx, &WRITE_LIMIT).await?;
            let layers = params["numberOfLayers"]
                .as_i64()
                .and_then(|n| i16::try_from(n).ok())
                .unwrap_or(0);
            let kept = sf::save_group_draft_in(
                conn,
                v,
                &text(&params["name"], sf::MAX_NAME_CHARS),
                layers,
                &blob(&params["data"]),
            )
            .await?;
            if !kept {
                tracing::info!(session = %ctx.req.ticket.live.session_id, "a vinyl group not kept");
            }
            Ok(Ok(json!({})))
        }
        ("OnlineProfile", "DeleteLiveryGroup") => {
            let n = text(&params["name"], sf::MAX_NAME_CHARS);
            sf::edit_group_draft_in(conn, me, &n, None).await?;
            Ok(Ok(json!({})))
        }
        ("OnlineProfile", "RenameLiveryGroup") => {
            let (from, to) = (
                text(&params["oldName"], sf::MAX_NAME_CHARS),
                text(&params["newName"], sf::MAX_NAME_CHARS),
            );
            sf::edit_group_draft_in(conn, me, &from, Some(&to)).await?;
            Ok(Ok(json!({})))
        }
        ("OnlineProfile", "UpdateCarSetup") => {
            // OD-178: a gated setup for a car the game cannot have or a traffic car: acknowledged,
            // not kept.
            if rpc::creation_refused(conn, ctx, method, i64_of(&params["carId"])).await? {
                return Ok(Ok(json!({})));
            }
            spend(conn, ctx, &WRITE_LIMIT).await?;
            let setup_name = text(&params["name"], sf::MAX_NAME_CHARS);
            let setup = Setup {
                car_id: i32_of(&params["carId"]),
                name: &setup_name,
                binary_version: params["binaryVersion"]
                    .as_i64()
                    .and_then(|n| i16::try_from(n).ok())
                    .unwrap_or(0),
                installed_parts: &params["installedParts"],
                tuning: &params["tuning"],
            };
            sf::save_setup_in(conn, v, &setup).await?;
            Ok(Ok(json!({})))
        }
        ("OnlineProfile", "DeleteCarSetup") => {
            let n = text(&params["name"], sf::MAX_NAME_CHARS);
            sf::edit_setup_in(conn, me, i32_of(&params["carId"]), &n, None).await?;
            Ok(Ok(json!({})))
        }
        ("OnlineProfile", "RenameCarSetup") => {
            let (from, to) = (
                text(&params["oldName"], sf::MAX_NAME_CHARS),
                text(&params["newName"], sf::MAX_NAME_CHARS),
            );
            sf::edit_setup_in(conn, me, i32_of(&params["carId"]), &from, Some(&to)).await?;
            Ok(Ok(json!({})))
        }
        // The livery library lives in the save; versioned liveries keep their own copy.
        ("OnlineProfile", "DeleteLivery" | "RenameLivery") => Ok(Ok(json!({}))),

        // ---------------------------------------------------------------- versioned content
        ("ContentVersioning", "CreateVersionedLayerGroup") => {
            if blocked(conn, ctx).await? {
                return Ok(Err(err("no such vinyl group")));
            }
            spend(conn, ctx, &WRITE_LIMIT).await?;
            let id = livery_id(ctx.fact_id());
            let n = text(&params["name"], sf::MAX_NAME_CHARS);
            versioned(
                sf::create_group_in(conn, v, id, &n, &blob(&params["thumbnail"])).await?,
                "versionedLayerGroupId",
                id,
                "no such vinyl group",
            )
        }
        ("ContentVersioning", "RetrieveVersionedLayerGroupById") => {
            let id = i64_of(&params["versionedLayerGroupId"]);
            Ok(match sf::retrieve_group_in(conn, v, id).await? {
                Some(g) => {
                    // The full content: a download of the file sharing it (FH2-S17); a
                    // counting error never fails the read.
                    count_download(conn, v, Kind::Group, id).await;
                    Ok(json!({ "versionedLayerGroupData": {
                    "name": g.name,
                    "numberOfLayers": g.number_of_layers,
                    "creatorXuid": g.creator_xuid,
                    "data": fi_xtsw::hex(&g.data),
                    "thumbnailData": g.thumbnail.as_deref().map(fi_xtsw::hex).unwrap_or_default(),
                    } }))
                }
                None => Err(err("no such vinyl group")),
            })
        }
        ("ContentVersioning", "CreateVersionedTune") => {
            if blocked(conn, ctx).await?
                || rpc::creation_refused(conn, ctx, method, i64_of(&params["carId"])).await?
            {
                return Ok(Err(err("no such setup")));
            }
            spend(conn, ctx, &WRITE_LIMIT).await?;
            let id = livery_id(ctx.fact_id());
            let n = text(&params["name"], sf::MAX_NAME_CHARS);
            versioned(
                sf::create_tune_in(conn, v, id, i32_of(&params["carId"]), &n).await?,
                "versionedTuneId",
                id,
                "no such setup",
            )
        }
        ("ContentVersioning", "CreateVersionedTuneFromStructs") => {
            if blocked(conn, ctx).await?
                || rpc::creation_refused(conn, ctx, method, i64_of(&params["carId"])).await?
            {
                return Ok(Err(err("no such setup")));
            }
            spend(conn, ctx, &WRITE_LIMIT).await?;
            let id = livery_id(ctx.fact_id());
            let tune = Tune {
                car_id: i32_of(&params["carId"]),
                name: String::new(),
                binary_version: 0,
                installed_parts: params["installedParts"].clone(),
                tuning: params["tuning"].clone(),
            };
            versioned(
                sf::create_tune_from_in(conn, v, id, &tune).await?,
                "versionedTuneId",
                id,
                "no such setup",
            )
        }
        ("ContentVersioning", "RetrieveVersionedTuneById") => {
            let id = i64_of(&params["versionedTuneId"]);
            Ok(match sf::retrieve_tune_in(conn, v, id).await? {
                Some(t) => {
                    // The full content: a download of the file sharing it (FH2-S17); a
                    // counting error never fails the read.
                    count_download(conn, v, Kind::Tune, id).await;
                    let xuid = u64::try_from(t.creator_xuid).unwrap_or(0);
                    let creator = match t.creator_name.as_deref() {
                        Some(n) => forza::game_tag(n),
                        None => forza::generated_gamertag(xuid),
                    };
                    Ok(json!({ "versionedTuneData": {
                        "CarId": t.tune.car_id,
                        "BinaryVersion": t.tune.binary_version,
                        "CreatorName": creator,
                        "CreatorXuid": xuid,
                        "InstalledParts": t.tune.installed_parts,
                        "Tuning": t.tune.tuning,
                    } }))
                }
                None => Err(err("no such tune")),
            })
        }
        ("ContentVersioning", "AddReferenceToVersionedLivery") => {
            let id = i64_of(&params["versionedLiveryId"]);
            sf::referenced_in(conn, v, Kind::Livery, id).await?;
            Ok(Ok(json!({})))
        }
        ("ContentVersioning", "AddReferenceToVersionedLayerGroup") => {
            let id = i64_of(&params["versionedLayerGroupId"]);
            sf::referenced_in(conn, v, Kind::Group, id).await?;
            Ok(Ok(json!({})))
        }
        ("ContentVersioning", "AddReferenceToVersionedTune") => {
            let id = i64_of(&params["versionedTuneId"]);
            sf::referenced_in(conn, v, Kind::Tune, id).await?;
            Ok(Ok(json!({})))
        }
        // Versions are kept by the per-profile cap and while shared: nothing to release.
        (
            "ContentVersioning",
            "RemoveReferenceToVersionedLayerGroup" | "RemoveReferenceToVersionedTune",
        ) => Ok(Ok(json!({}))),
        (short, method) => Err(ctx.not_served(short, method)),
    }
}

/// A `CreateVersioned*` answer: the id, or the Beta's Xls error 1 when nothing is named.
fn versioned(
    done: Versioned,
    field: &str,
    id: i64,
    missing: &'static str,
) -> Result<Reply, ApiError> {
    match done {
        Versioned::New | Versioned::Replay => {
            let mut m = serde_json::Map::new();
            m.insert(field.to_owned(), json!(id));
            Ok(Ok(Json::Object(m)))
        }
        Versioned::NoSource => Ok(Err(err(missing))),
        Versioned::Collision => {
            tracing::error!("a versioned storefront id collided");
            Err(ApiError::unavailable())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_db::gameplane;
    use fi_xtsw::{Schema, encode_results};

    fn encodes(class: &str, method: &str, results: &Json) {
        encode_results(Schema::fh2(), class, method, results)
            .unwrap_or_else(|e| panic!("{class}.{method}: {e:?}"));
    }

    #[test]
    fn every_method_served_is_in_the_schema() {
        let schema = Schema::fh2();
        let names: Vec<(String, String)> = schema
            .services()
            .flat_map(|(class, s)| {
                s.methods
                    .keys()
                    .map(move |m| (class.rsplit('.').next().unwrap().to_owned(), m.clone()))
            })
            .collect();
        let mut n = 0;
        for (short, list) in [
            ("Storefront", &STOREFRONT[..]),
            ("ContentVersioning", &VERSIONING[..]),
            ("OnlineProfile", &LIBRARY[..]),
        ] {
            for m in list {
                assert!(
                    names.contains(&(short.to_owned(), (*m).to_owned())),
                    "{short}.{m}"
                );
                assert_eq!(served(false, short, m), Some(Served::Store));
                n += 1;
            }
        }
        for short in [
            "StorefrontLivery",
            "StorefrontLayerGroup",
            "StorefrontTuning",
        ] {
            assert!(names.contains(&(short.to_owned(), "Create".to_owned())));
            assert_eq!(served(false, short, "Create"), Some(Served::Store));
        }
        assert_eq!(n, 38);
        // Not this module's: the livery store's own, and the photo and replay lists.
        for (short, m) in [
            ("ContentVersioning", "CreateVersionedLivery"),
            ("ContentVersioning", "RetrieveVersionedLiveryById"),
            ("OnlineProfile", "UpdateLivery"),
            ("Storefront", "GetLiveriesForDrivatar"),
            ("Storefront", "SearchPhotoFiles"),
        ] {
            assert_eq!(served(false, short, m), None, "{short}.{m}");
        }
        assert_eq!(served(true, "Storefront", "SearchLiveryFiles"), None);
    }

    #[test]
    fn keywords_are_the_betas_and_validated_by_bucket() {
        let k = keywords(usize::MAX);
        assert_eq!(k["keywords"].as_array().unwrap().len(), 46);
        assert_eq!(
            k["keywords"][0],
            json!({"Keyword": "RACING", "KeywordId": 1, "Bucket": 0})
        );
        assert_eq!(k["keywords"][24]["KeywordId"], 25);
        assert_eq!(keywords(3)["keywords"].as_array().unwrap().len(), 3);
        assert_eq!(keyword_id(&json!(1), 0), 1);
        assert_eq!(keyword_id(&json!(25), 1), 25);
        assert_eq!(keyword_id(&json!(25), 0), 0);
        assert_eq!(keyword_id(&json!(0), 0), 0);
        assert_eq!(keyword_id(&json!(47), 1), 0);
        assert_eq!(keyword_id(&json!(-3), 0), 0);
        encodes("Forza.WebServices.Storefront", "GetFileKeywords", &k);
    }

    fn file() -> File {
        File {
            id: 7,
            kind: Kind::Livery,
            profile_id: Uuid::nil(),
            xuid: 2_535_100_000_000_002,
            file_type: FILE_TYPE_LIVERY,
            item_id: 123_456_789,
            car_id: 289,
            title: "Stripes".into(),
            description: "Blue and gold".into(),
            keyword_one: 1,
            keyword_two: 25,
            layers: 3,
            meta: json!({}),
            downloads: 2,
            likes: 9,
            uses: 1,
            removed: false,
            created_at: fi_domain::UnixMs::from_millis(1_793_000_000_123),
        }
    }

    fn creators_of(f: &File) -> Creators {
        let p = rpc::player_of(gameplane::Player {
            xuid: f.xuid,
            name: Some("Painter".into()),
            card: gameplane::Card {
                career_level: 4,
                badge_id: 0,
                player_title_id: 0,
            },
            tables_tran_id: None,
        });
        Creators {
            users: HashMap::from([(f.profile_id, forza::user_data(&p))]),
            followed: vec![f.profile_id],
        }
    }

    #[test]
    fn every_reply_encodes() {
        let f = file();
        let c = creators_of(&f);
        let sfc = "Forza.WebServices.Storefront";
        let light = lightweight(&f, &c);
        // 9 likes + 3 × 1 use + 2 × 2 downloads = 16: two stars (FH2-S17).
        assert_eq!(light["PopularityRating"], 2);
        assert_eq!(light["IsCreatorFollowed"], true);
        for m in [
            "SearchLiveryFiles",
            "SearchLayerGroupFiles",
            "SearchTuningFiles",
        ] {
            encodes(sfc, m, &json!({ "searchResults": [light.clone()] }));
            encodes(sfc, m, &json!({ "searchResults": [] }));
        }
        encodes(
            sfc,
            "GetCuratedContent",
            &json!({ "curatedContent": [light.clone()] }),
        );
        let preview = vec![0x89u8, 0x50, 0x4e, 0x47];
        let heavy = heavyweight(&f, &c, Some(&preview));
        assert_eq!(heavy["PreviewPayload"], "89504e47");
        assert_eq!(heavy["VersionedContentId"], 123_456_789);
        encodes(
            sfc,
            "GetFileHeavyWeightData",
            &json!({ "fileData": [heavy] }),
        );
        encodes(
            sfc,
            "GetFileHeavyWeightData",
            &json!({ "fileData": [heavyweight(&f, &c, None)] }),
        );
        encodes(sfc, "GetMyFileList", &json!({ "files": [for_owner(&f)] }));
        encodes(
            sfc,
            "GetFileDataIfShared",
            &json!({ "found": true, "storefrontData": light }),
        );
        encodes(sfc, "GetFileDataIfShared", &json!({ "found": false }));
        for class in [
            "Forza.WebServices.StorefrontLivery",
            "Forza.WebServices.StorefrontLayerGroup",
            "Forza.WebServices.StorefrontTuning",
        ] {
            encodes(class, "Create", &json!({ "contentFileId": 7 }));
        }
        let cv = "Forza.WebServices.ContentVersioning";
        encodes(
            cv,
            "CreateVersionedLayerGroup",
            &json!({ "versionedLayerGroupId": 5 }),
        );
        encodes(cv, "CreateVersionedTune", &json!({ "versionedTuneId": 5 }));
        encodes(
            cv,
            "CreateVersionedTuneFromStructs",
            &json!({ "versionedTuneId": 5 }),
        );
        encodes(
            cv,
            "RetrieveVersionedLayerGroupById",
            &json!({ "versionedLayerGroupData": {
                "name": "Flames", "numberOfLayers": 2, "creatorXuid": 2_535_100_000_000_002u64,
                "data": "00ff", "thumbnailData": "",
            } }),
        );
        for m in [
            "AddReferenceToVersionedLivery",
            "AddReferenceToVersionedLayerGroup",
            "AddReferenceToVersionedTune",
            "RemoveReferenceToVersionedLayerGroup",
            "RemoveReferenceToVersionedTune",
        ] {
            encodes(cv, m, &json!({}));
        }
        for m in STOREFRONT.iter().filter(|m| {
            !matches!(
                **m,
                "GetFileKeywords"
                    | "SearchLiveryFiles"
                    | "SearchLayerGroupFiles"
                    | "SearchTuningFiles"
                    | "GetCuratedContent"
                    | "GetFileHeavyWeightData"
                    | "GetMyFileList"
                    | "GetFileDataIfShared"
            )
        }) {
            encodes(sfc, m, &json!({}));
        }
        for m in LIBRARY {
            encodes("Forza.WebServices.OnlineProfile", m, &json!({}));
        }
    }

    #[test]
    fn a_retrieved_tune_round_trips_the_games_structs() {
        // The parts and tuning of an UpdateCarSetup, as the game's encoding decodes them, are
        // answered back by RetrieveVersionedTuneById as they came.
        let schema = Schema::fh2();
        let op = "Forza.WebServices.OnlineProfile";
        let body = fi_xtsw::encode_params(
            schema,
            op,
            "UpdateCarSetup",
            &json!({
                "carId": 289, "name": "Grip", "binaryVersion": 3, "isLocked": false,
                "creatorName": "Painter", "creatorXuid": 2_535_100_000_000_002u64,
            }),
        )
        .expect("an UpdateCarSetup body");
        let setup = fi_xtsw::decode_params(schema, op, "UpdateCarSetup", &body)
            .expect("decodes")
            .to_json();
        let reply = json!({ "versionedTuneData": {
            "CarId": 289, "BinaryVersion": 3, "CreatorName": "Painter",
            "CreatorXuid": 2_535_100_000_000_002u64,
            "InstalledParts": setup["installedParts"], "Tuning": setup["tuning"],
        } });
        let cv = "Forza.WebServices.ContentVersioning";
        let bytes =
            encode_results(schema, cv, "RetrieveVersionedTuneById", &reply).expect("encodes");
        let back = fi_xtsw::decode_results(schema, cv, "RetrieveVersionedTuneById", &bytes)
            .expect("decodes")
            .to_json();
        assert_eq!(
            back["versionedTuneData"]["InstalledParts"],
            setup["installedParts"]
        );
        assert_eq!(back["versionedTuneData"]["Tuning"], setup["tuning"]);
    }

    #[test]
    fn searches_name_cars_through_the_catalog() {
        let catalog = Catalog::fh2();
        assert_eq!(cars_of(catalog, None, None), None);
        assert_eq!(cars_of(catalog, None, Some(289)), Some(vec![289]));
        let car = catalog.cars().next().expect("a car");
        let of_make = cars_of(catalog, Some(car.make_id), None).unwrap();
        assert!(of_make.contains(&i32::try_from(car.id).unwrap()));
        assert!(of_make.len() < catalog.cars().count());
        assert_eq!(
            cars_of(catalog, Some(car.make_id), Some(car.id)),
            Some(vec![i32::try_from(car.id).unwrap()])
        );
        assert_eq!(cars_of(catalog, Some(i64::MAX), None), Some(vec![]));
        let class = class_cars(catalog, car.class_id);
        assert!(class.contains(&i32::try_from(car.id).unwrap()));
        // Known car packs' cars: found by make (Ferrari 13, Chevrolet 9), model and class.
        for (id, make, class) in [(255, 13, 3), (9674, 9, 6)] {
            assert!(cars_of(catalog, Some(make), None).unwrap().contains(&id));
            assert_eq!(
                cars_of(catalog, Some(make), Some(i64::from(id))),
                Some(vec![id])
            );
            assert!(class_cars(catalog, class).contains(&id));
            assert_eq!(
                tune_meta(catalog, i64::from(id), &json!({}))["MakeId"],
                make
            );
        }
    }

    #[test]
    fn titles_and_descriptions_are_cleaned_and_bounded() {
        assert_eq!(
            text(&json!("  My   design\u{0007} "), TITLE_CHARS),
            "My design"
        );
        assert_eq!(
            text(&json!("x".repeat(40)), TITLE_CHARS).chars().count(),
            16
        );
        assert_eq!(text(&json!(null), TITLE_CHARS), "");
        assert_eq!(
            text(&json!("d".repeat(400)), DESCRIPTION_CHARS)
                .chars()
                .count(),
            256
        );
    }
}
