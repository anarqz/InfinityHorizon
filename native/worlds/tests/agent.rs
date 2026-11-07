// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! `worlds agent` end to end on loopback: a fake of MATCHMAKING_API §2.9's official routes (it
//! verifies every `FI-World-Agent` signature with the agent's public key) and the agent with its
//! real `worlds serve --profile public` child. hello → PUT → announce (a joiner's grant in its
//! punches) → admission → SIGTERM → DELETE → the engine shut down. Synthetic only: test keys,
//! synthetic XUIDs, loopback addresses.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ed25519_dalek::{Signer, SigningKey, Verifier};
use serde_json::{Value, json};
use worlds::{agent::sign, core::keys::encode_base64};

const VM: &str = "test-vm";
const AGENT_SEED: [u8; 32] = [5; 32];
const GRANT_SEED: [u8; 32] = [7; 32];
const KID: &str = "test-k1";
const XUID: &str = "1000000000000102";
const SUB: &str = "0b1c2d3e-4f50-4617-8829-3a4b5c6d7e81";
const GRANT: &str = "6d1f0c2e-0a3b-4c5d-8e6f-7a8b9c0d1e21";

fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A joiner's punch for an official world (MATCHMAKING_API §2.9: role `joiner`, host
/// `{official}`, no `relay` claim, `transport: "direct"`).
fn punch(world: &str) -> Value {
    let now = now_s();
    let claims = format!(
        r#"{{"v":1,"kid":"{KID}","grant":"{GRANT}","world":"{world}","sub":"{SUB}","xuid":"{XUID}","tier":"free","host":{{"official":"{VM}"}},"role":"joiner","iat":{},"exp":{}}}"#,
        now - 1,
        now + 59
    );
    let mut message = b"fi-relay-grant-v1\n".to_vec();
    message.extend_from_slice(claims.as_bytes());
    let sig = SigningKey::from_bytes(&GRANT_SEED).sign(&message);
    json!({"grantId": GRANT, "xuid": XUID, "kicked": false, "transport": "direct",
        "relay": {"claims": encode_base64(claims.as_bytes()), "signature": encode_base64(&sig.to_bytes()),
                  "udpProof": {"version": 1, "key": encode_base64(&[9u8; 32])}}})
}

type Log = Arc<Mutex<Vec<(String, String)>>>;

/// How the fake answers `POST …/game-sessions`: a status (201 with a `GameModeSession`, anything
/// else a refusal), or `HANG` (no answer for 16 s, past the agent's 14 s budget).
const HANG: u16 = 0;
/// The group world the fake server opens for a car meet session (WORLDS_OFFICIAL.md §11.4).
const GROUP_WORLD: &str = "01900000-0000-7000-8000-0000000000a1";

/// The fake server: answers until the test ends; records (method, target) of verified calls.
fn fake(listener: TcpListener, log: Log, sessions: u16) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let log = log.clone();
        std::thread::spawn(move || serve(stream, log, sessions));
    }
}

fn serve(stream: TcpStream, log: Log, sessions: u16) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut out = stream;
    let public = SigningKey::from_bytes(&AGENT_SEED).verifying_key();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (
            parts.next().unwrap_or("").to_string(),
            parts.next().unwrap_or("").to_string(),
        );
        let (mut length, mut auth, mut idem) = (0usize, None::<String>, None::<String>);
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            let (name, value) = h.split_once(':').unwrap();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap(),
                "fi-world-agent" => auth = Some(value.trim().to_string()),
                "idempotency-key" => idem = Some(value.trim().to_string()),
                _ => {}
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        let verified = auth.as_deref().is_some_and(|a| {
            let f: Vec<&str> = a.split(' ').collect();
            if f.len() != 4 || f[0] != "v1" || f[1] != VM {
                return false;
            }
            let Ok(secs) = f[2].parse::<u64>() else {
                return false;
            };
            if (secs as i64 - now_s()).abs() > 30 {
                return false;
            }
            let Some(raw) = worlds::core::keys::decode_base64(f[3]) else {
                return false;
            };
            let Ok(sig) = ed25519_dalek::Signature::from_slice(&raw) else {
                return false;
            };
            public
                .verify(&sign::message(&method, &target, secs, &body), &sig)
                .is_ok()
        });
        let (status, answer) = if !verified {
            (
                401,
                json!({"code": "MM.AGENT_UNAUTHORIZED", "message": "no"}),
            )
        } else {
            log.lock().unwrap().push((method.clone(), target.clone()));
            let world = target
                .strip_prefix("/mm/v1/official/worlds/")
                .map(|r| r.split('/').next().unwrap().to_string());
            match (method.as_str(), world) {
                ("GET", None) if target == "/mm/v1/official/hello" => (
                    200,
                    json!({"serverTimeMs": now_s() * 1000,
                        "grantVerifyKeys": [{"kid": KID, "publicKey": encode_base64(SigningKey::from_bytes(&GRANT_SEED).verifying_key().as_bytes())}],
                        "activities": ["freeroam", "carmeet", "roadtrip", "race"]}),
                ),
                ("PUT", Some(w)) => {
                    let settings: Value = serde_json::from_slice(&body).unwrap();
                    // OD-210, CR-S101 (W14): a mixed spare (`"any"`) per (game, activity) from an
                    // agent serving both modes; the car meet lobby keeps the first mode as before
                    // (its test serves Noob only)
                    if settings["activity"] == "carmeet" {
                        assert_eq!(settings["mode"], "noob", "{settings}");
                    } else {
                        assert_eq!(settings["mode"], "any", "{settings}");
                    }
                    if settings["activity"] == "race" {
                        // CR-S99 §5: an FM5 race world, up to 16 seats
                        assert_eq!(settings["game"], "fm5", "{settings}");
                        assert_eq!(settings["maxPlayers"], 16, "{settings}");
                    } else {
                        assert_eq!(settings["game"], "fh2");
                        assert!(
                            settings["activity"] == "freeroam" || settings["activity"] == "carmeet",
                            "{settings}"
                        );
                    }
                    (201, json!({"id": w, "punches": []}))
                }
                ("POST", Some(w)) if target.ends_with("/game-sessions") => {
                    assert!(idem.is_some(), "a game session carries Idempotency-Key");
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(request, json!({"kind": "freeroam", "xuids": [XUID]}));
                    match sessions {
                        HANG => {
                            std::thread::sleep(Duration::from_secs(16));
                            (503, json!({"code": "UNAVAILABLE", "message": "late"}))
                        }
                        201 => (
                            201,
                            json!({"kind": "freeroam", "scid": "788f0100-a4bc-46c3-b19b-d1001a96545b",
                                "templateName": "defaulttemplate",
                                "name": format!("fh2inf-invite-{}-{}", w.replace('-', ""), "0".repeat(32)),
                                "members": [XUID],
                                "world": {"worldId": GROUP_WORLD, "activity": "freeroam"}}),
                        ),
                        409 => (
                            409,
                            json!({"code": "PORTAL.CONFLICT", "message": "no", "details": {"reason": "max_worlds"}}),
                        ),
                        status => (
                            status,
                            json!({"code": "REQUEST.INVALID", "message": "no", "details": {"field": "xuids"}}),
                        ),
                    }
                }
                ("POST", Some(w)) if target.ends_with("/announce") => {
                    assert!(idem.is_some(), "an announce carries Idempotency-Key");
                    let beat: Value = serde_json::from_slice(&body).unwrap();
                    assert!(beat["peers"].is_array());
                    (200, json!({"id": w, "punches": [punch(&w)]}))
                }
                ("DELETE", Some(_)) => (204, Value::Null),
                _ => (404, json!({"code": "MM.WORLD_NOT_FOUND", "message": "no"})),
            }
        };
        let text = if answer.is_null() {
            String::new()
        } else {
            answer.to_string()
        };
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            text.len()
        );
        if out.write_all(head.as_bytes()).is_err() || out.write_all(text.as_bytes()).is_err() {
            return;
        }
    }
}

/// Waits for a stdout line of the agent matching `pick`.
fn wait_line(rx: &mpsc::Receiver<Value>, what: &str, pick: impl Fn(&Value) -> bool) -> Value {
    let until = Instant::now() + Duration::from_secs(40);
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(v) if pick(&v) => return v,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    panic!("no {what} line");
}

#[test]
fn hello_open_announce_admission_and_delete_end_to_end() {
    let dir = std::env::temp_dir().join(format!("fi-agent-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("agent.key");
    std::fs::write(&key, format!("{}\n", encode_base64(&AGENT_SEED))).unwrap();
    let control = dir.join("ctl").join("control.sock");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let server_log = log.clone();
    std::thread::spawn(move || fake(listener, server_log, 201));

    let mut child = Command::new(env!("CARGO_BIN_EXE_worlds"))
        .arg("agent")
        .env_clear()
        .env("FI_WORLDS_SERVER_URL", format!("http://127.0.0.1:{port}"))
        .env("FI_WORLDS_VM_ID", VM)
        .env("FI_WORLDS_KEY_FILE", &key)
        .env("FI_WORLDS_BIND", "127.0.0.1:0")
        .env("FI_WORLDS_CONTROL", format!("unix:{}", control.display()))
        .env("FI_WORLDS_ACTIVITIES", "freeroam")
        .env("FI_WORLDS_ANNOUNCE_SECONDS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let v: Value = serde_json::from_str(&line).expect("stdout carries JSON lines only");
            if tx.send(v).is_err() {
                return;
            }
        }
    });

    let started = wait_line(&rx, "agent.started", |v| v["ev"] == "agent.started");
    assert_eq!(
        started["publicKey"],
        encode_base64(
            SigningKey::from_bytes(&AGENT_SEED)
                .verifying_key()
                .as_bytes()
        )
    );
    assert!(
        !started.to_string().contains(&encode_base64(&AGENT_SEED)),
        "the seed is never logged"
    );
    let opened = wait_line(&rx, "agent.world.opened", |v| {
        v["ev"] == "agent.world.opened"
    });
    let world = opened["world"].as_str().unwrap().to_string();
    assert!(worlds::core::grant::uuid(&world));
    let admitted = wait_line(&rx, "admitted", |v| {
        v["ev"] == "agent.world.admission" && v["punches"] == 1
    });
    assert_eq!(admitted["admitted"], 1, "{admitted}");
    assert_eq!(admitted["world"], world.as_str());

    // a second session on the engine's control socket sees the one open world
    {
        let mut s = std::os::unix::net::UnixStream::connect(&control).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(b"{\"id\":1,\"op\":\"hello\",\"proto\":1}\n{\"id\":2,\"op\":\"capacity\"}\n")
            .unwrap();
        let r = BufReader::new(s);
        let capacity = r
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .find(|v| v["id"] == 2)
            .unwrap();
        assert_eq!(capacity["openWorlds"], 1, "{capacity}");
    }

    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < until, "the agent did not stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));
    let target = format!("/mm/v1/official/worlds/{world}");
    let calls = log.lock().unwrap().clone();
    let at = |m: &str, t: &str| calls.iter().position(|(cm, ct)| cm == m && ct == t);
    let hello = at("GET", "/mm/v1/official/hello").expect("hello");
    let put = at("PUT", &target).expect("PUT");
    let announce = at("POST", &format!("{target}/announce")).expect("announce");
    let delete = at("DELETE", &target).expect("DELETE");
    assert!(
        hello < put && put < announce && announce < delete,
        "{calls:?}"
    );
    // the engine was shut down with it: nobody answers on its control socket
    std::thread::sleep(Duration::from_millis(300));
    assert!(std::os::unix::net::UnixStream::connect(&control).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// CR-S99 §5 (OD-206, W6): an FM5 race spare. The agent opens `game: "fm5"`, `activity: "race"`
/// on its engine, `PUT`s it with `maxPlayers` 16 (the fake asserts both), announces it, applies
/// its punch (the engine's FM5 world takes the context and admits the joiner) and `DELETE`s it on
/// SIGTERM.
#[test]
fn an_fm5_race_spare_opens_announces_admits_and_is_deleted() {
    let dir = std::env::temp_dir().join(format!("fi-agent-fm5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("agent.key");
    std::fs::write(&key, format!("{}\n", encode_base64(&AGENT_SEED))).unwrap();
    let control = dir.join("ctl").join("control.sock");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let server_log = log.clone();
    std::thread::spawn(move || fake(listener, server_log, 201));

    let mut child = Command::new(env!("CARGO_BIN_EXE_worlds"))
        .arg("agent")
        .env_clear()
        .env("FI_WORLDS_SERVER_URL", format!("http://127.0.0.1:{port}"))
        .env("FI_WORLDS_VM_ID", VM)
        .env("FI_WORLDS_KEY_FILE", &key)
        .env("FI_WORLDS_BIND", "127.0.0.1:0")
        .env("FI_WORLDS_CONTROL", format!("unix:{}", control.display()))
        .env("FI_WORLDS_ACTIVITIES", "race")
        .env("FI_WORLDS_ANNOUNCE_SECONDS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let v: Value = serde_json::from_str(&line).expect("stdout carries JSON lines only");
            if tx.send(v).is_err() {
                return;
            }
        }
    });

    let opened = wait_line(&rx, "agent.world.opened", |v| {
        v["ev"] == "agent.world.opened"
    });
    assert_eq!(opened["activity"], "race", "{opened}");
    // OD-210, CR-S101 (W14): one mixed spare, both modes served by default
    assert_eq!(opened["mode"], "any", "{opened}");
    let world = opened["world"].as_str().unwrap().to_string();
    let admitted = wait_line(&rx, "admitted", |v| {
        v["ev"] == "agent.world.admission" && v["punches"] == 1
    });
    assert_eq!(admitted["admitted"], 1, "{admitted}");
    assert_eq!(admitted["world"], world.as_str());

    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < until, "the agent did not stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));
    let target = format!("/mm/v1/official/worlds/{world}");
    let calls = log.lock().unwrap().clone();
    let at = |m: &str, t: &str| calls.iter().position(|(cm, ct)| cm == m && ct == t);
    let put = at("PUT", &target).expect("PUT");
    let announce = at("POST", &format!("{target}/announce")).expect("announce");
    let delete = at("DELETE", &target).expect("DELETE");
    assert!(put < announce && announce < delete, "{calls:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_invalid_setting_refuses_the_start_with_a_stable_id() {
    let out = Command::new(env!("CARGO_BIN_EXE_worlds"))
        .arg("agent")
        .env_clear()
        .env("FI_WORLDS_SERVER_URL", "http://198.51.100.7:8480")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let line: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(line["ev"], "agent.config.invalid");
    assert_eq!(line["field"], "FI_WORLDS_SERVER_URL");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("198.51.100.7"));
}

/// OD-190/OD-191: an official car meet lobby (`carmeet`, both modes) opens, and the §4.4 game
/// session goes engine → agent → `POST …/game-sessions` → the group world opened on the engine
/// (then announced) → `world.gameSession.answer` → engine; Road Trip without a catalog, a refusal
/// and a call past the 14 s budget answer `session: null`. The engine's request is the
/// test-hooks synthetic one (`FI_DS_TEST_GAME_SESSION_XUIDS`): a real one needs a game.
#[cfg(feature = "test-hooks")]
#[test]
fn a_car_meet_lobby_opens_and_answers_its_game_session_end_to_end() {
    // (the fake's answer, the request's kind, the agent's outcome, a session given, calls made);
    // Road Trip without a catalog is never asked for (OD-191)
    for (status, kind, outcome, given, posts) in [
        (201, "freeroam", "created", true, 1),
        (201, "roadtrip", "not_served", false, 0),
        (400, "freeroam", "refused", false, 1),
        (409, "freeroam", "not_served", false, 1),
        (HANG, "freeroam", "timeout", false, 1),
    ] {
        let dir = std::env::temp_dir().join(format!(
            "fi-agent-cm-{}-{status}-{kind}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("agent.key");
        std::fs::write(&key, format!("{}\n", encode_base64(&AGENT_SEED))).unwrap();
        let control = dir.join("ctl").join("control.sock");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Log = Arc::default();
        let server_log = log.clone();
        std::thread::spawn(move || fake(listener, server_log, status));

        let mut child = Command::new(env!("CARGO_BIN_EXE_worlds"))
            .arg("agent")
            .env_clear()
            .env("FI_WORLDS_SERVER_URL", format!("http://127.0.0.1:{port}"))
            .env("FI_WORLDS_VM_ID", VM)
            .env("FI_WORLDS_KEY_FILE", &key)
            .env("FI_WORLDS_BIND", "127.0.0.1:0")
            .env("FI_WORLDS_CONTROL", format!("unix:{}", control.display()))
            .env("FI_WORLDS_ACTIVITIES", "carmeet")
            .env("FI_WORLDS_MODES", "noob")
            .env("FI_WORLDS_ANNOUNCE_SECONDS", "1")
            .env("FI_DS_TEST_GAME_SESSION_XUIDS", XUID)
            .env("FI_DS_TEST_GAME_SESSION_KIND", kind)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let v: Value = serde_json::from_str(&line).expect("stdout carries JSON lines only");
                if tx.send(v).is_err() {
                    return;
                }
            }
        });
        let opened = wait_line(&rx, "agent.world.opened", |v| {
            v["ev"] == "agent.world.opened"
        });
        assert_eq!(opened["activity"], "carmeet", "{opened}");
        assert_eq!(opened["mode"], "noob", "{opened}");
        let world = opened["world"].as_str().unwrap().to_string();
        // the engine's note and the agent's line come from two processes: either order
        let (mut answered, mut note, mut group) = (None, None, None);
        while answered.is_none() || note.is_none() {
            let v = wait_line(&rx, "agent.game_session and debug.game_session", |v| {
                v["ev"] == "agent.game_session"
                    || (v["ev"] == "world.note" && v["note"] == "debug.game_session")
                    || (v["ev"] == "agent.world.opened" && v["group"] == true)
            });
            if v["ev"] == "agent.game_session" {
                answered = Some(v);
            } else if v["ev"] == "agent.world.opened" {
                assert!(
                    answered.is_none(),
                    "the group world opens before the answer"
                );
                group = Some(v);
            } else {
                note = Some(v);
            }
        }
        // §11.4: a made session's group world is open on the engine before the answer
        assert_eq!(
            group.as_ref().map(|g| g["world"].clone()),
            given.then(|| json!(GROUP_WORLD)),
            "{group:?}"
        );
        if given {
            assert_eq!(group.as_ref().unwrap()["activity"], "freeroam");
            // and is announced like the others (its punches admit its players)
            wait_line(&rx, "the group world's admission", |v| {
                v["ev"] == "agent.world.admission" && v["world"] == GROUP_WORLD
            });
        }
        let (answered, note) = (answered.unwrap(), note.unwrap());
        assert_eq!(answered["world"], world.as_str(), "{answered}");
        assert_eq!(answered["outcome"], outcome, "{answered}");
        assert_eq!(
            answered["engine"], "ok",
            "the engine took the answer: {answered}"
        );
        assert_eq!(note["args"], json!([given]), "{note}");
        let line = answered.to_string();
        assert!(!line.contains(XUID), "no XUID is logged: {line}");

        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        let status_code = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(Instant::now() < until, "the agent did not stop");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status_code.code(), Some(0));
        let calls = log.lock().unwrap().clone();
        let sessions = format!("/mm/v1/official/worlds/{world}/game-sessions");
        assert_eq!(
            calls
                .iter()
                .filter(|(m, t)| m == "POST" && *t == sessions)
                .count(),
            posts,
            "{calls:?}"
        );
        if given {
            let group = format!("/mm/v1/official/worlds/{GROUP_WORLD}");
            let at = |m: &str, t: &str| calls.iter().position(|(cm, ct)| cm == m && ct == t);
            let posted = at("POST", &sessions).unwrap();
            let announced = at("POST", &format!("{group}/announce")).expect("group announce");
            assert!(posted < announced, "{calls:?}");
            assert!(at("PUT", &group).is_none(), "the server opened it: no PUT");
            assert!(at("DELETE", &group).is_some(), "closed with the agent");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
