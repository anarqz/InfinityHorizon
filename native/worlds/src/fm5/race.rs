// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! FM5's race server (OD-206, CR-S99 §5, W6), ported from the LAN lab's `tools/fm5-probe/world.py`,
//! which ran real races between real consoles (handoff "Worlds"; lobby spec §0-§10):
//!
//!   - the handshake, with `SessionHost` created **with** it (the lobby scene initializes once and
//!     crashes on click without it, 0x1403fe360) and a first GameOptions;
//!   - the consoles' objects relayed under the relay handle naming their owner, kept for later
//!     joiners (the Create, then every update in order); car and score messages with the owner in
//!     their handle, the static messages copied bit for bit;
//!   - the race: Ready (32) is never answered; all ready (two or more) → console var 2 set on every
//!     console → GameLaunch (29) → StartingGrid (27) every second with a rising counter until every
//!     console holds var 8 → all var 0 → StartRace (28) at now + 5 s; the end driven by the
//!     consoles' var 7, then var 2 cleared and MsgReadyRequestResponse (33) to clear every checkmark;
//!   - pings, the resend of unacked sequenced packets under a new id, the quiet drop.
//!
//! Infinity's additions (not in the lab): the world's admission asked per packet, at sync and
//! before each send (a peer that is not allowed is not served); held opens while a grant's FIUP
//! proof is expected; the `maxPlayers` seats; a cap on each console's kept object history; the
//! race state reset once nobody is left; `peer.synced`/`peer.left` events. Time is the `now` the
//! core passes in (ms), randomness the world's [`Rng`]; datagrams leave through `out`.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use serde_json::{Value, json};

use super::{
    nerd::{self, Fields, Handle, Object, Walked, Writer},
    options::{self, Options},
};
use crate::core::{
    game::{Admit, GameEvent, Send},
    rng::Rng,
};

/// The engine name of `peer.synced`/`peer.left` (the world's one activity).
pub const ENGINE: &str = "race";

/// Default race: Top Gear West Circuit (gamedb.slt Tracks id 222, 1.3 km), one lap. The lab's
/// choice so live test races stay short (world.py). In a private lobby the leader changes them on
/// FM5's own options screen (W16, the owner path below).
pub const DEFAULT_TRACK: u64 = 222;
pub const DEFAULT_LAPS: u64 = 1;

/// Var index, value, width. From the lobby spec §3: preset 1 (Circuit Race), track, end condition
/// 1 (laps), laps. Var 228 as the lap count was inferred from field order, then seen live as "3
/// Laps".
pub fn game_options(track: u64, laps: u64) -> Vec<(u32, u64, usize)> {
    vec![(0, 1, 8), (3, track, 32), (227, 1, 3), (228, laps, 32)]
}

pub const MIN_PLAYERS: usize = 2;
/// W16: at most this many parsed vars per `options.owner_update` line (the line also carries the count).
const OWNER_UPDATE_LOG_VARS: usize = 32;
/// ConsoleSynced: 4 is the in-race stage (lobby spec §10.5), 7 goes on with launch and off when
/// the race has ended.
pub const CONSOLE_READY: usize = 4;
pub const CONSOLE_IN_GAME: usize = 7;
/// Lobby spec §10.5: 8 is set on receiving the grid, 0 means loaded (inferred).
pub const CONSOLE_GRID: usize = 8;
pub const CONSOLE_LOADED: usize = 0;
pub const CONSOLE_PARTICIPATING: u64 = 2;
pub const START_DELAY_MS: u32 = 5000;
pub const RELAYED: [u8; 3] = [nerd::ROUTER_PLAYER, nerd::ROUTER_CAR, nerd::ROUTER_CONSOLE];
pub const PING_MS: u64 = 500;
pub const RESEND_MS: u64 = 400;
pub const RESEND_TRIES: u32 = 8;
pub const QUIET_MS: u64 = 15_000;
/// A grid sent too early is lost in the game: it repeats this often until every racer holds it.
pub const GRID_REPEAT_MS: u64 = 1000;
/// Infinity: an OpenConnection waits this long for its grant's FIUP proof (as FH2's held opens).
pub const HOLD_MS: u64 = 10_000;
pub const HELD_MAX: usize = 16;
/// Infinity: a console's kept objects (payloads and updates) for later joiners, at most; past it
/// the console is dropped (`doomed`), as FH2's history cap. The lab kept every update forever.
pub const HISTORY_MAX_BITS: usize = 2 * 1024 * 1024;
/// A console's GameOptions link before anything reached it (lobby spec §1: a link starts at
/// 0xfe), so the first options message it takes is seq 0; seqs run mod 255.
pub const OPTIONS_SEQ_START: u8 = 0xFE;

/// The seq after `seq` (mod 255).
pub fn seq_after(seq: u8) -> u8 {
    ((u16::from(seq) + 1) % 255) as u8
}

/// The seq before `seq` (mod 255).
pub fn seq_before(seq: u8) -> u8 {
    ((u16::from(seq) + 254) % 255) as u8
}

/// The longest packet the server sends options in (bytes, header included): the engine's
/// datagram bound ([`crate::core::MAX_PAYLOAD`]), below FH2's 1372-byte packet limit.
pub const PACKET_MAX_BYTES: usize = crate::core::MAX_PAYLOAD;
/// The longest options payload in one message (bits): with its message head (35 bits) and the
/// packet header it stays inside [`PACKET_MAX_BYTES`]. A long table (a full one is about 12.5-14
/// k bits, lobby spec §13.7) goes as several Updates.
pub const OPTIONS_CHUNK_BITS: usize = 8 * 1024;

/// Whether `m` is an options message the server may send (lobby spec §13.8): an Update (8), a
/// Grant (11) or a Transfer (12) on router 6 with the Transfer handle, id 0. Never an
/// ObjectCreate (7) on router 6: a Create of type 1 would overwrite the SessionHost slot
/// (`CLobby+0x150`, `0x140555ac0`). Every options message goes through [`Race::send_options`],
/// which asserts this.
pub(super) fn is_options_message(m: &Writer) -> bool {
    let bytes = m.bytes();
    let mut r = nerd::Reader::new(&bytes);
    let (Ok(ident), Ok(router), Ok(a), Ok(b), Ok(id)) = (
        r.bits(nerd::ID_BITS),
        r.bits(nerd::ROUTER_BITS),
        r.bits(1),
        r.bits(1),
        r.bits(nerd::LOBBY_ID_BITS),
    ) else {
        return false;
    };
    matches!(
        ident as u8,
        nerd::OBJECT_UPDATE | nerd::GRANT_OWNERSHIP | nerd::TRANSFER_OWNERSHIP
    ) && router == u64::from(nerd::ROUTER_LOBBY)
        && (a as u8, b as u8) == nerd::HANDLE_TRANSFER
        && id == 0
}

/// A GameOptions Update from the server (router 6, Transfer handle, id 0).
fn options_update(seq: u8, payload: &Writer) -> Writer {
    nerd::object_update(
        nerd::ROUTER_LOBBY,
        nerd::HANDLE_TRANSFER,
        0,
        seq,
        payload,
        None,
    )
}

/// Why a peer left (the `peer.left` reason, contract §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Closed,
    Quiet,
    Kicked,
    NotAllowed,
    Doomed,
}

impl Why {
    fn reason(self) -> &'static str {
        match self {
            Why::Closed => "close",
            Why::Quiet => "timeout",
            Why::Kicked => "kicked",
            Why::NotAllowed => "not_allowed",
            Why::Doomed => "doomed",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Why::Closed => "peer.dropped.closed",
            Why::Quiet => "peer.dropped.quiet",
            Why::Kicked => "peer.dropped.kicked",
            Why::NotAllowed => "peer.dropped.not_allowed",
            Why::Doomed => "peer.dropped.history",
        }
    }
}

/// A sequenced packet a console has not acked yet.
pub(super) struct InFlight {
    pub(super) ident: u16,
    messages: Vec<Writer>,
    pub(super) sent: u64,
    tries: u32,
    /// Resent until acked (options: a console's options seqs never have a gap).
    pub(super) forever: bool,
}

/// A console's object, kept so a later joiner gets it: its Create, then every update in order.
pub(super) struct Owned {
    router: u8,
    ident: u32,
    kind: u8,
    seq: u8,
    payload: Writer,
    updates: Vec<(u8, Writer)>,
    last_seq: u8,
    /// The server's own updates in the others' copy: later forwards are shifted by this.
    injected: u8,
}

impl Owned {
    fn bits(&self) -> usize {
        self.payload.len() + self.updates.iter().map(|u| u.1.len()).sum::<usize>()
    }
}

pub(super) struct Peer {
    pub(super) address: SocketAddr,
    pub(super) own_attempt: u32,
    next_id: u16,
    sync_index: u16,
    pub(super) synced: bool,
    synced_at: u64,
    last_heard: u64,
    acked: Option<u16>,
    ackbits: u32,
    pub(super) next_ping: u64,
    /// Owned objects by (router, id), in creation order.
    owned: Vec<((u8, u32), Owned)>,
    history_bits: usize,
    pub(super) index: u8,
    pub(super) ready: bool,
    pub(super) xuid: u64,
    console_vars: [bool; 16],
    saw_grid: bool,
    server_seq: Option<u8>,
    /// The last seq the server sent on this console's GameOptions receive queue (lobby spec §13.7
    /// Q1, Q2): the options Updates, the grant and the revoke share this one counter. The
    /// console's own Updates as owner run on its separate send counter and never move this one.
    pub(super) options_seq: u8,
    pub(super) in_flight: Vec<InFlight>,
}

impl Peer {
    pub(super) fn new(address: SocketAddr, own_attempt: u32, now: u64) -> Peer {
        Peer {
            address,
            own_attempt,
            next_id: 0,
            sync_index: 0,
            synced: false,
            synced_at: 0,
            last_heard: now,
            acked: None,
            ackbits: 0,
            next_ping: 0,
            owned: Vec::new(),
            history_bits: 0,
            index: 0,
            ready: false,
            xuid: 0,
            console_vars: [false; 16],
            saw_grid: false,
            server_seq: None,
            options_seq: OPTIONS_SEQ_START,
            in_flight: Vec::new(),
        }
    }

    /// The next seq of this console's GameOptions link, taken.
    pub(super) fn take_options_seq(&mut self) -> u8 {
        self.options_seq = seq_after(self.options_seq);
        self.options_seq
    }

    pub(super) fn note_received(&mut self, ident: u16) {
        if ident == nerd::NO_SEQ {
            return;
        }
        let Some(acked) = self.acked else {
            self.acked = Some(ident);
            self.ackbits = 0;
            return;
        };
        let ahead = ident.wrapping_sub(acked);
        if ahead == 0 || ahead >= 0x8000 {
            let behind = acked.wrapping_sub(ident);
            if 0 < behind && behind <= 32 {
                self.ackbits |= 1 << (behind - 1);
            }
            return;
        }
        self.ackbits = if ahead <= 32 {
            ((u64::from(self.ackbits) << ahead) | (1 << (ahead - 1))) as u32
        } else {
            0
        };
        self.acked = Some(ident);
    }

    pub(super) fn ack(&self) -> (u16, u32) {
        self.acked.map_or((nerd::NO_ACK, 0), |a| (a, self.ackbits))
    }

    fn take_id(&mut self) -> u16 {
        let ident = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id >= nerd::NO_ACK {
            self.next_id = 0;
        }
        ident
    }

    fn owned(&self, key: (u8, u32)) -> Option<&Owned> {
        self.owned.iter().find(|(k, _)| *k == key).map(|(_, o)| o)
    }

    fn owned_mut(&mut self, key: (u8, u32)) -> Option<&mut Owned> {
        self.owned
            .iter_mut()
            .find(|(k, _)| *k == key)
            .map(|(_, o)| o)
    }

    fn var(&self, index: usize) -> bool {
        self.console_vars[index]
    }
}

/// An OpenConnection held until its grant's proof arrives.
struct Held {
    from: SocketAddr,
    user: u64,
    data: Vec<u8>,
    until: u64,
}

/// The world's race server: the lab's `World`.
pub(super) struct Race {
    admit: Arc<dyn Admit>,
    rng: Rng,
    max_players: usize,
    pub(super) peers: Vec<Peer>,
    held: Vec<Held>,
    origin: Option<u64>,
    pub(super) now: u64,
    next_index: u8,
    launch_counter: u32,
    pub(super) launched: bool,
    pub(super) grid_done: bool,
    pub(super) start_sent: bool,
    next_grid: u64,
    /// Score messages seen this race, by message id.
    pub(super) results: BTreeMap<u8, u32>,
    pub(super) resent: u64,
    pub(super) options: Options,
    /// A private lobby (W16): its leader owns the GameOptions. Otherwise they stay the server's.
    pub(super) leader_owns_options: bool,
    /// The console the options were granted to (a private lobby only).
    pub(super) owner: Option<SocketAddr>,
    pub(super) dropped: u64,
    pub(super) events: Vec<GameEvent>,
    pub(super) out: Vec<Send>,
}

fn note(id: &'static str, args: Value) {
    if crate::log::enabled(crate::log::Level::Info) {
        crate::log::info(crate::log_ids::WORLD_NOTE)
            .field("engine", ENGINE)
            .field("note", id)
            .field("args", args)
            .emit();
    }
}

impl Race {
    pub(super) fn new(admit: Arc<dyn Admit>, rng: Rng, max_players: usize) -> Race {
        Race {
            admit,
            rng,
            max_players,
            peers: Vec::new(),
            held: Vec::new(),
            origin: None,
            now: 0,
            next_index: 1,
            launch_counter: 0,
            launched: false,
            grid_done: false,
            start_sent: false,
            next_grid: 0,
            results: BTreeMap::new(),
            resent: 0,
            options: Options::new(&game_options(DEFAULT_TRACK, DEFAULT_LAPS)),
            leader_owns_options: false,
            owner: None,
            dropped: 0,
            events: Vec::new(),
            out: Vec::new(),
        }
    }

    fn set_now(&mut self, now: u64) {
        self.now = self.now.max(now);
        self.origin.get_or_insert(now);
    }

    /// The server's clock on the wire (TimeSync, Pong, StartRace): ms since the world first ran,
    /// plus one (world.py `now_ms`).
    pub(super) fn now_ms(&self) -> u32 {
        (self.now - self.origin.unwrap_or(self.now) + 1) as u32
    }

    fn allowed(&self, xuid: u64, from: SocketAddr) -> bool {
        self.admit.allowed(xuid, from, 0, self.now)
    }

    pub(super) fn position(&self, address: SocketAddr) -> Option<usize> {
        self.peers.iter().position(|p| p.address == address)
    }

    /// Synced peers' addresses, in arrival order (world.py `racers`).
    pub(super) fn racers(&self) -> Vec<SocketAddr> {
        self.peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| p.address)
            .collect()
    }

    pub(super) fn synced(&self) -> usize {
        self.peers.iter().filter(|p| p.synced).count()
    }

    pub(super) fn handshaking(&self) -> usize {
        self.peers.iter().filter(|p| !p.synced).count() + self.held.len()
    }

    pub(super) fn occupants(&self, except: u64) -> usize {
        self.peers.iter().filter(|p| p.xuid != except).count()
            + self.held.iter().filter(|h| h.user != except).count()
    }

    pub(super) fn is_idle(&self) -> bool {
        self.peers.is_empty() && self.held.is_empty()
    }

    /// Synced players' XUIDs, in arrival order.
    pub(super) fn xuids(&self) -> Vec<u64> {
        self.peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| p.xuid)
            .collect()
    }

    /// A private world's leader: the longest-present synced player (earliest sync, the earlier
    /// seat on a tie), as FH2's rule; it changes only when that player leaves.
    pub(super) fn leader(&self) -> Option<u64> {
        self.leader_peer().map(|p| p.xuid)
    }

    fn leader_peer(&self) -> Option<&Peer> {
        self.peers
            .iter()
            .filter(|p| p.synced)
            .min_by_key(|p| (p.synced_at, p.index))
    }

    /// The private lobby's leader owns the GameOptions from now on (W16; one way, as the world's
    /// privacy).
    pub(super) fn give_options_to_leader(&mut self) {
        self.leader_owns_options = true;
        self.sync_owner();
    }

    /// Moves the options' ownership to the leader, when it is not the owner already (lobby spec
    /// §13.3): the old owner, while still here, gets a revoke (12) first; then the leader a grant
    /// (11) with the full current values, since its copy may lag (a table too long for one grant
    /// goes first as Updates, the grant carrying its last part). No owner path starts a race.
    pub(super) fn sync_owner(&mut self) {
        if !self.leader_owns_options {
            return;
        }
        let want = self.leader_peer().map(|p| p.address);
        if want == self.owner {
            return;
        }
        if let Some(old) = self.owner.take()
            && let Some(pos) = self.position(old)
        {
            let seq = self.peers[pos].take_options_seq();
            let index = self.peers[pos].index;
            let revoke =
                nerd::transfer_ownership(nerd::ROUTER_LOBBY, nerd::HANDLE_TRANSFER, 0, seq);
            self.send_options(old, vec![(seq, revoke)]);
            note("options.revoked", json!({"peer": index, "seq": seq}));
        }
        let Some(new) = want else {
            return;
        };
        let Some(pos) = self.position(new) else {
            return;
        };
        let mut parts = options::chunks(self.options.vars(), OPTIONS_CHUNK_BITS);
        let last = parts.pop().unwrap_or_default();
        let p = &mut self.peers[pos];
        let mut messages: Vec<(u8, Writer)> = parts
            .iter()
            .map(|part| {
                let seq = p.take_options_seq();
                (seq, options_update(seq, part))
            })
            .collect();
        let seq = p.take_options_seq();
        let index = p.index;
        let grant = nerd::grant_ownership(nerd::ROUTER_LOBBY, nerd::HANDLE_TRANSFER, 0, seq, &last);
        messages.push((seq, grant));
        self.send_options(new, messages);
        self.owner = Some(new);
        note(
            "options.granted",
            json!({"peer": index, "seq": seq, "bits": self.options.full().len()}),
        );
    }

    /// Options messages (each with its seq, in seq order) to `to` (lobby spec §13.7 Q4): packed
    /// into packets of at most [`PACKET_MAX_BYTES`], each sent again until the console acks it (a
    /// gap in a console's options seqs stalls it forever; a repeat is dropped harmlessly). If a
    /// packet cannot go, the console's counter goes back to before it, so no seq is ever skipped.
    pub(super) fn send_options(&mut self, to: SocketAddr, messages: Vec<(u8, Writer)>) {
        let mut packets: Vec<(u8, Vec<Writer>)> = Vec::new();
        let mut bits = 0;
        for (seq, m) in messages {
            debug_assert!(is_options_message(&m), "options only as 8/11/12, Transfer");
            let fits = packets.last().is_some_and(|_| {
                nerd::HEADER_LEN + (bits + m.len()).div_ceil(8) <= PACKET_MAX_BYTES
            });
            if !fits {
                packets.push((seq, Vec::new()));
                bits = 0;
            }
            bits += m.len();
            if let Some(last) = packets.last_mut() {
                last.1.push(m);
            }
        }
        for (first, messages) in packets {
            if !self.send_packet(to, messages, true, 0, true) {
                if let Some(pos) = self.position(to) {
                    self.peers[pos].options_seq = seq_before(first);
                }
                return;
            }
        }
    }

    /// Sends `messages` to the peer at `address` in one packet, sequenced (a new id, kept for its
    /// resend) or not. A peer the admission no longer allows is not served (the tick drops it).
    pub(super) fn send(
        &mut self,
        address: SocketAddr,
        messages: Vec<Writer>,
        sequenced: bool,
        tries: u32,
    ) {
        self.send_packet(address, messages, sequenced, tries, false);
    }

    /// [`Race::send`]; `forever`: resent until acked, never given up. Whether it went.
    fn send_packet(
        &mut self,
        address: SocketAddr,
        messages: Vec<Writer>,
        sequenced: bool,
        tries: u32,
        forever: bool,
    ) -> bool {
        let Some(pos) = self.position(address) else {
            return false;
        };
        let xuid = self.peers[pos].xuid;
        if !self.allowed(xuid, address) {
            return false;
        }
        let now = self.now;
        let p = &mut self.peers[pos];
        let ident = if sequenced { p.take_id() } else { nerd::NO_SEQ };
        let (ack, bits) = p.ack();
        let Some(data) = nerd::packet(&messages, ident, ack, bits) else {
            self.dropped += 1;
            return false;
        };
        self.out.push(Send { to: address, data });
        if sequenced {
            p.in_flight.push(InFlight {
                ident,
                messages,
                sent: now,
                tries,
                forever,
            });
        }
        true
    }

    /// A Close to the peer at `pos`, unsequenced (it is leaving: nothing waits for its ack).
    fn send_close(&mut self, pos: usize) {
        let p = &self.peers[pos];
        let (ack, bits) = p.ack();
        if let Some(data) = nerd::packet(&[nerd::close()], nerd::NO_SEQ, ack, bits) {
            self.out.push(Send {
                to: p.address,
                data,
            });
        }
    }

    /// Sequenced packets a console has not acked go again under a new id; on a real network losing
    /// a StartRace would strand a player. The game drops what it already applied by the objects'
    /// own seq; the rest are events. Options packets are never given up (W16, Q4): the console
    /// leaves by its quiet timeout instead.
    pub(super) fn resend_lost(&mut self, address: SocketAddr, now: u64) {
        let Some(pos) = self.position(address) else {
            return;
        };
        let waiting = std::mem::take(&mut self.peers[pos].in_flight);
        for f in waiting {
            if now.saturating_sub(f.sent) < RESEND_MS {
                if let Some(p) = self.position(address) {
                    self.peers[p].in_flight.push(f);
                }
            } else if f.forever || f.tries < RESEND_TRIES {
                self.resent += 1;
                self.send_packet(address, f.messages, true, f.tries + 1, f.forever);
            } else {
                note("send.gave_up", json!([self.peers[pos].index, f.tries]));
            }
        }
    }

    pub(super) fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64) {
        self.set_now(now);
        let Some(head) = nerd::header(data) else {
            self.dropped += 1;
            return;
        };
        if usize::from(head.size) != data.len() {
            self.dropped += 1;
            return;
        }
        let walked = if head.count > 0 {
            match nerd::walk_messages(data) {
                Ok(w) => w,
                Err(_) => {
                    self.dropped += 1;
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let open_user = walked.iter().find_map(|w| match &w.fields {
            Some(Fields::Open { user, .. }) => Some(*user),
            _ => None,
        });
        let pos = match self.position(from) {
            Some(p) => p,
            None => {
                let first = walked.first().and_then(|w| match &w.fields {
                    Some(Fields::Open { user, .. }) if w.ident == nerd::OPEN_CONNECTION => {
                        Some(*user)
                    }
                    _ => None,
                });
                let Some(user) = first else {
                    self.dropped += 1;
                    return;
                };
                match self.admit_open(user, from, data) {
                    Some(p) => p,
                    None => return,
                }
            }
        };
        // Infinity: every packet asks the admission; an open naming another player is not this
        // seat's
        let xuid = self.peers[pos].xuid;
        if open_user.is_some_and(|u| u != xuid) {
            self.dropped += 1;
            return;
        }
        if !self.allowed(xuid, from) {
            self.dropped += 1;
            self.drop_peer(from, Why::NotAllowed);
            return;
        }
        let p = &mut self.peers[pos];
        p.last_heard = now;
        p.note_received(head.ident);
        p.in_flight
            .retain(|f| !nerd::acks_packet(head.ack, head.ackbits, f.ident));
        let (mut outgoing, mut cars) = (Vec::new(), Vec::new());
        for Walked {
            ident,
            fields,
            start,
            end,
        } in walked
        {
            let (Some(fields), Some(end)) = (fields, end) else {
                continue;
            };
            match fields {
                Fields::Object(o)
                    if matches!(
                        ident,
                        nerd::OBJECT_CREATE | nerd::OBJECT_UPDATE | nerd::OBJECT_DELETE
                    ) =>
                {
                    self.relay(from, ident, &o, data)
                }
                Fields::Handled(h) if nerd::HANDLED_RELAY.contains(&ident) => {
                    if let Some(m) = self.relay_handled(from, ident, &h, end, data) {
                        if matches!(ident, nerd::CAR_PREDICTION | nerd::PLAYER_INPUT) {
                            cars.push(m);
                        } else {
                            outgoing.push(m);
                        }
                    }
                }
                _ if nerd::STATIC_RELAY.contains(&ident) => {
                    outgoing.push(nerd::bit_slice(data, start, end))
                }
                f => self.handle(from, ident, f),
            }
            if self.position(from).is_none() {
                break;
            }
        }
        for other in self.racers() {
            if other == from {
                continue;
            }
            if !cars.is_empty() {
                self.send(other, cars.clone(), false, 0);
            }
            if !outgoing.is_empty() {
                self.send(other, outgoing.clone(), true, 0);
            }
        }
    }

    /// A new endpoint's OpenConnection: seated when the admission allows it and a seat is free,
    /// held while its grant's proof is expected, else dropped. The lab took anybody.
    fn admit_open(&mut self, user: u64, from: SocketAddr, data: &[u8]) -> Option<usize> {
        if !self.allowed(user, from) {
            if self.admit.proof_expected(user, self.now) {
                self.held.retain(|h| h.from != from);
                if self.held.len() < HELD_MAX {
                    self.held.push(Held {
                        from,
                        user,
                        data: data.to_vec(),
                        until: self.now + HOLD_MS,
                    });
                    return None;
                }
            }
            self.dropped += 1;
            return None;
        }
        self.held.retain(|h| h.from != from);
        if self.peers.len() >= self.max_players {
            // FM5's refusal reasons are not known (only 0 and 6 are): no answer
            self.dropped += 1;
            return None;
        }
        let Some(index) = self.take_index() else {
            self.dropped += 1;
            return None;
        };
        let mut peer = Peer::new(from, self.rng.next_u64() as u32, self.now);
        peer.index = index;
        peer.xuid = user;
        self.peers.push(peer);
        Some(self.peers.len() - 1)
    }

    /// A car or score message of the sender's own: its relayed form (the owner in its handle), if
    /// it may go to the others.
    fn relay_handled(
        &mut self,
        from: SocketAddr,
        ident: u8,
        h: &Handle,
        end: usize,
        data: &[u8],
    ) -> Option<Writer> {
        if matches!(ident, nerd::LAP_SCORE | nerd::SCORE_KEYFRAME) {
            *self.results.entry(ident).or_default() += 1;
        }
        let p = &self.peers[self.position(from)?];
        // The spec: only after the sender's player object reached the others, or a wrong skip
        // misaligns.
        (h.form == nerd::HANDLE_OWN && p.owned((nerd::ROUTER_PLAYER, 0)).is_some())
            .then(|| nerd::relay_with_handle(data, ident, h, end, p.index))
    }

    fn handle(&mut self, from: SocketAddr, ident: u8, fields: Fields) {
        let Some(pos) = self.position(from) else {
            return;
        };
        match fields {
            Fields::Open {
                version,
                game,
                flags,
                attempt,
                ..
            } => {
                if version != nerd::VERSION || game != nerd::GAME {
                    note("open.unexpected_version", json!([self.peers[pos].index]));
                }
                let own = self.peers[pos].own_attempt;
                let response = nerd::open_response(flags, nerd::REASON_OK, attempt, own);
                self.send(from, vec![response], false, 0);
            }
            Fields::OpenResponse { reason, .. }
                if reason == nerd::REASON_RESPONSE_ACK && !self.peers[pos].synced =>
            {
                let wire = self.now_ms();
                let now = self.now;
                let p = &mut self.peers[pos];
                p.synced = true;
                p.synced_at = now;
                p.next_ping = now;
                let (index, own, xuid) = (p.sync_index, p.own_attempt, p.xuid);
                let peer = p.index;
                p.sync_index = p.sync_index.wrapping_add(1);
                let messages = vec![nerd::time_sync(index, wire), nerd::sync_done(own)];
                self.send(from, messages, false, 0);
                self.events.push(GameEvent::PeerSynced {
                    xuid,
                    engine: ENGINE,
                    peer,
                });
                self.send_lobby(from);
            }
            Fields::Ready { ready, .. } if ident == nerd::READY_REQUEST => {
                self.peers[pos].ready = ready;
                // No reply: MsgReadyRequestResponse means denied. Its handler (0x14020bc40 ->
                // 0x140555900) ignores the value and clears the local player's ready flag.
                self.check_launch();
            }
            Fields::Ping { time } => {
                let wire = self.now_ms();
                self.send(from, vec![nerd::pong(time, wire)], false, 0);
            }
            Fields::Close => self.drop_peer(from, Why::Closed),
            _ => {}
        }
    }

    /// Every connected console ready, two or more: launch (order inferred from FH2, lobby spec §4).
    pub(super) fn check_launch(&mut self) {
        let peers = self.racers();
        let all_ready = peers
            .iter()
            .all(|a| self.position(*a).is_some_and(|p| self.peers[p].ready));
        if self.launched || peers.len() < MIN_PLAYERS || !all_ready {
            return;
        }
        self.launched = true;
        self.set_participating(&peers, true);
        self.launch_counter += 1;
        let message = nerd::game_launch(self.launch_counter, &self.xuids(), nerd::LAUNCH_START, 0);
        for a in &peers {
            self.send(*a, vec![message.clone()], true, 0);
        }
        note("race.launch", json!([self.launch_counter, peers.len()]));
    }

    fn take_index(&mut self) -> Option<u8> {
        let used: Vec<u8> = self.peers.iter().map(|p| p.index).collect();
        for step in 0..254u16 {
            let index = ((u16::from(self.next_index) - 1 + step) % 254 + 1) as u8;
            if !used.contains(&index) {
                self.next_index = (u16::from(index) % 254 + 1) as u8;
                return Some(index);
            }
        }
        None
    }

    /// ConsoleSynced payload: 4-bit var index, then one bool, repeated.
    fn note_console_vars(&mut self, from: SocketAddr, data: &[u8], o: &Object) {
        let Some(pos) = self.position(from) else {
            return;
        };
        let mut r = nerd::Reader::new(nerd::body(data));
        r.position = o.payload_at;
        let end = o.payload_at + o.bits;
        while r.position + 5 <= end {
            let (Ok(index), Ok(value)) = (r.bits(4), r.bits(1)) else {
                break;
            };
            let p = &mut self.peers[pos];
            p.console_vars[index as usize] = value == 1;
            if index as usize == CONSOLE_GRID {
                // live: an early grid sets it, then it drops as the race loads
                p.saw_grid = value == 1;
            }
        }
        let peers = self.racers();
        let in_game = peers.iter().any(|a| {
            self.position(*a)
                .is_some_and(|p| self.peers[p].var(CONSOLE_IN_GAME))
        });
        if self.launched && !in_game {
            note("race.over", json!([self.results.values().sum::<u32>()]));
            self.set_participating(&peers, false);
            // Back in the lobby every checkmark would stay on and never be sent again, so the next
            // race would need X pressed twice. MsgReadyRequestResponse clears the flag, whatever
            // its value (0x14020bc40, 0x140555900). Unit-tested only in the lab: never run with
            // the game (handoff, "Not verified" 1).
            let denied = nerd::ready_denied(0);
            for a in &peers {
                if let Some(p) = self.position(*a) {
                    self.peers[p].ready = false;
                }
                self.send(*a, vec![denied.clone()], true, 0);
            }
            self.end_race(&peers);
        }
    }

    fn end_race(&mut self, peers: &[SocketAddr]) {
        self.launched = false;
        self.grid_done = false;
        self.start_sent = false;
        self.results.clear();
        for a in peers {
            if let Some(p) = self.position(*a) {
                self.peers[p].saw_grid = false;
            }
        }
    }

    /// Each game decides who races from the consoles' var 2 (FH2's ParticipatingInRace): the grid
    /// and the HUD's places. Without it each game raced alone, Place 1/1. On the owner's own
    /// object through the server handle, with the server's own sequence, and injected into the
    /// others' copy of it.
    pub(super) fn set_participating(&mut self, peers: &[SocketAddr], value: bool) {
        let payload = Writer::new()
            .push(CONSOLE_PARTICIPATING, 4)
            .push(u64::from(value), 1);
        for a in peers {
            let Some(pos) = self.position(*a) else {
                continue;
            };
            let p = &mut self.peers[pos];
            let index = p.index;
            if p.owned((nerd::ROUTER_CONSOLE, 0)).is_none() {
                note("race.no_console_object", json!([index]));
                continue;
            }
            let server_seq = p.server_seq.map_or(0, |s| ((u16::from(s) + 1) % 255) as u8);
            p.server_seq = Some(server_seq);
            p.history_bits += payload.len();
            let Some(item) = p.owned_mut((nerd::ROUTER_CONSOLE, 0)) else {
                continue;
            };
            item.injected = ((u16::from(item.injected) + 1) % 255) as u8;
            let seq = ((u16::from(item.last_seq) + u16::from(item.injected)) % 255) as u8;
            item.updates.push((seq, payload.clone()));
            let own = nerd::object_update(
                nerd::ROUTER_CONSOLE,
                nerd::HANDLE_SERVER,
                0,
                server_seq,
                &payload,
                None,
            );
            self.send(*a, vec![own], true, 0);
            let message = nerd::object_update(
                nerd::ROUTER_CONSOLE,
                nerd::HANDLE_RELAY,
                0,
                seq,
                &payload,
                Some(index),
            );
            for other in peers {
                if other != a {
                    self.send(*other, vec![message.clone()], true, 0);
                }
            }
        }
        note("race.participating", json!([value]));
    }

    /// Lobby spec §10.8. A grid sent too early is lost in the game, so it repeats with a rising
    /// counter from the first var 4 or 7 until every racer shows var 8. Then var 0 from all gets
    /// one future start time.
    pub(super) fn race_tick(&mut self, now: u64) {
        self.set_now(now);
        if !self.launched {
            return;
        }
        let peers = self.racers();
        let all = |race: &Race, f: &dyn Fn(&Peer) -> bool| {
            peers
                .iter()
                .all(|a| race.position(*a).is_some_and(|p| f(&race.peers[p])))
        };
        if !self.grid_done {
            let started = peers.iter().any(|a| {
                self.position(*a).is_some_and(|p| {
                    self.peers[p].var(CONSOLE_READY) || self.peers[p].var(CONSOLE_IN_GAME)
                })
            });
            if !started {
                return;
            }
            if all(self, &|p| p.var(CONSOLE_LOADED)) {
                self.grid_done = true;
                note("race.loaded", json!([peers.len()]));
            } else if !all(self, &|p| p.saw_grid) && now >= self.next_grid {
                self.next_grid = now + GRID_REPEAT_MS;
                self.launch_counter += 1;
                let indexes: Vec<u8> = (0..peers.len()).map(|i| i as u8).collect();
                let message = nerd::starting_grid(self.launch_counter, &indexes);
                for a in &peers {
                    self.send(*a, vec![message.clone()], true, 0);
                }
                note("race.grid", json!([self.launch_counter, peers.len()]));
            }
            if !self.grid_done {
                return;
            }
        }
        if !self.start_sent && all(self, &|p| p.var(CONSOLE_LOADED)) {
            self.start_sent = true;
            let when = self.now_ms().wrapping_add(START_DELAY_MS);
            for a in &peers {
                self.send(*a, vec![nerd::start_race(when)], true, 0);
            }
            note("race.start", json!([START_DELAY_MS]));
        }
    }

    /// A console's own objects go to the others under the relay handle naming their owner. The
    /// lobby router is the server's, and only a console's own handle is forwarded.
    fn relay(&mut self, from: SocketAddr, ident: u8, o: &Object, data: &[u8]) {
        if o.router == nerd::ROUTER_LOBBY {
            self.owner_update(from, ident, o, data);
            return;
        }
        if !RELAYED.contains(&o.router) || o.form != nerd::HANDLE_OWN {
            return;
        }
        if o.router == nerd::ROUTER_CONSOLE && ident != nerd::OBJECT_DELETE {
            self.note_console_vars(from, data, o);
        }
        let Some(pos) = self.position(from) else {
            return;
        };
        let key = (o.router, o.id);
        let p = &mut self.peers[pos];
        let index = p.index;
        let message = match ident {
            nerd::OBJECT_DELETE => {
                if let Some(i) = p.owned.iter().position(|(k, _)| *k == key) {
                    let (_, gone) = p.owned.remove(i);
                    p.history_bits = p.history_bits.saturating_sub(gone.bits());
                }
                nerd::object_delete(o.router, nerd::HANDLE_RELAY, o.id, o.seq, Some(index))
            }
            nerd::OBJECT_CREATE => {
                let item = Owned {
                    router: o.router,
                    ident: o.id,
                    kind: o.kind.unwrap_or(0),
                    seq: o.seq,
                    payload: nerd::payload_of(data, o),
                    updates: Vec::new(),
                    last_seq: o.seq,
                    injected: 0,
                };
                let message = create_message(index, &item);
                p.history_bits += item.bits();
                match p.owned.iter().position(|(k, _)| *k == key) {
                    Some(i) => {
                        p.history_bits = p.history_bits.saturating_sub(p.owned[i].1.bits());
                        p.owned[i].1 = item;
                    }
                    None => p.owned.push((key, item)),
                }
                message
            }
            _ => {
                let payload = nerd::payload_of(data, o);
                let mut seq = o.seq;
                if let Some(item) = p.owned_mut(key) {
                    item.last_seq = seq;
                    seq = ((u16::from(seq) + u16::from(item.injected)) % 255) as u8;
                    item.updates.push((seq, payload.clone()));
                    p.history_bits += payload.len();
                }
                nerd::object_update(
                    o.router,
                    nerd::HANDLE_RELAY,
                    o.id,
                    seq,
                    &payload,
                    Some(index),
                )
            }
        };
        if self.peers[pos].history_bits > HISTORY_MAX_BITS {
            self.drop_peer(from, Why::Doomed);
            return;
        }
        for other in self.racers() {
            if other != from {
                self.send(other, vec![message.clone()], true, 0);
            }
        }
    }

    /// A router-6 object message from a console. Only the options' owner's Update of the
    /// GameOptions (Transfer handle, id 0) counts (lobby spec §13); any other is dropped, as every
    /// router-6 message was before W16. Its seq is the owner's own send counter (§13.7 Q2): any
    /// is taken, and it never moves the server's counter to that console. It is read whole with
    /// every var's width (one naming an index past 290 or ending inside a value is dropped whole
    /// and logged), logged var by var (the set the owner writes is OPEN), merged, and relayed to
    /// every other console as the server's Transfer Update at that console's next seq.
    ///
    /// Lobby kind 7 only: there `0x140554e40` sets Ready on remote players, so an owner's Update
    /// aimed at another player's object would have to be dropped. Private lobbies are kind 5 and
    /// router 6 holds only SessionHost and GameOptions, so nothing else is taken here.
    fn owner_update(&mut self, from: SocketAddr, ident: u8, o: &Object, data: &[u8]) {
        if !self.leader_owns_options
            || self.owner != Some(from)
            || ident != nerd::OBJECT_UPDATE
            || o.form != nerd::HANDLE_TRANSFER
            || o.id != 0
        {
            return;
        }
        let Some(pos) = self.position(from) else {
            return;
        };
        let index = self.peers[pos].index;
        let payload = nerd::payload_of(data, o);
        let vars = match options::parse(&payload) {
            Ok(vars) => vars,
            Err(why) => {
                let reason = match why {
                    options::Refused::Unknown { index } => json!({"unknown": index}),
                    options::Refused::Truncated { index } => json!({"truncated": index}),
                };
                note(
                    "options.owner_update_dropped",
                    json!({"peer": index, "seq": o.seq, "bits": payload.len(), "reason": reason}),
                );
                return;
            }
        };
        self.options.merge(&vars);
        if crate::log::enabled(crate::log::Level::Info) {
            // parsed indexes and values only, never raw payload bytes; capped per line (MACBOOK, W16)
            let logged: Vec<Value> = vars
                .iter()
                .take(OWNER_UPDATE_LOG_VARS)
                .map(|v| json!([v.index, v.number(), v.value.len()]))
                .collect();
            note(
                "options.owner_update",
                json!({"peer": index, "seq": o.seq, "bits": payload.len(), "count": vars.len(),
                    "vars": logged}),
            );
        }
        let parts = options::chunks(&vars, OPTIONS_CHUNK_BITS);
        for other in self.racers() {
            if other == from {
                continue;
            }
            let Some(at) = self.position(other) else {
                continue;
            };
            let p = &mut self.peers[at];
            let messages = parts
                .iter()
                .map(|part| {
                    let seq = p.take_options_seq();
                    (seq, options_update(seq, part))
                })
                .collect();
            self.send_options(other, messages);
        }
    }

    /// What the others already own, to a console that just joined.
    fn send_history(&mut self, to: SocketAddr) {
        let mut messages = Vec::new();
        for other in self.peers.iter().filter(|p| p.address != to) {
            for (_, item) in &other.owned {
                messages.push(create_message(other.index, item));
                for (seq, payload) in &item.updates {
                    messages.push(nerd::object_update(
                        item.router,
                        nerd::HANDLE_RELAY,
                        item.ident,
                        *seq,
                        payload,
                        Some(other.index),
                    ));
                }
            }
        }
        for m in messages {
            self.send(to, vec![m], true, 0);
        }
    }

    /// The peer leaves: its objects are deleted for the others (`peer.left` when it was synced).
    pub(super) fn drop_peer(&mut self, address: SocketAddr, why: Why) {
        let Some(pos) = self.position(address) else {
            return;
        };
        if why == Why::Kicked {
            // past the admission check: a kick revokes the grant first
            self.send_close(pos);
        }
        let peer = self.peers.remove(pos);
        note(why.note(), json!([peer.index]));
        if peer.synced {
            self.events.push(GameEvent::PeerLeft {
                xuid: peer.xuid,
                engine: ENGINE,
                peer: peer.index,
                reason: why.reason(),
            });
        }
        for (_, item) in &peer.owned {
            let seq = ((usize::from(item.seq) + 1 + item.updates.len()) % 255) as u8;
            let message = nerd::object_delete(
                item.router,
                nerd::HANDLE_RELAY,
                item.ident,
                seq,
                Some(peer.index),
            );
            for other in self.racers() {
                self.send(other, vec![message.clone()], true, 0);
            }
        }
        // Infinity: with nobody left, a launched race would never end (its end is the consoles'
        // var 7) and the world could never launch again
        if self.synced() == 0 && self.launched {
            self.end_race(&[]);
        }
        // W16: the owner left, so nothing to revoke; the next leader gets the options
        if self.owner == Some(address) {
            self.owner = None;
        }
        self.sync_owner();
    }

    /// SessionHost first: the lobby scene initializes once and only if it already exists (lobby
    /// spec §0). Then a first GameOptions, pushed through the Transfer handle (the spec's option
    /// 2) from the console's first options seq (0): the table, the server's defaults with the
    /// owner's changes merged (only the vars ever written: the console has FM5's defaults for the
    /// rest), cut into Updates that fit a packet. In a private lobby whose options nobody owns
    /// yet, the leader's grant (11, empty after the full Update) follows on the same queue, before
    /// any Ready.
    fn send_lobby(&mut self, to: SocketAddr) {
        let host = nerd::object_create(
            nerd::ROUTER_LOBBY,
            nerd::HANDLE_OWN,
            0,
            nerd::TYPE_SESSION_HOST,
            0,
            None,
            None,
        );
        let Some(pos) = self.position(to) else {
            return;
        };
        let grant = self.leader_owns_options
            && self.owner.is_none()
            && self.leader_peer().map(|p| p.address) == Some(to);
        let parts = options::chunks(self.options.vars(), OPTIONS_CHUNK_BITS);
        let p = &mut self.peers[pos];
        let mut options: Vec<(u8, Writer)> = parts
            .iter()
            .map(|part| {
                let seq = p.take_options_seq();
                (seq, options_update(seq, part))
            })
            .collect();
        if grant {
            let seq = p.take_options_seq();
            let index = p.index;
            options.push((
                seq,
                nerd::grant_ownership(
                    nerd::ROUTER_LOBBY,
                    nerd::HANDLE_TRANSFER,
                    0,
                    seq,
                    &Writer::new(),
                ),
            ));
            self.owner = Some(to);
            note(
                "options.granted",
                json!({"peer": index, "seq": seq, "bits": 0}),
            );
        }
        self.send(to, vec![host], true, 0);
        self.send_options(to, options);
        self.send_history(to);
    }

    pub(super) fn tick(&mut self, now: u64) {
        self.set_now(now);
        // held opens: taken in once their proof came, forgotten when it is no longer expected
        let held = std::mem::take(&mut self.held);
        for h in held {
            if self.allowed(h.user, h.from) {
                self.receive(&h.data, h.from, now);
            } else if now < h.until && self.admit.proof_expected(h.user, now) {
                self.held.push(h);
            }
        }
        self.race_tick(now);
        for address in self.peers.iter().map(|p| p.address).collect::<Vec<_>>() {
            let Some(pos) = self.position(address) else {
                continue;
            };
            let (xuid, last_heard) = (self.peers[pos].xuid, self.peers[pos].last_heard);
            if !self.allowed(xuid, address) {
                self.drop_peer(address, Why::NotAllowed);
                continue;
            }
            if now.saturating_sub(last_heard) > QUIET_MS {
                self.drop_peer(address, Why::Quiet);
                continue;
            }
            self.resend_lost(address, now);
            let Some(pos) = self.position(address) else {
                continue;
            };
            if self.peers[pos].synced && now >= self.peers[pos].next_ping {
                self.peers[pos].next_ping = now + PING_MS;
                let wire = self.now_ms();
                self.send(address, vec![nerd::ping(wire)], false, 0);
            }
        }
    }

    /// Drops every peer of `xuid` (each gets a Close); the count dropped.
    pub(super) fn kick(&mut self, xuid: u64, now: u64) -> usize {
        self.set_now(now);
        self.held.retain(|h| h.user != xuid);
        let mine: Vec<SocketAddr> = self
            .peers
            .iter()
            .filter(|p| p.xuid == xuid)
            .map(|p| p.address)
            .collect();
        for a in &mine {
            self.drop_peer(*a, Why::Kicked);
        }
        mine.len()
    }

    /// Every synced peer gets a Close, and the world forgets everybody (drain, shutdown).
    pub(super) fn close_all(&mut self) {
        for pos in 0..self.peers.len() {
            if self.peers[pos].synced {
                self.send_close(pos);
            }
        }
        self.peers.clear();
        self.held.clear();
        self.owner = None;
        self.end_race(&[]);
    }
}

fn create_message(owner: u8, item: &Owned) -> Writer {
    nerd::object_create(
        item.router,
        nerd::HANDLE_RELAY,
        item.ident,
        item.kind,
        item.seq,
        Some(&item.payload),
        Some(owner),
    )
}
