//! The detector (MATCHMAKING_API.md §5d "The record", "Memory", "Detection"): per relayed grant the
//! two legs' cars are measured into per-second accumulators, merged into fixed 64-slot rings keyed
//! by (world, XUID, kind), and every second the final seconds are evaluated against the platform's
//! [`Rules`]. Everything is allocated once in [`Detector::new`]; nothing grows with traffic.
//!
//! - **Own leg** (joiner → host): the joiner's own car (an own handle) feeds the joiner's `own`
//!   ring; only a packet's last valid own car is measured (the one the host forwards). A car with
//!   any other handle there is counted (`foreign_owner`) and **taints** the world; so does a
//!   joiner's own PlayerData naming an XUID other than its grant's (`xuid_mismatch`, a §5d
//!   amendment: a joiner writing a relayed victim's or the host's XUID would otherwise have its own
//!   fast car bound to that XUID on the host's legs and frame the host). In a tainted world, for as
//!   long as one of its relayed grants lives on this relay, no forgery proof is made and the host's
//!   own car is not judged (review 2026-10-31: a time-limited void could be waited out, the owner
//!   byte binding it voids does not expire).
//! - **Forwarded leg** (host → joiner): owner bytes are bound to XUIDs by router-1 PlayerData on the
//!   same leg (one owner byte per XUID: a new claim moves it and voids the world, as above); the
//!   host's car feeds the host's `own` ring, a relayed joiner's car its `forwarded` ring, any other
//!   car is ignored.
//! - **Clocks:** each sample-to-sample speed is the lower of the game clock's
//!   ([`GAME_CLOCK_HZ`], V9-pending) and the relay's receive clock's; a teleport (more than twice
//!   the reported velocity plus 50 km/h) is a jump, never a speed.
//! - **Evaluation** on final seconds ([`FINAL_LAG_SECS`] after a second ends): violations on own
//!   rings, forgery proofs on forwarded rings ([`PROOF_MARGIN_SECS`] of clean own data on each side
//!   of the forwarded run), jumps (log only) on either.

use crate::nerd::{self, Car, Event, HandleKind, Walked};
use std::collections::HashMap;

/// A world id (UUID bytes).
pub type WorldId = [u8; 16];
/// A relay grant id (UUID bytes).
pub type GrantId = [u8; 16];
/// A report or episode id (UUID v4 bytes).
pub type Id = [u8; 16];

/// Slots per ring (`WITNESS_SECONDS`).
pub const WITNESS_SECONDS: u64 = 64;
/// The ring arena's hard cap.
pub const MAX_RINGS: usize = 3_000;
/// Rings per configured peer (own, forwarded, a host's own).
pub const RINGS_PER_PEER: usize = 3;
/// A kind is evaluated only with a sustain at most this long (64 slots minus the margins).
pub const MAX_SUSTAIN_MS: i64 = 48_000;
/// The game's prediction stamp unit, per second (V9-pending; the hook assumes milliseconds).
pub const GAME_CLOCK_HZ: f64 = 1_000.0;
/// A second is final this long after it ends.
pub const FINAL_LAG_SECS: u64 = 2;
/// Clean own seconds a forgery proof needs before and after the forwarded run.
pub const PROOF_MARGIN_SECS: u64 = 3;
/// A car silent this long reappears (its next sample is a first sample).
pub const REAPPEAR_MS: u64 = 2_000;
/// An open episode is reported again at most this often.
pub const REREPORT_SECS: u64 = 60;
/// Owner bytes bound per forwarded leg.
pub const OWNER_MAP: usize = 16;
/// Messages read from one packet at most (its count is a byte).
pub const MAX_MESSAGES: usize = 255;

/// Slot flag: the universe changed.
pub const UNIVERSE_CHANGED: u8 = 1 << 0;
/// Slot flag: a reset or teleport.
pub const TELEPORT: u8 = 1 << 1;
/// Slot flag: the first sample after (re)appearing.
pub const FIRST_SAMPLE: u8 = 1 << 2;
/// Slot flag: the game clock went backwards or stalled.
pub const CLOCK_BACK: u8 = 1 << 3;
/// Slot flag: a velocity axis at ±200.
pub const VELOCITY_SATURATED: u8 = 1 << 4;
/// Slot flag: the second's clock ratio outside [0.5, 2].
pub const CLOCK_RATIO: u8 = 1 << 5;
/// Slot flag: a foreign owner byte (or XUID mismatch) seen in this world (forwarded rings).
pub const FOREIGN_SEEN: u8 = 1 << 6;
/// Flags that make a second not covered.
pub const DISCONTINUITY: u8 = UNIVERSE_CHANGED | TELEPORT | FIRST_SAMPLE | CLOCK_BACK;

/// One limit of the rules.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limit {
    /// km/h.
    pub max_kmh: f64,
    /// Milliseconds over it.
    pub sustain_ms: i64,
}

/// `WatchRules` as the relay holds them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rules {
    /// The version.
    pub version: i64,
    /// Horizontal speed.
    pub speed: Limit,
    /// Upward speed.
    pub climb: Limit,
    /// One sample-to-sample move past this, metres (log only).
    pub jump_m: f64,
}

/// One second of one car, 24 bytes (§5d's table).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    /// Game-clock units covered by the second's sample deltas.
    pub span: u32,
    /// Highest horizontal speed, 0.1 km/h.
    pub h_max: u16,
    /// Lowest horizontal speed, 0.1 km/h.
    pub h_min: u16,
    /// Highest climb, 0.1 km/h.
    pub c_max: i16,
    /// Lowest climb, 0.1 km/h.
    pub c_min: i16,
    /// Highest reported horizontal velocity, 0.1 km/h.
    pub v_max: u16,
    /// Largest single move, 0.1 m.
    pub jump: u16,
    /// The second's last position, 2 m units.
    pub pos: [i16; 3],
    /// Samples (0 = no data).
    pub n: u8,
    /// Flags.
    pub flags: u8,
}

impl Slot {
    /// `n ≥ 2` and no discontinuity.
    pub fn covered(&self) -> bool {
        self.n >= 2 && self.flags & DISCONTINUITY == 0
    }

    /// The little-endian 24 bytes of §5d's table.
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..4].copy_from_slice(&self.span.to_le_bytes());
        b[4..6].copy_from_slice(&self.h_max.to_le_bytes());
        b[6..8].copy_from_slice(&self.h_min.to_le_bytes());
        b[8..10].copy_from_slice(&self.c_max.to_le_bytes());
        b[10..12].copy_from_slice(&self.c_min.to_le_bytes());
        b[12..14].copy_from_slice(&self.v_max.to_le_bytes());
        b[14..16].copy_from_slice(&self.jump.to_le_bytes());
        for (i, p) in self.pos.iter().enumerate() {
            b[16 + 2 * i..18 + 2 * i].copy_from_slice(&p.to_le_bytes());
        }
        b[22] = self.n;
        b[23] = self.flags;
        b
    }

    fn merge(&mut self, other: &Slot) {
        if self.n == 0 {
            *self = *other;
            return;
        }
        self.n = self.n.max(other.n);
        self.flags |= other.flags;
        self.span = self.span.max(other.span);
        self.h_max = self.h_max.max(other.h_max);
        self.h_min = self.h_min.max(other.h_min);
        self.c_max = self.c_max.max(other.c_max);
        self.c_min = self.c_min.max(other.c_min);
        self.v_max = self.v_max.max(other.v_max);
        self.jump = self.jump.max(other.jump);
        self.pos = other.pos;
    }
}

/// A ring's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RingKind {
    /// A relayed joiner's own car, or a host's own car.
    Own,
    /// What the host sent about a relayed joiner.
    Forwarded,
}

/// The leg a datagram crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    /// From the grant's joiner to its host.
    JoinerToHost,
    /// From the host to the joiner.
    HostToJoiner,
}

/// What the relay knows of a grant (its verified claims).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantInfo {
    /// The world.
    pub world: WorldId,
    /// The joiner's XUID.
    pub joiner: u64,
    /// The host's XUID.
    pub host: u64,
    /// When the grant expires (Unix seconds; extended by fresher hellos).
    pub exp: u64,
}

/// A report's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindingType {
    /// The measured player's own data broke the rules.
    Violation,
    /// The host forwarded speeds or climbs the player never sent.
    Forgery,
}

/// A report's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Horizontal speed.
    Speed,
    /// Upward speed.
    Climb,
    /// One sample-to-sample move (log only).
    Jump,
}

impl Kind {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Speed => "speed",
            Kind::Climb => "climb",
            Kind::Jump => "jump",
        }
    }
}

impl FindingType {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            FindingType::Violation => "violation",
            FindingType::Forgery => "forgery",
        }
    }
}

/// A report the detector produced (`RelayReport` without the batch).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Finding {
    /// The idempotency key.
    pub id: Id,
    /// The episode (repeats share it).
    pub episode: Id,
    /// Violation or forgery.
    pub kind_type: FindingType,
    /// Speed, climb or jump.
    pub kind: Kind,
    /// The world.
    pub world: WorldId,
    /// Violation: the measured player; forgery: the player whose data was forged.
    pub xuid: u64,
    /// The grant's host.
    pub host_xuid: u64,
    /// The episode's first second, Unix ms.
    pub from_ms: u64,
    /// Its last second, Unix ms.
    pub to_ms: u64,
    /// The rules version evaluated against.
    pub rules_version: i64,
    /// Violation: own peak; forgery: forwarded peak; jump: 0 (km/h, 0.1).
    pub peak_kmh: f64,
    /// The run's length × 1000.
    pub over_limit_ms: u64,
    /// Forgery: own maximum over the proof window.
    pub own_peak_kmh: Option<f64>,
    /// Forgery: host → joiner legs carrying the subject's car.
    pub legs: Option<u32>,
    /// Jump: the largest move, metres.
    pub max_jump_m: Option<f64>,
}

/// Monotonic counters since start (the relay sends differences).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Cars decoded.
    pub decoded: u64,
    /// NerdCore packets whose read stopped, and cars with an impossible position.
    pub undecodable: u64,
    /// Cars with a foreign handle on a joiner's own leg.
    pub foreign_owner: u64,
    /// A joiner's own PlayerData naming another XUID, or an XUID claimed by a second owner byte.
    pub xuid_mismatch: u64,
    /// Subjects (or grants) without a ring (the arena was full).
    pub witness_full: u64,
    /// Seconds evaluated without rules.
    pub rules_missing: u64,
    /// Kind-seconds not evaluated (a sustain past [`MAX_SUSTAIN_MS`]).
    pub rule_unevaluable: u64,
    /// Rings in use now (a gauge).
    pub rings_in_use: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Run {
    start: u64,
    len: u64,
    peak: u16,
    episode: Option<(Id, u64)>,
    last_report: u64,
}

#[derive(Clone, Debug)]
struct Ring {
    used: bool,
    world: WorldId,
    xuid: u64,
    host: u64,
    kind: RingKind,
    live: bool,
    last_second: u64,
    evaluated: u64,
    runs: [Run; 2],
    jump_last: Option<u64>,
    slots: [Slot; WITNESS_SECONDS as usize],
}

impl Ring {
    fn empty() -> Self {
        Self {
            used: false,
            world: [0; 16],
            xuid: 0,
            host: 0,
            kind: RingKind::Own,
            live: false,
            last_second: 0,
            evaluated: 0,
            runs: [Run::default(); 2],
            jump_last: None,
            slots: [Slot::default(); WITNESS_SECONDS as usize],
        }
    }

    fn slot(&self, second: u64) -> Slot {
        if second > self.last_second || self.last_second - second >= WITNESS_SECONDS {
            return Slot::default();
        }
        self.slots[(second % WITNESS_SECONDS) as usize]
    }

    /// Merges a second into its slot, advancing (and zeroing skipped slots) for a newer second.
    fn write(&mut self, second: u64, acc: &Slot) {
        if second > self.last_second {
            let from = (self.last_second + 1).max(second.saturating_sub(WITNESS_SECONDS - 1));
            for s in from..=second {
                self.slots[(s % WITNESS_SECONDS) as usize] = Slot::default();
            }
            self.last_second = second;
        } else if self.last_second - second >= WITNESS_SECONDS {
            return;
        }
        self.slots[(second % WITNESS_SECONDS) as usize].merge(acc);
    }
}

#[derive(Clone, Copy, Debug)]
struct Last {
    pos: [f32; 3],
    stamp: u32,
    recv_ms: u64,
    universe: u8,
}

/// One car source on one leg: the per-second accumulator and the last sample.
#[derive(Clone, Copy, Debug)]
struct Track {
    subject: Option<(u64, RingKind)>,
    car: u32,
    last: Option<Last>,
    second: u64,
    acc: Slot,
    speeds: u16,
    h_min: u16,
    c_min: i16,
    c_max: i16,
}

impl Track {
    const fn new() -> Self {
        Self {
            subject: None,
            car: u32::MAX,
            last: None,
            second: 0,
            acc: Slot {
                span: 0,
                h_max: 0,
                h_min: 0,
                c_max: 0,
                c_min: 0,
                v_max: 0,
                jump: 0,
                pos: [0; 3],
                n: 0,
                flags: 0,
            },
            speeds: 0,
            h_min: u16::MAX,
            c_min: i16::MAX,
            c_max: i16::MIN,
        }
    }

    fn reset_acc(&mut self, second: u64) {
        let subject = self.subject;
        let (car, last) = (self.car, self.last);
        *self = Track::new();
        self.subject = subject;
        self.car = car;
        self.last = last;
        self.second = second;
    }

    /// The accumulator as a slot (minima resolved; the clock ratio checked).
    fn finished(&self) -> Slot {
        let mut s = self.acc;
        if self.speeds > 0 {
            s.h_min = self.h_min;
            s.c_min = self.c_min;
            s.c_max = self.c_max;
        }
        if s.n >= 2 {
            let ratio = f64::from(s.span) / GAME_CLOCK_HZ;
            if !(0.5..=2.0).contains(&ratio) {
                s.flags |= CLOCK_RATIO;
            }
        }
        s
    }
}

#[derive(Clone, Copy, Debug)]
struct Owner {
    byte: u8,
    xuid: u64,
}

#[derive(Clone, Debug)]
struct GrantState {
    used: bool,
    id: GrantId,
    info: GrantInfo,
    own: Track,
    owners: [Option<Owner>; OWNER_MAP],
    fwd: [(u8, Track); OWNER_MAP],
    fwd_len: usize,
}

impl GrantState {
    fn empty() -> Self {
        Self {
            used: false,
            id: [0; 16],
            info: GrantInfo {
                world: [0; 16],
                joiner: 0,
                host: 0,
                exp: 0,
            },
            own: Track::new(),
            owners: [None; OWNER_MAP],
            fwd: [(0, Track::new()); OWNER_MAP],
            fwd_len: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Void {
    world: WorldId,
    last: u64,
    bits: u128,
    used: bool,
}

impl Void {
    fn mark(&mut self, second: u64) {
        if second > self.last {
            let shift = second - self.last;
            self.bits = if shift >= 128 { 0 } else { self.bits << shift };
            self.last = second;
        }
        let back = self.last - second;
        if back < 128 {
            self.bits |= 1 << back;
        }
    }

    fn any(&self, lo: u64, hi: u64) -> bool {
        (lo..=hi.min(self.last)).any(|s| {
            let back = self.last - s;
            back < 128 && self.bits & (1 << back) != 0
        })
    }
}

/// The detector's fixed sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// `FI_RELAY_MAX_PEERS`: relayed grants at once.
    pub max_peers: usize,
}

impl Config {
    /// `WITNESS_RINGS` = min(3 × peers, 3,000).
    pub fn rings(&self) -> usize {
        (RINGS_PER_PEER * self.max_peers.max(1)).min(MAX_RINGS)
    }
}

/// The detector: rings, grants, owner maps and voids, all allocated in [`Detector::new`].
#[derive(Debug)]
pub struct Detector {
    rings: Vec<Ring>,
    free: Vec<u16>,
    index: HashMap<(WorldId, u64, RingKind), u16>,
    grants: Vec<GrantState>,
    grant_index: HashMap<GrantId, u16>,
    free_grants: Vec<u16>,
    voids: Vec<Void>,
    scratch: Vec<Event>,
    rules: Option<Rules>,
    counters: Counters,
    rng: u64,
}

fn coarse(v: f32) -> i16 {
    (f64::from(v) / 2.0).round().clamp(-32768.0, 32767.0) as i16
}

fn tenth_u16(v: f64) -> u16 {
    (v * 10.0).round().clamp(0.0, 65535.0) as u16
}

fn tenth_i16(v: f64) -> i16 {
    (v * 10.0).round().clamp(-32768.0, 32767.0) as i16
}

impl Detector {
    /// A detector sized for `config`, ids drawn from `seed` (a fresh random value per start).
    pub fn new(config: Config, seed: u64) -> Self {
        let rings = config.rings();
        let peers = config.max_peers.max(1);
        Self {
            rings: vec![Ring::empty(); rings],
            free: (0..rings as u16).rev().collect(),
            index: HashMap::with_capacity(rings),
            grants: vec![GrantState::empty(); peers],
            grant_index: HashMap::with_capacity(peers),
            free_grants: (0..peers as u16).rev().collect(),
            voids: vec![Void::default(); 2 * peers],
            scratch: Vec::with_capacity(MAX_MESSAGES),
            rules: None,
            counters: Counters::default(),
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// The rules held.
    pub fn rules(&self) -> Option<Rules> {
        self.rules
    }

    /// Sets the rules; a new version restarts every open run (episodes end).
    pub fn set_rules(&mut self, rules: Option<Rules>) {
        if rules.map(|r| r.version) != self.rules.map(|r| r.version) {
            for r in &mut self.rings {
                r.runs = [Run::default(); 2];
            }
        }
        self.rules = rules;
    }

    /// The counters (the ring gauge refreshed).
    pub fn counters(&self) -> Counters {
        let mut c = self.counters;
        c.rings_in_use = (self.rings.len() - self.free.len()) as u64;
        c
    }

    /// Rings allocated (the arena's size).
    pub fn capacity(&self) -> usize {
        self.rings.len()
    }

    /// Bytes the detector holds, all allocated in [`Detector::new`] (rings, grants with their owner
    /// maps and tracks, voids, and the two indexes at their reserved capacity, entries and control
    /// bytes counted).
    pub fn footprint(&self) -> usize {
        use std::mem::size_of;
        let map = |cap: usize, entry: usize| cap * (entry + 1);
        size_of::<Self>()
            + self.rings.capacity() * size_of::<Ring>()
            + self.free.capacity() * size_of::<u16>()
            + map(
                self.index.capacity(),
                size_of::<((WorldId, u64, RingKind), u16)>(),
            )
            + self.grants.capacity() * size_of::<GrantState>()
            + map(self.grant_index.capacity(), size_of::<(GrantId, u16)>())
            + self.free_grants.capacity() * size_of::<u16>()
            + self.voids.capacity() * size_of::<Void>()
            + self.scratch.capacity() * size_of::<Event>()
    }

    fn next_id(&mut self) -> Id {
        let mut out = [0u8; 16];
        for half in out.chunks_mut(8) {
            // splitmix64
            self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.rng;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            half.copy_from_slice(&z.to_le_bytes());
        }
        out[6] = (out[6] & 0x0f) | 0x40;
        out[8] = (out[8] & 0x3f) | 0x80;
        out
    }

    fn void(&mut self, world: WorldId, second: u64) {
        let slot = self
            .voids
            .iter()
            .position(|v| v.used && v.world == world)
            .or_else(|| self.voids.iter().position(|v| !v.used))
            .unwrap_or_else(|| {
                // A world no live grant feeds makes room first, else the oldest.
                let live = |w: &WorldId| self.grants.iter().any(|g| g.used && &g.info.world == w);
                self.voids
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, v)| (live(&v.world), v.last))
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        let v = &mut self.voids[slot];
        if !v.used || v.world != world {
            *v = Void {
                world,
                last: second,
                bits: 0,
                used: true,
            };
        }
        v.mark(second);
    }

    /// Whether a foreign owner byte or an XUID claim was seen in `world`: from then on, for as long
    /// as a relayed grant of that world lives here, no forgery proof is made in it and the host's own
    /// car is not judged (a bound owner byte outlives any time window, so a time-limited void could
    /// be waited out by a joiner who once claimed someone else's XUID).
    fn tainted(&self, world: &WorldId) -> bool {
        self.voids.iter().any(|v| v.used && &v.world == world)
    }

    fn voided_at(&self, world: &WorldId, second: u64) -> bool {
        self.voids
            .iter()
            .any(|v| v.used && &v.world == world && v.any(second, second))
    }

    fn grant_slot(&mut self, id: &GrantId, info: GrantInfo) -> Option<usize> {
        if let Some(&i) = self.grant_index.get(id) {
            let g = &mut self.grants[usize::from(i)];
            g.info.exp = g.info.exp.max(info.exp);
            return Some(usize::from(i));
        }
        let Some(i) = self.free_grants.pop() else {
            self.counters.witness_full += 1;
            return None;
        };
        let g = &mut self.grants[usize::from(i)];
        *g = GrantState::empty();
        g.used = true;
        g.id = *id;
        g.info = info;
        self.grant_index.insert(*id, i);
        Some(usize::from(i))
    }

    /// The ring of a subject, allocating it when `create` (a forwarded ring only beside an own
    /// ring of the same player; a newcomer takes a free ring, else an ended one's oldest).
    fn ring(
        &mut self,
        world: WorldId,
        xuid: u64,
        host: u64,
        kind: RingKind,
        second: u64,
    ) -> Option<u16> {
        if let Some(&i) = self.index.get(&(world, xuid, kind)) {
            return Some(i);
        }
        if kind == RingKind::Forwarded && !self.index.contains_key(&(world, xuid, RingKind::Own)) {
            return None;
        }
        let i = match self.free.pop() {
            Some(i) => i,
            None => {
                // The ended ring with the oldest last write (never one with live data), and its
                // forwarded sibling with it.
                let victim = self
                    .rings
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.used && !r.live && r.kind == RingKind::Own)
                    .min_by_key(|(_, r)| r.last_second)
                    .map(|(i, _)| i as u16);
                match victim {
                    Some(v) => {
                        self.release(v);
                        self.free.pop()?
                    }
                    None => {
                        self.counters.witness_full += 1;
                        return None;
                    }
                }
            }
        };
        let r = &mut self.rings[usize::from(i)];
        *r = Ring::empty();
        r.used = true;
        r.world = world;
        r.xuid = xuid;
        r.host = host;
        r.kind = kind;
        r.live = true;
        r.last_second = second;
        r.evaluated = second.saturating_sub(1);
        self.index.insert((world, xuid, kind), i);
        Some(i)
    }

    /// Frees a ring (an own ring frees its forwarded sibling too).
    fn release(&mut self, i: u16) {
        let r = &mut self.rings[usize::from(i)];
        if !r.used {
            return;
        }
        r.used = false;
        let key = (r.world, r.xuid, r.kind);
        self.index.remove(&key);
        self.free.push(i);
        if key.2 == RingKind::Own
            && let Some(&f) = self.index.get(&(key.0, key.1, RingKind::Forwarded))
        {
            self.release(f);
        }
    }

    fn flush_track(&mut self, world: WorldId, host: u64, track: &mut Track) {
        if track.acc.n == 0 {
            return;
        }
        let Some((xuid, kind)) = track.subject else {
            return;
        };
        let second = track.second;
        let mut slot = track.finished();
        if kind == RingKind::Forwarded && self.voided_at(&world, second) {
            slot.flags |= FOREIGN_SEEN;
        }
        if let Some(i) = self.ring(world, xuid, host, kind, second) {
            self.rings[usize::from(i)].write(second, &slot);
        }
        track.reset_acc(second);
    }

    /// One sample of `car` into `track` (subject already set).
    fn sample(&mut self, world: WorldId, host: u64, track: &mut Track, car: &Car, now_ms: u64) {
        let second = now_ms / 1000;
        if track.acc.n > 0 && track.second != second {
            self.flush_track(world, host, track);
        }
        if track.acc.n == 0 {
            track.reset_acc(second);
        }
        if car.handle.id != track.car {
            track.car = car.handle.id;
            track.last = None;
        }
        let mut flags = 0u8;
        let [vx, _, vz] = car.velocity.map(f64::from);
        let vnorm = (vx * vx + vz * vz).sqrt() * 3.6;
        if car.velocity_saturated {
            flags |= VELOCITY_SATURATED;
        }
        match track.last {
            None => flags |= FIRST_SAMPLE,
            Some(l) if now_ms.saturating_sub(l.recv_ms) > REAPPEAR_MS => flags |= FIRST_SAMPLE,
            Some(l) => {
                if l.universe != car.universe {
                    flags |= UNIVERSE_CHANGED;
                }
                let dg = car.stamp.wrapping_sub(l.stamp) as i32;
                let [dx, dy, dz] =
                    [0, 1, 2].map(|i| f64::from(car.position[i]) - f64::from(l.pos[i]));
                let h = (dx * dx + dz * dz).sqrt();
                let moved = (h * h + dy * dy).sqrt();
                track.acc.jump = track.acc.jump.max(tenth_u16(moved));
                if dg <= 0 {
                    flags |= CLOCK_BACK;
                } else {
                    let dt_game = f64::from(dg) / GAME_CLOCK_HZ;
                    let dt_recv = now_ms.saturating_sub(l.recv_ms) as f64 / 1000.0;
                    let implied = h / dt_game * 3.6;
                    if implied > 2.0 * vnorm + 50.0 {
                        flags |= TELEPORT;
                    } else {
                        let on_recv = |d: f64| (dt_recv > 0.0).then(|| d / dt_recv * 3.6);
                        let speed = on_recv(h).map_or(implied, |r| implied.min(r));
                        let cg = dy / dt_game * 3.6;
                        // The climb nearer zero of the two clocks (0 when they disagree in sign).
                        let climb = match on_recv(dy) {
                            None => cg,
                            Some(cr) if cg > 0.0 && cr > 0.0 => cg.min(cr),
                            Some(cr) if cg < 0.0 && cr < 0.0 => cg.max(cr),
                            Some(_) => 0.0,
                        };
                        let (s10, c10) = (tenth_u16(speed), tenth_i16(climb));
                        track.acc.h_max = track.acc.h_max.max(s10);
                        track.h_min = track.h_min.min(s10);
                        track.c_max = track.c_max.max(c10);
                        track.c_min = track.c_min.min(c10);
                        track.speeds = track.speeds.saturating_add(1);
                        track.acc.span = track.acc.span.saturating_add(dg as u32);
                    }
                }
            }
        }
        track.acc.v_max = track.acc.v_max.max(tenth_u16(vnorm));
        track.acc.n = track.acc.n.saturating_add(1);
        track.acc.flags |= flags;
        track.acc.pos = car.position.map(coarse);
        track.last = Some(Last {
            pos: car.position,
            stamp: car.stamp,
            recv_ms: now_ms,
            universe: car.universe,
        });
    }

    /// One forwarded datagram's payload on `leg` of grant `id` (whatever it holds: a payload that
    /// is not a NerdCore packet is ignored, a read that stops is counted).
    pub fn observe(
        &mut self,
        id: &GrantId,
        info: GrantInfo,
        leg: Leg,
        payload: &[u8],
        now_ms: u64,
    ) {
        // Every message is read (at most 255 a packet: the scratch buffer never grows past it).
        let mut events = std::mem::take(&mut self.scratch);
        events.clear();
        let walked = nerd::walk(payload, |e| {
            if events.len() < MAX_MESSAGES {
                events.push(e);
            }
        });
        match walked {
            Walked::NotNerd => {
                self.scratch = events;
                return;
            }
            Walked::Stopped => self.counters.undecodable += 1,
            Walked::Complete => {}
        }
        if events.is_empty() {
            self.scratch = events;
            return;
        }
        let Some(gi) = self.grant_slot(id, info) else {
            self.scratch = events;
            return;
        };
        let now_s = now_ms / 1000;
        let info = self.grants[gi].info;
        // The host forwards only a packet's last valid own car (`RelayEngine.ReceivePacket`'s
        // `carNow`): that one is measured, the others are not.
        let mut last_own: Option<Car> = None;
        for e in events.iter() {
            match (leg, *e) {
                (_, Event::BadCar(_)) => self.counters.undecodable += 1,
                (Leg::JoinerToHost, Event::Car(car)) => {
                    if car.handle.kind != HandleKind::Own {
                        self.counters.foreign_owner += 1;
                        self.void(info.world, now_s);
                        continue;
                    }
                    self.counters.decoded += 1;
                    last_own = Some(car);
                }
                (
                    Leg::JoinerToHost,
                    Event::Player {
                        handle,
                        xuid: Some(x),
                    },
                ) => {
                    if handle.kind == HandleKind::Own && x != info.joiner {
                        self.counters.xuid_mismatch += 1;
                        self.void(info.world, now_s);
                    }
                }
                (Leg::JoinerToHost, Event::Player { .. }) => {}
                (
                    Leg::HostToJoiner,
                    Event::Player {
                        handle,
                        xuid: Some(x),
                    },
                ) => {
                    if handle.kind == HandleKind::Relay {
                        self.bind(gi, handle.owner, x, now_s);
                    }
                }
                (Leg::HostToJoiner, Event::Player { .. }) => {}
                (Leg::HostToJoiner, Event::Car(car)) => {
                    if car.handle.kind != HandleKind::Relay {
                        continue;
                    }
                    self.counters.decoded += 1;
                    let owner = car.handle.owner;
                    let Some(xuid) = self.grants[gi]
                        .owners
                        .iter()
                        .flatten()
                        .find(|o| o.byte == owner)
                        .map(|o| o.xuid)
                    else {
                        continue;
                    };
                    let subject = if xuid == info.host {
                        Some((xuid, RingKind::Own))
                    } else if self.index.contains_key(&(info.world, xuid, RingKind::Own)) {
                        Some((xuid, RingKind::Forwarded))
                    } else {
                        None
                    };
                    let Some(subject) = subject else { continue };
                    let g = &mut self.grants[gi];
                    let slot = match g.fwd[..g.fwd_len].iter().position(|(b, _)| *b == owner) {
                        Some(s) => s,
                        None if g.fwd_len < OWNER_MAP => {
                            g.fwd[g.fwd_len] = (owner, Track::new());
                            g.fwd_len += 1;
                            g.fwd_len - 1
                        }
                        None => continue,
                    };
                    let mut t = g.fwd[slot].1;
                    if t.subject != Some(subject) {
                        self.flush_track(info.world, info.host, &mut t);
                        t = Track::new();
                        t.subject = Some(subject);
                    }
                    self.sample(info.world, info.host, &mut t, &car, now_ms);
                    self.grants[gi].fwd[slot].1 = t;
                }
            }
        }
        self.scratch = events;
        if let Some(car) = last_own {
            let mut t = self.grants[gi].own;
            t.subject = Some((info.joiner, RingKind::Own));
            self.sample(info.world, info.host, &mut t, &car, now_ms);
            self.grants[gi].own = t;
        }
    }

    /// Binds `byte` to `xuid` on a forwarded leg: one owner byte per XUID; a claim that moves
    /// voids the world's forgery proofs around now.
    fn bind(&mut self, gi: usize, byte: u8, xuid: u64, now_s: u64) {
        let world = self.grants[gi].info.world;
        let g = &mut self.grants[gi];
        let mut conflict = false;
        for o in g.owners.iter_mut() {
            if let Some(e) = o
                && e.xuid == xuid
                && e.byte != byte
            {
                *o = None;
                conflict = true;
            }
        }
        match g
            .owners
            .iter_mut()
            .find(|o| o.is_some_and(|e| e.byte == byte))
        {
            Some(o) => {
                if o.is_some_and(|e| e.xuid != xuid) {
                    // A new player on a reused peer index: its track restarts.
                    if let Some(t) = g.fwd[..g.fwd_len].iter_mut().find(|(b, _)| *b == byte) {
                        t.1 = Track::new();
                    }
                }
                *o = Some(Owner { byte, xuid });
            }
            None => {
                if let Some(o) = g.owners.iter_mut().find(|o| o.is_none()) {
                    *o = Some(Owner { byte, xuid });
                }
            }
        }
        if conflict {
            self.counters.xuid_mismatch += 1;
            self.void(world, now_s);
        }
    }

    /// The once-a-second step: ended grants released, accumulators flushed, rings freed, final
    /// seconds evaluated. Findings are appended to `out`.
    pub fn tick(&mut self, now_ms: u64, out: &mut Vec<Finding>) {
        let now_s = now_ms / 1000;
        // Grants: flush what is complete; release the ended ones.
        for gi in 0..self.grants.len() {
            if !self.grants[gi].used {
                continue;
            }
            let info = self.grants[gi].info;
            let ended = info.exp <= now_s;
            let mut own = self.grants[gi].own;
            if ended || own.second < now_s {
                self.flush_track(info.world, info.host, &mut own);
            }
            self.grants[gi].own = own;
            for k in 0..self.grants[gi].fwd_len {
                let mut t = self.grants[gi].fwd[k].1;
                if ended || t.second < now_s {
                    self.flush_track(info.world, info.host, &mut t);
                }
                self.grants[gi].fwd[k].1 = t;
            }
            if ended {
                let id = self.grants[gi].id;
                self.grants[gi].used = false;
                self.grant_index.remove(&id);
                self.free_grants.push(gi as u16);
            }
        }
        // Liveness: a ring is live while a live grant feeds it.
        for r in &mut self.rings {
            r.live = false;
        }
        for gi in 0..self.grants.len() {
            if !self.grants[gi].used {
                continue;
            }
            let info = self.grants[gi].info;
            for key in [
                (info.world, info.joiner, RingKind::Own),
                (info.world, info.joiner, RingKind::Forwarded),
                (info.world, info.host, RingKind::Own),
            ] {
                if let Some(&i) = self.index.get(&key) {
                    self.rings[usize::from(i)].live = true;
                }
            }
            for k in 0..self.grants[gi].fwd_len {
                if let Some(subject) = self.grants[gi].fwd[k].1.subject
                    && let Some(&i) = self.index.get(&(info.world, subject.0, subject.1))
                {
                    self.rings[usize::from(i)].live = true;
                }
            }
        }
        // A world's taint ends once no relayed grant of it lives here (and its marks are old).
        for v in 0..self.voids.len() {
            let w = self.voids[v].world;
            if self.voids[v].used
                && now_s.saturating_sub(self.voids[v].last) >= 128
                && !self.grants.iter().any(|g| g.used && g.info.world == w)
            {
                self.voids[v].used = false;
            }
        }
        // Free: ended and silent for the whole window.
        for i in 0..self.rings.len() {
            let r = &self.rings[i];
            if r.used
                && !r.live
                && r.kind == RingKind::Own
                && now_s.saturating_sub(r.last_second) >= WITNESS_SECONDS
            {
                self.release(i as u16);
            }
        }
        // Evaluate the final seconds.
        let Some(final_s) = now_s.checked_sub(FINAL_LAG_SECS + 1) else {
            return;
        };
        let Some(rules) = self.rules else {
            if self.free.len() < self.rings.len() {
                self.counters.rules_missing += 1;
            }
            for r in self.rings.iter_mut().filter(|r| r.used) {
                r.evaluated = r.evaluated.max(final_s);
            }
            return;
        };
        for limit in [rules.speed, rules.climb] {
            if limit.sustain_ms > MAX_SUSTAIN_MS && self.free.len() < self.rings.len() {
                self.counters.rule_unevaluable += 1;
            }
        }
        for i in 0..self.rings.len() {
            if !self.rings[i].used {
                continue;
            }
            let from =
                (self.rings[i].evaluated + 1).max(final_s.saturating_sub(WITNESS_SECONDS - 1));
            for s in from..=final_s {
                self.evaluate(i, s, &rules, out);
            }
            if self.rings[i].used {
                self.rings[i].evaluated = self.rings[i].evaluated.max(final_s);
            }
        }
    }

    fn need(limit: &Limit) -> Option<u64> {
        if limit.sustain_ms > MAX_SUSTAIN_MS || limit.sustain_ms < 0 {
            return None;
        }
        Some(((limit.sustain_ms as u64).div_ceil(1000)).max(1))
    }

    fn evaluate(&mut self, i: usize, s: u64, rules: &Rules, out: &mut Vec<Finding>) {
        let kind = self.rings[i].kind;
        for (k, limit) in [(0usize, rules.speed), (1usize, rules.climb)] {
            let Some(need) = Self::need(&limit) else {
                continue;
            };
            let limit10 = (limit.max_kmh * 10.0).round();
            match kind {
                RingKind::Own => self.violation(i, k, s, need, limit10, rules, out),
                RingKind::Forwarded => self.forgery(i, k, s, need, limit10, rules, out),
            }
        }
        self.jump(i, s, rules, out);
    }

    fn over(slot: &Slot, k: usize, limit10: f64) -> bool {
        slot.covered()
            && if k == 0 {
                f64::from(slot.h_min) > limit10
            } else {
                f64::from(slot.c_min) > limit10
            }
    }

    fn peak(slot: &Slot, k: usize) -> u16 {
        if k == 0 {
            slot.h_max
        } else {
            slot.c_max.max(0) as u16
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn violation(
        &mut self,
        i: usize,
        k: usize,
        s: u64,
        need: u64,
        limit10: f64,
        rules: &Rules,
        out: &mut Vec<Finding>,
    ) {
        let slot = self.rings[i].slot(s);
        // The host's own ring is fed by owner bytes bound on host legs: in a tainted world one of
        // them may be a joiner's car under the host's XUID, so the host is not judged there.
        let host_ring = self.rings[i].xuid == self.rings[i].host;
        let tainted = host_ring && self.tainted(&self.rings[i].world);
        let run = &mut self.rings[i].runs[k];
        if tainted || !Self::over(&slot, k, limit10) {
            *run = Run::default();
            return;
        }
        if run.len == 0 {
            run.start = s;
            run.peak = 0;
        }
        run.len += 1;
        run.peak = run.peak.max(Self::peak(&slot, k));
        if run.len < need {
            return;
        }
        let report = match run.episode {
            None => true,
            Some(_) => s - run.last_report >= REREPORT_SECS,
        };
        if !report {
            return;
        }
        let run_now = *run;
        let episode = match run_now.episode {
            Some(e) => e,
            None => (self.next_id(), run_now.start),
        };
        let r = &mut self.rings[i];
        r.runs[k].episode = Some(episode);
        r.runs[k].last_report = s;
        let (world, xuid, host) = (r.world, r.xuid, r.host);
        let id = self.next_id();
        out.push(Finding {
            id,
            episode: episode.0,
            kind_type: FindingType::Violation,
            kind: if k == 0 { Kind::Speed } else { Kind::Climb },
            world,
            xuid,
            host_xuid: host,
            from_ms: episode.1 * 1000,
            to_ms: s * 1000,
            rules_version: rules.version,
            peak_kmh: f64::from(run_now.peak) / 10.0,
            over_limit_ms: run_now.len * 1000,
            own_peak_kmh: None,
            legs: None,
            max_jump_m: None,
        });
    }

    /// Forgery on forwarded ring `i` at final second `s`: the forwarded run is read
    /// [`PROOF_MARGIN_SECS`] behind, so the own seconds after it are final too.
    #[allow(clippy::too_many_arguments)]
    fn forgery(
        &mut self,
        i: usize,
        k: usize,
        s: u64,
        need: u64,
        limit10: f64,
        rules: &Rules,
        out: &mut Vec<Finding>,
    ) {
        let Some(e) = s.checked_sub(PROOF_MARGIN_SECS) else {
            return;
        };
        let (world, xuid, host) = (self.rings[i].world, self.rings[i].xuid, self.rings[i].host);
        let fslot = self.rings[i].slot(e);
        let run = &mut self.rings[i].runs[k];
        if !Self::over(&fslot, k, limit10) {
            *run = Run::default();
            return;
        }
        if run.len == 0 {
            run.start = e;
            run.peak = 0;
        }
        run.len += 1;
        run.peak = run.peak.max(Self::peak(&fslot, k));
        if run.len < need {
            return;
        }
        let run_now = *run;
        if run_now.episode.is_some() && s - run_now.last_report < REREPORT_SECS {
            return;
        }
        // The proof window: the latest `need` seconds of the run, PROOF_MARGIN_SECS on each side.
        let lo = e + 1 - need - PROOF_MARGIN_SECS;
        let Some(&own) = self.index.get(&(world, xuid, RingKind::Own)) else {
            return;
        };
        let own = &self.rings[usize::from(own)];
        let mut own_peak = 0u16;
        for t in lo..=s {
            let o = own.slot(t);
            if !o.covered() {
                return;
            }
            let p = Self::peak(&o, k);
            if f64::from(p) > limit10 {
                return;
            }
            own_peak = own_peak.max(p);
        }
        if self.tainted(&world) {
            return;
        }
        let legs = self
            .grants
            .iter()
            .filter(|g| g.used && g.info.world == world)
            .filter(|g| {
                g.fwd[..g.fwd_len]
                    .iter()
                    .any(|(_, t)| t.subject == Some((xuid, RingKind::Forwarded)))
            })
            .count()
            .max(1) as u32;
        let episode = match run_now.episode {
            Some(ep) => ep,
            None => (self.next_id(), lo + PROOF_MARGIN_SECS),
        };
        let r = &mut self.rings[i];
        r.runs[k].episode = Some(episode);
        r.runs[k].last_report = s;
        let id = self.next_id();
        out.push(Finding {
            id,
            episode: episode.0,
            kind_type: FindingType::Forgery,
            kind: if k == 0 { Kind::Speed } else { Kind::Climb },
            world,
            xuid,
            host_xuid: host,
            from_ms: episode.1 * 1000,
            to_ms: e * 1000,
            rules_version: rules.version,
            peak_kmh: f64::from(run_now.peak) / 10.0,
            // The proven window (the run's latest `need` seconds), never more than was checked.
            over_limit_ms: need * 1000,
            own_peak_kmh: Some(f64::from(own_peak) / 10.0),
            legs: Some(legs),
            max_jump_m: None,
        });
    }

    fn jump(&mut self, i: usize, s: u64, rules: &Rules, out: &mut Vec<Finding>) {
        let slot = self.rings[i].slot(s);
        let marked =
            slot.n >= 1 && slot.flags & (UNIVERSE_CHANGED | FIRST_SAMPLE | CLOCK_BACK) == 0;
        if !marked || f64::from(slot.jump) <= (rules.jump_m * 10.0).round() {
            return;
        }
        let (world, xuid, host) = (self.rings[i].world, self.rings[i].xuid, self.rings[i].host);
        // Once a minute per (world, XUID): the own ring holds the mark for both.
        let Some(&own) = self.index.get(&(world, xuid, RingKind::Own)) else {
            return;
        };
        let own = usize::from(own);
        if self.rings[own]
            .jump_last
            .is_some_and(|l| s.saturating_sub(l) < REREPORT_SECS)
        {
            return;
        }
        self.rings[own].jump_last = Some(s);
        let (id, episode) = (self.next_id(), self.next_id());
        out.push(Finding {
            id,
            episode,
            kind_type: FindingType::Violation,
            kind: Kind::Jump,
            world,
            xuid,
            host_xuid: host,
            from_ms: s * 1000,
            to_ms: s * 1000,
            rules_version: rules.version,
            peak_kmh: 0.0,
            over_limit_ms: 0,
            own_peak_kmh: None,
            legs: None,
            max_jump_m: Some(f64::from(slot.jump) / 10.0),
        });
    }

    #[cfg(test)]
    fn ring_slot(&self, world: WorldId, xuid: u64, kind: RingKind, s: u64) -> Option<Slot> {
        self.index
            .get(&(world, xuid, kind))
            .map(|&i| self.rings[usize::from(i)].slot(s))
    }
}

#[cfg(test)]
mod tests;
