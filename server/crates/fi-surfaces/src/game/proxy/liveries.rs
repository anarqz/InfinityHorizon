//! The players' car liveries, stored on the server for every ticket (owner decision 2026-10-27
//! "Liveries are stored on the server again; the live lobby sync retires"; GAME_PLATFORM.md §3a
//! "Liveries", register CR-S59, superseding OD-65's temporary rows and CR-S25's tier split),
//! served on both deployments ([`super::forza::served`]): a repaint is saved with
//! `OnlineProfile.UpdateLivery` (the design's bytes), then `ContentVersioning.CreateVersionedLivery`
//! (an id for it, which the game keeps on the car and replicates to the players it meets; it drops
//! the previous one with `RemoveReferenceToVersionedLivery`), and anyone shown the car fetches it
//! with `RetrieveVersionedLiveryById`. Storage is [`fi_db::liveries`] (bytes content-addressed per
//! profile in Postgres, so the bucket-less matchmaking deployment serves them too).
//!
//! - **Who may retrieve:** the owner; an active, unbanned owner's livery, neither it nor its bytes
//!   hidden, to a reader of the owner's current mode (any tier), and only a design without layers
//!   until the in-game check (`LiverySettings::layered_to_others`). Anything else, a missing id
//!   included, or past the per-profile read bucket (`RETRIEVE_LIMIT`), is the Beta's Xls error 1
//!   "no such livery": existence never leaks. Thumbnails are
//!   answered empty, as the Beta did, and base liveries by synthetic id are not served.
//! - **Downloads (FH2-S17):** a retrieve with `returnThumbnail` false is the Storefront's download
//!   (the game asks for a car seen in a lobby with its thumbnail): it counts one download of the
//!   file sharing the livery, once per account ([`fi_db::storefront::referenced_in`]). A retrieve
//!   that asks for the thumbnail, or names no `returnThumbnail`, never counts.
//! - **Thumbnails (OD-137, CR-S73):** the `LiveryThumbnail` of a `CreateVersionedLivery` that
//!   creates a livery is kept with it ([`fi_db::liveries::keep_thumbnail_in`]); a missing, empty,
//!   undecodable or oversized one is not kept and never changes the answer.
//! - **Retention:** permanent, per profile and car (`fi_db::liveries::MAX_PER_CAR`,
//!   `MAX_PER_PROFILE`, `MAX_DRAFTS`; the oldest dropped) and the account's byte quota
//!   (`FI_LIVERY_ACCOUNT_BYTES`): past it `UpdateLivery` is acknowledged and not stored and the
//!   `CreateVersionedLivery` that follows answers Xls error 1, as for a block.
//! - **Limits:** a design of at most [`MAX_DATA`] bytes (the schema's `max_len`; the body cap of
//!   `UpdateLivery` is that plus [`FIELDS_SLACK`], `super::rpc::LARGE_BODIES`, else 413); a
//!   per-profile bucket on each write (`fi_db::liveries::CREATE_LIMIT`, `UPDATE_LIMIT`; over it 429
//!   `REQUEST.RATE_LIMITED` with `Retry-After`).
//! - **Moderation:** an account a moderator blocked (`livery_upload_block`) has its `UpdateLivery`
//!   acknowledged and not stored and its `CreateVersionedLivery` answered with Xls error 1 (the
//!   game then keeps no id on the car); a hidden livery is answered as missing to everyone else.
//! - **Ids:** 62 bits of the request's fact id (UUIDv5 of the session and the request), so a
//!   replayed request answers the same id; not sequential, and the retrieve check above guards
//!   them anyway.
//! - **Storefront:** `GetLiveriesForDrivatar` and `GetFeaturedLiveriesForRandomCars` (curated
//!   content the game asks for in a loop) are answered with empty lists ([`curated`]), as the
//!   Beta did: no catalogue exists.
//!
//! Nothing here logs a XUID, a name or the bytes.

use super::forza;
use super::forza::{Reply, XlsError};
use super::rpc::Ctx;
use super::universe::ghosts::unhex;
use crate::config::LiverySettings;
use crate::errors::ApiError;
use fi_db::limits::{Hit, Limit};
use fi_db::liveries::{self, Created, Design, Reader, Saved, Writer};
use fi_db::storefront::Kind;
use fi_protocol::ErrorCode;
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

pub use fi_db::liveries::MAX_DATA;

/// What `UpdateLivery`'s body carries besides the design's bytes (its name and four numbers, with
/// the encoding's headers), within its body cap.
pub const FIELDS_SLACK: usize = 4096;

/// The body cap of `OnlineProfile.UpdateLivery`.
pub const UPDATE_BODY: usize = MAX_DATA + FIELDS_SLACK;

/// Whether `(xls, short, method)` is one of the four stored methods.
pub fn is_livery(xls: bool, short: &str, method: &str) -> bool {
    matches!(
        (xls, short, method),
        (false, "OnlineProfile", "UpdateLivery")
            | (
                false,
                "ContentVersioning",
                "CreateVersionedLivery"
                    | "RemoveReferenceToVersionedLivery"
                    | "RetrieveVersionedLiveryById"
            )
    )
}

/// The curated Storefront lists (empty: no catalogue), `None` for any other method. Served to
/// every ticket from the parameters alone ([`super::forza::Served::Pure`]).
pub fn curated(xls: bool, short: &str, method: &str) -> Option<Json> {
    match (xls, short, method) {
        // `{carId, maxContentResults}` → `{drivatarLiveries: [{Id, VersionedContentId}]}`.
        // TODO(CR-S59): the players' own liveries could fill it (newest per car); the Beta's
        // layered-livery crash on someone else's car (deprecated/beta/server/src/forza.rs
        // racing_info) must be ruled out in play first.
        (false, "Storefront", "GetLiveriesForDrivatar") => Some(json!({ "drivatarLiveries": [] })),
        (false, "Storefront", "GetFeaturedLiveriesForRandomCars") => {
            Some(json!({ "liveryData": [] }))
        }
        _ => None,
    }
}

/// The Beta's answer to a livery nobody may see.
pub fn no_such_livery() -> XlsError {
    XlsError {
        code: 1,
        message: "no such livery",
    }
}

/// The top of the server's livery id space: ids are below `0x4000_0000_0000_0000`, which the
/// hooks' content-addressed world ids (top bits `01`, MATCHMAKING_API.md §5a, OD-66) never meet.
pub const SERVER_ID_MASK: i64 = 0x3FFF_FFFF_FFFF_FFFF;

/// The versioned livery id of a request: 62 bits of its fact id, never 0.
pub fn livery_id(fact: Uuid) -> i64 {
    let b = fact.as_bytes();
    let n = i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) & SERVER_ID_MASK;
    n.max(1)
}

fn writer<'a>(ctx: &'a Ctx<'a>) -> Writer<'a> {
    let t = &ctx.req.ticket;
    Writer {
        game: t.game(),
        mode: &t.claims.mode,
        profile_id: t.live.profile_id,
        account_id: t.live.account_id,
        account_bytes: LiverySettings::process().account_bytes,
    }
}

/// Keeps `CreateVersionedLivery`'s `LiveryThumbnail` with the livery it just created (OD-137,
/// CR-S73). A missing, empty, undecodable or oversized one is not kept and changes nothing in
/// the answer; only its size and format are logged.
async fn keep_thumbnail(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    id: i64,
    params: &Json,
) -> Result<(), ApiError> {
    let Some(hex) = params["LiveryThumbnail"].as_str() else {
        return Ok(());
    };
    if hex.is_empty() || hex.len() > 2 * liveries::MAX_THUMBNAIL {
        tracing::info!(
            hex_len = hex.len(),
            "a livery thumbnail not kept (empty or oversized)"
        );
        return Ok(());
    }
    let Some(data) = unhex(hex) else {
        tracing::info!(hex_len = hex.len(), "a livery thumbnail not kept (not hex)");
        return Ok(());
    };
    // In a savepoint: a database error here (a timeout on the large write, say) is logged and
    // rolled back to before the insert, and the livery the request created still commits.
    let kept = match sqlx::Connection::begin(&mut *conn).await {
        Ok(mut sp) => match liveries::keep_thumbnail_in(&mut sp, writer(ctx), id, &data).await {
            Ok(kept) => match sp.commit().await {
                Ok(()) => kept,
                Err(e) => {
                    tracing::warn!(error = %e, "a livery thumbnail not kept (savepoint release)");
                    false
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "a livery thumbnail not kept (database)");
                if let Err(e) = sp.rollback().await {
                    tracing::warn!(error = %e, "a livery thumbnail savepoint not rolled back");
                }
                false
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "a livery thumbnail not kept (savepoint)");
            false
        }
    };
    tracing::info!(
        size = data.len(),
        format = liveries::thumbnail_format(&data),
        kept,
        "a livery thumbnail"
    );
    Ok(())
}

async fn bucket(conn: &mut PgConnection, ctx: &Ctx<'_>, limit: &Limit) -> Result<(), ApiError> {
    match liveries::hit_in(conn, limit, ctx.req.ticket.live.profile_id).await? {
        Hit::Allowed => Ok(()),
        Hit::Limited { retry_after_secs } => Err(ApiError::new(
            ErrorCode::RequestRateLimited,
            "too many livery writes, retry later",
        )
        .retry_after(u64::try_from(retry_after_secs).unwrap_or(1))),
    }
}

async fn blocked(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<bool, ApiError> {
    Ok(liveries::upload_blocked_in(conn, ctx.req.ticket.live.account_id).await?)
}

/// The reply of a versioned livery (`VersionedLiveryData`; thumbnails empty, as the Beta's).
pub fn versioned(seen: &liveries::Seen) -> Json {
    json!({ "versionedLiveryData": {
        "profileId": 0,
        "carId": seen.car_id,
        "numberOfLayers": seen.number_of_layers,
        "creatorXuid": seen.creator_xuid,
        "data": fi_xtsw::hex(&seen.data),
        "liveryThumbnail": "",
        "liveryThumbnailSmall": "",
    } })
}

/// Whether a `RetrieveVersionedLiveryById` is the Storefront's download: the data alone
/// (`returnThumbnail` false). Absent or true: a car shown in a lobby, never a download.
pub fn counts_download(params: &Json) -> bool {
    params["returnThumbnail"].as_bool() == Some(false)
}

fn int<T: TryFrom<i64>>(params: &Json, field: &str) -> Option<T> {
    params[field].as_i64().and_then(|n| T::try_from(n).ok())
}

/// One of the four, on the request's transaction.
pub(crate) async fn stored(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    let t = &ctx.req.ticket;
    match method {
        "UpdateLivery" => {
            let data = params["data"].as_str().and_then(unhex).unwrap_or_default();
            // A design without bytes has nothing to keep: answered, nothing stored.
            if data.is_empty() {
                return Ok(Ok(forza::updated()));
            }
            if data.len() > MAX_DATA {
                return Err(ApiError::invalid(
                    "body",
                    format!("a livery holds 1 to {MAX_DATA} bytes"),
                ));
            }
            // OD-178: a gated design for a car the game cannot have or a traffic car is answered
            // as a block is, nothing kept.
            let car = params["carId"].as_i64().unwrap_or(-1);
            if blocked(conn, ctx).await?
                || super::rpc::creation_refused(conn, ctx, method, car).await?
            {
                return Ok(Ok(forza::updated()));
            }
            bucket(conn, ctx, &liveries::UPDATE_LIMIT).await?;
            let design = Design {
                name: params["name"].as_str().unwrap_or(""),
                car_id: int(&params, "carId").unwrap_or(-1),
                number_of_layers: int(&params, "numberOfLayers").unwrap_or(0),
                data: &data,
            };
            // Past the account's quota: answered as stored (as a block is), nothing kept, and the
            // CreateVersionedLivery that follows finds no design (Xls error 1).
            if liveries::save_draft_in(conn, writer(ctx), &design).await? == Saved::OverQuota {
                tracing::info!(session = %t.live.session_id, "a livery past the account's quota");
            }
            Ok(Ok(forza::updated()))
        }
        "CreateVersionedLivery" => {
            let car = params["carId"].as_i64().unwrap_or(-1);
            if blocked(conn, ctx).await?
                || super::rpc::creation_refused(conn, ctx, method, car).await?
            {
                return Ok(Err(no_such_livery()));
            }
            bucket(conn, ctx, &liveries::CREATE_LIMIT).await?;
            let id = livery_id(ctx.fact_id());
            let created = liveries::create_in(
                conn,
                writer(ctx),
                id,
                params["name"].as_str().unwrap_or(""),
                int(&params, "carId").unwrap_or(-1),
            )
            .await?;
            if created == Created::New {
                keep_thumbnail(conn, ctx, id, &params).await?;
            }
            match created {
                Created::New | Created::Replay => Ok(Ok(json!({ "versionedLiveryId": id }))),
                // The game keeps no id on the car (its failure listener sets -1).
                Created::NoDesign => Ok(Err(no_such_livery())),
                Created::Collision => {
                    tracing::error!("a versioned livery id collided");
                    Err(ApiError::unavailable())
                }
            }
        }
        "RemoveReferenceToVersionedLivery" => {
            let id = params["versionedLiveryId"].as_i64().unwrap_or(0);
            liveries::remove_unshared_in(conn, t.game(), t.live.profile_id, id).await?;
            Ok(Ok(forza::updated()))
        }
        "RetrieveVersionedLiveryById" => {
            let id = params["versionedLiveryId"].as_i64().unwrap_or(0);
            // Over the read bucket: the same answer as a missing livery (stock paint), never a
            // failed call the hook would retry.
            if let Hit::Limited { .. } =
                liveries::hit_in(conn, &liveries::RETRIEVE_LIMIT, t.live.profile_id).await?
            {
                return Ok(Err(no_such_livery()));
            }
            let reader = Reader {
                game: t.game(),
                mode: &t.claims.mode,
                profile_id: t.live.profile_id,
                layered_to_others: LiverySettings::process().layered_to_others,
            };
            Ok(match liveries::retrieve_in(conn, reader, id).await? {
                Some(seen) => {
                    if counts_download(&params) {
                        let v = super::universe::storefront::viewer(ctx);
                        // A counting error is logged and never fails the read.
                        super::universe::storefront::count_download(conn, v, Kind::Livery, id)
                            .await;
                    }
                    Ok(versioned(&seen))
                }
                None => Err(no_such_livery()),
            })
        }
        other => Err(ctx.not_served("ContentVersioning", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_xtsw::{Schema, encode_results};

    #[test]
    fn only_a_retrieve_without_thumbnail_is_a_download() {
        assert!(counts_download(
            &json!({"versionedLiveryId": 7, "returnThumbnail": false})
        ));
        assert!(!counts_download(
            &json!({"versionedLiveryId": 7, "returnThumbnail": true})
        ));
        assert!(!counts_download(&json!({"versionedLiveryId": 7})));
    }

    #[test]
    fn ids_are_positive_and_stable() {
        let f = Uuid::now_v7();
        assert_eq!(livery_id(f), livery_id(f));
        assert!(livery_id(f) > 0);
        assert_eq!(livery_id(Uuid::nil()), 1);
        assert_eq!(livery_id(Uuid::max()), SERVER_ID_MASK);
    }

    #[test]
    fn every_reply_encodes() {
        let s = Schema::fh2();
        let cv = "Forza.WebServices.ContentVersioning";
        for (class, method, reply) in [
            (
                "Forza.WebServices.OnlineProfile",
                "UpdateLivery",
                forza::updated(),
            ),
            (cv, "RemoveReferenceToVersionedLivery", forza::updated()),
            (
                cv,
                "CreateVersionedLivery",
                json!({"versionedLiveryId": 77}),
            ),
            (
                cv,
                "RetrieveVersionedLiveryById",
                versioned(&liveries::Seen {
                    car_id: 512,
                    number_of_layers: 3,
                    creator_xuid: 42,
                    data: vec![1, 2, 3, 0xff],
                }),
            ),
        ] {
            encode_results(s, class, method, &reply).unwrap_or_else(|e| panic!("{method}: {e}"));
        }
    }

    #[test]
    fn the_curated_lists_encode_empty() {
        let s = Schema::fh2();
        let sf = "Forza.WebServices.Storefront";
        for method in ["GetLiveriesForDrivatar", "GetFeaturedLiveriesForRandomCars"] {
            let reply = curated(false, "Storefront", method).unwrap();
            encode_results(s, sf, method, &reply).unwrap_or_else(|e| panic!("{method}: {e}"));
        }
        assert_eq!(curated(false, "Storefront", "SearchLiveryFiles"), None);
        assert_eq!(curated(true, "Storefront", "GetLiveriesForDrivatar"), None);
    }

    const _: () = assert!(UPDATE_BODY <= super::super::MAX_BODY);
}
