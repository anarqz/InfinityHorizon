//! Detector scenarios on synthetic datagrams built by the C# codec's rules (never captures): a world
//! with host H and two relayed joiners J1 and J2, cars sampled 20 times a second.

use super::*;
use crate::nerd::synth::{car, packet, player};
use crate::nerd::{Handle, HandleKind};

const W: WorldId = [7; 16];
const H: u64 = 2_535_000_000_000_900;
const J1: u64 = 2_535_000_000_000_101;
const J2: u64 = 2_535_000_000_000_102;
const G1: GrantId = [1; 16];
const G2: GrantId = [2; 16];
const T0: u64 = 1_793_000_000_000;

fn rules() -> Rules {
    Rules {
        version: 1,
        speed: Limit {
            max_kmh: 451.0,
            sustain_ms: 3000,
        },
        climb: Limit {
            max_kmh: 150.0,
            sustain_ms: 3000,
        },
        jump_m: 200.0,
    }
}

fn info(joiner: u64) -> GrantInfo {
    GrantInfo {
        world: W,
        joiner,
        host: H,
        exp: T0 / 1000 + 100_000,
    }
}

fn own(id: u32) -> Handle {
    Handle {
        kind: HandleKind::Own,
        id,
        owner: 0,
    }
}

fn relay(id: u32, owner: u8) -> Handle {
    Handle {
        kind: HandleKind::Relay,
        id,
        owner,
    }
}

/// A simulated car: position along x, height y, the sender's clock.
#[derive(Clone, Copy, Debug)]
struct Sim {
    x: f64,
    y: f64,
    stamp: f64,
    universe: u8,
}

impl Sim {
    fn new() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            stamp: 100_000.0,
            universe: 0,
        }
    }

    /// One 50 ms step at `kmh` (and `up` km/h upward), the sender's clock at `rate`; the velocity
    /// field says `kmh`.
    fn step(&mut self, h: Handle, kmh: f64, up: f64, rate: f64) -> crate::nerd::synth::Writer {
        self.x += kmh / 3.6 * 0.05;
        self.y += up / 3.6 * 0.05;
        self.stamp += 50.0 * rate;
        car(
            h,
            [self.x as f32, self.y as f32, 0.0],
            [kmh / 3.6, up / 3.6, 0.0],
            self.universe,
            self.stamp as u32,
        )
    }
}

struct World {
    d: Detector,
    now: u64,
    out: Vec<Finding>,
}

impl World {
    fn new(max_peers: usize) -> Self {
        let mut d = Detector::new(Config { max_peers }, 42);
        d.set_rules(Some(rules()));
        let mut w = Self {
            d,
            now: T0,
            out: Vec::new(),
        };
        // The host's legs name H (owner 1), J1 (owner 2) and J2 (owner 3).
        w.bind(G1);
        w.bind(G2);
        w
    }

    fn bind(&mut self, g: GrantId) {
        let joiner = if g == G1 { J1 } else { J2 };
        let p = packet(
            1,
            &[
                player(true, relay(1, 1), H, 4),
                player(true, relay(2, 2), J1, 4),
                player(true, relay(3, 3), J2, 4),
            ],
        );
        self.d
            .observe(&g, info(joiner), Leg::HostToJoiner, &p, self.now);
    }

    /// Runs `ms` in 50 ms steps; `each` sends that step's datagrams; ticks every second.
    fn run(&mut self, ms: u64, mut each: impl FnMut(&mut Detector, u64)) {
        for _ in 0..ms / 50 {
            self.now += 50;
            each(&mut self.d, self.now);
            if self.now.is_multiple_of(1000) {
                self.d.tick(self.now, &mut self.out);
            }
        }
    }

    /// Lets every second become final.
    fn settle(&mut self) {
        self.run(5_000, |_, _| {});
    }

    fn found(&self, t: FindingType, k: Kind) -> Vec<Finding> {
        self.out
            .iter()
            .filter(|f| f.kind_type == t && f.kind == k)
            .copied()
            .collect()
    }
}

fn send_own(d: &mut Detector, g: GrantId, sim: &mut Sim, kmh: f64, now: u64) {
    let joiner = if g == G1 { J1 } else { J2 };
    let p = packet(1, &[sim.step(own(4), kmh, 0.0, 1.0)]);
    d.observe(&g, info(joiner), Leg::JoinerToHost, &p, now);
}

fn send_fwd(d: &mut Detector, g: GrantId, owner: u8, sim: &mut Sim, kmh: f64, now: u64) {
    let joiner = if g == G1 { J1 } else { J2 };
    let p = packet(
        9,
        &[sim.step(relay(10 + u32::from(owner), owner), kmh, 0.0, 1.0)],
    );
    d.observe(&g, info(joiner), Leg::HostToJoiner, &p, now);
}

#[test]
fn a_slot_is_24_little_endian_bytes() {
    assert_eq!(std::mem::size_of::<Slot>(), 24);
    let s = Slot {
        span: 0x0102_0304,
        h_max: 5000,
        h_min: 4600,
        c_max: -1,
        c_min: -2,
        v_max: 7,
        jump: 8,
        pos: [1, -1, 2],
        n: 20,
        flags: TELEPORT,
    };
    let b = s.to_bytes();
    assert_eq!(&b[..4], &[4, 3, 2, 1]);
    assert_eq!(&b[4..6], &5000u16.to_le_bytes());
    assert_eq!(&b[8..10], &[0xff, 0xff]);
    assert_eq!(&b[16..22], &[1, 0, 0xff, 0xff, 2, 0]);
    assert_eq!((b[22], b[23]), (20, 2));
    assert!(!s.covered(), "a teleport second is not covered");
}

#[test]
fn the_detector_for_100_peers_holds_under_a_megabyte() {
    let d = Detector::new(Config { max_peers: 100 }, 1);
    let bytes = d.footprint();
    println!(
        "detector footprint at 100 peers: {bytes} B (ring {} B, grant {} B, slot {} B)",
        std::mem::size_of::<Ring>(),
        std::mem::size_of::<GrantState>(),
        std::mem::size_of::<Slot>()
    );
    assert!(bytes < 1_000_000, "{bytes}");
    let d = Detector::new(Config { max_peers: 1000 }, 1);
    println!("detector footprint at 1000 peers: {} B", d.footprint());
    assert!(d.footprint() < 8_000_000);
}

#[test]
fn the_arena_is_sized_at_start_and_capped() {
    assert_eq!(Config { max_peers: 24 }.rings(), 72);
    assert_eq!(Config { max_peers: 100 }.rings(), 300);
    assert_eq!(Config { max_peers: 1000 }.rings(), 3000);
    assert_eq!(Config { max_peers: 5000 }.rings(), MAX_RINGS);
    let d = Detector::new(Config { max_peers: 100 }, 1);
    assert_eq!(d.capacity(), 300);
    assert_eq!(d.counters().rings_in_use, 0);
}

#[test]
fn a_joiner_over_the_limit_long_enough_is_one_violation_episode() {
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(3_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    w.run(6_000, |d, t| send_own(d, G1, &mut j1, 500.0, t));
    w.run(3_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    w.settle();
    let v = w.found(FindingType::Violation, Kind::Speed);
    assert_eq!(v.len(), 1, "{:?}", w.out);
    let f = v[0];
    assert_eq!((f.xuid, f.host_xuid, f.world), (J1, H, W));
    assert!((499.0..=501.0).contains(&f.peak_kmh), "{}", f.peak_kmh);
    assert!(f.over_limit_ms >= 3000);
    assert!(f.to_ms - f.from_ms >= 2000);
    assert_eq!(f.rules_version, 1);
    assert_eq!(f.id[6] >> 4, 4, "a UUID v4");
    assert!(w.found(FindingType::Forgery, Kind::Speed).is_empty());
    assert!(w.d.counters().decoded > 200);
}

#[test]
fn under_the_limit_or_too_short_is_never_reported() {
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(10_000, |d, t| send_own(d, G1, &mut j1, 440.0, t));
    // Two whole seconds over: shorter than the 3 s sustain.
    w.run(2_500, |d, t| send_own(d, G1, &mut j1, 500.0, t));
    w.run(3_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    w.settle();
    assert!(w.out.is_empty(), "{:?}", w.out);
}

#[test]
fn a_long_episode_is_reported_again_each_minute_with_the_same_episode() {
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(2_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    w.run(70_000, |d, t| send_own(d, G1, &mut j1, 500.0, t));
    w.run(2_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    w.settle();
    let v = w.found(FindingType::Violation, Kind::Speed);
    assert_eq!(v.len(), 2, "{v:?}");
    assert_eq!(v[0].episode, v[1].episode);
    assert_ne!(v[0].id, v[1].id);
    assert_eq!(v[0].from_ms, v[1].from_ms);
    assert!(v[1].to_ms >= v[0].to_ms + 60_000);
    assert!(v[1].over_limit_ms > 60_000);
}

#[test]
fn the_two_clocks_must_both_say_over() {
    // The sender's clock runs twice as fast: 250 km/h on it, 500 on the relay's.
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(8_000, |d, t| {
        let p = packet(1, &[j1.step(own(4), 500.0, 0.0, 2.0)]);
        d.observe(&G1, info(J1), Leg::JoinerToHost, &p, t);
    });
    w.settle();
    assert!(w.found(FindingType::Violation, Kind::Speed).is_empty());
    // A time-scaling sender (its clock slow): 625 km/h on it, 500 on the relay's: reported at 500.
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(8_000, |d, t| {
        let p = packet(1, &[j1.step(own(4), 500.0, 0.0, 0.8)]);
        d.observe(&G1, info(J1), Leg::JoinerToHost, &p, t);
    });
    w.settle();
    let v = w.found(FindingType::Violation, Kind::Speed);
    assert_eq!(v.len(), 1);
    assert!(v[0].peak_kmh < 510.0, "{}", v[0].peak_kmh);
}

#[test]
fn a_climb_over_its_limit_is_a_climb_violation() {
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(8_000, |d, t| {
        let p = packet(1, &[j1.step(own(4), 100.0, 200.0, 1.0)]);
        d.observe(&G1, info(J1), Leg::JoinerToHost, &p, t);
    });
    w.settle();
    let v = w.found(FindingType::Violation, Kind::Climb);
    assert_eq!(v.len(), 1, "{:?}", w.out);
    assert!((199.0..=201.0).contains(&v[0].peak_kmh));
}

#[test]
fn a_teleport_is_a_jump_never_a_speed_and_a_universe_change_is_neither() {
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(3_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    j1.x += 300.0;
    w.run(3_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    // A second teleport inside the minute is not reported again.
    j1.x += 300.0;
    w.run(3_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    // A universe change with a move: never a jump report (discontinuity).
    j1.universe = 3;
    j1.x += 5000.0;
    w.run(3_000, |d, t| send_own(d, G1, &mut j1, 100.0, t));
    w.settle();
    let j = w.found(FindingType::Violation, Kind::Jump);
    assert_eq!(j.len(), 1, "{:?}", w.out);
    assert!((300.0..=302.0).contains(&j[0].max_jump_m.unwrap()));
    assert!(w.found(FindingType::Violation, Kind::Speed).is_empty());
}

#[test]
fn the_hosts_own_car_is_judged_from_what_it_forwards() {
    let mut w = World::new(24);
    let mut h = Sim::new();
    w.run(8_000, |d, t| send_fwd(d, G1, 1, &mut h, 500.0, t));
    w.settle();
    let v = w.found(FindingType::Violation, Kind::Speed);
    assert_eq!(v.len(), 1, "{:?}", w.out);
    assert_eq!((v[0].xuid, v[0].host_xuid), (H, H));
}

/// J1 drives at 100 km/h (its own leg); the host tells J2 that J1 drives at 500.
fn forged(w: &mut World, mut extra: impl FnMut(&mut Detector, u64)) {
    let (mut j1, mut fwd) = (Sim::new(), Sim::new());
    w.run(6_000, |d, t| {
        send_own(d, G1, &mut j1, 100.0, t);
        send_fwd(d, G2, 2, &mut fwd, 100.0, t);
    });
    w.run(6_000, |d, t| {
        send_own(d, G1, &mut j1, 100.0, t);
        send_fwd(d, G2, 2, &mut fwd, 500.0, t);
        extra(d, t);
    });
    w.run(6_000, |d, t| {
        send_own(d, G1, &mut j1, 100.0, t);
        send_fwd(d, G2, 2, &mut fwd, 100.0, t);
    });
    w.settle();
}

#[test]
fn a_host_forwarding_speeds_the_player_never_sent_is_a_forgery_proof() {
    let mut w = World::new(24);
    forged(&mut w, |_, _| {});
    let f = w.found(FindingType::Forgery, Kind::Speed);
    assert_eq!(f.len(), 1, "{:?}", w.out);
    let f = f[0];
    assert_eq!((f.xuid, f.host_xuid), (J1, H));
    assert!((499.0..=501.0).contains(&f.peak_kmh));
    assert!(f.own_peak_kmh.unwrap() < 101.0);
    assert_eq!(f.legs, Some(1));
    assert!(w.found(FindingType::Violation, Kind::Speed).is_empty());
}

#[test]
fn a_foreign_car_on_a_joiners_leg_voids_the_proof() {
    let mut w = World::new(24);
    let mut injected = Sim::new();
    forged(&mut w, |d, t| {
        if t % 1000 == 0 {
            // J2 sends a car with another owner byte (it wants the host to forward it as J1's).
            let p = packet(1, &[injected.step(relay(2, 2), 500.0, 0.0, 1.0)]);
            d.observe(&G2, info(J2), Leg::JoinerToHost, &p, t);
        }
    });
    assert!(
        w.found(FindingType::Forgery, Kind::Speed).is_empty(),
        "{:?}",
        w.out
    );
    assert!(w.d.counters().foreign_owner > 0);
}

#[test]
fn a_joiner_claiming_another_xuid_voids_the_proof() {
    let mut w = World::new(24);
    let mut sent = false;
    forged(&mut w, |d, t| {
        if !sent {
            sent = true;
            let p = packet(1, &[player(true, own(1), J1, 2)]);
            d.observe(&G2, info(J2), Leg::JoinerToHost, &p, t);
        }
    });
    assert!(
        w.found(FindingType::Forgery, Kind::Speed).is_empty(),
        "{:?}",
        w.out
    );
    assert_eq!(w.d.counters().xuid_mismatch, 1);
}

#[test]
fn a_lost_own_second_means_no_proof() {
    let mut w = World::new(24);
    let (mut j1, mut fwd) = (Sim::new(), Sim::new());
    w.run(6_000, |d, t| {
        send_own(d, G1, &mut j1, 100.0, t);
        send_fwd(d, G2, 2, &mut fwd, 100.0, t);
    });
    w.run(6_000, |d, t| {
        // J1's own leg is silent for 3 s in the middle (its car reappears: not covered).
        if !(T0 + 8_000..T0 + 11_000).contains(&t) {
            send_own(d, G1, &mut j1, 100.0, t);
        }
        send_fwd(d, G2, 2, &mut fwd, 500.0, t);
    });
    w.run(6_000, |d, t| {
        send_own(d, G1, &mut j1, 100.0, t);
        send_fwd(d, G2, 2, &mut fwd, 100.0, t);
    });
    w.settle();
    // Every proof window of the forwarded run reaches into J1's silent seconds: no proof.
    assert!(
        w.found(FindingType::Forgery, Kind::Speed).is_empty(),
        "{:?}",
        w.out
    );
}

#[test]
fn owner_bytes_moving_between_xuids_void_and_unknown_owners_are_ignored() {
    let mut w = World::new(24);
    // An unbound owner byte (9): ignored entirely.
    let mut stray = Sim::new();
    w.run(8_000, |d, t| send_fwd(d, G1, 9, &mut stray, 500.0, t));
    w.settle();
    assert!(w.out.is_empty());
    // J1's XUID claimed by owner 5 on a host leg: the claim moves, the world is voided.
    let before = w.d.counters().xuid_mismatch;
    let p = packet(1, &[player(false, relay(7, 5), J1, 0)]);
    let now = w.now;
    w.d.observe(&G2, info(J2), Leg::HostToJoiner, &p, now);
    assert_eq!(w.d.counters().xuid_mismatch, before + 1);
}

#[test]
fn without_rules_nothing_is_evaluated_and_a_long_sustain_is_unevaluable() {
    let mut w = World::new(24);
    w.d.set_rules(None);
    let mut j1 = Sim::new();
    w.run(8_000, |d, t| send_own(d, G1, &mut j1, 500.0, t));
    w.settle();
    assert!(w.out.is_empty());
    assert!(w.d.counters().rules_missing > 0);
    let mut long = rules();
    long.speed.sustain_ms = 50_000;
    w.d.set_rules(Some(long));
    w.run(8_000, |d, t| send_own(d, G1, &mut j1, 500.0, t));
    w.settle();
    assert!(w.found(FindingType::Violation, Kind::Speed).is_empty());
    assert!(w.d.counters().rule_unevaluable > 0);
}

#[test]
fn rings_are_capped_ended_ones_freed_and_never_taken_while_live() {
    // One peer: 3 rings and one grant slot.
    let mut d = Detector::new(Config { max_peers: 1 }, 7);
    d.set_rules(Some(rules()));
    let mut out = Vec::new();
    let mut j1 = Sim::new();
    let short = GrantInfo {
        exp: T0 / 1000 + 10,
        ..info(J1)
    };
    let mut now = T0;
    for _ in 0..100 {
        now += 50;
        let p = packet(1, &[j1.step(own(4), 100.0, 0.0, 1.0)]);
        d.observe(&G1, short, Leg::JoinerToHost, &p, now);
        if now.is_multiple_of(1000) {
            d.tick(now, &mut out);
        }
    }
    assert_eq!(d.counters().rings_in_use, 1);
    // A second grant while the first is live: no grant slot.
    let p = packet(1, &[Sim::new().step(own(4), 100.0, 0.0, 1.0)]);
    d.observe(&G2, info(J2), Leg::JoinerToHost, &p, now);
    assert_eq!(d.counters().witness_full, 1);
    // The first grant expires; 64 s later its ring is free.
    for s in 1..=80 {
        d.tick(now + s * 1000, &mut out);
    }
    assert_eq!(d.counters().rings_in_use, 0);
}

#[test]
fn a_full_arena_takes_an_ended_ring_first_and_refuses_while_all_are_live() {
    let mut d = Detector::new(Config { max_peers: 1 }, 9);
    let mut out = Vec::new();
    let now = T0;
    // Fill the three rings with three own subjects of three successive grants (each ends).
    for (k, g) in [[11u8; 16], [12; 16], [13; 16]].iter().enumerate() {
        let gi = GrantInfo {
            world: W,
            joiner: 1000 + k as u64,
            host: H,
            exp: (now / 1000) + 3 * k as u64 + 2,
        };
        let mut s = Sim::new();
        for i in 0..20 {
            let t = now + (3 * k as u64) * 1000 + i * 50;
            let p = packet(1, &[s.step(own(4), 100.0, 0.0, 1.0)]);
            d.observe(g, gi, Leg::JoinerToHost, &p, t);
        }
        d.tick(now + (3 * k as u64 + 2) * 1000, &mut out);
    }
    assert_eq!(d.counters().rings_in_use, 3);
    // A fourth subject takes the oldest ended ring.
    let gi = GrantInfo {
        world: W,
        joiner: 2000,
        host: H,
        exp: now / 1000 + 1000,
    };
    let mut s = Sim::new();
    for i in 0..30 {
        let p = packet(1, &[s.step(own(4), 100.0, 0.0, 1.0)]);
        d.observe(&[14; 16], gi, Leg::JoinerToHost, &p, now + 20_000 + i * 50);
    }
    d.tick(now + 22_000, &mut out);
    assert_eq!(d.counters().rings_in_use, 3);
    assert!(
        d.ring_slot(W, 2000, RingKind::Own, (now + 20_000) / 1000)
            .is_some()
    );
    assert!(
        d.ring_slot(W, 1000, RingKind::Own, now / 1000).is_none(),
        "the oldest went"
    );
    assert_eq!(d.counters().witness_full, 0);
}

#[test]
fn undecodable_packets_are_counted_and_non_nerd_payloads_ignored() {
    let mut w = World::new(24);
    let mut bad = crate::nerd::synth::Writer::default();
    bad.push(127, 7);
    let p = packet(1, &[bad]);
    let now = w.now;
    w.d.observe(&G1, info(J1), Leg::JoinerToHost, &p, now);
    w.d.observe(&G1, info(J1), Leg::JoinerToHost, b"\x3e\x93voice", now);
    assert_eq!(w.d.counters().undecodable, 1);
}

#[test]
fn a_claim_long_before_the_forgery_still_taints_the_world() {
    // Review 2026-10-31: J2 names J1's XUID once, 25 s before the host's forwarded run; the
    // binding it causes outlives any time window, so the world stays tainted.
    let mut w = World::new(24);
    let p = packet(1, &[player(true, own(1), J1, 2)]);
    let now = w.now;
    w.d.observe(&G2, info(J2), Leg::JoinerToHost, &p, now);
    let (mut j1, mut fwd) = (Sim::new(), Sim::new());
    w.run(25_000, |d, t| {
        send_own(d, G1, &mut j1, 100.0, t);
        send_fwd(d, G2, 2, &mut fwd, 100.0, t);
    });
    w.run(8_000, |d, t| {
        send_own(d, G1, &mut j1, 100.0, t);
        send_fwd(d, G2, 2, &mut fwd, 500.0, t);
    });
    w.settle();
    assert!(
        w.found(FindingType::Forgery, Kind::Speed).is_empty(),
        "{:?}",
        w.out
    );
}

#[test]
fn the_hosts_own_car_is_not_judged_in_a_tainted_world() {
    let mut w = World::new(24);
    let mut injected = Sim::new();
    let p = packet(1, &[injected.step(relay(2, 1), 100.0, 0.0, 1.0)]);
    let now = w.now;
    w.d.observe(&G2, info(J2), Leg::JoinerToHost, &p, now);
    let mut h = Sim::new();
    w.run(8_000, |d, t| send_fwd(d, G1, 1, &mut h, 500.0, t));
    w.settle();
    assert!(
        w.found(FindingType::Violation, Kind::Speed).is_empty(),
        "{:?}",
        w.out
    );
    // A joiner's own data is still judged there.
    let mut j1 = Sim::new();
    w.run(8_000, |d, t| send_own(d, G1, &mut j1, 500.0, t));
    w.settle();
    assert_eq!(w.found(FindingType::Violation, Kind::Speed).len(), 1);
}

#[test]
fn only_a_packets_last_own_car_is_measured_and_the_size_field_is_ignored() {
    // Each packet: a decoy car far away, then the real one; the host forwards only the last.
    let mut w = World::new(24);
    let mut j1 = Sim::new();
    w.run(8_000, |d, t| {
        let real = j1.step(own(4), 100.0, 0.0, 1.0);
        let decoy = car(
            own(4),
            [(j1.x + 5000.0) as f32, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            0,
            j1.stamp as u32,
        );
        let mut p = packet(1, &[decoy, real]);
        // A wrong size field: the world server reads it anyway, so does the relay.
        p[9] ^= 0x55;
        d.observe(&G1, info(J1), Leg::JoinerToHost, &p, t);
    });
    w.settle();
    assert!(w.out.is_empty(), "{:?}", w.out);
    assert!(
        w.d.counters().decoded > 300,
        "both cars decoded, the last measured"
    );
}
