//! The database half of the registration switch; services also honor FI_REGISTRATION_ENABLED.

use sqlx::{PgConnection, PgPool};

/// Whether new accounts may be created.
pub async fn open_in(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT open FROM registration_control WHERE singleton")
        .fetch_one(conn)
        .await
}

/// Opens or closes registration (fi_maintenance).
pub async fn set(db: &PgPool, open: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE registration_control SET open = $1, updated_at = clock_timestamp() WHERE singleton",
    )
    .bind(open)
    .execute(db)
    .await?;
    Ok(())
}
