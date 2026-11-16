//! The Supporters' universe on the Forza RPC (SER-M3 step 6, OD-48; GAME_PLATFORM.md §3a
//! "Supporter universe", register CR-S18): what makes the world shared between Supporters, ported
//! from the Beta's `server/src/forza.rs`. Served on saves only ([`Features::Supporter`]): the free
//! allowlist never derives from it, so a free ticket keeps `GAME.NOT_SERVED` for every method here.
//!
//! - [`boards`]: `Scoreboard.SubmitScore`, `SubmitXpScore`, `GetBestScore`, `GetRows`,
//!   `GetRowsAnthem`, `GetTimeTrials` (the leaderboards and the XP ledger).
//! - [`ghosts`]: `Scoreboard.UploadFile`, `DownloadFile`, `GetRival` (ghost races and rivals).
//! - [`drivatars`]: `Drivatar.SaveDrivatarInformation`, `GetDrivatarInformationForRace`,
//!   `GetDrivatarsForCache` (BUG-0001: only trained drivatars, progression from the owner's
//!   profile).
//! - [`social`]: `User.GetCompareStatsForRelatedUsers`, `GetCompareStatsByBucket` ("Meus rivais",
//!   SER-M3 step 7), and the friend set of the friend-aware reads above. These two are on the
//!   free allowlist too, answered there with its own empty lists (`forza::pure`), never from here.
//! - [`inbox`]: Xls `Notifications.RetrieveForUser`, `MarkRead`, `Clear`, `SendTextNotification`
//!   (SER-M3 step 7: messages between friends, news, the rivals sync), and GetRival's pending
//!   challenges. The free set keeps its empty Pure inbox (`forza::served`).
//! - [`clubs`]: `CarClub.*`, Xls `Club.*`, `Scoreboard.GetLadderRows`, Xls
//!   `User.AcceptClubInvites` (SER-M3 step 8, CR-S70): Car Clubs, one mode each (OD-130), served
//!   to every ticket like the drivatars (`forza::served`).
//! - [`storefront`]: `Storefront.*`, `Storefront{Livery,LayerGroup,Tuning}.Create`, the vinyl group
//!   and tune `ContentVersioning` calls and the `OnlineProfile` libraries (OD-138, CR-S74): shared
//!   designs, vinyl groups and tunes, served to every ticket like the clubs.
//!
//! Every read sees only what [`fi_db::visibility`] allows the ticket's profile (same game and
//! mode, active, unbanned). Every write is one transaction with its progression fact (the
//! request's `target::fact_id`), so a replayed request id answers the same and writes nothing.
//!
//! [`Features::Supporter`]: super::Features::Supporter

pub mod boards;
pub mod clubs;
pub mod drivatars;
pub mod ghosts;
pub mod inbox;
pub mod rivals;
pub mod social;
pub mod storefront;

#[cfg(test)]
mod golden;

use super::forza::{Reply, Served};
use super::rpc::Ctx;
use crate::errors::ApiError;
use serde_json::Value as Json;
use sqlx::PgConnection;

/// How a universe method is answered on saves; `None`: not one of the universe's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    boards::served(xls, short, method)
        .or_else(|| ghosts::served(xls, short, method))
        .or_else(|| drivatars::served(xls, short, method))
        .or_else(|| social::served(xls, short, method))
        .or_else(|| inbox::served(xls, short, method))
        .or_else(|| clubs::served(xls, short, method))
        .or_else(|| storefront::served(xls, short, method))
}

/// A stored universe method, on the request's transaction.
pub(crate) async fn stored(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    xls: bool,
    short: &str,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    if boards::served(xls, short, method).is_some() {
        return boards::stored(conn, ctx, short, method, params).await;
    }
    if ghosts::served(xls, short, method).is_some() {
        return ghosts::stored(conn, ctx, short, method, params).await;
    }
    if drivatars::served(xls, short, method).is_some() {
        return drivatars::stored(conn, ctx, short, method, params).await;
    }
    if social::served(xls, short, method).is_some() {
        return social::stored(conn, ctx, short, method, params).await;
    }
    if inbox::served(xls, short, method).is_some() {
        return inbox::stored(conn, ctx, short, method, params).await;
    }
    if clubs::served(xls, short, method).is_some() {
        return clubs::stored(conn, ctx, short, method, params).await;
    }
    if storefront::served(xls, short, method).is_some() {
        return storefront::stored(conn, ctx, short, method, params).await;
    }
    Err(ctx.not_served(short, method))
}
