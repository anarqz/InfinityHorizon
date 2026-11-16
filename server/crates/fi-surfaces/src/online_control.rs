//! Shared, bounded online-policy gate (ONLINE_CONTROL.md, OD-82).
//! This derived cache never grants a seat or owns queue state.
use crate::{errors::ApiError, views};
use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    http::{Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use fi_db::{
    PgPool,
    online_control::{self as db, Control},
};
use fi_protocol::{
    ErrorCode,
    online_control::{CloseReason, OnlineState, OnlineStatus},
    routes,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const CACHE_TTL: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const FAILURE_BACKOFF: Duration = Duration::from_secs(5);
const SAVE_DRAIN_MS: i64 = 120_000;

#[derive(Clone)]
struct Observed {
    row: Control,
    valid_until: Instant,
    drain_until: Option<Instant>,
}

impl Observed {
    fn from_row(row: Control, started: Instant, previous: Option<&Self>) -> Self {
        let mut drain_until = row.closed_at.filter(|_| !row.open).map(|closed| {
            // A fresh process must not renew the drain after a substantial wall-clock rollback.
            if closed.millis().saturating_sub(row.observed_at.millis()) > 5_000 {
                return started;
            }
            let age = row
                .observed_at
                .millis()
                .saturating_sub(closed.millis())
                .max(0);
            started + Duration::from_millis(SAVE_DRAIN_MS.saturating_sub(age).max(0) as u64)
        });
        if let Some(old) = previous
            && !old.row.open
            && !row.open
            && old.row.closed_at == row.closed_at
            && let (Some(a), Some(b)) = (old.drain_until, drain_until)
        {
            drain_until = Some(a.min(b));
        }
        Self {
            row,
            valid_until: started + CACHE_TTL,
            drain_until,
        }
    }

    fn status(&self) -> OnlineStatus {
        OnlineStatus {
            state: if self.row.open {
                OnlineState::Open
            } else {
                OnlineState::Closed
            },
            revision: self.row.revision as u64,
            reason: self.row.reason.map(|r| match r {
                db::CloseReason::Budget => CloseReason::Budget,
                db::CloseReason::Operator => CloseReason::Operator,
            }),
            closed_at: self.row.closed_at.map(views::at),
            retry_after_seconds: if self.row.open { 5 } else { 300 },
        }
    }

    fn failure_fallback(&self) -> Option<Self> {
        (!self.row.open).then(|| self.clone())
    }
}

#[derive(Default)]
struct Cached {
    observed: Option<Observed>,
    retry_not_before: Option<Instant>,
}

struct Policy {
    pool: PgPool,
    // One refresh at a time; no handler connection exists while this middleware runs.
    cache: Mutex<Cached>,
}

impl Policy {
    async fn read(&self) -> Result<Observed, ApiError> {
        // A cancelled or slow refresher cannot leave an unbounded queue of HTTP tasks.
        let mut cache =
            tokio::time::timeout(READ_TIMEOUT + Duration::from_millis(100), self.cache.lock())
                .await
                .map_err(|_| unavailable())?;
        let started = Instant::now();
        if let Some(value) = cache.observed.as_ref().filter(|v| started < v.valid_until) {
            return Ok(value.clone());
        }
        if cache.retry_not_before.is_some_and(|t| started < t) {
            return cache
                .observed
                .as_ref()
                .and_then(Observed::failure_fallback)
                .ok_or_else(unavailable);
        }
        // Set before awaiting so cancellation also backs off instead of creating a retry storm.
        cache.retry_not_before = Some(started + FAILURE_BACKOFF);
        match tokio::time::timeout(READ_TIMEOUT, db::read(&self.pool)).await {
            Ok(Ok(row)) => {
                let value = Observed::from_row(row, started, cache.observed.as_ref());
                cache.observed = Some(value.clone());
                cache.retry_not_before = None;
                Ok(value)
            }
            _ => cache
                .observed
                .as_ref()
                .and_then(Observed::failure_fallback)
                .ok_or_else(unavailable),
        }
    }
}

fn unavailable() -> ApiError {
    ApiError::new(ErrorCode::ServiceUnavailable, "online policy unavailable").retry_after(5)
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
}

fn always_allowed(method: &Method, path: &str) -> bool {
    if matches!(*method, Method::GET | Method::HEAD)
        && matches!(
            path,
            routes::shared::HEALTH_LIVE
                | routes::shared::HEALTH_READY
                | routes::shared::META
                | routes::core::ONLINE_STATUS
        )
    {
        return true;
    }
    if *method == Method::POST && path == routes::launcher::SIGN_OUT {
        return true;
    }
    if *method == Method::DELETE && path == routes::mm::SEAT {
        return true;
    }
    let p: Vec<_> = path.split('/').collect();
    match (method, p.as_slice()) {
        (&Method::POST, ["", "gate", "v1", "sessions", id, "leave"])
        | (&Method::POST, ["", "launcher", "v1", "sessions", id, "end"])
        | (&Method::DELETE, ["", "mm", "v1", "join", id])
        | (&Method::DELETE, ["", "mm", "v1", "worlds", id]) => !id.is_empty(),
        (&Method::DELETE, ["", "portal", "v1", "sessions", "current"]) => true,
        _ => false,
    }
}

fn save_commit(method: &Method, path: &str) -> bool {
    if *method != Method::PUT {
        return false;
    }
    let p: Vec<_> = path.split('/').collect();
    matches!(p.as_slice(), ["", "launcher", "v1", "profiles", xuid, "save"] if !xuid.is_empty())
}

async fn guard(State(policy): State<Arc<Policy>>, request: Request, next: Next) -> Response {
    if always_allowed(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    let observed = match policy.read().await {
        Ok(value) => value,
        Err(error) => return no_store(error.into_response()),
    };
    if observed.row.open
        || (save_commit(request.method(), request.uri().path())
            && observed
                .drain_until
                .is_some_and(|deadline| Instant::now() < deadline))
    {
        return next.run(request).await;
    }
    let status = observed.status();
    let code = if status.reason == Some(CloseReason::Budget) {
        ErrorCode::ServiceBudgetClosed
    } else {
        ErrorCode::ServiceOnlineClosed
    };
    no_store(
        ApiError::new(code, "online play is paused")
            .with_details(serde_json::to_value(status).expect("online status serializes"))
            .retry_after(300)
            .into_response(),
    )
}

async fn status(Extension(policy): Extension<Arc<Policy>>) -> Response {
    match policy.read().await {
        Ok(value) => {
            let mut response = Json(value.status()).into_response();
            // Edge caching must not add another five seconds to an already cached open value.
            let remaining = value
                .valid_until
                .saturating_duration_since(Instant::now())
                .as_secs();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                format!("public, max-age={remaining}, must-revalidate")
                    .parse()
                    .expect("numeric header"),
            );
            response
        }
        Err(error) => no_store(error.into_response()),
    }
}

fn with_policy(routes: Router, policy: Arc<Policy>) -> Router {
    guarded(
        routes.route(routes::core::ONLINE_STATUS, get(status)),
        policy,
    )
}

fn guarded(routes: Router, policy: Arc<Policy>) -> Router {
    routes
        .layer(Extension(policy.clone()))
        .layer(axum::middleware::from_fn_with_state(policy, guard))
}

/// One online policy (one bounded cache) shared by every surface of a process (ADR 0023 §3): each
/// surface applies [`Self::guard`] where its own layers need it, and the process mounts
/// [`Self::status_router`] once.
#[derive(Clone)]
pub struct OnlineGuard(Arc<Policy>);

impl OnlineGuard {
    /// The policy over `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self(Arc::new(Policy {
            pool,
            cache: Mutex::new(Cached::default()),
        }))
    }

    /// `routes` behind the guard, without the status route.
    pub fn guard(&self, routes: Router) -> Router {
        guarded(routes, self.0.clone())
    }

    /// `GET /core/v1/online-status` alone, behind the guard (which always allows it).
    pub fn status_router(&self) -> Router {
        self.guard(Router::new().route(routes::core::ONLINE_STATUS, get(status)))
    }
}

/// Install outside application auth/rate limits, inside the origin lock. Every origin uses this.
pub fn protect(routes: Router, pool: PgPool) -> Router {
    with_policy(
        routes,
        Arc::new(Policy {
            pool,
            cache: Mutex::new(Cached::default()),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request as HttpRequest, StatusCode},
    };
    use fi_domain::UnixMs;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    fn row(open: bool, age: i64) -> Control {
        Control {
            open,
            revision: 2,
            reason: (!open).then_some(db::CloseReason::Budget),
            closed_at: (!open).then_some(UnixMs::from_millis(1_000_000)),
            observed_at: UnixMs::from_millis(1_000_000 + age),
        }
    }
    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy("postgres://unused:unused@127.0.0.1:9/unused")
            .unwrap()
    }
    async fn response(open: bool, age: i64, method: Method, path: &str) -> Response {
        let policy = Arc::new(Policy {
            pool: lazy_pool(),
            cache: Mutex::new(Cached {
                observed: Some(Observed::from_row(row(open, age), Instant::now(), None)),
                retry_not_before: None,
            }),
        });
        with_policy(
            Router::new().fallback(|| async { StatusCode::NO_CONTENT }),
            policy,
        )
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn closure_blocks_aliases_proxy_and_renewals_before_handlers() {
        for path in [
            "/gate/v1/sessions",
            "/launcher/v1/sessions",
            "/gate/v1/sessions/x/heartbeat",
            "/mm/v1/worlds",
            "/mm/v1/seat",
            "/mm/v1/punches",
            "/sessiondirectory.xboxlive.com/sessions/x",
            "/gate/v1/content/scans",
            "/launcher/v1/auth/token",
            "/crash/v1/reports",
        ] {
            let response = response(false, 0, Method::POST, path).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(response.headers()[header::RETRY_AFTER], "300");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let json: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(json["code"], "SERVICE.BUDGET_CLOSED");
        }
    }

    #[tokio::test]
    async fn exact_cleanup_allowlist_and_drain_preserve_only_existing_handoff() {
        for (method, path) in [
            (Method::POST, "/gate/v1/sessions/x/leave"),
            (Method::POST, "/launcher/v1/sessions/x/end"),
            (Method::DELETE, "/mm/v1/seat"),
            (Method::DELETE, "/mm/v1/join/x"),
            (Method::DELETE, "/mm/v1/worlds/x"),
        ] {
            assert_eq!(
                response(false, 999_999, method, path).await.status(),
                StatusCode::NO_CONTENT
            );
        }
        assert_eq!(
            response(
                false,
                119_000,
                Method::PUT,
                "/launcher/v1/profiles/123/save"
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
        for (method, path, age) in [
            (Method::PUT, "/launcher/v1/profiles/123/save", 120_000),
            (Method::POST, "/launcher/v1/profiles/123/save/seed", 0),
            (Method::PUT, "/launcher/v1/profiles/123/save/restore", 0),
            (Method::POST, "/gate/v1/sessions/x/leave/heartbeat", 0),
        ] {
            assert_eq!(
                response(false, age, method, path).await.status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        assert_eq!(
            response(true, 0, Method::POST, "/mm/v1/join")
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
    }

    #[test]
    fn refresh_or_clock_rollback_cannot_extend_the_drain() {
        let now = Instant::now();
        let first = Observed::from_row(row(false, 100_000), now, None);
        let second = Observed::from_row(row(false, 0), now + Duration::from_secs(10), Some(&first));
        assert_eq!(second.drain_until, first.drain_until);
        assert_eq!(
            Observed::from_row(row(false, -60_000), now, None).drain_until,
            Some(now)
        );
        assert!(first.failure_fallback().is_some());
        assert!(
            Observed::from_row(row(true, 0), now, None)
                .failure_fallback()
                .is_none()
        );
    }

    #[tokio::test]
    async fn expired_open_fails_closed_and_failure_backoff_is_shared() {
        let now = Instant::now();
        let policy = Arc::new(Policy {
            pool: lazy_pool(),
            cache: Mutex::new(Cached {
                observed: Some(Observed::from_row(
                    row(true, 0),
                    now - CACHE_TTL - Duration::from_secs(1),
                    None,
                )),
                retry_not_before: None,
            }),
        });
        assert!(policy.read().await.is_err());
        let next = policy.cache.lock().await.retry_not_before;
        let (a, b, c) = tokio::join!(policy.read(), policy.read(), policy.read());
        assert!(a.is_err() && b.is_err() && c.is_err());
        assert_eq!(policy.cache.lock().await.retry_not_before, next);
    }

    #[tokio::test]
    #[ignore = "needs an isolated FI_TEST_DATABASE_URL; changes the global test policy"]
    async fn database_latch_crosses_instances_and_only_explicit_reopen_clears_it() {
        let url = std::env::var("FI_TEST_DATABASE_URL").expect("isolated test database");
        let writer = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        fi_db::migrate::run(&writer).await.unwrap();
        let initial = db::read(&writer).await.unwrap();
        if !initial.open {
            db::reopen(&writer, initial.revision).await.unwrap();
        }
        let password = uuid::Uuid::now_v7().simple().to_string();
        let login = format!("fi_guard_{}", &password[20..]);
        fi_db::migrate::ensure_login(&writer, &login, &password, &["fi_runtime"])
            .await
            .unwrap();
        let options = url
            .parse::<sqlx::postgres::PgConnectOptions>()
            .unwrap()
            .username(&login)
            .password(&password);
        let runtime = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let policy = Arc::new(Policy {
            pool: runtime.clone(),
            cache: Mutex::new(Cached::default()),
        });
        assert!(policy.read().await.unwrap().row.open);
        let closed = db::close(&writer, db::CloseReason::Budget).await.unwrap();
        assert!(
            policy.read().await.unwrap().row.open,
            "only the bounded existing cache remains open"
        );
        policy
            .cache
            .lock()
            .await
            .observed
            .as_mut()
            .unwrap()
            .valid_until = Instant::now();
        assert_eq!(policy.read().await.unwrap().row.revision, closed.revision);
        let second = Policy {
            pool: runtime.clone(),
            cache: Mutex::new(Cached::default()),
        };
        assert!(
            !second.read().await.unwrap().row.open,
            "a fresh instance observes the durable latch"
        );
        let app = with_policy(
            Router::new().fallback(|| async { StatusCode::NO_CONTENT }),
            policy.clone(),
        );
        let r = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri(routes::core::ONLINE_STATUS)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(r.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["state"], "closed");
        assert_eq!(body["reason"], "budget");
        let r = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/gate/v1/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        db::reopen(&writer, closed.revision).await.unwrap();
        assert!(
            !policy.read().await.unwrap().row.open,
            "reopening also respects the bounded cache"
        );
        policy
            .cache
            .lock()
            .await
            .observed
            .as_mut()
            .unwrap()
            .valid_until = Instant::now();
        assert!(policy.read().await.unwrap().row.open);
        runtime.close().await;
        writer.close().await;
    }
}
