// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Free-roam events (`races` worlds; contract §7.1, §7.2): the options' ownership (10-12 on
//! router 6), the invitation (38), Ready (41/42), participant selection, loading, the starting grid
//! (39), the start (40), the scores (43-45), the race-end split and the event-end hold, and the
//! finishers who stay participants. OD-183: an event needs two participants.
//!
//! The event's lifecycle in SessionHostSynced (router 6): var 4 RaceReady, 5
//! RaceParticipantsSelected, 6 InRace, 2 CountdownType, 3 the invitation's end, 9 RaceStartTime;
//! per player its console (router 3): var 2 ParticipatingInRace, 4 Ready (server-owned), and the
//! game's own 0 grid ready, 3 RaceInProgress and 5 cars loaded.
//!
//! Tag events (King, Infected), team events and Road Trip events are later steps: their hooks
//! are in [`super::seams`].

use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

use serde_json::{Value, json};

use super::{
    Engine, M, Why,
    bits::Bits,
    options::{Applied, Fields, Journal},
    wire::{self, Handle, Kind, Msg, Obj, seq_next},
};

/// OD-183: the fewest participants an event starts with.
pub const MIN_EVENT_PARTICIPANTS: usize = 2;
/// A stuck loading or grid gets another step of at most this long after a drop.
const STUCK_STEP_MS: i64 = 10_000;
/// The options' ownership answers at most once per this window per player.
const OWNERSHIP_WINDOW_MS: i64 = 2000;

pub(super) const COUNTDOWN_TYPE: u8 = 2;
pub(super) const RACE_START_TIME: u8 = 9;
pub(super) const NUM_LAPS_COMPLETED: u8 = 10;
pub(super) const FURTHEST_PROGRESS: u8 = 11;
const COUNTDOWN_START_RACE: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Inviting,
    Gathering,
    Grid,
    Started,
}

impl Stage {
    fn describe(self) -> &'static str {
        match self {
            Stage::Inviting => "inviting",
            Stage::Gathering => "loading cars",
            Stage::Grid => "on the grid",
            Stage::Started => "racing",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Race {
    /// The owner's serial (indexes are reused) and index.
    pub owner_serial: u64,
    pub owner: u8,
    pub participants: Vec<u8>,
    pub finished: HashSet<u8>,
    pub in_progress: HashSet<u8>,
    /// Finishers whose RaceInProgress 0 during the race was logged (once per peer and event).
    pub results_noted: HashSet<u8>,
    pub stage: Stage,
    pub invite_until: i64,
    pub stage_until: i64,
    pub first_finish_at: Option<i64>,
    /// A team event's grid order (step 4); `None`: the participants' order.
    pub grid: Option<Vec<u8>>,
}

/// "Event in progress" for the free-roam players outside a running event (T24).
#[derive(Debug, Clone, Default)]
pub(super) struct Progress {
    laps: i32,
    started_at: i64,
    last_score_at: Option<i64>,
    quiet_noted: bool,
    first_noted: HashSet<u8>,
    refused_noted: HashSet<u8>,
    ignored_noted: HashSet<u8>,
    start_line_noted: HashSet<u8>,
    update_notes: i32,
    lap_notes: i32,
    progress: std::collections::HashMap<u8, f32>,
    laps_done: std::collections::HashMap<u8, i32>,
    num_laps_completed: i32,
    furthest: f32,
    sent_laps: i32,
    sent_furthest: f32,
    sent_furthest_at: Option<i64>,
    refused_laps: HashSet<u8>,
}

const FURTHEST_INTERVAL_MS: i64 = 1000;
const FURTHEST_MIN_DELTA: f32 = 0.005;
const LAPS_END_CONDITION: i32 = 1;
const UPDATE_NOTES: i32 = 20;
const LAP_NOTES: i32 = 24;
const QUIET_MS: i64 = 30_000;
/// T24c: a 44 is a lap only past this much progress beyond the laps already done.
const LAP_SCORE_MIN_PROGRESS: f32 = 0.5;
/// T38: the longest the hold keeps RaceReady and participants after the close.
pub const EVENT_END_HOLD_MS: i64 = 120_000;
/// T38: how long a finisher already at RaceInProgress 0 stays on the event side after the close.
pub const RESULTS_EXIT_MS: i64 = 35_000;
/// An event side that never empties still ends.
const PHASE_HOLD_MS: i64 = 10 * 60 * 1000;
pub(super) const FIRST_PRIVATE_UNIVERSE: u8 = 1;
pub(super) const LAST_PRIVATE_UNIVERSE: u8 = 17;

/// Every event state of an engine.
#[derive(Debug, Default)]
pub(super) struct Events {
    pub event_owner: Option<u8>,
    pub race: Option<Race>,
    /// The serials whose RaceInProgress went 1 → 0 in the packet being processed.
    pub race_exits: Vec<u64>,
    pub options: Journal,
    /// The journal was kept past an event's end (TG-2): only its writer takes it over again.
    pub options_outlived: bool,
    pub progress: Option<Progress>,
    pub progress_written: bool,
    pub held: bool,
    pub held_at: i64,
    pub event_side: BTreeSet<u8>,
    pub phasing: bool,
    pub phase_released_at: i64,
    pub results_kept: BTreeSet<u8>,
    pub results_kept_until: i64,
}

/// A float note argument: the f32's shortest decimal, as the world log prints it.
pub(super) fn f32_arg(x: f32) -> Value {
    if x.is_nan() {
        json!("NaN")
    } else if x.is_infinite() {
        json!(if x > 0.0 { "Infinity" } else { "-Infinity" })
    } else {
        let v: f64 = format!("{x}").parse().unwrap_or(0.0);
        if v.fract() == 0.0 && v.abs() < 1e15 {
            json!(v as i64)
        } else {
            json!(v)
        }
    }
}

/// A double as the reference world log's round-trip text: the shortest digits, positional for
/// exponents -5..14, else `d.dddE+XX`.
pub(super) fn round_trip(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let e = format!("{x:e}");
    let (mantissa, exponent) = e.split_once('e').unwrap_or((&e, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-5..15).contains(&exponent) {
        format!("{x}")
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}E{sign}{:02}", exponent.abs())
    }
}

/// SessionHostSynced's f32 value.
fn host_float(x: f32) -> u64 {
    u64::from(x.to_bits())
}

/// MsgReadyRequest (41): the lobby's event key, then the ready flag.
fn ready_request(body: &Bits) -> Option<bool> {
    let mut r = body.reader();
    Handle::read(&mut r, 2).ok()?;
    r.boolean().ok()
}

/// MsgRequestOwnership (10): router 6, type 1, a transfer key.
fn ownership_request(body: &Bits) -> Option<Handle> {
    let mut r = body.reader();
    let router = r.read(4).ok()? as u8;
    let (id_bits, type_bits) = wire::router_widths(router)?;
    let ty = r.read(type_bits).ok()?;
    let handle = Handle::read(&mut r, id_bits).ok()?;
    (router == 6 && ty == 1 && handle.kind == Kind::Transfer).then_some(handle)
}

/// MsgGrantOwnership (11): router, key, sequence, then the first fragment.
fn grant_ownership(handle: Handle, sequence: u8, payload: &Bits) -> M {
    let mut b = Bits::new();
    b.push(11, 7).push(6, 4);
    handle.write(&mut b, 2);
    b.push(u64::from(sequence), 8)
        .push(payload.len() as u64, 14)
        .append(payload);
    Arc::new(b)
}

/// MsgTransferOwnership (12): ownership refused, the requester keeps waiting on the owner.
fn transfer_ownership(handle: Handle, sequence: u8) -> M {
    let mut b = Bits::new();
    b.push(12, 7).push(6, 4);
    handle.write(&mut b, 2);
    b.push(u64::from(sequence), 8);
    Arc::new(b)
}

/// MsgReadyRequestResponse (42): the lobby's event key, then the 2-bit answer (0 accepted).
fn ready_response(code: u8) -> M {
    let mut b = Bits::new();
    b.push(42, 7);
    Handle::own(0).write(&mut b, 2);
    b.push(u64::from(code), 2);
    Arc::new(b)
}

fn start_race(time: u32) -> M {
    let mut b = Bits::new();
    b.push(40, 7);
    Handle::own(0).write(&mut b, 2);
    b.push(u64::from(time), 32);
    Arc::new(b)
}

fn starting_grid(handles: &[Handle]) -> M {
    let mut b = Bits::new();
    b.push(39, 7);
    Handle::own(0).write(&mut b, 2);
    b.push(handles.len().min(12) as u64, 4);
    for h in handles.iter().take(12) {
        h.write(&mut b, 7);
    }
    Arc::new(b)
}

/// A score message's body for the others: the sender's own key as its relay key.
fn score_for_others(body: &Bits, sender: u8) -> Option<Bits> {
    let mut r = body.reader();
    let own = Handle::read(&mut r, 7).ok()?;
    if own.kind != Kind::Own {
        return None;
    }
    let mut out = Bits::new();
    Handle::relay(own.id, sender).write(&mut out, 7);
    let left = r.left();
    out.append(&r.take(left).ok()?);
    Some(out)
}

/// MsgInProgressScoreUpdate (43): the sender's key, then the f32 score and a flag.
fn in_progress_score(body: &Bits) -> Option<(f32, bool)> {
    let mut r = body.reader();
    Handle::read(&mut r, 7).ok()?;
    if r.left() != 33 {
        return None;
    }
    let value = f32::from_bits(r.read(32).ok()? as u32);
    Some((value, r.boolean().ok()?))
}

struct LapScore {
    values: [f64; 3],
    count: u32,
    flag89: bool,
    flag8b: bool,
}

/// MsgLapScore (44) in the game's layout: the sender's own key, three 8-byte strings (doubles),
/// then exactly 34 bits (a u32 and two flags).
fn lap_score(body: &Bits) -> Option<LapScore> {
    let mut r = body.reader();
    if Handle::read(&mut r, 7).ok()?.kind != Kind::Own {
        return None;
    }
    let mut values = [0f64; 3];
    for v in &mut values {
        let bytes: [u8; 8] = r.byte_string(8).ok()?.try_into().ok()?;
        *v = f64::from_le_bytes(bytes);
    }
    if r.left() != 34 {
        return None;
    }
    Some(LapScore {
        values,
        count: r.read(32).ok()? as u32,
        flag89: r.boolean().ok()?,
        flag8b: r.boolean().ok()?,
    })
}

impl Engine {
    fn serial_of(&self, index: u8) -> Option<u64> {
        self.peers
            .iter()
            .find(|p| p.index == index)
            .map(|p| p.serial)
    }

    fn index_of(&self, serial: u64) -> Option<u8> {
        self.peer(serial).map(|p| p.index)
    }

    pub(super) fn is_road_trip(&self) -> bool {
        self.road_trip.is_some()
    }

    pub(super) fn is_event_owner(&self, serial: u64) -> bool {
        let Some(index) = self.index_of(serial) else {
            return false;
        };
        self.ev.event_owner == Some(index)
            && self
                .ev
                .race
                .as_ref()
                .is_none_or(|r| r.owner_serial == serial && r.participants.contains(&index))
    }

    /// SessionHostSynced variables: kept for joiners, one update to every synced player.
    pub(super) fn set_host_variables(&mut self, vars: &[(u8, u64, u32)]) {
        for (k, v, w) in vars {
            self.host_variables.insert(*k, (*v, *w));
        }
        self.host_sequence = seq_next(self.host_sequence);
        let m = Arc::new(wire::write(&Msg::Object(Obj {
            create: false,
            router: 6,
            handle: Handle::own(0),
            ty: 0,
            seq: self.host_sequence,
            payload: super::payload::session_host(vars),
        })));
        for s in self.synced_serials_except(None) {
            self.send_sequenced(s, vec![m.clone()]);
        }
    }

    /// The server-owned console flags of a player: to it, and on its relayed console to the
    /// others (authoritative even before its console exists).
    pub(super) fn set_console_variables(&mut self, serial: u64, vars: &[(u8, bool)]) {
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        for (k, v) in vars {
            if let Some(slot) = p.console_values.get_mut(usize::from(*k)) {
                *slot = u64::from(*v);
            }
        }
        if !p.synced {
            return;
        }
        let Some(id) = p.console_id else { return };
        let seq = p.console_sequence.map_or(0, seq_next);
        p.console_sequence = Some(seq);
        let index = p.index;
        let own = Arc::new(wire::write(&Msg::Object(super::payload::console_update(
            id, seq, vars,
        ))));
        self.send_sequenced(serial, vec![own]);
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        let Some(obj) = p.objects.get_mut(&(3, id)) else {
            return;
        };
        obj.injected = seq_next(obj.injected);
        let mut payload = Bits::new();
        for (k, v) in vars {
            obj.server_variables.insert(*k, *v);
            payload.push(u64::from(*k), 4).boolean(*v);
        }
        let seq = wire::seq_add(obj.last, obj.injected);
        let m = Arc::new(wire::write(&Msg::Object(Obj {
            create: false,
            router: 3,
            handle: Handle::relay(id, index),
            ty: 0,
            seq,
            payload,
        })));
        self.forward(serial, vec![m], true);
    }

    /// The `races` messages of a synced player (10, 38, 41, 43-45): the reliable answers that go
    /// with the packet's own, and whether a Ready ends its held join.
    pub(super) fn race_message(&mut self, serial: u64, kind: u8, body: &Bits) -> (Vec<M>, bool) {
        match kind {
            10 => (self.ownership(serial, body), false),
            38 => {
                self.request_race(serial);
                (Vec::new(), false)
            }
            41 => {
                let Some(ready) = ready_request(body) else {
                    return (Vec::new(), false);
                };
                // a participant's grid needs the others' cars: a Ready ends the join's hold
                let release = ready && self.holding.contains_key(&serial);
                self.ready(serial, ready);
                (Vec::new(), release)
            }
            43..=45 => {
                if self.pass_score(serial, kind, body) && kind == 45 {
                    self.finished(serial);
                }
                (Vec::new(), false)
            }
            _ => (Vec::new(), false),
        }
    }

    fn request_race(&mut self, serial: u64) {
        let Some(index) = self.index_of(serial) else {
            return;
        };
        if self.is_road_trip() {
            self.note("race.ignored_roadtrip", vec![json!(index)]);
            return;
        }
        if let Some(r) = &self.ev.race {
            let (owner, stage) = (r.owner, r.stage.describe());
            self.note(
                "race.ignored_running",
                vec![json!(index), json!(owner), json!(stage)],
            );
            return;
        }
        if !self.is_event_owner(serial) {
            match self.ev.event_owner {
                Some(o) => self.note("race.ignored_not_owner", vec![json!(index), json!(o)]),
                None => self.note("race.ignored_no_options", vec![json!(index)]),
            }
            return;
        }
        // a held event end goes out first, so the invitation's update follows it unchanged
        self.release_event_hold("a new event");
        let ready: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.synced && p.console_values[4] != 0)
            .map(|p| p.serial)
            .collect();
        for s in ready {
            self.set_console_variables(s, &[(4, false)]);
        }
        let invited = self.synced_serials_except(Some(serial)).len();
        let wait = if invited > 0 {
            self.settings.relay.race_invite_ms as i64
        } else {
            0
        };
        self.ev.race = Some(Race {
            owner_serial: serial,
            owner: index,
            participants: vec![index],
            finished: HashSet::new(),
            in_progress: HashSet::new(),
            results_noted: HashSet::new(),
            stage: Stage::Inviting,
            invite_until: self.now + wait,
            stage_until: 0,
            first_finish_at: None,
            grid: None,
        });
        self.note(
            "race.accepted",
            vec![json!(index), json!(invited), json!(wait)],
        );
        if invited > 0 {
            let until = self.now_wire().wrapping_add(wait as u32);
            self.set_host_variables(&[(4, 1, 1), (6, 0, 1), (2, 1, 2), (3, u64::from(until), 32)]);
            let m = Arc::new(wire::write(&Msg::Other {
                kind: 38,
                body: Bits::new(),
            }));
            self.forward(serial, vec![m], true);
        } else {
            self.set_host_variables(&[(4, 1, 1), (6, 0, 1)]);
            self.select_participants();
        }
    }

    pub(super) fn select_participants(&mut self) {
        let Some(race) = self.ev.race.as_mut().filter(|r| r.stage == Stage::Inviting) else {
            return;
        };
        let count = race.participants.len();
        if count < MIN_EVENT_PARTICIPANTS {
            self.note(
                "event.dropped_few",
                vec![json!(count), json!(MIN_EVENT_PARTICIPANTS)],
            );
            self.end_race("too few participants");
            return;
        }
        race.stage = Stage::Gathering;
        race.stage_until = self.now + self.settings.relay.race_prepare_ms as i64;
        let participants = race.participants.clone();
        self.note("event.selected", vec![json!(count)]);
        self.begin_phase(&participants);
        self.assign_teams();
        for s in self.participant_serials() {
            self.reset_console_observation(s, 5);
            self.reset_console_observation(s, 0);
        }
        self.set_host_variables(&[(5, 1, 1), (2, 0, 2)]);
        for index in participants {
            if let Some(s) = self.serial_of(index) {
                self.set_console_variables(s, &[(2, true)]);
            }
        }
    }

    /// The participants' serials, in engine order.
    fn participant_serials(&self) -> Vec<u64> {
        let Some(race) = &self.ev.race else {
            return Vec::new();
        };
        self.peers
            .iter()
            .filter(|p| race.participants.contains(&p.index))
            .map(|p| p.serial)
            .collect()
    }

    fn reset_console_observation(&mut self, serial: u64, var: u8) {
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        p.console_values[usize::from(var)] = 0;
        // a delayed older report of a variable not seen yet must not satisfy a new boundary
        if let Some(latest) = p.console_latest_sequence {
            p.console_variable_sequences.insert(var, latest);
        }
    }

    fn event_car_rule(&self) -> super::options::CarRule {
        Fields::read(self.ev.options.records())
            .map_or(super::options::CarRule::ANY, |f| f.car_rule())
    }

    fn ready(&mut self, serial: u64, ready: bool) {
        let Some(index) = self.index_of(serial) else {
            return;
        };
        // T11: a car outside the event's class/PI is refused; its Ready counts once a car fits
        if ready
            && self.settings.enforce_car_rule
            && self
                .ev
                .race
                .as_ref()
                .is_some_and(|r| r.stage == Stage::Inviting && r.owner != index)
        {
            let (class, pi) = self
                .peer(serial)
                .map_or((None, f32::NAN), |p| (p.car_class, p.car_pi));
            if !self.event_car_rule().accepts(class, pi) {
                if let Some(p) = self.peer_mut(serial) {
                    p.ready_refused = true;
                }
                self.send_sequenced(serial, vec![ready_response(1)]);
                self.note("ready.refused_car", vec![json!(index)]);
                return;
            }
        }
        if let Some(p) = self.peer_mut(serial) {
            p.ready_refused = false;
        }
        // the game waits for the answer to its Ready request
        self.send_sequenced(serial, vec![ready_response(0)]);
        let synced: Vec<u8> = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| p.index)
            .collect();
        let (now, close) = (self.now, self.settings.relay.invite_close_ms as i64);
        if let Some(race) = self.ev.race.as_mut().filter(|r| r.stage == Stage::Inviting) {
            if ready && !race.participants.contains(&index) {
                race.participants.push(index);
            } else if !ready && index != race.owner {
                race.participants.retain(|i| *i != index);
            }
            let count = race.participants.len();
            let everybody = synced.iter().all(|i| race.participants.contains(i));
            let early = everybody && race.invite_until > now + close;
            if early {
                race.invite_until = now + close;
            }
            self.note(
                "ready.invited",
                vec![json!(index), json!(u8::from(ready)), json!(count)],
            );
            if early {
                self.note("event.all_accepted", Vec::new());
            }
        } else {
            self.note("ready.outside", vec![json!(index), json!(u8::from(ready))]);
        }
        let same = self.peer(serial).is_some_and(|p| {
            p.console_values[4] == u64::from(ready) && p.console_sequence.is_some()
        });
        if !same {
            self.set_console_variables(serial, &[(4, ready)]);
        }
        self.road_trip_ready();
    }

    /// A player's console observations (stale per-variable sequences already filtered).
    pub(super) fn observe_console(&mut self, serial: u64, values: &[(u8, u64)]) {
        for (var, value) in values {
            let Some(p) = self.peer_mut(serial) else {
                return;
            };
            if let Some(slot) = p.console_values.get_mut(usize::from(*var)) {
                *slot = *value;
            }
            let index = p.index;
            // RaceInProgress 0 after the event closed: back in free roam from its results
            if *var == 3 && *value == 0 && self.ev.race.is_none() {
                self.leave_event_side(index, true);
            }
            if *var != 3 {
                continue;
            }
            let Some(race) = self
                .ev
                .race
                .as_mut()
                .filter(|r| r.stage == Stage::Started && r.participants.contains(&index))
            else {
                continue;
            };
            if *value != 0 {
                race.in_progress.insert(index);
            } else if race.in_progress.remove(&index) {
                // an active → inactive edge, decided at the end of the packet (T38)
                self.ev.race_exits.push(serial);
            }
        }
    }

    /// After a packet: each RaceInProgress 1 → 0 it carried. A finisher stays a participant (on
    /// its results); a racer who had not finished left the race.
    pub(super) fn resolve_race_exits(&mut self) {
        if self.ev.race_exits.is_empty() {
            return;
        }
        let exits = std::mem::take(&mut self.ev.race_exits);
        for serial in exits {
            let Some(p) = self.peer(serial) else { continue };
            let (index, at) = (p.index, p.console_values[3]);
            let Some(race) = self
                .ev
                .race
                .as_mut()
                .filter(|r| r.stage == Stage::Started && r.participants.contains(&index))
            else {
                continue;
            };
            if at != 0 {
                continue;
            }
            if race.finished.contains(&index) {
                if race.results_noted.insert(index) {
                    self.note("event.finisher_kept", vec![json!(index), json!(0)]);
                }
                continue;
            }
            self.note("event.left_race", vec![json!(index), json!(0)]);
            self.remove_race_participant(index, Some(serial));
        }
    }

    pub(super) fn race_step(&mut self) {
        self.tick_race();
        let Some(race) = &self.ev.race else { return };
        let all = |var: usize| {
            race.participants.iter().all(|i| {
                self.peers
                    .iter()
                    .any(|p| p.index == *i && p.console_values[var] != 0)
            })
        };
        if race.stage == Stage::Gathering && all(5) {
            // gathering may have seen readiness for an earlier route: the grid needs a new report
            for s in self.participant_serials() {
                self.reset_console_observation(s, 0);
            }
            let grid_ms = self.settings.relay.race_grid_ms as i64;
            let now = self.now;
            let Some(race) = self.ev.race.as_mut() else {
                return;
            };
            race.stage = Stage::Grid;
            race.stage_until = now + grid_ms;
            let participants = race.participants.clone();
            let order: Vec<u8> = race.grid.as_ref().map_or_else(
                || participants.clone(),
                |g| {
                    g.iter()
                        .copied()
                        .filter(|i| participants.contains(i))
                        .collect()
                },
            );
            self.note("event.grid", vec![json!(participants.len())]);
            self.watch_event_start("event: starting grid");
            for receiver in &participants {
                let grid: Vec<Handle> = order
                    .iter()
                    .filter_map(|i| self.peers.iter().find(|p| p.index == *i))
                    .filter_map(|p| {
                        let id = p.console_id?;
                        Some(if p.index == *receiver {
                            Handle::own(id)
                        } else {
                            Handle::relay(id, p.index)
                        })
                    })
                    .collect();
                if let Some(s) = self.serial_of(*receiver) {
                    self.send_sequenced(s, vec![starting_grid(&grid)]);
                }
            }
            self.begin_tag();
        } else if race.stage == Stage::Grid && all(0) {
            let start = self.now_wire().wrapping_add(5000);
            let Some(race) = self.ev.race.as_mut() else {
                return;
            };
            race.stage = Stage::Started;
            let participants = race.participants.clone();
            for index in &participants {
                if let Some(s) = self.serial_of(*index) {
                    self.send_sequenced(s, vec![start_race(start)]);
                }
            }
            // T24: free-roam players see the race in progress from "GO"
            self.set_host_variables(&[
                (COUNTDOWN_TYPE, COUNTDOWN_START_RACE, 2),
                (6, 1, 1),
                (RACE_START_TIME, u64::from(start), 32),
            ]);
            self.note("event.started", vec![json!(participants.len())]);
            self.watch_event_start("event: race start");
            self.tag_started(self.now + 5000);
            self.begin_event_progress(start);
        }
    }

    fn pass_score(&mut self, serial: u64, kind: u8, body: &Bits) -> bool {
        self.tick_race();
        let Some(index) = self.index_of(serial) else {
            return false;
        };
        let Some(race) = self.ev.race.as_ref().filter(|r| r.stage == Stage::Started) else {
            return false;
        };
        let participant = race.participants.contains(&index);
        let participants = race.participants.clone();
        let payload = if participant {
            score_for_others(body, index)
        } else {
            None
        };
        let Some(payload) = payload else {
            let why = if participant {
                "not its own handle"
            } else {
                "not a participant"
            };
            self.event_progress_ignored(index, kind, why);
            return false;
        };
        let m = Arc::new(wire::write(&Msg::Other {
            kind,
            body: payload,
        }));
        let others: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.serial != serial && p.synced && participants.contains(&p.index))
            .map(|p| p.serial)
            .collect();
        for o in others {
            if kind == 43 {
                self.send(o, vec![m.clone()], false, 0);
            } else {
                self.send_sequenced(o, vec![m.clone()]);
            }
        }
        if kind == 43 {
            self.event_progress_score(index, body);
        } else if kind == 44 {
            self.event_progress_lap(index, body);
        }
        true
    }

    fn finished(&mut self, serial: u64) {
        let Some(index) = self.index_of(serial) else {
            return;
        };
        if self.tag_too_early_to_finish() {
            return;
        }
        let now = self.now;
        let Some(race) = self
            .ev
            .race
            .as_mut()
            .filter(|r| r.stage == Stage::Started && r.participants.contains(&index))
        else {
            return;
        };
        if !race.finished.insert(index) {
            return;
        }
        race.first_finish_at.get_or_insert(now);
        self.reevaluate_race_completion();
    }

    fn reevaluate_race_completion(&mut self) {
        let Some(race) = self.ev.race.as_ref().filter(|r| r.stage == Stage::Started) else {
            return;
        };
        if race.participants.is_empty() {
            self.end_race("every participant left");
        } else if race.participants.iter().all(|i| race.finished.contains(i)) {
            self.end_race("every participant finished");
        } else if !self.tag_assigned()
            && race
                .first_finish_at
                .is_some_and(|f| self.now - f >= self.settings.relay.race_finish_ms as i64)
        {
            self.end_race("finish window over");
        }
    }

    pub(super) fn end_race(&mut self, why: &str) {
        if self.ev.race.is_none() {
            return;
        }
        self.tag_ended();
        let Some(race) = self.ev.race.take() else {
            return;
        };
        // TG-2: the owner's game writes only deltas, so the options outlive the event (compacted)
        if self.compact_game_options(true) && !self.ev.options.records().is_empty() {
            self.ev.options_outlived = true;
        }
        self.team_event_ended();
        if self.ev.event_owner == Some(race.owner) && !self.alive(race.owner_serial) {
            self.ev.event_owner = None;
        }
        self.note("event.closed", vec![json!(why)]);
        self.event_closed(&race);
        self.road_trip_event_closed();
        // T24/T38: with finishers on their results only InRace clears now
        let vars = if self.hold_event_end(&race) {
            vec![(6, 0, 1)]
        } else {
            self.event_end_variables()
        };
        self.set_host_variables(&vars);
        for index in &race.participants {
            if let Some(s) = self.serial_of(*index) {
                self.set_console_variables(s, &[(2, false), (4, false)]);
            }
        }
    }

    /// A participant leaves the event (`serial` `None` or gone: it left the engine).
    fn remove_race_participant(&mut self, index: u8, serial: Option<u64>) {
        let Some(race) = self.ev.race.as_mut() else {
            return;
        };
        if !race.participants.contains(&index) {
            return;
        }
        race.participants.retain(|i| *i != index);
        race.finished.remove(&index);
        race.in_progress.remove(&index);
        let (left, owner_left) = (
            race.participants.len(),
            serial == Some(race.owner_serial) && race.stage != Stage::Started,
        );
        self.event_progress_left(index);
        self.team_player_left(index);
        let alive = serial.filter(|s| self.alive(*s));
        if let Some(s) = alive {
            self.set_console_variables(s, &[(2, false), (4, false)]);
        }
        self.leave_event_side(index, alive.is_some());
        if left != 0 {
            self.tag_left(index);
        }
        if left == 0 {
            self.end_race("every participant left");
        } else if owner_left {
            self.end_race("its owner left before the start");
        } else {
            self.reevaluate_race_completion();
        }
    }

    /// A player left the engine.
    pub(super) fn race_peer_left(&mut self, index: u8, serial: u64) {
        if self.ev.race.is_none() {
            if self.ev.event_owner == Some(index) {
                self.ev.event_owner = None;
            }
            return;
        }
        // the event keeps its ownership until it ends, even if its owner left
        self.remove_race_participant(index, Some(serial));
        self.race_step();
    }

    pub(super) fn tick_race(&mut self) {
        if let Some(race) = &self.ev.race {
            if race.stage == Stage::Inviting && self.now >= race.invite_until {
                self.select_participants();
            } else if matches!(race.stage, Stage::Gathering | Stage::Grid)
                && self.now >= race.stage_until
            {
                self.stage_timed_out();
            } else {
                self.reevaluate_race_completion();
            }
        }
        self.tick_tag();
        self.tick_event_progress();
    }

    /// Loading or the grid ran out of time: only the stuck players leave. When nobody reported,
    /// one car may hold them all: the newest joiner other than the owner leaves first.
    fn stage_timed_out(&mut self) {
        let Some(race) = &self.ev.race else { return };
        let var = if race.stage == Stage::Gathering { 5 } else { 0 };
        let reported = |i: u8| {
            self.peers
                .iter()
                .find(|p| p.index == i)
                .is_some_and(|p| p.console_values[var] != 0)
        };
        let stuck: Vec<u8> = race
            .participants
            .iter()
            .copied()
            .filter(|i| !reported(*i))
            .collect();
        if stuck.is_empty() {
            return;
        }
        let stage = race.stage.describe();
        let drop: Vec<u8> = if stuck.len() < race.participants.len() {
            stuck
        } else {
            match stuck.iter().rev().find(|i| **i != race.owner) {
                Some(suspect) => vec![*suspect],
                None => {
                    self.end_race(&format!("nobody finished {stage} in time"));
                    return;
                }
            }
        };
        let step = (self.settings.relay.race_prepare_ms as i64).min(STUCK_STEP_MS);
        let now = self.now;
        if let Some(race) = self.ev.race.as_mut() {
            race.stage_until = now + step;
        }
        self.note("event.stage_timeout", vec![json!(stage), json!(drop)]);
        for index in drop {
            match self.serial_of(index) {
                Some(s) => self.remove_race_participant(index, Some(s)),
                None => {
                    if let Some(r) = self.ev.race.as_mut() {
                        r.participants.retain(|i| *i != index);
                    }
                }
            }
        }
        if self.ev.race.is_some() {
            self.race_step();
        }
    }

    // ---- the options (10-12, router 6 transfer key) ----

    fn ownership(&mut self, serial: u64, body: &Bits) -> Vec<M> {
        self.tick_game_options();
        let Some(p) = self.peer(serial) else {
            return Vec::new();
        };
        if p.doomed {
            return Vec::new();
        }
        let index = p.index;
        let Some(handle) = ownership_request(body) else {
            return Vec::new();
        };
        // a repeated grant must not jump over an unresolved source fragment
        if self.is_event_owner(serial) && self.ev.options.pending_count() != 0 {
            self.note("options.held", vec![json!(index)]);
            return Vec::new();
        }
        let now = self.now;
        let Some(p) = self.peer_mut(serial) else {
            return Vec::new();
        };
        let previous = p
            .lobby_sequences
            .get(&handle.id)
            .copied()
            .unwrap_or((254, now - OWNERSHIP_WINDOW_MS));
        if now - previous.1 < OWNERSHIP_WINDOW_MS {
            return Vec::new();
        }
        let sequence = seq_next(previous.0);
        p.lobby_sequences.insert(handle.id, (sequence, now));
        match self.ev.event_owner {
            Some(current) if !self.is_event_owner(serial) => {
                // during an event its owner keeps the options; between events they move
                if self.ev.race.is_some() {
                    self.note("options.refused", vec![json!(index), json!(current)]);
                    return vec![transfer_ownership(handle, sequence)];
                }
                self.note("options.moved", vec![json!(current), json!(index)]);
            }
            _ => self.note("options.granted", vec![json!(index)]),
        }
        // options kept past an event are their writer's: another player builds its own
        if self.ev.options_outlived && self.ev.options.source_owner != Some(serial) {
            self.note("options.previous_cleared", vec![json!(index)]);
            self.clear_game_options();
        }
        self.ev.event_owner = Some(index);
        self.ev.options.begin_source_epoch(serial);
        let records = self.ev.options.records().to_vec();
        if self.ev.options.object_id != Some(handle.id) || records.is_empty() {
            return vec![grant_ownership(handle, sequence, &Bits::new())];
        }
        // the grant carries the first fragment, the rest follow as updates in its sequence domain
        let mut replay = vec![grant_ownership(handle, sequence, &records[0])];
        for r in &records[1..] {
            replay.push(self.game_options_update(serial, handle.id, r));
        }
        replay
    }

    /// The owner's write of the options' object: published in order to the others (a held
    /// joiner gets it at its release).
    pub(super) fn on_game_options(&mut self, serial: u64, obj: &Obj) {
        self.tick_game_options();
        if self.peer(serial).is_none_or(|p| p.doomed) || !self.is_event_owner(serial) || obj.create
        {
            return;
        }
        let now = self.now;
        let mut ready = Vec::new();
        let mut result = self.ev.options.apply(
            serial,
            obj.handle.id,
            obj.seq,
            &obj.payload,
            now,
            &mut ready,
        );
        if result == Applied::TooLong {
            // not stored, not relayed; its writer's next fragment would wait on it forever, so
            // its session ends, as an overflow's does
            let (xuid, index) = self.peer(serial).map_or((0, 0), |p| (p.xuid, p.index));
            let bits = obj.payload.len() as u32;
            let id = if obj.create { 7 } else { 8 };
            self.rejected(xuid, "game_options_too_long", Some(id), Some(6), Some(bits));
            if self.peer_note(serial, "options_too_long") {
                self.note("options.too_long", vec![json!(index), json!(bits)]);
            }
            self.doom(serial, Why::OptionsOverflow);
            return;
        }
        // a long session of deltas fills the journal: its merged values take less
        if result == Applied::Overflow
            && obj.payload.len() <= wire::MAX_PAYLOAD_BITS
            && self.compact_game_options(false)
        {
            result = self.ev.options.apply(
                serial,
                obj.handle.id,
                obj.seq,
                &obj.payload,
                now,
                &mut ready,
            );
        }
        if result == Applied::Overflow {
            let xuid = self.peer(serial).map_or(0, |p| p.xuid);
            let id = if obj.create { 7 } else { 8 };
            self.rejected(xuid, "game_options_overflow", Some(id), Some(6), None);
            // a live, incomplete object nobody could recover: end this writer's session
            self.doom(serial, Why::OptionsOverflow);
            return;
        }
        if result != Applied::Stored {
            return;
        }
        let others: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.serial != serial && p.synced && !self.holding.contains_key(&p.serial))
            .map(|p| p.serial)
            .collect();
        for o in others {
            let updates: Vec<M> = ready
                .iter()
                .map(|r| self.game_options_update(o, obj.handle.id, r))
                .collect();
            self.send_sequenced(o, updates);
        }
    }

    fn game_options_update(&mut self, serial: u64, id: u32, payload: &Bits) -> M {
        let now = self.now;
        let next = match self.peer_mut(serial) {
            Some(p) => {
                let previous = p
                    .lobby_sequences
                    .get(&id)
                    .copied()
                    .unwrap_or((254, now - OWNERSHIP_WINDOW_MS));
                let next = seq_next(previous.0);
                p.lobby_sequences.insert(id, (next, previous.1));
                next
            }
            None => 0,
        };
        Arc::new(wire::write(&Msg::Object(Obj {
            create: false,
            router: 6,
            handle: Handle {
                kind: Kind::Transfer,
                id,
                peer: 0,
            },
            ty: 0,
            seq: next,
            payload: payload.clone(),
        })))
    }

    /// The journal for a released joiner (not its owner: the grant carried it), after whatever it
    /// was really sent in that sequence domain.
    pub(super) fn append_game_options(&mut self, serial: u64, held: &mut Vec<M>) {
        let Some(id) = self.ev.options.object_id else {
            return;
        };
        if self.is_event_owner(serial) {
            return;
        }
        let records = self.ev.options.records().to_vec();
        for r in &records {
            held.push(self.game_options_update(serial, id, r));
        }
    }

    fn clear_game_options(&mut self) {
        self.ev.options.clear();
        self.ev.options_outlived = false;
    }

    /// Compacts the journal; one that does not read is kept, or with `clear_unread` (an event's
    /// end) cleared. True when it read.
    fn compact_game_options(&mut self, clear_unread: bool) -> bool {
        let before = self.ev.options.records().len();
        if before == 0 {
            return true;
        }
        if !self.ev.options.compact() {
            if clear_unread {
                self.note("options.unread_cleared", vec![json!(before)]);
                self.clear_game_options();
            } else {
                self.note("options.unread_kept", vec![json!(before)]);
            }
            return false;
        }
        let after = self.ev.options.records().len();
        if after < before {
            self.note("options.compacted", vec![json!(before), json!(after)]);
        }
        true
    }

    /// PlayerChoices (router-1 var 81 CarClass s32, 82 PerfIndex f32), read when a write begins
    /// there, as a car choice does.
    pub(super) fn observe_car_choice(&mut self, serial: u64, payload: &Bits) {
        if payload.len() < 8 + 32 {
            return;
        }
        let mut r = payload.reader();
        if r.read(8) != Ok(81) {
            return;
        }
        let Ok(class) = r.read(32) else { return };
        let class = class as u32 as i32;
        let mut pi = f32::NAN;
        if r.left() >= 8 + 32
            && r.read(8) == Ok(82)
            && let Ok(bits) = r.read(32)
        {
            pi = f32::from_bits(bits as u32);
        }
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        p.car_class = Some(class);
        p.car_pi = pi;
        let (refused, index) = (p.ready_refused, p.index);
        if refused
            && self
                .ev
                .race
                .as_ref()
                .is_some_and(|r| r.stage == Stage::Inviting)
            && self.event_car_rule().accepts(Some(class), pi)
        {
            self.note("ready.car_fits", vec![json!(index)]);
            self.ready(serial, true);
        }
    }

    pub(super) fn tick_game_options(&mut self) {
        let gap = self.settings.relay.game_options_gap_ms as i64;
        if let Some(owner) = self.ev.options.expired_source(self.now, gap) {
            self.doom(owner, Why::OptionsStalled);
        }
    }

    // ---- event progress (T24, T24c) ----

    /// Forgets the progress state and any held event end (the launcher's reset, the clear).
    pub fn event_progress_reset(&mut self) {
        self.ev.progress = None;
        self.ev.progress_written = false;
        self.ev.held = false;
    }

    fn begin_event_progress(&mut self, start: u32) {
        self.ev.progress = None;
        let wire_now = self.now_wire();
        self.note("progress.start_time", vec![json!(start), json!(wire_now)]);
        let fields = Fields::read(self.ev.options.records());
        if self.is_road_trip() {
            self.note("progress.off", vec![json!("a Road Trip event")]);
        } else if self.tag_running() {
            self.note("progress.off", vec![json!("a tag event")]);
        } else if let Some(fields) = fields {
            match (fields.end_condition(), fields.num_laps()) {
                (Some(end), _) if end != LAPS_END_CONDITION => {
                    self.note("progress.off_end_condition", vec![json!(end)]);
                }
                (_, Some(laps)) if (1..=255).contains(&laps) => {
                    self.ev.progress = Some(Progress {
                        laps: laps as i32,
                        started_at: self.now,
                        ..Progress::default()
                    });
                    self.note("progress.followed", vec![json!(laps)]);
                }
                _ => self.note("progress.off", vec![json!("lap count unknown")]),
            }
        } else {
            self.note("progress.off", vec![json!("the options do not read")]);
        }
    }

    fn event_progress_score(&mut self, index: u8, body: &Bits) {
        let now = self.now;
        let Some(progress) = self.ev.progress.as_mut() else {
            return;
        };
        progress.last_score_at = Some(now);
        let Some((raw, flag)) = in_progress_score(body) else {
            self.refused_score(index, json!("?"), None);
            return;
        };
        if progress.first_noted.insert(index) {
            self.note(
                "progress.first",
                vec![json!(43), json!(index), f32_arg(raw), json!(u8::from(flag))],
            );
        }
        let value = raw.abs();
        if !value.is_finite() {
            self.refused_score(index, f32_arg(raw), Some("not finite"));
            return;
        }
        let Some(progress) = self.ev.progress.as_mut() else {
            return;
        };
        if value > (progress.laps + 1) as f32 {
            let above = progress.laps + 1;
            if progress.refused_noted.insert(index) {
                self.note(
                    "progress.refused_above",
                    vec![json!(43), json!(index), f32_arg(raw), json!(above)],
                );
            }
            return;
        }
        progress.progress.insert(index, value);
        self.publish_event_progress();
    }

    fn refused_score(&mut self, index: u8, value: Value, why: Option<&str>) {
        let Some(progress) = self.ev.progress.as_mut() else {
            return;
        };
        if progress.refused_noted.insert(index) {
            let why = why.unwrap_or("does not read");
            self.note(
                "progress.refused",
                vec![json!(43), json!(index), value, json!(why)],
            );
        }
    }

    fn event_progress_ignored(&mut self, index: u8, kind: u8, why: &str) {
        if !matches!(kind, 43 | 44) {
            return;
        }
        if let Some(progress) = self.ev.progress.as_mut()
            && progress.ignored_noted.insert(index)
        {
            self.note(
                "progress.score_ignored",
                vec![json!(kind), json!(index), json!(why)],
            );
        }
    }

    fn event_progress_lap(&mut self, index: u8, body: &Bits) {
        let Some(progress) = self.ev.progress.as_mut() else {
            return;
        };
        let Some(fields) = lap_score(body) else {
            if progress.refused_laps.insert(index) {
                self.note("progress.lap_layout", vec![json!(index)]);
            }
            return;
        };
        let done = progress.laps_done.get(&index).copied().unwrap_or(0);
        let at = progress.progress.get(&index).copied();
        let shown = format!(
            "u32 {}, f64 {}/{}/{}, flags {}{}",
            fields.count,
            round_trip(fields.values[0]),
            round_trip(fields.values[1]),
            round_trip(fields.values[2]),
            u8::from(fields.flag89),
            u8::from(fields.flag8b)
        );
        // T24c: a 44 at the start line (progress about 0) is not a lap
        let reached = match at {
            Some(r) if r >= done as f32 + LAP_SCORE_MIN_PROGRESS => r,
            _ => {
                if progress.start_line_noted.insert(index) {
                    match at {
                        Some(p) => self.note(
                            "progress.start_line",
                            vec![f32_arg(p), json!(index), json!(shown)],
                        ),
                        None => {
                            self.note("progress.start_line_none", vec![json!(index), json!(shown)])
                        }
                    }
                }
                return;
            }
        };
        if done >= progress.laps {
            return;
        }
        progress.laps_done.insert(index, done + 1);
        let laps = progress.laps;
        let noted = progress.lap_notes < LAP_NOTES;
        progress.lap_notes += 1;
        if noted {
            self.note(
                "progress.lap",
                vec![
                    json!(done + 1),
                    json!(laps),
                    json!(index),
                    f32_arg(reached),
                    json!(shown),
                ],
            );
        }
        self.publish_event_progress();
    }

    fn event_progress_left(&mut self, index: u8) {
        if let Some(progress) = self.ev.progress.as_mut() {
            progress.progress.remove(&index);
            progress.laps_done.remove(&index);
        }
    }

    fn tick_event_progress(&mut self) {
        let now = self.now;
        let racing = self
            .ev
            .race
            .as_ref()
            .filter(|r| r.stage == Stage::Started)
            .map(|r| r.participants.len());
        if let (Some(progress), Some(count)) = (self.ev.progress.as_mut(), racing)
            && !progress.quiet_noted
            && now - progress.last_score_at.unwrap_or(progress.started_at) >= QUIET_MS
        {
            progress.quiet_noted = true;
            self.note(
                "progress.quiet",
                vec![json!(43), json!(QUIET_MS / 1000), json!(count)],
            );
        }
        self.publish_event_progress();
    }

    /// The maxima over the participants, as high-water marks: NumLapsCompleted at once,
    /// FurthestProgress at most once a second after a 0.5 % change, at once when a completed
    /// lap lifts it or it reaches 100 %.
    fn publish_event_progress(&mut self) {
        let now = self.now;
        let Some(race) = self.ev.race.as_ref().filter(|r| r.stage == Stage::Started) else {
            return;
        };
        let participants = race.participants.clone();
        let Some(progress) = self.ev.progress.as_mut() else {
            return;
        };
        for index in &participants {
            let laps = progress.laps_done.get(index).copied().unwrap_or(0);
            let at = progress.progress.get(index).copied().unwrap_or(0.0);
            let furthest = (at.max(laps as f32) / progress.laps as f32).clamp(0.0, 1.0);
            progress.num_laps_completed = progress.num_laps_completed.max(laps);
            progress.furthest = progress.furthest.max(furthest);
        }
        let mut updates: Vec<(u8, u64, u32)> = Vec::new();
        if progress.num_laps_completed != progress.sent_laps {
            progress.sent_laps = progress.num_laps_completed;
            updates.push((NUM_LAPS_COMPLETED, progress.num_laps_completed as u64, 8));
        }
        let floor = (progress.num_laps_completed as f32 / progress.laps as f32).min(1.0);
        let due = progress.furthest > progress.sent_furthest
            && (progress.furthest >= 1.0
                || floor > progress.sent_furthest
                || (progress.furthest - progress.sent_furthest >= FURTHEST_MIN_DELTA
                    && progress
                        .sent_furthest_at
                        .is_none_or(|at| now - at >= FURTHEST_INTERVAL_MS)));
        if due {
            progress.sent_furthest = progress.furthest;
            progress.sent_furthest_at = Some(now);
            updates.push((FURTHEST_PROGRESS, host_float(progress.furthest), 32));
        }
        if updates.is_empty() {
            return;
        }
        let noted = progress.update_notes;
        progress.update_notes += 1;
        self.ev.progress_written = true;
        self.set_host_variables(&updates);
        if noted >= UPDATE_NOTES {
            return;
        }
        let sent = updates
            .iter()
            .map(|(var, value, _)| {
                if *var == NUM_LAPS_COMPLETED {
                    format!("NumLapsCompleted {value}")
                } else {
                    format!("FurthestProgress {:.3}", f32::from_bits(*value as u32))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if noted + 1 == UPDATE_NOTES {
            self.note("progress.sent_last", vec![json!(sent)]);
        } else {
            self.note("progress.sent", vec![json!(sent)]);
        }
    }

    /// The start time and, when some was sent, the progress (the event's end clears them).
    fn event_progress_cleared(&mut self) -> Vec<(u8, u64, u32)> {
        self.ev.progress = None;
        let cleared = if self.ev.progress_written {
            vec![
                (RACE_START_TIME, 0, 32),
                (NUM_LAPS_COMPLETED, 0, 8),
                (FURTHEST_PROGRESS, 0, 32),
            ]
        } else {
            vec![(RACE_START_TIME, 0, 32)]
        };
        self.ev.progress_written = false;
        cleared
    }

    fn event_end_variables(&mut self) -> Vec<(u8, u64, u32)> {
        let mut v = vec![(COUNTDOWN_TYPE, 0, 2), (4, 0, 1), (5, 0, 1), (6, 0, 1)];
        v.extend(self.event_progress_cleared());
        v
    }

    fn held_cleared(&mut self) -> Vec<(u8, u64, u32)> {
        let mut v = vec![(4, 0, 1), (5, 0, 1), (COUNTDOWN_TYPE, 0, 2)];
        v.extend(self.event_progress_cleared());
        v
    }

    /// T38: the event closed with finishers on their results: only InRace clears now; RaceReady,
    /// participants and the HUD are held until the side empties, a new event, or 120 s.
    fn hold_event_end(&mut self, race: &Race) -> bool {
        if race.stage != Stage::Started
            || self.is_road_trip()
            || !self.ev.phasing
            || self.ev.event_side.is_empty()
        {
            return false;
        }
        self.ev.progress = None;
        self.ev.held = true;
        self.ev.held_at = self.now;
        let count = self.ev.event_side.len();
        self.note("event.hold", vec![json!(count)]);
        true
    }

    fn release_event_hold(&mut self, why: &str) {
        if !self.ev.held {
            return;
        }
        self.ev.held = false;
        if why == BACKSTOP {
            self.note(
                "event.hold_released_backstop",
                vec![json!(EVENT_END_HOLD_MS / 1000)],
            );
        } else {
            self.note("event.hold_released", vec![json!(why)]);
        }
        let vars = self.held_cleared();
        self.set_host_variables(&vars);
    }

    fn tick_event_hold(&mut self) {
        // never under another open event (RequestRace releases it first)
        if self.ev.held && self.ev.race.is_none() && self.now - self.ev.held_at >= EVENT_END_HOLD_MS
        {
            self.release_event_hold(BACKSTOP);
        }
    }

    // ---- phasing ----

    /// Whether two players are on the same side of the event boundary.
    pub(super) fn same_side(&self, a: u64, b: u64) -> bool {
        if !self.ev.phasing {
            return true;
        }
        let side = |s: u64| {
            self.index_of(s)
                .is_some_and(|i| self.ev.event_side.contains(&i))
        };
        side(a) == side(b)
    }

    /// Participants were selected: they form the event side; every player gets a universe.
    fn begin_phase(&mut self, participants: &[u8]) {
        self.ev.event_side = participants.iter().copied().collect();
        self.ev.results_kept.clear();
        self.watch_event_start("event: participants selected");
        if !self.ev.phasing {
            self.ev.phasing = true;
            for s in self.synced_serials_except(None) {
                self.assign_universe(s);
            }
        }
        self.refresh_cars();
    }

    /// A player left the event side (back in free roam, dropped, or gone).
    pub(super) fn leave_event_side(&mut self, index: u8, alive: bool) {
        self.ev.results_kept.remove(&index);
        if !self.ev.phasing || !self.ev.event_side.remove(&index) {
            return;
        }
        if alive {
            self.refresh_cars();
        }
        self.end_phase_when_done();
    }

    /// The event closed: participants still in it keep the side; a finisher already seen going
    /// 1 → 0 is kept for at most 35 s; any other participant at 0 is back in free roam.
    fn event_closed(&mut self, race: &Race) {
        if !self.ev.phasing {
            return;
        }
        self.ev.phase_released_at = self.now;
        self.ev.results_kept.clear();
        let road_trip = self.is_road_trip();
        for index in race.participants.clone() {
            let Some(p) = self.peers.iter().find(|p| p.index == index) else {
                self.ev.event_side.remove(&index);
                continue;
            };
            if p.console_values[3] != 0 {
                continue;
            }
            let serial = p.serial;
            if race.stage == Stage::Started
                && race.finished.contains(&index)
                && !road_trip
                && self.ev.event_side.contains(&index)
                && (race.results_noted.contains(&index) || self.ev.race_exits.contains(&serial))
            {
                self.ev.results_kept.insert(index);
            } else {
                self.ev.event_side.remove(&index);
            }
        }
        if !self.ev.results_kept.is_empty() {
            self.ev.results_kept_until = self.now + RESULTS_EXIT_MS;
            let count = self.ev.results_kept.len();
            self.note(
                "phasing.kept",
                vec![json!(count), json!(0), json!(RESULTS_EXIT_MS / 1000)],
            );
        }
        self.refresh_cars();
        self.end_phase_when_done();
    }

    fn tick_results_kept(&mut self) {
        if self.ev.results_kept.is_empty() || self.now < self.ev.results_kept_until {
            return;
        }
        let kept = std::mem::take(&mut self.ev.results_kept);
        let released = kept.iter().filter(|i| self.ev.event_side.remove(i)).count();
        if !self.ev.phasing || released == 0 {
            return;
        }
        self.note(
            "phasing.kept_released",
            vec![json!(released), json!(RESULTS_EXIT_MS / 1000)],
        );
        self.refresh_cars();
        self.end_phase_when_done();
    }

    fn end_phase_when_done(&mut self) {
        if !self.ev.phasing || self.ev.race.is_some() || !self.ev.event_side.is_empty() {
            return;
        }
        self.ev.phasing = false;
        self.note("phasing.done", Vec::new());
        self.release_event_hold("nobody is on the event side any more");
        let phased: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.universe != 0)
            .map(|p| p.serial)
            .collect();
        for s in phased {
            self.set_universe(s, 0);
        }
        self.refresh_cars();
    }

    pub(super) fn tick_phasing(&mut self) {
        self.tick_results_kept();
        self.tick_event_hold();
        if self.ev.phasing
            && self.ev.race.is_none()
            && self.now - self.ev.phase_released_at >= PHASE_HOLD_MS
            && !self.ev.event_side.is_empty()
        {
            let count = self.ev.event_side.len();
            self.note("phasing.results_released", vec![json!(count)]);
            self.ev.event_side.clear();
            self.end_phase_when_done();
        }
    }

    pub(super) fn assign_universe(&mut self, serial: u64) {
        let Some(p) = self.peer(serial) else { return };
        if p.universe != 0 || self.bucket_list_busy(serial) {
            return;
        }
        if let Some(u) = self.free_universe() {
            self.set_universe(serial, u);
        }
    }

    /// The first private universe (1..17) nobody holds.
    pub(super) fn free_universe(&self) -> Option<u8> {
        (FIRST_PRIVATE_UNIVERSE..=LAST_PRIVATE_UNIVERSE).find(|u| {
            self.peers.iter().all(|p| p.universe != *u) && !self.bucket_list_universe_held(*u)
        })
    }

    /// The server's PrivateUniverseId of a player: to it, and on its relayed player object.
    pub(super) fn set_universe(&mut self, serial: u64, universe: u8) {
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        p.universe = universe;
        if !p.synced {
            return;
        }
        let Some(id) = p.objects.iter().find(|(k, _)| k.0 == 1).map(|(k, _)| k.1) else {
            return;
        };
        let index = p.index;
        let mut payload = Bits::new();
        payload
            .push(u64::from(super::payload::PRIVATE_UNIVERSE_VARIABLE), 8)
            .push(u64::from(universe), super::payload::PRIVATE_UNIVERSE_BITS);
        let seq = p.player_sequence.map_or(0, seq_next);
        p.player_sequence = Some(seq);
        let own = Arc::new(wire::write(&Msg::Object(Obj {
            create: false,
            router: 1,
            handle: Handle::server(id),
            ty: 0,
            seq,
            payload: payload.clone(),
        })));
        self.send_sequenced(serial, vec![own]);
        let Some(obj) = self
            .peer_mut(serial)
            .and_then(|p| p.objects.get_mut(&(1, id)))
        else {
            return;
        };
        obj.injected = seq_next(obj.injected);
        obj.universe = (universe != 0).then_some(universe);
        let seq = wire::seq_add(obj.last, obj.injected);
        let m = Arc::new(wire::write(&Msg::Object(Obj {
            create: false,
            router: 1,
            handle: Handle::relay(id, index),
            ty: 0,
            seq,
            payload,
        })));
        self.forward(serial, vec![m], true);
    }

    /// The boundary moved: each parked car's last prediction again, one tick newer.
    fn refresh_cars(&mut self) {
        let senders: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.synced && p.last_car.is_some())
            .map(|p| p.serial)
            .collect();
        for s in senders {
            let Some((handle, body)) = self.peer(s).and_then(|p| p.last_car.clone()) else {
                continue;
            };
            let body = super::join::restamp(&body);
            if let Some(p) = self.peer_mut(s) {
                p.last_car = Some((handle, body.clone()));
            }
            for r in self.synced_serials_except(Some(s)) {
                let car = self.car_for(s, r, handle, &body);
                self.send(r, vec![car], false, 0);
            }
        }
    }
}

/// The hold's backstop reason.
const BACKSTOP: &str = "backstop";
