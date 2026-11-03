// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! One FH2 engine of a world (contract §7): the free-roam world every world has, and later the car
//! meets and Road Trip. Single-owner and deterministic: time, randomness, the launcher's policy
//! answers and the datagrams all come from the caller, so a golden vector replays it exactly.
//!
//! Layout of the port (the reference is the Infinity C# world engine at client `f81d09c`; its
//! behaviour is ported, not its code):
//!
//! - this file: the state, the admission of OpenConnections (handshake rate, held opens, refusals
//!   1-4), the policy seam and the world log;
//! - [`packets`]: a packet's messages, objects and their histories, the handshake's completion;
//! - [`send`]: packets out (acknowledgements, reliable queue and window, pacing, resends, the
//!   1,372-byte datagram limit), drops and the tick;
//! - [`join`]: quiet joins (a joiner's cars wait for its FinaliseSessionEntry);
//! - [`side`]: the car-state side channel;
//! - [`voice`]: voice (off, open, proximity), its mute layers, seats and state down;
//! - [`races`] and [`options`]: free-roam events, the options journal, event progress, the
//!   race-end hold and phasing;
//! - [`seams`]: the features of later steps (races, events, car meets, Road Trip, tag, bucket-list
//!   pairing, voice, phasing, the watch), as the no-ops free roam needs until they are ported.

pub mod bits;
mod carmeet;
pub mod dict;
pub mod history;
mod join;
pub mod options;
mod packets;
pub mod payload;
mod races;
pub mod random;
pub mod roadtrip;
mod seams;
mod send;
mod side;
mod tag;
pub mod teams;
mod voice;
pub mod weather;
pub mod wire;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use serde_json::{Value, json};

use self::{
    bits::Bits,
    dict::DotMap,
    history::History,
    random::{EngineRandom, SeedPurpose},
    weather::Weather,
    wire::{AckWindow, Header, Msg, Obj},
};
use super::settings::RelaySettings;
use crate::core::game::Send;

/// A queued or in-flight message. Shared, so the quiet join can tell its own release messages by
/// identity while they wait in a queue or a packet.
pub type M = Arc<Bits>;

/// One engine kind of a world (the `engine` field of logs and events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EngineKind {
    FreeRoam,
    CarMeet3,
    CarMeet6,
    RoadTrip,
}

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::FreeRoam => "freeroam",
            EngineKind::CarMeet3 => "carmeet3",
            EngineKind::CarMeet6 => "carmeet6",
            EngineKind::RoadTrip => "roadtrip",
        }
    }
}

/// The launcher's (or the admission's) answers the engine asks for per packet. A policy that is
/// not installed is not the same as one that answers false: without `authorize` everybody is
/// admitted and handshakes are keyed by address only; without `authorize_hook` the hook falls back
/// to `authorize` and the same-IP rule; without `awaits_proof` no open waits for its proof.
pub trait Policy: std::marker::Send {
    fn has_authorize(&self) -> bool;
    fn has_authorize_hook(&self) -> bool;
    fn authorize(&mut self, xuid: u64, from: SocketAddr, now: i64) -> bool;
    fn authorize_hook(&mut self, xuid: u64, from: SocketAddr, now: i64) -> bool;
    /// `None`: not installed.
    fn awaits_proof(&mut self, xuid: u64, now: i64) -> Option<bool>;
    /// Whether a car meet invitation can ask for its shared session (`world.gameSession.request`
    /// has somebody to answer it); not installed: the invitation is cancelled at once.
    fn can_create_game_session(&self) -> bool {
        false
    }
    /// The server's voice block of `xuid` now (CR-S67, contract §6.4: the two sources merged,
    /// compared with the wall clock); not installed: never blocked.
    fn voice_blocked(&mut self, xuid: u64, now: i64) -> bool {
        let _ = (xuid, now);
        false
    }
}

/// Admits nobody: an engine before its world's first admission snapshot (fail closed).
#[derive(Debug, Default)]
pub struct DenyAll;

impl Policy for DenyAll {
    fn has_authorize(&self) -> bool {
        true
    }
    fn has_authorize_hook(&self) -> bool {
        false
    }
    fn authorize(&mut self, _: u64, _: SocketAddr, _: i64) -> bool {
        false
    }
    fn authorize_hook(&mut self, _: u64, _: SocketAddr, _: i64) -> bool {
        false
    }
    fn awaits_proof(&mut self, _: u64, _: i64) -> Option<bool> {
        None
    }
    fn voice_blocked(&mut self, _: u64, _: i64) -> bool {
        false
    }
}

/// Why a peer left (the world log's `peer.dropped.<id>` and the `peer.left` event's reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Moved,
    Kicked,
    RateLimit,
    NotAllowedAtSync,
    StillActive(u8),
    TakenOver(u8),
    WorldFull,
    Doomed,
    NotAllowedAnyMore,
    NotAllowed,
    QueueFull,
    Reconnected,
    Closed,
    HistoryBits,
    HistoryEntries,
    OptionsOverflow,
    OptionsStalled,
    RoadTripAlone,
    SyncUnconfirmed,
    Unacknowledged(u32),
    Quiet(i64),
    NeverSynced,
}

impl Why {
    pub fn id(&self) -> &'static str {
        match self {
            Why::Moved => "peer.dropped.moved",
            Why::Kicked => "peer.dropped.kicked",
            Why::RateLimit => "peer.dropped.rate_limit",
            Why::NotAllowedAtSync => "peer.dropped.not_allowed_at_sync",
            Why::StillActive(_) => "peer.dropped.still_active",
            Why::TakenOver(_) => "peer.dropped.taken_over",
            Why::WorldFull => "peer.dropped.world_full",
            Why::Doomed => "peer.dropped.doomed",
            Why::NotAllowedAnyMore => "peer.dropped.not_allowed_any_more",
            Why::NotAllowed => "peer.dropped.not_allowed",
            Why::QueueFull => "peer.dropped.queue_full",
            Why::Reconnected => "peer.dropped.reconnected",
            Why::Closed => "peer.dropped.closed",
            Why::HistoryBits => "peer.dropped.history_bits",
            Why::HistoryEntries => "peer.dropped.history_entries",
            Why::OptionsOverflow => "peer.dropped.options_overflow",
            Why::OptionsStalled => "peer.dropped.options_stalled",
            Why::RoadTripAlone => "peer.dropped.roadtrip_alone",
            Why::SyncUnconfirmed => "peer.dropped.sync_unconfirmed",
            Why::Unacknowledged(_) => "peer.dropped.unacknowledged",
            Why::Quiet(_) => "peer.dropped.quiet",
            Why::NeverSynced => "peer.dropped.never_synced",
        }
    }

    fn args(&self) -> Vec<Value> {
        match *self {
            Why::StillActive(p) | Why::TakenOver(p) => vec![json!(p)],
            Why::Unacknowledged(n) => vec![json!(n)],
            Why::Quiet(ms) => vec![json!(ms)],
            _ => Vec::new(),
        }
    }

    /// The contract's `peer.left` reason (§4.5).
    pub fn left_reason(&self) -> &'static str {
        match self {
            Why::Closed => "close",
            Why::Quiet(_) | Why::NeverSynced | Why::SyncUnconfirmed | Why::Unacknowledged(_) => {
                "timeout"
            }
            Why::Kicked => "kicked",
            Why::NotAllowed | Why::NotAllowedAnyMore | Why::NotAllowedAtSync => "not_allowed",
            Why::Moved => "moved",
            Why::Doomed
            | Why::RateLimit
            | Why::QueueFull
            | Why::HistoryBits
            | Why::HistoryEntries
            | Why::OptionsOverflow
            | Why::OptionsStalled => "doomed",
            _ => "other",
        }
    }
}

/// What an engine tells its world (and a golden vector's `emit` lines).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A world-log line: a stable id and its arguments (peer indexes only, never an identity or an
    /// address).
    Note { id: &'static str, args: Vec<Value> },
    /// An OpenConnection past the handshake rate: the peer index (0 refused), lobby type A, B and
    /// the refusal (1-4).
    ConnectionOpened {
        index: u8,
        a: u32,
        b: u32,
        refusal: Option<u8>,
    },
    /// A sanitizer refusal.
    ValidationRejected {
        xuid: u64,
        kind: &'static str,
        message: Option<u32>,
        router: Option<u32>,
        value: Option<u32>,
    },
    /// A held OpenConnection admitted after its endpoint proof.
    AdmittedLate { endpoint: SocketAddr },
    /// A car meet invitation's §3.8 request: its id, the invite kind and the XUIDs (the starter
    /// first).
    CreateGameSession { id: u32, kind: u8, xuids: Vec<u64> },
    /// A peer synced (`peer.synced`).
    PeerSynced { xuid: u64, peer: u8 },
    /// A peer left (`peer.left` when it was synced).
    PeerLeft {
        xuid: u64,
        peer: u8,
        why: Why,
        synced: bool,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Reliable {
    id: u16,
    messages: Vec<M>,
    sent_at: i64,
    tries: u32,
}

/// One connection to a game.
#[derive(Debug)]
pub(crate) struct Peer {
    /// Unique for the engine's life (indexes are reused).
    serial: u64,
    endpoint: SocketAddr,
    index: u8,
    xuid: u64,
    attempt: u32,
    client_attempt: u32,
    completion_confirmed: bool,
    last_completion: i64,
    acks: AckWindow,
    next_packet: u16,
    in_flight: Vec<Reliable>,
    synced: bool,
    sync_index: u16,
    received_bytes: i64,
    sent_bytes: i64,
    console_id: Option<u32>,
    console_sequence: Option<u8>,
    console_latest_sequence: Option<u8>,
    finalise_asked: bool,
    finalised: bool,
    console_values: [u64; 9],
    console_variable_sequences: HashMap<u8, u8>,
    objects: DotMap<(u8, u32), History>,
    orphans: VecDeque<Obj>,
    last_seen: i64,
    joined_at: i64,
    rtt: Option<u32>,
    packet_tokens: f64,
    message_tokens: f64,
    byte_tokens: f64,
    bucket_at: i64,
    over_since: Option<i64>,
    over_last: Option<i64>,
    queue: VecDeque<M>,
    queue_bits: usize,
    doomed: bool,
    doomed_by: Option<Why>,
    car_state: Option<super::carstate::State>,
    car_position: Option<[f32; 3]>,
    car_id: Option<u32>,
    /// The last car prediction (its relay key and body), sent again with a newer time.
    last_car: Option<(wire::Handle, Bits)>,
    universe: u8,
    player_sequence: Option<u8>,
    last_time_sync: i64,
    universe_due: bool,
    car_hook: Option<SocketAddr>,
    car_state_at: Option<i64>,
    last_car_at: Option<i64>,
    last_car_body: Option<Bits>,
    /// Voice proximity's jump counter (teleports, new cars, a car known again).
    car_jumps: u8,
    /// The options' object sequences this player was sent (and when it last asked).
    lobby_sequences: HashMap<u32, (u8, i64)>,
    /// The car this player chose (PlayerChoices CarClass and PerfIndex).
    car_class: Option<i32>,
    car_pi: f32,
    /// A Ready refused for the car (T11): it counts once the next car fits.
    ready_refused: bool,
    /// The running team event's team (teams.rs) and the TeamId last written on its object.
    team: Option<u8>,
    team_written: Option<u8>,
    /// Road Trip: when this player's own Initialising ends, and its vote object's sequence.
    road_trip_start_at: Option<i64>,
    road_trip_selection_sequence: Option<u8>,
}

impl Peer {
    fn history_bits(&self) -> usize {
        self.objects
            .iter()
            .map(|(_, o)| o.history_bits())
            .sum::<usize>()
            + self.orphans.iter().map(|o| o.payload.len()).sum::<usize>()
    }

    fn history_entries(&self) -> usize {
        self.objects
            .iter()
            .map(|(_, o)| o.history_entries())
            .sum::<usize>()
            + self.orphans.len()
    }

    fn take_packet_id(&mut self) -> u16 {
        let r = self.next_packet;
        self.next_packet = if r >= 0xfffd { 0 } else { r + 1 };
        r
    }
}

#[derive(Debug, Clone)]
struct HeldOpen {
    data: Vec<u8>,
    header: Header,
    open: Open,
    at: i64,
}

/// An OpenConnection's fields the engine keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Open {
    version: Vec<u8>,
    game: Vec<u8>,
    flags: u8,
    user: u64,
    a: u32,
    b: u32,
    attempt: u32,
}

impl Open {
    fn of(m: &Msg) -> Option<Open> {
        match m {
            Msg::Open {
                version,
                game,
                flags,
                user,
                a,
                b,
                attempt,
            } => Some(Open {
                version: version.clone(),
                game: game.clone(),
                flags: *flags,
                user: *user,
                a: *a,
                b: *b,
                attempt: *attempt,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct SideRate {
    at: i64,
    packets: f64,
    bytes: f64,
    controls: f64,
}

const MAX_RETIRED_ATTEMPTS: usize = 1024;
const RETIRED_ATTEMPT_MS: i64 = 60_000;
const NOTES_PER_WINDOW: i32 = 40;
const NOTE_WINDOW_MS: i64 = 10_000;
pub const HELD_OPEN_MS: i64 = 20_000;
const MAX_HELD_OPENS: usize = 32;
/// SessionHostSynced var 0 (LobbyType) of a Road Trip world.
pub const ROAD_TRIP_LOBBY: u64 = 1;

/// The engine-level settings: `RelaySettings` plus what `world.open` sets per engine.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineSettings {
    pub relay: RelaySettings,
    pub max_players: usize,
    pub races: bool,
    pub car_meet_lobby: Option<u32>,
    /// T11 (tests only, never from `world.open`): refuse a Ready from a car outside the event's
    /// class or PI.
    pub enforce_car_rule: bool,
    /// An online Road Trip world (lobby type 1): its catalog, Storm Island and the vote's seed.
    pub road_trip: Option<roadtrip::RoadTripSettings>,
}

impl EngineSettings {
    /// The checks the reference engine's constructor makes.
    pub fn valid(&self) -> bool {
        let s = &self.relay;
        (1..=12).contains(&self.max_players)
            && s.max_peers_per_ip >= 1
            && (1..=254).contains(&s.max_unsynced)
            && s.rate_packets >= 1
            && s.rate_messages >= 1
            && s.rate_bytes >= 1
            && s.history_max_bits >= 1
            && s.history_max_entries >= 1
            && s.queue_max_bits >= 1
            && s.queue_max_messages >= 1
            && (1..=256).contains(&s.reliable_window)
            && s.race_prepare_ms >= 1
            && s.race_grid_ms >= 1
            && s.race_finish_ms >= 1
            && s.game_options_gap_ms >= 1
            && s.sync_complete_ms >= 1
            && s.resend_after_ms >= 1
            && s.time_sync_ms >= 1
            && self.car_meet_lobby.is_none_or(|l| l == 3 || l == 6)
    }
}

pub struct Engine {
    pub kind: EngineKind,
    pub settings: EngineSettings,
    random: EngineRandom,
    policy: Box<dyn Policy>,
    peers: Vec<Peer>,
    next_serial: u64,
    handshakes: DotMap<(IpAddr, u64), (i64, u64)>,
    bans: DotMap<IpAddr, i64>,
    retired: DotMap<(SocketAddr, u32), i64>,
    retired_order: VecDeque<((SocketAddr, u32), i64)>,
    host_variables: DotMap<u8, (u64, u32)>,
    next_index: u8,
    host_sequence: u8,
    group_node: i32,
    now: i64,
    last_keepalive: i64,
    last_resend: i64,
    last_pace: i64,
    last_car_states: i64,
    closed: bool,
    note_window: Option<i64>,
    notes: i32,
    notes_dropped: i32,
    held_opens: DotMap<SocketAddr, HeldOpen>,
    weather: Weather,
    weather_sequence: u8,
    /// Held joiners (by serial) and when they synced.
    holding: DotMap<u64, i64>,
    /// Released joiners waiting for their release to be acknowledged before the cars.
    cars_pending: DotMap<u64, (HashSet<usize>, i64)>,
    skipped_cars_noted: HashSet<(u8, u32)>,
    /// Each pending release's messages, kept alive while their identity is checked.
    release_keep: HashMap<u64, Vec<M>>,
    side_rates: DotMap<u64, SideRate>,
    pub side_channel_rate_drops: u64,
    bucket_slots_sequence: u8,
    /// Events, the options and phasing (races.rs).
    ev: races::Events,
    /// The running tag event (tag.rs) and its draws.
    tag: Option<tag::TagGame>,
    /// The car meet's invitation (carmeet.rs).
    car_meet: carmeet::CarMeet,
    /// The online Road Trip (roadtrip.rs).
    road_trip: Option<roadtrip::RoadTrip>,
    road_trip_sequence: u8,
    road_trip_state_sent: Option<i64>,
    tag_random: Option<random::LegacyRandom>,
    /// Team events (teams.rs).
    team_event: bool,
    team_pending: HashSet<u8>,
    team_random: Option<random::LegacyRandom>,
    /// Voice (voice.rs).
    voice: voice::Voice,
    out: Vec<Send>,
    events: Vec<Event>,
    /// Datagrams nobody could take (stats).
    pub dropped: u64,
    /// Messages too large for any datagram, dropped instead of sent.
    pub oversize_dropped: u64,
    /// Datagrams over 1,372 bytes, refused on arrival.
    pub oversize_in: u64,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("kind", &self.kind)
            .field("peers", &self.peers.len())
            .finish_non_exhaustive()
    }
}

fn shared_xuid(xuid: u64) -> bool {
    xuid == 0 || xuid == 0x0009_0000_0000_0000
}

fn canonical(a: SocketAddr) -> SocketAddr {
    crate::stun::canonical(a)
}

impl Engine {
    /// A new engine; `None` when the settings fail the engine's own checks.
    pub fn new(
        kind: EngineKind,
        settings: EngineSettings,
        mut random: EngineRandom,
        policy: Box<dyn Policy>,
    ) -> Option<Engine> {
        if !settings.valid() {
            return None;
        }
        let mut host_variables = DotMap::new();
        // SessionHostSynced var 0 LobbyType (3 bits; the game's default 2 is free roam)
        if let Some(lobby) = settings.car_meet_lobby {
            host_variables.insert(0u8, (u64::from(lobby), 3u32));
        } else if settings.road_trip.is_some() {
            // the game loads the session's entities for this value: a Road Trip world says 1
            host_variables.insert(0u8, (ROAD_TRIP_LOBBY, 3u32));
        }
        let seed = settings
            .relay
            .weather_seed
            .unwrap_or_else(|| random.seed(SeedPurpose::Weather));
        let weather = Weather::new(seed, 0, 1);
        let road_trip = settings.road_trip.clone().map(roadtrip::RoadTrip::new);
        Some(Engine {
            kind,
            settings,
            random,
            policy,
            peers: Vec::new(),
            next_serial: 1,
            handshakes: DotMap::new(),
            bans: DotMap::new(),
            retired: DotMap::new(),
            retired_order: VecDeque::new(),
            host_variables,
            next_index: 1,
            host_sequence: 0,
            group_node: 0,
            now: 0,
            last_keepalive: 0,
            last_resend: 0,
            last_pace: 0,
            last_car_states: 0,
            closed: false,
            note_window: None,
            notes: 0,
            notes_dropped: 0,
            held_opens: DotMap::new(),
            weather,
            weather_sequence: 0,
            holding: DotMap::new(),
            cars_pending: DotMap::new(),
            skipped_cars_noted: HashSet::new(),
            release_keep: HashMap::new(),
            side_rates: DotMap::new(),
            side_channel_rate_drops: 0,
            bucket_slots_sequence: 0,
            ev: races::Events::default(),
            tag: None,
            car_meet: carmeet::CarMeet::default(),
            road_trip,
            road_trip_sequence: 0,
            road_trip_state_sent: None,
            tag_random: None,
            team_event: false,
            team_pending: HashSet::new(),
            team_random: None,
            voice: voice::Voice::default(),
            out: Vec::new(),
            events: Vec::new(),
            dropped: 0,
            oversize_dropped: 0,
            oversize_in: 0,
        })
    }

    pub fn set_policy(&mut self, policy: Box<dyn Policy>) {
        self.policy = policy;
    }

    pub fn policy_mut(&mut self) -> &mut dyn Policy {
        self.policy.as_mut()
    }

    /// The datagrams sent since the last call, in the order sent.
    pub fn take_out(&mut self) -> Vec<Send> {
        std::mem::take(&mut self.out)
    }

    /// The events since the last call, in order.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// The synced peers' XUIDs, in engine order.
    pub fn peer_xuids(&self) -> Vec<u64> {
        self.peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| p.xuid)
            .collect()
    }

    /// When the newest synced loopback peer (the host's own game) synced; `None`: none.
    pub fn loopback_synced_at(&self) -> Option<i64> {
        self.peers
            .iter()
            .filter(|p| p.synced && p.endpoint.ip().is_loopback())
            .map(|p| p.joined_at)
            .max()
    }

    pub fn has_peer(&self, endpoint: SocketAddr) -> bool {
        let e = canonical(endpoint);
        self.peers.iter().any(|p| p.endpoint == e)
    }

    pub fn synced(&self) -> usize {
        self.peers.iter().filter(|p| p.synced).count()
    }

    pub fn handshaking(&self) -> usize {
        self.peers.iter().filter(|p| !p.synced).count() + self.held_opens.len()
    }

    /// Peers (synced or handshaking) and held opens of any XUID but `except` (the host's own
    /// game is not an occupant: `world.empty`/`world.occupied`).
    pub fn occupants(&self, except: u64) -> usize {
        self.peers.iter().filter(|p| p.xuid != except).count()
            + self
                .held_opens
                .iter()
                .filter(|(_, h)| h.open.user != except)
                .count()
    }

    /// No peer, no held open: nothing a tick could do.
    pub fn is_idle(&self) -> bool {
        self.peers.is_empty() && self.held_opens.is_empty()
    }

    /// The lobby type (A) a datagram's first message asks for, when it is an OpenConnection.
    pub fn lobby_type_of(data: &[u8]) -> Option<u32> {
        if crate::stun::is_stun(data) {
            return None;
        }
        let h = Header::parse(data)?;
        if h.count == 0 {
            return None;
        }
        match wire::read(&mut bits::Rd::new(&data[wire::HEADER_LEN..])).ok()? {
            Msg::Open { a, .. } => Some(a),
            _ => None,
        }
    }

    /// A world that does not serve this datagram's lobby type (a public world runs one
    /// activity): its OpenConnection gets refusal 4, past the handshake rate.
    pub fn refuse_open(&mut self, data: &[u8], from: SocketAddr, now_ms: i64) {
        if self.closed || data.len() > wire::MAX_DATAGRAM || crate::stun::is_stun(data) {
            return;
        }
        self.now = self.now.max(now_ms);
        let from = canonical(from);
        let Some(header) = Header::parse(data) else {
            return;
        };
        if header.count == 0 {
            return;
        }
        let Ok(m) = wire::read(&mut bits::Rd::new(&data[wire::HEADER_LEN..])) else {
            return;
        };
        let Some(open) = Open::of(&m) else { return };
        if self.handshake_allowed(from, open.user) {
            self.refuse(from, header, &open, data.len(), 4);
        }
    }

    fn now_wire(&self) -> u32 {
        (self.now as u32).wrapping_add(1)
    }

    fn pos(&self, serial: u64) -> Option<usize> {
        self.peers.iter().position(|p| p.serial == serial)
    }

    fn peer(&self, serial: u64) -> Option<&Peer> {
        self.peers.iter().find(|p| p.serial == serial)
    }

    fn peer_mut(&mut self, serial: u64) -> Option<&mut Peer> {
        self.peers.iter_mut().find(|p| p.serial == serial)
    }

    fn alive(&self, serial: u64) -> bool {
        self.pos(serial).is_some()
    }

    fn synced_serials_except(&self, except: Option<u64>) -> Vec<u64> {
        self.peers
            .iter()
            .filter(|p| p.synced && Some(p.serial) != except)
            .map(|p| p.serial)
            .collect()
    }

    // ---- the world log ----

    /// A world-log line, bounded: at most 40 per 10 s, then one count of the suppressed ones.
    fn note(&mut self, id: &'static str, args: Vec<Value>) {
        if self
            .note_window
            .is_none_or(|w| self.now - w >= NOTE_WINDOW_MS)
        {
            if self.notes_dropped != 0 {
                self.events.push(Event::Note {
                    id: "notes.suppressed",
                    args: vec![json!(self.notes_dropped)],
                });
            }
            self.note_window = Some(self.now);
            self.notes = 0;
            self.notes_dropped = 0;
        }
        self.notes += 1;
        if self.notes > NOTES_PER_WINDOW {
            self.notes_dropped += 1;
            return;
        }
        self.events.push(Event::Note { id, args });
    }

    fn rejected(
        &mut self,
        xuid: u64,
        kind: &'static str,
        message: Option<u32>,
        router: Option<u32>,
        value: Option<u32>,
    ) {
        self.events.push(Event::ValidationRejected {
            xuid,
            kind,
            message,
            router,
            value,
        });
    }

    // ---- the policy seam ----

    fn allowed(&mut self, xuid: u64, endpoint: SocketAddr) -> bool {
        if self.policy.has_authorize() {
            self.policy.authorize(xuid, endpoint, self.now)
        } else {
            true
        }
    }

    fn allowed_hook(&mut self, xuid: u64, endpoint: SocketAddr) -> bool {
        if self.policy.has_authorize_hook() {
            self.policy.authorize_hook(xuid, endpoint, self.now)
        } else {
            self.allowed(xuid, endpoint)
        }
    }

    fn hook_source(&mut self, serial: u64, endpoint: SocketAddr) -> bool {
        let Some(p) = self.peer(serial) else {
            return false;
        };
        let (xuid, peer_endpoint) = (p.xuid, p.endpoint);
        if self.policy.has_authorize_hook() {
            self.allowed_hook(xuid, endpoint)
        } else {
            side::may_speak_for(peer_endpoint, endpoint)
        }
    }

    // ---- admission ----

    fn retired_attempt(&mut self, endpoint: SocketAddr, attempt: u32) -> bool {
        while let Some((key, at)) = self.retired_order.front().cloned() {
            if self.now - at < RETIRED_ATTEMPT_MS {
                break;
            }
            self.retired_order.pop_front();
            self.retired.remove(&key);
        }
        self.retired.contains_key(&(endpoint, attempt))
    }

    fn retire_attempt(&mut self, serial: u64) {
        let Some(p) = self.peer(serial) else { return };
        let key = (p.endpoint, p.client_attempt);
        if self.retired_attempt(key.0, key.1) {
            return;
        }
        while self.retired_order.len() >= MAX_RETIRED_ATTEMPTS {
            if let Some((k, _)) = self.retired_order.pop_front() {
                self.retired.remove(&k);
            }
        }
        self.retired.insert(key, self.now);
        self.retired_order.push_back((key, self.now));
    }

    fn handshake_allowed(&mut self, endpoint: SocketAddr, xuid: u64) -> bool {
        // an installed policy has verified this identity and endpoint: people sharing a NAT do not
        // collapse into one bucket
        let keyed = self.policy.has_authorize() && self.allowed(xuid, endpoint);
        let key = (endpoint.ip(), if keyed { xuid } else { 0 });
        let hps = self.settings.relay.handshakes_per_second;
        if let Some(&(at, count)) = self.handshakes.get(&key) {
            let bucket = if self.now - at >= 1000 {
                (self.now, 1)
            } else {
                (at, count + 1)
            };
            self.handshakes.insert(key, bucket);
            return bucket.1 <= hps;
        }
        let now = self.now;
        let expired: Vec<_> = self
            .handshakes
            .iter()
            .filter(|(_, v)| now - v.0 >= 1000)
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            self.handshakes.remove(&k);
        }
        if self.handshakes.len() >= 1024
            && let Some(k) = self.handshakes.first_key()
        {
            self.handshakes.remove(&k);
        }
        self.handshakes.insert(key, (now, 1));
        hps >= 1
    }

    /// An OpenConnection from an unknown endpoint: a new peer, a held open or a refusal.
    fn admit(
        &mut self,
        endpoint: SocketAddr,
        header: Header,
        open: &Open,
        data: &[u8],
    ) -> Option<u64> {
        if !self.handshake_allowed(endpoint, open.user) {
            return None;
        }
        // a seat whose endpoint proof may still come: no final refusal, the open waits for it
        if !self.allowed(open.user, endpoint) && self.hold(endpoint, header, open, data) {
            return None;
        }
        let s = self.settings.clone();
        let reason = if !self.allowed(open.user, endpoint) {
            Some(4)
        } else if self.bans.get(&endpoint.ip()).copied().unwrap_or(0) > self.now {
            Some(3)
        } else if open.version != wire::VERSION {
            Some(1)
        } else if open.game != wire::GAME {
            Some(2)
        } else {
            let unsynced_here = self
                .peers
                .iter()
                .filter(|p| !p.synced && p.endpoint.ip() == endpoint.ip())
                .count() as u64;
            let unsynced = self.peers.iter().filter(|p| !p.synced).count() as u64;
            let full = self.synced() >= s.max_players
                && (shared_xuid(open.user)
                    || !self.peers.iter().any(|p| p.synced && p.xuid == open.user));
            (unsynced_here >= s.relay.max_peers_per_ip || unsynced >= s.relay.max_unsynced || full)
                .then_some(4)
        };
        if let Some(refusal) = reason {
            self.refuse(endpoint, header, open, data.len(), refusal);
            return None;
        }
        let mut index = 0u8;
        for i in 0..254u16 {
            let candidate = ((u16::from(self.next_index) - 1 + i) % 254 + 1) as u8;
            if self.peers.iter().all(|p| p.index != candidate) {
                index = candidate;
                self.next_index = (u16::from(candidate) % 254 + 1) as u8;
                break;
            }
        }
        if index == 0 {
            return None;
        }
        let attempt = self.random.attempt();
        let serial = self.next_serial;
        self.next_serial += 1;
        let r = &self.settings.relay;
        self.peers.push(Peer {
            serial,
            endpoint,
            index,
            xuid: open.user,
            attempt,
            client_attempt: open.attempt,
            completion_confirmed: false,
            last_completion: 0,
            acks: AckWindow::default(),
            next_packet: 1,
            in_flight: Vec::new(),
            synced: false,
            sync_index: 0,
            received_bytes: 0,
            sent_bytes: 0,
            console_id: None,
            console_sequence: None,
            console_latest_sequence: None,
            finalise_asked: false,
            finalised: false,
            console_values: [0; 9],
            console_variable_sequences: HashMap::new(),
            objects: DotMap::new(),
            orphans: VecDeque::new(),
            last_seen: self.now,
            joined_at: self.now,
            rtt: None,
            packet_tokens: r.rate_packets as f64 * 2.0,
            message_tokens: r.rate_messages as f64 * 2.0,
            byte_tokens: r.rate_bytes as f64 * 2.0,
            bucket_at: self.now,
            over_since: None,
            over_last: None,
            queue: VecDeque::new(),
            queue_bits: 0,
            doomed: false,
            doomed_by: None,
            car_state: None,
            car_position: None,
            car_id: None,
            last_car: None,
            universe: 0,
            player_sequence: None,
            last_time_sync: 0,
            universe_due: false,
            car_hook: None,
            car_state_at: None,
            last_car_at: None,
            last_car_body: None,
            car_jumps: 0,
            lobby_sequences: HashMap::new(),
            car_class: None,
            car_pi: f32::NAN,
            ready_refused: false,
            team: None,
            team_written: None,
            road_trip_start_at: None,
            road_trip_selection_sequence: None,
        });
        self.events.push(Event::ConnectionOpened {
            index,
            a: open.a,
            b: open.b,
            refusal: None,
        });
        Some(serial)
    }

    fn refuse(
        &mut self,
        endpoint: SocketAddr,
        header: Header,
        open: &Open,
        bytes: usize,
        refusal: u8,
    ) {
        self.events.push(Event::ConnectionOpened {
            index: 0,
            a: open.a,
            b: open.b,
            refusal: Some(refusal),
        });
        let m = wire::write(&Msg::OpenResponse {
            flags: open.flags,
            reason: refusal,
            attempt: open.attempt,
            own_attempt: 0,
        });
        let ack = if header.id == wire::NO_SEQUENCE {
            wire::NO_ACK
        } else {
            header.id
        };
        let response = wire::packet([&m], wire::NO_SEQUENCE, ack, 0);
        // never more bytes back than came in (no amplification)
        if response.len() <= bytes {
            self.emit(endpoint, response);
        }
    }

    /// An OpenConnection from an endpoint not proven yet, for an identity whose proof may still
    /// come: kept unanswered (the game takes refusal 4 as final) and taken in again once allowed.
    fn hold(&mut self, endpoint: SocketAddr, header: Header, open: &Open, data: &[u8]) -> bool {
        if shared_xuid(open.user) || self.policy.awaits_proof(open.user, self.now) != Some(true) {
            return false;
        }
        if let Some(held) = self.held_opens.get(&endpoint) {
            if held.open.user != open.user {
                return false;
            }
            // the latest attempt; the wait is not extended
            let at = held.at;
            self.held_opens.insert(
                endpoint,
                HeldOpen {
                    data: data.to_vec(),
                    header,
                    open: open.clone(),
                    at,
                },
            );
            return true;
        }
        if self.held_opens.len() >= MAX_HELD_OPENS {
            return false;
        }
        self.held_opens.insert(
            endpoint,
            HeldOpen {
                data: data.to_vec(),
                header,
                open: open.clone(),
                at: self.now,
            },
        );
        self.note("open.held", vec![json!(open.a)]);
        true
    }

    fn tick_held_opens(&mut self) {
        if self.held_opens.is_empty() {
            return;
        }
        let all: Vec<(SocketAddr, HeldOpen)> = self
            .held_opens
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        for (endpoint, held) in all {
            if self.peers.iter().any(|p| p.endpoint == endpoint) {
                self.held_opens.remove(&endpoint); // a retry got in
                continue;
            }
            if self.allowed(held.open.user, endpoint) {
                self.held_opens.remove(&endpoint);
                if self.receive_packet(&held.data, endpoint).is_err() {
                    continue;
                }
                if self.peers.iter().any(|p| p.endpoint == endpoint) {
                    let waited = self.now - held.at;
                    self.note(
                        "open.admitted_late",
                        vec![json!(held.open.a), json!(waited)],
                    );
                    self.events.push(Event::AdmittedLate { endpoint });
                }
                continue;
            }
            if self.now - held.at < HELD_OPEN_MS
                && self.policy.awaits_proof(held.open.user, self.now) == Some(true)
            {
                continue;
            }
            self.held_opens.remove(&endpoint); // no proof came: the refusal it was spared
            self.refuse(endpoint, held.header, &held.open, held.data.len(), 4);
        }
    }

    fn take_rate(&mut self, serial: u64, header: Header, length: usize) -> bool {
        let now = self.now;
        let r = self.settings.relay.clone();
        let Some(p) = self.peer_mut(serial) else {
            return false;
        };
        let elapsed = (now - p.bucket_at).max(0) as f64 / 1000.0;
        p.packet_tokens =
            (r.rate_packets as f64 * 2.0).min(p.packet_tokens + elapsed * r.rate_packets as f64);
        p.message_tokens =
            (r.rate_messages as f64 * 2.0).min(p.message_tokens + elapsed * r.rate_messages as f64);
        p.byte_tokens =
            (r.rate_bytes as f64 * 2.0).min(p.byte_tokens + elapsed * r.rate_bytes as f64);
        p.bucket_at = now;
        let count = f64::from(header.count);
        if p.packet_tokens >= 1.0 && p.message_tokens >= count && p.byte_tokens >= length as f64 {
            p.packet_tokens -= 1.0;
            p.message_tokens -= count;
            p.byte_tokens -= length as f64;
            return true;
        }
        if p.over_last.is_none_or(|last| now - last >= 1500) {
            p.over_since = Some(now);
        }
        p.over_last = Some(now);
        if p.over_since.is_some_and(|s| now - s >= r.abuse_ms as i64) {
            let ip = p.endpoint.ip();
            send::doom(p, Why::RateLimit);
            let expired: Vec<IpAddr> = self
                .bans
                .iter()
                .filter(|(_, until)| **until <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in expired {
                self.bans.remove(&k);
            }
            if self.bans.len() >= 256
                && let Some(k) = self.bans.first_key()
            {
                self.bans.remove(&k);
            }
            self.bans.insert(ip, now + r.abuse_ban_ms as i64);
        }
        false
    }

    // ---- launcher inputs ----

    /// A datagram from `from` at `now_ms`.
    pub fn receive(&mut self, data: &[u8], from: SocketAddr, now_ms: i64) {
        if self.closed {
            return;
        }
        self.now = self.now.max(now_ms);
        // no datagram over the game's 1,372-byte limit is read at all (nothing legitimate is
        // larger; one that is could carry a message no copy of it fits)
        if data.len() > wire::MAX_DATAGRAM {
            self.dropped += 1;
            self.oversize_in += 1;
            return;
        }
        let from = canonical(from);
        if crate::stun::is_stun(data) {
            self.receive_side_channel(data, from);
            return;
        }
        // a malformed packet ends at the bad message; what it already did stands
        let _ = self.receive_packet(data, from);
        self.check_cars_pending();
        self.drop_doomed();
    }

    /// Drops every peer of `xuid` (`world.kick`); the count dropped.
    pub fn kick(&mut self, xuid: u64, now_ms: i64) -> usize {
        self.now = self.now.max(now_ms);
        let serials: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.xuid == xuid)
            .map(|p| p.serial)
            .collect();
        for s in &serials {
            self.drop_peer(*s, Why::Kicked);
        }
        serials.len()
    }

    /// The endpoint's game moved to another world on this host: its seat goes, with no Close.
    pub fn forget(&mut self, endpoint: SocketAddr, now_ms: i64) {
        self.now = self.now.max(now_ms);
        let endpoint = canonical(endpoint);
        self.held_opens.remove(&endpoint);
        let serials: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.endpoint == endpoint)
            .map(|p| p.serial)
            .collect();
        for s in serials {
            self.drop_peer(s, Why::Moved);
        }
    }

    /// The world closes: everything goes, nothing more is sent or answered.
    pub fn clear(&mut self) {
        self.closed = true;
        self.peers.clear();
        self.handshakes.clear();
        self.bans.clear();
        self.host_variables.clear();
        self.retired.clear();
        self.retired_order.clear();
        self.held_opens.clear();
        self.holding.clear();
        self.cars_pending.clear();
        self.release_keep.clear();
        self.skipped_cars_noted.clear();
        self.side_rates.clear();
        self.side_channel_rate_drops = 0;
        self.voice = voice::Voice::default();
        self.ev.options.clear();
        self.ev.options_outlived = false;
        self.event_progress_reset();
        self.ev.event_side.clear();
        self.ev.phasing = false;
        // the C# Clear keeps them; nothing reads them after the close, but a closed engine holds
        // no event (MACBOOK decision 4)
        self.ev.race = None;
        self.ev.event_owner = None;
    }

    /// Sends every synced peer the game's Close (drain, shutdown), then clears.
    pub fn close_all(&mut self) {
        let serials: Vec<u64> = self.peers.iter().map(|p| p.serial).collect();
        for s in serials {
            if let Some(p) = self.peer(s)
                && p.synced
            {
                let (ack, bits) = p.acks.header();
                let endpoint = p.endpoint;
                let close = wire::write(&Msg::Close);
                self.emit(
                    endpoint,
                    wire::packet([&close], wire::NO_SEQUENCE, ack, bits),
                );
            }
        }
        self.clear();
    }

    fn emit(&mut self, to: SocketAddr, data: Vec<u8>) {
        self.out.push(Send { to, data });
    }
}
