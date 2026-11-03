// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! King and Infected, the tag events. The event options say which one runs (GameTypePreset) and
//! its rules (TagOptions). Each game works a hit out and reports it (36: both cars' buckets before
//! and after); the server owns every car's bucket: it draws the first It car(s) while the
//! starting grid is up and tells every participant (37: the receiver's own car by its own key, the
//! others by relay keys). 37 travels unreliably, so each change goes again a few times.

use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use super::{
    Engine,
    options::{Fields, TagMode, TagRules},
    races::Stage,
    random::{LegacyRandom, SeedPurpose},
    wire::{self, Handle, Kind, Msg},
};

/// The first It car(s) are drawn this long after the starting grid.
const TAG_ASSIGN_DELAY_MS: i64 = 1500;
/// A change is sent again this long after its first send.
const TAG_REPEAT_MS: [i64; 3] = [400, 1200, 3000];
const TAG_DEFAULT_LIMIT_MS: i64 = 240_000;
const TAG_LONGEST_LIMIT_MS: i64 = 3_600_000;
/// A final score earlier than this after "GO" is not a finish (the games end on their own clock).
const TAG_EARLIEST_FINISH_MS: i64 = 20_000;
/// The server's own time-up is only a fallback this long after the limit.
const TAG_LATE_CLOSE_MS: i64 = 15_000;
/// A lone Infected car is infected from the start: its event ends this long after "GO".
const TAG_ALONE_CLOSE_MS: i64 = 3000;
/// 37's car slots.
const TAG_SLOTS: usize = 12;
/// A hit counts only between cars this close (metres) by their last accepted predictions, both
/// fresher than `TAG_CAR_FRESH_MS`: generous for latency (two cars at 100 m/s and a quarter
/// second each), yet no participant can tag a car across the map.
const TAG_HIT_RADIUS_M: f32 = 60.0;
const TAG_CAR_FRESH_MS: i64 = 1500;
/// The same two cars can't swap again this soon (no tag-back at message rate; a chain of
/// different cars, as Infected spreads, is not slowed).
const TAG_SWAP_COOLDOWN_MS: i64 = 1000;

#[derive(Debug, Clone)]
pub(super) struct TagGame {
    rules: TagRules,
    limit_ms: i64,
    assign_at: i64,
    assigned: bool,
    /// Each car's bucket by its player's peer index.
    buckets: BTreeMap<u8, u8>,
    /// When each pair of cars (lower index first) last swapped through a hit.
    swapped_at: BTreeMap<(u8, u8), i64>,
    repeats: Vec<i64>,
    go_at: Option<i64>,
    marked: i64,
    held_ms: BTreeMap<u8, i64>,
    hits: BTreeMap<u8, i32>,
}

impl TagGame {
    fn not_it(&self) -> u8 {
        if self.rules.it_bucket == TagRules::NOT_IT {
            TagRules::IT as u8
        } else {
            TagRules::NOT_IT as u8
        }
    }

    fn is_it(&self, index: u8) -> bool {
        i32::from(self.buckets.get(&index).copied().unwrap_or(self.not_it()))
            == self.rules.it_bucket
    }
}

fn describe(mode: TagMode) -> &'static str {
    match mode {
        TagMode::King => "King",
        TagMode::Infected => "Infected",
    }
}

impl Engine {
    fn tag_random(&mut self) -> &mut LegacyRandom {
        if self.tag_random.is_none() {
            let seed = self
                .settings
                .relay
                .tag_seed
                .unwrap_or_else(|| self.random.seed(SeedPurpose::Tag));
            self.tag_random = Some(LegacyRandom::new(seed));
        }
        self.tag_random.as_mut().expect("set above")
    }

    /// The starting grid was sent: a King or Infected event (by its decoded options) gets its
    /// tag game.
    pub(super) fn begin_tag(&mut self) {
        self.tag = None;
        let Some(fields) = Fields::read(self.ev.options.records()) else {
            return;
        };
        let Some(rules) = fields.tag() else { return };
        // TimeOptions.Time is the event's length whatever its end condition
        let time = fields.timed_race_length();
        let limit = match time {
            Some(s) if s > 0.0 => TAG_LONGEST_LIMIT_MS.min((f64::from(s) * 1000.0) as i64),
            _ => TAG_DEFAULT_LIMIT_MS,
        };
        self.tag = Some(TagGame {
            rules,
            limit_ms: limit,
            assign_at: self.now + TAG_ASSIGN_DELAY_MS,
            assigned: false,
            buckets: BTreeMap::new(),
            swapped_at: BTreeMap::new(),
            repeats: Vec::new(),
            go_at: None,
            marked: 0,
            held_ms: BTreeMap::new(),
            hits: BTreeMap::new(),
        });
        let unset = |v: Option<String>| v.unwrap_or_else(|| "unset".into());
        self.note(
            "tag.event",
            vec![
                json!(describe(rules.mode)),
                json!(limit / 1000),
                json!(rules.it_bucket),
                json!(unset(rules.players_per_it.map(|v| v.to_string()))),
                json!(unset(fields.end_condition().map(|v| v.to_string()))),
                json!(unset(time.map(|v| v.to_string()))),
            ],
        );
    }

    /// The start was sent: crown time counts from "GO", and the buckets go out again.
    pub(super) fn tag_started(&mut self, go_at: i64) {
        let now = self.now;
        let Some(tag) = self.tag.as_mut() else { return };
        tag.go_at = Some(go_at);
        tag.marked = now;
        if tag.assigned {
            self.send_tag_buckets(true);
        }
    }

    /// The participants whose car is known, in grid order (at most 37's slots).
    fn tag_cars(&self) -> Vec<(u64, u8, u32)> {
        let Some(race) = &self.ev.race else {
            return Vec::new();
        };
        race.participants
            .iter()
            .filter_map(|i| self.peers.iter().find(|p| p.index == *i))
            .filter(|p| p.synced)
            .filter_map(|p| p.car_id.map(|c| (p.serial, p.index, c)))
            .take(TAG_SLOTS)
            .collect()
    }

    pub(super) fn tick_tag(&mut self) {
        let (Some(tag), Some(race)) = (&self.tag, &self.ev.race) else {
            return;
        };
        if !tag.assigned {
            if self.now >= tag.assign_at {
                self.assign_tag();
            }
            return;
        }
        let started = race.stage == Stage::Started;
        let lone = race.participants.len() == 1;
        if !self.ensure_it() {
            let due = self
                .tag
                .as_ref()
                .is_some_and(|t| t.repeats.first().is_some_and(|r| self.now >= *r));
            if due {
                if let Some(t) = self.tag.as_mut() {
                    t.repeats.remove(0);
                }
                self.send_tag_buckets(false);
            }
        }
        if !started {
            return;
        }
        let cars = self.tag_cars();
        let Some(tag) = &self.tag else { return };
        // one car alone is everyone, once "GO" passed and its game had a moment to finish
        if tag.rules.mode == TagMode::Infected
            && !cars.is_empty()
            && cars.iter().all(|c| tag.is_it(c.1))
            && (cars.len() > 1
                || lone
                    && tag
                        .go_at
                        .is_some_and(|g| self.now >= g + TAG_ALONE_CLOSE_MS))
        {
            self.note("tag.everyone_infected", Vec::new());
            self.end_race("everyone infected");
        } else if tag
            .go_at
            .is_some_and(|g| self.now >= g + tag.limit_ms + TAG_LATE_CLOSE_MS)
        {
            self.note("tag.time_up", Vec::new());
            self.end_race("tag time up");
        }
    }

    fn assign_tag(&mut self) {
        let cars = self.tag_cars();
        if cars.is_empty() {
            return;
        }
        self.account_tag();
        let Some(tag) = self.tag.as_mut() else { return };
        tag.buckets.clear();
        let not_it = tag.not_it();
        for c in &cars {
            tag.buckets.insert(c.1, not_it);
        }
        // OD-153: Infected starts with one patient zero; King: max(1, ceil(players / per)) crowns
        let mut count = match (tag.rules.mode, tag.rules.players_per_it) {
            (TagMode::King, Some(per)) if per > 0 => {
                1.max((cars.len() as i32 + per - 1) / per) as usize
            }
            _ => 1,
        };
        count = count.min(cars.len());
        let mut pool: Vec<u8> = cars.iter().map(|c| c.1).collect();
        for _ in 0..count {
            let k = self.tag_random().next_below(pool.len() as i32) as usize;
            let pick = pool.remove(k);
            if let Some(tag) = self.tag.as_mut() {
                tag.buckets.insert(pick, tag.rules.it_bucket as u8);
            }
        }
        let Some(tag) = self.tag.as_mut() else { return };
        tag.assigned = true;
        let what = if tag.rules.mode == TagMode::Infected {
            "infected"
        } else {
            "crowned"
        };
        self.note(
            "tag.assigned",
            vec![json!(what), json!(count), json!(cars.len())],
        );
        self.send_tag_buckets(true);
    }

    /// With nobody It any more (the It car left), a new one is drawn. True when it was.
    fn ensure_it(&mut self) -> bool {
        let cars = self.tag_cars();
        let Some(tag) = &self.tag else { return false };
        if cars.is_empty() || cars.iter().any(|c| tag.is_it(c.1)) {
            return false;
        }
        let k = self.tag_random().next_below(cars.len() as i32) as usize;
        let pick = cars[k].1;
        self.account_tag();
        if let Some(tag) = self.tag.as_mut() {
            tag.buckets.insert(pick, tag.rules.it_bucket as u8);
        }
        self.note("tag.new_it", vec![json!(pick)]);
        self.send_tag_buckets(true);
        true
    }

    /// 37 to every participant; `repeat` schedules its repeats.
    fn send_tag_buckets(&mut self, repeat: bool) {
        let Some(race) = &self.ev.race else { return };
        let Some(tag) = &self.tag else { return };
        let cars = self.tag_cars();
        let receivers: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.synced && race.participants.contains(&p.index))
            .map(|p| p.serial)
            .collect();
        let mut sends = Vec::new();
        for r in receivers {
            let mut keys = Vec::with_capacity(TAG_SLOTS);
            let mut buckets = Vec::with_capacity(TAG_SLOTS);
            for i in 0..TAG_SLOTS {
                match cars.get(i) {
                    None => {
                        keys.push(Handle::relay(127, 0));
                        buckets.push(0);
                    }
                    Some(&(serial, index, car)) => {
                        keys.push(if serial == r {
                            Handle::own(car)
                        } else {
                            Handle::relay(car, index)
                        });
                        buckets.push(tag.buckets.get(&index).copied().unwrap_or(tag.not_it()));
                    }
                }
            }
            sends.push((
                r,
                Arc::new(wire::write(&Msg::TagBuckets {
                    cars: keys,
                    buckets,
                })),
            ));
        }
        for (r, m) in sends {
            self.send(r, vec![m], false, 0);
        }
        if repeat {
            let now = self.now;
            if let Some(tag) = self.tag.as_mut() {
                tag.repeats = TAG_REPEAT_MS.iter().map(|d| now + d).collect();
            }
        }
    }

    /// The peer index a key names, as the reporting game knows it.
    fn tag_car(&self, reporter: u8, key: Handle) -> Option<u8> {
        match key.kind {
            Kind::Own => Some(reporter),
            Kind::Relay => self
                .peers
                .iter()
                .find(|p| p.index == key.peer)
                .map(|p| p.index),
            _ => None,
        }
    }

    /// Whether two participants' cars may have touched: when both cars' last accepted
    /// predictions are fresh, they must be within `TAG_HIT_RADIUS_M`; without both positions the
    /// hit stands, as in the reference (the swap cooldown still bounds the rate).
    fn tag_cars_close(&self, x: u8, y: u8) -> bool {
        let now = self.now;
        let at = |index: u8| {
            self.peers
                .iter()
                .find(|p| p.index == index)
                .filter(|p| p.last_car_at.is_some_and(|t| now - t <= TAG_CAR_FRESH_MS))
                .and_then(|p| p.car_position)
        };
        match (at(x), at(y)) {
            (Some(a), Some(b)) => {
                let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
                (dx * dx + dy * dy + dz * dz).sqrt() <= TAG_HIT_RADIUS_M
            }
            _ => true,
        }
    }

    /// A game's hit report (36): applied once, when the buckets it saw are the server's and
    /// exactly one car is It.
    pub(super) fn tag_collision(&mut self, serial: u64, a: Handle, b: Handle, values: [u8; 4]) {
        let Some(index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        if !self.tag.as_ref().is_some_and(|t| t.assigned)
            || !self
                .ev
                .race
                .as_ref()
                .is_some_and(|r| r.participants.contains(&index))
        {
            return;
        }
        let (ca, cb) = (self.tag_car(index, a), self.tag_car(index, b));
        let Some(tag) = &self.tag else { return };
        let pair = match (ca, cb) {
            (Some(x), Some(y)) if x != y && (x == index || y == index) => {
                match (tag.buckets.get(&x), tag.buckets.get(&y)) {
                    (Some(bx), Some(by)) => Some((x, y, *bx, *by)),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((x, y, bx, by)) = pair else {
            self.note("tag.hit_ignored_outside", vec![json!(index)]);
            return;
        };
        // the other car's game reports the same hit too: once applied, its "before" no longer
        // matches (nor does a stale one)
        if bx != values[0] || by != values[2] {
            return;
        }
        if tag.is_it(x) == tag.is_it(y) {
            self.note("tag.hit_ignored_pair", vec![json!(index)]);
            return;
        }
        let now = self.now;
        let pair_key = (x.min(y), x.max(y));
        if tag
            .swapped_at
            .get(&pair_key)
            .is_some_and(|at| now - at < TAG_SWAP_COOLDOWN_MS)
        {
            if self.peer_note(serial, "tag.hit_ignored_soon") {
                self.note("tag.hit_ignored_soon", vec![json!(index)]);
            }
            return;
        }
        if !self.tag_cars_close(x, y) {
            if self.peer_note(serial, "tag.hit_ignored_far") {
                self.note("tag.hit_ignored_far", vec![json!(index)]);
            }
            return;
        }
        let (it, other) = if tag.is_it(x) { (x, y) } else { (y, x) };
        self.account_tag();
        let Some(tag) = self.tag.as_mut() else { return };
        tag.swapped_at.retain(|_, at| now - *at < TAG_SWAP_COOLDOWN_MS);
        tag.swapped_at.insert(pair_key, now);
        tag.buckets.insert(it, tag.rules.it_becomes as u8);
        tag.buckets.insert(other, tag.rules.not_it_becomes as u8);
        let scorer = if tag.rules.mode == TagMode::King {
            other
        } else {
            it
        };
        *tag.hits.entry(scorer).or_insert(0) += 1;
        let reported = (
            if it == x { values[1] } else { values[3] },
            if other == x { values[1] } else { values[3] },
        );
        let now_buckets = (tag.buckets[&it], tag.buckets[&other]);
        if reported == now_buckets {
            self.note("tag.tagged", vec![json!(it), json!(other)]);
        } else {
            self.note(
                "tag.tagged_reported",
                vec![
                    json!(it),
                    json!(other),
                    json!(reported.0),
                    json!(reported.1),
                ],
            );
        }
        self.send_tag_buckets(true);
    }

    /// A participant left the event: its car leaves the buckets, and a new It car is drawn when
    /// it was the last one.
    pub(super) fn tag_left(&mut self, index: u8) {
        if self.tag.is_none() {
            return;
        }
        self.account_tag();
        let Some(tag) = self.tag.as_mut() else { return };
        tag.buckets.remove(&index);
        if tag.assigned {
            self.ensure_it();
        }
    }

    /// A final score this early in a tag event is not its end (a lone Infected car's ends it
    /// from "GO" on; a lone King has no early end).
    pub(super) fn tag_too_early_to_finish(&self) -> bool {
        let Some(tag) = &self.tag else { return false };
        let Some(go) = tag.go_at else { return true };
        if self.now < go {
            return true;
        }
        let lone_infected = tag.rules.mode == TagMode::Infected
            && self
                .ev
                .race
                .as_ref()
                .is_some_and(|r| r.participants.len() == 1);
        !lone_infected && self.now - go < TAG_EARLIEST_FINISH_MS.min(tag.limit_ms / 2)
    }

    /// A tag event with its It cars drawn (its finish window is its time-up).
    pub(super) fn tag_assigned(&self) -> bool {
        self.tag.as_ref().is_some_and(|t| t.assigned)
    }

    /// A tag event runs.
    pub(super) fn tag_running(&self) -> bool {
        self.tag.is_some()
    }

    /// Adds the time since the last bucket change to every It car's time (from "GO" on).
    fn account_tag(&mut self) {
        let now = self.now;
        let Some(tag) = self.tag.as_mut() else { return };
        if let Some(go) = tag.go_at {
            let from = go.max(tag.marked);
            if now > from {
                let its: Vec<u8> = tag
                    .buckets
                    .iter()
                    .filter(|(_, b)| i32::from(**b) == tag.rules.it_bucket)
                    .map(|(k, _)| *k)
                    .collect();
                for i in its {
                    *tag.held_ms.entry(i).or_insert(0) += now - from;
                }
            }
        }
        tag.marked = now;
    }

    /// The event closed: its tag results (peer indexes only) go to the world log.
    pub(super) fn tag_ended(&mut self) {
        if !self.tag.as_ref().is_some_and(|t| t.assigned) {
            self.tag = None;
            return;
        }
        self.account_tag();
        let Some(tag) = self.tag.take() else { return };
        let winner = tag
            .held_ms
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(k, _)| *k);
        let survivors: Vec<String> = tag
            .buckets
            .iter()
            .filter(|(_, b)| i32::from(**b) != tag.rules.it_bucket)
            .map(|(k, _)| k.to_string())
            .collect();
        let hits: i32 = tag.hits.values().sum();
        if tag.rules.mode == TagMode::King {
            let longest = winner.map_or_else(
                || "nobody".to_string(),
                |w| {
                    let tenths = (tag.held_ms[&w] + 50) / 100;
                    format!("car {w} ({}.{} s)", tenths / 10, tenths % 10)
                },
            );
            self.note("tag.results_king", vec![json!(longest), json!(hits)]);
        } else {
            let left = if survivors.is_empty() {
                "none".to_string()
            } else {
                format!("cars {}", survivors.join(","))
            };
            self.note("tag.results_infected", vec![json!(hits), json!(left)]);
        }
    }
}
