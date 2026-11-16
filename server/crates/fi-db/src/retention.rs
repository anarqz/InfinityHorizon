//! Data retention (HANDOFF §5; the windows' sources below): the nightly `fi-cli data-retention`
//! job deletes rows the contracts promise not to keep for ever (LGPD data minimisation, Postgres
//! cost). As a `fi_maintenance` login (`fi_jobs`), in batches of [`BATCH`] rows, each batch its
//! own statement: no long transaction, no network call, idempotent (a rerun deletes nothing).
//!
//! | Table | Deleted | Source |
//! | --- | --- | --- |
//! | `integrity_assessment` | `at` older than [`ASSESSMENT_DAYS`] | ADR 0020 §5, CLOUD_SAVES.md §7.3 |
//! | `account_friend_request` | `created_at` older than [`FRIEND_REQUEST_DAYS`] (lapsed already: `fi_db::friends` reads only younger ones) | `fi_db::friends` (the 30-day lapse) |
//! | `game_notification` | `sent_at` older than [`NOTIFICATION_DAYS`] (the game's `expirationDate`) | GAME_PLATFORM.md §3 inbox |
//! | `session_idle_observation` | `observed_at` older than 90 days | GATE_API.md §4a (OD-121) |
//! | `credential_mail_token` | expired or used more than [`MAIL_TOKEN_DAYS`] ago (a `local` instance's single-use links, CR-S53) | ACCOUNTS.md §4.1 |
//! | `world_invite` | answered, withdrawn or expired more than [`INVITE_DAYS`] ago (a TTL of at most 15 minutes) | MATCHMAKING_API.md §3.4 |
//! | `public_event` | `at` older than [`crate::discord::FEED_DAYS`] (the Discord feed) | CORE_API.md §5a (CR-S63) |
//! | `discord_link_code` | expired or spent more than [`LINK_CODE_DAYS`] ago (10-minute single-use codes) | CORE_API.md §5a.2 (CR-S63) |
//! | `peer_observation` | `created_at` older than [`crate::observations::OBSERVATION_DAYS`] | MATCHMAKING_API.md §3.9 (OD-147) |
//! | `cheat_quorum` | `reached_at` older than a day (a non-host's flag reads 24 hours back) | MATCHMAKING_API.md §3.9 |
//! | `cheat_flag` | ended more than [`crate::observations::CHEAT_POOL_REPEAT_DAYS`] ago (a new flag within them doubles, OD-149) | MATCHMAKING_API.md §3.9 |
//! | `voice_mute_event` | `at` older than [`crate::voice::MUTE_EVENT_DAYS`] (the community rule reads 7 days back) | CORE_API.md §4.4 (CR-S68) |
//! | `play_session`, `session_summary` | a deleted profile's ended sessions and their summaries, [`DELETED_PROFILE_SESSION_DAYS`] after its `deleted_at` (the closed worlds it hosted lose the link; a session a crash report names waits for the report's expiry) | ACCOUNTS.md §8 (owner 2026-10-27, PD-Q1) |
//!
//! The save audit's queue (`integrity::audit_due_in`, at most 90 days back) cannot bring a revision
//! back once its `save_audit` row is deleted here: an audit row is always recorded after its
//! revision's `committed_at`, so it outlives the queue's window. Keep [`ASSESSMENT_DAYS`] at least
//! the queue's 90 days.
//!
//! Erasure of an account's rows (friends included) is the account-erasure job's (`crate::erasure`).

use crate::identity::IdentityError;
use sqlx::PgPool;

/// Rows per delete statement.
pub const BATCH: i64 = 1_000;
/// Guardrails' assessments are kept 90 days.
pub const ASSESSMENT_DAYS: i32 = 90;
/// A friend request lapses 30 days after it was sent.
pub const FRIEND_REQUEST_DAYS: i32 = 30;
/// An in-game message or rival challenge expires 30 days after it was sent.
pub const NOTIFICATION_DAYS: i32 = 30;
/// A spent or expired mail link (CR-S53) is kept 7 days (support context), then deleted.
pub const MAIL_TOKEN_DAYS: i32 = 7;
/// An ended invite or request to join is kept 7 days (moderation context), then deleted.
pub const INVITE_DAYS: i32 = 7;
/// A deleted profile's session history is kept 90 days after the deletion (owner 2026-10-27,
/// PD-Q1). Fixed inside `fi_purge_deleted_profile_sessions` (migration
/// `20261028003000_deleted_profile_sessions.sql`); this constant documents it.
pub const DELETED_PROFILE_SESSION_DAYS: i32 = 90;
/// A spent or expired Discord link code is kept 7 days (a replayed link answers while it stands).
pub const LINK_CODE_DAYS: i32 = 7;

/// What one run deleted, per table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    /// `integrity_assessment` rows.
    pub assessments: u64,
    /// `account_friend_request` rows.
    pub friend_requests: u64,
    /// `game_notification` rows.
    pub notifications: u64,
    /// `world_invite` rows.
    pub invites: u64,
    /// `session_idle_observation` rows (OD-121, 90 days).
    pub idle_observations: u64,
    /// `credential_mail_token` rows (CR-S53, 7 days after expiry or use).
    pub mail_tokens: u64,
    /// `public_event` rows (CR-S63, 7 days).
    pub feed_events: u64,
    /// `discord_link_code` rows (CR-S63, 7 days after expiry or use).
    pub link_codes: u64,
    /// `peer_observation` rows (OD-147, 30 days).
    pub peer_observations: u64,
    /// `cheat_quorum` rows (a day).
    pub cheat_quorums: u64,
    /// `cheat_flag` rows that ended more than 30 days ago.
    pub cheat_flags: u64,
    /// `voice_mute_event` rows (CR-S68, 30 days).
    pub voice_mute_events: u64,
    /// `play_session` rows of profiles deleted over 90 days ago (PD-Q1).
    pub deleted_profile_sessions: u64,
    /// Their `session_summary` rows.
    pub deleted_profile_summaries: u64,
    /// Closed `world` rows those profiles hosted, whose `host_session_id` was cleared.
    pub deleted_profile_worlds: u64,
}

/// `(table, the rows past retention)`: SQL predicates over constants only, `$1` the batch size.
const RULES: [(&str, &str); 12] = [
    (
        "integrity_assessment",
        "at < clock_timestamp() - make_interval(days => 90)",
    ),
    (
        "account_friend_request",
        "created_at < clock_timestamp() - make_interval(days => 30)",
    ),
    (
        "game_notification",
        "sent_at < clock_timestamp() - make_interval(days => 30)",
    ),
    (
        "world_invite",
        "((state <> 'pending' AND ended_at < clock_timestamp() - make_interval(days => 7)) \
          OR (state = 'pending' AND expires_at < clock_timestamp() - make_interval(days => 7)))",
    ),
    (
        "session_idle_observation",
        "observed_at < clock_timestamp() - make_interval(days => 90)",
    ),
    (
        "credential_mail_token",
        "(expires_at < clock_timestamp() - make_interval(days => 7) \
          OR used_at < clock_timestamp() - make_interval(days => 7))",
    ),
    (
        "public_event",
        "at < clock_timestamp() - make_interval(days => 7)",
    ),
    (
        "discord_link_code",
        "(expires_at < clock_timestamp() - make_interval(days => 7) \
          OR used_at < clock_timestamp() - make_interval(days => 7))",
    ),
    (
        "peer_observation",
        "created_at < clock_timestamp() - make_interval(days => 30)",
    ),
    (
        "cheat_quorum",
        "reached_at < clock_timestamp() - make_interval(days => 1)",
    ),
    (
        "cheat_flag",
        "until < clock_timestamp() - make_interval(days => 30)",
    ),
    (
        "voice_mute_event",
        "at < clock_timestamp() - make_interval(days => 30)",
    ),
];

/// Deletes every row past its table's retention, in batches; returns the counts.
pub async fn purge(db: &PgPool) -> Result<Purged, IdentityError> {
    let mut out = Purged::default();
    for (table, past) in RULES {
        // No row lock in the subquery (fi_maintenance has no UPDATE on most of these); the outer
        // predicate is re-checked, so a row a writer changed meanwhile stays.
        let sql = format!(
            "DELETE FROM {table} WHERE ctid IN (SELECT ctid FROM {table} WHERE {past} LIMIT $1) \
             AND {past}"
        );
        let mut deleted = 0u64;
        loop {
            let n = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(BATCH)
                .execute(db)
                .await?
                .rows_affected();
            deleted += n;
            if n < BATCH as u64 {
                break;
            }
        }
        match table {
            "integrity_assessment" => out.assessments = deleted,
            "account_friend_request" => out.friend_requests = deleted,
            "game_notification" => out.notifications = deleted,
            "session_idle_observation" => out.idle_observations = deleted,
            "credential_mail_token" => out.mail_tokens = deleted,
            "public_event" => out.feed_events = deleted,
            "discord_link_code" => out.link_codes = deleted,
            "peer_observation" => out.peer_observations = deleted,
            "cheat_quorum" => out.cheat_quorums = deleted,
            "cheat_flag" => out.cheat_flags = deleted,
            "voice_mute_event" => out.voice_mute_events = deleted,
            _ => out.invites = deleted,
        }
    }
    // A deleted profile's session history (PD-Q1), through the migrator's function (the job holds
    // no DELETE on play_session or session_summary), one batch per statement.
    loop {
        let c: serde_json::Value =
            sqlx::query_scalar("SELECT fi_purge_deleted_profile_sessions($1::integer)")
                .bind(BATCH as i32)
                .fetch_one(db)
                .await?;
        let count = |k: &str| c.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        let sessions = count("play_session");
        out.deleted_profile_sessions += sessions;
        out.deleted_profile_summaries += count("session_summary");
        out.deleted_profile_worlds += count("world");
        if sessions < BATCH as u64 {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_predicates_use_the_windows() {
        let days = |table: &str| {
            RULES
                .iter()
                .find(|(t, _)| *t == table)
                .map(|(_, p)| p.to_string())
                .unwrap()
        };
        assert!(days("integrity_assessment").contains(&format!("days => {ASSESSMENT_DAYS}")));
        assert!(days("account_friend_request").contains(&format!("days => {FRIEND_REQUEST_DAYS}")));
        assert!(days("game_notification").contains(&format!("days => {NOTIFICATION_DAYS}")));
        assert!(days("world_invite").contains(&format!("days => {INVITE_DAYS}")));
        assert!(days("credential_mail_token").contains(&format!("days => {MAIL_TOKEN_DAYS}")));
        let feed_days = crate::discord::FEED_DAYS;
        assert!(days("public_event").contains(&format!("days => {feed_days}")));
        assert!(days("discord_link_code").contains(&format!("days => {LINK_CODE_DAYS}")));
        let observation_days = crate::observations::OBSERVATION_DAYS;
        assert!(days("peer_observation").contains(&format!("days => {observation_days}")));
        // A flag's row outlives it by the repeat window (OD-149: a new flag within it doubles).
        let repeat_days = crate::observations::CHEAT_POOL_REPEAT_DAYS;
        assert!(days("cheat_flag").contains(&format!("days => {repeat_days}")));
        let mute_days = crate::voice::MUTE_EVENT_DAYS;
        assert!(days("voice_mute_event").contains(&format!("days => {mute_days}")));
        // The community rule reads 7 days of mutes.
        const {
            assert!(
                crate::voice::MUTE_EVENT_DAYS as i64 * 86_400
                    >= crate::voice::COMMUNITY_WINDOW_SECS
            )
        };
        // A non-host's flag reads the quorums of the last 24 hours.
        const { assert!(crate::observations::HOSTS_WINDOW_HOURS <= 24) };
        // The save audit's queue looks back at most 90 days (integrity::audit_due_in).
        const { assert!(ASSESSMENT_DAYS >= 90) };
    }
}
