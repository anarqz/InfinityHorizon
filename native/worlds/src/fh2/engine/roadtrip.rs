// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Online Road Trip (lobby type 1). The server owns RoadTripSynced (router 9, type 0, id 0; 4-bit
//! var ids) and runs its state: a destination vote, car select, the drive, the destination's
//! four events one after another, then a new vote. The vote is the players' RoadTripEventSelection
//! transfer object (router 9, id 0), filled by the server. Players report where they are on their
//! own RoadTripPlayerSynced (router 9, type 2), which the server relays and reads. The first
//! player to arrive gives the others a short time to follow; everybody arriving ends the drive.
//!
//! OD-194 (amends OD-183's start): the first player opens the destination vote, alone or not, and
//! players arriving during it are shown it with its time left and vote. A Road Trip is still for
//! two players or more (OD-183): a vote that ends with fewer than two sends its player back; after
//! the vote a player left alone is sent back 10 s after the others left; an event with fewer than
//! two ready is dropped and the trip goes on. The trip takes newcomers only until its vote ends
//! ([`Engine::road_trip_in_progress`], announced so matchmaking places nobody else in it).
//!
//! The catalog (the game's online events, their modes' minimum players and allowed car classes)
//! reaches the engine as rows through the control channel (`catalog.set`, contract §9); the
//! engine reads no game file.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use serde_json::json;

use super::{
    Engine,
    bits::Bits,
    key::ObjKey,
    message::{Message, ObjectChange},
    races::{MIN_EVENT_PARTICIPANTS, Race, Stage},
    random::LegacyRandom,
    send,
};
use crate::fh2::codec::object::seq_next;

/// One of the game's online events: its catalog id, location, race mode, the players its mode
/// needs and the car classes it allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoadTripEvent {
    pub id: i32,
    pub location: i32,
    pub mode: i32,
    pub min_players: i32,
    pub classes: BTreeSet<i32>,
}

/// A destination on the vote: its location, its four events (and the players each needs) and
/// the car class they share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoadTripDestination {
    pub location: i32,
    pub events: Vec<i32>,
    pub min_players: Vec<i32>,
    pub car_class: i32,
}

/// The game's Road Trip destinations (3..9; 9 is Storm Island, an expansion).
pub const FIRST_LOCATION: i32 = 3;
pub const LAST_LOCATION: i32 = 9;
pub const STORM_ISLAND: i32 = 9;
/// CarClasses 0..5 (D..S2).
const CLASSES: i32 = 6;
pub const EVENTS_PER_DESTINATION: usize = 4;
pub const DESTINATIONS: usize = 3;
/// NR-10: an event's team twin is its id + 10000.
const TEAM_VARIANT_OFFSET: i32 = super::teams::TEAM_VARIANT_OFFSET;
/// A destination offers one event as its team twin from this many players.
const TEAM_PLAYERS: usize = 4;
/// The race modes that rank the whole field, the only ones offered as a team twin.
const TEAM_MODES: [i32; 6] = [0, 1, 2, 4, 5, 31];

/// The Road Trip catalog: the online events at the destinations, in catalog order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoadTripCatalog {
    pub events: Vec<RoadTripEvent>,
}

/// `OrderBy(_ => random.Next())`: one key per element, in order, then a stable sort.
fn shuffled<T>(items: Vec<T>, random: &mut LegacyRandom) -> Vec<T> {
    let mut keyed: Vec<(i32, usize, T)> = items
        .into_iter()
        .enumerate()
        .map(|(i, x)| (random.next_int(), i, x))
        .collect();
    keyed.sort_by_key(|(k, i, _)| (*k, *i));
    keyed.into_iter().map(|(_, _, x)| x).collect()
}

impl RoadTripCatalog {
    /// The events at a destination (location 3..9); minimum players at least 1.
    pub fn new(events: impl IntoIterator<Item = RoadTripEvent>) -> RoadTripCatalog {
        RoadTripCatalog {
            events: events
                .into_iter()
                .filter(|e| (FIRST_LOCATION..=LAST_LOCATION).contains(&e.location))
                .map(|mut e| {
                    e.min_players = e.min_players.max(1);
                    e
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Three destinations for `players` players: only events their mode can run with them, each
    /// destination four distinct events that share one allowed class (modes mixed), the three
    /// locations and where possible the three classes different; Storm Island only when the host
    /// has it. With 4 players or more, each destination offers one event that ranks the field as
    /// its team twin. `None` when the catalog cannot fill three destinations. Draw order as the
    /// reference engine draws (the seeded vote replays exactly).
    pub fn draw(
        &self,
        players: usize,
        storm_island: bool,
        random: &mut LegacyRandom,
    ) -> Option<Vec<RoadTripDestination>> {
        let eligible: Vec<&RoadTripEvent> = self
            .events
            .iter()
            .filter(|e| {
                e.id < TEAM_VARIANT_OFFSET
                    && e.min_players <= players.max(1) as i32
                    && (storm_island || e.location != STORM_ISLAND)
            })
            .collect();
        let mut distinct: Vec<i32> = Vec::new();
        for e in &eligible {
            if !distinct.contains(&e.location) {
                distinct.push(e.location);
            }
        }
        let locations = shuffled(distinct, random);
        let mut chosen = Vec::new();
        let mut used: HashSet<i32> = HashSet::new();
        for location in locations {
            if chosen.len() == DESTINATIONS {
                break;
            }
            let here: Vec<&RoadTripEvent> = eligible
                .iter()
                .copied()
                .filter(|e| e.location == location)
                .collect();
            // a class not used yet first, then any class; each tried in a random order
            let mut order: Vec<(bool, i32, usize, i32)> = (0..CLASSES)
                .enumerate()
                .map(|(i, c)| (used.contains(&c), random.next_int(), i, c))
                .collect();
            order.sort_by_key(|(u, k, i, _)| (*u, *k, *i));
            for (_, _, _, class) in order {
                let fits: Vec<&RoadTripEvent> = here
                    .iter()
                    .copied()
                    .filter(|e| e.classes.contains(&class))
                    .collect();
                if fits.len() < EVENTS_PER_DESTINATION {
                    continue;
                }
                let picked = mix(&fits, random);
                let mut ids: Vec<i32> = picked.iter().map(|e| e.id).collect();
                let mut minimum: Vec<i32> = picked.iter().map(|e| e.min_players).collect();
                let teamable: Vec<usize> = (0..picked.len())
                    .filter(|k| TEAM_MODES.contains(&picked[*k].mode))
                    .collect();
                if players >= TEAM_PLAYERS && !teamable.is_empty() {
                    let k = teamable[random.next_below(teamable.len() as i32) as usize];
                    ids[k] += TEAM_VARIANT_OFFSET;
                    minimum[k] = minimum[k].max(TEAM_PLAYERS as i32);
                }
                chosen.push(RoadTripDestination {
                    location,
                    events: ids,
                    min_players: minimum,
                    car_class: class,
                });
                used.insert(class);
                break;
            }
        }
        (chosen.len() == DESTINATIONS).then_some(chosen)
    }
}

/// Four events, round robin over their race modes in a random order (the groups' own orders are
/// drawn first, group by group, then the groups' order).
fn mix<'a>(events: &[&'a RoadTripEvent], random: &mut LegacyRandom) -> Vec<&'a RoadTripEvent> {
    let mut modes: Vec<i32> = Vec::new();
    for e in events {
        if !modes.contains(&e.mode) {
            modes.push(e.mode);
        }
    }
    let groups: Vec<Vec<&RoadTripEvent>> = modes
        .iter()
        .map(|m| {
            let g: Vec<&RoadTripEvent> = events.iter().copied().filter(|e| e.mode == *m).collect();
            shuffled(g, random)
        })
        .collect();
    let mut pools: Vec<std::collections::VecDeque<&RoadTripEvent>> = shuffled(groups, random)
        .into_iter()
        .map(Into::into)
        .collect();
    let mut picked = Vec::with_capacity(EVENTS_PER_DESTINATION);
    while picked.len() < EVENTS_PER_DESTINATION {
        for pool in pools.iter_mut().filter(|p| !p.is_empty()) {
            if picked.len() < EVENTS_PER_DESTINATION
                && let Some(e) = pool.pop_front()
            {
                picked.push(e);
            }
        }
    }
    picked
}

/// An online Road Trip world's settings: its catalog, whether the host has Storm Island, and the
/// vote's seed.
#[derive(Debug, Clone, PartialEq)]
pub struct RoadTripSettings {
    pub catalog: Arc<RoadTripCatalog>,
    pub storm_island: bool,
    pub seed: i32,
}

/// RoadTripServerState (SAURON's RE: the game's enum includes Solo = 3, which this server never
/// sends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoadTripState {
    Initialising = 0,
    DestinationVote = 1,
    CarSelect = 2,
    Solo = 3,
    DriveToEvent = 4,
    ArrivedAtDestination = 5,
    PrepareForEvent = 6,
    InEvent = 7,
    FinaliseEvent = 8,
}

impl RoadTripState {
    fn name_of(value: i64) -> String {
        let n = match value {
            0 => "Initialising",
            1 => "DestinationVote",
            2 => "CarSelect",
            3 => "Solo",
            4 => "DriveToEvent",
            5 => "ArrivedAtDestination",
            6 => "PrepareForEvent",
            7 => "InEvent",
            8 => "FinaliseEvent",
            v => return v.to_string(),
        };
        n.to_string()
    }

    fn name(self) -> String {
        Self::name_of(self as i64)
    }
}

/// OD-194: a fresh trip's vote opens once its first player's own Initialising is over (FH2MP's
/// 3 s too), alone or not.
const INITIALISING_MS: i64 = PLAYER_START_MS;
const VOTE_MS: i64 = 60_000;
const ALL_VOTED_MS: i64 = 10_000;
/// A player's RoadTripSynced is created in Initialising and changes state this long after (the
/// game leaves its wait on a state change).
const PLAYER_START_MS: i64 = 3000;
const CAR_SELECT_MS: i64 = 30_000;
const ALL_PICKED_MS: i64 = 3000;
const CONFIGURE_MS: i64 = 12_000;
const DRIVE_CAP_MS: i64 = 300_000;
const FIRST_ARRIVAL_MS: i64 = 30_000;
const DRIVE_HOLD_MS: i64 = 180_000;
const PREPARE_MS: i64 = 120_000;
/// OD-183: how long the last player of a trip the others left stays (after the vote; a vote
/// always runs to its end, OD-194).
const ALONE_MS: i64 = 10_000;
/// A router-1 update this large during car select is a player's car choice.
pub const CAR_CHOICE_BITS: usize = 1000;
/// LobbyPlayer.PlayerHost on the player object (the game's "local player leads").
const LEADER_VARIABLE: u64 = 13;
/// The player object's own IsLeader.
const IS_LEADER_VARIABLE: u64 = 5;
const FINALISE_MS: i64 = 6000;

#[derive(Debug, Clone)]
pub(super) struct RoadTrip {
    settings: RoadTripSettings,
    random: LegacyRandom,
    state: RoadTripState,
    until: i64,
    first_seen_at: i64,
    plan: Option<Vec<RoadTripDestination>>,
    votes: HashMap<u8, i32>,
    picked: HashSet<u8>,
    player_states: HashMap<u8, i32>,
    winner: Option<usize>,
    event_index: usize,
    last_race: Option<i32>,
    configured: bool,
    configure_until: i64,
    hold_drive: bool,
    hold_until: i64,
    leader: Option<u8>,
    alone_since: Option<i64>,
    last_dropped: bool,
}

impl RoadTrip {
    pub(super) fn new(settings: RoadTripSettings) -> RoadTrip {
        RoadTrip {
            random: LegacyRandom::new(settings.seed),
            settings,
            state: RoadTripState::Initialising,
            until: i64::MAX,
            first_seen_at: -1,
            plan: None,
            votes: HashMap::new(),
            picked: HashSet::new(),
            player_states: HashMap::new(),
            winner: None,
            event_index: 0,
            last_race: None,
            configured: false,
            configure_until: 0,
            hold_drive: false,
            hold_until: 0,
            leader: None,
            alone_since: None,
            last_dropped: false,
        }
    }

    fn current_race(&self) -> Option<i32> {
        let w = self.winner?;
        self.plan
            .as_ref()?
            .get(w)?
            .events
            .get(self.event_index)
            .copied()
    }

    fn location(&self) -> i64 {
        self.winner
            .and_then(|w| self.plan.as_ref()?.get(w))
            .map_or(0, |d| i64::from(d.location))
    }
}

/// RoadTripSynced variables: a 4-bit id and the value at its width.
fn variables(vars: &[(u8, i64)]) -> Bits {
    let mut out = Bits::new();
    for (var, value) in vars {
        let width = match var {
            1 => 3,
            4 | 5 => 14,
            7 => 1,
            8 => 4,
            _ => 32,
        };
        out.push(u64::from(*var), 4)
            .push((*value as u64) & ((1u64 << width) - 1), width);
    }
    out
}

fn wire_at(engine_ms: i64) -> i64 {
    i64::from((engine_ms as u32).wrapping_add(1))
}

impl Engine {
    /// The running Road Trip event's catalog id (its team twin is id + 10000).
    pub(super) fn road_trip_current_race(&self) -> Option<i32> {
        self.road_trip.as_ref()?.current_race()
    }

    /// OD-194: the trip is past its destination vote and takes no newcomers (matchmaking places a
    /// Road Trip newcomer in a lobby still voting, or a fresh one).
    pub fn road_trip_in_progress(&self) -> bool {
        self.road_trip.as_ref().is_some_and(|t| {
            !matches!(
                t.state,
                RoadTripState::Initialising | RoadTripState::DestinationVote
            )
        })
    }

    fn trip(&mut self) -> &mut RoadTrip {
        self.road_trip.as_mut().expect("a Road Trip engine")
    }

    /// The whole RoadTripSynced as it is now.
    fn road_trip_snapshot_payload(&self) -> Bits {
        let Some(trip) = &self.road_trip else {
            return Bits::new();
        };
        let mut values = vec![
            (8, trip.state as i64),
            (
                9,
                if trip.until == i64::MAX {
                    0
                } else {
                    wire_at(trip.until)
                },
            ),
        ];
        if let Some(race) = trip.current_race() {
            values.push((0, trip.location()));
            values.push((1, trip.event_index as i64));
            values.push((3, i64::from(race)));
            values.push((2, i64::from(trip.last_race.unwrap_or(0))));
        }
        variables(&values)
    }

    fn road_trip_update(&mut self, vars: &[(u8, i64)]) {
        for (var, value) in vars {
            if *var != 8 || self.road_trip_state_sent == Some(*value) {
                continue;
            }
            self.road_trip_state_sent = Some(*value);
            self.note(
                "roadtrip.state_sent",
                vec![json!(RoadTripState::name_of(*value)), json!(value)],
            );
        }
        self.road_trip_broadcast(variables(vars));
    }

    fn road_trip_broadcast(&mut self, vars: Bits) {
        self.road_trip_sequence = seq_next(self.road_trip_sequence);
        let update = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 9,
            key: ObjKey::own(0),
            ty: 0,
            sequence: self.road_trip_sequence,
            vars,
        })));
        for s in self.synced_serials_except(None) {
            self.send_sequenced(s, vec![update.clone()]);
        }
    }

    fn road_trip_players(&self) -> Vec<u64> {
        self.synced_serials_except(None)
    }

    /// The players whose own Initialising is over: they are shown the vote.
    fn road_trip_shown(&self) -> Vec<u64> {
        self.peers
            .iter()
            .filter(|p| p.synced && p.road_trip_start_at.is_none())
            .map(|p| p.serial)
            .collect()
    }

    fn start_road_trip_players(&mut self) {
        let now = self.now;
        let due: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.synced && p.road_trip_start_at.is_some_and(|at| now >= at))
            .map(|p| p.serial)
            .collect();
        for s in due {
            let Some(p) = self.peer_mut(s) else { continue };
            p.road_trip_start_at = None;
            let index = p.index;
            let (state, planned) = {
                let t = self.trip();
                (t.state, t.plan.is_some())
            };
            if state == RoadTripState::Initialising {
                self.note("roadtrip.waits_vote", vec![json!(index)]);
                continue;
            }
            if planned {
                let m = self.event_selection(s, false);
                self.send_sequenced(s, vec![m]);
            }
            let payload = self.road_trip_snapshot_payload();
            self.road_trip_broadcast(payload);
            self.note(
                "roadtrip.peer_starts",
                vec![json!(index), json!(state as i64)],
            );
        }
    }

    /// The vote's transfer object for one player (7-bit var ids: the locations, then per
    /// destination its events and class, and the vote counts).
    fn event_selection(&mut self, serial: u64, votes_only: bool) -> Arc<Bits> {
        let counts = self.vote_counts();
        let plan = self.trip().plan.clone().unwrap_or_default();
        let mut payload = Bits::new();
        if !votes_only {
            payload.push(0, 7);
            for d in &plan {
                payload.push(u64::from(d.location as u32), 32);
            }
        }
        // the vote's blocks (from var 1) and their twin set (from var 31)
        for first in [1u64, 31] {
            for (b, count) in counts.iter().enumerate() {
                let block = first + 10 * b as u64;
                if !votes_only && let Some(d) = plan.get(b) {
                    for k in 0..EVENTS_PER_DESTINATION {
                        let id = d.events.get(k).copied().unwrap_or(0);
                        let min = d.min_players.get(k).copied().unwrap_or(1);
                        payload
                            .push(block + 2 * k as u64, 7)
                            .push(u64::from(id as u32), 32);
                        payload
                            .push(block + 2 * k as u64 + 1, 7)
                            .push(min.clamp(0, 12) as u64, 4);
                    }
                    payload.push(block + 8, 7).push(d.car_class as u64, 3);
                }
                payload.push(block + 9, 7).push((*count).min(31) as u64, 5);
            }
        }
        let seq = {
            let Some(p) = self.peer_mut(serial) else {
                return Arc::new(Bits::new());
            };
            let seq = p.road_trip_selection_sequence.map_or(0, seq_next);
            p.road_trip_selection_sequence = Some(seq);
            seq
        };
        Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 9,
            key: ObjKey::transfer(0),
            ty: 0,
            sequence: seq,
            vars: payload,
        })))
    }

    fn vote_counts(&self) -> [usize; DESTINATIONS] {
        let mut counts = [0usize; DESTINATIONS];
        if let Some(trip) = &self.road_trip {
            for (index, choice) in &trip.votes {
                if (0..DESTINATIONS as i32).contains(choice)
                    && self.peers.iter().any(|p| p.index == *index && p.synced)
                {
                    counts[*choice as usize] += 1;
                }
            }
        }
        counts
    }

    /// A joiner's snapshot: its RoadTripSynced, created in Initialising.
    pub(super) fn append_road_trip_snapshot(&mut self, serial: u64, snapshot: &mut Vec<Bits>) {
        let now = self.now;
        let Some(trip) = self.road_trip.as_mut() else {
            return;
        };
        if trip.first_seen_at < 0 {
            trip.first_seen_at = now;
            trip.until = now + INITIALISING_MS;
        }
        let state = trip.state.name();
        snapshot.push(Message::encode(&Message::Object(ObjectChange {
            is_create: true,
            router_id: 9,
            key: ObjKey::own(0),
            ty: 0,
            sequence: self.road_trip_sequence,
            vars: variables(&[(8, RoadTripState::Initialising as i64), (9, 0)]),
        })));
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        p.road_trip_start_at = Some(now + PLAYER_START_MS);
        let index = p.index;
        self.note(
            "roadtrip.created",
            vec![
                json!(9),
                json!(0),
                json!(0),
                json!(0),
                json!(index),
                json!(state),
            ],
        );
    }

    pub(super) fn road_trip_joined(&mut self, _serial: u64) {
        if self.road_trip.is_none() {
            return;
        }
        self.choose_road_trip_leader();
        // a second player before the vote is drawn: draw it now, for both
        if self.trip().state == RoadTripState::Initialising && self.road_trip_players().len() >= 2 {
            self.begin_vote();
        }
    }

    fn choose_road_trip_leader(&mut self) {
        if let Some(leader) = self.trip().leader
            && self.peers.iter().any(|p| p.index == leader && p.synced)
        {
            return;
        }
        let next = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .min_by_key(|p| p.joined_at)
            .map(|p| p.serial);
        let Some(next) = next else {
            self.trip().leader = None;
            return;
        };
        let Some(p) = self.peer_mut(next) else { return };
        let index = p.index;
        let player = p.objects.iter().find(|(k, _)| k.0 == 1).map(|(k, _)| k.1);
        let mut update = None;
        if let Some(id) = player {
            let seq = p.player_sequence.map_or(0, seq_next);
            p.player_sequence = Some(seq);
            let mut payload = Bits::new();
            payload
                .push(IS_LEADER_VARIABLE, 8)
                .push(1, 1)
                .push(LEADER_VARIABLE, 8)
                .push(1, 1);
            update = Some(Arc::new(Message::encode(&Message::Object(ObjectChange {
                is_create: false,
                router_id: 1,
                key: ObjKey::server(id),
                ty: 0,
                sequence: seq,
                vars: payload,
            }))));
        }
        self.trip().leader = Some(index);
        if let Some(m) = update {
            self.send_sequenced(next, vec![m]);
        }
        self.note("roadtrip.leader", vec![json!(index)]);
    }

    fn begin_vote(&mut self) {
        let players = self.road_trip_players().len();
        let now = self.now;
        let trip = self.trip();
        let storm = trip.settings.storm_island;
        let catalog = trip.settings.catalog.clone();
        // OD-194: the vote opens alone, but the trip only runs with two (OD-183): draw for two
        // at least, so a joiner sees the events a pair can run
        trip.plan = catalog.draw(players.max(MIN_EVENT_PARTICIPANTS), storm, &mut trip.random);
        if trip.plan.is_none() {
            trip.until = i64::MAX;
            self.note("roadtrip.no_vote", Vec::new());
            return;
        }
        trip.votes.clear();
        trip.winner = None;
        trip.event_index = 0;
        trip.state = RoadTripState::DestinationVote;
        trip.until = now + VOTE_MS;
        let (state, until) = (trip.state as i64, trip.until);
        let locations: Vec<i32> = trip
            .plan
            .as_ref()
            .map(|p| p.iter().map(|d| d.location).collect())
            .unwrap_or_default();
        for s in self.road_trip_shown() {
            let m = self.event_selection(s, false);
            self.send_sequenced(s, vec![m]);
        }
        self.road_trip_update(&[(1, 0), (8, state), (9, wire_at(until))]);
        self.note("roadtrip.vote", vec![json!(locations), json!(players)]);
    }

    /// 47: a player's vote.
    pub(super) fn road_trip_vote(&mut self, serial: u64, body: &Bits) {
        if !self
            .road_trip
            .as_ref()
            .is_some_and(|t| t.state == RoadTripState::DestinationVote)
        {
            return;
        }
        let Some(index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        let mut r = body.reader();
        if ObjKey::read(&mut r, 2).is_err() {
            return;
        }
        let Ok(choice) = r.read(32) else { return };
        let choice = choice as u32 as i32;
        if !(0..DESTINATIONS as i32).contains(&choice) {
            return;
        }
        // the same vote again changes nothing: no reliable rebroadcast
        if self.trip().votes.insert(index, choice) == Some(choice) {
            return;
        }
        let shown = self.road_trip_shown();
        if !shown.is_empty() && !self.charge_fanout(serial, 1) {
            return;
        }
        for s in shown {
            let m = self.event_selection(s, true);
            self.send_sequenced(s, vec![m]);
        }
        let players = self.road_trip_players();
        let now = self.now;
        let all = players.iter().all(|s| {
            self.peer(*s).is_some_and(|p| {
                self.road_trip
                    .as_ref()
                    .is_some_and(|t| t.votes.contains_key(&p.index))
            })
        });
        let trip = self.trip();
        if all && trip.until > now + ALL_VOTED_MS {
            trip.until = now + ALL_VOTED_MS;
            let until = trip.until;
            self.road_trip_update(&[(9, wire_at(until))]);
        }
    }

    fn finish_vote(&mut self) {
        // OD-194 / OD-183: nobody joined during the vote: its player goes back to free roam
        let players: Vec<(u64, u8)> = self
            .peers
            .iter()
            .filter(|p| p.synced && !p.doomed)
            .map(|p| (p.serial, p.index))
            .collect();
        if players.len() < MIN_EVENT_PARTICIPANTS {
            for (serial, index) in players {
                self.note(
                    "roadtrip.nobody_joined",
                    vec![json!(index), json!(MIN_EVENT_PARTICIPANTS)],
                );
                if let Some(p) = self.peer_mut(serial) {
                    send::doom(p, super::Why::RoadTripAlone);
                }
            }
            self.reset_road_trip("its vote ended with nobody else");
            return;
        }
        let counts = self.vote_counts();
        let max = counts.iter().copied().max().unwrap_or(0);
        // a tie goes to the first of the tied destinations
        let winner = counts.iter().position(|c| *c == max).unwrap_or(0);
        let now = self.now;
        let trip = self.trip();
        trip.winner = Some(winner);
        trip.state = RoadTripState::CarSelect;
        trip.until = now + CAR_SELECT_MS;
        trip.picked.clear();
        let (location, race, state, until) = (
            trip.location(),
            i64::from(trip.current_race().unwrap_or(0)),
            trip.state as i64,
            trip.until,
        );
        // CurrentLocationId must be one of the voted locations
        self.road_trip_update(&[(0, location), (3, race), (8, state), (9, wire_at(until))]);
        self.note("roadtrip.won", vec![json!(location), json!(counts)]);
        self.configure_next_event();
    }

    /// The leader builds the event's GameOptions (50): the game finds the event by them.
    fn configure_next_event(&mut self) {
        let now = self.now;
        let trip = self.trip();
        trip.configured = false;
        trip.configure_until = now
            + CONFIGURE_MS
            + if trip.state == RoadTripState::CarSelect {
                CAR_SELECT_MS
            } else {
                0
            };
        self.choose_road_trip_leader();
        let Some(leader) = self.trip().leader else {
            return;
        };
        let Some(serial) = self
            .peers
            .iter()
            .find(|p| p.index == leader)
            .map(|p| p.serial)
        else {
            return;
        };
        let race = self.trip().current_race().unwrap_or(0);
        self.ev.event_owner = Some(leader); // its GameOptions are the event's
        let mut body = Bits::new();
        body.push(u64::from(race as u32), 32).push(1, 32);
        let m = Arc::new(Message::encode(&Message::Opaque { id: 50, body }));
        self.send_sequenced(serial, vec![m]);
        self.note("roadtrip.configures", vec![json!(leader), json!(race)]);
    }

    /// 51: the leader configured the event.
    pub(super) fn road_trip_configured(&mut self, serial: u64) {
        let index = self.peer(serial).map(|p| p.index);
        if let Some(trip) = self.road_trip.as_mut()
            && trip.leader.is_some()
            && trip.leader == index
        {
            trip.configured = true;
        }
    }

    /// A large router-1 update during car select: the player chose its car.
    pub(super) fn road_trip_car_picked(&mut self, serial: u64) {
        let Some(index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        let Some(trip) = self.road_trip.as_mut() else {
            return;
        };
        if trip.state != RoadTripState::CarSelect || !trip.picked.insert(index) {
            return;
        }
        let picked = trip.picked.clone();
        let now = self.now;
        let all = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .all(|p| picked.contains(&p.index));
        let trip = self.trip();
        if all && trip.until > now + ALL_PICKED_MS {
            trip.until = now + ALL_PICKED_MS;
            let until = trip.until;
            self.road_trip_update(&[(9, wire_at(until))]);
        }
    }

    fn start_drive(&mut self) {
        let players: Vec<u8> = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| p.index)
            .collect();
        let trip = self.trip();
        trip.state = RoadTripState::DriveToEvent;
        for i in players {
            trip.player_states.insert(i, 0);
        }
        let vars = [
            (0, trip.location()),
            (1, trip.event_index as i64),
            (2, i64::from(trip.last_race.unwrap_or(0))),
            (3, i64::from(trip.current_race().unwrap_or(0))),
            (8, trip.state as i64),
            (9, 0),
        ];
        let (race, index) = (trip.current_race().unwrap_or(0), trip.event_index);
        // the game starts its route only when StateEndTime goes 0 -> non-zero
        self.set_host_variables(&[(4, 1, 1)]);
        self.road_trip_update(&vars);
        let now = self.now;
        let trip = self.trip();
        // a dropped event never ran: nobody is on a results screen, its next drive starts at once
        if trip.event_index == 0 || trip.last_dropped {
            trip.last_dropped = false;
            self.begin_countdown();
        } else {
            // after an event the players are on their results: the countdown waits for the road
            trip.hold_drive = true;
            trip.hold_until = now + DRIVE_HOLD_MS;
            trip.until = i64::MAX;
        }
        self.note(
            "roadtrip.drive",
            vec![json!(race), json!(index + 1), json!(EVENTS_PER_DESTINATION)],
        );
    }

    fn begin_countdown(&mut self) {
        let now = self.now;
        let trip = self.trip();
        trip.hold_drive = false;
        trip.until = now + DRIVE_CAP_MS;
        let until = trip.until;
        self.road_trip_update(&[(9, wire_at(until))]);
    }

    /// A player's RoadTripPlayerSynced (3-bit var ids: 0 State, 4 bits; 1 CompletedEventCount,
    /// 3 bits; 2 XPOnDestinationJoin, s32).
    pub(super) fn road_trip_player_update(&mut self, serial: u64, payload: &Bits) {
        if self.road_trip.is_none() {
            return;
        }
        let Some(index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        let mut r = payload.reader();
        while r.left() >= 3 {
            let Ok(var) = r.read(3) else { return };
            let ok = match var {
                0 => match r.read(4) {
                    Ok(v) => {
                        self.trip().player_states.insert(index, v as i32);
                        true
                    }
                    Err(_) => false,
                },
                1 => r.skip(3).is_ok(),
                2 => r.skip(32).is_ok(),
                _ => break, // a layout this server does not read: still relayed as it came
            };
            if !ok {
                return;
            }
        }
        self.road_trip_progress();
    }

    fn player_state(&self, index: u8) -> i32 {
        self.road_trip
            .as_ref()
            .and_then(|t| t.player_states.get(&index).copied())
            .unwrap_or(0)
    }

    fn road_trip_progress(&mut self) {
        let Some(trip) = &self.road_trip else { return };
        if trip.state != RoadTripState::DriveToEvent {
            return;
        }
        let players: Vec<u8> = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| p.index)
            .collect();
        if trip.hold_drive {
            if !players.is_empty()
                && players
                    .iter()
                    .all(|i| self.player_state(*i) == RoadTripState::DriveToEvent as i32)
            {
                self.begin_countdown();
            }
            return;
        }
        let arrived = players
            .iter()
            .filter(|i| self.player_state(**i) >= RoadTripState::ArrivedAtDestination as i32)
            .count();
        if arrived == 0 {
            return;
        }
        let now = self.now;
        let trip = self.trip();
        if arrived == players.len() {
            trip.until = now;
            return;
        }
        if trip.until > now + FIRST_ARRIVAL_MS {
            trip.until = now + FIRST_ARRIVAL_MS;
            let until = trip.until;
            self.road_trip_update(&[(9, wire_at(until))]);
            self.note(
                "roadtrip.first_arrival",
                vec![json!(FIRST_ARRIVAL_MS / 1000)],
            );
        }
    }

    fn begin_prepare(&mut self) {
        let now = self.now;
        let trip = self.trip();
        trip.state = RoadTripState::PrepareForEvent;
        trip.until = now + PREPARE_MS;
        let (state, until) = (trip.state as i64, trip.until);
        self.road_trip_update(&[(8, state), (9, wire_at(until))]);
        self.road_trip_ready_with(false);
    }

    /// A Road Trip's own event start once its players are ready (a Ready request).
    pub(super) fn road_trip_ready(&mut self) {
        self.road_trip_ready_with(false);
    }

    /// Everybody ready, or the time is up with somebody ready: the event starts with those
    /// ready (OD-183: two at least, else the event is dropped).
    fn road_trip_ready_with(&mut self, force: bool) {
        if !self
            .road_trip
            .as_ref()
            .is_some_and(|t| t.state == RoadTripState::PrepareForEvent)
            || self.ev.race.is_some()
        {
            return;
        }
        let players: Vec<u64> = self.road_trip_players();
        let ready: Vec<u64> = players
            .iter()
            .copied()
            .filter(|s| self.peer(*s).is_some_and(|p| p.console_values[4] != 0))
            .collect();
        if ready.is_empty() || !force && ready.len() != players.len() {
            return;
        }
        let race_id = self.trip().current_race().unwrap_or(0);
        if ready.len() < MIN_EVENT_PARTICIPANTS {
            if !force {
                return;
            }
            self.note(
                "roadtrip.event_dropped",
                vec![
                    json!(race_id),
                    json!(ready.len()),
                    json!(players.len()),
                    json!(MIN_EVENT_PARTICIPANTS),
                ],
            );
            // as the race end does for its participants: the next preparation needs a new Ready
            for s in &ready {
                self.set_console_variables(*s, &[(2, false), (4, false)]);
            }
            self.road_trip_event_dropped();
            return;
        }
        let leader_index = self.trip().leader;
        let leader = ready
            .iter()
            .copied()
            .find(|s| self.peer(*s).map(|p| p.index) == leader_index)
            .unwrap_or(ready[0]);
        let Some(li) = self.peer(leader).map(|p| p.index) else {
            return;
        };
        self.ev.event_owner = Some(li);
        self.trip().until = i64::MAX;
        // the state stays PrepareForEvent until the race begins
        self.road_trip_update(&[(9, 0)]);
        let mut participants = vec![li];
        for s in &ready {
            if *s != leader
                && let Some(p) = self.peer(*s)
            {
                participants.push(p.index);
            }
        }
        self.ev.race = Some(Race {
            owner_serial: leader,
            owner: li,
            participants,
            finished: HashSet::new(),
            in_progress: HashSet::new(),
            results_noted: HashSet::new(),
            stage: Stage::Inviting,
            invite_until: self.now,
            stage_until: 0,
            first_finish_at: None,
            grid: None,
        });
        self.set_host_variables(&[(4, 1, 1), (6, 0, 1)]);
        self.note(
            "roadtrip.event_starts",
            vec![json!(race_id), json!(ready.len()), json!(players.len())],
        );
        self.select_participants();
    }

    /// The event closed: the destination's next event, or a new vote after its fourth.
    pub(super) fn road_trip_event_closed(&mut self) {
        let now = self.now;
        let Some(trip) = self.road_trip.as_mut().filter(|t| t.winner.is_some()) else {
            return;
        };
        trip.last_race = trip.current_race();
        trip.last_dropped = false;
        if trip.event_index + 1 < EVENTS_PER_DESTINATION {
            trip.event_index += 1;
            trip.state = RoadTripState::FinaliseEvent;
            trip.until = now + FINALISE_MS; // the players finish returning to free roam
            self.configure_next_event();
        } else {
            self.begin_vote();
        }
    }

    /// An event dropped before it ran (OD-183): on to the destination's next event like a closed
    /// one, but with no EventRaceId and no wait for results screens.
    fn road_trip_event_dropped(&mut self) {
        let now = self.now;
        let Some(trip) = self.road_trip.as_mut().filter(|t| t.winner.is_some()) else {
            return;
        };
        trip.last_race = None;
        trip.last_dropped = true;
        if trip.event_index + 1 < EVENTS_PER_DESTINATION {
            trip.event_index += 1;
            trip.state = RoadTripState::FinaliseEvent;
            trip.until = now;
            self.configure_next_event();
        } else {
            self.begin_vote();
        }
    }

    pub(super) fn tick_road_trip(&mut self) {
        if self.road_trip.is_none() || self.road_trip_alone() {
            return;
        }
        self.start_road_trip_players();
        let now = self.now;
        let trip = self.trip();
        if trip.state == RoadTripState::DriveToEvent && trip.hold_drive && now >= trip.hold_until {
            self.begin_countdown();
            return;
        }
        if now < trip.until {
            return;
        }
        let configured = trip.configured || now >= trip.configure_until;
        let state = trip.state;
        match state {
            // OD-194: the first player opens the vote, alone or not
            RoadTripState::Initialising if !self.road_trip_players().is_empty() => {
                self.begin_vote();
            }
            RoadTripState::DestinationVote => self.finish_vote(),
            RoadTripState::CarSelect | RoadTripState::FinaliseEvent if configured => {
                self.start_drive();
            }
            RoadTripState::DriveToEvent => self.begin_prepare(),
            RoadTripState::PrepareForEvent => self.road_trip_ready_with(true),
            _ => {}
        }
    }

    /// OD-183: a player alone in the trip, before anybody else joined or after the others left,
    /// is sent back (its connection closed, as a host ending the session); with nobody left the
    /// trip starts over. True when the tick is done.
    fn road_trip_alone(&mut self) -> bool {
        let present: Vec<(u64, bool, u8)> = self
            .peers
            .iter()
            .filter(|p| !p.doomed)
            .map(|p| (p.serial, p.synced, p.index))
            .collect();
        let players: Vec<(u64, u8)> = present.iter().filter(|p| p.1).map(|p| (p.0, p.2)).collect();
        let now = self.now;
        if players.is_empty() {
            let trip = self.trip();
            trip.alone_since = None;
            if present.is_empty()
                && (trip.state != RoadTripState::Initialising || trip.first_seen_at >= 0)
            {
                self.reset_road_trip("nobody left");
            }
            return false;
        }
        let trip = self.trip();
        // OD-194: the vote runs to its end whoever is there (finish_vote sends a lone player back)
        if players.len() >= MIN_EVENT_PARTICIPANTS
            || present.len() >= MIN_EVENT_PARTICIPANTS // a second player is joining
            || matches!(
                trip.state,
                RoadTripState::Initialising | RoadTripState::DestinationVote
            )
        {
            trip.alone_since = None;
            return false;
        }
        let since = *trip.alone_since.get_or_insert(now);
        if now - since < ALONE_MS {
            return false;
        }
        let (lone, index) = players[0];
        self.note(
            "roadtrip.alone",
            vec![json!(index), json!(MIN_EVENT_PARTICIPANTS)],
        );
        if let Some(p) = self.peer_mut(lone) {
            send::doom(p, super::Why::RoadTripAlone);
        }
        // at once: a player joining in between gets a fresh trip and its full vote
        self.reset_road_trip("its last player was sent back");
        true
    }

    /// The trip starts over, and SessionHost's event flag goes back to free roam's.
    fn reset_road_trip(&mut self, why: &str) {
        let Some(settings) = self.settings.road_trip.clone() else {
            return;
        };
        self.road_trip = Some(RoadTrip::new(settings));
        self.road_trip_state_sent = None;
        if self.host_variables.get(&4).is_some_and(|v| v.0 != 0) {
            self.set_host_variables(&[(4, 0, 1)]);
        }
        self.note("roadtrip.starts_over", vec![json!(why)]);
    }

    pub(super) fn road_trip_peer_left(&mut self, serial: u64, index: u8) {
        let _ = serial;
        let Some(trip) = self.road_trip.as_mut() else {
            return;
        };
        trip.votes.remove(&index);
        trip.picked.remove(&index);
        trip.player_states.remove(&index);
        if trip.leader == Some(index) {
            trip.leader = None;
            self.choose_road_trip_leader();
        }
        self.road_trip_progress();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{Event, testkit},
        *,
    };
    use crate::{core::Profile, fh2::RelaySettings};

    /// Three locations of four two-player events, one class: a vote for any group size.
    fn catalog() -> Arc<RoadTripCatalog> {
        let events = (0..12).map(|i| RoadTripEvent {
            id: 100 + i,
            location: 3 + i / 4,
            mode: i % 2,
            min_players: 2,
            classes: BTreeSet::from([1]),
        });
        Arc::new(RoadTripCatalog::new(events))
    }

    fn road_trip() -> Engine {
        let settings = super::super::EngineSettings {
            relay: RelaySettings::for_profile(Profile::Public),
            max_players: 12,
            races: true,
            car_meet_lobby: None,
            enforce_car_rule: false,
            road_trip: Some(RoadTripSettings {
                catalog: catalog(),
                storm_island: false,
                seed: 7,
            }),
        };
        let seeds = super::super::random::Seeds {
            weather: 1,
            tag: 2,
            team: 3,
            attempt: 4,
            transaction: 5,
        };
        Engine::new(
            super::super::EngineKind::RoadTrip,
            settings,
            super::super::random::EngineRandom::new(seeds),
            Box::new(testkit::AllowAll),
        )
        .expect("valid settings")
    }

    /// A player arriving: seated, its RoadTripSynced created, joined.
    fn arrive(e: &mut Engine, index: u8) -> u64 {
        let serial = testkit::seat(e, index, 1_000_000_000_000_100 + u64::from(index));
        let mut snapshot = Vec::new();
        e.append_road_trip_snapshot(serial, &mut snapshot);
        e.road_trip_joined(serial);
        serial
    }

    fn notes(e: &mut Engine) -> Vec<&'static str> {
        e.take_events()
            .into_iter()
            .filter_map(|ev| match ev {
                Event::Note { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    fn tick_for(e: &mut Engine, ms: i64) {
        e.now += ms;
        e.tick_road_trip();
    }

    fn doomed(e: &Engine, serial: u64) -> bool {
        e.peer(serial).is_some_and(|p| p.doomed)
    }

    #[test]
    fn a_lone_player_opens_the_vote_and_is_sent_back_when_it_ends() {
        let mut e = road_trip();
        let lone = arrive(&mut e, 1);
        tick_for(&mut e, INITIALISING_MS + 1);
        assert_eq!(e.trip().state, RoadTripState::DestinationVote, "OD-194");
        assert!(!e.road_trip_in_progress(), "joinable during the vote");
        assert!(notes(&mut e).contains(&"roadtrip.vote"));
        // the 10 s alone rule does not cut the vote short
        tick_for(&mut e, ALONE_MS + 1);
        assert_eq!(e.trip().state, RoadTripState::DestinationVote);
        assert!(!doomed(&e, lone));
        tick_for(&mut e, VOTE_MS);
        assert!(doomed(&e, lone), "sent back at the vote's end");
        assert!(notes(&mut e).contains(&"roadtrip.nobody_joined"));
        assert_eq!(e.trip().state, RoadTripState::Initialising, "a fresh trip");
        assert!(!e.road_trip_in_progress());
    }

    #[test]
    fn a_late_joiner_votes_and_the_trip_then_takes_no_newcomers() {
        let mut e = road_trip();
        let first = arrive(&mut e, 1);
        tick_for(&mut e, INITIALISING_MS + 1);
        assert_eq!(e.trip().state, RoadTripState::DestinationVote);
        let until = e.trip().until;
        tick_for(&mut e, 20_000);
        let late = arrive(&mut e, 2);
        tick_for(&mut e, PLAYER_START_MS + 1);
        assert_eq!(e.trip().until, until, "the vote keeps its end time");
        assert!(
            notes(&mut e).contains(&"roadtrip.peer_starts"),
            "shown the vote"
        );
        e.now = until + 1;
        e.tick_road_trip();
        assert_eq!(e.trip().state, RoadTripState::CarSelect);
        assert!(
            e.road_trip_in_progress(),
            "closed to newcomers after the vote"
        );
        assert!(!doomed(&e, first) && !doomed(&e, late));
    }

    #[test]
    fn a_vote_the_other_player_left_still_ends_in_a_send_back() {
        let mut e = road_trip();
        let first = arrive(&mut e, 1);
        let second = arrive(&mut e, 2);
        tick_for(&mut e, INITIALISING_MS + 1);
        assert_eq!(e.trip().state, RoadTripState::DestinationVote);
        e.peers.retain(|p| p.serial != second);
        tick_for(&mut e, ALONE_MS + 1);
        assert!(!doomed(&e, first), "the vote runs to its end");
        tick_for(&mut e, VOTE_MS);
        assert!(doomed(&e, first));
    }
}
