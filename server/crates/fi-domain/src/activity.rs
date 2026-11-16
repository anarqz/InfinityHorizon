//! Idle in game (OD-121, GATE_API.md §4a): the hook's activity report (`X-Fi-Activity`,
//! GAME_PLATFORM.md §3a) and the pure classification of a session from its reports. No I/O, no
//! clock: callers pass the database clock in milliseconds.

/// The rich-presence screens that count as a menu (photo, autoshow, paint shop, garage, car meet;
/// SAURON's RE `re/afk-activity-signals.md` §RE).
pub const MENU_SCREENS: [u8; 5] = [10, 11, 12, 13, 21];

/// FH2's rich-presence screens of a race (SAURON's RE `re/afk-activity-signals.md` §R3: 4
/// EVENT_CIRCUIT_OR_P2P, 5 EVENT_STREET, 6 EVENT_CROSSCOUNTRY, 7 EVENT_SHOWCASE,
/// 8 EVENT_SHOWCASE_GENERIC, 18 ONLINE_RACE). The playground games (16, 17), wagers (20), bucket
/// lists and road trips (3, 14) are not races. CORE_API.md §4.2a `doing.racing` (CR-S102).
pub const FH2_RACE_SCREENS: [u8; 6] = [4, 5, 6, 7, 8, 18];

/// Seconds after which a session's last activity report no longer tells what it shows (three
/// missed 30 s reports).
pub const REPORT_FRESH_SECS: i64 = 90;

/// Whether the rich-presence screen `rp` of a `game` session is a race; `None` when the game's
/// screen ids are not known (only FH2's are).
pub fn racing(game: &str, rp: u8) -> Option<bool> {
    (game == "fh2").then(|| FH2_RACE_SCREENS.contains(&rp))
}

/// Seconds without input below which a stopped car still counts as engaged (browsing a menu).
pub const ENGAGED_INPUT_SECS: u32 = 60;

/// The largest `seq` and `odo_m` accepted (2^53: exact in every JSON reader).
pub const MAX_COUNTER: u64 = 1 << 53;

/// The largest `idle_s` accepted (a day).
pub const MAX_IDLE_SECS: u32 = 86_400;

/// One activity report, as the hook sends it: `v1.<seq>.<odo_m>.<idle_s>.<rp>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// The hook's counter, strictly increasing within the session.
    pub seq: u64,
    /// The hook's monotonic odometer of the player's car, metres.
    pub odo_m: u64,
    /// The game's seconds since the last player input.
    pub idle_s: u32,
    /// The game's current rich-presence screen id.
    pub rp: u8,
}

impl Report {
    /// Parses the header value; `None` for anything but exactly the v1 shape within range.
    pub fn parse(value: &str) -> Option<Report> {
        let mut parts = value.trim().split('.');
        if parts.next()? != "v1" {
            return None;
        }
        let mut field = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty() || p.len() > 16 || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let (seq, odo_m, idle_s, rp) = (field()?, field()?, field()?, field()?);
        if parts.next().is_some()
            || seq > MAX_COUNTER
            || odo_m > MAX_COUNTER
            || idle_s > u64::from(MAX_IDLE_SECS)
            || rp > 255
        {
            return None;
        }
        Some(Report {
            seq,
            odo_m,
            idle_s: idle_s as u32,
            rp: rp as u8,
        })
    }

    /// The header value of this report.
    pub fn header(&self) -> String {
        format!("v1.{}.{}.{}.{}", self.seq, self.odo_m, self.idle_s, self.rp)
    }

    /// Whether the player is engaged without driving: recent input or a menu screen.
    pub fn engaged(&self) -> bool {
        self.idle_s < ENGAGED_INPUT_SECS || MENU_SCREENS.contains(&self.rp)
    }
}

/// The thresholds of the rule (GATE_API.md §4a; `FI_IDLE_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// Metres of odometer that count as progress.
    pub progress_m: u64,
    /// Idle window of a free session, seconds.
    pub free_secs: u32,
    /// Idle window of a Supporter's session, seconds.
    pub supporter_secs: u32,
    /// Most menu time after the last progress a free session may count as play, seconds.
    pub menu_free_secs: u32,
    /// The same for a Supporter's session.
    pub menu_supporter_secs: u32,
    /// Whether an idle session someone could replace is ended (`FI_IDLE_EVICT`, OD-121 follow-up
    /// "enforcement on"); `false` only records episodes (observation).
    pub evict: bool,
    /// Warning before the end, seconds: the answers carry `evict_in` for this long first.
    pub warn_secs: u32,
}

impl Thresholds {
    /// The owner's values (OD-121).
    pub const DEFAULT: Thresholds = Thresholds {
        progress_m: 50,
        free_secs: 300,
        supporter_secs: 1_200,
        menu_free_secs: 600,
        menu_supporter_secs: 1_800,
        evict: false,
        warn_secs: 60,
    };

    /// The idle window, milliseconds: the longer (Supporter) one for every tier (OD-129).
    pub fn window_ms(&self, _tier: &str) -> i64 {
        i64::from(self.supporter_secs.max(self.free_secs)) * 1_000
    }

    /// The menu ceiling, milliseconds: the longer (Supporter) one for every tier (OD-129).
    pub fn menu_ceiling_ms(&self, _tier: &str) -> i64 {
        i64::from(self.menu_supporter_secs.max(self.menu_free_secs)) * 1_000
    }
}

/// What the server keeps of a session's activity between reports (`play_session.activity_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Track {
    /// The last accepted report's `seq`.
    pub seq: u64,
    /// The odometer at the last progress (the baseline progress is measured from).
    pub odo_base_m: u64,
    /// When the last progress was seen (ms); the first report counts as progress.
    pub progress_at: i64,
    /// When the player was last engaged (ms).
    pub engaged_at: i64,
}

/// A session's state, as `X-Fi-Activity-State` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Progress within the window.
    Active,
    /// No progress, but a menu or recent input counts as play (under the ceiling).
    Menu,
    /// Idle past the tier's window.
    Idle,
    /// No valid report ever received.
    NoSignal,
}

impl State {
    /// The header's word.
    pub const fn as_str(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Menu => "menu",
            State::Idle => "idle",
            State::NoSignal => "no_signal",
        }
    }
}

/// The result of applying a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Judged {
    /// The track after the report.
    pub track: Track,
    /// The state now.
    pub state: State,
    /// How long the session has been idle by the rule (ms, 0 while active).
    pub idle_ms: i64,
    /// How long it has been in a menu since the last progress (ms).
    pub menu_ms: i64,
}

/// The idle time of `track` at `now` with `tier`'s menu ceiling (GATE_API.md §4a):
/// `now − max(progress_at, min(engaged_at, progress_at + ceiling))`.
pub fn idle_ms(track: &Track, now: i64, tier: &str, t: &Thresholds) -> i64 {
    let credited = track
        .engaged_at
        .min(track.progress_at.saturating_add(t.menu_ceiling_ms(tier)));
    (now - track.progress_at.max(credited)).max(0)
}

/// Applies `report` (already known newer than `previous.seq`, or the first one when `previous` is
/// `None`) at `now`; `xp` is a skill score seen from this session since the last report.
pub fn judge(
    previous: Option<Track>,
    report: &Report,
    xp: bool,
    now: i64,
    tier: &str,
    t: &Thresholds,
) -> Judged {
    let mut track = match previous {
        // The first report: a baseline, counted as progress now.
        None => Track {
            seq: report.seq,
            odo_base_m: report.odo_m,
            progress_at: now,
            engaged_at: now,
        },
        Some(p) => p,
    };
    track.seq = report.seq;
    if report.odo_m < track.odo_base_m {
        // A hook restart: a new baseline, not a regression.
        track.odo_base_m = report.odo_m;
    }
    if xp || report.odo_m - track.odo_base_m >= t.progress_m {
        track.progress_at = now;
        track.odo_base_m = report.odo_m;
    }
    if report.engaged() {
        track.engaged_at = now;
    }
    classify(track, now, tier, t)
}

/// The state of `track` at `now` (no new report).
pub fn classify(track: Track, now: i64, tier: &str, t: &Thresholds) -> Judged {
    let idle = idle_ms(&track, now, tier, t);
    let menu = (track
        .engaged_at
        .min(track.progress_at + t.menu_ceiling_ms(tier))
        - track.progress_at)
        .max(0);
    let window = t.window_ms(tier);
    let state = if now - track.progress_at < window {
        State::Active
    } else if idle < window {
        State::Menu
    } else {
        State::Idle
    };
    Judged {
        track,
        state,
        idle_ms: if state == State::Active { 0 } else { idle },
        menu_ms: menu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Thresholds = Thresholds::DEFAULT;
    const MIN: i64 = 60_000;

    fn r(seq: u64, odo_m: u64, idle_s: u32, rp: u8) -> Report {
        Report {
            seq,
            odo_m,
            idle_s,
            rp,
        }
    }

    #[test]
    fn only_fh2_race_screens_are_races() {
        assert_eq!(racing("fh2", 18), Some(true));
        assert_eq!(racing("fh2", 4), Some(true));
        assert_eq!(racing("fh2", 15), Some(false));
        assert_eq!(racing("fh2", 21), Some(false));
        assert_eq!(racing("fm5", 18), None);
    }

    #[test]
    fn the_header_parses_strictly() {
        assert_eq!(Report::parse("v1.3.1200.5.2"), Some(r(3, 1200, 5, 2)));
        assert_eq!(Report::parse(" v1.0.0.0.0 "), Some(r(0, 0, 0, 0)));
        assert_eq!(r(9, 8, 7, 6).header(), "v1.9.8.7.6");
        for bad in [
            "",
            "v2.1.1.1.1",
            "v1.1.1.1",
            "v1.1.1.1.1.1",
            "v1.-1.1.1.1",
            "v1.1.1.1.256",
            "v1.1.1.86401.1",
            "v1.9007199254740993.1.1.1",
            "v1.1..1.1",
            "v1.1.1.1.0x1",
            "v1.99999999999999999.1.1.1",
        ] {
            assert_eq!(Report::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn driving_is_active_and_stopping_turns_idle_after_the_window() {
        let first = judge(None, &r(1, 0, 0, 2), false, 0, "free", &T);
        assert_eq!(first.state, State::Active);
        // Driving 100 m every 30 s.
        let mut j = first;
        for i in 1..10 {
            j = judge(
                Some(j.track),
                &r(1 + i, 100 * i, 120, 2),
                false,
                i as i64 * 30_000,
                "free",
                &T,
            );
            assert_eq!(j.state, State::Active);
        }
        let stopped_at = 9 * 30_000;
        // Parked with no input: active for the window, then idle. OD-129: every tier has the
        // Supporter's 20 minutes (a free session was idle after 5).
        let j = judge(
            Some(j.track),
            &r(20, 900, 300, 2),
            false,
            stopped_at + 6 * MIN,
            "free",
            &T,
        );
        assert_eq!(j.state, State::Active);
        let j = judge(
            Some(j.track),
            &r(21, 910, 400, 2),
            false,
            stopped_at + 21 * MIN,
            "free",
            &T,
        );
        assert_eq!((j.state, j.idle_ms), (State::Idle, 21 * MIN));
        // A Supporter has the same 20 minutes.
        let s = classify(j.track, stopped_at + 19 * MIN, "vip", &T);
        assert_eq!(s.state, State::Active);
        assert_eq!(
            classify(j.track, stopped_at + 21 * MIN, "vip", &T).state,
            State::Idle
        );
    }

    #[test]
    fn a_menu_counts_up_to_its_ceiling() {
        let j = judge(None, &r(1, 0, 0, 2), false, 0, "free", &T);
        // In the garage from minute 1 on, with input.
        let mut j = j;
        for m in 1..=60 {
            j = judge(
                Some(j.track),
                &r(1 + m as u64, 0, 5, 13),
                false,
                m * MIN,
                "free",
                &T,
            );
            let expected = if m < 20 {
                State::Active
            } else if m < 50 {
                // OD-129: every tier has the Supporter's 30-minute ceiling after the last
                // progress + its 20-minute window (a free session had 10 + 5).
                State::Menu
            } else {
                State::Idle
            };
            assert_eq!(j.state, expected, "minute {m}");
        }
        assert_eq!(j.menu_ms, 30 * MIN);
        // The Supporter's ceiling is the same 30 minutes (+ 20 of window).
        let s = judge(None, &r(1, 0, 0, 2), false, 0, "vip", &T);
        let s = judge(Some(s.track), &r(2, 0, 5, 12), false, 49 * MIN, "vip", &T);
        assert_eq!(s.state, State::Menu);
        let s = judge(Some(s.track), &r(3, 0, 5, 12), false, 51 * MIN, "vip", &T);
        assert_eq!(s.state, State::Idle);
    }

    #[test]
    fn a_skill_score_or_a_hook_restart() {
        let j = judge(None, &r(1, 5_000, 900, 2), false, 0, "free", &T);
        // A skill score is progress without moving (past the 20-minute window every tier has
        // since OD-129, where no progress would be idle).
        let j = judge(
            Some(j.track),
            &r(2, 5_000, 900, 2),
            true,
            21 * MIN,
            "free",
            &T,
        );
        assert_eq!(j.state, State::Active);
        // A restarted hook's odometer starts again from 0: a baseline, then 50 m is progress.
        let j = judge(
            Some(j.track),
            &r(3, 10, 900, 2),
            false,
            22 * MIN,
            "free",
            &T,
        );
        assert_eq!(j.track.odo_base_m, 10);
        let j = judge(
            Some(j.track),
            &r(4, 70, 900, 2),
            false,
            27 * MIN,
            "free",
            &T,
        );
        assert_eq!((j.state, j.track.progress_at), (State::Active, 27 * MIN));
        // 40 m is not progress.
        let j = judge(
            Some(j.track),
            &r(5, 110, 900, 2),
            false,
            48 * MIN,
            "free",
            &T,
        );
        assert_eq!(j.state, State::Idle);
    }
}
