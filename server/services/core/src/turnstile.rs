//! Turnstile verification (ADR 0903 §2, CORE_API.md §1 "challenged"): the widget's response
//! token arrives in `X-Fi-Challenge`; core spends it once (`fi_db::challenge`, before any
//! network call, so two requests with one token cannot both pass), then verifies it.
//!
//! Test mode answers as Cloudflare documents for its test secrets, without a network call; live
//! mode posts JSON to siteverify (3 s timeout, one retry on a network error or a 5xx), refuses a
//! token whose `hostname` is not the web's, and never logs the token. Cloudflare's documented
//! test keys (developers.cloudflare.com/turnstile/troubleshooting/testing/):
//! site key `1x00000000000000000000AA` always passes; secrets `1x…AA` pass, `2x…AA` fail, `3x…AA`
//! report "token already spent". With a test site key the real widget answers the same dummy
//! token every time ([`TEST_DUMMY_TOKEN`]), so test mode does not spend that one token: a second
//! challenged request (sign-up after sign-in) passes as it does on the web's fake server. Every
//! other token, and every token in live mode, is spent once.

use crate::config::TurnstileConfig;
use fi_db::PgPool;
use fi_protocol::{ErrorCode, headers};
use fi_surfaces::errors::ApiError;
use serde::Deserialize;
use serde_json::json;
use std::net::IpAddr;
use std::time::Duration;

/// The always-pass test site key (the web uses it locally).
pub const TEST_SITE_KEY_PASS: &str = "1x00000000000000000000AA";
/// The always-pass test secret.
pub const TEST_SECRET_PASS: &str = "1x0000000000000000000000000000000AA";
/// The always-fail test secret.
pub const TEST_SECRET_FAIL: &str = "2x0000000000000000000000000000000AA";
/// The "token already spent" test secret.
pub const TEST_SECRET_SPENT: &str = "3x0000000000000000000000000000000AA";
/// The response token the widget returns for a test site key (always the same).
pub const TEST_DUMMY_TOKEN: &str = "XXXX.DUMMY.TOKEN.XXXX";
/// Cloudflare's siteverify.
pub const SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// Longest response token accepted (Cloudflare's documented maximum).
pub const MAX_TOKEN_LEN: usize = 2048;
/// Timeout of one siteverify attempt.
pub const TIMEOUT: Duration = Duration::from_secs(3);

/// One of Cloudflare's documented test secrets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestSecret {
    /// `1x…AA`: every token passes.
    Pass,
    /// `2x…AA`: every token fails.
    Fail,
    /// `3x…AA`: every token is reported spent.
    Spent,
}

impl TestSecret {
    /// The test secret `text` names, if it is one.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            TEST_SECRET_PASS => Some(TestSecret::Pass),
            TEST_SECRET_FAIL => Some(TestSecret::Fail),
            TEST_SECRET_SPENT => Some(TestSecret::Spent),
            _ => None,
        }
    }
}

/// The answer of [`Verifier::verify`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A human solved it.
    Pass,
    /// Invalid, expired, spent, or solved elsewhere.
    Fail,
}

/// siteverify could not be reached (both attempts): `SERVICE.UNAVAILABLE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unavailable;

/// Verifies response tokens.
pub struct Verifier {
    config: TurnstileConfig,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct SiteVerify {
    success: bool,
    #[serde(default)]
    hostname: Option<String>,
}

impl Verifier {
    /// A verifier for `config`.
    pub fn new(config: TurnstileConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { config, client }
    }

    /// Whether tokens are answered by a test secret (no network call).
    pub fn is_test(&self) -> bool {
        matches!(self.config, TurnstileConfig::Test(_))
    }

    /// `test` or `live` (for `/meta`).
    pub fn mode(&self) -> &'static str {
        match self.config {
            TurnstileConfig::Deferred => "deferred",
            TurnstileConfig::Test(_) => "test",
            TurnstileConfig::Live { .. } => "live",
        }
    }

    /// Verifies one response token (already spent in `challenge_use` by the caller).
    pub async fn verify(
        &self,
        token: &str,
        remote_ip: Option<IpAddr>,
    ) -> Result<Verdict, Unavailable> {
        let (secret, url, hostname) = match &self.config {
            TurnstileConfig::Deferred => return Err(Unavailable),
            TurnstileConfig::Test(TestSecret::Pass) => return Ok(Verdict::Pass),
            TurnstileConfig::Test(_) => return Ok(Verdict::Fail),
            TurnstileConfig::Live {
                secret,
                url,
                hostname,
            } => (secret, url, hostname),
        };
        let mut body = json!({"secret": secret, "response": token});
        if let Some(ip) = remote_ip {
            body["remoteip"] = json!(ip.to_string());
        }
        for attempt in 0..2 {
            match self.client.post(url).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    let Ok(answer) = r.json::<SiteVerify>().await else {
                        tracing::warn!(attempt, "siteverify answered something unreadable");
                        continue;
                    };
                    let host_ok = match (hostname, &answer.hostname) {
                        (Some(want), Some(got)) => want.eq_ignore_ascii_case(got),
                        (Some(_), None) => false,
                        (None, _) => true,
                    };
                    return Ok(if answer.success && host_ok {
                        Verdict::Pass
                    } else {
                        Verdict::Fail
                    });
                }
                Ok(r) if r.status().is_server_error() => {
                    tracing::warn!(attempt, status = r.status().as_u16(), "siteverify failed");
                }
                Ok(r) => {
                    // 4xx: our request is wrong (secret); retrying cannot help.
                    tracing::error!(
                        status = r.status().as_u16(),
                        "siteverify refused the request"
                    );
                    return Err(Unavailable);
                }
                Err(e) => {
                    tracing::warn!(attempt, timeout = e.is_timeout(), "siteverify unreachable");
                }
            }
        }
        Err(Unavailable)
    }
}

/// The challenge of a "challenged" route: `X-Fi-Challenge` present (else 403
/// `AUTH.CHALLENGE_REQUIRED {challenge: "turnstile"}`), never seen before (else
/// `AUTH.CHALLENGE_FAILED`), and verified (else `AUTH.CHALLENGE_FAILED`). Holds a pooled
/// connection only for the spend, never across the siteverify call.
pub async fn require(
    verifier: &Verifier,
    pool: &PgPool,
    request_headers: &axum::http::HeaderMap,
    remote_ip: Option<IpAddr>,
) -> Result<(), ApiError> {
    if matches!(verifier.config, TurnstileConfig::Deferred) {
        return Err(ApiError::unavailable());
    }
    let token = request_headers
        .get(headers::FI_CHALLENGE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::AuthChallengeRequired,
                "this route needs a Turnstile challenge (X-Fi-Challenge)",
            )
            .with_details(json!({"challenge": "turnstile"}))
        })?;
    let failed = || ApiError::new(ErrorCode::AuthChallengeFailed, "challenge failed");
    if token.len() > MAX_TOKEN_LEN {
        return Err(failed());
    }
    // The test widget's one dummy token is never spent (test mode only: a live secret never
    // accepts it, so it cannot be replayed against Cloudflare).
    let first = if verifier.is_test() && token == TEST_DUMMY_TOKEN {
        true
    } else {
        let mut conn = pool.acquire().await?;
        fi_db::challenge::spend_in(&mut conn, &fi_auth::sha256(token.as_bytes())).await?
    };
    if !first {
        return Err(failed());
    }
    match verifier.verify(token, remote_ip).await {
        Ok(Verdict::Pass) => Ok(()),
        Ok(Verdict::Fail) => Err(failed()),
        Err(Unavailable) => Err(ApiError::unavailable()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_secrets_answer_without_the_network() {
        let pass = Verifier::new(TurnstileConfig::Test(TestSecret::Pass));
        assert_eq!(pass.verify("any", None).await, Ok(Verdict::Pass));
        assert_eq!(pass.mode(), "test");
        for s in [TestSecret::Fail, TestSecret::Spent] {
            let v = Verifier::new(TurnstileConfig::Test(s));
            assert_eq!(v.verify("any", None).await, Ok(Verdict::Fail));
        }
        assert_eq!(TestSecret::parse(TEST_SECRET_PASS), Some(TestSecret::Pass));
        assert_eq!(
            TestSecret::parse("1x0000000000000000000000000000000AB"),
            None
        );
        assert_eq!(TEST_SITE_KEY_PASS.len(), 24);
    }

    /// siteverify and the probes are https: the client must have a TLS backend (an https request
    /// fails on the connection, never with reqwest's "no TLS backend" builder error).
    #[tokio::test]
    async fn the_client_speaks_https() {
        let v = Verifier::new(TurnstileConfig::Test(TestSecret::Pass));
        let e = v
            .client
            .get("https://127.0.0.1:1/")
            .send()
            .await
            .unwrap_err();
        assert!(e.is_connect() && !e.is_builder(), "{e:?}");
    }

    #[tokio::test]
    async fn live_mode_posts_to_siteverify_retries_once_and_checks_the_hostname() {
        use axum::{Json, Router, routing::post};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A local stand-in for siteverify: the first call fails with 503, then it answers for
        // the `response` it gets ("good" passes on 127.0.0.1, "elsewhere" on another hostname).
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route(
            "/siteverify",
            post(move |Json(body): Json<serde_json::Value>| {
                let seen = seen.clone();
                async move {
                    let n = seen.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(body["secret"], "local-fake-live-secret-0001");
                    if n == 0 {
                        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
                    }
                    let (ok, host) = match body["response"].as_str() {
                        Some("good") => (true, "127.0.0.1"),
                        Some("elsewhere") => (true, "evil.example"),
                        _ => (false, "127.0.0.1"),
                    };
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({"success": ok, "hostname": host, "error-codes": []})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let v = Verifier::new(TurnstileConfig::Live {
            secret: "local-fake-live-secret-0001".into(),
            url: format!("http://{addr}/siteverify"),
            hostname: Some("127.0.0.1".into()),
        });
        assert_eq!(v.mode(), "live");
        assert_eq!(
            v.verify("good", None).await,
            Ok(Verdict::Pass),
            "after one retry"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(v.verify("bad", None).await, Ok(Verdict::Fail));
        assert_eq!(v.verify("elsewhere", None).await, Ok(Verdict::Fail));
        let down = Verifier::new(TurnstileConfig::Live {
            secret: "local-fake-live-secret-0001".into(),
            url: "http://127.0.0.1:9/siteverify".into(),
            hostname: None,
        });
        assert_eq!(down.verify("good", None).await, Err(Unavailable));
    }
}
