//! The friend routes (CORE_API.md §4.1) over fi-core's whole router, both mountings; friendships
//! are between accounts (OD-55): who may call (a Supporter account, whatever profile it plays,
//! OD-25), who may be asked (a free, lapsed, banned or deleted account, or a hidden or unlisted
//! profile, reads exactly like an unknown gamertag; a noob profile names its account), the
//! mutual request, accept, decline, favorite, remove and cancel by a handle or any XUID,
//! idempotent repeats and replays, a friendship through a mode change, a deleted profile and a
//! new one, what the lists hide, the per-account request limit, the outgoing and friend limits on
//! both sides, and one pooled connection per request. Run with the rest of the e2e suites (see
//! `super`'s header); the emulator users each test creates are deleted when it ends.

use super::*;
use std::sync::Mutex;

/// The emulator users of one test, deleted when it ends (even when it fails).
struct Users {
    host: String,
    tokens: Mutex<Vec<String>>,
}

impl Users {
    fn new(st: &Stack) -> Self {
        Self {
            host: st.emulator.clone(),
            tokens: Mutex::new(Vec::new()),
        }
    }
}

/// [`emulator_post`] that never panics (it runs in `Drop`, maybe while a test unwinds): `None`
/// on any failure.
fn try_emulator_post(host: &str, path: &str, body: &Value, owner: bool) -> Option<Value> {
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
    let mut stream = std::net::TcpStream::connect(host).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, json) = response.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200")
        .then(|| serde_json::from_str(json).ok())
        .flatten()
}

impl Drop for Users {
    fn drop(&mut self) {
        let base = "/identitytoolkit.googleapis.com/v1";
        let tokens = match self.tokens.lock() {
            Ok(t) => t.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for token in tokens {
            let uid = try_emulator_post(
                &self.host,
                &format!("{base}/accounts:lookup?key=local"),
                &json!({"idToken": token}),
                false,
            )
            .and_then(|found| found["users"][0]["localId"].as_str().map(str::to_owned));
            if let Some(uid) = uid {
                try_emulator_post(
                    &self.host,
                    &format!("{base}/projects/{PROJECT}/accounts:delete"),
                    &json!({"localId": uid}),
                    true,
                );
            }
        }
    }
}

/// A player with a web session, a launcher session and one profile of `mode`, a Supporter when
/// `supporter` (as [`Stack::player`], keeping the emulator user for [`Users`]).
async fn member(st: &Stack, users: &Users, mode: &str, supporter: bool) -> Player {
    let id_token = emulator_user(&st.emulator, true);
    users.tokens.lock().unwrap().push(id_token.clone());
    let (cookie, account_id) = st.web_sign_up(&id_token).await;
    let signed = st.launcher_sign_in(&id_token, &device()).await;
    assert_eq!(signed.status, StatusCode::OK, "{}", signed.body);
    let access = signed.body["accessToken"].as_str().unwrap().to_owned();
    let name = format!("F{}", &fresh()[22..]);
    let profile = st
        .post(
            lr::PROFILES,
            Who::Launcher(Some(&access)),
            json!({"game": "fh2", "name": name, "mode": mode}),
        )
        .send()
        .await;
    assert_eq!(profile.status, StatusCode::CREATED, "{}", profile.body);
    if supporter {
        st.make_supporter(&account_id).await;
    }
    Player {
        cookie,
        access,
        account_id,
        xuid: profile.body["xuid"].as_str().unwrap().to_owned(),
        gamertag: profile.body["gamertag"].as_str().unwrap().to_owned(),
        name,
    }
}

/// A stack whose sign-in limits (per client network: every member of a test signs in from the
/// test's one address) allow the many players these suites make. The friend request limit is
/// per account and never scaled.
async fn roomy_stack() -> Stack {
    stack_with(Options {
        vars: vec![("FI_LOCAL_LIMIT_SCALE", "10".into())],
        ..Options::default()
    })
    .await
}

/// A Supporter account with one Hardcore profile.
async fn supporter(st: &Stack, users: &Users) -> Player {
    member(st, users, "hardcore", true).await
}

/// `/launcher/v1/friends/{friendId}{action}`, the friend named by its first profile's XUID (its
/// handle).
fn at(other: &Player, action: &str) -> String {
    format!("{}/{}{action}", lr::FRIENDS, other.xuid)
}

/// `/core/v1/me/friends/{friendId}{action}`.
fn web(other: &Player, action: &str) -> String {
    format!("{}/{}{action}", cr::ME_FRIENDS, other.xuid)
}

impl Stack {
    /// `from` asks `tag` (of fh2) through the launcher.
    async fn ask(&self, from: &Player, tag: &str) -> Answer {
        self.post(
            lr::FRIENDS,
            Who::Launcher(Some(&from.access)),
            json!({"game": "fh2", "gamertag": tag}),
        )
        .send()
        .await
    }

    /// `from` asks the profile `xuid` (of fh2) through the launcher (OD-193).
    async fn ask_xuid(&self, from: &Player, xuid: &str) -> Answer {
        self.post(
            lr::FRIENDS,
            Who::Launcher(Some(&from.access)),
            json!({"game": "fh2", "xuid": xuid}),
        )
        .send()
        .await
    }

    async fn befriend(&self, a: &Player, b: &Player) {
        let asked = self.ask(a, &b.gamertag).await;
        assert_eq!(asked.body["state"], "requested", "{}", asked.body);
        let made = self.ask(b, &a.gamertag).await;
        assert_eq!(made.body["state"], "friends", "{}", made.body);
    }

    async fn list(&self, p: &Player) -> Value {
        let a = self
            .get(lr::FRIENDS, Who::Launcher(Some(&p.access)))
            .send()
            .await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.body);
        a.body
    }

    /// Another profile of `p`'s account: `(xuid, gamertag)`.
    async fn another(&self, p: &Player, mode: &str) -> (String, String) {
        let a = self
            .post(
                lr::PROFILES,
                Who::Launcher(Some(&p.access)),
                json!({"game": "fh2", "name": format!("S{}", &fresh()[22..]), "mode": mode}),
            )
            .send()
            .await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
        (
            a.body["xuid"].as_str().unwrap().to_owned(),
            a.body["gamertag"].as_str().unwrap().to_owned(),
        )
    }

    /// The account's Supporter access ended an hour ago, through the local entitlement writer
    /// (the only writer of the table outside fi-billing, ADR 0305 §3).
    async fn lapse(&self, account_id: &str) {
        let mut conn = self.owner.acquire().await.unwrap();
        fi_db::admission::set_local_entitlement_in(
            &mut conn,
            uuid::Uuid::parse_str(account_id).unwrap(),
            "active",
            Some(fi_domain::UnixMs::now_system().plus_millis(-3_600_000)),
            Some("comp"),
        )
        .await
        .unwrap();
    }

    async fn owner_sql(&self, statement: &'static str, id: &str) {
        sqlx::query(statement)
            .bind(id)
            .execute(&self.owner)
            .await
            .unwrap();
    }

    /// `n` fresh accounts with one Hardcore profile of fh2 each, unlisted (they never reach the
    /// public directory other suites read), linked to `p`'s account by `insert` (`me.id` = `p`'s
    /// account, `acc.id` the new ones).
    /// Every account `p` asked becomes a Supporter for 30 days, through the local entitlement
    /// writer (the only writer of the table outside fi-billing, ADR 0305 §3).
    async fn entitle_asked(&self, p: &Player) {
        let asked: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT to_account FROM account_friend_request WHERE from_account = $1::uuid",
        )
        .bind(&p.account_id)
        .fetch_all(&self.owner)
        .await
        .unwrap();
        let mut conn = self.owner.acquire().await.unwrap();
        for account in asked {
            fi_db::admission::set_local_entitlement_in(
                &mut conn,
                account,
                "active",
                Some(fi_domain::UnixMs::now_system().plus_millis(30 * 24 * 3_600_000)),
                Some("comp"),
            )
            .await
            .unwrap();
        }
    }

    async fn bulk(&self, p: &Player, n: i64, insert: &'static str) {
        let sql = format!(
            "WITH acc AS (INSERT INTO account (origin) SELECT 'registered' FROM generate_series(1, $2) RETURNING id), \
                  pro AS (INSERT INTO profile (account_id, game, xuid, name, discriminator, mode, origin, social_listed) \
                          SELECT id, 'fh2', nextval('profile_xuid_seq'), 'E' || substr(replace(id::text, '-', ''), 21, 12), 3, 'hardcore', 'created', false \
                          FROM acc RETURNING id, account_id), \
                  me AS (SELECT $1::uuid AS id) \
             {insert}"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&p.account_id)
            .bind(n)
            .execute(&self.owner)
            .await
            .unwrap();
    }
}

/// The row ids of `list[field]`.
fn ids(list: &Value, field: &str) -> Vec<String> {
    list[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The XUIDs of a row's profiles.
fn profile_xuids(row: &Value) -> Vec<String> {
    row["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["xuid"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn friends_are_every_accounts_and_closed_targets_read_as_unknown() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let a = supporter(&st, &users).await;
    let free = member(&st, &users, "hardcore", false).await;
    let lapsed = supporter(&st, &users).await;
    st.lapse(&lapsed.account_id).await;

    // OD-129: a free and a lapsed account manage friends like any account (no
    // `PORTAL.VIP_REQUIRED`): both mountings read, and each is asked like anyone.
    for p in [&free, &lapsed] {
        for r in [
            st.get(lr::FRIENDS, Who::Launcher(Some(&p.access)))
                .send()
                .await,
            st.get(cr::ME_FRIENDS, Who::Web(Some(&p.cookie)))
                .send()
                .await,
        ] {
            assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        }
        let asked = st.ask(&a, &p.gamertag).await;
        assert_eq!(
            asked.body["state"], "requested",
            "{} {}",
            p.name, asked.body
        );
    }
    // A Supporter playing a noob profile manages friends all the same (OD-55): the friendship
    // is the account's; and a noob profile's gamertag names its account.
    let noob = member(&st, &users, "noob", true).await;
    let r = st.ask(&noob, &a.gamertag).await;
    assert_eq!(r.body["state"], "requested", "{}", r.body);
    assert_eq!(r.body["request"]["id"].as_str(), Some(a.xuid.as_str()));
    assert_eq!(
        st.ask(&a, &noob.gamertag).await.body["state"],
        "friends",
        "the noob profile's gamertag names the account"
    );

    // Banned, hidden, unlisted and deleted targets answer like an unknown one (a free or a
    // lapsed one no longer does: OD-129).
    let unknown = st.ask(&a, &format!("Z{}#00001", &fresh()[22..])).await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let banned = supporter(&st, &users).await;
    st.owner_sql(
        "UPDATE account SET banned_at = now() WHERE id = $1::uuid",
        &banned.account_id,
    )
    .await;
    let hidden = supporter(&st, &users).await;
    st.owner_sql(
        "UPDATE profile SET social_hidden_at = now() WHERE xuid = $1::bigint",
        &hidden.xuid,
    )
    .await;
    let unlisted = supporter(&st, &users).await;
    st.owner_sql(
        "UPDATE profile SET social_listed = false WHERE xuid = $1::bigint",
        &unlisted.xuid,
    )
    .await;
    let deleted = supporter(&st, &users).await;
    st.owner_sql(
        "UPDATE profile SET status = 'deleted', name = NULL, discriminator = NULL, deleted_at = now() \
         WHERE xuid = $1::bigint",
        &deleted.xuid,
    )
    .await;
    for p in [&banned, &hidden, &unlisted, &deleted] {
        let r = st.ask(&a, &p.gamertag).await;
        assert_eq!(
            (r.status, &r.body),
            (unknown.status, &unknown.body),
            "{}",
            p.name
        );
        let accept = st
            .call(
                Method::POST,
                &at(p, "/accept"),
                Who::Launcher(Some(&a.access)),
            )
            .key()
            .send()
            .await;
        assert_code(&accept, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    }
    let as_banned = st
        .get(lr::FRIENDS, Who::Launcher(Some(&banned.access)))
        .send()
        .await;
    assert_code(&as_banned, StatusCode::FORBIDDEN, "ACCOUNTS.BANNED");

    // One's own account, by any of its profiles: refused as invalid.
    let (_, second) = st.another(&a, "noob").await;
    for tag in [second.as_str(), a.gamertag.as_str()] {
        let own = st.ask(&a, tag).await;
        assert_code(&own, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(own.body["details"]["field"], "gamertag");
    }
    for bad in ["1abc", "bad name", "1abc#00001", "Name#123", "Name#1234567"] {
        let r = st.ask(&a, bad).await;
        assert_code(
            &r,
            StatusCode::UNPROCESSABLE_ENTITY,
            "ACCOUNTS.INVALID_GAMERTAG",
        );
    }
    // Malformed bodies and ids.
    let b = supporter(&st, &users).await;
    for (uri, body) in [
        (lr::FRIENDS.to_owned(), json!({"gamertag": b.gamertag})),
        (
            lr::FRIENDS.to_owned(),
            json!({"game": "FH2", "gamertag": b.gamertag}),
        ),
        (
            lr::FRIENDS.to_owned(),
            json!({"game": "fh2", "gamertag": b.gamertag, "fromXuid": a.xuid}),
        ),
        (format!("{}/abc/accept", lr::FRIENDS), json!({})),
        (format!("{}/12/accept", lr::FRIENDS), json!({})),
    ] {
        let r = st
            .post(&uri, Who::Launcher(Some(&a.access)), body)
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    }
    let another_game = st
        .post(
            lr::FRIENDS,
            Who::Launcher(Some(&a.access)),
            json!({"game": "fh3", "gamertag": b.gamertag}),
        )
        .send()
        .await;
    assert_eq!(
        (another_game.status, &another_game.body),
        (unknown.status, &unknown.body)
    );
    let favorite_extra = st
        .call(
            Method::PUT,
            &at(&b, "/favorite"),
            Who::Launcher(Some(&a.access)),
        )
        .key()
        .json(json!({"favorite": true, "fromXuid": a.xuid}))
        .send()
        .await;
    assert_code(&favorite_extra, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    let no_key = st
        .call(Method::POST, lr::FRIENDS, Who::Launcher(Some(&a.access)))
        .json(json!({"game": "fh2", "gamertag": b.gamertag}))
        .send()
        .await;
    assert_code(&no_key, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    // Unknown friends: accept and favorite are not found; decline and delete change nothing.
    let nobody = format!("{}/2535999999999999", lr::FRIENDS);
    let r = st
        .call(
            Method::POST,
            &format!("{nobody}/accept"),
            Who::Launcher(Some(&a.access)),
        )
        .key()
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    for (method, uri) in [
        (Method::POST, format!("{nobody}/decline")),
        (Method::DELETE, nobody.clone()),
    ] {
        let r = st
            .call(method, &uri, Who::Launcher(Some(&a.access)))
            .key()
            .send()
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{uri}");
    }
    // The launcher's alias takes the access token only; the web's routes need a session.
    let cookie_on_launcher = st.get(lr::FRIENDS, Who::Web(Some(&a.cookie))).send().await;
    assert_code(
        &cookie_on_launcher,
        StatusCode::UNAUTHORIZED,
        "AUTH.TOKEN_INVALID",
    );
    let anonymous = st.get(cr::ME_FRIENDS, Who::Web(None)).send().await;
    assert_code(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "PORTAL.SESSION_REQUIRED",
    );
    // A browser write without the web's headers is refused before anything (CSRF).
    let csrf = st
        .post(
            cr::ME_FRIENDS,
            Who::Bare,
            json!({"game": "fh2", "gamertag": b.gamertag}),
        )
        .header("cookie", &format!("__Host-fi_session={}", a.cookie))
        .send()
        .await;
    assert_code(&csrf, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    // An account pending deletion.
    let leaving = supporter(&st, &users).await;
    st.owner_sql(
        "UPDATE account SET status = 'deletion_pending', deletion_requested_at = now(), \
           deletion_due_at = now() + interval '14 days' WHERE id = $1::uuid",
        &leaving.account_id,
    )
    .await;
    let r = st
        .get(lr::FRIENDS, Who::Launcher(Some(&leaving.access)))
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "ACCOUNTS.DELETION_PENDING");
    // An account that never had a profile cannot ask (nobody could see or address it).
    let id_token = emulator_user(&st.emulator, true);
    users.tokens.lock().unwrap().push(id_token.clone());
    let (cookie, account) = st.web_sign_up(&id_token).await;
    st.make_supporter(&account).await;
    let r = st
        .post(
            cr::ME_FRIENDS,
            Who::Web(Some(&cookie)),
            json!({"game": "fh2", "gamertag": b.gamertag}),
        )
        .send()
        .await;
    assert_code(&r, StatusCode::CONFLICT, "PORTAL.CONFLICT");
    assert_eq!(r.body["details"]["reason"], "no_profile");
    // Nothing above wrote a row of A's but the requests to the free and the lapsed account and
    // the friendship with the noob player (OD-129).
    let rows: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM account_friend_request WHERE from_account = $1::uuid OR to_account = $1::uuid) \
              + (SELECT count(*) FROM account_friendship WHERE account_id = $1::uuid)",
    )
    .bind(&a.account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(rows, 3);
}

/// OD-193 (register CR-S89): the game's own player menus name the player by XUID. Any profile of
/// the account, any mode, under the gamertag's rule; exactly one of `gamertag` and `xuid`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_player_the_game_shows_is_asked_by_xuid() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let a = supporter(&st, &users).await;
    let b = member(&st, &users, "hardcore", false).await;
    // B's second profile (noob) names B's account; B answers by A's XUID and they are friends.
    let (noob_xuid, _) = st.another(&b, "noob").await;
    let asked = st.ask_xuid(&a, &noob_xuid).await;
    assert_eq!(asked.body["state"], "requested", "{}", asked.body);
    assert_eq!(
        asked.body["request"]["id"].as_str(),
        Some(noob_xuid.as_str())
    );
    let made = st.ask_xuid(&b, &a.xuid).await;
    assert_eq!(made.body["state"], "friends", "{}", made.body);
    assert_eq!(st.ask_xuid(&a, &b.xuid).await.body["state"], "friends");
    // One's own profile; malformed; both or neither field.
    let own = st.ask_xuid(&a, &a.xuid).await;
    assert_code(&own, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(own.body["details"]["field"], "xuid");
    for bad in ["abc", "12", "-1", ""] {
        let r = st.ask_xuid(&a, bad).await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], "xuid", "{bad}");
    }
    for body in [
        json!({"game": "fh2", "gamertag": b.gamertag, "xuid": b.xuid}),
        json!({"game": "fh2"}),
    ] {
        let r = st
            .post(lr::FRIENDS, Who::Launcher(Some(&a.access)), body)
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    }
    // Unknown, hidden and another game's XUID answer alike.
    let unknown = st.ask_xuid(&a, "2535999999999999").await;
    assert_code(&unknown, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    let hidden = supporter(&st, &users).await;
    st.owner_sql(
        "UPDATE profile SET social_hidden_at = now() WHERE xuid = $1::bigint",
        &hidden.xuid,
    )
    .await;
    let r = st.ask_xuid(&a, &hidden.xuid).await;
    assert_eq!((r.status, &r.body), (unknown.status, &unknown.body));
    let c = supporter(&st, &users).await;
    let another_game = st
        .post(
            lr::FRIENDS,
            Who::Launcher(Some(&a.access)),
            json!({"game": "fh3", "xuid": c.xuid}),
        )
        .send()
        .await;
    assert_eq!(
        (another_game.status, &another_game.body),
        (unknown.status, &unknown.body)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_mutual_request_makes_friends_and_every_write_is_idempotent() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let (a, b, c, d) = (
        supporter(&st, &users).await,
        supporter(&st, &users).await,
        supporter(&st, &users).await,
        supporter(&st, &users).await,
    );
    // B's account also has a noob profile; A names B by it.
    let (b_noob, b_noob_tag) = st.another(&b, "noob").await;

    // A asks B: a request; its replay and a repeat answer the same; the key is the request's.
    let key = Uuid::now_v7().to_string();
    let send = |tag: &str| {
        st.call(Method::POST, lr::FRIENDS, Who::Launcher(Some(&a.access)))
            .header("idempotency-key", &key)
            .json(json!({"game": "fh2", "gamertag": tag}))
    };
    let first = send(&b_noob_tag).send().await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["state"], "requested");
    let req = &first.body["request"];
    // The asker's row is the profile it asked, by its XUID: never B's handle or B's other
    // profiles (review finding 2).
    assert_eq!(
        req["id"].as_str(),
        Some(b_noob.as_str()),
        "the profile asked"
    );
    assert_eq!(profile_xuids(req), vec![b_noob.clone()]);
    assert_eq!(
        req["profiles"][0]["gamertag"].as_str(),
        Some(b_noob_tag.as_str())
    );
    assert_eq!(req["profiles"][0]["game"], "fh2");
    assert_eq!(req["profiles"][0]["mode"], "noob");
    let replay = send(&b_noob_tag).send().await;
    assert_eq!((replay.status, &replay.body), (first.status, &first.body));
    let reused = send(&c.gamertag).send().await;
    assert_code(
        &reused,
        StatusCode::CONFLICT,
        "REQUEST.IDEMPOTENCY_KEY_REUSED",
    );
    let repeat = st.ask(&a, &b.gamertag).await;
    assert_eq!(
        repeat.body, first.body,
        "the first request answers, nothing new"
    );
    let listed = st
        .get(cr::ME_FRIENDS, Who::Web(Some(&b.cookie)))
        .send()
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.header("cache-control"), Some("private, no-store"));
    // The asked side sees the asker's handle and profiles; the asker only what it named.
    assert_eq!(ids(&listed.body, "incoming"), vec![a.xuid.clone()]);
    assert_eq!(
        profile_xuids(&listed.body["incoming"][0]),
        vec![a.xuid.clone()]
    );
    let outgoing = st.list(&a).await;
    assert_eq!(ids(&outgoing, "outgoing"), vec![b_noob.clone()]);
    assert_eq!(
        profile_xuids(&outgoing["outgoing"][0]),
        vec![b_noob.clone()],
        "never B's other profiles"
    );

    // B asks A back on the web, typing the name in lower case: friends, no request left.
    let made = st
        .post(
            cr::ME_FRIENDS,
            Who::Web(Some(&b.cookie)),
            json!({"game": "fh2", "gamertag": a.gamertag.to_ascii_lowercase()}),
        )
        .send()
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(made.body["state"], "friends");
    assert_eq!(made.body["friend"]["id"].as_str(), Some(a.xuid.as_str()));
    let list = st.list(&a).await;
    assert_eq!(ids(&list, "friends"), vec![b.xuid.clone()]);
    assert!(ids(&list, "incoming").is_empty() && ids(&list, "outgoing").is_empty());
    assert_eq!(
        profile_xuids(&list["friends"][0]),
        [b.xuid.clone(), b_noob.clone()]
    );
    assert_eq!(list["friends"][0]["favorite"], false);
    assert_eq!(list["friends"][0]["online"], false);
    assert_eq!(st.ask(&a, &b.gamertag).await.body["state"], "friends");
    // Online: one of B's profiles has an active play session.
    st.live_session(&b, "active").await;
    assert_eq!(st.list(&a).await["friends"][0]["online"], true);

    // Favorite: the caller's side only, on both mountings, by the handle or any of B's XUIDs.
    let fav = st
        .call(
            Method::PUT,
            &format!("{}/{b_noob}/favorite", lr::FRIENDS),
            Who::Launcher(Some(&a.access)),
        )
        .key()
        .json(json!({"favorite": true}))
        .send()
        .await;
    assert_eq!(fav.status, StatusCode::OK, "{}", fav.body);
    assert_eq!(
        (fav.body["favorite"].clone(), fav.body["id"].as_str()),
        (json!(true), Some(b.xuid.as_str()))
    );
    assert_eq!(st.list(&b).await["friends"][0]["favorite"], false);
    let unset = st
        .call(
            Method::PUT,
            &web(&a, "/favorite"),
            Who::Web(Some(&b.cookie)),
        )
        .key()
        .json(json!({"favorite": false}))
        .send()
        .await;
    assert_eq!(
        (unset.status, &unset.body["favorite"]),
        (StatusCode::OK, &json!(false))
    );
    assert_eq!(st.list(&a).await["friends"][0]["favorite"], true);
    let stranger = st
        .call(
            Method::PUT,
            &at(&c, "/favorite"),
            Who::Launcher(Some(&a.access)),
        )
        .key()
        .json(json!({"favorite": true}))
        .send()
        .await;
    assert_code(&stranger, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");

    // Accept: C asks A; A accepts; the replay and a repeat answer the same Friend.
    assert_eq!(st.ask(&c, &a.gamertag).await.body["state"], "requested");
    let key = Uuid::now_v7().to_string();
    let accept = || {
        st.call(
            Method::POST,
            &at(&c, "/accept"),
            Who::Launcher(Some(&a.access)),
        )
        .header("idempotency-key", &key)
    };
    let accepted = accept().send().await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    assert_eq!(accepted.body["id"].as_str(), Some(c.xuid.as_str()));
    assert_eq!(accept().send().await.body, accepted.body);
    let again = st
        .call(Method::POST, &web(&c, "/accept"), Who::Web(Some(&a.cookie)))
        .key()
        .send()
        .await;
    assert_eq!(
        (again.status, &again.body),
        (StatusCode::OK, &accepted.body)
    );
    let none = st
        .call(
            Method::POST,
            &at(&d, "/accept"),
            Who::Launcher(Some(&a.access)),
        )
        .key()
        .send()
        .await;
    assert_code(&none, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");

    // Decline: D asks A; A declines on the web, twice; D's request is gone on both sides.
    assert_eq!(st.ask(&d, &a.gamertag).await.body["state"], "requested");
    for _ in 0..2 {
        let declined = st
            .call(
                Method::POST,
                &web(&d, "/decline"),
                Who::Web(Some(&a.cookie)),
            )
            .key()
            .send()
            .await;
        assert_eq!(declined.status, StatusCode::NO_CONTENT, "{}", declined.body);
    }
    assert!(ids(&st.list(&a).await, "incoming").is_empty());
    assert!(ids(&st.list(&d).await, "outgoing").is_empty());

    // Cancel: A asks D, then deletes: D sees nothing.
    assert_eq!(st.ask(&a, &d.gamertag).await.body["state"], "requested");
    let cancelled = st
        .call(Method::DELETE, &at(&d, ""), Who::Launcher(Some(&a.access)))
        .key()
        .send()
        .await;
    assert_eq!(cancelled.status, StatusCode::NO_CONTENT);
    assert!(ids(&st.list(&d).await, "incoming").is_empty());

    // Remove: both sides lose the friendship; repeats (both mountings) answer 204.
    let removed = st
        .call(Method::DELETE, &at(&b, ""), Who::Launcher(Some(&a.access)))
        .key()
        .send()
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(ids(&st.list(&a).await, "friends"), vec![c.xuid.clone()]);
    assert!(ids(&st.list(&b).await, "friends").is_empty());
    let web_remove = st
        .call(Method::DELETE, &web(&b, ""), Who::Web(Some(&a.cookie)))
        .key()
        .send()
        .await;
    assert_eq!(
        web_remove.status,
        StatusCode::NO_CONTENT,
        "{}",
        web_remove.body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn friendships_survive_profile_changes_and_the_limits_are_per_account() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let a = supporter(&st, &users).await;
    let (banned, moved, lapsed) = (
        supporter(&st, &users).await,
        supporter(&st, &users).await,
        supporter(&st, &users).await,
    );
    for p in [&banned, &moved, &lapsed] {
        st.befriend(&a, p).await;
    }
    st.owner_sql(
        "UPDATE account SET banned_at = now() WHERE id = $1::uuid",
        &banned.account_id,
    )
    .await;
    // The friend's only profile moves to noob, then is deleted, then the account makes a new
    // one: the friendship and its handle stay throughout.
    st.owner_sql(
        "UPDATE profile SET mode = 'noob', mode_changed_at = now() WHERE xuid = $1::bigint",
        &moved.xuid,
    )
    .await;
    let row = |list: &Value| {
        list["friends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"].as_str() == Some(moved.xuid.as_str()))
            .cloned()
            .expect("still a friend")
    };
    assert_eq!(row(&st.list(&a).await)["profiles"][0]["mode"], "noob");
    st.owner_sql(
        "UPDATE profile SET status = 'deleted', name = NULL, discriminator = NULL, deleted_at = now() \
         WHERE xuid = $1::bigint",
        &moved.xuid,
    )
    .await;
    assert!(profile_xuids(&row(&st.list(&a).await)).is_empty());
    let (fresh_xuid, _) = st.another(&moved, "hardcore").await;
    assert_eq!(
        profile_xuids(&row(&st.list(&a).await)),
        vec![fresh_xuid.clone()]
    );
    st.lapse(&lapsed.account_id).await;
    let mut shown = ids(&st.list(&a).await, "friends");
    shown.sort();
    let mut want = vec![moved.xuid.clone(), lapsed.xuid.clone()];
    want.sort();
    assert_eq!(shown, want, "banned hidden; lapsed and moved stay");
    // A hidden friend can still be removed by its handle.
    let removed = st
        .call(
            Method::DELETE,
            &at(&banned, ""),
            Who::Launcher(Some(&a.access)),
        )
        .key()
        .send()
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);

    // 20 requests per hour per account, whichever profile is played: the 21st is 429.
    let asker = supporter(&st, &users).await;
    for _ in 0..20 {
        let r = st.ask(&asker, &format!("Z{}#00001", &fresh()[22..])).await;
        assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    }
    let limited = st.ask(&asker, &a.gamertag).await;
    assert_code(
        &limited,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );
    assert!(limited.header("retry-after").is_some());
    let on_web = st
        .post(
            cr::ME_FRIENDS,
            Who::Web(Some(&asker.cookie)),
            json!({"game": "fh2", "gamertag": a.gamertag}),
        )
        .send()
        .await;
    assert_code(
        &on_web,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );

    // 100 live outgoing requests per account: the next is refused, nothing written.
    let busy = supporter(&st, &users).await;
    st.bulk(
        &busy,
        100,
        // Each asked by its profile; they become Supporters below (the limit counts requests to
        // open accounts).
        " INSERT INTO account_friend_request (from_account, to_account, asked_profile) \
         SELECT me.id, pro.account_id, pro.id FROM me, pro",
    )
    .await;
    st.entitle_asked(&busy).await;
    let r = st.ask(&busy, &a.gamertag).await;
    assert_code(&r, StatusCode::CONFLICT, "SOCIAL.LIMIT_REACHED");
    assert_eq!(
        r.body["details"],
        json!({"limit": "outgoing", "max": 100, "side": "caller"})
    );

    // 200 listed friends per account: refused for the full side, named from the caller's view.
    let popular = supporter(&st, &users).await;
    st.bulk(
        &popular,
        200,
        "INSERT INTO account_friendship (account_id, friend_id) SELECT me.id, acc.id FROM me, acc",
    )
    .await;
    let fan = supporter(&st, &users).await;
    assert_eq!(
        st.ask(&fan, &popular.gamertag).await.body["state"],
        "requested"
    );
    let r = st
        .call(
            Method::POST,
            &at(&fan, "/accept"),
            Who::Launcher(Some(&popular.access)),
        )
        .key()
        .send()
        .await;
    assert_code(&r, StatusCode::CONFLICT, "SOCIAL.LIMIT_REACHED");
    assert_eq!(
        r.body["details"],
        json!({"limit": "friends", "max": 200, "side": "caller"})
    );
    let r = st.ask(&popular, &a.gamertag).await;
    assert_eq!(
        r.body["state"], "requested",
        "a request is not a friendship: {}",
        r.body
    );
    let r = st
        .call(
            Method::POST,
            &at(&popular, "/accept"),
            Who::Launcher(Some(&a.access)),
        )
        .key()
        .send()
        .await;
    assert_code(&r, StatusCode::CONFLICT, "SOCIAL.LIMIT_REACHED");
    assert_eq!(
        r.body["details"],
        json!({"limit": "friends", "max": 200, "side": "other"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn friend_routes_hold_one_pooled_connection_at_a_time() {
    let st = stack_with(Options {
        connections: 1,
        ..Options::default()
    })
    .await;
    let users = Users::new(&st);
    let (a, b) = (supporter(&st, &users).await, supporter(&st, &users).await);
    assert_eq!(st.ask(&a, &b.gamertag).await.body["state"], "requested");
    let accepted = st
        .call(
            Method::POST,
            &at(&a, "/accept"),
            Who::Launcher(Some(&b.access)),
        )
        .key()
        .send()
        .await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    assert_eq!(ids(&st.list(&a).await, "friends"), vec![b.xuid.clone()]);
    for (method, uri, body) in [
        (
            Method::PUT,
            web(&b, "/favorite"),
            Some(json!({"favorite": true})),
        ),
        (Method::POST, web(&b, "/decline"), None),
        (Method::DELETE, web(&b, ""), None),
    ] {
        let mut call = st.call(method, &uri, Who::Web(Some(&a.cookie))).key();
        if let Some(body) = body {
            call = call.json(body);
        }
        let r = call.send().await;
        assert!(r.status.is_success(), "{uri}: {} {}", r.status, r.body);
    }
}

// ---- Presence (CORE_API.md §4.1, OD-56) and the inbox of invites and requests (§4.2, OD-59) ----

impl Stack {
    /// An open `everyone` world of 4 seats hosted by `p`'s first profile in a live play session:
    /// its id.
    async fn host_world(&self, p: &Player, xuid: &str) -> String {
        let session = self.live_session_of(xuid).await;
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO world (id, game, host_profile_id, host_account_id, host_session_id, host_tier, kind, name, \
               mode, players, peak_players, max_players, races, audience, endpoint_ip, endpoint_port, expires_at) \
             SELECT $1, p.game, p.id, p.account_id, $2::uuid, 'vip', 'supporter', 'E2E world', p.mode, 1, 1, 4, true, \
               'everyone', '198.51.100.9', 5101, now() + interval '45 seconds' \
             FROM profile p WHERE p.xuid = $3::bigint AND p.account_id = $4::uuid",
        )
        .bind(id)
        .bind(&session)
        .bind(xuid)
        .bind(&p.account_id)
        .execute(&self.owner)
        .await
        .unwrap();
        id.to_string()
    }

    /// An active play session of the profile `xuid` (any of a player's profiles).
    async fn live_session_of(&self, xuid: &str) -> String {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO play_session (id, account_id, profile_id, device_binding_id, pool, state, launcher_version, \
               admitted_at, expires_at, max_until, game_until) \
             SELECT $1, p.account_id, p.id, (SELECT d.id FROM device_binding d WHERE d.account_id = p.account_id LIMIT 1), \
               (SELECT name FROM pool WHERE game = p.game ORDER BY name LIMIT 1), 'active', '0.1.0', now(), \
               now() + interval '5 minutes', now() + interval '12 hours', now() + interval '12 hours' \
             FROM profile p WHERE p.xuid = $2::bigint",
        )
        .bind(id)
        .bind(xuid)
        .execute(&self.owner)
        .await
        .unwrap();
        id.to_string()
    }

    /// `p`'s profile `xuid` takes a used seat in `world`.
    async fn seat(&self, xuid: &str, world: &str) {
        self.live_session_of(xuid).await;
        sqlx::query(
            "INSERT INTO mm_grant (id, game, world_id, profile_id, account_id, xuid, tier, state, issued_at, expires_at, \
               hold_until, used_at, announced) \
             SELECT $1, p.game, $2::uuid, p.id, p.account_id, p.xuid, 'vip', 'used', now(), now() + interval '60 seconds', \
               now() + interval '10 minutes', now(), true FROM profile p WHERE p.xuid = $3::bigint",
        )
        .bind(Uuid::now_v7())
        .bind(world)
        .bind(xuid)
        .execute(&self.owner)
        .await
        .unwrap();
    }

    /// A pending `kind` (`invite` | `request`) about `world` from the profile `from` to the
    /// account of `to`: its id.
    async fn offer(&self, kind: &str, world: &str, from: &str, to: &Player) -> String {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO world_invite (id, game, kind, world_id, from_account_id, from_profile_id, to_account_id, expires_at) \
             SELECT $1, p.game, $2, $3::uuid, p.account_id, p.id, $4::uuid, now() + interval '2 minutes' \
             FROM profile p WHERE p.xuid = $5::bigint",
        )
        .bind(id)
        .bind(kind)
        .bind(world)
        .bind(&to.account_id)
        .bind(from)
        .execute(&self.owner)
        .await
        .unwrap();
        id.to_string()
    }

    /// An offer as [`Stack::offer`], made `ago_secs` seconds ago and lapsing `ttl_secs` after it
    /// was made (CR-S106: one past its `expires_at` the sweep has not ended yet).
    async fn offer_at(
        &self,
        kind: &str,
        world: &str,
        from: &str,
        to: &Player,
        ago_secs: i64,
        ttl_secs: i64,
    ) -> String {
        let id = self.offer(kind, world, from, to).await;
        sqlx::query(
            "UPDATE world_invite SET created_at = now() - make_interval(secs => $2), \
               expires_at = now() - make_interval(secs => $2) + make_interval(secs => $3) WHERE id = $1::uuid",
        )
        .bind(&id)
        .bind(ago_secs as f64)
        .bind(ttl_secs as f64)
        .execute(&self.owner)
        .await
        .unwrap();
        id
    }

    async fn get_ok(&self, uri: &str, p: &Player) -> Value {
        let a = self.get(uri, Who::Launcher(Some(&p.access))).send().await;
        assert_eq!(a.status, StatusCode::OK, "{uri}: {}", a.body);
        assert_eq!(a.header("cache-control"), Some("private, no-store"));
        a.body
    }
}

/// The friend row of `xuid` (a handle) in a list.
fn row<'a>(list: &'a Value, xuid: &str) -> &'a Value {
    list["friends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == xuid)
        .unwrap_or_else(|| panic!("{xuid} listed: {list}"))
}

/// OD-56: each friend row shows the friend account's tier and the profile it plays now (in a
/// world, in the game, or the one it played last); in-world friends first, then in-game, then
/// those inviting, then the rest; `joinable` only with `?asXuid=`, from that profile: a noob
/// profile never joins a hardcore world, a hardcore one never a noob world.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn friend_rows_show_the_profile_a_friend_plays_and_whether_its_world_is_joinable() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let me = supporter(&st, &users).await;
    let (my_noob, _) = st.another(&me, "noob").await;
    let off = supporter(&st, &users).await;
    let host = supporter(&st, &users).await;
    let seated = supporter(&st, &users).await;
    let gamer = supporter(&st, &users).await;
    let noob = member(&st, &users, "noob", true).await;
    let inviter = supporter(&st, &users).await;
    for f in [&off, &host, &seated, &gamer, &noob, &inviter] {
        st.befriend(&me, f).await;
    }
    let world = st.host_world(&host, &host.xuid).await;
    st.seat(&seated.xuid, &world).await;
    st.live_session_of(&gamer.xuid).await;
    let noob_world = st.host_world(&noob, &noob.xuid).await;
    // The inviter is not playing now: its invite's world outlives its session in this setup.
    let invite_world = st.host_world(&inviter, &inviter.xuid).await;
    st.owner_sql(
        "UPDATE play_session SET state = 'ended', ended_at = now(), end_reason = 'game_exit' \
         WHERE profile_id = (SELECT id FROM profile WHERE xuid = $1::bigint) AND state = 'active'",
        &inviter.xuid,
    )
    .await;
    let invite = st.offer("invite", &invite_world, &inviter.xuid, &me).await;
    // A friend whose Supporter access lapsed is listed as free.
    st.lapse(&seated.account_id).await;

    let list = st.get_ok(lr::FRIENDS, &me).await;
    assert_eq!(
        ids(&list, "friends"),
        [
            host.xuid.clone(),
            seated.xuid.clone(),
            noob.xuid.clone(),
            gamer.xuid.clone(),
            inviter.xuid.clone(),
            off.xuid.clone()
        ],
        "in a world, in the game, inviting, the rest"
    );
    let h = row(&list, &host.xuid);
    assert_eq!(h["tier"], "vip");
    assert_eq!(h["playing"]["state"], "in_world");
    assert_eq!(h["playing"]["worldId"].as_str(), Some(world.as_str()));
    assert_eq!(h["playing"]["xuid"].as_str(), Some(host.xuid.as_str()));
    assert_eq!(
        h["playing"]["gamertag"].as_str(),
        Some(host.gamertag.as_str())
    );
    assert_eq!(h["playing"]["mode"], "hardcore");
    assert!(
        h["playing"].get("joinable").is_none(),
        "no asXuid, no joinable"
    );
    let s = row(&list, &seated.xuid);
    assert_eq!(
        (&s["tier"], &s["playing"]["state"]),
        (&json!("free"), &json!("in_world"))
    );
    assert_eq!(s["playing"]["worldId"].as_str(), Some(world.as_str()));
    assert_eq!(row(&list, &gamer.xuid)["playing"]["state"], "in_game");
    assert!(row(&list, &gamer.xuid)["playing"].get("worldId").is_none());
    assert_eq!(row(&list, &noob.xuid)["playing"]["mode"], "noob");
    let i = row(&list, &inviter.xuid);
    assert_eq!(i["playing"]["state"], "offline");
    assert_eq!(i["invite"]["id"].as_str(), Some(invite.as_str()));
    assert_eq!(i["invite"]["worldId"].as_str(), Some(invite_world.as_str()));
    assert!(
        row(&list, &off.xuid).get("playing").is_none(),
        "never played"
    );

    // joinable from my hardcore profile, then from my noob one: modes never meet.
    let hc = st
        .get_ok(&format!("{}?asXuid={}", lr::FRIENDS, me.xuid), &me)
        .await;
    assert_eq!(row(&hc, &host.xuid)["playing"]["joinable"], true);
    assert_eq!(row(&hc, &seated.xuid)["playing"]["joinable"], true);
    assert_eq!(row(&hc, &noob.xuid)["playing"]["joinable"], false);
    assert!(row(&hc, &gamer.xuid)["playing"].get("joinable").is_none());
    let nb = st
        .get_ok(&format!("{}?asXuid={my_noob}", lr::FRIENDS), &me)
        .await;
    assert_eq!(row(&nb, &host.xuid)["playing"]["joinable"], false);
    assert_eq!(row(&nb, &noob.xuid)["playing"]["joinable"], true);
    let web = st
        .get(
            &format!("{}?asXuid={my_noob}", cr::ME_FRIENDS),
            Who::Web(Some(&me.cookie)),
        )
        .send()
        .await;
    assert_eq!(web.status, StatusCode::OK, "{}", web.body);
    assert_eq!(row(&web.body, &noob.xuid)["playing"]["joinable"], true);
    let _ = noob_world;

    // asXuid must be a live profile of the caller's account; nothing else is accepted.
    for (query, field) in [
        (format!("asXuid={}", host.xuid), "asXuid"),
        ("asXuid=abc".to_owned(), "asXuid"),
        ("bogus=1".to_owned(), "query"),
    ] {
        for (uri, who) in [
            (
                format!("{}?{query}", lr::FRIENDS),
                Who::Launcher(Some(&me.access)),
            ),
            (
                format!("{}?{query}", cr::ME_FRIENDS),
                Who::Web(Some(&me.cookie)),
            ),
        ] {
            let a = st.get(&uri, who).send().await;
            assert_code(&a, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
            assert_eq!(a.body["details"]["field"], field, "{uri}");
        }
    }
}

/// OD-56, OD-59: the launcher's inbox lists the invites to the account and the requests to join
/// the worlds it hosts, with the world as the current profile sees it and `sameMode`; declining
/// ends one (204, idempotent); a free account reads its own (OD-129: friends are every account's).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_inbox_lists_invites_and_requests_and_declines_them() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let me = supporter(&st, &users).await;
    let (my_noob, _) = st.another(&me, "noob").await;
    let host = supporter(&st, &users).await;
    let asker = supporter(&st, &users).await;
    let free = member(&st, &users, "hardcore", false).await;
    st.befriend(&me, &host).await;
    let world = st.host_world(&host, &host.xuid).await;
    let invite = st.offer("invite", &world, &host.xuid, &me).await;
    let request = st.offer("request", &world, &asker.xuid, &host).await;

    // No current profile: the world unjudged by mode, sameMode absent.
    let inbox = st.get_ok(lr::INVITES, &me).await;
    let items = inbox["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{inbox}");
    let it = &items[0];
    assert_eq!(it["id"].as_str(), Some(invite.as_str()));
    assert_eq!(it["kind"], "invite");
    assert_eq!(it["from"]["xuid"].as_str(), Some(host.xuid.as_str()));
    assert_eq!(
        it["from"]["gamertag"].as_str(),
        Some(host.gamertag.as_str())
    );
    assert_eq!(
        (&it["from"]["mode"], &it["from"]["tier"]),
        (&json!("hardcore"), &json!("vip"))
    );
    assert_eq!(it["world"]["id"].as_str(), Some(world.as_str()));
    assert_eq!(it["world"]["invited"], true);
    assert_eq!(it["world"]["canJoin"], true);
    assert_eq!(it["world"]["pingClass"], "unknown");
    assert!(it.get("sameMode").is_none());
    assert!(it["expiresAt"].as_str().is_some() && it["createdAt"].as_str().is_some());
    assert!(
        !inbox.to_string().contains("198.51.100.9"),
        "never an address"
    );
    // From my profiles: the hardcore one is the world's mode, the noob one must switch.
    let hc = st
        .get_ok(&format!("{}?asXuid={}", lr::INVITES, me.xuid), &me)
        .await;
    assert_eq!(hc["items"][0]["sameMode"], true);
    let nb = st
        .get_ok(&format!("{}?asXuid={my_noob}", lr::INVITES), &me)
        .await;
    assert_eq!(nb["items"][0]["sameMode"], false);
    assert_eq!(nb["items"][0]["world"]["canJoin"], false);
    assert_eq!(nb["items"][0]["world"]["why"], "mode");
    // Without asXuid, the profile of the account's active session is the current one.
    st.live_session_of(&my_noob).await;
    let live = st.get_ok(lr::INVITES, &me).await;
    assert_eq!(live["items"][0]["sameMode"], false);
    // The friend row shows the invite too.
    let list = st.get_ok(lr::FRIENDS, &me).await;
    assert_eq!(
        row(&list, &host.xuid)["invite"]["id"].as_str(),
        Some(invite.as_str())
    );

    // The host's inbox holds the request to join its world (its own: why "own").
    let hosted = st.get_ok(lr::INVITES, &host).await;
    let r = &hosted["items"][0];
    assert_eq!(hosted["items"].as_array().unwrap().len(), 1, "{hosted}");
    assert_eq!(
        (r["id"].as_str(), &r["kind"]),
        (Some(request.as_str()), &json!("request"))
    );
    assert_eq!(r["from"]["xuid"].as_str(), Some(asker.xuid.as_str()));
    assert_eq!(r["world"]["why"], "own");

    // Decline: 204, gone, idempotent; an unknown id 204; a malformed one 400; another account's
    // invite is untouched.
    let decline = |p: &Player, id: &str| {
        st.call(
            Method::POST,
            &format!("{}/{id}/decline", lr::INVITES),
            Who::Launcher(Some(&p.access)),
        )
        .send()
    };
    let foreign = decline(&host, &invite).await;
    assert_eq!(foreign.status, StatusCode::NO_CONTENT);
    assert_eq!(
        st.get_ok(lr::INVITES, &me).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "only the invitee declines an invite"
    );
    for _ in 0..2 {
        let d = decline(&me, &invite).await;
        assert_eq!(d.status, StatusCode::NO_CONTENT, "{}", d.body);
    }
    assert!(
        st.get_ok(lr::INVITES, &me).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let state: String = sqlx::query_scalar("SELECT state FROM world_invite WHERE id = $1::uuid")
        .bind(&invite)
        .fetch_one(&st.owner)
        .await
        .unwrap();
    assert_eq!(state, "declined");
    assert!(
        row(&st.get_ok(lr::FRIENDS, &me).await, &host.xuid)
            .get("invite")
            .is_none()
    );
    let d = decline(&host, &request).await;
    assert_eq!(d.status, StatusCode::NO_CONTENT);
    assert!(
        st.get_ok(lr::INVITES, &host).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        decline(&me, &Uuid::now_v7().to_string()).await.status,
        StatusCode::NO_CONTENT
    );
    let bad = decline(&me, "nope").await;
    assert_code(&bad, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(bad.body["details"]["field"], "inviteId");

    // A free account (OD-129): its own empty inbox; declining what is not its own is a no-op
    // (204, idempotent, as for an unknown id). Unknown parameters 400.
    let own = st
        .get(lr::INVITES, Who::Launcher(Some(&free.access)))
        .send()
        .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    assert_eq!(own.body["items"], json!([]), "{}", own.body);
    assert_eq!(decline(&free, &invite).await.status, StatusCode::NO_CONTENT);
    let q = st
        .get(
            &format!("{}?bogus=1", lr::INVITES),
            Who::Launcher(Some(&me.access)),
        )
        .send()
        .await;
    assert_code(&q, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(q.body["details"]["field"], "query");
    let other = st
        .get(
            &format!("{}?asXuid={}", lr::INVITES, host.xuid),
            Who::Launcher(Some(&me.access)),
        )
        .send()
        .await;
    assert_code(&other, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    // Launcher alias only: no web mounting.
    let web = st
        .get("/core/v1/me/invites", Who::Web(Some(&me.cookie)))
        .send()
        .await;
    assert_eq!(web.status, StatusCode::NOT_FOUND);
}

/// CR-S106: the inbox's `sent` lists the caller's own invites and requests of the last 2 minutes
/// (or still pending) with their state: pending, declined (by the invitee's route), accepted,
/// expired by time before the sweep, withdrawn (the world closed); a request to a private official
/// world names its current leader; older ended ones and other accounts' offers never appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_inbox_lists_what_the_caller_sent_with_its_state() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let me = supporter(&st, &users).await;
    let pending = supporter(&st, &users).await;
    let decliner = supporter(&st, &users).await;
    let acceptor = supporter(&st, &users).await;
    let lapsed = supporter(&st, &users).await;
    let host = supporter(&st, &users).await;
    let owner = supporter(&st, &users).await;
    let leader = supporter(&st, &users).await;
    let other = supporter(&st, &users).await;
    for f in [
        &pending, &decliner, &acceptor, &lapsed, &host, &owner, &leader,
    ] {
        st.befriend(&me, f).await;
    }
    let world = st.host_world(&me, &me.xuid).await;
    let theirs = st.host_world(&host, &host.xuid).await;
    let private = st.official_world("freeroam", Some(&owner), Some(&me)).await;
    st.seat(&owner.xuid, &private).await;

    // Oldest first: an ended invite of 3 minutes ago (outside the window), then the one that
    // lapsed by time (still `pending` in the table), then the rest.
    let old = st
        .offer_at("invite", &world, &me.xuid, &pending, 180, 120)
        .await;
    st.owner_sql(
        "UPDATE world_invite SET state = 'declined', ended_at = now() - interval '170 seconds' WHERE id = $1::uuid",
        &old,
    )
    .await;
    let expired = st
        .offer_at("invite", &world, &me.xuid, &lapsed, 90, 60)
        .await;
    let withdrawn = st
        .offer_at("request", &theirs, &me.xuid, &host, 60, 120)
        .await;
    let accepted = st
        .offer_at("invite", &world, &me.xuid, &acceptor, 50, 120)
        .await;
    let declined = st
        .offer_at("invite", &world, &me.xuid, &decliner, 40, 120)
        .await;
    let request = st
        .offer_at("request", &private, &me.xuid, &owner, 30, 120)
        .await;
    let invite = st
        .offer_at("invite", &world, &me.xuid, &pending, 20, 120)
        .await;
    st.owner_sql(
        "UPDATE world_invite SET state = 'accepted', ended_at = now() WHERE id = $1::uuid",
        &accepted,
    )
    .await;
    let d = st
        .call(
            Method::POST,
            &format!("{}/{declined}/decline", lr::INVITES),
            Who::Launcher(Some(&decliner.access)),
        )
        .send()
        .await;
    assert_eq!(d.status, StatusCode::NO_CONTENT, "{}", d.body);
    // The host's world closes (its game left): its pending offers are withdrawn.
    let session: Uuid = sqlx::query_scalar("SELECT host_session_id FROM world WHERE id = $1::uuid")
        .bind(&theirs)
        .fetch_one(&st.owner)
        .await
        .unwrap();
    let mut c = st.owner.acquire().await.unwrap();
    assert_eq!(
        fi_db::matchmaking::close_worlds_of_session_in(&mut c, session)
            .await
            .unwrap(),
        1
    );
    drop(c);
    // Another account's invite to one of my friends.
    let foreign_world = st.host_world(&other, &other.xuid).await;
    let foreign = st
        .offer("invite", &foreign_world, &other.xuid, &pending)
        .await;

    let inbox = st.get_ok(lr::INVITES, &me).await;
    let sent = inbox["sent"]
        .as_array()
        .unwrap_or_else(|| panic!("sent present: {inbox}"));
    let got: Vec<(&str, &str, &str, &str)> = sent
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap(),
                s["kind"].as_str().unwrap(),
                s["state"].as_str().unwrap(),
                s["to"]["friendId"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (invite.as_str(), "invite", "pending", pending.xuid.as_str()),
            (request.as_str(), "request", "pending", owner.xuid.as_str()),
            (
                declined.as_str(),
                "invite",
                "declined",
                decliner.xuid.as_str()
            ),
            (
                accepted.as_str(),
                "invite",
                "accepted",
                acceptor.xuid.as_str()
            ),
            (
                withdrawn.as_str(),
                "request",
                "withdrawn",
                host.xuid.as_str()
            ),
            (expired.as_str(), "invite", "expired", lapsed.xuid.as_str()),
        ],
        "newest first, the old ended one and the foreign one absent: {inbox}"
    );
    let first = &sent[0];
    assert_eq!(first["worldId"].as_str(), Some(world.as_str()));
    assert_eq!(
        first["to"]["gamertag"].as_str(),
        Some(pending.gamertag.as_str())
    );
    assert!(sent.iter().all(|s| s["expiresAt"].as_str().is_some()));
    assert_eq!(sent[4]["worldId"].as_str(), Some(theirs.as_str()));
    let raw: String = sqlx::query_scalar("SELECT state FROM world_invite WHERE id = $1::uuid")
        .bind(&expired)
        .fetch_one(&st.owner)
        .await
        .unwrap();
    assert_eq!(raw, "pending", "expired reads expired before the sweep");
    assert!(!inbox.to_string().contains(&foreign));
    assert!(!inbox.to_string().contains(&old));

    // The private world's leader now answers the request: `to` follows it.
    st.seat(&leader.xuid, &private).await;
    sqlx::query("UPDATE world SET private_leader_xuid = $2::bigint WHERE id = $1::uuid")
        .bind(&private)
        .bind(&leader.xuid)
        .execute(&st.owner)
        .await
        .unwrap();
    let led = st.get_ok(lr::INVITES, &me).await;
    let r = led["sent"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == request.as_str())
        .unwrap_or_else(|| panic!("request listed: {led}"))
        .clone();
    assert_eq!(r["to"]["friendId"].as_str(), Some(leader.xuid.as_str()));
    assert_eq!(r["to"]["gamertag"].as_str(), Some(leader.gamertag.as_str()));
    assert_eq!(r["worldId"].as_str(), Some(private.as_str()));

    // Each account sees only its own: the invitee and the other sender.
    let theirs_sent = st.get_ok(lr::INVITES, &other).await;
    let ids: Vec<&str> = theirs_sent["sent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [foreign.as_str()], "{theirs_sent}");
    assert_eq!(st.get_ok(lr::INVITES, &pending).await["sent"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn presence_and_the_inbox_hold_one_pooled_connection_at_a_time() {
    let st = stack_with(Options {
        connections: 1,
        vars: vec![("FI_LOCAL_LIMIT_SCALE", "10".into())],
        ..Options::default()
    })
    .await;
    let users = Users::new(&st);
    let (a, b) = (supporter(&st, &users).await, supporter(&st, &users).await);
    st.befriend(&a, &b).await;
    let world = st.host_world(&b, &b.xuid).await;
    let invite = st.offer("invite", &world, &b.xuid, &a).await;
    let list = st
        .get_ok(&format!("{}?asXuid={}", lr::FRIENDS, a.xuid), &a)
        .await;
    assert_eq!(row(&list, &b.xuid)["playing"]["joinable"], true);
    let inbox = st
        .get_ok(&format!("{}?asXuid={}", lr::INVITES, a.xuid), &a)
        .await;
    assert_eq!(inbox["items"][0]["id"].as_str(), Some(invite.as_str()));
    let d = st
        .call(
            Method::POST,
            &format!("{}/{invite}/decline", lr::INVITES),
            Who::Launcher(Some(&a.access)),
        )
        .send()
        .await;
    assert_eq!(d.status, StatusCode::NO_CONTENT, "{}", d.body);
}

/// The per-account read bucket (review finding 6): 30 friend-list reads a minute per account,
/// both mountings counted together, then 429 `REQUEST.RATE_LIMITED` with `Retry-After`; the
/// invite inbox has its own 30.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn friend_and_invite_reads_are_thirty_a_minute_per_account() {
    let st = stack_with(Options {
        rates: Rates {
            account_read: Rate::per_minute(30),
            ..roomy()
        },
        ..Options::default()
    })
    .await;
    let users = Users::new(&st);
    let a = supporter(&st, &users).await;
    for i in 0..30 {
        let r = if i % 2 == 0 {
            st.get(lr::FRIENDS, Who::Launcher(Some(&a.access)))
                .send()
                .await
        } else {
            st.get(cr::ME_FRIENDS, Who::Web(Some(&a.cookie)))
                .send()
                .await
        };
        assert_eq!(r.status, StatusCode::OK, "read {i}: {}", r.body);
    }
    let limited = st
        .get(lr::FRIENDS, Who::Launcher(Some(&a.access)))
        .send()
        .await;
    assert_code(
        &limited,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );
    assert!(limited.header("retry-after").is_some());
    // Another account reads on.
    let b = supporter(&st, &users).await;
    let other = st
        .get(lr::FRIENDS, Who::Launcher(Some(&b.access)))
        .send()
        .await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
    for i in 0..30 {
        let r = st
            .get(lr::INVITES, Who::Launcher(Some(&b.access)))
            .send()
            .await;
        assert_eq!(r.status, StatusCode::OK, "invites {i}: {}", r.body);
    }
    let limited = st
        .get(lr::INVITES, Who::Launcher(Some(&b.access)))
        .send()
        .await;
    assert_code(
        &limited,
        StatusCode::TOO_MANY_REQUESTS,
        "REQUEST.RATE_LIMITED",
    );
}

// ---- Sidebar data and friends coming online (CORE_API.md §4.2a, OD-211, CR-S102) ----

impl Stack {
    /// An open official world of the VM `e2e-vm` running `activity`; with `owner`, a private one
    /// (an official group world owned by that player's first profile, MATCHMAKING_API.md §2.10)
    /// to which `access` has access.
    async fn official_world(
        &self,
        activity: &str,
        owner: Option<&Player>,
        access: Option<&Player>,
    ) -> String {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO world (id, game, host_kind, host_vm_id, host_tier, kind, name, mode, players, peak_players, \
               max_players, races, audience, endpoint_ip, endpoint_port, opened_at, expires_at, activity, lobbies, \
               any_mode, voice, transport, official_group, official_owner_profile_id, official_owner_account_id) \
             VALUES ($1, 'fh2', 'official', 'e2e-vm', 'free', 'free', 'E2E official', 'hardcore', 0, 0, 12, true, \
               'everyone', '198.51.100.20', 5101, now(), now() + interval '45 seconds', $2::text, \
               CASE WHEN $2::text = 'freeroam' THEN '{}'::text[] ELSE ARRAY[$2::text] END, $2::text = 'carmeet', \
               'proximity', \
               'direct', $3::uuid IS NOT NULL, \
               (SELECT id FROM profile WHERE account_id = $3::uuid ORDER BY created_at LIMIT 1), $3::uuid)",
        )
        .bind(id)
        .bind(activity)
        .bind(owner.map(|o| o.account_id.clone()))
        .execute(&self.owner)
        .await
        .unwrap();
        if let Some(a) = access {
            sqlx::query(
                "INSERT INTO world_access (world_id, account_id, via) VALUES ($1, $2::uuid, 'invite')",
            )
            .bind(id)
            .bind(&a.account_id)
            .execute(&self.owner)
            .await
            .unwrap();
        }
        id.to_string()
    }

    /// The hook's activity report on the active session of the profile `xuid`: rich-presence
    /// screen `rp`, reported `age_secs` ago.
    async fn report_screen(&self, xuid: &str, rp: i16, age_secs: i64) {
        sqlx::query(
            "UPDATE play_session SET activity_seq = 1, activity_odo_base_m = 0, activity_progress_at = now(), \
               activity_engaged_at = now(), activity_reported_at = now() - make_interval(secs => $3), \
               activity_rp = $2 \
             WHERE profile_id = (SELECT id FROM profile WHERE xuid = $1::bigint) AND state = 'active'",
        )
        .bind(xuid)
        .bind(rp)
        .bind(age_secs as f64)
        .execute(&self.owner)
        .await
        .unwrap();
    }

    /// The `online` list of `p`'s inbox poll.
    async fn came_online(&self, p: &Player) -> Vec<Value> {
        let inbox = self.get_ok(lr::INVITES, p).await;
        inbox["online"]
            .as_array()
            .unwrap_or_else(|| panic!("online present: {inbox}"))
            .clone()
    }

    /// Ends the active sessions of the profile `xuid` `ago_secs` seconds ago.
    async fn end_sessions(&self, xuid: &str, ago_secs: i64) {
        sqlx::query(
            "UPDATE play_session SET state = 'ended', ended_at = now() - make_interval(secs => $2), \
               end_reason = 'game_exit', admitted_at = least(admitted_at, now() - make_interval(secs => $2 + 1)) \
             WHERE profile_id = (SELECT id FROM profile WHERE xuid = $1::bigint) AND state = 'active'",
        )
        .bind(xuid)
        .bind(ago_secs as f64)
        .execute(&self.owner)
        .await
        .unwrap();
    }
}

fn friend_ids(online: &[Value]) -> Vec<String> {
    online
        .iter()
        .map(|o| o["friendId"].as_str().unwrap().to_owned())
        .collect()
}

/// CR-S102: a friend row carries the account's creation, its verified Discord link (the avatar
/// built from the stored hash, CORE_API.md §5a.2), each profile's play dashboard page while the
/// website lists it, and, in a world, what the friend does there: the activity, a private
/// official world, and a race from FH2's fresh activity report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn friend_rows_carry_the_sidebar_data() {
    let st = stack_with(Options {
        vars: vec![
            ("FI_LOCAL_LIMIT_SCALE", "10".into()),
            ("FI_PUBLIC_CORE_URL", "https://play.example.test".into()),
        ],
        ..Options::default()
    })
    .await;
    let users = Users::new(&st);
    let me = supporter(&st, &users).await;
    let linked = supporter(&st, &users).await;
    let unlisted = supporter(&st, &users).await;
    let hidden = supporter(&st, &users).await;
    let host = supporter(&st, &users).await;
    let racer = supporter(&st, &users).await;
    let cruiser = supporter(&st, &users).await;
    let stale = supporter(&st, &users).await;
    let lobby = supporter(&st, &users).await;
    let secret = supporter(&st, &users).await;
    for f in [
        &linked, &unlisted, &hidden, &host, &racer, &cruiser, &stale, &lobby, &secret,
    ] {
        st.befriend(&me, f).await;
    }
    let hash = "0123456789abcdef0123456789abcdef";
    sqlx::query(
        "UPDATE account SET discord_user_id = 80351110224678912, discord_username = 'synthetic_user', \
           discord_global_name = 'Synthetic', discord_avatar = $2, discord_linked_at = now(), \
           discord_refreshed_at = now() WHERE id = $1::uuid",
    )
    .bind(&linked.account_id)
    .bind(hash)
    .execute(&st.owner)
    .await
    .unwrap();
    st.owner_sql(
        "UPDATE profile SET web_listed = false WHERE xuid = $1::bigint",
        &unlisted.xuid,
    )
    .await;
    st.owner_sql(
        "UPDATE profile SET social_hidden_at = now() WHERE xuid = $1::bigint",
        &hidden.xuid,
    )
    .await;
    // A player world; official worlds: a race screen fresh and a cruise fresh (FH2's ids), a race
    // screen reported too long ago, and two private lobbies, one the caller may see.
    let player_world = st.host_world(&host, &host.xuid).await;
    let meet = st.official_world("carmeet", None, None).await;
    st.seat(&racer.xuid, &meet).await;
    st.report_screen(&racer.xuid, 18, 5).await;
    let trip = st.official_world("roadtrip", None, None).await;
    st.seat(&cruiser.xuid, &trip).await;
    st.report_screen(&cruiser.xuid, 15, 5).await;
    st.seat(&stale.xuid, &trip).await;
    st.report_screen(&stale.xuid, 18, 300).await;
    let private = st.official_world("freeroam", Some(&lobby), Some(&me)).await;
    st.seat(&lobby.xuid, &private).await;
    let closed = st.official_world("freeroam", Some(&secret), None).await;
    st.seat(&secret.xuid, &closed).await;

    let list = st.get_ok(lr::FRIENDS, &me).await;
    let created: String = sqlx::query_scalar(
        "SELECT to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS') FROM account WHERE id = $1::uuid",
    )
    .bind(&linked.account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    let l = row(&list, &linked.xuid);
    assert!(
        l["accountSince"].as_str().unwrap().starts_with(&created),
        "{l}"
    );
    assert_eq!(
        l["discord"],
        json!({"username": "synthetic_user", "globalName": "Synthetic",
               "avatarUrl": format!("https://cdn.discordapp.com/avatars/80351110224678912/{hash}.png?size=256")})
    );
    assert_eq!(
        l["profiles"][0]["profileUrl"].as_str(),
        Some(
            format!(
                "https://play.example.test/fh2/hardcore/profiles/{}",
                linked.gamertag
            )
            .as_str()
        )
    );
    assert!(l.get("playing").is_none(), "never played: {l}");
    for f in [&unlisted, &hidden] {
        let r = row(&list, &f.xuid);
        assert!(r.get("discord").is_none(), "no link: {r}");
        assert!(r["accountSince"].is_string());
        assert!(
            r["profiles"][0].get("profileUrl").is_none(),
            "not on the website: {r}"
        );
    }
    let h = &row(&list, &host.xuid)["playing"];
    assert_eq!(h["worldId"].as_str(), Some(player_world.as_str()));
    assert_eq!(
        h["doing"],
        json!({"activity": "freeroam", "private": false})
    );
    let r = &row(&list, &racer.xuid)["playing"];
    assert_eq!(r["officialActivity"], "carmeet");
    assert_eq!(
        r["doing"],
        json!({"activity": "carmeet", "private": false, "racing": true})
    );
    assert_eq!(
        row(&list, &cruiser.xuid)["playing"]["doing"],
        json!({"activity": "roadtrip", "private": false, "racing": false})
    );
    assert_eq!(
        row(&list, &stale.xuid)["playing"]["doing"],
        json!({"activity": "roadtrip", "private": false}),
        "a report older than 90 s says nothing"
    );
    let p = &row(&list, &lobby.xuid)["playing"];
    assert_eq!(p["worldId"].as_str(), Some(private.as_str()));
    assert_eq!(p["doing"], json!({"activity": "freeroam", "private": true}));
    // A private lobby the caller has no access to: no world, nothing of what it does there.
    let s = &row(&list, &secret.xuid)["playing"];
    assert_eq!(s["state"], "in_game");
    assert!(
        s.get("worldId").is_none() && s.get("doing").is_none(),
        "{s}"
    );
    assert!(
        !list.to_string().contains("80351110224678912\""),
        "the Discord id only inside the avatar URL"
    );

    // The same sidebar data on the web mounting and on a write's answer.
    let web = st
        .get(cr::ME_FRIENDS, Who::Web(Some(&me.cookie)))
        .send()
        .await;
    assert_eq!(web.status, StatusCode::OK, "{}", web.body);
    assert_eq!(row(&web.body, &linked.xuid)["discord"], l["discord"]);
    let fav = st
        .call(
            Method::PUT,
            &at(&linked, "/favorite"),
            Who::Launcher(Some(&me.access)),
        )
        .key()
        .json(json!({"favorite": true}))
        .send()
        .await;
    assert_eq!(fav.status, StatusCode::OK, "{}", fav.body);
    assert_eq!(fav.body["discord"], l["discord"]);
    assert_eq!(fav.body["accountSince"], l["accountSince"]);
}

/// CR-S102: the inbox names the friends whose account was admitted to a play session since this
/// device's previous poll, oldest first: none on a device's first poll (or after an hour without
/// one), never a stranger, never twice, and not a friend who came back within 2 minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_inbox_names_friends_who_came_online_since_the_last_poll() {
    let st = roomy_stack().await;
    let users = Users::new(&st);
    let me = supporter(&st, &users).await;
    let my_token = users.tokens.lock().unwrap().last().unwrap().clone();
    let (first, second, third, back, late) = (
        supporter(&st, &users).await,
        member(&st, &users, "noob", true).await,
        supporter(&st, &users).await,
        supporter(&st, &users).await,
        supporter(&st, &users).await,
    );
    let stranger = supporter(&st, &users).await;
    for f in [&first, &second, &third, &back, &late] {
        st.befriend(&me, f).await;
    }
    // Already online before the launcher started: the first poll names nobody.
    st.live_session_of(&back.xuid).await;
    assert!(st.came_online(&me).await.is_empty(), "the first poll");

    st.live_session_of(&first.xuid).await;
    st.live_session_of(&second.xuid).await;
    st.live_session_of(&stranger.xuid).await;
    let online = st.came_online(&me).await;
    assert_eq!(
        friend_ids(&online),
        [first.xuid.clone(), second.xuid.clone()],
        "oldest first, friends only"
    );
    assert_eq!(
        online[1]["gamertag"].as_str(),
        Some(second.gamertag.as_str())
    );
    assert_eq!(
        (&online[1]["game"], &online[1]["mode"]),
        (&json!("fh2"), &json!("noob"))
    );
    assert!(online[1]["since"].is_string());
    assert!(st.came_online(&me).await.is_empty(), "never twice");

    // Back within 2 minutes: not repeated; back after longer: named again.
    st.end_sessions(&back.xuid, 30).await;
    st.live_session_of(&back.xuid).await;
    st.end_sessions(&first.xuid, 600).await;
    st.live_session_of(&first.xuid).await;
    assert_eq!(
        friend_ids(&st.came_online(&me).await),
        [first.xuid.as_str()]
    );

    // Another device (a new launcher sign-in): its own first poll names nobody.
    let again = st.launcher_sign_in(&my_token, &device()).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    let second_device = Player {
        cookie: me.cookie.clone(),
        access: again.body["accessToken"].as_str().unwrap().to_owned(),
        account_id: me.account_id.clone(),
        xuid: me.xuid.clone(),
        gamertag: me.gamertag.clone(),
        name: me.name.clone(),
    };
    st.live_session_of(&third.xuid).await;
    assert!(st.came_online(&second_device).await.is_empty());
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM friend_online_poll WHERE account_id = $1::uuid")
            .bind(&me.account_id)
            .fetch_one(&st.owner)
            .await
            .unwrap();
    assert_eq!(rows, 2, "one row per device");

    // An hour without a poll counts as a fresh start.
    st.owner_sql(
        "UPDATE friend_online_poll SET polled_at = now() - interval '61 minutes' WHERE account_id = $1::uuid",
        &me.account_id,
    )
    .await;
    st.live_session_of(&late.xuid).await;
    assert!(
        st.came_online(&second_device).await.is_empty(),
        "stale poll"
    );
    st.end_sessions(&late.xuid, 600).await;
    st.live_session_of(&late.xuid).await;
    assert_eq!(
        friend_ids(&st.came_online(&second_device).await),
        [late.xuid.as_str()]
    );

    // A launcher restart within the hour marks its first poll (`first=true`): recorded, nobody named,
    // and who came online before it is not named on the next poll either.
    st.end_sessions(&third.xuid, 600).await;
    st.live_session_of(&third.xuid).await;
    let restarted = st
        .get_ok(&format!("{}?first=true", lr::INVITES), &second_device)
        .await;
    assert_eq!(restarted["online"], json!([]), "{restarted}");
    assert!(
        st.came_online(&second_device).await.is_empty(),
        "after first"
    );
}
