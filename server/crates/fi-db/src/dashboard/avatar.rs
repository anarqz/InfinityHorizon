//! Where a listed player's Discord avatar is (DASHBOARD_API.md §4 `GET /media/avatar/{name}`):
//! the one statement of `fi_db::dashboard` that selects a Discord user id, because the CDN path
//! needs it (`/avatars/{user id}/{hash}.png`). The id goes from here to the platform's fetch and
//! nowhere else: never into a response, a log or a fact.

use crate::identity::IdentityError;
use sqlx::PgConnection;

/// The Discord user id and avatar hash of the listed profile of `game` named `name`
/// (case-insensitive, any mode; [`super::players::listed`]); `None` when the name is missing, the
/// profile is not listed, or no Discord account with an avatar is linked.
pub async fn avatar_of_in(
    conn: &mut PgConnection,
    game: &str,
    name: &str,
) -> Result<Option<(i64, String)>, IdentityError> {
    let listed = super::players::listed();
    let sql = format!(
        "SELECT a.discord_user_id, a.discord_avatar FROM profile p JOIN account a ON a.id = p.account_id \
         WHERE p.game = $1 AND lower(p.name) = lower($2) AND {listed} \
           AND a.discord_user_id IS NOT NULL AND a.discord_avatar IS NOT NULL"
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?)
}
