//! End-to-end flows over fi-core's whole router (guards included): Postgres as a `fi_runtime`
//! login, ID tokens from the Firebase Auth emulator (project `demo-forza-infinity`), Turnstile in
//! test mode with Cloudflare's documented test secrets (no network call), the MCP skeleton with
//! test-minted tokens. Every CORE_API.md row and code has a request here; the identity flows of
//! fi-api's e2e pass through this router too.
//!
//! ```sh
//! docker compose up -d --wait postgres auth-emulator
//! FI_TEST_DATABASE_URL=postgres://fi_migrator:fi_migrator_local@127.0.0.1:5452/forza_infinity_test \
//! FIREBASE_AUTH_EMULATOR_HOST=127.0.0.1:9399 cargo test -p fi-core -- --include-ignored
//! ```
//!
//! Every test uses fresh emulator users, client addresses and a fresh router (its own token
//! buckets), so runs never interfere.

use crate::config::{CoreConfig, Rates};
use crate::turnstile::{TEST_SECRET_FAIL, TEST_SECRET_PASS};
use crate::{Core, router};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use fi_db::{PgPool, migrate};
use fi_protocol::routes::{core as cr, launcher as lr, portal as pr};
use fi_runtime::ServiceInfo;
use fi_runtime::ratelimit::Rate;
use fi_surfaces::config::{IdentityConfig, IdentityNeeds, PlatformConfig};
use fi_surfaces::state::AppState;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::io::{Read, Write};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

const LOGIN: &str = "fi_app_core_e2e";
const PROJECT: &str = "demo-forza-infinity";
const WEB: &str = "http://127.0.0.1:3120";
/// The well-known local values of .env.example (refused in prod).
const LOCAL_TOKEN: &str = "at-local-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=";
const LOCAL_TICKET: &str = "lt-local-1:8BxGVNw9Cyi6bbk5aWzYhSbdgW8CRkM125qYUron8uI=";
const LOCAL_PEPPER: &str = "1:84yC96SRqM8E+D3MCtARPCjLJ7eiFc/pGvdtLu2TaHQ=";
/// A test-only MCP key ring (the seed is a public label's bytes, like the local keys).
const MCP_KEYS: &str = "mcp-test-1:bWNwLXRlc3Qta2V5LW5vdC1hLXNlY3JldC0wMDAwMDA=";

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

fn random_ip() -> String {
    let b = Uuid::now_v7().into_bytes();
    format!("10.{}.{}.{}", b[10], b[11], b[12])
}

/// Generous buckets for the flows (the rate-limit test uses the contract's).
fn roomy() -> Rates {
    Rates {
        public_read: Rate::per_minute(10_000),
        public_write: Rate::per_minute(10_000),
        account_write: Rate::per_minute(10_000),
        comment: Rate::per_minute(10_000),
        account_read: Rate::per_minute(10_000),
        bot: Rate::per_minute(10_000),
    }
}

struct Options {
    vars: Vec<(&'static str, String)>,
    rates: Rates,
    connections: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            vars: Vec::new(),
            rates: roomy(),
            connections: 8,
        }
    }
}

struct Stack {
    app: Router,
    emulator: String,
    ip: String,
    owner: PgPool,
    identity: Arc<IdentityConfig>,
}

async fn stack() -> Stack {
    stack_with(Options::default()).await
}

async fn stack_with(o: Options) -> Stack {
    let url = env("FI_TEST_DATABASE_URL");
    let emulator = env("FIREBASE_AUTH_EMULATOR_HOST");
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
        .max_connections(o.connections)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with(options)
        .await
        .expect("connect as the runtime login");
    let mut vars: Vec<(&'static str, String)> = vec![
        ("APP_ENV", "local".into()),
        ("GCP_PROJECT_ID", PROJECT.into()),
        ("FIREBASE_AUTH_EMULATOR_HOST", emulator.clone()),
        ("FI_IDP_PROJECT_ID", PROJECT.into()),
        // The per-account launcher buckets (OD-215) out of the way of per-route limit tests.
        ("FI_LAUNCHER_ACCOUNT_SCALE", "100".into()),
        ("FI_TOKEN_SIGNING_KEYS", LOCAL_TOKEN.into()),
        ("FI_LAUNCH_TICKET_SIGNING_KEYS", LOCAL_TICKET.into()),
        ("FI_SITE_KEY_PEPPER", LOCAL_PEPPER.into()),
        ("FI_PUBLIC_CORE_URL", "http://127.0.0.1:8480".into()),
        ("FI_PUBLIC_GATE_URL", "http://127.0.0.1:8480".into()),
    ];
    for (k, v) in o.vars {
        vars.retain(|(name, _)| *name != k);
        vars.push((k, v));
    }
    let identity = Arc::new(
        IdentityConfig::from_env_for(&vars, IdentityNeeds::CORE).expect("identity config"),
    );
    let mut config = CoreConfig::from_env(&vars).expect("core config");
    config.rates = o.rates;
    let core = Arc::new(Core::new(config));
    let state = AppState {
        pool,
        identity: Some(identity.clone()),
    };
    let owner = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect to the test database");
    Stack {
        app: router(
            ServiceInfo::from_env("fi-core", "0.0.0"),
            state,
            PlatformConfig::defaults(None),
            core,
        ),
        emulator,
        ip: random_ip(),
        owner,
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

/// A verified e-mail/password user of the emulator: `idToken`.
fn emulator_user(host: &str, verified: bool) -> String {
    let base = "/identitytoolkit.googleapis.com/v1";
    let email = format!("core-e2e-{}@example.test", fresh());
    let credentials =
        json!({"email": email, "password": "synthetic-pass-1", "returnSecureToken": true});
    let created = emulator_post(
        host,
        &format!("{base}/accounts:signUp?key=local"),
        &credentials,
        false,
    );
    if !verified {
        return created["idToken"].as_str().unwrap().to_owned();
    }
    let uid = created["localId"].as_str().unwrap().to_owned();
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
    signed["idToken"].as_str().unwrap().to_owned()
}

/// An HTTP answer.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.body["code"].as_str().unwrap_or_default()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The `__Host-fi_session` value the answer sets.
    fn cookie(&self) -> Option<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix("__Host-fi_session="))
            .and_then(|v| v.split(';').next())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    }
}

/// Who calls.
#[derive(Clone, Copy)]
enum Who<'a> {
    /// A browser on the web origin, with the session cookie when given.
    Web(Option<&'a str>),
    /// The launcher, with its access token when given.
    Launcher(Option<&'a str>),
    /// Anything else (no Origin, no X-Fi-Client, no credential).
    Bare,
}

/// A request in the making.
struct Call<'a> {
    st: &'a Stack,
    req: axum::http::request::Builder,
    body: Option<Value>,
}

impl<'a> Call<'a> {
    fn header(mut self, name: &str, value: &str) -> Self {
        self.req = self.req.header(name, value);
        self
    }

    fn key(self) -> Self {
        let k = Uuid::now_v7().to_string();
        self.header("idempotency-key", &k)
    }

    fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    fn challenge(self) -> Self {
        let t = format!("XXXX.DUMMY.TOKEN.{}", fresh());
        self.header("x-fi-challenge", &t)
    }

    async fn send(self) -> Answer {
        let req = match self.body {
            Some(b) => self
                .req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => self.req.body(Body::empty()),
        }
        .unwrap();
        let response = self.st.app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Answer {
            status,
            headers,
            body,
        }
    }
}

impl Stack {
    fn call<'a>(&'a self, method: Method, uri: &str, who: Who<'_>) -> Call<'a> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-forwarded-for", &self.ip);
        match who {
            Who::Web(cookie) => {
                req = req.header("origin", WEB).header("x-fi-client", "web/0.1.0");
                if let Some(c) = cookie {
                    req = req.header("cookie", format!("theme=dark; __Host-fi_session={c}"));
                }
            }
            Who::Launcher(Some(t)) => req = req.header("authorization", format!("Bearer {t}")),
            Who::Launcher(None) | Who::Bare => {}
        }
        Call {
            st: self,
            req,
            body: None,
        }
    }

    fn get<'a>(&'a self, uri: &str, who: Who<'_>) -> Call<'a> {
        self.call(Method::GET, uri, who)
    }

    fn post<'a>(&'a self, uri: &str, who: Who<'_>, body: Value) -> Call<'a> {
        self.call(Method::POST, uri, who).key().json(body)
    }

    /// The web's sign-up: sign-in (needs an account), account creation; returns the cookie and
    /// the account id.
    async fn web_sign_up(&self, id_token: &str) -> (String, String) {
        let signed = self
            .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
            .challenge()
            .send()
            .await;
        assert_eq!(signed.status, StatusCode::CREATED, "{}", signed.body);
        assert_eq!(signed.body["status"], "needs_account");
        let created = self
            .post(
                pr::ACCOUNTS,
                Who::Web(None),
                json!({"identityTicket": signed.body["identityTicket"]}),
            )
            .challenge()
            .send()
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        (
            created.cookie().expect("the session cookie"),
            created.body["viewer"]["accountId"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    }

    /// The launcher's native ID-token exchange.
    async fn launcher_sign_in(&self, id_token: &str, device: &Value) -> Answer {
        self.post(
            lr::AUTH_TOKEN,
            Who::Launcher(None),
            json!({"grantType": "id_token", "idToken": id_token, "device": device}),
        )
        .send()
        .await
    }

    /// A player with a web session, a launcher session and one profile of `mode`.
    async fn player(&self, mode: &str) -> Player {
        let id_token = emulator_user(&self.emulator, true);
        let (cookie, account_id) = self.web_sign_up(&id_token).await;
        let signed = self.launcher_sign_in(&id_token, &device()).await;
        assert_eq!(signed.status, StatusCode::OK, "{}", signed.body);
        let access = signed.body["accessToken"].as_str().unwrap().to_owned();
        let name = format!("C{}", &fresh()[22..]);
        let profile = self
            .post(
                lr::PROFILES,
                Who::Launcher(Some(&access)),
                json!({"game": "fh2", "name": name, "mode": mode}),
            )
            .send()
            .await;
        assert_eq!(profile.status, StatusCode::CREATED, "{}", profile.body);
        Player {
            cookie,
            access,
            account_id,
            xuid: profile.body["xuid"].as_str().unwrap().to_owned(),
            gamertag: profile.body["gamertag"].as_str().unwrap().to_owned(),
            name,
        }
    }

    async fn set_role(&self, account: &str, role: &str) {
        sqlx::query("UPDATE account SET role = $2 WHERE id = $1::uuid")
            .bind(account)
            .bind(role)
            .execute(&self.owner)
            .await
            .unwrap();
    }

    async fn make_supporter(&self, account: &str) {
        sqlx::query(
            "INSERT INTO entitlement (account_id, product, status, plan, provider, coverage_end, access_until, auto_renew, past_due, blocked, version) \
             VALUES ($1::uuid, 'vip', 'active', 'comp', 'admin', clock_timestamp() + interval '30 days', clock_timestamp() + interval '30 days', true, false, false, 1) \
             ON CONFLICT (account_id, product) DO UPDATE SET status = 'active', access_until = EXCLUDED.access_until",
        )
        .bind(account)
        .execute(&self.owner)
        .await
        .unwrap();
    }

    async fn audit_rows(&self, actor: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM admin_action WHERE actor_account_id = $1::uuid")
            .bind(actor)
            .fetch_one(&self.owner)
            .await
            .unwrap()
    }

    /// A live play session of the player's profile in a pool of this run (`active` or `queued`).
    async fn live_session(&self, p: &Player, state: &str) -> String {
        let pool = format!("t-{}", &fresh()[18..]);
        sqlx::query(
            "INSERT INTO pool (name, tier, capacity, game_endpoint) VALUES ($1, 'vip', 3, 'http://game.test')",
        )
        .bind(&pool)
        .execute(&self.owner)
        .await
        .unwrap();
        let id = Uuid::now_v7();
        let active = state == "active";
        sqlx::query(
            "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, state, launcher_version, \
               queued_at, admitted_at, expires_at, max_until, game_until) \
             SELECT $1, p.account_id, p.id, (SELECT d.id FROM device_binding d WHERE d.account_id = p.account_id LIMIT 1), \
               $2, $3, '0.1.0', clock_timestamp(), CASE WHEN $4 THEN clock_timestamp() END, \
               clock_timestamp() + interval '5 minutes', CASE WHEN $4 THEN now() + interval '12 hours' END, \
               CASE WHEN $4 THEN now() + interval '12 hours' END \
             FROM profile p WHERE p.xuid = $5::bigint",
        )
        .bind(id)
        .bind(&pool)
        .bind(state)
        .bind(active)
        .bind(&p.xuid)
        .execute(&self.owner)
        .await
        .unwrap();
        id.to_string()
    }
}

struct Player {
    cookie: String,
    access: String,
    account_id: String,
    xuid: String,
    gamertag: String,
    name: String,
}

fn device() -> Value {
    json!({"deviceId": Uuid::now_v7().to_string(), "label": "E2E PC", "platform": "windows"})
}

/// A game access ticket (the gate's token: `aud = game`) signed with the same local ring, built
/// from its wire claims so the test does not depend on the minting code.
fn game_ticket(st: &Stack) -> String {
    let (kid, key) = st.identity.access_keys.signing().unwrap().current();
    let now = fi_domain::UnixMs::now_system().millis() / 1_000;
    let claims = json!({
        "sid": Uuid::now_v7().to_string(), "sub": Uuid::now_v7().to_string(), "acc": Uuid::now_v7().to_string(),
        "xuid": "2535100000000001", "pool": "vip", "tier": "vip", "mode": "noob", "cdb": "", "aud": "game",
        "iat": fi_protocol::instant::format_unix_secs(now).unwrap(),
        "exp": fi_protocol::instant::format_unix_secs(now + 600).unwrap(),
    });
    let footer = json!({"kid": kid}).to_string();
    fi_auth::paseto::sign(key, claims.to_string().as_bytes(), footer.as_bytes(), b"")
}

fn assert_code(a: &Answer, status: StatusCode, code: &str) {
    assert_eq!((a.status, a.code()), (status, code), "{}", a.body);
}

/// The friend routes (CORE_API.md §4.1).
mod friends;

/// Reports from the game (CORE_API.md §4.3).
mod reports;

/// Discord: the feed, the link and the announce switch (CORE_API.md §5a).
mod discord;

/// Voice moderation: mutes, the community mute, voice bans (CORE_API.md §4.4, §5; CR-S68).
mod voice;

/// Lifting a cheat flag (CORE_API.md §5, MATCHMAKING_API.md §3.9).
mod cheat;

// ---------------------------------------------------------------------------------------------
// The identity flows of fi-api, through fi-core's router.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn identity_flows_pass_through_fi_cores_router() {
    let st = stack().await;
    // OD-74: the production core router cannot mint or approve retired launcher codes.
    for (path, who) in [
        ("/launcher/v1/auth/device", Who::Launcher(None)),
        ("/portal/v1/auth/launcher-codes", Who::Web(None)),
        ("/portal/v1/auth/device-approvals", Who::Web(None)),
    ] {
        let response = st.post(path, who, json!({})).send().await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
    }
    for grant in [
        json!({"grantType": "authorization_code", "code": "retired", "device": device()}),
        json!({"grantType": "device_code", "deviceCode": "retired", "device": device()}),
    ] {
        let response = st
            .post(lr::AUTH_TOKEN, Who::Launcher(None), grant)
            .send()
            .await;
        assert_code(&response, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    }
    let unverified = emulator_user(&st.emulator, false);
    let id_token = emulator_user(&st.emulator, true);

    // An unverified password e-mail and garbage are refused, behind the challenge.
    let a = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": unverified}))
        .challenge()
        .send()
        .await;
    assert_code(&a, StatusCode::FORBIDDEN, "AUTH.EMAIL_UNVERIFIED");
    let a = st
        .post(
            pr::SESSIONS,
            Who::Web(None),
            json!({"idToken": "not-a-token"}),
        )
        .challenge()
        .send()
        .await;
    assert_code(&a, StatusCode::UNAUTHORIZED, "AUTH.ID_TOKEN_INVALID");

    // Sign-up on the web: the replay of the same key answers a new identity ticket.
    let key = Uuid::now_v7().to_string();
    let first = st
        .call(Method::POST, pr::SESSIONS, Who::Web(None))
        .header("idempotency-key", &key)
        .json(json!({"idToken": id_token}))
        .challenge()
        .send()
        .await;
    assert_eq!(first.body["status"], "needs_account", "{}", first.body);
    let replay = st
        .call(Method::POST, pr::SESSIONS, Who::Web(None))
        .header("idempotency-key", &key)
        .json(json!({"idToken": id_token}))
        .challenge()
        .send()
        .await;
    assert_eq!(replay.status, StatusCode::CREATED);
    assert_ne!(first.body["identityTicket"], replay.body["identityTicket"]);
    let created = st
        .post(
            pr::ACCOUNTS,
            Who::Web(None),
            json!({"identityTicket": replay.body["identityTicket"], "locale": "pt-BR"}),
        )
        .challenge()
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert!(
        created.body.get("token").is_none(),
        "the token is a cookie only"
    );
    let account_id = created.body["viewer"]["accountId"]
        .as_str()
        .unwrap()
        .to_owned();
    let cookie = created.cookie().unwrap();
    let me = st.get(pr::ME, Who::Web(Some(&cookie))).send().await;
    assert_eq!(me.body["accountId"].as_str(), Some(account_id.as_str()));
    let again = st
        .post(
            pr::ACCOUNTS,
            Who::Web(None),
            json!({"identityTicket": replay.body["identityTicket"]}),
        )
        .challenge()
        .send()
        .await;
    assert_code(
        &again,
        StatusCode::UNAUTHORIZED,
        "AUTH.IDENTITY_TICKET_INVALID",
    );

    // Native sign-in and CR-L4 replay.
    let dev = device();
    let grant = || json!({"grantType": "id_token", "idToken": id_token, "device": dev});
    let key = Uuid::now_v7().to_string();
    let signed = st
        .call(Method::POST, lr::AUTH_TOKEN, Who::Launcher(None))
        .header("idempotency-key", &key)
        .json(grant())
        .send()
        .await;
    assert_eq!(signed.body["status"], "signed_in", "{}", signed.body);
    let resigned = st
        .call(Method::POST, lr::AUTH_TOKEN, Who::Launcher(None))
        .header("idempotency-key", &key)
        .json(grant())
        .send()
        .await;
    assert_ne!(signed.body["refreshToken"], resigned.body["refreshToken"]);
    let stale = st
        .get(lr::ME, Who::Launcher(signed.body["accessToken"].as_str()))
        .send()
        .await;
    assert_code(&stale, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    let access = resigned.body["accessToken"].as_str().unwrap().to_owned();
    let me = st.get(lr::ME, Who::Launcher(Some(&access))).send().await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);

    // The launcher's native sign-in (OD-43): the same identity's ID token, no web page between.
    let native = st
        .post(
            lr::AUTH_TOKEN,
            Who::Launcher(None),
            json!({"grantType": "id_token", "idToken": id_token, "device": dev}),
        )
        .send()
        .await;
    assert_eq!(native.status, StatusCode::OK, "{}", native.body);
    assert_eq!(native.body["status"], "signed_in");
    assert_eq!(native.body["accountId"].as_str(), Some(account_id.as_str()));
    // OD-93: this newer successful native login supersedes the previous family.
    let replaced = st.get(lr::ME, Who::Launcher(Some(&access))).send().await;
    assert_code(&replaced, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    let access = native.body["accessToken"].as_str().unwrap().to_owned();
    let native_unverified = st
        .post(
            lr::AUTH_TOKEN,
            Who::Launcher(None),
            json!({"grantType": "id_token", "idToken": unverified, "device": dev}),
        )
        .send()
        .await;
    assert_code(
        &native_unverified,
        StatusCode::FORBIDDEN,
        "AUTH.EMAIL_UNVERIFIED",
    );

    // A profile, its replay, and a key reused for another body.
    let key = Uuid::now_v7().to_string();
    let body = json!({"game": "fh2", "name": format!("E{}", &fresh()[24..]), "mode": "noob"});
    let profile = st
        .call(Method::POST, lr::PROFILES, Who::Launcher(Some(&access)))
        .header("idempotency-key", &key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(profile.status, StatusCode::CREATED, "{}", profile.body);
    let same = st
        .call(Method::POST, lr::PROFILES, Who::Launcher(Some(&access)))
        .header("idempotency-key", &key)
        .json(body)
        .send()
        .await;
    assert_eq!(same.body["profileId"], profile.body["profileId"]);
    let reused = st
        .call(Method::POST, lr::PROFILES, Who::Launcher(Some(&access)))
        .header("idempotency-key", &key)
        .json(json!({"game": "fh2", "name": "Other", "mode": "noob"}))
        .send()
        .await;
    assert_code(
        &reused,
        StatusCode::CONFLICT,
        "REQUEST.IDEMPOTENCY_KEY_REUSED",
    );

    // Refresh rotation and reuse detection.
    let r2 = native.body["refreshToken"].clone();
    let pair = st
        .post(lr::TOKEN, Who::Launcher(None), json!({"refreshToken": r2}))
        .send()
        .await;
    assert_eq!(pair.status, StatusCode::OK, "{}", pair.body);
    let reuse = st
        .post(lr::TOKEN, Who::Launcher(None), json!({"refreshToken": r2}))
        .send()
        .await;
    assert_code(&reuse, StatusCode::UNAUTHORIZED, "AUTH.REFRESH_REUSED");

    // Sign-out of a fresh launcher session.
    let fresh_in = st.launcher_sign_in(&id_token, &dev).await;
    let access = fresh_in.body["accessToken"].as_str().unwrap().to_owned();
    let out = st
        .call(Method::POST, lr::SIGN_OUT, Who::Launcher(Some(&access)))
        .send()
        .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    let gone = st.get(lr::ME, Who::Launcher(Some(&access))).send().await;
    assert_eq!(gone.status, StatusCode::UNAUTHORIZED);

    // A fresh native identity creates its account; the website then signs straight in.
    let other = emulator_user(&st.emulator, true);
    let dev = device();
    let result = st.launcher_sign_in(&other, &dev).await;
    assert_eq!(result.body["status"], "needs_account");
    let account = st
        .post(
            lr::ACCOUNTS,
            Who::Launcher(None),
            json!({
                "identityTicket": result.body["identityTicket"], "device": dev,
                // `starterCar` left the contract (OD-60): a launcher that still sends it is
                // accepted and the field ignored.
                "profile": {"game": "fh2", "name": format!("D{}", &fresh()[24..]), "mode": "hardcore", "starterCar": 289},
            }),
        )
        .send()
        .await;
    assert_eq!(account.status, StatusCode::CREATED, "{}", account.body);
    let web = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": other}))
        .challenge()
        .send()
        .await;
    assert_eq!(web.body["status"], "signed_in", "{}", web.body);
    assert_eq!(web.body["viewer"]["accountId"], account.body["accountId"]);
    assert!(web.cookie().is_some());

    // /launcher/v1/info names the services and the games; /internal/v1 is mounted.
    let info = st.get(lr::INFO, Who::Launcher(None)).send().await;
    assert_eq!(info.status, StatusCode::OK);
    assert_eq!(info.body["services"]["core"], "http://127.0.0.1:8480");
    let fh2 = info.body["games"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "fh2")
        .cloned()
        .unwrap();
    assert_eq!(fh2["title"], "Forza Horizon 2");
    assert!(info.header("cache-control").unwrap().starts_with("public"));
    // The native sign-in's Identity Platform settings (OD-43); no public emulator URL is set here.
    assert_eq!(
        info.body["idp"],
        json!({"apiKey": "demo-api-key", "projectId": PROJECT})
    );
    // Local only (FI_LOCAL_ADMIN, off here; GAME_PLATFORM.md §3.1, CR-S47).
    let pools = st.get("/internal/v1/pools", Who::Bare).send().await;
    assert_eq!(pools.status, StatusCode::FORBIDDEN, "{}", pools.body);
    assert_eq!(pools.body["code"], "AUTH.FORBIDDEN");
    // Sessions and saves are not core's.
    let sessions = st
        .post(lr::SESSIONS, Who::Launcher(Some(&access)), json!({}))
        .send()
        .await;
    assert_eq!(sessions.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn every_route_holds_one_pooled_connection_at_a_time() {
    let st = stack_with(Options {
        connections: 1,
        ..Options::default()
    })
    .await;
    let p = st.player("noob").await;
    st.make_supporter(&p.account_id).await;
    let other = st.player("noob").await;
    for uri in [
        cr::STATUS.to_owned(),
        format!("{}?game=fh2", cr::NOTICES),
        cr::ME_NOTICES.to_owned(),
        cr::ME_STRIKES.to_owned(),
        format!("{}?game=fh2", cr::SOCIAL_PLAYERS),
        format!("/core/v1/social/players/{}", other.xuid),
    ] {
        let a = st.get(&uri, Who::Web(Some(&p.cookie))).send().await;
        assert_eq!(a.status, StatusCode::OK, "{uri}: {}", a.body);
    }
    let c = st
        .post(
            cr::SOCIAL_COMMENTS,
            Who::Web(Some(&p.cookie)),
            json!({"target": format!("player:{}", other.xuid), "body": "synthetic"}),
        )
        .challenge()
        .send()
        .await;
    assert_eq!(c.status, StatusCode::CREATED, "{}", c.body);
    // A report from the game (CR-L25) about a world the two share (set up on the owner's pool).
    let world = reports::open_world(&st, &reports::in_game(&st, &other).await).await;
    reports::seat(&st, &reports::in_game(&st, &p).await, world).await;
    let r = st
        .post(
            lr::REPORTS,
            Who::Launcher(Some(&p.access)),
            json!({"game": "fh2", "xuid": other.xuid, "reason": "other", "worldId": world.to_string()}),
        )
        .send()
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    st.set_role(&p.account_id, "admin").await;
    let view = st
        .get(
            &format!("/core/v1/admin/accounts/{}", other.account_id),
            Who::Web(Some(&p.cookie)),
        )
        .send()
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    let kick = st
        .post(
            cr::ADMIN_KICKS,
            Who::Web(Some(&p.cookie)),
            json!({"xuid": other.xuid, "reason": "synthetic", "cooldownMinutes": 1}),
        )
        .send()
        .await;
    assert_eq!(kick.status, StatusCode::OK, "{}", kick.body);
    let ban = st
        .post(
            cr::ADMIN_BANS,
            Who::Web(Some(&p.cookie)),
            json!({"accountId": other.account_id, "until": null, "reason": "synthetic"}),
        )
        .send()
        .await;
    assert_eq!(ban.status, StatusCode::CREATED, "{}", ban.body);
}

// ---------------------------------------------------------------------------------------------
// The web session: cookie, CORS, CSRF, sign-out.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_web_session_is_an_http_only_cookie_with_cors_and_csrf_rules() {
    let st = stack().await;
    let id_token = emulator_user(&st.emulator, true);
    let (_, _) = st.web_sign_up(&id_token).await;
    let signed = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
        .challenge()
        .send()
        .await;
    assert_eq!(signed.status, StatusCode::CREATED);
    let set = signed.header("set-cookie").unwrap().to_owned();
    assert!(set.starts_with("__Host-fi_session=fi_ps_"), "{set}");
    for attr in [
        "; Path=/",
        "; Secure",
        "; HttpOnly",
        "; SameSite=Strict",
        "; Max-Age=",
    ] {
        assert!(set.contains(attr), "{attr} in {set}");
    }
    assert!(!set.contains("Domain"));
    assert_eq!(signed.header("cache-control"), Some("no-store"));
    assert!(signed.body.get("token").is_none());
    let cookie = signed.cookie().unwrap();
    // CORS: exactly the web origin, with credentials.
    assert_eq!(signed.header("access-control-allow-origin"), Some(WEB));
    assert_eq!(
        signed.header("access-control-allow-credentials"),
        Some("true")
    );
    let pre = st
        .call(Method::OPTIONS, pr::SESSIONS, Who::Bare)
        .header("origin", WEB)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "content-type, x-fi-challenge",
        )
        .send()
        .await;
    assert_eq!(pre.status, StatusCode::NO_CONTENT);
    assert_eq!(pre.header("access-control-allow-origin"), Some(WEB));
    assert!(
        pre.header("access-control-allow-headers")
            .unwrap()
            .contains("x-fi-challenge")
    );
    let evil = st
        .call(Method::OPTIONS, pr::SESSIONS, Who::Bare)
        .header("origin", "https://evil.example")
        .header("access-control-request-method", "POST")
        .send()
        .await;
    assert_eq!(evil.status, StatusCode::FORBIDDEN);
    assert_eq!(evil.header("access-control-allow-origin"), None);
    let other_read = st
        .get(pr::ME, Who::Bare)
        .header("origin", "https://evil.example")
        .header("cookie", &format!("__Host-fi_session={cookie}"))
        .send()
        .await;
    assert_eq!(
        other_read.header("access-control-allow-origin"),
        Some(WEB),
        "always the web origin, never the caller's"
    );
    assert!(
        other_read
            .header("vary")
            .unwrap_or_default()
            .contains("Origin")
    );
    // CSRF: a cookie write without Origin, without X-Fi-Client, or from another origin.
    let me = st.get(pr::ME, Who::Web(Some(&cookie))).send().await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(
        me.header("cache-control"),
        Some("no-store"),
        "personal answers are never cached"
    );
    let cookie_header = format!("__Host-fi_session={cookie}");
    let no_origin = st
        .call(Method::DELETE, pr::SESSIONS_CURRENT, Who::Bare)
        .header("cookie", &cookie_header)
        .header("x-fi-client", "web/0.1.0")
        .send()
        .await;
    assert_code(&no_origin, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    assert_eq!(no_origin.body["details"]["reason"], "csrf");
    let no_client = st
        .call(Method::DELETE, pr::SESSIONS_CURRENT, Who::Bare)
        .header("cookie", &cookie_header)
        .header("origin", WEB)
        .send()
        .await;
    assert_code(&no_client, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    let foreign = st
        .call(Method::DELETE, pr::SESSIONS_CURRENT, Who::Bare)
        .header("cookie", &cookie_header)
        .header("origin", "https://evil.example")
        .header("x-fi-client", "web/0.1.0")
        .send()
        .await;
    assert_code(&foreign, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    let comment = st
        .call(Method::POST, cr::SOCIAL_REPORTS, Who::Bare)
        .header("cookie", &cookie_header)
        .key()
        .json(json!({"target": "player:2535100000000001", "reason": "other"}))
        .send()
        .await;
    assert_code(&comment, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    assert_eq!(
        st.get(pr::ME, Who::Web(Some(&cookie))).send().await.status,
        StatusCode::OK
    );
    // Sign-out revokes and clears.
    let out = st
        .call(
            Method::DELETE,
            pr::SESSIONS_CURRENT,
            Who::Web(Some(&cookie)),
        )
        .send()
        .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(
        out.header("set-cookie")
            .unwrap()
            .contains("__Host-fi_session=; Path=/; Max-Age=0")
    );
    let after = st.get(pr::ME, Who::Web(Some(&cookie))).send().await;
    assert_code(&after, StatusCode::UNAUTHORIZED, "PORTAL.SESSION_REQUIRED");
    let again = st
        .call(
            Method::DELETE,
            pr::SESSIONS_CURRENT,
            Who::Web(Some(&cookie)),
        )
        .send()
        .await;
    assert_code(&again, StatusCode::UNAUTHORIZED, "PORTAL.SESSION_REQUIRED");
    assert!(again.header("set-cookie").unwrap().contains("Max-Age=0"));
    // No credential at all on a session route.
    let none = st.get(cr::ME_NOTICES, Who::Web(None)).send().await;
    assert_code(&none, StatusCode::UNAUTHORIZED, "PORTAL.SESSION_REQUIRED");
}

// ---------------------------------------------------------------------------------------------
// Turnstile.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn turnstile_is_required_spent_once_and_verified() {
    let st = stack().await;
    let id_token = emulator_user(&st.emulator, true);
    let missing = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
        .send()
        .await;
    assert_code(&missing, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_REQUIRED");
    assert_eq!(missing.body["details"]["challenge"], "turnstile");
    let token = format!("XXXX.DUMMY.TOKEN.{}", fresh());
    let ok = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
        .header("x-fi-challenge", &token)
        .send()
        .await;
    assert_eq!(
        ok.status,
        StatusCode::CREATED,
        "always-pass secret: {}",
        ok.body
    );
    let reused = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
        .header("x-fi-challenge", &token)
        .send()
        .await;
    assert_code(&reused, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_FAILED");
    let long = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
        .header("x-fi-challenge", &"x".repeat(2049))
        .send()
        .await;
    assert_code(&long, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_FAILED");
    let account = st
        .post(
            pr::ACCOUNTS,
            Who::Web(None),
            json!({"identityTicket": ok.body["identityTicket"]}),
        )
        .send()
        .await;
    assert_code(&account, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_REQUIRED");

    // The always-fail secret: every token fails, on every challenged route.
    let failing = stack_with(Options {
        vars: vec![("FI_TURNSTILE_SECRET", TEST_SECRET_FAIL.into())],
        ..Options::default()
    })
    .await;
    for (uri, body) in [
        (pr::SESSIONS, json!({"idToken": id_token})),
        (pr::ACCOUNTS, json!({"identityTicket": "fi_it_x"})),
    ] {
        let a = failing
            .post(uri, Who::Web(None), body)
            .challenge()
            .send()
            .await;
        assert_code(&a, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_FAILED");
    }
    // Comments and reports are challenged too (a session that may write them).
    let p = st.player("noob").await;
    st.make_supporter(&p.account_id).await;
    let target = st.player("noob").await;
    for (uri, body) in [
        (
            cr::SOCIAL_COMMENTS,
            json!({"target": format!("player:{}", target.xuid), "body": "hi"}),
        ),
        (
            cr::SOCIAL_REPORTS,
            json!({"target": format!("player:{}", target.xuid), "reason": "other"}),
        ),
    ] {
        let a = st
            .post(uri, Who::Web(Some(&p.cookie)), body.clone())
            .send()
            .await;
        assert_code(&a, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_REQUIRED");
        let a = st
            .post(uri, Who::Launcher(Some(&p.access)), body)
            .challenge()
            .send()
            .await;
        assert!(a.status.is_success(), "{uri}: {}", a.body);
    }
    assert_eq!(TEST_SECRET_PASS.len(), 35);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_test_widgets_dummy_token_passes_every_time_in_test_mode_only() {
    use crate::turnstile::TEST_DUMMY_TOKEN;
    let st = stack().await;
    // The real widget with the always-pass test site key answers this one token every time: a
    // sign-in and the sign-up that follows both pass with it, as on the web's fake server.
    for _ in 0..2 {
        let id_token = emulator_user(&st.emulator, true);
        let signed = st
            .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
            .header("x-fi-challenge", TEST_DUMMY_TOKEN)
            .send()
            .await;
        assert_eq!(signed.status, StatusCode::CREATED, "{}", signed.body);
        assert_eq!(signed.body["status"], "needs_account");
        let created = st
            .post(
                pr::ACCOUNTS,
                Who::Web(None),
                json!({"identityTicket": signed.body["identityTicket"]}),
            )
            .header("x-fi-challenge", TEST_DUMMY_TOKEN)
            .send()
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    }
    // Any other token is still spent once in test mode.
    let id_token = emulator_user(&st.emulator, true);
    let token = format!("XXXX.DUMMY.TOKEN.{}", fresh());
    for expect in [StatusCode::CREATED, StatusCode::FORBIDDEN] {
        let a = st
            .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
            .header("x-fi-challenge", &token)
            .send()
            .await;
        assert_eq!(a.status, expect, "{}", a.body);
    }
    // The always-fail test secret fails the dummy token too: the exemption skips the spend only.
    let failing = stack_with(Options {
        vars: vec![("FI_TURNSTILE_SECRET", TEST_SECRET_FAIL.into())],
        ..Options::default()
    })
    .await;
    let a = failing
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": id_token}))
        .header("x-fi-challenge", TEST_DUMMY_TOKEN)
        .send()
        .await;
    assert_code(&a, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_FAILED");
}

/// No real Turnstile key in the tree: Cloudflare's real site keys and secrets are `0x4` followed
/// by `AAA` and at least 12 more key characters (only the documented `1x`/`2x`/`3x` test keys may
/// appear; a prose mention of the prefix is not a key).
#[test]
fn no_real_turnstile_key_anywhere_in_the_tree() {
    let needle = format!("0x{}", "4AAA");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut stack = vec![root];
    let mut hits = Vec::new();
    let skip = ["target", "node_modules", ".git", ".local", "dist", ".next"];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !skip.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if let Ok(text) = std::fs::read_to_string(&path)
                && looks_like_key(&text, &needle)
            {
                hits.push(path.display().to_string());
            }
        }
    }
    assert!(hits.is_empty(), "real-looking Turnstile keys in: {hits:?}");
    // The detector itself: a key-shaped string (built at run time, never in the tree) is found.
    assert!(looks_like_key(
        &format!("k = \"{needle}AAAAbbbbCCCC1234\""),
        &needle
    ));
    assert!(!looks_like_key(&format!("`{needle}` prefixes"), &needle));
}

fn looks_like_key(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(at, _)| {
        let tail = &text.as_bytes()[at + needle.len()..];
        tail.len() >= 12
            && tail[..12]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
    })
}

// ---------------------------------------------------------------------------------------------
// Roles, bans, audit, maintenance, notices.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn roles_gate_every_admin_route_and_every_write_is_audited_once() {
    let st = stack().await;
    let player = st.player("hardcore").await;
    let moderator = st.player("noob").await;
    let admin = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    st.set_role(&admin.account_id, "admin").await;
    let m = Who::Web(Some(&moderator.cookie));
    let a = Who::Launcher(Some(&admin.access));

    // Players get AUTH.ROLE_REQUIRED on every admin route.
    for (method, uri) in [
        (
            Method::GET,
            "/core/v1/admin/maintenance?game=fh2".to_owned(),
        ),
        (Method::GET, cr::ADMIN_NOTICES.to_owned()),
        (
            Method::GET,
            format!("/core/v1/admin/accounts/{}", admin.account_id),
        ),
        (
            Method::GET,
            format!("/core/v1/admin/accounts?xuid={}", admin.xuid),
        ),
        (Method::GET, cr::ADMIN_REPORTS.to_owned()),
        (Method::GET, cr::ADMIN_AUDIT.to_owned()),
    ] {
        let r = st
            .call(method, &uri, Who::Web(Some(&player.cookie)))
            .send()
            .await;
        assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    }
    let r = st
        .post(
            cr::ADMIN_STRIKES,
            Who::Web(Some(&player.cookie)),
            json!({"xuid": admin.xuid, "source": "manual", "reason": "x"}),
        )
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    assert_eq!(r.body["details"]["role"], "moderator");

    // A MOD may strike and review, idempotently, one audit row per write.
    let before = st.audit_rows(&moderator.account_id).await;
    let key = Uuid::now_v7().to_string();
    let body = json!({"xuid": player.xuid, "source": "manual", "reason": "synthetic speed"});
    let strike = st
        .call(Method::POST, cr::ADMIN_STRIKES, m)
        .header("idempotency-key", &key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(strike.status, StatusCode::CREATED, "{}", strike.body);
    assert_eq!(
        (strike.body["game"].as_str(), strike.body["source"].as_str()),
        (Some("fh2"), Some("manual"))
    );
    let replay = st
        .call(Method::POST, cr::ADMIN_STRIKES, m)
        .header("idempotency-key", &key)
        .json(body)
        .send()
        .await;
    assert_eq!(
        (replay.status, &replay.body["id"]),
        (StatusCode::CREATED, &strike.body["id"])
    );
    let reused = st
        .call(Method::POST, cr::ADMIN_STRIKES, m)
        .header("idempotency-key", &key)
        .json(json!({"xuid": player.xuid, "source": "manual", "reason": "other"}))
        .send()
        .await;
    assert_code(
        &reused,
        StatusCode::CONFLICT,
        "REQUEST.IDEMPOTENCY_KEY_REUSED",
    );
    assert_eq!(
        st.audit_rows(&moderator.account_id).await,
        before + 1,
        "one row, not one per replay"
    );
    let missing_key = st
        .call(Method::POST, cr::ADMIN_STRIKES, m)
        .json(json!({"xuid": player.xuid, "source": "manual", "reason": "x"}))
        .send()
        .await;
    assert_code(&missing_key, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let bad_source = st
        .post(
            cr::ADMIN_STRIKES,
            m,
            json!({"xuid": player.xuid, "source": "save_gate", "reason": "x"}),
        )
        .send()
        .await;
    assert_code(&bad_source, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let unknown = st
        .post(
            cr::ADMIN_STRIKES,
            m,
            json!({"xuid": "2535999999999999", "source": "manual", "reason": "x"}),
        )
        .send()
        .await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND");
    let own = st
        .post(
            cr::ADMIN_STRIKES,
            m,
            json!({"xuid": moderator.xuid, "source": "manual", "reason": "x"}),
        )
        .send()
        .await;
    assert_code(&own, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    let review = st
        .post(
            &format!(
                "/core/v1/admin/strikes/{}/review",
                strike.body["id"].as_str().unwrap()
            ),
            m,
            json!({"outcome": "upheld", "note": "seen the replay"}),
        )
        .send()
        .await;
    assert_eq!(review.status, StatusCode::OK, "{}", review.body);
    assert_eq!(review.body["reviewed"]["outcome"], "upheld");
    assert_eq!(st.audit_rows(&moderator.account_id).await, before + 2);
    // The player sees the strike, and a notice about it.
    let mine = st
        .get(cr::ME_STRIKES, Who::Web(Some(&player.cookie)))
        .send()
        .await;
    assert_eq!(
        mine.body["items"][0]["id"], strike.body["id"],
        "{}",
        mine.body
    );
    let notices = st
        .get(cr::ME_NOTICES, Who::Launcher(Some(&player.access)))
        .send()
        .await;
    assert!(
        notices.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["kind"] == "strike")
    );
    assert_eq!(notices.header("cache-control"), Some("private, no-store"));

    // A MOD may not ban; the ADM may, and the ban revokes everything.
    let ban_body =
        json!({"accountId": player.account_id, "until": null, "reason": "synthetic cheating"});
    let refused = st.post(cr::ADMIN_BANS, m, ban_body.clone()).send().await;
    assert_code(&refused, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    assert_eq!(refused.body["details"]["role"], "admin");
    let session = st.live_session(&player, "active").await;
    let admin_before = st.audit_rows(&admin.account_id).await;
    let banned = st.post(cr::ADMIN_BANS, a, ban_body).send().await;
    assert_eq!(banned.status, StatusCode::CREATED, "{}", banned.body);
    assert_eq!(banned.body["until"], Value::Null);
    let row = sqlx::query_as::<_, (String, bool)>(
        "SELECT state, game_until <= clock_timestamp() FROM play_session WHERE id = $1::uuid",
    )
    .bind(&session)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(row, ("revoked".to_owned(), true));
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM refresh_token WHERE account_id = $1::uuid AND revoked_at IS NULL",
    )
    .bind(&player.account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(live, 0, "launcher families and portal sessions");
    let web = st.get(pr::ME, Who::Web(Some(&player.cookie))).send().await;
    assert_eq!(web.status, StatusCode::UNAUTHORIZED);
    let launcher = st
        .get(lr::ME, Who::Launcher(Some(&player.access)))
        .send()
        .await;
    assert!(
        matches!(launcher.code(), "ACCOUNTS.BANNED" | "AUTH.TOKEN_INVALID"),
        "{}",
        launcher.body
    );
    assert_eq!(st.audit_rows(&admin.account_id).await, admin_before + 1);
    let view = st
        .get(&format!("/core/v1/admin/accounts/{}", player.account_id), m)
        .send()
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert_eq!(view.body["account"]["ban"]["active"], true);
    assert_eq!(
        view.body["account"]["emailMasked"],
        Value::Null,
        "MOD sees no e-mail"
    );
    assert_eq!(view.body["sessions"]["recent"][0]["state"], "revoked");
    assert_eq!(view.body["strikes"].as_array().map(Vec::len), Some(1));
    assert!(
        view.body["audit"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["action"] == "account.ban")
    );
    let view = st
        .get(&format!("/core/v1/admin/accounts/{}", player.account_id), a)
        .send()
        .await;
    assert!(
        view.body["account"]["emailMasked"]
            .as_str()
            .unwrap()
            .contains("***@example.test")
    );
    let unknown = st
        .get(&format!("/core/v1/admin/accounts/{}", Uuid::now_v7()), a)
        .send()
        .await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND");
    let lifted = st
        .call(
            Method::DELETE,
            &format!("/core/v1/admin/bans/{}", player.account_id),
            a,
        )
        .key()
        .send()
        .await;
    assert_eq!(lifted.status, StatusCode::NO_CONTENT);
    let past = st
        .post(cr::ADMIN_BANS, a, json!({"accountId": player.account_id, "until": "2020-01-01T00:00:00.000000Z", "reason": "x"}))
        .send()
        .await;
    assert_code(&past, StatusCode::BAD_REQUEST, "REQUEST.INVALID");

    // Roles: ADM only, never one's own.
    let own = st
        .call(
            Method::PUT,
            &format!("/core/v1/admin/accounts/{}/role", admin.account_id),
            a,
        )
        .key()
        .json(json!({"role": "player"}))
        .send()
        .await;
    assert_code(&own, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    let by_mod = st
        .call(
            Method::PUT,
            &format!("/core/v1/admin/accounts/{}/role", player.account_id),
            m,
        )
        .key()
        .json(json!({"role": "moderator"}))
        .send()
        .await;
    assert_code(&by_mod, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    let promoted = st
        .call(
            Method::PUT,
            &format!("/core/v1/admin/accounts/{}/role", player.account_id),
            a,
        )
        .key()
        .json(json!({"role": "moderator"}))
        .send()
        .await;
    assert_eq!(promoted.status, StatusCode::OK, "{}", promoted.body);
    assert_eq!(
        (
            promoted.body["role"].as_str(),
            promoted.body["previous"].as_str()
        ),
        (Some("moderator"), Some("player"))
    );
    let bad = st
        .call(
            Method::PUT,
            &format!("/core/v1/admin/accounts/{}/role", player.account_id),
            a,
        )
        .key()
        .json(json!({"role": "owner"}))
        .send()
        .await;
    assert_code(&bad, StatusCode::BAD_REQUEST, "REQUEST.INVALID");

    // Lookup: MOD by gamertag and XUID, never by e-mail; ADM by e-mail too.
    let by_tag = st
        .get(
            &format!(
                "/core/v1/admin/accounts?gamertag={}",
                player.gamertag.replace('#', "%23")
            ),
            m,
        )
        .send()
        .await;
    assert_eq!(
        by_tag.body["items"][0]["accountId"].as_str(),
        Some(player.account_id.as_str()),
        "{}",
        by_tag.body
    );
    let by_prefix = st
        .get(
            &format!("/core/v1/admin/accounts?gamertag={}", player.name),
            m,
        )
        .send()
        .await;
    assert_eq!(
        by_prefix.body["items"][0]["xuid"].as_str(),
        Some(player.xuid.as_str())
    );
    let by_mail = st
        .get("/core/v1/admin/accounts?email=a@example.test", m)
        .send()
        .await;
    assert_code(&by_mail, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    let by_mail = st
        .get("/core/v1/admin/accounts?email=nobody@example.test", a)
        .send()
        .await;
    assert_eq!(by_mail.status, StatusCode::OK);
    let both = st
        .get(
            &format!("/core/v1/admin/accounts?xuid={}&gamertag=x", player.xuid),
            m,
        )
        .send()
        .await;
    assert_code(&both, StatusCode::BAD_REQUEST, "REQUEST.INVALID");

    // Erasure (ADM): the forced 14-day deletion.
    let target = st.player("noob").await;
    let erased = st
        .call(
            Method::POST,
            &format!("/core/v1/admin/accounts/{}/erase", target.account_id),
            a,
        )
        .key()
        .send()
        .await;
    assert_eq!(erased.status, StatusCode::ACCEPTED, "{}", erased.body);
    assert!(erased.body["purgeAt"].is_string());
    let gone = st.get(pr::ME, Who::Web(Some(&target.cookie))).send().await;
    assert_eq!(
        gone.status,
        StatusCode::UNAUTHORIZED,
        "a forced erasure signs every session out"
    );
    let view = st
        .get(&format!("/core/v1/admin/accounts/{}", target.account_id), a)
        .send()
        .await;
    assert_eq!(
        view.body["account"]["deletion"]["purgeAt"],
        erased.body["purgeAt"]
    );
    assert_eq!(view.body["account"]["status"], "deletion_pending");

    // The audit feed pages by id.
    let feed = st
        .get(
            &format!("{}?actor={}", cr::ADMIN_AUDIT, admin.account_id),
            m,
        )
        .send()
        .await;
    let items = feed.body["items"].as_array().unwrap();
    assert!(items.len() >= 4, "{}", feed.body);
    assert!(
        items
            .iter()
            .all(|r| r["actor"]["accountId"] == admin.account_id.as_str())
    );
    assert!(items.iter().all(|r| r["actor"]["role"] == "admin"));
    let target_feed = st
        .get(
            &format!("{}?target=account:{}", cr::ADMIN_AUDIT, player.account_id),
            m,
        )
        .send()
        .await;
    assert!(target_feed.body["items"].as_array().unwrap().len() >= 2);
    let bad_cursor = st
        .get(&format!("{}?cursor=x", cr::ADMIN_AUDIT), m)
        .send()
        .await;
    assert_code(&bad_cursor, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
}

/// CR-S62: `POST /core/v1/admin/kicks`, every status and code: MOD and ADM kick players (by XUID
/// or account), live sessions end `revoked` / `kicked` without a ban or a revoked family, the
/// cooldown is reported, the player gets a `kicked` notice, one `account.kick` row per write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn moderators_kick_players_without_a_ban() {
    let st = stack().await;
    let player = st.player("hardcore").await;
    let moderator = st.player("noob").await;
    let other_mod = st.player("noob").await;
    let admin = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    st.set_role(&other_mod.account_id, "moderator").await;
    st.set_role(&admin.account_id, "admin").await;
    let m = Who::Web(Some(&moderator.cookie));
    let a = Who::Launcher(Some(&admin.access));

    // A player: AUTH.ROLE_REQUIRED {role: moderator}.
    let r = st
        .post(
            cr::ADMIN_KICKS,
            Who::Launcher(Some(&player.access)),
            json!({"accountId": admin.account_id, "reason": "x"}),
        )
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    assert_eq!(r.body["details"]["role"], "moderator");

    // 400 REQUEST.INVALID {field} for every bad body.
    for (body, field) in [
        (json!({"reason": "x"}), "xuid"),
        (
            json!({"xuid": player.xuid, "accountId": player.account_id, "reason": "x"}),
            "accountId",
        ),
        (json!({"xuid": "12x", "reason": "x"}), "xuid"),
        (json!({"xuid": "0", "reason": "x"}), "xuid"),
        (
            json!({"accountId": "not-a-uuid", "reason": "x"}),
            "accountId",
        ),
        (json!({"xuid": player.xuid, "reason": ""}), "reason"),
        (
            json!({"xuid": player.xuid, "reason": "x".repeat(501)}),
            "reason",
        ),
        (
            json!({"xuid": player.xuid, "reason": "x", "cooldownMinutes": 61}),
            "cooldownMinutes",
        ),
    ] {
        let r = st.post(cr::ADMIN_KICKS, m, body.clone()).send().await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], field, "{body}");
    }
    for (body, field) in [
        (
            json!({"xuid": player.xuid, "reason": "x", "cooldownMinutes": -1}),
            "cooldownMinutes",
        ),
        (
            json!({"xuid": player.xuid, "reason": "x", "extra": 1}),
            "extra",
        ),
    ] {
        let r = st.post(cr::ADMIN_KICKS, m, body).send().await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], field);
    }
    // 404 PORTAL.NOT_FOUND: an unknown XUID or account.
    for body in [
        json!({"xuid": "2535999999999999", "reason": "x"}),
        json!({"accountId": Uuid::now_v7(), "reason": "x"}),
    ] {
        let r = st.post(cr::ADMIN_KICKS, m, body).send().await;
        assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    }
    // 403 AUTH.FORBIDDEN: oneself (by account or by one's own XUID), a MOD kicking staff.
    for body in [
        json!({"accountId": moderator.account_id, "reason": "x"}),
        json!({"xuid": moderator.xuid, "reason": "x"}),
        json!({"accountId": other_mod.account_id, "reason": "x"}),
        json!({"xuid": admin.xuid, "reason": "x"}),
    ] {
        let r = st.post(cr::ADMIN_KICKS, m, body.clone()).send().await;
        assert_code(&r, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    }
    assert_eq!(
        st.audit_rows(&moderator.account_id).await,
        0,
        "refusals write nothing"
    );

    // A MOD kicks a player by XUID: its live session ends, no ban, the families stay.
    let active = st.live_session(&player, "active").await;
    let families = |account: String| {
        let owner = st.owner.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM refresh_token WHERE account_id = $1::uuid AND revoked_at IS NULL",
            )
            .bind(account)
            .fetch_one(&owner)
            .await
            .unwrap()
        }
    };
    let live_before = families(player.account_id.clone()).await;
    assert!(live_before >= 2, "launcher family and web session");
    let key = Uuid::now_v7().to_string();
    let body = json!({"xuid": player.xuid, "reason": "synthetic griefing", "cooldownMinutes": 10});
    let kicked = st
        .call(Method::POST, cr::ADMIN_KICKS, m)
        .header("idempotency-key", &key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(kicked.status, StatusCode::OK, "{}", kicked.body);
    assert_eq!(kicked.body["accountId"], player.account_id.as_str());
    assert_eq!(kicked.body["sessionsEnded"], 1);
    assert!(kicked.body["until"].is_string(), "{}", kicked.body);
    let replay = st
        .call(Method::POST, cr::ADMIN_KICKS, m)
        .header("idempotency-key", &key)
        .json(body)
        .send()
        .await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::OK, &kicked.body)
    );
    assert_eq!(
        st.audit_rows(&moderator.account_id).await,
        1,
        "one row, not one per replay"
    );
    let rows = sqlx::query_as::<_, (String, String, bool)>(
        "SELECT state, end_reason, game_until <= clock_timestamp() FROM play_session \
         WHERE id::text = ANY($1) ORDER BY id",
    )
    .bind(vec![active.clone()])
    .fetch_all(&st.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![("revoked".to_owned(), "kicked".to_owned(), true)]
    );
    assert_eq!(families(player.account_id.clone()).await, live_before);
    let me = st
        .get(lr::ME, Who::Launcher(Some(&player.access)))
        .send()
        .await;
    assert_eq!(me.status, StatusCode::OK, "still signed in: {}", me.body);
    let audit = sqlx::query_as::<_, (String, Option<String>, Value, String)>(
        "SELECT target, reason, detail, actor_role FROM admin_action \
         WHERE actor_account_id = $1::uuid AND action = 'account.kick'",
    )
    .bind(&moderator.account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(
        audit,
        (
            format!("account:{}", player.account_id),
            Some("synthetic griefing".to_owned()),
            json!({"sessionsEnded": 1, "cooldownMinutes": 10}),
            "moderator".to_owned()
        )
    );
    let view = st
        .get(&format!("/core/v1/admin/accounts/{}", player.account_id), m)
        .send()
        .await;
    assert_eq!(
        view.body["account"]["ban"]["active"],
        Value::Null,
        "{}",
        view.body
    );
    // The player's notice: no reason, the cooldown's end.
    let notices = st
        .get(cr::ME_NOTICES, Who::Web(Some(&player.cookie)))
        .send()
        .await;
    let notice = notices.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "kicked")
        .cloned()
        .expect("a kicked notice");
    assert_eq!(notice["level"], "warning");
    assert!(!notice["text"].as_str().unwrap().contains("griefing"));

    // No live session: still 200 with sessionsEnded 0; an ADM kicks a MOD by account.
    let r = st
        .post(
            cr::ADMIN_KICKS,
            a,
            json!({"accountId": other_mod.account_id, "reason": "synthetic"}),
        )
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        (r.body["sessionsEnded"].as_u64(), &r.body["until"]),
        (Some(0), &Value::Null)
    );
    // 403 AUTH.FORBIDDEN for the ADM's own account.
    let r = st
        .post(
            cr::ADMIN_KICKS,
            a,
            json!({"accountId": admin.account_id, "reason": "x"}),
        )
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn maintenance_and_the_notice_board() {
    let st = stack().await;
    let admin = st.player("noob").await;
    let moderator = st.player("noob").await;
    st.set_role(&admin.account_id, "admin").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let a = Who::Web(Some(&admin.cookie));
    // A game of this run, so parallel tests never see its switch.
    let game = format!("t{}", &fresh()[18..]);
    sqlx::query("INSERT INTO game (code, title) VALUES ($1, 'Synthetic test game')")
        .bind(&game)
        .execute(&st.owner)
        .await
        .unwrap();
    let get = st
        .get(&format!("/core/v1/admin/maintenance?game={game}"), a)
        .send()
        .await;
    assert_eq!(
        (get.status, &get.body["until"]),
        (StatusCode::OK, &Value::Null),
        "{}",
        get.body
    );
    let by_mod = st
        .get(
            &format!("/core/v1/admin/maintenance?game={game}"),
            Who::Web(Some(&moderator.cookie)),
        )
        .send()
        .await;
    assert_code(&by_mod, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    let until = fi_protocol::instant::format_unix_secs(
        fi_domain::UnixMs::now_system().millis() / 1000 + 3600,
    )
    .unwrap();
    let before = st.audit_rows(&admin.account_id).await;
    let set = st
        .call(Method::PUT, cr::ADMIN_MAINTENANCE, a)
        .key()
        .json(json!({"game": game, "until": until, "message": "Synthetic upgrade"}))
        .send()
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(set.body["message"], "Synthetic upgrade");
    assert_eq!(st.audit_rows(&admin.account_id).await, before + 1);
    let status = st.get(cr::STATUS, Who::Bare).send().await;
    assert_eq!(status.body["status"], "maintenance");
    let g = status.body["games"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == game.as_str())
        .cloned()
        .unwrap();
    assert_eq!(g["accepting"], false);
    assert!(g["maintenanceUntil"].is_string());
    let board = st
        .get(
            &format!("{}?game={game}&lang=pt-BR", cr::NOTICES),
            Who::Bare,
        )
        .send()
        .await;
    assert_eq!(
        board.body["items"][0]["kind"], "maintenance",
        "{}",
        board.body
    );
    assert_eq!(board.body["items"][0]["title"], "Manutenção");
    assert_eq!(board.body["items"][0]["body"], "Synthetic upgrade");
    let info = st.get(lr::INFO, Who::Bare).send().await;
    let gi = info.body["games"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == game.as_str())
        .cloned()
        .unwrap();
    assert_eq!(gi["accepting"], false);
    let lift = st
        .call(Method::PUT, cr::ADMIN_MAINTENANCE, a)
        .key()
        .json(json!({"game": game, "until": null}))
        .send()
        .await;
    assert_eq!(
        (lift.status, &lift.body["until"]),
        (StatusCode::OK, &Value::Null)
    );
    let unknown = st
        .call(Method::PUT, cr::ADMIN_MAINTENANCE, a)
        .key()
        .json(json!({"game": "zz404", "until": null}))
        .send()
        .await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");

    // Online capacity (OD-71): ADM reads and sets it, audited; a moderator cannot; bad values and
    // unknown games are refused; null is unlimited.
    let cap = st
        .get(&format!("/core/v1/admin/capacity?game={game}"), a)
        .send()
        .await;
    assert_eq!(cap.status, StatusCode::OK, "{}", cap.body);
    assert_eq!(
        (
            &cap.body["onlineCapacity"],
            &cap.body["active"],
            &cap.body["queued"]
        ),
        (&Value::Null, &json!(0), &json!(0))
    );
    let by_mod = st
        .call(
            Method::PUT,
            cr::ADMIN_CAPACITY,
            Who::Web(Some(&moderator.cookie)),
        )
        .key()
        .json(json!({"game": game, "onlineCapacity": 50}))
        .send()
        .await;
    assert_code(&by_mod, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    let before = st.audit_rows(&admin.account_id).await;
    let set = st
        .call(Method::PUT, cr::ADMIN_CAPACITY, a)
        .key()
        .json(json!({"game": game, "onlineCapacity": 50}))
        .send()
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(set.body["onlineCapacity"], 50);
    assert_eq!(st.audit_rows(&admin.account_id).await, before + 1);
    for bad in [json!(0), json!(100_001), json!("50")] {
        let refused = st
            .call(Method::PUT, cr::ADMIN_CAPACITY, a)
            .key()
            .json(json!({"game": game, "onlineCapacity": bad}))
            .send()
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{bad}: {}",
            refused.body
        );
    }
    let unlimited = st
        .call(Method::PUT, cr::ADMIN_CAPACITY, a)
        .key()
        .json(json!({"game": game, "onlineCapacity": null}))
        .send()
        .await;
    assert_eq!(
        (unlimited.status, &unlimited.body["onlineCapacity"]),
        (StatusCode::OK, &Value::Null)
    );
    let unknown = st
        .call(Method::PUT, cr::ADMIN_CAPACITY, a)
        .key()
        .json(json!({"game": "zz404", "onlineCapacity": 10}))
        .send()
        .await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");

    // Notices: ADM writes, the public board reads, cached.
    let created = st
        .post(
            cr::ADMIN_NOTICES,
            a,
            json!({"game": game, "kind": "news", "level": "info", "title": "Season one", "body": "Synthetic **news**.", "lang": "en"}),
        )
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let platform = st
        .post(
            cr::ADMIN_NOTICES,
            a,
            json!({"game": null, "kind": "event", "level": "info", "title": "Platform-wide", "body": "Everyone.", "lang": "en"}),
        )
        .send()
        .await;
    assert_eq!(platform.status, StatusCode::CREATED);
    let board = st
        .get(&format!("{}?game={game}", cr::NOTICES), Who::Bare)
        .send()
        .await;
    assert_eq!(board.status, StatusCode::OK);
    assert_eq!(
        board.header("cache-control"),
        Some("public, max-age=60, s-maxage=60")
    );
    let ids: Vec<&Value> = board.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| &n["id"])
        .collect();
    assert!(ids.contains(&&created.body["id"]) && ids.contains(&&platform.body["id"]));
    for bad in [
        json!({"game": game, "kind": "maintenance", "level": "info", "title": "x", "body": "x", "lang": "en"}),
        json!({"game": game, "kind": "news", "level": "info", "title": "", "body": "x", "lang": "en"}),
        json!({"game": game, "kind": "news", "level": "info", "title": "x", "body": "x".repeat(4097), "lang": "en"}),
        json!({"game": game, "kind": "news", "level": "info", "title": "x", "body": "x", "lang": "fr"}),
        json!({"game": game, "kind": "news", "level": "loud", "title": "x", "body": "x", "lang": "en"}),
    ] {
        let r = st.post(cr::ADMIN_NOTICES, a, bad).send().await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    }
    let listed = st.get(cr::ADMIN_NOTICES, a).send().await;
    assert!(
        listed.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == created.body["id"])
    );
    let uri = format!(
        "/core/v1/admin/notices/{}",
        created.body["id"].as_str().unwrap()
    );
    let deleted = st.call(Method::DELETE, &uri, a).key().send().await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let twice = st.call(Method::DELETE, &uri, a).key().send().await;
    assert_code(&twice, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let platform_uri = format!(
        "/core/v1/admin/notices/{}",
        platform.body["id"].as_str().unwrap()
    );
    assert_eq!(
        st.call(Method::DELETE, &platform_uri, a)
            .key()
            .send()
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    let missing = st.get(cr::NOTICES, Who::Bare).send().await;
    assert_code(&missing, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let unknown = st
        .get(&format!("{}?game=zz404", cr::NOTICES), Who::Bare)
        .send()
        .await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
}

// ---------------------------------------------------------------------------------------------
// Rate limits and the edge lock.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_eleventh_public_write_per_minute_is_429_and_health_is_never_limited() {
    let st = stack_with(Options {
        rates: Rates::default(),
        ..Options::default()
    })
    .await;
    for i in 0..10 {
        let a = st
            .post(pr::SESSIONS, Who::Web(None), json!({"idToken": "x"}))
            .send()
            .await;
        assert_code(&a, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_REQUIRED");
        assert_ne!(a.status, StatusCode::TOO_MANY_REQUESTS, "write {i}");
    }
    let eleventh = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": "x"}))
        .send()
        .await;
    assert_code(
        &eleventh,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );
    let retry: u64 = eleventh.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=6).contains(&retry), "{retry}");
    assert_eq!(
        eleventh.header("access-control-allow-origin"),
        Some(WEB),
        "the web can read the 429"
    );
    // Another network still writes; reads have their own bucket.
    let other = st
        .post(pr::SESSIONS, Who::Bare, json!({"idToken": "x"}))
        .header("x-forwarded-for", "192.0.2.10")
        .header("origin", WEB)
        .header("x-fi-client", "web/0.1.0")
        .send()
        .await;
    assert_code(&other, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_REQUIRED");
    assert_eq!(
        st.get(cr::STATUS, Who::Bare).send().await.status,
        StatusCode::OK
    );
    for _ in 0..100 {
        for uri in ["/health/live", "/health/ready"] {
            let h = st.get(uri, Who::Bare).send().await;
            assert_eq!(h.status, StatusCode::OK, "{uri}");
        }
    }
    // 61 public reads: the 61st is limited.
    for _ in 0..59 {
        st.get(&format!("{}?game=fh2", cr::SOCIAL_PLAYERS), Who::Bare)
            .send()
            .await;
    }
    let limited = st
        .get(&format!("{}?game=fh2", cr::SOCIAL_PLAYERS), Who::Bare)
        .send()
        .await;
    assert_code(
        &limited,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );
    // The per-account write bucket (30/min) holds across networks.
    let p = {
        let roomy = stack().await;
        roomy.player("noob").await
    };
    let mut codes = Vec::new();
    for i in 0..31 {
        let a = st
            .post(
                cr::SOCIAL_REPORTS,
                Who::Launcher(Some(&p.access)),
                json!({"target": "player:2535100000000001", "reason": "other"}),
            )
            .header("x-forwarded-for", &format!("198.51.{}.1", i))
            .send()
            .await;
        codes.push(a.code().to_owned());
    }
    assert!(
        codes[..30].iter().all(|c| c == "AUTH.CHALLENGE_REQUIRED"),
        "{codes:?}"
    );
    assert_eq!(codes[30], "REQUEST.RATE_LIMITED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn requests_without_the_edge_secret_are_refused_except_health() {
    let secret = format!("edge-{}", "s".repeat(40));
    let st = stack_with(Options {
        vars: vec![("FI_EDGE_SECRET", format!("{secret},{}", "o".repeat(32)))],
        rates: Rates::default(),
        ..Options::default()
    })
    .await;
    let bare = st.get(cr::STATUS, Who::Bare).send().await;
    assert_code(&bare, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    let wrong = st
        .get(cr::STATUS, Who::Bare)
        .header("x-fi-edge", "nope")
        .send()
        .await;
    assert_code(&wrong, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    let ok = st
        .get(cr::STATUS, Who::Bare)
        .header("x-fi-edge", &secret)
        .send()
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    let rotated = st
        .get(cr::STATUS, Who::Bare)
        .header("x-fi-edge", &"o".repeat(32))
        .send()
        .await;
    assert_eq!(
        rotated.status,
        StatusCode::OK,
        "the previous value during a rotation"
    );
    for uri in ["/health/live", "/health/ready"] {
        assert_eq!(st.get(uri, Who::Bare).send().await.status, StatusCode::OK);
    }
    // Behind the edge the client is CF-Connecting-IP, not X-Forwarded-For.
    for i in 0..10 {
        let a = st
            .post(pr::SESSIONS, Who::Web(None), json!({"idToken": "x"}))
            .header("x-fi-edge", &secret)
            .header("cf-connecting-ip", "203.0.113.77")
            .header("x-forwarded-for", &format!("198.51.100.{i}"))
            .send()
            .await;
        assert_code(&a, StatusCode::FORBIDDEN, "AUTH.CHALLENGE_REQUIRED");
    }
    let limited = st
        .post(pr::SESSIONS, Who::Web(None), json!({"idToken": "x"}))
        .header("x-fi-edge", &secret)
        .header("cf-connecting-ip", "203.0.113.77")
        .header("x-forwarded-for", "198.51.100.200")
        .send()
        .await;
    assert_code(
        &limited,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );
}

// ---------------------------------------------------------------------------------------------
// MCP.
// ---------------------------------------------------------------------------------------------

async fn mcp_connection(st: &Stack, account: &str, role: &str) -> String {
    sqlx::query(
        "INSERT INTO mcp_client (id, name, redirect_uris) VALUES ('core-e2e', 'Core e2e client', ARRAY['http://127.0.0.1:33418/callback']) ON CONFLICT DO NOTHING",
    )
    .execute(&st.owner)
    .await
    .unwrap();
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO mcp_connection (account_id, client_id, role, scopes, expires_at) \
         VALUES ($1::uuid, 'core-e2e', $2, ARRAY['platform:read'], clock_timestamp() + interval '30 days') RETURNING id",
    )
    .bind(account)
    .bind(role)
    .fetch_one(&st.owner)
    .await
    .unwrap()
    .to_string()
}

fn mcp_token(account: &str, role: &str, connection: &str) -> String {
    let keys = fi_auth::keys::SigningKeys::parse("FI_MCP_SIGNING_KEYS", MCP_KEYS).unwrap();
    let now = fi_domain::UnixMs::now_system().millis() / 1_000;
    fi_auth::mcp::issue(
        &keys,
        &fi_auth::mcp::Grant {
            account_id: account,
            role,
            scopes: &["platform:read"],
            connection_id: connection,
            token_id: &fresh(),
        },
        now,
    )
    .unwrap()
    .token
    .into_inner()
}

fn rpc(method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_mcp_skeleton_authenticates_lists_and_refuses_calls() {
    let off = stack().await;
    let pointer = "Bearer resource_metadata=\"http://127.0.0.1:8480/.well-known/oauth-protected-resource/mcp\"";
    let r = off
        .call(Method::POST, cr::MCP, Who::Bare)
        .json(rpc("initialize"))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(
        r.header("www-authenticate").unwrap().starts_with(pointer),
        "{:?}",
        r.header("www-authenticate")
    );
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(
        off.get(cr::MCP_RESOURCE_METADATA, Who::Bare)
            .send()
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let st = stack_with(Options {
        vars: vec![
            ("FI_MCP_ENABLED", "true".into()),
            ("FI_MCP_SIGNING_KEYS", MCP_KEYS.into()),
        ],
        ..Options::default()
    })
    .await;
    let r = st
        .call(Method::POST, cr::MCP, Who::Bare)
        .json(rpc("initialize"))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(
        !r.header("www-authenticate")
            .unwrap()
            .contains("invalid_token")
    );
    let r = st
        .call(Method::POST, cr::MCP, Who::Bare)
        .header("authorization", "Bearer garbage")
        .json(rpc("initialize"))
        .send()
        .await;
    assert!(
        r.header("www-authenticate")
            .unwrap()
            .contains("error=\"invalid_token\"")
    );

    // Discovery documents (RFC 9728, RFC 8414 required members).
    let prm = st.get(cr::MCP_RESOURCE_METADATA, Who::Bare).send().await;
    assert_eq!(prm.status, StatusCode::OK);
    assert_eq!(prm.body["resource"], "http://127.0.0.1:8480/mcp");
    assert_eq!(
        prm.body["authorization_servers"],
        json!(["http://127.0.0.1:8480/mcp/oauth"])
    );
    assert_eq!(prm.body["bearer_methods_supported"], json!(["header"]));
    let asm = st.get(cr::MCP_AUTH_METADATA, Who::Bare).send().await;
    assert_eq!(asm.body["issuer"], "http://127.0.0.1:8480/mcp/oauth");
    for member in [
        "authorization_endpoint",
        "token_endpoint",
        "jwks_uri",
        "registration_endpoint",
    ] {
        assert!(
            asm.body[member]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:8480/mcp/oauth/"),
            "{member}"
        );
    }
    assert_eq!(asm.body["response_types_supported"], json!(["code"]));
    assert_eq!(
        asm.body["code_challenge_methods_supported"],
        json!(["S256"])
    );

    // A moderator's connection.
    let moderator = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let connection = mcp_connection(&st, &moderator.account_id, "moderator").await;
    let token = mcp_token(&moderator.account_id, "moderator", &connection);
    let bearer = format!("Bearer {token}");
    let call = |body: Value| {
        st.call(Method::POST, cr::MCP, Who::Bare)
            .header("authorization", &bearer)
            .json(body)
    };
    let init = call(rpc("initialize")).send().await;
    assert_eq!(init.status, StatusCode::OK, "{}", init.body);
    assert_eq!(init.body["result"]["protocolVersion"], "2025-06-18");
    assert!(init.body["result"]["capabilities"]["tools"].is_object());
    let note = call(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .await;
    assert_eq!(note.status, StatusCode::ACCEPTED);
    let list = call(rpc("tools/list")).send().await;
    let names: Vec<&str> = list.body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, crate::mcp::READ_TOOLS);
    let called = call(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "platform_status", "arguments": {}}})).send().await;
    assert_eq!(called.status, StatusCode::OK);
    assert_eq!(called.body["result"]["isError"], true);
    assert_eq!(
        called.body["result"]["structuredContent"]["code"],
        "AUTH.FORBIDDEN"
    );
    let batch = call(json!([rpc("tools/list")])).send().await;
    assert_eq!(batch.status, StatusCode::BAD_REQUEST);
    let unknown = call(rpc("resources/list")).send().await;
    assert_eq!(unknown.body["error"]["code"], -32601);
    let big = st
        .call(Method::POST, cr::MCP, Who::Bare)
        .header("authorization", &bearer)
        .json(json!({"jsonrpc": "2.0", "id": 3, "method": "ping", "params": {"pad": "x".repeat(40_000)}}))
        .send()
        .await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE);
    let get = st
        .get(cr::MCP, Who::Bare)
        .header("authorization", &bearer)
        .send()
        .await;
    assert_eq!(get.status, StatusCode::METHOD_NOT_ALLOWED);
    // A launcher token, a demoted moderator, a revoked connection: 401.
    let launcher = st
        .call(
            Method::POST,
            cr::MCP,
            Who::Launcher(Some(&moderator.access)),
        )
        .json(rpc("initialize"))
        .send()
        .await;
    assert_eq!(launcher.status, StatusCode::UNAUTHORIZED);
    st.set_role(&moderator.account_id, "player").await;
    assert_eq!(
        call(rpc("tools/list")).send().await.status,
        StatusCode::UNAUTHORIZED
    );
    st.set_role(&moderator.account_id, "moderator").await;
    assert_eq!(call(rpc("tools/list")).send().await.status, StatusCode::OK);
    sqlx::query("UPDATE mcp_connection SET revoked_at = clock_timestamp(), revoke_reason = 'player' WHERE id = $1::uuid")
        .bind(&connection)
        .execute(&st.owner)
        .await
        .unwrap();
    assert_eq!(
        call(rpc("tools/list")).send().await.status,
        StatusCode::UNAUTHORIZED
    );
}

// ---------------------------------------------------------------------------------------------
// Public server details, tickets, the social hub.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_game_ticket_never_opens_core_and_public_answers_are_cacheable() {
    let st = stack().await;
    let p = st.player("noob").await;
    let ticket = game_ticket(&st);
    for uri in [
        cr::ME_NOTICES.to_owned(),
        cr::ME_STRIKES.to_owned(),
        cr::ADMIN_AUDIT.to_owned(),
        format!("{}?game=fh2", cr::ADMIN_ASSESSMENTS),
        pr::ME.to_owned(),
        lr::ME.to_owned(),
        cr::STATUS.to_owned(),
        format!("/core/v1/social/players/{}", p.xuid),
    ] {
        let a = st.get(&uri, Who::Launcher(Some(&ticket))).send().await;
        assert_code(&a, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    }
    let w = st
        .post(
            cr::SOCIAL_REPORTS,
            Who::Launcher(Some(&ticket)),
            json!({"target": format!("player:{}", p.xuid), "reason": "other"}),
        )
        .send()
        .await;
    assert_code(&w, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");

    let status = st.get(cr::STATUS, Who::Bare).send().await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.body);
    assert_eq!(
        status.header("cache-control"),
        Some("public, max-age=10, s-maxage=10")
    );
    assert_eq!(status.body["stats"], Value::Null, "no gold snapshot yet");
    assert_eq!(status.body["services"]["gate"], "unknown");
    assert!(
        status.body["games"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == "fh2")
    );
    let live = st.live_session(&p, "queued").await;
    let status = st.get(cr::STATUS, Who::Bare).send().await;
    let fh2 = status.body["games"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "fh2")
        .cloned()
        .unwrap();
    assert!(
        fh2["queue"]
            .as_object()
            .unwrap()
            .values()
            .any(|q| q["queued"].as_u64() >= Some(1)),
        "{fh2}"
    );
    let mine = st
        .get(cr::ME_NOTICES, Who::Web(Some(&p.cookie)))
        .send()
        .await;
    let queue = mine.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "queue")
        .cloned()
        .unwrap();
    assert_eq!(
        (queue["position"].as_u64(), queue["game"].as_str()),
        (Some(1), Some("fh2")),
        "{live}"
    );
    let notices = st
        .get(&format!("{}?game=fh2", cr::NOTICES), Who::Bare)
        .send()
        .await;
    assert!(
        notices
            .header("cache-control")
            .unwrap()
            .starts_with("public")
    );
    let support = st
        .get(cr::ME_SUPPORT, Who::Web(Some(&p.cookie)))
        .send()
        .await;
    assert_eq!(support.status, StatusCode::OK);
    assert!(
        support.body["links"]["help"]
            .as_str()
            .unwrap()
            .starts_with(WEB)
    );
    assert_eq!(support.body["reportToken"], Value::Null);
    let bad = st
        .get("/core/v1/me/strikes?game=FH2", Who::Web(Some(&p.cookie)))
        .send()
        .await;
    assert_code(&bad, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    // A probed service that does not answer makes the status degraded.
    let probed = stack_with(Options {
        vars: vec![(
            "FI_PROBE_GATE_URL",
            "http://127.0.0.1:9/health/ready".into(),
        )],
        ..Options::default()
    })
    .await;
    let s = probed.get(cr::STATUS, Who::Bare).send().await;
    assert_eq!(s.body["services"]["gate"], "down");
    assert!(matches!(
        s.body["status"].as_str(),
        Some("degraded" | "maintenance")
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_social_hub_opens_cards_and_comments_to_every_account() {
    let st = stack().await;
    let free = st.player("noob").await;
    let supporter = st.player("noob").await;
    let hardcore = st.player("hardcore").await;
    st.make_supporter(&supporter.account_id).await;
    let target = st.player("noob").await;

    // Directory and cards: public, basic for everyone.
    let dir = st
        .get(
            &format!("{}?game=fh2&q={}", cr::SOCIAL_PLAYERS, target.name),
            Who::Bare,
        )
        .send()
        .await;
    assert_eq!(dir.status, StatusCode::OK, "{}", dir.body);
    assert_eq!(
        dir.body["items"][0]["xuid"].as_str(),
        Some(target.xuid.as_str())
    );
    assert!(dir.body["items"][0].get("accountId").is_none());
    assert_eq!(
        dir.header("cache-control"),
        Some("public, max-age=30, s-maxage=30")
    );
    let page = st
        .get(&format!("{}?game=fh2", cr::SOCIAL_PLAYERS), Who::Bare)
        .send()
        .await;
    if let Some(next) = page.body["next"].as_str() {
        let more = st
            .get(
                &format!("{}?game=fh2&cursor={next}", cr::SOCIAL_PLAYERS),
                Who::Bare,
            )
            .send()
            .await;
        assert_eq!(more.status, StatusCode::OK);
    }
    let bad = st
        .get(
            &format!("{}?game=fh2&q=a%25", cr::SOCIAL_PLAYERS),
            Who::Bare,
        )
        .send()
        .await;
    assert_code(&bad, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let card_uri = format!("/core/v1/social/players/{}", target.xuid);
    let anonymous = st.get(&card_uri, Who::Bare).send().await;
    assert_eq!(
        (anonymous.status, anonymous.body.get("full")),
        (StatusCode::OK, None)
    );
    assert_eq!(anonymous.body["tier"], "free");
    // OD-129: every signed-in viewer gets the full card (it was a Supporter's).
    let as_free = st.get(&card_uri, Who::Web(Some(&free.cookie))).send().await;
    assert!(as_free.body["full"].is_object(), "{}", as_free.body);
    assert_eq!(as_free.header("cache-control"), Some("private, no-store"));
    let full = st
        .get(&card_uri, Who::Web(Some(&supporter.cookie)))
        .send()
        .await;
    assert!(full.body["full"].is_object(), "{}", full.body);
    let stale = st
        .get(&card_uri, Who::Web(Some("fi_ps_stale")))
        .send()
        .await;
    assert_eq!(
        stale.status,
        StatusCode::OK,
        "a stale cookie reads the public card"
    );
    let unknown = st
        .get("/core/v1/social/players/2535999999999999", Who::Bare)
        .send()
        .await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let malformed = st
        .get("/core/v1/social/players/abc", Who::Bare)
        .send()
        .await;
    assert_code(&malformed, StatusCode::BAD_REQUEST, "REQUEST.INVALID");

    // Comments: every account's (OD-129; they were Supporters' only), challenged,
    // mode-isolated, idempotent.
    let body = json!({"target": format!("player:{}", target.xuid), "body": "Synthetic GG"});
    let as_free = st
        .post(
            cr::SOCIAL_COMMENTS,
            Who::Web(Some(&free.cookie)),
            json!({"target": format!("player:{}", target.xuid), "body": "Synthetic free GG"}),
        )
        .challenge()
        .send()
        .await;
    assert_eq!(as_free.status, StatusCode::CREATED, "{}", as_free.body);
    let free_comment = st
        .call(
            Method::DELETE,
            &format!(
                "/core/v1/social/comments/{}",
                as_free.body["id"].as_str().unwrap()
            ),
            Who::Web(Some(&free.cookie)),
        )
        .send()
        .await;
    assert_eq!(free_comment.status, StatusCode::NO_CONTENT);
    let key = Uuid::now_v7().to_string();
    let posted = st
        .call(
            Method::POST,
            cr::SOCIAL_COMMENTS,
            Who::Web(Some(&supporter.cookie)),
        )
        .header("idempotency-key", &key)
        .json(body.clone())
        .challenge()
        .send()
        .await;
    assert_eq!(posted.status, StatusCode::CREATED, "{}", posted.body);
    assert_eq!(
        posted.body["author"]["gamertag"].as_str(),
        Some(supporter.gamertag.as_str())
    );
    let replay = st
        .call(
            Method::POST,
            cr::SOCIAL_COMMENTS,
            Who::Web(Some(&supporter.cookie)),
        )
        .header("idempotency-key", &key)
        .json(body)
        .challenge()
        .send()
        .await;
    assert_eq!(
        (replay.status, &replay.body["id"]),
        (StatusCode::CREATED, &posted.body["id"])
    );
    st.make_supporter(&hardcore.account_id).await;
    let cross = st
        .post(
            cr::SOCIAL_COMMENTS,
            Who::Launcher(Some(&hardcore.access)),
            json!({"target": format!("player:{}", target.xuid), "body": "x"}),
        )
        .challenge()
        .send()
        .await;
    assert_code(&cross, StatusCode::CONFLICT, "PORTAL.MODE_MISMATCH");
    for bad in [
        json!({"target": "player:abc", "body": "x"}),
        json!({"target": "nope:1", "body": "x"}),
        json!({"target": format!("player:{}", target.xuid), "body": ""}),
        json!({"target": format!("player:{}", target.xuid), "body": "x".repeat(1025)}),
    ] {
        let r = st
            .post(cr::SOCIAL_COMMENTS, Who::Web(Some(&supporter.cookie)), bad)
            .challenge()
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    }
    let club = st
        .post(
            cr::SOCIAL_COMMENTS,
            Who::Web(Some(&supporter.cookie)),
            json!({"target": "club:abc", "body": "x"}),
        )
        .challenge()
        .send()
        .await;
    assert_code(&club, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let list_uri = format!("{}?target=player:{}", cr::SOCIAL_COMMENTS, target.xuid);
    let list = st.get(&list_uri, Who::Bare).send().await;
    assert_eq!(
        list.body["items"][0]["id"], posted.body["id"],
        "{}",
        list.body
    );
    assert!(list.header("cache-control").unwrap().starts_with("public"));
    let comment_uri = format!(
        "/core/v1/social/comments/{}",
        posted.body["id"].as_str().unwrap()
    );
    let by_other = st
        .call(Method::DELETE, &comment_uri, Who::Web(Some(&free.cookie)))
        .send()
        .await;
    assert_code(&by_other, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let by_author = st
        .call(
            Method::DELETE,
            &comment_uri,
            Who::Web(Some(&supporter.cookie)),
        )
        .send()
        .await;
    assert_eq!(by_author.status, StatusCode::NO_CONTENT);
    assert!(
        st.get(&list_uri, Who::Bare).send().await.body["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // A moderator deletes somebody else's comment: audited.
    let second = st
        .post(
            cr::SOCIAL_COMMENTS,
            Who::Web(Some(&supporter.cookie)),
            json!({"target": format!("player:{}", target.xuid), "body": "Another"}),
        )
        .challenge()
        .send()
        .await;
    let moderator = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let before = st.audit_rows(&moderator.account_id).await;
    let removed = st
        .call(
            Method::DELETE,
            &format!(
                "/core/v1/social/comments/{}",
                second.body["id"].as_str().unwrap()
            ),
            Who::Web(Some(&moderator.cookie)),
        )
        .send()
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(st.audit_rows(&moderator.account_id).await, before + 1);

    // Reports: no self-report; three distinct reporters hide the card; a dismissal shows it again.
    let own = st
        .post(
            cr::SOCIAL_REPORTS,
            Who::Web(Some(&target.cookie)),
            json!({"target": format!("player:{}", target.xuid), "reason": "other"}),
        )
        .challenge()
        .send()
        .await;
    assert_code(&own, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let bad_reason = st
        .post(
            cr::SOCIAL_REPORTS,
            Who::Web(Some(&free.cookie)),
            json!({"target": format!("player:{}", target.xuid), "reason": "spite"}),
        )
        .challenge()
        .send()
        .await;
    assert_code(&bad_reason, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let mut report_id = None;
    for reporter in [&free, &supporter, &hardcore] {
        let r = st
            .post(cr::SOCIAL_REPORTS, Who::Web(Some(&reporter.cookie)), json!({"target": format!("player:{}", target.xuid), "reason": "abuse", "text": "synthetic"}))
            .challenge()
            .send()
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
        report_id = r.body["id"].as_str().map(str::to_owned);
    }
    assert_code(
        &st.get(&card_uri, Who::Bare).send().await,
        StatusCode::NOT_FOUND,
        "PORTAL.NOT_FOUND",
    );
    let m = Who::Web(Some(&moderator.cookie));
    let queue = st
        .get(&format!("{}?state=open", cr::ADMIN_REPORTS), m)
        .send()
        .await;
    assert!(
        queue.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"].as_str() == report_id.as_deref()),
        "{}",
        queue.body
    );
    let report_uri = format!("/core/v1/admin/reports/{}", report_id.as_deref().unwrap());
    let ban = st
        .post(&report_uri, m, json!({"action": "ban"}))
        .send()
        .await;
    assert_code(&ban, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    let before = st.audit_rows(&moderator.account_id).await;
    let decided = st
        .post(
            &report_uri,
            m,
            json!({"action": "dismiss", "note": "not abuse"}),
        )
        .send()
        .await;
    assert_eq!(decided.status, StatusCode::OK, "{}", decided.body);
    assert_eq!(decided.body["state"], "dismissed");
    assert_eq!(st.audit_rows(&moderator.account_id).await, before + 1);
    assert_eq!(
        st.get(&card_uri, Who::Bare).send().await.status,
        StatusCode::OK
    );
    let again = st
        .post(&report_uri, m, json!({"action": "hide"}))
        .send()
        .await;
    assert_code(&again, StatusCode::CONFLICT, "PORTAL.CONFLICT");
    // A report on a comment, decided with a strike on its author.
    let third = st
        .post(
            cr::SOCIAL_COMMENTS,
            Who::Web(Some(&supporter.cookie)),
            json!({"target": format!("player:{}", target.xuid), "body": "Synthetic insult"}),
        )
        .challenge()
        .send()
        .await;
    let on_comment = st
        .post(cr::SOCIAL_REPORTS, Who::Web(Some(&target.cookie)), json!({"target": format!("comment:{}", third.body["id"].as_str().unwrap()), "reason": "abuse"}))
        .challenge()
        .send()
        .await;
    assert_eq!(
        on_comment.status,
        StatusCode::ACCEPTED,
        "{}",
        on_comment.body
    );
    let struck = st
        .post(
            &format!(
                "/core/v1/admin/reports/{}",
                on_comment.body["id"].as_str().unwrap()
            ),
            m,
            json!({"action": "strike", "note": "synthetic insult"}),
        )
        .send()
        .await;
    assert_eq!(
        (struck.status, struck.body["state"].as_str()),
        (StatusCode::OK, Some("actioned")),
        "{}",
        struck.body
    );
    let strikes = st
        .get(cr::ME_STRIKES, Who::Web(Some(&supporter.cookie)))
        .send()
        .await;
    assert_eq!(strikes.body["items"][0]["reason"], "synthetic insult");
    let missing = st
        .post(
            cr::SOCIAL_REPORTS,
            Who::Web(Some(&free.cookie)),
            json!({"target": format!("comment:{}", Uuid::now_v7()), "reason": "abuse"}),
        )
        .challenge()
        .send()
        .await;
    assert_code(&missing, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let unknown_report = st
        .post(
            &format!("/core/v1/admin/reports/{}", Uuid::now_v7()),
            m,
            json!({"action": "dismiss"}),
        )
        .send()
        .await;
    assert_code(&unknown_report, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    // News comments: on a notice of the game.
    let admin = st.player("noob").await;
    st.set_role(&admin.account_id, "admin").await;
    let news = st
        .post(cr::ADMIN_NOTICES, Who::Web(Some(&admin.cookie)), json!({"game": "fh2", "kind": "news", "level": "info", "title": "Synthetic news", "body": "x", "lang": "en"}))
        .send()
        .await;
    let on_news = st
        .post(cr::SOCIAL_COMMENTS, Who::Launcher(Some(&hardcore.access)), json!({"target": format!("news:{}", news.body["id"].as_str().unwrap()), "body": "Nice"}))
        .challenge()
        .send()
        .await;
    assert_eq!(
        on_news.status,
        StatusCode::CREATED,
        "any mode comments on news: {}",
        on_news.body
    );
    let _ = st
        .call(
            Method::DELETE,
            &format!(
                "/core/v1/admin/notices/{}",
                news.body["id"].as_str().unwrap()
            ),
            Who::Web(Some(&admin.cookie)),
        )
        .key()
        .send()
        .await;
}

// ---------------------------------------------------------------------------------------------
// The Hardcore guardrails' assessments (OD-62).
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn moderators_read_the_guardrails_assessments() {
    let st = stack().await;
    let player = st.player("hardcore").await;
    let other = st.player("hardcore").await;
    let moderator = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let m = Who::Web(Some(&moderator.cookie));
    // Three assessments of the player (oldest first) and one of another player, as the gates
    // record them (`fi_db::integrity::record_assessment_in`; synthetic numbers).
    let insert = |xuid: String,
                  score: i16,
                  refusal: Option<&'static str>,
                  reasons: Vec<&'static str>| {
        let owner = st.owner.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO integrity_assessment (game, profile_id, mode, play_session, source, hours, \
                   deltas, rates, score, reasons, refusal, decision, would_block, rules_version) \
                 SELECT p.game, p.id, 'hardcore', $2, 'save_commit', 0.25, \
                   '{\"credits\": 24000000, \"xp\": 0, \"levels\": 0, \"winnings\": null, \"cars\": null, \"unexplained\": 24000000, \"game\": {\"credits\": 1, \"xp\": 2, \"levels\": 0, \"hoursSince\": 0.5}}', \
                   '{\"credits\": 24000000.0, \"unexplained\": 24000000.0, \"winnings\": 0.0, \"xp\": 0.0, \"levels\": 0.0, \"cars\": 0.0}', \
                   $3, $4, $5, CASE WHEN $5::text IS NULL THEN 'accepted' ELSE 'refused' END, $3 >= 60, 'guard-1' \
                 FROM profile p WHERE p.xuid = $1::bigint RETURNING id",
            )
            .bind(xuid)
            .bind(Uuid::now_v7())
            .bind(score)
            .bind(reasons)
            .bind(refusal)
            .fetch_one(&owner)
            .await
            .unwrap()
        }
    };
    let first = insert(player.xuid.clone(), 0, None, vec![]).await;
    insert(
        player.xuid.clone(),
        80,
        None,
        vec!["CREDITS_RATE_10_25M", "SAVE_AHEAD_OF_GAME"],
    )
    .await;
    let newest = insert(
        player.xuid.clone(),
        100,
        Some("CREDITS_CEILING"),
        vec!["CREDITS_RATE_25M_PLUS", "SAVE_AHEAD_OF_GAME"],
    )
    .await;
    insert(other.xuid.clone(), 70, None, vec!["LEVEL_XP_MISMATCH"]).await;

    // By XUID: newest first, the DTO's shape, no e-mail, IP or gamertag.
    let by_xuid = st
        .get(
            &format!("{}?game=fh2&xuid={}", cr::ADMIN_ASSESSMENTS, player.xuid),
            m,
        )
        .send()
        .await;
    assert_eq!(by_xuid.status, StatusCode::OK, "{}", by_xuid.body);
    assert_eq!(by_xuid.header("cache-control"), Some("private, no-store"));
    let items = by_xuid.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let top = &items[0];
    assert_eq!(top["id"], newest.to_string());
    assert_eq!(
        (
            &top["xuid"],
            &top["game"],
            &top["source"],
            &top["score"],
            &top["refusal"],
            &top["decision"],
            &top["wouldBlock"],
        ),
        (
            &json!(player.xuid),
            &json!("fh2"),
            &json!("save_commit"),
            &json!(100),
            &json!("CREDITS_CEILING"),
            &json!("refused"),
            &json!(true),
        )
    );
    assert_eq!(top["deltas"]["game"]["hoursSince"], 0.5);
    assert_eq!(top["rates"]["credits"], 24_000_000.0);
    assert_eq!(top["reasons"][0], "CREDITS_RATE_25M_PLUS");
    assert_eq!(by_xuid.body["next"], Value::Null, "one page");
    let text = by_xuid.body.to_string();
    assert!(!text.contains('@') && !text.contains(&player.gamertag));
    // By account and score; the game's feed by score; a page after a cursor.
    let by_account = st
        .get(
            &format!(
                "{}?game=fh2&accountId={}&minScore=60",
                cr::ADMIN_ASSESSMENTS,
                player.account_id
            ),
            m,
        )
        .send()
        .await;
    assert_eq!(by_account.status, StatusCode::OK, "{}", by_account.body);
    assert_eq!(by_account.body["items"].as_array().unwrap().len(), 2);
    let feed = st
        .get(
            &format!("{}?game=fh2&minScore=70", cr::ADMIN_ASSESSMENTS),
            m,
        )
        .send()
        .await;
    assert_eq!(feed.status, StatusCode::OK, "{}", feed.body);
    let feed = feed.body["items"].as_array().unwrap().clone();
    assert!(feed.iter().any(|i| i["xuid"] == json!(other.xuid)));
    assert!(feed.iter().all(|i| i["score"].as_i64().unwrap() >= 70));
    let older = st
        .get(
            &format!(
                "{}?game=fh2&xuid={}&cursor={}",
                cr::ADMIN_ASSESSMENTS,
                player.xuid,
                items[1]["id"].as_str().unwrap()
            ),
            m,
        )
        .send()
        .await;
    assert_eq!(older.body["items"][0]["id"], first.to_string());
    assert_eq!(older.body["items"].as_array().unwrap().len(), 1);
    // An unknown game, a profile without assessments: no items.
    for uri in [
        format!("{}?game=zz9none", cr::ADMIN_ASSESSMENTS),
        format!("{}?game=fh2&xuid={}", cr::ADMIN_ASSESSMENTS, moderator.xuid),
    ] {
        let r = st.get(&uri, m).send().await;
        assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.body);
        assert_eq!(r.body["items"], json!([]), "{uri}");
    }
    // Malformed queries: REQUEST.INVALID with the field.
    for (query, field) in [
        ("", "game"),
        ("game=FH2", "game"),
        ("game=fh2&xuid=1&accountId=x", "query"),
        ("game=fh2&xuid=abc", "xuid"),
        ("game=fh2&accountId=nope", "accountId"),
        ("game=fh2&minScore=101", "minScore"),
        ("game=fh2&minScore=-1", "minScore"),
        ("game=fh2&cursor=x", "cursor"),
        ("game=fh2&email=a@b.c", "query"),
    ] {
        let r = st
            .get(&format!("{}?{query}", cr::ADMIN_ASSESSMENTS), m)
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], field, "{query}");
    }
    // A player: AUTH.ROLE_REQUIRED; no credential: PORTAL.SESSION_REQUIRED; a game ticket:
    // AUTH.TOKEN_INVALID.
    let uri = format!("{}?game=fh2", cr::ADMIN_ASSESSMENTS);
    let r = st.get(&uri, Who::Web(Some(&player.cookie))).send().await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    assert_eq!(r.body["details"]["role"], "moderator");
    let r = st
        .get(&uri, Who::Launcher(Some(&player.access)))
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    let r = st.get(&uri, Who::Web(None)).send().await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "PORTAL.SESSION_REQUIRED");
    let ticket = game_ticket(&st);
    let r = st.get(&uri, Who::Launcher(Some(&ticket))).send().await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres (see the file header; the auth emulator's host is read, never called)"]
async fn the_server_summary_is_public_live_and_the_nightly_snapshot() {
    let st = stack().await;
    // A game of this run with one live and one queued session.
    let game = format!("t{}", &fresh()[18..]);
    sqlx::query("INSERT INTO game (code, title) VALUES ($1, 'Synthetic summary game')")
        .bind(&game)
        .execute(&st.owner)
        .await
        .unwrap();
    let pool = format!("t-{}", &fresh()[18..]);
    sqlx::query(
        "INSERT INTO pool (name, game, tier, capacity, game_endpoint) VALUES ($1, $2, 'vip', 3, 'http://game.test')",
    )
    .bind(&pool)
    .bind(&game)
    .execute(&st.owner)
    .await
    .unwrap();
    for state in ["active", "queued"] {
        let account: Uuid =
            sqlx::query_scalar("INSERT INTO account (origin) VALUES ('registered') RETURNING id")
                .fetch_one(&st.owner)
                .await
                .unwrap();
        sqlx::query(
            "WITH p AS (INSERT INTO profile (account_id, game, xuid, name, discriminator, mode, origin, created_at) \
               VALUES ($1, $2, nextval('profile_xuid_seq'), $3, 1, 'noob', 'created', now() - interval '3 days') RETURNING id), \
             d AS (INSERT INTO device_binding (account_id, device_id, label, platform) \
               VALUES ($1, $4, 'PC', 'linux') RETURNING id) \
             INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, state, \
               launcher_version, queued_at, admitted_at, expires_at, max_until, game_until) \
             SELECT $5, $1, p.id, d.id, $6, $7, '0.1.0', clock_timestamp(), \
               CASE WHEN $8 THEN clock_timestamp() END, clock_timestamp() + interval '5 minutes', \
               CASE WHEN $8 THEN now() + interval '12 hours' END, \
               CASE WHEN $8 THEN now() + interval '12 hours' END FROM p, d",
        )
        .bind(account)
        .bind(&game)
        .bind(format!("S{}", &fresh()[20..]))
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .bind(&pool)
        .bind(state)
        .bind(state == "active")
        .execute(&st.owner)
        .await
        .unwrap();
    }
    let uri = format!("{}?game={game}", cr::SUMMARY);

    let before = st.get(&uri, Who::Bare).send().await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    assert_eq!(
        before.header("cache-control"),
        Some("public, max-age=300, s-maxage=300")
    );
    assert_eq!(before.body["game"], game.as_str());
    assert_eq!(
        before.body["onlineNow"], 1,
        "the queued session is not online"
    );
    assert_eq!(before.body["nightly"], Value::Null, "no nightly run yet");
    assert!(before.body["generatedAt"].is_string());

    // The nightly job's snapshot (computed and stored as consolidate-nightly does).
    let nightly = fi_db::summary::compute(&st.owner, &game, fi_db::summary::last_day())
        .await
        .unwrap();
    fi_db::summary::store(&st.owner, &game, &nightly)
        .await
        .unwrap();
    let after = st.get(&uri, Who::Bare).send().await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.body);
    let parsed: fi_protocol::core::ServerSummary =
        serde_json::from_value(after.body.clone()).expect("the contract's shape");
    assert_eq!(parsed.online_now, 1);
    assert_eq!(parsed.nightly, Some(nightly));
    let n = &after.body["nightly"];
    assert_eq!(n["players"]["profiles"], 2, "{n}");
    assert_eq!(n["players"]["accounts"], 2, "{n}");
    assert_eq!(
        n["online"]["sessionsTotal"], 0,
        "admitted today, not yesterday"
    );

    let missing = st.get(cr::SUMMARY, Who::Bare).send().await;
    assert_code(&missing, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let extra = st.get(&format!("{uri}&lang=en"), Who::Bare).send().await;
    assert_code(&extra, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let unknown = st
        .get(&format!("{}?game=zz9nosuchgame", cr::SUMMARY), Who::Bare)
        .send()
        .await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
}

// Liveries (CR-S59).
// ---------------------------------------------------------------------------------------------

/// CR-S59 (CORE_API.md §5): a moderator lists an account's liveries, hides one (an `admin_action`
/// row each, replay-safe), deletes one, and blocks the account's uploads; players are refused,
/// unknown ids and accounts are 404, and nobody moderates their own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_moderator_lists_hides_deletes_and_blocks_liveries() {
    let st = stack().await;
    let player = st.player("noob").await;
    let moderator = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let m = Who::Web(Some(&moderator.cookie));
    // Two synthetic liveries of the player's profile (bytes content-addressed, CR-S59).
    let profile: Uuid = sqlx::query_scalar(
        "SELECT id FROM profile WHERE account_id = $1::uuid ORDER BY created_at LIMIT 1",
    )
    .bind(&player.account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    let base = (i64::from_be_bytes(Uuid::now_v7().as_bytes()[8..].try_into().unwrap())
        & 0x3FFF_FFFF_FFFF_FFF0)
        | 2;
    let (a, b) = (base, base + 1);
    sqlx::query(
        "INSERT INTO livery_blob (game, profile_id, sha256, size, data) \
         VALUES ('fh2', $1, sha256('\\x0a0b'::bytea), 2, '\\x0a0b'::bytea)",
    )
    .bind(profile)
    .execute(&st.owner)
    .await
    .unwrap();
    for (id, car) in [(a, 289), (b, 512)] {
        sqlx::query(
            "INSERT INTO game_livery (id, game, mode, profile_id, name, car_id, number_of_layers, blob_sha256) \
             VALUES ($1, 'fh2', 'noob', $2, 'Synthetic', $3, 2, sha256('\\x0a0b'::bytea))",
        )
        .bind(id)
        .bind(profile)
        .bind(car)
        .execute(&st.owner)
        .await
        .unwrap();
    }
    let list_uri = format!("/core/v1/admin/accounts/{}/liveries", player.account_id);
    // A player: AUTH.ROLE_REQUIRED.
    let r = st
        .get(&list_uri, Who::Web(Some(&player.cookie)))
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    // The list: both, never the bytes.
    let r = st.get(&list_uri, m).send().await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let items = r.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(r.body["uploadsBlocked"], false);
    assert!(r.body["next"].is_null());
    let a_id = a.to_string();
    let first = items
        .iter()
        .find(|i| i["id"].as_str() == Some(a_id.as_str()))
        .unwrap();
    assert_eq!(
        (
            first["carId"].as_i64(),
            first["size"].as_i64(),
            first["xuid"].as_str(),
            first["hiddenAt"].is_null()
        ),
        (Some(289), Some(2), Some(player.xuid.as_str()), true)
    );
    assert_eq!(first["sha256"].as_str().map(str::len), Some(64));
    assert!(first.get("data").is_none());
    // Hide: one audit row, replay-safe.
    let before = st.audit_rows(&moderator.account_id).await;
    let key = Uuid::now_v7().to_string();
    let uri = format!("/core/v1/admin/liveries/{a}");
    let body = json!({"hidden": true, "reason": "synthetic offensive paint"});
    for _ in 0..2 {
        let r = st
            .call(Method::PUT, &uri, m)
            .header("idempotency-key", &key)
            .json(body.clone())
            .send()
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert!(r.body["hiddenAt"].is_string());
    }
    assert_eq!(st.audit_rows(&moderator.account_id).await, before + 1);
    let hidden: bool =
        sqlx::query_scalar("SELECT hidden_at IS NOT NULL FROM game_livery WHERE id = $1")
            .bind(a)
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert!(hidden);
    // Bad reason, unknown id.
    let r = st
        .call(Method::PUT, &uri, m)
        .key()
        .json(json!({"hidden": true, "reason": ""}))
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let r = st
        .call(Method::PUT, "/core/v1/admin/liveries/42", m)
        .key()
        .json(body.clone())
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    // Delete: the row goes; the bytes stay while the other livery names them.
    let del = json!({"reason": "synthetic duplicate paint"});
    let r = st
        .call(Method::DELETE, &format!("/core/v1/admin/liveries/{b}"), m)
        .key()
        .json(del.clone())
        .send()
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    let r = st
        .call(Method::DELETE, &format!("/core/v1/admin/liveries/{b}"), m)
        .key()
        .json(del)
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let r = st
        .call(Method::DELETE, &format!("/core/v1/admin/liveries/{a}"), m)
        .key()
        .json(json!({"reason": ""}))
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let left: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM game_livery WHERE profile_id = $1), \
                (SELECT count(*) FROM livery_blob WHERE profile_id = $1)",
    )
    .bind(profile)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(left, (1, 1));
    // Block the uploads; the list says so.
    let uploads = format!(
        "/core/v1/admin/accounts/{}/livery-uploads",
        player.account_id
    );
    let r = st
        .call(Method::PUT, &uploads, m)
        .key()
        .json(json!({"blocked": true, "reason": "repeated offensive paints"}))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        r.body,
        json!({"accountId": player.account_id, "blocked": true})
    );
    let r = st.get(&list_uri, m).send().await;
    assert_eq!(r.body["uploadsBlocked"], true);
    // Unknown account; one's own.
    let r = st
        .call(
            Method::PUT,
            &format!("/core/v1/admin/accounts/{}/livery-uploads", Uuid::now_v7()),
            m,
        )
        .key()
        .json(json!({"blocked": true, "reason": "x"}))
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let r = st
        .call(
            Method::PUT,
            &format!(
                "/core/v1/admin/accounts/{}/livery-uploads",
                moderator.account_id
            ),
            m,
        )
        .key()
        .json(json!({"blocked": true, "reason": "x"}))
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    // Bulk: hide every livery of the account; a bad action; an unknown account.
    let bulk = format!("/core/v1/admin/accounts/{}/liveries", player.account_id);
    let r = st
        .call(Method::POST, &bulk, m)
        .key()
        .json(json!({"action": "hide", "reason": "every paint of this account"}))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        r.body,
        json!({"accountId": player.account_id, "action": "hide", "affected": 1})
    );
    let r = st
        .call(Method::POST, &bulk, m)
        .key()
        .json(json!({"action": "burn", "reason": "x"}))
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(r.body["details"]["field"], "action");
    let r = st
        .get(
            &format!("/core/v1/admin/accounts/{}/liveries", Uuid::now_v7()),
            m,
        )
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let r = st.get(&format!("{list_uri}?cursor=nope"), m).send().await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM admin_action WHERE actor_account_id = $1::uuid AND action LIKE 'livery.%' ORDER BY at",
    )
    .bind(&moderator.account_id)
    .fetch_all(&st.owner)
    .await
    .unwrap();
    assert_eq!(
        actions,
        [
            "livery.hide",
            "livery.delete",
            "livery.block",
            "livery.hide_all"
        ]
    );
}
