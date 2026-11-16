//! Voice moderation (CORE_API.md §4.4, §5, §5a, §2.3; register CR-S68) over fi-core's whole router:
//! `POST /launcher/v1/voice/mutes` with the launcher's access token (202 for a change and for a
//! repeat of the current state, 400 per field and for oneself, 401, 404 `VOICE.NOT_SHARED` for
//! every way of not sharing the named world, 429 `VOICE.RATE_LIMITED` past `voice.mute`); the
//! community mute (3 distinct muter accounts with a played session within 7 days → 3 h, an
//! unmute removes a muter, never extended, only newer mutes after it ends); the moderators' voice
//! bans and their lift (every status and code, CR-S62's role rules); the `voice_blocked` notice;
//! the staff feed rows and the moderation view. Worlds and seats are matchmaking's own code on live
//! play sessions (`super::reports`). Run with the rest of the e2e suites (see `super`'s header).

use super::reports::{in_game, open_world, seat};
use super::*;
use fi_db::matchmaking;

/// A stack whose sign-in and account-creation limits allow the players these suites make.
async fn roomy_stack() -> Stack {
    stack_with(Options {
        vars: vec![("FI_LOCAL_LIMIT_SCALE", "10".into())],
        ..Options::default()
    })
    .await
}

fn me(p: &Player) -> Who<'_> {
    Who::Launcher(Some(p.access.as_str()))
}

fn mute(target: &Player, action: &str, world: Uuid) -> Value {
    json!({"xuid": target.xuid, "action": action, "worldId": world.to_string()})
}

async fn events(st: &Stack, muter: &Player, target: &Player) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT action FROM voice_mute_event WHERE muter_account_id = $1::uuid \
           AND target_account_id = $2::uuid ORDER BY at, id",
    )
    .bind(&muter.account_id)
    .bind(&target.account_id)
    .fetch_all(&st.owner)
    .await
    .unwrap()
}

/// The account's `voice_community_until` and `voice_banned_until`, as Unix seconds.
async fn blocks(st: &Stack, p: &Player) -> (Option<f64>, Option<f64>) {
    sqlx::query_as(
        "SELECT extract(epoch FROM voice_community_until)::float8, \
           extract(epoch FROM voice_banned_until)::float8 FROM account WHERE id = $1::uuid",
    )
    .bind(&p.account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap()
}

async fn now_secs(st: &Stack) -> f64 {
    sqlx::query_scalar("SELECT extract(epoch FROM clock_timestamp())::float8")
        .fetch_one(&st.owner)
        .await
        .unwrap()
}

/// The player's `voice_blocked` notice, if any.
async fn voice_notice(st: &Stack, p: &Player) -> Option<Value> {
    let notices = st
        .get(cr::ME_NOTICES, Who::Web(Some(&p.cookie)))
        .send()
        .await;
    assert_eq!(notices.status, StatusCode::OK, "{}", notices.body);
    notices.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "voice_blocked")
        .cloned()
}

/// Staff feed rows about `p` of `kind`: `(actor account, until set)`.
async fn staff_rows(st: &Stack, p: &Player, kind: &str) -> Vec<(Option<Uuid>, bool)> {
    sqlx::query_as(
        "SELECT actor_account, until IS NOT NULL FROM public_event \
         WHERE subject_account = $1::uuid AND kind = $2 AND audience = 'staff' ORDER BY id",
    )
    .bind(&p.account_id)
    .bind(kind)
    .fetch_all(&st.owner)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn a_voice_mute_needs_the_named_world_shared_in_the_last_half_hour() {
    let st = roomy_stack().await;
    let host = st.player("noob").await;
    let seated = st.player("noob").await;
    let stranger = st.player("noob").await;
    let h = in_game(&st, &host).await;
    let s = in_game(&st, &seated).await;
    let world = open_world(&st, &h).await;
    seat(&st, &s, world).await;
    let uri = lr::VOICE_MUTES;

    // 202, no Idempotency-Key needed; the row names accounts and the world.
    let first = st
        .call(Method::POST, uri, me(&seated))
        .json(mute(&host, "muted", world))
        .send()
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.body);
    assert_eq!(
        first.body,
        json!({"xuid": host.xuid, "action": "muted"}),
        "{}",
        first.body
    );
    assert_eq!(events(&st, &seated, &host).await, ["muted"]);
    // A repeat of the current state: 202, nothing written.
    let again = st
        .post(uri, me(&seated), mute(&host, "muted", world))
        .send()
        .await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
    assert_eq!(events(&st, &seated, &host).await, ["muted"]);
    // An unmute changes it; an unmute of nobody muted is a repeat.
    let un = st
        .post(uri, me(&seated), mute(&host, "unmuted", world))
        .send()
        .await;
    assert_eq!(un.status, StatusCode::ACCEPTED, "{}", un.body);
    let un2 = st
        .post(uri, me(&host), mute(&seated, "unmuted", world))
        .send()
        .await;
    assert_eq!(un2.status, StatusCode::ACCEPTED, "{}", un2.body);
    assert_eq!(events(&st, &seated, &host).await, ["muted", "unmuted"]);
    assert!(events(&st, &host, &seated).await.is_empty());
    // The host mutes its guest too (both ways).
    let back = st
        .post(uri, me(&host), mute(&seated, "muted", world))
        .send()
        .await;
    assert_eq!(back.status, StatusCode::ACCEPTED, "{}", back.body);
    // Two players changing their mutes of each other at once: both answered (no lock cycle).
    for round in ["unmuted", "muted", "unmuted", "muted"] {
        let (one, two) = tokio::join!(
            st.post(uri, me(&host), mute(&seated, round, world)).send(),
            st.post(uri, me(&seated), mute(&host, round, world)).send(),
        );
        assert_eq!(one.status, StatusCode::ACCEPTED, "{}", one.body);
        assert_eq!(two.status, StatusCode::ACCEPTED, "{}", two.body);
    }
    accepted(&st, &seated, &host, "unmuted", world).await;
    assert_eq!(
        events(&st, &seated, &host).await,
        ["muted", "unmuted", "muted", "unmuted", "muted", "unmuted"]
    );
    // A non-canonical XUID is refused.
    let padded = st
        .post(
            uri,
            me(&seated),
            json!({"xuid": format!("0{}", host.xuid), "action": "muted", "worldId": world.to_string()}),
        )
        .send()
        .await;
    assert_code(&padded, StatusCode::BAD_REQUEST, "REQUEST.INVALID");

    // 400: oneself, each field, an unknown field.
    let own = st
        .post(uri, me(&seated), mute(&seated, "muted", world))
        .send()
        .await;
    assert_code(&own, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(own.body["details"]["field"], "xuid");
    let w = world.to_string();
    for (bad, field) in [
        (
            json!({"xuid": "player", "action": "muted", "worldId": w}),
            "xuid",
        ),
        (
            json!({"xuid": host.xuid, "action": "deafened", "worldId": w}),
            "action",
        ),
        (
            json!({"xuid": host.xuid, "action": "muted", "worldId": "w"}),
            "worldId",
        ),
        (json!({"xuid": host.xuid, "action": "muted"}), "worldId"),
        (json!({"action": "muted", "worldId": w}), "xuid"),
        (
            json!({"xuid": host.xuid, "action": "muted", "worldId": w, "reason": "x"}),
            "reason",
        ),
    ] {
        let a = st.post(uri, me(&seated), bad.clone()).send().await;
        assert_code(&a, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(a.body["details"]["field"], field, "{bad}");
    }
    // 401: no launcher token (a web session does not open the launcher's alias).
    for who in [Who::Bare, Who::Web(Some(seated.cookie.as_str()))] {
        let a = st.post(uri, who, mute(&host, "muted", world)).send().await;
        assert_code(&a, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    }
    // 404 VOICE.NOT_SHARED, one answer: never together, an unknown player, an unknown world,
    // another world the target was never in.
    let other_world = open_world(&st, &in_game(&st, &stranger).await).await;
    for (who, b) in [
        (&stranger, mute(&host, "muted", world)),
        (
            &seated,
            json!({"xuid": "2535999999999999", "action": "muted", "worldId": w}),
        ),
        (&seated, mute(&host, "muted", Uuid::now_v7())),
        (&seated, mute(&host, "muted", other_world)),
    ] {
        let a = st.post(uri, me(who), b.clone()).send().await;
        assert_code(&a, StatusCode::NOT_FOUND, "VOICE.NOT_SHARED");
    }
    assert!(events(&st, &stranger, &host).await.is_empty());
    // Over 30 minutes ago: a mute is 404, but withdrawing a mute held is always accepted.
    matchmaking::leave_seat(&st.owner, &s).await.unwrap();
    sqlx::query(
        "UPDATE mm_grant SET issued_at = issued_at - interval '1 hour', expires_at = expires_at - interval '1 hour', \
           hold_until = hold_until - interval '1 hour', ended_at = ended_at - interval '1 hour' \
         WHERE world_id = $1 AND profile_id = $2",
    )
    .bind(world)
    .bind(s.profile_id)
    .execute(&st.owner)
    .await
    .unwrap();
    let late = st
        .post(uri, me(&seated), mute(&host, "muted", world))
        .send()
        .await;
    assert_code(&late, StatusCode::NOT_FOUND, "VOICE.NOT_SHARED");
    let withdraw = st
        .post(uri, me(&host), mute(&seated, "unmuted", world))
        .send()
        .await;
    assert_eq!(withdraw.status, StatusCode::ACCEPTED, "{}", withdraw.body);
    assert_eq!(
        events(&st, &host, &seated).await,
        ["muted", "unmuted", "muted", "unmuted", "muted", "unmuted"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn the_sixty_first_voice_mute_in_an_hour_is_429() {
    let st = roomy_stack().await;
    let p = st.player("noob").await;
    let host = st.player("noob").await;
    let world = open_world(&st, &in_game(&st, &host).await).await;
    seat(&st, &in_game(&st, &p).await, world).await;
    // Every request past the field checks counts: changes, repeats and unknown players alike.
    for i in 0..60 {
        let b = match i % 3 {
            0 => mute(&host, "muted", world),
            1 => mute(&host, "unmuted", world),
            _ => {
                json!({"xuid": "2535999999999998", "action": "muted", "worldId": world.to_string()})
            }
        };
        let a = st.post(lr::VOICE_MUTES, me(&p), b).send().await;
        assert_ne!(a.status, StatusCode::TOO_MANY_REQUESTS, "mute {i}");
    }
    let over = st
        .post(lr::VOICE_MUTES, me(&p), mute(&host, "muted", world))
        .send()
        .await;
    assert_code(&over, StatusCode::TOO_MANY_REQUESTS, "VOICE.RATE_LIMITED");
    let retry: u64 = over.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=3600).contains(&retry), "{retry}");
    // A bad body is refused before the bucket (400, not 429).
    let bad = st
        .post(lr::VOICE_MUTES, me(&p), json!({"xuid": "x"}))
        .send()
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
}

async fn accepted(st: &Stack, who: &Player, target: &Player, action: &str, world: Uuid) {
    let r = st
        .post(lr::VOICE_MUTES, me(who), mute(target, action, world))
        .send()
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn three_muters_in_a_week_block_a_voice_for_three_hours() {
    let st = roomy_stack().await;
    let target = st.player("noob").await;
    let muters = [
        st.player("noob").await,
        st.player("noob").await,
        st.player("noob").await,
        st.player("noob").await,
    ];
    let never_played = st.player("noob").await;
    let moderator = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    let t = in_game(&st, &target).await;
    let world = open_world(&st, &t).await;
    for m in &muters {
        seat(&st, &in_game(&st, m).await, world).await;
    }
    let [a, b, c, d] = &muters;
    // A muter account that never played does not count (its mute written directly: it could not
    // share a world without a session).
    sqlx::query(
        "INSERT INTO voice_mute_event (muter_account_id, target_account_id, action, world_id) \
         VALUES ($1::uuid, $2::uuid, 'muted', $3)",
    )
    .bind(&never_played.account_id)
    .bind(&target.account_id)
    .bind(world)
    .execute(&st.owner)
    .await
    .unwrap();
    accepted(&st, a, &target, "muted", world).await;
    accepted(&st, b, &target, "muted", world).await;
    assert_eq!(
        blocks(&st, &target).await.0,
        None,
        "two players and one who never played"
    );
    // An unmute removes its muter: B, C = 2.
    accepted(&st, a, &target, "unmuted", world).await;
    accepted(&st, c, &target, "muted", world).await;
    assert_eq!(blocks(&st, &target).await.0, None);
    assert!(voice_notice(&st, &target).await.is_none());
    // A again: three distinct accounts muting now, each with a played session → 3 hours.
    accepted(&st, a, &target, "muted", world).await;
    let now = now_secs(&st).await;
    let until = blocks(&st, &target).await.0.expect("a community mute");
    assert!((until - now - 10_800.0).abs() < 60.0, "{until} {now}");
    assert_eq!(
        staff_rows(&st, &target, "voice_community_muted").await,
        [(None, true)],
        "one staff row, no actor"
    );
    // The player is told: a warning with the end, never who muted.
    let notice = voice_notice(&st, &target)
        .await
        .expect("a voice_blocked notice");
    assert_eq!(notice["level"], "warning");
    assert!(notice["until"].is_string(), "{notice}");
    assert!(notice["game"].is_null(), "{notice}");
    let text = notice["text"].as_str().unwrap();
    for m in &muters {
        assert!(!text.contains(&m.name), "{text}");
    }
    // A block in force is not extended by a fourth mute.
    accepted(&st, d, &target, "muted", world).await;
    assert_eq!(blocks(&st, &target).await.0, Some(until));
    assert_eq!(
        staff_rows(&st, &target, "voice_community_muted")
            .await
            .len(),
        1
    );
    // Moderators read who muted, when and in which world.
    let view = st
        .get(
            &format!("/core/v1/admin/accounts/{}", target.account_id),
            Who::Web(Some(&moderator.cookie)),
        )
        .send()
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    let voice = &view.body["voice"];
    assert!(voice["communityUntil"].is_string(), "{voice}");
    assert!(voice["bannedUntil"].is_null(), "{voice}");
    let mutes = voice["mutes"].as_array().unwrap();
    assert_eq!(mutes.len(), 7, "{voice}");
    assert!(
        mutes
            .iter()
            .all(|m| m["worldId"] == world.to_string() && m["game"] == "fh2")
    );
    assert_eq!(mutes[0]["muterAccountId"], d.account_id.as_str());
    // After it ends only newer mutes count: the four standing mutes are older than its end (it
    // ends now).
    sqlx::query(
        "UPDATE account SET voice_community_until = clock_timestamp() \
         WHERE id = $1::uuid",
    )
    .bind(&target.account_id)
    .execute(&st.owner)
    .await
    .unwrap();
    assert!(voice_notice(&st, &target).await.is_none(), "it ended");
    accepted(&st, a, &target, "unmuted", world).await;
    accepted(&st, a, &target, "muted", world).await;
    accepted(&st, b, &target, "unmuted", world).await;
    accepted(&st, b, &target, "muted", world).await;
    let ended = blocks(&st, &target).await.0;
    let now = now_secs(&st).await;
    assert!(ended.is_some_and(|e| e < now), "not yet: two newer");
    accepted(&st, c, &target, "unmuted", world).await;
    accepted(&st, c, &target, "muted", world).await;
    let again = blocks(&st, &target).await.0.unwrap();
    assert!(again > now_secs(&st).await + 10_000.0, "three newer mutes");
    // A moderator's lift ends the community mute in force; only mutes newer than the lift count
    // afterwards: A, B and C still mute within the week, yet D's new mute alone blocks nobody (a
    // lift that forgot the end would count four).
    let lift = st
        .call(
            Method::DELETE,
            &format!("/core/v1/admin/voice-bans/{}", target.account_id),
            Who::Web(Some(&moderator.cookie)),
        )
        .key()
        .send()
        .await;
    assert_eq!(lift.status, StatusCode::NO_CONTENT, "{}", lift.body);
    assert!(voice_notice(&st, &target).await.is_none());
    assert_eq!(
        staff_rows(&st, &target, "voice_unbanned").await,
        [(Some(Uuid::parse_str(&moderator.account_id).unwrap()), false)]
    );
    accepted(&st, d, &target, "unmuted", world).await;
    accepted(&st, d, &target, "muted", world).await;
    let after_lift = blocks(&st, &target).await.0.unwrap();
    assert!(after_lift <= now_secs(&st).await, "one newer mute only");
    // Mutes older than 7 days never count: with no earlier end to compare against, four standing
    // mutes moved 8 days back and D's fresh one make one in the window.
    sqlx::query("UPDATE voice_mute_event SET at = at - interval '8 days' WHERE target_account_id = $1::uuid")
        .bind(&target.account_id)
        .execute(&st.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE account SET voice_community_until = NULL WHERE id = $1::uuid")
        .bind(&target.account_id)
        .execute(&st.owner)
        .await
        .unwrap();
    accepted(&st, d, &target, "unmuted", world).await;
    accepted(&st, d, &target, "muted", world).await;
    assert_eq!(blocks(&st, &target).await.0, None, "one mute in the window");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn moderators_ban_and_lift_a_voice() {
    let st = roomy_stack().await;
    let player = st.player("noob").await;
    let moderator = st.player("noob").await;
    let other_mod = st.player("noob").await;
    let admin = st.player("noob").await;
    st.set_role(&moderator.account_id, "moderator").await;
    st.set_role(&other_mod.account_id, "moderator").await;
    st.set_role(&admin.account_id, "admin").await;
    let m = Who::Web(Some(&moderator.cookie));
    let a = Who::Launcher(Some(&admin.access));
    let uri = cr::ADMIN_VOICE_BANS;
    let lift_uri = |p: &Player| format!("/core/v1/admin/voice-bans/{}", p.account_id);

    // A player: AUTH.ROLE_REQUIRED {role: moderator}, for both routes.
    let r = st
        .post(
            uri,
            Who::Launcher(Some(&player.access)),
            json!({"accountId": admin.account_id, "minutes": 5, "reason": "x"}),
        )
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    assert_eq!(r.body["details"]["role"], "moderator");
    let r = st
        .call(
            Method::DELETE,
            &lift_uri(&admin),
            Who::Launcher(Some(&player.access)),
        )
        .key()
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    // 400 REQUEST.INVALID {field} for every bad body.
    for (body, field) in [
        (json!({"minutes": 5, "reason": "x"}), "xuid"),
        (
            json!({"xuid": player.xuid, "accountId": player.account_id, "minutes": 5, "reason": "x"}),
            "accountId",
        ),
        (json!({"xuid": "12a", "minutes": 5, "reason": "x"}), "xuid"),
        (
            json!({"accountId": "nope", "minutes": 5, "reason": "x"}),
            "accountId",
        ),
        (
            json!({"xuid": player.xuid, "minutes": 0, "reason": "x"}),
            "minutes",
        ),
        (
            json!({"xuid": player.xuid, "minutes": 43_201, "reason": "x"}),
            "minutes",
        ),
        (
            json!({"xuid": player.xuid, "minutes": 5, "reason": " "}),
            "reason",
        ),
        (
            json!({"xuid": player.xuid, "minutes": 5, "reason": "x", "extra": 1}),
            "extra",
        ),
    ] {
        let r = st.post(uri, m, body.clone()).send().await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], field, "{body}");
    }
    let r = st
        .call(Method::DELETE, "/core/v1/admin/voice-bans/nope", m)
        .key()
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(r.body["details"]["field"], "accountId");
    // 404 PORTAL.NOT_FOUND: an unknown XUID or account.
    for body in [
        json!({"xuid": "2535999999999999", "minutes": 5, "reason": "x"}),
        json!({"accountId": Uuid::now_v7(), "minutes": 5, "reason": "x"}),
    ] {
        let r = st.post(uri, m, body).send().await;
        assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    }
    let r = st
        .call(
            Method::DELETE,
            &format!("/core/v1/admin/voice-bans/{}", Uuid::now_v7()),
            m,
        )
        .key()
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    // 403 AUTH.FORBIDDEN: oneself (by account or own XUID), a MOD acting on staff.
    for body in [
        json!({"accountId": moderator.account_id, "minutes": 5, "reason": "x"}),
        json!({"xuid": moderator.xuid, "minutes": 5, "reason": "x"}),
        json!({"accountId": other_mod.account_id, "minutes": 5, "reason": "x"}),
        json!({"xuid": admin.xuid, "minutes": 5, "reason": "x"}),
    ] {
        let r = st.post(uri, m, body.clone()).send().await;
        assert_code(&r, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    }
    for target in [&moderator, &other_mod] {
        let r = st
            .call(Method::DELETE, &lift_uri(target), m)
            .key()
            .send()
            .await;
        assert_code(&r, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    }
    assert_eq!(
        st.audit_rows(&moderator.account_id).await,
        0,
        "refusals write nothing"
    );

    // A MOD bans a player by XUID for an hour: 200 with the end; a replay answers the same.
    let key = Uuid::now_v7().to_string();
    let body = json!({"xuid": player.xuid, "minutes": 60, "reason": "synthetic shouting"});
    let banned = st
        .call(Method::POST, uri, m)
        .header("idempotency-key", &key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(banned.status, StatusCode::OK, "{}", banned.body);
    assert_eq!(banned.body["accountId"], player.account_id.as_str());
    let replay = st
        .call(Method::POST, uri, m)
        .header("idempotency-key", &key)
        .json(body)
        .send()
        .await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::OK, &banned.body)
    );
    let now = now_secs(&st).await;
    let until = blocks(&st, &player).await.1.unwrap();
    assert!((until - now - 3_600.0).abs() < 60.0, "{until} {now}");
    // Its audit row, its staff row (with the moderator), the player's notice without the reason.
    let audit: (String, Option<String>, Value) = sqlx::query_as(
        "SELECT action, reason, detail FROM admin_action WHERE actor_account_id = $1::uuid",
    )
    .bind(&moderator.account_id)
    .fetch_one(&st.owner)
    .await
    .unwrap();
    assert_eq!(
        audit,
        (
            "account.voice_ban".into(),
            Some("synthetic shouting".into()),
            json!({"minutes": 60})
        )
    );
    assert_eq!(
        staff_rows(&st, &player, "voice_banned").await,
        [(Some(Uuid::parse_str(&moderator.account_id).unwrap()), true)]
    );
    let notice = voice_notice(&st, &player)
        .await
        .expect("a voice_blocked notice");
    assert_eq!(notice["level"], "warning");
    assert!(!notice["text"].as_str().unwrap().contains("shouting"));
    assert!(notice["until"].is_string());
    // A new ban replaces it (here shorter); an ADM bans a MOD by account.
    let shorter = st
        .post(
            uri,
            a,
            json!({"accountId": player.account_id, "minutes": 2, "reason": "x"}),
        )
        .send()
        .await;
    assert_eq!(shorter.status, StatusCode::OK, "{}", shorter.body);
    let until = blocks(&st, &player).await.1.unwrap();
    assert!((until - now_secs(&st).await - 120.0).abs() < 30.0);
    let on_mod = st
        .post(
            uri,
            a,
            json!({"accountId": moderator.account_id, "minutes": 43_200, "reason": "x"}),
        )
        .send()
        .await;
    assert_eq!(on_mod.status, StatusCode::OK, "{}", on_mod.body);
    // The moderation view shows it.
    let view = st
        .get(&format!("/core/v1/admin/accounts/{}", player.account_id), m)
        .send()
        .await;
    assert!(
        view.body["voice"]["bannedUntil"].is_string(),
        "{}",
        view.body
    );
    // The lift: 204, the notice gone, `account.voice_unban`, one staff row; again: 204, no row.
    let lifted = st
        .call(Method::DELETE, &lift_uri(&player), m)
        .key()
        .send()
        .await;
    assert_eq!(lifted.status, StatusCode::NO_CONTENT, "{}", lifted.body);
    assert_eq!(blocks(&st, &player).await.1, None);
    assert!(voice_notice(&st, &player).await.is_none());
    let again = st
        .call(Method::DELETE, &lift_uri(&player), m)
        .key()
        .send()
        .await;
    assert_eq!(again.status, StatusCode::NO_CONTENT, "{}", again.body);
    assert_eq!(staff_rows(&st, &player, "voice_unbanned").await.len(), 1);
    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT detail FROM admin_action WHERE actor_account_id = $1::uuid \
           AND action = 'account.voice_unban' ORDER BY at",
    )
    .bind(&moderator.account_id)
    .fetch_all(&st.owner)
    .await
    .unwrap();
    assert_eq!(
        details,
        [
            json!({"wasBanned": true, "wasCommunityMuted": false}),
            json!({"wasBanned": false, "wasCommunityMuted": false})
        ]
    );
}
