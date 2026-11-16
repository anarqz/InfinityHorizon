//! The free rename after an OD-120 clash rename (ACCOUNTS.md §5 "Plain names and renames", item 2;
//! migration `20261025120000_plain_names.sql`).
//!
//! The backfill gave every clashing profile a free name and a rename notice. [`rename_in`] lets
//! the player use it once: the current name (case-insensitively) keeps it, another free name takes
//! it, and either clears the notice. Locks and the in-use rules are the mode change's
//! ([`crate::purge`]): the account row, then the profile row (`FOR NO KEY UPDATE`), then the save
//! head (`FOR UPDATE`); a queued or active session, a live lease or a game ticket the game plane
//! may still serve refuses it, because the game got the name at launch.

use crate::accounts::{self, ProfileRow};
use crate::identity::IdentityError;
use crate::purge::LIVENESS_CACHE_SECS;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// What a rename did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Renamed {
    /// The notice was already gone and the name is the current one: an idempotent repeat of a
    /// rename whose record expired. Nothing written.
    Unchanged(ProfileRow),
    /// The notice cleared: kept (`renamed: false`) or renamed (`true`). The caller commits.
    Done {
        /// The profile after the change.
        profile: ProfileRow,
        /// The name changed (else kept).
        renamed: bool,
    },
}

/// Why a rename is refused.
#[derive(Debug, thiserror::Error)]
pub enum RenameError {
    /// No such live profile of this account (`ACCOUNTS.NOT_FOUND`).
    #[error("not found")]
    NotFound,
    /// The account or the profile is pending deletion (`ACCOUNTS.DELETION_PENDING`).
    #[error("pending deletion")]
    DeletionPending,
    /// No free rename (`ACCOUNTS.RENAME_NOT_ALLOWED`).
    #[error("no free rename")]
    NotAllowed,
    /// Taken in the game or reserved (`ACCOUNTS.NAME_UNAVAILABLE`).
    #[error("name unavailable")]
    NameUnavailable,
    /// A queued or active play session or a live lease (`SESSIONS.PROFILE_IN_USE`) of a career of
    /// the profile in `game` (CR-S98: `details.game`).
    #[error("profile in use")]
    InUse {
        /// The game of the career in use.
        game: String,
    },
    /// A game ticket of the profile may still be served until `until` (`SESSIONS.PROFILE_IN_USE`
    /// with `details.ticketExpiresAt`).
    #[error("a game ticket of the profile is live")]
    TicketLive {
        /// When it stops.
        until: fi_domain::UnixMs,
    },
    /// Database trouble.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for RenameError {
    fn from(e: sqlx::Error) -> Self {
        RenameError::Db(e.into())
    }
}

/// Renames (or keeps) the account's live profile `xuid` to `name`, a valid plain name (the caller
/// validates it: `ACCOUNTS.INVALID_NAME`), in the caller's transaction. `game` picks the career
/// whose notice is used (CR-S98); the name is the profile's, so the rename reaches every live
/// career of it (the sync triggers of `20261107180000_player_profiles.sql`), and any of them in use
/// refuses it.
pub async fn rename_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    name: &str,
    game: &str,
) -> Result<Renamed, RenameError> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM account WHERE id = $1 FOR NO KEY UPDATE")
            .bind(account_id)
            .fetch_optional(&mut *conn)
            .await?;
    match status.as_deref() {
        None | Some("deleted") => return Err(RenameError::NotFound),
        Some("deletion_pending") => return Err(RenameError::DeletionPending),
        Some(_) => {}
    }
    let xuid = i64::try_from(xuid).map_err(|_| RenameError::NotFound)?;
    let row = sqlx::query(
        "SELECT id, player_profile_id, name, status, rename_notice_from IS NOT NULL AS notice FROM profile \
         WHERE account_id = $1 AND xuid = $2 AND game = $3 AND status <> 'deleted' FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .bind(xuid)
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RenameError::NotFound)?;
    let profile_id: Uuid = row.try_get("id")?;
    let parent: Uuid = row.try_get("player_profile_id")?;
    if row.try_get::<String, _>("status")? != "active" {
        return Err(RenameError::DeletionPending);
    }
    let current: String = row.try_get("name")?;
    let keep = current.eq_ignore_ascii_case(name);
    if !row.try_get::<bool, _>("notice")? {
        if current == name {
            return accounts::profile_in(conn, profile_id)
                .await?
                .map(Renamed::Unchanged)
                .ok_or(RenameError::NotFound);
        }
        return Err(RenameError::NotAllowed);
    }
    if !keep && fi_domain::gamertag::is_reserved(name) {
        return Err(RenameError::NameUnavailable);
    }

    sqlx::query(
        "INSERT INTO save_head (profile_id) VALUES ($1) ON CONFLICT (profile_id) DO NOTHING",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    let leased: bool = sqlx::query_scalar(
        "SELECT lease_session_id IS NOT NULL AND lease_expires_at > clock_timestamp() \
         FROM save_head WHERE profile_id = $1 FOR UPDATE",
    )
    .bind(profile_id)
    .fetch_one(&mut *conn)
    .await?;
    if leased {
        return Err(RenameError::InUse {
            game: game.to_owned(),
        });
    }
    // CR-S98: the name is every career's, so any career of the profile in use refuses it.
    let busy: Option<String> = sqlx::query_scalar(
        "SELECT c.game FROM profile c WHERE c.player_profile_id = $1 \
           AND (EXISTS (SELECT 1 FROM play_session s WHERE s.profile_id = c.id \
                  AND s.state IN ('queued', 'active')) \
             OR EXISTS (SELECT 1 FROM save_head h WHERE h.profile_id = c.id \
                  AND h.lease_session_id IS NOT NULL AND h.lease_expires_at > clock_timestamp())) \
         ORDER BY c.created_at LIMIT 1",
    )
    .bind(parent)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(game) = busy {
        return Err(RenameError::InUse { game });
    }
    let ticket: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT max(least(ticket_exp, coalesce(ended_at + make_interval(secs => $2), ticket_exp))) \
         FROM play_session WHERE profile_id IN (SELECT id FROM profile WHERE player_profile_id = $1) \
           AND ticket_exp > clock_timestamp() \
           AND (ended_at IS NULL OR ended_at > clock_timestamp() - make_interval(secs => $2))",
    )
    .bind(parent)
    .bind(LIVENESS_CACHE_SECS)
    .fetch_one(&mut *conn)
    .await?;
    if let Some(until) = ticket {
        return Err(RenameError::TicketLive {
            until: crate::identity::ms(until),
        });
    }

    let mut sp = sqlx::Connection::begin(&mut *conn).await?;
    let updated = sqlx::query(
        "UPDATE profile SET name = CASE WHEN $2 THEN name ELSE $3 END, \
           rename_notice_from = NULL, rename_notice_at = NULL WHERE id = $1",
    )
    .bind(profile_id)
    .bind(keep)
    .bind(name)
    .execute(&mut *sp)
    .await;
    match updated {
        Ok(_) => sp.commit().await?,
        Err(e) if accounts::is_name_taken(&e) => {
            sp.rollback().await?;
            return Err(RenameError::NameUnavailable);
        }
        Err(e) => return Err(e.into()),
    }
    let profile = accounts::profile_in(conn, profile_id)
        .await?
        .ok_or(RenameError::NotFound)?;
    sqlx::query(
        "INSERT INTO account_audit (account_id, kind, actor, detail) VALUES ($1, 'profile.renamed', 'player', $2)",
    )
    .bind(account_id)
    .bind(serde_json::json!({"profileId": profile_id, "kept": keep}))
    .execute(&mut *conn)
    .await?;
    Ok(Renamed::Done {
        profile,
        renamed: !keep,
    })
}
