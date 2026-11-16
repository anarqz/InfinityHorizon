//! Hardcore → Noob (OD-52; ACCOUNTS.md §5 "Mode change", SAVES_API.md §8 "Retired line";
//! migration `20261019150000_mode_change.sql`).
//!
//! Only a Supporter's Hardcore profile keeps data on the servers, so the one mode change allowed
//! deletes that data. [`change_mode_in`] runs it in the caller's transaction, which the caller
//! commits only on [`ModeChanged::Changed`]:
//! 1. locks the account row, then the profile row (both `FOR NO KEY UPDATE`), then the save head
//!    (`FOR UPDATE`). `NO KEY UPDATE` keeps the order of `accounts.rs` (account first) and
//!    serializes with every account and profile write, but does not conflict with the
//!    `KEY SHARE` locks foreign-key checks take: admission inserts a play session (keys to the
//!    account and the profile) while it holds the head's lock, and a `FOR UPDATE` here would wait
//!    for it while it waits for us;
//! 2. refuses a queued or active play session or a live lease, read under the head's lock, so the
//!    change never races a game, an admission or a commit (admission takes the account's row
//!    `FOR KEY SHARE`, which does not wait for this one, then the profile's `FOR SHARE`, which
//!    waits for this lock or makes this wait, so its session is either seen here or sees the
//!    retired line); then refuses while a game access ticket minted for any session of the
//!    profile is unexpired and its session is open or closed within [`LIVENESS_CACHE_SECS`]
//!    (`play_session.ticket_exp`, [`ModeChangeError::TicketLive`]): the game plane may still serve
//!    it from its liveness cache, and an expired session's heartbeat could resume it;
//! 3. without confirmation, answers what would be deleted ([`Warning`]) and writes nothing;
//! 4. confirmed: the profile's `expired` sessions are closed (`ended`, their end reason kept), so
//!    no heartbeat resumes one with its Hardcore ticket; `mode = noob`, the line retired (lease
//!    cleared), the universe rows deleted by
//!    `fi_purge_cloud_data` (the runtime holds no DELETE on them: the function, owned by the
//!    migrator, deletes one Noob, retired profile's rows), and
//!    an `account_audit` row with the counts.
//!
//! The tables are listed once, in `cloud_purge_target` (the migration); friendships are not among
//! them (they belong to the account, owner 2026-10-19). Nothing here logs a XUID or a gamertag.

use crate::accounts::{self, ProfileRow};
use crate::identity::IdentityError;
use fi_domain::ProfileMode;
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The warning's category of the save line's revisions (the rest come from `cloud_purge_target`).
pub const SAVE_REVISIONS: &str = "saveRevisions";

/// The longest the game plane serves a session's liveness from its cache after the session closed
/// (fi-api's `LivenessCache` caps its TTL at 60 s, ADR 0008 §4 amendment): a Hardcore → Noob change
/// waits that long after a session's end, never the ticket's whole remaining life.
pub const LIVENESS_CACHE_SECS: f64 = 60.0;

/// What a Hardcore → Noob change deletes (`ACCOUNTS.MODE_CHANGE_CONFIRM` details).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The save line's head revision.
    pub save_revision: u64,
    /// Rows by category, zeros included.
    pub deletes: BTreeMap<String, u64>,
}

/// The outcome of [`change_mode_in`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModeChanged {
    /// The profile is already Noob: nothing to do, nothing written.
    Unchanged(ProfileRow),
    /// Not confirmed: what the change would delete. Nothing written.
    Confirm(Warning),
    /// Done (the caller records the idempotency key and commits).
    Changed {
        /// The profile, now Noob.
        profile: ProfileRow,
        /// What was deleted: the warning's counts at the change.
        warning: Warning,
    },
}

/// Why a mode change is refused.
#[derive(Debug, thiserror::Error)]
pub enum ModeChangeError {
    /// No such live profile of this account (`ACCOUNTS.NOT_FOUND`).
    #[error("not found")]
    NotFound,
    /// The account or the profile is pending deletion (`ACCOUNTS.DELETION_PENDING`).
    #[error("pending deletion")]
    DeletionPending,
    /// Not Hardcore → Noob (`ACCOUNTS.MODE_CHANGE_FORBIDDEN`).
    #[error("only hardcore to noob")]
    Forbidden,
    /// CR-S98 review (`ACCOUNTS.MODE_CHANGE_FORBIDDEN` with `details: {reason}`): the mode is the
    /// profile's, so the change is refused when the profile has careers in more than one game
    /// (`careers_in_games`) or a career's game does not allow the target mode (`mode_not_allowed`). One
    /// live profile per mode is a creation limit only: it does not refuse a change.
    #[error("mode change refused: {reason}")]
    Refused {
        /// The reason code.
        reason: &'static str,
    },
    /// A queued or active play session, or a live save lease (`SESSIONS.PROFILE_IN_USE`).
    #[error("profile in use")]
    InUse,
    /// A game access ticket of one of the profile's sessions is unexpired until `until`
    /// (`SESSIONS.PROFILE_IN_USE` with `details.ticketExpiresAt`): the change waits for it.
    #[error("a game ticket of the profile is live")]
    TicketLive {
        /// The last ticket's `exp`.
        until: fi_domain::UnixMs,
    },
    /// Database trouble.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for ModeChangeError {
    fn from(e: sqlx::Error) -> Self {
        ModeChangeError::Db(e.into())
    }
}

/// Changes the mode of the account's live profile `xuid` to `to` (ACCOUNTS.md §5 "Mode change"),
/// in the caller's transaction. `confirmed` is the request's `confirm: true`. The caller commits on
/// [`ModeChanged::Changed`] and may roll back otherwise (nothing else is written).
pub async fn change_mode_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    to: ProfileMode,
    confirmed: bool,
    game: &str,
) -> Result<ModeChanged, ModeChangeError> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM account WHERE id = $1 FOR NO KEY UPDATE")
            .bind(account_id)
            .fetch_optional(&mut *conn)
            .await?;
    match status.as_deref() {
        None | Some("deleted") => return Err(ModeChangeError::NotFound),
        Some("deletion_pending") => return Err(ModeChangeError::DeletionPending),
        Some(_) => {}
    }
    let xuid = i64::try_from(xuid).map_err(|_| ModeChangeError::NotFound)?;
    let row = sqlx::query(
        "SELECT id, game, mode, status, player_profile_id FROM profile \
         WHERE account_id = $1 AND xuid = $2 AND game = $3 AND status <> 'deleted' FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .bind(xuid)
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ModeChangeError::NotFound)?;
    let profile_id: Uuid = row.try_get("id")?;
    let game: String = row.try_get("game")?;
    if row.try_get::<String, _>("status")? != "active" {
        return Err(ModeChangeError::DeletionPending);
    }
    let from: ProfileMode = row
        .try_get::<String, _>("mode")?
        .parse()
        .map_err(|_| IdentityError::Event("profile has an invalid mode".into()))?;
    if from == ProfileMode::Noob && to == ProfileMode::Noob {
        return accounts::profile_in(conn, profile_id)
            .await?
            .map(ModeChanged::Unchanged)
            .ok_or(ModeChangeError::NotFound);
    }
    from.change_to(to).map_err(|_| ModeChangeError::Forbidden)?;
    refuse_profile_wide_in(conn, row.try_get("player_profile_id")?, to).await?;

    let revision = match lock_idle_line_in(conn, profile_id).await? {
        Ok(revision) => revision,
        Err(InUse::Session) => return Err(ModeChangeError::InUse),
        Err(InUse::TicketLive { until }) => return Err(ModeChangeError::TicketLive { until }),
    };
    let warning = warning_in(conn, &game, profile_id, revision).await?;
    if !confirmed {
        return Ok(ModeChanged::Confirm(warning));
    }

    // An expired session's heartbeat would re-admit it (`admission::heartbeat_any_in`): closed
    // now, its end reason and time kept (not closed twice).
    sqlx::query(
        "UPDATE play_session SET state = 'ended' WHERE profile_id = $1 AND state = 'expired'",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE profile SET mode = $2, mode_changed_at = clock_timestamp() WHERE id = $1")
        .bind(profile_id)
        .bind(ProfileMode::Noob.as_str())
        .execute(&mut *conn)
        .await?;
    // The worlds lane (MATCHMAKING_API.md §3.3): no seat or world of the old mode stays.
    crate::matchmaking::release_profile_in(conn, &game, profile_id).await?;
    // Car Clubs (CR-S70, OD-130): a club is of one mode, so the profile leaves its club of the old
    // mode (succession as a leave) and its pending invites and requests go.
    crate::clubs::leave_mode_in(conn, &game, from.as_str(), profile_id).await?;
    // An expired lease may still be recorded: a retired line holds none (its CHECK).
    sqlx::query(
        "UPDATE save_head SET retired_at = clock_timestamp(), lease_session_id = NULL, lease_token = NULL, \
           lease_device_id = NULL, lease_device_label = NULL, lease_acquired_at = NULL, lease_expires_at = NULL, \
           lease_ttl_ms = NULL, lease_accounted_at = NULL \
         WHERE profile_id = $1",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    let deleted: serde_json::Value = sqlx::query_scalar("SELECT fi_purge_cloud_data($1)")
        .bind(profile_id)
        .fetch_one(&mut *conn)
        .await?;
    let profile = accounts::profile_in(conn, profile_id)
        .await?
        .ok_or(ModeChangeError::NotFound)?;
    let detail = serde_json::json!({
        "profileId": profile_id,
        "from": from.as_str(),
        "to": ProfileMode::Noob.as_str(),
        "saveRevision": warning.save_revision,
        "saveRevisions": warning.deletes.get(SAVE_REVISIONS),
        "deleted": deleted,
    });
    sqlx::query(
        "INSERT INTO account_audit (account_id, kind, actor, detail) VALUES ($1, 'profile.mode_changed', 'player', $2)",
    )
    .bind(account_id)
    .bind(detail)
    .execute(&mut *conn)
    .await?;
    Ok(ModeChanged::Changed { profile, warning })
}

/// The careers' part of [`refuse_profile_wide_in`]: careers in more than one game, or a game of
/// them that does not allow `to`.
fn careers_refusal(rules: &[&fi_domain::GameRules], to: ProfileMode) -> Option<&'static str> {
    if rules.len() > 1 {
        Some("careers_in_games")
    } else if !rules.iter().all(|r| r.allows(to)) {
        Some("mode_not_allowed")
    } else {
        None
    }
}

/// The profile-wide refusals of a mode change (CR-S98 review; [`ModeChangeError::Refused`]): the
/// mode is the profile `parent`'s, shared by its careers.
async fn refuse_profile_wide_in(
    conn: &mut PgConnection,
    parent: Uuid,
    to: ProfileMode,
) -> Result<(), ModeChangeError> {
    let games: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT game FROM profile WHERE player_profile_id = $1 AND status <> 'deleted'",
    )
    .bind(parent)
    .fetch_all(&mut *conn)
    .await?;
    let rules: Vec<&fi_domain::GameRules> =
        games.iter().map(|g| fi_domain::GameRules::of(g)).collect();
    // One live profile per mode is a creation limit only (CAREERS.md §2): an account that already
    // holds a profile of the target mode changes as before (§6.8, released launchers).
    match careers_refusal(&rules, to) {
        Some(reason) => Err(ModeChangeError::Refused { reason }),
        None => Ok(()),
    }
}

/// Why a profile is in use: a mode change and a profile deletion refuse it
/// (`SESSIONS.PROFILE_IN_USE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InUse {
    /// A queued or active play session (one past its expiry included, until the gate closes it),
    /// or a live save lease.
    Session,
    /// A game access ticket of one of the profile's sessions may still be served until `until`.
    TicketLive {
        /// The earlier of the ticket's `exp` and the liveness cache's window end.
        until: fi_domain::UnixMs,
    },
}

/// Locks the profile's save head (`FOR UPDATE`, created when missing so the lock always holds a
/// row) after the caller locked the account and the profile rows (`FOR NO KEY UPDATE`, in that
/// order), and answers the head's revision when the profile is idle: no queued or active play
/// session, no live lease, and no game access ticket the game plane may still serve (an unexpired
/// ticket of a session that is open or closed within [`LIVENESS_CACHE_SECS`]).
pub async fn lock_idle_line_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
) -> Result<Result<i64, InUse>, sqlx::Error> {
    // Every profile has its line (accounts::insert_profile_in); a missing one is created so the
    // lock below always holds a row.
    sqlx::query(
        "INSERT INTO save_head (profile_id) VALUES ($1) ON CONFLICT (profile_id) DO NOTHING",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    let head = sqlx::query(
        "SELECT revision, lease_session_id IS NOT NULL AND lease_expires_at > clock_timestamp() AS leased \
         FROM save_head WHERE profile_id = $1 FOR UPDATE",
    )
    .bind(profile_id)
    .fetch_one(&mut *conn)
    .await?;
    let leased: bool = head.try_get("leased")?;
    // Expired sessions count until the gate closes them: the launcher ends its own first.
    let session: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM play_session WHERE profile_id = $1 AND state IN ('queued', 'active'))",
    )
    .bind(profile_id)
    .fetch_one(&mut *conn)
    .await?;
    if leased || session {
        return Ok(Err(InUse::Session));
    }
    // A ticket outlives its session in the game plane's liveness cache (at most
    // LIVENESS_CACHE_SECS), and an expired session's heartbeat brings its ticket back (the callers
    // close those): an unexpired ticket of a session that is open, or that closed within the
    // cache's window, may still be served, so the caller waits until the earlier of the ticket's
    // exp and the window's end. A session closed longer ago is never served again.
    let ticket: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT max(least(ticket_exp, coalesce(ended_at + make_interval(secs => $2), ticket_exp))) \
         FROM play_session WHERE profile_id = $1 AND ticket_exp > clock_timestamp() \
           AND (ended_at IS NULL OR ended_at > clock_timestamp() - make_interval(secs => $2))",
    )
    .bind(profile_id)
    .bind(LIVENESS_CACHE_SECS)
    .fetch_one(&mut *conn)
    .await?;
    if let Some(until) = ticket {
        return Ok(Err(InUse::TicketLive {
            until: crate::identity::ms(until),
        }));
    }
    Ok(Ok(head.try_get("revision")?))
}

/// The warning of a change: the universe rows by category, and the save line's revisions.
async fn warning_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    head_revision: i64,
) -> Result<Warning, ModeChangeError> {
    let counts: serde_json::Value = sqlx::query_scalar("SELECT fi_cloud_data_counts($1, $2)")
        .bind(game)
        .bind(profile_id)
        .fetch_one(&mut *conn)
        .await?;
    let mut deletes: BTreeMap<String, u64> = counts
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0)))
                .collect()
        })
        .unwrap_or_default();
    let revisions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM save_revision WHERE profile_id = $1")
            .bind(profile_id)
            .fetch_one(&mut *conn)
            .await?;
    deletes.insert(SAVE_REVISIONS.to_owned(), revisions.max(0) as u64);
    Ok(Warning {
        save_revision: head_revision.max(0) as u64,
        deletes,
    })
}

#[cfg(test)]
mod careers_tests {
    use super::*;
    use fi_domain::game::{FH2_RULES, FM5_RULES};

    #[test]
    fn a_mode_change_reads_every_careers_rules() {
        // A game that keeps Hardcore only (none does today): Noob is not allowed there.
        let hardcore_only = fi_domain::GameRules {
            modes: &[ProfileMode::Hardcore],
            ..FH2_RULES
        };
        assert_eq!(
            careers_refusal(&[&hardcore_only], ProfileMode::Noob),
            Some("mode_not_allowed")
        );
        assert_eq!(
            careers_refusal(&[&FH2_RULES, &FM5_RULES], ProfileMode::Noob),
            Some("careers_in_games")
        );
        assert_eq!(careers_refusal(&[&FH2_RULES], ProfileMode::Noob), None);
        assert_eq!(careers_refusal(&[], ProfileMode::Noob), None);
    }
}
