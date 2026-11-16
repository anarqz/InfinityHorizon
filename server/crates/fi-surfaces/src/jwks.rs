//! Google's rotating securetoken keys. Derived, bounded per-instance cache; no token is logged.
//! Refresh completes before callers acquire a database connection/transaction (ACCOUNTS §4.1).
use fi_auth::idtoken::{KeySet, Mode, StaticKeys, Verifier};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fixed production endpoint: no environment-controlled URL, redirect or proxy trust.
pub const GOOGLE_JWKS: &str =
    "https://www.googleapis.com/service_accounts/v1/jwk/securetoken@system.gserviceaccount.com";
const MAX_BYTES: usize = 64 * 1024;
const UNKNOWN_BACKOFF: Duration = Duration::from_secs(60);
const FAILURE_BACKOFF: Duration = Duration::from_secs(5);

/// Invalid token/key id versus temporarily unavailable trusted key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshError {
    /// Malformed token, or key absent after the bounded rotation refresh.
    Invalid,
    /// Trusted keys could not be refreshed; expired keys are never used.
    Unavailable,
}
struct Snapshot {
    keys: Arc<StaticKeys>,
    verifier: Verifier,
    expires: Instant,
}
#[derive(Default)]
struct Cache {
    snapshot: Option<Snapshot>,
    unknown_attempt: Option<Instant>,
    failed_at: Option<Instant>,
}
/// Single-flight refresh with one bounded key set and an unknown-kid cooldown, never a kid map.
pub struct RotatingVerifier {
    client: reqwest::Client,
    url: String,
    project: String,
    providers: Vec<String>,
    cache: Mutex<Cache>,
    refresh: tokio::sync::Mutex<()>,
}
impl std::fmt::Debug for RotatingVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotatingVerifier").finish_non_exhaustive()
    }
}
impl RotatingVerifier {
    /// Uses only Google's fixed HTTPS endpoint and system certificate validation.
    pub fn google(project: &str, providers: &[String]) -> Result<Self, reqwest::Error> {
        Self::new(project, providers, GOOGLE_JWKS, true)
    }
    fn new(
        project: &str,
        providers: &[String],
        url: &str,
        https: bool,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .https_only(https)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(3))
                .build()?,
            url: url.into(),
            project: project.into(),
            providers: providers.to_vec(),
            cache: Mutex::new(Cache::default()),
            refresh: tokio::sync::Mutex::new(()),
        })
    }
    fn cached(
        &self,
        kid: &str,
        now: Instant,
        owns_refresh: bool,
    ) -> Result<Option<Verifier>, RefreshError> {
        let cache = self.cache.lock().map_err(|_| RefreshError::Unavailable)?;
        if let Some(s) = &cache.snapshot
            && now < s.expires
            && s.keys.key(kid).is_some()
        {
            return Ok(Some(s.verifier.clone()));
        }
        if cache
            .failed_at
            .is_some_and(|at| now.duration_since(at) < FAILURE_BACKOFF)
        {
            // A live refresher coalesces waiters; a cancelled one released its mutex and
            // leaves this guard in place, preventing an immediate replacement request.
            if !owns_refresh && self.refresh.try_lock().is_err() {
                return Ok(None);
            }
            return Err(RefreshError::Unavailable);
        }
        if cache.snapshot.as_ref().is_some_and(|s| now < s.expires)
            && cache
                .unknown_attempt
                .is_some_and(|at| now.duration_since(at) < UNKNOWN_BACKOFF)
        {
            return Err(RefreshError::Invalid);
        }
        Ok(None)
    }
    /// Gets a fresh trusted verifier, refreshing cold/expired keys or a newly seen signing kid.
    pub async fn verifier_for(&self, token: &str) -> Result<Verifier, RefreshError> {
        tokio::time::timeout(Duration::from_secs(4), self.prepare(token))
            .await
            .map_err(|_| RefreshError::Unavailable)?
    }
    async fn prepare(&self, token: &str) -> Result<Verifier, RefreshError> {
        let kid = token_kid(token)?;
        if let Some(v) = self.cached(&kid, Instant::now(), false)? {
            return Ok(v);
        }
        let _single_flight = self.refresh.lock().await;
        let now = Instant::now();
        if let Some(v) = self.cached(&kid, now, true)? {
            return Ok(v);
        }
        {
            let mut cache = self.cache.lock().map_err(|_| RefreshError::Unavailable)?;
            // Cold/expiry refresh also counts as an unknown-kid attempt: a forged kid cannot
            // force an immediate second fetch after a cold load containing other valid keys.
            cache.unknown_attempt = Some(now);
            // Set before awaiting: cancellation/timeout also retains a retry cooldown.
            cache.failed_at = Some(now);
        }
        match self.fetch().await {
            Ok(snapshot) => {
                let known = snapshot.keys.key(&kid).is_some();
                let verifier = snapshot.verifier.clone();
                let mut cache = self.cache.lock().map_err(|_| RefreshError::Unavailable)?;
                cache.snapshot = Some(snapshot);
                cache.failed_at = None;
                if known {
                    Ok(verifier)
                } else {
                    Err(RefreshError::Invalid)
                }
            }
            Err(_) => {
                self.cache
                    .lock()
                    .map_err(|_| RefreshError::Unavailable)?
                    .failed_at = Some(Instant::now());
                Err(RefreshError::Unavailable)
            }
        }
    }
    async fn fetch(&self) -> Result<Snapshot, ()> {
        let started = Instant::now();
        let mut response = self.client.get(&self.url).send().await.map_err(|_| ())?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(());
        }
        let control = response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .ok_or(())?;
        let age = response
            .headers()
            .get(reqwest::header::AGE)
            .map(|v| {
                v.to_str()
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or(())
            })
            .transpose()?
            .unwrap_or(0);
        let ttl = ttl(control, age)?;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return Err(());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
        let value: serde_json::Value = serde_json::from_str(text).map_err(|_| ())?;
        let entries = value.get("keys").and_then(|v| v.as_array()).ok_or(())?;
        if entries.is_empty() || entries.len() > 16 {
            return Err(());
        }
        let mut kids = BTreeSet::new();
        for key in entries {
            let kid = key.get("kid").and_then(|v| v.as_str()).ok_or(())?;
            if kid.is_empty()
                || kid.len() > 128
                || !kids.insert(kid)
                || key["kty"] != "RSA"
                || key.get("alg").is_some_and(|v| v != "RS256")
                || key.get("use").is_some_and(|v| v != "sig")
            {
                return Err(());
            }
        }
        let keys = Arc::new(StaticKeys::from_jwks(text).map_err(|_| ())?);
        let verifier = Verifier::new(Mode::Rs256(keys.clone()), &self.project, &self.providers)
            .map_err(|_| ())?;
        let expires = started + ttl;
        if expires <= Instant::now() {
            return Err(());
        }
        Ok(Snapshot {
            keys,
            verifier,
            expires,
        })
    }
}
fn token_kid(token: &str) -> Result<String, RefreshError> {
    #[derive(Deserialize)]
    struct Header {
        alg: String,
        kid: String,
    }
    if token.len() > 16 * 1024 {
        return Err(RefreshError::Invalid);
    }
    let parts: Vec<_> = token.splitn(4, '.').collect();
    if parts.len() != 3 || parts.iter().any(|s| s.is_empty()) || parts[0].len() > 2048 {
        return Err(RefreshError::Invalid);
    }
    let payload = fi_auth::b64::url_decode(parts[1]).ok_or(RefreshError::Invalid)?;
    if !serde_json::from_slice::<serde_json::Value>(&payload).is_ok_and(|value| value.is_object())
        || fi_auth::b64::url_decode(parts[2]).is_none()
    {
        return Err(RefreshError::Invalid);
    }
    let bytes = fi_auth::b64::url_decode(parts[0]).ok_or(RefreshError::Invalid)?;
    let h: Header = serde_json::from_slice(&bytes).map_err(|_| RefreshError::Invalid)?;
    if h.alg != "RS256"
        || h.kid.is_empty()
        || h.kid.len() > 128
        || !h.kid.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(RefreshError::Invalid);
    }
    Ok(h.kid)
}
fn ttl(control: &str, age: u64) -> Result<Duration, ()> {
    let mut max_age = None;
    for directive in control.split(',').map(str::trim) {
        if directive.eq_ignore_ascii_case("no-store") || directive.eq_ignore_ascii_case("no-cache")
        {
            return Err(());
        }
        if let Some((key, value)) = directive.split_once('=')
            && key.eq_ignore_ascii_case("max-age")
        {
            if max_age.is_some() {
                return Err(());
            }
            max_age = Some(value.trim_matches('"').parse::<u64>().map_err(|_| ())?);
        }
    }
    let remaining = max_age
        .ok_or(())?
        .checked_sub(age)
        .filter(|n| *n > 0)
        .ok_or(())?;
    Ok(Duration::from_secs(remaining.min(86_400)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn token(kid: &str) -> String {
        format!(
            "{}.e30.c2ln",
            fi_auth::b64::url_encode(format!(r#"{{"alg":"RS256","kid":"{kid}"}}"#).as_bytes())
        )
    }
    fn document(kid: &str) -> String {
        format!(
            r#"{{"keys":[{{"kid":"{kid}","kty":"RSA","alg":"RS256","use":"sig","n":"{}","e":"AQAB"}}]}}"#,
            fi_auth::b64::url_encode(&[3; 256])
        )
    }
    async fn server(
        body: String,
        status: u16,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = count.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0; 4096];
                let _ = socket.read(&mut buf).await;
                if body == "__HANG__" {
                    std::future::pending::<()>().await;
                }
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nCache-Control: public, max-age=3600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), count, task)
    }
    #[test]
    fn cache_control_age_and_malformed_tokens_fail_closed() {
        assert_eq!(
            ttl("public,max-age=3600", 30),
            Ok(Duration::from_secs(3570))
        );
        for header in [
            "no-store,max-age=60",
            "no-cache,max-age=60",
            "max-age=0",
            "max-age=1,max-age=2",
            "public",
        ] {
            assert!(ttl(header, 0).is_err());
        }
        assert!(ttl("max-age=10", 10).is_err());
        assert!(token_kid("none.e30.").is_err());
        assert!(token_kid(&"x".repeat(20_000)).is_err());
    }
    #[tokio::test]
    async fn refresh_is_single_flight_and_unknown_kids_are_bounded() {
        let (url, count, task) = server(document("k1"), 200).await;
        let remote =
            Arc::new(RotatingVerifier::new("project", &["password".into()], &url, false).unwrap());
        let a = remote.clone();
        let b = remote.clone();
        let t = token("k1");
        let (x, y) = tokio::join!(a.verifier_for(&t), b.verifier_for(&t));
        assert!(x.is_ok() && y.is_ok());
        assert_eq!(
            remote.verifier_for(&token("forged")).await.err(),
            Some(RefreshError::Invalid)
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        remote.cache.lock().unwrap().unknown_attempt = Some(Instant::now() - UNKNOWN_BACKOFF);
        assert_eq!(
            remote.verifier_for(&token("rotated")).await.err(),
            Some(RefreshError::Invalid)
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        task.abort();
    }
    #[tokio::test]
    async fn expired_keys_and_failed_fetches_are_never_trusted() {
        let (url, count, task) = server(document("k1"), 200).await;
        let remote = RotatingVerifier::new("project", &["password".into()], &url, false).unwrap();
        assert!(remote.verifier_for(&token("k1")).await.is_ok());
        remote
            .cache
            .lock()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        task.abort();
        assert_eq!(
            remote.verifier_for(&token("k1")).await.err(),
            Some(RefreshError::Unavailable)
        );
        assert_eq!(
            remote.verifier_for(&token("k1")).await.err(),
            Some(RefreshError::Unavailable)
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn invalid_or_empty_key_sets_and_redirects_are_refused() {
        for (body, status) in [
            ("{\"keys\":[]}".to_owned(), 200),
            (document("k1"), 302),
            ("x".repeat(MAX_BYTES + 1), 200),
        ] {
            let (url, _, task) = server(body, status).await;
            let remote =
                RotatingVerifier::new("project", &["password".into()], &url, false).unwrap();
            assert_eq!(
                remote.verifier_for(&token("k1")).await.err(),
                Some(RefreshError::Unavailable)
            );
            task.abort();
        }
    }
    #[tokio::test]
    async fn rotating_keys_verify_real_rs256_signatures_and_expiry() {
        use rsa::{pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
        let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let public = private.to_public_key();
        let jwk = |kid: &str| {
            format!(
                r#"{{"keys":[{{"kid":"{kid}","kty":"RSA","alg":"RS256","n":"{}","e":"{}"}}]}}"#,
                fi_auth::b64::url_encode(&public.n().to_bytes_be()),
                fi_auth::b64::url_encode(&public.e().to_bytes_be())
            )
        };
        let now = 1_792_584_000i64;
        let claims = serde_json::json!({"iss":"https://securetoken.google.com/project","aud":"project","sub":"synthetic-user","iat":now,"auth_time":now,"exp":now+3600,"email_verified":true,"firebase":{"sign_in_provider":"password"}});
        let encoding =
            jsonwebtoken::EncodingKey::from_rsa_der(private.to_pkcs1_der().unwrap().as_bytes());
        let signed = |kid: &str| {
            let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
            h.kid = Some(kid.into());
            jsonwebtoken::encode(&h, &claims, &encoding).unwrap()
        };
        let (url, _, task) = server(jwk("k2"), 200).await;
        let remote = RotatingVerifier::new("project", &["password".into()], &url, false).unwrap();
        // A previously cached k1 is replaced by Google's next response containing k2.
        let old = Arc::new(StaticKeys::from_jwks(&jwk("k1")).unwrap());
        remote.cache.lock().unwrap().snapshot = Some(Snapshot {
            keys: old.clone(),
            verifier: Verifier::new(Mode::Rs256(old), "project", &["password".into()]).unwrap(),
            expires: Instant::now() + Duration::from_secs(60),
        });
        let token = signed("k2");
        let verifier = remote.verifier_for(&token).await.unwrap();
        assert_eq!(verifier.verify(&token, now).unwrap().uid, "synthetic-user");
        assert!(verifier.verify(&signed("k1"), now).is_err());
        let forged = format!(
            "{}.{}.{}",
            token.split('.').next().unwrap(),
            fi_auth::b64::url_encode(b"{}"),
            token.rsplit('.').next().unwrap()
        );
        assert!(verifier.verify(&forged, now).is_err());
        task.abort();
    }
    #[tokio::test]
    async fn cancelled_refresh_retains_retry_backoff() {
        let (url, count, server) = server("__HANG__".into(), 200).await;
        let remote =
            Arc::new(RotatingVerifier::new("project", &["password".into()], &url, false).unwrap());
        let first = remote.clone();
        let refresh = tokio::spawn(async move { first.verifier_for(&token("k1")).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        refresh.abort();
        let _ = refresh.await;
        assert_eq!(
            remote.verifier_for(&token("k1")).await.err(),
            Some(RefreshError::Unavailable)
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
    }
}
