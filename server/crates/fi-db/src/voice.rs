//! Voice moderation (CORE_API.md §4.4, §5; register CR-S68, OD-154, OD-163): the overlay's mutes
//! sent by the launcher (`voice_mute_event`), the community mute (3 distinct muter accounts within
//! 7 days → 3 hours), the moderators' voice bans, and the block in force that reaches the relays
//! (`RelayGrantClaims.voiceBlockedUntil`, `WorldMember.voiceBlockedUntil`; MATCHMAKING_API.md
//! §3.2, §3.7) and the player (`voice_blocked` notice).
//!
//! Everything is per **account**: an account mutes another account (whichever profiles), and an
//! account is blocked. Rows carry ids only; nothing here is shown to the muted player but the end
//! of its block. Every function runs in the caller's transaction or connection.

use crate::identity::{IdentityError, ms};
use crate::limits::Limit;
use fi_domain::UnixMs;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Mutes per account (`POST /launcher/v1/voice/mutes`): 60 an hour, counted for every request past
/// the field checks, keyed by the HMAC of `account:<id>` (429 `VOICE.RATE_LIMITED`).
pub const VOICE_MUTES: Limit = Limit {
    name: "voice.mute",
    max: 60,
    window_s: 3_600,
};
/// A mute needs the world shared within this many seconds (30 minutes; §4.3's rule).
pub const SHARED_WITHIN_SECS: i64 = crate::moderation::SHARED_WITHIN_SECS;
/// Distinct muter accounts that start a community mute.
pub const COMMUNITY_MUTERS: i64 = 3;
/// Only mutes made within this many seconds count (a rolling 7 days).
pub const COMMUNITY_WINDOW_SECS: i64 = 7 * 86_400;
/// A community mute lasts this many seconds (3 hours).
pub const COMMUNITY_BLOCK_SECS: i64 = 3 * 3_600;
/// The longest moderator voice ban, in minutes (30 days).
pub const BAN_MAX_MINUTES: u32 = 43_200;
/// Mute events are kept this many days (`fi_db::retention`).
pub const MUTE_EVENT_DAYS: i32 = 30;

/// The SQL of an account's voice block in force, as a scalar subquery over `account_expr` (a
/// column or a parameter): the later of `voice_banned_until` and `voice_community_until` when it
/// is in the future, else NULL.
pub fn blocked_until_sql(account_expr: &str) -> String {
    format!(
        "(SELECT CASE WHEN greatest(va.voice_banned_until, va.voice_community_until) > clock_timestamp() \
           THEN greatest(va.voice_banned_until, va.voice_community_until) END \
         FROM account va WHERE va.id = {account_expr})"
    )
}

/// The account's voice block in force (`None`: none).
pub async fn blocked_until_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Option<UnixMs>, sqlx::Error> {
    let sql = format!("SELECT {}", blocked_until_sql("$1"));
    let until: Option<OffsetDateTime> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(account)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    Ok(until.map(ms))
}

/// A mute or unmute from the launcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mute<'a> {
    /// The muting account (the launcher session's).
    pub muter: Uuid,
    /// The target profile's XUID (in the world's game).
    pub xuid: u64,
    /// `muted` | `unmuted` (validated by the service).
    pub action: &'a str,
    /// The world the overlay names.
    pub world_id: Uuid,
}

/// What [`mute_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Muted {
    /// No such world, no live profile of its game with that XUID, or (a `muted`) the two were not
    /// in that world together within [`SHARED_WITHIN_SECS`]: `VOICE.NOT_SHARED`, nothing written.
    NotShared,
    /// A profile of the muter's own account: `REQUEST.INVALID {field: "xuid"}`.
    Own,
    /// The muter's current state toward the target's account already is the action: nothing
    /// written.
    Unchanged,
    /// Recorded; `community` is the end of the community mute this mute started, if it did.
    Changed {
        /// The target account's new community mute's end.
        community: Option<UnixMs>,
    },
}

/// Applies a mute or an unmute (CORE_API.md §4.4), the first that applies: an unknown world or
/// target; one's own; a repeat of the current state (none = `unmuted`; withdrawing a held mute is
/// accepted however long ago the two met); a `muted` without the world shared in the last 30
/// minutes; else the `voice_mute_event` row and, for a `muted`, [`community_in`]. The target
/// account's row is locked first, so mutes of one target are decided one at a time.
pub async fn mute_in(conn: &mut PgConnection, m: &Mute<'_>) -> Result<Muted, IdentityError> {
    let Ok(xuid) = i64::try_from(m.xuid) else {
        return Ok(Muted::NotShared);
    };
    let game: Option<String> = sqlx::query_scalar("SELECT game FROM world WHERE id = $1")
        .bind(m.world_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(game) = game else {
        return Ok(Muted::NotShared);
    };
    // A withdrawal still finds a profile its account deleted since (the tombstone keeps its XUID
    // and account), so a mute is never stuck; a mute needs a live profile.
    let target: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, account_id FROM profile WHERE game = $1 AND xuid = $2 \
           AND (status <> 'deleted' OR $3) ORDER BY (status = 'deleted'), created_at DESC LIMIT 1",
    )
    .bind(&game)
    .bind(xuid)
    .bind(m.action == "unmuted")
    .fetch_optional(&mut *conn)
    .await?;
    let Some((profile, account)) = target else {
        return Ok(Muted::NotShared);
    };
    if account == m.muter {
        return Ok(Muted::Own);
    }
    // NO KEY UPDATE: mutes of one target wait for each other, yet two players muting each other at
    // once do not deadlock on the foreign keys' KEY SHARE locks (nor stall matchmaking's).
    sqlx::query("SELECT 1 FROM account WHERE id = $1 FOR NO KEY UPDATE")
        .bind(account)
        .execute(&mut *conn)
        .await?;
    let current: Option<String> = sqlx::query_scalar(
        "SELECT action FROM voice_mute_event WHERE muter_account_id = $1 AND target_account_id = $2 \
         ORDER BY at DESC, id DESC LIMIT 1",
    )
    .bind(m.muter)
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?;
    if current.as_deref().unwrap_or("unmuted") == m.action {
        return Ok(Muted::Unchanged);
    }
    if m.action == "muted" {
        let shared: bool = sqlx::query_scalar(crate::moderation::SHARED_WORLD_SQL)
            .bind(&game)
            .bind(profile)
            .bind(m.muter)
            .bind(Some(m.world_id))
            .bind(SHARED_WITHIN_SECS as f64)
            .fetch_one(&mut *conn)
            .await?;
        if !shared {
            return Ok(Muted::NotShared);
        }
    }
    sqlx::query(
        "INSERT INTO voice_mute_event (muter_account_id, target_account_id, action, world_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(m.muter)
    .bind(account)
    .bind(m.action)
    .bind(m.world_id)
    .execute(&mut *conn)
    .await?;
    let community = if m.action == "muted" {
        community_in(conn, account).await?
    } else {
        None
    };
    Ok(Muted::Changed { community })
}

/// The community rule on the (locked) target account: when no community mute is in force and at
/// least [`COMMUNITY_MUTERS`] distinct accounts currently mute it (their latest event toward it is
/// `muted`), each mute made within [`COMMUNITY_WINDOW_SECS`] and after the end of its previous
/// community mute, each muter account with an admitted non-dedicated play session, the account is
/// blocked for [`COMMUNITY_BLOCK_SECS`] and a `voice_community_muted` staff row is written.
/// Returns the new block's end.
pub async fn community_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Option<UnixMs>, IdentityError> {
    let until: Option<OffsetDateTime> = sqlx::query_scalar(
        "WITH a AS (SELECT voice_community_until AS cu FROM account WHERE id = $1), \
         latest AS ( \
           SELECT DISTINCT ON (e.muter_account_id) e.muter_account_id AS muter, e.action, e.at \
             FROM voice_mute_event e \
            WHERE e.target_account_id = $1 AND e.at > clock_timestamp() - make_interval(secs => $2) \
            ORDER BY e.muter_account_id, e.at DESC, e.id DESC), \
         counted AS ( \
           SELECT count(*) AS n FROM latest l, a \
            WHERE l.action = 'muted' AND l.at > coalesce(a.cu, '-infinity'::timestamptz) \
              AND EXISTS (SELECT 1 FROM play_session s WHERE s.account_id = l.muter \
                AND s.admitted_at IS NOT NULL AND NOT s.dedicated)) \
         UPDATE account SET voice_community_until = clock_timestamp() + make_interval(secs => $3) \
           FROM a, counted \
          WHERE account.id = $1 AND counted.n >= $4 \
            AND (a.cu IS NULL OR a.cu <= clock_timestamp()) \
         RETURNING account.voice_community_until",
    )
    .bind(account)
    .bind(COMMUNITY_WINDOW_SECS as f64)
    .bind(COMMUNITY_BLOCK_SECS as f64)
    .bind(COMMUNITY_MUTERS)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(until) = until.map(ms) else {
        return Ok(None);
    };
    crate::discord::staff_event_in(
        conn,
        crate::discord::StaffKind::VoiceCommunityMuted,
        account,
        None,
        None,
        Some(until),
    )
    .await?;
    Ok(Some(until))
}

/// A moderator's voice ban (CORE_API.md §5): `voice_banned_until = now + minutes`, replacing an
/// earlier ban (a community mute in force is untouched). Run after `moderation::kick_target_in`
/// locked the account and the caller checked the roles. Returns the ban's end.
pub async fn ban_in(
    conn: &mut PgConnection,
    account: Uuid,
    minutes: u32,
) -> Result<UnixMs, IdentityError> {
    let until: OffsetDateTime = sqlx::query_scalar(
        "UPDATE account SET voice_banned_until = clock_timestamp() + make_interval(mins => $2) \
         WHERE id = $1 RETURNING voice_banned_until",
    )
    .bind(account)
    .bind(minutes.clamp(1, BAN_MAX_MINUTES) as i32)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(IdentityError::NotFound)?;
    Ok(ms(until))
}

/// What a lift ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lifted {
    /// A moderator voice ban was in force.
    pub banned: bool,
    /// A community mute was in force.
    pub community: bool,
}

/// Lifts a moderator voice ban and a community mute in force (CORE_API.md §5): the ban is cleared;
/// a community mute in force ends now, so only mutes newer than the lift count. Run after
/// `moderation::kick_target_in` locked the account.
pub async fn lift_in(conn: &mut PgConnection, account: Uuid) -> Result<Lifted, IdentityError> {
    let row = sqlx::query(
        "WITH old AS (SELECT voice_banned_until AS b, voice_community_until AS c FROM account \
           WHERE id = $1 FOR UPDATE) \
         UPDATE account SET voice_banned_until = NULL, \
           voice_community_until = CASE WHEN old.c > clock_timestamp() THEN clock_timestamp() ELSE old.c END \
         FROM old WHERE account.id = $1 \
         RETURNING coalesce(old.b > clock_timestamp(), false) AS banned, \
           coalesce(old.c > clock_timestamp(), false) AS community",
    )
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(IdentityError::NotFound)?;
    Ok(Lifted {
        banned: row.try_get("banned")?,
        community: row.try_get("community")?,
    })
}

/// An account's voice state for the moderators: each block's end while it is in force.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceState {
    /// A moderator voice ban in force.
    pub banned_until: Option<UnixMs>,
    /// A community mute in force.
    pub community_until: Option<UnixMs>,
}

/// The account's voice blocks in force.
pub async fn state_in(conn: &mut PgConnection, account: Uuid) -> Result<VoiceState, sqlx::Error> {
    let row: Option<(Option<OffsetDateTime>, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT CASE WHEN voice_banned_until > clock_timestamp() THEN voice_banned_until END, \
           CASE WHEN voice_community_until > clock_timestamp() THEN voice_community_until END \
         FROM account WHERE id = $1",
    )
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?;
    let (b, c) = row.unwrap_or((None, None));
    Ok(VoiceState {
        banned_until: b.map(ms),
        community_until: c.map(ms),
    })
}

/// One mute event about an account, as the moderators see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MuteRow {
    /// Who muted.
    pub muter_account_id: Uuid,
    /// `muted` | `unmuted`.
    pub action: String,
    /// Which world.
    pub world_id: Uuid,
    /// Its game.
    pub game: String,
    /// When.
    pub at: UnixMs,
}

/// The newest `limit` mute events about the account (moderators only).
pub async fn mutes_about_in(
    conn: &mut PgConnection,
    account: Uuid,
    limit: i64,
) -> Result<Vec<MuteRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT e.muter_account_id, e.action, e.world_id, w.game, e.at FROM voice_mute_event e \
           JOIN world w ON w.id = e.world_id \
         WHERE e.target_account_id = $1 ORDER BY e.at DESC, e.id DESC LIMIT $2",
    )
    .bind(account)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(MuteRow {
                muter_account_id: r.try_get("muter_account_id")?,
                action: r.try_get("action")?,
                world_id: r.try_get("world_id")?,
                game: r.try_get("game")?,
                at: ms(r.try_get("at")?),
            })
        })
        .collect()
}
