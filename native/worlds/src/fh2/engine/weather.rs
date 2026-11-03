// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! A world's shared weather: the server-owned WeatherSynced (router 10, id 0; NERDCORE_WIRE.md
//! §7.5). The atmosphere moves between four levels (clear, mostly sunny, cloudy, rain), ramping one
//! whole unit in 165 s, then holds 90-180 s before the next target; it rains only above 0.603. Each
//! change carries current and desired values with their network-clock ticks, so every game
//! interpolates the same ramp. The choices come from a seed, so a world is reproducible.
//!
//! The arithmetic keeps the reference engine's precisions: the ramp and the intensities are single
//! precision, the rain threshold and the holds double precision.

use super::{bits::Bits, random::LegacyRandom};

const LEVELS: [f64; 4] = [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0];
const RAMP_MS_PER_UNIT: i64 = 165_000;
const RAIN_START: f64 = 0.603;
const HOLD_MIN_MS: i64 = 90_000;
const HOLD_MAX_MS: i64 = 180_000;
/// The odds of the next level (the current one excluded, the rest renormalised).
const NEXT: [(usize, f64); 3] = [(1, 0.45), (2, 0.35), (3, 0.20)];

#[derive(Debug, Clone)]
pub struct Weather {
    random: LegacyRandom,
    pub level: usize,
    pub atmosphere_current: f32,
    pub atmosphere_current_tick: u32,
    pub atmosphere_desired: f32,
    pub atmosphere_desired_tick: u32,
    pub rain_current: f32,
    pub rain_current_tick: u32,
    pub rain_desired: f32,
    pub rain_desired_tick: u32,
    pub presets: [u32; 4],
    pub next_change_at: i64,
}

fn at(from: f32, from_tick: u32, to: f32, to_tick: u32, tick: u32) -> f32 {
    let span = to_tick.wrapping_sub(from_tick);
    let done = tick.wrapping_sub(from_tick);
    if span == 0 || done >= span {
        to
    } else {
        from + (to - from) * (done as f32) / (span as f32)
    }
}

impl Weather {
    pub fn new(seed: i32, now: i64, tick: u32) -> Weather {
        let mut w = Weather {
            random: LegacyRandom::new(seed),
            level: 0,
            atmosphere_current: 0.0,
            atmosphere_current_tick: tick,
            atmosphere_desired: 0.0,
            atmosphere_desired_tick: tick,
            rain_current: 0.0,
            rain_current_tick: tick,
            rain_desired: 0.0,
            rain_desired_tick: tick,
            presets: [0; 4],
            next_change_at: 0,
        };
        w.level = w.pick(None);
        w.atmosphere_current = LEVELS[w.level] as f32;
        w.atmosphere_desired = w.atmosphere_current;
        w.rain_current = if w.level == 3 { 1.0 } else { 0.0 };
        w.rain_desired = w.rain_current;
        w.presets = w.presets_for();
        w.next_change_at = now + w.hold();
        w
    }

    fn pick(&mut self, current: Option<usize>) -> usize {
        let choices: Vec<(usize, f64)> = NEXT
            .iter()
            .copied()
            .filter(|(l, _)| Some(*l) != current)
            .collect();
        let total: f64 = choices.iter().map(|c| c.1).sum();
        let mut roll = self.random.next_double() * total;
        for (level, weight) in &choices {
            if roll < *weight {
                return *level;
            }
            roll -= weight;
        }
        choices.last().map_or(1, |c| c.0)
    }

    /// The preset per level: clear 0, mostly sunny 1, cloudy 3 or 5, rain 7.
    fn presets_for(&mut self) -> [u32; 4] {
        let cloudy = if self.random.next_below(2) == 0 { 3 } else { 5 };
        [0, 1, cloudy, 7]
    }

    fn hold(&mut self) -> i64 {
        HOLD_MIN_MS + (self.random.next_double() * (HOLD_MAX_MS - HOLD_MIN_MS) as f64) as i64
    }

    /// A new target when the hold is over: true when the weather changed.
    pub fn tick(&mut self, now: i64, tick: u32) -> bool {
        if now < self.next_change_at {
            return false;
        }
        let current = at(
            self.atmosphere_current,
            self.atmosphere_current_tick,
            self.atmosphere_desired,
            self.atmosphere_desired_tick,
            tick,
        );
        let rain = at(
            self.rain_current,
            self.rain_current_tick,
            self.rain_desired,
            self.rain_desired_tick,
            tick,
        );
        self.level = self.pick(Some(self.level));
        let target = LEVELS[self.level] as f32;
        let ramp_ms = ((target - current).abs() * RAMP_MS_PER_UNIT as f32) as i64;
        let atmosphere_end = tick.wrapping_add(ramp_ms as u32);
        let rain_target: f32 = if self.level == 3 { 1.0 } else { 0.0 };
        let (mut rain_start, mut rain_end) = (tick, tick);
        if rain_target != rain {
            if rain_target > rain {
                // rain begins once the atmosphere passes the threshold, full when it arrives
                let lead = ((RAIN_START - f64::from(current)) * RAMP_MS_PER_UNIT as f64).max(0.0);
                rain_start = tick.wrapping_add(lead as u32);
                rain_end = atmosphere_end;
            } else {
                let fall =
                    ((f64::from(current) - RAIN_START) * RAMP_MS_PER_UNIT as f64).max(1000.0);
                rain_end = tick.wrapping_add(fall as u32);
            }
        }
        self.atmosphere_current = current;
        self.atmosphere_current_tick = tick;
        self.atmosphere_desired = target;
        self.atmosphere_desired_tick = atmosphere_end;
        self.rain_current = rain;
        self.rain_current_tick = rain_start;
        self.rain_desired = rain_target;
        self.rain_desired_tick = rain_end;
        self.presets = self.presets_for();
        self.next_change_at = now + ramp_ms + self.hold();
        true
    }

    /// Every WeatherSynced variable: 0-3 atmosphere, 4 the four presets, 5-8 rain (4-bit ids).
    pub fn payload(&self) -> Bits {
        let mut b = Bits::new();
        let f = |b: &mut Bits, v: u64, x: f32| {
            b.push(v, 4).push(u64::from(x.to_bits()), 32);
        };
        let t = |b: &mut Bits, v: u64, x: u32| {
            b.push(v, 4).push(u64::from(x), 32);
        };
        f(&mut b, 0, self.atmosphere_current);
        t(&mut b, 1, self.atmosphere_current_tick);
        f(&mut b, 2, self.atmosphere_desired);
        t(&mut b, 3, self.atmosphere_desired_tick);
        b.push(4, 4);
        for p in self.presets {
            b.push(u64::from(p), 32);
        }
        f(&mut b, 5, self.rain_current);
        t(&mut b, 6, self.rain_current_tick);
        f(&mut b, 7, self.rain_desired);
        t(&mut b, 8, self.rain_desired_tick);
        b
    }
}
