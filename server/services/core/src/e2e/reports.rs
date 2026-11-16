//! Reports from the game (CORE_API.md §4.3; OD-63, register CR-L25) over fi-core's whole router:
//! `POST /launcher/v1/reports` with the launcher's access token and no Turnstile; every status and
//! code (202, the repeat and the replay, 400 per field and for oneself, 401, 404 for every way of
//! not having shared a named world, 202 without one from any player card (OD-193), 409 for a reused key, 429 past `report.game`, which a replay never
//! spends); the report in the social store with `source = game` and its world, its target stored
//! canonically like the web's, counted by the three-report rule with the web's, and shown so to
//! moderators. The shared worlds are made by matchmaking's own code
//! (`fi_db::matchmaking::host` and `join`) on live play sessions. Run with the rest of the e2e
//! suites (see `super`'s header).

use super::*;
use fi_db::matchmaking::{self, Caller, Endpoint, JoinInput, JoinOutcome, Settings};

/// A stack whose sign-in and account-creation limits (per client network: every player of a test
/// signs in from the test's one address) allow the players these suites make.
async fn roomy_stack() -> Stack {
    stack_with(Options {
        vars: vec![("FI_LOCAL_LIMIT_SCALE", "10".into())],
        ..Options::default()
    })
    .await
}

/// `p` as matchmaking sees it, on a live play session of its noob profile (one per player).
pub(super) async fn in_game(st: &Stack, p: &Player) -> Caller {
    let session = st.live_session(p, "active").await;
    let (profile_id, account_id, xuid): (Uuid, Uuid, i64) =
        sqlx::query_as("SELECT id, account_id, xuid FROM profile WHERE xuid = $1::bigint")
            .bind(&p.xuid)
            .fetch_one(&st.owner)
            .await
            .unwrap();
    Caller {
        game: "fh2".into(),
        profile_id,
        account_id,
        session_id: Uuid::parse_str(&session).unwrap(),
        xuid,
        tier: "vip".into(),
        mode: "noob".into(),
    }
}

/// Opens a world hosted by `host`.
pub(super) async fn open_world(st: &Stack, host: &Caller) -> Uuid {
    let world = Uuid::now_v7();
    matchmaking::host(
        &st.owner,
        host,
        world,
        Some(&Settings {
            name: "Synthetic cruise".into(),
            mode: host.mode.clone(),
            open_modes: None,
            max_players: 8,
            audience: "everyone".into(),
            max_rtt_ms: None,
            races: true,
            message: None,
            allow: None,
            mods: fi_db::matchmaking::ModSet::vanilla(),
            lobbies: Vec::new(),
        }),
        &Endpoint {
            ip: "198.51.100.7".into(),
            cell: None,
            region: None,
            port: Some(5101),
            local_port: Some(5101),
            lan: Some(vec![]),
        },
        None,
    )
    .await
    .unwrap();
    world
}

/// Seats `p` in `world` (a reserved seat).
pub(super) async fn seat(st: &Stack, p: &Caller, world: Uuid) {
    let out = matchmaking::join(
        &st.owner,
        p,
        &JoinInput {
            world_id: Some(world),
            code: None,
            ip: Some("198.51.100.9".into()),
            cell: None,
            hold_secs: matchmaking::DEFAULT_SEAT_HOLD_SECS,
            mods: fi_db::matchmaking::ModSet::vanilla(),
            activity: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(out, JoinOutcome::Granted(_)), "{out:?}");
}

/// The launcher of `p`.
fn me(p: &Player) -> Who<'_> {
    Who::Launcher(Some(p.access.as_str()))
}

fn body(target: &Player, world: Option<Uuid>) -> Value {
    let mut b = json!({"game": "fh2", "xuid": target.xuid, "reason": "cheating"});
    if let Some(w) = world {
        b["worldId"] = json!(w.to_string());
    }
    b
}

async fn report_rows(st: &Stack, reporter: &Player, target: &Player) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM report WHERE reporter_account_id = $1::uuid AND target_kind = 'player' AND target_id = $2",
    )
    .bind(&reporter.account_id)
    .bind(&target.xuid)
    .fetch_one(&st.owner)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_report_from_the_game_naming_a_world_needs_it_shared_in_the_last_half_hour() {
    let st = roomy_stack().await;
    let host = st.player("noob").await;
    let seated = st.player("noob").await;
    let second = st.player("noob").await;
    let stranger = st.player("noob").await;
    let h = in_game(&st, &host).await;
    let s = in_game(&st, &seated).await;
    let s2 = in_game(&st, &second).await;
    let world = open_world(&st, &h).await;
    seat(&st, &s, world).await;
    seat(&st, &s2, world).await;
    let uri = lr::REPORTS;

    // 202 with the world named; the row is the social store's, marked as the game's.
    let first = st
        .post(uri, me(&seated), body(&host, Some(world)))
        .send()
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.body);
    let id = first.body["id"].as_str().unwrap().to_owned();
    let (source, world_id, kind, target_account): (String, Option<Uuid>, String, Option<Uuid>) =
        sqlx::query_as(
            "SELECT source, world_id, target_kind, target_account_id FROM report WHERE id = $1::uuid",
        )
        .bind(&id)
        .fetch_one(&st.owner)
        .await
        .unwrap();
    assert_eq!(
        (source.as_str(), world_id, kind.as_str()),
        ("game", Some(world), "player")
    );
    assert_eq!(
        target_account.map(|a| a.to_string()),
        Some(host.account_id.clone())
    );
    // A repeat within a day (a new key, another reason, no world) answers the first id, no row.
    let again = st
        .post(
            uri,
            me(&seated),
            json!({"game": "fh2", "xuid": host.xuid, "reason": "abuse"}),
        )
        .send()
        .await;
    assert_eq!(
        (again.status, again.body["id"].as_str()),
        (StatusCode::ACCEPTED, Some(id.as_str()))
    );
    assert_eq!(report_rows(&st, &seated, &host).await, 1);
    // The same key replays; the same key with another body is 409.
    let key = Uuid::now_v7().to_string();
    let keyed = |b: Value| {
        st.call(Method::POST, uri, me(&host))
            .header("idempotency-key", &key)
            .json(b)
    };
    let made = keyed(body(&seated, None)).send().await;
    assert_eq!(made.status, StatusCode::ACCEPTED, "{}", made.body);
    let replay = keyed(body(&seated, None)).send().await;
    assert_eq!(
        (replay.status, replay.body["id"].as_str()),
        (StatusCode::ACCEPTED, made.body["id"].as_str())
    );
    assert_code(
        &keyed(body(&second, None)).send().await,
        StatusCode::CONFLICT,
        "REQUEST.IDEMPOTENCY_KEY_REUSED",
    );
    // 400: oneself, each field, an unknown field (no free text), no key.
    let own = st.post(uri, me(&seated), body(&seated, None)).send().await;
    assert_code(&own, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(own.body["details"]["field"], "xuid");
    for (bad, field) in [
        (
            json!({"game": "FH2", "xuid": host.xuid, "reason": "abuse"}),
            "game",
        ),
        (
            json!({"game": "fh2", "xuid": "player", "reason": "abuse"}),
            "xuid",
        ),
        (
            json!({"game": "fh2", "xuid": host.xuid, "reason": "spite"}),
            "reason",
        ),
        (
            json!({"game": "fh2", "xuid": host.xuid, "reason": "abuse", "worldId": "w"}),
            "worldId",
        ),
        (
            json!({"game": "fh2", "xuid": host.xuid, "reason": "abuse", "text": "synthetic"}),
            "text",
        ),
        (json!({"game": "fh2", "reason": "abuse"}), "xuid"),
    ] {
        let a = st.post(uri, me(&second), bad.clone()).send().await;
        assert_code(&a, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(a.body["details"]["field"], field, "{bad}");
    }
    let keyless = st
        .call(Method::POST, uri, me(&second))
        .json(body(&host, None))
        .send()
        .await;
    assert_code(&keyless, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(keyless.body["details"]["field"], "Idempotency-Key");
    // 401: no launcher token (a web session does not open the launcher's alias).
    for who in [Who::Bare, Who::Web(Some(seated.cookie.as_str()))] {
        let a = st.post(uri, who, body(&host, None)).send().await;
        assert_code(&a, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    }
    // 404, one answer: never in the world named, an unknown player, another game, another world.
    let other_world = open_world(&st, &in_game(&st, &stranger).await).await;
    for (who, b) in [
        (&stranger, body(&host, Some(world))),
        (
            &seated,
            json!({"game": "fh2", "xuid": "2535999999999999", "reason": "abuse"}),
        ),
        (
            &seated,
            json!({"game": "fh9", "xuid": host.xuid, "reason": "abuse"}),
        ),
        (&second, body(&host, Some(other_world))),
    ] {
        let a = st.post(uri, me(who), b.clone()).send().await;
        assert_code(&a, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    }
    // Without a world (the game's own player menus, OD-193) any player card may be reported.
    let anyone = st.player("noob").await;
    let any_card = st.post(uri, me(&anyone), body(&second, None)).send().await;
    assert_eq!(any_card.status, StatusCode::ACCEPTED, "{}", any_card.body);
    assert_eq!(report_rows(&st, &anyone, &second).await, 1);
    // A seat that ended since still counts; over 30 minutes ago it does not.
    matchmaking::leave_seat(&st.owner, &s2).await.unwrap();
    let after_leaving = st
        .post(uri, me(&second), body(&host, Some(world)))
        .send()
        .await;
    assert_eq!(
        after_leaving.status,
        StatusCode::ACCEPTED,
        "{}",
        after_leaving.body
    );
    sqlx::query(
        "UPDATE mm_grant SET issued_at = issued_at - interval '1 hour', expires_at = expires_at - interval '1 hour', \
           hold_until = hold_until - interval '1 hour', ended_at = ended_at - interval '1 hour' \
         WHERE world_id = $1 AND profile_id = $2",
    )
    .bind(world)
    .bind(s2.profile_id)
    .execute(&st.owner)
    .await
    .unwrap();
    let too_late = st
        .post(uri, me(&host), body(&second, Some(world)))
        .send()
        .await;
    assert_code(&too_late, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    // Three distinct accounts hide the card: two from the game, one from the web, whose target
    // (spelled with a leading zero) is stored canonically, so it is the same target.
    let card = format!("/core/v1/social/players/{}", host.xuid);
    assert_eq!(st.get(&card, Who::Bare).send().await.status, StatusCode::OK);
    let web = st
        .post(
            cr::SOCIAL_REPORTS,
            Who::Web(Some(&stranger.cookie)),
            json!({"target": format!("player:0{}", host.xuid), "reason": "abuse"}),
        )
        .challenge()
        .send()
        .await;
    assert_eq!(web.status, StatusCode::ACCEPTED, "{}", web.body);
    assert_eq!(report_rows(&st, &stranger, &host).await, 1);
    // An open web report is no pass for a named world the stranger never shared with the host;
    // without a world it is the repeat, nothing written.
    let unshared = st
        .post(uri, me(&stranger), body(&host, Some(world)))
        .send()
        .await;
    assert_code(&unshared, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let open_one = st.post(uri, me(&stranger), body(&host, None)).send().await;
    assert_eq!(
        (open_one.status, open_one.body["id"].as_str()),
        (StatusCode::ACCEPTED, web.body["id"].as_str())
    );
    assert_eq!(report_rows(&st, &stranger, &host).await, 1);
    assert_code(
        &st.get(&card, Who::Bare).send().await,
        StatusCode::NOT_FOUND,
        "PORTAL.NOT_FOUND",
    );
    // Moderators see where it came from; a dismissal shows the card again.
    let moderator = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let decided = st
        .post(
            &format!("/core/v1/admin/reports/{id}"),
            Who::Web(Some(&moderator.cookie)),
            json!({"action": "dismiss", "note": "synthetic"}),
        )
        .send()
        .await;
    assert_eq!(decided.status, StatusCode::OK, "{}", decided.body);
    assert_eq!(
        (
            decided.body["source"].as_str(),
            decided.body["worldId"].as_str()
        ),
        (Some("game"), Some(world.to_string().as_str()))
    );
    assert_eq!(decided.body["state"], "dismissed");
    assert_eq!(st.get(&card, Who::Bare).send().await.status, StatusCode::OK);
    // A decided report still answers a repeat within the day: one report per target a day.
    let repeat = st.post(uri, me(&seated), body(&host, None)).send().await;
    assert_eq!(
        (repeat.status, repeat.body["id"].as_str()),
        (StatusCode::ACCEPTED, Some(id.as_str()))
    );
    assert_eq!(report_rows(&st, &seated, &host).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_eleventh_report_from_the_game_in_an_hour_is_429() {
    let st = roomy_stack().await;
    let p = st.player("noob").await;
    let host = st.player("noob").await;
    let world = open_world(&st, &in_game(&st, &host).await).await;
    seat(&st, &in_game(&st, &p).await, world).await;
    let who = Who::Launcher(Some(p.access.as_str()));
    let key = Uuid::now_v7().to_string();
    let keyed = || {
        st.call(Method::POST, lr::REPORTS, who)
            .header("idempotency-key", &key)
            .json(body(&host, None))
    };
    let filed = keyed().send().await;
    assert_eq!(filed.status, StatusCode::ACCEPTED, "{}", filed.body);
    // Every request past the field checks counts, unknown targets included (9 more: 10).
    for i in 0..9 {
        let a = st
            .post(
                lr::REPORTS,
                who,
                json!({"game": "fh2", "xuid": "2535999999999998", "reason": "other"}),
            )
            .send()
            .await;
        assert_code(&a, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
        assert_ne!(a.status, StatusCode::TOO_MANY_REQUESTS, "report {i}");
    }
    // A replay of the first report's key answers its 202 and spends nothing, even now.
    for _ in 0..3 {
        let replay = keyed().send().await;
        assert_eq!(
            (replay.status, replay.body["id"].as_str()),
            (StatusCode::ACCEPTED, filed.body["id"].as_str())
        );
    }
    let eleventh = st
        .post(
            lr::REPORTS,
            who,
            json!({"game": "fh2", "xuid": "2535999999999998", "reason": "other"}),
        )
        .send()
        .await;
    assert_code(
        &eleventh,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );
    let retry: u64 = eleventh.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=3600).contains(&retry), "{retry}");
}
