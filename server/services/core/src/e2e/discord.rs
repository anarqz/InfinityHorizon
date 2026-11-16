//! Discord (CORE_API.md §5a, register CR-S63) over fi-core's whole router: the bot's routes behind
//! `X-Fi-Feed-Key` (feed, link preview, link, handle refresh) and the player's (settings, link
//! code, unlink, announce switch), every status and code; the moderator's view of the link; kicks
//! and bans in the feed as `staff` rows naming subject and actor; an opted-out profile hidden from
//! the feed; the routes off without configuration; the bot's own bucket. Run with the rest of the
//! e2e suites (see `super`'s header).

use super::*;

const LINK_URL: &str = "https://discord.example.test";

fn feed_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| format!("e2e-feed-key-{}{}", fresh(), fresh()))
}

fn digest(key: &str) -> String {
    fi_auth::hex(&fi_auth::sha256(key.as_bytes()))
}

async fn discord_stack(rates: Rates) -> Stack {
    stack_with(Options {
        vars: vec![
            ("FI_LOCAL_LIMIT_SCALE", "10".into()),
            ("FI_FEED_KEYS", format!("sha256:{}", digest(feed_key()))),
            ("FI_DISCORD_LINK_URL", LINK_URL.into()),
        ],
        rates,
        ..Options::default()
    })
    .await
}

/// A Discord user id no other run took.
fn snowflake() -> String {
    let b = Uuid::now_v7().into_bytes();
    let tail = u64::from_be_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
    (1_000_000_000_000_000 + tail % 8_000_000_000_000_000_000).to_string()
}

impl Stack {
    fn bot<'a>(&'a self, method: Method, uri: &str) -> Call<'a> {
        self.call(method, uri, Who::Bare)
            .header("x-fi-feed-key", feed_key())
    }

    /// The feed's end now, then every item after it until `done` holds (the feed holds back rows
    /// of transactions older than a running one, so a few polls may be needed).
    async fn feed_tail(&self) -> Option<String> {
        let mut after: Option<String> = None;
        loop {
            let uri = match &after {
                Some(a) => format!("{}?after={a}&limit=200", cr::FEED),
                None => format!("{}?limit=200", cr::FEED),
            };
            let page = self.bot(Method::GET, &uri).send().await;
            assert_eq!(page.status, StatusCode::OK, "{}", page.body);
            let next = page.body["next"].as_str().map(str::to_owned);
            if next == after {
                return after;
            }
            after = next;
        }
    }

    async fn feed_items(&self, after: &Option<String>) -> Vec<Value> {
        let mut out = Vec::new();
        let mut cursor = after.clone();
        loop {
            let uri = match &cursor {
                Some(a) => format!("{}?after={a}&limit=200", cr::FEED),
                None => format!("{}?limit=200", cr::FEED),
            };
            let page = self.bot(Method::GET, &uri).send().await;
            assert_eq!(page.status, StatusCode::OK, "{}", page.body);
            out.extend(page.body["items"].as_array().unwrap().iter().cloned());
            let next = page.body["next"].as_str().map(str::to_owned);
            if next == cursor {
                return out;
            }
            cursor = next;
        }
    }

    /// The feed items about `gamertag` after `after`, polling until `n` of them are visible.
    async fn feed_about(&self, after: &Option<String>, gamertag: &str, n: usize) -> Vec<Value> {
        for _ in 0..50 {
            let items: Vec<Value> = self
                .feed_items(after)
                .await
                .into_iter()
                .filter(|i| i["subject"]["name"] == gamertag)
                .collect();
            if items.len() >= n {
                return items;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("fewer than {n} feed items about {gamertag}");
    }
}

fn code_of(url: &str) -> String {
    url.strip_prefix(&format!("{LINK_URL}/link?code="))
        .unwrap_or_else(|| panic!("a link URL: {url}"))
        .to_owned()
}

/// Every status and code of the bot's and the player's routes, the link's lifecycle and what
/// moderators see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn discord_routes_answer_every_status_and_code() {
    let st = discord_stack(roomy()).await;
    let player = st.player("noob").await;
    let other = st.player("noob").await;
    let moderator = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let me = Who::Launcher(Some(&player.access));
    let start = st.feed_tail().await;

    // The bot key: missing, wrong, a launcher token instead: 401 AUTH.TOKEN_INVALID.
    let r = st.get(cr::FEED, Who::Bare).send().await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    let r = st
        .get(cr::FEED, Who::Bare)
        .header("x-fi-feed-key", &format!("{}x", feed_key()))
        .send()
        .await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    let r = st
        .get(cr::FEED, Who::Bare)
        .header("x-fi-feed-key", &digest(feed_key()))
        .send()
        .await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    let r = st.get(cr::FEED, me).send().await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    // 400 REQUEST.INVALID {field}.
    for (query, field) in [
        ("after=12", "after"),
        ("after=x.1", "after"),
        ("limit=0", "limit"),
        ("limit=201", "limit"),
        ("limit=ten", "limit"),
        ("other=1", "after"),
    ] {
        let r = st
            .bot(Method::GET, &format!("{}?{query}", cr::FEED))
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], field, "{query}");
    }
    // A cursor that names no kept row: a gap, an empty page, `next` the cursor itself.
    let r = st
        .bot(Method::GET, &format!("{}?after=1.1", cr::FEED))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["gap"], true);
    assert_eq!(r.headers.get("cache-control").unwrap(), "private, no-store");

    // The player's settings: available, unlinked, the profile announced by default.
    let s = st.get(cr::ME_DISCORD, me).send().await;
    assert_eq!(s.status, StatusCode::OK, "{}", s.body);
    assert_eq!(s.body["available"], true);
    assert_eq!(s.body["link"], Value::Null);
    assert_eq!(
        s.body["profiles"],
        json!([{"xuid": player.xuid, "game": "fh2", "gamertag": player.gamertag, "announce": true}])
    );
    let r = st.get(cr::ME_DISCORD, Who::Bare).send().await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);

    // The announce switch: 404 for an unknown or another account's profile, 400 for a bad body.
    let announce = |xuid: &str| cr::ME_PROFILE_ANNOUNCE.replace("{xuid}", xuid);
    for xuid in ["2535999999999999", other.xuid.as_str(), "x", "0"] {
        let r = st
            .call(Method::PUT, &announce(xuid), me)
            .json(json!({"announce": false}))
            .send()
            .await;
        assert_code(&r, StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND");
    }
    for body in [
        json!({}),
        json!({"announce": "no"}),
        json!({"announce": true, "x": 1}),
    ] {
        let r = st
            .call(Method::PUT, &announce(&player.xuid), me)
            .json(body.clone())
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    }

    // A link code: 201 with the bot's URL; a second one replaces it.
    let first = st.call(Method::POST, cr::ME_DISCORD_LINK, me).send().await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    let stale = code_of(first.body["url"].as_str().unwrap());
    let issued = st.call(Method::POST, cr::ME_DISCORD_LINK, me).send().await;
    assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.body);
    assert!(issued.body["expiresAt"].is_string());
    let code = code_of(issued.body["url"].as_str().unwrap());
    assert_eq!(code.len(), 43);

    // The preview: 401 without the key, 400 for a malformed or unknown/replaced code, 200.
    let r = st
        .post(cr::DISCORD_LINK_PREVIEW, Who::Bare, json!({"code": code}))
        .send()
        .await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    let r = st
        .bot(Method::POST, cr::DISCORD_LINK_PREVIEW)
        .json(json!({"code": "short"}))
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(r.body["details"]["field"], "code");
    for c in [stale.clone(), "A".repeat(43)] {
        let r = st
            .bot(Method::POST, cr::DISCORD_LINK_PREVIEW)
            .json(json!({"code": c}))
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "AUTH.CODE_INVALID");
    }
    let r = st
        .bot(Method::POST, cr::DISCORD_LINK_PREVIEW)
        .json(json!({"code": code}))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["names"], json!([player.gamertag]));

    // The link: 400 per field, 200, a replay 200, the same code with another user 400.
    let user = snowflake();
    for (body, field) in [
        (
            json!({"code": code, "discordUserId": "0", "username": "ab"}),
            "discordUserId",
        ),
        (
            json!({"code": code, "discordUserId": "01", "username": "ab"}),
            "discordUserId",
        ),
        (
            json!({"code": code, "discordUserId": user, "username": "a"}),
            "username",
        ),
        (
            json!({"code": code, "discordUserId": user, "username": "ab", "globalName": "x\u{7}"}),
            "globalName",
        ),
        (
            json!({"code": code, "discordUserId": user, "username": "ab", "email": "x"}),
            "email",
        ),
        (
            json!({"code": code, "discordUserId": user, "username": "ab", "avatar": "../../x.png"}),
            "avatar",
        ),
    ] {
        let r = st
            .bot(Method::PUT, cr::DISCORD_LINKS)
            .json(body.clone())
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], field, "{body}");
    }
    let avatar = "a_0123456789abcdef0123456789abcdef";
    let body = json!({"code": code, "discordUserId": user, "username": "linked.handle", "globalName": "Linked", "avatar": avatar});
    let linked = st
        .bot(Method::PUT, cr::DISCORD_LINKS)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(linked.status, StatusCode::OK, "{}", linked.body);
    assert_eq!(linked.body["names"], json!([player.gamertag]));
    let replay = st
        .bot(Method::PUT, cr::DISCORD_LINKS)
        .json(body)
        .send()
        .await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::OK, &linked.body)
    );
    let r = st
        .bot(Method::PUT, cr::DISCORD_LINKS)
        .json(json!({"code": code, "discordUserId": snowflake(), "username": "ab"}))
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "AUTH.CODE_INVALID");
    let s = st.get(cr::ME_DISCORD, me).send().await;
    assert_eq!(s.body["link"]["discordUserId"], user.as_str());
    assert_eq!(s.body["link"]["username"], "linked.handle");
    assert_eq!(s.body["link"]["globalName"], "Linked");
    assert_eq!(
        s.body["link"]["avatarUrl"],
        format!("https://cdn.discordapp.com/avatars/{user}/{avatar}.png?size=256")
    );

    // Another account with the same Discord user: 409 ACCOUNTS.ALREADY_LINKED, code unspent.
    let theirs = st
        .call(
            Method::POST,
            cr::ME_DISCORD_LINK,
            Who::Launcher(Some(&other.access)),
        )
        .send()
        .await;
    let their_code = code_of(theirs.body["url"].as_str().unwrap());
    let r = st
        .bot(Method::PUT, cr::DISCORD_LINKS)
        .json(json!({"code": their_code, "discordUserId": user, "username": "ab"}))
        .send()
        .await;
    assert_code(&r, StatusCode::CONFLICT, "ACCOUNTS.ALREADY_LINKED");
    assert_eq!(r.body["details"]["reason"], "discord_user_taken");
    let r = st
        .bot(Method::POST, cr::DISCORD_LINK_PREVIEW)
        .json(json!({"code": their_code}))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "still unspent: {}", r.body);

    // The bot's handle refresh: 200, 404 for an unlinked user, 400 for a bad id.
    let refresh = |id: &str| cr::DISCORD_USER.replace("{discordUserId}", id);
    let r = st
        .bot(Method::PUT, &refresh(&user))
        .json(json!({"username": "renamed.handle"}))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        (&r.body["username"], &r.body["globalName"]),
        (&json!("renamed.handle"), &Value::Null)
    );
    let r = st
        .bot(Method::PUT, &refresh(&snowflake()))
        .json(json!({"username": "ab"}))
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "ACCOUNTS.NOT_FOUND");
    let r = st
        .bot(Method::PUT, &refresh("abc"))
        .json(json!({"username": "ab"}))
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");

    // Moderators see the link on the account view.
    let view = st
        .get(
            &format!("/core/v1/admin/accounts/{}", player.account_id),
            Who::Launcher(Some(&moderator.access)),
        )
        .send()
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert_eq!(view.body["account"]["discordLink"]["discordUserId"], user);
    assert_eq!(
        view.body["account"]["discordLink"]["username"],
        "renamed.handle"
    );

    // A kick by the moderator: a staff row naming the player (with its Discord user) and the
    // moderator, even after the player opts out.
    let r = st
        .call(Method::PUT, &announce(&player.xuid), me)
        .json(json!({"announce": false}))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["announce"], false);
    let kick = st
        .post(
            cr::ADMIN_KICKS,
            Who::Launcher(Some(&moderator.access)),
            json!({"xuid": player.xuid, "reason": "synthetic", "cooldownMinutes": 1}),
        )
        .send()
        .await;
    assert_eq!(kick.status, StatusCode::OK, "{}", kick.body);
    let items = st.feed_about(&start, &player.gamertag, 1).await;
    let kicked = items
        .iter()
        .find(|i| i["kind"] == "player_kicked")
        .expect("a kick row");
    assert_eq!(kicked["audience"], "staff");
    assert_eq!(kicked["subject"]["discord"]["id"], user);
    assert_eq!(kicked["actor"]["name"], moderator.gamertag);
    assert_eq!(kicked["actor"]["discord"], Value::Null);
    assert!(kicked["until"].is_string());

    // Unlink: 204, again 204, the link gone; the feed reads the link now.
    for _ in 0..2 {
        let r = st
            .call(Method::DELETE, cr::ME_DISCORD_LINK, me)
            .send()
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    }
    let s = st.get(cr::ME_DISCORD, me).send().await;
    assert_eq!(s.body["link"], Value::Null);
    let items = st.feed_about(&start, &player.gamertag, 1).await;
    assert!(items.iter().all(|i| i["subject"]["discord"].is_null()));
}

/// An opted-out profile is never named in a public row; a ban is a staff row; the bot's routes are
/// off (503) without configuration and have their own bucket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_feed_hides_opted_out_profiles_and_the_routes_need_configuration() {
    let st = discord_stack(roomy()).await;
    let shown = st.player("noob").await;
    let quiet = st.player("noob").await;
    let admin = st.player("noob").await;
    st.set_role(&admin.account_id, "admin").await;
    let start = st.feed_tail().await;
    let r = st
        .call(
            Method::PUT,
            &cr::ME_PROFILE_ANNOUNCE.replace("{xuid}", &quiet.xuid),
            Who::Web(Some(&quiet.cookie)),
        )
        .json(json!({"announce": false}))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // Both profiles' first admission, as the admission writes it.
    for p in [&shown, &quiet] {
        let mut conn = st.owner.acquire().await.unwrap();
        let profile: Uuid = sqlx::query_scalar("SELECT id FROM profile WHERE xuid = $1::bigint")
            .bind(&p.xuid)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        fi_db::discord::first_admission_in(&mut conn, profile)
            .await
            .unwrap();
    }
    // A ban of the quiet player: a staff row names it anyway.
    let ban = st
        .post(
            cr::ADMIN_BANS,
            Who::Launcher(Some(&admin.access)),
            json!({"accountId": quiet.account_id, "until": null, "reason": "synthetic"}),
        )
        .send()
        .await;
    assert_eq!(ban.status, StatusCode::CREATED, "{}", ban.body);
    let shown_items = st.feed_about(&start, &shown.gamertag, 1).await;
    assert_eq!(shown_items[0]["kind"], "player_joined");
    assert_eq!(shown_items[0]["audience"], "public");
    assert_eq!(shown_items[0]["actor"], Value::Null);
    assert_eq!(shown_items[0]["record"], Value::Null);
    // CR-S64: two record rows of the shown player, as SubmitScore writes them (fi-db's own suite
    // proves when): the previous holder named (the admin) and hidden (the quiet player).
    for rival in [&admin, &quiet] {
        sqlx::query(
            "INSERT INTO public_event (game, kind, audience, subject_account, subject_profile, \
               subject_name, actor_account, actor_profile, actor_name, record) \
             SELECT s.game, 'speed_record', 'public', s.account_id, s.id, s.name, r.account_id, r.id, r.name, \
               '{\"key\": \"speed_trap:9:5:11\", \"board\": \"speed_trap\", \"mode\": \"noob\", \
                 \"unit\": \"speed_mps\", \"value\": 67.2, \"previousValue\": 64.0, \"carId\": 247, \
                 \"carName\": \"Toyota 2000 GT 1969\", \"boardId\": 11, \"routeId\": null, \"routeName\": null}' \
             FROM profile s, profile r WHERE s.xuid = $1::bigint AND r.xuid = $2::bigint",
        )
        .bind(&shown.xuid)
        .bind(&rival.xuid)
        .execute(&st.owner)
        .await
        .unwrap();
    }
    let records: Vec<Value> = st
        .feed_about(&start, &shown.gamertag, 3)
        .await
        .into_iter()
        .filter(|i| i["kind"] == "speed_record")
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["audience"], "public");
    assert_eq!(records[0]["actor"]["name"], admin.gamertag);
    assert_eq!(
        records[0]["record"],
        json!({"board": "speed_trap", "mode": "noob", "unit": "speed_mps", "value": 67.2,
            "previousValue": 64.0, "carId": 247, "carName": "Toyota 2000 GT 1969", "boardId": 11,
            "routeId": null, "routeName": null})
    );
    assert_eq!(
        records[1]["actor"],
        Value::Null,
        "an opted-out previous holder"
    );
    let quiet_items = st.feed_about(&start, &quiet.gamertag, 1).await;
    assert_eq!(
        quiet_items
            .iter()
            .map(|i| (i["kind"].clone(), i["audience"].clone()))
            .collect::<Vec<_>>(),
        vec![(json!("player_banned"), json!("staff"))]
    );
    assert_eq!(quiet_items[0]["actor"]["name"], admin.gamertag);
    assert_eq!(quiet_items[0]["until"], Value::Null);
    // The ban replayed (same key) writes no second row.
    let replay_key = Uuid::now_v7().to_string();
    let replay = st
        .call(
            Method::POST,
            cr::ADMIN_BANS,
            Who::Launcher(Some(&admin.access)),
        )
        .header("idempotency-key", &replay_key)
        .json(json!({"accountId": shown.account_id, "until": null, "reason": "synthetic"}))
        .send()
        .await;
    assert_eq!(replay.status, StatusCode::CREATED, "{}", replay.body);
    let again = st
        .call(
            Method::POST,
            cr::ADMIN_BANS,
            Who::Launcher(Some(&admin.access)),
        )
        .header("idempotency-key", &replay_key)
        .json(json!({"accountId": shown.account_id, "until": null, "reason": "synthetic"}))
        .send()
        .await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.body);
    let bans = st
        .feed_about(&start, &shown.gamertag, 2)
        .await
        .into_iter()
        .filter(|i| i["kind"] == "player_banned")
        .count();
    assert_eq!(bans, 1, "a replay writes no row");

    // Without configuration: the bot's routes and the link code answer 503; settings say so.
    let bare = stack_with(Options {
        vars: vec![("FI_LOCAL_LIMIT_SCALE", "10".into())],
        ..Options::default()
    })
    .await;
    let r = bare.bot(Method::GET, cr::FEED).send().await;
    assert_code(&r, StatusCode::SERVICE_UNAVAILABLE, "SERVICE.UNAVAILABLE");
    let p = bare.player("noob").await;
    let r = bare
        .call(
            Method::POST,
            cr::ME_DISCORD_LINK,
            Who::Launcher(Some(&p.access)),
        )
        .send()
        .await;
    assert_code(&r, StatusCode::SERVICE_UNAVAILABLE, "SERVICE.UNAVAILABLE");
    let s = bare
        .get(cr::ME_DISCORD, Who::Launcher(Some(&p.access)))
        .send()
        .await;
    assert_eq!(s.body["available"], false);

    // A pending deletion: no link code (403 ACCOUNTS.DELETION_PENDING).
    let leaving = st.player("noob").await;
    sqlx::query(
        "UPDATE account SET status = 'deletion_pending', deletion_requested_at = clock_timestamp(), \
           deletion_due_at = clock_timestamp() + interval '14 days' WHERE id = $1::uuid",
    )
    .bind(&leaving.account_id)
    .execute(&st.owner)
    .await
    .unwrap();
    let r = st
        .call(
            Method::POST,
            cr::ME_DISCORD_LINK,
            Who::Launcher(Some(&leaving.access)),
        )
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "ACCOUNTS.DELETION_PENDING");
}

/// The bot's bucket (60 a minute per network in the contract; 2 here): the third call is 429 with
/// `Retry-After`, and public reads keep their own bucket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_bot_has_its_own_bucket() {
    let st = discord_stack(Rates {
        bot: Rate::per_minute(2),
        ..roomy()
    })
    .await;
    for _ in 0..2 {
        let r = st.bot(Method::GET, cr::FEED).send().await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    }
    let r = st.bot(Method::GET, cr::FEED).send().await;
    assert_code(&r, StatusCode::TOO_MANY_REQUESTS, "REQUEST.RATE_LIMITED");
    assert!(r.headers.get("retry-after").is_some());
    let r = st.get(cr::STATUS, Who::Bare).send().await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}
