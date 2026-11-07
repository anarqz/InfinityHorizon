// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The golden vectors of FM5's NerdCore and race server (OD-206, CR-S99, W6): every test of the
//! LAN lab's `tools/fm5-probe/test_world.py` that applies, with its inputs and expected bytes, then
//! Infinity's own (admission, seats, events, the `world.open` ceiling, private worlds). Synthetic
//! packets only: no game, no capture, synthetic XUIDs, loopback addresses.
//!
//! Not ported: the lab's control-file commands (`start`, `host`, `console`, `options`, `grid`,
//! `complete`: a live-experiment tool, not a product feature) and the two tests that read a
//! private capture (`~/re/fm5/world-capture.bin`, kept out of Git).

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};

use super::{
    nerd::{self, Fields, Handle, Object, Reader, Writer},
    race::{self, Peer, Race},
};
use crate::core::{
    game::{Admit, GameEvent, Send},
    rng::Rng,
};

/// The world's admission as a test sets it: per XUID, allowed or a proof expected.
#[derive(Default)]
struct Gate {
    denied: Mutex<Vec<u64>>,
    proving: Mutex<Vec<u64>>,
}

impl Admit for Gate {
    fn allowed(&self, xuid: u64, _from: SocketAddr, _kind: u8, _mono: u64) -> bool {
        !self.denied.lock().unwrap().contains(&xuid)
            && !self.proving.lock().unwrap().contains(&xuid)
    }
    fn proof_expected(&self, xuid: u64, _mono: u64) -> bool {
        self.proving.lock().unwrap().contains(&xuid)
    }
}

const X1: u64 = 1_000_000_000_000_101;
const X2: u64 = 1_000_000_000_000_102;

/// The lab's sockets: every datagram the world sent, read per destination in order.
struct Lab {
    race: Race,
    gate: Arc<Gate>,
    queue: Vec<Send>,
    now: u64,
}

impl Lab {
    fn new() -> Lab {
        Lab::with_seats(16)
    }

    fn with_seats(seats: usize) -> Lab {
        crate::log::mute();
        let gate = Arc::new(Gate::default());
        Lab {
            race: Race::new(gate.clone(), Rng::new(7), seats),
            gate,
            queue: Vec::new(),
            now: 0,
        }
    }

    fn pull(&mut self) {
        self.queue.append(&mut self.race.out);
    }

    fn receive(&mut self, data: &[u8], from: SocketAddr) {
        self.race.receive(data, from, self.now);
        self.pull();
    }

    /// Everything waiting for `to` (world.py's test `drain`).
    fn drain(&mut self, to: SocketAddr) -> Vec<Vec<u8>> {
        self.pull();
        let (mine, rest): (Vec<Send>, Vec<Send>) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|s| s.to == to);
        self.queue = rest;
        mine.into_iter().map(|s| s.data).collect()
    }

    /// The first datagram waiting for `to` (a socket's `recvfrom`).
    fn recv(&mut self, to: SocketAddr) -> Vec<u8> {
        self.pull();
        let i = self
            .queue
            .iter()
            .position(|s| s.to == to)
            .expect("a datagram");
        self.queue.remove(i).data
    }

    fn exchange(&mut self, messages: Vec<Writer>, ident: u16, from: SocketAddr) -> Vec<u8> {
        self.receive(&packet(messages, ident), from);
        self.recv(from)
    }

    /// The handshake (OpenConnection, then the OpenResponse ack); what the console got after it.
    fn connect(&mut self, from: SocketAddr) -> Vec<Vec<u8>> {
        self.connect_as(from, 0)
    }

    fn connect_as(&mut self, from: SocketAddr, user: u64) -> Vec<Vec<u8>> {
        let reply = self.exchange(vec![open_connection(nerd::VERSION, user, 7, 6)], 0, from);
        let own = own_attempt(&reply);
        let ack = nerd::open_response(0, nerd::REASON_RESPONSE_ACK, own, 7);
        self.receive(&packet(vec![ack], 1), from);
        self.drain(from)
    }

    fn peer(&mut self, at: SocketAddr) -> &mut Peer {
        let i = self.race.position(at).unwrap();
        &mut self.race.peers[i]
    }

    fn ready(&mut self, from: SocketAddr, ident: u16) {
        let message = nerd::lobby_message(nerd::READY_REQUEST).push(1, 1);
        self.receive(&packet(vec![message], ident), from);
    }

    fn console_update(&mut self, from: SocketAddr, ident: u16, seq: u8, pairs: &[(u64, u64)]) {
        let mut payload = Writer::new();
        for (index, value) in pairs {
            payload.put(*index, 4).put(*value, 1);
        }
        let message = nerd::object_update(
            nerd::ROUTER_CONSOLE,
            nerd::HANDLE_OWN,
            0,
            seq,
            &payload,
            None,
        );
        self.receive(&packet(vec![message], ident), from);
    }

    fn console_of(&mut self, from: SocketAddr) {
        let message =
            nerd::object_create(nerd::ROUTER_CONSOLE, nerd::HANDLE_OWN, 0, 0, 0, None, None);
        self.receive(&packet(vec![message], 4), from);
    }

    fn player_create(&mut self, from: SocketAddr, ident: u16) {
        self.receive(&packet(vec![player_create(0, 0b1011, 4)], ident), from);
    }

    fn launch_two(&mut self) -> (SocketAddr, SocketAddr) {
        let (first, second) = (at(1), at(2));
        self.connect(first);
        self.connect(second);
        self.ready(first, 5);
        self.ready(second, 5);
        self.console_update(first, 6, 1, &[(4, 1), (7, 1)]);
        self.console_update(second, 6, 1, &[(4, 1), (7, 1)]);
        self.drain(first);
        self.drain(second);
        (first, second)
    }

    /// The StartingGrids waiting for `to`: (counter, indexes).
    fn grids(&mut self, to: SocketAddr) -> Vec<(u64, Vec<u64>)> {
        let mut out = Vec::new();
        for p in self.drain(to) {
            let mut r = Reader::new(nerd::body(&p));
            if r.bits(nerd::ID_BITS).unwrap() == u64::from(nerd::STARTING_GRID) {
                nerd::read_handle(&mut r, nerd::LOBBY_ID_BITS).unwrap();
                let (counter, count) = (r.bits(32).unwrap(), r.bits(5).unwrap());
                out.push((counter, (0..count).map(|_| r.bits(8).unwrap()).collect()));
            }
        }
        out
    }
}

fn at(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40_000 + n))
}

fn packet(messages: Vec<Writer>, ident: u16) -> Vec<u8> {
    nerd::packet(&messages, ident, nerd::NO_ACK, 0).unwrap()
}

/// test_world.py `open_connection`.
fn open_connection(version: &[u8], user: u64, attempt: u32, id_bits: usize) -> Writer {
    Writer::new()
        .push(u64::from(nerd::OPEN_CONNECTION), id_bits)
        .string(version)
        .string(nerd::GAME)
        .push(0, 2)
        .push(user, 64)
        .push(0, 32)
        .push(0, 32)
        .push(u64::from(attempt), 32)
}

fn player_create(seq: u8, value: u64, bits: usize) -> Writer {
    let payload = Writer::new().push(value, bits);
    nerd::object_create(
        nerd::ROUTER_PLAYER,
        nerd::HANDLE_OWN,
        0,
        0,
        seq,
        Some(&payload),
        None,
    )
}

/// test_world.py `own_message`: the id, an own handle with car id 0, then the rest.
fn own_message(ident: u8, rest: &[(u64, usize)]) -> Writer {
    let mut w = Writer::new()
        .push(u64::from(ident), nerd::ID_BITS)
        .push(0, 2)
        .push(0, nerd::CAR_ID_BITS);
    for (value, bits) in rest {
        w.put(*value, *bits);
    }
    w
}

fn messages(data: &[u8]) -> Vec<(u8, Option<Fields>)> {
    nerd::read_messages(data).unwrap()
}

fn ids(data: &[u8]) -> Vec<u8> {
    messages(data).into_iter().map(|m| m.0).collect()
}

fn first_id(data: &[u8]) -> u8 {
    Reader::new(nerd::body(data)).bits(nerd::ID_BITS).unwrap() as u8
}

fn object(data: &[u8]) -> Object {
    match messages(data).remove(0).1 {
        Some(Fields::Object(o)) => o,
        other => panic!("not an object: {other:?}"),
    }
}

fn handled(data: &[u8]) -> Handle {
    match messages(data).remove(0).1 {
        Some(Fields::Handled(h)) => h,
        other => panic!("not a handled message: {other:?}"),
    }
}

fn own_attempt(reply: &[u8]) -> u32 {
    match messages(reply).remove(0).1 {
        Some(Fields::OpenResponse { own_attempt, .. }) => own_attempt,
        other => panic!("not an OpenResponse: {other:?}"),
    }
}

// ---- Codec ----

/// deprecated/beta/dedicated/src/nerd.rs `open_connection_matches_capture`: FH2's real bytes.
#[test]
fn matches_fh2s_captured_open_connection() {
    let data = packet(vec![open_connection(b"6CC152FD", 0, 0, 7)], 0);
    let captured = [
        0x00, 0x00, 0xfe, 0xff, 0, 0, 0, 0, 0x01, 0x33, 0x00, 0x00, 0x10, 0x9b, 0xa1, 0xa1, 0x98,
        0x1a, 0x19, 0x23, 0x22, 0x10, 0x23, 0xb6, 0xb0, 0xb3, 0x39, 0xb4, 0x34, 0x38,
    ];
    assert_eq!(&data[..30], &captured);
    assert_eq!(data.len(), 51);
}

#[test]
fn message_ids_of_fm5() {
    assert_eq!(nerd::NAMES.len(), 43);
    assert_eq!(nerd::NAMES[29], "MsgGameLaunch");
    assert_eq!(nerd::NAMES[42], "MsgUpdateAI");
}

#[test]
fn round_trip() {
    let data = nerd::packet(&[nerd::ping(5), nerd::time_sync(2, 9)], 3, 1, 0).unwrap();
    assert_eq!(
        messages(&data),
        vec![
            (nerd::PING, Some(Fields::Ping { time: 5 })),
            (
                nerd::TIME_SYNC,
                Some(Fields::TimeSync { index: 2, time: 9 })
            ),
        ]
    );
}

#[test]
fn unknown_layout_stops_the_parse() {
    let data = nerd::packet(
        &[Writer::new().push(29, nerd::ID_BITS), nerd::ping(5)],
        3,
        1,
        0,
    )
    .unwrap();
    assert_eq!(messages(&data), vec![(29, None)]);
}

// ---- Handshake ----

#[test]
fn full_handshake_and_ping() {
    let mut lab = Lab::new();
    let a = at(1);
    let reply = lab.exchange(vec![open_connection(nerd::VERSION, 0, 7, 6)], 0, a);
    let head = nerd::header(&reply).unwrap();
    assert_eq!((head.ident, head.ack), (nerd::NO_SEQ, 0));
    let Some(Fields::OpenResponse {
        reason,
        attempt,
        own_attempt: own,
        ..
    }) = messages(&reply).remove(0).1
    else {
        panic!("no OpenResponse");
    };
    assert_eq!(
        (first_id(&reply), reason, attempt),
        (nerd::OPEN_RESPONSE, nerd::REASON_OK, 7)
    );
    let ack = nerd::open_response(0, nerd::REASON_RESPONSE_ACK, own, 7);
    let reply = lab.exchange(vec![ack], 1, a);
    assert_eq!(ids(&reply), vec![nerd::TIME_SYNC, nerd::SYNC_DONE]);
    assert_eq!(
        messages(&reply)[1].1,
        Some(Fields::SyncDone { attempt: own })
    );
    lab.recv(a); // SessionHost
    lab.recv(a); // GameOptions
    let reply = lab.exchange(vec![nerd::ping(1234)], 2, a);
    assert_eq!(
        messages(&reply)[0],
        (
            nerd::PONG,
            Some(Fields::Pong {
                echo: 1234,
                time: 1
            })
        )
    );
    assert_eq!(nerd::header(&reply).unwrap().ack, 2);
}

#[test]
fn server_pings_a_synced_peer() {
    let mut lab = Lab::new();
    let a = at(1);
    lab.exchange(vec![open_connection(nerd::VERSION, 0, 7, 6)], 0, a);
    let p = lab.peer(a);
    p.synced = true;
    p.next_ping = 0;
    lab.race.tick(0);
    let reply = lab.recv(a);
    assert_eq!(first_id(&reply), nerd::PING);
}

#[test]
fn packets_from_strangers_make_no_peer() {
    let mut lab = Lab::new();
    lab.receive(&packet(vec![nerd::ping(1)], 0), at(1));
    assert!(lab.race.peers.is_empty());
}

#[test]
fn wrong_size_is_refused() {
    let mut lab = Lab::new();
    let mut data = packet(vec![open_connection(nerd::VERSION, 0, 7, 6)], 0);
    data.push(b'x');
    lab.receive(&data, at(1));
    assert!(lab.race.peers.is_empty());
}

#[test]
fn acks_track_received_packets() {
    let mut peer = Peer::new(at(1), 0, 0);
    for ident in [0, 1, 3] {
        peer.note_received(ident);
    }
    // bit 0 is id 2 (missing), bit 1 id 1, bit 2 id 0
    assert_eq!(peer.ack(), (3, 0b110));
}

// ---- Objects ----

#[test]
fn session_host_create_layout() {
    let message = nerd::object_create(
        nerd::ROUTER_LOBBY,
        nerd::HANDLE_OWN,
        0,
        nerd::TYPE_SESSION_HOST,
        0,
        None,
        None,
    );
    assert_eq!(message.len(), 6 + 3 + 2 + 2 + 2 + 8 + 14);
    let bytes = message.bytes();
    let mut r = Reader::new(&bytes);
    assert_eq!(r.bits(6).unwrap(), u64::from(nerd::OBJECT_CREATE));
    assert_eq!(r.bits(3).unwrap(), 6);
    let o = object(&packet(vec![message], 0));
    assert_eq!(
        (o.router, o.id, o.kind, o.seq, o.bits),
        (6, 0, Some(0), 0, 0)
    );
}

#[test]
fn game_options_payload_widths() {
    let options = race::game_options(race::DEFAULT_TRACK, race::DEFAULT_LAPS);
    let payload = nerd::vars_payload(&options, nerd::GAME_OPTIONS_INDEX_BITS);
    assert_eq!(payload.len(), 4 * 9 + 8 + 32 + 3 + 32);
    let bytes = payload.bytes();
    let mut r = Reader::new(&bytes);
    assert_eq!((r.bits(9).unwrap(), r.bits(8).unwrap()), (0, 1));
    assert_eq!(
        (r.bits(9).unwrap(), r.bits(32).unwrap()),
        (3, race::DEFAULT_TRACK)
    );
    let update = nerd::object_update(
        nerd::ROUTER_LOBBY,
        nerd::HANDLE_TRANSFER,
        0,
        0,
        &payload,
        None,
    );
    let o = object(&packet(vec![update], 1));
    assert_eq!((o.form, o.bits), (nerd::HANDLE_TRANSFER, payload.len()));
}

#[test]
fn session_host_follows_the_handshake() {
    let mut lab = Lab::new();
    let a = at(1);
    let reply = lab.exchange(vec![open_connection(nerd::VERSION, 0, 7, 6)], 0, a);
    let own = own_attempt(&reply);
    let ack = nerd::open_response(0, nerd::REASON_RESPONSE_ACK, own, 7);
    let first = lab.exchange(vec![ack], 1, a);
    assert_eq!(ids(&first), vec![nerd::TIME_SYNC, nerd::SYNC_DONE]);
    let (second, third) = (lab.recv(a), lab.recv(a));
    assert_eq!(object(&second).router, nerd::ROUTER_LOBBY);
    assert_eq!(nerd::header(&second).unwrap().ident, 0);
    assert_eq!(nerd::header(&third).unwrap().ident, 1);
    assert_eq!(first_id(&third), nerd::OBJECT_UPDATE);
}

// ---- Two consoles ----

#[test]
fn each_console_gets_the_other_with_its_payload_intact() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.player_create(first, 2);
    let joined = lab.connect(second);
    let creates: Vec<&Vec<u8>> = joined
        .iter()
        .filter(|p| first_id(p) == nerd::OBJECT_CREATE && object(p).router == nerd::ROUTER_PLAYER)
        .collect();
    assert_eq!(creates.len(), 1);
    let o = object(creates[0]);
    assert_eq!((o.form, o.peer), (nerd::HANDLE_RELAY, Some(1)));
    assert_eq!(nerd::payload_of(creates[0], &o).value(), 0b1011);
    lab.receive(&packet(vec![player_create(0, 1, 2)], 2), second);
    let o = object(&lab.recv(first));
    assert_eq!((o.form, o.peer, o.bits), (nerd::HANDLE_RELAY, Some(2), 2));
}

#[test]
fn a_late_joiner_gets_updates_in_order() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.player_create(first, 2);
    let update = nerd::object_update(
        nerd::ROUTER_PLAYER,
        nerd::HANDLE_OWN,
        0,
        1,
        &Writer::new().push(5, 3),
        None,
    );
    lab.receive(&packet(vec![update], 3), first);
    let kinds: Vec<(u8, Option<u8>)> = lab
        .connect(second)
        .iter()
        .map(|p| match messages(p).remove(0) {
            (id, Some(Fields::Object(o))) => (id, Some(o.router)),
            (id, _) => (id, None),
        })
        .collect();
    assert_eq!(
        kinds[kinds.len() - 2..],
        [
            (nerd::OBJECT_CREATE, Some(nerd::ROUTER_PLAYER)),
            (nerd::OBJECT_UPDATE, Some(nerd::ROUTER_PLAYER))
        ]
    );
}

#[test]
fn both_get_one_session_host_and_never_each_others_lobby() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    let lobby = nerd::object_update(
        nerd::ROUTER_LOBBY,
        nerd::HANDLE_OWN,
        0,
        1,
        &Writer::new().push(1, 9),
        None,
    );
    lab.connect(second);
    lab.receive(&packet(vec![lobby], 2), first);
    assert!(lab.drain(second).is_empty());
}

#[test]
fn leaving_deletes_its_objects_for_the_other() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.player_create(first, 2);
    lab.drain(second);
    lab.receive(
        &packet(
            vec![Writer::new().push(u64::from(nerd::CLOSE), nerd::ID_BITS)],
            3,
        ),
        first,
    );
    let data = lab.recv(second);
    assert_eq!(
        (first_id(&data), object(&data).peer),
        (nerd::OBJECT_DELETE, Some(1))
    );
    assert_eq!(lab.race.peers.len(), 1);
}

#[test]
fn launch_only_when_everyone_is_ready() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.ready(first, 5);
    assert!(!lab.race.launched);
    lab.drain(first);
    lab.ready(second, 5);
    assert!(lab.race.launched);
    let data = lab.drain(first).pop().unwrap();
    let mut r = Reader::new(nerd::body(&data));
    assert_eq!(r.bits(nerd::ID_BITS).unwrap(), u64::from(nerd::GAME_LAUNCH));
    let (form, id, _) = nerd::read_handle(&mut r, nerd::LOBBY_ID_BITS).unwrap();
    assert_eq!((form, id), (nerd::HANDLE_OWN, 0));
    assert_eq!(r.bits(32).unwrap(), 1);
    assert_eq!(r.bits(5).unwrap(), 2);
}

#[test]
fn grid_repeats_until_every_console_holds_it_and_after_a_drop() {
    let mut lab = Lab::new();
    let (first, second) = lab.launch_two();
    lab.race.race_tick(100_000);
    lab.race.race_tick(100_500); // not yet a second
    lab.race.race_tick(101_000);
    let grids = lab.grids(first);
    assert_eq!(
        grids.iter().map(|g| g.1.clone()).collect::<Vec<_>>(),
        vec![vec![0, 1], vec![0, 1]]
    );
    assert!(grids[1].0 > grids[0].0);
    lab.console_update(first, 7, 2, &[(8, 1)]);
    lab.console_update(second, 7, 2, &[(8, 1)]);
    lab.drain(first);
    lab.race.race_tick(102_000);
    assert!(lab.grids(first).is_empty());
    // seen live: var 8 drops as the race loads, and that first grid was not enough
    lab.drain(second);
    lab.console_update(first, 8, 3, &[(8, 0)]);
    lab.race.race_tick(103_000);
    assert_eq!(lab.grids(second).len(), 1);
    lab.console_update(first, 9, 4, &[(8, 1), (0, 1)]);
    lab.console_update(second, 9, 4, &[(0, 1)]);
    lab.race.race_tick(104_000);
    assert!(lab.race.grid_done);
}

#[test]
fn participant_var_reaches_the_owner_and_the_others_copy() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.console_of(first);
    lab.console_of(second);
    lab.drain(first);
    lab.drain(second);
    let racers = lab.race.racers();
    lab.race.set_participating(&racers, true);
    let fields: Vec<Object> = lab.drain(first).iter().map(|p| object(p)).collect();
    let own = fields
        .iter()
        .find(|f| f.form == nerd::HANDLE_SERVER)
        .unwrap();
    let other = fields
        .iter()
        .find(|f| f.form == nerd::HANDLE_RELAY)
        .unwrap();
    assert_eq!(
        (own.router, own.seq, own.bits),
        (nerd::ROUTER_CONSOLE, 0, 5)
    );
    // the second's object, one past its Create
    assert_eq!((other.peer, other.seq), (Some(2), 1));
}

#[test]
fn forwards_after_an_injection_stay_newer() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.console_of(first);
    lab.drain(second);
    let racers = lab.race.racers();
    lab.race.set_participating(&racers[..1], true);
    lab.drain(second);
    lab.console_update(first, 5, 1, &[(7, 1)]);
    let o = object(&lab.recv(second));
    // the console's 1 plus one injected
    assert_eq!((o.form, o.seq), (nerd::HANDLE_RELAY, 2));
}

#[test]
fn start_only_after_every_console_is_loaded() {
    let mut lab = Lab::new();
    let (first, second) = lab.launch_two();
    lab.console_update(first, 7, 2, &[(8, 1)]);
    lab.console_update(second, 7, 2, &[(8, 1)]);
    lab.race.race_tick(100_000);
    lab.console_update(first, 8, 3, &[(0, 1)]);
    lab.race.race_tick(101_000);
    assert!(!lab.race.start_sent);
    lab.console_update(second, 8, 3, &[(0, 1)]);
    lab.drain(first);
    let before = lab.race.now_ms();
    lab.race.race_tick(102_000);
    assert!(lab.race.start_sent);
    let data = lab.recv(first);
    let mut r = Reader::new(nerd::body(&data));
    assert_eq!(r.bits(nerd::ID_BITS).unwrap(), u64::from(nerd::START_RACE));
    nerd::read_handle(&mut r, nerd::LOBBY_ID_BITS).unwrap();
    assert!(r.bits(32).unwrap() >= u64::from(before + race::START_DELAY_MS));
    lab.race.race_tick(103_000);
    assert!(lab.drain(first).is_empty());
}

#[test]
fn car_data_gets_the_owner_in_its_handle_and_keeps_every_bit() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.player_create(first, 2);
    lab.drain(second);
    // the pattern (1 << 700) | 0xABCDEF, 702 bits
    let car = own_message(
        nerd::CAR_PREDICTION,
        &[(0xAB_CDEF, 64), (0, 636), (1, 1), (0, 1)],
    );
    lab.receive(&packet(vec![car], 3), first);
    let data = lab.recv(second);
    assert_eq!(nerd::header(&data).unwrap().ident, nerd::NO_SEQ);
    let h = handled(&data);
    assert_eq!((h.form, h.peer), (nerd::HANDLE_RELAY, Some(1)));
    assert_eq!(first_id(&data), nerd::CAR_PREDICTION);
    let mut r = Reader::new(nerd::body(&data));
    r.position = h.after_handle;
    assert_eq!(r.bits(64).unwrap(), 0xAB_CDEF);
    for _ in 0..636 / 53 {
        assert_eq!(r.bits(53).unwrap(), 0);
    }
    assert_eq!((r.bits(1).unwrap(), r.bits(1).unwrap()), (1, 0));
}

#[test]
fn car_data_before_its_player_exists_is_held_back() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    let car = own_message(nerd::CAR_PREDICTION, &[(1, 702)]);
    lab.receive(&packet(vec![car], 3), first);
    assert!(lab.drain(second).is_empty());
}

#[test]
fn a_packet_of_many_messages_stays_aligned() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.player_create(first, 2);
    lab.drain(second);
    let lap = own_message(nerd::LAP_SCORE, &[(0, 8), (0, 8), (0, 8), (7, 32), (0, 2)]);
    let glass = Writer::new().push(16, nerd::ID_BITS).push(5, 356);
    let score = own_message(nerd::IN_PROGRESS_SCORE, &[(9, 32)]);
    lab.receive(&packet(vec![lap, glass, score], 3), first);
    let data = lab.recv(second);
    assert_eq!(ids(&data), vec![35, 16, 34]);
    assert_eq!(lab.race.results.get(&nerd::LAP_SCORE), Some(&1));
}

#[test]
fn layout_table() {
    let lengths = [
        (14, 53),
        (15, 711),
        (16, 356),
        (17, 196),
        (18, 400),
        (19, 432),
        (21, 368),
        (23, 16),
        (24, 128),
        (25, 38),
        (34, 41),
    ];
    let zeros = [0u8; 100];
    for (ident, bits) in lengths {
        let mut r = Reader::new(&zeros);
        nerd::read_one(&mut r, ident).unwrap();
        assert_eq!(r.position, bits, "{}", nerd::name(ident));
    }
}

#[test]
fn abandoned_race_can_launch_again() {
    let mut lab = Lab::new();
    let (first, second) = lab.launch_two();
    lab.console_update(first, 8, 2, &[(7, 0), (4, 0)]);
    assert!(lab.race.launched);
    lab.console_update(second, 8, 2, &[(7, 0), (4, 0)]);
    assert!(!lab.race.launched);
    assert!(!lab.race.grid_done);
    assert!(!lab.race.peers.iter().any(|p| p.ready));
}

#[test]
fn race_end_clears_every_checkmark_and_one_press_launches_again() {
    let mut lab = Lab::new();
    let (first, second) = lab.launch_two();
    lab.drain(first);
    lab.console_update(first, 8, 2, &[(7, 0), (4, 0)]);
    lab.console_update(second, 8, 2, &[(7, 0), (4, 0)]);
    let sent: Vec<u8> = lab.drain(first).iter().map(|p| first_id(p)).collect();
    assert!(sent.contains(&nerd::READY_RESPONSE));
    lab.ready(first, 9);
    assert!(!lab.race.launched);
    lab.ready(second, 9);
    assert!(lab.race.launched);
}

#[test]
fn ready_survives_the_consoles_var_4_changes() {
    let mut lab = Lab::new();
    let (first, _) = lab.launch_two();
    lab.console_update(first, 8, 2, &[(4, 0)]);
    assert!(lab.race.peers.iter().all(|p| p.ready));
}

#[test]
fn unacked_packets_go_again_and_acked_ones_do_not() {
    let mut lab = Lab::new();
    let first = at(1);
    lab.connect(first);
    lab.race.send(first, vec![nerd::start_race(5)], true, 0);
    lab.drain(first);
    let sent = lab.peer(first).in_flight.last().unwrap().sent;
    let later = sent + race::RESEND_MS + 1000;
    lab.race.resend_lost(first, later);
    let again = lab.drain(first);
    assert!(again.iter().any(|p| first_id(p) == nerd::START_RACE));
    let ack = lab
        .peer(first)
        .in_flight
        .iter()
        .map(|f| f.ident)
        .max()
        .unwrap();
    let data = nerd::packet(&[nerd::ping(1)], 9, ack, 0xFFFF_FFFF).unwrap();
    lab.receive(&data, first);
    assert!(lab.peer(first).in_flight.is_empty());
}

#[test]
fn acks_packet_matches_fh2() {
    assert!(nerd::acks_packet(5, 0, 5));
    assert!(nerd::acks_packet(5, 0b10, 3));
    assert!(!nerd::acks_packet(5, 0b10, 4));
    assert!(!nerd::acks_packet(nerd::NO_ACK, 0, nerd::NO_ACK));
}

#[test]
fn ready_gets_no_answer() {
    let mut lab = Lab::new();
    let first = at(1);
    lab.connect(first);
    lab.ready(first, 5);
    assert!(lab.drain(first).is_empty());
}

#[test]
fn one_console_does_not_launch() {
    let mut lab = Lab::new();
    let first = at(1);
    lab.connect(first);
    lab.ready(first, 5);
    assert!(!lab.race.launched);
}

#[test]
fn peers_have_distinct_indexes_from_one() {
    let mut lab = Lab::new();
    lab.connect(at(1));
    lab.connect(at(2));
    let mut indexes: Vec<u8> = lab.race.peers.iter().map(|p| p.index).collect();
    indexes.sort_unstable();
    assert_eq!(indexes, vec![1, 2]);
}

// ---- Infinity: admission, seats, events ----

#[test]
fn a_player_the_admission_refuses_is_not_served() {
    let mut lab = Lab::new();
    lab.gate.denied.lock().unwrap().push(X1);
    let a = at(1);
    lab.receive(
        &packet(vec![open_connection(nerd::VERSION, X1, 7, 6)], 0),
        a,
    );
    assert!(lab.race.peers.is_empty());
    assert!(lab.drain(a).is_empty());
    assert_eq!(lab.race.dropped, 1);
}

#[test]
fn a_revoked_player_is_dropped_at_its_next_packet_and_by_the_tick() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect_as(first, X1);
    lab.connect_as(second, X2);
    lab.race.events.clear();
    lab.gate.denied.lock().unwrap().push(X1);
    lab.receive(&packet(vec![nerd::ping(1)], 2), first);
    assert!(lab.drain(first).is_empty(), "no pong");
    assert_eq!(lab.race.peers.len(), 1);
    lab.gate.denied.lock().unwrap().push(X2);
    lab.race.tick(10);
    assert!(lab.race.peers.is_empty());
    assert_eq!(
        lab.race.events,
        vec![
            GameEvent::PeerLeft {
                xuid: X1,
                engine: "race",
                peer: 1,
                reason: "not_allowed"
            },
            GameEvent::PeerLeft {
                xuid: X2,
                engine: "race",
                peer: 2,
                reason: "not_allowed"
            },
        ]
    );
}

#[test]
fn an_open_waits_for_its_proof_then_is_answered() {
    let mut lab = Lab::new();
    lab.gate.proving.lock().unwrap().push(X1);
    let a = at(1);
    lab.receive(
        &packet(vec![open_connection(nerd::VERSION, X1, 7, 6)], 0),
        a,
    );
    assert!(lab.drain(a).is_empty());
    assert_eq!((lab.race.synced(), lab.race.handshaking()), (0, 1));
    lab.race.tick(100);
    assert_eq!(lab.race.handshaking(), 1, "still waiting");
    lab.gate.proving.lock().unwrap().clear();
    lab.race.tick(200);
    let reply = lab.drain(a);
    assert_eq!(reply.len(), 1);
    assert_eq!(first_id(&reply[0]), nerd::OPEN_RESPONSE);
    assert_eq!(lab.race.peers.len(), 1);
}

#[test]
fn a_held_open_is_forgotten_when_its_hold_ends() {
    let mut lab = Lab::new();
    lab.gate.proving.lock().unwrap().push(X1);
    lab.receive(
        &packet(vec![open_connection(nerd::VERSION, X1, 7, 6)], 0),
        at(1),
    );
    lab.race.tick(race::HOLD_MS + 1);
    assert_eq!(lab.race.handshaking(), 0);
}

#[test]
fn no_seat_past_max_players() {
    let mut lab = Lab::with_seats(1);
    lab.connect(at(1));
    lab.receive(
        &packet(vec![open_connection(nerd::VERSION, 0, 7, 6)], 0),
        at(2),
    );
    assert!(lab.drain(at(2)).is_empty());
    assert_eq!(lab.race.peers.len(), 1);
}

#[test]
fn an_open_naming_another_player_is_not_this_seats() {
    let mut lab = Lab::new();
    let a = at(1);
    lab.connect_as(a, X1);
    lab.receive(
        &packet(vec![open_connection(nerd::VERSION, X2, 7, 6)], 2),
        a,
    );
    assert!(lab.drain(a).is_empty());
    assert_eq!(lab.peer(a).xuid, X1);
}

#[test]
fn sync_and_close_are_events() {
    let mut lab = Lab::new();
    let a = at(1);
    lab.connect_as(a, X1);
    lab.receive(
        &packet(
            vec![Writer::new().push(u64::from(nerd::CLOSE), nerd::ID_BITS)],
            2,
        ),
        a,
    );
    assert_eq!(
        lab.race.events,
        vec![
            GameEvent::PeerSynced {
                xuid: X1,
                engine: "race",
                peer: 1
            },
            GameEvent::PeerLeft {
                xuid: X1,
                engine: "race",
                peer: 1,
                reason: "close"
            },
        ]
    );
}

#[test]
fn a_kick_sends_close_even_with_the_grant_revoked() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    lab.connect_as(first, X1);
    lab.connect_as(second, X2);
    lab.gate.denied.lock().unwrap().push(X1);
    assert_eq!(lab.race.kick(X1, 5), 1);
    let sent = lab.drain(first);
    assert_eq!(sent.len(), 1);
    assert_eq!(ids(&sent[0]), vec![nerd::CLOSE]);
    assert_eq!(lab.race.peers.len(), 1);
}

#[test]
fn a_quiet_console_is_dropped() {
    let mut lab = Lab::new();
    lab.connect(at(1));
    lab.race.tick(race::QUIET_MS);
    assert_eq!(lab.race.peers.len(), 1);
    lab.race.tick(race::QUIET_MS + 1);
    assert!(lab.race.peers.is_empty());
}

#[test]
fn a_race_everybody_left_ends() {
    let mut lab = Lab::new();
    let (first, second) = lab.launch_two();
    for a in [first, second] {
        lab.receive(
            &packet(
                vec![Writer::new().push(u64::from(nerd::CLOSE), nerd::ID_BITS)],
                9,
            ),
            a,
        );
    }
    assert!(!lab.race.launched);
}

#[test]
fn close_all_sends_close_to_every_synced_console() {
    let mut lab = Lab::new();
    lab.connect(at(1));
    lab.receive(
        &packet(vec![open_connection(nerd::VERSION, 0, 7, 6)], 0),
        at(2),
    );
    lab.drain(at(2));
    lab.race.close_all();
    assert_eq!(ids(&lab.drain(at(1))[0]), vec![nerd::CLOSE]);
    assert!(lab.drain(at(2)).is_empty(), "not synced: no Close");
    assert!(lab.race.is_idle());
}

// ---- Infinity: the game module behind the core ----

mod server {
    use serde_json::{Value, json};

    use crate::{
        control::{ConnId, Transport},
        core::{BoundWorld, Config, Io, Profile, Server, SockId, keys::KeyRing},
        net::BindFailure,
    };

    const CONN: ConnId = 1;
    const W1: &str = "4f1c2a6e-1b2d-4c3e-8f40-5a6b7c8d9e01";
    const VM: &str = "sao-1";

    struct NoIo;

    impl Io for NoIo {
        fn bind_world(&mut self, _port: u16) -> Result<BoundWorld, BindFailure> {
            Err(BindFailure::Other)
        }
        fn release_world(&mut self, _sock: SockId) {}
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

    fn server() -> (Server, NoIo) {
        crate::log::mute();
        let mut c = Config::new(Profile::Public);
        let public = ed25519_dalek::SigningKey::from_bytes(&[7; 32]).verifying_key();
        c.keys = KeyRing::parse(&format!(
            "test-k1:{}",
            crate::core::keys::encode_base64(public.as_bytes())
        ))
        .unwrap();
        c.games = crate::registered_games(Default::default());
        c.public_socket = true;
        c.max_seats = 64;
        let mut s = Server::new(c, 1, 0);
        s.connect(CONN, Transport::Unix);
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

    fn open(s: &mut Server, io: &mut NoIo, game: &str, activity: &str, players: u64) -> Value {
        s.request(
            io,
            0,
            CONN,
            &json!({"id": 2, "op": "world.open", "worldId": W1, "game": game,
                "host": {"kind": "official", "vmId": VM}, "activity": activity,
                "maxPlayers": players, "races": true, "requireProof": true}),
        )
    }

    #[test]
    fn an_fm5_race_world_seats_16_and_fh2_still_12() {
        let (mut s, mut io) = server();
        for (game, activity, players) in [
            ("fm5", "race", 17),
            ("fm5", "race", 0),
            ("fh2", "freeroam", 13),
        ] {
            let r = open(&mut s, &mut io, game, activity, players);
            assert_eq!(r["error"]["field"], "maxPlayers", "{game} {players}: {r}");
        }
        let r = open(&mut s, &mut io, "fm5", "freeroam", 16);
        assert_eq!(r["error"]["field"], "activity", "{r}");
        let r = open(&mut s, &mut io, "fm5", "race", 16);
        assert_eq!(r["ok"], true, "{r}");
        // idempotent: the same open answers ok
        let r = open(&mut s, &mut io, "fm5", "race", 16);
        assert_eq!(r["ok"], true, "{r}");
    }

    #[test]
    fn an_fm5_race_world_may_become_private() {
        let (mut s, mut io) = server();
        assert_eq!(open(&mut s, &mut io, "fm5", "race", 16)["ok"], true);
        let r = s.request(
            &mut io,
            0,
            CONN,
            &json!({"id": 3, "op": "world.context", "worldId": W1, "serverTimeMs": 1_798_000_000_000u64,
                "world": {"id": W1, "game": "fm5", "host": {"official": VM}, "private": true}}),
        );
        assert!(r["leaseUntilMs"].is_u64(), "{r}");
        // the context names the world's game
        let r = s.request(
            &mut io,
            0,
            CONN,
            &json!({"id": 4, "op": "world.context", "worldId": W1, "serverTimeMs": 1_798_000_000_000u64,
                "world": {"id": W1, "game": "fh2", "host": {"official": VM}}}),
        );
        assert_eq!(r["error"]["code"], "context_invalid", "{r}");
    }
}

// ---- Infinity: the GameWorld's state ----

#[test]
fn the_state_names_the_race_its_players_and_a_private_leader() {
    use crate::core::game::GameWorld;
    let gate = Arc::new(Gate::default());
    let mut world = super::Fm5World {
        race: Race::new(gate, Rng::new(1), 16),
        activity: Some(super::ACTIVITY),
        is_player: false,
        host_xuid: 0,
        private: false,
    };
    let mut lab_out = Vec::new();
    for (n, x) in [(1, X1), (2, X2)] {
        let a = at(n);
        world.receive(
            &packet(vec![open_connection(nerd::VERSION, x, 7, 6)], 0),
            a,
            0,
            &mut lab_out,
        );
        let own = own_attempt(&lab_out.iter().rfind(|s| s.to == a).unwrap().data);
        let ack = nerd::open_response(0, nerd::REASON_RESPONSE_ACK, own, 7);
        world.receive(&packet(vec![ack], 1), a, n.into(), &mut lab_out);
    }
    let state = Value::Object(world.state());
    assert_eq!(
        state,
        json!({"peers": [X1.to_string(), X2.to_string()], "lobbyPlayers": 2, "activity": "race",
            "engines": {"race": 2}, "hostInWorld": false})
    );
    assert!(world.supports_private());
    world.set_private(&mut lab_out);
    let state = world.state();
    assert_eq!(state["private"], true);
    assert_eq!(state["leader"], X1.to_string(), "the longest present");
    assert_eq!(world.spec(), json!({"activity": "race"}));
    assert_eq!(world.next_tick(crate::core::Profile::Public, 5), Some(25));
}

// ---- W16: the private lobby leader owns the GameOptions (lobby spec §13) ----

/// Every router-6 options message in `packets` (Update 8, Grant 11, Transfer 12), in order, with
/// its payload's bits.
fn options_messages(packets: &[Vec<u8>]) -> Vec<(u8, Object, Writer)> {
    let mut out = Vec::new();
    for data in packets {
        let count = nerd::header(data).unwrap().count;
        let mut r = Reader::new(nerd::body(data));
        for _ in 0..count {
            let ident = r.bits(nerd::ID_BITS).unwrap() as u8;
            let o = match ident {
                nerd::GRANT_OWNERSHIP | nerd::TRANSFER_OWNERSHIP => {
                    nerd::read_ownership(&mut r, ident).unwrap()
                }
                _ => match nerd::read_one(&mut r, ident).unwrap() {
                    Some(Fields::Object(o)) => o,
                    _ => break,
                },
            };
            if o.router == nerd::ROUTER_LOBBY && ident != nerd::OBJECT_CREATE {
                let payload = nerd::payload_of(data, &o);
                out.push((ident, o, payload));
            }
        }
    }
    out
}

/// (message id, seq) of each options message.
fn option_seqs(packets: &[Vec<u8>]) -> Vec<(u8, u8)> {
    options_messages(packets)
        .into_iter()
        .map(|(i, o, _)| (i, o.seq))
        .collect()
}

fn owner_payload(vars: &[(u32, u64, usize)]) -> Writer {
    nerd::vars_payload(vars, nerd::GAME_OPTIONS_INDEX_BITS)
}

impl Lab {
    /// A private lobby: its leader owns the options.
    fn private() -> Lab {
        let mut lab = Lab::new();
        lab.race.give_options_to_leader();
        lab
    }

    /// A GameOptions Update from `from` through the Transfer handle.
    fn options_update(&mut self, from: SocketAddr, ident: u16, seq: u8, payload: &Writer) {
        let m = nerd::object_update(
            nerd::ROUTER_LOBBY,
            nerd::HANDLE_TRANSFER,
            0,
            seq,
            payload,
            None,
        );
        self.receive(&packet(vec![m], ident), from);
    }
}

#[test]
fn grant_and_transfer_golden_vectors() {
    let empty = Writer::new();
    let grant = |seq, p: &Writer| {
        nerd::grant_ownership(nerd::ROUTER_LOBBY, nerd::HANDLE_TRANSFER, 0, seq, p)
    };
    // [11 u6][router 6 u3][a=1 b=0][id u2 0][seq u8][size u14 0]
    assert_eq!(grant(0, &empty).len(), 6 + 3 + 2 + 2 + 8 + 14);
    assert_eq!(grant(0, &empty).bytes(), [0x8B, 0x03, 0x00, 0x00, 0x00]);
    assert_eq!(grant(1, &empty).bytes(), [0x8B, 0x23, 0x00, 0x00, 0x00]);
    // with a payload: var 227 (EEndConditionType) = 2, 9 + 3 bits
    let payload = owner_payload(&[(227, 2, 3)]);
    let g = grant(1, &payload);
    assert_eq!(g.len(), 35 + 12);
    assert_eq!(g.bytes(), [0x8B, 0x23, 0x80, 0x01, 0x18, 0x27]);
    // [12 u6][router 6 u3][a=1 b=0][id u2 0][seq u8], no size, no payload
    let t = nerd::transfer_ownership(nerd::ROUTER_LOBBY, nerd::HANDLE_TRANSFER, 0, 2);
    assert_eq!(t.len(), 6 + 3 + 2 + 2 + 8);
    assert_eq!(t.bytes(), [0x8C, 0x43, 0x00]);
    // and they read back
    let data = packet(vec![g, t], 0);
    let read = options_messages(&[data]);
    assert_eq!(
        read.iter()
            .map(|(i, o, p)| (*i, o.form, o.id, o.seq, p.value()))
            .collect::<Vec<_>>(),
        vec![
            (
                nerd::GRANT_OWNERSHIP,
                nerd::HANDLE_TRANSFER,
                0,
                1,
                payload.value()
            ),
            (nerd::TRANSFER_OWNERSHIP, nerd::HANDLE_TRANSFER, 0, 2, 0),
        ]
    );
}

#[test]
fn options_seqs_start_at_zero_after_0xfe_and_wrap_at_255() {
    let mut peer = Peer::new(at(1), 0, 0);
    assert_eq!(peer.options_seq, 0xFE);
    assert_eq!(peer.take_options_seq(), 0);
    assert_eq!(peer.take_options_seq(), 1);
    peer.options_seq = 253;
    assert_eq!(peer.take_options_seq(), 254);
    assert_eq!(peer.take_options_seq(), 0, "mod 255: 255 is never a seq");
}

#[test]
fn the_leader_is_granted_after_its_first_options_on_the_same_counter() {
    let mut lab = Lab::private();
    let (first, second) = (at(1), at(2));
    let joined = lab.connect(first);
    // SessionHost, then one packet: the full Update at seq 0, the empty grant at seq 1
    assert_eq!(
        option_seqs(&joined),
        vec![(nerd::OBJECT_UPDATE, 0), (nerd::GRANT_OWNERSHIP, 1)]
    );
    let options = options_messages(&joined);
    assert_eq!(options[1].1.bits, 0, "empty after the full Update");
    assert_eq!(
        nerd::header(&joined[2]).unwrap().count,
        2,
        "the grant shares the full Update's packet"
    );
    assert_eq!(lab.race.owner, Some(first));
    // a second console: its own first push, no grant
    assert_eq!(
        option_seqs(&lab.connect(second)),
        vec![(nerd::OBJECT_UPDATE, 0)]
    );
    // the owner's next Update: relayed to the second at its next seq (1); the owner's own seq is
    // its own counter, any is taken
    lab.options_update(first, 5, 40, &owner_payload(&[(228, 3, 32)]));
    assert_eq!(
        option_seqs(&lab.drain(second)),
        vec![(nerd::OBJECT_UPDATE, 1)]
    );
    assert!(lab.drain(first).is_empty(), "not echoed to the owner");
    // Q2: the owner's seq 40 is its send counter; the server's next message to it is still 2
    assert_eq!(lab.peer(first).options_seq, 1);
    lab.race.owner = Some(second);
    lab.race.sync_owner();
    assert_eq!(
        option_seqs(&lab.drain(first)),
        vec![(nerd::GRANT_OWNERSHIP, 2)]
    );
}

#[test]
fn an_owner_update_reaches_each_other_console_at_its_own_next_seq() {
    let mut lab = Lab::private();
    let (first, second, third) = (at(1), at(2), at(3));
    lab.connect(first);
    lab.connect(second);
    lab.connect(third);
    lab.peer(third).options_seq = 253;
    let payload = owner_payload(&[(3, 3, 32), (227, 1, 3), (228, 5, 32)]);
    lab.options_update(first, 5, 1, &payload);
    let to_second = options_messages(&lab.drain(second));
    let to_third = options_messages(&lab.drain(third));
    assert_eq!(to_second.len(), 1);
    assert_eq!(
        (
            to_second[0].0,
            to_second[0].1.form,
            to_second[0].1.id,
            to_second[0].1.seq
        ),
        (nerd::OBJECT_UPDATE, nerd::HANDLE_TRANSFER, 0, 1)
    );
    assert_eq!(to_third[0].1.seq, 254);
    for (_, o, p) in to_second.iter().chain(&to_third) {
        assert_eq!(o.bits, payload.len());
        assert_eq!(p, &payload, "the owner's bits as they came");
    }
    // merged; and no owner path starts a race
    assert_eq!(lab.race.options.value(3), Some(3));
    assert_eq!(lab.race.options.value(228), Some(5));
    assert!(!lab.race.launched);
    lab.options_update(first, 6, 2, &owner_payload(&[(4, 2, 8)]));
    assert_eq!(options_messages(&lab.drain(third))[0].1.seq, 0, "wrapped");
}

#[test]
fn a_non_owners_options_update_is_dropped() {
    let mut lab = Lab::private();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    let before = lab.race.options.clone();
    lab.options_update(second, 5, 1, &owner_payload(&[(3, 3, 32)]));
    assert!(lab.drain(first).is_empty());
    assert!(lab.drain(second).is_empty());
    assert_eq!(lab.race.options, before);
    // an owner's message on router 6 under another handle is dropped too
    let own = nerd::object_update(
        nerd::ROUTER_LOBBY,
        nerd::HANDLE_OWN,
        0,
        1,
        &owner_payload(&[(3, 3, 32)]),
        None,
    );
    lab.receive(&packet(vec![own], 6), first);
    assert!(lab.drain(second).is_empty());
    assert_eq!(lab.race.options, before);
}

#[test]
fn a_public_lobby_keeps_the_options_the_servers() {
    let mut lab = Lab::new();
    let (first, second) = (at(1), at(2));
    let joined = lab.connect(first);
    assert_eq!(option_seqs(&joined), vec![(nerd::OBJECT_UPDATE, 0)]);
    lab.connect(second);
    lab.options_update(first, 5, 1, &owner_payload(&[(3, 3, 32)]));
    assert!(lab.drain(second).is_empty());
    assert_eq!(lab.race.owner, None);
    assert_eq!(lab.race.options.value(3), Some(race::DEFAULT_TRACK));
}

#[test]
fn a_late_joiners_full_update_has_the_owners_changes() {
    let mut lab = Lab::private();
    let (first, late) = (at(1), at(2));
    lab.connect(first);
    lab.options_update(first, 5, 1, &owner_payload(&[(3, 3, 32), (228, 5, 32)]));
    lab.options_update(first, 6, 2, &owner_payload(&[(0, 2, 8), (2, 1, 3)]));
    let options = options_messages(&lab.connect(late));
    assert_eq!(options.len(), 1);
    let (ident, o, payload) = &options[0];
    assert_eq!((*ident, o.seq), (nerd::OBJECT_UPDATE, 0));
    assert_eq!(
        vars_of(payload),
        vec![(0, 2, 8), (2, 1, 3), (3, 3, 32), (227, 1, 3), (228, 5, 32)]
    );
}

#[test]
fn every_var_merges_and_a_bad_write_is_dropped_whole() {
    let mut lab = Lab::private();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    // a bucket var (100, 2 bits) and laps: both merged now
    lab.options_update(first, 5, 1, &owner_payload(&[(100, 2, 2), (228, 7, 32)]));
    assert_eq!(lab.race.options.value(100), Some(2));
    assert_eq!(lab.race.options.value(228), Some(7));
    assert_eq!(
        option_seqs(&lab.drain(second)),
        vec![(nerd::OBJECT_UPDATE, 1)]
    );
    let before = lab.race.options.clone();
    // an index past 290, after a good var: nothing merged, nothing relayed
    lab.options_update(first, 6, 2, &owner_payload(&[(3, 9, 32), (291, 1, 1)]));
    // a payload that ends inside laps
    let cut = Writer::from_bits(&owner_payload(&[(228, 9, 32)]).bytes(), 0, 9 + 20);
    lab.options_update(first, 7, 3, &cut);
    assert_eq!(lab.race.options, before);
    assert!(lab.drain(second).is_empty());
    // and the next good one takes the next seq: no gap
    lab.options_update(first, 8, 4, &owner_payload(&[(3, 9, 32)]));
    assert_eq!(
        option_seqs(&lab.drain(second)),
        vec![(nerd::OBJECT_UPDATE, 2)]
    );
}

#[test]
fn the_next_leader_is_granted_the_full_values_when_the_owner_leaves() {
    let mut lab = Lab::private();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.options_update(first, 5, 1, &owner_payload(&[(228, 4, 32)]));
    lab.drain(second);
    lab.receive(
        &packet(
            vec![Writer::new().push(u64::from(nerd::CLOSE), nerd::ID_BITS)],
            6,
        ),
        first,
    );
    let options = options_messages(&lab.drain(second));
    assert_eq!(options.len(), 1);
    let (ident, o, payload) = &options[0];
    // seq 0 its first push, 1 the relayed Update, 2 the grant
    assert_eq!((*ident, o.seq), (nerd::GRANT_OWNERSHIP, 2));
    assert_eq!(payload, &lab.race.options.full());
    assert_eq!(vars_of(payload)[3], (228, 4, 32));
    assert_eq!(lab.race.owner, Some(second));
    // the new owner's Updates are taken, the old one's never were relayed to it again
    lab.options_update(second, 7, 0, &owner_payload(&[(228, 6, 32)]));
    assert_eq!(lab.race.options.value(228), Some(6));
}

#[test]
fn a_leader_change_revokes_the_old_owner_then_grants_the_new() {
    let mut lab = Lab::private();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    // the second console holds the options while the first leads (a leader change with the
    // old owner still here)
    lab.race.owner = Some(second);
    lab.race.sync_owner();
    let revoked = options_messages(&lab.drain(second));
    assert_eq!(revoked.len(), 1);
    assert_eq!(
        (
            revoked[0].0,
            revoked[0].1.form,
            revoked[0].1.seq,
            revoked[0].1.bits
        ),
        (nerd::TRANSFER_OWNERSHIP, nerd::HANDLE_TRANSFER, 1, 0)
    );
    let granted = options_messages(&lab.drain(first));
    // the first: seq 0 its push, 1 its grant at join, 2 this one, with the full values
    assert_eq!((granted[0].0, granted[0].1.seq), (nerd::GRANT_OWNERSHIP, 2));
    assert_eq!(granted[0].2, lab.race.options.full());
    assert_eq!(lab.race.owner, Some(first));
    // the old owner's Updates are now dropped
    lab.options_update(second, 5, 1, &owner_payload(&[(3, 3, 32)]));
    assert!(lab.drain(first).is_empty());
}

#[test]
fn a_world_made_private_grants_its_leader() {
    use crate::core::game::GameWorld;
    let gate = Arc::new(Gate::default());
    let mut world = super::Fm5World {
        race: Race::new(gate, Rng::new(1), 16),
        activity: Some(super::ACTIVITY),
        is_player: false,
        host_xuid: 0,
        private: false,
    };
    let mut out = Vec::new();
    for (n, x) in [(1, X1), (2, X2)] {
        let a = at(n);
        world.receive(
            &packet(vec![open_connection(nerd::VERSION, x, 7, 6)], 0),
            a,
            0,
            &mut out,
        );
        let own = own_attempt(&out.iter().rfind(|s| s.to == a).unwrap().data);
        let ack = nerd::open_response(0, nerd::REASON_RESPONSE_ACK, own, 7);
        world.receive(&packet(vec![ack], 1), a, n.into(), &mut out);
    }
    out.clear();
    world.set_private(&mut out);
    let to = |a: SocketAddr| -> Vec<Vec<u8>> {
        out.iter()
            .filter(|s| s.to == a)
            .map(|s| s.data.clone())
            .collect()
    };
    let granted = options_messages(&to(at(1)));
    assert_eq!((granted[0].0, granted[0].1.seq), (nerd::GRANT_OWNERSHIP, 1));
    assert!(to(at(2)).is_empty());
    assert_eq!(world.state()["leader"], X1.to_string());
}

/// (index, value, width) of each var of an options payload (values past 64 bits as 0).
fn vars_of(payload: &Writer) -> Vec<(u32, u64, usize)> {
    super::options::parse(payload)
        .unwrap()
        .iter()
        .map(|v| (v.index, v.number().unwrap_or(0), v.value.len()))
        .collect()
}

impl Lab {
    /// The console acks every packet the server has in flight to it.
    fn ack_all(&mut self, from: SocketAddr, ident: u16) {
        let last = self.peer(from).in_flight.iter().map(|f| f.ident).max();
        if let Some(ack) = last {
            let data = nerd::packet(&[nerd::ping(1)], ident, ack, 0xFFFF_FFFF).unwrap();
            self.receive(&data, from);
        }
        assert!(self.peer(from).in_flight.is_empty());
    }
}

/// Every var of GameOptions at once (strings at their longest, 24 chars): about 14 k bits.
fn every_var() -> Writer {
    let mut w = Writer::new();
    for index in 0..super::options::VARS as u32 {
        w.put(u64::from(index), nerd::GAME_OPTIONS_INDEX_BITS);
        match super::options::width(index).unwrap() {
            super::options::Width::Bits(n) => {
                w.put(u64::from(index) & 1, n);
            }
            super::options::Width::String => {
                w.put(24, 5);
                for c in 0..24u64 {
                    w.put(u64::from(b'a') + c, 16);
                }
            }
        }
    }
    w
}

#[test]
fn a_lost_options_packet_goes_again_at_its_seq_and_never_leaves_a_gap() {
    let mut lab = Lab::private();
    let (first, second) = (at(1), at(2));
    lab.connect(first);
    lab.connect(second);
    lab.ack_all(second, 10);
    lab.options_update(first, 5, 1, &owner_payload(&[(3, 3, 32)]));
    // lost: never acked, past the 8 tries other packets get
    let mut seen = Vec::new();
    let mut now = 0;
    while now < race::RESEND_MS * u64::from(race::RESEND_TRIES + 4) {
        now += race::RESEND_MS + 1;
        lab.receive(&packet(vec![nerd::ping(1)], 0x8000), first);
        lab.race.tick(now);
        seen.extend(option_seqs(&lab.drain(second)));
    }
    assert!(seen.len() > race::RESEND_TRIES as usize, "{seen:?}");
    assert!(
        seen.iter().all(|s| *s == (nerd::OBJECT_UPDATE, 1)),
        "{seen:?}"
    );
    // acked at last: the next one is the next seq
    lab.ack_all(second, 11);
    lab.options_update(first, 6, 2, &owner_payload(&[(228, 2, 32)]));
    assert_eq!(
        option_seqs(&lab.drain(second)),
        vec![(nerd::OBJECT_UPDATE, 2)]
    );
}

#[test]
fn grants_updates_and_revokes_interleave_with_contiguous_seqs() {
    let mut lab = Lab::private();
    let (a, b, c) = (at(1), at(2), at(3));
    let mut got: Vec<(SocketAddr, Vec<(u8, u8)>)> = Vec::new();
    for x in [a, b, c] {
        let seqs = option_seqs(&lab.connect(x));
        got.push((x, seqs));
    }
    let collect = |lab: &mut Lab, got: &mut Vec<(SocketAddr, Vec<(u8, u8)>)>| {
        for (x, seqs) in got.iter_mut() {
            seqs.extend(option_seqs(&lab.drain(*x)));
        }
    };
    lab.options_update(a, 5, 9, &owner_payload(&[(3, 3, 32)]));
    lab.options_update(b, 5, 9, &owner_payload(&[(3, 4, 32)])); // not the owner: dropped
    lab.options_update(a, 6, 200, &owner_payload(&[(228, 2, 32)]));
    collect(&mut lab, &mut got);
    // b holds the options while a leads: revoke b, grant a
    lab.race.owner = Some(b);
    lab.race.sync_owner();
    lab.options_update(a, 7, 3, &owner_payload(&[(4, 1, 8)]));
    collect(&mut lab, &mut got);
    // a leaves: b leads and is granted
    lab.receive(
        &packet(
            vec![Writer::new().push(u64::from(nerd::CLOSE), nerd::ID_BITS)],
            8,
        ),
        a,
    );
    lab.options_update(b, 9, 0, &owner_payload(&[(2, 1, 3)]));
    collect(&mut lab, &mut got);
    for (x, seqs) in &got {
        let numbers: Vec<u8> = seqs.iter().map(|s| s.1).collect();
        let want: Vec<u8> = (0..numbers.len() as u8).collect();
        assert_eq!(numbers, want, "{x}: {seqs:?}");
    }
    let kinds = |x: SocketAddr| -> Vec<u8> {
        got.iter()
            .find(|g| g.0 == x)
            .unwrap()
            .1
            .iter()
            .map(|s| s.0)
            .collect()
    };
    let (u, g, t) = (
        nerd::OBJECT_UPDATE,
        nerd::GRANT_OWNERSHIP,
        nerd::TRANSFER_OWNERSHIP,
    );
    assert_eq!(kinds(a), vec![u, g, g]);
    assert_eq!(kinds(b), vec![u, u, u, t, u, g]);
    assert_eq!(kinds(c), vec![u, u, u, u, u]);
}

#[test]
fn no_options_packet_is_longer_than_a_relay_leg_carries() {
    let mut lab = Lab::private();
    let (first, second, late) = (at(1), at(2), at(3));
    lab.connect(first);
    lab.connect(second);
    let all = every_var();
    assert!(all.len() > 13_000, "{}", all.len());
    lab.options_update(first, 5, 1, &all);
    let relayed = lab.drain(second);
    let joined = lab.connect(late);
    // and a leader change hands the whole table over in a grant
    lab.receive(
        &packet(
            vec![Writer::new().push(u64::from(nerd::CLOSE), nerd::ID_BITS)],
            6,
        ),
        first,
    );
    let granted = lab.drain(second);
    for (what, packets) in [("relay", &relayed), ("join", &joined), ("grant", &granted)] {
        for p in packets {
            assert!(
                p.len() <= race::PACKET_MAX_BYTES,
                "{what}: {} bytes",
                p.len()
            );
        }
        let options = options_messages(packets);
        assert!(options.len() > 1, "{what}: cut into several messages");
        let seqs: Vec<u8> = options.iter().map(|o| o.1.seq).collect();
        let first_seq = seqs[0];
        assert!(
            seqs.iter()
                .enumerate()
                .all(|(i, s)| usize::from(*s) == (usize::from(first_seq) + i) % 255),
            "{what}: {seqs:?}"
        );
        let mut vars = Vec::new();
        for (_, _, payload) in &options {
            vars.extend(super::options::parse(payload).unwrap());
        }
        if what == "relay" {
            assert_eq!(
                super::options::encode(&vars),
                all,
                "the owner's bits, in order"
            );
        } else {
            assert_eq!(vars, lab.race.options.vars(), "{what}: the whole table");
        }
    }
    assert_eq!(
        options_messages(&granted).last().unwrap().0,
        nerd::GRANT_OWNERSHIP,
        "the grant carries the table's last part"
    );
    assert_eq!(lab.race.options.vars().len(), super::options::VARS);
}

/// Every router-6 message of `packets` as (id, fields, payload), checking the packet walks to its
/// end exactly (no message reads past its data or leaves bits behind but the byte padding).
fn router6_messages(packets: &[Vec<u8>]) -> Vec<(u8, Object, Writer)> {
    let mut out = Vec::new();
    for data in packets {
        let count = nerd::header(data).unwrap().count;
        let mut r = Reader::new(nerd::body(data));
        let mut whole = true;
        for _ in 0..count {
            let ident = r.bits(nerd::ID_BITS).unwrap() as u8;
            let o = match ident {
                nerd::GRANT_OWNERSHIP | nerd::TRANSFER_OWNERSHIP => {
                    nerd::read_ownership(&mut r, ident).unwrap()
                }
                _ => match nerd::read_one(&mut r, ident).unwrap() {
                    Some(Fields::Object(o)) => o,
                    Some(_) => continue,
                    None => {
                        whole = false;
                        break;
                    }
                },
            };
            if o.router == nerd::ROUTER_LOBBY {
                out.push((ident, o.clone(), nerd::payload_of(data, &o)));
            }
        }
        if whole {
            assert!(
                r.length - r.position < 8,
                "packet ends where its messages do"
            );
        }
    }
    out
}

/// Join, late join, relay, grant and leader change, with a table long enough to be split: every
/// packet each console got.
fn every_options_path() -> Vec<Vec<u8>> {
    let mut lab = Lab::private();
    let (first, second, late) = (at(1), at(2), at(3));
    let mut all = lab.connect(first);
    all.extend(lab.connect(second));
    lab.options_update(first, 5, 1, &every_var());
    lab.options_update(first, 6, 2, &owner_payload(&[(3, 3, 32), (228, 5, 32)]));
    all.extend(lab.connect(late));
    lab.race.owner = Some(second);
    lab.race.sync_owner();
    lab.receive(
        &packet(
            vec![Writer::new().push(u64::from(nerd::CLOSE), nerd::ID_BITS)],
            7,
        ),
        first,
    );
    for x in [first, second, late] {
        all.extend(lab.drain(x));
    }
    all
}

#[test]
fn options_only_ever_go_as_transfer_updates_grants_and_revokes() {
    let messages = router6_messages(&every_options_path());
    let mut kinds = std::collections::BTreeSet::new();
    for (ident, o, _) in &messages {
        if *ident == nerd::OBJECT_CREATE {
            // only the SessionHost, never a GameOptions Create (it would overwrite its slot)
            assert_eq!(
                (o.kind, o.form),
                (Some(nerd::TYPE_SESSION_HOST), nerd::HANDLE_OWN)
            );
            continue;
        }
        assert!(
            matches!(
                *ident,
                nerd::OBJECT_UPDATE | nerd::GRANT_OWNERSHIP | nerd::TRANSFER_OWNERSHIP
            ),
            "{ident}"
        );
        assert_eq!((o.form, o.id), (nerd::HANDLE_TRANSFER, 0), "{ident}");
        kinds.insert(*ident);
    }
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        vec![
            nerd::OBJECT_UPDATE,
            nerd::GRANT_OWNERSHIP,
            nerd::TRANSFER_OWNERSHIP
        ]
    );
    assert!(!race::is_options_message(&nerd::object_create(
        nerd::ROUTER_LOBBY,
        nerd::HANDLE_TRANSFER,
        0,
        1,
        0,
        None,
        None
    )));
}

#[test]
fn each_split_updates_declared_size_is_its_payload_exactly() {
    let messages = router6_messages(&every_options_path());
    let mut split = 0;
    for (ident, o, payload) in &messages {
        if !matches!(*ident, nerd::OBJECT_UPDATE | nerd::GRANT_OWNERSHIP) {
            continue;
        }
        assert_eq!(payload.len(), o.bits);
        let vars = super::options::parse(payload).expect("reads whole");
        assert_eq!(
            super::options::encode(&vars).len(),
            o.bits,
            "no bit past its vars"
        );
        if o.bits > 4096 {
            split += 1;
        }
    }
    assert!(split >= 4, "the long table went split: {split}");
}
