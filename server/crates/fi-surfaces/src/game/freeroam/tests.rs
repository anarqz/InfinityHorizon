//! [`super::handle`] over each host and status. The stateless answers and every refusal decided
//! before Postgres run without a database (a lazy pool that never connects); the rest run against
//! a real Postgres 18 as a `fi_runtime` login (a missing grant fails here as in production):
//!
//! ```sh
//! FI_TEST_DATABASE_URL=postgres://fi_migrator:fi_migrator_local@127.0.0.1:5452/forza_infinity_test \
//!   cargo test -p fi-api freeroam -- --include-ignored
//! ```
//!
//! The replay of the Beta's recorded free-roam traffic is opt-in on top: `FI_FH2_CAPTURES_DIR`
//! names a folder of capture `.jsonl` files (outside the repository: they hold real identities;
//! nothing of them is copied or printed here but counts).

use super::doc;
use super::{
    GAMESERVERDS, GAMESERVERMS, MOMATCH, NO_WORLD_RETRY_SECS, SESSIONDIRECTORY, Settings, handle,
    handle_with,
};
use crate::game::request::GameRequest;
use crate::game::ticket::{Ticket, TicketClaims};
use crate::state::AppState;
use axum::body::to_bytes;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use fi_db::freeroam;
use fi_db::game::Liveness;
use fi_db::matchmaking::{self, Endpoint, JoinInput, JoinOutcome, Settings as WorldSettings};
use fi_db::{PgPool, migrate};
use fi_domain::UnixMs;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::OnceLock;
use uuid::Uuid;

const SCID: &str = "00000000-0000-0000-0000-0000000000aa";
const TEMPLATE: &str = "defaultTemplate_qos";
const LOGIN: &str = "fi_app_freeroam_api";

fn lazy_state() -> AppState {
    AppState {
        pool: PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap(),
        identity: None,
    }
}

/// A ticket for a profile that only exists in memory (refusals decided before Postgres).
fn synthetic_ticket(tier: &str) -> Ticket {
    let now = UnixMs::now_system();
    ticket_of(
        tier,
        "fh2",
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        2_535_100_000_000_001,
        now,
    )
}

fn ticket_of(
    tier: &str,
    game: &str,
    session: Uuid,
    account: Uuid,
    profile: Uuid,
    xuid: i64,
    now: UnixMs,
) -> Ticket {
    let pool = if tier == "vip" { "vip" } else { "matchmaking" };
    let secs = now.millis() / 1_000;
    Ticket {
        claims: TicketClaims {
            sid: session.to_string(),
            sub: profile.to_string(),
            acc: account.to_string(),
            xuid: xuid.to_string(),
            game: Some(game.to_owned()),
            pool: pool.into(),
            tier: tier.into(),
            mode: "noob".into(),
            cdb: String::new(),
            line: false,
            aud: "game".into(),
            iat: fi_protocol::instant::format_unix_secs(secs).unwrap(),
            exp: fi_protocol::instant::format_unix_secs(secs + 600).unwrap(),
        },
        live: Liveness {
            session_id: session,
            account_id: account,
            profile_id: profile,
            pool: pool.into(),
            state: "active".into(),
            game_until: Some(UnixMs::from_millis(now.millis() + 3_600_000)),
            cloud: false,
            dedicated: false,
            modded: false,
            integrity: false,
            now,
        },
        xuid,
    }
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["code"].as_str().unwrap_or("")
    }
}

async fn read(r: Response) -> Answer {
    let status = r.status();
    let headers = r.headers().clone();
    let bytes = to_bytes(r.into_body(), 1 << 22).await.unwrap();
    Answer {
        status,
        headers,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

#[allow(clippy::too_many_arguments)]
async fn send(
    state: &AppState,
    settings: &Settings,
    ticket: &Ticket,
    ip: Option<IpAddr>,
    method: Method,
    host: &str,
    path: &str,
    body: &[u8],
) -> Option<Answer> {
    let headers = HeaderMap::new();
    let req = GameRequest {
        ticket,
        method: &method,
        host,
        path,
        query: None,
        headers: &headers,
        body,
        client_ip: ip,
    };
    match handle_with(state, &req, settings).await {
        Some(r) => Some(read(r).await),
        None => None,
    }
}

fn session_path(name: &str) -> String {
    session_path_in(TEMPLATE, name)
}

fn session_path_in(template: &str, name: &str) -> String {
    format!("/serviceconfigs/{SCID}/sessionTemplates/{template}/sessions/{name}")
}

/// A car meet's target session (in the game's target template, not the ticket session's).
fn meet_path(name: &str) -> String {
    session_path_in(doc::CARMEET_TARGET_TEMPLATE, name)
}

fn hopper_path(hopper: &str) -> String {
    format!("/Serviceconfigs/{SCID}/hoppers/{hopper}")
}

#[tokio::test]
async fn stateless_answers_and_early_refusals_need_no_database() {
    let state = lazy_state();
    let t = synthetic_ticket("free");
    let s = Settings::default();
    let headers = HeaderMap::new();
    let other = GameRequest {
        ticket: &t,
        method: &Method::GET,
        host: "profile.xboxlive.com",
        path: "/users/me/profile/settings",
        query: None,
        headers: &headers,
        body: b"",
        client_ip: None,
    };
    assert!(
        handle(&state, &other).await.is_none(),
        "not a free-roam host"
    );
    let call = |method: Method, host: &'static str, path: String, body: &'static [u8]| {
        let (state, t) = (state.clone(), t.clone());
        async move {
            send(&state, &s, &t, None, method, host, &path, body)
                .await
                .unwrap()
        }
    };
    // gameserverds: fixed answers, JSON with an ETag.
    let a = call(
        Method::POST,
        GAMESERVERDS,
        "/titles/1368890819/variants".into(),
        br#"{"locale":"en-us","maxVariants":8,"publisherOnly":false}"#,
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.json, doc::variants(&doc::FH2_SESSION));
    assert_eq!(
        a.headers[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    assert!(a.headers.contains_key(header::ETAG));
    let a = call(Method::GET, GAMESERVERDS, "/qosservers".into(), b"").await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::OK, doc::qos_servers(&doc::FH2_SESSION))
    );
    let a = call(Method::GET, GAMESERVERDS, "/titles/1/other".into(), b"").await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "GAME.NOT_SERVED")
    );
    assert_eq!(a.json["details"]["host"], GAMESERVERDS);
    // sessiondirectory: the listing is always empty; other paths are not served; a malformed
    // key is invalid.
    let listing = format!("/serviceconfigs/{SCID}/sessiontemplates/{TEMPLATE}/sessions");
    let a = call(Method::GET, SESSIONDIRECTORY, listing, b"").await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::OK, json!({"results": []}))
    );
    let a = call(
        Method::GET,
        SESSIONDIRECTORY,
        format!("/serviceconfigs/{SCID}/handles"),
        b"",
    )
    .await;
    assert_eq!(a.code(), "GAME.NOT_SERVED");
    let a = call(Method::GET, SESSIONDIRECTORY, "/other".into(), b"").await;
    assert_eq!(a.code(), "GAME.NOT_SERVED");
    let a = call(
        Method::GET,
        SESSIONDIRECTORY,
        session_path("bad%20name"),
        b"",
    )
    .await;
    assert_eq!(
        (a.status, a.code(), a.json["details"]["field"].clone()),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID", json!("path"))
    );
    let a = call(Method::PATCH, SESSIONDIRECTORY, session_path("s1"), b"").await;
    assert_eq!(a.code(), "GAME.NOT_SERVED");
    // momatch: FreeRoam_qos, CarMeet_qos and RoadTrip_qos tickets only; a car meet needs its meet.
    for hopper in ["Showdown_qos", "CarMeet_qos"] {
        let a = call(Method::POST, MOMATCH, hopper_path(hopper), b"{}").await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "GAME.NOT_SERVED")
        );
        assert_eq!(a.json["details"]["hopper"], hopper);
    }
    // No other hopper, and no car meet ticket without its meet (OD-144).
    let meets = Settings::default();
    for (hopper, body) in [
        (
            "Showdown_qos",
            &br#"{"ticketAttributes":{"Location":6}}"#[..],
        ),
        (
            "CarMeet_qos",
            br#"{"ticketAttributes":{"hopperId":3,"ParentBucket":3}}"#,
        ),
        ("CarMeet_qos", b"{}"),
    ] {
        let a = send(
            &state,
            &meets,
            &t,
            None,
            Method::POST,
            MOMATCH,
            &hopper_path(hopper),
            body,
        )
        .await
        .unwrap();
        assert_eq!(
            (a.status, a.code(), a.json["details"]["hopper"].clone()),
            (StatusCode::NOT_FOUND, "GAME.NOT_SERVED", json!(hopper)),
            "{hopper}"
        );
    }
    let a = call(Method::PUT, MOMATCH, hopper_path("FreeRoam_qos"), b"{}").await;
    assert_eq!(a.code(), "GAME.NOT_SERVED");
    let a = call(Method::GET, MOMATCH, "/serviceconfigs/x".into(), b"").await;
    assert_eq!(a.code(), "GAME.NOT_SERVED");
    // A ticket on a non-UUID id, or with none, is answered without the database.
    let a = call(
        Method::DELETE,
        MOMATCH,
        format!("{}/tickets/", hopper_path("CarMeet_qos")),
        b"",
    )
    .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
    let a = call(
        Method::GET,
        MOMATCH,
        format!("{}/tickets/not-a-uuid", hopper_path("FreeRoam_qos")),
        b"",
    )
    .await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::NOT_FOUND, json!({}))
    );
    // gameserverms: only POST …/clusters.
    let a = call(Method::GET, GAMESERVERMS, "/titles/1/clusters".into(), b"").await;
    assert_eq!(a.code(), "GAME.NOT_SERVED");
    // Oversized bodies are refused before Postgres.
    let big: &'static [u8] = Box::leak(vec![b' '; super::MAX_DOC + 1].into_boxed_slice());
    let a = call(Method::PUT, SESSIONDIRECTORY, session_path("s1"), big).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::PAYLOAD_TOO_LARGE, "REQUEST.TOO_LARGE")
    );
    let a = call(Method::POST, GAMESERVERMS, "/titles/1/clusters".into(), big).await;
    assert_eq!(a.code(), "REQUEST.TOO_LARGE");
    // Ticket details Postgres would refuse or blow up are refused before anything is read.
    let reference = format!(
        r#""ticketSessionRef": {{"name": "t1", "scid": "{SCID}", "templateName": "{TEMPLATE}"}}"#
    );
    let nul: &'static [u8] = Box::leak(
        format!(r#"{{{reference}, "ticketAttributes": {{"k": "a\u0000b"}}}}"#)
            .into_bytes()
            .into_boxed_slice(),
    );
    let a = call(Method::POST, MOMATCH, hopper_path("FreeRoam_qos"), nul).await;
    assert_eq!(
        (a.status, a.code(), a.json["details"]["field"].clone()),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID", json!("body"))
    );
    let numbers = vec!["1e308"; 900].join(",");
    let huge: &'static [u8] = Box::leak(
        format!(r#"{{{reference}, "ticketAttributes": {{"k": [{numbers}]}}}}"#)
            .into_bytes()
            .into_boxed_slice(),
    );
    assert!(huge.len() < 8 << 10);
    let a = call(Method::POST, MOMATCH, hopper_path("FreeRoam_qos"), huge).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::PAYLOAD_TOO_LARGE, "REQUEST.TOO_LARGE")
    );
}

// ---------------------------------------------------------------------------------------------
// Against Postgres.

fn password() -> &'static str {
    static PASSWORD: OnceLock<String> = OnceLock::new();
    PASSWORD.get_or_init(|| format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple()))
}

fn database_url() -> String {
    std::env::var("FI_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("FI_TEST_DATABASE_URL is not set (see the file header)"))
}

async fn runtime_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(6)
        .connect_with(
            PgConnectOptions::from_str(&database_url())
                .unwrap()
                .username(LOGIN)
                .password(password()),
        )
        .await
        .expect("connect as the runtime login")
}

struct Pg {
    state: AppState,
    game: String,
}

struct Player {
    ticket: Ticket,
    ip: IpAddr,
}

async fn pg() -> Pg {
    let owner = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url())
        .await
        .expect("connect to the test database");
    {
        static READY: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);
        let mut ready = READY.lock().await;
        if !*ready {
            migrate::run(&owner).await.expect("migrations apply");
            migrate::ensure_login(&owner, LOGIN, password(), &["fi_runtime"])
                .await
                .expect("runtime login");
            *ready = true;
        }
    }
    let game = format!("r{}", &Uuid::now_v7().simple().to_string()[20..30]);
    sqlx::query("INSERT INTO game (code, title) VALUES ($1, 'Free roam HTTP test')")
        .bind(&game)
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mm_free_slot (game, free_limit) VALUES ($1, 50)")
        .bind(&game)
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO pool (name, game, tier, game_endpoint) VALUES ($1, $1, 'free', 'http://127.0.0.1:1/mm')",
    )
    .bind(&game)
    .execute(&owner)
    .await
    .unwrap();
    owner.close().await;
    Pg {
        state: AppState {
            pool: runtime_pool().await,
            identity: None,
        },
        game,
    }
}

impl Pg {
    async fn player(&self, tier: &str, ip: &str) -> Player {
        let db = &self.state.pool;
        let account: Uuid =
            sqlx::query_scalar("INSERT INTO account (origin) VALUES ('registered') RETURNING id")
                .fetch_one(db)
                .await
                .unwrap();
        let (profile, xuid): (Uuid, i64) = sqlx::query_as(
            "INSERT INTO profile (account_id, game, xuid, name, discriminator, mode, origin) \
             VALUES ($1, $2, nextval('profile_xuid_seq'), $3, 42, 'noob', 'created') RETURNING id, xuid",
        )
        .bind(account)
        .bind(&self.game)
        .bind(format!("Rider{}", &Uuid::now_v7().simple().to_string()[24..32]))
        .fetch_one(db)
        .await
        .unwrap();
        let binding: Uuid = sqlx::query_scalar(
            "INSERT INTO device_binding (account_id, device_id, label, platform) VALUES ($1, $2, 'PC', 'linux') RETURNING id",
        )
        .bind(account)
        .bind(Uuid::now_v7())
        .fetch_one(db)
        .await
        .unwrap();
        let session = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, state, launcher_version, \
               admitted_at, expires_at, max_until, game_until) \
             VALUES ($1, $2, $3, $4, $5, 'active', '0.1.0', clock_timestamp(), clock_timestamp() + interval '5 minutes', \
               now() + interval '12 hours', now() + interval '12 hours')",
        )
        .bind(session)
        .bind(account)
        .bind(profile)
        .bind(binding)
        .bind(&self.game)
        .execute(db)
        .await
        .unwrap();
        Player {
            ticket: ticket_of(
                tier,
                &self.game,
                session,
                account,
                profile,
                xuid,
                UnixMs::now_system(),
            ),
            ip: ip.parse().unwrap(),
        }
    }

    fn mm(&self, p: &Player) -> matchmaking::Caller {
        matchmaking::Caller {
            game: self.game.clone(),
            profile_id: p.ticket.live.profile_id,
            account_id: p.ticket.live.account_id,
            session_id: p.ticket.live.session_id,
            xuid: p.xuid(),
            tier: p.ticket.claims.tier.clone(),
            mode: p.ticket.claims.mode.clone(),
        }
    }

    /// The launcher's join, from the player's address.
    async fn join(&self, p: &Player, world: Uuid) -> JoinOutcome {
        matchmaking::join(
            &self.state.pool,
            &self.mm(p),
            &JoinInput {
                world_id: Some(world),
                code: None,
                ip: Some(p.ip.to_string()),
                cell: None,
                hold_secs: matchmaking::DEFAULT_SEAT_HOLD_SECS,
                mods: fi_db::matchmaking::ModSet::vanilla(),
                activity: None,
            },
        )
        .await
        .unwrap()
    }

    /// The host's hook announces its world (as fi-matchmaking's route would, from its address).
    async fn host(&self, p: &Player, lan: &[&str], peers: Vec<i64>) -> Uuid {
        let world = Uuid::now_v7();
        self.announce(p, world, lan, peers).await;
        world
    }

    /// A Supporter host's world is Supporter-only (the first version's public Supporter world),
    /// a free host's is open to everyone.
    async fn announce(&self, p: &Player, world: Uuid, lan: &[&str], peers: Vec<i64>) {
        let caller = self.mm(p);
        matchmaking::host(
            &self.state.pool,
            &caller,
            world,
            Some(&WorldSettings {
                name: "Evening cruise".into(),
                mode: caller.mode.clone(),
                open_modes: None,
                max_players: 8,
                audience: if caller.is_vip() {
                    "supporters"
                } else {
                    "everyone"
                }
                .into(),
                max_rtt_ms: None,
                races: true,
                message: None,
                allow: None,
                mods: fi_db::matchmaking::ModSet::vanilla(),
                lobbies: Vec::new(),
            }),
            &Endpoint {
                ip: p.ip.to_string(),
                cell: None,
                region: None,
                port: Some(5101),
                local_port: Some(5101),
                lan: Some(lan.iter().map(|s| (*s).to_owned()).collect()),
            },
            Some(&peers),
        )
        .await
        .unwrap();
    }

    async fn call(
        &self,
        p: &Player,
        method: Method,
        host: &str,
        path: &str,
        body: Value,
    ) -> Answer {
        self.call_with(&Settings::default(), p, method, host, path, body)
            .await
    }

    async fn call_with(
        &self,
        settings: &Settings,
        p: &Player,
        method: Method,
        host: &str,
        path: &str,
        body: Value,
    ) -> Answer {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        send(
            &self.state,
            settings,
            &p.ticket,
            Some(p.ip),
            method,
            host,
            path,
            &bytes,
        )
        .await
        .unwrap()
    }

    /// The game's ticket session: what it writes before asking SmartMatch.
    async fn ticket_session(&self, p: &Player, name: &str) -> Answer {
        let gsp = json!({"gameVariantId": doc::GAME_VARIANT, "gsiSetId": doc::GSI_SET, "maxAllowedPlayers": 12});
        self.call(
            p,
            Method::PUT,
            SESSIONDIRECTORY,
            &session_path(name),
            json!({
                "constants": {"system": {"version": 1, "visibility": "open"},
                              "custom": {"hopperId": 2, "gameServerPlatform": gsp}},
                "properties": {"system": {"matchmaking": {"targetSessionConstants": {
                    "custom": {"hopperId": 2, "gameServerPlatform": gsp}}}}},
                "members": {"me": {"constants": {"system": {"xuid": p.xuid().to_string(), "initialize": true}},
                                   "properties": {"system": {"active": true}}}}
            }),
        )
        .await
    }

    async fn ticket(&self, p: &Player, ticket_session: &str) -> Answer {
        self.call(
            p,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            json!({
                "preserveSession": "never", "scid": SCID, "giveUpDuration": 3_600_000,
                "ticketSessionRef": {"name": ticket_session, "scid": SCID, "templateName": TEMPLATE},
                "ticketAttributes": {"gsiSetId": doc::GSI_SET, "hopperId": 2}
            }),
        )
        .await
    }
}

impl Player {
    fn xuid(&self) -> i64 {
        self.ticket.xuid
    }
}

/// CR-S96 (WORLDS_OFFICIAL.md §12.2, §12.4): the game's private session write never allocates; with
/// the owner's seat it links, and only accounts with access or a live invite see the session.
#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn a_private_session_links_to_its_owners_world_and_stays_private() {
    let pg = pg().await;
    let (owner, stranger, friend) = (
        pg.player("free", "198.51.100.1").await,
        pg.player("free", "198.51.100.2").await,
        pg.player("free", "198.51.100.3").await,
    );
    let mode = pg.mm(&owner).mode.clone();
    let world = Uuid::now_v7();
    let vm = format!("v-{}", &Uuid::now_v7().simple().to_string()[..8]);
    // Two spares; `world` is the older.
    for id in [world, Uuid::now_v7()] {
        matchmaking::official_put(
            &pg.state.pool,
            &matchmaking::OfficialHost {
                vm_id: &vm,
                name: "Official Test",
                ip: "203.0.113.50",
                port: 5200,
                max_worlds: 8,
                region: None,
            },
            id,
            &matchmaking::OfficialSettings {
                game: pg.game.clone(),
                mode: mode.clone(),
                activity: "freeroam".into(),
                max_players: 12,
                pool: false,
            },
        )
        .await
        .unwrap();
    }
    let body = json!({"constants": {"system": {"version": 1, "visibility": "open"},
        "custom": {"hopperId": 2, "gameServerPlatform": {"maxAllowedPlayers": 12, "location": "North Central US"}}},
        "members": {"me": {"constants": {"system": {"initialize": true}},
            "properties": {"system": {"active": true}}}}});
    let linked = |name: String| {
        let pool = pg.state.pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT private_world_id FROM freeroam_session WHERE name = $1",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let name = |t: &str| format!("{t}-{}", Uuid::now_v7().simple());
    // No seat (an old client too): a plain session, no allocation.
    let plain = name("plain");
    let a = pg
        .call(
            &owner,
            Method::PUT,
            SESSIONDIRECTORY,
            &session_path_in("defaultTemplate_qos", &plain),
            body.clone(),
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED);
    assert_eq!(linked(plain).await, None);
    let idle: bool = sqlx::query_scalar("SELECT NOT official_group FROM world WHERE id = $1")
        .bind(world)
        .fetch_one(&pg.state.pool)
        .await
        .unwrap();
    assert!(idle, "a PUT never promotes");
    // The launcher's create, then the game's write.
    sqlx::query("UPDATE play_session SET features = '{private_official_v1}' WHERE id = $1")
        .bind(owner.ticket.live.session_id)
        .execute(&pg.state.pool)
        .await
        .unwrap();
    let granted = matchmaking::official_private_create(
        &pg.state.pool,
        &pg.mm(&owner),
        "freeroam",
        &JoinInput {
            world_id: None,
            code: None,
            ip: Some(owner.ip.to_string()),
            cell: None,
            hold_secs: matchmaking::DEFAULT_SEAT_HOLD_SECS,
            mods: fi_db::matchmaking::ModSet::vanilla(),
            activity: Some("freeroam".into()),
        },
        &matchmaking::JoinRelay::default(),
    )
    .await
    .unwrap();
    assert!(matches!(granted, JoinOutcome::Granted(_)));
    // A server-made session and a matchmade one don't link, even with the seat.
    for (template, session, sent) in [
        (
            "defaultTemplate_qos",
            format!("fh2inf-invite-{}", Uuid::now_v7().simple()),
            body.clone(),
        ),
        ("CarMeet_qos", name("meet"), body.clone()),
        (
            "defaultTemplate_qos",
            name("hopper"),
            json!({"constants": {"custom": {"hopperId": 3, "gameServerPlatform": {"location": "x"}}}}),
        ),
    ] {
        pg.call(
            &owner,
            Method::PUT,
            SESSIONDIRECTORY,
            &session_path_in(template, &session),
            sent,
        )
        .await;
        assert_eq!(linked(session).await, None);
    }
    let private = name("private");
    let path = session_path_in("defaultTemplate_qos", &private);
    let made = pg
        .call(&owner, Method::PUT, SESSIONDIRECTORY, &path, body.clone())
        .await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert_eq!(linked(private.clone()).await, Some(world));
    assert_eq!(
        made.json["servers"]["gameServerPlatform"]["properties"]["custom"]["hostName"],
        "203.0.113.50",
        "its server is the owner's seat"
    );
    // A stranger: 404 on read and on write.
    for method in [Method::GET, Method::PUT] {
        let no = pg
            .call(&stranger, method, SESSIONDIRECTORY, &path, json!({}))
            .await;
        assert_eq!(no.status, StatusCode::NOT_FOUND);
    }
    // An account with access reads it, from its own route, and becomes a member.
    sqlx::query("INSERT INTO world_access (world_id, account_id, via) VALUES ($1, $2, 'invite')")
        .bind(world)
        .bind(friend.ticket.live.account_id)
        .execute(&pg.state.pool)
        .await
        .unwrap();
    let read = pg
        .call(&friend, Method::GET, SESSIONDIRECTORY, &path, json!({}))
        .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.json["membersInfo"]["count"], 2);
    assert_eq!(
        read.json["members"]["0"]["constants"]["system"]["xuid"],
        friend.xuid().to_string()
    );
    let write = pg
        .call(
            &friend,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"members": {"me": {"properties": {"system": {"active": true}}}}}),
        )
        .await;
    assert_eq!(write.status, StatusCode::OK);
    // A pending invitee reads it without becoming a member, and can't write it: 404.
    let invitee = pg.player("free", "198.51.100.4").await;
    sqlx::query(
        "INSERT INTO world_invite (id, game, kind, world_id, from_account_id, from_profile_id, to_account_id, expires_at) \
         VALUES ($1, $2, 'invite', $3, $4, $5, $6, clock_timestamp() + interval '2 minutes')",
    )
    .bind(Uuid::now_v7())
    .bind(&pg.game)
    .bind(world)
    .bind(owner.ticket.live.account_id)
    .bind(owner.ticket.live.profile_id)
    .bind(invitee.ticket.live.account_id)
    .execute(&pg.state.pool)
    .await
    .unwrap();
    let invited = pg
        .call(&invitee, Method::GET, SESSIONDIRECTORY, &path, json!({}))
        .await;
    assert_eq!(invited.status, StatusCode::OK);
    assert_eq!(invited.json["membersInfo"]["count"], 2, "not added");
    let refused = pg
        .call(
            &invitee,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"members": {"me": {"properties": {"system": {"active": true}}}}}),
        )
        .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    // The owner's DELETE leaves it for the others while the world lives.
    let gone = pg
        .call(&owner, Method::DELETE, SESSIONDIRECTORY, &path, json!({}))
        .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    assert_eq!(
        pg.call(&friend, Method::GET, SESSIONDIRECTORY, &path, json!({}))
            .await
            .status,
        StatusCode::OK
    );
    // Search still lists nothing.
    let search = pg
        .call(
            &friend,
            Method::GET,
            SESSIONDIRECTORY,
            &path[..path.rfind('/').unwrap()],
            json!({}),
        )
        .await;
    assert_eq!(search.json, json!({"results": []}));
}

fn address_of(entry: &Value) -> String {
    let b64 = entry["properties"]["custom"]["secureDeviceAddress"]
        .as_str()
        .unwrap_or_default();
    String::from_utf8(fi_auth::b64::std_decode(b64).unwrap_or_default()).unwrap_or_default()
}

#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn sessions_create_merge_read_delete_and_stay_private() {
    let pg = pg().await;
    let (alice, bob) = (
        pg.player("free", "198.51.100.1").await,
        pg.player("vip", "198.51.100.2").await,
    );
    let path = session_path("Lobby-1");
    let a = pg
        .call(
            &alice,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"properties": {"custom": {"k": 1}},
                   "members": {"me": {"properties": {"system": {"active": true}}}}}),
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED);
    assert_eq!(a.headers[header::ETAG], "\"1\"");
    assert_eq!(a.json["contractVersion"], 105);
    assert_eq!(a.json["membersInfo"]["count"], 1);
    let me = &a.json["members"]["0"];
    assert_eq!(me["constants"]["system"]["xuid"], alice.xuid().to_string());
    assert!(me["gamertag"].as_str().unwrap().starts_with("Rider"));
    assert!(me["gamertag"].as_str().unwrap().chars().count() <= 15);
    assert_eq!(
        a.json["servers"],
        json!({}),
        "not a game session: no server"
    );
    let start = a.json["startTime"].as_str().unwrap().to_owned();
    assert_eq!(start.len(), 24, "three fraction digits: {start}");
    // POST merges like PUT; the revision is the ETag.
    let a = pg
        .call(
            &alice,
            Method::POST,
            SESSIONDIRECTORY,
            &path
                .to_uppercase()
                .replace("/SERVICECONFIGS/", "/serviceconfigs/"),
            json!({"properties": {"custom": {"k": null, "j": 2}}, "startTime": "forged"}),
        )
        .await;
    assert_eq!(
        (a.status, a.headers[header::ETAG].clone()),
        (StatusCode::OK, "\"2\"".parse().unwrap())
    );
    assert_eq!(a.json["properties"]["custom"], json!({"j": 2}));
    assert_eq!(a.json["startTime"], start.as_str(), "server fields stay");
    let a = pg
        .call(&alice, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(
        (a.status, a.json["properties"]["custom"]["j"].clone()),
        (StatusCode::OK, json!(2))
    );
    // Owner-only: for Bob the session does not exist, and his write changes nothing.
    let a = pg
        .call(&bob, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::NOT_FOUND, json!({}))
    );
    let a = pg
        .call(
            &bob,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"properties": {"custom": {"evil": 1}}}),
        )
        .await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::NOT_FOUND, json!({}))
    );
    let a = pg
        .call(&bob, Method::DELETE, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    let a = pg
        .call(&alice, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(a.json["properties"]["custom"], json!({"j": 2}));
    // Bodies: not an object, empty (an empty merge).
    let bad = send(
        &pg.state,
        &Settings::default(),
        &alice.ticket,
        None,
        Method::PUT,
        SESSIONDIRECTORY,
        &path,
        b"[1]",
    )
    .await
    .unwrap();
    assert_eq!(
        (bad.status, bad.code()),
        (StatusCode::BAD_REQUEST, "REQUEST.INVALID")
    );
    assert_eq!(bad.json["details"]["field"], "body");
    let empty = send(
        &pg.state,
        &Settings::default(),
        &alice.ticket,
        None,
        Method::PUT,
        SESSIONDIRECTORY,
        &path,
        b"",
    )
    .await
    .unwrap();
    assert_eq!(
        (empty.status, empty.headers[header::ETAG].clone()),
        (StatusCode::OK, "\"3\"".parse().unwrap())
    );
    // A merged document over 128 KiB is refused and nothing changes.
    let blob = "x".repeat(70 << 10);
    let a = pg
        .call(
            &alice,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"properties": {"custom": {"a": blob}}}),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    let a = pg
        .call(
            &alice,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"properties": {"custom": {"b": blob}}}),
        )
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::PAYLOAD_TOO_LARGE, "REQUEST.TOO_LARGE")
    );
    let a = pg
        .call(&alice, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert!(a.json["properties"]["custom"].get("b").is_none());
    // What Postgres would refuse or grow is the caller's 400 or 413, never a 5xx: U+0000 in a
    // string or a key, and 900 copies of 1e308 (5 KB of body, 280 KB once written out).
    for (body, status) in [
        (
            json!({"properties": {"custom": {"n": "a\u{0}b"}}}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"properties": {"custom": {"\u{0}": 1}}}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"properties": {"custom": {"n": vec![json!(1e308); 900]}}}),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let a = pg
            .call(&alice, Method::PUT, SESSIONDIRECTORY, &path, body)
            .await;
        assert_eq!(a.status, status, "{}", a.json);
    }
    // `me: null` leaves; DELETE ends; then the session is gone for its owner too.
    let a = pg
        .call(
            &alice,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"members": {"me": null}}),
        )
        .await;
    assert_eq!(a.json["membersInfo"]["count"], 0);
    let a = pg
        .call(&alice, Method::DELETE, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
    let a = pg
        .call(&alice, Method::DELETE, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "idempotent");
    let a = pg
        .call(&alice, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    // Ended, the name is free: Bob creates it anew.
    let a = pg
        .call(&bob, Method::PUT, SESSIONDIRECTORY, &path, json!({}))
        .await;
    assert_eq!(
        (a.status, a.headers[header::ETAG].clone()),
        (StatusCode::CREATED, "\"1\"".parse().unwrap())
    );
}

#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn a_hosted_world_and_a_granted_joiner_meet_through_smartmatch() {
    let pg = pg().await;
    let host = pg.player("free", "203.0.113.5").await;
    let world = pg.host(&host, &["192.168.1.40"], vec![]).await;
    // The host's game: its ticket session gets its own relay at once.
    let a = pg.ticket_session(&host, "host-ticket").await;
    assert_eq!(a.status, StatusCode::CREATED);
    let entry = &a.json["servers"]["gameServerPlatform"];
    assert_eq!(address_of(entry), "fh2inf:127.0.0.1:5101");
    assert_eq!(entry["properties"]["custom"]["hostName"], "127.0.0.1");
    assert_eq!(entry["properties"]["custom"]["status"], "Active");
    assert_eq!(entry["properties"]["custom"]["datacenter"], doc::LOCATION);
    let a = pg.ticket(&host, "host-ticket").await;
    assert_eq!(a.status, StatusCode::OK);
    let ticket_id = a.json["ticketId"].as_str().unwrap().to_owned();
    assert_eq!(a.json["waitTime"], 0);
    let t = pg
        .call(
            &host,
            Method::GET,
            MOMATCH,
            &format!("{}/tickets/{ticket_id}", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(
        (t.status, t.json["ticketStatus"].clone()),
        (StatusCode::OK, json!("found"))
    );
    assert_eq!(t.json["ticketSessionRef"]["name"], "host-ticket");
    let target = t.json["targetSessionRef"]["name"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        target.starts_with("fh2inf-freeroam_qos-") && target.len() == 52,
        "{target}"
    );
    // The ticket session says found; the target session holds the world and the host, reserved.
    let s = pg
        .call(
            &host,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path("host-ticket"),
            Value::Null,
        )
        .await;
    let found = &s.json["servers"]["matchmaking"]["properties"]["system"];
    assert_eq!(
        (
            found["status"].clone(),
            found["targetSessionRef"]["name"].clone()
        ),
        (json!("found"), json!(target))
    );
    let s = pg
        .call(
            &host,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path(&target),
            Value::Null,
        )
        .await;
    assert_eq!(s.status, StatusCode::OK);
    assert_eq!(
        address_of(&s.json["servers"]["gameServerPlatform"]),
        "fh2inf:127.0.0.1:5101"
    );
    assert_eq!(s.json["constants"]["system"]["maxMembersCount"], 12);
    assert_eq!(
        s.json["constants"]["custom"]["hopperId"], 2,
        "from targetSessionConstants"
    );
    assert_eq!(s.json["members"]["0"]["reserved"], true);
    assert_eq!(
        s.json["members"]["0"]["constants"]["system"]["xuid"],
        host.xuid().to_string()
    );
    // The game then writes itself active in its target.
    let s = pg
        .call(
            &host,
            Method::PUT,
            SESSIONDIRECTORY,
            &session_path(&target),
            json!({"members": {"me": {"properties": {"system": {"active": true}}}}}),
        )
        .await;
    assert_eq!(
        (s.status, s.json["membersInfo"]["active"].clone()),
        (StatusCode::OK, json!(1))
    );
    // A joiner from elsewhere without a grant: no world yet (the Beta's 503).
    let joiner = pg.player("vip", "198.51.100.9").await;
    pg.ticket_session(&joiner, "joiner-ticket").await;
    let a = pg.ticket(&joiner, "joiner-ticket").await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::SERVICE_UNAVAILABLE, "SERVICE.UNAVAILABLE")
    );
    assert_eq!(
        a.headers[header::RETRY_AFTER],
        NO_WORLD_RETRY_SECS.to_string().as_str()
    );
    // The launcher's join: the grant's world at the host's public address.
    let granted = pg.join(&joiner, world).await;
    let JoinOutcome::Granted(seat) = &granted else {
        panic!("{granted:?}");
    };
    // The player walks to FREE ROAM ONLINE: 50 s of the seat's 60 are gone.
    sqlx::query(
        "UPDATE mm_grant SET issued_at = issued_at - interval '50 seconds', \
           expires_at = expires_at - interval '50 seconds', hold_until = hold_until - interval '50 seconds' WHERE id = $1",
    )
    .bind(seat.id)
    .execute(&pg.state.pool)
    .await
    .unwrap();
    let a = pg.ticket_session(&joiner, "joiner-ticket-2").await;
    assert_eq!(
        address_of(&a.json["servers"]["gameServerPlatform"]),
        "fh2inf:203.0.113.5:5101"
    );
    let a = pg.ticket(&joiner, "joiner-ticket-2").await;
    assert_eq!(a.status, StatusCode::OK);
    // Arrival (MATCHMAKING_API.md §4a): the seat that routed the ticket got 60 s more, once.
    let arrived = || async {
        let (arrived, left): (bool, f64) = sqlx::query_as(
            "SELECT arrived_at IS NOT NULL, extract(epoch FROM expires_at - clock_timestamp())::float8 \
             FROM mm_grant WHERE id = $1",
        )
        .bind(seat.id)
        .fetch_one(&pg.state.pool)
        .await
        .unwrap();
        (arrived, left)
    };
    let (yes, left) = arrived().await;
    assert!(yes && left > 50.0, "{left}");
    let joiner_ticket = a.json["ticketId"].as_str().unwrap().to_owned();
    let t = pg
        .call(
            &joiner,
            Method::GET,
            MOMATCH,
            &format!("{}/tickets/{joiner_ticket}", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    let target = t.json["targetSessionRef"]["name"]
        .as_str()
        .unwrap()
        .to_owned();
    let s = pg
        .call(
            &joiner,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path(&target),
            Value::Null,
        )
        .await;
    let custom = &s.json["servers"]["gameServerPlatform"]["properties"]["custom"];
    assert_eq!(
        custom["secureDeviceAddress"],
        doc::secure_device_address(&"203.0.113.5:5101".parse().unwrap())
    );
    assert_eq!(custom["hostName"], "203.0.113.5");
    // The host never sees the joiner's sessions or tickets.
    let a = pg
        .call(
            &host,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path(&target),
            Value::Null,
        )
        .await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    let a = pg
        .call(
            &host,
            Method::GET,
            MOMATCH,
            &format!("{}/tickets/{joiner_ticket}", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::NOT_FOUND, json!({}))
    );
    let a = pg
        .call(
            &host,
            Method::DELETE,
            MOMATCH,
            &format!("{}/tickets/{joiner_ticket}", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(
        a.status,
        StatusCode::NO_CONTENT,
        "answered, and nothing ended"
    );
    let t = pg
        .call(
            &joiner,
            Method::GET,
            MOMATCH,
            &format!("{}/tickets/{joiner_ticket}", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(t.status, StatusCode::OK, "the joiner's ticket lives on");
    // A joiner on the host's network: its first LAN address.
    let neighbour = pg.player("free", "203.0.113.5").await;
    pg.join(&neighbour, world).await;
    let a = pg.ticket_session(&neighbour, "n-ticket").await;
    assert_eq!(
        address_of(&a.json["servers"]["gameServerPlatform"]),
        "fh2inf:192.168.1.40:5101"
    );
    // gameserverms: the cluster names the caller's own session; fulfilled with the address.
    pg.call(
        &joiner,
        Method::PUT,
        SESSIONDIRECTORY,
        &session_path("cluster-s"),
        json!({"constants": {"custom": {"gameServerPlatform": {"location": "West Europe"}}}}),
    )
    .await;
    let c = pg
        .call(&joiner, Method::POST, GAMESERVERMS, "/titles/1368890819/clusters",
              json!({"sessionId": format!("/serviceconfigs/{SCID}/sessionTemplates/{TEMPLATE}/sessions/CLUSTER-S")}))
        .await;
    assert_eq!(
        (c.status, c.json["fulfillmentState"].clone()),
        (StatusCode::OK, json!("Fulfilled"))
    );
    assert_eq!(c.json["hostName"], "203.0.113.5");
    assert_eq!(
        c.json["secureDeviceAddress"],
        doc::secure_device_address(&"203.0.113.5:5101".parse().unwrap())
    );
    let s = pg
        .call(
            &joiner,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path("cluster-s"),
            Value::Null,
        )
        .await;
    assert_eq!(
        s.json["servers"]["gameServerPlatform"]["properties"]["custom"]["datacenter"],
        "West Europe"
    );
    assert_eq!(
        s.headers[header::ETAG],
        "\"2\"",
        "the cluster rewrote the caller's session"
    );
    // Another player's session name is never touched by a cluster request.
    let c = pg
        .call(
            &host,
            Method::POST,
            GAMESERVERMS,
            "/titles/1368890819/clusters",
            json!({"sessionId": "cluster-s"}),
        )
        .await;
    assert_eq!(c.status, StatusCode::OK);
    let s = pg
        .call(
            &joiner,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path("cluster-s"),
            Value::Null,
        )
        .await;
    assert_eq!(s.headers[header::ETAG], "\"2\"");
    // Tickets: stats count the live ones; DELETE ends the caller's; a ticket session that is not
    // the caller's is 404 {}; a body without its reference is invalid.
    let stats = pg
        .call(
            &host,
            Method::GET,
            MOMATCH,
            &format!("{}/stats", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(
        (stats.status, stats.json["population"].clone()),
        (StatusCode::OK, json!(2))
    );
    let d = pg
        .call(
            &host,
            Method::DELETE,
            MOMATCH,
            &format!("{}/tickets/{ticket_id}", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(d.status, StatusCode::NO_CONTENT);
    let g = pg
        .call(
            &host,
            Method::GET,
            MOMATCH,
            &format!("{}/tickets/{ticket_id}", hopper_path("FreeRoam_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(
        (g.status, g.json.clone()),
        (StatusCode::NOT_FOUND, json!({}))
    );
    let a = pg.ticket(&host, "joiner-ticket").await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::NOT_FOUND, json!({})),
        "another player's ticket session"
    );
    let a = pg
        .call(
            &host,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            json!({"scid": SCID}),
        )
        .await;
    assert_eq!(
        (a.status, a.json["details"]["field"].clone()),
        (StatusCode::BAD_REQUEST, json!("ticketSessionRef"))
    );
    // The host closes its world: nobody is routed to it any more.
    matchmaking::close_world(&pg.state.pool, &pg.mm(&host), world)
        .await
        .unwrap();
    let a = pg.ticket(&host, "host-ticket").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    // A configured fallback world then takes them.
    let fallback = Settings {
        auto: false,
        fallback: Some("192.0.2.77:5101".parse().unwrap()),
    };
    let a = pg
        .call_with(
            &fallback,
            &host,
            Method::PUT,
            SESSIONDIRECTORY,
            &session_path("fb"),
            json!({"constants": {"custom": {"gameServerPlatform": {}}}}),
        )
        .await;
    assert_eq!(
        address_of(&a.json["servers"]["gameServerPlatform"]),
        "fh2inf:192.0.2.77:5101"
    );
}

/// A car meet target's name: a fresh session of the caller's own, `fh2inf-carmeet_qos-<32 hex>`.
fn assert_carmeet_target(name: &str) {
    let hex = name
        .strip_prefix("fh2inf-carmeet_qos-")
        .unwrap_or_else(|| panic!("not a car meet target: {name}"));
    assert_eq!(hex.len(), 32, "{name}");
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
}

/// The game's own car meet ticket session (SAURON capture, 2026-10-30): its target constants
/// carry the meet.
async fn meet_session(pg: &Pg, p: &Player, name: &str) -> Answer {
    let gsp = json!({"gameVariantId": doc::GAME_VARIANT, "gsiSetId": doc::GSI_SET, "maxAllowedPlayers": 12});
    pg.call(
        p,
        Method::PUT,
        SESSIONDIRECTORY,
        &session_path(name),
        json!({
            "constants": {"system": {"version": 1, "visibility": "open"},
                          "custom": {"hopperId": 3, "EnableFullArbitration": true, "gameServerPlatform": gsp}},
            "properties": {"system": {"matchmaking": {"targetSessionConstants": {
                "custom": {"hopperId": 3, "EnableFullArbitration": true, "CarMeetLocation": 6,
                           "gameServerPlatform": gsp}}}}},
            "members": {"me": {"constants": {"system": {"xuid": p.xuid().to_string(), "initialize": true}},
                               "properties": {"system": {"active": true}}}}
        }),
    )
    .await
}

#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn a_car_meet_ticket_is_found_in_the_callers_world() {
    let pg = pg().await;
    let host = pg.player("free", "203.0.113.6").await;
    let world = pg.host(&host, &[], vec![]).await;
    let meets = Settings::default();
    let body = |name: &str| {
        json!({
            "preserveSession": "never", "giveUpDuration": 3_600_000,
            "ticketSessionRef": {"name": name, "templateName": TEMPLATE},
            "ticketAttributes": {"gsiSetId": doc::GSI_SET, "hopperId": 3, "skill": 25,
                                 "Location": 6, "ParentBucket": 3}
        })
    };
    assert_eq!(
        meet_session(&pg, &host, "meet-ticket").await.status,
        StatusCode::CREATED
    );
    // Matched at the POST, its target in the caller's world with the meet's constants; the POST
    // answers only the ticket.
    let a = pg
        .call_with(
            &meets,
            &host,
            Method::POST,
            MOMATCH,
            &hopper_path("CarMeet_qos"),
            body("meet-ticket"),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["waitTime"], 0);
    let ticket_id = a.json["ticketId"].as_str().unwrap().to_owned();
    // The ticket session stays as the game wrote it: no `servers.matchmaking` (a found one made
    // the game drop the ticket without polling it, hook logs 2026-10-31).
    let ts = pg
        .call(
            &host,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path("meet-ticket"),
            Value::Null,
        )
        .await;
    assert_eq!(ts.status, StatusCode::OK, "{}", ts.json);
    assert_eq!(
        ts.json["servers"]["matchmaking"],
        Value::Null,
        "{}",
        ts.json
    );
    // The GET of the ticket carries the match.
    let t = pg
        .call(
            &host,
            Method::GET,
            MOMATCH,
            &format!("{}/tickets/{ticket_id}", hopper_path("CarMeet_qos")),
            Value::Null,
        )
        .await;
    assert_eq!(t.status, StatusCode::OK, "{}", t.json);
    assert_eq!(t.json["ticketStatus"], "found");
    assert_eq!(t.json["ticketAttributes"]["Location"], 6);
    assert_eq!(t.json["ticketSessionRef"]["templateName"], TEMPLATE);
    assert_eq!(
        t.json["targetSessionRef"]["templateName"],
        "defaultTemplate"
    );
    let target = t.json["targetSessionRef"]["name"]
        .as_str()
        .unwrap()
        .to_owned();
    // A target of the caller's own (live 2026-11-02: a shared, pre-activated meet session made
    // the host's game skip joining and the joiner's show "no longer exists").
    assert_carmeet_target(&target);
    // Not in the ticket session's template.
    let s = pg
        .call(
            &host,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path(&target),
            Value::Null,
        )
        .await;
    assert_eq!(s.status, StatusCode::NOT_FOUND, "{}", s.json);
    let s = pg
        .call(
            &host,
            Method::GET,
            SESSIONDIRECTORY,
            &meet_path(&target),
            Value::Null,
        )
        .await;
    assert_eq!(s.status, StatusCode::OK, "{}", s.json);
    assert_eq!(
        address_of(&s.json["servers"]["gameServerPlatform"]),
        "fh2inf:127.0.0.1:5101"
    );
    assert_eq!(s.json["constants"]["custom"]["CarMeetLocation"], 6);
    assert_eq!(s.json["constants"]["custom"]["hopperId"], 3);
    // As free roam's target: only the caller, reserved and inactive (the game joins it).
    assert_eq!(s.json["membersInfo"]["count"], 1, "only the caller");
    assert_eq!(s.json["members"]["0"]["reserved"], true);
    assert_eq!(
        s.json["members"]["0"]["properties"]["system"]["active"],
        false
    );
    assert_eq!(
        s.json["members"]["0"]["constants"]["system"]["xuid"],
        json!(host.xuid().to_string())
    );
    // The host's game joins its target.
    let w = pg
        .call(
            &host,
            Method::PUT,
            SESSIONDIRECTORY,
            &meet_path(&target),
            json!({"members": {"me": {"properties": {"system": {"active": true}}}}}),
        )
        .await;
    assert_eq!(w.status, StatusCode::OK, "{}", w.json);
    assert_eq!(w.json["membersInfo"]["active"], 1);
    // A seated player at the same car meet gets a different target of its own, in the same world.
    let guest = pg.player("free", "198.51.100.41").await;
    let JoinOutcome::Granted(_) = pg.join(&guest, world).await else {
        panic!("seat");
    };
    meet_session(&pg, &guest, "guest-meet").await;
    let a = pg
        .call_with(
            &meets,
            &guest,
            Method::POST,
            MOMATCH,
            &hopper_path("CarMeet_qos"),
            body("guest-meet"),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    let gt = pg
        .call(
            &guest,
            Method::GET,
            MOMATCH,
            &format!(
                "{}/tickets/{}",
                hopper_path("CarMeet_qos"),
                a.json["ticketId"].as_str().unwrap()
            ),
            Value::Null,
        )
        .await;
    assert_eq!(gt.json["ticketStatus"], "found");
    assert_eq!(
        gt.json["targetSessionRef"]["templateName"],
        "defaultTemplate"
    );
    let guest_target = gt.json["targetSessionRef"]["name"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_carmeet_target(&guest_target);
    assert_ne!(guest_target, target, "one target per player");
    let gs = pg
        .call(
            &guest,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path("guest-meet"),
            Value::Null,
        )
        .await;
    assert_eq!(gs.json["servers"]["matchmaking"], Value::Null);
    let g = pg
        .call(
            &guest,
            Method::GET,
            SESSIONDIRECTORY,
            &meet_path(&guest_target),
            Value::Null,
        )
        .await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.json);
    assert_eq!(g.json["membersInfo"]["count"], 1, "only the guest");
    assert_eq!(g.json["members"]["0"]["reserved"], true);
    assert_eq!(
        g.json["members"]["0"]["properties"]["system"]["active"],
        false
    );
    assert_eq!(
        g.json["members"]["0"]["constants"]["system"]["xuid"],
        json!(guest.xuid().to_string())
    );
    assert_eq!(
        address_of(&g.json["servers"]["gameServerPlatform"]),
        "fh2inf:203.0.113.6:5101",
        "the guest's own route to the host's world"
    );
    // The host's target is not the guest's.
    let h = pg
        .call(
            &guest,
            Method::GET,
            SESSIONDIRECTORY,
            &meet_path(&target),
            Value::Null,
        )
        .await;
    assert_eq!(h.status, StatusCode::NOT_FOUND, "{}", h.json);
    // Online Road Trip: free roam with another lobby type. Each player's target is its own and
    // names the host's world (the world server runs the Road Trip).
    let mut targets = Vec::new();
    for (p, name) in [(&host, "host-rt"), (&guest, "guest-rt")] {
        meet_session(&pg, p, name).await;
        let mut rt = body(name);
        rt["ticketAttributes"] = json!({"hopperId": 1, "NewPlayer": 0, "ParentBucket": 0});
        let a = pg
            .call_with(
                &meets,
                p,
                Method::POST,
                MOMATCH,
                &hopper_path("RoadTrip_qos"),
                rt,
            )
            .await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.json);
        let t = pg
            .call(
                p,
                Method::GET,
                MOMATCH,
                &format!(
                    "{}/tickets/{}",
                    hopper_path("RoadTrip_qos"),
                    a.json["ticketId"].as_str().unwrap()
                ),
                Value::Null,
            )
            .await;
        let name = t.json["targetSessionRef"]["name"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(name, freeroam::roadtrip_name(world));
        assert_eq!(t.json["targetSessionRef"]["templateName"], TEMPLATE);
        // Unchanged for the Road Trip: its ticket session says found at the POST.
        let ts = pg
            .call(
                p,
                Method::GET,
                SESSIONDIRECTORY,
                &session_path(t.json["ticketSessionRef"]["name"].as_str().unwrap()),
                Value::Null,
            )
            .await;
        assert_eq!(
            ts.json["servers"]["matchmaking"]["properties"]["system"]["targetSessionRef"]["name"],
            json!(name.as_str())
        );
        let s = pg
            .call(
                p,
                Method::GET,
                SESSIONDIRECTORY,
                &session_path(&name),
                Value::Null,
            )
            .await;
        assert_eq!(s.status, StatusCode::OK, "{}", s.json);
        assert_eq!(s.json["membersInfo"]["count"], 1, "only the caller");
        targets.push((name, address_of(&s.json["servers"]["gameServerPlatform"])));
    }
    assert_ne!(targets[0].0, targets[1].0, "one target per player");
    assert_eq!(
        targets[0].1, "fh2inf:127.0.0.1:5101",
        "the host's own route"
    );
    assert_eq!(
        targets[1].1, "fh2inf:203.0.113.6:5101",
        "the guest's route to the host's world"
    );
    // Another location in the same world is matched too.
    meet_session(&pg, &guest, "guest-meet-7").await;
    let mut other = body("guest-meet-7");
    other["ticketAttributes"]["Location"] = json!(7);
    let a = pg
        .call_with(
            &meets,
            &guest,
            Method::POST,
            MOMATCH,
            &hopper_path("CarMeet_qos"),
            other,
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    // A player in no world gets the Beta's 503, never an automatic choice.
    let lone = pg.player("free", "198.51.100.40").await;
    meet_session(&pg, &lone, "lone-meet").await;
    let auto_meets = Settings {
        auto: true,
        ..meets
    };
    let a = pg
        .call_with(
            &auto_meets,
            &lone,
            Method::POST,
            MOMATCH,
            &hopper_path("CarMeet_qos"),
            body("lone-meet"),
        )
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::SERVICE_UNAVAILABLE, "SERVICE.UNAVAILABLE")
    );
    matchmaking::close_world(&pg.state.pool, &pg.mm(&host), world)
        .await
        .unwrap();
}

/// A reused ticket session that still carries an earlier FreeRoam ticket's `servers.matchmaking`
/// (a stale "found" at the old target): a car meet ticket's POST removes it, so the game never
/// reads the old match (review of the car meet fix, 2026-11-02).
#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn a_car_meet_ticket_clears_a_stale_found_on_a_reused_ticket_session() {
    let pg = pg().await;
    let host = pg.player("free", "203.0.113.6").await;
    let world = pg.host(&host, &[], vec![]).await;
    assert_eq!(
        meet_session(&pg, &host, "meet-reuse").await.status,
        StatusCode::CREATED
    );
    // As an earlier FreeRoam ticket on this session left it.
    let mut stale = json!({});
    doc::set_found(
        &mut stale,
        &json!({"scid": SCID, "templateName": TEMPLATE, "name": "old-target"}),
    );
    sqlx::query(
        "UPDATE freeroam_session SET doc = jsonb_set(doc, '{servers}', \
           COALESCE(doc->'servers', '{}'::jsonb) || $3::jsonb) WHERE game = $1 AND name = $2",
    )
    .bind(&pg.game)
    .bind("meet-reuse")
    .bind(stale["servers"].to_string())
    .execute(&pg.state.pool)
    .await
    .unwrap();
    let path = session_path("meet-reuse");
    let get_session = || pg.call(&host, Method::GET, SESSIONDIRECTORY, &path, Value::Null);
    let before = get_session().await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.json);
    assert_eq!(
        before.json["servers"]["matchmaking"]["properties"]["system"]["status"], "found",
        "{}",
        before.json
    );
    let a = pg
        .call_with(
            &Settings::default(),
            &host,
            Method::POST,
            MOMATCH,
            &hopper_path("CarMeet_qos"),
            json!({
                "preserveSession": "never", "giveUpDuration": 3_600_000,
                "ticketSessionRef": {"name": "meet-reuse", "templateName": TEMPLATE},
                "ticketAttributes": {"gsiSetId": doc::GSI_SET, "hopperId": 3, "skill": 25,
                                     "Location": 6, "ParentBucket": 3}
            }),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    let after = get_session().await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.json);
    assert_eq!(
        after.json["servers"]["matchmaking"],
        Value::Null,
        "{}",
        after.json
    );
    // The match is still the GET of the ticket's, at a target of the caller's own.
    let t = pg
        .call(
            &host,
            Method::GET,
            MOMATCH,
            &format!(
                "{}/tickets/{}",
                hopper_path("CarMeet_qos"),
                a.json["ticketId"].as_str().unwrap()
            ),
            Value::Null,
        )
        .await;
    assert_eq!(t.status, StatusCode::OK, "{}", t.json);
    assert_eq!(t.json["ticketStatus"], "found");
    assert_eq!(
        t.json["targetSessionRef"]["templateName"],
        "defaultTemplate"
    );
    assert_carmeet_target(t.json["targetSessionRef"]["name"].as_str().unwrap());
    matchmaking::close_world(&pg.state.pool, &pg.mm(&host), world)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn a_car_meet_invite_session_is_the_hosts_and_its_members_join_it() {
    let pg = pg().await;
    let host = pg.player("free", "203.0.113.7").await;
    let world = pg.host(&host, &[], vec![]).await;
    let joiner = pg.player("free", "198.51.100.17").await;
    let JoinOutcome::Granted(_) = pg.join(&joiner, world).await else {
        panic!("seat");
    };
    let stranger = pg.player("free", "198.51.100.18").await;
    let m = pg.mm(&host);
    let host_caller = freeroam::Caller {
        game: m.game,
        profile_id: m.profile_id,
        account_id: m.account_id,
        session_id: m.session_id,
        xuid: m.xuid,
        tier: m.tier,
        mode: m.mode,
        ip: None,
    };
    let invited = |p: &Player| super::invite::Invited {
        xuid: p.xuid(),
        gamertag: Some("Driver#00001".into()),
    };
    let key = super::invite::create(
        &pg.state.pool,
        &host_caller,
        world,
        &[invited(&host), invited(&joiner)],
    )
    .await
    .unwrap();
    assert!(key.name.starts_with("fh2inf-invite-"), "{}", key.name);
    assert!(
        key.name.len() <= super::invite::INVITE_NAME_MAX,
        "{}",
        key.name
    );
    let path = format!(
        "/serviceconfigs/{}/sessionTemplates/{}/sessions/{}",
        key.scid, key.template, key.name
    );
    // The host reads its own session: its own world on loopback, both members inactive.
    let h = pg
        .call(&host, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(h.status, StatusCode::OK);
    assert_eq!(
        address_of(&h.json["servers"]["gameServerPlatform"]),
        "fh2inf:127.0.0.1:5101"
    );
    assert_eq!(h.json["membersInfo"]["count"], 2);
    assert_eq!(h.json["membersInfo"]["active"], 0);
    assert_eq!(h.json["constants"]["custom"]["hopperId"], 0);
    // Invitees are inactive but not reserved (a reserved joiner takes the invite-accept path).
    for m in h.json["members"].as_object().unwrap().values() {
        assert_eq!(m["reserved"], false);
    }
    // The joiner reads it at its own route to the host's world.
    let j = pg
        .call(&joiner, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(j.status, StatusCode::OK);
    assert_eq!(
        address_of(&j.json["servers"]["gameServerPlatform"]),
        "fh2inf:203.0.113.7:5101"
    );
    // ... and marks itself active; anything else of its body is ignored.
    let w = pg
        .call(
            &joiner,
            Method::PUT,
            SESSIONDIRECTORY,
            &path,
            json!({"constants": {"custom": {"hopperId": 9}},
                   "members": {"me": {"gamertag": "Someone#00002", "deviceToken": "X",
                       "constants": {"custom": {"big": "x"}},
                       "properties": {"system": {"active": true}}}}}),
        )
        .await;
    assert_eq!(w.status, StatusCode::OK, "{}", w.json);
    assert_eq!(w.json["membersInfo"]["active"], 1);
    assert_eq!(w.json["constants"]["custom"]["hopperId"], 0);
    // Its name and device token stay the server's; only its state was taken.
    let slot = w.json["members"]
        .as_object()
        .unwrap()
        .values()
        .find(|m| m["constants"]["system"]["xuid"] == json!(joiner.xuid().to_string()))
        .unwrap()
        .clone();
    assert_ne!(slot["gamertag"], "Someone#00002");
    assert_eq!(slot["deviceToken"].as_str().unwrap().len(), 32);
    assert!(slot["constants"]["custom"].get("big").is_none());
    let h = pg
        .call(&host, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(h.json["membersInfo"]["active"], 1, "the host sees it");
    // A stranger never sees it, nor writes it.
    for method in [Method::GET, Method::PUT] {
        let a = pg
            .call(&stranger, method, SESSIONDIRECTORY, &path, json!({}))
            .await;
        assert_eq!(
            (a.status, a.json.clone()),
            (StatusCode::NOT_FOUND, json!({}))
        );
    }
    // A new invite of the world ends the earlier one.
    let next = super::invite::create(&pg.state.pool, &host_caller, world, &[invited(&joiner)])
        .await
        .unwrap();
    assert_ne!(next.name, key.name);
    let old = pg
        .call(&joiner, Method::GET, SESSIONDIRECTORY, &path, Value::Null)
        .await;
    assert_eq!(old.status, StatusCode::NOT_FOUND);
    matchmaking::close_world(&pg.state.pool, &pg.mm(&host), world)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn the_automatic_choice_follows_matchmaking_and_takes_a_grant() {
    let pg = pg().await;
    let auto = Settings {
        auto: true,
        fallback: None,
    };
    let vip_host = pg.player("vip", "203.0.113.20").await;
    let supporter_world = pg.host(&vip_host, &[], vec![]).await;
    let ticket = |name: &str| json!({"ticketSessionRef": {"name": name, "scid": SCID, "templateName": TEMPLATE}});
    // OD-129: a Supporter-only world is everyone's; a free player reaches it automatically, with
    // a grant taken for it.
    let early = pg.player("free", "198.51.100.33").await;
    pg.call_with(
        &auto,
        &early,
        Method::PUT,
        SESSIONDIRECTORY,
        &session_path("e-t"),
        json!({}),
    )
    .await;
    let a = pg
        .call_with(
            &auto,
            &early,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            ticket("e-t"),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    let world: Uuid = sqlx::query_scalar(
        "SELECT world_id FROM mm_grant WHERE profile_id = $1 AND state IN ('issued', 'used')",
    )
    .bind(early.ticket.live.profile_id)
    .fetch_one(&pg.state.pool)
    .await
    .unwrap();
    assert_eq!(world, supporter_world);
    // With it closed, a free player has no world until a free host opens one.
    matchmaking::close_world(&pg.state.pool, &pg.mm(&vip_host), supporter_world)
        .await
        .unwrap();
    let free = pg.player("free", "198.51.100.30").await;
    pg.call_with(
        &auto,
        &free,
        Method::PUT,
        SESSIONDIRECTORY,
        &session_path("f-t"),
        json!({}),
    )
    .await;
    let a = pg
        .call_with(
            &auto,
            &free,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            ticket("f-t"),
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    // A free host's world: the free player is sent there, with a grant taken for it.
    let free_host = pg.player("free", "203.0.113.21").await;
    let free_world = pg.host(&free_host, &[], vec![]).await;
    // OD-150: a world running Online Road Trip is never the automatic free-roam choice.
    let set_activity = |a: &'static str| {
        sqlx::query("UPDATE world SET activity = $2 WHERE id = $1")
            .bind(free_world)
            .bind(a)
            .execute(&pg.state.pool)
    };
    set_activity("roadtrip").await.unwrap();
    pg.call_with(
        &auto,
        &free,
        Method::PUT,
        SESSIONDIRECTORY,
        &session_path("f-rt"),
        json!({}),
    )
    .await;
    let a = pg
        .call_with(
            &auto,
            &free,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            ticket("f-rt"),
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{}", a.json);
    set_activity("freeroam").await.unwrap();
    // Sessions and clusters never choose automatically: no address goes out without a grant.
    let a = pg
        .call_with(
            &auto,
            &free,
            Method::PUT,
            SESSIONDIRECTORY,
            &session_path("f-gsp"),
            json!({"constants": {"custom": {"gameServerPlatform": {}}}}),
        )
        .await;
    assert_eq!(
        a.json["servers"]["gameServerPlatform"]["properties"]["custom"]["secureDeviceAddress"],
        doc::PLACEHOLDER_ADDRESS
    );
    let c = pg
        .call_with(
            &auto,
            &free,
            Method::POST,
            GAMESERVERMS,
            "/titles/1368890819/clusters",
            json!({"sessionId": "f-gsp"}),
        )
        .await;
    assert_eq!(c.json["hostName"], doc::PLACEHOLDER_HOST);
    // A refused ticket (its session is not the caller's) takes no grant.
    let a = pg
        .call_with(
            &auto,
            &free,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            ticket("no-such-session"),
        )
        .await;
    assert_eq!(
        (a.status, a.json.clone()),
        (StatusCode::NOT_FOUND, json!({}))
    );
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_grant WHERE profile_id = $1")
        .bind(free.ticket.live.profile_id)
        .fetch_one(&pg.state.pool)
        .await
        .unwrap();
    assert_eq!(grants, 0);
    let a = pg
        .call_with(
            &auto,
            &free,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            ticket("f-t"),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    let grant: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT world_id, state FROM mm_grant WHERE profile_id = $1 AND state IN ('issued', 'used')",
    )
    .bind(free.ticket.live.profile_id)
    .fetch_optional(&pg.state.pool)
    .await
    .unwrap();
    assert_eq!(
        grant,
        Some((free_world, "issued".to_owned())),
        "the lock holds"
    );
    // A Supporter is sent to the open (free host's) world as well.
    let vip = pg.player("vip", "198.51.100.31").await;
    pg.announce(&free_host, free_world, &[], vec![]).await;
    pg.call_with(
        &auto,
        &vip,
        Method::PUT,
        SESSIONDIRECTORY,
        &session_path("v-t"),
        json!({}),
    )
    .await;
    let a = pg
        .call_with(
            &auto,
            &vip,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            ticket("v-t"),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    let world: Uuid = sqlx::query_scalar(
        "SELECT world_id FROM mm_grant WHERE profile_id = $1 AND state IN ('issued', 'used')",
    )
    .bind(vip.ticket.live.profile_id)
    .fetch_one(&pg.state.pool)
    .await
    .unwrap();
    assert_eq!(world, free_world);
    // Off (the default), nobody is sent anywhere without a grant.
    let other = pg.player("vip", "198.51.100.32").await;
    pg.call(
        &other,
        Method::PUT,
        SESSIONDIRECTORY,
        &session_path("o-t"),
        json!({}),
    )
    .await;
    let a = pg
        .call(
            &other,
            Method::POST,
            MOMATCH,
            &hopper_path("FreeRoam_qos"),
            ticket("o-t"),
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
#[ignore = "needs Postgres: FI_TEST_DATABASE_URL"]
async fn two_instances_answer_from_one_state() {
    let one = pg().await;
    let two = Pg {
        state: AppState {
            pool: runtime_pool().await,
            identity: None,
        },
        game: one.game.clone(),
    };
    let host = one.player("vip", "203.0.113.40").await;
    one.host(&host, &[], vec![]).await;
    // The ticket session on instance one, the ticket on instance two, read back on one.
    assert_eq!(
        one.ticket_session(&host, "x-ticket").await.status,
        StatusCode::CREATED
    );
    let a = two.ticket(&host, "x-ticket").await;
    assert_eq!(a.status, StatusCode::OK);
    let t = one
        .call(
            &host,
            Method::GET,
            MOMATCH,
            &format!(
                "{}/tickets/{}",
                hopper_path("FreeRoam_qos"),
                a.json["ticketId"].as_str().unwrap()
            ),
            Value::Null,
        )
        .await;
    assert_eq!(t.status, StatusCode::OK);
    let target = t.json["targetSessionRef"]["name"].as_str().unwrap();
    let s = two
        .call(
            &host,
            Method::GET,
            SESSIONDIRECTORY,
            &session_path(target),
            Value::Null,
        )
        .await;
    assert_eq!(
        address_of(&s.json["servers"]["gameServerPlatform"]),
        "fh2inf:127.0.0.1:5101"
    );
    let s = one
        .call(
            &host,
            Method::PUT,
            SESSIONDIRECTORY,
            &session_path(target),
            json!({"members": {"me": {"properties": {"system": {"active": true}}}}}),
        )
        .await;
    assert_eq!(
        s.headers[header::ETAG],
        "\"2\"",
        "instance one continues instance two's revision"
    );
}

// ---------------------------------------------------------------------------------------------
// Replay of the Beta's recorded free-roam traffic (opt-in).

/// Counts of one replay, by host.
#[derive(Debug, Default)]
struct Tally {
    records: usize,
    same: usize,
    differ: BTreeMap<String, usize>,
}

#[tokio::test]
#[ignore = "needs Postgres (FI_TEST_DATABASE_URL) and FI_FH2_CAPTURES_DIR"]
async fn replays_the_betas_recorded_free_roam_traffic() {
    let Ok(dir) = std::env::var("FI_FH2_CAPTURES_DIR") else {
        eprintln!("FI_FH2_CAPTURES_DIR is not set: replay skipped");
        return;
    };
    // The Beta's captures ran with the host's own world or `--freeroam-world`: a fixed world here.
    let settings = Settings {
        auto: false,
        fallback: Some("127.0.0.1:5101".parse::<SocketAddr>().unwrap()),
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut tally: BTreeMap<&'static str, Tally> = BTreeMap::new();
    let mut builtin = (0usize, 0usize);
    let mut stateless_bodies = (0usize, 0usize);
    for file in &files {
        // One capture file is one run of the Beta, whose state lived in memory: a game of its own
        // here. Each recorded caller (an opaque hash of its token) is a fresh test player, and
        // server-minted names (ticket ids, target sessions) map from the recording's to ours,
        // whole path segments only.
        let pg = pg().await;
        let mut players: HashMap<[u8; 32], Player> = HashMap::new();
        let mut renames: HashMap<String, String> = HashMap::new();
        let mut last_touch: HashMap<String, i64> = HashMap::new();
        let text = std::fs::read_to_string(file).unwrap();
        for line in text.lines() {
            let Ok(r) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let host = r["host"].as_str().unwrap_or("").to_ascii_lowercase();
            let Some(&host) = super::HOSTS.iter().find(|h| **h == host) else {
                continue;
            };
            let token = r["request_headers"]
                .as_object()
                .and_then(|h| {
                    h.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("x-fh2-token"))
                })
                .and_then(|(_, v)| v.as_str())
                .unwrap_or("anonymous");
            let who = fi_auth::sha256(token.as_bytes());
            let player = match players.entry(who) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(pg.player("vip", "198.51.100.200").await),
            };
            let path = r["path"]
                .as_str()
                .unwrap_or("/")
                .split('/')
                .map(|seg| {
                    renames
                        .get(&seg.to_ascii_lowercase())
                        .map_or(seg, String::as_str)
                })
                .collect::<Vec<_>>()
                .join("/");
            let body = r["request_body_b64"]
                .as_str()
                .and_then(fi_auth::b64::std_decode)
                .unwrap_or_default();
            let method =
                Method::from_bytes(r["method"].as_str().unwrap_or("GET").as_bytes()).unwrap();
            // The recording's clock: a session idle for over 30 minutes there has expired here
            // too (the replay does not wait).
            let at = r["time_ms"].as_i64().unwrap_or(0);
            let session = (host == SESSIONDIRECTORY)
                .then(|| path.rsplit('/').next().unwrap_or("").to_ascii_lowercase())
                .filter(|n| {
                    path.split('/').filter(|s| !s.is_empty()).count() == 6 && !n.is_empty()
                });
            let touched = match (host, &session) {
                (SESSIONDIRECTORY, Some(name)) => Some(name.clone()),
                (MOMATCH, _) if method == Method::POST => {
                    serde_json::from_slice::<Value>(&body).ok().and_then(|b| {
                        b["ticketSessionRef"]["name"]
                            .as_str()
                            .map(str::to_ascii_lowercase)
                    })
                }
                _ => None,
            };
            if let Some(name) = &touched
                && last_touch
                    .get(name)
                    .is_some_and(|prev| at - prev > freeroam::SESSION_TTL_SECS * 1_000)
            {
                sqlx::query(
                    "UPDATE freeroam_session SET expires_at = clock_timestamp() WHERE game = $1 AND name = $2",
                )
                .bind(&pg.game)
                .bind(name)
                .execute(&pg.state.pool)
                .await
                .unwrap();
            }
            let ours = send(
                &pg.state,
                &settings,
                &player.ticket,
                Some(player.ip),
                method.clone(),
                host,
                &path,
                &body,
            )
            .await
            .expect("a free-roam host is always answered");
            let recorded_status = r["response_status"].as_u64().unwrap_or(0);
            let recorded: Value = r["response_body_b64"]
                .as_str()
                .and_then(fi_auth::b64::std_decode)
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            // Learn the server-minted names.
            if host == MOMATCH && ours.status == StatusCode::OK {
                for (a, b) in [
                    (&recorded["ticketId"], &ours.json["ticketId"]),
                    (
                        &recorded["targetSessionRef"]["name"],
                        &ours.json["targetSessionRef"]["name"],
                    ),
                ] {
                    if let (Some(a), Some(b)) = (a.as_str(), b.as_str()) {
                        renames.insert(a.to_ascii_lowercase(), b.to_owned());
                    }
                }
                if let Some(target) = ours.json["targetSessionRef"]["name"].as_str() {
                    last_touch.entry(target.to_owned()).or_insert(at);
                }
            }
            if let Some(name) = touched
                && (ours.status.is_success() || host == MOMATCH)
            {
                last_touch.insert(name, at);
            }
            let t = tally.entry(host).or_default();
            t.records += 1;
            let source = r["response_source"].as_str().unwrap_or("");
            let is_builtin = source.starts_with("builtin:freeroam");
            if u64::from(ours.status.as_u16()) == recorded_status {
                t.same += 1;
                if is_builtin {
                    builtin.0 += 1;
                }
            } else {
                *t.differ
                    .entry(format!(
                        "{method} {} recorded {recorded_status} ({}) -> {}",
                        subject(host, &path),
                        if is_builtin { "builtin" } else { "stub" },
                        ours.status.as_u16()
                    ))
                    .or_default() += 1;
                if is_builtin {
                    builtin.1 += 1;
                }
            }
            if host == GAMESERVERDS && is_builtin {
                if recorded == ours.json {
                    stateless_bodies.0 += 1;
                } else {
                    stateless_bodies.1 += 1;
                }
            }
        }
        pg.state.pool.close().await;
    }
    let records: usize = tally.values().map(|t| t.records).sum();
    eprintln!("replay: {} files, {records} free-roam records", files.len());
    for (host, t) in &tally {
        eprintln!("  {host}: {} records, {} same status", t.records, t.same);
        for (what, n) in &t.differ {
            eprintln!("    differ {n}: {what}");
        }
    }
    eprintln!(
        "  recorded with the Beta's free roam on: {} same status, {} differ; gameserverds bodies {} equal, {} differ",
        builtin.0, builtin.1, stateless_bodies.0, stateless_bodies.1
    );
    assert!(records > 0, "no free-roam records in {dir}");
    assert_eq!(stateless_bodies.1, 0, "gameserverds answers are the Beta's");
    for t in tally.values() {
        for what in t.differ.keys() {
            assert!(explained(what), "unexpected difference: {what}");
        }
    }
}

/// What a request is about, without its ids: the hopper of a momatch request; for a session,
/// whether it is a SmartMatch target of another hopper (`fh2inf-roadtrip_qos-…`).
fn subject(host: &str, path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    match host {
        MOMATCH => path.split('/').nth(4).unwrap_or("").to_owned(),
        SESSIONDIRECTORY
            if last.starts_with("fh2inf-") && !last.starts_with("fh2inf-freeroam_qos") =>
        {
            "other-hopper-target".to_owned()
        }
        _ => "-".to_owned(),
    }
}

/// The differences a replay may show, each by design: the recording ran with the Beta's free
/// roam off (stub answers); a hopper other than FreeRoam_qos (now `GAME.NOT_SERVED`, which the
/// hook turns into the 503 the game saw then), a ticket of one or its target session that were
/// therefore never made; a FreeRoam ticket the Beta refused for want of a world, which the
/// replay's fixed world grants.
fn explained(what: &str) -> bool {
    let other_hopper = (what.starts_with("POST ") || what.starts_with("GET "))
        && !what.contains(" FreeRoam_qos ")
        && !what.contains(" - ")
        && what.ends_with("-> 404");
    what.contains("(stub)")
        || other_hopper
        || what.contains(" other-hopper-target ")
        || what.starts_with("POST FreeRoam_qos recorded 503 (builtin) -> 200")
}
