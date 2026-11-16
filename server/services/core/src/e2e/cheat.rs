//! Lifting a cheat flag (CORE_API.md §5 `DELETE /core/v1/admin/cheat-flags/{profileId}`,
//! MATCHMAKING_API.md §3.9 "Lifting a flag") over fi-core's whole router: every status and code
//! (401 for an invalid token, 403 `AUTH.ROLE_REQUIRED`, 400 `REQUEST.INVALID {field}` per field, 404 `PORTAL.NOT_FOUND`,
//! 403 `AUTH.FORBIDDEN` for one's own profile and a moderator acting on staff, 409 for a reused
//! key), the 204 that ends the flag now and marks it lifted, the replay and the idempotent repeat,
//! the `profile.cheat_unflag` audit rows and the moderation view's `cheat`. Run with the rest of
//! the e2e suites (see `super`'s header).

use super::*;

async fn roomy_stack() -> Stack {
    stack_with(Options {
        vars: vec![("FI_LOCAL_LIMIT_SCALE", "10".into())],
        ..Options::default()
    })
    .await
}

async fn profile_id(st: &Stack, p: &Player) -> Uuid {
    sqlx::query_scalar("SELECT id FROM profile WHERE xuid = $1::bigint")
        .bind(&p.xuid)
        .fetch_one(&st.owner)
        .await
        .unwrap()
}

/// A live 2-hour flag, as a quorum would have set it (episode `episode`).
async fn flag(st: &Stack, profile: Uuid, episode: i32) {
    sqlx::query(
        "INSERT INTO cheat_flag (profile_id, first_at, last_at, until, kinds, episode) \
         VALUES ($1, clock_timestamp() - interval '1 minute', clock_timestamp() - interval '1 minute', \
           clock_timestamp() + interval '2 hours', ARRAY['speed'], $2)",
    )
    .bind(profile)
    .bind(episode)
    .execute(&st.owner)
    .await
    .unwrap();
}

/// The flag's seconds left (negative once ended) and its `lifted` mark.
async fn flag_state(st: &Stack, profile: Uuid) -> Option<(f64, bool)> {
    sqlx::query_as(
        "SELECT extract(epoch FROM until - clock_timestamp())::float8, lifted FROM cheat_flag WHERE profile_id = $1",
    )
    .bind(profile)
    .fetch_optional(&st.owner)
    .await
    .unwrap()
}

fn uri(profile: impl std::fmt::Display) -> String {
    format!("/core/v1/admin/cheat-flags/{profile}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Postgres and the auth emulator (see the file header)"]
async fn moderators_lift_a_cheat_flag() {
    let st = roomy_stack().await;
    let player = st.player("hardcore").await;
    let clean = st.player("hardcore").await;
    let moderator = st.player("hardcore").await;
    let other_mod = st.player("hardcore").await;
    let admin = st.player("hardcore").await;
    st.set_role(&moderator.account_id, "moderator").await;
    st.set_role(&other_mod.account_id, "moderator").await;
    st.set_role(&admin.account_id, "admin").await;
    let m = Who::Web(Some(&moderator.cookie));
    let a = Who::Launcher(Some(&admin.access));
    let flagged = profile_id(&st, &player).await;
    let mod_profile = profile_id(&st, &moderator).await;
    let other_mod_profile = profile_id(&st, &other_mod).await;
    let admin_profile = profile_id(&st, &admin).await;
    flag(&st, flagged, 2).await;
    flag(&st, other_mod_profile, 1).await;
    let body = json!({"reason": "synthetic false positive"});

    // An invalid bearer: 401 AUTH.TOKEN_INVALID (no credential at all is the CSRF guard's 403).
    let r = st
        .call(
            Method::DELETE,
            &uri(flagged),
            Who::Launcher(Some("not-a-token")),
        )
        .key()
        .json(body.clone())
        .send()
        .await;
    assert_code(&r, StatusCode::UNAUTHORIZED, "AUTH.TOKEN_INVALID");
    // A player: AUTH.ROLE_REQUIRED {role: moderator}.
    let r = st
        .call(
            Method::DELETE,
            &uri(flagged),
            Who::Launcher(Some(&player.access)),
        )
        .key()
        .json(body.clone())
        .send()
        .await;
    assert_code(&r, StatusCode::FORBIDDEN, "AUTH.ROLE_REQUIRED");
    assert_eq!(r.body["details"]["role"], "moderator");

    // 400 REQUEST.INVALID {field} for every bad request.
    for (path, body, field) in [
        (uri("nope"), json!({"reason": "x"}), "profileId"),
        (uri(flagged), json!({}), "reason"),
        (uri(flagged), json!({"reason": " "}), "reason"),
        (uri(flagged), json!({"reason": "x".repeat(501)}), "reason"),
        (uri(flagged), json!({"reason": "line\nbreak"}), "reason"),
        (uri(flagged), json!({"reason": "x", "extra": 1}), "extra"),
    ] {
        let r = st
            .call(Method::DELETE, &path, m)
            .key()
            .json(body.clone())
            .send()
            .await;
        assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
        assert_eq!(r.body["details"]["field"], field, "{path} {body}");
    }
    // Without an Idempotency-Key: 400 {field: Idempotency-Key}.
    let r = st
        .call(Method::DELETE, &uri(flagged), m)
        .json(body.clone())
        .send()
        .await;
    assert_code(&r, StatusCode::BAD_REQUEST, "REQUEST.INVALID");
    assert_eq!(r.body["details"]["field"], "Idempotency-Key");
    // 404 PORTAL.NOT_FOUND: an unknown profile.
    let r = st
        .call(Method::DELETE, &uri(Uuid::now_v7()), m)
        .key()
        .json(body.clone())
        .send()
        .await;
    assert_code(&r, StatusCode::NOT_FOUND, "PORTAL.NOT_FOUND");
    // 403 AUTH.FORBIDDEN: one's own profile, a MOD acting on a MOD's or an ADM's profile.
    for target in [mod_profile, other_mod_profile, admin_profile] {
        let r = st
            .call(Method::DELETE, &uri(target), m)
            .key()
            .json(body.clone())
            .send()
            .await;
        assert_code(&r, StatusCode::FORBIDDEN, "AUTH.FORBIDDEN");
    }
    assert_eq!(
        st.audit_rows(&moderator.account_id).await,
        0,
        "refusals write nothing"
    );
    let (left, lifted) = flag_state(&st, flagged).await.unwrap();
    assert!(left > 7_000.0 && !lifted, "refusals lift nothing");

    // The moderation view shows the live flag.
    let view = st
        .call(
            Method::GET,
            &format!("/core/v1/admin/accounts/{}", player.account_id),
            m,
        )
        .send()
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    let cheat = view.body["cheat"].as_array().unwrap();
    assert_eq!(cheat.len(), 1);
    assert_eq!(cheat[0]["profileId"], flagged.to_string());
    assert_eq!(cheat[0]["episode"], 2);
    assert_eq!(cheat[0]["kinds"], json!(["speed"]));
    assert_eq!(cheat[0]["inForce"], true);
    assert_eq!(cheat[0]["lifted"], false);
    assert!(cheat[0]["until"].is_string() && cheat[0]["firstAt"].is_string());

    // A MOD lifts the player's flag: 204; it ended now and is marked lifted; a replay answers the
    // same and writes nothing; the same key on another profile is 409.
    let key = Uuid::now_v7().to_string();
    let lift = st
        .call(Method::DELETE, &uri(flagged), m)
        .header("idempotency-key", &key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(lift.status, StatusCode::NO_CONTENT, "{}", lift.body);
    let (left, lifted) = flag_state(&st, flagged).await.unwrap();
    assert!((-60.0..=0.0).contains(&left) && lifted, "{left} {lifted}");
    let replay = st
        .call(Method::DELETE, &uri(flagged), m)
        .header("idempotency-key", &key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(replay.status, StatusCode::NO_CONTENT);
    let clean_profile = profile_id(&st, &clean).await;
    let reused = st
        .call(Method::DELETE, &uri(clean_profile), m)
        .header("idempotency-key", &key)
        .json(body.clone())
        .send()
        .await;
    assert_code(
        &reused,
        StatusCode::CONFLICT,
        "REQUEST.IDEMPOTENCY_KEY_REUSED",
    );
    // A new lift of the same profile (nothing in force) and of a never-flagged one: 204 both.
    for target in [flagged, clean_profile] {
        let r = st
            .call(Method::DELETE, &uri(target), m)
            .key()
            .json(body.clone())
            .send()
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    }
    assert_eq!(flag_state(&st, clean_profile).await, None);
    // One `profile.cheat_unflag` row per fresh write, with the reason; only the first lifted.
    type Row = (String, Option<String>, Option<String>, String, Value);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT action, reason, target, account_id::text, detail FROM admin_action \
         WHERE actor_account_id = $1::uuid ORDER BY at, id",
    )
    .bind(&moderator.account_id)
    .fetch_all(&st.owner)
    .await
    .unwrap();
    let target = format!("profile:{flagged}");
    assert_eq!(
        rows,
        [
            (
                "profile.cheat_unflag".into(),
                Some("synthetic false positive".into()),
                Some(target.clone()),
                player.account_id.clone(),
                json!({"wasFlagged": true, "episode": 2}),
            ),
            (
                "profile.cheat_unflag".into(),
                Some("synthetic false positive".into()),
                Some(target),
                player.account_id.clone(),
                json!({"wasFlagged": false, "episode": null}),
            ),
            (
                "profile.cheat_unflag".into(),
                Some("synthetic false positive".into()),
                Some(format!("profile:{clean_profile}")),
                clean.account_id.clone(),
                json!({"wasFlagged": false, "episode": null}),
            ),
        ]
    );
    // The view keeps the ended flag (it still sets the next one's length), lifted, not in force.
    let view = st
        .call(
            Method::GET,
            &format!("/core/v1/admin/accounts/{}", player.account_id),
            m,
        )
        .send()
        .await;
    let cheat = &view.body["cheat"][0];
    assert_eq!(
        (&cheat["inForce"], &cheat["lifted"], &cheat["episode"]),
        (&json!(false), &json!(true), &json!(2))
    );
    // An ADM lifts a MOD's flag.
    let r = st
        .call(Method::DELETE, &uri(other_mod_profile), a)
        .key()
        .json(body)
        .send()
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    assert!(flag_state(&st, other_mod_profile).await.unwrap().1);
}
