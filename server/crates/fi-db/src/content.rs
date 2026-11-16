//! OD-79 startup checks against the small private original-file dictionary.
//! No inventory/catalogue/world model, client attribution or sanction writer.
use fi_protocol::content::*;
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;
/// Safe bounded refusals. Never print inner SQL errors or player bodies.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Unavailable storage.
    #[error("content storage unavailable")]
    Db(#[from] sqlx::Error),
    /// Invalid fields or unknown file id.
    #[error("invalid content observation")]
    Invalid,
    /// Changed id/key/exact bytes.
    #[error("content idempotency conflict")]
    Conflict,
    /// Missing or foreign owned context, hide existence.
    #[error("owned content context not found")]
    NotFound,
    /// Current verified identity required.
    #[error("verified identity required")]
    Unverified,
    /// Shared request/fresh-scan quota.
    #[error("content quota exceeded")]
    Rate(u64),
    /// Bounded retained storage full.
    #[error("content capacity exhausted")]
    Capacity,
    /// Non-punitive online permission refusal.
    #[error("content check required")]
    Denied(ContentReason),
}
fn id(s: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(s)
        .ok()
        .filter(|v| !v.is_nil())
        .ok_or(Error::Invalid)
}
async fn now(conn: &mut PgConnection) -> Result<i64, Error> {
    let t: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(conn)
        .await?;
    Ok((t.unix_timestamp_nanos() / 1_000_000) as i64)
}
/// Hold before admission/session locks, until the caller's transaction commits.
pub async fn read_lock_in(conn: &mut PgConnection) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(753182501)")
        .execute(conn)
        .await?;
    Ok(())
}
async fn write_lock(conn: &mut PgConnection) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(753182501)")
        .execute(conn)
        .await?;
    Ok(())
}
/// Bearer-derived verified account and live launcher device; never trust body account/device.
pub async fn device_in(conn: &mut PgConnection, owner: Uuid, family: Uuid) -> Result<Uuid, Error> {
    let verified: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM account_identity WHERE account_id=$1 AND email_verified)",
    )
    .bind(owner)
    .fetch_one(&mut *conn)
    .await?;
    if !verified {
        return Err(Error::Unverified);
    }
    sqlx::query_scalar("SELECT r.device_binding_id FROM refresh_token r JOIN device_binding d ON d.id=r.device_binding_id WHERE r.family_id=$1 AND r.account_id=$2 AND r.client='launcher' AND r.revoked_at IS NULL AND r.expires_at>clock_timestamp() AND d.account_id=$2 AND d.unbound_at IS NULL LIMIT 1").bind(family).bind(owner).fetch_optional(conn).await?.ok_or(Error::NotFound)
}
/// Exact shared authenticated attempt quotas, malformed/retried requests included.
pub async fn request_limit(pool: &PgPool, owner: Uuid, class: &str) -> Result<(), Error> {
    let (max, window_s) = match class {
        "permission" => (30, 60),
        "scan" => (120, 3600),
        _ => (60, 60),
    };
    let limit = crate::limits::Limit {
        name: "content.attempt",
        max,
        window_s,
    };
    let key = format!(
        "content.attempt:{}",
        fi_content::digest(format!("{owner}:{class}").as_bytes())
    );
    match crate::limits::hit(pool, &key, &limit)
        .await
        .map_err(|_| Error::Capacity)?
    {
        crate::limits::Hit::Allowed => Ok(()),
        crate::limits::Hit::Limited { retry_after_secs } => {
            Err(Error::Rate(retry_after_secs.max(1) as u64))
        }
    }
}
async fn known_game(conn: &mut PgConnection, game: &str) -> Result<(), Error> {
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM game WHERE code=$1)")
        .bind(game)
        .fetch_one(conn)
        .await?
    {
        Ok(())
    } else {
        Err(Error::Invalid)
    }
}
/// Return ONLY the short named-file list, not expected digests or directory scopes.
pub async fn capabilities_in(
    conn: &mut PgConnection,
    game: &str,
    required: bool,
) -> Result<ContentCapabilities, Error> {
    known_game(conn, game).await?;
    Ok(ContentCapabilities {
        schema_version: 1,
        game: game.into(),
        enforcement: if required {
            ContentEnforcement::Required
        } else {
            ContentEnforcement::LocalTestBypass
        },
        manifest: fi_content::reference_dictionary(game).map(|d| d.manifest),
        max_files: fi_content::MAX_FILES as u8,
        max_scan_bytes: fi_content::SCAN_MAX as u32,
    })
}
async fn context(
    conn: &mut PgConnection,
    owner: Uuid,
    device: Uuid,
    session: Uuid,
    game: Option<&str>,
) -> Result<(), Error> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM device_binding WHERE id=$1 AND account_id=$2 AND unbound_at IS NULL)").bind(device).bind(owner).fetch_one(&mut *conn).await?;
    if !valid {
        return Err(Error::NotFound);
    }
    let foreign:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM play_session WHERE id=$1 AND (account_id<>$2 OR device_binding_id<>$3 OR ($4::text IS NOT NULL AND game<>$4))) OR EXISTS(SELECT 1 FROM content_session_binding WHERE session_id=$1 AND (owner<>$2 OR device<>$3 OR ($4::text IS NOT NULL AND game<>$4)))").bind(session).bind(owner).bind(device).bind(game).fetch_one(conn).await?;
    if foreign {
        Err(Error::NotFound)
    } else {
        Ok(())
    }
}
/// Current own-session permission, including a future scan-bound session. Foreign is404.
pub async fn permission_in(
    conn: &mut PgConnection,
    owner: Uuid,
    device: Uuid,
    session: Uuid,
) -> Result<ContentPermission, Error> {
    context(conn, owner, device, session, None).await?;
    let row=sqlx::query("SELECT a.id,a.game,a.body,a.created_at,a.expires_at>clock_timestamp() AS retained FROM content_session_binding b JOIN content_scan a ON a.id=b.scan_id WHERE b.session_id=$1 AND b.owner=$2 AND b.device=$3").bind(session).bind(owner).bind(device).fetch_optional(&mut *conn).await?;
    let Some(row) = row else {
        let owned:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM play_session WHERE id=$1 AND account_id=$2 AND device_binding_id=$3)").bind(session).bind(owner).bind(device).fetch_one(conn).await?;
        return if owned {
            Ok(fi_content::denied(
                &session.to_string(),
                None,
                ContentReason::MissingScan,
            ))
        } else {
            Err(Error::NotFound)
        };
    };
    let scan: ContentScan =
        serde_json::from_value(row.try_get("body")?).map_err(|_| Error::Capacity)?;
    if !row.try_get::<bool, _>("retained")? {
        return Ok(fi_content::denied(
            &scan.session_id,
            Some(&scan.scan_id),
            ContentReason::ScanExpired,
        ));
    }
    let Some(d) = fi_content::reference_dictionary(&row.try_get::<String, _>("game")?) else {
        return Ok(fi_content::denied(
            &scan.session_id,
            Some(&scan.scan_id),
            ContentReason::PolicyUnavailable,
        ));
    };
    let created: OffsetDateTime = row.try_get("created_at")?;
    fi_content::permission(
        &d,
        &scan,
        (created.unix_timestamp_nanos() / 1_000_000) as i64,
        now(conn).await?,
    )
    .map_err(|_| Error::Invalid)
}
/// Guard ordinary online continuation; never modifies account/session/save state.
pub async fn require_in(
    conn: &mut PgConnection,
    owner: Uuid,
    device: Uuid,
    session: Uuid,
) -> Result<ContentPermission, Error> {
    let p = permission_in(conn, owner, device, session).await?;
    if p.online_allowed {
        Ok(p)
    } else {
        Err(Error::Denied(p.reason))
    }
}
/// Whether `session` of `owner` is bound to a content scan (OD-113, CONTENT_API.md §3; CR-S51):
/// only a Hardcore session with a save line is (`NOT dedicated AND NOT modded AND save_line AND
/// mode = 'hardcore'`; every Hardcore line since OD-239), never a
/// dedicated host (OD-109) or a modded session (never Hardcore, OD-112 follow-up). `None` for an unknown or another account's session (the ordinary checks answer).
pub async fn session_bound_in(
    conn: &mut PgConnection,
    owner: Uuid,
    session: Uuid,
) -> Result<Option<bool>, Error> {
    Ok(sqlx::query_scalar(
        "SELECT NOT s.dedicated AND NOT s.modded AND s.save_line AND p.mode = 'hardcore' \
         FROM play_session s JOIN profile p ON p.id = s.profile_id \
         WHERE s.id = $1 AND s.account_id = $2",
    )
    .bind(session)
    .bind(owner)
    .fetch_optional(conn)
    .await?)
}
/// Whether a new ordinary admission of `owner`'s profile `xuid` of `game` must carry a content
/// scan (OD-113, CR-S51): the profile is `hardcore` and would be admitted with a save line (its
/// OD-239: any Hardcore profile of a game with a line, whatever the tier). A dedicated or modded admission never is (the caller). `true` for an
/// unknown profile, so the ordinary checks answer.
pub async fn create_bound_in(
    conn: &mut PgConnection,
    owner: Uuid,
    game: &str,
    xuid: &str,
) -> Result<bool, Error> {
    let Ok(xuid) = xuid.parse::<i64>() else {
        return Ok(true);
    };
    let mode: Option<String> = sqlx::query_scalar(
        "SELECT mode FROM profile WHERE account_id = $1 AND game = $2 AND xuid = $3 \
         ORDER BY (status = 'deleted'), created_at DESC LIMIT 1",
    )
    .bind(owner)
    .bind(game)
    .bind(xuid)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(mode) = mode else { return Ok(true) };
    // OD-239: every Hardcore profile keeps a line, whatever the tier.
    Ok(mode == "hardcore" && fi_domain::GameRules::of(game).line_mode(&mode))
}
/// Return the verified original car database digest for normal game ticket comparison.
pub async fn require_session_in(conn: &mut PgConnection, session: Uuid) -> Result<String, Error> {
    // Hot game requests used to perform six sequential reads here. One statement observes the
    // same ownership, live-device, current binding and scan policy without caching permission.
    let row = sqlx::query(
        "SELECT s.game, s.account_id, s.device_binding_id, \
         EXISTS(SELECT 1 FROM device_binding d WHERE d.id=s.device_binding_id \
           AND d.account_id=s.account_id AND d.unbound_at IS NULL) AS valid_device, \
         b.owner AS binding_owner, b.device AS binding_device, a.body, a.game AS scan_game, \
         a.created_at, a.expires_at > clock_timestamp() AS retained, clock_timestamp() AS now \
         FROM play_session s LEFT JOIN content_session_binding b ON b.session_id=s.id \
         LEFT JOIN content_scan a ON a.id=b.scan_id WHERE s.id=$1",
    )
    .bind(session)
    .fetch_optional(conn)
    .await?
    .ok_or(Error::NotFound)?;
    if !row.try_get::<bool, _>("valid_device")? {
        return Err(Error::NotFound);
    }
    if let Some(owner) = row.try_get::<Option<Uuid>, _>("binding_owner")? {
        if owner != row.try_get::<Uuid, _>("account_id")?
            || row.try_get::<Option<Uuid>, _>("binding_device")?
                != row.try_get::<Option<Uuid>, _>("device_binding_id")?
        {
            return Err(Error::NotFound);
        }
    } else {
        return Err(Error::Denied(ContentReason::MissingScan));
    }
    let body: serde_json::Value = row.try_get("body")?;
    let scan: ContentScan = serde_json::from_value(body).map_err(|_| Error::Capacity)?;
    if !row.try_get::<bool, _>("retained")? {
        return Err(Error::Denied(ContentReason::ScanExpired));
    }
    let dictionary = fi_content::reference_dictionary(&row.try_get::<String, _>("scan_game")?)
        .ok_or(Error::Denied(ContentReason::PolicyUnavailable))?;
    let created: OffsetDateTime = row.try_get("created_at")?;
    let observed: OffsetDateTime = row.try_get("now")?;
    let permission = fi_content::permission(
        &dictionary,
        &scan,
        (created.unix_timestamp_nanos() / 1_000_000) as i64,
        (observed.unix_timestamp_nanos() / 1_000_000) as i64,
    )
    .map_err(|_| Error::Invalid)?;
    if !permission.online_allowed {
        return Err(Error::Denied(permission.reason));
    }
    let game: String = row.try_get("game")?;
    fi_content::reference_dictionary(&game)
        .and_then(|d| d.expected.get("car_database").cloned())
        .ok_or(Error::Denied(ContentReason::PolicyUnavailable))
}
/// Admission checks the startup result and real nonempty carDbSha256. Hook origin remains OD-76.
pub async fn require_create_in(
    conn: &mut PgConnection,
    owner: Uuid,
    device: Uuid,
    req: &fi_protocol::saves::PlaySessionCreate,
) -> Result<(), Error> {
    let session = id(&req.session_id)?;
    context(conn, owner, device, session, Some(&req.game)).await?;
    let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM content_session_binding WHERE session_id=$1) OR EXISTS(SELECT 1 FROM play_session WHERE id=$1)").bind(session).fetch_one(&mut *conn).await?;
    if !exists {
        return Err(Error::Denied(ContentReason::MissingScan));
    }
    require_in(conn, owner, device, session).await?;
    let d = fi_content::reference_dictionary(&req.game)
        .ok_or(Error::Denied(ContentReason::PolicyUnavailable))?;
    if !d
        .expected
        .get("car_database")
        .is_some_and(|expected| req.car_db_sha256.eq_ignore_ascii_case(expected))
    {
        return Err(Error::Denied(ContentReason::IncompatibleFiles));
    }
    Ok(())
}
/// Accept exact bytes once; a delayed older action cannot rewind the latest own-session scan.
pub async fn scan_in(
    conn: &mut PgConnection,
    owner: Uuid,
    device: Uuid,
    key: Uuid,
    scan: &ContentScan,
    raw: &[u8],
) -> Result<ContentScanAccepted, Error> {
    fi_content::validate_scan(scan).map_err(|_| Error::Invalid)?;
    if raw.len() > fi_content::SCAN_MAX
        || id(&scan.scan_id)? != key
        || serde_json::from_slice::<ContentScan>(raw).ok().as_ref() != Some(scan)
    {
        return Err(Error::Invalid);
    }
    write_lock(conn).await?;
    known_game(conn, &scan.game).await?;
    let session = id(&scan.session_id)?;
    context(conn, owner, device, session, Some(&scan.game)).await?;
    let hash = fi_content::digest(raw);
    let old=sqlx::query("SELECT owner,device,request_hash,expires_at>clock_timestamp() AS retained FROM content_scan WHERE id=$1").bind(key).fetch_optional(&mut *conn).await?;
    if let Some(old) = old {
        if old.try_get::<Uuid, _>("owner")? != owner
            || old.try_get::<Uuid, _>("device")? != device
            || old.try_get::<String, _>("request_hash")? != hash
        {
            return Err(Error::Conflict);
        }
        if !old.try_get::<bool, _>("retained")? {
            return Err(Error::Denied(ContentReason::ScanExpired));
        }
    } else {
        let time = now(conn).await?;
        fi_content::fresh_action(key, time)
            .map_err(|_| Error::Denied(ContentReason::ScanExpired))?;
        if let Some(d) = fi_content::reference_dictionary(&scan.game) {
            fi_content::permission(&d, scan, time, time).map_err(|_| Error::Invalid)?;
        }
        let (hour,day):(i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE created_at>clock_timestamp()-interval '1 hour'),count(*) FROM content_scan WHERE owner=$1 AND created_at>clock_timestamp()-interval '24 hours'").bind(owner).fetch_one(&mut *conn).await?;
        if hour >= 60 || day >= 1200 {
            return Err(Error::Rate(if hour >= 60 { 3600 } else { 86400 }));
        }
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM content_scan")
            .fetch_one(&mut *conn)
            .await?;
        if total >= 100000 {
            return Err(Error::Capacity);
        }
        sqlx::query("INSERT INTO content_scan(id,owner,device,game,session_id,request_hash,body) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(key).bind(owner).bind(device).bind(&scan.game).bind(session).bind(hash).bind(serde_json::to_value(scan).map_err(|_|Error::Invalid)?).execute(&mut *conn).await?;
        sqlx::query("INSERT INTO content_session_binding(session_id,owner,device,game,scan_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT(session_id) DO UPDATE SET scan_id=excluded.scan_id WHERE content_session_binding.scan_id<excluded.scan_id").bind(session).bind(owner).bind(device).bind(&scan.game).bind(key).execute(&mut *conn).await?;
    }
    Ok(ContentScanAccepted {
        scan_id: key.to_string(),
        permission: permission_in(conn, owner, device, session).await?,
    })
}
/// Private bounded retention job; no eviction of live scans to make space.
pub async fn cleanup_in(conn: &mut PgConnection) -> Result<u64, Error> {
    write_lock(conn).await?;
    Ok(sqlx::query("DELETE FROM content_scan WHERE id IN (SELECT id FROM content_scan WHERE expires_at<=clock_timestamp() ORDER BY expires_at,id LIMIT 100)").execute(conn).await?.rows_affected())
}
