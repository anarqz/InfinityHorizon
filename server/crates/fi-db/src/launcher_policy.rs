//! Shared authoritative launcher version checks; uses the caller's one connection.
//! No network, mutable process state, session/lease mutation or client binary attestation.
use fi_domain::version;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// Why launcher compatibility could not be established.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Current effective minimum; no token or raw client input.
    #[error("launcher update required")]
    UpdateRequired(String),
    /// Invalid persisted/deployment policy must fail closed.
    #[error("launcher policy unavailable")]
    PolicyUnavailable,
    /// Missing or foreign owned session.
    #[error("no such session")]
    NotFound,
    /// Existing create id belongs to another account/profile/game.
    #[error("session id reused")]
    KeyReused,
    /// No configured game.
    #[error("unknown game")]
    UnknownGame,
    /// Storage failure (never log submitted payloads).
    #[error("launcher policy storage failure")]
    Db(#[from] sqlx::Error),
}

fn require(version: &str, global: &str, game: Option<&str>) -> Result<(), Error> {
    let floor = version::effective_min(global, game).map_err(|_| Error::PolicyUnavailable)?;
    if version::meets_min(version, floor).map_err(|_| Error::PolicyUnavailable)? {
        Ok(())
    } else {
        Err(Error::UpdateRequired(floor.to_owned()))
    }
}

/// New admission or same-id replay. A new body version cannot upgrade stored session metadata.
pub async fn require_create_in(
    conn: &mut PgConnection,
    owner: Uuid,
    profile: Uuid,
    session: Uuid,
    game: &str,
    proposed: &str,
    global: &str,
) -> Result<(), Error> {
    // Both canonical and alias admission take this transaction lock BEFORE pool/head locks.
    // Concurrent same-id creates cannot pass on a fresh version and replay a stored old one.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('launcher-version-session:' || $1::text, 0))")
        .bind(session).execute(&mut *conn).await?;
    let existing = sqlx::query(
        "SELECT account_id, profile_id, game, launcher_version FROM play_session WHERE id=$1",
    )
    .bind(session)
    .fetch_optional(&mut *conn)
    .await?;
    let version = if let Some(ref row) = existing {
        if row.try_get::<Uuid, _>("account_id")? != owner
            || row.try_get::<Uuid, _>("profile_id")? != profile
            || row.try_get::<String, _>("game")? != game
        {
            return Err(Error::KeyReused);
        }
        row.try_get::<String, _>("launcher_version")?
    } else {
        proposed.to_owned()
    };
    let floor: Option<Option<String>> =
        sqlx::query_scalar("SELECT min_launcher FROM game WHERE code=$1")
            .bind(game)
            .fetch_optional(conn)
            .await?;
    require(
        &version,
        global,
        floor.ok_or(Error::UnknownGame)?.as_deref(),
    )
}

/// The floor of a session still in the queue, checked when the launcher polls it (its admission
/// is still ahead). An admitted session is never refused below a raised floor (OD-121 follow-up
/// "Admission only": the floor applies when a session is created or admitted, never to one that
/// already plays). Ownership is checked before policy.
pub async fn require_queued_in(
    conn: &mut PgConnection,
    owner: Uuid,
    session: Uuid,
    global: &str,
) -> Result<(), Error> {
    let row = sqlx::query("SELECT s.state, s.launcher_version, g.min_launcher FROM play_session s JOIN game g ON g.code=s.game WHERE s.id=$1 AND s.account_id=$2")
        .bind(session).bind(owner).fetch_optional(conn).await?.ok_or(Error::NotFound)?;
    if row.try_get::<String, _>("state")? != "queued" {
        return Ok(());
    }
    require(
        &row.try_get::<String, _>("launcher_version")?,
        global,
        row.try_get::<Option<String>, _>("min_launcher")?.as_deref(),
    )
}
