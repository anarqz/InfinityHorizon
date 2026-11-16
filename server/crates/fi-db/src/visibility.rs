//! Who sees whom in the Supporters' universe (SER-M3 step 6, OD-48): the one visibility rule every
//! universe read of the game plane applies (boards, ghosts, rivals, drivatars; later friends,
//! clubs and the storefront), ported from the Beta's `meets` / `same_mode` / `can_see`.
//!
//! A row of another profile is visible to a [`Viewer`] when it is of the viewer's game and mode
//! (hardcore and noob never see each other, SER-3.2), the row's mode is still its profile's (a
//! profile that moved to noob leaves its hardcore rows behind), the profile is active, and its
//! account is active and not banned (a timed ban that has passed no longer hides it). Free players
//! never appear in the Supporter-only reads because they never write those rows (OD-24); a lapsed
//! Supporter's rows stay (they were earned while a Supporter). The drivatars are every ticket's and
//! cross modes (OD-128, `crate::drivatars`): they use [`VISIBLE`] without the mode condition.
//!
//! Queries join the row's profile as `p` and its account as `a` and add [`VISIBLE`]:
//!
//! ```sql
//! FROM game_score s
//! JOIN profile p ON p.id = s.profile_id
//! JOIN account a ON a.id = p.account_id
//! WHERE s.game = $1 AND s.mode = $2 AND p.mode = s.mode AND <VISIBLE>
//! ```

use uuid::Uuid;

/// The SQL condition on the joined `profile p` and `account a` of a visible row (see the module).
pub const VISIBLE: &str = "p.status = 'active' AND a.status = 'active' \
     AND (a.banned_at IS NULL OR (a.banned_until IS NOT NULL AND a.banned_until <= now()))";

/// The profile a universe read is for: the game access ticket's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewer<'a> {
    /// The ticket's game (`fh2`).
    pub game: &'a str,
    /// The ticket's mode (`hardcore` or `noob`).
    pub mode: &'a str,
    /// The ticket's profile.
    pub profile_id: Uuid,
    /// Its account.
    pub account_id: Uuid,
    /// Its XUID.
    pub xuid: i64,
    /// The play session.
    pub session_id: Uuid,
}
