//! Idle in game (OD-121, GATE_API.md §4a, GAME_PLATFORM.md §3a): the hook's `X-Fi-Activity`
//! report on any game request, applied with `fi_db::activity` and answered with
//! `X-Fi-Activity-State`. **It never changes the game's answer:** a missing, malformed, stale or
//! failed report only means no response header (counted in the logs without values). Observation
//! only: nobody is evicted (`FI_IDLE_EVICT` stays off until the owner says so).

use axum::http::{HeaderMap, HeaderValue};
pub use fi_api_thresholds::thresholds;
use fi_domain::activity::Report;
use std::time::Duration;
use uuid::Uuid;

/// The request header.
pub const REQUEST: &str = fi_protocol::headers::FI_ACTIVITY;
/// The response header.
pub const RESPONSE: &str = fi_protocol::headers::FI_ACTIVITY_STATE;

/// The most a report may add to the game's request: past it the request is answered without the
/// state header (a busy pool or a held row never delays the game, review OD-121 finding 1).
pub const BUDGET: Duration = Duration::from_millis(250);

/// Applies the report `headers` carry for `session`; the response header's value when one was
/// applied within [`BUDGET`].
pub async fn apply(
    pool: &fi_db::PgPool,
    session: &str,
    headers: &HeaderMap,
) -> Option<HeaderValue> {
    let raw = headers.get(REQUEST)?;
    let Some(report) = raw.to_str().ok().and_then(Report::parse) else {
        tracing::debug!("activity report ignored: malformed");
        return None;
    };
    let session = Uuid::parse_str(session).ok()?;
    let applied = match tokio::time::timeout(
        BUDGET,
        fi_db::activity::report(pool, session, &report, thresholds()),
    )
    .await
    {
        Ok(Ok(Some(applied))) => applied,
        Ok(Ok(None)) => {
            tracing::debug!("activity report ignored: no active session, or its row is busy");
            return None;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "activity report not applied");
            return None;
        }
        Err(_) => {
            tracing::warn!("activity report skipped: over its time budget");
            return None;
        }
    };
    if applied.evicted {
        tracing::info!(
            session = %session,
            idle_secs = applied.idle_ms / 1_000,
            waiting = applied.waiting,
            "idle in game: session ended"
        );
    } else if applied.observed {
        tracing::info!(
            session = %session,
            idle_secs = applied.idle_ms / 1_000,
            waiting = applied.waiting,
            "idle in game: would evict"
        );
    }
    let value = match applied.evict_in {
        Some(secs) => format!("{};evict_in={secs}", applied.state.as_str()),
        None => applied.state.as_str().to_owned(),
    };
    HeaderValue::from_str(&value).ok()
}

/// Judges a session that reported before from its stored track (the gate calls it after a
/// heartbeat, so a hook that stops reporting turns idle by time); within [`BUDGET`], failures only
/// logged.
pub async fn evaluate(pool: &fi_db::PgPool, session: Uuid) {
    match tokio::time::timeout(
        BUDGET,
        fi_db::activity::evaluate(pool, session, thresholds()),
    )
    .await
    {
        Ok(Ok(Some(a))) if a.evicted => tracing::info!(
            session = %session,
            idle_secs = a.idle_ms / 1_000,
            waiting = a.waiting,
            "idle in game: session ended (no report)"
        ),
        Ok(Ok(Some(a))) if a.observed => tracing::info!(
            session = %session,
            idle_secs = a.idle_ms / 1_000,
            waiting = a.waiting,
            "idle in game: would evict (no report)"
        ),
        Ok(Err(e)) => tracing::warn!(error = %e, "activity evaluation failed"),
        _ => {}
    }
}

/// Whether a game RPC is a skill score (`Scoreboard.SubmitXpScore`; the class arrives as
/// `Forza.WebServices.Scoreboard, <assembly>`).
pub fn is_skill_score(class: &str, method: &str) -> bool {
    method == "SubmitXpScore"
        && class
            .split(',')
            .next()
            .unwrap_or_default()
            .trim()
            .ends_with(".Scoreboard")
}

/// A skill score the session submitted is progress; failures are only logged.
pub async fn skill_scored(pool: &fi_db::PgPool, session: &str) {
    let Ok(session) = Uuid::parse_str(session) else {
        return;
    };
    match tokio::time::timeout(BUDGET, fi_db::activity::skill_scored(pool, session)).await {
        Ok(Err(e)) => tracing::warn!(error = %e, "skill score not recorded as activity"),
        Err(_) => tracing::warn!("skill score activity skipped: over its time budget"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_scores_are_recognised_by_class_and_method() {
        assert!(is_skill_score(
            "Forza.WebServices.Scoreboard, Forza.WebServices.Interfaces",
            "SubmitXpScore"
        ));
        assert!(is_skill_score(
            "Forza.WebServices.Scoreboard",
            "SubmitXpScore"
        ));
        assert!(!is_skill_score(
            "Forza.WebServices.Scoreboard",
            "SubmitScore"
        ));
        assert!(!is_skill_score(
            "Forza.WebServices.NotScoreboardish",
            "SubmitXpScore"
        ));
    }
}

mod fi_api_thresholds {
    use fi_domain::activity::Thresholds;
    use std::sync::OnceLock;

    /// `FI_IDLE_PROGRESS_M`, `FI_IDLE_FREE_SECS`, `FI_IDLE_SUPPORTER_SECS`,
    /// `FI_IDLE_MENU_FREE_SECS`, `FI_IDLE_MENU_SUPPORTER_SECS` (GATE_API.md §4a), read once per
    /// process; a missing or out-of-range value keeps the owner's default (OD-121).
    pub fn thresholds() -> &'static Thresholds {
        static T: OnceLock<Thresholds> = OnceLock::new();
        T.get_or_init(|| from(|name| std::env::var(name).ok()))
    }

    pub(super) fn from(get: impl Fn(&str) -> Option<String>) -> Thresholds {
        let d = Thresholds::DEFAULT;
        let num = |name: &str, default: u64, lo: u64, hi: u64| -> u64 {
            match get(name).map(|v| v.trim().parse::<u64>()) {
                None => default,
                Some(Ok(v)) if (lo..=hi).contains(&v) => v,
                Some(_) => {
                    tracing::warn!(variable = name, "out of range; the default is used");
                    default
                }
            }
        };
        Thresholds {
            progress_m: num("FI_IDLE_PROGRESS_M", d.progress_m, 1, 10_000),
            free_secs: num("FI_IDLE_FREE_SECS", d.free_secs.into(), 60, 86_400) as u32,
            supporter_secs: num(
                "FI_IDLE_SUPPORTER_SECS",
                d.supporter_secs.into(),
                60,
                86_400,
            ) as u32,
            menu_free_secs: num("FI_IDLE_MENU_FREE_SECS", d.menu_free_secs.into(), 0, 86_400)
                as u32,
            menu_supporter_secs: num(
                "FI_IDLE_MENU_SUPPORTER_SECS",
                d.menu_supporter_secs.into(),
                0,
                86_400,
            ) as u32,
            evict: match get("FI_IDLE_EVICT").as_deref().map(str::trim) {
                None | Some("") | Some("false") => false,
                Some("true") => true,
                Some(_) => {
                    tracing::warn!(variable = "FI_IDLE_EVICT", "not true or false; observation");
                    false
                }
            },
            warn_secs: d.warn_secs,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn thresholds_come_from_the_environment_within_bounds() {
            assert_eq!(from(|_| None), Thresholds::DEFAULT);
            let t = from(|n| match n {
                "FI_IDLE_FREE_SECS" => Some("120".into()),
                "FI_IDLE_PROGRESS_M" => Some("0".into()),
                "FI_IDLE_MENU_FREE_SECS" => Some("x".into()),
                _ => None,
            });
            assert_eq!(
                (t.free_secs, t.progress_m, t.menu_free_secs, t.evict),
                (120, 50, 600, false)
            );
            for (value, evict) in [
                ("true", true),
                ("false", false),
                ("yes", false),
                (" true ", true),
            ] {
                let t = from(|n| (n == "FI_IDLE_EVICT").then(|| value.to_owned()));
                assert_eq!(t.evict, evict, "{value:?}");
            }
        }
    }
}
