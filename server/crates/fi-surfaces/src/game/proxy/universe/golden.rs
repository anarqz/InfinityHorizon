//! Golden decode (opt-in) of the Beta's captured Scoreboard requests (`server/captures/*.jsonl`):
//! every request of a method [`super::boards`] and [`super::ghosts`] serve decodes with the schema
//! and goes through the same parsing as in service (a submission, an XP report, a board query, a
//! recording); a reply of the shape the Beta's answered (built with synthetic rows, since boards
//! depend on what other players stored) encodes and decodes with the schema and has the Beta
//! handler's top-level fields; where a reply depends on nothing stored (XP, time trials, uploads)
//! it equals the Beta's. The Beta's own replies decode too.
//!
//! The captures hold real player identities: they stay outside the repository and the test prints
//! only counts and method names, never values.
//!
//! `FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-api universe::golden -- --ignored --nocapture`

use super::super::forza::{self, Reply};
use super::super::rpc::{self, class_key};
use super::{boards, ghosts};
use fi_auth::b64;
use fi_db::gameplane::{self, Card};
use fi_db::scores::{Assists, BoardFilter, BoardKey, BoardPage, ScoreRow};
use fi_domain::UnixMs;
use fi_gamedb::{Catalog, gates};
use fi_xtsw::{
    Envelope, Schema, decode_params, decode_results, encode_results, error_envelope, ok_envelope,
    parse_envelope,
};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Default)]
struct Tally {
    replayed: usize,
    decoded: usize,
    parsed: usize,
    encoded: usize,
    same_fields: usize,
    compared: usize,
    equal: usize,
    beta_decoded: usize,
    beta_not_ok: usize,
    failures: BTreeMap<String, usize>,
}

fn keys(v: &Json) -> Vec<String> {
    let mut k: Vec<String> = v
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    k.sort();
    k
}

fn synthetic_row(position: i64) -> ScoreRow {
    ScoreRow {
        position,
        profile_id: Uuid::nil(),
        player: gameplane::Player {
            xuid: 2_535_100_000_000_001,
            name: Some("Tester".into()),
            card: Card::default(),
            tables_tran_id: None,
        },
        key: BoardKey {
            scoreboard_type: 1,
            score_type: 1,
            game_scoreboard_id: 70,
            track_id: 0,
        },
        score: 95.5,
        submitted_at: UnixMs::from_millis(1_792_000_000_000),
        car_id: 247,
        car_class_id: 0,
        car_pi: 0.33,
        assists: Assists::default(),
        score_id: Uuid::nil(),
        ghost_file_id: Some(1),
    }
}

#[test]
#[ignore = "reads the Beta's captures from FI_FH2_CAPTURES_DIR (outside the repository)"]
fn captured_scoreboard_requests_decode_and_get_replies_shaped_like_the_betas() {
    let dir = std::env::var("FI_FH2_CAPTURES_DIR")
        .expect("set FI_FH2_CAPTURES_DIR to a directory of Beta capture .jsonl files");
    let schema = Schema::fh2();
    let catalog = Catalog::fh2();
    let caller = rpc::player_of(synthetic_row(1).player);
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read FI_FH2_CAPTURES_DIR")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut per: BTreeMap<String, Tally> = BTreeMap::new();
    // What the parsing found, by kind (counts only).
    let mut shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut shape = |k: String| *shapes.entry(k).or_default() += 1;
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read a capture file");
        for line in text.lines() {
            let Ok(r) = serde_json::from_str::<Json>(line) else {
                continue;
            };
            let (Some(class), Some(method)) =
                (r["forza_class"].as_str(), r["forza_method"].as_str())
            else {
                continue;
            };
            let key = class_key(class);
            let short = key.rsplit('.').next().unwrap_or(key);
            let xls = key.starts_with("Xls.");
            if boards::served(xls, short, method).is_none()
                && ghosts::served(xls, short, method).is_none()
            {
                continue;
            }
            let t = per.entry(format!("{key}.{method}")).or_default();
            t.replayed += 1;
            let fail = |t: &mut Tally, why: String| *t.failures.entry(why).or_default() += 1;
            let body =
                b64::std_decode(r["request_body_b64"].as_str().unwrap_or("")).unwrap_or_default();
            if method == "UploadFile" && body.len() > ghosts::UPLOAD_BODY {
                fail(t, "body over the UploadFile cap".into());
                continue;
            }
            let params = match decode_params(schema, key, method, &body) {
                Ok(v) => v.into_json(),
                Err(e) => {
                    fail(t, format!("request does not decode: {e}"));
                    continue;
                }
            };
            t.decoded += 1;
            let beta = &r["response_xtsw"];
            let beta_error = beta.get("xtsw_error").is_some();
            // The parsing of the handler, then the reply the Beta's answer corresponds to.
            let reply: Reply = match method {
                "SubmitScore" => match boards::submission(&params) {
                    Some(s) => {
                        // Real play passes the hardcore score gate, every type.
                        let gate = boards::gate(&s);
                        if gate.is_err() {
                            fail(t, "a captured score fails the score gate".into());
                            continue;
                        }
                        shape(format!(
                            "SubmitScore type {} passes the score gate",
                            s.key.score_type
                        ));
                        let fact = boards::score_fact(&s, true);
                        shape(format!(
                            "SubmitScore board {} as {}",
                            fact["board"].as_str().unwrap_or(""),
                            fact["scoreType"].as_str().unwrap_or("")
                        ));
                        Ok(boards::submitted(Uuid::now_v7(), true))
                    }
                    None => Err(boards::invalid_score()),
                },
                "SubmitXpScore" => {
                    let xp = gates::xp_of_report(params["xp"].as_f64().unwrap_or(0.0));
                    shape(format!(
                        "SubmitXpScore within one report: {}",
                        xp < gates::XP_PER_REPORT
                    ));
                    Ok(boards::xp_reply())
                }
                "GetBestScore" => Ok(boards::best_reply(
                    (beta["scoreFound"] == true).then_some((95.5, true)),
                )),
                "GetRows" | "GetRowsAnthem" => {
                    let circle = boards::circle_of(&params, &[], &[]);
                    let q = boards::board_query(&params, circle.as_deref());
                    shape(format!(
                        "{method} {} {} near_me={} circle={}",
                        if matches!(q.filter, BoardFilter::Route(_)) {
                            "route"
                        } else {
                            "event"
                        },
                        if boards::is_club_xp_board(&params) {
                            "club-xp"
                        } else {
                            "scores"
                        },
                        q.near_me,
                        q.circle.is_some()
                    ));
                    let page = BoardPage {
                        rows: vec![synthetic_row(1)],
                        me: Some(synthetic_row(1)),
                        available: 1,
                    };
                    Ok(boards::rows_reply(
                        &page,
                        Some(&caller),
                        method == "GetRowsAnthem",
                    ))
                }
                "GetTimeTrials" => Ok(boards::time_trials(forza::Lang::En, &params, |_| true)),
                "UploadFile" => match ghosts::recording(&params) {
                    Ok(_) => {
                        shape("UploadFile recording within the limit".into());
                        Ok(json!({}))
                    }
                    Err(x) => Err(x),
                },
                "DownloadFile" if beta_error => Err(ghosts::no_such_ghost()),
                "DownloadFile" => Ok(ghosts::downloaded(&[1, 2, 3])),
                "GetRival" => {
                    let cars = ghosts::cars_of_class(catalog, boards::int(&params, "pivotId"));
                    shape(format!(
                        "GetRival class filter={} explicit rival={}",
                        cars.is_some(),
                        params["rivalXuid"].as_u64().is_some_and(|x| x != 0)
                    ));
                    Ok(ghosts::rival_reply(catalog, &synthetic_row(3), Some(1)))
                }
                _ => unreachable!("every served Scoreboard method has a replay"),
            };
            t.parsed += 1;
            let ours = match &reply {
                Ok(results) => match encode_results(schema, key, method, results) {
                    Ok(bytes) => ok_envelope(&bytes),
                    Err(e) => {
                        fail(t, format!("reply does not encode: {e}"));
                        continue;
                    }
                },
                Err(x) => error_envelope(x.code, x.message),
            };
            let decoded = |bytes: &[u8]| match parse_envelope(bytes) {
                Ok(Envelope::Ok { results }) => decode_results(schema, key, method, results)
                    .ok()
                    .map(|v| v.to_json()),
                Ok(Envelope::Error { code, .. }) => Some(json!({ "xtsw_error": code })),
                Err(_) => None,
            };
            let Some(ours_dec) = decoded(&ours) else {
                fail(t, "reply does not decode".into());
                continue;
            };
            t.encoded += 1;
            let beta_bytes =
                b64::std_decode(r["response_body_b64"].as_str().unwrap_or("")).unwrap_or_default();
            let Some(beta_dec) = decoded(&beta_bytes) else {
                // The Beta's 503 (`_default.json`): no reply to compare.
                t.beta_not_ok += 1;
                continue;
            };
            t.beta_decoded += 1;
            let our_fields = match &reply {
                Ok(results) => keys(results),
                Err(_) => vec!["xtsw_error".to_owned()],
            };
            // GetTimeTrials: the Beta's handler sent `{}` and the codec filled the empty list.
            let same = if method == "GetTimeTrials" {
                beta_dec == ours_dec
            } else {
                keys(beta) == our_fields
            };
            if same {
                t.same_fields += 1;
            } else {
                fail(t, "top-level fields differ from the Beta's".into());
            }
            if matches!(method, "SubmitXpScore" | "GetTimeTrials" | "UploadFile") {
                t.compared += 1;
                if beta_dec == ours_dec {
                    t.equal += 1;
                } else {
                    fail(
                        t,
                        "reply differs from the Beta's where it must agree".into(),
                    );
                }
            }
        }
    }
    println!(
        "{:<52} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "method",
        "replayed",
        "decoded",
        "parsed",
        "encoded",
        "fields",
        "compared",
        "equal",
        "beta-dec",
        "beta!ok"
    );
    let mut total = Tally::default();
    for (name, t) in &per {
        println!(
            "{name:<52} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            t.replayed,
            t.decoded,
            t.parsed,
            t.encoded,
            t.same_fields,
            t.compared,
            t.equal,
            t.beta_decoded,
            t.beta_not_ok
        );
        for (why, n) in &t.failures {
            println!("    {n:>6} x {why}");
        }
        total.replayed += t.replayed;
        total.decoded += t.decoded;
        total.encoded += t.encoded;
        for (why, n) in &t.failures {
            *total.failures.entry(format!("{name}: {why}")).or_default() += n;
        }
    }
    for (what, n) in &shapes {
        println!("  {n:>6} x {what}");
    }
    println!(
        "{} files, {} replayed, {} decoded, {} encoded",
        files.len(),
        total.replayed,
        total.decoded,
        total.encoded
    );
    assert!(total.replayed > 0, "no Scoreboard request in the captures");
    assert!(
        total.failures.is_empty(),
        "{} failures: {:?}",
        total.failures.values().sum::<usize>(),
        total.failures
    );
    // The allowlist agrees: each of these is a Supporter's stored method.
    for name in per.keys() {
        let method = name.rsplit('.').next().unwrap_or("");
        assert_eq!(
            forza::served(
                super::super::Features::Supporter,
                false,
                "Scoreboard",
                method
            ),
            Some(forza::Served::Store)
        );
    }
}
