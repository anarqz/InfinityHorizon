//! Cloud Storage adapter for save bundles (CLOUD_SAVES.md §6, ADR 0005): [`GcsBlobStore`] is the
//! `fi_saves::BlobStore` over the Cloud Storage JSON API. Objects live under
//! `blobs/sha256/<hex>` in `CLOUD_SAVES_BUCKET`, are written create-only (`ifGenerationMatch=0`),
//! and are never overwritten here; only garbage collection deletes (`delete`, CLOUD_SAVES.md §6).
//!
//! Locally `STORAGE_EMULATOR_HOST` names fake-gcs-server (compose service `fake-gcs`), with no
//! credentials. Without it the adapter talks to `https://storage.googleapis.com` as the instance's
//! service account: an OAuth access token from the metadata server
//! (`http://metadata.google.internal/.../service-accounts/default/token`, `Metadata-Flavor:
//! Google`; Cloud Run and Compute Engine), cached per instance until 5 minutes before it expires.
//! The token is never logged and never leaves the `Authorization` header.
//!
//! - `put`: `POST {endpoint}/upload/storage/v1/b/{bucket}/o?uploadType=media&name=<name>&ifGenerationMatch=0`
//!   with the bytes. `412 Precondition Failed` means the object already exists: content addressing
//!   makes that a success (the bytes are the same by construction, and readers re-hash anyway).
//! - `get`: `GET {endpoint}/storage/v1/b/{bucket}/o/<name>?alt=media`. A missing object and any
//!   other failure are `SaveError::Unavailable` (retryable for the client; the save service
//!   re-hashes what it gets, so a corrupted object is reported, never served).
//!
//! - `delete` (garbage collection only, `fi-cli saves-gc`; CLOUD_SAVES.md §6): `DELETE
//!   {endpoint}/storage/v1/b/{bucket}/o/<name>`. `404` is success (already gone). Not part of
//!   `fi_saves::BlobStore`, so no service path can call it; only `fi-jobs` holds the permission.
//!
//! Errors and logs name the digest's object, never bytes; the client has a 10 s timeout so a stuck
//! emulator can never hold a request (or the commit behind it) forever.

use fi_saves::{BlobStore, Digest, SaveError};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Why the adapter cannot be configured.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// A setting is missing or malformed (never echoes a value).
    #[error("storage configuration: {0}")]
    Config(String),
}

/// What a JSON API path segment or query value may carry unencoded (RFC 3986 unreserved).
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Per-request timeout of the HTTP client.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Cloud Storage's JSON API.
pub const PRODUCTION_ENDPOINT: &str = "https://storage.googleapis.com";
/// The metadata server's access token of the instance's service account.
pub const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
/// A cached token is refreshed this long before it expires.
pub const TOKEN_MARGIN: Duration = Duration::from_secs(300);

/// Where the requests' credentials come from.
#[derive(Clone)]
enum Credentials {
    /// None: the emulator.
    None,
    /// The metadata server's token, cached (per-instance derived state).
    Metadata {
        url: String,
        cached: Arc<Mutex<Option<(String, Instant)>>>,
    },
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::None => f.write_str("None"),
            Credentials::Metadata { url, .. } => write!(f, "Metadata({url}, ***)"),
        }
    }
}

#[derive(serde::Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

/// The cloud-saves bucket over the JSON API.
#[derive(Clone, Debug)]
pub struct GcsBlobStore {
    endpoint: String,
    bucket: String,
    http: reqwest::Client,
    credentials: Credentials,
}

impl GcsBlobStore {
    /// Reads `CLOUD_SAVES_BUCKET` and `STORAGE_EMULATOR_HOST`: the emulator when it is set, else
    /// Cloud Storage with the metadata server's credentials.
    pub fn from_env() -> Result<Self, StorageError> {
        let bucket = std::env::var("CLOUD_SAVES_BUCKET")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| StorageError::Config("CLOUD_SAVES_BUCKET is required".into()))?;
        Ok(
            match std::env::var("STORAGE_EMULATOR_HOST")
                .ok()
                .filter(|v| !v.trim().is_empty())
            {
                Some(endpoint) => Self::new(endpoint.trim(), bucket.trim()),
                None => Self::cloud(bucket.trim()),
            },
        )
    }

    /// A store over the emulator at `endpoint` (`http://host:port`, no trailing slash) and
    /// `bucket`, without credentials.
    pub fn new(endpoint: impl Into<String>, bucket: impl Into<String>) -> Self {
        Self::with_credentials(endpoint, bucket, Credentials::None)
    }

    /// Cloud Storage as the instance's service account (Cloud Run).
    pub fn cloud(bucket: impl Into<String>) -> Self {
        Self::with_metadata(PRODUCTION_ENDPOINT, METADATA_TOKEN_URL, bucket)
    }

    /// `endpoint` with tokens from `token_url` (tests stand in for both).
    pub fn with_metadata(
        endpoint: impl Into<String>,
        token_url: impl Into<String>,
        bucket: impl Into<String>,
    ) -> Self {
        Self::with_credentials(
            endpoint,
            bucket,
            Credentials::Metadata {
                url: token_url.into(),
                cached: Arc::new(Mutex::new(None)),
            },
        )
    }

    fn with_credentials(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        credentials: Credentials,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            bucket: bucket.into(),
            http,
            credentials,
        }
    }

    /// Whether requests carry the metadata server's credentials (not the emulator).
    pub fn uses_metadata_credentials(&self) -> bool {
        matches!(self.credentials, Credentials::Metadata { .. })
    }

    /// The `Authorization` value of the next request, if any (one metadata round trip when the
    /// cached token is missing or near its end; no lock is held across it).
    async fn authorization(&self) -> Result<Option<String>, String> {
        let Credentials::Metadata { url, cached } = &self.credentials else {
            return Ok(None);
        };
        let now = Instant::now();
        if let Some((token, until)) = cached.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
            && now < *until
        {
            return Ok(Some(format!("Bearer {token}")));
        }
        let response = self
            .http
            .get(url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| format!("metadata token: {}", without_url(&e)))?;
        if !response.status().is_success() {
            return Err(format!(
                "metadata token: HTTP {}",
                response.status().as_u16()
            ));
        }
        let token: MetadataToken = response
            .json()
            .await
            .map_err(|_| "metadata token: unreadable answer".to_owned())?;
        let until = now + Duration::from_secs(token.expires_in).saturating_sub(TOKEN_MARGIN);
        let value = format!("Bearer {}", token.access_token);
        *cached.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.access_token, until));
        Ok(Some(value))
    }

    /// The bucket.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The API endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The object name of a digest (`blobs/sha256/<hex>`, CLOUD_SAVES.md §6).
    pub fn object_name(digest: Digest) -> String {
        format!("blobs/sha256/{}", digest.hex())
    }

    fn upload_url(&self, digest: Digest) -> String {
        format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}&ifGenerationMatch=0",
            self.endpoint,
            utf8_percent_encode(&self.bucket, COMPONENT),
            utf8_percent_encode(&Self::object_name(digest), COMPONENT),
        )
    }

    fn object_url(&self, digest: Digest) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.endpoint,
            utf8_percent_encode(&self.bucket, COMPONENT),
            utf8_percent_encode(&Self::object_name(digest), COMPONENT),
        )
    }

    fn media_url(&self, digest: Digest) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}?alt=media",
            self.endpoint,
            utf8_percent_encode(&self.bucket, COMPONENT),
            utf8_percent_encode(&Self::object_name(digest), COMPONENT),
        )
    }
}

fn unavailable(what: &str, digest: Digest, detail: impl std::fmt::Display) -> SaveError {
    SaveError::Unavailable(format!(
        "blob {what} of {} failed: {detail}",
        GcsBlobStore::object_name(digest)
    ))
}

impl BlobStore for GcsBlobStore {
    async fn put(&self, digest: Digest, bytes: &[u8]) -> Result<(), SaveError> {
        let auth = self
            .authorization()
            .await
            .map_err(|e| unavailable("put", digest, e))?;
        let mut request = self.http.post(self.upload_url(digest));
        if let Some(a) = auth {
            request = request.header("authorization", a);
        }
        let response = request
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| unavailable("put", digest, without_url(&e)))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            // ifGenerationMatch=0 and the object exists: the same bytes are already stored.
            412 => Ok(()),
            status => Err(unavailable("put", digest, format!("HTTP {status}"))),
        }
    }

    async fn get(&self, digest: Digest) -> Result<Vec<u8>, SaveError> {
        self.get_if_present(digest)
            .await?
            .ok_or_else(|| unavailable("get", digest, "HTTP 404"))
    }
}

impl GcsBlobStore {
    /// Deletes the object of `digest` (garbage collection only); an absent object is success.
    pub async fn delete(&self, digest: Digest) -> Result<(), SaveError> {
        let auth = self
            .authorization()
            .await
            .map_err(|e| unavailable("delete", digest, e))?;
        let mut request = self.http.delete(self.object_url(digest));
        if let Some(a) = auth {
            request = request.header("authorization", a);
        }
        let response = request
            .send()
            .await
            .map_err(|e| unavailable("delete", digest, without_url(&e)))?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            status => Err(unavailable("delete", digest, format!("HTTP {status}"))),
        }
    }

    /// The bytes stored under `digest`, `None` when the bucket answers 404 (a blob collected or
    /// never stored: the save audit records it instead of fetching it again every run).
    pub async fn get_if_present(&self, digest: Digest) -> Result<Option<Vec<u8>>, SaveError> {
        let auth = self
            .authorization()
            .await
            .map_err(|e| unavailable("get", digest, e))?;
        let mut request = self.http.get(self.media_url(digest));
        if let Some(a) = auth {
            request = request.header("authorization", a);
        }
        let response = request
            .send()
            .await
            .map_err(|e| unavailable("get", digest, without_url(&e)))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if !(200..=299).contains(&status) {
            return Err(unavailable("get", digest, format!("HTTP {status}")));
        }
        response
            .bytes()
            .await
            .map(|b| Some(b.to_vec()))
            .map_err(|e| unavailable("get", digest, without_url(&e)))
    }
}

/// A transport error without its URL (the URL is fine to log, but keep messages short and stable).
fn without_url(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        "connection refused or unreachable".into()
    } else {
        "transport error".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_names_follow_the_schema_check() {
        let d = Digest::of(b"abc");
        let name = GcsBlobStore::object_name(d);
        assert_eq!(name, format!("blobs/sha256/{}", d.hex()));
        assert_eq!(
            GcsBlobStore::new("http://fake-gcs:4453/", "fi-dev-cloud-saves").endpoint(),
            "http://fake-gcs:4453"
        );
    }

    #[test]
    fn urls_encode_the_object_name() {
        let store = GcsBlobStore::new("http://127.0.0.1:4453", "fi-dev-cloud-saves");
        let d = Digest::of(b"abc");
        assert_eq!(
            store.upload_url(d),
            format!(
                "http://127.0.0.1:4453/upload/storage/v1/b/fi-dev-cloud-saves/o?uploadType=media&name=blobs%2Fsha256%2F{}&ifGenerationMatch=0",
                d.hex()
            )
        );
        assert_eq!(
            store.object_url(d),
            format!(
                "http://127.0.0.1:4453/storage/v1/b/fi-dev-cloud-saves/o/blobs%2Fsha256%2F{}",
                d.hex()
            )
        );
        assert_eq!(
            store.media_url(d),
            format!(
                "http://127.0.0.1:4453/storage/v1/b/fi-dev-cloud-saves/o/blobs%2Fsha256%2F{}?alt=media",
                d.hex()
            )
        );
    }

    #[tokio::test]
    async fn cloud_requests_carry_the_metadata_token_cached_until_near_its_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // One local listener stands in for the metadata server (/token) and Cloud Storage.
        let tokens = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = tokens.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let served = served.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65_536];
                    let mut len = 0;
                    loop {
                        let n = socket.read(&mut buf[len..]).await.unwrap_or(0);
                        len += n;
                        let text = String::from_utf8_lossy(&buf[..len]).to_string();
                        if n == 0 || text.contains("\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..len]).to_ascii_lowercase();
                    let (status, body) = if head.starts_with("get /token") {
                        assert!(head.contains("metadata-flavor: google"), "{head}");
                        let n = served.fetch_add(1, Ordering::SeqCst);
                        // The first token expires inside the refresh margin: never reused.
                        let expires = if n == 0 { 60 } else { 3600 };
                        (
                            "200 OK",
                            format!(
                                r#"{{"access_token":"tok-{n}","expires_in":{expires},"token_type":"Bearer"}}"#
                            ),
                        )
                    } else if head.contains(&format!(
                        "authorization: bearer tok-{}",
                        served.load(Ordering::SeqCst).saturating_sub(1)
                    )) {
                        ("200 OK", "blob".to_owned())
                    } else {
                        ("401 Unauthorized", String::new())
                    };
                    let answer = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        let store = GcsBlobStore::with_metadata(
            format!("http://{addr}"),
            format!("http://{addr}/token"),
            "fi-test-bucket",
        );
        assert!(store.uses_metadata_credentials());
        assert!(!format!("{store:?}").contains("tok-"));
        let d = Digest::of(b"blob");
        assert_eq!(store.get(d).await.unwrap(), b"blob");
        assert_eq!(store.get(d).await.unwrap(), b"blob");
        assert_eq!(
            tokens.load(Ordering::SeqCst),
            2,
            "a token inside the margin is fetched again"
        );
        store.get(d).await.unwrap();
        assert_eq!(tokens.load(Ordering::SeqCst), 2, "a fresh token is reused");
        assert!(!format!("{store:?}").contains("tok-"));
        // The emulator never asks for a token.
        assert!(!GcsBlobStore::new(format!("http://{addr}"), "b").uses_metadata_credentials());
    }

    fn emulator() -> GcsBlobStore {
        GcsBlobStore::from_env().expect(
            "STORAGE_EMULATOR_HOST and CLOUD_SAVES_BUCKET (docker compose up -d --wait fake-gcs)",
        )
    }

    /// Unique bytes per run, so a rerun on the same emulator volume starts from a missing object.
    fn fresh_bytes() -> Vec<u8> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!(
            "fi-storage test blob {nanos} {:?}",
            std::thread::current().id()
        )
        .into_bytes()
    }

    #[tokio::test]
    #[ignore = "needs fake-gcs: docker compose up -d --wait fake-gcs, STORAGE_EMULATOR_HOST, CLOUD_SAVES_BUCKET"]
    async fn put_is_create_only_and_idempotent_and_get_returns_the_bytes() {
        let store = emulator();
        let bytes = fresh_bytes();
        let digest = Digest::of(&bytes);
        assert!(
            matches!(store.get(digest).await, Err(SaveError::Unavailable(_))),
            "missing before the put"
        );
        store.put(digest, &bytes).await.expect("first put");
        store
            .put(digest, &bytes)
            .await
            .expect("second put of the same digest is a no-op (412 is success)");
        assert_eq!(store.get(digest).await.unwrap(), bytes);
    }

    #[tokio::test]
    #[ignore = "needs fake-gcs: docker compose up -d --wait fake-gcs, STORAGE_EMULATOR_HOST, CLOUD_SAVES_BUCKET"]
    async fn a_missing_object_or_a_dead_endpoint_is_unavailable() {
        let store = emulator();
        let digest = Digest::of(&fresh_bytes());
        assert!(matches!(
            store.get(digest).await,
            Err(SaveError::Unavailable(_))
        ));
        let dead = GcsBlobStore::new("http://127.0.0.1:9", store.bucket());
        assert!(matches!(
            dead.put(digest, b"x").await,
            Err(SaveError::Unavailable(_))
        ));
        assert!(matches!(
            dead.get(digest).await,
            Err(SaveError::Unavailable(_))
        ));
    }
}

/// Save blobs on the local file system (`FI_BLOB_BACKEND=fs`, OD-122 follow-up "two backends",
/// ADR 0023 §9): `<dir>/blobs/sha256/<hex>`, the bucket's object names. A blob is written to a
/// temporary file in the same directory and renamed into place, so a reader sees all of it or
/// nothing; an existing object is success (content-addressed, like `ifGenerationMatch=0`).
#[derive(Clone, Debug)]
pub struct FsBlobStore {
    dir: std::path::PathBuf,
}

impl FsBlobStore {
    /// A store under `dir` (created on the first write).
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, digest: Digest) -> std::path::PathBuf {
        self.dir.join(GcsBlobStore::object_name(digest))
    }

    /// Deletes the file of `digest` (garbage collection only); an absent file is success.
    pub async fn delete(&self, digest: Digest) -> Result<(), SaveError> {
        match tokio::fs::remove_file(self.path(digest)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(unavailable("delete", digest, e.kind())),
        }
    }

    /// The bytes stored under `digest`, `None` when there is no such file.
    pub async fn get_if_present(&self, digest: Digest) -> Result<Option<Vec<u8>>, SaveError> {
        match tokio::fs::read(self.path(digest)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(unavailable("get", digest, e.kind())),
        }
    }
}

impl BlobStore for FsBlobStore {
    async fn put(&self, digest: Digest, bytes: &[u8]) -> Result<(), SaveError> {
        let path = self.path(digest);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }
        let parent = path
            .parent()
            .ok_or_else(|| unavailable("put", digest, "no parent directory"))?
            .to_path_buf();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            std::fs::create_dir_all(&parent)?;
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let tmp = parent.join(format!(".tmp-{}-{nanos}", std::process::id()));
            let result = (|| {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(&bytes)?;
                f.sync_all()?;
                std::fs::rename(&tmp, &path)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result
        })
        .await
        .map_err(|e| unavailable("put", digest, e))?
        .map_err(|e| unavailable("put", digest, e.kind()))
    }

    async fn get(&self, digest: Digest) -> Result<Vec<u8>, SaveError> {
        self.get_if_present(digest)
            .await?
            .ok_or_else(|| unavailable("get", digest, "not found"))
    }
}

/// The save blobs of an instance: the Cloud Storage bucket (`FI_BLOB_BACKEND=firebase`, the
/// default; Cloud Storage for Firebase is the same bucket) or the local file system (`fs`).
#[derive(Clone, Debug)]
pub enum Blobs {
    /// Cloud Storage, or its emulator locally.
    Gcs(GcsBlobStore),
    /// A directory (`FI_BLOB_DIR`).
    Fs(FsBlobStore),
}

impl Blobs {
    /// Reads `FI_BLOB_BACKEND` (`firebase`, the default, or `fs`): `fs` reads `FI_BLOB_DIR`
    /// (default `/data/blobs`) and is refused when `FI_ENV` or `APP_ENV` is `prod`; `firebase`
    /// reads what [`GcsBlobStore::from_env`] does.
    pub fn from_env() -> Result<Self, StorageError> {
        let var = |n: &str| std::env::var(n).ok().filter(|v| !v.trim().is_empty());
        let prod = ["FI_ENV", "APP_ENV"]
            .iter()
            .any(|n| var(n).is_some_and(|v| v.trim().eq_ignore_ascii_case("prod")));
        Self::from_parts(
            var("FI_BLOB_BACKEND").as_deref(),
            var("FI_BLOB_DIR").as_deref(),
            prod,
        )
        .and_then(|b| match b {
            Some(fs) => Ok(fs),
            None => GcsBlobStore::from_env().map(Blobs::Gcs),
        })
    }

    /// The `fs` store when `backend` says `fs` (`None`: the bucket, configured elsewhere).
    pub fn from_parts(
        backend: Option<&str>,
        dir: Option<&str>,
        prod: bool,
    ) -> Result<Option<Self>, StorageError> {
        match backend.map(str::trim) {
            None | Some("firebase") => Ok(None),
            Some("fs") if prod => Err(StorageError::Config(
                "FI_BLOB_BACKEND=fs is refused when FI_ENV or APP_ENV is prod".into(),
            )),
            Some("fs") => Ok(Some(Blobs::Fs(FsBlobStore::new(
                dir.map(str::trim).unwrap_or("/data/blobs"),
            )))),
            Some(_) => Err(StorageError::Config(
                "FI_BLOB_BACKEND must be firebase or fs".into(),
            )),
        }
    }

    /// `cloud`, `emulator` or `fs` (a `/meta` label).
    pub fn label(&self) -> &'static str {
        match self {
            Blobs::Gcs(g) if g.uses_metadata_credentials() => "cloud",
            Blobs::Gcs(_) => "emulator",
            Blobs::Fs(_) => "fs",
        }
    }

    /// Deletes the object of `digest` (garbage collection only, `fi-cli saves-gc`); absent is
    /// success.
    pub async fn delete(&self, digest: Digest) -> Result<(), SaveError> {
        match self {
            Blobs::Gcs(g) => g.delete(digest).await,
            Blobs::Fs(f) => f.delete(digest).await,
        }
    }

    /// The bytes stored under `digest`, `None` when absent.
    pub async fn get_if_present(&self, digest: Digest) -> Result<Option<Vec<u8>>, SaveError> {
        match self {
            Blobs::Gcs(g) => g.get_if_present(digest).await,
            Blobs::Fs(f) => f.get_if_present(digest).await,
        }
    }
}

impl From<GcsBlobStore> for Blobs {
    fn from(g: GcsBlobStore) -> Self {
        Blobs::Gcs(g)
    }
}

impl BlobStore for Blobs {
    async fn put(&self, digest: Digest, bytes: &[u8]) -> Result<(), SaveError> {
        match self {
            Blobs::Gcs(g) => g.put(digest, bytes).await,
            Blobs::Fs(f) => f.put(digest, bytes).await,
        }
    }

    async fn get(&self, digest: Digest) -> Result<Vec<u8>, SaveError> {
        match self {
            Blobs::Gcs(g) => g.get(digest).await,
            Blobs::Fs(f) => f.get(digest).await,
        }
    }
}

#[cfg(test)]
mod fs_tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "fi-storage-fs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn fs_puts_once_reads_back_and_reports_absent() {
        let dir = temp_dir();
        let store = Blobs::Fs(FsBlobStore::new(&dir));
        let bytes = b"synthetic save bytes".to_vec();
        let digest = Digest::of(&bytes);
        assert_eq!(store.get_if_present(digest).await.unwrap(), None);
        assert!(store.get(digest).await.is_err());
        store.put(digest, &bytes).await.unwrap();
        // A second put of the same content is success and leaves the object as it was.
        store.put(digest, &bytes).await.unwrap();
        assert_eq!(store.get(digest).await.unwrap(), bytes);
        let path = dir.join(GcsBlobStore::object_name(digest));
        assert!(path.is_file());
        // No temporary file is left beside it.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![digest.hex()]);
        assert_eq!(store.label(), "fs");
        // Garbage collection deletes it; a second delete of the absent object is success.
        store.delete(digest).await.unwrap();
        assert_eq!(store.get_if_present(digest).await.unwrap(), None);
        store.delete(digest).await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_backend_setting() {
        assert!(Blobs::from_parts(None, None, true).unwrap().is_none());
        assert!(
            Blobs::from_parts(Some("firebase"), None, true)
                .unwrap()
                .is_none()
        );
        assert!(Blobs::from_parts(Some("fs"), None, true).is_err());
        assert!(Blobs::from_parts(Some("s3"), None, false).is_err());
        match Blobs::from_parts(Some("fs"), None, false).unwrap() {
            Some(Blobs::Fs(f)) => assert_eq!(f.dir(), std::path::Path::new("/data/blobs")),
            other => panic!("{other:?}"),
        }
    }
}
