//! Play-session admission primitives (ADR 0008 §2, ACCOUNTS.md §7).
//!
//! Admission is one transaction with a fixed lock order: the pool (this module), then the profile's
//! save head ([`crate::saves::acquire_in`]), then the `play_session` rows and the read models. The pool lock
//! is a transaction-scoped advisory lock rather than a `SELECT ... FOR UPDATE` of the `pool` row:
//! the row lock needs an UPDATE grant on `pool` that the runtime role must not have, and the
//! advisory lock is released by commit or rollback whatever happens. Two admissions to one pool
//! serialize. Since OD-71 (one online capacity per game across its pools) a decision that may admit
//! (a launch, a queued session's poll, an expired session's heartbeat) first takes the pool's
//! game's lock ([`lock_game_of`]), then the pool's: admissions of one game serialize, while a
//! heartbeat of an active session or a leave, which never takes a slot, holds the pool's lock
//! alone. Only a game with an online capacity has the lock: an uncapped game's pools never wait
//! for each other (as before OD-71). Setting a cap is read by the next decision; one that read the
//! game uncapped just before may still admit without the game's lock, so the cap can be passed by
//! the decisions in flight at that instant. Pools of different games never wait for each other.
//!
//! Since OD-228 (one global online capacity across every game, `global_online_capacity`) the same
//! two functions first take the global lock while that cap is set: the order is global, game,
//! pool. With a global cap every admitting decision of every game serializes on it; unset, nothing
//! changes. Setting it has the same in-flight caveat as a game's cap.

use fi_domain::PoolName;
use sqlx::PgConnection;

/// First key of the two-key advisory locks of pool admission ("FIP0"). The two-key form's key
/// space never meets the one-key form sqlx's migrator locks with.
pub const POOL_LOCK_NAMESPACE: i32 = 0x4649_5030;
/// First key of the games' admission locks ("FIG0"), taken before a pool's (OD-71).
pub const GAME_LOCK_NAMESPACE: i32 = 0x4649_4730;

/// First key of the global admission lock ("FIW0"), taken before any game's (OD-228).
pub const GLOBAL_LOCK_NAMESPACE: i32 = 0x4649_5730;

/// Takes the global admission lock (none while `global_online_capacity` is unset), then the
/// admission lock of `pool`'s game (none for a pool without a row or a game without an
/// online capacity) until the caller's
/// transaction ends; always before [`lock_pool`]. Waits at most the connection's `lock_timeout`.
pub async fn lock_game_of(conn: &mut PgConnection, pool: &PoolName) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock($1, 0) FROM global_online_capacity WHERE capacity IS NOT NULL",
    )
    .bind(GLOBAL_LOCK_NAMESPACE)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock($1, hashtext(p.game)) FROM pool p JOIN game g ON g.code = p.game \
         WHERE p.name = $2 AND g.online_capacity IS NOT NULL",
    )
        .bind(GAME_LOCK_NAMESPACE)
        .bind(pool.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// [`lock_game_of`] without waiting, for a decision that already holds the pool's lock (taking it
/// blocking then would invert the order): `false` when another decision holds the global lock or
/// the game's (the caller rolls back, releasing whatever this took).
pub async fn try_lock_game_of(
    conn: &mut PgConnection,
    pool: &PoolName,
) -> Result<bool, sqlx::Error> {
    let global: Option<bool> = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock($1, 0) FROM global_online_capacity WHERE capacity IS NOT NULL",
    )
    .bind(GLOBAL_LOCK_NAMESPACE)
    .fetch_optional(&mut *conn)
    .await?;
    if global == Some(false) {
        return Ok(false);
    }
    let got: Option<bool> = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock($1, hashtext(p.game)) FROM pool p JOIN game g ON g.code = p.game \
         WHERE p.name = $2 AND g.online_capacity IS NOT NULL",
    )
    .bind(GAME_LOCK_NAMESPACE)
    .bind(pool.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(got.unwrap_or(true))
}

/// Takes `pool`'s admission lock until the caller's transaction ends. Waits at most the
/// connection's `lock_timeout` (2 s for services), then fails with `55P03` (retryable).
pub async fn lock_pool(conn: &mut PgConnection, pool: &PoolName) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(POOL_LOCK_NAMESPACE)
        .bind(pool.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}
