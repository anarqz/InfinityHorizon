//! End-to-end flows over the real router: Postgres as a `fi_runtime` login (never the owner, so a
//! missing grant fails here as in production), ID tokens minted by the Firebase Auth emulator
//! (Identity Platform locally, project `demo-forza-infinity`) and, for saves, the fake-gcs bucket.
//! The identity flows of SER-1.1, then the local environment v0's platform (workstream A): play
//! sessions with admission and the queue, three commits with CAS, replay and conflict, the
//! re-hashed download, the game plane of a pool (session, progression, presence, wrong pool),
//! takeover fencing, `/internal/v1/pools` and the local entitlement stand-in. Since OD-52 the
//! sessions and the save line are a Supporter's Hardcore profile's: its line starts with the
//! server's fresh career (written here as the system actor until SER-2.3 builds it), and the
//! aliases refuse every other profile (`SAVES.NOT_ELIGIBLE`).
//!
//! ```sh
//! docker compose up -d --wait postgres auth-emulator fake-gcs
//! FI_TEST_DATABASE_URL=postgres://fi_migrator:fi_migrator_local@127.0.0.1:5452/forza_infinity_test \
//! FIREBASE_AUTH_EMULATOR_HOST=127.0.0.1:9399 STORAGE_EMULATOR_HOST=http://127.0.0.1:4453 \
//! CLOUD_SAVES_BUCKET=fi-dev-cloud-saves cargo test -p fi-api -- --include-ignored
//! ```
//!
//! Every run uses fresh emulator users, devices, client networks and pools, so the shared rate
//! limits, idempotency records and sessions of earlier runs never interfere.
//!
//! The `local` sign-in backend's flows (OD-122 M2.3, the end of this file) need Postgres only.

use crate::app::{self, Config};
use crate::config::{self, IdentityConfig};
use crate::state::AppState;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, request};
use fi_db::{PgPool, migrate};
use fi_domain::{PoolName, PoolRouting};
use fi_protocol::routes::{game as gr, internal as ir, launcher as lr, portal as pr};
use fi_runtime::ServiceInfo;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::io::{Read, Write};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

const LOGIN: &str = "fi_app_api_e2e";
const PROJECT: &str = "demo-forza-infinity";
const REDIRECT: &str = "http://127.0.0.1:53682/callback";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set: docker compose up -d --wait postgres auth-emulator (see the file header)")
    })
}

fn password() -> &'static str {
    static PASSWORD: OnceLock<String> = OnceLock::new();
    PASSWORD.get_or_init(|| format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple()))
}

fn fresh() -> String {
    Uuid::now_v7().simple().to_string()
}

/// A random private address: this run's client network (bytes of the UUIDv7 random part).
fn random_ip() -> String {
    let b = Uuid::now_v7().into_bytes();
    format!("10.{}.{}.{}", b[10], b[11], b[12])
}

struct Stack {
    app: Router,
    emulator: String,
    portal_ip: String,
    launcher_ip: String,
    /// The migrator (pool rows, the entitlement stand-in, row checks).
    owner: PgPool,
    /// The runtime login the routers use.
    pool: PgPool,
    identity: Arc<IdentityConfig>,
}

/// This run's two pools (tier vip and free), routed `vip -> vip`, `free -> free`.
struct Pools {
    vip: String,
    free: String,
}

async fn stack() -> Stack {
    stack_with("launcher,portal", None).await
}

async fn stack_with(surfaces: &str, pools: Option<&Pools>) -> Stack {
    stack_sized(surfaces, pools, 8).await
}

/// [`stack_with`] whose runtime pool holds `connections` connections (acquire timeout 2 s).
async fn stack_sized(surfaces: &str, pools: Option<&Pools>, connections: u32) -> Stack {
    stack_env(surfaces, pools, connections, &[]).await
}

/// [`stack_sized`] with `extra` identity settings on top of the local ones.
async fn stack_env(
    surfaces: &str,
    pools: Option<&Pools>,
    connections: u32,
    extra: &[(&'static str, &str)],
) -> Stack {
    let emulator = env("FIREBASE_AUTH_EMULATOR_HOST");
    let (url, pool) = runtime_pool(connections).await;
    let mut vars = config::tests::local();
    vars.retain(|(k, _)| *k != "FIREBASE_AUTH_EMULATOR_HOST");
    vars.push(("FIREBASE_AUTH_EMULATOR_HOST", emulator.clone()));
    for (k, v) in extra {
        vars.retain(|(name, _)| name != k);
        vars.push((k, (*v).to_owned()));
    }
    stack_of(url, pool, emulator, &vars, surfaces, pools).await
}

/// The migrated test database and a pool of the runtime login holding `connections` connections
/// (acquire timeout 2 s).
async fn runtime_pool(connections: u32) -> (String, PgPool) {
    let url = env("FI_TEST_DATABASE_URL");
    {
        static READY: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);
        let mut ready = READY.lock().await;
        if !*ready {
            let owner = PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .expect("connect to the test database");
            migrate::run(&owner).await.expect("migrations apply");
            migrate::ensure_login(&owner, LOGIN, password(), &["fi_runtime"])
                .await
                .expect("runtime login");
            *ready = true;
        }
    }
    let options = PgConnectOptions::from_str(&url)
        .unwrap()
        .username(LOGIN)
        .password(password());
    let pool = PgPoolOptions::new()
        .max_connections(connections)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with(options)
        .await
        .expect("connect as the runtime login");
    (url, pool)
}

/// The router over `pool` with the identity settings `vars`.
async fn stack_of(
    url: String,
    pool: PgPool,
    emulator: String,
    vars: &Vec<(&'static str, String)>,
    surfaces: &str,
    pools: Option<&Pools>,
) -> Stack {
    let identity =
        Arc::new(IdentityConfig::from_env(vars, true).expect("local identity configuration"));
    let mut config = Config::parse(surfaces, None).unwrap();
    if let Some(pools) = pools {
        let p = &mut config.platform;
        p.routing = PoolRouting {
            vip: PoolName::new(&pools.vip).unwrap(),
            free: PoolName::new(&pools.free).unwrap(),
        };
        p.local_admin = true;
        p.instance_id = "e2e-account-1".into();
        p.session_keys = identity.access_keys.signing().cloned();
        p.blobs = Some(fi_storage::Blobs::from_env().expect(
            "STORAGE_EMULATOR_HOST and CLOUD_SAVES_BUCKET (docker compose up -d --wait fake-gcs)",
        ));
    }
    let state = AppState {
        pool: pool.clone(),
        identity: Some(identity.clone()),
    };
    let owner = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect to the test database");
    Stack {
        app: app::router(ServiceInfo::from_env("fi-api", "0.0.0"), state, &config),
        emulator,
        portal_ip: random_ip(),
        launcher_ip: random_ip(),
        owner,
        pool,
        identity,
    }
}

/// One JSON POST to the emulator's REST API (plain HTTP/1.0, so the answer is not chunked).
fn emulator_post(host: &str, path: &str, body: &Value, owner: bool) -> Value {
    let body = body.to_string();
    let auth = if owner {
        "Authorization: Bearer owner\r\n"
    } else {
        ""
    };
    let request = format!(
        "POST {path} HTTP/1.0\r\nHost: {host}\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = std::net::TcpStream::connect(host).expect("the auth emulator answers");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, json) = response.split_once("\r\n\r\n").expect("an HTTP answer");
    assert!(head.starts_with("HTTP/1.1 200"), "emulator {path}: {head}");
    serde_json::from_str(json).expect("emulator JSON")
}

/// An e-mail/password user of the emulator; `verified` marks the e-mail verified (as the
/// verification link would) before signing in again. Returns `(uid, idToken)`.
fn emulator_user(host: &str, verified: bool) -> (String, String) {
    let base = "/identitytoolkit.googleapis.com/v1";
    let email = format!("e2e-{}@example.test", fresh());
    let credentials =
        json!({"email": email, "password": "synthetic-pass-1", "returnSecureToken": true});
    let created = emulator_post(
        host,
        &format!("{base}/accounts:signUp?key=local"),
        &credentials,
        false,
    );
    let uid = created["localId"].as_str().unwrap().to_owned();
    if !verified {
        return (uid, created["idToken"].as_str().unwrap().to_owned());
    }
    emulator_post(
        host,
        &format!("{base}/projects/{PROJECT}/accounts:update"),
        &json!({"localId": uid, "emailVerified": true}),
        true,
    );
    let signed = emulator_post(
        host,
        &format!("{base}/accounts:signInWithPassword?key=local"),
        &credentials,
        false,
    );
    (uid, signed["idToken"].as_str().unwrap().to_owned())
}

/// Deletes one emulator user of this run (never the project-wide reset: other suites share it).
fn emulator_delete(host: &str, uid: &str) {
    emulator_post(
        host,
        &format!("/identitytoolkit.googleapis.com/v1/projects/{PROJECT}/accounts:delete"),
        &json!({"localId": uid}),
        true,
    );
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// `token`'s claims with `edit` applied, re-encoded as an emulator token (unsigned, which is
/// exactly what emulator mode accepts), so every refusal below comes from the edited claim.
fn reforged(token: &str, edit: impl FnOnce(&mut Value)) -> String {
    let payload = token.split('.').nth(1).expect("a JWT");
    let mut claims: Value =
        serde_json::from_slice(&fi_auth::b64::url_decode(payload).expect("base64url")).unwrap();
    edit(&mut claims);
    format!(
        "{}.{}.",
        fi_auth::b64::url_encode(br#"{"alg":"none","typ":"JWT"}"#),
        fi_auth::b64::url_encode(&serde_json::to_vec(&claims).unwrap())
    )
}

fn id_token_grant(id_token: &str, device: &Value) -> Value {
    json!({"grantType": "id_token", "idToken": id_token, "device": device})
}

#[derive(Clone, Copy)]
enum Caller<'a> {
    /// The portal's server: the assertion and the visitor IP.
    Portal(Option<&'a str>),
    /// The launcher: an optional bearer access token.
    Launcher(Option<&'a str>),
    /// The portal's server without its assertion.
    Stranger,
}

impl Stack {
    async fn call(
        &self,
        method: Method,
        uri: &str,
        caller: Caller<'_>,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let bearer = match caller {
            Caller::Portal(token) => {
                req = req
                    .header("x-fi-client-assertion", config::tests::LOCAL_SECRET)
                    .header("x-fi-client-ip", &self.portal_ip);
                token
            }
            Caller::Launcher(token) => {
                req = req.header("x-forwarded-for", &self.launcher_ip);
                token
            }
            Caller::Stranger => None,
        };
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(k) = key {
            req = req.header("idempotency-key", k);
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("a JSON answer")
        };
        (status, value)
    }

    async fn post(
        &self,
        uri: &str,
        caller: Caller<'_>,
        key: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        self.call(Method::POST, uri, caller, Some(key), Some(body))
            .await
    }

    async fn get(&self, uri: &str, caller: Caller<'_>) -> (StatusCode, Value) {
        self.call(Method::GET, uri, caller, None, None).await
    }

    /// Native ID-token sign-in through the production router.
    async fn launcher_sign_in(&self, id_token: &str, device: &Value) -> (StatusCode, Value) {
        self.post(
            lr::AUTH_TOKEN,
            Caller::Launcher(None),
            &fresh(),
            id_token_grant(id_token, device),
        )
        .await
    }
}

fn device() -> Value {
    json!({"deviceId": Uuid::now_v7().to_string(), "label": "E2E PC", "platform": "windows"})
}

fn code(v: &Value) -> &str {
    v["code"].as_str().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn retired_browser_and_device_sign_in_are_not_served() {
    let st = stack().await;
    for (path, caller) in [
        ("/launcher/v1/auth/device", Caller::Launcher(None)),
        ("/portal/v1/auth/launcher-codes", Caller::Portal(None)),
        ("/portal/v1/auth/device-approvals", Caller::Portal(None)),
    ] {
        let (s, _) = st.post(path, caller, &fresh(), json!({})).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
    }
    for grant in [
        json!({"grantType": "authorization_code", "code": "retired", "codeVerifier": "retired",
               "redirectUri": REDIRECT, "device": device()}),
        json!({"grantType": "device_code", "deviceCode": "retired", "device": device()}),
    ] {
        let (s, b) = st
            .post(lr::AUTH_TOKEN, Caller::Launcher(None), &fresh(), grant)
            .await;
        assert_eq!((s, code(&b)), (StatusCode::BAD_REQUEST, "REQUEST.INVALID"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn portal_sign_up_then_launcher_sign_in_rotation_and_sign_out() {
    let st = stack().await;
    let (_, unverified) = emulator_user(&st.emulator, false);
    let (_, id_token) = emulator_user(&st.emulator, true);

    // Only the portal's server may call /portal/v1; an unverified password e-mail is refused.
    let (s, b) = st
        .post(
            pr::SESSIONS,
            Caller::Stranger,
            &fresh(),
            json!({"idToken": id_token}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "AUTH.FORBIDDEN"));
    let (s, b) = st
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &fresh(),
            json!({"idToken": unverified}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "AUTH.EMAIL_UNVERIFIED")
    );
    let (s, b) = st
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &fresh(),
            json!({"idToken": "not-a-token"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );

    // New identity: needs an account. A replay (same key, same body) answers a new ticket.
    let key = fresh();
    let (s, first) = st
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &key,
            json!({"idToken": id_token}),
        )
        .await;
    assert_eq!(
        (s, first["status"].as_str()),
        (StatusCode::CREATED, Some("needs_account")),
        "{first}"
    );
    let (s, replay) = st
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &key,
            json!({"idToken": id_token}),
        )
        .await;
    assert_eq!(
        (s, replay["status"].as_str()),
        (StatusCode::CREATED, Some("needs_account"))
    );
    assert_ne!(first["identityTicket"], replay["identityTicket"]);
    let (s, b) = st
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &key,
            json!({"idToken": unverified}),
        )
        .await;
    assert_eq!(s.as_u16(), 403, "{b}");

    // The account (no profiles yet) and a portal session.
    let (s, created) = st
        .post(
            pr::ACCOUNTS,
            Caller::Portal(None),
            &fresh(),
            json!({"identityTicket": replay["identityTicket"], "locale": "pt-BR"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let account_id = created["viewer"]["accountId"].as_str().unwrap().to_owned();
    assert_eq!(created["viewer"]["profiles"], json!([]));
    assert_eq!(created["viewer"]["access"]["tier"], "free");
    let session = created["token"].as_str().unwrap().to_owned();
    let (s, me) = st.get(pr::ME, Caller::Portal(Some(&session))).await;
    assert_eq!(
        (s, me["accountId"].as_str()),
        (StatusCode::OK, Some(account_id.as_str()))
    );
    let (s, b) = st
        .post(
            pr::ACCOUNTS,
            Caller::Portal(None),
            &fresh(),
            json!({"identityTicket": replay["identityTicket"]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.IDENTITY_TICKET_INVALID")
    );

    // Native launcher sign-in after the website created the account.
    let dev = device();
    let grant = || id_token_grant(&id_token, &dev);
    let key = fresh();
    let (s, signed) = st
        .post(lr::AUTH_TOKEN, Caller::Launcher(None), &key, grant())
        .await;
    assert_eq!(
        (s, signed["status"].as_str()),
        (StatusCode::OK, Some("signed_in")),
        "{signed}"
    );
    assert_eq!(signed["accountId"].as_str(), Some(account_id.as_str()));
    // CR-L4: the answer was lost; the replay re-issues and revokes the first family.
    let (s, again) = st
        .post(lr::AUTH_TOKEN, Caller::Launcher(None), &key, grant())
        .await;
    assert_eq!(
        (s, again["status"].as_str()),
        (StatusCode::OK, Some("signed_in")),
        "{again}"
    );
    assert_ne!(signed["refreshToken"], again["refreshToken"]);
    let (s, b) = st
        .get(lr::ME, Caller::Launcher(signed["accessToken"].as_str()))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID")
    );
    let access = again["accessToken"].as_str().unwrap().to_owned();
    let (s, me) = st.get(lr::ME, Caller::Launcher(Some(&access))).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["devices"].as_array().map(Vec::len), Some(1));
    assert_eq!(me["devices"][0]["deviceId"], dev["deviceId"]);

    // A profile; its replay answers the same profile.
    let name = format!("E2e{}", &fresh()[24..]);
    let body = json!({"game": "fh2", "name": name, "mode": "noob"});
    let key = fresh();
    let (s, profile) = st
        .post(
            lr::PROFILES,
            Caller::Launcher(Some(&access)),
            &key,
            body.clone(),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{profile}");
    let (s, replayed) = st
        .post(lr::PROFILES, Caller::Launcher(Some(&access)), &key, body)
        .await;
    assert_eq!(
        (s, &replayed["profileId"]),
        (StatusCode::CREATED, &profile["profileId"])
    );
    let (s, b) = st
        .post(
            lr::PROFILES,
            Caller::Launcher(Some(&access)),
            &key,
            json!({"game": "fh2", "name": "Other", "mode": "noob"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "REQUEST.IDEMPOTENCY_KEY_REUSED")
    );
    let (_, me) = st.get(lr::ME, Caller::Launcher(Some(&access))).await;
    assert_eq!(me["profiles"].as_array().map(Vec::len), Some(1));
    assert_eq!(me["profiles"][0]["mode"], "noob");

    // Refresh rotation; presenting a rotated token revokes the whole family.
    let r2 = again["refreshToken"].clone();
    let (s, pair) = st
        .post(
            lr::TOKEN,
            Caller::Launcher(None),
            &fresh(),
            json!({"refreshToken": r2}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{pair}");
    let (s, b) = st
        .post(
            lr::TOKEN,
            Caller::Launcher(None),
            &fresh(),
            json!({"refreshToken": r2}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.REFRESH_REUSED")
    );
    let (s, _) = st
        .post(
            lr::TOKEN,
            Caller::Launcher(None),
            &fresh(),
            json!({"refreshToken": pair["refreshToken"]}),
        )
        .await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "the successor dies with its family"
    );

    // A new sign-in on the same device, then sign-out.
    let (s, fresh_in) = st.launcher_sign_in(&id_token, &dev).await;
    assert_eq!(s, StatusCode::OK, "{fresh_in}");
    let access = fresh_in["accessToken"].as_str().unwrap().to_owned();
    let (s, _) = st
        .call(
            Method::POST,
            lr::SIGN_OUT,
            Caller::Launcher(Some(&access)),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = st.get(lr::ME, Caller::Launcher(Some(&access))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Portal sign-out.
    let (s, _) = st
        .call(
            Method::DELETE,
            pr::SESSIONS_CURRENT,
            Caller::Portal(Some(&session)),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = st.get(pr::ME, Caller::Portal(Some(&session))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "PORTAL.SESSION_REQUIRED")
    );
}

/// A request holds at most one pooled connection at a time: with a pool of ONE connection the
/// whole native launcher sign-in (exchange, account creation, sign-in again) still
/// answers. A handler that takes a second connection while its transaction holds the first waits
/// for itself until the acquire timeout and answers 503 here; under a burst that is the 503 storm
/// of review finding R2 (docs/server/LOCAL_ENV.md §12).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn sign_in_holds_one_pooled_connection_at_a_time() {
    let st = stack_sized("launcher,portal", None, 1).await;
    let p = st.player().await;
    let (s, me) = st.get(lr::ME, Caller::Launcher(Some(&p.access))).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    let dev = device();
    for _ in 0..2 {
        let (s, signed) = st.launcher_sign_in(&p.id_token, &dev).await;
        assert_eq!(
            (s, signed["status"].as_str()),
            (StatusCode::OK, Some("signed_in")),
            "{signed}"
        );
    }
    // The native grant (OD-43) and its replay on the same one-connection pool.
    let key = fresh();
    for _ in 0..2 {
        let (s, signed) = st
            .post(
                lr::AUTH_TOKEN,
                Caller::Launcher(None),
                &key,
                id_token_grant(&p.id_token, &dev),
            )
            .await;
        assert_eq!(
            (s, signed["status"].as_str()),
            (StatusCode::OK, Some("signed_in")),
            "{signed}"
        );
    }
}

/// Deletion's native re-authentication (OD-74): freshness, uid binding, verified password
/// identity, typed confirmation and idempotency on a one-connection runtime pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn native_deletion_reauth_is_bound_recent_verified_and_idempotent() {
    let st = stack_sized("launcher,portal", None, 1).await;
    let p = st.player().await;
    let (s, me) = st.get(lr::ME, Caller::Launcher(Some(&p.access))).await;
    assert_eq!(s, StatusCode::OK);
    let gamertag = me["profiles"][0]["gamertag"].as_str().unwrap();
    let (_, other) = emulator_user(&st.emulator, true);
    let now = unix_now();
    let caller = Caller::Launcher(Some(&p.access));
    let request =
        |token: &str, tag: &str| json!({"confirmGamertag": tag, "reauth": {"idToken": token}});
    let cases = [
        ("not-a-token".to_owned(), "AUTH.ID_TOKEN_INVALID"),
        (
            reforged(&p.id_token, |c| {
                c["iat"] = json!(now - 360);
                c["auth_time"] = json!(now - 360);
            }),
            "AUTH.ID_TOKEN_INVALID",
        ),
        // A fresh token minted through refresh does not prove a recent password sign-in.
        (
            reforged(&p.id_token, |c| c["auth_time"] = json!(now - 360)),
            "AUTH.REAUTH_REQUIRED",
        ),
        (other, "AUTH.FORBIDDEN"),
        (
            reforged(&p.id_token, |c| c["email_verified"] = json!(false)),
            "AUTH.EMAIL_UNVERIFIED",
        ),
        (
            reforged(&p.id_token, |c| {
                c["firebase"]["sign_in_provider"] = json!("google.com")
            }),
            "AUTH.PROVIDER_NOT_ALLOWED",
        ),
    ];
    for (token, expected) in cases {
        let (s, b) = st
            .post(lr::ME_DELETION, caller, &fresh(), request(&token, gamertag))
            .await;
        let status = if matches!(expected, "AUTH.ID_TOKEN_INVALID" | "AUTH.REAUTH_REQUIRED") {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!((s, code(&b)), (status, expected));
    }
    let (s, b) = st
        .post(
            lr::ME_DELETION,
            caller,
            &fresh(),
            request(&p.id_token, "Wrong#12345"),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "ACCOUNTS.CONFIRMATION_MISMATCH"
        )
    );
    let (s, b) = st
        .post(
            lr::ME_DELETION,
            caller,
            &fresh(),
            json!({
                "confirmGamertag": gamertag,
                "reauth": {"code": "old", "codeVerifier": "old", "redirectUri": REDIRECT}
            }),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::BAD_REQUEST, "REQUEST.INVALID"));
    let (_, unchanged) = st.get(lr::ME, caller).await;
    assert!(unchanged["deletion"].is_null());

    let key = fresh();
    let body = request(&p.id_token, gamertag);
    let (first, concurrent) = tokio::join!(
        st.post(lr::ME_DELETION, caller, &key, body.clone()),
        st.post(lr::ME_DELETION, caller, &key, body.clone())
    );
    assert_eq!(first.0, StatusCode::ACCEPTED);
    assert_eq!(concurrent, first);
    let (s, replay) = st.post(lr::ME_DELETION, caller, &key, body).await;
    assert_eq!((s, replay), first);
    let (s, b) = st
        .post(
            lr::ME_DELETION,
            caller,
            &key,
            request(&p.id_token, "Other#12345"),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "REQUEST.IDEMPOTENCY_KEY_REUSED")
    );

    // Cancel, then replay the original request: it must return its recorded answer, never
    // schedule the deletion again. A new key is a new, explicitly authorized request.
    let (s, _) = st
        .call(Method::DELETE, lr::ME_DELETION, caller, None, None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, replay) = st
        .post(
            lr::ME_DELETION,
            caller,
            &key,
            request(&p.id_token, gamertag),
        )
        .await;
    assert_eq!((s, replay), first);
    let (_, after_cancel) = st.get(lr::ME, caller).await;
    assert!(after_cancel["deletion"].is_null());
}

/// A multi-connection pool exercises the idempotency lock rather than serialization by the
/// one-connection pool of the security test above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn native_deletion_concurrent_duplicates_share_one_result() {
    let st = stack_sized("launcher,portal", None, 4).await;
    let p = st.player().await;
    let caller = Caller::Launcher(Some(&p.access));
    let (s, me) = st.get(lr::ME, caller).await;
    assert_eq!(s, StatusCode::OK);
    let body = json!({
        "confirmGamertag": me["profiles"][0]["gamertag"],
        "reauth": {"idToken": p.id_token}
    });
    let key = fresh();
    let (first, second, third, fourth) = tokio::join!(
        st.post(lr::ME_DELETION, caller, &key, body.clone()),
        st.post(lr::ME_DELETION, caller, &key, body.clone()),
        st.post(lr::ME_DELETION, caller, &key, body.clone()),
        st.post(lr::ME_DELETION, caller, &key, body.clone())
    );
    assert_eq!(first.0, StatusCode::ACCEPTED);
    for answer in [second, third, fourth] {
        assert_eq!(answer, first);
    }
    let (s, _) = st
        .call(Method::DELETE, lr::ME_DELETION, caller, None, None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

/// Native-only sign-in (OD-74, ACCOUNTS.md §5): `grantType: "id_token"` answers (`needs_account` then account creation, `signed_in`), replays a lost answer under the same
/// key and body, refuses another body under that key, every ID-token check of §4.1, and the
/// latest-login-wins rule without device quotas (OD-93).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn native_id_token_sign_in_verifies_claims_and_replays() {
    let st = stack().await;
    let (unverified_uid, unverified) = emulator_user(&st.emulator, false);
    let (uid, id_token) = emulator_user(&st.emulator, true);
    let (other_uid, other_token) = emulator_user(&st.emulator, true);
    let dev = device();
    let token_call = |id_token: String, key: String, device: Value| {
        let st = &st;
        async move {
            st.post(
                lr::AUTH_TOKEN,
                Caller::Launcher(None),
                &key,
                id_token_grant(&id_token, &device),
            )
            .await
        }
    };

    // An unverified e-mail, garbage, and every claim the verifier checks (§4.1).
    let (s, b) = token_call(unverified.clone(), fresh(), dev.clone()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "AUTH.EMAIL_UNVERIFIED"),
        "{b}"
    );
    let now = unix_now();
    let refused: Vec<(&str, String)> = vec![
        ("garbage", "not-a-token".into()),
        (
            "a signature in emulator mode",
            format!(
                "{}.{}.c2lnbmF0dXJl",
                fi_auth::b64::url_encode(br#"{"alg":"RS256","kid":"k1","typ":"JWT"}"#),
                id_token.split('.').nth(1).unwrap()
            ),
        ),
        (
            "minted more than 5 minutes ago",
            reforged(&id_token, |c| {
                c["iat"] = json!(now - 301);
                c["auth_time"] = json!(now - 301);
            }),
        ),
        (
            "expired",
            reforged(&id_token, |c| c["exp"] = json!(now - 1)),
        ),
        (
            "another project",
            reforged(&id_token, |c| c["aud"] = json!("demo-another-project")),
        ),
        (
            "another issuer",
            reforged(&id_token, |c| {
                c["iss"] = json!("https://securetoken.google.com/demo-another-project")
            }),
        ),
        (
            "a tenant",
            reforged(&id_token, |c| c["firebase"]["tenant"] = json!("t1")),
        ),
    ];
    for (what, token) in refused {
        let (s, b) = token_call(token, fresh(), dev.clone()).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID"),
            "{what}: {b}"
        );
    }
    // OD-42: only e-mail and password; a Google ID token is refused as such.
    let google = reforged(&id_token, |c| {
        c["firebase"]["sign_in_provider"] = json!("google.com")
    });
    let (s, b) = token_call(google, fresh(), dev.clone()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "AUTH.PROVIDER_NOT_ALLOWED"),
        "{b}"
    );

    // A verified identity without an account: needs_account. The reforging itself is sound: the
    // same claims re-encoded are accepted.
    let (s, control) = token_call(reforged(&id_token, |_| {}), fresh(), dev.clone()).await;
    assert_eq!(
        (s, control["status"].as_str()),
        (StatusCode::OK, Some("needs_account")),
        "{control}"
    );
    let key = fresh();
    let (s, first) = token_call(id_token.clone(), key.clone(), dev.clone()).await;
    assert_eq!(
        (s, first["status"].as_str()),
        (StatusCode::OK, Some("needs_account")),
        "{first}"
    );
    assert!(first["email"].as_str().unwrap().ends_with("@example.test"));
    // The answer was lost: the same key and body replay it with a new ticket (CR-L4) ...
    let (s, replay) = token_call(id_token.clone(), key.clone(), dev.clone()).await;
    assert_eq!(
        (s, replay["status"].as_str()),
        (StatusCode::OK, Some("needs_account")),
        "{replay}"
    );
    assert_ne!(first["identityTicket"], replay["identityTicket"]);
    // ... and another body under the same key (another identity, the same device) is refused.
    let (s, b) = token_call(other_token.clone(), key.clone(), dev.clone()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "REQUEST.IDEMPOTENCY_KEY_REUSED"),
        "{b}"
    );

    // Account creation from the replayed ticket, as in the browser flows.
    let (s, created) = st
        .post(
            lr::ACCOUNTS,
            Caller::Launcher(None),
            &fresh(),
            json!({
                "identityTicket": replay["identityTicket"],
                "device": dev,
                "profile": {"game": "fh2", "name": format!("Nat{}", &fresh()[24..]), "mode": "noob"},
            }),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let account_id = created["accountId"].as_str().unwrap().to_owned();
    // A second replay of a needs_account is not served: the record points at the ticket the
    // first replay issued, which the account creation redeemed (as for the code grants and the
    // portal).
    let (s, b) = token_call(id_token.clone(), key.clone(), dev.clone()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "REQUEST.IDEMPOTENCY_KEY_REUSED"),
        "{b}"
    );

    // Signed in; a replay re-issues the family (the first one, never received, is revoked).
    let key = fresh();
    let (s, signed) = token_call(id_token.clone(), key.clone(), dev.clone()).await;
    assert_eq!(
        (s, signed["status"].as_str(), signed["accountId"].as_str()),
        (StatusCode::OK, Some("signed_in"), Some(account_id.as_str())),
        "{signed}"
    );
    let (s, again) = token_call(id_token.clone(), key.clone(), dev.clone()).await;
    assert_eq!(
        (s, again["status"].as_str(), again["accountId"].as_str()),
        (StatusCode::OK, Some("signed_in"), Some(account_id.as_str())),
        "{again}"
    );
    assert_ne!(signed["refreshToken"], again["refreshToken"]);
    let (s, _) = st
        .get(lr::ME, Caller::Launcher(signed["accessToken"].as_str()))
        .await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "the unreceived family is revoked"
    );
    let (s, me) = st
        .get(lr::ME, Caller::Launcher(again["accessToken"].as_str()))
        .await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["devices"].as_array().map(Vec::len), Some(1));

    // OD-93: each fresh login replaces the last; new Proton prefixes are allowed.
    let mut previous = again;
    for _ in 0..4 {
        let (s, next) = token_call(id_token.clone(), fresh(), device()).await;
        assert_eq!(
            (s, next["status"].as_str()),
            (StatusCode::OK, Some("signed_in")),
            "{next}"
        );
        let (s, b) = st
            .get(lr::ME, Caller::Launcher(previous["accessToken"].as_str()))
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID"),
            "{b}"
        );
        let (s, b) = st
            .post(
                lr::TOKEN,
                Caller::Launcher(None),
                &fresh(),
                json!({"refreshToken": previous["refreshToken"]}),
            )
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "AUTH.REFRESH_INVALID"),
            "{b}"
        );
        previous = next;
    }

    for uid in [unverified_uid, uid, other_uid] {
        emulator_delete(&st.emulator, &uid);
    }
}

/// The per-network sign-in limit (ACCOUNTS.md §9, 30 per 15 minutes) counts every native attempt
/// before the token is even read, so guessing with forged tokens is bounded; the 31st answers
/// AUTH.RATE_LIMITED with Retry-After.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn native_sign_in_attempts_count_per_client_network() {
    let st = stack().await;
    let dev = device();
    let attempt = || {
        let req = Request::builder()
            .method(Method::POST)
            .uri(lr::AUTH_TOKEN)
            .header("x-forwarded-for", &st.launcher_ip)
            .header("idempotency-key", fresh())
            .header("content-type", "application/json")
            .body(Body::from(id_token_grant("not-a-token", &dev).to_string()))
            .unwrap();
        send(&st.app, req)
    };
    for n in 0..fi_db::limits::SIGNIN_NET.max {
        let a = attempt().await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID".into()),
            "attempt {n}"
        );
    }
    let a = attempt().await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED".into())
    );
    let retry: u64 = a
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap();
    assert!((1..=900).contains(&retry), "{retry}");
}

impl Stack {
    /// A fresh verified identity with an account whose first device is `dev`; the creation's own
    /// family is signed out, so every live family afterwards is the caller's. Returns
    /// `(uid, idToken, accountId)`.
    async fn account_on(&self, dev: &Value) -> (String, String, String) {
        let (uid, id_token) = emulator_user(&self.emulator, true);
        let (s, asked) = self
            .post(
                lr::AUTH_TOKEN,
                Caller::Launcher(None),
                &fresh(),
                id_token_grant(&id_token, dev),
            )
            .await;
        assert_eq!(
            (s, asked["status"].as_str()),
            (StatusCode::OK, Some("needs_account")),
            "{asked}"
        );
        let (s, created) = self
            .post(
                lr::ACCOUNTS,
                Caller::Launcher(None),
                &fresh(),
                json!({
                    "identityTicket": asked["identityTicket"],
                    "device": dev,
                    "profile": {"game": "fh2", "name": format!("Rp{}", &fresh()[24..]), "mode": "noob"},
                }),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{created}");
        let (s, _) = self
            .call(
                Method::POST,
                lr::SIGN_OUT,
                Caller::Launcher(created["accessToken"].as_str()),
                None,
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let account = created["accountId"].as_str().unwrap().to_owned();
        assert_eq!(self.live_families(&account, "launcher").await, 0);
        (uid, id_token, account)
    }

    /// How many families of `client` (`launcher` | `portal`) of `account` are live.
    async fn live_families(&self, account: &str, client: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(DISTINCT family_id) FROM refresh_token \
             WHERE account_id = $1 AND client = $2 AND revoked_at IS NULL",
        )
        .bind(Uuid::parse_str(account).unwrap())
        .bind(client)
        .fetch_one(&self.owner)
        .await
        .unwrap()
    }
}

/// CR-L4 on a launcher sign-in whose first answer is `first`: three replays of the same request
/// (`again`: same key, same body) each answer `signed_in`, and afterwards exactly one family of the
/// account is live, the latest answer's; every earlier answer is refused on `/me` and on refresh.
/// After the latest is signed out the key replays nothing (`refused`) and the signed-out tokens
/// stay refused.
async fn replays_then_sign_out<F, Fut>(
    st: &Stack,
    account: &str,
    first: Value,
    again: F,
    refused: (StatusCode, &str),
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = (StatusCode, Value)>,
{
    assert_eq!(
        (first["status"].as_str(), first["accountId"].as_str()),
        (Some("signed_in"), Some(account)),
        "{first}"
    );
    let mut answers = vec![first];
    for n in 0..3 {
        let (s, b) = again().await;
        assert_eq!(
            (s, b["status"].as_str(), b["accountId"].as_str()),
            (StatusCode::OK, Some("signed_in"), Some(account)),
            "replay {n}: {b}"
        );
        answers.push(b);
        assert_eq!(
            st.live_families(account, "launcher").await,
            1,
            "replay {n}: one live family"
        );
    }
    let (latest, earlier) = answers.split_last().unwrap();
    for (n, a) in earlier.iter().enumerate() {
        let (s, b) = st
            .get(lr::ME, Caller::Launcher(a["accessToken"].as_str()))
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID"),
            "answer {n} on /me"
        );
        let (s, b) = st
            .post(
                lr::TOKEN,
                Caller::Launcher(None),
                &fresh(),
                json!({"refreshToken": a["refreshToken"]}),
            )
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "AUTH.REFRESH_INVALID"),
            "answer {n} on refresh"
        );
    }
    let access = latest["accessToken"].as_str();
    let (s, me) = st.get(lr::ME, Caller::Launcher(access)).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "the latest answer is the live family: {me}"
    );
    assert_eq!(st.live_families(account, "launcher").await, 1);

    // Signed out, the key replays nothing: the sign-out stays.
    let (s, _) = st
        .call(
            Method::POST,
            lr::SIGN_OUT,
            Caller::Launcher(access),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = again().await;
    assert_eq!((s, code(&b)), refused, "a replay after sign-out: {b}");
    let (s, _) = st.get(lr::ME, Caller::Launcher(access)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "signed out stays signed out");
    let (s, b) = st
        .post(
            lr::TOKEN,
            Caller::Launcher(None),
            &fresh(),
            json!({"refreshToken": latest["refreshToken"]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.REFRESH_INVALID")
    );
    assert_eq!(st.live_families(account, "launcher").await, 0);
}

/// CR-L4 for the exchanges that consume nothing (review of 2026-10-19): replays of a native
/// `id_token` sign-in and of a portal sign-in leave one live family (session), and once it is
/// signed out the key is refused (`REQUEST.IDEMPOTENCY_KEY_REUSED`) instead of signing in again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn id_token_and_portal_replays_keep_one_live_family_and_never_undo_sign_out() {
    let st = stack().await;
    let key_reused = (StatusCode::CONFLICT, "REQUEST.IDEMPOTENCY_KEY_REUSED");

    let dev = device();
    let (native_uid, id_token, account) = st.account_on(&dev).await;
    let key = fresh();
    let again = || {
        let (st, key, body) = (&st, key.clone(), id_token_grant(&id_token, &dev));
        async move {
            st.post(lr::AUTH_TOKEN, Caller::Launcher(None), &key, body)
                .await
        }
    };
    let (s, first) = again().await;
    assert_eq!(s, StatusCode::OK, "{first}");
    replays_then_sign_out(&st, &account, first, again, key_reused).await;

    // The portal's sessions (the portal's server calls with the visitor's ID token).
    let (portal_uid, id_token, account) = st.account_on(&device()).await;
    let key = fresh();
    let again = || {
        let (st, key, body) = (&st, key.clone(), json!({"idToken": id_token}));
        async move {
            st.post(pr::SESSIONS, Caller::Portal(None), &key, body)
                .await
        }
    };
    let mut answers = Vec::new();
    for n in 0..4 {
        let (s, b) = again().await;
        assert_eq!(
            (s, b["status"].as_str(), b["viewer"]["accountId"].as_str()),
            (
                StatusCode::CREATED,
                Some("signed_in"),
                Some(account.as_str())
            ),
            "answer {n}: {b}"
        );
        answers.push(b["token"].as_str().unwrap().to_owned());
        assert_eq!(st.live_families(&account, "portal").await, 1, "answer {n}");
    }
    let (latest, earlier) = answers.split_last().unwrap();
    for (n, token) in earlier.iter().enumerate() {
        let (s, b) = st.get(pr::ME, Caller::Portal(Some(token))).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "PORTAL.SESSION_REQUIRED"),
            "answer {n}"
        );
    }
    let (s, me) = st.get(pr::ME, Caller::Portal(Some(latest))).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    let (s, _) = st
        .call(
            Method::DELETE,
            pr::SESSIONS_CURRENT,
            Caller::Portal(Some(latest)),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = again().await;
    assert_eq!((s, code(&b)), key_reused, "a replay after sign-out: {b}");
    let (s, _) = st.get(pr::ME, Caller::Portal(Some(latest))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(st.live_families(&account, "portal").await, 0);

    for uid in [native_uid, portal_uid] {
        emulator_delete(&st.emulator, &uid);
    }
}

/// The native grant's replays are exchanges of a known identity: they count on the per-identity
/// limit (ACCOUNTS.md §9, 10 per 15 minutes), so the 11th answer under one key is 429, and each
/// `needs_account` replay carries the verified e-mail like the first answer. Each replay spends
/// the ticket the previous answer carried: only the latest one creates the account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn native_replays_count_per_identity_and_carry_the_email() {
    let st = stack().await;
    let (uid, id_token) = emulator_user(&st.emulator, true);
    let dev = device();
    let key = fresh();
    let exchange = || {
        let req = Request::builder()
            .method(Method::POST)
            .uri(lr::AUTH_TOKEN)
            .header("x-forwarded-for", &st.launcher_ip)
            .header("idempotency-key", &key)
            .header("content-type", "application/json")
            .body(Body::from(id_token_grant(&id_token, &dev).to_string()))
            .unwrap();
        send(&st.app, req)
    };
    let mut tickets: Vec<String> = Vec::new();
    let mut email = None;
    for n in 0..fi_db::limits::SIGNIN_UID.max {
        let a = exchange().await;
        let b = a.json();
        assert_eq!(
            (a.status, b["status"].as_str()),
            (StatusCode::OK, Some("needs_account")),
            "answer {n}: {b}"
        );
        let shown = b["email"].as_str().expect("the e-mail").to_owned();
        assert!(shown.ends_with("@example.test"), "{shown}");
        assert_eq!(email.get_or_insert_with(|| shown.clone()), &shown);
        let ticket = b["identityTicket"].as_str().unwrap().to_owned();
        assert!(!tickets.contains(&ticket), "a new ticket each time");
        tickets.push(ticket);
    }
    let a = exchange().await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED".into()),
        "the 11th exchange of one identity"
    );
    let retry: u64 = a
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap();
    assert!((1..=900).contains(&retry), "{retry}");

    let create = |ticket: &str| {
        let (st, body) = (
            &st,
            json!({
                "identityTicket": ticket,
                "device": dev,
                "profile": {"game": "fh2", "name": format!("Rl{}", &fresh()[24..]), "mode": "noob"},
            }),
        );
        async move {
            st.post(lr::ACCOUNTS, Caller::Launcher(None), &fresh(), body)
                .await
        }
    };
    let (s, b) = create(&tickets[tickets.len() - 2]).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.IDENTITY_TICKET_INVALID"),
        "an earlier replay's ticket was spent by the next replay"
    );
    let (s, created) = create(tickets.last().unwrap()).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    emulator_delete(&st.emulator, &uid);
}

/// Concurrent identical requests (the same key and body, a client retrying before its first
/// attempt answered) serialize on the key: each one answers like a replay, none is refused as a
/// reused key, and one family (session) is left live, the last answer's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn concurrent_duplicates_replay_instead_of_refusing_the_key() {
    let st = stack().await;
    let dev = device();
    let (uid, id_token, account) = st.account_on(&dev).await;
    let key = fresh();
    let native = || {
        st.post(
            lr::AUTH_TOKEN,
            Caller::Launcher(None),
            &key,
            id_token_grant(&id_token, &dev),
        )
    };
    let (a, b, c) = tokio::join!(native(), native(), native());
    let mut working = 0;
    for (s, v) in [a, b, c] {
        assert_eq!(
            (s, v["status"].as_str()),
            (StatusCode::OK, Some("signed_in")),
            "{v}"
        );
        let (s, _) = st
            .get(lr::ME, Caller::Launcher(v["accessToken"].as_str()))
            .await;
        working += usize::from(s == StatusCode::OK);
    }
    assert_eq!(working, 1, "only the last answer's family lives");
    assert_eq!(st.live_families(&account, "launcher").await, 1);

    let key = fresh();
    let portal = || {
        st.post(
            pr::SESSIONS,
            Caller::Portal(None),
            &key,
            json!({"idToken": id_token}),
        )
    };
    let (a, b, c) = tokio::join!(portal(), portal(), portal());
    for (s, v) in [a, b, c] {
        assert_eq!(
            (s, v["status"].as_str()),
            (StatusCode::CREATED, Some("signed_in")),
            "{v}"
        );
    }
    assert_eq!(st.live_families(&account, "portal").await, 1);
    emulator_delete(&st.emulator, &uid);
}

/// `/info` carries the Identity Platform web configuration (OD-43): the emulator's public origin
/// only when it is configured (compose: `http://127.0.0.1:9399`, the LAN address in LAN mode).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn info_names_the_identity_platform_settings() {
    let st = stack().await;
    let (s, info) = st.get(lr::INFO, Caller::Launcher(None)).await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert_eq!(
        info["idp"],
        json!({"apiKey": "demo-api-key", "projectId": PROJECT})
    );
    let lan = stack_env(
        "launcher,portal",
        None,
        8,
        &[("FI_IDP_EMULATOR_PUBLIC_URL", "http://192.0.2.10:9399")],
    )
    .await;
    let (s, info) = lan.get(lr::INFO, Caller::Launcher(None)).await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert_eq!(
        info["idp"],
        json!({"apiKey": "demo-api-key", "projectId": PROJECT,
               "emulatorUrl": "http://192.0.2.10:9399"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn native_sign_in_then_launcher_account_creation() {
    let st = stack().await;
    let (_, id_token) = emulator_user(&st.emulator, true);
    let (s, info) = st.get(lr::INFO, Caller::Launcher(None)).await;
    assert_eq!(
        (s, info["realm"].as_str()),
        (StatusCode::OK, Some("forzainfinity-local"))
    );

    let dev = device();
    let (s, result) = st.launcher_sign_in(&id_token, &dev).await;
    assert_eq!(
        (s, result["status"].as_str()),
        (StatusCode::OK, Some("needs_account"))
    );

    // The launcher creates the account with its first profile; the replay answers the same account.
    let name = format!("Dev{}", &fresh()[24..]);
    let body = json!({
        "identityTicket": result["identityTicket"],
        "device": dev,
        "profile": {"game": "fh2", "name": name, "mode": "hardcore"},
    });
    let key = fresh();
    let (s, created) = st
        .post(lr::ACCOUNTS, Caller::Launcher(None), &key, body.clone())
        .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    assert_eq!(created["profile"]["mode"], "hardcore");
    let (s, replay) = st
        .post(lr::ACCOUNTS, Caller::Launcher(None), &key, body)
        .await;
    assert_eq!(
        (s, &replay["accountId"]),
        (StatusCode::CREATED, &created["accountId"])
    );
    assert_ne!(replay["refreshToken"], created["refreshToken"]);
    let (s, me) = st
        .get(lr::ME, Caller::Launcher(replay["accessToken"].as_str()))
        .await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["profiles"].as_array().map(Vec::len), Some(1));

    // The same identity on the portal now signs straight in.
    let (s, b) = st
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &fresh(),
            json!({"idToken": id_token}),
        )
        .await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::CREATED, Some("signed_in")),
        "{b}"
    );
    assert_eq!(b["viewer"]["accountId"], created["accountId"]);
}

// ---------------------------------------------------------------------------------------------
// Local environment v0, workstream A: play sessions, saves, the game plane, /internal/v1.
// ---------------------------------------------------------------------------------------------

/// A signed-in launcher with one Noob profile.
struct Player {
    access: String,
    account_id: String,
    xuid: String,
    id_token: String,
}

/// An HTTP answer.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_owned()
    }
}

async fn send(app: &Router, req: Request<Body>) -> Answer {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    Answer {
        status,
        headers,
        body,
    }
}

const NONCE: &str = "00112233445566778899aabbccddeeff";
const HOOK: &str = "1111111111111111111111111111111111111111111111111111111111111111";

impl Stack {
    /// A launcher request builder (bearer, client network).
    fn launcher(&self, method: Method, uri: &str, access: &str) -> request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("x-forwarded-for", &self.launcher_ip)
            .header("authorization", format!("Bearer {access}"))
    }

    async fn launcher_json(
        &self,
        method: Method,
        uri: &str,
        access: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> Answer {
        let mut b = self.launcher(method, uri, access);
        if let Some(k) = key {
            b = b.header("idempotency-key", k);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        send(&self.app, req).await
    }

    /// A new identity, its account and one Noob profile, signed in on a new device.
    async fn player(&self) -> Player {
        self.player_as("noob").await
    }

    /// [`Stack::player`] with a profile of `mode` (no save: a Hardcore line starts from the seed).
    async fn player_as(&self, mode: &str) -> Player {
        let (_, id_token) = emulator_user(&self.emulator, true);
        let dev = device();
        let (s, signed) = self.launcher_sign_in(&id_token, &dev).await;
        assert_eq!(
            (s, signed["status"].as_str()),
            (StatusCode::OK, Some("needs_account")),
            "{signed}"
        );
        let (s, created) = self
            .post(
                lr::ACCOUNTS,
                Caller::Launcher(None),
                &fresh(),
                json!({
                    "identityTicket": signed["identityTicket"],
                    "device": dev,
                    "profile": {"game": "fh2", "name": format!("Pl{}", &fresh()[24..]), "mode": mode},
                }),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{created}");
        Player {
            access: created["accessToken"].as_str().unwrap().to_owned(),
            account_id: created["accountId"].as_str().unwrap().to_owned(),
            xuid: created["profile"]["xuid"].as_str().unwrap().to_owned(),
            id_token,
        }
    }

    async fn create_session(&self, p: &Player, access: &str, take_over: bool) -> (String, Answer) {
        let sid = Uuid::now_v7().to_string();
        let a = self
            .launcher_json(
                Method::POST,
                lr::SESSIONS,
                access,
                Some(&sid),
                Some(json!({
                    "sessionId": sid, "game": "fh2", "xuid": p.xuid, "nonce": NONCE,
                    "launcherVersion": "0.1.0", "hookSha256": HOOK, "carDbSha256": "",
                    "baseRevision": 0, "takeOver": take_over,
                })),
            )
            .await;
        (sid, a)
    }

    /// A Supporter for a day with one Hardcore profile: since OD-52 the only profile the aliases
    /// admit and lease a save line to.
    async fn supporter(&self) -> Player {
        let p = self.player_as("hardcore").await;
        let until = fi_protocol::instant::format_unix_secs(crate::state::now_secs() + 86_400);
        let e = self.put_entitlement(&p.account_id, "active", until).await;
        assert_eq!(e.json()["tier"], "vip", "{}", e.json());
        p
    }

    /// The line's revision 1 from the server's seed (OD-60, SAVES_API.md §4.8): the synthetic
    /// starter imported as fh2's seed (what `fi-cli seed-import --synthetic` does; every fh2 test
    /// imports the same bytes, so concurrent suites agree), then `POST .../save/seed` under the
    /// session's lease. A Hardcore line takes no player upload before it.
    async fn fresh_career(&self, p: &Player, session: &str, fence: u64) {
        let bytes = fi_saves::synthetic::starter();
        let v = fi_saves::bundle::verify(bytes).unwrap();
        let blob = fi_saves::BlobRef {
            sha256: fi_saves::Digest::of(bytes),
            size: bytes.len() as u32,
            format: fi_saves::SaveFormat::of_magic(bytes),
        };
        let store = fi_storage::GcsBlobStore::from_env().expect("the save bucket's emulator");
        fi_saves::BlobStore::put(&store, blob.sha256, bytes)
            .await
            .unwrap();
        let mut tx = self.owner.begin().await.unwrap();
        fi_db::seeds::import_in(
            &mut tx,
            &fi_db::seeds::NewSeed {
                game: "fh2",
                kind: fi_db::seeds::HARDCORE,
                blob,
                content: v.content,
                label: &v.label_hex(),
                summary: v.summary,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let req = self
            .launcher(
                Method::POST,
                &format!("/launcher/v1/profiles/{}/save/seed", p.xuid),
                &p.access,
            )
            .header("idempotency-key", Uuid::now_v7().to_string())
            .header("x-fi-session-id", session)
            .header("x-fi-fence", fence.to_string())
            .body(Body::empty())
            .unwrap();
        let seeded = send(&self.app, req).await;
        assert_eq!(seeded.status, StatusCode::CREATED, "{}", seeded.json());
        assert_eq!(
            (&seeded.json()["revision"], &seeded.json()["kind"]),
            (&json!(1), &json!("fresh_career"))
        );
        assert_eq!(seeded.json()["blobSha256"], blob.sha256.hex());
    }

    async fn put_entitlement(&self, account: &str, status: &str, until: Option<String>) -> Answer {
        let req = Request::builder()
            .method(Method::PUT)
            .uri(ir::LOCAL_ENTITLEMENTS)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"accountId": account, "status": status, "accessUntil": until}).to_string(),
            ))
            .unwrap();
        send(&self.app, req).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit(
        &self,
        p: &Player,
        session: &str,
        fence: u64,
        if_match: u64,
        key: &str,
        kind: &str,
        bundle: &[u8],
    ) -> Answer {
        let content = fi_saves::bundle::verify(bundle).unwrap().content.hex();
        let req = self
            .launcher(Method::PUT, &save_uri(&p.xuid), &p.access)
            .header("if-match", format!("\"{if_match}\""))
            .header("idempotency-key", key)
            .header("x-fi-session-id", session)
            .header("x-fi-fence", fence.to_string())
            .header("x-fi-commit-kind", kind)
            .header(
                "repr-digest",
                fi_protocol::headers::repr_digest(&fi_saves::Digest::of(bundle).0),
            )
            .header("x-fi-content-sha256", content)
            .header("content-type", "application/x-fh2save1")
            .body(Body::from(bundle.to_vec()))
            .unwrap();
        send(&self.app, req).await
    }

    async fn poll(&self, p: &Player, session: &str) -> Answer {
        self.launcher_json(
            Method::GET,
            &session_uri(lr::SESSION, session),
            &p.access,
            None,
            None,
        )
        .await
    }

    async fn ended_rows(&self, session: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM session_summary WHERE play_session_id::text = $1")
            .bind(session)
            .fetch_one(&self.owner)
            .await
            .unwrap()
    }

    /// A game-plane router of `pool`, as `FI_SURFACES=game FI_POOL=<pool>` builds it.
    fn game_router(&self, pool: &str, instance: &str) -> Router {
        let mut config = Config::parse("game", Some(pool.to_owned())).unwrap();
        config.platform.session_keys = self.identity.access_keys.signing().cloned();
        config.platform.instance_id = instance.to_owned();
        app::router(
            ServiceInfo::from_env("fi-api", "0.0.0"),
            AppState {
                pool: self.pool.clone(),
                identity: None,
            },
            &config,
        )
    }
}

fn save_uri(xuid: &str) -> String {
    lr::SAVE.replace("{xuid}", xuid)
}

fn session_uri(route: &str, sid: &str) -> String {
    route.replace("{sessionId}", sid)
}

async fn game_call(
    game: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    request_id: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("x-fi-session", t);
    }
    if let Some(r) = request_id {
        b = b.header("x-fh2-request-id", r);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string())),
        None => b.body(Body::empty()),
    }
    .unwrap();
    send(game, req).await
}

/// A test pool (configuration: the migrator writes it).
async fn test_pool(owner: &PgPool, tier: &str, capacity: i32) -> String {
    let name = format!("e2e{}{}", &tier[..1], &fresh()[20..32]);
    sqlx::query(
        "INSERT INTO pool (name, tier, capacity, queue_limit, game_endpoint) VALUES ($1, $2, $3, 100, $4)",
    )
    .bind(&name)
    .bind(tier)
    .bind(capacity)
    .bind(format!("http://127.0.0.1:8480/pools/{name}"))
    .execute(owner)
    .await
    .expect("pool row");
    name
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Postgres, the auth emulator and fake-gcs (see the file header)"]
async fn play_sessions_saves_the_game_plane_and_internal_pools() {
    // This run's pools and the local entitlement stand-in (what `fi-cli local-seed` creates).
    let setup = stack().await;
    let pools = Pools {
        vip: test_pool(&setup.owner, "vip", 2).await,
        free: test_pool(&setup.owner, "free", 2).await,
    };
    {
        let mut conn = setup.owner.acquire().await.unwrap();
        fi_db::admission::ensure_local_entitlement_in(&mut conn)
            .await
            .expect("entitlement stand-in");
    }
    let st = stack_env("launcher,portal,internal", Some(&pools), 8, &[]).await;
    let game = st.game_router(&pools.vip, "e2e-game-vip-1");

    // A signs up (free, a Hardcore profile) and becomes a supporter: its next session lands on
    // the VIP pool with the save line (OD-52).
    let a = st.player_as("hardcore").await;
    let until = fi_protocol::instant::format_unix_secs(crate::state::now_secs() + 86_400);
    let e = st
        .put_entitlement(&a.account_id, "active", until.clone())
        .await;
    assert_eq!(e.status, StatusCode::OK, "{}", e.json());
    assert_eq!(e.json()["tier"], "vip");
    let bad = st.put_entitlement("not-a-uuid", "active", None).await;
    assert_eq!(
        (bad.status, bad.code()),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID".into())
    );
    let unbounded = st.put_entitlement(&a.account_id, "active", None).await;
    assert_eq!(
        (unbounded.status, unbounded.code()),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID".into())
    );
    let unknown = st
        .put_entitlement(&Uuid::now_v7().to_string(), "active", until)
        .await;
    assert_eq!(
        (unknown.status, unknown.code()),
        (StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND".into())
    );

    // Create: 201, the lease held, a ticket the launcher's own verifier accepts, a session token.
    let (sid, created) = st.create_session(&a, &a.access, false).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let body = created.json();
    assert_eq!(body["state"], "active");
    assert_eq!(body["pool"]["name"], pools.vip.as_str());
    assert_eq!(body["pool"]["tier"], "vip");
    assert_eq!(
        body["pool"]["server"],
        format!("http://127.0.0.1:8480/pools/{}", pools.vip)
    );
    assert_eq!(body["save"]["revision"], 0);
    assert_eq!(body["lease"]["ttlMs"], 300_000);
    assert_eq!(body["lease"]["heartbeatEveryMs"], 60_000);
    let fence = body["lease"]["fenceToken"].as_u64().unwrap();
    let session_token = body["sessionToken"].as_str().unwrap().to_owned();
    {
        // The signature over the ticket prefix and claims, with the published key; that the
        // launcher's own verifier accepts fi_auth::ticket's tickets is proven by the launcher's
        // tests.
        use ed25519_dalek::{Signature, VerifyingKey};
        let claims = fi_auth::b64::std_decode(body["ticket"]["claims"].as_str().unwrap()).unwrap();
        let signature: [u8; 64] =
            fi_auth::b64::std_decode(body["ticket"]["signature"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
        let (kid, public) = st
            .identity
            .ticket_keys
            .as_ref()
            .unwrap()
            .public_keys()
            .remove(0);
        VerifyingKey::from_bytes(&public)
            .unwrap()
            .verify_strict(
                &[fi_auth::ticket::SIGN_PREFIX, &claims].concat(),
                &Signature::from_bytes(&signature),
            )
            .expect("the server's ticket verifies with its published key");
        let parsed: Value = serde_json::from_slice(&claims).unwrap();
        assert_eq!(parsed["kid"], kid.as_str());
        assert_eq!(parsed["nonce"], NONCE);
        assert_eq!(parsed["sid"], sid.as_str());
        assert_eq!(parsed["pool"], pools.vip.as_str());
        let now = crate::state::now_secs() as u64;
        let (iat, exp) = (
            parsed["iat"].as_u64().unwrap(),
            parsed["exp"].as_u64().unwrap(),
        );
        assert!(iat <= now && now < exp && exp - iat <= fi_auth::ticket::MAX_LIFETIME_SECS);
        let verified = fi_auth::session::verify(
            st.identity.access_keys.signing().unwrap(),
            &session_token,
            crate::state::now_secs(),
        )
        .expect("the session token verifies");
        assert_eq!(verified.pool(), pools.vip);
        assert_eq!(verified.claims.sid, sid);
        assert_eq!(verified.claims.xuid, a.xuid);
    }
    // Creating twice with the same session id answers the same session.
    let replay = st
        .launcher_json(
            Method::POST,
            lr::SESSIONS,
            &a.access,
            Some(&sid),
            Some(json!({
                "sessionId": sid, "game": "fh2", "xuid": a.xuid, "nonce": NONCE,
                "launcherVersion": "0.1.0", "hookSha256": HOOK, "carDbSha256": "",
                "baseRevision": 0, "takeOver": false,
            })),
        )
        .await;
    assert_eq!(replay.status, StatusCode::CREATED);
    assert_eq!(replay.json()["lease"]["fenceToken"], fence);
    let mismatch = st
        .launcher_json(
            Method::POST,
            lr::SESSIONS,
            &a.access,
            Some(&Uuid::now_v7().to_string()),
            Some(json!({
                "sessionId": sid, "game": "fh2", "xuid": a.xuid, "nonce": NONCE,
                "launcherVersion": "0.1.0", "hookSha256": HOOK, "carDbSha256": "",
                "baseRevision": 0, "takeOver": false,
            })),
        )
        .await;
    assert_eq!(
        mismatch.status,
        StatusCode::BAD_REQUEST,
        "Idempotency-Key = sessionId"
    );

    // Head: revision 0.
    let head = st
        .launcher_json(Method::GET, &save_uri(&a.xuid), &a.access, None, None)
        .await;
    assert_eq!(head.status, StatusCode::OK, "{}", head.json());
    assert_eq!(head.header("etag"), Some("\"0\""));
    assert_eq!(head.json()["revision"], 0);
    assert_eq!(head.json()["lease"]["live"], true);

    // The line starts with the server's fresh career, then three uploads within the Hardcore
    // allowance (the gates measure each against the head).
    st.fresh_career(&a, &sid, fence).await;
    let seed = u64::from_le_bytes(Uuid::now_v7().as_bytes()[8..16].try_into().unwrap());
    let spec = |label: u64, seed: u64, credits: i32, xp: i32, level: i32| {
        fi_saves::synthetic::Spec {
            credits,
            xp,
            level,
            ..fi_saves::synthetic::Spec::online(label, seed)
        }
        .bundle()
    };
    let b1 = spec(0x10a, seed, 100_000, 50_000, 3);
    let b2 = spec(0x10b, seed ^ 1, 150_000, 90_000, 4);
    let b3 = spec(0x10c, seed ^ 2, 200_000, 130_000, 5);
    let k1 = Uuid::now_v7().to_string();
    let c1 = st.commit(&a, &sid, fence, 1, &k1, "upload", &b1).await;
    assert_eq!(c1.status, StatusCode::CREATED, "{}", c1.json());
    assert_eq!(c1.json()["revision"], 2);
    assert_eq!(c1.header("etag"), Some("\"2\""));
    assert_eq!(
        c1.json()["summary"]["level"],
        3,
        "numbers read by the server"
    );
    let again = st.commit(&a, &sid, fence, 1, &k1, "upload", &b1).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json());
    assert_eq!(again.header("x-fi-replayed"), Some("true"));
    assert_eq!(again.json()["revision"], 2);
    // An offline career never starts or enters a Hardcore line.
    let offline = fi_saves::synthetic::Spec::offline(0x109, seed ^ 7).bundle();
    let import = st
        .commit(
            &a,
            &sid,
            fence,
            2,
            &Uuid::now_v7().to_string(),
            "import_offline",
            &offline,
        )
        .await;
    assert_eq!(
        (import.status, import.code()),
        (
            StatusCode::CONFLICT,
            "SAVES.HARDCORE_NEEDS_FRESH_CAREER".into()
        )
    );
    let c2 = st
        .commit(
            &a,
            &sid,
            fence,
            2,
            &Uuid::now_v7().to_string(),
            "upload",
            &b2,
        )
        .await;
    assert_eq!(c2.status, StatusCode::CREATED, "{}", c2.json());
    let stale = st
        .commit(
            &a,
            &sid,
            fence,
            2,
            &Uuid::now_v7().to_string(),
            "upload",
            &b3,
        )
        .await;
    assert_eq!(
        (stale.status, stale.code()),
        (
            StatusCode::PRECONDITION_FAILED,
            "SAVES.REVISION_CONFLICT".into()
        )
    );
    assert_eq!(stale.json()["details"]["head"], 3);
    let req = st
        .launcher(Method::PUT, &save_uri(&a.xuid), &a.access)
        .header("if-match", "\"3\"")
        .header("idempotency-key", Uuid::now_v7().to_string())
        .header("x-fi-session-id", &sid)
        .header("x-fi-fence", fence.to_string())
        .header(
            "repr-digest",
            fi_protocol::headers::repr_digest(&fi_saves::Digest::of(&b3).0),
        )
        .header("x-fi-content-sha256", "00".repeat(32))
        .header("content-type", "application/x-fh2save1")
        .body(Body::from(b3.clone()))
        .unwrap();
    let mismatch = send(&st.app, req).await;
    assert_eq!(
        (mismatch.status, mismatch.code()),
        (StatusCode::BAD_REQUEST, "SAVES.DIGEST_MISMATCH".into())
    );
    let c3 = st
        .commit(
            &a,
            &sid,
            fence,
            3,
            &Uuid::now_v7().to_string(),
            "upload",
            &b3,
        )
        .await;
    assert_eq!(c3.status, StatusCode::CREATED, "{}", c3.json());
    assert_eq!(c3.json()["revision"], 4);

    // Download, re-hashed; history newest first.
    let req = st
        .launcher(
            Method::GET,
            &lr::SAVE_REVISION_BUNDLE
                .replace("{xuid}", &a.xuid)
                .replace("{revision}", "4"),
            &a.access,
        )
        .body(Body::empty())
        .unwrap();
    let dl = send(&st.app, req).await;
    assert_eq!(dl.status, StatusCode::OK);
    assert_eq!(dl.body.as_ref(), b3.as_slice());
    assert_eq!(dl.header("content-type"), Some("application/x-fh2save1"));
    assert_eq!(
        dl.header("repr-digest").map(str::to_owned),
        Some(fi_protocol::headers::repr_digest(
            &fi_saves::Digest::of(&b3).0
        ))
    );
    assert_eq!(dl.header("etag"), Some("\"4\""));
    assert_eq!(
        dl.header("x-fi-content-sha256").map(str::to_owned),
        Some(fi_saves::bundle::verify(&b3).unwrap().content.hex())
    );
    let history = st
        .launcher_json(
            Method::GET,
            &format!("{}?limit=2", lr::SAVE_REVISIONS.replace("{xuid}", &a.xuid)),
            &a.access,
            None,
            None,
        )
        .await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.json());
    let items = history.json()["items"].clone();
    assert_eq!(items.as_array().map(Vec::len), Some(2));
    assert_eq!(
        (items[0]["revision"].clone(), items[1]["revision"].clone()),
        (json!(4), json!(3))
    );
    assert_eq!(items[1]["deviceLabel"], "E2E PC");
    assert_eq!(history.json()["next"], 3);

    // OD-239 (CR-S111): the tier no longer decides the line, so an access that ends refuses
    // nothing and marks nothing.
    let lapsed = st.put_entitlement(&a.account_id, "none", None).await;
    assert_eq!(lapsed.json()["tier"], "free");
    let head = st
        .launcher_json(Method::GET, &save_uri(&a.xuid), &a.access, None, None)
        .await;
    assert_eq!(head.status, StatusCode::OK, "{}", head.json());
    assert_eq!(head.json()["revision"], 4);
    assert!(head.json()["lapsedSince"].is_null(), "{}", head.json());
    // Renewed before any heartbeat ran: the lease renews below, and the mark stays until the
    // line's next player commit (the launcher restores this head first: SAVES_API.md §4.7).
    let renewed = st
        .put_entitlement(
            &a.account_id,
            "active",
            fi_protocol::instant::format_unix_secs(crate::state::now_secs() + 86_400),
        )
        .await;
    assert_eq!(renewed.json()["tier"], "vip");

    // Heartbeat: the same lease renewed and a fresh session token; token rotation alone.
    let hb = st
        .launcher_json(
            Method::POST,
            &session_uri(lr::SESSION_HEARTBEAT, &sid),
            &a.access,
            None,
            Some(json!({"fenceToken": fence})),
        )
        .await;
    assert_eq!(hb.status, StatusCode::OK, "{}", hb.json());
    assert_eq!(hb.json()["lease"]["fenceToken"], fence);
    assert_eq!(hb.json()["gamePlane"]["open"], true);
    let rotated = st
        .launcher_json(
            Method::POST,
            &session_uri(lr::SESSION_TOKEN, &sid),
            &a.access,
            None,
            None,
        )
        .await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.json());
    let token = hb.json()["sessionToken"].as_str().unwrap().to_owned();

    // The VIP game plane.
    let gs = game_call(&game, Method::GET, gr::SESSION, Some(&token), None, None).await;
    assert_eq!(gs.status, StatusCode::OK, "{}", gs.json());
    assert_eq!(gs.header("x-fi-pool"), Some(pools.vip.as_str()));
    assert_eq!(gs.header("x-fi-instance"), Some("e2e-game-vip-1"));
    assert_eq!(gs.json()["sessionId"], sid.as_str());
    assert_eq!(gs.json()["xuid"], a.xuid.as_str());
    assert_eq!(gs.json()["tier"], "vip");
    assert_eq!(gs.json()["instance"], "e2e-game-vip-1");
    let none = game_call(&game, Method::GET, gr::SESSION, None, None, None).await;
    assert_eq!(
        (none.status, none.code()),
        (StatusCode::UNAUTHORIZED, "AUTH.SESSION_EXPIRED".into())
    );
    assert_eq!(
        none.header("x-fi-pool"),
        Some(pools.vip.as_str()),
        "refusals are stamped too"
    );
    let request_id = Uuid::now_v7().to_string();
    let fact = json!({"kind": "race_result", "data": {"position": 1, "accepted": true}});
    let first = game_call(
        &game,
        Method::POST,
        gr::PROGRESSION,
        Some(&token),
        Some(&request_id),
        Some(fact.clone()),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.json());
    assert_eq!(first.json()["recorded"], true);
    let retry = game_call(
        &game,
        Method::POST,
        gr::PROGRESSION,
        Some(&token),
        Some(&request_id),
        Some(fact.clone()),
    )
    .await;
    assert_eq!(retry.status, StatusCode::OK, "{}", retry.json());
    assert_eq!(retry.json()["recorded"], false);
    assert_eq!(retry.json()["eventId"], first.json()["eventId"]);
    let facts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM progression_event WHERE event_id = $1::uuid")
            .bind(first.json()["eventId"].as_str().unwrap())
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert_eq!(facts, 1, "the retry is the same fact");
    let no_id = game_call(
        &game,
        Method::POST,
        gr::PROGRESSION,
        Some(&token),
        None,
        Some(fact.clone()),
    )
    .await;
    assert_eq!(
        (no_id.status, no_id.code()),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID".into())
    );
    let bad_kind = game_call(
        &game,
        Method::POST,
        gr::PROGRESSION,
        Some(&token),
        Some(&Uuid::now_v7().to_string()),
        Some(json!({"kind": "lap", "data": {}})),
    )
    .await;
    assert_eq!(bad_kind.status, StatusCode::BAD_REQUEST);
    let presence = game_call(&game, Method::POST, gr::PRESENCE, Some(&token), None, None).await;
    assert_eq!(presence.status, StatusCode::NO_CONTENT);
    let (pool, instance): (String, String) =
        sqlx::query_as("SELECT pool, instance FROM game_presence WHERE session_id = $1::uuid")
            .bind(&sid)
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert_eq!(
        (pool.as_str(), instance.as_str()),
        (pools.vip.as_str(), "e2e-game-vip-1")
    );

    // OD-93: login on the second PC ends the first login and play session before Play.
    let dev2 =
        json!({"deviceId": Uuid::now_v7().to_string(), "label": "STEAMDECK", "platform": "proton"});
    let (s, second) = st.launcher_sign_in(&a.id_token, &dev2).await;
    assert_eq!(s, StatusCode::OK, "{second}");
    let access2 = second["accessToken"].as_str().unwrap().to_owned();
    let (sid2, taken) = st.create_session(&a, &access2, false).await;
    assert_eq!(taken.status, StatusCode::CREATED, "{}", taken.json());
    let fence2 = taken.json()["lease"]["fenceToken"].as_u64().unwrap();
    assert!(fence2 > fence);
    assert_eq!(taken.json()["save"]["revision"], 4);
    assert!(
        taken.json()["save"]["lapsedSince"].is_null(),
        "OD-239: no lapse mark: {}",
        taken.json()
    );
    let lost = st
        .launcher_json(
            Method::POST,
            &session_uri(lr::SESSION_HEARTBEAT, &sid),
            &a.access,
            None,
            Some(json!({"fenceToken": fence})),
        )
        .await;
    assert_eq!(
        (lost.status, lost.code()),
        (StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID".into())
    );
    let b4 = fi_saves::synthetic::Spec::online(0x10d, seed ^ 3).bundle();
    let fenced = st
        .commit(
            &a,
            &sid,
            fence,
            4,
            &Uuid::now_v7().to_string(),
            "upload",
            &b4,
        )
        .await;
    assert_eq!(
        (fenced.status, fenced.code()),
        (StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID".into())
    );
    let old_token = game_call(&game, Method::GET, gr::SESSION, Some(&token), None, None).await;
    assert_eq!(
        old_token.status,
        StatusCode::UNAUTHORIZED,
        "the replaced session is no longer live"
    );
    assert_eq!(st.ended_rows(&sid).await, 1, "session.ended (takeover)");

    // End: 204, idempotent, its session.ended row.
    let end_body = json!({"fenceToken": fence2, "reason": "game_exit", "endRevision": 4});
    let ended = st
        .launcher_json(
            Method::POST,
            &session_uri(lr::SESSION_END, &sid2),
            &access2,
            None,
            Some(end_body.clone()),
        )
        .await;
    assert_eq!(ended.status, StatusCode::NO_CONTENT, "{}", ended.json());
    let ended = st
        .launcher_json(
            Method::POST,
            &session_uri(lr::SESSION_END, &sid2),
            &access2,
            None,
            Some(end_body),
        )
        .await;
    assert_eq!(ended.status, StatusCode::NO_CONTENT);
    assert_eq!(st.ended_rows(&sid2).await, 1);

    // The queue: capacity 2 on the vip pool, FIFO (Supporter Hardcore profiles: OD-52).
    let free: Vec<Player> = {
        let mut v = Vec::new();
        for _ in 0..4 {
            v.push(st.supporter().await);
        }
        v
    };
    let mut sessions = Vec::new();
    for p in &free[..2] {
        let (sid, a) = st.create_session(p, &p.access, false).await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.json());
        assert_eq!(a.json()["pool"]["name"], pools.vip.as_str());
        sessions.push((sid, a.json()));
    }
    let (q1, a3) = st.create_session(&free[2], &free[2].access, false).await;
    assert_eq!(a3.status, StatusCode::ACCEPTED, "{}", a3.json());
    assert_eq!(a3.json()["queue"]["position"], 1);
    let (q2, a4) = st.create_session(&free[3], &free[3].access, false).await;
    assert_eq!(a4.json()["queue"]["position"], 2);
    let waiting = st.poll(&free[3], &q2).await;
    assert_eq!(
        (waiting.status, waiting.json()["queue"]["position"].clone()),
        (StatusCode::OK, json!(2))
    );

    // A VIP session's token on another pool's game plane: refused before anything else.
    let free_game = st.game_router(&pools.free, "e2e-game-free-1");
    let vip_token = sessions[0].1["sessionToken"].as_str().unwrap().to_owned();
    let wrong_pool = game_call(
        &free_game,
        Method::GET,
        gr::SESSION,
        Some(&vip_token),
        None,
        None,
    )
    .await;
    assert_eq!(
        (wrong_pool.status, wrong_pool.code()),
        (StatusCode::FORBIDDEN, "GAME.WRONG_POOL".into())
    );
    assert_eq!(wrong_pool.header("x-fi-pool"), Some(pools.free.as_str()));

    // /internal/v1/pools: live counts by the database clock.
    let req = Request::builder()
        .uri(ir::POOLS)
        .body(Body::empty())
        .unwrap();
    let listed = send(&st.app, req).await;
    assert_eq!(listed.status, StatusCode::OK);
    let find = |name: &str| {
        listed.json()["pools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .cloned()
            .unwrap()
    };
    let (vip_row, free_row) = (find(&pools.vip), find(&pools.free));
    assert_eq!(
        (vip_row["active"].clone(), vip_row["queued"].clone()),
        (json!(2), json!(2))
    );
    assert_eq!(
        (free_row["active"].clone(), free_row["queued"].clone()),
        (json!(0), json!(0))
    );
    assert_eq!(vip_row["capacity"], 2);
    assert_eq!(vip_row["instanceCapacity"], Value::Null);

    // The first active session ends: the first in line is admitted on its next poll.
    let (s0, b0) = &sessions[0];
    let end0 = st
        .launcher_json(
            Method::POST,
            &session_uri(lr::SESSION_END, s0),
            &free[0].access,
            None,
            Some(json!({"fenceToken": b0["lease"]["fenceToken"], "reason": "player", "endRevision": null})),
        )
        .await;
    assert_eq!(end0.status, StatusCode::NO_CONTENT);
    let behind = st.poll(&free[3], &q2).await;
    assert_eq!(behind.json()["state"], "queued", "nobody jumps the line");
    let admitted = st.poll(&free[2], &q1).await;
    assert_eq!(admitted.status, StatusCode::OK, "{}", admitted.json());
    assert_eq!(admitted.json()["state"], "active");
    assert!(
        admitted.json()["ticket"]["claims"].is_string(),
        "a queued session's ticket is signed at admission"
    );
    assert_eq!(st.poll(&free[3], &q2).await.json()["queue"]["position"], 1);

    // OD-239: these routes always answer a lease, so a profile without a save line (a Noob/Azimuth
    // one, whatever the tier) is refused before admission.
    let free_noob = st.player().await;
    let noob_supporter = st.player().await;
    let up = st
        .put_entitlement(
            &noob_supporter.account_id,
            "active",
            fi_protocol::instant::format_unix_secs(crate::state::now_secs() + 3_600),
        )
        .await;
    assert_eq!(up.json()["tier"], "vip");
    for (p, tier, mode) in [
        (&free_noob, "free", "noob"),
        (&noob_supporter, "vip", "noob"),
    ] {
        let (sid, refused) = st.create_session(p, &p.access, false).await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, "SAVES.NOT_ELIGIBLE".into()),
            "{tier} {mode}"
        );
        assert_eq!(
            refused.json()["details"],
            json!({"tier": tier, "mode": mode})
        );
        assert_eq!(
            st.poll(p, &sid).await.status,
            StatusCode::NOT_FOUND,
            "nothing admitted"
        );
    }
    // A supporter's next session lands on the VIP pool (its queue, full here).
    let free_hardcore = st.player_as("hardcore").await;
    let up = st
        .put_entitlement(
            &free_hardcore.account_id,
            "active",
            fi_protocol::instant::format_unix_secs(crate::state::now_secs() + 3_600),
        )
        .await;
    assert_eq!(up.json()["tier"], "vip");
    let (_, vip_again) = st
        .create_session(&free_hardcore, &free_hardcore.access, false)
        .await;
    assert_eq!(
        (
            vip_again.status,
            vip_again.json()["queue"]["position"].clone()
        ),
        (StatusCode::ACCEPTED, json!(2)),
        "{}",
        vip_again.json()
    );
    let down = st
        .put_entitlement(&free_hardcore.account_id, "none", None)
        .await;
    assert_eq!(
        (down.json()["tier"].clone(), down.json()["status"].clone()),
        (json!("free"), json!("none"))
    );
}

/// The Identity Platform uid of an emulator ID token (its `user_id` claim).
fn uid_of(id_token: &str) -> String {
    let payload = id_token.split('.').nth(1).expect("a JWT");
    let claims: Value =
        serde_json::from_slice(&fi_auth::b64::url_decode(payload).expect("base64url")).unwrap();
    claims["user_id"].as_str().expect("user_id").to_owned()
}

fn mode_uri(xuid: &str) -> String {
    lr::PROFILE_MODE.replace("{xuid}", xuid)
}

fn name_uri(xuid: &str) -> String {
    lr::PROFILE_NAME.replace("{xuid}", xuid)
}

fn presence_uri(xuid: &str) -> String {
    lr::PROFILE_PRESENCE.replace("{xuid}", xuid)
}

/// A PNG preview of `w` by `h`, colour type 6 (synthetic: the signature and an IHDR only).
fn synthetic_png(w: u32, h: u32) -> Vec<u8> {
    let mut png = fi_db::storefront::PNG_SIGNATURE.to_vec();
    png.extend_from_slice(&13u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&w.to_be_bytes());
    png.extend_from_slice(&h.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    png
}

/// Shares a design of `car` painted by the profile `xuid` with `preview`; its file id.
async fn share_livery(st: &Stack, xuid: &str, mode: &'static str, car: i32, preview: &[u8]) -> i32 {
    let (profile, account): (Uuid, Uuid) =
        sqlx::query_as("SELECT id, account_id FROM profile WHERE xuid = $1")
            .bind(xuid.parse::<i64>().unwrap())
            .fetch_one(&st.owner)
            .await
            .unwrap();
    let writer = fi_db::liveries::Writer {
        game: "fh2",
        mode,
        profile_id: profile,
        account_id: account,
        account_bytes: fi_db::liveries::DEFAULT_ACCOUNT_BYTES,
    };
    let name = format!("D{car}");
    let data: Vec<u8> = (0..64u8).collect();
    let livery = i64::from_be_bytes(Uuid::now_v7().as_bytes()[8..].try_into().unwrap())
        & 0x3FFF_FFFF_FFFF_FFFF
        | 1;
    let mut tx = st.owner.begin().await.unwrap();
    fi_db::liveries::save_draft_in(
        &mut tx,
        writer,
        &fi_db::liveries::Design {
            name: &name,
            car_id: car,
            number_of_layers: 0,
            data: &data,
        },
    )
    .await
    .unwrap();
    fi_db::liveries::create_in(&mut tx, writer, livery, &name, car)
        .await
        .unwrap();
    let viewer = fi_db::storefront::Viewer {
        game: "fh2",
        mode,
        profile_id: profile,
        account_id: account,
    };
    let meta = json!({});
    let shared = fi_db::storefront::share_in(
        &mut tx,
        viewer,
        &fi_db::storefront::NewFile {
            kind: fi_db::storefront::Kind::Livery,
            item_id: livery,
            file_type: 2,
            title: "Stripes",
            description: "synthetic",
            keyword_one: 1,
            keyword_two: 2,
            meta: &meta,
            preview,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    match shared {
        fi_db::storefront::Shared::File(id) => id,
        other => panic!("not shared: {other:?}"),
    }
}

/// CR-S77 (ACCOUNTS.md §5 "Presence", §5a): `?car=` names the caller's own servable preview of
/// that car as an absolute https URL; the public route serves it as `image/png` with its length,
/// cache headers and ETag, and 404s a taken-off, hidden, banned-owner or non-PNG file at once;
/// another account's car files are never used; a malformed or missing `car` only leaves it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_presence_names_the_cars_storefront_preview_and_media_serves_it() {
    let st = stack_env(
        "launcher,portal",
        None,
        8,
        &[("FI_PUBLIC_CORE_URL", "https://play.example.test")],
    )
    .await;
    let me = st.player_as("hardcore").await;
    let other = st.player().await;
    let png = synthetic_png(816, 459);
    let mine = share_livery(&st, &me.xuid, "hardcore", 297, &png).await;
    let theirs = share_livery(&st, &other.xuid, "noob", 298, &png).await;
    let read = |p: &Player, query: &str| {
        let (access, uri, st) = (
            p.access.clone(),
            format!("{}{query}", presence_uri(&p.xuid)),
            &st,
        );
        async move {
            st.launcher_json(Method::GET, &uri, &access, None, None)
                .await
        }
    };
    let image = read(&me, "?car=297").await.json()["liveryImage"]
        .as_str()
        .expect("liveryImage")
        .to_owned();
    let path = image
        .strip_prefix("https://play.example.test")
        .expect("absolute on the core origin")
        .to_owned();
    let token = path
        .strip_prefix("/media/v1/previews/")
        .and_then(|t| t.strip_suffix(".png"))
        .unwrap();
    assert_eq!(token.len(), 32);
    // Not the file id in any form (a small serial), nor the profile's XUID.
    assert!(token != format!("{mine:032x}") && token != format!("{mine:032}"));
    assert!(!token.contains(&me.xuid), "the token names no XUID");
    // Absent: another car, another account's car, a malformed or missing car.
    for q in ["?car=298", "?car=abc", "?car=-1", "?car=", ""] {
        let a = read(&me, q).await;
        assert_eq!(a.status, StatusCode::OK, "{q}");
        assert!(a.json().get("liveryImage").is_none(), "{q}: {}", a.json());
    }
    assert!(
        read(&other, "?car=298")
            .await
            .json()
            .get("liveryImage")
            .is_some()
    );
    // The public route: no credentials.
    let get = |uri: String, etag: Option<String>| {
        let (app, ip) = (st.app.clone(), st.launcher_ip.clone());
        async move {
            let mut b = Request::builder()
                .method(Method::GET)
                .uri(uri)
                .header("x-forwarded-for", ip);
            if let Some(e) = etag {
                b = b.header("if-none-match", e);
            }
            send(&app, b.body(Body::empty()).unwrap()).await
        }
    };
    let a = get(path.clone(), None).await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.header("content-type"), Some("image/png"));
    assert_eq!(a.header("cache-control"), Some("public, max-age=300"));
    assert_eq!(a.body.as_ref(), png.as_slice());
    assert_eq!(&a.body[12..16], b"IHDR");
    let etag = a.header("etag").expect("etag").to_owned();
    assert_eq!(
        get(path.clone(), Some(etag)).await.status,
        StatusCode::NOT_MODIFIED
    );
    // Unknown and malformed tokens, and a file of another kind of name: one 404.
    for bad in [
        "/media/v1/previews/00000000000000000000000000000000.png".to_owned(),
        format!("/media/v1/previews/{token}"),
        format!("/media/v1/previews/{}.png", token.to_uppercase()),
        format!("/media/v1/previews/{mine}.png"),
    ] {
        assert_eq!(
            get(bad.clone(), None).await.status,
            StatusCode::NOT_FOUND,
            "{bad}"
        );
    }
    // Hidden livery, taken off, banned owner: 404 at once, and the presence drops the field.
    let livery: i64 = sqlx::query_scalar("SELECT livery_id FROM storefront_file WHERE id = $1")
        .bind(mine)
        .fetch_one(&st.owner)
        .await
        .unwrap();
    for (on, off) in [
        (
            "UPDATE game_livery SET hidden_at = now() WHERE id = $1",
            "UPDATE game_livery SET hidden_at = NULL WHERE id = $1",
        ),
        (
            "UPDATE storefront_file SET removed_at = now() WHERE livery_id = $1",
            "UPDATE storefront_file SET removed_at = NULL WHERE livery_id = $1",
        ),
        (
            "UPDATE account SET banned_at = now() WHERE id = (SELECT p.account_id FROM game_livery l JOIN profile p ON p.id = l.profile_id WHERE l.id = $1)",
            "UPDATE account SET banned_at = NULL WHERE id = (SELECT p.account_id FROM game_livery l JOIN profile p ON p.id = l.profile_id WHERE l.id = $1)",
        ),
    ] {
        sqlx::query(on)
            .bind(livery)
            .execute(&st.owner)
            .await
            .unwrap();
        assert_eq!(
            get(path.clone(), None).await.status,
            StatusCode::NOT_FOUND,
            "{on}"
        );
        sqlx::query(off)
            .bind(livery)
            .execute(&st.owner)
            .await
            .unwrap();
        assert_eq!(
            get(path.clone(), None).await.status,
            StatusCode::OK,
            "{off}"
        );
    }
    sqlx::query("UPDATE storefront_file SET removed_at = now() WHERE id = $1")
        .bind(mine)
        .execute(&st.owner)
        .await
        .unwrap();
    assert!(
        read(&me, "?car=297")
            .await
            .json()
            .get("liveryImage")
            .is_none()
    );
    // Not a PNG: never served, never named.
    let jpeg = share_livery(
        &st,
        &other.xuid,
        "noob",
        299,
        &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 4, 5],
    )
    .await;
    assert!(
        read(&other, "?car=299")
            .await
            .json()
            .get("liveryImage")
            .is_none()
    );
    let mut conn = st.owner.acquire().await.unwrap();
    let t = fi_db::storefront::preview_token_in(&mut conn, jpeg)
        .await
        .unwrap()
        .unwrap();
    let t2 = fi_db::storefront::preview_token_in(&mut conn, theirs)
        .await
        .unwrap()
        .unwrap();
    drop(conn);
    assert_ne!(t, t2, "a token per file");
    assert_eq!(
        get(
            fi_protocol::routes::media::preview_path(&fi_db::storefront::token_hex(&t)),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

/// OD-215, ACCOUNTS.md §9: every authenticated `/launcher/v1` request counts against its account
/// (a burst of 50, then 10 per second); the 51st request at once is 429 `REQUEST.RATE_LIMITED` with
/// `Retry-After` and the limit's name, and another account's budget is its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn launcher_requests_count_per_account() {
    let st = stack().await;
    let me = st.player().await;
    let other = st.player().await;
    // Sign-up's own requests may have taken a token; empty the bucket, then expect a refusal soon.
    let mut refused = None;
    for _ in 0..60 {
        let a = st
            .launcher_json(Method::GET, lr::ME, &me.access, None, None)
            .await;
        if a.status != StatusCode::OK {
            refused = Some(a);
            break;
        }
    }
    let a = refused.expect("the 51st request at once is refused");
    assert_eq!(
        (a.status, a.code()),
        (
            StatusCode::TOO_MANY_REQUESTS,
            "REQUEST.RATE_LIMITED".to_owned()
        )
    );
    assert!(a.header("retry-after").is_some());
    assert_eq!(a.json()["details"]["limit"], "launcher.account");
    let a = st
        .launcher_json(Method::GET, lr::ME, &other.access, None, None)
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json());
}

/// CR-S72, ACCOUNTS.md §5 "Presence", over the router: the caller's own profile's club of its
/// mode and last accepted profile sync; empty before either; another account's, an unknown or a
/// malformed XUID is 404; the 61st read in an hour is 429.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_presence_answers_the_callers_own_club_and_last_sync() {
    let st = stack().await;
    let me = st.player_as("hardcore").await;
    let other = st.player().await;
    let read = |p: &Player, xuid: &str| {
        let (access, uri, st) = (p.access.clone(), presence_uri(xuid), &st);
        async move {
            st.launcher_json(Method::GET, &uri, &access, None, None)
                .await
        }
    };
    // Nothing known yet: an empty object.
    let a = read(&me, &me.xuid).await;
    assert_eq!((a.status, a.json()), (StatusCode::OK, json!({})));
    // A club of the profile's mode and an accepted sync.
    let profile: Uuid = sqlx::query_scalar("SELECT id FROM profile WHERE xuid = $1")
        .bind(me.xuid.parse::<i64>().unwrap())
        .fetch_one(&st.owner)
        .await
        .unwrap();
    let tag = format!("T{}", &fresh()[29..32]).to_uppercase();
    let club: i32 = sqlx::query_scalar(
        "INSERT INTO game_club (game, mode, tag, name, privacy) VALUES ('fh2', 'hardcore', $1, 'Night Shift', 1) RETURNING id",
    )
    .bind(&tag)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO game_club_member (game, profile_id, club_id, role) VALUES ('fh2', $1, $2, 1)",
    )
    .bind(profile)
    .bind(club)
    .execute(&st.owner)
    .await
    .unwrap();
    // A row from before CR-S81's second half holds the game's in-band step (4): the presence
    // shows the driver level its 900,000 XP reaches (48, shown in game as 49).
    sqlx::query(
        "INSERT INTO integrity_baseline (game, profile_id, mode, credits, xp, level, winnings, top_speed_mps, synced_at) \
         VALUES ('fh2', $1, 'hardcore', 1234567, 900000, 4, 2000000, 80, '2026-10-29T12:00:00Z')",
    )
    .bind(profile)
    .execute(&st.owner)
    .await
    .unwrap();
    // A Noob-mode baseline of the same profile is not this mode's: ignored.
    sqlx::query(
        "INSERT INTO integrity_baseline (game, profile_id, mode, credits, xp, level, winnings, top_speed_mps, synced_at) \
         VALUES ('fh2', $1, 'noob', 5, 5, 1, 5, 1, now())",
    )
    .bind(profile)
    .execute(&st.owner)
    .await
    .unwrap();
    let a = read(&me, &me.xuid).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json());
    assert_eq!(
        a.json(),
        json!({
            "club": {"tag": tag, "name": "Night Shift"},
            "level": 48,
            "credits": 1234567,
            "syncedAt": "2026-10-29T12:00:00.000000Z",
        })
    );
    // Another account's profile, an unknown and a malformed XUID: 404.
    for xuid in [other.xuid.as_str(), "2535999999999999", "not-a-xuid"] {
        let a = read(&me, xuid).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND".to_owned()),
            "{xuid}"
        );
    }
    // 60 reads an hour per account, a malformed XUID uncounted: the 61st is 429 with
    // Retry-After. The counter is set to 59 (the window is the clock hour, so counting up through
    // 56 more reads could cross its end).
    let key = fi_db::limits::account_key(
        &fi_db::limits::PRESENCE_READS,
        me.account_id.parse().unwrap(),
    );
    sqlx::query("UPDATE rate_limit SET count = 59 WHERE key = $1")
        .bind(&key)
        .execute(&st.owner)
        .await
        .unwrap();
    assert_eq!(read(&me, &me.xuid).await.status, StatusCode::OK);
    let a = read(&me, &me.xuid).await;
    assert_eq!(
        (a.status, a.code()),
        (
            StatusCode::TOO_MANY_REQUESTS,
            "REQUEST.RATE_LIMITED".to_owned()
        )
    );
    assert!(a.header("retry-after").is_some());
    // Another account's budget is its own.
    assert_eq!(read(&other, &other.xuid).await.status, StatusCode::OK);
    // The club goes (its member row with it), so a reused database never meets its tag again.
    sqlx::query("DELETE FROM game_club WHERE game = 'fh2' AND id = $1")
        .bind(club)
        .execute(&st.owner)
        .await
        .unwrap();
}

/// OD-120, ACCOUNTS.md §5 "Plain names and renames", over the router: every status and code of
/// `POST /launcher/v1/profiles/{xuid}/name`, `renameNotice` on `/me`, the replay, and the device
/// fingerprint kept only as its HMAC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_free_rename_after_a_clash_and_plain_names() {
    let st = stack().await;
    let me = st.player().await;
    let other = st.player().await;
    let leaving = st.player().await;
    let call = |p: &Player, xuid: &str, key: &str, body: Value| {
        let access = p.access.clone();
        let uri = name_uri(xuid);
        let key = key.to_owned();
        let st = &st;
        async move {
            st.launcher_json(Method::POST, &uri, &access, Some(&key), Some(body))
                .await
        }
    };
    let (_, me_view) = st
        .get(lr::ME, Caller::Launcher(Some(me.access.as_str())))
        .await;
    let mine = &me_view["profiles"][0];
    let name = mine["gamertag"].as_str().unwrap().to_owned();
    assert!(!name.contains('#'), "a plain name: {name}");
    assert_eq!(mine["handle"], name.as_str());
    assert_eq!(mine["renameNotice"], Value::Null);
    let (_, other_view) = st
        .get(lr::ME, Caller::Launcher(Some(other.access.as_str())))
        .await;
    let taken = other_view["profiles"][0]["gamertag"]
        .as_str()
        .unwrap()
        .to_owned();

    // A name taken by another account (any case) answers NAME_UNAVAILABLE at creation (in the mode
    // the account has no profile of: CR-S98's one live profile per mode answers first otherwise).
    let dup = st
        .launcher_json(
            Method::POST,
            lr::PROFILES,
            &me.access,
            Some(&fresh()),
            Some(json!({"game": "fh2", "name": taken.to_ascii_uppercase(), "mode": "hardcore"})),
        )
        .await;
    assert_eq!(
        (dup.status, dup.code()),
        (StatusCode::CONFLICT, "ACCOUNTS.NAME_UNAVAILABLE".into())
    );

    // Without a notice: no rename (the current name is a harmless no-op).
    let refused = call(
        &me,
        &me.xuid,
        &fresh(),
        json!({"name": format!("Nn{}", &fresh()[24..])}),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::CONFLICT, "ACCOUNTS.RENAME_NOT_ALLOWED".into())
    );
    let same = call(&me, &me.xuid, &fresh(), json!({"name": name})).await;
    assert_eq!(same.status, StatusCode::OK, "{}", same.json());

    // The clash backfill's notice (written as the migration would).
    sqlx::query(
        "UPDATE profile SET rename_notice_from = 'Alex', rename_notice_at = clock_timestamp() WHERE xuid = $1",
    )
    .bind(me.xuid.parse::<i64>().unwrap())
    .execute(&st.owner)
    .await
    .unwrap();
    let (_, me_view) = st
        .get(lr::ME, Caller::Launcher(Some(me.access.as_str())))
        .await;
    assert_eq!(
        me_view["profiles"][0]["renameNotice"]["previousName"],
        "Alex"
    );
    assert!(me_view["profiles"][0]["renameNotice"]["renamedAt"].is_string());

    for (body, status, code) in [
        (
            json!({"name": "9lives"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ACCOUNTS.INVALID_NAME",
        ),
        (
            json!({"name": taken}),
            StatusCode::CONFLICT,
            "ACCOUNTS.NAME_UNAVAILABLE",
        ),
        (
            json!({"name": "Admin"}),
            StatusCode::CONFLICT,
            "ACCOUNTS.NAME_UNAVAILABLE",
        ),
        (
            json!({"name": "Ok", "discriminator": 1}),
            StatusCode::BAD_REQUEST,
            "REQUEST.INVALID",
        ),
    ] {
        let a = call(&me, &me.xuid, &fresh(), body).await;
        assert_eq!((a.status, a.code()), (status, code.into()));
    }
    for xuid in ["2535999999999999", "nope", other.xuid.as_str()] {
        let a = call(&me, xuid, &fresh(), json!({"name": "Fine"})).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND".into()),
            "{xuid}"
        );
    }

    // A live play session refuses (the game got the name at launch).
    let binding: Uuid = sqlx::query_scalar(
        "INSERT INTO device_binding (account_id, device_id, label, platform) VALUES ($1, $2, 'PC', 'linux') RETURNING id",
    )
    .bind(Uuid::parse_str(&me.account_id).unwrap())
    .bind(Uuid::now_v7())
    .fetch_one(&st.owner)
    .await
    .unwrap();
    let profile_id: Uuid = sqlx::query_scalar("SELECT id FROM profile WHERE xuid = $1")
        .bind(me.xuid.parse::<i64>().unwrap())
        .fetch_one(&st.owner)
        .await
        .unwrap();
    let session = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, state, launcher_version, \
           admitted_at, expires_at, max_until, game_until) \
         VALUES ($1, $2, $3, $4, 'matchmaking', 'active', '0.1.0', clock_timestamp(), clock_timestamp() + interval '5 minutes', \
           now() + interval '12 hours', now() + interval '12 hours')",
    )
    .bind(session)
    .bind(Uuid::parse_str(&me.account_id).unwrap())
    .bind(profile_id)
    .bind(binding)
    .execute(&st.owner)
    .await
    .unwrap();
    let busy = call(&me, &me.xuid, &fresh(), json!({"name": "Busy"})).await;
    assert_eq!(
        (busy.status, busy.code()),
        (StatusCode::CONFLICT, "SESSIONS.PROFILE_IN_USE".into())
    );
    sqlx::query(
        "UPDATE play_session SET state = 'ended', ended_at = clock_timestamp() - interval '2 minutes', \
           end_reason = 'game_exit' WHERE id = $1",
    )
    .bind(session)
    .execute(&st.owner)
    .await
    .unwrap();

    // The free rename, its replay, and the key reused for another body.
    let wanted = format!("Rn{}", &fresh()[24..]);
    let key = fresh();
    let done = call(&me, &me.xuid, &key, json!({"name": wanted})).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.json());
    assert_eq!(done.json()["gamertag"], wanted.as_str());
    assert_eq!(
        done.json()["xuid"],
        me.xuid.as_str(),
        "the XUID never changes"
    );
    assert_eq!(done.json()["renameNotice"], Value::Null);
    let replay = call(&me, &me.xuid, &key, json!({"name": wanted})).await;
    assert_eq!(
        (replay.status, replay.json()),
        (StatusCode::OK, done.json())
    );
    let reused = call(&me, &me.xuid, &key, json!({"name": "Different"})).await;
    assert_eq!(
        (reused.status, reused.code()),
        (
            StatusCode::CONFLICT,
            "REQUEST.IDEMPOTENCY_KEY_REUSED".into()
        )
    );
    let again = call(&me, &me.xuid, &fresh(), json!({"name": "Third"})).await;
    assert_eq!(
        (again.status, again.code()),
        (StatusCode::CONFLICT, "ACCOUNTS.RENAME_NOT_ALLOWED".into()),
        "used once"
    );

    // An account pending deletion renames nothing.
    sqlx::query(
        "UPDATE account SET status = 'deletion_pending', deletion_requested_at = now(), \
           deletion_due_at = now() + interval '14 days' WHERE id = $1",
    )
    .bind(Uuid::parse_str(&leaving.account_id).unwrap())
    .execute(&st.owner)
    .await
    .unwrap();
    let pending = call(&leaving, &leaving.xuid, &fresh(), json!({"name": "Late"})).await;
    assert_eq!(
        (pending.status, pending.code()),
        (StatusCode::FORBIDDEN, "ACCOUNTS.DELETION_PENDING".into())
    );

    // A fingerprint is stored only as its HMAC, never returned; a malformed one is refused.
    let (_, id_token) = emulator_user(&st.emulator, true);
    let mut dev = device();
    dev["fingerprint"] = json!("PSXtcjPZwnXs4v2YDc0sOcRnKGvb6RCkn9-d3jPPPWg");
    let mut bad = dev.clone();
    bad["fingerprint"] = json!("short");
    let (s, refused) = st.launcher_sign_in(&id_token, &bad).await;
    assert_eq!(
        (s, code(&refused)),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID")
    );
    let (s, signed) = st.launcher_sign_in(&id_token, &dev).await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    let (s, created) = st
        .post(
            lr::ACCOUNTS,
            Caller::Launcher(None),
            &fresh(),
            json!({
                "identityTicket": signed["identityTicket"],
                "device": dev,
                "profile": {"game": "fh2", "name": format!("Fp{}", &fresh()[24..]), "mode": "noob"},
            }),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let stored: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT fingerprint_hmac FROM device_binding WHERE account_id = $1")
            .bind(Uuid::parse_str(created["accountId"].as_str().unwrap()).unwrap())
            .fetch_one(&st.owner)
            .await
            .unwrap();
    let stored = stored.expect("the HMAC is stored");
    assert_eq!(stored.len(), 32);
    assert_ne!(
        stored.as_slice(),
        b"PSXtcjPZwnXs4v2YDc0sOcRnKGvb6RCkn9-d3jPPPWg".as_slice()
    );
    let (_, view) = st
        .get(
            lr::ME,
            Caller::Launcher(Some(created["accessToken"].as_str().unwrap())),
        )
        .await;
    assert!(!view.to_string().contains("fingerprint"), "{view}");

    emulator_delete(&st.emulator, &uid_of(&id_token));
    for p in [&me, &other, &leaving] {
        emulator_delete(&st.emulator, &uid_of(&p.id_token));
    }
}

/// OD-52, ACCOUNTS.md §5 "Mode change", over the router: every status and code of
/// `POST /launcher/v1/profiles/{xuid}/mode`, the warning's details, the confirmed change and its
/// replay, and what it deletes (the universe rows) and keeps (friendships, the retired line).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn hardcore_to_noob_warns_then_deletes_the_cloud_data() {
    let st = stack().await;
    let me = st.player().await;
    let other = st.player().await;
    let leaving = st.player().await;
    let call = |p: &Player, xuid: &str, key: Option<&str>, body: Value| {
        let access = p.access.clone();
        let uri = mode_uri(xuid);
        let key = key.map(str::to_owned);
        let st = &st;
        async move {
            st.launcher_json(Method::POST, &uri, &access, key.as_deref(), Some(body))
                .await
        }
    };
    let created = st
        .launcher_json(
            Method::POST,
            lr::PROFILES,
            &me.access,
            Some(&fresh()),
            Some(
                json!({"game": "fh2", "name": format!("Hc{}", &fresh()[24..]), "mode": "hardcore"}),
            ),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    assert_eq!(created.json()["mode"], "hardcore");
    let hc = created.json()["xuid"].as_str().unwrap().to_owned();
    let hc_id = Uuid::parse_str(created.json()["profileId"].as_str().unwrap()).unwrap();
    let noob_id: Uuid = sqlx::query_scalar("SELECT id FROM profile WHERE xuid = $1")
        .bind(me.xuid.parse::<i64>().unwrap())
        .fetch_one(&st.owner)
        .await
        .unwrap();
    // Cloud data of the Hardcore profile, and a friendship of its account (OD-55).
    sqlx::query(
        "INSERT INTO game_score (game, mode, profile_id, scoreboard_type, score_type, game_scoreboard_id, track_id, score, \
           score_id, car_id, car_class_id, car_pi, is_clean, used_stm, used_tcs, used_abs, used_auto_brake, \
           used_auto_shifting, used_clutch, data) \
         VALUES ('fh2', 'hardcore', $1, 1, 1, 10, 3, 61.5, $2, 289, 4, 0.6, true, false, false, false, false, false, false, '{}')",
    )
    .bind(hc_id)
    .bind(Uuid::now_v7())
    .execute(&st.owner)
    .await
    .unwrap();
    sqlx::query("INSERT INTO game_profile (game, profile_id, data) VALUES ('fh2', $1, '{}')")
        .bind(hc_id)
        .execute(&st.owner)
        .await
        .unwrap();
    // Friendships are the accounts' (OD-55): this account and another player's.
    let account_of = |profile: Uuid| {
        let owner = st.owner.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>("SELECT account_id FROM profile WHERE id = $1")
                .bind(profile)
                .fetch_one(&owner)
                .await
                .unwrap()
        }
    };
    let other_id: Uuid = sqlx::query_scalar("SELECT id FROM profile WHERE xuid = $1")
        .bind(other.xuid.parse::<i64>().unwrap())
        .fetch_one(&st.owner)
        .await
        .unwrap();
    let (mine, theirs) = (account_of(noob_id).await, account_of(other_id).await);
    for (a, b) in [(mine, theirs), (theirs, mine)] {
        sqlx::query("INSERT INTO account_friendship (account_id, friend_id) VALUES ($1, $2)")
            .bind(a)
            .bind(b)
            .execute(&st.owner)
            .await
            .unwrap();
    }

    // Noob -> Noob: a no-op, no confirmation needed.
    let same = call(&me, &me.xuid, Some(&fresh()), json!({"mode": "noob"})).await;
    assert_eq!(same.status, StatusCode::OK, "{}", same.json());
    assert_eq!(same.json()["mode"], "noob");
    // Refusals: forbidden directions, malformed bodies, the key, unknown and others' profiles.
    for (xuid, body) in [
        (&hc, json!({"mode": "hardcore"})),
        (&me.xuid, json!({"mode": "hardcore", "confirm": true})),
    ] {
        let a = call(&me, xuid, Some(&fresh()), body).await;
        assert_eq!(
            (a.status, a.code()),
            (
                StatusCode::CONFLICT,
                "ACCOUNTS.MODE_CHANGE_FORBIDDEN".into()
            )
        );
    }
    for body in [
        json!({"mode": "turbo"}),
        json!({"mode": "noob", "force": true}),
    ] {
        let a = call(&me, &hc, Some(&fresh()), body).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::BAD_REQUEST, "REQUEST.INVALID".into())
        );
    }
    let keyless = call(&me, &hc, None, json!({"mode": "noob"})).await;
    assert_eq!(keyless.code(), "REQUEST.INVALID");
    for (caller, xuid) in [
        (&me, "2535999999999999"),
        (&me, "abc"),
        (&other, hc.as_str()),
    ] {
        let a = call(
            caller,
            xuid,
            Some(&fresh()),
            json!({"mode": "noob", "confirm": true}),
        )
        .await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND".into()),
            "{xuid}"
        );
    }

    // A live play session refuses, warning and change alike.
    let binding: Uuid = sqlx::query_scalar(
        "INSERT INTO device_binding (account_id, device_id, label, platform) VALUES ($1, $2, 'PC', 'linux') RETURNING id",
    )
    .bind(Uuid::parse_str(&me.account_id).unwrap())
    .bind(Uuid::now_v7())
    .fetch_one(&st.owner)
    .await
    .unwrap();
    let session = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, state, launcher_version, \
           admitted_at, expires_at, max_until, game_until) \
         VALUES ($1, $2, $3, $4, 'matchmaking', 'active', '0.1.0', clock_timestamp(), clock_timestamp() + interval '5 minutes', \
           now() + interval '12 hours', now() + interval '12 hours')",
    )
    .bind(session)
    .bind(Uuid::parse_str(&me.account_id).unwrap())
    .bind(hc_id)
    .bind(binding)
    .execute(&st.owner)
    .await
    .unwrap();
    for body in [
        json!({"mode": "noob"}),
        json!({"mode": "noob", "confirm": true}),
    ] {
        let a = call(&me, &hc, Some(&fresh()), body).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::CONFLICT, "SESSIONS.PROFILE_IN_USE".into())
        );
    }
    sqlx::query(
        "UPDATE play_session SET state = 'ended', ended_at = clock_timestamp(), end_reason = 'game_exit' WHERE id = $1",
    )
    .bind(session)
    .execute(&st.owner)
    .await
    .unwrap();

    // The warning: counts, nothing written, nothing recorded (its key serves the confirmation).
    let key = fresh();
    let warned = call(&me, &hc, Some(&key), json!({"mode": "noob"})).await;
    assert_eq!(
        (warned.status, warned.code()),
        (StatusCode::CONFLICT, "ACCOUNTS.MODE_CHANGE_CONFIRM".into()),
        "{}",
        warned.json()
    );
    let details = &warned.json()["details"];
    assert_eq!(details["saveRevision"], 0);
    assert_eq!(
        details["deletes"],
        json!({"drivatars": 0, "ghosts": 0, "inbox": 0, "onlineProfile": 1, "saveRevisions": 0, "scores": 1, "xp": 0})
    );
    let declined = call(
        &me,
        &hc,
        Some(&fresh()),
        json!({"mode": "noob", "confirm": false}),
    )
    .await;
    assert_eq!(declined.code(), "ACCOUNTS.MODE_CHANGE_CONFIRM");
    let (_, viewer) = st
        .get(lr::ME, Caller::Launcher(Some(me.access.as_str())))
        .await;
    let mode_of = |viewer: &Value, xuid: &str| {
        viewer["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["xuid"] == xuid)
            .map(|p| p["mode"].clone())
    };
    assert_eq!(mode_of(&viewer, &hc), Some(json!("hardcore")));

    // Confirmed with the warning's key; its replay answers the same and deletes nothing twice.
    let confirm = json!({"mode": "noob", "confirm": true});
    let changed = call(&me, &hc, Some(&key), confirm.clone()).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.json());
    assert_eq!(changed.json()["mode"], "noob");
    assert_eq!(changed.json()["xuid"], hc.as_str());
    let replay = call(&me, &hc, Some(&key), confirm.clone()).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json(), changed.json());
    let reused = call(&me, &hc, Some(&key), json!({"mode": "noob"})).await;
    assert_eq!(reused.code(), "REQUEST.IDEMPOTENCY_KEY_REUSED");
    // The same key and body on another profile is another request.
    let elsewhere = call(&me, &me.xuid, Some(&key), confirm.clone()).await;
    assert_eq!(elsewhere.code(), "REQUEST.IDEMPOTENCY_KEY_REUSED");
    let again = call(&me, &hc, Some(&fresh()), confirm.clone()).await;
    assert_eq!(
        (again.status, again.json()["mode"].clone()),
        (StatusCode::OK, json!("noob"))
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM account_audit WHERE account_id = $1 AND kind = 'profile.mode_changed'",
    )
    .bind(Uuid::parse_str(&me.account_id).unwrap())
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    let (_, viewer) = st
        .get(lr::ME, Caller::Launcher(Some(me.access.as_str())))
        .await;
    assert_eq!(mode_of(&viewer, &hc), Some(json!("noob")));

    // Deleted: the universe rows. Kept: the friendship, both sides. Retired: the line.
    let count = |sql: &'static str| {
        let owner = st.owner.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(hc_id)
                .fetch_one(&owner)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count("SELECT count(*) FROM game_score WHERE profile_id = $1").await,
        0
    );
    assert_eq!(
        count("SELECT count(*) FROM game_profile WHERE profile_id = $1").await,
        0
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM account_friendship f JOIN profile p ON p.id = $1 \
             WHERE f.account_id = p.account_id OR f.friend_id = p.account_id"
        )
        .await,
        2
    );
    assert_eq!(
        count("SELECT count(*) FROM save_head WHERE profile_id = $1 AND retired_at IS NOT NULL")
            .await,
        1
    );

    // An account pending deletion changes nothing.
    sqlx::query(
        "UPDATE account SET status = 'deletion_pending', deletion_requested_at = now(), \
           deletion_due_at = now() + interval '14 days' WHERE id = $1",
    )
    .bind(Uuid::parse_str(&leaving.account_id).unwrap())
    .execute(&st.owner)
    .await
    .unwrap();
    let pending = call(
        &leaving,
        &leaving.xuid,
        Some(&fresh()),
        json!({"mode": "noob"}),
    )
    .await;
    assert_eq!(
        (pending.status, pending.code()),
        (StatusCode::FORBIDDEN, "ACCOUNTS.DELETION_PENDING".into())
    );

    for p in [&me, &other, &leaving] {
        emulator_delete(&st.emulator, &uid_of(&p.id_token));
    }
}

fn profile_uri(xuid: &str) -> String {
    lr::PROFILE.replace("{xuid}", xuid)
}

/// Owner 2026-10-27 (ACCOUNTS.md §5 `DELETE /profiles/{xuid}`, CR-S61), over the router: every
/// status and code of a player's profile deletion, the replay, the freed name and slot, the last
/// profile, and the rolling-hour limit with its `Retry-After`. CR-S98: a released launcher's route; each
/// profile here has one (FH2) career, so its deletion takes the profile and frees name and slot
/// (Hardcore: the player's own profile is Noob, one live profile per mode).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_player_deletes_a_profile_from_the_cloud_at_once() {
    let st = stack().await;
    let me = st.player().await;
    let other = st.player().await;
    let solo = st.player().await;
    let leaving = st.player().await;
    let banned = st.player().await;
    let del = |p: &Player, xuid: &str, key: Option<&str>| {
        let access = p.access.clone();
        let uri = profile_uri(xuid);
        let key = key.map(str::to_owned);
        let st = &st;
        async move {
            st.launcher_json(Method::DELETE, &uri, &access, key.as_deref(), None)
                .await
        }
    };
    let add = |p: &Player, name: String| {
        let access = p.access.clone();
        let st = &st;
        async move {
            st.launcher_json(
                Method::POST,
                lr::PROFILES,
                &access,
                Some(&fresh()),
                Some(json!({"game": "fh2", "name": name, "mode": "hardcore"})),
            )
            .await
        }
    };
    let names = |p: &Player| {
        let access = p.access.clone();
        let st = &st;
        async move {
            let (_, viewer) = st
                .get(lr::ME, Caller::Launcher(Some(access.as_str())))
                .await;
            viewer["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["xuid"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    let account = |p: &Player| Uuid::parse_str(&p.account_id).unwrap();

    // 204, no body; the replay answers the same; the key on another profile is another request.
    let name1 = format!("Dl{}", &fresh()[24..]);
    let p1 = add(&me, name1.clone()).await;
    assert_eq!(p1.status, StatusCode::CREATED, "{}", p1.json());
    let x1 = p1.json()["xuid"].as_str().unwrap().to_owned();
    let p1_id = Uuid::parse_str(p1.json()["profileId"].as_str().unwrap()).unwrap();
    sqlx::query("INSERT INTO game_profile (game, profile_id, data) VALUES ('fh2', $1, '{}')")
        .bind(p1_id)
        .execute(&st.owner)
        .await
        .unwrap();
    let keyless = del(&me, &x1, None).await;
    assert_eq!(
        (keyless.status, keyless.code()),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID".into())
    );
    let key = fresh();
    let done = del(&me, &x1, Some(&key)).await;
    assert_eq!(done.status, StatusCode::NO_CONTENT, "{}", done.json());
    assert!(done.body.is_empty());
    let replay = del(&me, &x1, Some(&key)).await;
    assert_eq!(replay.status, StatusCode::NO_CONTENT);
    let elsewhere = del(&me, &me.xuid, Some(&key)).await;
    assert_eq!(elsewhere.code(), "REQUEST.IDEMPOTENCY_KEY_REUSED");
    assert!(!names(&me).await.contains(&x1));
    let (status, data): (String, i64) = sqlx::query_as(
        "SELECT p.status, (SELECT count(*) FROM game_profile g WHERE g.profile_id = p.id) FROM profile p WHERE p.id = $1",
    )
    .bind(p1_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!((status.as_str(), data), ("deleted", 0));
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM account_audit WHERE account_id = $1 AND kind = 'profile.deleted' AND actor = 'player'",
    )
    .bind(account(&me))
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    // The name is free again (here, for the same account).
    let again = add(&me, name1.clone()).await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.json());
    let x2 = again.json()["xuid"].as_str().unwrap().to_owned();
    let p2_id = Uuid::parse_str(again.json()["profileId"].as_str().unwrap()).unwrap();
    assert_ne!(x2, x1);

    // 404: the deleted profile (a new key), an unknown or malformed XUID, another account's.
    for (caller, xuid) in [
        (&me, x1.as_str()),
        (&me, "2535999999999999"),
        (&me, "abc"),
        (&other, x2.as_str()),
    ] {
        let a = del(caller, xuid, Some(&fresh())).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND".into()),
            "{xuid}"
        );
    }

    // 409: a live play session; then a game ticket the game plane may still serve.
    let binding: Uuid = sqlx::query_scalar(
        "INSERT INTO device_binding (account_id, device_id, label, platform) VALUES ($1, $2, 'PC', 'linux') RETURNING id",
    )
    .bind(account(&me))
    .bind(Uuid::now_v7())
    .fetch_one(&st.owner)
    .await
    .unwrap();
    let session = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, state, launcher_version, \
           admitted_at, expires_at, max_until, game_until, ticket_kid, ticket_exp) \
         VALUES ($1, $2, $3, $4, 'matchmaking', 'active', '0.1.0', clock_timestamp(), clock_timestamp() + interval '5 minutes', \
           now() + interval '12 hours', now() + interval '12 hours', 'k1', clock_timestamp() + interval '5 minutes')",
    )
    .bind(session)
    .bind(account(&me))
    .bind(p2_id)
    .bind(binding)
    .execute(&st.owner)
    .await
    .unwrap();
    let busy = del(&me, &x2, Some(&fresh())).await;
    assert_eq!(
        (busy.status, busy.code()),
        (StatusCode::CONFLICT, "SESSIONS.PROFILE_IN_USE".into())
    );
    sqlx::query(
        "UPDATE play_session SET state = 'ended', ended_at = clock_timestamp(), end_reason = 'game_exit' WHERE id = $1",
    )
    .bind(session)
    .execute(&st.owner)
    .await
    .unwrap();
    let ticket = del(&me, &x2, Some(&fresh())).await;
    assert_eq!(
        (ticket.status, ticket.code()),
        (StatusCode::CONFLICT, "SESSIONS.PROFILE_IN_USE".into())
    );
    assert!(ticket.json()["details"]["ticketExpiresAt"].is_string());
    let wait: u64 = ticket.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
    sqlx::query(
        "UPDATE play_session SET ended_at = clock_timestamp() - interval '2 minutes', \
           admitted_at = clock_timestamp() - interval '3 minutes' WHERE id = $1",
    )
    .bind(session)
    .execute(&st.owner)
    .await
    .unwrap();
    assert!(names(&me).await.contains(&x2));

    // 2 and 3 of the hour land (the ended session's profile included); the 4th is 429.
    let second = del(&me, &x2, Some(&fresh())).await;
    assert_eq!(second.status, StatusCode::NO_CONTENT, "{}", second.json());
    let p3 = add(&me, format!("Dm{}", &fresh()[24..])).await;
    let x3 = p3.json()["xuid"].as_str().unwrap().to_owned();
    assert_eq!(
        del(&me, &x3, Some(&fresh())).await.status,
        StatusCode::NO_CONTENT
    );
    let p4 = add(&me, format!("Dn{}", &fresh()[24..])).await;
    let x4 = p4.json()["xuid"].as_str().unwrap().to_owned();
    let limited = del(&me, &x4, Some(&fresh())).await;
    assert_eq!(
        (limited.status, limited.code()),
        (StatusCode::TOO_MANY_REQUESTS, "REQUEST.RATE_LIMITED".into())
    );
    let wait: u64 = limited.header("retry-after").unwrap().parse().unwrap();
    assert!((3500..=3600).contains(&wait), "{wait}");
    assert!(names(&me).await.contains(&x4));

    // The last profile may go: the account stays, with none.
    let last = del(&solo, &solo.xuid, Some(&fresh())).await;
    assert_eq!(last.status, StatusCode::NO_CONTENT, "{}", last.json());
    assert!(names(&solo).await.is_empty());

    // 403: an account pending deletion; a banned account (a block pending review).
    sqlx::query(
        "UPDATE account SET status = 'deletion_pending', deletion_requested_at = now(), \
           deletion_due_at = now() + interval '14 days' WHERE id = $1",
    )
    .bind(account(&leaving))
    .execute(&st.owner)
    .await
    .unwrap();
    let pending = del(&leaving, &leaving.xuid, Some(&fresh())).await;
    assert_eq!(
        (pending.status, pending.code()),
        (StatusCode::FORBIDDEN, "ACCOUNTS.DELETION_PENDING".into())
    );
    sqlx::query(
        "UPDATE account SET banned_at = clock_timestamp(), ban_reason = 'test', ban_review = 'seed_from_profile' WHERE id = $1",
    )
    .bind(account(&banned))
    .execute(&st.owner)
    .await
    .unwrap();
    let refused = del(&banned, &banned.xuid, Some(&fresh())).await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "ACCOUNTS.BANNED".into())
    );
    assert_eq!(refused.json()["details"]["review"], "seed_from_profile");
    for p in [&leaving, &banned] {
        let status: String = sqlx::query_scalar("SELECT status FROM profile WHERE xuid = $1")
            .bind(p.xuid.parse::<i64>().unwrap())
            .fetch_one(&st.owner)
            .await
            .unwrap();
        assert_ne!(status, "deleted");
    }

    for p in [&me, &other, &solo, &leaving, &banned] {
        emulator_delete(&st.emulator, &uid_of(&p.id_token));
    }
}

/// CR-S98 (CAREERS.md §3), over the router: `POST /players/{xuid}/careers` (201, the replay, 409
/// `ACCOUNTS.CAREER_EXISTS`, 400 `profile.mode` for FM5 Hardcore and `profile.game`, 404, the
/// key), `players` on `/me`, `?game=` picking the career, and the two deletions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_profile_starts_and_deletes_careers_of_other_games() {
    let st = stack().await;
    // FM5 is open only while a pool of it accepts sessions (the fm5 suites share this row).
    sqlx::query(
        "INSERT INTO pool (name, game, tier, game_endpoint, accepting) \
         VALUES ('fm5', 'fm5', 'free', 'http://127.0.0.1:1/x', true) \
         ON CONFLICT (name) DO UPDATE SET accepting = true",
    )
    .execute(&st.owner)
    .await
    .unwrap();
    let me = st.player().await;
    let hard = st.player_as("hardcore").await;
    let careers = |p: &Player, xuid: &str, key: Option<String>, game: &str| {
        let access = p.access.clone();
        let uri = lr::PLAYER_CAREERS.replace("{xuid}", xuid);
        let body = json!({ "game": game });
        let st = &st;
        async move {
            st.launcher_json(Method::POST, &uri, &access, key.as_deref(), Some(body))
                .await
        }
    };
    let players = |p: &Player| {
        let access = p.access.clone();
        let st = &st;
        async move {
            let (_, viewer) = st
                .get(lr::ME, Caller::Launcher(Some(access.as_str())))
                .await;
            viewer
        }
    };

    // 201: the FM5 career, the profile's XUID and name; the replay answers it again.
    let key = fresh();
    let made = careers(&me, &me.xuid, Some(key.clone()), "fm5").await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.json());
    assert_eq!(made.json()["game"], "fm5");
    assert_eq!(made.json()["xuid"], me.xuid.as_str());
    let replay = careers(&me, &me.xuid, Some(key), "fm5").await;
    assert_eq!(
        (replay.status, &replay.json()["profileId"]),
        (StatusCode::CREATED, &made.json()["profileId"])
    );
    // 409: it exists (FH2's too); 400: no key, FM5 Hardcore, an unknown game; 404: another's.
    for game in ["fm5", "fh2"] {
        let again = careers(&me, &me.xuid, Some(fresh()), game).await;
        assert_eq!(
            (again.status, again.code()),
            (StatusCode::CONFLICT, "ACCOUNTS.CAREER_EXISTS".into()),
            "{game}"
        );
    }
    let keyless = careers(&me, &me.xuid, None, "fm5").await;
    assert_eq!(keyless.code(), "REQUEST.INVALID");
    let hardcore = careers(&hard, &hard.xuid, Some(fresh()), "fm5").await;
    assert_eq!(
        (hardcore.status, hardcore.json()["details"]["field"].clone()),
        (StatusCode::BAD_REQUEST, json!("profile.mode"))
    );
    let unknown = careers(&me, &me.xuid, Some(fresh()), "zz9").await;
    assert_eq!(
        (unknown.status, unknown.json()["details"]["field"].clone()),
        (StatusCode::BAD_REQUEST, json!("profile.game"))
    );
    let foreign = careers(&hard, &me.xuid, Some(fresh()), "fm5").await;
    assert_eq!(
        (foreign.status, foreign.code()),
        (StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND".into())
    );

    // `/me`: two careers in `profiles`, one profile above them in `players`.
    let viewer = players(&me).await;
    assert_eq!(viewer["profiles"].as_array().map(Vec::len), Some(2));
    assert_eq!(viewer["players"].as_array().map(Vec::len), Some(1));
    let games: Vec<&str> = viewer["players"][0]["careers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["game"].as_str().unwrap())
        .collect();
    assert_eq!(games, ["fh2", "fm5"]);

    // `?game=` picks the career: FM5's presence answers, an unknown game's is not found.
    let presence = |game: &str| {
        let uri = format!(
            "{}?game={game}",
            lr::PROFILE_PRESENCE.replace("{xuid}", &me.xuid)
        );
        let access = me.access.clone();
        let st = &st;
        async move {
            st.launcher_json(Method::GET, &uri, &access, None, None)
                .await
        }
    };
    assert_eq!(presence("fm5").await.status, StatusCode::OK);
    assert_eq!(presence("fh2").await.status, StatusCode::OK);
    assert_eq!(presence("zz9").await.code(), "ACCOUNTS.NOT_FOUND");
    // CAREERS.md §6.1: a XUID with careers in two games names the game, else 400 and nothing done.
    let bare = st
        .launcher_json(
            Method::GET,
            &lr::PROFILE_PRESENCE.replace("{xuid}", &me.xuid),
            &me.access,
            None,
            None,
        )
        .await;
    assert_eq!(
        (bare.status, bare.json()["details"]["field"].clone()),
        (StatusCode::BAD_REQUEST, json!("game"))
    );
    let unnamed = st
        .launcher_json(
            Method::DELETE,
            &profile_uri(&me.xuid),
            &me.access,
            Some(&fresh()),
            None,
        )
        .await;
    assert_eq!(
        (unnamed.status, unnamed.json()["details"]["field"].clone()),
        (StatusCode::BAD_REQUEST, json!("game"))
    );
    let head = st
        .launcher_json(
            Method::GET,
            &lr::SAVE.replace("{xuid}", &me.xuid),
            &me.access,
            None,
            None,
        )
        .await;
    assert_eq!(head.json()["details"]["field"], "game");
    // `players` carries the profile's status and notice; no progress before a sync.
    let viewer = players(&me).await;
    assert_eq!(viewer["players"][0]["status"], "active");
    assert!(viewer["players"][0]["renameNotice"].is_null());
    assert!(viewer["players"][0]["careers"][0].get("progress").is_none());
    assert_eq!(
        viewer["players"][0]["careers"][1]["careerId"],
        made.json()["profileId"]
    );

    // Delete career (FM5): FH2's career and the profile stay.
    let gone = st
        .launcher_json(
            Method::DELETE,
            &format!("{}?game=fm5", profile_uri(&me.xuid)),
            &me.access,
            Some(&fresh()),
            None,
        )
        .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.json());
    let viewer = players(&me).await;
    assert_eq!(viewer["profiles"].as_array().map(Vec::len), Some(1));
    assert_eq!(viewer["profiles"][0]["game"], "fh2");
    assert_eq!(
        viewer["players"][0]["careers"].as_array().map(Vec::len),
        Some(1)
    );
    // The deleted career's game starts again (its tombstone does not count).
    let restarted = careers(&me, &me.xuid, Some(fresh()), "fm5").await;
    assert_eq!(
        restarted.status,
        StatusCode::CREATED,
        "{}",
        restarted.json()
    );
    assert_ne!(restarted.json()["profileId"], made.json()["profileId"]);
    // Delete profile: everything goes; again is 404.
    let player_uri = lr::PLAYER.replace("{xuid}", &me.xuid);
    let all = st
        .launcher_json(
            Method::DELETE,
            &player_uri,
            &me.access,
            Some(&fresh()),
            None,
        )
        .await;
    assert_eq!(all.status, StatusCode::NO_CONTENT, "{}", all.json());
    let viewer = players(&me).await;
    assert_eq!(viewer["players"].as_array().map(Vec::len), Some(0));
    assert_eq!(viewer["profiles"].as_array().map(Vec::len), Some(0));
    let again = st
        .launcher_json(
            Method::DELETE,
            &player_uri,
            &me.access,
            Some(&fresh()),
            None,
        )
        .await;
    assert_eq!(again.code(), "ACCOUNTS.NOT_FOUND");

    for p in [&me, &hard] {
        emulator_delete(&st.emulator, &uid_of(&p.id_token));
    }
}

/// Registration-only closure: pending/replayed/new identities cannot create an account;
/// existing accounts still log in, rotate refresh and sign out on the same real DB/router.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and auth emulator"]
async fn registration_only_closure_preserves_existing_accounts_and_pending_tickets() {
    let open = stack().await;
    let existing = open.player().await;
    let (_, unknown) = emulator_user(&open.emulator, true);
    let (_, portal_unknown) = emulator_user(&open.emulator, true);
    let dev = device();
    let native_key = fresh();
    let portal_key = fresh();
    let grant = id_token_grant(&unknown, &dev);
    let (status, pending) = open
        .post(
            lr::AUTH_TOKEN,
            Caller::Launcher(None),
            &native_key,
            grant.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, portal_pending) = open
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &portal_key,
            json!({"idToken":portal_unknown}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let mut identity = (*open.identity).clone();
    identity.info.registration_enabled = false;
    let identity = Arc::new(identity);
    let closed = Stack {
        app: app::router(
            ServiceInfo::from_env("fi-api", "0.0.0"),
            AppState {
                pool: open.pool.clone(),
                identity: Some(identity.clone()),
            },
            &Config::parse("launcher,portal", None).unwrap(),
        ),
        emulator: open.emulator.clone(),
        portal_ip: open.portal_ip.clone(),
        launcher_ip: open.launcher_ip.clone(),
        owner: open.owner.clone(),
        pool: open.pool.clone(),
        identity,
    };
    let (status, info) = closed.get(lr::INFO, Caller::Launcher(None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["registrationEnabled"], false);
    let account = json!({"identityTicket":pending["identityTicket"],"device":dev,"profile":{"game":"fh2","name":format!("Rg{}",&fresh()[24..]),"mode":"noob"}});
    let portal_account = json!({"identityTicket":portal_pending["identityTicket"]});
    for (path, caller, key, body) in [
        (
            lr::AUTH_TOKEN,
            Caller::Launcher(None),
            native_key.clone(),
            grant.clone(),
        ),
        (
            lr::AUTH_TOKEN,
            Caller::Launcher(None),
            fresh(),
            grant.clone(),
        ),
        (
            lr::ACCOUNTS,
            Caller::Launcher(None),
            fresh(),
            account.clone(),
        ),
        (
            pr::SESSIONS,
            Caller::Portal(None),
            portal_key,
            json!({"idToken":portal_unknown}),
        ),
        (
            pr::SESSIONS,
            Caller::Portal(None),
            fresh(),
            json!({"idToken":portal_unknown}),
        ),
        (
            pr::ACCOUNTS,
            Caller::Portal(None),
            fresh(),
            portal_account.clone(),
        ),
    ] {
        let (status, body) = closed.post(path, caller, &key, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        assert_eq!(
            code(&body),
            "ACCOUNTS.REGISTRATION_DISABLED",
            "{path}: {body}"
        );
    }
    let (status, signed) = closed.launcher_sign_in(&existing.id_token, &device()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(signed["status"], "signed_in");
    let (status, refreshed) = closed
        .post(
            lr::TOKEN,
            Caller::Launcher(None),
            &fresh(),
            json!({"refreshToken":signed["refreshToken"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    let (status, _) = closed
        .post(
            lr::SIGN_OUT,
            Caller::Launcher(refreshed["accessToken"].as_str()),
            &fresh(),
            json!({}),
        )
        .await;
    assert!(status.is_success());
    let (status, portal) = closed
        .post(
            pr::SESSIONS,
            Caller::Portal(None),
            &fresh(),
            json!({"idToken":existing.id_token}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(portal["status"], "signed_in");
    // Exact creation replay remains blocked after closure, including already successful keys.
    let native_create_key = fresh();
    let portal_create_key = fresh();
    let (status, created) = open
        .post(
            lr::ACCOUNTS,
            Caller::Launcher(None),
            &native_create_key,
            account.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, created) = open
        .post(
            pr::ACCOUNTS,
            Caller::Portal(None),
            &portal_create_key,
            portal_account.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    for (path, caller, key, body) in [
        (
            lr::ACCOUNTS,
            Caller::Launcher(None),
            native_create_key,
            account,
        ),
        (
            pr::ACCOUNTS,
            Caller::Portal(None),
            portal_create_key,
            portal_account,
        ),
    ] {
        let (status, replay_refused) = closed.post(path, caller, &key, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {replay_refused}");
        assert_eq!(code(&replay_refused), "ACCOUNTS.REGISTRATION_DISABLED");
    }
}

// ---------------------------------------------------------------------------------------------
// OD-122 M2.3: the `local` sign-in backend (ACCOUNTS.md §4.1 "Two identity backends", §5).
// Postgres only: the instance is its own identity provider.
// ---------------------------------------------------------------------------------------------

/// A stack on `FI_AUTH_BACKEND=local` with a runtime pool of `connections`.
async fn local_stack(connections: u32) -> Stack {
    let (url, pool) = runtime_pool(connections).await;
    stack_of(
        url,
        pool,
        String::new(),
        &config::tests::local_backend(),
        "launcher,portal",
        None,
    )
    .await
}

fn local_email() -> String {
    format!("Local-{}@Example.TEST", fresh())
}

fn credentials(email: &str, password: &str) -> Value {
    json!({"email": email, "password": password})
}

impl Stack {
    fn local_idp(&self) -> &fi_auth::localidp::LocalIdp {
        self.identity.local_idp.as_ref().expect("a local stack")
    }

    /// The credential id an ID token names.
    fn local_sub(&self, id_token: &str) -> String {
        self.local_idp()
            .verify_unconfirmed(id_token, unix_now())
            .expect("a valid local ID token")
            .uid
    }

    async fn sign_up(&self, email: &str, password: &str) -> (StatusCode, Value) {
        self.post(
            lr::AUTH_SIGN_UP,
            Caller::Launcher(None),
            &fresh(),
            credentials(email, password),
        )
        .await
    }

    async fn sign_in_local(&self, email: &str, password: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            lr::AUTH_SIGN_IN,
            Caller::Launcher(None),
            None,
            Some(credentials(email, password)),
        )
        .await
    }

    async fn change_password(&self, id_token: &str, new_password: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            lr::AUTH_PASSWORD,
            Caller::Launcher(None),
            None,
            Some(json!({"idToken": id_token, "newPassword": new_password})),
        )
        .await
    }

    /// Sign-up, exchange, account creation: `(email, accountId, refreshToken, accessToken, name)`.
    async fn local_player(&self, password: &str) -> (String, String, String, String, String) {
        let email = local_email();
        let (s, up) = self.sign_up(&email, password).await;
        assert_eq!(s, StatusCode::CREATED, "{up}");
        let dev = device();
        let (s, ex) = self
            .launcher_sign_in(up["idToken"].as_str().unwrap(), &dev)
            .await;
        assert_eq!(
            (s, ex["status"].as_str()),
            (StatusCode::OK, Some("needs_account")),
            "{ex}"
        );
        let name = format!("Lo{}", &fresh()[24..]);
        let (s, created) = self
            .post(
                lr::ACCOUNTS,
                Caller::Launcher(None),
                &fresh(),
                json!({"identityTicket": ex["identityTicket"], "device": dev,
                       "profile": {"game": "fh2", "name": name, "mode": "noob"}}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{created}");
        (
            email,
            created["accountId"].as_str().unwrap().to_owned(),
            created["refreshToken"].as_str().unwrap().to_owned(),
            created["accessToken"].as_str().unwrap().to_owned(),
            name,
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_sign_up_exchange_account_me_and_sign_in() {
    let st = local_stack(8).await;
    let (s, info) = st.get(lr::INFO, Caller::Launcher(None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["idp"], json!({"kind": "local"}), "{info}");

    // Sign-up: 201 with a 5-minute ID token for the normalized address.
    let email = local_email();
    let key = fresh();
    let body = credentials(&format!("  {email} "), "synthetic-pass-1");
    let (s, up) = st
        .post(lr::AUTH_SIGN_UP, Caller::Launcher(None), &key, body.clone())
        .await;
    assert_eq!(s, StatusCode::CREATED, "{up}");
    assert_eq!(up["expiresIn"], 300);
    let id_token = up["idToken"].as_str().unwrap().to_owned();
    let sub = st.local_sub(&id_token);
    let verified = st.local_idp().verify(&id_token, unix_now()).unwrap();
    assert_eq!(
        verified.email.as_deref(),
        Some(email.to_lowercase().as_str())
    );

    // The same key and body: a fresh token for the same credential; another body: key reused.
    let (s, again) = st
        .post(lr::AUTH_SIGN_UP, Caller::Launcher(None), &key, body)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{again}");
    assert_eq!(st.local_sub(again["idToken"].as_str().unwrap()), sub);
    let (s, b) = st
        .post(
            lr::AUTH_SIGN_UP,
            Caller::Launcher(None),
            &key,
            credentials(&email, "synthetic-pass-2"),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "REQUEST.IDEMPOTENCY_KEY_REUSED")
    );
    // The address, whatever its case, under a new key: taken.
    let (s, b) = st.sign_up(&email.to_uppercase(), "synthetic-pass-1").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "ACCOUNTS.EMAIL_TAKEN")
    );
    // The idempotency record holds no plain digest of the request.
    let plain = fi_auth::sha256(
        format!(
            "POST /launcher/v1/auth/sign-up\n{}",
            credentials(&email, "synthetic-pass-1")
        )
        .as_bytes(),
    );
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM idempotency_record WHERE key = $1::uuid AND request_sha256 = $2",
    )
    .bind(&key)
    .bind(plain.as_slice())
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(stored, 0);
    let hash: String =
        sqlx::query_scalar("SELECT password_hash FROM credential WHERE id = $1::uuid")
            .bind(&sub)
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$") && !hash.contains("synthetic"));

    // The exchange takes it like an Identity Platform token; then the account and /me.
    let dev = device();
    let (s, ex) = st.launcher_sign_in(&id_token, &dev).await;
    assert_eq!(
        (s, ex["status"].as_str()),
        (StatusCode::OK, Some("needs_account")),
        "{ex}"
    );
    assert_eq!(ex["email"], email.to_lowercase());
    let (s, created) = st
        .post(
            lr::ACCOUNTS,
            Caller::Launcher(None),
            &fresh(),
            json!({"identityTicket": ex["identityTicket"], "device": dev,
                   "profile": {"game": "fh2", "name": format!("Lo{}", &fresh()[24..]), "mode": "noob"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let (s, me) = st
        .get(lr::ME, Caller::Launcher(created["accessToken"].as_str()))
        .await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["accountId"], created["accountId"]);

    // Sign-in with the address in any case, then the exchange signs straight in.
    let (s, signed) = st
        .sign_in_local(&email.to_uppercase(), "synthetic-pass-1")
        .await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    assert_eq!(st.local_sub(signed["idToken"].as_str().unwrap()), sub);
    let (s, ex) = st
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(
        (s, ex["status"].as_str(), &ex["accountId"]),
        (StatusCode::OK, Some("signed_in"), &created["accountId"]),
        "{ex}"
    );
    // A Firebase-shaped token is not this instance's.
    let (s, b) = st.launcher_sign_in("a.b.c", &device()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_sign_in_answers_one_refusal_and_validates_sign_up() {
    let st = local_stack(8).await;
    let email = local_email();
    let (s, _) = st.sign_up(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let wrong = st.sign_in_local(&email, "synthetic-pass-x").await;
    let unknown = st.sign_in_local(&local_email(), "synthetic-pass-1").await;
    let malformed = st.sign_in_local("no-at", "synthetic-pass-1").await;
    assert_eq!(
        (wrong.0, code(&wrong.1)),
        (StatusCode::UNAUTHORIZED, "AUTH.INVALID_CREDENTIALS")
    );
    assert_eq!(
        wrong, unknown,
        "an unknown address answers exactly as a wrong password"
    );
    assert_eq!(wrong, malformed);

    for (body, field) in [
        (
            credentials("no-at.example.test", "synthetic-pass-1"),
            "email",
        ),
        (credentials("a@b@c", "synthetic-pass-1"), "email"),
        (credentials(&local_email(), "short"), "password"),
        (credentials(&local_email(), &"x".repeat(129)), "password"),
    ] {
        let (s, b) = st
            .post(lr::AUTH_SIGN_UP, Caller::Launcher(None), &fresh(), body)
            .await;
        assert_eq!(
            (s, code(&b), b["details"]["field"].as_str()),
            (StatusCode::BAD_REQUEST, "REQUEST.INVALID", Some(field)),
            "{b}"
        );
    }
    let (s, b) = st
        .call(
            Method::POST,
            lr::AUTH_SIGN_UP,
            Caller::Launcher(None),
            None,
            Some(credentials(&local_email(), "synthetic-pass-1")),
        )
        .await;
    assert_eq!(
        (s, b["details"]["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("Idempotency-Key"))
    );

    // Registration closed: no new credential; existing ones still sign in.
    let mut identity = (*st.identity).clone();
    identity.info.registration_enabled = false;
    let identity = Arc::new(identity);
    let closed = Stack {
        app: app::router(
            ServiceInfo::from_env("fi-api", "0.0.0"),
            AppState {
                pool: st.pool.clone(),
                identity: Some(identity.clone()),
            },
            &Config::parse("launcher,portal", None).unwrap(),
        ),
        emulator: String::new(),
        portal_ip: st.portal_ip.clone(),
        launcher_ip: st.launcher_ip.clone(),
        owner: st.owner.clone(),
        pool: st.pool.clone(),
        identity,
    };
    let (s, b) = closed.sign_up(&local_email(), "synthetic-pass-1").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "ACCOUNTS.REGISTRATION_DISABLED")
    );
    let (s, _) = closed.sign_in_local(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK);

    // Ten attempts per address per 15 minutes, whether or not it has a credential.
    let target = local_email();
    for _ in 0..10 {
        let (s, _) = st.sign_in_local(&target, "synthetic-pass-1").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let (s, b) = st.sign_in_local(&target, "synthetic-pass-1").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_password_change_needs_a_recent_sign_in_and_revokes_every_family() {
    let st = local_stack(8).await;
    let (email, account_id, refresh, access, name) = st.local_player("synthetic-pass-1").await;
    let (_, signed) = st.sign_in_local(&email, "synthetic-pass-1").await;
    let fresh_token = signed["idToken"].as_str().unwrap().to_owned();
    let sub = st.local_sub(&fresh_token);
    let now = unix_now();
    let lower = email.to_lowercase();

    // Not recent: auth_time older than 5 minutes; a token that is no token; a short password.
    let stale = st.local_idp().issue(&sub, &lower, now - 400, now).token;
    let (s, b) = st.change_password(stale.expose(), "synthetic-pass-2").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.REAUTH_REQUIRED")
    );
    let (s, b) = st.change_password("not-a-token", "synthetic-pass-2").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
    let (s, b) = st.change_password(&fresh_token, "short").await;
    assert_eq!(
        (s, b["details"]["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("newPassword"))
    );

    // Deletion reauth takes a local token too (wrong name: nothing written).
    let (s, b) = st
        .post(
            lr::ME_DELETION,
            Caller::Launcher(Some(&access)),
            &fresh(),
            json!({"confirmGamertag": format!("{name}x"), "reauth": {"idToken": fresh_token}}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "ACCOUNTS.CONFIRMATION_MISMATCH"
        ),
        "{b}"
    );

    // A token minted before the change dies with it.
    let before = st.local_idp().issue(&sub, &lower, now - 2, now - 2).token;
    let (s, b) = st.change_password(&fresh_token, "synthetic-pass-2").await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = st
        .post(
            lr::TOKEN,
            Caller::Launcher(None),
            &fresh(),
            json!({"refreshToken": refresh}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.REFRESH_INVALID")
    );
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM refresh_token WHERE account_id = $1::uuid AND revoked_at IS NULL",
    )
    .bind(&account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(live, 0);
    let (s, b) = st.launcher_sign_in(before.expose(), &device()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
    let (s, _) = st.sign_in_local(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, signed) = st.sign_in_local(&email, "synthetic-pass-2").await;
    assert_eq!(s, StatusCode::OK);
    let (s, ex) = st
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(
        (s, &ex["accountId"]),
        (StatusCode::OK, &json!(account_id)),
        "{ex}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_routes_hold_one_pooled_connection_at_a_time() {
    let st = local_stack(1).await;
    let (email, ..) = st.local_player("synthetic-pass-1").await;
    let (s, signed) = st.sign_in_local(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = st
        .change_password(signed["idToken"].as_str().unwrap(), "synthetic-pass-2")
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn a_firebase_instance_serves_no_local_route_and_refuses_local_tokens() {
    let local = local_stack(4).await;
    let (_, up) = local.sign_up(&local_email(), "synthetic-pass-1").await;
    let (url, pool) = runtime_pool(4).await;
    let firebase = stack_of(
        url,
        pool,
        String::new(),
        &config::tests::local(),
        "launcher,portal",
        None,
    )
    .await;
    for path in [
        lr::AUTH_SIGN_UP,
        lr::AUTH_SIGN_IN,
        lr::AUTH_PASSWORD,
        lr::AUTH_CONFIRMATION,
        lr::AUTH_PASSWORD_RESET,
        lr::AUTH_EMAIL_CONFIRM,
        lr::AUTH_EMAIL_PASSWORD,
    ] {
        let (s, _) = firebase
            .post(path, Caller::Launcher(None), &fresh(), json!({}))
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
    }
    let (_, info) = firebase.get(lr::INFO, Caller::Launcher(None)).await;
    assert!(info["idp"].get("kind").is_none(), "{info}");
    let (s, b) = firebase
        .launcher_sign_in(up["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
}

// ---------------------------------------------------------------------------------------------
// CR-S53: a `local` instance's e-mail confirmation, password reset and Firebase import, over a
// recording mailer. Postgres only. Every log line of the test binary is captured: no address, link
// or token may appear in one.
// ---------------------------------------------------------------------------------------------

/// Every log line of this test binary from the first mail test on.
fn captured_logs() -> &'static Arc<std::sync::Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        #[derive(Clone)]
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Capture;
            fn make_writer(&'a self) -> Capture {
                self.clone()
            }
        }
        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(logs.clone()))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            // No timestamps: their microseconds would collide with 6-digit codes.
            .without_time()
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        logs
    })
}

fn assert_logs_hold_none_of(secrets: &[&str]) {
    let logs = String::from_utf8_lossy(&captured_logs().lock().unwrap()).to_lowercase();
    assert!(!logs.contains("fi_mt_"), "a mail token reached a log line");
    for s in secrets {
        assert!(
            !logs.contains(&s.to_lowercase()),
            "{s:?} reached a log line"
        );
    }
}

/// No 6-digit `codes` in a log line of our crates (targets `fi_…`; other crates' lines carry
/// timings whose digits could collide), nor their spaced form anywhere.
fn assert_logs_hold_no_code(codes: &[String]) {
    let logs = String::from_utf8_lossy(&captured_logs().lock().unwrap()).into_owned();
    for c in codes {
        let spaced = format!("{} {}", &c[..3], &c[3..]);
        assert!(!logs.contains(&spaced), "a code reached a log line");
        for line in logs.lines().filter(|l| l.contains(" fi_")) {
            assert!(
                !line.contains(c.as_str()),
                "a code reached a log line: {line}"
            );
        }
    }
}

/// A mailer that always fails (its warning must hold no address either).
struct FailingMailer;

impl fi_mail::Mailer for FailingMailer {
    fn send(&self, _message: fi_mail::Message) -> fi_mail::Sending<'_> {
        Box::pin(async { Err(fi_mail::MailError("connection or protocol failure".into())) })
    }
    fn delivers(&self) -> bool {
        true
    }
    fn backend(&self) -> &'static str {
        "failing"
    }
}

fn form_encode(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| {
            let enc: String = v
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                        char::from(b).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("{k}={enc}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The 6-digit code of a confirmation mail ("…: 123 456").
fn mailed_code(m: &fi_mail::Message) -> String {
    let at = m.text.find(": ").expect("a code") + 2;
    let code: String = m.text[at..at + 7]
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    assert_eq!(code.len(), 6, "{code}");
    code
}

/// The token of a mailed link (`?token=fi_mt_…`).
fn mailed_token(m: &fi_mail::Message) -> String {
    let at = m.text.find("token=").expect("a link with a token") + "token=".len();
    m.text[at..]
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap()
        .to_owned()
}

impl Stack {
    /// This stack with another identity configuration (same pool, networks and database).
    fn with_identity(&self, identity: IdentityConfig) -> Stack {
        let identity = Arc::new(identity);
        Stack {
            app: app::router(
                ServiceInfo::from_env("fi-api", "0.0.0"),
                AppState {
                    pool: self.pool.clone(),
                    identity: Some(identity.clone()),
                },
                &Config::parse("launcher,portal", None).unwrap(),
            ),
            emulator: String::new(),
            portal_ip: self.portal_ip.clone(),
            launcher_ip: self.launcher_ip.clone(),
            owner: self.owner.clone(),
            pool: self.pool.clone(),
            identity,
        }
    }

    /// This stack seen from another client network (fresh per-network limits).
    fn elsewhere(&self) -> Stack {
        let mut other = self.with_identity((*self.identity).clone());
        other.launcher_ip = random_ip();
        other
    }

    fn with_mailer(&self, mailer: Arc<dyn fi_mail::Mailer>) -> Stack {
        let mut identity = (*self.identity).clone();
        identity.mail = Some(crate::mail::MailConfig::new(
            mailer,
            "https://fi.example.test",
        ));
        self.with_identity(identity)
    }

    /// A plain request answered with HTML: `(status, body)`.
    async fn html(&self, method: Method, uri: &str, form: Option<String>) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-forwarded-for", &self.launcher_ip);
        let body = match form {
            Some(f) => {
                req = req.header("content-type", "application/x-www-form-urlencoded");
                Body::from(f)
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        if status.is_success() || status.is_client_error() {
            assert_eq!(
                response.headers()["cache-control"],
                "no-store",
                "{uri} pages are never cached"
            );
        }
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn confirm_link(&self, token: &str, password: &str) -> (StatusCode, String) {
        self.html(
            Method::POST,
            lr::AUTH_EMAIL_CONFIRM,
            Some(form_encode(&[("token", token), ("password", password)])),
        )
        .await
    }

    async fn set_password_link(
        &self,
        token: &str,
        password: &str,
        repeat: &str,
    ) -> (StatusCode, String) {
        self.html(
            Method::POST,
            lr::AUTH_EMAIL_PASSWORD,
            Some(form_encode(&[
                ("token", token),
                ("password", password),
                ("repeat", repeat),
            ])),
        )
        .await
    }

    async fn resend(&self, id_token: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            lr::AUTH_CONFIRMATION,
            Caller::Launcher(None),
            None,
            Some(json!({"idToken": id_token})),
        )
        .await
    }

    async fn confirm_code(&self, id_token: &str, code: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            lr::AUTH_CONFIRMATION_CODE,
            Caller::Launcher(None),
            None,
            Some(json!({"idToken": id_token, "code": code})),
        )
        .await
    }

    async fn ask_reset(&self, email: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            lr::AUTH_PASSWORD_RESET,
            Caller::Launcher(None),
            None,
            Some(json!({"email": email})),
        )
        .await
    }
}

/// The messages sent to `to`, waiting up to 5 s for the `n`th (mail leaves from a task).
async fn wait_mail(rec: &fi_mail::Recording, to: &str, n: usize) -> Vec<fi_mail::Message> {
    for _ in 0..100 {
        let sent = rec.sent_to(to);
        if sent.len() >= n {
            return sent;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("mail {n} never arrived");
}

/// A recording-mail `local` stack.
async fn mail_stack() -> (Stack, fi_mail::Recording) {
    captured_logs();
    let rec = fi_mail::Recording::new();
    let st = local_stack(8).await.with_mailer(Arc::new(rec.clone()));
    (st, rec)
}

/// Sign-up, confirmation by mail, sign-in, exchange, account: `(email, accountId, refreshToken,
/// accessToken, name)`.
async fn confirmed_player(
    st: &Stack,
    rec: &fi_mail::Recording,
    password: &str,
) -> (String, String, String, String, String) {
    let email = local_email();
    let (s, up) = st.sign_up(&email, password).await;
    assert_eq!(s, StatusCode::CREATED, "{up}");
    let mail = wait_mail(rec, &email, 1).await;
    let (s, _) = st.confirm_link(&mailed_token(&mail[0]), password).await;
    assert_eq!(s, StatusCode::OK);
    let (s, signed) = st.sign_in_local(&email, password).await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    let dev = device();
    let (s, ex) = st
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &dev)
        .await;
    assert_eq!(ex["status"], "needs_account", "{s} {ex}");
    let name = format!("Lm{}", &fresh()[24..]);
    let (s, created) = st
        .post(
            lr::ACCOUNTS,
            Caller::Launcher(None),
            &fresh(),
            json!({"identityTicket": ex["identityTicket"], "device": dev,
                   "profile": {"game": "fh2", "name": name, "mode": "noob"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    (
        email,
        created["accountId"].as_str().unwrap().to_owned(),
        created["refreshToken"].as_str().unwrap().to_owned(),
        created["accessToken"].as_str().unwrap().to_owned(),
        name,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_mail_sign_up_waits_for_the_confirmation_link() {
    let (st, rec) = mail_stack().await;
    let email = local_email();
    let lower = email.to_lowercase();
    let (s, up) = st.sign_up(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED, "{up}");
    let id_token = up["idToken"].as_str().unwrap().to_owned();
    let identity = st
        .local_idp()
        .verify_unconfirmed(&id_token, unix_now())
        .unwrap();
    assert!(!identity.email_verified);
    // Unconfirmed: the exchange refuses as it does an unverified Identity Platform user.
    let (s, b) = st.launcher_sign_in(&id_token, &device()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "AUTH.EMAIL_UNVERIFIED")
    );
    // Sign-in works and still says unconfirmed.
    let (s, signed) = st.sign_in_local(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = st
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "AUTH.EMAIL_UNVERIFIED")
    );

    // One confirmation mail, its link under the configured origin, bilingual without a header.
    let mail = wait_mail(&rec, &lower, 1).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].kind, fi_mail::Kind::Confirm);
    assert!(
        mail[0]
            .text
            .contains("https://fi.example.test/launcher/v1/auth/email/confirm?token=fi_mt_")
    );
    assert!(mail[0].text.contains("Seu código") && mail[0].text.contains("confirmation code"));
    let first = mailed_token(&mail[0]);
    // Only its SHA-256 is stored.
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM credential_mail_token WHERE token_sha256 = $1")
            .bind(fi_auth::sha256(first.as_bytes()).as_slice())
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert_eq!(stored, 1);

    // The resend: same ID token, limited per credential (3 per hour); pt-BR by Accept-Language.
    for n in 2..=4 {
        let (s, b) = st.resend(&id_token).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        wait_mail(&rec, &lower, n).await;
    }
    let (s, b) = st.resend(&id_token).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED")
    );
    let (s, b) = st.resend("not-a-token").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
    // A new mail replaces the open link and code: the first link is spent, the newest works.
    let (s, _) = st.confirm_link(&first, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "replaced by a newer mail");
    let first = mailed_token(wait_mail(&rec, &lower, 4).await.last().unwrap());
    let req = Request::builder()
        .method(Method::POST)
        .uri(lr::AUTH_PASSWORD_RESET)
        .header("x-forwarded-for", random_ip())
        .header("accept-language", "pt-BR,pt;q=0.9")
        .header("content-type", "application/json")
        .body(Body::from(json!({"email": email}).to_string()))
        .unwrap();
    assert_eq!(
        st.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    let mail = wait_mail(&rec, &lower, 5).await;
    let reset = mail
        .iter()
        .find(|m| m.kind == fi_mail::Kind::Reset)
        .unwrap();
    assert!(reset.text.contains("Abra este link") && !reset.text.contains("Open this link"));

    // The page: GET renders a form and spends nothing (mail scanners), POST spends once.
    for _ in 0..2 {
        let (s, html) = st
            .html(
                Method::GET,
                &format!("{}?token={first}", lr::AUTH_EMAIL_CONFIRM),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        assert!(html.contains("<form method=\"post\"") && html.contains(&first));
    }
    let (s, html) = st
        .html(
            Method::GET,
            &format!("{}?token=junk", lr::AUTH_EMAIL_CONFIRM),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{html}");
    // The account's password is required; a wrong one keeps the token.
    let (s, html) = st.confirm_link(&first, "synthetic-pass-x").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(html.contains("A senha está incorreta"));
    let (s, html) = st.confirm_link(&first, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK, "{html}");
    assert!(html.contains("E-mail confirmado"));
    let (s, _) = st.confirm_link(&first, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "single use");
    let (s, _) = st.confirm_link("fi_mt_x", "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Confirmed: a new sign-in exchanges; a resend sends nothing more.
    let (_, signed) = st.sign_in_local(&email, "synthetic-pass-1").await;
    let (s, ex) = st
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(
        (s, ex["status"].as_str()),
        (StatusCode::OK, Some("needs_account")),
        "{ex}"
    );

    // An expired confirmation link is refused (another network: the page posts count per network).
    let st = st.elsewhere();
    let email2 = local_email();
    let (s, _) = st.sign_up(&email2, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let late = mailed_token(&wait_mail(&rec, &email2, 1).await[0]);
    sqlx::query(
        "UPDATE credential_mail_token SET created_at = clock_timestamp() - interval '25 hours', \
         expires_at = clock_timestamp() - interval '2 hours' WHERE token_sha256 = $1",
    )
    .bind(fi_auth::sha256(late.as_bytes()).as_slice())
    .execute(&st.owner)
    .await
    .unwrap();
    let (s, _) = st.confirm_link(&late, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "expired");

    // Registration closed: no sign-up, but an existing address still gets its reset.
    let mut closed = (*st.identity).clone();
    closed.info.registration_enabled = false;
    let closed = st.with_identity(closed).elsewhere();
    let (s, b) = closed.sign_up(&local_email(), "synthetic-pass-1").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "ACCOUNTS.REGISTRATION_DISABLED")
    );
    let (s, _) = closed.ask_reset(&email2).await;
    assert_eq!(s, StatusCode::ACCEPTED);

    assert_logs_hold_none_of(&[&lower, &email2, &first, &late]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_mail_password_reset_is_silent_single_use_and_signs_out_everywhere() {
    let (st, rec) = mail_stack().await;
    let (email, account_id, refresh, access, name) =
        confirmed_player(&st, &rec, "synthetic-pass-1").await;
    let lower = email.to_lowercase();
    let (_, before) = st.sign_in_local(&email, "synthetic-pass-1").await;
    let before = before["idToken"].as_str().unwrap().to_owned();
    // No sleep: password changes compare at millisecond precision (security review M2), so a
    // token of the same second as the reset dies too.

    // Unknown and known addresses answer alike; only the known one gets mail.
    let unknown = local_email();
    let a = st.ask_reset(&unknown).await;
    let b = st.ask_reset(&email).await;
    assert_eq!(a, (StatusCode::ACCEPTED, Value::Null));
    assert_eq!(a, b);
    let mail = wait_mail(&rec, &lower, 2).await;
    let reset = mail
        .iter()
        .find(|m| m.kind == fi_mail::Kind::Reset)
        .unwrap();
    assert!(
        reset
            .text
            .contains("/launcher/v1/auth/email/password?token=fi_mt_")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rec.sent_to(&unknown).is_empty());
    let (s, b) = st.ask_reset("no-at").await;
    assert_eq!(
        (s, b["details"]["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("email"))
    );
    let token = mailed_token(reset);
    // A new password link spends the older one (security review L1).
    let (s, _) = st.ask_reset(&email).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let newer = wait_mail(&rec, &lower, 3).await;
    let newer = mailed_token(
        newer
            .iter()
            .rev()
            .find(|m| m.kind == fi_mail::Kind::Reset)
            .unwrap(),
    );
    let (s, _) = st
        .set_password_link(&token, "synthetic-pass-2", "synthetic-pass-2")
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the older link is spent");
    let token = newer;

    // The page: GET spends nothing; a short or mismatched password keeps the token.
    let (s, html) = st
        .html(
            Method::GET,
            &format!("{}?token={token}", lr::AUTH_EMAIL_PASSWORD),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("type=\"password\""));
    let (s, html) = st
        .set_password_link(&token, "synthetic-pass-2", "synthetic-pass-3")
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(html.contains("As duas senhas"));
    let (s, _) = st.set_password_link(&token, "short", "short").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, html) = st
        .set_password_link(&token, "synthetic-pass-2", "synthetic-pass-2")
        .await;
    assert_eq!(s, StatusCode::OK, "{html}");
    let (s, _) = st
        .set_password_link(&token, "synthetic-pass-9", "synthetic-pass-9")
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "single use");

    // Signed out everywhere: refresh refused, every family revoked as password_reset, and the
    // ID token minted before the reset dies.
    let (s, b) = st
        .post(
            lr::TOKEN,
            Caller::Launcher(None),
            &fresh(),
            json!({"refreshToken": refresh}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.REFRESH_INVALID")
    );
    let reasons: Vec<Option<String>> =
        sqlx::query_scalar("SELECT revoke_reason FROM refresh_token WHERE account_id = $1::uuid")
            .bind(&account_id)
            .fetch_all(&st.owner)
            .await
            .unwrap();
    assert!(!reasons.is_empty());
    assert!(
        reasons
            .iter()
            .all(|r| r.as_deref() == Some("password_reset")),
        "{reasons:?}"
    );
    let (s, _) = st.get(lr::ME, Caller::Launcher(Some(&access))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, b) = st.launcher_sign_in(&before, &device()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
    let (s, _) = st.sign_in_local(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, signed) = st.sign_in_local(&email, "synthetic-pass-2").await;
    assert_eq!(s, StatusCode::OK);
    let fresh_token = signed["idToken"].as_str().unwrap().to_owned();
    let (s, ex) = st.launcher_sign_in(&fresh_token, &device()).await;
    assert_eq!(
        (s, &ex["accountId"]),
        (StatusCode::OK, &json!(account_id)),
        "{ex}"
    );
    let access = ex["accessToken"].as_str().unwrap().to_owned();

    // Deletion reauth with a local ID token: a token signed before the reset is dead; a fresh
    // sign-in is accepted. (The 5-minute auth_time rule: another player, below.)
    let sub = st.local_sub(&fresh_token);
    let now = unix_now();
    let old = st.local_idp().issue(&sub, &lower, now - 400, now).token;
    let (s, b) = st
        .post(
            lr::ME_DELETION,
            Caller::Launcher(Some(&access)),
            &fresh(),
            json!({"confirmGamertag": name, "reauth": {"idToken": old.expose()}}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID"),
        "{b}"
    );
    let (s, b) = st
        .post(
            lr::ME_DELETION,
            Caller::Launcher(Some(&access)),
            &fresh(),
            json!({"confirmGamertag": name, "reauth": {"idToken": fresh_token}}),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert!(b["purgeAt"].is_string());
    let st = st.elsewhere();
    let (email2, _, _, access2, name2) = confirmed_player(&st, &rec, "synthetic-pass-1").await;
    let (_, signed2) = st.sign_in_local(&email2, "synthetic-pass-1").await;
    let sub2 = st.local_sub(signed2["idToken"].as_str().unwrap());
    let now = unix_now();
    let stale = st
        .local_idp()
        .issue(&sub2, &email2.to_lowercase(), now - 400, now)
        .token;
    let (s, b) = st
        .post(
            lr::ME_DELETION,
            Caller::Launcher(Some(&access2)),
            &fresh(),
            json!({"confirmGamertag": name2, "reauth": {"idToken": stale.expose()}}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.REAUTH_REQUIRED"),
        "{b}"
    );

    // Three password mails per address per hour, known or not.
    let st = st.elsewhere();
    let target = local_email();
    for _ in 0..3 {
        assert_eq!(st.ask_reset(&target).await.0, StatusCode::ACCEPTED);
    }
    let (s, b) = st.ask_reset(&target).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED")
    );

    // An expired password link is refused before any password work.
    let st = st.elsewhere();
    let other = local_email();
    let (s, _) = st.sign_up(&other, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let st2 = st.elsewhere();
    let (s, _) = st2.ask_reset(&other).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let mail = wait_mail(&rec, &other, 2).await;
    let link = mailed_token(
        mail.iter()
            .find(|m| m.kind == fi_mail::Kind::Reset)
            .unwrap(),
    );
    sqlx::query(
        "UPDATE credential_mail_token SET created_at = clock_timestamp() - interval '2 hours', \
         expires_at = clock_timestamp() - interval '1 hour' WHERE token_sha256 = $1",
    )
    .bind(fi_auth::sha256(link.as_bytes()).as_slice())
    .execute(&st.owner)
    .await
    .unwrap();
    let (s, _) = st
        .set_password_link(&link, "synthetic-pass-2", "synthetic-pass-2")
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "expired");

    // A reset also confirms an unconfirmed address (and spends its confirmation link).
    let (s, _) = st2.ask_reset(&other).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let mail = wait_mail(&rec, &other, 3).await;
    let link = mailed_token(
        mail.iter()
            .rev()
            .find(|m| m.kind == fi_mail::Kind::Reset)
            .unwrap(),
    );
    let confirm = mailed_token(
        mail.iter()
            .find(|m| m.kind == fi_mail::Kind::Confirm)
            .unwrap(),
    );
    let (s, _) = st
        .set_password_link(&link, "synthetic-pass-4", "synthetic-pass-4")
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = st.confirm_link(&confirm, "synthetic-pass-4").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a reset spends the other links");
    let (_, signed) = st.sign_in_local(&other, "synthetic-pass-4").await;
    let (s, ex) = st
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(ex["status"], "needs_account", "{s} {ex}");

    // A failing mailer: the request still answers 202, the warning holds no address.
    let failing = st.with_mailer(Arc::new(FailingMailer));
    assert_eq!(failing.ask_reset(&email).await.0, StatusCode::ACCEPTED);
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_logs_hold_none_of(&[&lower, &unknown, &other, &token, &link, &before]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_mail_imported_firebase_users_sign_in_and_set_passwords() {
    let (st, rec) = mail_stack().await;
    // Firebase's published sample project and user (fi_auth::firebase_scrypt's known answer).
    let mut identity = (*st.identity).clone();
    identity.firebase_hash = Some(
        fi_auth::firebase_scrypt::HashConfig::new(
            "jxspr8Ki0RYycVU8zykbdLGjFQ3McFUH0uiiTvC8pVMXAn210wjLNmdZJzxUECKbm0QsEmYUSDzZvpjeJ9WmXA==",
            "Bw==",
            8,
            14,
        )
        .unwrap(),
    );
    let st = st.with_identity(identity);
    let email = local_email();
    let lower = email.to_lowercase();
    let google = local_email();
    let mut owner = st.owner.acquire().await.unwrap();
    for (address, hash) in [
        (
            lower.clone(),
            fi_auth::firebase_scrypt::stored(
                "42xEC+ixf3L2lw==",
                "lSrfV15cpx95/sZS2W9c9Kp6i/LVgQNDNC/qzrCnh1SAyZvqmZqAjTdn3aoItz+VHjoZilo78198JAdRuid5lQ==",
            ),
        ),
        (google.to_lowercase(), None),
    ] {
        fi_db::credential::import_in(
            &mut owner,
            &fi_db::credential::ImportedUser {
                firebase_uid: format!("fb{}", fresh()),
                email_lower: address,
                password_hash: hash,
                email_verified: true,
            },
        )
        .await
        .unwrap();
    }
    drop(owner);

    // The old password signs in and the hash becomes Argon2id; a wrong one does not.
    let (s, _) = st.sign_in_local(&email, "user2password").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, signed) = st.sign_in_local(&email, "user1password").await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    let hash: String =
        sqlx::query_scalar("SELECT password_hash FROM credential WHERE email_lower = $1")
            .bind(&lower)
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert!(hash.starts_with("$argon2id$"), "rehashed");
    let (s, ex) = st
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(ex["status"], "needs_account", "{s} {ex}");
    let (s, _) = st.sign_in_local(&email, "user1password").await;
    assert_eq!(s, StatusCode::OK, "the Argon2id hash verifies");

    // A Google-only user has no password: refused like a wrong one, then set by mail.
    let (s, b) = st.sign_in_local(&google, "synthetic-pass-1").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.INVALID_CREDENTIALS")
    );
    assert_eq!(st.ask_reset(&google).await.0, StatusCode::ACCEPTED);
    let mail = wait_mail(&rec, &google, 1).await;
    let (s, _) = st
        .set_password_link(
            &mailed_token(&mail[0]),
            "synthetic-pass-1",
            "synthetic-pass-1",
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = st.sign_in_local(&google, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK);

    // Without the project's parameters an imported hash never matches.
    let mut none = (*st.identity).clone();
    none.firebase_hash = None;
    let bare = st.with_identity(none);
    let other = local_email();
    let mut owner = st.owner.acquire().await.unwrap();
    fi_db::credential::import_in(
        &mut owner,
        &fi_db::credential::ImportedUser {
            firebase_uid: format!("fb{}", fresh()),
            email_lower: other.to_lowercase(),
            password_hash: fi_auth::firebase_scrypt::stored(
                "42xEC+ixf3L2lw==",
                "lSrfV15cpx95/sZS2W9c9Kp6i/LVgQNDNC/qzrCnh1SAyZvqmZqAjTdn3aoItz+VHjoZilo78198JAdRuid5lQ==",
            ),
            email_verified: true,
        },
    )
    .await
    .unwrap();
    drop(owner);
    let (s, _) = bare.sign_in_local(&other, "user1password").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    assert_logs_hold_none_of(&[&lower, &google, &other]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_mail_log_backend_needs_no_confirmation_and_logs_no_address() {
    captured_logs();
    let st = local_stack(4).await;
    assert_eq!(
        st.identity.mail.as_ref().unwrap().mailer.backend(),
        "log",
        "the default"
    );
    let email = local_email();
    let (s, up) = st.sign_up(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, ex) = st
        .launcher_sign_in(up["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(ex["status"], "needs_account", "{s} {ex}");
    assert_eq!(st.ask_reset(&email).await.0, StatusCode::ACCEPTED);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let logs = String::from_utf8_lossy(&captured_logs().lock().unwrap()).into_owned();
    assert!(
        logs.contains(&fi_mail::address_hash(&email)),
        "the log line names the hash"
    );
    assert_logs_hold_none_of(&[&email]);
    // The pages exist only on a local instance and answer HTML.
    let (s, html) = st
        .html(
            Method::GET,
            &format!("{}?token=", lr::AUTH_EMAIL_PASSWORD),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(html.starts_with("<!doctype html>"));
}

/// Security review H1, M1, M2 and L5: log-mode rows stay unconfirmed and must confirm once mail is
/// on; an unconfirmed, account-less credential is taken over by a new sign-up while a credential
/// with an account is not; a password change kills a token of the same second; a 6-character
/// imported password signs in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_mail_review_fixes_hold() {
    captured_logs();
    let log = local_stack(4).await;
    let rec = fi_mail::Recording::new();
    let smtp = log.with_mailer(Arc::new(rec.clone()));

    // H1: a log-mode sign-up works at once but is not stored confirmed.
    let (email, account_id, ..) = log.local_player("synthetic-pass-1").await;
    let bare = local_email();
    let (s, _) = log.sign_up(&bare, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let confirmed: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT email_verified_at::text FROM credential WHERE email_lower = ANY($1)",
    )
    .bind(vec![email.to_lowercase(), bare.to_lowercase()])
    .fetch_all(&log.owner)
    .await
    .unwrap();
    assert_eq!(confirmed, vec![None, None]);
    // Mail on: the same player must confirm (resend works with its token).
    let (_, signed) = smtp.sign_in_local(&email, "synthetic-pass-1").await;
    let id_token = signed["idToken"].as_str().unwrap().to_owned();
    let (s, b) = smtp.launcher_sign_in(&id_token, &device()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "AUTH.EMAIL_UNVERIFIED")
    );
    assert_eq!(smtp.resend(&id_token).await.0, StatusCode::ACCEPTED);

    // M1: a credential with an FI account is never taken over...
    let (s, b) = smtp.sign_up(&email, "synthetic-pass-9").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "ACCOUNTS.EMAIL_TAKEN")
    );
    // ...an unconfirmed one without an account is: the squatter's password, token and link die.
    let victim = local_email();
    let (s, squat) = smtp.sign_up(&victim, "squatter-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let squat_link = mailed_token(&wait_mail(&rec, &victim, 1).await[0]);
    let (s, mine) = smtp.sign_up(&victim, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED, "{mine}");
    assert_eq!(
        smtp.local_sub(squat["idToken"].as_str().unwrap()),
        smtp.local_sub(mine["idToken"].as_str().unwrap()),
        "the same credential"
    );
    let (s, _) = smtp.sign_in_local(&victim, "squatter-pass-1").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = smtp.confirm_link(&squat_link, "squatter-pass-1").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the squatter's link is spent");
    let mine_link = mailed_token(&wait_mail(&rec, &victim, 2).await[1]);
    let (s, _) = smtp.confirm_link(&mine_link, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK);

    // M1 residual: the owner signs up, an attacker takes the unconfirmed address over, and the
    // owner clicks the newest link with the owner's own password: refused, nothing confirmed, the
    // link kept; only the holder of the password now set confirms.
    let owner = local_email();
    let (s, _) = smtp.sign_up(&owner, "owner-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = smtp.sign_up(&owner, "attacker-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let newest = mailed_token(wait_mail(&rec, &owner, 2).await.last().unwrap());
    let (s, html) = smtp.confirm_link(&newest, "owner-pass-1").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(html.contains("The password is incorrect"));
    let still: Option<String> =
        sqlx::query_scalar("SELECT email_verified_at::text FROM credential WHERE email_lower = $1")
            .bind(owner.to_lowercase())
            .fetch_one(&smtp.owner)
            .await
            .unwrap();
    assert_eq!(still, None, "not confirmed");
    let (s, _) = smtp.confirm_link(&newest, "attacker-pass-1").await;
    assert_eq!(
        s,
        StatusCode::OK,
        "the token was kept for the current password"
    );
    let (s, b) = smtp.sign_up(&victim, "squatter-pass-2").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "ACCOUNTS.EMAIL_TAKEN"),
        "confirmed: no more takeovers"
    );

    // M2: a change half a second into the token's auth_time second kills it; the next sign-in
    // waits into the following second and exchanges.
    let (_, signed) = log.sign_in_local(&email, "synthetic-pass-1").await;
    let token = signed["idToken"].as_str().unwrap().to_owned();
    let auth = log
        .local_idp()
        .verify(&token, unix_now())
        .unwrap()
        .auth_time;
    sqlx::query(
        "UPDATE credential SET updated_at = greatest(created_at, to_timestamp($2::float8 + 0.5)) \
         WHERE email_lower = $1",
    )
    .bind(email.to_lowercase())
    .bind(auth as f64)
    .execute(&log.owner)
    .await
    .unwrap();
    let (s, b) = log.launcher_sign_in(&token, &device()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
    let (s, signed) = log.sign_in_local(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK);
    let (s, ex) = log
        .launcher_sign_in(signed["idToken"].as_str().unwrap(), &device())
        .await;
    assert_eq!(
        (s, &ex["accountId"]),
        (StatusCode::OK, &json!(account_id)),
        "{ex}"
    );

    // L5: an imported 6-character Firebase password signs in (and is rehashed).
    let config = fi_auth::firebase_scrypt::HashConfig::new(
        "jxspr8Ki0RYycVU8zykbdLGjFQ3McFUH0uiiTvC8pVMXAn210wjLNmdZJzxUECKbm0QsEmYUSDzZvpjeJ9WmXA==",
        "Bw==",
        8,
        14,
    )
    .unwrap();
    let salt = b"synthetic-salt";
    let hash = config.hash(b"abc123", salt).unwrap();
    let mut identity = (*log.identity).clone();
    identity.firebase_hash = Some(config);
    let fb = log.with_identity(identity);
    let short = local_email();
    let mut owner = log.owner.acquire().await.unwrap();
    fi_db::credential::import_in(
        &mut owner,
        &fi_db::credential::ImportedUser {
            firebase_uid: format!("fb{}", fresh()),
            email_lower: short.to_lowercase(),
            password_hash: fi_auth::firebase_scrypt::stored(
                &fi_auth::b64::std_encode(salt),
                &fi_auth::b64::std_encode(&hash),
            ),
            email_verified: true,
        },
    )
    .await
    .unwrap();
    drop(owner);
    let (s, b) = fb.sign_in_local(&short, "abc123").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _) = fb.sign_in_local(&short, "abc124").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    assert_logs_hold_none_of(&[&email, &bare, &victim, &short, &squat_link, &mine_link]);
}

/// The 6-digit code (owner-decisions "e-mail confirmation by a 6-digit code in the launcher"):
/// a current sign-in token plus the newest code answers a confirmed token that exchanges; a token
/// from before a takeover is refused; five wrong codes spend it; an older mail's code is invalid;
/// limits; codes never reach a log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_mail_confirmation_code() {
    let (st, rec) = mail_stack().await;
    let email = local_email();
    let lower = email.to_lowercase();
    let (s, up) = st.sign_up(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let old_mail = wait_mail(&rec, &lower, 1).await[0].clone();
    assert!(
        old_mail.text.contains("código de confirmação")
            && old_mail.text.contains("confirmation code")
    );
    let old_code = mailed_code(&old_mail);
    // Only a keyed MAC of the code is stored, never the digits.
    let plain: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM credential_mail_token WHERE code_mac = $1 OR code_mac = $2",
    )
    .bind(fi_auth::sha256(old_code.as_bytes()).as_slice())
    .bind(old_code.as_bytes())
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(plain, 0);
    // A resend replaces the code.
    assert_eq!(
        st.resend(up["idToken"].as_str().unwrap()).await.0,
        StatusCode::ACCEPTED
    );
    let mail = wait_mail(&rec, &lower, 2).await;
    let code_now = mailed_code(&mail[1]);
    let (_, signed) = st.sign_in_local(&email, "synthetic-pass-1").await;
    let token = signed["idToken"].as_str().unwrap().to_owned();
    if old_code != code_now {
        let (s, b) = st.confirm_code(&token, &old_code).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::BAD_REQUEST, "AUTH.CONFIRMATION_CODE_INVALID"),
            "an older mail's code"
        );
    }
    let (s, b) = st.confirm_code(&token, "12a456").await;
    assert_eq!(
        (s, b["details"]["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("code"))
    );
    let (s, b) = st.confirm_code("not-a-token", &code_now).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID")
    );
    // Right code with spaces + current token: a confirmed token, straight to the exchange.
    let spaced = format!("{} {}", &code_now[..3], &code_now[3..]);
    let (s, done) = st.confirm_code(&token, &spaced).await;
    assert_eq!(s, StatusCode::OK, "{done}");
    let confirmed = done["idToken"].as_str().unwrap();
    let (s, ex) = st.launcher_sign_in(confirmed, &device()).await;
    assert_eq!(
        (s, ex["status"].as_str()),
        (StatusCode::OK, Some("needs_account")),
        "{ex}"
    );
    // Spent with its link.
    let (s, _) = st
        .confirm_link(&mailed_token(&mail[1]), "synthetic-pass-1")
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Confirmed: no token for any 6 digits any more (L-C1).
    let (s, b) = st.confirm_code(&token, "000000").await;
    assert_eq!(
        (s, b["details"]["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("code")),
        "{b}"
    );

    // Five wrong codes spend it: the right one then fails too.
    let st2 = st.elsewhere();
    let other = local_email();
    let (s, _) = st2.sign_up(&other, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let right = mailed_code(&wait_mail(&rec, &other, 1).await[0]);
    let wrong = if right == "000000" {
        "000001"
    } else {
        "000000"
    };
    let (_, signed) = st2.sign_in_local(&other, "synthetic-pass-1").await;
    let token = signed["idToken"].as_str().unwrap().to_owned();
    for _ in 0..5 {
        let (s, b) = st2.confirm_code(&token, wrong).await;
        assert_eq!(code(&b), "AUTH.CONFIRMATION_CODE_INVALID", "{s}");
    }
    let (s, b) = st2.confirm_code(&token, &right).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, "AUTH.CONFIRMATION_CODE_INVALID"),
        "spent after five wrong codes"
    );
    // Ten submissions per credential per 15 minutes (6 so far): the 11th is limited.
    for _ in 0..4 {
        st2.confirm_code(&token, wrong).await;
    }
    let (s, b) = st2.confirm_code(&token, wrong).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED")
    );

    // A token from before a takeover is refused even with the right (new) code.
    let st3 = st.elsewhere();
    let victim = local_email();
    let (s, _) = st3.sign_up(&victim, "owner-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, before) = st3.sign_in_local(&victim, "owner-pass-1").await;
    let before = before["idToken"].as_str().unwrap().to_owned();
    let (s, _) = st3.sign_up(&victim, "attacker-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let newest = mailed_code(wait_mail(&rec, &victim, 2).await.last().unwrap());
    let (s, b) = st3.confirm_code(&before, &newest).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID"),
        "{b}"
    );
    let unconfirmed: Option<String> =
        sqlx::query_scalar("SELECT email_verified_at::text FROM credential WHERE email_lower = $1")
            .bind(victim.to_lowercase())
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert_eq!(unconfirmed, None);

    // The per-network limit: 30 per 15 minutes.
    let st4 = st.elsewhere();
    for _ in 0..30 {
        st4.confirm_code("not-a-token", "000000").await;
    }
    let (s, b) = st4.confirm_code("not-a-token", "000000").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED")
    );

    let codes: Vec<String> = rec
        .sent()
        .iter()
        .filter(|m| m.kind == fi_mail::Kind::Confirm)
        .map(mailed_code)
        .collect();
    assert_logs_hold_no_code(&codes);
    assert_logs_hold_none_of(&[&lower, &other, &victim]);
}

/// Security re-review of the code: at most 10 wrong codes per credential per 24 hours across new
/// codes (M-C1), 30 over its life until the link or a reset confirms it, and one open code per
/// credential even when a resend and a takeover race (L-C2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Postgres (see the file header)"]
async fn local_mail_confirmation_code_totals() {
    let (st, rec) = mail_stack().await;
    // Raise the per-15-minute limits so only the totals answer below.
    let mut identity = (*st.identity).clone();
    identity.local_limit_scale = 10;
    let st = st.with_identity(identity);
    let email = local_email();
    let lower = email.to_lowercase();
    let (s, _) = st.sign_up(&email, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, signed) = st.sign_in_local(&email, "synthetic-pass-1").await;
    let token = signed["idToken"].as_str().unwrap().to_owned();
    let mut sent = 1;
    let mut wrongs = 0;
    while wrongs < 10 {
        let right = mailed_code(wait_mail(&rec, &lower, sent).await.last().unwrap());
        let wrong = if right == "000000" {
            "000001"
        } else {
            "000000"
        };
        for _ in 0..5 {
            let (s, b) = st.confirm_code(&token, wrong).await;
            assert_eq!(code(&b), "AUTH.CONFIRMATION_CODE_INVALID", "{s}");
            wrongs += 1;
        }
        // A new code after five wrong ones.
        assert_eq!(st.resend(&token).await.0, StatusCode::ACCEPTED);
        sent += 1;
    }
    let right = mailed_code(wait_mail(&rec, &lower, sent).await.last().unwrap());
    let (s, b) = st.confirm_code(&token, &right).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED"),
        "{b}"
    );
    assert!(b["message"].as_str().unwrap().contains("today"), "{b}");
    // The link with the password still confirms.
    let link = mailed_token(wait_mail(&rec, &lower, sent).await.last().unwrap());
    let (s, _) = st.confirm_link(&link, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::OK);

    // The lifetime cap: 30 wrong codes refuse even the right code until the link confirms.
    let other = local_email();
    let (s, _) = st.sign_up(&other, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    sqlx::query("UPDATE credential SET code_failures = 30 WHERE email_lower = $1")
        .bind(other.to_lowercase())
        .execute(&st.owner)
        .await
        .unwrap();
    let mail = wait_mail(&rec, &other, 1).await;
    let (_, signed) = st.sign_in_local(&other, "synthetic-pass-1").await;
    let (s, b) = st
        .confirm_code(signed["idToken"].as_str().unwrap(), &mailed_code(&mail[0]))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "AUTH.RATE_LIMITED"),
        "{b}"
    );
    let (s, _) = st
        .confirm_link(&mailed_token(&mail[0]), "synthetic-pass-1")
        .await;
    assert_eq!(s, StatusCode::OK);
    let failures: i32 =
        sqlx::query_scalar("SELECT code_failures FROM credential WHERE email_lower = $1")
            .bind(other.to_lowercase())
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert_eq!(failures, 0, "confirmation clears the count");

    // L-C2: a resend and a takeover at once leave one open code.
    let raced = local_email();
    let (s, up) = st.sign_up(&raced, "synthetic-pass-1").await;
    assert_eq!(s, StatusCode::CREATED);
    let held = up["idToken"].as_str().unwrap().to_owned();
    let (a, b, c) = tokio::join!(
        st.resend(&held),
        st.sign_up(&raced, "synthetic-pass-2"),
        st.resend(&held)
    );
    assert_eq!(b.0, StatusCode::CREATED, "{}", b.1);
    let _ = (a, c);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM credential_mail_token t JOIN credential c ON c.id = t.credential_id \
         WHERE c.email_lower = $1 AND t.purpose = 'confirm' AND t.used_at IS NULL",
    )
    .bind(raced.to_lowercase())
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(open, 1);
}

/// CR-S83 (OD-168 follow-up DB-Q4, ACCOUNTS.md §5): `POST /launcher/v1/profiles/{xuid}/website`
/// over the router on a `local` stack: every status and code, the no-op repeat without a key, and
/// `websiteListed` on every `Profile` answer (`/me` included).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header)"]
async fn the_website_toggle_sets_the_callers_own_profile() {
    let st = local_stack(8).await;
    let (_, _, _, me, _) = st.local_player("correct horse battery").await;
    let (_, other_account, _, other, _) = st.local_player("correct horse battery").await;
    let xuid_of = |view: &Value| view["profiles"][0]["xuid"].as_str().unwrap().to_owned();
    let (s, view) = st.get(lr::ME, Caller::Launcher(Some(me.as_str()))).await;
    assert_eq!(s, StatusCode::OK, "{view}");
    assert_eq!(view["profiles"][0]["websiteListed"], true, "default listed");
    let xuid = xuid_of(&view);
    let (_, other_view) = st.get(lr::ME, Caller::Launcher(Some(other.as_str()))).await;
    let other_xuid = xuid_of(&other_view);
    let uri = |x: &str| lr::PROFILE_WEBSITE.replace("{xuid}", x);
    let set = |access: &str, x: &str, body: Value| {
        let (access, uri) = (access.to_owned(), uri(x));
        let st = &st;
        async move {
            st.launcher_json(Method::POST, &uri, &access, None, Some(body))
                .await
        }
    };

    // Hidden, then the same value again: 200 both times, no idempotency key needed.
    for _ in 0..2 {
        let a = set(&me, &xuid, json!({"listed": false})).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.json());
        assert_eq!(a.json()["websiteListed"], false);
        assert_eq!(a.json()["xuid"], xuid.as_str());
    }
    let (_, view) = st.get(lr::ME, Caller::Launcher(Some(me.as_str()))).await;
    assert_eq!(view["profiles"][0]["websiteListed"], false);
    let back = set(&me, &xuid, json!({"listed": true})).await;
    assert_eq!(
        (back.status, back.json()["websiteListed"].clone()),
        (StatusCode::OK, json!(true))
    );

    // Bodies: a boolean `listed` and nothing else.
    for body in [
        json!({}),
        json!({"listed": "no"}),
        json!({"listed": true, "name": "x"}),
    ] {
        let a = set(&me, &xuid, body.clone()).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::BAD_REQUEST, "REQUEST.INVALID".into()),
            "{body}"
        );
    }
    // Only the caller's own live profile.
    for x in [other_xuid.as_str(), "2535999999999999", "nope"] {
        let a = set(&me, x, json!({"listed": false})).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND".into()),
            "{x}"
        );
    }
    let (_, other_view) = st.get(lr::ME, Caller::Launcher(Some(other.as_str()))).await;
    assert_eq!(
        other_view["profiles"][0]["websiteListed"], true,
        "untouched"
    );
    // Without a token.
    let anon = st
        .call(
            Method::POST,
            &uri(&xuid),
            Caller::Launcher(None),
            None,
            Some(json!({"listed": false})),
        )
        .await;
    assert_eq!(anon.0, StatusCode::UNAUTHORIZED, "{}", anon.1);
    // An account pending deletion changes nothing.
    sqlx::query(
        "UPDATE account SET status = 'deletion_pending', deletion_requested_at = now(), \
           deletion_due_at = now() + interval '14 days' WHERE id = $1",
    )
    .bind(Uuid::parse_str(&other_account).unwrap())
    .execute(&st.owner)
    .await
    .unwrap();
    let pending = set(&other, &other_xuid, json!({"listed": false})).await;
    assert_eq!(
        (pending.status, pending.code()),
        (StatusCode::FORBIDDEN, "ACCOUNTS.DELETION_PENDING".into())
    );
    let listed: bool = sqlx::query_scalar("SELECT web_listed FROM profile WHERE xuid = $1")
        .bind(other_xuid.parse::<i64>().unwrap())
        .fetch_one(&st.owner)
        .await
        .unwrap();
    assert!(listed);
}
