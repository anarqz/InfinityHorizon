//! Authoritative global online stop (OD-82, ONLINE_CONTROL.md).
//! Mutations serialize on the singleton using one pooled connection. Runtime services have
//! SELECT only; maintenance callers may close or explicitly compare-and-swap a reopen.
use fi_domain::UnixMs;
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use std::str::FromStr;

/// Why online access is closed. A financial stop takes precedence over an operator stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// The configured financial stop was reached.
    Budget,
    /// An operator explicitly closed online access.
    Operator,
}
impl CloseReason {
    /// The bounded database/CLI spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Operator => "operator",
        }
    }
}
impl FromStr for CloseReason {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "budget" => Ok(Self::Budget),
            "operator" => Ok(Self::Operator),
            _ => Err(Error::InvalidReason),
        }
    }
}
/// Stored policy. This is a repository row, not the public status DTO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Control {
    /// Whether new online work is allowed.
    pub open: bool,
    /// Monotonically increasing version used for an explicit reopen.
    pub revision: i64,
    /// Absent when open.
    pub reason: Option<CloseReason>,
    /// First closure time, unchanged when an operator closure becomes a budget closure.
    pub closed_at: Option<UnixMs>,
    /// Database observation time for monotonic cleanup deadlines at HTTP callers.
    pub observed_at: UnixMs,
}
/// An unavailable policy must never be interpreted as open by a caller.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Database access or decoding failed.
    #[error("online control database operation failed")]
    Database(#[from] sqlx::Error),
    /// Only documented reasons are accepted; arbitrary caller text is never echoed.
    #[error("online control reason must be budget or operator")]
    InvalidReason,
    /// A revision is a nonnegative signed database integer.
    #[error("online control expected revision must be nonnegative")]
    InvalidRevision,
    /// The caller's explicit reopen is stale.
    #[error("online control revision conflict: expected {expected}, current {current}")]
    RevisionConflict {
        /// The version the caller explicitly supplied.
        expected: i64,
        /// The version found under the singleton lock.
        current: i64,
    },
    /// Never wrap a monotonic revision.
    #[error("online control revision exhausted")]
    RevisionExhausted,
}
const SELECT: &str = "SELECT status, revision, reason, \
    (extract(epoch FROM closed_at) * 1000)::bigint AS closed_at_ms, \
    (extract(epoch FROM clock_timestamp()) * 1000)::bigint AS observed_at_ms \
    FROM online_control WHERE singleton";
fn row(value: &PgRow) -> Result<Control, Error> {
    Ok(Control {
        open: value.try_get::<String, _>("status")? == "open",
        revision: value.try_get("revision")?,
        reason: value
            .try_get::<Option<String>, _>("reason")?
            .map(|r| r.parse())
            .transpose()?,
        closed_at: value
            .try_get::<Option<i64>, _>("closed_at_ms")?
            .map(UnixMs::from_millis),
        observed_at: UnixMs::from_millis(value.try_get("observed_at_ms")?),
    })
}
/// Read the singleton. Missing/unreadable state is an error, never a default-open result.
pub async fn read(db: &PgPool) -> Result<Control, Error> {
    row(&sqlx::query(SELECT).fetch_one(db).await?)
}
async fn locked(conn: &mut PgConnection) -> Result<Control, Error> {
    row(&sqlx::query(
        "SELECT status, revision, reason, \
        (extract(epoch FROM closed_at) * 1000)::bigint AS closed_at_ms, \
        (extract(epoch FROM clock_timestamp()) * 1000)::bigint AS observed_at_ms \
        FROM online_control WHERE singleton FOR UPDATE",
    )
    .fetch_one(conn)
    .await?)
}
async fn write(
    conn: &mut PgConnection,
    previous: &Control,
    reason: Option<CloseReason>,
) -> Result<Control, Error> {
    let revision = previous
        .revision
        .checked_add(1)
        .ok_or(Error::RevisionExhausted)?;
    sqlx::query(
        "UPDATE online_control SET status = $1, revision = $2, reason = $3, \
         closed_at = CASE WHEN $1 = 'open' THEN NULL \
           ELSE coalesce(closed_at, clock_timestamp()) END WHERE singleton",
    )
    .bind(if reason.is_none() { "open" } else { "closed" })
    .bind(revision)
    .bind(reason.map(CloseReason::as_str))
    .execute(&mut *conn)
    .await?;
    row(&sqlx::query(SELECT).fetch_one(conn).await?)
}
/// Close idempotently. Budget may replace operator, never the reverse. Requires maintenance.
pub async fn close(db: &PgPool, reason: CloseReason) -> Result<Control, Error> {
    let mut tx = db.begin().await?;
    let current = locked(&mut tx).await?;
    let next = if current.open
        || current.reason == Some(CloseReason::Operator) && reason == CloseReason::Budget
    {
        write(&mut tx, &current, Some(reason)).await?
    } else {
        current
    };
    tx.commit().await?;
    Ok(next)
}
/// Explicit CAS reopen. Stale revisions always fail. An already-open row with the exact
/// current revision is unchanged.
pub async fn reopen(db: &PgPool, expected_revision: i64) -> Result<Control, Error> {
    if expected_revision < 0 {
        return Err(Error::InvalidRevision);
    }
    let mut tx = db.begin().await?;
    let current = locked(&mut tx).await?;
    if current.revision != expected_revision {
        return Err(Error::RevisionConflict {
            expected: expected_revision,
            current: current.revision,
        });
    }
    let next = if current.open {
        current
    } else {
        write(&mut tx, &current, None).await?
    };
    tx.commit().await?;
    Ok(next)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reasons_are_exact_and_never_echo_untrusted_input() {
        for reason in [CloseReason::Budget, CloseReason::Operator] {
            assert_eq!(reason.as_str().parse::<CloseReason>().unwrap(), reason);
        }
        for invalid in ["", "Budget", "operator ", "arbitrary-user-text"] {
            assert_eq!(
                invalid.parse::<CloseReason>().unwrap_err().to_string(),
                "online control reason must be budget or operator"
            );
        }
    }
}
