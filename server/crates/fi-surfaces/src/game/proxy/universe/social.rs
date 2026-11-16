//! Friends in the game (SER-M3 step 7, OD-48 item 2; GAME_PLATFORM.md §3a "Supporter social",
//! register CR-S19): the one friend set the friend-aware universe reads take ([`friend_ids`]: the
//! friend views of the boards, drivatars and rivals friend-first), and "Meus rivais", the compare
//! stats of `User.GetCompareStatsForRelatedUsers` and `GetCompareStatsByBucket`, ported from the
//! Beta's arms of `handle` (`deprecated/beta/server/src/forza.rs`), over [`fi_db::friends`].
//!
//! A friend is a profile of the caller's game **and mode** (modes never meet, ACCOUNTS.md) with
//! both rows of the friendship, visible under [`fi_db::visibility`] (active, its account active
//! and not banned); at most [`MAX_FRIENDS_READ`], oldest friendship first. Each request reads its
//! friend set once, in one statement on the request's connection.
//!
//! - **GetCompareStatsForRelatedUsers** `{numDesiredUsers}`: the caller's friends, each with the
//!   career stats of its merged online profile (`compareStats[0]` of OnlineProfile.UpdateAll;
//!   `{}` when it synced none) and its XUID, at most `numDesiredUsers` ([`wanted_users`]).
//! - **GetCompareStatsByBucket** `{bucketId, numUsers, xuids}`: each XUID asked (the caller's
//!   friends when the list is empty), in the order asked, with its `bucketedCompareStats` row of
//!   `bucketId` (OnlineProfile.UpdateAllTables; `{Id: bucketId}` when it synced none); only
//!   profiles of the caller's game and mode visible to it, any other XUID left out without an
//!   error (the Beta's `meets`); at most `numUsers`.
//!
//! Stored stats reach other players' games: [`checked_stats`] rebuilds them with exactly the
//! schema's fields, each within its kind, so a reply always encodes.
//!
//! Supporters only, on saves ([`served`] is the universe's): a ticket that keeps no cloud data
//! (not `vip` and `hardcore`, OD-52) is served `Features::Free` there (`Features::for_ticket`):
//! the free set's empty `Pure` reply where it has one (the compare stats, `forza::pure`, OD-40),
//! else `GAME.NOT_SERVED`, as on the free deployment.

use super::super::forza::{Reply, Served};
use super::super::rpc::Ctx;
use crate::errors::ApiError;
use fi_db::friends::{self, MAX_FRIENDS_READ};
use fi_xtsw::{Kind, Schema};
use serde_json::{Map, Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

const RELATED: &str = "GetCompareStatsForRelatedUsers";
const BY_BUCKET: &str = "GetCompareStatsByBucket";

/// The career stats type of a compare stats row (`UserCompareStats.base`).
pub const CAREER_STATS: &str = "ForzaProfileCompareStats";

/// The per-bucket stats type (`UserBucketedCompareStats.base`).
pub const BUCKET_STATS: &str = "ForzaProfileBucketedCompareStats";

/// Rows of a compare stats reply when the game asks for none or fewer than one (the Beta's).
pub const DEFAULT_USERS: u32 = 10;

/// The most rows of a compare stats reply (the Beta's).
pub const MAX_USERS: u32 = 100;

/// How `(xls, class, method)` is answered; `None`: not this module's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    match (xls, short, method) {
        (false, "User", RELATED | BY_BUCKET) => Some(Served::Store),
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
        RELATED => related(conn, ctx, &params).await,
        BY_BUCKET => by_bucket(conn, ctx, &params).await,
        _ => Err(ctx.not_served(short, method)),
    }
}

/// The caller's friends (the module's rule) as profile ids: the friend set of the boards' friend
/// views, drivatars and rivals. One statement.
pub(crate) async fn friend_ids(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
) -> Result<Vec<Uuid>, ApiError> {
    Ok(friends::friends_in(conn, &ctx.viewer(), MAX_FRIENDS_READ)
        .await?
        .into_iter()
        .map(|f| f.profile_id)
        .collect())
}

/// The rows a compare stats request wants (`numDesiredUsers`, `numUsers`): above 0, at most
/// [`MAX_USERS`]; [`DEFAULT_USERS`] otherwise.
pub fn wanted_users(n: &Json) -> u32 {
    n.as_i64().filter(|n| *n > 0).map_or(DEFAULT_USERS, |n| {
        u32::try_from(n.min(i64::from(MAX_USERS))).unwrap_or(MAX_USERS)
    })
}

/// One stored field as its kind holds it: an integer within the kind's range, a finite float
/// within an `f32`; anything else (absent, out of range, not a number) is 0.
fn checked_number(kind: &Kind, v: &Json) -> Option<Json> {
    Some(match kind {
        Kind::U8 | Kind::U16 | Kind::U32 | Kind::U64 => {
            let max = match kind {
                Kind::U8 => u64::from(u8::MAX),
                Kind::U16 => u64::from(u16::MAX),
                Kind::U32 => u64::from(u32::MAX),
                _ => u64::MAX,
            };
            Json::from(v.as_u64().filter(|n| *n <= max).unwrap_or(0))
        }
        Kind::I16 | Kind::I32 | Kind::I64 => {
            let (min, max) = match kind {
                Kind::I16 => (i64::from(i16::MIN), i64::from(i16::MAX)),
                Kind::I32 => (i64::from(i32::MIN), i64::from(i32::MAX)),
                _ => (i64::MIN, i64::MAX),
            };
            Json::from(v.as_i64().filter(|n| (min..=max).contains(n)).unwrap_or(0))
        }
        Kind::F32 | Kind::F64 => Json::from(
            v.as_f64()
                .filter(|f| f.is_finite() && f.abs() <= f64::from(f32::MAX))
                .unwrap_or(0.0),
        ),
        Kind::Bool => Json::from(v.as_bool().unwrap_or(false)),
        // The stats types hold numbers only; any other kind is left to the codec's default.
        _ => return None,
    })
}

/// A stored stats struct of schema type `ty` rebuilt with exactly the schema's fields, each
/// within its kind (unknown fields dropped, a missing or unusable one 0); `None` or a value that
/// is not an object gives every field 0. Stored rows came through the schema's own decode, so this
/// changes nothing on the game's data; it keeps a reply that reaches others always encodable.
pub fn checked_stats(ty: &str, stored: Option<&Json>) -> Json {
    let Some(def) = Schema::fh2().type_def(ty) else {
        return json!({});
    };
    let empty = Map::new();
    let src = stored.and_then(Json::as_object).unwrap_or(&empty);
    let mut out = Map::new();
    for f in &def.fields {
        if let Some(v) = checked_number(&f.kind, src.get(&f.name).unwrap_or(&Json::Null)) {
            out.insert(f.name.clone(), v);
        }
    }
    Json::Object(out)
}

/// GetCompareStatsForRelatedUsers' reply: each friend's XUID and career stats (`{}` without a
/// synced profile, as the Beta's).
pub fn related_reply(rows: &[(i64, Option<Json>)]) -> Json {
    let stats: Vec<Json> = rows
        .iter()
        .map(|(xuid, stored)| {
            let base = match stored {
                Some(s @ Json::Object(_)) => checked_stats(CAREER_STATS, Some(s)),
                _ => json!({}),
            };
            json!({ "base": base, "xuid": u64::try_from(*xuid).unwrap_or(0) })
        })
        .collect();
    json!({ "compareStats": stats })
}

/// GetCompareStatsByBucket's reply: each player's XUID and stats of `bucket` (`{Id: bucket}`
/// without a synced row, as the Beta's).
pub fn bucket_reply(bucket: i32, rows: &[(i64, Option<Json>)]) -> Json {
    let stats: Vec<Json> = rows
        .iter()
        .map(|(xuid, stored)| {
            let base = match stored {
                Some(s @ Json::Object(_)) => {
                    let mut b = checked_stats(BUCKET_STATS, Some(s));
                    // The row of the asked bucket (the table is keyed by `Id`).
                    b["Id"] = bucket.into();
                    b
                }
                _ => json!({ "Id": bucket }),
            };
            json!({ "base": base, "xuid": u64::try_from(*xuid).unwrap_or(0) })
        })
        .collect();
    json!({ "bucketedCompareStats": stats })
}

/// The XUIDs a GetCompareStatsByBucket names (`None`: an empty list, the caller's friends);
/// one that is not a XUID a profile can have is left out.
pub fn asked_xuids(params: &Json) -> Option<Vec<i64>> {
    let list = params["xuids"].as_array().filter(|a| !a.is_empty())?;
    Some(
        list.iter()
            .filter_map(Json::as_u64)
            .filter_map(|x| i64::try_from(x).ok())
            .collect(),
    )
}

async fn related(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let n = wanted_users(&params["numDesiredUsers"]);
    let rows: Vec<(i64, Option<Json>)> = friends::friends_compare_stats_in(conn, &ctx.viewer(), n)
        .await?
        .into_iter()
        .map(|(f, stats)| (f.xuid, stats))
        .collect();
    Ok(Ok(related_reply(&rows)))
}

async fn by_bucket(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
) -> Result<Reply, ApiError> {
    let bucket = params["bucketId"]
        .as_i64()
        .and_then(|b| i32::try_from(b).ok())
        .unwrap_or(-1);
    let n = wanted_users(&params["numUsers"]);
    let asked = asked_xuids(params);
    let rows = friends::bucket_stats_in(conn, &ctx.viewer(), bucket, asked.as_deref(), n).await?;
    Ok(Ok(bucket_reply(bucket, &rows)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_xtsw::{decode_results, encode_results};

    const CLASS: &str = "Forza.WebServices.User";

    fn encodes(method: &str, reply: &Json) -> Json {
        let schema = Schema::fh2();
        let bytes = encode_results(schema, CLASS, method, reply)
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        decode_results(schema, CLASS, method, &bytes)
            .unwrap()
            .to_json()
    }

    /// Every field of a stats type, each set to `n` (or `n` as a float for the float fields).
    fn full(ty: &str, n: u32) -> Json {
        let def = Schema::fh2().type_def(ty).unwrap();
        Json::Object(
            def.fields
                .iter()
                .map(|f| {
                    let v = match f.kind {
                        Kind::F32 => json!(f64::from(n) + 0.5),
                        _ => json!(n),
                    };
                    (f.name.clone(), v)
                })
                .collect(),
        )
    }

    /// Every reply of this module encodes with the game's schema (an encode failure is a 503 the
    /// game repeats), whatever was stored: a full row, unknown fields, nulls, values out of their
    /// kind's range, and no row at all.
    #[test]
    fn every_reply_encodes_with_the_schema() {
        let career = full(CAREER_STATS, 7);
        assert_eq!(
            career.as_object().unwrap().len(),
            48,
            "the schema's 48 career stats"
        );
        let mut odd = career.clone();
        odd["unknownField"] = json!("text");
        odd["wristbandLevel"] = Json::Null;
        odd["garageValue"] = json!(-4);
        odd["carsInGarage"] = json!(5e12);
        let x1 = 2_535_100_000_000_001_i64;
        let reply = related_reply(&[
            (x1, Some(career.clone())),
            (x1 + 1, Some(odd)),
            (x1 + 2, None),
            (x1 + 3, Some(json!([1, 2]))),
        ]);
        let back = encodes(RELATED, &reply);
        let rows = back["compareStats"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["xuid"], 2_535_100_000_000_001_u64);
        assert_eq!(rows[0]["base"], career, "a stored row comes back as it was");
        assert_eq!(rows[1]["base"]["wristbandLevel"], 0);
        assert_eq!(rows[1]["base"]["garageValue"], 0);
        assert_eq!(rows[1]["base"]["carsInGarage"], 0);
        assert_eq!(rows[1]["base"]["horizonFinalProgress"], 7);
        assert_eq!(rows[2]["base"]["wristbandLevel"], 0, "no profile: zeros");
        assert_eq!(rows[3]["base"]["barnFinds"], 0);
        assert!(
            reply["compareStats"][1]["base"]
                .get("unknownField")
                .is_none()
        );
        assert!(
            reply["compareStats"][2]["base"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        // No friends: the empty list, as the Beta's and the free deployment's.
        assert_eq!(
            encodes(RELATED, &related_reply(&[])),
            json!({"compareStats": []})
        );

        let bucketed = full(BUCKET_STATS, 3);
        assert_eq!(bucketed.as_object().unwrap().len(), 21, "Id and 20 stats");
        let mut nan = bucketed.clone();
        nan["topSpeed"] = json!("NaN");
        nan["biggestAir"] = json!(1e300);
        let reply = bucket_reply(
            12,
            &[
                (x1, Some(bucketed.clone())),
                (x1 + 1, Some(nan)),
                (x1 + 2, None),
            ],
        );
        let back = encodes(BY_BUCKET, &reply);
        let rows = back["bucketedCompareStats"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["base"]["Id"], 12, "the asked bucket");
        assert_eq!(rows[0]["base"]["distanceDriven"], 3);
        assert!((rows[0]["base"]["topSpeed"].as_f64().unwrap() - 3.5).abs() < 1e-6);
        assert_eq!(rows[1]["base"]["topSpeed"], 0.0);
        assert_eq!(rows[1]["base"]["biggestAir"], 0.0);
        assert_eq!(rows[2]["base"]["Id"], 12);
        assert_eq!(rows[2]["base"]["distanceDriven"], 0);
        assert_eq!(rows[2]["xuid"], 2_535_100_000_000_003_u64);
        assert_eq!(reply["bucketedCompareStats"][2]["base"], json!({"Id": 12}));
        assert_eq!(
            encodes(BY_BUCKET, &bucket_reply(3, &[])),
            json!({"bucketedCompareStats": []})
        );
    }

    #[test]
    fn requests_name_how_many_and_whom() {
        assert_eq!(wanted_users(&json!(10)), 10);
        assert_eq!(wanted_users(&json!(0)), DEFAULT_USERS);
        assert_eq!(wanted_users(&json!(-3)), DEFAULT_USERS);
        assert_eq!(wanted_users(&Json::Null), DEFAULT_USERS);
        assert_eq!(wanted_users(&json!(5000)), MAX_USERS);
        assert_eq!(asked_xuids(&json!({"xuids": []})), None);
        assert_eq!(asked_xuids(&json!({})), None);
        assert_eq!(
            asked_xuids(&json!({"xuids": [2_535_100_000_000_001_u64, u64::MAX]})),
            Some(vec![2_535_100_000_000_001])
        );
        assert_eq!(
            served(false, "User", RELATED),
            Some(Served::Store),
            "a stored universe method"
        );
        assert_eq!(served(false, "User", BY_BUCKET), Some(Served::Store));
        assert_eq!(served(true, "User", RELATED), None);
        assert_eq!(served(false, "User", "Logon"), None);
    }

    /// Golden decode (opt-in) of the Beta's captured compare stats (`server/captures/*.jsonl`):
    /// every request decodes with the schema and names how many rows it wants; every Beta reply
    /// decodes; ours, built from the Beta's own rows (each `base` through [`checked_stats`], a
    /// synthetic XUID), encodes and decodes with the Beta handler's fields and the very same
    /// `base` values; with no friend, ours equals the Beta's empty reply. The captures hold real
    /// player identities: only counts are printed, never values.
    ///
    /// `FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-api universe::social -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the Beta's captures from FI_FH2_CAPTURES_DIR (outside the repository)"]
    fn captured_compare_stats_decode_and_replies_keep_the_betas_values() {
        use fi_xtsw::{Envelope, decode_params, parse_envelope};
        use std::collections::BTreeMap;
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
        // method -> counter -> count
        let mut tally: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
        let mut failures: BTreeMap<String, usize> = BTreeMap::new();
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
                let key = super::super::super::rpc::class_key(class);
                let (method, list_key, stats_ty) = match (key, method) {
                    (CLASS, RELATED) => (RELATED, "compareStats", CAREER_STATS),
                    (CLASS, BY_BUCKET) => (BY_BUCKET, "bucketedCompareStats", BUCKET_STATS),
                    _ => continue,
                };
                let t = tally.entry(method).or_default();
                let mut count = |k: &'static str| *t.entry(k).or_default() += 1;
                count("requests");
                let body = fi_auth::b64::std_decode(r["request_body_b64"].as_str().unwrap_or(""))
                    .unwrap_or_default();
                let params = match decode_params(schema, CLASS, method, &body) {
                    Ok(v) => v.into_json(),
                    Err(_) => {
                        *failures.entry(format!("{method}: request")).or_default() += 1;
                        continue;
                    }
                };
                count("decoded");
                let n = if method == RELATED {
                    wanted_users(&params["numDesiredUsers"])
                } else {
                    wanted_users(&params["numUsers"])
                };
                if (1..=MAX_USERS).contains(&n) {
                    count("bounded");
                }
                let beta = fi_auth::b64::std_decode(r["response_body_b64"].as_str().unwrap_or(""))
                    .unwrap_or_default();
                let Ok(Envelope::Ok { results }) = parse_envelope(&beta) else {
                    count("beta not ok");
                    continue;
                };
                let Ok(beta) = decode_results(schema, CLASS, method, results) else {
                    *failures.entry(format!("{method}: beta reply")).or_default() += 1;
                    continue;
                };
                let beta = beta.to_json();
                count("beta decoded");
                let rows: Vec<Json> = beta[list_key].as_array().cloned().unwrap_or_default();
                let synthetic: Vec<(i64, Option<Json>)> = rows
                    .iter()
                    .zip(2_535_100_000_000_001_i64..)
                    .map(|(row, x)| (x, Some(row["base"].clone())))
                    .collect();
                let ours = if method == RELATED {
                    related_reply(&synthetic)
                } else {
                    let bucket =
                        i32::try_from(params["bucketId"].as_i64().unwrap_or(-1)).unwrap_or(-1);
                    bucket_reply(bucket, &synthetic)
                };
                let Ok(bytes) = encode_results(schema, CLASS, method, &ours) else {
                    *failures.entry(format!("{method}: our reply")).or_default() += 1;
                    continue;
                };
                let back = decode_results(schema, CLASS, method, &bytes)
                    .unwrap()
                    .to_json();
                count("ours encoded");
                let keys = |v: &Json| v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>());
                if keys(&back) != keys(&r["response_xtsw"]) {
                    *failures.entry(format!("{method}: fields")).or_default() += 1;
                }
                let ours_rows = back[list_key].as_array().cloned().unwrap_or_default();
                if rows.is_empty() {
                    count("empty");
                    if back == beta {
                        count("empty equal");
                    }
                } else {
                    count("with rows");
                    let same = ours_rows.len() == rows.len()
                        && ours_rows.iter().zip(&rows).all(|(o, b)| {
                            // `{}` stays `{}` in ours; the decoded Beta row has every field.
                            let expected = checked_stats(stats_ty, Some(&b["base"]));
                            o["base"] == b["base"] || o["base"] == expected
                        });
                    if same {
                        count("rows equal");
                    } else {
                        *failures.entry(format!("{method}: row values")).or_default() += 1;
                    }
                    let full = rows
                        .iter()
                        .filter(|b| b["base"].as_object().is_some_and(|o| !o.is_empty()))
                        .count();
                    *t.entry("rows").or_default() += rows.len();
                    *t.entry("rows with stats").or_default() += full;
                }
            }
        }
        for (method, counts) in &tally {
            println!("{method}: {counts:?}");
        }
        println!("{} files", files.len());
        assert!(
            tally
                .get(RELATED)
                .is_some_and(|t| t.get("decoded").is_some()),
            "no captured compare stats"
        );
        assert!(failures.is_empty(), "{failures:?}");
    }
}
