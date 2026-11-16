//! Golden replay (opt-in): every Forza RPC the Beta recorded (`server/captures/*.jsonl`) whose
//! method this proxy serves goes through the same steps as in service (schema decode of the
//! captured body, the reply builder of [`super::forza`] with a synthetic player, encode, envelope)
//! and is compared with the Beta's recorded reply: the reply decodes with the schema, the builder's
//! top-level fields equal the Beta handler's, and, where the reply must agree (see [`comparable`]),
//! the decoded reply equals the Beta's (`compared` / `equal` columns). A second replay runs the
//! captured Xbox Live and free-roam requests through the foreign-XUID rule
//! ([`captured_requests_under_the_foreign_xuid_rule`]).
//!
//! The captures hold real player identities: they stay outside the repository and the test prints
//! only counts and method names, never values.
//!
//! `FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-api golden -- --ignored --nocapture`

use super::Features;
use super::foreign::Named;
use super::forza::{self, Lang, Player, Served};
use super::rpc::class_key;
use super::target::percent_decode;
use axum::http::Method;
use fi_auth::b64;
use fi_db::gameplane::Card;
use fi_xtsw::{
    Envelope, Schema, decode_params, decode_results, encode_results, error_envelope, ok_envelope,
    parse_envelope,
};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

#[derive(Default)]
struct Tally {
    replayed: usize,
    decoded: usize,
    encoded: usize,
    same_fields: usize,
    deterministic: usize,
    byte_equal: usize,
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

/// What of a reply must equal the Beta's: everything for methods whose reply depends on nothing
/// but the method; for the friend- and inbox-dependent ones, everything when the Beta's list was
/// empty too; for Logon, everything but the player (`userData`), the tables checkpoint and the
/// category texts. `None`: nothing beyond the top-level fields (GetUserData: the player).
fn comparable(xls: bool, short: &str, method: &str, beta: &Json) -> Option<Json> {
    let empty = |k: &str| beta[k].as_array().is_some_and(Vec::is_empty);
    match (xls, short, method) {
        (true, "User", "GetUserData") => None,
        (true, "Notifications", "RetrieveForUser") => empty("results").then(|| beta.clone()),
        (false, "User", "GetCompareStatsForRelatedUsers") => {
            empty("compareStats").then(|| beta.clone())
        }
        (false, "User", "GetCompareStatsByBucket") => {
            empty("bucketedCompareStats").then(|| beta.clone())
        }
        (false, "User", "Logon") => Some(logon_shape(beta)),
        _ => Some(beta.clone()),
    }
}

/// A Logon reply without the player, the checkpoint and the category texts.
fn logon_shape(reply: &Json) -> Json {
    let mut r = reply.clone();
    if let Some(d) = r["responseData"].as_object_mut() {
        d.remove("userData");
        d.remove("lastOnlineProfileUpdateAllTablesTranId");
        if let Some(c) = d
            .get_mut("notificationCategories")
            .and_then(Json::as_array_mut)
        {
            for cat in c {
                if let Some(o) = cat.as_object_mut() {
                    o.remove("description");
                }
            }
        }
    }
    r
}

#[test]
#[ignore = "reads the Beta's captures from FI_FH2_CAPTURES_DIR (outside the repository)"]
fn captured_requests_get_replies_shaped_like_the_betas() {
    let dir = std::env::var("FI_FH2_CAPTURES_DIR")
        .expect("set FI_FH2_CAPTURES_DIR to a directory of Beta capture .jsonl files");
    let schema = Schema::fh2();
    let player = Player {
        xuid: 2_535_100_000_000_001,
        gamertag: forza::game_tag("Tester"),
        card: Card::default(),
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read FI_FH2_CAPTURES_DIR")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut per: BTreeMap<String, Tally> = BTreeMap::new();
    let mut records = 0usize;
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read a capture file");
        for line in text.lines() {
            let Ok(r) = serde_json::from_str::<Json>(line) else {
                continue;
            };
            records += 1;
            let (Some(class), Some(method)) =
                (r["forza_class"].as_str(), r["forza_method"].as_str())
            else {
                continue;
            };
            let key = class_key(class);
            let short = key.rsplit('.').next().unwrap_or(key);
            let xls = key.starts_with("Xls.");
            let Some(served) = forza::served(Features::Supporter, xls, short, method) else {
                continue;
            };
            // The universe's replies depend on what other players stored: replayed by its own
            // golden decodes (`universe::boards`, `universe::ghosts`, `universe::drivatars`).
            if super::universe::served(xls, short, method).is_some() {
                continue;
            }
            let t = per.entry(format!("{key}.{method}")).or_default();
            t.replayed += 1;
            let fail = |t: &mut Tally, why: String| *t.failures.entry(why).or_default() += 1;
            let body =
                b64::std_decode(r["request_body_b64"].as_str().unwrap_or("")).unwrap_or_default();
            let params = match decode_params(schema, key, method, &body) {
                Ok(v) => v.into_json(),
                Err(e) => {
                    fail(t, format!("request does not decode: {e}"));
                    continue;
                }
            };
            t.decoded += 1;
            let reply: forza::Reply = match served {
                Served::Pure => Ok(forza::pure(xls, short, method, &params).expect("pure reply")),
                Served::Store => match (xls, short, method) {
                    (false, "User", "Logon") => Ok(forza::logon(
                        &player,
                        &Default::default(),
                        None,
                        &params,
                        Lang::En,
                    )),
                    (true, "User", "GetUserData") => {
                        Ok(forza::get_user_data(&player, &Default::default()))
                    }
                    (false, "OnlineProfile", "UpdateAll") => {
                        let fact = forza::profile_sync_fact(&params);
                        assert_eq!(fact["accepted"], true);
                        Ok(forza::updated())
                    }
                    (false, "OnlineProfile", "UpdateAllTables") => {
                        let tables = forza::table_updates(params.clone());
                        assert!(forza::tables_sync_fact(&tables)["rows"].is_u64());
                        Ok(forza::updated())
                    }
                    (false, "User", "SetPlayerCardInfo") => Ok(json!({})),
                    // No transaction kept here: the Beta's one capture also did not know it.
                    (true, "Transaction", "Reconcile") => forza::reconcile(None),
                    _ => unreachable!("every stored method has a replay"),
                },
            };
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
            match parse_envelope(&ours) {
                Ok(Envelope::Ok { results }) => {
                    if let Err(e) = decode_results(schema, key, method, results) {
                        fail(t, format!("reply does not decode: {e}"));
                        continue;
                    }
                }
                Ok(Envelope::Error { .. }) => {}
                Err(e) => {
                    fail(t, format!("envelope does not parse: {e}"));
                    continue;
                }
            }
            t.encoded += 1;
            let beta =
                b64::std_decode(r["response_body_b64"].as_str().unwrap_or("")).unwrap_or_default();
            if !matches!(parse_envelope(&beta), Ok(Envelope::Ok { .. }))
                && !matches!(parse_envelope(&beta), Ok(Envelope::Error { .. }))
            {
                t.beta_not_ok += 1;
                continue;
            }
            // The Beta's handler JSON (`response_xtsw`; `{"xtsw_error": ...}` for an Xls error).
            let beta_json = &r["response_xtsw"];
            let our_fields = match &reply {
                Ok(results) => keys(results),
                Err(_) => vec!["xtsw_error".to_owned()],
            };
            if keys(beta_json) == our_fields {
                t.same_fields += 1;
            } else {
                fail(t, "top-level fields differ from the Beta's".to_owned());
            }
            // Both replies decoded with the schema, then compared where they must agree.
            let decoded = |bytes: &[u8]| match parse_envelope(bytes) {
                Ok(Envelope::Ok { results }) => decode_results(schema, key, method, results)
                    .ok()
                    .map(|v| v.to_json()),
                Ok(Envelope::Error { code, .. }) => Some(json!({ "xtsw_error": code })),
                Err(_) => None,
            };
            let (Some(beta_dec), Some(ours_dec)) = (decoded(&beta), decoded(&ours)) else {
                fail(t, "a reply does not decode for the comparison".to_owned());
                continue;
            };
            if let Some(expected) = comparable(xls, short, method, &beta_dec) {
                t.deterministic += 1;
                let ours_cmp = if (xls, short, method) == (false, "User", "Logon") {
                    logon_shape(&ours_dec)
                } else {
                    ours_dec
                };
                if ours_cmp == expected {
                    t.byte_equal += 1;
                } else {
                    fail(
                        t,
                        "reply differs from the Beta's where it must agree".to_owned(),
                    );
                }
            }
        }
    }
    println!(
        "{:<58} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "method", "replayed", "decoded", "encoded", "fields", "compared", "equal", "beta!ok"
    );
    let mut total = Tally::default();
    for (name, t) in &per {
        println!(
            "{name:<58} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            t.replayed,
            t.decoded,
            t.encoded,
            t.same_fields,
            t.deterministic,
            t.byte_equal,
            t.beta_not_ok
        );
        for (why, n) in &t.failures {
            println!("    {n:>6} x {why}");
        }
        total.replayed += t.replayed;
        total.decoded += t.decoded;
        total.encoded += t.encoded;
        total.same_fields += t.same_fields;
        total.deterministic += t.deterministic;
        total.byte_equal += t.byte_equal;
        total.beta_not_ok += t.beta_not_ok;
        for (why, n) in &t.failures {
            *total.failures.entry(format!("{name}: {why}")).or_default() += n;
        }
    }
    println!(
        "{:<58} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "TOTAL",
        total.replayed,
        total.decoded,
        total.encoded,
        total.same_fields,
        total.deterministic,
        total.byte_equal,
        total.beta_not_ok
    );
    println!("{} files, {records} records", files.len());
    assert!(total.replayed > 0, "no served method in the captures");
    assert!(
        total.failures.is_empty(),
        "{} failures: {:?}",
        total.failures.values().sum::<usize>(),
        total.failures
    );
}

/// Per host: what the foreign-XUID rule ([`super::foreign`]) would do to the captured requests.
#[derive(Default)]
struct XuidTally {
    requests: usize,
    naming: usize,
    free_any: usize,
    free_main: usize,
    supporter_any: usize,
    supporter_main: usize,
    no_owner: usize,
    solo_naming: usize,
    solo_free: usize,
    solo_supporter: usize,
}

/// One captured request to an Xbox Live or free-roam host, as the rule sees it.
struct Captured {
    method: Method,
    host: String,
    path: String,
    query: Option<String>,
    body: Vec<u8>,
}

impl Captured {
    fn named(&self) -> Named {
        Named::of(
            &self.method,
            &self.host,
            &self.path,
            self.query.as_deref(),
            &self.body,
        )
    }

    /// The XUIDs it names about its own player (XSAPI's self calls): the presence write's
    /// `xuid(N)`, the MPSD `?xuid=N` query of the caller's sessions and an MPSD `me` member's
    /// `constants.system.xuid`.
    fn self_signals(&self) -> Vec<u64> {
        match self.host.as_str() {
            "userpresence.xboxlive.com"
                if self.method == Method::POST
                    && self.path.ends_with("/devices/current/titles/current") =>
            {
                self.named().addressed
            }
            "sessiondirectory.xboxlive.com" => {
                let mut out = if self.method == Method::GET {
                    self.named().addressed
                } else {
                    Vec::new()
                };
                let body: Json = serde_json::from_slice(&self.body).unwrap_or(Json::Null);
                match &body["members"]["me"]["constants"]["system"]["xuid"] {
                    Json::String(s) => out.extend(s.parse::<u64>().ok()),
                    other => out.extend(other.as_u64()),
                }
                out
            }
            _ => Vec::new(),
        }
    }
}

/// The foreign-XUID rule over the Beta's captured Xbox Live and free-roam requests (opt-in, the
/// same `FI_FH2_CAPTURES_DIR`). A capture does not say which player sent a request (the Beta's
/// token is per account, and one server run holds several players), so a file's players are the
/// XUIDs of its self calls ([`Captured::self_signals`]), and a request counts as refused "any"
/// when it is refused whichever of them sent it, "main" when it is refused for the file's most
/// frequent one; in a file of one player ("solo") the owner is known and the counts are exact. The
/// Supporter path must refuse no captured request under any owner, nor any of a solo file; the
/// free counts are reported. Only counts and host names are printed, never a XUID.
///
/// `FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-api golden -- --ignored --nocapture`
#[test]
#[ignore = "reads the Beta's captures from FI_FH2_CAPTURES_DIR (outside the repository)"]
fn captured_requests_under_the_foreign_xuid_rule() {
    let dir = std::env::var("FI_FH2_CAPTURES_DIR")
        .expect("set FI_FH2_CAPTURES_DIR to a directory of Beta capture .jsonl files");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read FI_FH2_CAPTURES_DIR")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut per: BTreeMap<String, XuidTally> = BTreeMap::new();
    let mut files_by_players: BTreeMap<usize, usize> = BTreeMap::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read a capture file");
        let mut recs = Vec::new();
        for line in text.lines() {
            let Ok(r) = serde_json::from_str::<Json>(line) else {
                continue;
            };
            if r["forza_class"].is_string() {
                continue;
            }
            let host = r["host"].as_str().unwrap_or("").to_ascii_lowercase();
            // Only what reaches the rule: a host the deployments answer (others are 404 first).
            if super::body_limit(Features::Free, &host, None, None).is_err() {
                continue;
            }
            let raw = r["path"].as_str().unwrap_or("/");
            recs.push(Captured {
                method: Method::from_bytes(r["method"].as_str().unwrap_or("GET").as_bytes())
                    .unwrap_or(Method::GET),
                host,
                path: String::from_utf8(percent_decode(raw)).unwrap_or_else(|_| raw.to_owned()),
                query: r["query"]
                    .as_str()
                    .filter(|q| !q.is_empty())
                    .map(str::to_owned),
                body: b64::std_decode(r["request_body_b64"].as_str().unwrap_or(""))
                    .unwrap_or_default(),
            });
        }
        let mut players: BTreeMap<u64, usize> = BTreeMap::new();
        for x in recs.iter().flat_map(Captured::self_signals) {
            *players.entry(x).or_default() += 1;
        }
        *files_by_players.entry(players.len()).or_default() += 1;
        let main = players
            .iter()
            .max_by_key(|(x, n)| (**n, std::cmp::Reverse(**x)))
            .map(|(x, _)| *x);
        for r in &recs {
            let t = per.entry(r.host.clone()).or_default();
            t.requests += 1;
            let named = r.named();
            if named == Named::default() {
                continue;
            }
            t.naming += 1;
            let Some(main) = main else {
                t.no_owner += 1;
                continue;
            };
            let any = |free: bool| {
                players
                    .keys()
                    .all(|o| named.refused(Some(*o), free, &r.method))
            };
            t.free_any += usize::from(any(true));
            t.free_main += usize::from(named.refused(Some(main), true, &r.method));
            t.supporter_any += usize::from(any(false));
            t.supporter_main += usize::from(named.refused(Some(main), false, &r.method));
            if players.len() == 1 {
                t.solo_naming += 1;
                t.solo_free += usize::from(named.refused(Some(main), true, &r.method));
                t.solo_supporter += usize::from(named.refused(Some(main), false, &r.method));
            }
        }
    }
    let row = |name: &str, t: &XuidTally| {
        println!(
            "{name:<32} {:>8} {:>8} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            t.requests,
            t.naming,
            t.free_any,
            t.free_main,
            t.supporter_any,
            t.supporter_main,
            t.no_owner,
            t.solo_naming,
            t.solo_free,
            t.solo_supporter
        );
    };
    println!(
        "{:<32} {:>8} {:>8} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "host",
        "requests",
        "naming",
        "free:any",
        "free:main",
        "sup:any",
        "sup:main",
        "no-owner",
        "solo",
        "solo:fr",
        "solo:sup"
    );
    let mut total = XuidTally::default();
    for (host, t) in &per {
        row(host, t);
        total.requests += t.requests;
        total.naming += t.naming;
        total.free_any += t.free_any;
        total.free_main += t.free_main;
        total.supporter_any += t.supporter_any;
        total.supporter_main += t.supporter_main;
        total.no_owner += t.no_owner;
        total.solo_naming += t.solo_naming;
        total.solo_free += t.solo_free;
        total.solo_supporter += t.solo_supporter;
    }
    row("TOTAL", &total);
    println!(
        "{} files; files by number of players seen: {files_by_players:?}",
        files.len()
    );
    assert!(
        total.requests > 0,
        "no Xbox Live or free-roam request in the captures"
    );
    assert_eq!(
        (total.supporter_any, total.solo_supporter),
        (0, 0),
        "the Supporter path refuses a captured request"
    );
}
