//! Cloud eligibility on the save line (OD-52, register CR-S20; SAVES_API.md §4.7, GATE_API.md §2
//! and §4): who may hold a save lease and commit, and the renewal mark ("the cloud wins").
//!
//! - The rule (OD-239, CR-S111): a Hardcore profile of a game with a line keeps one, whatever
//!   the tier ([`fi_domain::GameRules::line_mode`]); a Noob (Azimuth) profile never does.
//!   Admission ([`crate::admission`]) grants the lease only on it, and drops a lease whose session
//!   stops meeting it; the save line's commit route checks it again ([`refusal_in`]).
//! - The line's coverage (`save_head.covered_until`, `covered_at`): the account's `access_until`
//!   recorded at every lease grant and renewal ([`cover_in`]).
//! - The renewal mark (`save_head.lapsed_at`): set when a grant finds the coverage the line last
//!   saw already past ([`cover_in`] with `grant`), and when the server meets a Hardcore profile
//!   whose account is not a Supporter ([`mark_lapsed_in`]; since OD-239 no tier lapses, so
//!   nothing marks a line any more); read as `SaveHead.lapsedSince`
//!   ([`lapsed_since_in`]) while the line has a head; cleared by the line's next player commit
//!   ([`crate::saves::commit_in`]).
//!
//! The monetization ledger is not read: the local entitlement writer logs no events and ticks lag
//! behind `access_until`. What this can and cannot detect is SAVES_API.md §4.7's.

use crate::accounts::AccessRow;
use crate::identity::ts;
use fi_domain::{GameRules, ProfileId, Tier, UnixMs};
use sqlx::PgConnection;
use time::OffsetDateTime;

/// The account's tier in an access row read at the database clock.
pub fn tier(access: &AccessRow) -> Tier {
    if access.vip { Tier::Vip } else { Tier::Free }
}

/// The commit-time check under the line's lock ([`crate::saves::commit_checked_in`]): `None` when
/// the profile keeps cloud data now; else its account's tier and its mode (`SAVES.NOT_ELIGIBLE`).
/// The mode is read by its own statement, after the line's lock, so a mode change that committed
/// meanwhile is seen. A game without a line for the mode keeps none whatever `lines` says (as the
/// route's check: CR-S97, CR-S105 review 3: an FM5 profile that is Hardcore by any route).
pub async fn refusal_in(
    conn: &mut PgConnection,
    account_id: uuid::Uuid,
    profile: ProfileId,
) -> Result<Option<(Tier, String)>, crate::identity::IdentityError> {
    let (game, mode): (String, String) =
        sqlx::query_as("SELECT game, mode FROM profile WHERE id = $1")
            .bind(profile.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
    let access = crate::accounts::access_in(conn, account_id).await?;
    let kept = GameRules::of(&game).line_mode(&mode);
    Ok((!kept).then(|| (tier(&access), mode)))
}

/// Records the account's `access_until` on the line at a lease grant or renewal (`now`: the
/// decision's database clock). With `grant`, a line whose recorded coverage had already ended
/// before `now` is marked lapsed first (at that coverage's end), when it has a head.
pub async fn cover_in(
    conn: &mut PgConnection,
    profile: ProfileId,
    access_until: Option<UnixMs>,
    now: UnixMs,
    grant: bool,
) -> Result<(), sqlx::Error> {
    let Some(until) = access_until else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE save_head SET \
           lapsed_at = CASE WHEN $4 AND revision > 0 AND covered_until <= $3 \
                            THEN COALESCE(lapsed_at, covered_until) ELSE lapsed_at END, \
           covered_until = $2, covered_at = $3 \
         WHERE profile_id = $1",
    )
    .bind(profile.as_uuid())
    .bind(ts(until))
    .bind(ts(now))
    .bind(grant)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Marks the line lapsed when the server meets its Hardcore profile while the account is not a
/// Supporter: at the end of the coverage the line last saw, at most `now`. Keeps an earlier mark;
/// a line without a head has nothing to restore and is left alone.
pub async fn mark_lapsed_in(
    conn: &mut PgConnection,
    profile: ProfileId,
    now: UnixMs,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE save_head SET lapsed_at = COALESCE(lapsed_at, LEAST(COALESCE(covered_until, $2), $2)) \
         WHERE profile_id = $1 AND revision > 0",
    )
    .bind(profile.as_uuid())
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `SaveHead.lapsedSince` of a line: its renewal mark while it has a head.
pub async fn lapsed_since_in(
    conn: &mut PgConnection,
    profile: ProfileId,
) -> Result<Option<UnixMs>, sqlx::Error> {
    let at: Option<Option<OffsetDateTime>> = sqlx::query_scalar(
        "SELECT lapsed_at FROM save_head WHERE profile_id = $1 AND revision > 0",
    )
    .bind(profile.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(at.flatten().map(crate::identity::ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(vip: bool) -> AccessRow {
        AccessRow {
            vip,
            ..AccessRow::free()
        }
    }

    #[test]
    fn the_tier_reads_the_access_row() {
        assert_eq!(tier(&access(true)), Tier::Vip);
        assert_eq!(tier(&access(false)), Tier::Free);
    }
}
