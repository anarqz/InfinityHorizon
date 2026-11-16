//! The players' car liveries, stored on the server for every ticket (owner decision 2026-10-27
//! "Liveries are stored on the server again; the live lobby sync retires"; GAME_PLATFORM.md §3a
//! "Liveries", register CR-S59, which supersedes OD-65's temporary rows): `livery_blob`,
//! `game_livery_draft` and `game_livery` (migrations `20261019220000_liveries.sql`,
//! `20261027170000_livery_store.sql`), ported from the Beta's `liveries` table.
//!
//! - [`save_draft_in`]: `OnlineProfile.UpdateLivery`, the design's bytes by (name, car), the
//!   pending copy; the bytes go to `livery_blob` under their SHA-256 (per profile), within the
//!   account's byte quota.
//! - [`create_in`]: `ContentVersioning.CreateVersionedLivery`, a versioned livery from the pending
//!   copy of that name (the car's, else the newest of the name), under the caller's id.
//! - [`keep_thumbnail_in`]: that call's `LiveryThumbnail`, kept with the new livery and gone with
//!   it (`game_livery_thumbnail`, OD-137, CR-S73).
//! - [`remove_in`]: `RemoveReferenceToVersionedLivery`, the caller's own only
//!   ([`remove_unshared_in`] keeps a livery shared on the Storefront, OD-138).
//! - [`retrieve_in`]: `RetrieveVersionedLiveryById`, only what the viewer may see ([`Seen`]).
//! - Moderation ([`admin_list_in`], [`set_hidden_in`], [`admin_delete_in`], [`admin_bulk_in`],
//!   [`set_upload_block_in`]): fi-core's `/core/v1/admin/…liveries…` routes.
//!
//! **Retention.** Every row written now is permanent (no session): kept per profile and car,
//! at most [`MAX_PER_CAR`] versions of a car and [`MAX_PER_PROFILE`] in all, [`MAX_DRAFTS`] drafts,
//! the oldest dropped, and at most the account's byte quota (`FI_LIVERY_ACCOUNT_BYTES`,
//! [`DEFAULT_ACCOUNT_BYTES`]) of distinct bytes over its profiles. Rows written before CR-S59
//! for a free or noob request still carry their session and go when it ends (the OD-65 trigger,
//! left in place). A Hardcore -> Noob change keeps them (owner 2026-10-27 "a mode change keeps the
//! liveries"); erasure deletes them.
//!
//! **Bytes.** One `livery_blob` row per (profile, SHA-256) whatever names it; [`release_in`]
//! deletes the ones no draft or livery names any more, in the transaction that dropped the last
//! reference, except bytes a moderator hid (kept, so the same design applied again stays hidden).
//!
//! **Locks.** [`save_draft_in`] takes the account's advisory lock (the quota), then upserts the
//! blob (a no-op `DO UPDATE` locks an existing row), then the draft (`FOR UPDATE`), then releases.
//! [`create_in`] reads the draft `FOR SHARE` in its own statement before inserting, so a
//! concurrent `save_draft_in` of the same draft either commits first (create reads its bytes) or
//! waits for create; a draft's blob is never released while the draft names it, so the insert's
//! foreign key always holds. [`release_in`] locks the blob rows in key order and reads the
//! references in a later statement (fresh snapshot): a writer that locked a row first keeps it.
//!
//! Nothing here logs a XUID, a name or the bytes.

use crate::identity::IdentityError;
use crate::limits::{self, Hit, Limit};
use crate::visibility::VISIBLE;
use fi_domain::UnixMs;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The largest design (`VersionedLiveryData.data` `max_len` in the game's schema: 512 KiB); the
/// game can never be handed more.
pub const MAX_DATA: usize = 524_288;

/// The longest name kept (the Beta's 64 characters).
pub const MAX_NAME_CHARS: usize = 64;

/// Versioned liveries one profile keeps of one car: only the current one (owner 2026-10-27 "one
/// livery per car"): a new design for a car replaces that car's previous one, no history.
pub const MAX_PER_CAR: i64 = 1;

/// Cars with a livery one profile keeps (owner 2026-10-27: 30 cars; the oldest car's livery goes first).
pub const MAX_PER_PROFILE: i64 = 30;

/// Drafts (pending copies) one profile keeps: the game saves a design right before naming it, so
/// only the last few are ever read.
pub const MAX_DRAFTS: i64 = 5;

/// The default byte quota of an account's liveries over all its profiles
/// (`FI_LIVERY_ACCOUNT_BYTES`): 25 MiB, 50 designs of the largest size.
pub const DEFAULT_ACCOUNT_BYTES: i64 = 25 * 1024 * 1024;

/// Rows of a moderator's page.
pub const ADMIN_PAGE: i64 = 100;

/// Who writes: the ticket's profile, its account, its mode and the account's byte quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Writer<'a> {
    /// The game (`fh2`).
    pub game: &'a str,
    /// `hardcore` or `noob` (the ticket's).
    pub mode: &'a str,
    /// The profile.
    pub profile_id: Uuid,
    /// Its account.
    pub account_id: Uuid,
    /// The account's byte quota ([`DEFAULT_ACCOUNT_BYTES`] unless configured).
    pub account_bytes: i64,
}

/// A design as `UpdateLivery` sends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Design<'a> {
    /// The design's name (cut to [`MAX_NAME_CHARS`]).
    pub name: &'a str,
    /// The car.
    pub car_id: i32,
    /// Its layer count.
    pub number_of_layers: i16,
    /// Its bytes (1 to [`MAX_DATA`]).
    pub data: &'a [u8],
}

fn name(n: &str) -> String {
    n.chars().take(MAX_NAME_CHARS).collect()
}

/// The SHA-256 of a design's bytes (the `livery_blob` key).
pub fn digest(data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(data).to_vec()
}

/// Whether a moderator blocked `account_id` from uploading liveries.
pub async fn upload_blocked_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM livery_upload_block WHERE account_id = $1)",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// The distinct livery bytes an account holds over its profiles.
pub async fn account_bytes_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<i64, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(sum(b.size), 0)::bigint FROM livery_blob b \
           JOIN profile p ON p.id = b.profile_id WHERE p.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// What [`save_draft_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    /// The design is the pending copy of (name, car).
    Stored,
    /// New bytes past the account's quota: nothing stored, and the drafts of that name are
    /// dropped, so the `CreateVersionedLivery` that follows finds no design.
    OverQuota,
}

/// Drops the profile's drafts named `livery_name` (any car) and releases their bytes.
async fn drop_drafts_named(
    conn: &mut PgConnection,
    w: Writer<'_>,
    livery_name: &str,
) -> Result<(), IdentityError> {
    let gone: Vec<Vec<u8>> = sqlx::query_scalar::<_, Option<Vec<u8>>>(
        "DELETE FROM game_livery_draft WHERE game = $1 AND profile_id = $2 AND name = $3 \
         RETURNING blob_sha256",
    )
    .bind(w.game)
    .bind(w.profile_id)
    .bind(livery_name)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .flatten()
    .collect();
    release_in(conn, w.profile_id, &gone).await?;
    Ok(())
}

/// Stores `design` as the profile's pending copy of (name, car), replacing the previous one, and
/// drops the profile's oldest drafts past [`MAX_DRAFTS`]; bytes the profile does not hold yet
/// count against the account's quota first (under the account's advisory lock), and past it
/// nothing is stored ([`Saved::OverQuota`]). The caller checked the size (the table refuses more
/// than [`MAX_DATA`] anyway).
pub async fn save_draft_in(
    conn: &mut PgConnection,
    w: Writer<'_>,
    design: &Design<'_>,
) -> Result<Saved, IdentityError> {
    let sha = digest(design.data);
    let size = i32::try_from(design.data.len()).unwrap_or(i32::MAX);
    let livery_name = name(design.name);
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('livery:' || $1::text, 0))")
        .bind(w.account_id)
        .execute(&mut *conn)
        .await?;
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM livery_blob WHERE profile_id = $1 AND sha256 = $2)",
    )
    .bind(w.profile_id)
    .bind(&sha)
    .fetch_one(&mut *conn)
    .await?;
    if !known && account_bytes_in(conn, w.account_id).await? + i64::from(size) > w.account_bytes {
        drop_drafts_named(conn, w, &livery_name).await?;
        return Ok(Saved::OverQuota);
    }
    // The bytes, once per profile. DO UPDATE (a no-op) rather than DO NOTHING: it locks an existing
    // row against a concurrent release, or waits for it and inserts again when it went.
    sqlx::query(
        "INSERT INTO livery_blob (game, profile_id, sha256, size, data) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (profile_id, sha256) DO UPDATE SET size = livery_blob.size",
    )
    .bind(w.game)
    .bind(w.profile_id)
    .bind(&sha)
    .bind(size)
    .bind(design.data)
    .execute(&mut *conn)
    .await?;
    let previous: Option<Option<Vec<u8>>> = sqlx::query_scalar(
        "SELECT blob_sha256 FROM game_livery_draft \
          WHERE game = $1 AND profile_id = $2 AND name = $3 AND car_id = $4 FOR UPDATE",
    )
    .bind(w.game)
    .bind(w.profile_id)
    .bind(&livery_name)
    .bind(design.car_id)
    .fetch_optional(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO game_livery_draft (game, profile_id, name, car_id, number_of_layers, data, blob_sha256, session_id) \
         VALUES ($1, $2, $3, $4, $5, NULL, $6, NULL) \
         ON CONFLICT (game, profile_id, name, car_id) DO UPDATE SET \
           number_of_layers = EXCLUDED.number_of_layers, data = NULL, \
           blob_sha256 = EXCLUDED.blob_sha256, session_id = NULL, updated_at = clock_timestamp()",
    )
    .bind(w.game)
    .bind(w.profile_id)
    .bind(&livery_name)
    .bind(design.car_id)
    .bind(design.number_of_layers)
    .bind(&sha)
    .execute(&mut *conn)
    .await?;
    let mut released: Vec<Vec<u8>> = sqlx::query_scalar::<_, Option<Vec<u8>>>(
        "DELETE FROM game_livery_draft d WHERE d.game = $1 AND d.profile_id = $2 \
           AND (d.name, d.car_id) NOT IN ( \
             SELECT k.name, k.car_id FROM game_livery_draft k WHERE k.game = $1 AND k.profile_id = $2 \
              ORDER BY k.updated_at DESC, k.name, k.car_id LIMIT $3) \
         RETURNING d.blob_sha256",
    )
    .bind(w.game)
    .bind(w.profile_id)
    .bind(MAX_DRAFTS)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .flatten()
    .collect();
    if let Some(Some(old)) = previous
        && old != sha
    {
        released.push(old);
    }
    release_in(conn, w.profile_id, &released).await?;
    Ok(Saved::Stored)
}

/// What [`create_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Created {
    /// A new versioned livery.
    New,
    /// The same id already is this profile's (a replayed request): nothing written.
    Replay,
    /// The id belongs to another profile (a 2^-62 collision): nothing written.
    Collision,
    /// No pending copy of that name (never saved, or refused over the quota): nothing written.
    NoDesign,
}

/// Creates the versioned livery `id` of (name, car) from the profile's pending copy of the car,
/// else the newest of the name (the Beta's fallback), read `FOR SHARE` before the insert. Then
/// drops the profile's oldest past [`MAX_PER_CAR`] of the car and [`MAX_PER_PROFILE`] in all.
/// Creating adds no bytes (it names the draft's), so the quota is [`save_draft_in`]'s.
pub async fn create_in(
    conn: &mut PgConnection,
    w: Writer<'_>,
    id: i64,
    livery_name: &str,
    car_id: i32,
) -> Result<Created, IdentityError> {
    let livery_name = name(livery_name);
    let owner_of = async |conn: &mut PgConnection| -> Result<Created, IdentityError> {
        let owner: Uuid = sqlx::query_scalar("SELECT profile_id FROM game_livery WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
        Ok(if owner == w.profile_id {
            Created::Replay
        } else {
            Created::Collision
        })
    };
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM game_livery WHERE id = $1)")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
    if exists {
        return owner_of(conn).await;
    }
    let draft = sqlx::query(
        "SELECT number_of_layers, data, blob_sha256 FROM game_livery_draft \
          WHERE game = $1 AND profile_id = $2 AND name = $3 \
          ORDER BY (car_id = $4) DESC, updated_at DESC LIMIT 1 FOR SHARE",
    )
    .bind(w.game)
    .bind(w.profile_id)
    .bind(&livery_name)
    .bind(car_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(draft) = draft else {
        return Ok(Created::NoDesign);
    };
    let inserted = sqlx::query(
        "INSERT INTO game_livery (id, game, mode, profile_id, name, car_id, number_of_layers, data, blob_sha256, session_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL) ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(w.game)
    .bind(w.mode)
    .bind(w.profile_id)
    .bind(&livery_name)
    .bind(car_id)
    .bind(draft.try_get::<i16, _>("number_of_layers")?)
    .bind(draft.try_get::<Option<Vec<u8>>, _>("data")?)
    .bind(draft.try_get::<Option<Vec<u8>>, _>("blob_sha256")?)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 0 {
        return owner_of(conn).await;
    }
    let released: Vec<Vec<u8>> = sqlx::query_scalar::<_, Option<Vec<u8>>>(
        "WITH ranked AS ( \
           SELECT id, row_number() OVER (PARTITION BY car_id ORDER BY created_at DESC, id) AS per_car, \
                  row_number() OVER (ORDER BY created_at DESC, id) AS overall \
             FROM game_livery WHERE game = $1 AND profile_id = $2 AND session_id IS NULL \
              AND NOT EXISTS (SELECT 1 FROM storefront_file f WHERE f.livery_id = game_livery.id)) \
         DELETE FROM game_livery l USING ranked r \
          WHERE l.id = r.id AND (r.per_car > $3 OR r.overall > $4) \
         RETURNING l.blob_sha256",
    )
    .bind(w.game)
    .bind(w.profile_id)
    .bind(MAX_PER_CAR)
    .bind(MAX_PER_PROFILE)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .flatten()
    .collect();
    release_in(conn, w.profile_id, &released).await?;
    Ok(Created::New)
}

/// The largest thumbnail kept (`VersionedLiveryData.liveryThumbnail` `max_len` in the game's
/// schema: 400 KiB).
pub const MAX_THUMBNAIL: usize = 409_600;

/// The format of a thumbnail, from its first bytes (`game_livery_thumbnail.format`).
pub fn thumbnail_format(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpeg"
    } else if data.starts_with(b"DDS ") {
        "dds"
    } else if data.starts_with(b"BM") {
        "bmp"
    } else {
        "unknown"
    }
}

/// Keeps `data` as the thumbnail of the caller's livery `id` (OD-137, CR-S73), on the transaction
/// that created it: false, nothing kept, when it is empty or over [`MAX_THUMBNAIL`], when `id` is
/// not the caller's, or when the livery already has one (a replayed create keeps the first).
pub async fn keep_thumbnail_in(
    conn: &mut PgConnection,
    w: Writer<'_>,
    id: i64,
    data: &[u8],
) -> Result<bool, IdentityError> {
    if data.is_empty() || data.len() > MAX_THUMBNAIL {
        return Ok(false);
    }
    let kept = sqlx::query(
        "INSERT INTO game_livery_thumbnail (livery_id, game, profile_id, size, format, data) \
         SELECT l.id, l.game, l.profile_id, $4, $5, $6 FROM game_livery l \
          WHERE l.id = $1 AND l.game = $2 AND l.profile_id = $3 \
         ON CONFLICT (livery_id) DO NOTHING",
    )
    .bind(id)
    .bind(w.game)
    .bind(w.profile_id)
    .bind(data.len() as i32)
    .bind(thumbnail_format(data))
    .bind(data)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(kept == 1)
}

/// `RemoveReferenceToVersionedLivery`: [`remove_in`], except that a livery the profile shares on
/// the Storefront stays (OD-138, CR-S74: other players' cars may carry its id; it goes with its
/// file's removal and the caps, or with a moderator's delete). The rows deleted.
pub async fn remove_unshared_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    id: i64,
) -> Result<u64, IdentityError> {
    let shared: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM storefront_file WHERE livery_id = $1 AND profile_id = $2)",
    )
    .bind(id)
    .bind(profile_id)
    .fetch_one(&mut *conn)
    .await?;
    if shared {
        return Ok(0);
    }
    remove_in(conn, game, profile_id, id).await
}

/// Drops the caller's reference to `id`: deletes it when it is the profile's own (anything else
/// is left alone, and the answer is the same). The rows deleted.
pub async fn remove_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    id: i64,
) -> Result<u64, IdentityError> {
    let gone: Vec<Option<Vec<u8>>> = sqlx::query_scalar(
        "DELETE FROM game_livery WHERE id = $1 AND game = $2 AND profile_id = $3 RETURNING blob_sha256",
    )
    .bind(id)
    .bind(game)
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    let n = gone.len() as u64;
    let shas: Vec<Vec<u8>> = gone.into_iter().flatten().collect();
    release_in(conn, profile_id, &shas).await?;
    Ok(n)
}

/// Deletes the `livery_blob` rows of `profile_id` among `shas` that no draft or livery names
/// any more and no moderator hid. The rows are locked first (in key order) and the references
/// read by a later statement, so a concurrent writer that locked one first keeps it. The rows
/// deleted.
pub async fn release_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    shas: &[Vec<u8>],
) -> Result<u64, IdentityError> {
    if shas.is_empty() {
        return Ok(0);
    }
    let _locked: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT sha256 FROM livery_blob WHERE profile_id = $1 AND sha256 = ANY ($2) \
          ORDER BY sha256 FOR UPDATE",
    )
    .bind(profile_id)
    .bind(shas)
    .fetch_all(&mut *conn)
    .await?;
    Ok(sqlx::query(
        "DELETE FROM livery_blob b WHERE b.profile_id = $1 AND b.sha256 = ANY ($2) \
           AND b.hidden_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM game_livery l \
                            WHERE l.profile_id = b.profile_id AND l.blob_sha256 = b.sha256) \
           AND NOT EXISTS (SELECT 1 FROM game_livery_draft d \
                            WHERE d.profile_id = b.profile_id AND d.blob_sha256 = b.sha256)",
    )
    .bind(profile_id)
    .bind(shas)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Who reads ([`retrieve_in`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reader<'a> {
    /// The ticket's game.
    pub game: &'a str,
    /// The ticket's mode.
    pub mode: &'a str,
    /// The ticket's profile.
    pub profile_id: Uuid,
    /// Whether designs with layers reach other players (`FI_LIVERY_LAYERED`, default on since the
    /// in-game check of GAME_PLATFORM.md §3a "Liveries" passed, owner 2026-10-27).
    pub layered_to_others: bool,
}

/// A livery as the game is handed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The car.
    pub car_id: i32,
    /// The layer count.
    pub number_of_layers: i16,
    /// The owner's XUID (the creator: only own designs are stored).
    pub creator_xuid: i64,
    /// The bytes.
    pub data: Vec<u8>,
}

/// Livery `id` if `reader` may see it, else `None` (missing, byte-less, hidden, ended, layered,
/// or not allowed: the same answer, so nothing tells them apart):
///
/// - its own, always (hidden or layered included: the owner's own game shows it anyway);
/// - another profile's, to a reader of that profile's current mode, to any reader while a live
///   Storefront file shares it (OD-141, CR-S75: the Storefront is both modes'), or to a reader
///   of the other mode while both hold a live place (host, or a seat `used` or reserved and
///   unexpired, each with its play session active and inside its game time) in the same open
///   world opened to `any` (a hosted world also needs its host's session live; an official world
///   has no host, so the two seats alone decide) (OD-141, CR-S76: "Sim, para quem está no
///   lobby"); always while the profile and its account are active and unbanned, neither the livery nor its bytes hidden by
///   a moderator, (until the in-game check, [`Reader::layered_to_others`]) the design has no
///   layers, and no Storefront file sharing it was taken off by reports (OD-138 follow-up).
///
/// A row written before CR-S59 with a session is served only while that session is `active`
/// and unexpired.
pub async fn retrieve_in(
    conn: &mut PgConnection,
    reader: Reader<'_>,
    id: i64,
) -> Result<Option<Seen>, IdentityError> {
    let sql = format!(
        "SELECT l.car_id, l.number_of_layers, coalesce(b.data, l.data) AS data, p.xuid \
           FROM game_livery l \
           JOIN profile p ON p.id = l.profile_id \
           JOIN account a ON a.id = p.account_id \
           LEFT JOIN livery_blob b ON b.profile_id = l.profile_id AND b.sha256 = l.blob_sha256 \
           LEFT JOIN play_session s ON s.id = l.session_id \
          WHERE l.id = $1 AND l.game = $2 AND coalesce(b.data, l.data) IS NOT NULL \
            AND (l.session_id IS NULL OR (s.state = 'active' AND s.expires_at > clock_timestamp())) \
            AND (l.profile_id = $4 OR ( \
                 (p.mode = $3 \
                   OR EXISTS (SELECT 1 FROM storefront_file sl WHERE sl.livery_id = l.id AND sl.removed_at IS NULL) \
                   OR EXISTS (SELECT 1 FROM world w WHERE w.game = $2 AND w.any_mode \
                        AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
                        AND (w.host_session_id IS NULL OR EXISTS (SELECT 1 FROM play_session hs WHERE hs.id = w.host_session_id \
                              AND hs.profile_id = w.host_profile_id AND hs.state = 'active' AND hs.game_until > clock_timestamp())) \
                        AND (w.host_profile_id = $4 OR EXISTS (SELECT 1 FROM mm_grant g JOIN play_session gs ON gs.id = g.session_id \
                              WHERE g.world_id = w.id AND g.profile_id = $4 AND gs.profile_id = g.profile_id \
                              AND gs.state = 'active' AND gs.game_until > clock_timestamp() \
                              AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())))) \
                        AND (w.host_profile_id = l.profile_id OR EXISTS (SELECT 1 FROM mm_grant g JOIN play_session gs ON gs.id = g.session_id \
                              WHERE g.world_id = w.id AND g.profile_id = l.profile_id AND gs.profile_id = g.profile_id \
                              AND gs.state = 'active' AND gs.game_until > clock_timestamp() \
                              AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())))))) \
                 AND l.hidden_at IS NULL AND b.hidden_at IS NULL \
                 AND ($5 OR l.number_of_layers = 0) AND {VISIBLE} \
                 AND NOT EXISTS (SELECT 1 FROM storefront_file sf \
                                  WHERE sf.livery_id = l.id AND sf.removed_at IS NOT NULL)))"
    );
    // Audited: `VISIBLE` is a constant; everything else is bound.
    let Some(row) = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(id)
        .bind(reader.game)
        .bind(reader.mode)
        .bind(reader.profile_id)
        .bind(reader.layered_to_others)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(Seen {
        car_id: row.try_get("car_id")?,
        number_of_layers: row.try_get("number_of_layers")?,
        creator_xuid: row.try_get("xuid")?,
        data: row.try_get("data")?,
    }))
}

/// `CreateVersionedLivery` per profile: 30 an hour (a repaint is one).
pub const CREATE_LIMIT: Limit = Limit {
    name: "livery.create",
    max: 30,
    window_s: 3600,
};

/// `UpdateLivery` per profile: 60 an hour (the game saves a design before naming it, and a
/// library edit saves too).
pub const UPDATE_LIMIT: Limit = Limit {
    name: "livery.update",
    max: 60,
    window_s: 3600,
};

/// `RetrieveVersionedLiveryById` per profile: 120 a minute (a lobby of cars, each fetched once
/// and then cached by the game under its id).
pub const RETRIEVE_LIMIT: Limit = Limit {
    name: "livery.retrieve",
    max: 120,
    window_s: 60,
};

/// The rate-limit key of `limit` for `profile` (SHA-256 of the profile id).
pub fn limit_key(limit: &Limit, profile: Uuid) -> String {
    let hex: String = digest(format!("livery:{profile}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{}:{hex}", limit.name)
}

/// Counts one call of `profile` on `limit`, in the caller's transaction.
pub async fn hit_in(
    conn: &mut PgConnection,
    limit: &Limit,
    profile: Uuid,
) -> Result<Hit, IdentityError> {
    limits::hit_in(conn, &limit_key(limit, profile), limit).await
}

// Moderation ----------------------------------------------------------------------------------

/// A livery as a moderator sees it (never the bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminRow {
    /// The versioned livery id.
    pub id: i64,
    /// The owner's account.
    pub account_id: Uuid,
    /// The owner's profile.
    pub profile_id: Uuid,
    /// The owner's XUID.
    pub xuid: i64,
    /// The game.
    pub game: String,
    /// The car.
    pub car_id: i32,
    /// The design's name.
    pub name: String,
    /// Its layer count.
    pub number_of_layers: i16,
    /// Its bytes' size (0: none).
    pub size: i32,
    /// Its bytes' SHA-256 (empty: none, or kept in the row before CR-S59).
    pub sha256: Vec<u8>,
    /// When it was created.
    pub created_at: UnixMs,
    /// When a moderator hid it or its bytes.
    pub hidden_at: Option<UnixMs>,
    /// The page cursor after this row (`<created µs>.<id>`).
    pub cursor: String,
}

const ADMIN_SELECT: &str = "SELECT l.id, p.account_id, l.profile_id, p.xuid, l.game, l.car_id, l.name, \
        l.number_of_layers, coalesce(b.size, octet_length(l.data), 0) AS size, \
        coalesce(l.blob_sha256, ''::bytea) AS sha256, l.created_at, \
        coalesce(l.hidden_at, b.hidden_at) AS hidden_at, \
        (extract(epoch FROM l.created_at) * 1000000)::bigint AS micros \
   FROM game_livery l JOIN profile p ON p.id = l.profile_id \
   LEFT JOIN livery_blob b ON b.profile_id = l.profile_id AND b.sha256 = l.blob_sha256";

fn ms(t: OffsetDateTime) -> UnixMs {
    UnixMs::from_millis(i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(0))
}

fn admin_row(r: &sqlx::postgres::PgRow) -> Result<AdminRow, sqlx::Error> {
    let id: i64 = r.try_get("id")?;
    let micros: i64 = r.try_get("micros")?;
    Ok(AdminRow {
        id,
        account_id: r.try_get("account_id")?,
        profile_id: r.try_get("profile_id")?,
        xuid: r.try_get("xuid")?,
        game: r.try_get("game")?,
        car_id: r.try_get("car_id")?,
        name: r.try_get("name")?,
        number_of_layers: r.try_get("number_of_layers")?,
        size: r.try_get("size")?,
        sha256: r.try_get("sha256")?,
        created_at: ms(r.try_get("created_at")?),
        hidden_at: r.try_get::<Option<OffsetDateTime>, _>("hidden_at")?.map(ms),
        cursor: format!("{micros}.{id}"),
    })
}

/// Parses a page cursor (`<created µs>.<id>`).
pub fn parse_cursor(text: &str) -> Option<(i64, i64)> {
    let (micros, id) = text.split_once('.')?;
    Some((micros.parse().ok()?, id.parse().ok()?))
}

/// One page of the liveries of every profile of `account_id`, newest first, at most
/// [`ADMIN_PAGE`], after `cursor` (the last row's [`AdminRow::cursor`]).
pub async fn admin_list_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    cursor: Option<(i64, i64)>,
) -> Result<Vec<AdminRow>, IdentityError> {
    let sql = format!(
        "SELECT * FROM ({ADMIN_SELECT} WHERE p.account_id = $1) x \
          WHERE $2::bigint IS NULL OR (x.micros, x.id) < ($2, $3) \
          ORDER BY x.micros DESC, x.id DESC LIMIT {ADMIN_PAGE}"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .bind(cursor.map(|c| c.0))
        .bind(cursor.map(|c| c.1))
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.iter().map(admin_row).collect::<Result<_, _>>()?)
}

/// One livery by id, as a moderator sees it.
pub async fn admin_get_in(
    conn: &mut PgConnection,
    id: i64,
) -> Result<Option<AdminRow>, IdentityError> {
    let sql = format!("{ADMIN_SELECT} WHERE l.id = $1");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.as_ref().map(admin_row).transpose()?)
}

/// Hides (or shows again) livery `id` and its bytes: hidden bytes stay hidden whatever livery
/// names them, so applying the same design again does not show it. The row after, `None` when
/// there is none.
pub async fn set_hidden_in(
    conn: &mut PgConnection,
    id: i64,
    hidden: bool,
) -> Result<Option<AdminRow>, IdentityError> {
    let named: Option<(Uuid, Option<Vec<u8>>)> = sqlx::query_as(
        "UPDATE game_livery SET hidden_at = CASE WHEN $2 THEN coalesce(hidden_at, clock_timestamp()) END \
          WHERE id = $1 RETURNING profile_id, blob_sha256",
    )
    .bind(id)
    .bind(hidden)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((profile, Some(sha))) = named {
        sqlx::query(
            "UPDATE livery_blob SET hidden_at = CASE WHEN $3 THEN coalesce(hidden_at, clock_timestamp()) END \
              WHERE profile_id = $1 AND sha256 = $2",
        )
        .bind(profile)
        .bind(sha)
        .bind(hidden)
        .execute(&mut *conn)
        .await?;
    }
    admin_get_in(conn, id).await
}

/// Deletes livery `id` (and its bytes when nothing else names them and they are not hidden);
/// the row it was, `None` when there is none.
pub async fn admin_delete_in(
    conn: &mut PgConnection,
    id: i64,
) -> Result<Option<AdminRow>, IdentityError> {
    let Some(row) = admin_get_in(conn, id).await? else {
        return Ok(None);
    };
    remove_in(conn, &row.game, row.profile_id, id).await?;
    Ok(Some(row))
}

/// A moderator's action on every livery of an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bulk {
    /// Hide every livery and its bytes.
    Hide,
    /// Delete every livery and draft (bytes released unless hidden).
    Delete,
}

/// Applies `action` to every livery of every profile of `account_id`; the liveries affected.
pub async fn admin_bulk_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    action: Bulk,
) -> Result<u64, IdentityError> {
    match action {
        Bulk::Hide => {
            let n = sqlx::query(
                "UPDATE game_livery l SET hidden_at = coalesce(l.hidden_at, clock_timestamp()) \
                   FROM profile p WHERE p.id = l.profile_id AND p.account_id = $1",
            )
            .bind(account_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            sqlx::query(
                "UPDATE livery_blob b SET hidden_at = coalesce(b.hidden_at, clock_timestamp()) \
                   FROM profile p WHERE p.id = b.profile_id AND p.account_id = $1",
            )
            .bind(account_id)
            .execute(&mut *conn)
            .await?;
            Ok(n)
        }
        Bulk::Delete => {
            let gone: Vec<(Uuid, Option<Vec<u8>>)> = sqlx::query_as(
                "DELETE FROM game_livery l USING profile p \
                  WHERE p.id = l.profile_id AND p.account_id = $1 RETURNING l.profile_id, l.blob_sha256",
            )
            .bind(account_id)
            .fetch_all(&mut *conn)
            .await?;
            let drafts: Vec<(Uuid, Option<Vec<u8>>)> = sqlx::query_as(
                "DELETE FROM game_livery_draft d USING profile p \
                  WHERE p.id = d.profile_id AND p.account_id = $1 RETURNING d.profile_id, d.blob_sha256",
            )
            .bind(account_id)
            .fetch_all(&mut *conn)
            .await?;
            let n = gone.len() as u64;
            let mut by_profile: std::collections::BTreeMap<Uuid, Vec<Vec<u8>>> =
                std::collections::BTreeMap::new();
            for (profile, sha) in gone.into_iter().chain(drafts) {
                if let Some(sha) = sha {
                    by_profile.entry(profile).or_default().push(sha);
                }
            }
            for (profile, shas) in by_profile {
                release_in(conn, profile, &shas).await?;
            }
            Ok(n)
        }
    }
}

/// Blocks (or unblocks) `account_id` from uploading liveries; whether it was blocked before.
pub async fn set_upload_block_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    blocked: bool,
) -> Result<bool, IdentityError> {
    let before = upload_blocked_in(conn, account_id).await?;
    if blocked {
        sqlx::query(
            "INSERT INTO livery_upload_block (account_id) VALUES ($1) ON CONFLICT (account_id) DO NOTHING",
        )
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query("DELETE FROM livery_upload_block WHERE account_id = $1")
            .bind(account_id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_formats_are_sniffed_from_the_first_bytes() {
        assert_eq!(thumbnail_format(b"\x89PNG\r\n\x1a\n\0\0"), "png");
        assert_eq!(thumbnail_format(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpeg");
        assert_eq!(thumbnail_format(b"DDS |\0\0\0"), "dds");
        assert_eq!(thumbnail_format(b"BM6\0"), "bmp");
        assert_eq!(thumbnail_format(&[0x00, 0xFF]), "unknown");
        assert_eq!(thumbnail_format(&[]), "unknown");
        assert_eq!(thumbnail_format(b"\x89PN"), "unknown");
    }
}
