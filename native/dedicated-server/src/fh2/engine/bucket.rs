// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Bucket-list co-op in free roam (router 12, 4 id bits, 2 type bits; contract §7.2 item 4).
//!
//! Each player's own BucketListPlayerStateSynced (type 0, id 0) is relayed, sanitized (failing
//! open to its clean prefix), never owned by anyone else. The server owns the
//! BucketListSlotArraySynced (type 1, id 0): twelve entries of SlotId, ChallengeId, OwnerSyncId,
//! which give the other games their "Join" prompt. A player asks for a partner (63, or its
//! PlayerState's RequestingPartner when no 63 comes within the grace); the first player waiting on
//! the same slot and challenge is its partner (65 to both, carrying the pair's bucket-list
//! universe, 18..23, the same to both). The start time (61) goes to both once both PlayerStates
//! report Ready or Active after the pairing, once per pair, with a fallback after both cancels.
//! A 62 or a leave ends the pair; a 64 ends only a wait. Waiting and paired players stay in
//! universe 0, also while an event phases the others.
//!
//! The status messages 66-68 carry the receiver-relative PlayerState keys first (66 the waiter's;
//! 67 the joiner's who completed the pair, then the waiter's; 68 the ender's, then its partner's),
//! then the challenge, and 68 the ending 62's flag; 66 goes to everyone but the waiter, 67 and 68
//! to everyone. A message one of whose PlayerStates is missing (a leaver's) goes to nobody.
//!
//! Output produced while a packet is handled (65, 61, 66-68 and the slot array's one delta) is
//! staged and sent after that packet's relayed objects ([`Engine::flush_bucket_list`]), so a
//! partner's PlayerState create always precedes the 65 that names it.

use std::{collections::HashMap, sync::Arc};

use serde_json::{Value, json};

use super::{
    Engine, M, Peer,
    bits::Bits,
    dict::DotMap,
    packets::{BUCKET_LIST_ROUTER, BUCKET_LIST_SLOTS_TYPE},
    payload::{self, Fault},
    seams::CAR_UNIVERSE_OFFSET,
    wire::{self, Handle, Msg, Obj, seq_newer, seq_next},
};

pub const ENTRIES: usize = 12;
pub const WAIT_MS: i64 = 120_000;
pub const START_DELAY_MS: u32 = 3000;
/// A player's 63 is taken at most this often; the newest extra one waits for the window.
pub const REQUEST_MIN_MS: i64 = 1000;
/// A raised RequestingPartner is a request when no 63 from that player comes within this long.
pub const REQUEST_GRACE_MS: i64 = 800;
/// A pair not started this long after its pairing is logged once.
pub const START_STALL_MS: i64 = 15_000;
/// The game's BucketListPlayerStateEnum values that make a partner ready for 61.
pub const STATE_READY: u64 = 2;
pub const STATE_ACTIVE: u64 = 3;
/// 61 still goes this long after both partners' cancel (64) when neither reported Ready.
pub const START_FALLBACK_MS: i64 = 10_000;
/// Decoded PlayerState lines per player and wait or pair, and sanitizer refusals per player.
pub const TRACE_LINES: u32 = 12;
pub const NAME_MAX: usize = wire::BUCKET_LIST_NAME_MAX;
/// The game's BucketList1..6 universes.
pub const FIRST_UNIVERSE: u8 = 18;
pub const LAST_UNIVERSE: u8 = 23;
/// A freed pair universe is not handed to another pair this long while another is free.
pub const UNIVERSE_COOL_MS: i64 = 30_000;
/// The most objects a peer keeps (packets.rs MAX_OBJECTS), for the cap note.
const OBJECT_CAP: usize = 256;

type Entry = (i32, i32, i32);

#[derive(Debug)]
struct Player {
    has_state: bool,
    /// The newest PlayerState sequence seen.
    latest: Option<u8>,
    /// Per variable, the sequence its value came from.
    sequences: HashMap<u8, u8>,
    state: u64,
    slot: i32,
    challenge: i32,
    requesting: bool,
    /// The player object's last NetworkName (UTF-16 units), from each relayed router-1 payload.
    name: Option<Vec<u16>>,
    flag_request_at: Option<i64>,
    last_63_at: Option<i64>,
    /// Slot, challenge, since, order.
    waiting: Option<(i32, i32, i64, i64)>,
    pair: Option<u64>,
    last_request_at: Option<i64>,
    pending_request: Option<(i32, i32)>,
    rate_noted: bool,
    repeat_noted: bool,
    early_noted: bool,
    refusal_noted: bool,
    trace: u32,
    refused_trace: u32,
    cap_noted: bool,
}

impl Default for Player {
    fn default() -> Self {
        Player {
            has_state: false,
            latest: None,
            sequences: HashMap::new(),
            state: 0,
            slot: 0,
            challenge: 0,
            requesting: false,
            name: None,
            flag_request_at: None,
            last_63_at: None,
            waiting: None,
            pair: None,
            last_request_at: None,
            pending_request: None,
            rate_noted: false,
            repeat_noted: false,
            early_noted: false,
            refusal_noted: false,
            trace: 0,
            refused_trace: TRACE_LINES,
            cap_noted: false,
        }
    }
}

#[derive(Debug)]
struct Pair {
    first: u64,
    second: u64,
    first_index: u8,
    second_index: u8,
    slot: i32,
    challenge: i32,
    paired_at: i64,
    universe: u8,
    /// Each partner's newest PlayerState sequence at the pairing (or at its recreate).
    floors: Vec<(u64, Option<u8>)>,
    /// The partners that reported Ready or Active after the pairing, in order.
    ready: Vec<u64>,
    started: bool,
    stall_noted: bool,
    cancelled: Vec<u64>,
    cancelled_at: Option<i64>,
    flag_fell: Vec<u64>,
    /// Each partner's last car prediction when 61 went; removed once a newer one is seen.
    cars_at_start: Vec<(u64, Option<(Handle, Bits)>)>,
}

impl Pair {
    fn partners(&self) -> [u64; 2] {
        [self.first, self.second]
    }

    fn floor(&self, serial: u64) -> Option<u8> {
        self.floors
            .iter()
            .find(|(s, _)| *s == serial)
            .and_then(|(_, f)| *f)
    }

    fn set_floor(&mut self, serial: u64, floor: Option<u8>) {
        match self.floors.iter_mut().find(|(s, _)| *s == serial) {
            Some(f) => f.1 = floor,
            None => self.floors.push((serial, floor)),
        }
    }
}

/// The world's bucket-list state.
#[derive(Debug, Default)]
pub(super) struct Bucket {
    players: DotMap<u64, Player>,
    pairs: HashMap<u64, Pair>,
    next_pair: u64,
    slots: [Entry; ENTRIES],
    /// The slot array as the players have it (the last delta sent): joiners get this.
    slots_sent: [Entry; ENTRIES],
    slots_sequence: u8,
    outbox: Vec<(u64, M)>,
    order: i64,
    /// When each pair universe was last freed.
    universes_freed: HashMap<u8, i64>,
    /// Players whose wait or pair began or ended, settled at the flush (first-seen order).
    universe_due: Vec<u64>,
}

fn entry_variables(slots: &[Entry; ENTRIES], i: usize) -> [(u8, i32); 3] {
    let i8 = i as u8;
    [
        (3 * i8, slots[i].0),
        (3 * i8 + 1, slots[i].1),
        (3 * i8 + 2, slots[i].2),
    ]
}

fn fault_name(f: &Fault) -> &'static str {
    match f {
        Fault::Router(_) => "Router",
        Fault::Index(_) => "Index",
        Fault::Duplicate(_) => "Duplicate",
        Fault::NotFinite(_) => "NotFinite",
        Fault::NameTooLong(_) => "NameTooLong",
        Fault::Truncated => "Truncated",
        Fault::TooBig(_) => "TooBig",
        Fault::OutOfRange(_) => "OutOfRange",
    }
}

fn upper_hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02X}")).collect()
}

fn ready_state(state: u64) -> bool {
    state == STATE_READY || state == STATE_ACTIVE
}

/// The universe a car prediction says its car is in (bits 395-399); `None` without one.
fn car_universe(car: Option<&(Handle, Bits)>) -> Option<u64> {
    let (_, body) = car?;
    let width = payload::PRIVATE_UNIVERSE_BITS;
    if body.len() < CAR_UNIVERSE_OFFSET + width as usize {
        return None;
    }
    let mut r = body.reader();
    r.skip(CAR_UNIVERSE_OFFSET).ok()?;
    r.read(width).ok()
}

fn opt_text(v: Option<u64>) -> String {
    v.map_or(String::new(), |u| u.to_string())
}

/// 65 for one partner: the other's PlayerState as a relayed key, the pair's universe, found, the
/// other's name (0x20-0x7E and 0xA0-0xFF kept, anything else `?`, at most 36 bytes).
fn partner_message(partner_index: u8, universe: u8, name: &[u16]) -> Bits {
    let mut b = Bits::new();
    b.push(65, 7);
    Handle::relay(0, partner_index).write(&mut b, 7);
    let text: Vec<u8> = name
        .iter()
        .take(NAME_MAX)
        .map(|&c| {
            if (0x20..=0x7e).contains(&c) || (0xa0..=0xff).contains(&c) {
                c as u8
            } else {
                b'?'
            }
        })
        .collect();
    b.push(u64::from(universe), 5)
        .boolean(true)
        .byte_string(&text);
    b
}

fn start_message(time: u32) -> Bits {
    let mut b = Bits::new();
    b.push(61, 7).push(u64::from(time), 32);
    b
}

/// A PlayerState key as `receiver` knows it: its own as an own key, anyone else's as a relay key.
fn key_for(receiver: u64, owner: u64, owner_index: u8) -> Handle {
    if receiver == owner {
        Handle::own(0)
    } else {
        Handle::relay(0, owner_index)
    }
}

impl Engine {
    /// Bucket-list co-op is free roam's: no car meet or Road Trip world serves it.
    pub(super) fn bucket_list_world(&self) -> bool {
        self.settings.car_meet_lobby.is_none() && self.kind != super::EngineKind::RoadTrip
    }

    /// A waiting or paired player keeps universe 0.
    pub(super) fn bucket_list_busy(&self, serial: u64) -> bool {
        self.bucket
            .players
            .get(&serial)
            .is_some_and(|p| p.waiting.is_some() || p.pair.is_some())
    }

    /// Whether a running pair holds this universe (pairs use 18..23 and phasing 1..17: a guard).
    pub(super) fn bucket_list_universe_held(&self, universe: u8) -> bool {
        universe != 0 && self.bucket.pairs.values().any(|p| p.universe == universe)
    }

    /// A paired player's car universe (its pair's); `None` when not paired.
    pub(super) fn bucket_list_pair_universe(&self, serial: u64) -> Option<u8> {
        let pair = self.bucket.players.get(&serial)?.pair?;
        self.bucket
            .pairs
            .get(&pair)
            .map(|p| p.universe)
            .filter(|u| *u != 0)
    }

    fn bucket_player(&mut self, serial: u64) -> &mut Player {
        if !self.bucket.players.contains_key(&serial) {
            self.bucket.players.insert(serial, Player::default());
        }
        self.bucket
            .players
            .get_mut(&serial)
            .expect("inserted above")
    }

    fn bucket_index(&self, serial: u64) -> u8 {
        self.peer(serial).map_or(0, |p| p.index)
    }

    fn bucket_synced(&self, serial: u64) -> bool {
        self.peer(serial).is_some_and(|p| p.synced)
    }

    /// The server-owned slot array (router 12, type 1, id 0) of a joiner's snapshot: each used
    /// entry's three variables, ascending; an empty table is an empty create.
    pub(super) fn append_bucket_list_snapshot(&self, snapshot: &mut Vec<Bits>) {
        if !self.bucket_list_world() {
            return;
        }
        let vars: Vec<(u8, i32)> = (0..ENTRIES)
            .filter(|i| self.bucket.slots_sent[*i] != (0, 0, 0))
            .flat_map(|i| entry_variables(&self.bucket.slots_sent, i))
            .collect();
        snapshot.push(wire::write(&Msg::Object(Obj {
            create: true,
            router: BUCKET_LIST_ROUTER,
            handle: Handle::own(0),
            ty: BUCKET_LIST_SLOTS_TYPE,
            seq: self.bucket.slots_sequence,
            payload: payload::bucket_list_slots(&vars),
        })));
    }

    fn bucket_send(&mut self, to: u64, m: Bits) {
        self.bucket.outbox.push((to, Arc::new(m)));
    }

    fn bucket_universe_due(&mut self, serial: u64) {
        if !self.bucket.universe_due.contains(&serial) {
            self.bucket.universe_due.push(serial);
        }
    }

    /// A waiting or paired player is at universe 0; one that no longer is gets a private universe
    /// while phasing, as everybody else (run at the flush).
    fn bucket_list_universe(&mut self, serial: u64) {
        let Some(p) = self.peer(serial) else { return };
        let (universe, due) = (p.universe, p.universe_due);
        if self.bucket_list_busy(serial) {
            if universe == 0 {
                return;
            }
            if due {
                if let Some(p) = self.peer_mut(serial) {
                    p.universe = 0;
                }
            } else {
                self.set_universe(serial, 0);
            }
        } else if self.ev.phasing && universe == 0 {
            if !due {
                self.assign_universe(serial);
            } else if let Some(u) = self.free_universe()
                && let Some(p) = self.peer_mut(serial)
            {
                p.universe = u;
            }
        }
    }

    /// Whether every game has `subject`'s PlayerState: its own, or one relayed to it (live or in
    /// its join snapshot; router 12 is never held for a joiner). A leaver's is gone everywhere.
    fn bucket_state_known(&self, subject: u64) -> bool {
        self.peer(subject)
            .is_some_and(|p| p.objects.contains_key(&(BUCKET_LIST_ROUTER, 0)))
    }

    /// A status message (66 waiting: `a` the waiter; 67 started: `a` the joiner who completed the
    /// pair, `b` the waiter; 68 ended: `a` the ender, `b` its partner, then the ending 62's flag),
    /// the keys relative to each receiver, then the challenge, to every synced receiver but
    /// `except`. When a key's PlayerState is missing, no receiver gets it and the skipped receivers
    /// are noted once per message.
    fn bucket_list_status(
        &mut self,
        kind: u8,
        challenge: i32,
        a: (u64, u8),
        b: Option<(u64, u8)>,
        flag: bool,
        except: Option<u64>,
    ) {
        let known =
            self.bucket_state_known(a.0) && b.is_none_or(|(s, _)| self.bucket_state_known(s));
        let receivers: Vec<u64> = self
            .synced_serials_except(None)
            .into_iter()
            .filter(|s| Some(*s) != except)
            .collect();
        let mut skipped = Vec::new();
        for r in receivers {
            if !known {
                skipped.push(self.bucket_index(r));
                continue;
            }
            let mut m = Bits::new();
            m.push(u64::from(kind), 7);
            for (owner, index) in std::iter::once(a).chain(b) {
                key_for(r, owner, index).write(&mut m, 7);
            }
            m.push(u64::from(challenge as u32), 32);
            if kind == 68 {
                m.boolean(flag);
            }
            self.bucket_send(r, m);
        }
        if !skipped.is_empty() {
            let subjects: Vec<u8> = std::iter::once(a.1).chain(b.map(|x| x.1)).collect();
            self.note(
                "bucket.status_skipped",
                vec![json!(kind), json!(skipped), json!(subjects)],
            );
        }
    }

    /// The staged output: the players' universes, the slot array's one delta (only the variables
    /// that differ from what was sent), then the messages.
    pub(super) fn flush_bucket_list(&mut self) {
        if !self.bucket.universe_due.is_empty() {
            let due = std::mem::take(&mut self.bucket.universe_due);
            for s in due {
                self.bucket_list_universe(s);
            }
        }
        let changed: Vec<(u8, i32)> = (0..ENTRIES)
            .flat_map(|i| {
                let now = entry_variables(&self.bucket.slots, i);
                let sent = entry_variables(&self.bucket.slots_sent, i);
                now.into_iter()
                    .zip(sent)
                    .filter(|(a, b)| a.1 != b.1)
                    .map(|(a, _)| a)
                    .collect::<Vec<_>>()
            })
            .collect();
        if !changed.is_empty() {
            self.bucket.slots_sent = self.bucket.slots;
            self.bucket.slots_sequence = seq_next(self.bucket.slots_sequence);
            let update: M = Arc::new(wire::write(&Msg::Object(Obj {
                create: false,
                router: BUCKET_LIST_ROUTER,
                handle: Handle::own(0),
                ty: 0,
                seq: self.bucket.slots_sequence,
                payload: payload::bucket_list_slots(&changed),
            })));
            for s in self.synced_serials_except(None) {
                self.send_sequenced(s, vec![update.clone()]);
            }
        }
        if self.bucket.outbox.is_empty() {
            return;
        }
        let outbox = std::mem::take(&mut self.bucket.outbox);
        for (to, m) in outbox {
            if self.bucket_synced(to) {
                self.send_sequenced(to, vec![m]);
            }
        }
    }

    fn free_bucket_entries(&mut self, owners: &[u8]) {
        for e in self.bucket.slots.iter_mut() {
            if e.2 != 0 && owners.iter().any(|o| i32::from(*o) == e.2) {
                *e = (0, 0, 0);
            }
        }
    }

    fn claim_bucket_entry(&mut self, serial: u64, slot: i32, challenge: i32) {
        let index = self.bucket_index(serial);
        if slot == 0 {
            self.note(
                "bucket.no_entry",
                vec![json!(index), json!(slot), json!(challenge)],
            );
            return;
        }
        match self.bucket.slots.iter().position(|e| e.2 == 0) {
            Some(free) => self.bucket.slots[free] = (slot, challenge, i32::from(index)),
            None => self.note("bucket.entries_full", vec![json!(ENTRIES), json!(index)]),
        }
    }

    /// A router-12 object refused (any other shape, or any in a world without co-op): noted
    /// every time in free roam, once per peer outside it or after the object cap.
    pub(super) fn bucket_list_refused(&mut self, serial: u64, obj: &Obj) {
        let world = self.bucket_list_world();
        let index = self.bucket_index(serial);
        let player = self.bucket_player(serial);
        if (!world || player.cap_noted) && player.refusal_noted {
            return;
        }
        player.refusal_noted = true;
        self.note(
            "bucket.object_refused",
            vec![
                json!(index),
                json!(obj.router),
                json!(if obj.create { "True" } else { "False" }),
                json!(obj.ty),
                json!(format!("{:?}", obj.handle.kind)),
                json!(obj.handle.id),
                json!(obj.payload.len()),
            ],
        );
    }

    /// A PlayerState the sanitizer refused: its clean prefix, re-encoded, to relay instead (fail
    /// open), or `None`: dropped. Bounded notes per player.
    pub(super) fn bucket_list_sanitize_refused(
        &mut self,
        serial: u64,
        obj: &Obj,
        fault: Fault,
    ) -> Option<Bits> {
        let prefix = if obj.payload.len() <= payload::BUCKET_LIST_PLAYER_MAX_BITS {
            let clean = payload::bucket_list_player_of(&obj.payload);
            (!clean.is_empty()).then(|| payload::bucket_list_player(&clean))
        } else {
            None
        };
        let index = self.bucket_index(serial);
        let player = self.bucket_player(serial);
        if player.refused_trace > 0 {
            player.refused_trace -= 1;
            // the hex is safe to log: a PlayerState (at most 128 bits) holds numbers and flags
            let mut args: Vec<Value> = vec![
                json!(index),
                json!(if obj.create { "create" } else { "update" }),
                json!(obj.seq),
                json!(fault_name(&fault)),
                json!(fault.value()),
                json!(obj.payload.len()),
                json!(upper_hex(obj.payload.bytes())),
            ];
            match &prefix {
                Some(p) => {
                    args.push(json!(p.len()));
                    self.note("bucket.sanitizer_prefix", args);
                }
                None => self.note("bucket.sanitizer_dropped", args),
            }
        }
        prefix
    }

    /// A PlayerState create refused at the object cap: noted once per peer.
    pub(super) fn bucket_list_capped(&mut self, serial: u64) {
        let index = self.bucket_index(serial);
        let player = self.bucket_player(serial);
        if player.cap_noted {
            return;
        }
        player.cap_noted = true;
        player.refusal_noted = false; // the refusals that follow get one note of their own
        self.note("bucket.object_cap", vec![json!(index), json!(OBJECT_CAP)]);
    }

    /// A relayed router-1 payload: the NetworkName it carries, if any.
    pub(super) fn bucket_list_player_object(&mut self, serial: u64, payload: &Bits) {
        if !self.bucket_list_world() {
            return;
        }
        if let Some(name) = payload::player_name(payload) {
            self.bucket_player(serial).name = Some(name);
        }
    }

    /// A player's own PlayerState, sanitized and accepted: each variable takes a value only from
    /// a sequence newer than its last one; a State Ready/Active carried by an update newer than
    /// the pairing makes the partner ready.
    pub(super) fn bucket_list_player_update(
        &mut self,
        serial: u64,
        create: bool,
        seq: u8,
        payload: &Bits,
    ) {
        let index = self.bucket_index(serial);
        let variables = payload::bucket_list_player_of(payload);
        let pair_id = self.bucket.players.get(&serial).and_then(|p| p.pair);
        let mut trace_note = None;
        let (raised_check, dropped, reports_ready) = {
            let pairs = &mut self.bucket.pairs;
            if !self.bucket.players.contains_key(&serial) {
                self.bucket.players.insert(serial, Player::default());
            }
            let player = self
                .bucket
                .players
                .get_mut(&serial)
                .expect("inserted above");
            if create {
                player.state = 0;
                player.slot = 0;
                player.challenge = 0;
                player.requesting = false;
                player.sequences.clear();
                player.latest = Some(seq);
                // a recreated PlayerState numbers afresh: the pair's floor moves to this create
                if let Some(pid) = pair_id
                    && let Some(pair) = pairs.get_mut(&pid)
                    && !pair.started
                {
                    pair.set_floor(serial, Some(seq));
                }
            } else if player.latest.is_none_or(|l| seq_newer(seq, l)) {
                player.latest = Some(seq);
            }
            player.has_state = true;
            let mut reports_ready = false;
            let (was_requesting, old_slot, old_challenge) =
                (player.requesting, player.slot, player.challenge);
            let mut raises = false;
            if (player.waiting.is_some() || player.pair.is_some()) && player.trace > 0 {
                player.trace -= 1;
                let text: Vec<String> = variables.iter().map(|(v, x)| format!("{v}={x}")).collect();
                trace_note = Some(vec![
                    json!(index),
                    json!(if create { "create" } else { "update" }),
                    json!(seq),
                    json!(if player.pair.is_some() {
                        "paired"
                    } else {
                        "waiting"
                    }),
                    json!(text.join(" ")),
                ]);
            }
            for (var, value) in &variables {
                if usize::from(*var) == payload::BUCKET_LIST_STATE_VARIABLE && ready_state(*value) {
                    reports_ready = true;
                }
                if player
                    .sequences
                    .get(var)
                    .is_some_and(|last| !seq_newer(seq, *last))
                {
                    continue;
                }
                player.sequences.insert(*var, seq);
                match *var {
                    payload::BUCKET_LIST_SLOT_VARIABLE => player.slot = *value as u32 as i32,
                    payload::BUCKET_LIST_CHALLENGE_VARIABLE => {
                        player.challenge = *value as u32 as i32
                    }
                    payload::BUCKET_LIST_REQUESTING_VARIABLE => {
                        player.requesting = *value != 0;
                        raises |= *value != 0;
                    }
                    v if usize::from(v) == payload::BUCKET_LIST_STATE_VARIABLE => {
                        player.state = *value
                    }
                    _ => {}
                }
            }
            let raised = player.requesting
                && (raises || player.slot != old_slot || player.challenge != old_challenge);
            let dropped = !raised && was_requesting && !player.requesting;
            (raised, dropped, reports_ready)
        };
        if let Some(args) = trace_note {
            self.note("bucket.trace", args);
        }
        if raised_check {
            self.bucket_list_flag_raised(serial);
        } else if dropped {
            self.bucket_list_flag_dropped(serial);
        }
        if !reports_ready {
            return;
        }
        let Some(pid) = self.bucket.players.get(&serial).and_then(|p| p.pair) else {
            return;
        };
        let Some(pair) = self.bucket.pairs.get_mut(&pid) else {
            return;
        };
        if pair.started {
            return;
        }
        if !create
            && let Some(floor) = pair.floor(serial)
            && !seq_newer(seq, floor)
        {
            return;
        }
        if !pair.ready.contains(&serial) {
            pair.ready.push(serial);
            if pair.ready.len() == 2 {
                self.start_bucket_pair(pid, false);
            }
        }
    }

    /// RequestingPartner raised (or the slot/challenge moved under it): a request once the grace
    /// passes without a 63. Nothing for what is already being served.
    fn bucket_list_flag_raised(&mut self, serial: u64) {
        let now = self.now;
        let pair = self
            .bucket
            .players
            .get(&serial)
            .and_then(|p| p.pair)
            .and_then(|id| self.bucket.pairs.get(&id))
            .map(|p| (p.slot, p.challenge));
        let Some(player) = self.bucket.players.get_mut(&serial) else {
            return;
        };
        let served = player.waiting.is_some_and(|(s, c, at, _)| {
            s == player.slot && c == player.challenge && now - at < WAIT_MS
        }) || pair.is_some_and(|(s, c)| s == player.slot && c == player.challenge);
        if served {
            player.flag_request_at = None;
            return;
        }
        if player
            .last_63_at
            .is_some_and(|at| now - at < REQUEST_GRACE_MS)
        {
            return; // a 63 came: it wins
        }
        player.flag_request_at.get_or_insert(now);
    }

    /// RequestingPartner fell to 0: a pending flag request is dropped; before a pair the wait ends
    /// (as 64); a pair stays.
    fn bucket_list_flag_dropped(&mut self, serial: u64) {
        let Some(player) = self.bucket.players.get_mut(&serial) else {
            return;
        };
        player.flag_request_at = None;
        if let Some(pid) = player.pair {
            if let Some(pair) = self.bucket.pairs.get_mut(&pid)
                && !pair.flag_fell.contains(&serial)
            {
                pair.flag_fell.push(serial);
            }
            return;
        }
        if player.waiting.is_none() {
            return;
        }
        player.last_request_at = None;
        player.last_63_at = None;
        player.pending_request = None;
        self.cancel_bucket_wait(serial, "RequestingPartner fell to 0");
    }

    /// The player deleted its own PlayerState: its wait ends; a pair stays.
    pub(super) fn bucket_list_player_deleted(&mut self, serial: u64) {
        let Some(player) = self.bucket.players.get_mut(&serial) else {
            return;
        };
        player.has_state = false;
        player.latest = None;
        player.sequences.clear();
        player.flag_request_at = None;
        player.requesting = false;
        player.slot = 0;
        player.challenge = 0;
        if player.waiting.is_some() {
            self.cancel_bucket_wait(serial, "its PlayerState was deleted");
        }
    }

    /// 62, 63 and 64 from a player.
    pub(super) fn bucket_list_message(&mut self, serial: u64, kind: u8, body: &Bits) {
        let mut r = body.reader();
        match kind {
            63 => {
                let now = self.now;
                let player = self.bucket_player(serial);
                player.last_63_at = Some(now);
                player.flag_request_at = None; // the 63 wins over a flag request inside the grace
                let slot = r.read(32).unwrap_or(0) as u32 as i32;
                let challenge = r.read(32).unwrap_or(0) as u32 as i32;
                self.request_bucket_partner(serial, slot, challenge);
            }
            64 => self.cancel_bucket_request(serial),
            62 => {
                let completed = r.boolean().unwrap_or(false);
                self.bucket_challenge_ended(serial, completed);
            }
            _ => {}
        }
    }

    /// A request (63, the flag, or a pending one): the first player waiting on the same slot and
    /// challenge is the partner; else this one waits.
    fn request_bucket_partner(&mut self, serial: u64, slot: i32, challenge: i32) {
        let now = self.now;
        let index = self.bucket_index(serial);
        let pair = self
            .bucket
            .players
            .get(&serial)
            .and_then(|p| p.pair)
            .and_then(|id| {
                self.bucket
                    .pairs
                    .get(&id)
                    .map(|p| (id, p.slot, p.challenge))
            });
        let player = self.bucket_player(serial);
        if !player.has_state {
            let noted = player.early_noted;
            player.early_noted = true;
            if !noted {
                self.note(
                    "bucket.early",
                    vec![json!(index), json!(slot), json!(challenge)],
                );
            }
            return;
        }
        let repeat = player
            .waiting
            .is_some_and(|(s, c, at, _)| s == slot && c == challenge && now - at < WAIT_MS)
            || pair.is_some_and(|(_, s, c)| s == slot && c == challenge);
        if repeat {
            player.pending_request = None; // the game's latest request is the one already served
            let noted = player.repeat_noted;
            player.repeat_noted = true;
            if !noted {
                self.note(
                    "bucket.repeat",
                    vec![json!(index), json!(slot), json!(challenge)],
                );
            }
            return;
        }
        if player
            .last_request_at
            .is_some_and(|at| now - at < REQUEST_MIN_MS)
        {
            // kept as the newest request, applied when the window passes
            player.pending_request = Some((slot, challenge));
            let noted = player.rate_noted;
            player.rate_noted = true;
            if !noted {
                self.note("bucket.rate", vec![json!(index), json!(REQUEST_MIN_MS)]);
            }
            return;
        }
        player.last_request_at = Some(now);
        player.pending_request = None;
        player.rate_noted = false;
        player.repeat_noted = false;
        let waiting = player.waiting.is_some();
        if let Some((pid, _, _)) = pair {
            self.end_bucket_pair(
                pid,
                &format!("peer {index} asks for another challenge"),
                serial,
                false,
            );
        }
        if waiting {
            self.cancel_bucket_wait(serial, "a new request replaces it");
        }
        let waiter = self
            .bucket
            .players
            .iter()
            .filter(|(s, p)| {
                **s != serial
                    && p.pair.is_none()
                    && p.waiting.is_some_and(|(ws, wc, at, _)| {
                        ws == slot && wc == challenge && now - at < WAIT_MS
                    })
            })
            .map(|(s, p)| (*s, p.waiting.map_or(0, |w| w.3)))
            .filter(|(s, _)| self.bucket_synced(*s))
            .min_by_key(|(_, order)| *order)
            .map(|(s, _)| s);
        if let Some(w) = waiter {
            if let Some(universe) = self.bucket_list_pick_universe() {
                self.pair_bucket_players(w, serial, slot, challenge, universe);
                return;
            }
            // a guard (12 players make at most 6 pairs): no 65, never a shared universe; this
            // player waits too and the next request pairs them
            let wi = self.bucket_index(w);
            self.note(
                "bucket.not_paired",
                vec![
                    json!(wi),
                    json!(index),
                    json!(LAST_UNIVERSE - FIRST_UNIVERSE + 1),
                ],
            );
        }
        let order = self.bucket.order;
        self.bucket.order += 1;
        let player = self.bucket_player(serial);
        player.waiting = Some((slot, challenge, now, order));
        player.trace = TRACE_LINES;
        self.bucket_universe_due(serial); // a waiter is at universe 0
        self.claim_bucket_entry(serial, slot, challenge);
        // to everyone but the waiter
        self.bucket_list_status(66, challenge, (serial, index), None, false, Some(serial));
        self.note(
            "bucket.waiting",
            vec![json!(index), json!(slot), json!(challenge)],
        );
    }

    fn pair_bucket_players(
        &mut self,
        first: u64,
        second: u64,
        slot: i32,
        challenge: i32,
        universe: u8,
    ) {
        let (first_index, second_index) = (self.bucket_index(first), self.bucket_index(second));
        let id = self.bucket.next_pair;
        self.bucket.next_pair += 1;
        let mut pair = Pair {
            first,
            second,
            first_index,
            second_index,
            slot,
            challenge,
            paired_at: self.now,
            universe,
            floors: Vec::new(),
            ready: Vec::new(),
            started: false,
            stall_noted: false,
            cancelled: Vec::new(),
            cancelled_at: None,
            flag_fell: Vec::new(),
            cars_at_start: Vec::new(),
        };
        for s in [first, second] {
            let player = self.bucket_player(s);
            player.waiting = None;
            player.pair = Some(id);
            player.trace = TRACE_LINES;
            let latest = player.latest;
            pair.set_floor(s, latest);
            // the relay's own phasing keeps the partners at 0: the game moves them itself
            self.bucket_universe_due(s);
        }
        self.bucket.pairs.insert(id, pair);
        // the first waiter's entry is freed at the pairing (a full pair shows no "Join")
        self.free_bucket_entries(&[first_index]);
        for (receiver, partner, partner_index) in
            [(first, second, second_index), (second, first, first_index)]
        {
            let name = self
                .bucket
                .players
                .get(&partner)
                .and_then(|p| p.name.clone())
                .unwrap_or_default();
            let message = partner_message(partner_index, universe, &name);
            let bits = message.len();
            self.bucket_send(receiver, message);
            let known = self
                .peer(partner)
                .and_then(|p| p.objects.get(&(BUCKET_LIST_ROUTER, 0)))
                .map_or("no".to_string(), |h| {
                    format!("yes (its sequence {})", h.last)
                });
            let receiver_index = self.bucket_index(receiver);
            self.note(
                "bucket.partner_message",
                vec![
                    json!(65),
                    json!(receiver_index),
                    json!(bits),
                    json!(partner_index),
                    json!(universe),
                    json!(name.len().min(NAME_MAX)),
                    json!(known),
                ],
            );
        }
        // A: the player who joined (completed the pair), B: the waiter
        self.bucket_list_status(
            67,
            challenge,
            (second, second_index),
            Some((first, first_index)),
            false,
            None,
        );
        self.note(
            "bucket.paired",
            vec![
                json!(first_index),
                json!(second_index),
                json!(slot),
                json!(challenge),
                json!(universe),
                json!(STATE_READY),
                json!(STATE_ACTIVE),
                json!(START_FALLBACK_MS),
            ],
        );
    }

    /// The highest of 18..23 that no pair uses, no peer holds and no pair left in the last 30 s;
    /// when all free ones are that fresh, the least recently freed; `None` when all are in use.
    fn bucket_list_pick_universe(&self) -> Option<u8> {
        let taken: Vec<u8> = self
            .bucket
            .pairs
            .values()
            .map(|p| p.universe)
            .chain(self.peers.iter().map(|p| p.universe))
            .collect();
        let free: Vec<u8> = (FIRST_UNIVERSE..=LAST_UNIVERSE)
            .rev()
            .filter(|u| !taken.contains(u))
            .collect();
        let freed_at = |u: u8| self.bucket.universes_freed.get(&u).copied();
        let cooling = |u: u8| freed_at(u).is_some_and(|at| self.now - at < UNIVERSE_COOL_MS);
        if let Some(u) = free.iter().copied().find(|u| !cooling(*u)) {
            return Some(u);
        }
        // all cooling: the least recently freed (the first of equals, highest first)
        let mut best: Option<(u8, i64)> = None;
        for u in free {
            let at = freed_at(u).unwrap_or(0);
            if best.is_none_or(|(_, b)| at < b) {
                best = Some((u, at));
            }
        }
        best.map(|(u, _)| u)
    }

    fn start_bucket_pair(&mut self, pid: u64, fallback: bool) {
        let cars: Vec<(u64, Option<(Handle, Bits)>)> = {
            let Some(pair) = self.bucket.pairs.get(&pid) else {
                return;
            };
            pair.partners()
                .iter()
                .map(|s| (*s, self.peer(*s).and_then(|p| p.last_car.clone())))
                .collect()
        };
        let Some(pair) = self.bucket.pairs.get_mut(&pid) else {
            return;
        };
        pair.started = true;
        pair.cars_at_start = cars;
        let (first, second, fi, si) =
            (pair.first, pair.second, pair.first_index, pair.second_index);
        let mut start = self.now_wire().wrapping_add(START_DELAY_MS);
        if start == 0 {
            start = 1; // 0 is "no start time"
        }
        self.bucket_send(first, start_message(start));
        self.bucket_send(second, start_message(start));
        let id = if fallback {
            "bucket.start_fallback"
        } else {
            "bucket.start"
        };
        self.note(id, vec![json!(fi), json!(si), json!(START_DELAY_MS)]);
    }

    /// The pair still runs on both sides: both partners here, both still in it, and not both with
    /// RequestingPartner fallen since the pairing.
    fn bucket_pair_live(&self, pid: u64) -> bool {
        let Some(pair) = self.bucket.pairs.get(&pid) else {
            return false;
        };
        pair.partners()
            .iter()
            .all(|s| self.alive(*s) && self.bucket.players.get(s).and_then(|p| p.pair) == Some(pid))
            && pair.flag_fell.len() < 2
    }

    /// Cancel/desync (64): ends a wait; a pair stays (the game sends one right after it is paired).
    fn cancel_bucket_request(&mut self, serial: u64) {
        let now = self.now;
        let index = self.bucket_index(serial);
        let Some(player) = self.bucket.players.get_mut(&serial) else {
            return;
        };
        player.pending_request = None;
        player.flag_request_at = None;
        if let Some(pid) = player.pair {
            if let Some(pair) = self.bucket.pairs.get_mut(&pid)
                && !pair.cancelled.contains(&serial)
            {
                pair.cancelled.push(serial);
                if pair.cancelled.len() == 2 {
                    pair.cancelled_at = Some(now);
                }
            }
            self.note("bucket.cancel_paired", vec![json!(index)]);
            return;
        }
        if player.waiting.is_none() {
            return;
        }
        player.last_request_at = None;
        player.last_63_at = None;
        self.cancel_bucket_wait(serial, "cancelled");
    }

    /// ChallengeEnded (62): the pair ends, or the wait.
    fn bucket_challenge_ended(&mut self, serial: u64, completed: bool) {
        let index = self.bucket_index(serial);
        let Some(player) = self.bucket.players.get_mut(&serial) else {
            return;
        };
        player.pending_request = None;
        player.flag_request_at = None;
        if let Some(pid) = player.pair {
            let partners = self.bucket.pairs.get(&pid).map(Pair::partners);
            for s in partners.into_iter().flatten() {
                if let Some(other) = self.bucket.players.get_mut(&s) {
                    other.last_request_at = None;
                    other.last_63_at = None;
                }
            }
            let why = format!("peer {index} ended its challenge ({})", u8::from(completed));
            self.end_bucket_pair(pid, &why, serial, completed);
        } else if player.waiting.is_some() {
            player.last_request_at = None;
            player.last_63_at = None;
            self.cancel_bucket_wait(serial, "its challenge ended");
        }
    }

    fn cancel_bucket_wait(&mut self, serial: u64, why: &str) {
        let index = self.bucket_index(serial);
        let Some((slot, challenge, _, _)) = self
            .bucket
            .players
            .get_mut(&serial)
            .and_then(|p| p.waiting.take())
        else {
            return;
        };
        self.free_bucket_entries(&[index]);
        self.bucket_universe_due(serial); // settled at the flush
        self.note(
            "bucket.stops_waiting",
            vec![json!(index), json!(slot), json!(challenge), json!(why)],
        );
    }

    /// `ender`: the partner whose 62 (or new request, or leave) ended the pair, 68's first key;
    /// `flag`: the ending 62's flag (false for a new request or a leave).
    fn end_bucket_pair(&mut self, pid: u64, why: &str, ender: u64, flag: bool) {
        let Some(pair) = self.bucket.pairs.remove(&pid) else {
            return;
        };
        for s in pair.partners() {
            if let Some(p) = self.bucket.players.get_mut(&s)
                && p.pair == Some(pid)
            {
                p.pair = None;
                p.waiting = None;
            }
        }
        self.free_bucket_entries(&[pair.first_index, pair.second_index]);
        // a leaver's PlayerState is gone from every game (its deletes went out first): skipped
        let (a, b) = if ender == pair.first {
            (
                (pair.first, pair.first_index),
                (pair.second, pair.second_index),
            )
        } else {
            (
                (pair.second, pair.second_index),
                (pair.first, pair.first_index),
            )
        };
        self.bucket_list_status(68, pair.challenge, a, Some(b), flag, None);
        for s in pair.partners() {
            self.bucket_universe_due(s);
        }
        if pair.universe != 0 {
            self.bucket.universes_freed.insert(pair.universe, self.now);
        }
        let mut args = vec![
            json!(pair.first_index),
            json!(pair.second_index),
            json!(why),
            json!(pair.universe),
        ];
        if pair.started {
            let car =
                |s: u64| opt_text(car_universe(self.peer(s).and_then(|p| p.last_car.as_ref())));
            args.push(json!(format!("{}/{}", car(pair.first), car(pair.second))));
            self.note("bucket.pair_ended_cars", args);
        } else {
            self.note("bucket.pair_ended", args);
        }
    }

    /// The first car prediction of each partner after 61, with the universe it reports.
    fn trace_bucket_pair_cars(&mut self, pid: u64) {
        let Some(pair) = self.bucket.pairs.get(&pid) else {
            return;
        };
        let universe = pair.universe;
        let mut seen = Vec::new();
        for (s, at_start) in &pair.cars_at_start {
            let Some(p) = self.peer(*s) else { continue };
            let Some(car) = &p.last_car else { continue };
            if at_start.as_ref() == Some(car) {
                continue;
            }
            seen.push((*s, p.index, car_universe(Some(car))));
        }
        for (s, index, car) in seen {
            if let Some(pair) = self.bucket.pairs.get_mut(&pid) {
                pair.cars_at_start.retain(|(x, _)| *x != s);
            }
            self.note(
                "bucket.car_universe",
                vec![
                    json!(index),
                    car.map_or(Value::String(String::new()), |u| json!(u)),
                    json!(universe),
                ],
            );
        }
    }

    pub(super) fn tick_bucket_list(&mut self) {
        let now = self.now;
        for serial in self.bucket.players.keys() {
            let expired = self
                .bucket
                .players
                .get(&serial)
                .and_then(|p| p.waiting)
                .is_some_and(|w| now - w.2 >= WAIT_MS);
            if expired {
                self.cancel_bucket_wait(serial, &format!("no partner in {WAIT_MS} ms"));
            }
            // RequestingPartner raised and no 63 within the grace: the flag is the request
            let flag = self
                .bucket
                .players
                .get(&serial)
                .and_then(|p| p.flag_request_at)
                .is_some_and(|at| now - at >= REQUEST_GRACE_MS);
            if flag {
                let alive = self.alive(serial);
                let index = self.bucket_index(serial);
                let Some(player) = self.bucket.players.get_mut(&serial) else {
                    continue;
                };
                player.flag_request_at = None;
                let (slot, challenge) = (player.slot, player.challenge);
                // no challenge selected (slot 0, challenge 0) is no request
                if player.requesting && alive && (slot != 0 || challenge != 0) {
                    self.note(
                        "bucket.request_from_state",
                        vec![
                            json!(index),
                            json!(63),
                            json!(REQUEST_GRACE_MS),
                            json!(slot),
                            json!(challenge),
                        ],
                    );
                    self.request_bucket_partner(serial, slot, challenge);
                }
            }
            // the newest request that came inside the rate window, once the window has passed
            let alive = self.alive(serial);
            let Some(player) = self.bucket.players.get_mut(&serial) else {
                continue;
            };
            if let Some((slot, challenge)) = player.pending_request
                && player
                    .last_request_at
                    .is_none_or(|at| now - at >= REQUEST_MIN_MS)
                && alive
            {
                player.pending_request = None;
                self.request_bucket_partner(serial, slot, challenge);
            }
        }
        let mut pairs: Vec<u64> = Vec::new();
        for (_, p) in self.bucket.players.iter() {
            if let Some(id) = p.pair
                && !pairs.contains(&id)
                && self.bucket.pairs.contains_key(&id)
            {
                pairs.push(id);
            }
        }
        for pid in pairs {
            let Some(pair) = self.bucket.pairs.get(&pid) else {
                continue;
            };
            let fallback = !pair.started
                && pair
                    .cancelled_at
                    .is_some_and(|at| now - at >= START_FALLBACK_MS);
            if fallback && self.bucket_pair_live(pid) {
                let Some(pair) = self.bucket.pairs.get(&pid) else {
                    continue;
                };
                let ready: Vec<u8> = pair.ready.iter().map(|s| self.bucket_index(*s)).collect();
                let state = |s: u64| {
                    self.bucket
                        .players
                        .get(&s)
                        .map_or(String::new(), |p| p.state.to_string())
                };
                let args = vec![
                    json!(pair.first_index),
                    json!(pair.second_index),
                    json!(START_FALLBACK_MS),
                    json!(ready),
                    json!(format!("{}/{}", state(pair.first), state(pair.second))),
                ];
                self.note("bucket.started_unready", args);
                self.start_bucket_pair(pid, true);
            }
            // log lines only: nothing is sent, the pair stays (the games' own timeout ends it)
            if self.bucket.pairs.get(&pid).is_some_and(|p| p.started) {
                self.trace_bucket_pair_cars(pid);
            }
            let Some(pair) = self.bucket.pairs.get(&pid) else {
                continue;
            };
            if pair.started || pair.stall_noted || now - pair.paired_at < START_STALL_MS {
                continue;
            }
            let partners: Vec<String> = pair
                .partners()
                .iter()
                .zip([pair.first_index, pair.second_index])
                .map(|(s, index)| {
                    let state = self
                        .bucket
                        .players
                        .get(s)
                        .map_or(String::new(), |p| p.state.to_string());
                    let ready = if pair.ready.contains(s) {
                        "ready"
                    } else {
                        "not ready"
                    };
                    format!("peer {index} {ready} (last State {state})")
                })
                .collect();
            let args = vec![
                json!(pair.first_index),
                json!(pair.second_index),
                json!(START_STALL_MS),
                json!(partners.join(", ")),
            ];
            if let Some(pair) = self.bucket.pairs.get_mut(&pid) {
                pair.stall_noted = true;
            }
            self.note("bucket.stalled", args);
        }
        self.flush_bucket_list();
    }

    /// A peer left: its pair ends, or its wait.
    pub(super) fn bucket_list_peer_left(&mut self, peer: &Peer) {
        let Some(player) = self.bucket.players.remove(&peer.serial) else {
            return;
        };
        if let Some(pid) = player.pair {
            self.end_bucket_pair(
                pid,
                &format!("peer {} left", peer.index),
                peer.serial,
                false,
            );
        } else if player.waiting.is_some() {
            self.free_bucket_entries(&[peer.index]);
        }
        self.flush_bucket_list();
    }

    /// The world closes.
    pub(super) fn clear_bucket_list(&mut self) {
        let b = &mut self.bucket;
        b.players.clear();
        b.pairs.clear();
        b.outbox.clear();
        b.universe_due.clear();
        b.universes_freed.clear();
        b.slots = [(0, 0, 0); ENTRIES];
        b.slots_sent = [(0, 0, 0); ENTRIES];
    }
}
