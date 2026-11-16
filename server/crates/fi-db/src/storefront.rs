//! The Storefront (owner decision OD-138, 2026-10-29; GAME_PLATFORM.md §3a "Storefront", register
//! CR-S74): players share designs, vinyl groups and tunes, browse, download, like and report them,
//! ported from the Beta's `db/storefront.rs` (migration `20261029130000_storefront.sql`).
//!
//! - **Libraries** the game syncs: vinyl groups ([`save_group_draft_in`], by name) and car setups
//!   ([`save_setup_in`], by car and name). `CreateVersionedLayerGroup` / `CreateVersionedTune`
//!   send only a name, so their content comes from here, as a livery's comes from its draft.
//! - **Versioned content** ([`create_group_in`], [`create_tune_in`], [`create_tune_from_in`]):
//!   what a shared file points at and other games fetch by id ([`retrieve_group_in`],
//!   [`retrieve_tune_in`]). Each profile keeps at most [`MAX_VERSIONS`] of each kind that no file
//!   shares, the oldest dropped.
//! - **Shared files** ([`share_in`]): one per shared item (a livery, a vinyl group or a tune), gone
//!   with it; a shared item is never pruned while its file lives (`fi_db::liveries` skips shared
//!   liveries too). At most [`MAX_SHARED`] per profile.
//! - **Who sees a file:** its owner, always; every player of the game, **both modes** (OD-141,
//!   CR-S75: "Comum aos dois", superseding OD-138's "modes never meet" here), while the owner is
//!   visible ([`VISIBLE`]), the file is not taken off by reports and the item is not hidden (the
//!   livery or its bytes, a group or a tune). Every player, not only friends: the owner's
//!   "everything should be shared". Searches, curated lists, details, marks and follows all cross
//!   modes; the viewer's mode (`$2`) is still bound but no longer filters.
//! - **Counters** ([`mark_in`]): a download, like, use or view counts once per account and file,
//!   never on a file of the marker's own account; a report is a strike; [`STRIKES_TO_REMOVE`]
//!   reports from different accounts take the file off until a moderator decides. A taken-off file
//!   stays through its owner's Remove and re-share (also of the same content as another item), and
//!   its item is missing to everyone but its owner (`fi_db::liveries::retrieve_in`,
//!   [`retrieve_group_in`], [`retrieve_tune_in`]). Markers are stored as a pseudonymous SHA-256 of
//!   their account id ([`account_key`]), followers of their profile id ([`viewer_key`]); both go on
//!   erasure.
//!
//! Text the game posts (titles, descriptions, names) is cleaned by the caller. Nothing here logs a
//! XUID, a name or content bytes.

use crate::identity::{IdentityError, ms};
use crate::visibility::VISIBLE;
use fi_domain::UnixMs;
use serde_json::Value as Json;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Shared files one profile keeps (the Beta's 100).
pub const MAX_SHARED: i64 = 100;
/// Vinyl groups in one profile's library.
pub const MAX_GROUP_DRAFTS: i64 = 200;
/// Car setups in one profile's library.
pub const MAX_SETUPS: i64 = 300;
/// Versioned vinyl groups or tunes one profile keeps of each kind besides the shared ones.
pub const MAX_VERSIONS: i64 = 100;
/// A vinyl group's bytes (`VersionedLayerGroupData.data` `max_len`).
pub const MAX_GROUP_DATA: usize = 262_144;
/// A preview or thumbnail (`ForzaStorefrontFile.PreviewPayload` `max_len`).
pub const MAX_PREVIEW: usize = 409_600;
/// Reports from different players that take a file off the storefront (the Beta's).
pub const STRIKES_TO_REMOVE: i16 = 3;
/// The longest name kept (library entries and versions).
pub const MAX_NAME_CHARS: usize = 64;

/// Who reads: the ticket's game, mode and profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewer<'a> {
    /// The game (`fh2`).
    pub game: &'a str,
    /// `hardcore` or `noob` (the ticket's).
    pub mode: &'a str,
    /// The profile.
    pub profile_id: Uuid,
    /// Its account (marks count once per account, OD-138 follow-up).
    pub account_id: Uuid,
}

/// A shared item's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A design (`game_livery`).
    Livery,
    /// A vinyl group (`game_layer_group`).
    Group,
    /// A tune (`game_tune`).
    Tune,
}

impl Kind {
    /// `storefront_file.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Livery => "livery",
            Kind::Group => "group",
            Kind::Tune => "tune",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Kind::Livery => "livery_id",
            Kind::Group => "group_id",
            Kind::Tune => "tune_id",
        }
    }

    fn of(s: &str) -> Option<Kind> {
        match s {
            "livery" => Some(Kind::Livery),
            "group" => Some(Kind::Group),
            "tune" => Some(Kind::Tune),
            _ => None,
        }
    }
}

/// A follower's key in `storefront_follow`: a pseudonymous SHA-256 of its profile id.
pub fn viewer_key(profile_id: Uuid) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(format!("storefront:{profile_id}").as_bytes()).to_vec()
}

/// A marker's key in `storefront_mark`: a pseudonymous SHA-256 of its account id, so every
/// profile of one account counts once (a report, a like, a use, a view, a download).
pub fn account_key(account_id: Uuid) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(format!("storefront-account:{account_id}").as_bytes()).to_vec()
}

/// The longest search text kept (`ManualSearchKeywords`, `GamerTag`), characters.
pub const MAX_SEARCH_CHARS: usize = 64;

fn name(n: &str) -> String {
    n.chars().take(MAX_NAME_CHARS).collect()
}

// Libraries -----------------------------------------------------------------------------------

/// `OnlineProfile.UpdateLiveryGroup`: the group's bytes by name. False, nothing kept, for empty or
/// oversized bytes, an empty name, or a full library (a new name past [`MAX_GROUP_DRAFTS`]).
pub async fn save_group_draft_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    group_name: &str,
    layers: i16,
    data: &[u8],
) -> Result<bool, IdentityError> {
    let group_name = name(group_name);
    if group_name.is_empty() || data.is_empty() || data.len() > MAX_GROUP_DATA {
        return Ok(false);
    }
    let n = sqlx::query(
        "INSERT INTO game_layer_group_draft (game, profile_id, name, number_of_layers, data) \
         SELECT $1, $2, $3, $4, $5 \
          WHERE EXISTS (SELECT 1 FROM game_layer_group_draft WHERE profile_id = $2 AND name = $3) \
             OR (SELECT count(*) FROM game_layer_group_draft WHERE profile_id = $2) < $6 \
         ON CONFLICT (profile_id, name) DO UPDATE SET number_of_layers = EXCLUDED.number_of_layers, \
           data = EXCLUDED.data, updated_at = clock_timestamp()",
    )
    .bind(v.game)
    .bind(v.profile_id)
    .bind(&group_name)
    .bind(layers.max(0))
    .bind(data)
    .bind(MAX_GROUP_DRAFTS)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// `OnlineProfile.DeleteLiveryGroup` (`to` `None`) or `RenameLiveryGroup`: the caller's own entry;
/// a rename onto an existing name replaces it. The rows changed.
pub async fn edit_group_draft_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    from: &str,
    to: Option<&str>,
) -> Result<u64, IdentityError> {
    let from = name(from);
    Ok(match to.map(name) {
        None => {
            sqlx::query("DELETE FROM game_layer_group_draft WHERE profile_id = $1 AND name = $2")
                .bind(profile_id)
                .bind(&from)
                .execute(&mut *conn)
                .await?
                .rows_affected()
        }
        Some(to) if to.is_empty() || to == from => 0,
        Some(to) => {
            sqlx::query("DELETE FROM game_layer_group_draft WHERE profile_id = $1 AND name = $2")
                .bind(profile_id)
                .bind(&to)
                .execute(&mut *conn)
                .await?;
            sqlx::query(
                "UPDATE game_layer_group_draft SET name = $3, updated_at = clock_timestamp() \
                  WHERE profile_id = $1 AND name = $2",
            )
            .bind(profile_id)
            .bind(&from)
            .bind(&to)
            .execute(&mut *conn)
            .await?
            .rows_affected()
        }
    })
}

/// A car setup as `UpdateCarSetup` sends it.
#[derive(Clone, Debug, PartialEq)]
pub struct Setup<'a> {
    /// The car.
    pub car_id: i32,
    /// The setup's name.
    pub name: &'a str,
    /// `binaryVersion`.
    pub binary_version: i16,
    /// `installedParts` as decoded.
    pub installed_parts: &'a Json,
    /// `tuning` as decoded.
    pub tuning: &'a Json,
}

/// `OnlineProfile.UpdateCarSetup`: the setup by car and name. False, nothing kept, for an empty
/// name or a full library (a new entry past [`MAX_SETUPS`]).
pub async fn save_setup_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    s: &Setup<'_>,
) -> Result<bool, IdentityError> {
    let setup_name = name(s.name);
    if setup_name.is_empty() {
        return Ok(false);
    }
    let n = sqlx::query(
        "INSERT INTO game_car_setup (game, profile_id, car_id, name, binary_version, installed_parts, tuning) \
         SELECT $1, $2, $3, $4, $5, $6, $7 \
          WHERE EXISTS (SELECT 1 FROM game_car_setup WHERE profile_id = $2 AND car_id = $3 AND name = $4) \
             OR (SELECT count(*) FROM game_car_setup WHERE profile_id = $2) < $8 \
         ON CONFLICT (profile_id, car_id, name) DO UPDATE SET binary_version = EXCLUDED.binary_version, \
           installed_parts = EXCLUDED.installed_parts, tuning = EXCLUDED.tuning, updated_at = clock_timestamp()",
    )
    .bind(v.game)
    .bind(v.profile_id)
    .bind(s.car_id)
    .bind(&setup_name)
    .bind(s.binary_version)
    .bind(s.installed_parts)
    .bind(s.tuning)
    .bind(MAX_SETUPS)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// `OnlineProfile.DeleteCarSetup` (`to` `None`) or `RenameCarSetup`: the caller's own entry; a
/// rename onto an existing name replaces it. The rows changed.
pub async fn edit_setup_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    car_id: i32,
    from: &str,
    to: Option<&str>,
) -> Result<u64, IdentityError> {
    let from = name(from);
    let delete = "DELETE FROM game_car_setup WHERE profile_id = $1 AND car_id = $2 AND name = $3";
    Ok(match to.map(name) {
        None => sqlx::query(delete)
            .bind(profile_id)
            .bind(car_id)
            .bind(&from)
            .execute(&mut *conn)
            .await?
            .rows_affected(),
        Some(to) if to.is_empty() || to == from => 0,
        Some(to) => {
            sqlx::query(delete)
                .bind(profile_id)
                .bind(car_id)
                .bind(&to)
                .execute(&mut *conn)
                .await?;
            sqlx::query(
                "UPDATE game_car_setup SET name = $4, updated_at = clock_timestamp() \
                  WHERE profile_id = $1 AND car_id = $2 AND name = $3",
            )
            .bind(profile_id)
            .bind(car_id)
            .bind(&from)
            .bind(&to)
            .execute(&mut *conn)
            .await?
            .rows_affected()
        }
    })
}

// Versioned content ---------------------------------------------------------------------------

/// What a `CreateVersioned*` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Versioned {
    /// Created under the caller's id.
    New,
    /// A replay: the id is already the caller's.
    Replay,
    /// Nothing in the library by that name.
    NoSource,
    /// The id is another profile's (never expected: 62 random bits).
    Collision,
}

async fn replay_of(
    conn: &mut PgConnection,
    table: &'static str,
    id: i64,
    profile_id: Uuid,
) -> Result<Option<Versioned>, IdentityError> {
    // Audited: `table` is one of two constants.
    let owner: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT profile_id FROM {table} WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(owner.map(|o| {
        if o == profile_id {
            Versioned::Replay
        } else {
            Versioned::Collision
        }
    }))
}

/// Drops the profile's oldest versions of `table` past [`MAX_VERSIONS`] that no file shares.
async fn prune_in(
    conn: &mut PgConnection,
    table: &'static str,
    column: &'static str,
    profile_id: Uuid,
) -> Result<(), IdentityError> {
    // Audited: `table` and `column` are constants.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} t WHERE t.id IN ( \
           SELECT id FROM ( \
             SELECT v.id, row_number() OVER (ORDER BY v.created_at DESC, v.id) AS n FROM {table} v \
              WHERE v.profile_id = $1 \
                AND NOT EXISTS (SELECT 1 FROM storefront_file f WHERE f.{column} = v.id)) r \
            WHERE r.n > $2)"
    )))
    .bind(profile_id)
    .bind(MAX_VERSIONS)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `ContentVersioning.CreateVersionedLayerGroup`: a versioned group from the library entry
/// `group_name`, with the game's thumbnail (kept when 1 to [`MAX_PREVIEW`] bytes).
pub async fn create_group_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    id: i64,
    group_name: &str,
    thumbnail: &[u8],
) -> Result<Versioned, IdentityError> {
    if let Some(done) = replay_of(conn, "game_layer_group", id, v.profile_id).await? {
        return Ok(done);
    }
    let thumbnail = (!thumbnail.is_empty() && thumbnail.len() <= MAX_PREVIEW).then_some(thumbnail);
    let n = sqlx::query(
        "INSERT INTO game_layer_group (id, game, profile_id, name, number_of_layers, data, thumbnail) \
         SELECT $1, d.game, d.profile_id, d.name, d.number_of_layers, d.data, $4 \
           FROM game_layer_group_draft d WHERE d.game = $2 AND d.profile_id = $3 AND d.name = $5 \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(v.game)
    .bind(v.profile_id)
    .bind(thumbnail)
    .bind(name(group_name))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(replay_of(conn, "game_layer_group", id, v.profile_id)
            .await?
            .unwrap_or(Versioned::NoSource));
    }
    prune_in(conn, "game_layer_group", "group_id", v.profile_id).await?;
    Ok(Versioned::New)
}

/// A versioned vinyl group as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupSeen {
    /// Its name.
    pub name: String,
    /// Its layer count.
    pub number_of_layers: i16,
    /// The owner's XUID (the creator is always the owner).
    pub creator_xuid: i64,
    /// Its bytes.
    pub data: Vec<u8>,
    /// Its thumbnail, when kept.
    pub thumbnail: Option<Vec<u8>>,
}

/// The owner condition of a versioned item `v` joined with its profile `p` and account `a`: the
/// caller's own, or a visible owner of either mode (OD-141; `$2`, the caller's mode, is bound and
/// unused), the item not hidden and no file sharing
/// it (`column` of `storefront_file`) taken off by reports.
fn readable(column: &str) -> String {
    format!(
        "(v.profile_id = $3 OR (v.hidden_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM storefront_file sf WHERE sf.{column} = v.id AND sf.removed_at IS NOT NULL) \
           AND {VISIBLE}))"
    )
}

/// `ContentVersioning.RetrieveVersionedLayerGroupById`: `None` for anything the viewer may not see.
pub async fn retrieve_group_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    id: i64,
) -> Result<Option<GroupSeen>, IdentityError> {
    let sql = format!(
        "SELECT v.name, v.number_of_layers, v.data, v.thumbnail, p.xuid FROM game_layer_group v \
           JOIN profile p ON p.id = v.profile_id JOIN account a ON a.id = p.account_id \
          WHERE v.id = $1 AND v.game = $4 AND {}",
        readable("group_id")
    );
    // Audited: constants only; everything else is bound.
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(id)
        .bind(v.mode)
        .bind(v.profile_id)
        .bind(v.game)
        .fetch_optional(&mut *conn)
        .await?;
    row.map(|r| {
        Ok(GroupSeen {
            name: r.try_get("name")?,
            number_of_layers: r.try_get("number_of_layers")?,
            creator_xuid: r.try_get("xuid")?,
            data: r.try_get("data")?,
            thumbnail: r.try_get("thumbnail")?,
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// The content of a versioned tune.
#[derive(Clone, Debug, PartialEq)]
pub struct Tune {
    /// The car.
    pub car_id: i32,
    /// Its name (empty for `CreateVersionedTuneFromStructs`).
    pub name: String,
    /// `binaryVersion`.
    pub binary_version: i16,
    /// `installedParts` as decoded.
    pub installed_parts: Json,
    /// `tuning` as decoded.
    pub tuning: Json,
}

async fn insert_tune_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    id: i64,
    t: &Tune,
) -> Result<Versioned, IdentityError> {
    let n = sqlx::query(
        "INSERT INTO game_tune (id, game, profile_id, car_id, name, binary_version, installed_parts, tuning) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(v.game)
    .bind(v.profile_id)
    .bind(t.car_id)
    .bind(name(&t.name))
    .bind(t.binary_version)
    .bind(&t.installed_parts)
    .bind(&t.tuning)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(replay_of(conn, "game_tune", id, v.profile_id)
            .await?
            .unwrap_or(Versioned::Collision));
    }
    prune_in(conn, "game_tune", "tune_id", v.profile_id).await?;
    Ok(Versioned::New)
}

/// `ContentVersioning.CreateVersionedTune`: a versioned tune from the library setup
/// (`car_id`, `setup_name`).
pub async fn create_tune_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    id: i64,
    car_id: i32,
    setup_name: &str,
) -> Result<Versioned, IdentityError> {
    if let Some(done) = replay_of(conn, "game_tune", id, v.profile_id).await? {
        return Ok(done);
    }
    let setup = sqlx::query(
        "SELECT name, binary_version, installed_parts, tuning FROM game_car_setup \
          WHERE game = $1 AND profile_id = $2 AND car_id = $3 AND name = $4",
    )
    .bind(v.game)
    .bind(v.profile_id)
    .bind(car_id)
    .bind(name(setup_name))
    .fetch_optional(&mut *conn)
    .await?;
    let Some(s) = setup else {
        return Ok(Versioned::NoSource);
    };
    let tune = Tune {
        car_id,
        name: s.try_get("name")?,
        binary_version: s.try_get("binary_version")?,
        installed_parts: s.try_get("installed_parts")?,
        tuning: s.try_get("tuning")?,
    };
    insert_tune_in(conn, v, id, &tune).await
}

/// `ContentVersioning.CreateVersionedTuneFromStructs`: a versioned tune from the parts and tuning
/// the call carries.
pub async fn create_tune_from_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    id: i64,
    tune: &Tune,
) -> Result<Versioned, IdentityError> {
    if let Some(done) = replay_of(conn, "game_tune", id, v.profile_id).await? {
        return Ok(done);
    }
    insert_tune_in(conn, v, id, tune).await
}

/// A versioned tune as a reader sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct TuneSeen {
    /// Its content.
    pub tune: Tune,
    /// The owner's XUID (the creator is always the owner).
    pub creator_xuid: i64,
    /// The owner's name (`None` once erased).
    pub creator_name: Option<String>,
}

/// `ContentVersioning.RetrieveVersionedTuneById`: `None` for anything the viewer may not see.
pub async fn retrieve_tune_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    id: i64,
) -> Result<Option<TuneSeen>, IdentityError> {
    let sql = format!(
        "SELECT v.car_id, v.name, v.binary_version, v.installed_parts, v.tuning, p.xuid, p.name AS owner \
           FROM game_tune v JOIN profile p ON p.id = v.profile_id JOIN account a ON a.id = p.account_id \
          WHERE v.id = $1 AND v.game = $4 AND {}",
        readable("tune_id")
    );
    // Audited: constants only; everything else is bound.
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(id)
        .bind(v.mode)
        .bind(v.profile_id)
        .bind(v.game)
        .fetch_optional(&mut *conn)
        .await?;
    row.map(|r| {
        Ok(TuneSeen {
            tune: Tune {
                car_id: r.try_get("car_id")?,
                name: r.try_get("name")?,
                binary_version: r.try_get("binary_version")?,
                installed_parts: r.try_get("installed_parts")?,
                tuning: r.try_get("tuning")?,
            },
            creator_xuid: r.try_get("xuid")?,
            creator_name: r.try_get("owner")?,
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

// Shared files --------------------------------------------------------------------------------

/// A `Storefront*.Create`.
#[derive(Clone, Debug, PartialEq)]
pub struct NewFile<'a> {
    /// What is shared.
    pub kind: Kind,
    /// The caller's own versioned item.
    pub item_id: i64,
    /// `fileHeader.Type`.
    pub file_type: i32,
    /// Cleaned title (at most 16 characters).
    pub title: &'a str,
    /// Cleaned description (at most 256 characters).
    pub description: &'a str,
    /// `KeywordOne` (an id of the server's list, else 0).
    pub keyword_one: i16,
    /// `KeywordTwo`.
    pub keyword_two: i16,
    /// A tune's search fields (`{}` otherwise).
    pub meta: &'a Json,
    /// The game's preview (kept when 1 to [`MAX_PREVIEW`] bytes).
    pub preview: &'a [u8],
}

/// What [`share_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shared {
    /// Shared (or shared again: the same file, its header updated) under this id.
    File(i32),
    /// The item is not the caller's (or no longer exists).
    NoItem,
    /// The profile already shares [`MAX_SHARED`] files.
    TooMany,
    /// The file was taken off by reports: it stays off until a moderator decides.
    Removed,
}

/// The caller's own item of `kind` with id `item_id`: its layers and car (a group has no car, -1);
/// `None` when it is not the caller's (or no longer exists).
pub async fn own_item_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    kind: Kind,
    item_id: i64,
) -> Result<Option<(i16, i32)>, IdentityError> {
    let sql = match kind {
        Kind::Livery => {
            "SELECT number_of_layers, car_id FROM game_livery WHERE id = $1 AND game = $2 AND profile_id = $3"
        }
        Kind::Group => {
            "SELECT number_of_layers, -1 AS car_id FROM game_layer_group WHERE id = $1 AND game = $2 AND profile_id = $3"
        }
        Kind::Tune => {
            "SELECT 0::smallint AS number_of_layers, car_id FROM game_tune WHERE id = $1 AND game = $2 AND profile_id = $3"
        }
    };
    Ok(sqlx::query_as(sql)
        .bind(item_id)
        .bind(v.game)
        .bind(v.profile_id)
        .fetch_optional(&mut *conn)
        .await?)
}

/// Shares the caller's own item: the file's header from the call, its layers from the item.
pub async fn share_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    f: &NewFile<'_>,
) -> Result<Shared, IdentityError> {
    // The item's own layers and car: never the header's (a group has no car, -1).
    let Some((layers, car_id)) = own_item_in(conn, v, f.kind, f.item_id).await? else {
        return Ok(Shared::NoItem);
    };
    if taken_off_content_in(conn, v, f.kind, f.item_id).await? {
        return Ok(Shared::Removed);
    }
    let column = f.kind.column();
    // Audited: `column` is a constant.
    let existing: Option<(i32, Option<OffsetDateTime>)> = sqlx::query_as(sqlx::AssertSqlSafe(
        format!("SELECT id, removed_at FROM storefront_file WHERE {column} = $1 FOR UPDATE"),
    ))
    .bind(f.item_id)
    .fetch_optional(&mut *conn)
    .await?;
    let preview = (!f.preview.is_empty() && f.preview.len() <= MAX_PREVIEW).then_some(f.preview);
    if let Some((id, removed)) = existing {
        if removed.is_some() {
            return Ok(Shared::Removed);
        }
        sqlx::query(
            "UPDATE storefront_file SET title = $2, description = $3, keyword_one = $4, keyword_two = $5, \
               meta = $6, preview = coalesce($7, preview) WHERE id = $1",
        )
        .bind(id)
        .bind(f.title)
        .bind(f.description)
        .bind(f.keyword_one)
        .bind(f.keyword_two)
        .bind(f.meta)
        .bind(preview)
        .execute(&mut *conn)
        .await?;
        return Ok(Shared::File(id));
    }
    let shared: i64 =
        sqlx::query_scalar("SELECT count(*) FROM storefront_file WHERE profile_id = $1")
            .bind(v.profile_id)
            .fetch_one(&mut *conn)
            .await?;
    if shared >= MAX_SHARED {
        return Ok(Shared::TooMany);
    }
    // Audited: `column` is a constant.
    let id: i32 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "INSERT INTO storefront_file (game, profile_id, kind, {column}, file_type, car_id, title, description, \
           keyword_one, keyword_two, layers, meta, preview) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id"
    )))
    .bind(v.game)
    .bind(v.profile_id)
    .bind(f.kind.as_str())
    .bind(f.item_id)
    .bind(f.file_type)
    .bind(car_id)
    .bind(f.title)
    .bind(f.description)
    .bind(f.keyword_one)
    .bind(f.keyword_two)
    .bind(layers.max(0))
    .bind(f.meta)
    .bind(preview)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Shared::File(id))
}

/// Whether the account already has a file taken off by reports that shares the same content as
/// `kind`'s item `item_id` (another versioned copy of the same design bytes, vinyl group bytes or
/// tune), so a new version of reported content stays off until a moderator decides.
async fn taken_off_content_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    kind: Kind,
    item_id: i64,
) -> Result<bool, IdentityError> {
    let sql = match kind {
        Kind::Livery => {
            "SELECT EXISTS (SELECT 1 FROM game_livery me \
               JOIN game_livery o ON o.profile_id IN (SELECT id FROM profile WHERE account_id = $2) \
                AND ((me.blob_sha256 IS NOT NULL AND o.blob_sha256 = me.blob_sha256) OR (me.data IS NOT NULL AND o.data = me.data)) \
               JOIN storefront_file f ON f.livery_id = o.id AND f.removed_at IS NOT NULL \
              WHERE me.id = $1)"
        }
        Kind::Group => {
            "SELECT EXISTS (SELECT 1 FROM game_layer_group me \
               JOIN game_layer_group o ON o.profile_id IN (SELECT id FROM profile WHERE account_id = $2) \
                AND o.data = me.data \
               JOIN storefront_file f ON f.group_id = o.id AND f.removed_at IS NOT NULL \
              WHERE me.id = $1)"
        }
        Kind::Tune => {
            "SELECT EXISTS (SELECT 1 FROM game_tune me \
               JOIN game_tune o ON o.profile_id IN (SELECT id FROM profile WHERE account_id = $2) \
                AND o.car_id = me.car_id AND o.installed_parts = me.installed_parts AND o.tuning = me.tuning \
               JOIN storefront_file f ON f.tune_id = o.id AND f.removed_at IS NOT NULL \
              WHERE me.id = $1)"
        }
    };
    Ok(sqlx::query_scalar(sql)
        .bind(item_id)
        .bind(v.account_id)
        .fetch_one(&mut *conn)
        .await?)
}

/// A shared file as a reader sees it (never its preview: [`preview_in`]).
#[derive(Clone, Debug, PartialEq)]
pub struct File {
    /// `contentFileId`.
    pub id: i32,
    /// What it shares.
    pub kind: Kind,
    /// The owner's profile.
    pub profile_id: Uuid,
    /// The owner's XUID.
    pub xuid: i64,
    /// `fileHeader.Type`.
    pub file_type: i32,
    /// The shared versioned item.
    pub item_id: i64,
    /// The car (-1 for a group).
    pub car_id: i32,
    /// Title.
    pub title: String,
    /// Description.
    pub description: String,
    /// `KeywordOne`.
    pub keyword_one: i16,
    /// `KeywordTwo`.
    pub keyword_two: i16,
    /// Layers of the shared design or group.
    pub layers: i16,
    /// A tune's search fields.
    pub meta: Json,
    /// Downloads.
    pub downloads: i32,
    /// Likes.
    pub likes: i32,
    /// Uses.
    pub uses: i32,
    /// Taken off by reports (shown to its owner as under review).
    pub removed: bool,
    /// When shared.
    pub created_at: UnixMs,
}

const FILE_COLUMNS: &str = "f.id, f.kind, f.profile_id, p.xuid, f.file_type, \
     coalesce(f.livery_id, f.group_id, f.tune_id) AS item_id, f.car_id, f.title, f.description, \
     f.keyword_one, f.keyword_two, f.layers, f.meta, f.downloads, f.likes, f.uses, \
     f.removed_at IS NOT NULL AS removed, f.created_at";

/// The joins and conditions of a file `f` the viewer may see (`$1` game, `$2` mode, bound and unused
/// since OD-141, `$3` profile): its own, or a visible owner of either mode, the file on and its item
/// not hidden.
fn seen_from() -> String {
    format!(
        "FROM storefront_file f JOIN profile p ON p.id = f.profile_id JOIN account a ON a.id = p.account_id \
           LEFT JOIN game_livery l ON l.id = f.livery_id \
           LEFT JOIN livery_blob b ON b.profile_id = l.profile_id AND b.sha256 = l.blob_sha256 \
           LEFT JOIN game_layer_group g ON g.id = f.group_id \
           LEFT JOIN game_tune t ON t.id = f.tune_id \
          WHERE f.game = $1 AND (f.profile_id = $3 OR ( \
                f.removed_at IS NULL AND {VISIBLE} \
                AND l.hidden_at IS NULL AND b.hidden_at IS NULL AND g.hidden_at IS NULL AND t.hidden_at IS NULL))"
    )
}

fn file_of(r: &sqlx::postgres::PgRow) -> Result<File, sqlx::Error> {
    let kind: String = r.try_get("kind")?;
    Ok(File {
        id: r.try_get("id")?,
        kind: Kind::of(&kind).unwrap_or(Kind::Livery),
        profile_id: r.try_get("profile_id")?,
        xuid: r.try_get("xuid")?,
        file_type: r.try_get("file_type")?,
        item_id: r.try_get("item_id")?,
        car_id: r.try_get("car_id")?,
        title: r.try_get("title")?,
        description: r.try_get("description")?,
        keyword_one: r.try_get("keyword_one")?,
        keyword_two: r.try_get("keyword_two")?,
        layers: r.try_get("layers")?,
        meta: r.try_get("meta")?,
        downloads: r.try_get("downloads")?,
        likes: r.try_get("likes")?,
        uses: r.try_get("uses")?,
        removed: r.try_get("removed")?,
        created_at: ms(r.try_get::<OffsetDateTime, _>("created_at")?),
    })
}

/// A search (`Search*Files`, `GetCuratedContent`, `GetMyFileList`); `None` and 0 mean "any".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query<'a> {
    /// The kind.
    pub kind: Option<Kind>,
    /// `fileType`.
    pub file_type: Option<i32>,
    /// Only the viewer's own files (`GetMyFileList`; removed ones included).
    pub own: bool,
    /// Only these cars (the model, and every car of the make).
    pub cars: Option<Vec<i32>>,
    /// A class: a file of a car of `class_cars`, or a tune whose `meta.ClassId` is it.
    pub class_id: Option<i64>,
    /// The cars of `class_id` (from the game's catalog).
    pub class_cars: Vec<i32>,
    /// `KeywordOneId`.
    pub keyword_one: Option<i16>,
    /// `KeywordTwoId`.
    pub keyword_two: Option<i16>,
    /// A tune's track (`meta.TrackId`).
    pub track_id: Option<i64>,
    /// At most this many layers.
    pub max_layers: Option<i16>,
    /// In the title or description (case-insensitive).
    pub text: &'a str,
    /// The owner's name starts with it (case-insensitive).
    pub gamertag: &'a str,
    /// Most used first (`GetCuratedContent`), the viewer's own last; newest first otherwise.
    pub popular: bool,
    /// At most this many.
    pub limit: i64,
}

fn like_pattern(text: &str) -> String {
    let escaped: String = text
        .chars()
        .flat_map(|c| match c {
            '%' | '_' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    format!("%{escaped}%")
}

/// A file's popularity score: `3 × uses + 2 × downloads + likes` (FH2-S17). Every mark counts
/// once per account, so the score is a count of distinct players weighted by what each did: a
/// use (the item put on a car or raced) costs the player the most and says the most, a download
/// (the full content fetched, `AddReferenceTo*` or the full retrieve) shows intent, a like is one
/// press. The same weights order the curated content ([`POPULARITY_SQL`]) and the stars
/// ([`popularity_rating`]), so the two never disagree.
pub fn popularity_score(likes: i32, uses: i32, downloads: i32) -> i64 {
    let n = |v: i32| i64::from(v.max(0));
    3 * n(uses) + 2 * n(downloads) + n(likes)
}

/// [`popularity_score`] in SQL over `storefront_file f`.
pub const POPULARITY_SQL: &str = "(3 * f.uses + 2 * f.downloads + f.likes)";

/// The score each star of `PopularityRating` needs (FH2-S17): one star from the first mark by
/// anyone else, then about ×2.5 per star. A player who downloads, uses and likes a file adds 6,
/// so the steps are about 1, 3, 8 and 20 such players. Five stars (20 players) is a file taken
/// broadly in a community the size of the platform's admission capacity (50 at once), not one a
/// few friends passed around. A download is weighted below a use because it now counts on every
/// full retrieve (a look at the content), not only on an explicit reference.
pub const POPULARITY_STEPS: [i64; 5] = [1, 6, 18, 48, 120];

/// `PopularityRating`, 0 to 5 stars: one per step of [`POPULARITY_STEPS`] the score reached.
pub fn popularity_rating(likes: i32, uses: i32, downloads: i32) -> i64 {
    let score = popularity_score(likes, uses, downloads);
    POPULARITY_STEPS.iter().filter(|t| score >= **t).count() as i64
}

/// The files the viewer may see that match `q`.
pub async fn files_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    q: &Query<'_>,
) -> Result<Vec<File>, IdentityError> {
    let order = if q.popular {
        format!("(f.profile_id = $3), {POPULARITY_SQL} DESC, f.created_at DESC, f.id")
    } else {
        "f.created_at DESC, f.id DESC".to_owned()
    };
    let sql = format!(
        "SELECT {FILE_COLUMNS} {} \
            AND ($4::text IS NULL OR f.kind = $4) \
            AND ($5::int IS NULL OR f.file_type = $5) \
            AND (NOT $6 OR f.profile_id = $3) AND ($6 OR f.removed_at IS NULL) \
            AND ($7::int[] IS NULL OR f.car_id = ANY($7)) \
            AND ($8::bigint IS NULL OR f.car_id = ANY($16) OR (f.meta->>'ClassId')::bigint = $8) \
            AND ($9::smallint IS NULL OR f.keyword_one = $9) \
            AND ($10::smallint IS NULL OR f.keyword_two = $10) \
            AND ($11::bigint IS NULL OR coalesce((f.meta->>'TrackId')::bigint, 0) IN (0, $11)) \
            AND ($12::smallint IS NULL OR f.layers <= $12) \
            AND ($13::text IS NULL OR f.title ILIKE $13 OR f.description ILIKE $13) \
            AND ($14::text IS NULL OR lower(p.name) LIKE $14) \
          ORDER BY {order} LIMIT $15",
        seen_from()
    );
    let bounded = |s: &str| s.trim().chars().take(MAX_SEARCH_CHARS).collect::<String>();
    let pattern = |s: &str| {
        let s = bounded(s);
        (!s.is_empty()).then(|| like_pattern(&s))
    };
    // A prefix of the lowered name (index `profile_name_prefix`): `like_pattern` without its
    // leading `%`.
    let prefix = |s: &str| {
        let s = bounded(s).to_lowercase();
        (!s.is_empty()).then(|| like_pattern(&s)[1..].to_owned())
    };
    // Audited: `FILE_COLUMNS`, `VISIBLE` and `order` are constants; everything else is bound.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(v.game)
        .bind(v.mode)
        .bind(v.profile_id)
        .bind(q.kind.map(Kind::as_str))
        .bind(q.file_type)
        .bind(q.own)
        .bind(q.cars.as_deref())
        .bind(q.class_id)
        .bind(q.keyword_one)
        .bind(q.keyword_two)
        .bind(q.track_id)
        .bind(q.max_layers)
        .bind(pattern(q.text))
        .bind(prefix(q.gamertag))
        .bind(q.limit.clamp(0, 500))
        .bind(&q.class_cars)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(file_of)
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

/// The files of `ids` the viewer may see, in the order asked (unknown ones left out).
pub async fn files_by_id_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    ids: &[i32],
) -> Result<Vec<File>, IdentityError> {
    let sql = format!(
        "SELECT {FILE_COLUMNS} {} AND f.id = ANY($4) AND (f.profile_id = $3 OR f.removed_at IS NULL)",
        seen_from()
    );
    // Audited: constants only; everything else is bound.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(v.game)
        .bind(v.mode)
        .bind(v.profile_id)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    let found: Vec<File> = rows.iter().map(file_of).collect::<Result<_, _>>()?;
    Ok(ids
        .iter()
        .filter_map(|id| found.iter().find(|f| f.id == *id).cloned())
        .collect())
}

/// The file the viewer may see that shares `kind`'s item `item_id` (`GetFileDataIfShared`).
pub async fn file_of_item_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    kind: Kind,
    item_id: i64,
) -> Result<Option<File>, IdentityError> {
    let column = kind.column();
    let sql = format!(
        "SELECT {FILE_COLUMNS} {} AND f.{column} = $4 AND (f.profile_id = $3 OR f.removed_at IS NULL)",
        seen_from()
    );
    // Audited: constants only; everything else is bound.
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(v.game)
        .bind(v.mode)
        .bind(v.profile_id)
        .bind(item_id)
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(file_of).transpose().map_err(Into::into)
}

/// The previews of `ids` (files the caller already read through [`files_by_id_in`]).
pub async fn previews_in(
    conn: &mut PgConnection,
    ids: &[i32],
) -> Result<Vec<(i32, Vec<u8>)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT id, preview FROM storefront_file WHERE id = ANY($1) AND preview IS NOT NULL",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?)
}

/// An owner's edit of its own file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit<'a> {
    /// `RemoveFile`: the file goes (its item is pruned again like any other).
    Remove,
    /// `ChangeFileTitle`.
    Title(&'a str),
    /// `ChangeFileDescription`.
    Description(&'a str),
}

/// The caller's edit of its own file `id`; anything else changes nothing. Whether it changed.
pub async fn edit_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    id: i32,
    edit: Edit<'_>,
) -> Result<bool, IdentityError> {
    let n = match edit {
        // A file taken off by reports stays until a moderator decides (its strikes with it).
        Edit::Remove => sqlx::query(
            "DELETE FROM storefront_file WHERE id = $1 AND profile_id = $2 AND removed_at IS NULL",
        )
        .bind(id)
        .bind(profile_id)
        .execute(&mut *conn)
        .await?,
        Edit::Title(t) => {
            sqlx::query("UPDATE storefront_file SET title = $3 WHERE id = $1 AND profile_id = $2")
                .bind(id)
                .bind(profile_id)
                .bind(t)
                .execute(&mut *conn)
                .await?
        }
        Edit::Description(d) => {
            sqlx::query(
                "UPDATE storefront_file SET description = $3 WHERE id = $1 AND profile_id = $2",
            )
            .bind(id)
            .bind(profile_id)
            .bind(d)
            .execute(&mut *conn)
            .await?
        }
    }
    .rows_affected();
    Ok(n == 1)
}

/// A viewer's mark on a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// A download: `AddReferenceTo*`, or a full retrieve of the shared item
    /// (`RetrieveVersionedLiveryById` without its thumbnail, `…LayerGroupById`, `…TuneById`).
    Download,
    /// `IncrementLikes`.
    Like,
    /// `IncrementUsage`.
    Use,
    /// `IncrementViews`, `BatchIncrementViews`.
    View,
    /// `ReportContent`: a strike.
    Report,
}

impl Mark {
    fn kind(self) -> &'static str {
        match self {
            Mark::Download => "download",
            Mark::Like => "like",
            Mark::Use => "use",
            Mark::View => "view",
            Mark::Report => "report",
        }
    }

    fn counter(self) -> &'static str {
        match self {
            Mark::Download => "downloads = downloads + 1",
            Mark::Like => "likes = likes + 1",
            Mark::Use => "uses = uses + 1",
            Mark::View => "views = views + 1",
            Mark::Report => "strikes = strikes + 1",
        }
    }
}

/// What a mark did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marked {
    /// Not counted: the viewer's own file, one it may not see, or already marked.
    Ignored,
    /// Counted.
    Counted,
    /// A report that took the file off (the [`STRIKES_TO_REMOVE`]th).
    Removed,
}

/// Counts the viewer's `mark` on file `id`, once per account: never on a file of the viewer's own
/// account, one it may not see or one taken off.
pub async fn mark_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    id: i32,
    mark: Mark,
) -> Result<Marked, IdentityError> {
    let sql = format!(
        "SELECT f.id {} AND f.id = $4 AND p.account_id <> $5 AND f.removed_at IS NULL",
        seen_from()
    );
    // Audited: constants only; everything else is bound.
    let visible: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(v.game)
        .bind(v.mode)
        .bind(v.profile_id)
        .bind(id)
        .bind(v.account_id)
        .fetch_optional(&mut *conn)
        .await?;
    if visible.is_none() {
        return Ok(Marked::Ignored);
    }
    let new = sqlx::query(
        "INSERT INTO storefront_mark (file_id, kind, viewer) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(mark.kind())
    .bind(account_key(v.account_id))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if new == 0 {
        return Ok(Marked::Ignored);
    }
    // Audited: `counter()` is a constant.
    let strikes: i16 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE storefront_file SET {} WHERE id = $1 RETURNING strikes",
        mark.counter()
    )))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if mark == Mark::Report && strikes >= STRIKES_TO_REMOVE {
        let n = sqlx::query(
            "UPDATE storefront_file SET removed_at = clock_timestamp() WHERE id = $1 AND removed_at IS NULL",
        )
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if n == 1 {
            return Ok(Marked::Removed);
        }
    }
    Ok(Marked::Counted)
}

/// A download of the file sharing `kind`'s item `item_id`, when there is one the viewer may see
/// (not its own account's): `AddReferenceTo*` and the full retrieves of the item (FH2-S17). Both
/// write the same per-account mark, so a player counts once whatever it called.
pub async fn referenced_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    kind: Kind,
    item_id: i64,
) -> Result<Marked, IdentityError> {
    match file_of_item_in(conn, v, kind, item_id).await? {
        // `mark_in` refuses the viewer's own account's files.
        Some(f) if !f.removed => mark_in(conn, v, f.id, Mark::Download).await,
        _ => Ok(Marked::Ignored),
    }
}

/// Follows (or stops following) the creator `xuid` of the viewer's game, of either mode (OD-141,
/// CR-S75: the Storefront is shared by both modes; an unknown or inactive profile, or oneself,
/// changes nothing).
pub async fn follow_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    xuid: i64,
    on: bool,
) -> Result<(), IdentityError> {
    let creator: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM profile \
          WHERE game = $1 AND xuid = $2 AND status = 'active' AND id <> $3",
    )
    .bind(v.game)
    .bind(xuid)
    .bind(v.profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(creator) = creator else {
        return Ok(());
    };
    let sql = if on {
        "INSERT INTO storefront_follow (follower, creator_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
    } else {
        "DELETE FROM storefront_follow WHERE follower = $1 AND creator_id = $2"
    };
    sqlx::query(sql)
        .bind(viewer_key(v.profile_id))
        .bind(creator)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The creators among `creators` the viewer follows.
pub async fn followed_in(
    conn: &mut PgConnection,
    v: Viewer<'_>,
    creators: &[Uuid],
) -> Result<Vec<Uuid>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT creator_id FROM storefront_follow WHERE follower = $1 AND creator_id = ANY($2)",
    )
    .bind(viewer_key(v.profile_id))
    .bind(creators)
    .fetch_all(&mut *conn)
    .await?)
}

/// The creators `ids` as the game plane shows them (name, XUID, card), in one statement.
pub async fn creators_in(
    conn: &mut PgConnection,
    game: &str,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, crate::gameplane::Player)>, IdentityError> {
    let rows = sqlx::query(
        "SELECT p.id, p.xuid, p.name, l.career_level, l.badge_id, l.player_title_id, l.tables_tran_id \
           FROM profile p LEFT JOIN game_logon l ON l.game = $1 AND l.profile_id = p.id \
          WHERE p.id = ANY($2)",
    )
    .bind(game)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            let level: Option<i64> = r.try_get("career_level")?;
            let badge: Option<i32> = r.try_get("badge_id")?;
            let title: Option<i32> = r.try_get("player_title_id")?;
            Ok((
                r.try_get("id")?,
                crate::gameplane::Player {
                    xuid: r.try_get("xuid")?,
                    name: r.try_get("name")?,
                    card: crate::gameplane::Card {
                        career_level: level.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
                        badge_id: badge.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
                        player_title_id: title.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
                    },
                    tables_tran_id: r.try_get("tables_tran_id")?,
                },
            ))
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// What [`erase_profile_in`] deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfileErased {
    /// `storefront_file` rows (its own; a shared livery's go with the livery too).
    pub files: u64,
    /// `storefront_mark` rows of its account (only once the account itself is erased).
    pub marks: u64,
    /// `storefront_follow` rows it made or that named it.
    pub follows: u64,
    /// `game_layer_group` rows.
    pub groups: u64,
    /// `game_tune` rows.
    pub tunes: u64,
    /// `game_layer_group_draft` rows.
    pub group_drafts: u64,
    /// `game_car_setup` rows.
    pub setups: u64,
}

/// Erasure (ACCOUNTS.md §8): every Storefront row of the profile, the follows it made and of it,
/// and its account's marks once the account itself is erased (while the account lives they stay,
/// so a deleted and recreated profile cannot mark or report again).
pub async fn erase_profile_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<ProfileErased, sqlx::Error> {
    let key = viewer_key(profile);
    let account: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT a.id, a.status FROM profile p JOIN account a ON a.id = p.account_id WHERE p.id = $1",
    )
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    let mut out = ProfileErased::default();
    if let Some((account_id, status)) = &account
        && status == "deleted"
    {
        out.marks = sqlx::query("DELETE FROM storefront_mark WHERE viewer = $1")
            .bind(account_key(*account_id))
            .execute(&mut *conn)
            .await?
            .rows_affected();
    }
    let mut del = async |sql: &'static str, by_key: bool| -> Result<u64, sqlx::Error> {
        let q = sqlx::query(sql);
        let q = if by_key {
            q.bind(key.clone())
        } else {
            q.bind(profile)
        };
        Ok(q.execute(&mut *conn).await?.rows_affected())
    };
    out.files = del("DELETE FROM storefront_file WHERE profile_id = $1", false).await?;
    out.follows = del("DELETE FROM storefront_follow WHERE follower = $1", true).await?
        + del("DELETE FROM storefront_follow WHERE creator_id = $1", false).await?;
    out.groups = del("DELETE FROM game_layer_group WHERE profile_id = $1", false).await?;
    out.tunes = del("DELETE FROM game_tune WHERE profile_id = $1", false).await?;
    out.group_drafts = del(
        "DELETE FROM game_layer_group_draft WHERE profile_id = $1",
        false,
    )
    .await?;
    out.setups = del("DELETE FROM game_car_setup WHERE profile_id = $1", false).await?;
    Ok(out)
}

// ---- Public previews (CR-S77, OD-137; ACCOUNTS.md §5a) -----------------------------------------

/// The joins and conditions of a livery file `f` whose preview the public may read: not taken off,
/// its livery and the livery's bytes not hidden, its owner visible ([`VISIBLE`]), and a preview of 1
/// to [`MAX_PREVIEW`] bytes that starts with the PNG signature.
const PUBLIC_PREVIEW: &str = "FROM storefront_file f JOIN profile p ON p.id = f.profile_id \
       JOIN account a ON a.id = p.account_id \
       JOIN game_livery l ON l.id = f.livery_id \
       LEFT JOIN livery_blob b ON b.profile_id = l.profile_id AND b.sha256 = l.blob_sha256 \
      WHERE f.kind = 'livery' AND f.removed_at IS NULL \
        AND l.hidden_at IS NULL AND b.hidden_at IS NULL \
        AND f.preview IS NOT NULL AND octet_length(f.preview) BETWEEN 9 AND 409600 \
        AND substring(f.preview FROM 1 FOR 8) = '\\x89504e470d0a1a0a'::bytea";

/// The PNG signature every served preview starts with.
pub const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// The public preview of the file named by `token` (`GET /media/v1/previews/{token}.png`), when it
/// may be served now; `None` for anything else (one answer).
pub async fn public_preview_in(
    conn: &mut PgConnection,
    token: &[u8; 16],
) -> Result<Option<Vec<u8>>, IdentityError> {
    let sql = format!("SELECT f.preview {PUBLIC_PREVIEW} AND f.preview_token = $1 AND {VISIBLE}");
    // Audited: `PUBLIC_PREVIEW` and `VISIBLE` are constants; the token is bound.
    let bytes: Option<Vec<u8>> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(&token[..])
        .fetch_optional(&mut *conn)
        .await?;
    Ok(bytes.filter(|b| b.len() <= MAX_PREVIEW && b.starts_with(&PNG_SIGNATURE)))
}

/// The preview token of the newest livery file of `car_id` shared by the account's own profile
/// `xuid`, that [`public_preview_in`] would serve now (`ProfilePresence.liveryImage`, ACCOUNTS.md
/// §5 "Presence"); `None` without one. Another account's profile never answers.
pub async fn own_preview_token_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    car_id: i32,
    game: &str,
) -> Result<Option<[u8; 16]>, IdentityError> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT f.preview_token {PUBLIC_PREVIEW} AND p.account_id = $1 AND p.xuid = $2 \
           AND f.car_id = $3 AND p.game = $4 AND {VISIBLE} ORDER BY f.created_at DESC, f.id DESC LIMIT 1"
    );
    // Audited: `PUBLIC_PREVIEW` and `VISIBLE` are constants; everything else is bound.
    let token: Option<Vec<u8>> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .bind(xuid)
        .bind(car_id)
        .bind(game)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(token.and_then(|t| <[u8; 16]>::try_from(t.as_slice()).ok()))
}

/// The preview token of file `id` (tests and operators; never on a game or launcher wire).
pub async fn preview_token_in(
    conn: &mut PgConnection,
    id: i32,
) -> Result<Option<[u8; 16]>, IdentityError> {
    let token: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT preview_token FROM storefront_file WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(token.and_then(|t| <[u8; 16]>::try_from(t.as_slice()).ok()))
}

/// A token in its wire form: 32 lowercase hex digits.
pub fn token_hex(token: &[u8; 16]) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses a token's wire form (exactly 32 lowercase hex digits).
pub fn parse_token(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 32
        || !bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in bytes.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Public preview reads per token (CR-S77): 300 an hour.
pub const PREVIEW_READS: crate::limits::Limit = crate::limits::Limit {
    name: "media.preview",
    max: 300,
    window_s: 3600,
};

/// The `rate_limit` key of a token under [`PREVIEW_READS`]: a SHA-256 of it (never the token).
pub fn preview_key(token: &[u8; 16]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(b"preview:");
    h.update(token);
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{hex}", PREVIEW_READS.name)
}

/// The `ETag` of a served preview: a quoted prefix of its SHA-256.
pub fn preview_etag(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

#[cfg(test)]
mod popularity_tests {
    use super::*;

    #[test]
    fn stars_step_on_likes_uses_and_downloads() {
        assert_eq!(popularity_rating(0, 0, 0), 0);
        // One like, one download or one use: the first star.
        assert_eq!(popularity_rating(1, 0, 0), 1);
        assert_eq!(popularity_rating(0, 0, 1), 1);
        assert_eq!(popularity_rating(0, 1, 0), 1);
        // One player who downloaded, used and liked: 6, the second star.
        assert_eq!(popularity_score(1, 1, 1), 6);
        assert_eq!(popularity_rating(1, 1, 1), 2);
        assert_eq!(popularity_rating(0, 0, 2), 1, "two downloads alone: 4");
        // Each step exactly, and one below it.
        for (stars, step) in POPULARITY_STEPS.iter().enumerate() {
            let at = i32::try_from(*step).unwrap();
            assert_eq!(popularity_rating(at, 0, 0), stars as i64 + 1);
            assert_eq!(popularity_rating(at - 1, 0, 0), stars as i64);
        }
        // 20 players who did all three: 120, five stars; never above five.
        assert_eq!(popularity_rating(20, 20, 20), 5);
        assert_eq!(popularity_rating(i32::MAX, i32::MAX, i32::MAX), 5);
        // A corrupt negative counter counts as none.
        assert_eq!(popularity_rating(-5, -5, -5), 0);
    }

    #[test]
    fn the_sql_score_is_the_rust_one() {
        assert_eq!(POPULARITY_SQL, "(3 * f.uses + 2 * f.downloads + f.likes)");
        assert_eq!(popularity_score(1, 0, 0), 1);
        assert_eq!(popularity_score(0, 1, 0), 3);
        assert_eq!(popularity_score(0, 0, 1), 2);
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_other_forms() {
        let t = [0xabu8; 16];
        assert_eq!(parse_token(&token_hex(&t)), Some(t));
        for bad in [
            "",
            "ABABABABABABABABABABABABABABABAB",
            "abab",
            "zz".repeat(16).as_str(),
        ] {
            assert_eq!(parse_token(bad), None, "{bad}");
        }
        assert_ne!(preview_key(&t), preview_key(&[0u8; 16]));
        assert!(!preview_key(&t).contains(&token_hex(&t)));
        let e = preview_etag(b"one");
        assert!(e.starts_with('"') && e.ends_with('"') && e.len() == 34);
        assert_ne!(e, preview_etag(b"two"));
    }
}

// ---- The play dashboard's shared creations (DASHBOARD_API.md §7, PD-5) -------------------------

/// A shared file as the play dashboard shows it: published, on, its item not hidden and its owner
/// website-listed. The storefront id stays server-side (the dashboard sends an opaque id).
#[derive(Clone, Debug, PartialEq)]
pub struct SharedFile {
    /// `storefront_file.id` (server-side only).
    pub id: i32,
    /// The owner's profile (server-side only).
    pub profile_id: Uuid,
    /// The owner's mode (the creation's mode: a file has none of its own).
    pub mode: String,
    /// The owner's plain name.
    pub name: String,
    /// `livery` | `group` | `tune`.
    pub kind: String,
    /// The file's car (0 or less: none).
    pub car_id: i32,
    /// The title.
    pub title: String,
    /// The description.
    pub description: String,
    /// Counters.
    pub downloads: i32,
    /// Likes.
    pub likes: i32,
    /// Uses.
    pub uses: i32,
    /// When shared.
    pub created_at: UnixMs,
    /// The preview's first 8 bytes (its format), when it has one.
    pub preview_head: Option<Vec<u8>>,
}

/// The conditions of a file the play dashboard shows, over [`seen_from`] read as no one (`$3`
/// the nil profile: never "its own"): on, its owner visible and its item not hidden, plus the owner
/// listed on the website (`crate::dashboard::players::listed`).
fn shared_from() -> String {
    format!(
        "{} AND {}",
        seen_from(),
        crate::dashboard::players::listed()
    )
}

/// Every shared file of `game` whose owner is of `mode` (or of every mode when `None`), newest
/// first. One statement; a privately saved livery (no storefront row) never appears.
pub async fn shared_files_in(
    conn: &mut PgConnection,
    game: &str,
    mode: Option<&str>,
) -> Result<Vec<SharedFile>, IdentityError> {
    let sql = format!(
        "SELECT f.id, f.profile_id, p.mode, p.name, f.kind, f.car_id, f.title, f.description, f.downloads, \
           f.likes, f.uses, f.created_at, substring(f.preview FROM 1 FOR 8) AS preview_head \
         {} AND ($2::text IS NULL OR p.mode = $2) \
         ORDER BY f.created_at DESC, f.id DESC",
        shared_from()
    );
    // Audited: `seen_from`, `VISIBLE` and `listed` are constants; everything else is bound.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(mode)
        .bind(Uuid::nil())
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(SharedFile {
                id: r.try_get("id")?,
                profile_id: r.try_get("profile_id")?,
                mode: r.try_get("mode")?,
                name: r.try_get::<Option<String>, _>("name")?.unwrap_or_default(),
                kind: r.try_get("kind")?,
                car_id: r.try_get("car_id")?,
                title: r.try_get("title")?,
                description: r.try_get("description")?,
                downloads: r.try_get("downloads")?,
                likes: r.try_get("likes")?,
                uses: r.try_get("uses")?,
                created_at: ms(r.try_get::<OffsetDateTime, _>("created_at")?),
                preview_head: r.try_get("preview_head")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// The ids of every file [`shared_files_in`] shows in `game` (any mode): the play dashboard's live
/// allowlist of creations (`GET /listed`, sent as opaque ids). One statement.
pub async fn shared_file_ids_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Vec<i32>, IdentityError> {
    let sql = format!("SELECT f.id {} AND $2::text IS NULL", shared_from());
    // Audited: constants only; the values are bound.
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(None::<String>)
        .bind(Uuid::nil())
        .fetch_all(&mut *conn)
        .await?)
}

/// The preview bytes of shared file `id` in `game`, when [`shared_files_in`] shows it and it has
/// one; `None` for anything else.
pub async fn shared_preview_in(
    conn: &mut PgConnection,
    game: &str,
    id: i32,
) -> Result<Option<Vec<u8>>, IdentityError> {
    let sql = format!(
        "SELECT f.preview {} AND $2::text IS NULL AND f.id = $4 AND f.preview IS NOT NULL",
        shared_from()
    );
    // Audited: constants only; the values are bound.
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(None::<String>)
        .bind(Uuid::nil())
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?)
}
