// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The anti-cheat watch (AC-2, NR-5, NR-6; MATCHMAKING_API §3.9 and §5e, contract §4.3 and §4.5):
//! while the controller set rules, the engine measures every other player's car from its car
//! predictions (speed, climb, jumps, the clock ratio), checks its installed parts against the
//! game database's rows (the parts rule, rows from the controller through the catalog store),
//! and sends each proven joiner the watch roster (who is behind each peer index). Never the
//! host's own car (its XUID, a loopback peer) nor the shared XUIDs. Deterministic: every time
//! comes from the caller. Logs carry peer indexes, models and counts only.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};

use super::{Engine, Event, bits::Bits, dict::DotMap, payload::PRIVATE_UNIVERSE_BITS};
use crate::fh2::catalog::{CatalogStore, ModelParts, PartsLookup, Requester};

/// The rules (the reference's `PeerWatchRules.From(rules, forceSpeed)`).
#[derive(Debug, Clone, PartialEq)]
pub struct WatchRules {
    pub version: u32,
    pub speed_max_kmh: f64,
    pub speed_sustain_ms: u32,
    pub climb_max_kmh: f64,
    pub climb_sustain_ms: u32,
    pub jump_max_meters: f64,
    /// AC-5, tests only: one synthetic speed episode for the next eligible peer.
    pub force_speed: bool,
    /// NR-5: `speed.perCarFloorKmh` (`None`: a server older than NR-5).
    pub speed_floor_kmh: Option<f64>,
}

impl WatchRules {
    /// NR-5: speed against the per-car floor, with the held speed and the car.
    pub fn per_car(&self) -> bool {
        self.speed_floor_kmh.is_some()
    }

    /// What speed is measured against: the floor, else the flat limit.
    pub fn speed_measured_kmh(&self) -> f64 {
        self.speed_floor_kmh.unwrap_or(self.speed_max_kmh)
    }
}

/// One ended episode (`watch.report`).
#[derive(Debug, Clone, PartialEq)]
pub struct WatchReport {
    pub xuid: u64,
    pub kind: &'static str,
    pub value: f64,
    pub sustain_ms: Option<u32>,
    pub rules_version: u32,
    pub peer: u8,
    pub car_id: Option<u32>,
    pub held_kmh: Option<f64>,
}

/// The watch roster's STUN kind and attribute (§5e).
pub const ROSTER_KIND: u16 = 0x3E95;
const ROSTER_ATTRIBUTE: u16 = 0xC0F7;
const ROSTER_MAX: usize = 12;
const ROSTER_MS: i64 = 30_000;

const WINDOW_MS: i64 = 250;
const GAP_MS: i64 = 1500;
const QUIET_MS: i64 = 2000;
const CLOCK_MIN_MS: i64 = 30_000;
const MAX_CARS: usize = 32;
/// The parts rule reports a player at most once per car model, for at most this many models per
/// player and this many (player, model) pairs per world: a watched peer cycling impossible rows
/// or models can't grow the set or the report stream without bound.
const MAX_PART_REPORTS_PER_PLAYER: usize = 4;
const MAX_PART_REPORTS: usize = 1024;
/// A process-unique id per parts watch: the catalog's per-world need budget.
static NEXT_PARTS_WATCH: AtomicU64 = AtomicU64::new(1);
/// CarPredictionFull: velocity s16 × 3 at bit 96; the universe (5 bits) at 395; the stamp last.
const VELOCITY_OFFSET: usize = 96;
const CAR_UNIVERSE_OFFSET: usize = 395;
/// The velocity's provisional scale (±32767 = ±200 m/s per axis).
const VELOCITY_SCALE: f64 = 200.0 / 32767.0;
const HELD_SEGMENTS: usize = 64;
const MAX_CAR_ID: u32 = 1_000_000;
/// §3.9: a `parts` value is a whole row id under 1e9.
const MAX_PART_VALUE: u32 = 1_000_000_000;

const SPEED: &str = "speed";
const CLIMB: &str = "climb";
const JUMP: &str = "jump";
const CLOCK: &str = "clock";
const PARTS: &str = "parts";

// ---- number formatting as the reference writes its notes (invariant culture) ----

/// `value.ToString("0.0…")` with `decimals` places: the value's first `significant` digits
/// (15 for a double, 7 for a single), then rounded half away from zero on those digits.
fn fixed(value: f64, decimals: usize, significant: usize) -> String {
    if !value.is_finite() {
        return format!("{value}");
    }
    let negative = value.is_sign_negative();
    let sci = format!("{:.*e}", significant - 1, value.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i64 = exp.parse().unwrap_or(0);
    let digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    // digit i has the place value 10^(exp - i); keep places down to 10^-decimals
    let keep = exp + 1 + decimals as i64;
    let mut kept: Vec<u8> = if keep <= 0 {
        Vec::new()
    } else {
        digits.iter().take(keep as usize).copied().collect()
    };
    while (kept.len() as i64) < keep {
        kept.push(0);
    }
    let round_up = keep >= 0 && digits.get(keep as usize).is_some_and(|d| *d >= 5);
    // the integer of kept digits, at scale 10^-decimals
    let mut n: Vec<u8> = if kept.is_empty() { vec![0] } else { kept };
    if round_up {
        let mut i = n.len();
        loop {
            if i == 0 {
                n.insert(0, 1);
                break;
            }
            i -= 1;
            if n[i] == 9 {
                n[i] = 0;
            } else {
                n[i] += 1;
                break;
            }
        }
    }
    while n.len() <= decimals {
        n.insert(0, 0);
    }
    let split = n.len() - decimals;
    let mut int: String = n[..split].iter().map(|d| (b'0' + d) as char).collect();
    let trimmed = int.trim_start_matches('0');
    int = if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.into()
    };
    let frac: String = n[split..].iter().map(|d| (b'0' + d) as char).collect();
    let sign = if negative { "-" } else { "" };
    if decimals == 0 {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    }
}

fn f0(v: f64) -> String {
    fixed(v, 0, 15)
}

/// A note's number hole from a formatted value.
fn num(s: &str) -> Value {
    s.parse::<f64>().map_or(Value::Null, |f| json!(f))
}

/// `Math.Round(value, digits)` (to even).
fn round_even(v: f64, digits: i32) -> f64 {
    let p = 10f64.powi(digits);
    (v * p).round_ties_even() / p
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// What the watch tells its engine, in order.
#[derive(Debug, Clone)]
enum Out {
    Note(&'static str, Vec<Value>),
    Report(WatchReport),
}

// ---- the player object's model and parts (NR-6) ----

const MODEL_VARIABLE: u8 = 103;
const PARTS_VARIABLE: u8 = 107;
const NO_PART: u32 = u32::MAX;
/// The installed-upgrade slots (var 107).
pub const PART_SLOTS: usize = 43;

/// The widths (bits) of player-object variables; 0: unknown (a write reaching it stops), -2:
/// a 5-bit count of UTF-16 units.
fn player_width(index: u8) -> i32 {
    match index {
        0 | 4 => 64,
        1 | 2 | 5 | 7 => 1,
        3 => -2,
        6 | 8..=11 => 32,
        81..=88 => 32,
        89 | 90 => 1,
        91 => 3,
        92 => 32,
        93 | 94 => 64,
        95..=97 => 2,
        98 | 99 => 1,
        100 => 2,
        101 => 1,
        102 => 3,
        103..=106 => 32,
        107 => 32 * PART_SLOTS as i32,
        108..=120 | 122..=143 => 32,
        121 => 9 * 32,
        144 => 1,
        145..=148 => 32,
        149..=153 => 1,
        154..=157 => 2,
        158..=161 => 32,
        _ => 0,
    }
}

/// The model and the part rows a player-object write sets (each `None` when absent or past an
/// unknown variable; a truncated write keeps what was read before it).
fn player_parts(payload: &Bits) -> (Option<u32>, Option<Vec<u32>>) {
    let (mut model, mut parts) = (None, None);
    let mut r = payload.reader();
    loop {
        if r.left() < 8 {
            break;
        }
        let Ok(index) = r.read(8) else { break };
        let index = index as u8;
        let width = player_width(index);
        if width == 0 {
            break;
        }
        if index == MODEL_VARIABLE {
            match r.read(32) {
                Ok(m) => model = Some(m as u32),
                Err(_) => break,
            }
            continue;
        }
        if index == PARTS_VARIABLE {
            let mut read = Vec::with_capacity(PART_SLOTS);
            for _ in 0..PART_SLOTS {
                match r.read(32) {
                    Ok(p) => read.push(p as u32),
                    Err(_) => break,
                }
            }
            if read.len() != PART_SLOTS {
                break;
            }
            parts = Some(read);
            continue;
        }
        let skip = if width == -2 {
            match r.read(5) {
                Ok(n) => 16 * n as usize,
                Err(_) => break,
            }
        } else {
            width as usize
        };
        if r.skip(skip).is_err() {
            break;
        }
    }
    (model, parts)
}

// ---- the car measurement (AC-2) ----

#[derive(Debug, Clone, Default)]
struct Episode {
    active: bool,
    peak: f64,
    peak_velocity: f64,
    peak_sender: f64,
    peak_receiver: f64,
    held: f64,
    run: i64,
    max_run: i64,
    last_over_at: i64,
    sender_ms: i64,
    receiver_ms: i64,
    disagreed: i32,
    windows: Vec<(f64, i64)>,
    windows_ms: i64,
}

impl Episode {
    fn reset_run(&mut self) {
        self.run = 0;
        self.windows.clear();
        self.windows_ms = 0;
    }

    /// NR-5: the held speed is the larger of itself and the lowest value of the shortest
    /// suffix of the run lasting at least `sustain`.
    fn hold(&mut self, value: f64, ms: i64, sustain: u32) {
        if self.windows.len() >= HELD_SEGMENTS {
            self.windows[1] = (
                self.windows[0].0.min(self.windows[1].0),
                self.windows[0].1 + self.windows[1].1,
            );
            self.windows.remove(0);
        }
        self.windows.push((value, ms));
        self.windows_ms += ms;
        while self.windows.len() > 1 && self.windows_ms - self.windows[0].1 >= i64::from(sustain) {
            self.windows_ms -= self.windows[0].1;
            self.windows.remove(0);
        }
        if self.windows_ms < i64::from(sustain) {
            return;
        }
        let lowest = self.windows.iter().fold(f64::MAX, |m, (v, _)| m.min(*v));
        self.held = self.held.max(lowest);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Speed,
    Climb,
}

impl Which {
    fn kind(self) -> &'static str {
        match self {
            Which::Speed => SPEED,
            Which::Climb => CLIMB,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Car {
    index: u8,
    xuid: u64,
    has: bool,
    position: [f32; 3],
    anchor_position: [f32; 3],
    stamp: u32,
    anchor_stamp: u32,
    at: i64,
    anchor_at: i64,
    universe: u8,
    speed: Episode,
    climb: Episode,
    jump_active: bool,
    jump_peak: f64,
    jump_last: i64,
    jumps: i32,
    clock_sender: i64,
    clock_receiver: i64,
    clock_sent: bool,
    velocity_horizontal: f64,
    velocity_up: f64,
    velocity_samples: i32,
    first: i32,
    universes: i32,
    stale: i32,
    events: i32,
}

impl Car {
    fn episode(&mut self, which: Which) -> &mut Episode {
        match which {
            Which::Speed => &mut self.speed,
            Which::Climb => &mut self.climb,
        }
    }

    fn restart(&mut self, position: [f32; 3], stamp: u32, universe: u8, at: i64) {
        self.has = true;
        self.position = position;
        self.stamp = stamp;
        self.at = at;
        self.universe = universe;
        self.speed.reset_run();
        self.climb.reset_run();
        self.anchor();
    }

    fn anchor(&mut self) {
        self.anchor_position = self.position;
        self.anchor_stamp = self.stamp;
        self.anchor_at = self.at;
        self.velocity_horizontal = 0.0;
        self.velocity_up = 0.0;
        self.velocity_samples = 0;
    }
}

/// The car measurement of every other player (the reference's `PeerWatch`).
#[derive(Debug, Clone)]
struct PeerWatch {
    rules: WatchRules,
    cars: DotMap<u8, Car>,
    /// NR-5: each peer's car ordinal (var 103) from its own player object.
    models: BTreeMap<u8, (u64, Option<u32>)>,
    forced: bool,
    units_logged: bool,
}

impl PeerWatch {
    fn new(rules: WatchRules) -> PeerWatch {
        PeerWatch {
            rules,
            cars: DotMap::new(),
            models: BTreeMap::new(),
            forced: false,
            units_logged: false,
        }
    }

    fn report(&self, out: &mut Vec<Out>, r: WatchReport) {
        let _ = self;
        out.push(Out::Report(r));
    }

    fn model(&mut self, index: u8, xuid: u64, model: Option<u32>, fresh: bool, out: &mut Vec<Out>) {
        if xuid == 0 {
            return;
        }
        let had = self.models.get(&index).copied();
        let known = had.is_some_and(|h| h.0 == xuid) && !fresh;
        if !known && had.is_none() && self.models.len() >= MAX_CARS {
            return;
        }
        let had_model = had.and_then(|h| h.1);
        let next = model.or(if known { had_model } else { None });
        if known && next == had_model {
            return;
        }
        self.models.insert(index, (xuid, next));
        if let Some(car) = next
            && (!known || had_model != Some(car))
        {
            out.push(Out::Note(
                "watch.car",
                vec![json!(index), json!(car), json!(MODEL_VARIABLE)],
            ));
        }
    }

    fn car_id(&self, car: &Car) -> Option<u32> {
        self.models
            .get(&car.index)
            .filter(|m| m.0 == car.xuid)
            .and_then(|m| m.1)
            .filter(|m| *m <= MAX_CAR_ID)
    }

    #[allow(clippy::too_many_arguments)]
    fn sample(
        &mut self,
        index: u8,
        xuid: u64,
        position: [f32; 3],
        velocity: [f32; 3],
        stamp: u32,
        universe: u8,
        at: i64,
        out: &mut Vec<Out>,
    ) {
        if xuid == 0
            || !position
                .iter()
                .chain(velocity.iter())
                .all(|v| v.is_finite())
        {
            return;
        }
        match self.cars.get(&index).map(|c| c.xuid) {
            Some(x) if x == xuid => {}
            existing => {
                if existing.is_some() {
                    self.end(index, at, true, out);
                }
                if existing.is_none() && self.cars.len() >= MAX_CARS {
                    return;
                }
                self.cars.insert(
                    index,
                    Car {
                        index,
                        xuid,
                        ..Car::default()
                    },
                );
            }
        }
        if self.rules.force_speed && !self.forced {
            self.forced = true;
            out.push(Out::Note("watch.forced", vec![json!(index)]));
            let value = self.rules.speed_max_kmh + 50.0;
            let r = WatchReport {
                xuid,
                kind: SPEED,
                value,
                sustain_ms: Some(self.rules.speed_sustain_ms.wrapping_add(1000)),
                rules_version: self.rules.version,
                peer: index,
                car_id: None,
                held_kmh: self.rules.per_car().then_some(value),
            };
            self.report(out, r);
        }
        self.end_quiet(index, at, out);
        let rules = self.rules.clone();
        let Some(car) = self.cars.get_mut(&index) else {
            return;
        };
        if !car.has || at - car.at > GAP_MS {
            if car.has {
                out.push(Out::Note(
                    "watch.first_sample",
                    vec![json!(index), json!(at - car.at)],
                ));
            }
            car.first = car.first.saturating_add(1);
            car.restart(position, stamp, universe, at);
            return;
        }
        if universe != car.universe {
            car.universes = car.universes.saturating_add(1);
            out.push(Out::Note(
                "watch.universe",
                vec![json!(index), json!(car.universe), json!(universe)],
            ));
            car.restart(position, stamp, universe, at);
            return;
        }
        let sent = stamp.wrapping_sub(car.stamp) as i32;
        if sent <= 0 {
            car.stale = car.stale.saturating_add(1);
            return;
        }
        let moved = f64::from(distance(position, car.position));
        if moved > rules.jump_max_meters {
            let received = at - car.at;
            if !car.jump_active {
                car.jump_active = true;
                car.jump_peak = moved;
                car.jumps = 0;
            }
            car.jump_peak = car.jump_peak.max(moved);
            car.jumps = car.jumps.saturating_add(1);
            car.jump_last = at;
            out.push(Out::Note(
                "watch.jump",
                vec![json!(index), num(&f0(moved)), json!(sent), json!(received)],
            ));
            car.restart(position, stamp, universe, at);
            return;
        }
        car.position = position;
        car.stamp = stamp;
        car.at = at;
        let (vx, vy, vz) = (
            f64::from(velocity[0]),
            f64::from(velocity[1]),
            f64::from(velocity[2]),
        );
        car.velocity_horizontal += (vx * vx + vz * vz).sqrt();
        car.velocity_up += 0f64.max(vy);
        car.velocity_samples = car.velocity_samples.saturating_add(1);
        let window_received = at - car.anchor_at;
        let window_sent = stamp.wrapping_sub(car.anchor_stamp) as i32;
        if window_received < WINDOW_MS {
            return;
        }
        if window_sent <= 0 {
            car.anchor();
            return;
        }
        let delta = [
            position[0] - car.anchor_position[0],
            position[1] - car.anchor_position[1],
            position[2] - car.anchor_position[2],
        ];
        let (dx, dy, dz) = (
            f64::from(delta[0]),
            f64::from(delta[1]),
            f64::from(delta[2]),
        );
        let horizontal = (dx * dx + dz * dz).sqrt();
        let up = 0f64.max(dy);
        let (ws, wr) = (f64::from(window_sent), window_received as f64);
        let (speed_sent, speed_received) = (horizontal / ws * 3600.0, horizontal / wr * 3600.0);
        let (climb_sent, climb_received) = (up / ws * 3600.0, up / wr * 3600.0);
        let samples = f64::from(car.velocity_samples.max(1));
        let speed_velocity = car.velocity_horizontal / samples * 3.6;
        let climb_velocity = car.velocity_up / samples * 3.6;
        if !self.units_logged {
            self.units_logged = true;
            out.push(Out::Note(
                "watch.units",
                vec![
                    json!(index),
                    json!(format!(
                        "velocity {} km/h horizontal, {} km/h up; position delta {} m horizontal, {} m vertical: {} km/h on {} stamp units (sender), {} km/h on {} ms (receiver)",
                        f0(speed_velocity),
                        f0(climb_velocity),
                        fixed(horizontal, 1, 15),
                        fixed(dy, 1, 7),
                        f0(speed_sent),
                        window_sent,
                        f0(speed_received),
                        window_received
                    )),
                ],
            ));
        }
        car.clock_sender += i64::from(window_sent);
        car.clock_receiver += window_received;
        let measured_speed = rules.speed_measured_kmh();
        Self::step(
            &rules,
            car,
            Which::Speed,
            [speed_velocity, speed_sent, speed_received],
            measured_speed,
            window_sent,
            window_received,
            at,
            out,
        );
        Self::step(
            &rules,
            car,
            Which::Climb,
            [climb_velocity, climb_sent, climb_received],
            rules.climb_max_kmh,
            window_sent,
            window_received,
            at,
            out,
        );
        car.anchor();
        // the clock ratio, once per car per rules (log only)
        if !car.clock_sent && car.clock_receiver >= CLOCK_MIN_MS && car.clock_sender > 0 {
            car.clock_sent = true;
            let ratio = car.clock_sender as f64 / car.clock_receiver as f64;
            out.push(Out::Note(
                "watch.clock",
                vec![
                    json!(index),
                    num(&fixed(ratio, 3, 15)),
                    json!(car.clock_sender),
                    json!(car.clock_receiver),
                ],
            ));
            if ratio > 0.0 && ratio <= 1000.0 {
                out.push(Out::Report(WatchReport {
                    xuid: car.xuid,
                    kind: CLOCK,
                    value: round_even(ratio, 3),
                    sustain_ms: None,
                    rules_version: rules.version,
                    peer: index,
                    car_id: None,
                    held_kmh: None,
                }));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step(
        rules: &WatchRules,
        car: &mut Car,
        which: Which,
        measures: [f64; 3],
        maximum: f64,
        window_sent: i32,
        window_received: i64,
        at: i64,
        out: &mut Vec<Out>,
    ) {
        let [velocity, sent, received] = measures;
        let index = car.index;
        let e = car.episode(which);
        let (ov, os, or) = (velocity > maximum, sent > maximum, received > maximum);
        if ov && os && or {
            if !e.active {
                *e = Episode {
                    active: true,
                    ..Episode::default()
                };
                out.push(Out::Note(
                    "watch.episode_starts",
                    vec![
                        json!(index),
                        json!(which.kind()),
                        json!(format!(
                            "{} km/h (velocity), {} km/h (sender clock), {} km/h (receiver clock), over {}",
                            f0(velocity),
                            f0(sent),
                            f0(received),
                            f0(maximum)
                        )),
                    ],
                ));
            }
            e.run += window_received;
            e.max_run = e.max_run.max(e.run);
            let measured = velocity.min(sent.min(received));
            e.peak = e.peak.max(measured);
            if which == Which::Speed && rules.per_car() {
                e.hold(measured, window_received, rules.speed_sustain_ms);
            }
            e.peak_velocity = e.peak_velocity.max(velocity);
            e.peak_sender = e.peak_sender.max(sent);
            e.peak_receiver = e.peak_receiver.max(received);
            e.sender_ms += i64::from(window_sent);
            e.receiver_ms += window_received;
            e.last_over_at = at;
            return;
        }
        e.reset_run();
        if (ov || os || or) && e.active {
            e.disagreed = e.disagreed.saturating_add(1);
        }
    }

    /// An event starts: every car restarts its measurement.
    fn exclude(&mut self, why: &str, out: &mut Vec<Out>) {
        for (_, car) in self.cars.iter_mut() {
            car.has = false;
            car.events = car.events.saturating_add(1);
            car.speed.reset_run();
            car.climb.reset_run();
        }
        if !self.cars.is_empty() {
            out.push(Out::Note("watch.restart", vec![json!(why)]));
        }
    }

    fn forget(&mut self, index: u8, at: i64, out: &mut Vec<Out>) {
        self.models.remove(&index);
        if let Some(mut car) = self.cars.remove(&index) {
            self.end_car(&mut car, at, true, out);
        }
    }

    fn tick(&mut self, at: i64, out: &mut Vec<Out>) {
        for index in self.cars.keys() {
            self.end_quiet(index, at, out);
        }
    }

    fn flush(&mut self, at: i64, out: &mut Vec<Out>) {
        for index in self.cars.keys() {
            self.end(index, at, true, out);
        }
        self.cars.clear();
        self.models.clear();
    }

    fn end_quiet(&mut self, index: u8, at: i64, out: &mut Vec<Out>) {
        let Some(mut car) = self.cars.get(&index).cloned() else {
            return;
        };
        self.end_episode(&mut car, Which::Speed, at, false, out);
        self.end_episode(&mut car, Which::Climb, at, false, out);
        if car.jump_active && at - car.jump_last >= QUIET_MS {
            self.end_jump(&mut car, out);
        }
        if let Some(c) = self.cars.get_mut(&index) {
            *c = car;
        }
    }

    fn end(&mut self, index: u8, at: i64, force: bool, out: &mut Vec<Out>) {
        let Some(mut car) = self.cars.get(&index).cloned() else {
            return;
        };
        self.end_car(&mut car, at, force, out);
        if let Some(c) = self.cars.get_mut(&index) {
            *c = car;
        }
    }

    fn end_car(&mut self, car: &mut Car, at: i64, force: bool, out: &mut Vec<Out>) {
        self.end_episode(car, Which::Speed, at, force, out);
        self.end_episode(car, Which::Climb, at, force, out);
        if car.jump_active {
            self.end_jump(car, out);
        }
    }

    fn end_jump(&self, car: &mut Car, out: &mut Vec<Out>) {
        car.jump_active = false;
        out.push(Out::Note(
            "watch.jump_episode_ends",
            vec![
                json!(car.index),
                json!(format!(
                    "{} jump(s), longest {} m; reported (log only)",
                    car.jumps,
                    f0(car.jump_peak)
                )),
            ],
        ));
        out.push(Out::Report(WatchReport {
            xuid: car.xuid,
            kind: JUMP,
            value: round_even(car.jump_peak, 1),
            sustain_ms: None,
            rules_version: self.rules.version,
            peer: car.index,
            car_id: None,
            held_kmh: None,
        }));
    }

    fn end_episode(&self, car: &mut Car, which: Which, at: i64, force: bool, out: &mut Vec<Out>) {
        let sustain = match which {
            Which::Speed => self.rules.speed_sustain_ms,
            Which::Climb => self.rules.climb_sustain_ms,
        };
        let per_car = which == Which::Speed && self.rules.per_car();
        let car_id = if per_car { self.car_id(car) } else { None };
        let (xuid, index, first, universes, stale, events, jumps) = (
            car.xuid,
            car.index,
            car.first,
            car.universes,
            car.stale,
            car.events,
            car.jumps,
        );
        let e = car.episode(which);
        if !e.active || !force && at - e.last_over_at < QUIET_MS {
            return;
        }
        e.active = false;
        let counted = e.max_run >= i64::from(sustain);
        let held = if per_car && counted {
            let h = (e.held * 10.0).floor() / 10.0;
            (h > 0.0).then_some(h)
        } else {
            None
        };
        let per_car_part = if per_car {
            format!(
                ", held {} km/h, car {}",
                f0(e.held),
                car_id.map_or("unknown".to_string(), |c| c.to_string())
            )
        } else {
            String::new()
        };
        out.push(Out::Note(
            "watch.episode_ends",
            vec![
                json!(index),
                json!(which.kind()),
                json!(format!(
                    "peak {} km/h (velocity {}, sender {}, receiver {}){}, longest run {} ms of {}, over for {} ms sender / {} ms receiver, measures disagreed {}x, exclusions first {} universe {} stale {} event {} jump {}: {}",
                    f0(e.peak),
                    f0(e.peak_velocity),
                    f0(e.peak_sender),
                    f0(e.peak_receiver),
                    per_car_part,
                    e.max_run,
                    sustain,
                    e.sender_ms,
                    e.receiver_ms,
                    e.disagreed,
                    first,
                    universes,
                    stale,
                    events,
                    jumps,
                    if counted {
                        "reported"
                    } else {
                        "under the rule, not reported"
                    }
                )),
            ],
        ));
        if counted {
            out.push(Out::Report(WatchReport {
                xuid,
                kind: which.kind(),
                value: (e.peak * 10.0).ceil() / 10.0,
                sustain_ms: Some(e.max_run.min(3_600_000) as u32),
                rules_version: self.rules.version,
                peer: index,
                car_id,
                held_kmh: held,
            }));
        }
    }
}

// ---- the parts rule (NR-6) ----

#[derive(Debug, Clone)]
struct PartsCar {
    xuid: u64,
    model: Option<u32>,
    parts: Option<Vec<u32>>,
    episode: Option<(u32, u32)>,
    unknown_model_logged: Option<u32>,
    checked: Option<String>,
    /// The model's rows were asked for and have not come yet: checked again on the tick.
    awaiting: bool,
}

/// The parts rule (the reference's `PartsWatch`), with the model rows from the catalog store.
#[derive(Debug)]
struct PartsWatch {
    store: Arc<Mutex<CatalogStore>>,
    rules_version: u32,
    cars: DotMap<u8, PartsCar>,
    /// (player, model) pairs reported, and models reported per player (both capped).
    reported: HashSet<(u64, u32)>,
    per_player: HashMap<u64, usize>,
    /// This watch's world in the catalog's need budget.
    world: u64,
}

fn is_part(p: u32) -> bool {
    p != NO_PART && p != 0
}

impl PartsWatch {
    fn lookup(&self, model: u32, xuid: u64, now: i64) -> PartsLookup {
        let who = Requester {
            world: self.world,
            peer: xuid,
        };
        self.store.lock().map_or(PartsLookup::Unknown, |mut s| {
            s.parts_for(model, now.max(0) as u64, who)
        })
    }

    /// Whether `(xuid, model)` may be reported now (and record it): once per pair, within the
    /// per-player and per-world caps.
    fn may_report(&mut self, xuid: u64, model: u32) -> bool {
        if self.reported.contains(&(xuid, model))
            || self.reported.len() >= MAX_PART_REPORTS
            || self.per_player.get(&xuid).copied().unwrap_or(0) >= MAX_PART_REPORTS_PER_PLAYER
        {
            return false;
        }
        self.reported.insert((xuid, model));
        *self.per_player.entry(xuid).or_insert(0) += 1;
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn observe(
        &mut self,
        index: u8,
        xuid: u64,
        model: Option<u32>,
        parts: Option<Vec<u32>>,
        fresh: bool,
        now: i64,
        out: &mut Vec<Out>,
    ) {
        if xuid == 0 || parts.as_ref().is_some_and(|p| p.len() != PART_SLOTS) {
            return;
        }
        let existing = self.cars.get(&index).map(|c| c.xuid);
        if existing != Some(xuid) || fresh {
            if let Some(car) = self.cars.get_mut(&index) {
                Self::end(index, car, "a new player object", out);
            }
            if existing.is_none() && self.cars.len() >= MAX_CARS {
                return;
            }
            self.cars.insert(
                index,
                PartsCar {
                    xuid,
                    model: None,
                    parts: None,
                    episode: None,
                    unknown_model_logged: None,
                    checked: None,
                    awaiting: false,
                },
            );
        }
        if let Some(car) = self.cars.get_mut(&index) {
            if model.is_some() {
                car.model = model;
            }
            if parts.is_some() {
                car.parts = parts;
            }
        }
        self.check(index, now, out);
    }

    /// Cars whose model rows were awaited: judged now if they came.
    fn retry(&mut self, now: i64, out: &mut Vec<Out>) {
        for index in self.cars.keys() {
            if self.cars.get(&index).is_some_and(|c| c.awaiting) {
                self.check(index, now, out);
            }
        }
    }

    fn check(&mut self, index: u8, now: i64, out: &mut Vec<Out>) {
        let Some(car) = self.cars.get(&index) else {
            return;
        };
        let (Some(model), Some(installed)) = (car.model, car.parts.clone()) else {
            return;
        };
        let joined: Vec<String> = installed.iter().map(u32::to_string).collect();
        let key = format!("{model}:{}", joined.join(","));
        if car.checked.as_ref() == Some(&key) {
            return; // the same car again: nothing new to judge
        }
        // the rows come from the controller: until they do, nothing is judged (never a flag)
        let rows: Arc<ModelParts> = match self.lookup(model, car.xuid, now) {
            PartsLookup::Known(rows) => rows,
            PartsLookup::Unknown => {
                if let Some(car) = self.cars.get_mut(&index) {
                    car.awaiting = true;
                }
                return;
            }
        };
        let version = self.rules_version;
        let Some(car) = self.cars.get_mut(&index) else {
            return;
        };
        car.awaiting = false;
        car.checked = Some(key);
        let firsts: Vec<String> = installed
            .iter()
            .enumerate()
            .filter(|(_, p)| is_part(**p))
            .take(6)
            .map(|(slot, p)| format!("{slot}:{p}"))
            .collect();
        out.push(Out::Note(
            "parts.rows",
            vec![
                json!(index),
                json!(MODEL_VARIABLE),
                json!(model),
                json!(PARTS_VARIABLE),
                json!(format!(
                    "{} x 0xFFFFFFFF, {} x 0, first rows (slot:row) {}",
                    installed.iter().filter(|p| **p == NO_PART).count(),
                    installed.iter().filter(|p| **p == 0).count(),
                    firsts.join(" ")
                )),
            ],
        ));
        if !rows.known {
            if car.unknown_model_logged != Some(model) {
                out.push(Out::Note(
                    "parts.unknown_model",
                    vec![json!(index), json!(model)],
                ));
            }
            car.unknown_model_logged = Some(model);
            Self::end(index, car, "a model this database does not know", out);
            return;
        }
        let all = rows.all();
        let in_slot = |slot: usize, p: u32| rows.by_slot.get(slot).is_some_and(|s| s.contains(&p));
        let present: Vec<u32> = installed.iter().copied().filter(|p| is_part(*p)).collect();
        let known = present.iter().filter(|p| all.contains(p)).count();
        // an omitted table can't be checked: its slot counts as present and is never judged
        let has = |slot: usize| !rows.judged(slot) || present.iter().any(|p| in_slot(slot, *p));
        if !(has(0) || has(3)) || !has(1) || !has(2) {
            out.push(Out::Note(
                "parts.layout",
                vec![
                    json!(index),
                    json!(model),
                    json!(present.len()),
                    json!(known),
                ],
            ));
            Self::end(index, car, "an unrecognised layout", out);
            return;
        }
        let (mut outside, mut misplaced, mut unsendable) = (0usize, 0usize, 0usize);
        let mut offending = None;
        for (slot, &part) in installed.iter().enumerate() {
            if !is_part(part) || !rows.judged(slot) {
                continue;
            }
            if !all.contains(&part) {
                outside += 1;
                if part < MAX_PART_VALUE {
                    offending = offending.or(Some(part));
                } else {
                    unsendable += 1;
                }
            } else if !in_slot(slot, part) {
                misplaced += 1;
            }
        }
        let not = if unsendable == 0 {
            format!("{outside} not")
        } else {
            format!("{outside} not ({unsendable} of them 1e9 or over)")
        };
        out.push(Out::Note(
            "parts.summary",
            vec![
                json!(index),
                json!(model),
                json!(present.len()),
                json!(known),
                json!(not),
                json!(misplaced),
            ],
        ));
        let Some(first) = offending else {
            let why = if outside == 0 {
                "every part row belongs to the car"
            } else {
                "its only impossible rows are 1e9 or over (not sendable)"
            };
            Self::end(index, car, why, out);
            return;
        };
        if car.episode == Some((model, first)) {
            return;
        }
        if car.episode.is_some() {
            Self::end(index, car, "another impossible part", out);
        }
        car.episode = Some((model, first));
        let xuid = car.xuid;
        if !self.may_report(xuid, model) {
            out.push(Out::Note(
                "parts.already",
                vec![json!(index), json!(model), json!(first), json!(version)],
            ));
            return;
        }
        out.push(Out::Note(
            "parts.reported",
            vec![json!(index), json!(model), json!(first)],
        ));
        out.push(Out::Report(WatchReport {
            xuid,
            kind: PARTS,
            value: f64::from(first),
            sustain_ms: None,
            rules_version: version,
            peer: index,
            car_id: None,
            held_kmh: None,
        }));
    }

    fn end(index: u8, car: &mut PartsCar, why: &str, out: &mut Vec<Out>) {
        let Some((model, _)) = car.episode.take() else {
            return;
        };
        out.push(Out::Note(
            "parts.episode_ends",
            vec![json!(index), json!(model), json!(why)],
        ));
    }

    fn forget(&mut self, index: u8, out: &mut Vec<Out>) {
        if let Some(mut car) = self.cars.remove(&index) {
            Self::end(index, &mut car, "the player left", out);
        }
    }

    fn flush(&mut self, out: &mut Vec<Out>) {
        for (index, car) in self.cars.iter_mut() {
            Self::end(*index, car, "the rules ended", out);
        }
        self.cars.clear();
    }
}

/// An engine's watch state.
#[derive(Debug)]
pub(crate) struct Watch {
    peer: Option<PeerWatch>,
    parts: Option<PartsWatch>,
    self_xuid: u64,
    roster_at: i64,
    roster_sent: String,
}

impl Default for Watch {
    fn default() -> Watch {
        Watch {
            peer: None,
            parts: None,
            self_xuid: 0,
            roster_at: i64::MIN,
            roster_sent: String::new(),
        }
    }
}

/// The watch roster: `{1, count ≤ 12, count × {index, xuid u64 BE}}` in 0xC0F7 of kind 0x3E95.
pub fn roster(entries: &[(u8, u64)], txid: &[u8; 12]) -> Vec<u8> {
    let entries = &entries[..entries.len().min(ROSTER_MAX)];
    let mut v = vec![1u8, entries.len() as u8];
    for (index, xuid) in entries {
        v.push(*index);
        v.extend_from_slice(&xuid.to_be_bytes());
    }
    crate::fh2::voice::indication(ROSTER_KIND, txid, &[(ROSTER_ATTRIBUTE, &v)])
}

impl Engine {
    /// The rules to measure against (`None`: stop; every episode in flight ends now), the
    /// host's own XUID (never measured), whether ended episodes have somewhere to go (`false`:
    /// no watch), and the catalog store of the parts rule (`None`: no parts rule).
    pub fn set_watch(
        &mut self,
        rules: Option<WatchRules>,
        self_xuid: u64,
        report: bool,
        parts: Option<Arc<Mutex<CatalogStore>>>,
        now_ms: i64,
    ) {
        self.now = self.now.max(now_ms);
        let now = self.now;
        let mut out = Vec::new();
        if let Some(w) = self.watch.peer.as_mut() {
            w.flush(now, &mut out);
        }
        if let Some(p) = self.watch.parts.as_mut() {
            p.flush(&mut out);
        }
        self.drain_watch(out);
        let rules = rules.filter(|_| report);
        self.watch.parts = match (&rules, parts) {
            (Some(r), Some(store)) => Some(PartsWatch {
                store,
                rules_version: r.version,
                cars: DotMap::new(),
                reported: HashSet::new(),
                per_player: HashMap::new(),
                world: NEXT_PARTS_WATCH.fetch_add(1, Ordering::Relaxed),
            }),
            _ => None,
        };
        self.watch.self_xuid = self_xuid;
        self.watch.roster_at = i64::MIN;
        self.watch.roster_sent.clear();
        self.watch.peer = rules.clone().map(PeerWatch::new);
        if let Some(r) = rules {
            let per_car = if r.per_car() {
                format!(
                    " (speed against the per-car floor {} km/h, with the held speed and the car)",
                    r.speed_measured_kmh()
                )
            } else {
                String::new()
            };
            let parts = if self.watch.parts.is_none() {
                "; no parts rule (no readable game database)"
            } else {
                ", their parts too"
            };
            self.note(
                "watch.on",
                vec![json!(r.version), json!(format!("{per_car}{parts}"))],
            );
        }
    }

    /// Whether the watch runs.
    pub fn watching(&self) -> bool {
        self.watch.peer.is_some()
    }

    fn drain_watch(&mut self, out: Vec<Out>) {
        for o in out {
            match o {
                Out::Note(id, args) => self.note(id, args),
                Out::Report(r) => self.events.push(Event::WatchReport(r)),
            }
        }
    }

    fn watched(&self, serial: u64) -> bool {
        self.peer(serial).is_some_and(|p| {
            p.xuid != self.watch.self_xuid
                && !super::shared_xuid(p.xuid)
                && !p.endpoint.ip().is_loopback()
        })
    }

    /// NR-6 and NR-5: a peer's own player object (router 1, sanitised) as it arrived on its own
    /// leg: its model (the car of its speed reports) and part rows.
    pub(super) fn watch_player_object(&mut self, serial: u64, create: bool, payload: &Bits) {
        let Some(per_car) = self.watch.peer.as_ref().map(|w| w.rules.per_car()) else {
            return;
        };
        if !self.watched(serial) || self.watch.parts.is_none() && !per_car {
            return;
        }
        let Some((index, xuid)) = self.peer(serial).map(|p| (p.index, p.xuid)) else {
            return;
        };
        let (model, rows) = player_parts(payload);
        if model.is_none() && rows.is_none() && !create {
            return;
        }
        let now = self.now;
        let mut out = Vec::new();
        if per_car && let Some(w) = self.watch.peer.as_mut() {
            w.model(index, xuid, model, create, &mut out);
        }
        if let Some(p) = self.watch.parts.as_mut() {
            p.observe(index, xuid, model, rows, create, now, &mut out);
        }
        self.drain_watch(out);
    }

    /// A peer's own car prediction, as it arrived (its position already read and bounded).
    pub(super) fn watch_car(&mut self, serial: u64, body: &Bits) {
        if self.watch.peer.is_none() || !self.watched(serial) {
            return;
        }
        let Some(p) = self.peer(serial) else { return };
        let (Some(position), true) = (p.car_position, p.synced) else {
            return;
        };
        let (index, xuid) = (p.index, p.xuid);
        let mut r = body.reader();
        let mut read = |skip: usize, width: u32| -> u64 {
            if r.skip(skip).is_err() {
                return 0;
            }
            r.read(width).unwrap_or(0)
        };
        let axis = |raw: u64| ((raw as u16 as i16) as f64 * VELOCITY_SCALE) as f32;
        let vx = axis(read(VELOCITY_OFFSET, 16));
        let vy = axis(read(0, 16));
        let vz = axis(read(0, 16));
        let universe = read(
            CAR_UNIVERSE_OFFSET - VELOCITY_OFFSET - 48,
            PRIVATE_UNIVERSE_BITS,
        ) as u8;
        let stamp = read(
            super::packets::CAR_STAMP_OFFSET - CAR_UNIVERSE_OFFSET - PRIVATE_UNIVERSE_BITS as usize,
            32,
        ) as u32;
        let now = self.now;
        let mut out = Vec::new();
        if let Some(w) = self.watch.peer.as_mut() {
            w.sample(
                index,
                xuid,
                position,
                [vx, vy, vz],
                stamp,
                universe,
                now,
                &mut out,
            );
        }
        self.drain_watch(out);
    }

    /// An event starts (participants selected, a starting grid, a race start).
    pub(super) fn watch_event_start(&mut self, why: &str) {
        let mut out = Vec::new();
        if let Some(w) = self.watch.peer.as_mut() {
            w.exclude(why, &mut out);
        }
        self.drain_watch(out);
    }

    pub(super) fn watch_peer_left(&mut self, index: u8) {
        let now = self.now;
        let mut out = Vec::new();
        if let Some(w) = self.watch.peer.as_mut() {
            w.forget(index, now, &mut out);
        }
        if let Some(p) = self.watch.parts.as_mut() {
            p.forget(index, &mut out);
        }
        self.watch.roster_at = i64::MIN;
        self.drain_watch(out);
    }

    pub(super) fn tick_watch(&mut self) {
        let now = self.now;
        let mut out = Vec::new();
        let Some(w) = self.watch.peer.as_mut() else {
            return;
        };
        w.tick(now, &mut out);
        if let Some(p) = self.watch.parts.as_mut() {
            p.retry(now, &mut out);
        }
        self.drain_watch(out);
        let synced: Vec<(u64, u8, u64, std::net::SocketAddr)> = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| (p.serial, p.index, p.xuid, p.endpoint))
            .collect();
        let key: Vec<String> = synced
            .iter()
            .map(|(_, i, x, _)| format!("{i}:{x}"))
            .collect();
        let key = key.join(",");
        if key == self.watch.roster_sent && now.wrapping_sub(self.watch.roster_at) < ROSTER_MS {
            return;
        }
        self.watch.roster_sent = key;
        self.watch.roster_at = now;
        // only to a joiner whose hook proved this very socket (its proxy drops the kind)
        if !self.policy.has_authorize_hook() {
            return;
        }
        for &(serial, _, xuid, endpoint) in &synced {
            if endpoint.ip().is_loopback()
                || !self.allowed(xuid, endpoint)
                || !self.allowed_hook(xuid, endpoint)
            {
                continue;
            }
            let others: Vec<(u8, u64)> = synced
                .iter()
                .filter(|(s, _, x, _)| *s != serial && !super::shared_xuid(*x))
                .take(ROSTER_MAX)
                .map(|(_, i, x, _)| (*i, *x))
                .collect();
            let txid = self.random.transaction();
            self.emit(endpoint, roster(&others, &txid));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fields::Obj;

    fn store_with(models: &[u32]) -> Arc<Mutex<CatalogStore>> {
        let mut s = CatalogStore::default();
        for &m in models {
            let body = json!({"model": m, "slots": {
                "List_UpgradeEngine": [10], "List_UpgradeDrivetrain": [11],
                "List_UpgradeCarBody": [12], "List_UpgradeSpringDamper": []}});
            let Value::Object(o) = body else { panic!() };
            s.set_parts(Obj::new(&o, "")).unwrap();
        }
        Arc::new(Mutex::new(s))
    }

    fn rows(bogus: u32) -> Vec<u32> {
        let mut p = vec![NO_PART; PART_SLOTS];
        (p[0], p[1], p[2], p[5]) = (10, 11, 12, bogus);
        p
    }

    fn reports(out: &[Out]) -> usize {
        out.iter().filter(|o| matches!(o, Out::Report(_))).count()
    }

    fn watch(store: Arc<Mutex<CatalogStore>>) -> PartsWatch {
        PartsWatch {
            store,
            rules_version: 1,
            cars: DotMap::new(),
            reported: HashSet::new(),
            per_player: HashMap::new(),
            world: 1,
        }
    }

    fn answer(store: &Arc<Mutex<CatalogStore>>, body: Value) {
        let Value::Object(o) = body else { panic!() };
        store.lock().unwrap().set_parts(Obj::new(&o, "")).unwrap();
    }

    fn notes(out: &[Out], id: &str) -> usize {
        out.iter()
            .filter(|o| matches!(o, Out::Note(n, _) if *n == id))
            .count()
    }

    #[test]
    fn an_empty_answer_is_a_model_the_database_lacks_and_is_not_judged() {
        let store = store_with(&[]);
        answer(&store, json!({"model": 300, "slots": {}}));
        let mut w = watch(store);
        let mut out = Vec::new();
        w.observe(3, 42, Some(300), Some(rows(5000)), false, 0, &mut out);
        assert_eq!(reports(&out), 0);
        assert_eq!(notes(&out, "parts.unknown_model"), 1);
    }

    #[test]
    fn an_omitted_table_is_not_judged_but_a_sent_empty_one_is() {
        let store = store_with(&[]);
        // slot 5 (List_UpgradeSpringDamper) omitted: its part is not judged
        answer(
            &store,
            json!({"model": 301, "slots": {
            "List_UpgradeEngine": [10], "List_UpgradeDrivetrain": [11], "List_UpgradeCarBody": [12]}}),
        );
        // slot 5 sent with no row: a part there belongs to none of the car's tables
        answer(
            &store,
            json!({"model": 302, "slots": {
            "List_UpgradeEngine": [10], "List_UpgradeDrivetrain": [11], "List_UpgradeCarBody": [12],
            "List_UpgradeSpringDamper": []}}),
        );
        let mut w = watch(store);
        let mut out = Vec::new();
        w.observe(3, 42, Some(301), Some(rows(5000)), false, 0, &mut out);
        assert_eq!(reports(&out), 0);
        out.clear();
        w.observe(4, 43, Some(302), Some(rows(5000)), false, 0, &mut out);
        assert_eq!(reports(&out), 1);
    }

    #[test]
    fn parts_reports_are_once_per_player_and_model_and_capped() {
        let models: Vec<u32> = (100..120).collect();
        let mut w = PartsWatch {
            store: store_with(&models),
            rules_version: 1,
            cars: DotMap::new(),
            reported: HashSet::new(),
            per_player: HashMap::new(),
            world: 1,
        };
        let mut out = Vec::new();
        // the same real model with a different impossible row each update: one report
        for (i, bogus) in (5000..6000).enumerate() {
            w.observe(
                3,
                42,
                Some(100),
                Some(rows(bogus)),
                false,
                i as i64,
                &mut out,
            );
        }
        assert_eq!(reports(&out), 1);
        // a different real model each update: at most the per-player cap
        out.clear();
        for (i, &m) in models.iter().enumerate() {
            w.observe(
                3,
                42,
                Some(m),
                Some(rows(7000 + i as u32)),
                false,
                2000,
                &mut out,
            );
        }
        assert_eq!(reports(&out), MAX_PART_REPORTS_PER_PLAYER - 1);
        assert!(w.reported.len() <= MAX_PART_REPORTS_PER_PLAYER);
    }
}
