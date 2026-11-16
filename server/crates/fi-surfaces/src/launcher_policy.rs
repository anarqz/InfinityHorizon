//! Thin shared HTTP mapping for authoritative launcher-version policy (CR-S32).
use crate::errors::ApiError;
use fi_protocol::ErrorCode;

/// Never expose stored input or database errors.
pub fn error(error: fi_db::launcher_policy::Error) -> ApiError {
    use fi_db::launcher_policy::Error as E;
    match error {
        E::UpdateRequired(minimum) => ApiError::new(
            ErrorCode::GameUpdateRequired,
            "update the launcher before playing online",
        )
        .with_details(serde_json::json!({"minLauncher": minimum})),
        E::NotFound => ApiError::new(ErrorCode::AccountsNotFound, "no such play session"),
        E::KeyReused => ApiError::new(
            ErrorCode::RequestIdempotencyKeyReused,
            "session id already used",
        ),
        E::UnknownGame => ApiError::invalid("game", "a known game"),
        E::PolicyUnavailable | E::Db(_) => ApiError::unavailable(),
    }
}
