// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Official worlds (WORLDS_OFFICIAL.md §3, §6; contract §5.2, §6.3): the
//! `official` open and context, the `{official: vmId}` grant host and the FIUP host tag, and the
//! one direct public socket. Synthetic only: a test key, synthetic XUIDs, TEST-NET addresses.

use std::net::SocketAddr;

use serde_json::{Value, json};

use super::{
    Config, Io, Server, fiup,
    grant::{self, Context, Failure},
    keys::{KeyRing, encode_base64},
};
use crate::{
    control::ConnId,
    fh2::engine::{
        bits::Rd,
        datagram,
        message::{self, Message},
    },
};

const SEED: [u8; 32] = [7; 32];
const KID: &str = "test-k1";
const CONN: ConnId = 1;
const VM: &str = "sao-1";
const W1: &str = "4f1c2a6e-1b2d-4c3e-8f40-5a6b7c8d9e01";
const W2: &str = "4f1c2a6e-1b2d-4c3e-8f40-5a6b7c8d9e02";
const G1: &str = "6d1f0c2e-0a3b-4c5d-8e6f-7a8b9c0d1e21";
const G2: &str = "6d1f0c2e-0a3b-4c5d-8e6f-7a8b9c0d1e22";
const SUB: &str = "0b1c2d3e-4f50-4617-8829-3a4b5c6d7e81";
const HOST_XUID: u64 = 1_000_000_000_000_101;
const J1: u64 = 1_000_000_000_000_102;
const S0: u64 = 1_798_000_000_000;
const NOW: i64 = (S0 / 1000) as i64;
const PROOF_KEY: [u8; 32] = [9; 32];

#[derive(Default)]
struct NoIo;

impl Io for NoIo {
    fn memory_used_bytes(&self) -> u64 {
        0
    }
    fn cpu_ms(&self) -> Option<u64> {
        None
    }
    fn rss_bytes(&self) -> Option<u64> {
        None
    }
}

fn ring() -> KeyRing {
    let public = ed25519_dalek::SigningKey::from_bytes(&SEED).verifying_key();
    KeyRing::parse(&format!("{KID}:{}", encode_base64(public.as_bytes()))).unwrap()
}

fn sign(claims: &str) -> (String, String) {
    use ed25519_dalek::Signer;
    let mut message = b"fi-relay-grant-v1\n".to_vec();
    message.extend_from_slice(claims.as_bytes());
    let sig = ed25519_dalek::SigningKey::from_bytes(&SEED).sign(&message);
    (
        encode_base64(claims.as_bytes()),
        encode_base64(&sig.to_bytes()),
    )
}

/// Grant claims for `world` with the raw `host` member and `extra` members: role `joiner` for an
/// official host (the only role it may carry), `host` for a player host.
fn claims(grant: &str, world: &str, host: &str, extra: &str) -> String {
    let role = if host.contains("official") {
        "joiner"
    } else {
        "host"
    };
    claims_as(grant, world, host, role, extra)
}

fn claims_as(grant: &str, world: &str, host: &str, role: &str, extra: &str) -> String {
    format!(
        r#"{{"v":1,"kid":"{KID}","grant":"{grant}","world":"{world}","sub":"{SUB}","xuid":"{J1}","tier":"free","host":{host},"role":"{role}","iat":{},"exp":{}{extra}}}"#,
        NOW - 1,
        NOW + 59
    )
}

fn official_host() -> String {
    format!(r#"{{"official":"{VM}"}}"#)
}

fn punch(grant: &str, world: &str, host: &str) -> Value {
    let (c, s) = sign(&claims(grant, world, host, ""));
    json!({"grantId": grant, "xuid": J1.to_string(), "kicked": false, "transport": "direct",
        "endpoint": {"ip": "192.0.2.20", "port": 3074},
        "relay": {"claims": c, "signature": s,
                  "udpProof": {"version": 1, "key": encode_base64(&PROOF_KEY)}}})
}

fn server() -> (Server, NoIo) {
    crate::log::mute();
    let mut c = Config::new();
    c.keys = ring();
    c.games = crate::registered_games(Default::default());
    c.public_socket = true;
    let mut s = Server::new(c, 1, 0);
    s.connect(CONN);
    let mut io = NoIo;
    let r = s.request(
        &mut io,
        0,
        CONN,
        &json!({"id": 1, "op": "hello", "proto": 1, "controller": {"kind": "vm", "version": "1.0.0"}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    (s, io)
}

fn call(s: &mut Server, io: &mut NoIo, now: u64, req: Value) -> Value {
    s.request(io, now, CONN, &req)
}

fn open_req(world: &str, vm: &str) -> Value {
    json!({"id": 2, "op": "world.open", "worldId": world, "game": "fh2",
        "host": {"kind": "official", "vmId": vm}, "activity": "freeroam",
        "maxPlayers": 8, "races": true, "requireProof": false})
}

fn context_req(world: &str, host: Value) -> Value {
    json!({"id": 3, "op": "world.context", "worldId": world, "serverTimeMs": S0,
        "world": {"id": world, "game": "fh2", "host": host}})
}

fn open(s: &mut Server, io: &mut NoIo, world: &str) {
    let r = call(s, io, 0, open_req(world, VM));
    assert_eq!(r["ok"], true, "{r}");
    let r = call(s, io, 0, context_req(world, json!({"official": VM})));
    assert_eq!(r["leaseUntilMs"], S0 + 45_000, "{r}");
}

fn admit(s: &mut Server, io: &mut NoIo, world: &str, punches: Vec<Value>) -> Value {
    call(
        s,
        io,
        20,
        json!({"id": 4, "op": "world.admission", "worldId": world,
            "serverTimeMs": S0 + 20, "punches": punches}),
    )
}

fn header(host: u64) -> [u8; 56] {
    fiup::header(
        grant::uuid_bytes(G1).unwrap(),
        grant::uuid_bytes(W1).unwrap(),
        host,
        J1,
    )
}

fn ep(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn open_connection(user: u64) -> Vec<u8> {
    open_connection_to(user, 2)
}

/// An OpenConnection for lobby type `lobby` (1 Road Trip, 2 free roam, 3/6 car meets).
fn open_connection_to(user: u64, lobby: u32) -> Vec<u8> {
    let m = Message::encode(&Message::Connect {
        version: message::PROTOCOL_VERSION.to_vec(),
        game: message::GAME_TITLE.to_vec(),
        flags: 0,
        user,
        a: lobby,
        b: 1,
        attempt: 0x1234_5678,
    });
    datagram::assemble([&m], 0, datagram::NO_ACK, 0)
}

/// The OpenResponse reasons sent to `to` (0 admitted, 4 refused).
fn responses(s: &mut Server, to: SocketAddr) -> Vec<u8> {
    let mut out = Vec::new();
    for d in s.take_datagrams().into_iter().filter(|d| d.to == to) {
        let Some(h) = datagram::header_of(&d.data) else {
            continue;
        };
        let mut r = Rd::new(datagram::messages_of(&d.data));
        for _ in 0..h.count {
            match Message::decode(&mut r) {
                Ok(Message::ConnectReply { reason, .. }) => out.push(reason),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    out
}

/// Runs FIUP for the game socket at `game` with `host` in the header: the ack's type, if any.
fn prove(s: &mut Server, io: &mut NoIo, game: SocketAddr, host: u64) -> Option<u8> {
    let hello = fiup::joiner::hello(&header(host), 0, [5; 16], &PROOF_KEY);
    s.datagram(io, 30, game, &hello);
    let challenge = s
        .take_datagrams()
        .into_iter()
        .find(|d| d.to == game && d.data.starts_with(b"FIUP"))?
        .data;
    let response = fiup::joiner::response(&challenge, &PROOF_KEY);
    s.datagram(io, 40, game, &response);
    let ack = s.take_datagrams().into_iter().find(|d| d.to == game)?;
    Some(ack.data[5])
}

#[test]
fn the_grant_host_is_exactly_one_of_xuid_or_official() {
    let ctx = |host: &'static str| Context {
        world: W1,
        host_xuid: host,
        peer_xuid: "1000000000000102",
        role: if host.starts_with("official:") {
            "joiner"
        } else {
            "host"
        },
        grant: Some(G1),
    };
    let verify = |host: &str, extra: &str, context: Context| {
        let (c, s) = sign(&claims(G1, W1, host, extra));
        grant::verify(&c, &s, &ring(), NOW, context).map(|v| v.0)
    };
    // an official grant is a joiner grant: a host-role one is refused (WORLDS_OFFICIAL.md §3)
    let (c, s) = sign(&claims_as(G1, W1, &official_host(), "host", ""));
    let host_role = Context {
        role: "host",
        ..ctx("official:sao-1")
    };
    assert_eq!(
        grant::verify(&c, &s, &ring(), NOW, host_role).unwrap_err(),
        Failure::Invalid
    );
    let official = verify(&official_host(), "", ctx("official:sao-1")).unwrap();
    assert_eq!(official.role, "joiner");
    assert_eq!(official.host_xuid, "official:sao-1");
    let player = format!(r#"{{"xuid":"{HOST_XUID}"}}"#);
    assert!(verify(&player, "", ctx("1000000000000101")).is_ok());
    // the other VM, or a player context, is not this grant's
    assert_eq!(
        verify(&official_host(), "", ctx("official:sao-2")).unwrap_err(),
        Failure::ContextMismatch
    );
    let player_ctx = Context {
        role: "joiner",
        ..ctx("1000000000000101")
    };
    assert_eq!(
        verify(&official_host(), "", player_ctx).unwrap_err(),
        Failure::ContextMismatch
    );
    let both = format!(r#"{{"xuid":"{HOST_XUID}","official":"{VM}"}}"#);
    let bad_vm = r#"{"official":"SAO-1"}"#;
    let long_vm = format!(r#"{{"official":"{}"}}"#, "a".repeat(33));
    let typed = format!(r#"{{"xuid":"{HOST_XUID}","official":1}}"#);
    for host in [
        both.as_str(),
        "{}",
        bad_vm,
        long_vm.as_str(),
        typed.as_str(),
        r#"{"official":""}"#,
        r#"{"official":"official:sao-1"}"#,
        r#"{"official":null}"#,
        r#"{"xuid":null,"official":"sao-1"}"#,
        r#"{"official":"sao 1"}"#,
        r#"{"official":"sao-1","extra":1}"#,
    ] {
        assert_eq!(
            verify(host, "", ctx("official:sao-1")).unwrap_err(),
            Failure::Invalid,
            "{host}"
        );
    }
    // an official world's grant names no relay (OD-189: direct)
    let relay = r#","relay":{"key":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","addr":"192.0.2.9","binding":3478,"port":5101}"#;
    assert_eq!(
        verify(&official_host(), relay, ctx("official:sao-1")).unwrap_err(),
        Failure::Invalid
    );
    assert!(verify(&player, relay, ctx("1000000000000101")).is_ok());
}

#[test]
fn an_official_world_opens_with_a_valid_vm_id_and_checks_its_context() {
    let (mut s, mut io) = server();
    for vm in ["VM-1", "", "vm_1", &"a".repeat(33)] {
        let r = call(&mut s, &mut io, 0, open_req(W1, vm));
        assert_eq!(r["error"]["field"], "host.vmId", "{vm}: {r}");
    }
    let mut player = open_req(W1, VM);
    player["host"] = json!({"kind": "player", "xuid": HOST_XUID.to_string(), "isPlayer": false});
    assert_eq!(
        call(&mut s, &mut io, 0, player)["error"]["field"],
        "host.kind"
    );
    let r = call(&mut s, &mut io, 0, open_req(W1, &"a".repeat(32)));
    assert_eq!(r["ok"], true, "the longest vmId: {r}");
    let r = call(&mut s, &mut io, 0, open_req(W2, VM));
    assert_eq!(r["ok"], true, "{r}");
    // the context names this world's VM, alone
    let r = call(&mut s, &mut io, 0, context_req(W2, json!({"official": VM})));
    assert_eq!(r["leaseUntilMs"], S0 + 45_000, "{r}");
    for host in [
        json!({"official": "sao-2"}),
        json!({"official": VM, "xuid": HOST_XUID.to_string()}),
    ] {
        let r = call(&mut s, &mut io, 0, context_req(W2, host.clone()));
        assert_eq!(r["error"]["code"], "context_invalid", "{host}: {r}");
    }
    let r = call(
        &mut s,
        &mut io,
        0,
        context_req(W2, json!({"xuid": HOST_XUID.to_string()})),
    );
    assert_eq!(r["error"]["field"], "world.host.official", "{r}");
    let st = call(
        &mut s,
        &mut io,
        0,
        json!({"id": 5, "op": "world.status", "worldId": W2}),
    );
    assert_eq!(st["running"], true, "{st}");
}

#[test]
fn a_joiner_proven_by_fiup_with_the_official_tag_is_admitted_on_the_public_socket() {
    let (mut s, mut io) = server();
    open(&mut s, &mut io, W1);
    let r = admit(&mut s, &mut io, W1, vec![punch(G1, W1, &official_host())]);
    assert_eq!(
        (r["admitted"].as_u64(), r["proofGrants"].as_u64()),
        (Some(1), Some(1)),
        "{r}"
    );
    assert!(
        r.get("legs").is_none(),
        "no relay legs on an official world: {r}"
    );
    let game = ep("192.0.2.20:3074");
    // an unproven socket's game datagram has no world on the public socket
    s.datagram(&mut io, 25, game, &open_connection(J1));
    assert!(s.take_datagrams().is_empty());
    assert_eq!(s.stats.unrouted_dropped, 1);
    // a header that names the host as 0 or as a player XUID is not this world's proof
    assert_eq!(prove(&mut s, &mut io, game, 0), None);
    assert_eq!(prove(&mut s, &mut io, game, HOST_XUID), None);
    let tag = fiup::official_host_tag(VM);
    assert_eq!(prove(&mut s, &mut io, game, tag), Some(4), "the ack");
    s.datagram(&mut io, 2000, game, &open_connection(J1));
    assert_eq!(responses(&mut s, game), vec![0], "admitted");
    // the same address from another port is neither proven nor routed
    let other = ep("192.0.2.20:3075");
    let dropped = s.stats.unrouted_dropped;
    s.datagram(&mut io, 2010, other, &open_connection(J1));
    assert!(responses(&mut s, other).is_empty());
    assert_eq!(s.stats.unrouted_dropped, dropped + 1);
}

#[test]
fn datagrams_reach_only_the_world_that_holds_their_grant_or_binding() {
    let (mut s, mut io) = server();
    open(&mut s, &mut io, W1);
    open(&mut s, &mut io, W2);
    admit(&mut s, &mut io, W1, vec![punch(G1, W1, &official_host())]);
    // a FIUP frame for a grant no world holds: dropped, no answer
    let stranger = fiup::joiner::hello(
        &fiup::header(
            grant::uuid_bytes(G2).unwrap(),
            grant::uuid_bytes(W2).unwrap(),
            fiup::official_host_tag(VM),
            J1,
        ),
        0,
        [6; 16],
        &PROOF_KEY,
    );
    let game = ep("192.0.2.20:3074");
    s.datagram(&mut io, 30, game, &stranger);
    assert!(s.take_datagrams().is_empty());
    assert_eq!(s.stats.unrouted_dropped, 1);
    // a game datagram from a source no world bound yet: dropped, no answer
    s.datagram(&mut io, 30, game, &open_connection(J1));
    assert!(s.take_datagrams().is_empty());
    assert_eq!(s.stats.unrouted_dropped, 2);
    assert_eq!(
        prove(&mut s, &mut io, game, fiup::official_host_tag(VM)),
        Some(4)
    );
    s.datagram(&mut io, 2000, game, &open_connection(J1));
    assert_eq!(responses(&mut s, game), vec![0]);
    let st = |s: &mut Server, io: &mut NoIo, w: &str| {
        call(
            s,
            io,
            2100,
            json!({"id": 6, "op": "world.status", "worldId": w}),
        )
    };
    assert_eq!(st(&mut s, &mut io, W1)["admission"]["grants"], 1);
    let other = st(&mut s, &mut io, W2);
    assert_eq!(
        other["admission"]["grants"].as_u64().unwrap_or(0),
        0,
        "{other}"
    );
}

#[test]
fn a_player_hosted_grant_is_refused_on_an_official_world() {
    let (mut s, mut io) = server();
    open(&mut s, &mut io, W1);
    let player = format!(r#"{{"xuid":"{HOST_XUID}"}}"#);
    let r = admit(&mut s, &mut io, W1, vec![punch(G1, W1, &player)]);
    assert_eq!(
        (r["admitted"].as_u64(), r["proofGrants"].as_u64()),
        (Some(0), Some(0)),
        "{r}"
    );
    let game = ep("192.0.2.20:3074");
    for host in [HOST_XUID, fiup::official_host_tag(VM)] {
        assert_eq!(prove(&mut s, &mut io, game, host), None);
    }
    // an official grant for another VM is not this world's either
    let other = r#"{"official":"sao-2"}"#;
    let r = admit(&mut s, &mut io, W1, vec![punch(G1, W1, other)]);
    assert_eq!(r["admitted"], 0, "{r}");
    // nor a host-role grant naming this VM: the engine is the host
    let (c, sig) = sign(&claims_as(G1, W1, &official_host(), "host", ""));
    let mut p = punch(G1, W1, &official_host());
    p["relay"]["claims"] = json!(c);
    p["relay"]["signature"] = json!(sig);
    let r = admit(&mut s, &mut io, W1, vec![p]);
    assert_eq!(r["admitted"], 0, "{r}");
}

/// The lobby types a proven joiner's OpenConnection is admitted to (0) or refused from (4) in a
/// public world opened with `activity` (and `carMeetLobby`, when given).
fn answers_by_lobby(activity: &str, car_meet_lobby: Option<u32>) -> Vec<(u32, Vec<u8>)> {
    [1u32, 2, 3, 6]
        .into_iter()
        .map(|lobby| {
            let (mut s, mut io) = server();
            let mut req = open_req(W1, VM);
            req["activity"] = json!(activity);
            if let Some(l) = car_meet_lobby {
                req["carMeetLobby"] = json!(l);
            }
            let r = call(&mut s, &mut io, 0, req);
            assert_eq!(r["ok"], true, "{r}");
            let r = call(&mut s, &mut io, 0, context_req(W1, json!({"official": VM})));
            assert_eq!(r["leaseUntilMs"], S0 + 45_000, "{r}");
            admit(&mut s, &mut io, W1, vec![punch(G1, W1, &official_host())]);
            let game = ep("192.0.2.20:3074");
            assert_eq!(
                prove(&mut s, &mut io, game, fiup::official_host_tag(VM)),
                Some(4)
            );
            s.datagram(&mut io, 2000, game, &open_connection_to(J1, lobby));
            (lobby, responses(&mut s, game))
        })
        .collect()
}

#[test]
fn the_official_car_meet_lobby_serves_both_locations_and_their_free_roam() {
    // OD-190: `carmeet` without a location is the official car meet lobby, open to both modes
    // (the server's seats decide); Road Trip is the one lobby it refuses
    let (mut s, mut io) = server();
    let mut req = open_req(W1, VM);
    req["activity"] = json!("carmeet");
    let r = call(&mut s, &mut io, 0, req.clone());
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["lobbies"], json!(["carmeet"]), "{r}");
    // the same open again is idempotent; with a location it is another world's spec
    assert_eq!(call(&mut s, &mut io, 0, req.clone())["ok"], true);
    let mut located = req.clone();
    located["carMeetLobby"] = json!(3);
    let r = call(&mut s, &mut io, 0, located);
    assert_eq!(r["error"]["code"], "exists", "{r}");
    let r = call(&mut s, &mut io, 0, context_req(W1, json!({"official": VM})));
    assert_eq!(r["leaseUntilMs"], S0 + 45_000, "{r}");
    let r = call(
        &mut s,
        &mut io,
        10,
        json!({"id": 9, "op": "world.status", "worldId": W1}),
    );
    assert_eq!(r["activity"], "carmeet", "{r}");
    assert_eq!(
        answers_by_lobby("carmeet", None),
        vec![(1, vec![4]), (2, vec![0]), (3, vec![0]), (6, vec![0])]
    );
}

fn status(s: &mut Server, io: &mut NoIo, world: &str) -> Value {
    call(
        s,
        io,
        10,
        json!({"id": 7, "op": "world.status", "worldId": world}),
    )
}

fn private_context(world: &str, private: Value) -> Value {
    let mut req = context_req(world, json!({"official": VM}));
    req["world"]["private"] = private;
    req
}

#[test]
fn a_public_official_worlds_state_names_no_privacy_and_no_leader() {
    let (mut s, mut io) = server();
    open(&mut s, &mut io, W1);
    let st = status(&mut s, &mut io, W1);
    assert_eq!(st["running"], true, "{st}");
    assert!(
        st.get("private").is_none() && st.get("leader").is_none(),
        "{st}"
    );
    // `private: false` is the same as no member
    let r = call(&mut s, &mut io, 0, private_context(W1, json!(false)));
    assert_eq!(r["ok"], true, "{r}");
    let st = status(&mut s, &mut io, W1);
    assert!(st.get("private").is_none(), "{st}");
}

#[test]
fn the_contexts_private_member_makes_a_free_roam_or_road_trip_world_private_for_good() {
    for activity in ["freeroam", "roadtrip"] {
        let (mut s, mut io) = server();
        let mut req = open_req(W1, VM);
        req["activity"] = json!(activity);
        assert_eq!(call(&mut s, &mut io, 0, req)["ok"], true);
        let r = call(&mut s, &mut io, 0, private_context(W1, json!(true)));
        assert_eq!(r["ok"], true, "{activity}: {r}");
        let st = status(&mut s, &mut io, W1);
        assert_eq!(st["private"], true, "{activity}: {st}");
        assert!(st.get("leader").is_none(), "nobody in, nobody leads: {st}");
        // one way: a later context without it (or with false) keeps the world private
        for later in [
            context_req(W1, json!({"official": VM})),
            private_context(W1, json!(false)),
        ] {
            let r = call(&mut s, &mut io, 0, later);
            assert_eq!(r["ok"], true, "{r}");
            assert_eq!(status(&mut s, &mut io, W1)["private"], true);
        }
    }
}

#[test]
fn a_car_meet_world_or_a_malformed_private_member_is_refused_and_changes_nothing() {
    let (mut s, mut io) = server();
    let mut req = open_req(W1, VM);
    req["activity"] = json!("carmeet");
    assert_eq!(call(&mut s, &mut io, 0, req)["ok"], true);
    let r = call(&mut s, &mut io, 0, private_context(W1, json!(true)));
    assert_eq!(r["error"]["field"], "world.private", "{r}");
    let st = status(&mut s, &mut io, W1);
    assert!(st.get("private").is_none(), "{st}");
    assert_eq!(
        st["admission"]["leaseUntilMs"],
        Value::Null,
        "no lease: {st}"
    );
    open(&mut s, &mut io, W2);
    let r = call(&mut s, &mut io, 0, private_context(W2, json!("yes")));
    assert_eq!(r["error"]["field"], "world.private", "{r}");
    assert!(status(&mut s, &mut io, W2).get("private").is_none());
}

#[test]
fn a_located_car_meet_and_a_free_roam_world_serve_only_their_own_lobby() {
    assert_eq!(
        answers_by_lobby("carmeet", Some(3)),
        vec![(1, vec![4]), (2, vec![4]), (3, vec![0]), (6, vec![4])]
    );
    assert_eq!(
        answers_by_lobby("freeroam", None),
        vec![(1, vec![0]), (2, vec![0]), (3, vec![4]), (6, vec![4])]
    );
    // a location belongs to `carmeet` only
    let (mut s, mut io) = server();
    let mut req = open_req(W1, VM);
    req["carMeetLobby"] = json!(3);
    let r = call(&mut s, &mut io, 0, req);
    assert_eq!(r["error"]["field"], "carMeetLobby", "{r}");
}
