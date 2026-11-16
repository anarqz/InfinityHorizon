//! Account erasure (ACCOUNTS.md §8 "What erasure does per table"; owner 2026-10-26 "Account
//! erasure: build now"; migration `20261026190000_account_erasure.sql`).
//!
//! [`run`] (`fi-cli account-erasure`, daily, before `saves-retention` and `saves-gc`) erases every
//! account whose 14-day grace has passed (`status = 'deletion_pending'` and `deletion_due_at <=`
//! the database clock), in pages of [`BATCH`], **one account per transaction** ([`erase_in`]):
//!
//! 1. the account row is locked (`FOR UPDATE SKIP LOCKED`: `cancel_deletion_in` and
//!    `request_deletion_in` take the same lock, so a cancel either lands first and the account is
//!    no longer due, or waits and finds it erased) and the predicate is read again under the lock;
//! 2. it is left for the next run ([`Outcome::Busy`]) while one of its play sessions is queued or
//!    active or its game ticket may still be served (60 s after `ticket_exp`), a save head of its
//!    profiles is locked or holds an unexpired lease, or it hosts or owns a world still open;
//! 3. an identity mapping that names no `credential` row (an Identity Platform user) is left for
//!    the next run on a `firebase` instance ([`Outcome::IdpPending`]): this job makes no network
//!    call and there is no Admin API client, so the IdP user must be deleted first (open item); on
//!    a `local` instance (`FI_AUTH_BACKEND=local`, [`Options::local_identity`]) every mapping is
//!    the instance's own and is erased;
//! 4. the account-scoped statements of [`STEPS`] run in order (foreign keys decide it), each over
//!    the account (`k.a`) and its profiles (`k.p`, only to tombstone them), then each profile's
//!    rows go through [`PROFILE_ERASURE`] (`fi_erase_profile_data`), the one per-profile list a
//!    player's own profile deletion ([`delete_profile_in`], owner 2026-10-27) shares;
//! 5. one `account_audit` row (`account.erased`, actor `system`, the per-table counts, no names)
//!    is written. No `account.deleted` event (the outbox is gone, owner 2026-10-26): billing's
//!    ledger (`entitlement`, `entitlement_event`) stays in [`KEPT`] until monetization pseudonymises
//!    it in this transaction (ACCOUNTS.md §8 "account.deleted dependency"; open).
//!
//! Nothing of another account is deleted except rows that are the erased account's own data on
//! both sides (a friendship, a friend request, an invite, a world ban, an in-game message it sent,
//! its participation in another player's crash report). What stays is [`KEPT`], each with its
//! reason; the Postgres suite scans every column of every table for the account's ids, XUIDs,
//! e-mail and names and fails on any hit outside it; after a single profile's deletion it scans for
//! that profile's id, XUID and name, allowing [`KEPT`] and [`PROFILE_KEPT`]. Save blobs are never touched: the save line's
//! rows go, and `saves-gc` deletes the objects no row names any more (CLOUD_SAVES.md §6).
//!
//! Idempotent: an erased account is `deleted`, so a rerun selects nothing. Logs ids and counts
//! only.

use sqlx::{PgConnection, PgPool, Row};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use uuid::Uuid;

/// Accounts read per page.
pub const BATCH: i64 = 100;

/// How long after a ticket's expiry the game plane may still serve it from its liveness cache
/// (`purge::LIVENESS_CACHE_SECS`).
const TICKET_TAIL_SECS: f64 = crate::purge::LIVENESS_CACHE_SECS;

/// One `(table, column)` that may still hold the erased account's ids after [`erase_in`], only in
/// the rows `rows` selects (an SQL predicate over the row `t`, `$2` the erased account), and why
/// (ACCOUNTS.md §8). Any other column, or another row of these, holding its ids, XUIDs, e-mail,
/// names or install id is a leak (the Postgres suite scans every column of every table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kept {
    /// The table.
    pub table: &'static str,
    /// The column.
    pub column: &'static str,
    /// The rows where it may stay.
    pub rows: &'static str,
    /// Why.
    pub why: &'static str,
}

/// What stays of an erased account.
pub const KEPT: &[Kept] = &[
    Kept {
        table: "account",
        column: "id",
        rows: "t.id = $2",
        why: "the tombstone (status deleted)",
    },
    Kept {
        table: "profile",
        column: "id",
        rows: "t.account_id = $2",
        why: "the tombstone (status deleted, no name): XUIDs are never reused",
    },
    Kept {
        table: "profile",
        column: "account_id",
        rows: "t.account_id = $2",
        why: "the tombstone",
    },
    Kept {
        table: "profile",
        column: "xuid",
        rows: "t.account_id = $2",
        why: "retired, never reused (§3)",
    },
    Kept {
        table: "profile",
        column: "player_profile_id",
        rows: "t.account_id = $2",
        why: "the career tombstone's profile (CR-S98)",
    },
    Kept {
        table: "player_profile",
        column: "id",
        rows: "t.account_id = $2",
        why: "the profile's tombstone (status deleted, no name), the careers' foreign key (CR-S98)",
    },
    Kept {
        table: "player_profile",
        column: "account_id",
        rows: "t.account_id = $2",
        why: "the tombstone",
    },
    Kept {
        table: "player_profile",
        column: "xuid",
        rows: "t.account_id = $2",
        why: "retired, never reused (§3)",
    },
    Kept {
        table: "save_head",
        column: "profile_id",
        rows: "t.profile_id IN (SELECT id FROM profile WHERE account_id = $2)",
        why: "the line's head, returned to revision 0 (the guard's erasure path)",
    },
    Kept {
        table: "play_session",
        column: "id",
        rows: "t.account_id = $2",
        why: "pseudonymous session record: pool and server totals, foreign key target of kept rows; deleted by data-retention 90 days after the profile's deletion (PD-Q1)",
    },
    Kept {
        table: "play_session",
        column: "account_id",
        rows: "t.account_id = $2",
        why: "as play_session.id",
    },
    Kept {
        table: "play_session",
        column: "profile_id",
        rows: "t.account_id = $2",
        why: "as play_session.id",
    },
    Kept {
        table: "play_session",
        column: "device_binding_id",
        rows: "t.account_id = $2",
        why: "as play_session.id",
    },
    Kept {
        table: "device_binding",
        column: "id",
        rows: "t.account_id = $2",
        why: "anonymised: label, fingerprint and install id replaced; play sessions name it",
    },
    Kept {
        table: "device_binding",
        column: "account_id",
        rows: "t.account_id = $2",
        why: "as device_binding.id",
    },
    Kept {
        table: "session_summary",
        column: "play_session_id",
        rows: "t.profile_id IN (SELECT id FROM profile WHERE account_id = $2)",
        why: "aggregate counters (numbers only): the server totals stay consistent; deleted with its session 90 days after the profile's deletion (PD-Q1)",
    },
    Kept {
        table: "session_summary",
        column: "profile_id",
        rows: "t.profile_id IN (SELECT id FROM profile WHERE account_id = $2)",
        why: "as session_summary.play_session_id",
    },
    Kept {
        table: "world",
        column: "host_account_id",
        rows: "t.host_account_id = $2",
        why: "its closed world, anonymised: other players' grants, reports and crash reports name it",
    },
    Kept {
        table: "world",
        column: "host_profile_id",
        rows: "t.host_account_id = $2",
        why: "as world.host_account_id",
    },
    Kept {
        table: "world",
        column: "host_session_id",
        rows: "t.host_account_id = $2",
        why: "as world.host_account_id; cleared with the session 90 days after the profile's deletion (PD-Q1)",
    },
    Kept {
        table: "account_audit",
        column: "account_id",
        rows: "t.account_id = $2",
        why: "its security trail without install ids, no IP or e-mail; ends with account.erased",
    },
    Kept {
        table: "account_audit",
        column: "detail",
        rows: "t.account_id = $2",
        why: "profile ids and counts of its own audit rows (no names)",
    },
    Kept {
        table: "admin_action",
        column: "account_id",
        rows: "t.account_id = $2 OR t.actor_account_id = $2",
        why: "operator audit, append-only (moderation history)",
    },
    Kept {
        table: "admin_action",
        column: "actor_account_id",
        rows: "t.account_id = $2 OR t.actor_account_id = $2",
        why: "as admin_action.account_id",
    },
    Kept {
        table: "admin_action",
        column: "target",
        rows: "t.account_id = $2 OR t.actor_account_id = $2",
        why: "as admin_action.account_id",
    },
    Kept {
        table: "admin_action",
        column: "detail",
        rows: "t.account_id = $2 OR t.actor_account_id = $2",
        why: "as admin_action.account_id",
    },
    Kept {
        table: "integrity_strike",
        column: "profile_id",
        rows: "t.profile_id IN (SELECT id FROM profile WHERE account_id = $2)",
        why: "moderation history (its parked bundle link is cleared)",
    },
    Kept {
        table: "integrity_strike",
        column: "created_by",
        rows: "t.created_by = $2",
        why: "the moderator of a manual strike (the schema requires one)",
    },
    Kept {
        table: "crash_review",
        column: "subject",
        rows: "t.subject = $2 OR t.moderator = $2",
        why: "human moderation decision, its own 90-day retention (CRASH_API.md)",
    },
    Kept {
        table: "crash_review",
        column: "moderator",
        rows: "t.subject = $2 OR t.moderator = $2",
        why: "as crash_review.subject",
    },
    Kept {
        table: "crash_restriction",
        column: "account_id",
        rows: "t.account_id = $2 OR t.released_by = $2",
        why: "as crash_review.subject",
    },
    Kept {
        table: "crash_restriction",
        column: "released_by",
        rows: "t.account_id = $2 OR t.released_by = $2",
        why: "as crash_review.subject",
    },
    Kept {
        table: "report",
        column: "reporter_account_id",
        rows: "t.reporter_account_id = $2 OR t.target_account_id = $2 OR t.decided_by = $2",
        why: "moderation history; the erased reporter's free text is cleared",
    },
    Kept {
        table: "report",
        column: "target_account_id",
        rows: "t.reporter_account_id = $2 OR t.target_account_id = $2 OR t.decided_by = $2",
        why: "as report.reporter_account_id",
    },
    Kept {
        table: "report",
        column: "target_id",
        rows: "t.reporter_account_id = $2 OR t.target_account_id = $2 OR t.decided_by = $2",
        why: "as report.reporter_account_id (may hold the name a reporter typed)",
    },
    Kept {
        table: "report",
        column: "decided_by",
        rows: "t.reporter_account_id = $2 OR t.target_account_id = $2 OR t.decided_by = $2",
        why: "as report.reporter_account_id",
    },
    Kept {
        table: "notice",
        column: "created_by",
        rows: "t.created_by = $2",
        why: "an operator's published notice",
    },
    Kept {
        table: "entitlement",
        column: "account_id",
        rows: "t.account_id = $2",
        why: "billing ledger: kept until monetization pseudonymises it at erasure (ADR 0303 §5, ADR 0305 §8; open, no account.deleted event any more)",
    },
    Kept {
        table: "entitlement_event",
        column: "account_id",
        rows: "t.account_id = $2",
        why: "as entitlement.account_id",
    },
    Kept {
        table: "ghost_blob",
        column: "uploaded_by",
        rows: "t.uploaded_by IN (SELECT id FROM profile WHERE account_id = $2)",
        why: "shared content-addressed ghost object other players' ghosts may name; released by the ghost garbage collection (open)",
    },
];

/// The erasure's account-scoped statements, in order: `(counter, SQL)`. Every statement starts
/// from `k` (`a` the account, `p` its profile ids, `x` their XUIDs) so all bind the same three
/// parameters. What names a profile is not here: after these, [`erase_in`] erases each profile
/// with [`PROFILE_ERASURE`] (`fi_erase_profile_data`, migration
/// `20261027200000_profile_erasure.sql`), the one list a player's profile deletion
/// ([`delete_profile_in`]) shares, and its counts are added to these counters.
pub const STEPS: &[(&str, &str)] = &[
    // Identity first: the tickets and the local credential are found through the mapping.
    (
        "identity_ticket",
        "DELETE FROM identity_ticket t USING k WHERE t.idp_uid IN \
           (SELECT i.idp_uid FROM account_identity i WHERE i.account_id = k.a)",
    ),
    (
        "credential_mail_token",
        "DELETE FROM credential_mail_token t USING k WHERE t.credential_id::text IN \
           (SELECT i.idp_uid FROM account_identity i WHERE i.account_id = k.a)",
    ),
    // As fi_maintenance, before the mapping that names it (security review M1, 2026-10-25): only
    // the credential its mapping names. Never by e-mail: an unmapped credential of the same
    // address may be another person's sign-up (review M1, 2026-10-26).
    (
        "credential",
        "DELETE FROM credential t USING k WHERE t.id::text IN \
           (SELECT i.idp_uid FROM account_identity i WHERE i.account_id = k.a)",
    ),
    (
        "account_identity",
        "DELETE FROM account_identity t USING k WHERE t.account_id = k.a",
    ),
    (
        "account_site_key",
        "DELETE FROM account_site_key t USING k WHERE t.account_id = k.a",
    ),
    (
        "friend_online_poll",
        "DELETE FROM friend_online_poll t USING k WHERE t.account_id = k.a",
    ),
    (
        "refresh_token",
        "DELETE FROM refresh_token t USING k WHERE t.account_id = k.a",
    ),
    (
        "mcp_refresh",
        "DELETE FROM mcp_refresh t USING k WHERE t.connection_id IN \
           (SELECT c.id FROM mcp_connection c WHERE c.account_id = k.a)",
    ),
    (
        "mcp_connection",
        "DELETE FROM mcp_connection t USING k WHERE t.account_id = k.a",
    ),
    (
        "mcp_request",
        "DELETE FROM mcp_request t USING k WHERE t.account_id = k.a",
    ),
    (
        "content_session_binding",
        "DELETE FROM content_session_binding t USING k WHERE t.owner = k.a",
    ),
    (
        "content_scan",
        "DELETE FROM content_scan t USING k WHERE t.owner = k.a",
    ),
    // Before the bindings lose their install ids (`device:<id>` scopes). Records naming a profile
    // go with the profile.
    (
        "idempotency_record",
        "DELETE FROM idempotency_record t USING k WHERE t.scope = 'account:' || k.a::text \
           OR t.scope IN (SELECT 'device:' || d.device_id::text FROM device_binding d \
             WHERE d.account_id = k.a) \
           OR t.resource_id = k.a",
    ),
    // The gate's per-account limits (`gate::rate_limit_key`); the others are peppered HMACs that
    // expire within the hour.
    (
        "rate_limit",
        "DELETE FROM rate_limit t USING k WHERE t.key LIKE \
           '%:' || encode(sha256(convert_to('gate:' || k.a::text, 'UTF8')), 'hex')",
    ),
    (
        "device_binding",
        "UPDATE device_binding t SET label = 'erased', fingerprint_hmac = NULL, \
           device_id = gen_random_uuid(), unbound_at = COALESCE(t.unbound_at, clock_timestamp()), \
           unbind_reason = COALESCE(t.unbind_reason, 'account_deleted') \
         FROM k WHERE t.account_id = k.a",
    ),
    (
        "account_friendship",
        "DELETE FROM account_friendship t USING k WHERE t.account_id = k.a OR t.friend_id = k.a",
    ),
    (
        "account_friend_request",
        "DELETE FROM account_friend_request t USING k WHERE t.from_account = k.a \
           OR t.to_account = k.a",
    ),
    (
        "world_invite",
        "DELETE FROM world_invite t USING k WHERE t.from_account_id = k.a \
           OR t.to_account_id = k.a \
           OR t.grant_id IN (SELECT g.id FROM mm_grant g WHERE g.account_id = k.a)",
    ),
    (
        "world_access",
        "DELETE FROM world_access t USING k WHERE t.account_id = k.a",
    ),
    // A leader reported for someone else's open world, its seat going below (closing a world clears
    // the owner and leader columns, so a closed one holds neither: nothing of them is in [`KEPT`]).
    (
        "world_leader",
        "UPDATE world t SET private_leader_xuid = NULL FROM k \
         WHERE t.private_leader_xuid IN (SELECT q.xuid FROM profile q WHERE q.account_id = k.a)",
    ),
    (
        "mm_grant",
        "DELETE FROM mm_grant t USING k WHERE t.account_id = k.a",
    ),
    (
        "livery_upload_block",
        "DELETE FROM livery_upload_block t USING k WHERE t.account_id = k.a",
    ),
    (
        "social_comment",
        "DELETE FROM social_comment t USING k WHERE t.author_account_id = k.a",
    ),
    // Its crash reports (participants, notifications cascade; reviews keep a NULL report), and
    // its participation in other players' reports.
    (
        "crash_participant",
        "DELETE FROM crash_participant t USING k WHERE t.account_id = k.a",
    ),
    (
        "crash_report",
        "DELETE FROM crash_report t USING k WHERE t.reporter = k.a",
    ),
    (
        "report_text",
        "UPDATE report t SET text = NULL FROM k \
         WHERE t.reporter_account_id = k.a AND t.text IS NOT NULL",
    ),
    // Rows an older image wrote to the outbox before this release stopped writing it (CR-S55: the
    // table stays until every running image is this one): its own events and any other account's
    // event whose envelope names its account (its profiles' and XUIDs' go with each profile). Goes
    // with the table's drop.
    (
        "outbox_event",
        "DELETE FROM outbox_event t USING k WHERE t.account_id = k.a \
           OR t.envelope::text LIKE '%' || k.a::text || '%'",
    ),
    // Its audit rows that carry an install id (the binding's is replaced above); the rest stays.
    (
        "account_audit",
        "DELETE FROM account_audit t USING k WHERE t.account_id = k.a AND t.device_id IS NOT NULL",
    ),
    // CR-S63: its Discord link codes, the feed rows about the account, and its name as the actor
    // of rows about other players (kept for #moderation-chat as "a moderator"); its profiles' rows
    // and announce switches go with each profile (`crate::discord::erase_profile_in`).
    (
        "discord_link_code",
        "DELETE FROM discord_link_code t USING k WHERE t.account_id = k.a",
    ),
    (
        "public_event",
        "DELETE FROM public_event t USING k WHERE t.subject_account = k.a",
    ),
    (
        "public_event_actor",
        "UPDATE public_event t SET actor_account = NULL, actor_profile = NULL, actor_name = NULL \
         FROM k WHERE t.actor_account = k.a",
    ),
    // CR-S68: the voice mutes it made or that named it (rows name accounts only).
    (
        "voice_mute_event",
        "DELETE FROM voice_mute_event t USING k WHERE t.muter_account_id = k.a OR t.target_account_id = k.a",
    ),
    // The tombstones: the name is freed (the trigger forgets the legacy name and the notice). Each
    // profile's rows go after these ([`PROFILE_ERASURE`] erases only a tombstone).
    (
        "profile",
        "UPDATE profile t SET status = 'deleted', name = NULL, deleted_at = clock_timestamp(), \
           social_listed = false \
         FROM k WHERE t.id = ANY (k.p) AND t.status <> 'deleted'",
    ),
    // CR-S98: the profiles above the careers, tombstones too (the gamertag freed).
    (
        "player_profile",
        "UPDATE player_profile t SET status = 'deleted', name = NULL, deleted_at = clock_timestamp() \
         FROM k WHERE t.account_id = k.a AND t.status <> 'deleted'",
    ),
    (
        "account",
        "UPDATE account t SET status = 'deleted', deleted_at = clock_timestamp(), locale = NULL, \
           time_zone = NULL, discord = NULL, ban_reason = NULL, \
           discord_user_id = NULL, discord_username = NULL, discord_global_name = NULL, \
           discord_avatar = NULL, discord_linked_at = NULL, discord_refreshed_at = NULL, \
           voice_banned_until = NULL, voice_community_until = NULL \
         FROM k WHERE t.id = k.a",
    ),
];

/// One profile's erasure, shared by the account erasure and a player's profile deletion: the
/// migrator-owned `fi_erase_profile_data(profile)` (migration
/// `20261027200000_profile_erasure.sql`). It erases only a tombstone with no queued or active play
/// session, no unexpired lease and no open world, and answers its rows per counter (the counters
/// of [`STEPS`] where a table has both an account part and a profile part).
pub const PROFILE_ERASURE: &str = "SELECT fi_erase_profile_data($1)";

/// What stays of one deleted profile while its account lives, besides what [`KEPT`] allows of the
/// account (`$2` the account): rows of the account's other profiles that name it, and (CR-S98) the
/// live profile above a deleted career, which keeps its id (the FH2 career's, for a profile made
/// before CR-S98), gamertag and XUID until `DELETE /players/{xuid}`.
pub const PROFILE_KEPT: &[Kept] = &[
    // Its id and XUID are KEPT's (the tombstone's rows hold them as the live profile's do).
    Kept {
        table: "player_profile",
        column: "name",
        rows: "t.account_id = $2 AND t.status <> 'deleted'",
        why: "the live profile above the deleted career: the gamertag stays the profile's (CAREERS.md §3 delete career)",
    },
    Kept {
        table: "save_revision",
        column: "seed_source",
        rows: "t.profile_id IN (SELECT id FROM profile WHERE account_id = $2 AND status <> 'deleted')",
        why: "another profile's line seeded from it (CR-S52 provenance, the foreign key holds the tombstone)",
    },
    Kept {
        table: "save_event",
        column: "detail",
        rows: "t.profile_id IN (SELECT id FROM profile WHERE account_id = $2 AND status <> 'deleted')",
        why: "as save_revision.seed_source (the seeded commit's event)",
    },
    Kept {
        table: "integrity_assessment",
        column: "seed_target",
        rows: "t.profile_id IN (SELECT id FROM profile WHERE account_id = $2 AND status <> 'deleted')",
        why: "a seed check recorded on another profile of the account (its save's history)",
    },
];

/// Erases one tombstoned profile's rows ([`PROFILE_ERASURE`]) and adds its counts to `rows`.
async fn erase_profile_rows_in(
    conn: &mut PgConnection,
    profile: Uuid,
    rows: &mut BTreeMap<String, u64>,
) -> Result<(), sqlx::Error> {
    let counts: serde_json::Value = sqlx::query_scalar(PROFILE_ERASURE)
        .bind(profile)
        .fetch_one(&mut *conn)
        .await?;
    if let Some(map) = counts.as_object() {
        for (k, v) in map {
            *rows.entry(k.clone()).or_default() += v.as_u64().unwrap_or(0);
        }
    }
    // CR-S63: its Discord feed rows and announce switch (the account's link is the account's).
    let feed = crate::discord::erase_profile_in(conn, profile).await?;
    *rows.entry("public_event".to_owned()).or_default() += feed.events;
    *rows.entry("public_event_actor".to_owned()).or_default() += feed.acted;
    *rows.entry("profile_feed".to_owned()).or_default() += feed.feed;
    // CR-S70: its Car Club (a silent leave: succession, the empty club deleted) and club rows.
    let clubs = crate::clubs::erase_profile_in(conn, profile).await?;
    for (k, n) in [
        ("game_club_member", clubs.member),
        ("game_club", clubs.club),
        ("game_club_invite", clubs.invites),
        ("game_club_request", clubs.requests),
        ("game_club_notice", clubs.notices),
        ("game_club_history", clubs.history),
        ("game_club_setting", clubs.setting),
    ] {
        *rows.entry(k.to_owned()).or_default() += n;
    }
    *rows.entry("game_notification".to_owned()).or_default() += clubs.notifications;
    // OD-147: the peer observations it made or that observed it, its quorums and its flag.
    let seen = crate::observations::erase_profile_in(conn, profile).await?;
    for (k, n) in [
        ("peer_observation", seen.observations),
        ("cheat_quorum", seen.quorums),
        ("cheat_flag", seen.flags),
    ] {
        *rows.entry(k.to_owned()).or_default() += n;
    }
    // §5d (CR-S80): the relay's reports about it, as the measured player or as the host.
    let relayed = crate::relay_reports::erase_profile_in(conn, profile).await?;
    *rows.entry("relay_report".to_owned()).or_default() += relayed;
    // CR-S74: its Storefront files, versions and libraries, and the marks and follows it made.
    let sf = crate::storefront::erase_profile_in(conn, profile).await?;
    for (k, n) in [
        ("storefront_file", sf.files),
        ("storefront_mark", sf.marks),
        ("storefront_follow", sf.follows),
        ("game_layer_group", sf.groups),
        ("game_tune", sf.tunes),
        ("game_layer_group_draft", sf.group_drafts),
        ("game_car_setup", sf.setups),
    ] {
        *rows.entry(k.to_owned()).or_default() += n;
    }
    Ok(())
}

/// How [`run`] treats identities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// A `local` instance (`FI_AUTH_BACKEND=local`): every identity mapping is the instance's own
    /// and is erased. Otherwise (Identity Platform) an account whose mapping names no local
    /// `credential` waits ([`Outcome::IdpPending`]) until its IdP user is deleted.
    pub local_identity: bool,
}

/// What [`erase_in`] did with one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Erased: rows per counter of [`STEPS`] (zeros included). The caller commits.
    Erased(BTreeMap<String, u64>),
    /// Not (or no longer) due: cancelled, already erased, still in grace, unknown. Nothing written.
    NotDue,
    /// In use or locked: retried by the next run. Nothing written.
    Busy,
    /// Its Identity Platform user must be deleted first. Nothing written.
    IdpPending,
}

/// What one [`run`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Erased {
    /// Accounts erased.
    pub accounts: u64,
    /// Their profiles.
    pub profiles: u64,
    /// Accounts left for the next run because they were in use.
    pub busy: u64,
    /// Accounts waiting for their Identity Platform user's deletion.
    pub idp_pending: u64,
    /// Accounts whose transaction failed (logged; retried by the next run).
    pub failed: u64,
    /// Rows per counter of [`STEPS`], over every erased account.
    pub rows: BTreeMap<String, u64>,
}

/// Why an erasure failed.
#[derive(Debug, thiserror::Error)]
pub enum ErasureError {
    /// A database failure.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// Erases every account past its grace, one per transaction; see the module documentation.
pub async fn run(db: &PgPool, options: Options) -> Result<Erased, ErasureError> {
    let mut out = Erased::default();
    let mut after: Option<(OffsetDateTime, Uuid)> = None;
    loop {
        let page: Vec<(OffsetDateTime, Uuid)> = sqlx::query_as(
            "SELECT deletion_due_at, id FROM account \
             WHERE status = 'deletion_pending' AND deletion_due_at <= clock_timestamp() \
               AND ($1::timestamptz IS NULL OR (deletion_due_at, id) > ($1, $2)) \
             ORDER BY deletion_due_at, id LIMIT $3",
        )
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1).unwrap_or_default())
        .bind(BATCH)
        .fetch_all(db)
        .await?;
        let Some(last) = page.last() else { break };
        after = Some(*last);
        for (_, account) in &page {
            let mut tx = db.begin().await?;
            match erase_in(&mut tx, *account, options).await {
                Ok(Outcome::Erased(rows)) => match tx.commit().await {
                    Ok(()) => {
                        out.accounts += 1;
                        out.profiles += rows.get("profile").copied().unwrap_or(0);
                        for (k, v) in rows {
                            *out.rows.entry(k).or_default() += v;
                        }
                        tracing::info!(%account, "account erased");
                    }
                    Err(e) => {
                        out.failed += 1;
                        tracing::warn!(%account, error = %e, "account erasure failed to commit");
                    }
                },
                Ok(Outcome::Busy) => out.busy += 1,
                Ok(Outcome::IdpPending) => {
                    out.idp_pending += 1;
                    tracing::warn!(%account, "account erasure waits for its Identity Platform user");
                }
                Ok(Outcome::NotDue) => {}
                Err(e) => {
                    out.failed += 1;
                    tracing::warn!(%account, error = %e, "account erasure failed");
                }
            }
        }
        if (page.len() as i64) < BATCH {
            break;
        }
    }
    Ok(out)
}

/// Erases `account` in the caller's transaction when it is due and idle; the caller commits only
/// [`Outcome::Erased`] (every other outcome wrote nothing).
pub async fn erase_in(
    conn: &mut PgConnection,
    account: Uuid,
    options: Options,
) -> Result<Outcome, ErasureError> {
    // 1. The account, under its lock, still due.
    let row = sqlx::query(
        "SELECT status = 'deletion_pending' AND deletion_due_at <= clock_timestamp() AS due \
           FROM account WHERE id = $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM account WHERE id = $1)")
                .bind(account)
                .fetch_one(&mut *conn)
                .await?;
        return Ok(if exists {
            Outcome::Busy
        } else {
            Outcome::NotDue
        });
    };
    if !row.try_get::<bool, _>("due")? {
        return Ok(Outcome::NotDue);
    }

    // Its profiles (all of them, tombstones included, so a partial earlier state is completed).
    let profiles: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, xuid FROM profile WHERE account_id = $1 ORDER BY id FOR NO KEY UPDATE",
    )
    .bind(account)
    .fetch_all(&mut *conn)
    .await?;
    let ids: Vec<Uuid> = profiles.iter().map(|p| p.0).collect();
    let xuids: Vec<i64> = profiles.iter().map(|p| p.1).collect();

    // 2. Busy: a live session or ticket, a held or leased head, an open world.
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM play_session WHERE account_id = $1 \
           AND (state IN ('queued', 'active') \
             OR ticket_exp > clock_timestamp() - make_interval(secs => $2)))",
    )
    .bind(account)
    .bind(TICKET_TAIL_SECS)
    .fetch_one(&mut *conn)
    .await?;
    if live {
        return Ok(Outcome::Busy);
    }
    let heads: i64 =
        sqlx::query_scalar("SELECT count(*) FROM save_head WHERE profile_id = ANY ($1)")
            .bind(&ids)
            .fetch_one(&mut *conn)
            .await?;
    let locked: Vec<bool> = sqlx::query_scalar(
        "SELECT COALESCE(lease_expires_at > clock_timestamp(), false) FROM save_head \
         WHERE profile_id = ANY ($1) FOR UPDATE SKIP LOCKED",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    if locked.len() as i64 != heads || locked.iter().any(|leased| *leased) {
        return Ok(Outcome::Busy);
    }
    let open: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM world WHERE (host_account_id = $1 OR official_owner_account_id = $1) \
           AND closed_at IS NULL)",
    )
    .bind(account)
    .fetch_one(&mut *conn)
    .await?;
    if open {
        return Ok(Outcome::Busy);
    }

    // 3. An Identity Platform user this job cannot delete.
    if !options.local_identity {
        let external: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM account_identity i WHERE i.account_id = $1 \
               AND NOT EXISTS (SELECT 1 FROM credential c WHERE c.id::text = i.idp_uid))",
        )
        .bind(account)
        .fetch_one(&mut *conn)
        .await?;
        if external {
            return Ok(Outcome::IdpPending);
        }
    }

    // 4. The steps.
    let mut rows = BTreeMap::new();
    for (counter, sql) in STEPS {
        let sql =
            format!("WITH k AS (SELECT $1::uuid AS a, $2::uuid[] AS p, $3::bigint[] AS x) {sql}");
        let n = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(account)
            .bind(&ids)
            .bind(&xuids)
            .execute(&mut *conn)
            .await?
            .rows_affected();
        rows.insert((*counter).to_owned(), n);
    }
    for profile in &ids {
        erase_profile_rows_in(conn, *profile, &mut rows).await?;
    }

    // 5. The audit row.
    let detail = serde_json::json!({ "rows": rows });
    sqlx::query(
        "INSERT INTO account_audit (account_id, kind, actor, detail) \
         VALUES ($1, 'account.erased', 'system', $2)",
    )
    .bind(account)
    .bind(detail)
    .execute(&mut *conn)
    .await?;
    Ok(Outcome::Erased(rows))
}

/// Profiles an account may delete per rolling hour (owner 2026-10-27: "The user can only delete 3
/// profiles every hour. To avoid abuse.").
pub const PROFILE_DELETIONS_PER_HOUR: i64 = 3;

/// The audit kind of a player's profile deletion; the rolling-hour limit counts these rows.
pub const PROFILE_DELETED: &str = "profile.deleted";

/// A player's profile deletion, done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileDeleted {
    /// The profile, now a tombstone.
    pub profile_id: Uuid,
    /// Rows per counter of [`PROFILE_ERASURE`] (zeros included).
    pub rows: BTreeMap<String, u64>,
}

/// Why a player's profile deletion is refused (ACCOUNTS.md §5 `DELETE /profiles/{xuid}`). Every
/// refusal wrote nothing.
#[derive(Debug, thiserror::Error)]
pub enum ProfileDeletionError {
    /// No such live profile of this account (`ACCOUNTS.NOT_FOUND`).
    #[error("not found")]
    NotFound,
    /// The account or the profile is pending deletion (`ACCOUNTS.DELETION_PENDING`).
    #[error("pending deletion")]
    DeletionPending,
    /// The account is banned, a block pending review included (`ACCOUNTS.BANNED {review?}`): the
    /// moderator's evidence stays until the ban is lifted.
    #[error("banned")]
    Banned {
        /// `account.ban_review` while the ban is in force (CR-S52 `seed_from_profile`).
        review: Option<String>,
    },
    /// [`PROFILE_DELETIONS_PER_HOUR`] deletions in the last hour (`REQUEST.RATE_LIMITED`).
    #[error("rate limited")]
    RateLimited {
        /// Seconds until the oldest of the hour's deletions leaves the window (at least 1).
        retry_after_secs: u64,
    },
    /// A queued or active play session, or a live save lease (`SESSIONS.PROFILE_IN_USE`).
    #[error("profile in use")]
    InUse,
    /// A game access ticket of the profile may still be served until `until`
    /// (`SESSIONS.PROFILE_IN_USE` with `details.ticketExpiresAt`).
    #[error("a game ticket of the profile is live")]
    TicketLive {
        /// The earlier of the ticket's exp and the liveness cache's window end.
        until: fi_domain::UnixMs,
    },
    /// CR-S98 (`DELETE /players/{xuid}`): one career of `game` refused the deletion
    /// ([`Self::InUse`] or [`Self::TicketLive`]), so nothing was deleted (CAREERS.md §6.5).
    #[error("a career of {game} is in use")]
    Career {
        /// The career's game.
        game: String,
        /// Its refusal.
        error: Box<ProfileDeletionError>,
    },
    /// Database trouble.
    #[error(transparent)]
    Db(#[from] crate::identity::IdentityError),
}

impl From<sqlx::Error> for ProfileDeletionError {
    fn from(e: sqlx::Error) -> Self {
        ProfileDeletionError::Db(e.into())
    }
}

/// Deletes the account's live career of `game` of the profile `xuid` from the cloud at once (owner
/// 2026-10-27; ACCOUNTS.md §5 `DELETE /profiles/{xuid}` and §8 "Profile deletion"; CR-S98: a career,
/// CAREERS.md §3 "delete career": the profile, its gamertag and XUID stay), in the caller's
/// transaction, which the caller commits only on `Ok`:
/// 1. the account row (`FOR NO KEY UPDATE`), then the profile row, then its save head, the lock
///    order of `purge::change_mode_in`; refused when the account is gone or the profile is not its
///    own live one (404), pending deletion, or banned (a block pending review included: deleting
///    would destroy what the moderator reviews);
/// 2. at most [`PROFILE_DELETIONS_PER_HOUR`] per rolling hour, counted from the account's
///    [`PROFILE_DELETED`] audit rows under the account's lock (only deletions that committed count);
/// 3. not while in use (`purge::lock_idle_line_in`, the mode change's rules);
/// 4. its `expired` sessions closed (no heartbeat resumes one), its seats released and open worlds
///    closed, the tombstone (`status = deleted`, the name freed), [`PROFILE_ERASURE`], and one
///    `account_audit` row [`PROFILE_DELETED`] (actor `player`, the profile id and the counts).
///
/// The account stays, with its other profiles or none.
pub async fn delete_profile_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    game: &str,
) -> Result<ProfileDeleted, ProfileDeletionError> {
    lock_deleting_account_in(conn, account_id).await?;
    let xuid = i64::try_from(xuid).map_err(|_| ProfileDeletionError::NotFound)?;
    // The lock order of `delete_player_in`: the profile above, then the career.
    let parent: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM player_profile \
         WHERE account_id = $1 AND xuid = $2 AND status <> 'deleted' FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .bind(xuid)
    .fetch_optional(&mut *conn)
    .await?;
    let parent = parent.ok_or(ProfileDeletionError::NotFound)?;
    let profile = sqlx::query(
        "SELECT id, game, status FROM profile \
         WHERE player_profile_id = $1 AND game = $2 AND status <> 'deleted' FOR NO KEY UPDATE",
    )
    .bind(parent)
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ProfileDeletionError::NotFound)?;
    let profile_id: Uuid = profile.try_get("id")?;
    let game: String = profile.try_get("game")?;
    if profile.try_get::<String, _>("status")? != "active" {
        return Err(ProfileDeletionError::DeletionPending);
    }
    require_deletion_allowance_in(conn, account_id).await?;
    let mut rows = BTreeMap::new();
    erase_career_in(conn, profile_id, &game, xuid, &mut rows).await?;
    // CR-S98 review (HIGH 1): the profile's last live career takes the profile with it, as a
    // released launcher's deletion always did: the gamertag and the mode's slot are free again.
    let others: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM profile WHERE player_profile_id = $1 AND status <> 'deleted')",
    )
    .bind(parent)
    .fetch_one(&mut *conn)
    .await?;
    if !others {
        tombstone_player_in(conn, parent).await?;
    }
    sqlx::query(
        "INSERT INTO account_audit (account_id, kind, actor, detail) VALUES ($1, $2, 'player', $3)",
    )
    .bind(account_id)
    .bind(PROFILE_DELETED)
    .bind(serde_json::json!({ "profileId": profile_id, "rows": rows }))
    .execute(&mut *conn)
    .await?;
    Ok(ProfileDeleted { profile_id, rows })
}

/// Deletes the account's live profile `xuid` above its careers (CR-S98, CAREERS.md §3 `DELETE
/// /players/{xuid}`), in the caller's transaction, which the caller commits only on `Ok`: the
/// refusals of [`delete_profile_in`] (account, ban, pending deletion, the rolling hour, any career
/// in use), then every live career's deletion as [`delete_profile_in`] does it, then the profile's
/// tombstone (`status = deleted`, the gamertag freed; its id and XUID stay, never reused). One
/// [`PROFILE_DELETED`] audit row (the profile id, its careers' count and their rows) counts once in
/// the rolling hour, as a career's deletion does. `profile_id` of the answer is the profile's.
pub async fn delete_player_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
) -> Result<ProfileDeleted, ProfileDeletionError> {
    lock_deleting_account_in(conn, account_id).await?;
    let xuid = i64::try_from(xuid).map_err(|_| ProfileDeletionError::NotFound)?;
    let parent = sqlx::query(
        "SELECT id, status FROM player_profile \
         WHERE account_id = $1 AND xuid = $2 AND status <> 'deleted' FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .bind(xuid)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ProfileDeletionError::NotFound)?;
    let parent_id: Uuid = parent.try_get("id")?;
    if parent.try_get::<String, _>("status")? != "active" {
        return Err(ProfileDeletionError::DeletionPending);
    }
    let careers: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, game, status FROM profile WHERE player_profile_id = $1 AND status <> 'deleted' \
         ORDER BY created_at, id FOR NO KEY UPDATE",
    )
    .bind(parent_id)
    .fetch_all(&mut *conn)
    .await?;
    if careers.iter().any(|(_, _, status)| status != "active") {
        return Err(ProfileDeletionError::DeletionPending);
    }
    require_deletion_allowance_in(conn, account_id).await?;
    let mut rows = BTreeMap::new();
    for (career, game, _) in &careers {
        erase_career_in(conn, *career, game, xuid, &mut rows)
            .await
            .map_err(|e| match e {
                ProfileDeletionError::InUse | ProfileDeletionError::TicketLive { .. } => {
                    ProfileDeletionError::Career {
                        game: game.clone(),
                        error: Box::new(e),
                    }
                }
                other => other,
            })?;
    }
    tombstone_player_in(conn, parent_id).await?;
    sqlx::query(
        "INSERT INTO account_audit (account_id, kind, actor, detail) VALUES ($1, $2, 'player', $3)",
    )
    .bind(account_id)
    .bind(PROFILE_DELETED)
    .bind(serde_json::json!({ "profileId": parent_id, "careers": careers.len(), "rows": rows }))
    .execute(&mut *conn)
    .await?;
    Ok(ProfileDeleted {
        profile_id: parent_id,
        rows,
    })
}

/// The profile's tombstone once no career of it lives (CR-S98): `status = deleted`, the gamertag
/// freed; its id and XUID stay, never reused.
async fn tombstone_player_in(conn: &mut PgConnection, parent: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE player_profile SET status = 'deleted', name = NULL, deleted_at = clock_timestamp() \
         WHERE id = $1 AND status <> 'deleted'",
    )
    .bind(parent)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Step 1 of a player's deletion: the account row (`FOR NO KEY UPDATE`), refused when it is gone,
/// pending deletion or banned.
async fn lock_deleting_account_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(), ProfileDeletionError> {
    let account = sqlx::query(
        "SELECT status, (banned_at IS NOT NULL AND (banned_until IS NULL OR banned_until > clock_timestamp())) AS banned, \
           ban_review FROM account WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ProfileDeletionError::NotFound)?;
    match account.try_get::<String, _>("status")?.as_str() {
        "deleted" => return Err(ProfileDeletionError::NotFound),
        "deletion_pending" => return Err(ProfileDeletionError::DeletionPending),
        _ => {}
    }
    if account.try_get::<bool, _>("banned")? {
        return Err(ProfileDeletionError::Banned {
            review: account.try_get("ban_review")?,
        });
    }
    Ok(())
}

/// Step 2: the rolling hour; the third newest deletion of the hour leaves it first.
async fn require_deletion_allowance_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(), ProfileDeletionError> {
    let third: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM occurred_at + interval '1 hour' - clock_timestamp())::float8 \
         FROM account_audit WHERE account_id = $1 AND kind = $2 AND actor = 'player' \
           AND occurred_at > clock_timestamp() - interval '1 hour' \
         ORDER BY occurred_at DESC OFFSET $3 LIMIT 1",
    )
    .bind(account_id)
    .bind(PROFILE_DELETED)
    .bind(PROFILE_DELETIONS_PER_HOUR - 1)
    .fetch_optional(&mut *conn)
    .await?;
    match third {
        Some(wait) => Err(ProfileDeletionError::RateLimited {
            retry_after_secs: wait.ceil().max(1.0) as u64,
        }),
        None => Ok(()),
    }
}

/// Steps 3 and 4 for one career (`profile` row) of `game`: refused while in use, then its expired
/// sessions closed, seats released, its private-world leadership of `game` cleared, the tombstone
/// and [`PROFILE_ERASURE`] (counts added to `rows`).
async fn erase_career_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    game: &str,
    xuid: i64,
    rows: &mut BTreeMap<String, u64>,
) -> Result<(), ProfileDeletionError> {
    use crate::purge::{InUse, lock_idle_line_in};
    match lock_idle_line_in(conn, profile_id).await? {
        Ok(_) => {}
        Err(InUse::Session) => return Err(ProfileDeletionError::InUse),
        Err(InUse::TicketLive { until }) => return Err(ProfileDeletionError::TicketLive { until }),
    }
    // CR-S96: the owner of an open private world waits for it to close (friends may be playing).
    let owns_world: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM world WHERE official_owner_profile_id = $1 AND closed_at IS NULL)",
    )
    .bind(profile_id)
    .fetch_one(&mut *conn)
    .await?;
    if owns_world {
        return Err(ProfileDeletionError::InUse);
    }
    // An expired session's heartbeat would re-admit it: closed now, its end kept.
    sqlx::query(
        "UPDATE play_session SET state = 'ended' WHERE profile_id = $1 AND state = 'expired'",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    crate::matchmaking::release_profile_in(conn, game, profile_id).await?;
    // CR-S98: the XUID is also the profile's other careers'; only this game's worlds forget it.
    sqlx::query(
        "UPDATE world SET private_leader_xuid = NULL WHERE private_leader_xuid = $1 AND game = $2",
    )
    .bind(xuid)
    .bind(game)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE profile SET status = 'deleted', name = NULL, deleted_at = clock_timestamp(), \
           social_listed = false WHERE id = $1",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    erase_profile_rows_in(conn, profile_id, rows).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_names_its_table_and_kept_has_no_duplicates() {
        for (counter, sql) in STEPS {
            let table = counter
                .strip_suffix("_lease")
                .or_else(|| counter.strip_suffix("_peer"))
                .or_else(|| counter.strip_suffix("_text"))
                .or_else(|| counter.strip_suffix("_actor"))
                .or_else(|| counter.strip_suffix("_leader"))
                .unwrap_or(counter);
            assert!(sql.contains(&format!(" {table} t ")), "{counter}: {sql}");
            assert!(sql.contains("k"), "{counter}");
        }
        // The credential goes before the mapping that names it (security review M1).
        let at = |c: &str| STEPS.iter().position(|(n, _)| *n == c).unwrap();
        assert!(at("credential") < at("account_identity"));
        assert!(at("credential_mail_token") < at("credential"));
        assert!(at("world_invite") < at("mm_grant"));
        assert!(at("idempotency_record") < at("device_binding"));
        // The profiles are tombstones before PROFILE_ERASURE runs (it erases only a tombstone);
        // nothing of STEPS names a profile any more (that is the function's).
        assert_eq!(STEPS.last().map(|s| s.0), Some("account"));
        for (counter, sql) in STEPS {
            if *counter != "profile" {
                assert!(!sql.contains("k.p") && !sql.contains("k.x"), "{counter}");
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for k in KEPT.iter().chain(PROFILE_KEPT) {
            assert!(seen.insert((k.table, k.column)), "{}.{}", k.table, k.column);
            assert!(!k.why.is_empty() && k.rows.contains("$2"));
        }
    }
}
