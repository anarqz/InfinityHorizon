//! The game plane's integrity gates over what a player's game reports (SER-M3 step 6), ported
//! from the Beta's `server/src/forza/gates.rs`: pure functions of the report, the catalog and, for
//! the profile sync, what was accepted before. Nobody outside a player's circle sees their
//! numbers, so these stop the plain cheats: impossible numbers and times, cars that do not exist,
//! and (Hardcore) progress no play produces. A refused report is not stored (the game is answered
//! as usual) and the profile gets a strike once per refusal episode (the caller's:
//! `integrity_strike`).
//!
//! **Guardrails (OD-62, ADR 0020).** A Hardcore profile sync's growth since its baseline is
//! assessed by `fi_guard` with FH2's own rules ([`struct@crate::RULES`]; [`assess_profile`],
//! [`assess_garage`]): ceilings refuse absurd values from day one, while rates and conservation
//! gaps only score (0-100 with reason codes) and refuse only with the deployment's enforcement
//! switch on. The Beta's token buckets (credits and winnings 540k refilled at 620k/h, XP 384k at
//! 503k/h, levels 12 at 16/h, cars 6 at 20/h), which refused legitimate bursts of saved level-up
//! spins, are gone; twice their hourly rates calibrate the guardrails' rate flags. The game's own
//! limits ([`progress`]) and the technical checks still refuse every sync. OD-60: a Hardcore
//! profile always has a baseline before its sync is measured (the seed's numbers, `fi_db::seeds`),
//! so its first sync is measured like any other and never becomes the baseline; one measured
//! without a baseline is measured from zero (fail closed).
//!
//! Time is an argument (Unix milliseconds, the caller's database clock): the crate reads no clock.

use crate::Catalog;
use fi_guard::{Assessment, Floor, Judged, Numbers, Rates};
use serde_json::Value;

/// Over a car's stock top speed: upgrades, drafting, downhill.
pub const SPEED_MARGIN: f64 = 1.3;

/// XP the game may report with `Scoreboard.SubmitXpScore` in any hour.
pub const XP_REPORTED_PER_HOUR: f64 = 503_000.0;

/// The most XP one `SubmitXpScore` carries (one race pays a few thousand; the club ladder adds
/// them up), the Beta's clamp.
pub const XP_PER_REPORT: f64 = 50_000.0;

/// Highest car id the game uses (base cars go to 2501; car packs add their own in between and
/// above, which the catalog does not list).
pub const MAX_CAR_ID: i64 = 4095;
/// The game's own limit on credits and race winnings, whatever the history.
pub const MAX_CREDITS: i64 = 999_999_999;
/// The game's own limit on XP.
pub const MAX_XP: i64 = 200_000_000;
/// The game's own limit on the level.
pub const MAX_LEVEL: i64 = 999;

/// A new top speed record is checked only when it beats the last accepted one by more than this
/// (m/s): the same record comes back in every later sync.
pub const TOP_SPEED_RECORD_STEP: f64 = 0.5;

/// Why a report was refused: a stable rule id (the fact's `refusal`, the logs) and an English
/// reason without player data (the strike's).
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    /// `race_time`, `xp_rate`; the profile sync's `numbers`, `limits`; the garage's
    /// `invalid_car`; the guardrails' ceilings (`credits_ceiling`, `credits_unexplained_ceiling`,
    /// `winnings_ceiling`, `xp_ceiling`, `level_ceiling`) and `anomaly_score` (a score of 60 or
    /// more with the enforcement switch on); `top_speed` (a signal only).
    pub rule: &'static str,
    /// For support.
    pub reason: String,
}

fn refusal(rule: &'static str, reason: String) -> Refusal {
    Refusal { rule, reason }
}

impl From<fi_guard::Refusal> for Refusal {
    fn from(r: fi_guard::Refusal) -> Self {
        Refusal {
            rule: r.rule(),
            reason: r.reason(),
        }
    }
}

/// A race time: the track's length cannot be covered faster than the car's top speed (with
/// [`SPEED_MARGIN`]) allows ([`car_reference`]: a car id the game cannot have is refused, a
/// car-pack car the catalog does not list is held to the catalog's fastest car). A track the
/// catalog does not list has no reference: passed.
pub fn race_time(catalog: &Catalog, car: i64, track: i64, seconds: f64) -> Result<(), Refusal> {
    let speed = car_reference(catalog, car)?;
    let Some(track) = catalog.track(track) else {
        return Ok(());
    };
    let fastest = speed * SPEED_MARGIN;
    if fastest > 0.0 && track.length_m > 0 && seconds < track.length_m as f64 / fastest {
        return Err(Refusal {
            rule: "race_time",
            reason: format!(
                "{seconds:.2} s over {} m with car {car} (reference top speed {:.0} km/h)",
                track.length_m,
                speed * 3.6
            ),
        });
    }
    Ok(())
}

/// One XP report as the game sends it, clamped to `0..=`[`XP_PER_REPORT`] (not finite: 0).
pub fn xp_of_report(xp: f64) -> f64 {
    if xp.is_finite() {
        xp.clamp(0.0, XP_PER_REPORT)
    } else {
        0.0
    }
}

/// An XP report on top of what the profile reported in the last hour.
pub fn xp_rate(last_hour: f64, xp: f64) -> Result<(), Refusal> {
    if last_hour + xp > XP_REPORTED_PER_HOUR {
        return Err(Refusal {
            rule: "xp_rate",
            reason: format!("{XP_REPORTED_PER_HOUR:.0} XP over an hour"),
        });
    }
    Ok(())
}

/// The numbers of an `OnlineProfile.UpdateAll` the gates look at, read from the merged profile
/// (`playerHistory[0].credits`, `playerRaceHistory[0].{xp, level, totalWinnings, topSpeed}`).
///
/// `playerRaceHistory[0].level` is **not** the driver level (CR-S81, SAURON 2026-10-31, decoded
/// UpdateAll bodies: 0 at XP 228,874, 1 at 230,494 and at 232,194, where the driver level is 14,
/// 15, 15): like ProfileData `Main/Level` it is the level-ups done inside the current colour band,
/// back to 0 at every band boundary. So [`Progress::level`] is the driver level derived from the XP
/// ([`driver_level`]) and the reported value is kept apart as [`Progress::band_level`], which no
/// check compares with anything but the game's own limit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    /// Credits.
    pub credits: i64,
    /// XP.
    pub xp: i64,
    /// The driver level the XP reaches on FH2's curve ([`driver_level`]; stored from 0, the game
    /// shows level + 1): what every guardrail, the baseline and the presence use.
    pub level: i64,
    /// `playerRaceHistory[0].level` as the game reported it: the in-band step (level-ups inside
    /// the current colour band), never the driver level. Checked only against [`MAX_LEVEL`].
    pub band_level: i64,
    /// Total race winnings (a lifetime figure the game keeps through a career reset).
    pub winnings: i64,
    /// m/s, the fastest the player ever went.
    pub top_speed: f64,
}

/// What was last accepted of a profile's UpdateAll, with the guardrails' rates at it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Baseline {
    /// Credits.
    pub credits: i64,
    /// XP.
    pub xp: i64,
    /// Level.
    pub level: i64,
    /// Total race winnings.
    pub winnings: i64,
    /// When it was accepted (Unix ms).
    pub synced_ms: i64,
    /// The rates per hour at it (zero after a seed).
    pub rates: Rates,
    /// The highest level accepted since the seed (`None`: `level`): level-up spins are granted
    /// only above it (review G2-3).
    pub level_high: Option<i64>,
    /// The fact checks' values refused by score since it was accepted (review G2-6).
    pub judged: Judged,
}

/// What was last accepted of a profile's UpdateAllTables that carried a garage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GarageBaseline {
    /// The garage's size after it.
    pub cars: i64,
    /// When it was accepted (Unix ms).
    pub synced_ms: i64,
    /// Cars per hour at it (zero after a seed).
    pub rate: f64,
}

/// Whether a merged profile carries both sections the numbers come from (`playerHistory[0]` and
/// `playerRaceHistory[0]`), so it can seed a baseline: a section the game has not sent yet would
/// read as 0 and make its first real numbers look like growth.
pub fn complete(profile: &Value) -> bool {
    profile["playerHistory"][0].is_object() && profile["playerRaceHistory"][0].is_object()
}

/// The numbers alone, technically checked (none negative), with no limit: what a Noob profile's
/// sync is held to. A missing number reads as 0, and so does a top speed that is not a finite
/// number (the codec writes NaN and the infinities as `"NaN"`, `"Infinity"`): an absent record,
/// never a refusal, as the game keeps sending it.
pub fn progress_numbers(profile: &Value) -> Result<Progress, Refusal> {
    let history = &profile["playerHistory"][0];
    let race = &profile["playerRaceHistory"][0];
    let int = |v: &Value, name: &str| -> Result<i64, Refusal> {
        match v
            .as_i64()
            .or_else(|| v.as_u64().map(|u| u.min(i64::MAX as u64) as i64))
        {
            Some(n) if n >= 0 => Ok(n),
            Some(_) => Err(refusal("numbers", format!("negative {name}"))),
            None => Ok(0),
        }
    };
    let top_speed = race["topSpeed"]
        .as_f64()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0);
    if top_speed < 0.0 {
        return Err(refusal("numbers", "negative top speed".into()));
    }
    let xp = int(&race["xp"], "XP")?;
    Ok(Progress {
        credits: int(&history["credits"], "credits")?,
        xp,
        level: driver_level(xp),
        band_level: int(&race["level"], "level")?,
        winnings: int(&race["totalWinnings"], "winnings")?,
        top_speed,
    })
}

/// The numbers, technically checked and within the game's limits ([`MAX_CREDITS`], [`MAX_XP`],
/// [`MAX_LEVEL`]): what a Hardcore profile's sync is held to before its growth is assessed.
pub fn progress(profile: &Value) -> Result<Progress, Refusal> {
    let p = progress_numbers(profile)?;
    if p.credits > MAX_CREDITS
        || p.winnings > MAX_CREDITS
        || p.xp > MAX_XP
        || p.band_level > MAX_LEVEL
    {
        return Err(refusal("limits", "numbers beyond the game's limits".into()));
    }
    Ok(p)
}

/// The driver level `xp` reaches on FH2's curve ([`crate::rules::LEVEL_CURVE`], stored from 0):
/// the level every sync-path check uses, whatever level the game reported (CR-S81). The same curve
/// as a save's (`fi_saves::gates::with_driver_level` with [`struct@crate::RULES`]).
///
/// FH2 is the only game; a second game's sync path needs its own curve here.
pub fn driver_level(xp: i64) -> i64 {
    crate::rules::LEVEL_CURVE.level_of(xp)
}

/// The driver level the game **shows** for an XP total: the stored level ([`driver_level`]) plus
/// one, at most [`MAX_LEVEL`] (223,773 XP is stored 14, shown 15; the curve's last step,
/// 128,412,500, is shown 999). `None` without XP (0 or less): no level, never level 1.
pub fn shown_level(xp: i64) -> Option<i64> {
    (xp > 0).then(|| (driver_level(xp) + 1).min(MAX_LEVEL))
}

/// The career level a gated (Hardcore) player card keeps (`User.Logon`'s `loginData` and
/// `SetPlayerCardInfo`'s `currentCareerLevel`, OD-177): at most [`MAX_LEVEL`], the game's own limit
/// on the level. Other players see it on the cards of boards, clubs and shared files.
pub fn card_level(level: u32) -> u32 {
    level.min(MAX_LEVEL as u32)
}

/// The badge and title a gated (Hardcore) player card keeps (`currentBadgeId`,
/// `currentPlayerTitleId`; OD-178 item 4): an id the game's `List_BadgeAndTitleUnlocks` does not
/// list as a badge (or a title) becomes [`crate::card_ids::FALLBACK`], UnlockId 1, exactly what the
/// game itself shows for a row it cannot find, so others see what the game would render and the
/// player's Logon is never refused. Ownership (the unlock condition) is not checked.
pub fn card_ids(badge: u16, title: u16) -> (u16, u16) {
    let ids = crate::card_ids::CardIds::fh2();
    let keep = |ok: bool, id: u16| if ok { id } else { crate::card_ids::FALLBACK };
    (keep(ids.badge(badge), badge), keep(ids.title(title), title))
}

/// Hours from `then_ms` to `now_ms`, never negative, at most [`fi_guard::MAX_HOURS`]: what a
/// profile sync is measured over (wall time since the last accepted sync).
pub fn hours_since(then_ms: i64, now_ms: i64) -> f64 {
    (now_ms.saturating_sub(then_ms).max(0) as f64 / 3_600_000.0).min(fi_guard::MAX_HOURS)
}

/// The most hours a profile sync's **fast** rates count ([`fi_guard::FAST_MAX_HOURS`], one: the
/// rates' time constant). The game syncs during play, every race or so, so a longer gap since the
/// last accepted sync is time the server did not see played (the player away, the game offline,
/// syncs held back): the fast step counts one hour, never more, so waiting earns nothing beyond
/// it. The slow (day) rates see the whole gap and are what bounds a sync every hour (review G2-1).
pub const SYNC_MAX_HOURS: f64 = fi_guard::FAST_MAX_HOURS;

/// Which sections of a merged profile the game has sent: a section it has not sent yet is
/// measured as unchanged, never as zeros (they would read as an XP and level drop, review G2-3,
/// and as a credits change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sections {
    /// `playerHistory[0]` (credits).
    pub history: bool,
    /// `playerRaceHistory[0]` (XP, level, winnings).
    pub race: bool,
}

impl Sections {
    /// Both sections.
    pub const ALL: Sections = Sections {
        history: true,
        race: true,
    };

    /// What `profile` carries.
    pub fn of(profile: &Value) -> Sections {
        Sections {
            history: profile["playerHistory"][0].is_object(),
            race: profile["playerRaceHistory"][0].is_object(),
        }
    }
}

/// A profile sync's numbers as the guardrails measure them (no garage).
pub fn numbers(p: &Progress) -> Numbers {
    Numbers {
        credits: Some(p.credits),
        xp: Some(p.xp),
        level: Some(p.level),
        winnings: Some(p.winnings),
        cars: None,
        skills: None,
    }
}

/// The guardrails' assessment of a Hardcore UpdateAll (`new`, already within the game's limits,
/// with the `sections` its merged profile carries) against the profile half of its baseline, over
/// the wall time since that sync ([`hours_since`]; the fast rates count at most an hour of it,
/// [`SYNC_MAX_HOURS`]), with FH2's rules, the baseline's level high-water mark and its refused
/// facts. Its [`Assessment::rates`] and [`Assessment::level_high`] are what an accepted sync
/// stores with the new baseline. No baseline: measured from zero now (fail closed; OD-60 seeds one
/// before any Hardcore sync).
///
/// Levels are driver levels on both sides (CR-S81): `new`'s from its XP ([`progress_numbers`]),
/// and the baseline's and its judged drop floor's re-derived from their stored XP here, since rows
/// written before the fix hold the game's in-band step (a seed's `Main/Level`, a sync's reported
/// level). Without that a stored in-band 1 read against a derived 15 would be 14 levels in one
/// sync (`LEVELS_RATE_HIGH`, `LEVEL_CEILING`, 14 spins), and a band change (in-band 13 then 0) an
/// `XP_LEVEL_DROP`. A stored level high-water mark is kept: a game's own in-band step is never
/// above the driver level at the same XP (the band's first level is at least 0), so a mark built
/// from genuine reports is at most the true one, and spins are still granted only above the
/// previous driver level. A mark inflated before the fix by an edited reported level (accepted in
/// shadow, or the second time with the switch on, G2-6) stays above until the career passes it:
/// fewer spins explained, never more (REVIEW_LOG "A profile sync's level" counts them). With levels
/// derived from the XP, `LEVEL_XP_MISMATCH` cannot fire on a sync (the deviation is 0 by
/// construction): the reported in-band step is not checked against the XP (CLOUD_SAVES.md §7.3
/// "A save's level").
pub fn assess_profile(
    previous: Option<&Baseline>,
    new: &Progress,
    sections: Sections,
    now_ms: i64,
) -> Assessment {
    let b = previous.copied().unwrap_or(Baseline {
        credits: 0,
        xp: 0,
        level: 0,
        winnings: 0,
        synced_ms: now_ms,
        rates: Rates::default(),
        level_high: None,
        judged: Judged::default(),
    });
    let b = Baseline {
        level: driver_level(b.xp),
        judged: Judged {
            drop_floor: b.judged.drop_floor.map(|f| Floor {
                level: driver_level(f.xp),
                ..f
            }),
            ..b.judged
        },
        ..b
    };
    let mut next = numbers(new);
    if !sections.history {
        next.credits = Some(b.credits);
    }
    if !sections.race {
        next.xp = Some(b.xp);
        next.level = Some(b.level);
        next.winnings = Some(b.winnings);
    }
    fi_guard::assess(&fi_guard::Input {
        previous: Numbers {
            credits: Some(b.credits),
            xp: Some(b.xp),
            level: Some(b.level),
            winnings: Some(b.winnings),
            cars: None,
            skills: None,
        },
        next,
        hours: hours_since(b.synced_ms, now_ms),
        rates: b.rates,
        level_high: b.level_high,
        judged: b.judged,
        game: None,
        rules: *crate::RULES,
    })
}

/// The guardrails' assessment of a Hardcore UpdateAllTables that carries a garage: the garage's
/// size after it (`after`) against the garage half of the baseline. Cars only score, never refuse
/// (no car ceiling: the study's measure is cars without an origin, register CR-S24). No baseline:
/// measured from an empty garage now.
pub fn assess_garage(previous: Option<&GarageBaseline>, after: i64, now_ms: i64) -> Assessment {
    let b = previous.copied().unwrap_or(GarageBaseline {
        cars: 0,
        synced_ms: now_ms,
        rate: 0.0,
    });
    let cars = |n| Numbers {
        cars: Some(n),
        ..Numbers::default()
    };
    fi_guard::assess(&fi_guard::Input {
        previous: cars(b.cars),
        next: cars(after),
        hours: hours_since(b.synced_ms, now_ms),
        rates: Rates {
            cars: b.rate,
            ..Rates::default()
        },
        level_high: None,
        judged: Judged::default(),
        game: None,
        rules: *crate::RULES,
    })
}

/// The fastest stock car among `cars` (m/s); `None` when there is none, or when any of them is a
/// car the catalog does not list (a car pack's may be the fastest): no reference, the check is
/// skipped (CLOUD_SAVES.md §7.1 rule 2; the Beta fell back to the whole game's fastest car only
/// when it knew none, so a starter plus a car-pack car was capped by the starter, failure 18).
pub fn fastest(catalog: &Catalog, cars: &[i64]) -> Option<f64> {
    let mut best: Option<f64> = None;
    for id in cars {
        let speed = catalog.car(*id)?.top_speed_mps;
        best = Some(best.map_or(speed, |b: f64| b.max(speed)));
    }
    best
}

/// A top speed record against the garage's fastest stock car with [`SPEED_MARGIN`]; passed when
/// [`fastest`] has no reference. A signal only, never a refusal (the Beta's hotfix 2026-10-18,
/// CLOUD_SAVES.md §7.1 rule 5): tuning a car past its stock speed is the game's core loop.
pub fn check_top_speed(catalog: &Catalog, cars: &[i64], top_speed: f64) -> Result<(), Refusal> {
    let Some(limit) = fastest(catalog, cars).map(|f| f * SPEED_MARGIN) else {
        return Ok(());
    };
    if limit > 0.0 && top_speed > limit {
        return Err(refusal(
            "top_speed",
            format!(
                "top speed {:.0} km/h, the garage's cars allow {:.0} km/h",
                top_speed * 3.6,
                limit * 3.6
            ),
        ));
    }
    Ok(())
}

/// The `garageCars` of an UpdateAllTables: car ids the game can have (the catalog's, or a car
/// pack's within `1..=`[`MAX_CAR_ID`]). Every sync, the first included, and every mode.
pub fn check_garage_cars(catalog: &Catalog, tables: &Value) -> Result<(), Refusal> {
    let rows = tables["garageCars"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for row in rows {
        let car = &row["carId"];
        let valid = car.as_i64().is_some_and(|c| known_car(catalog, c));
        if !valid {
            return Err(refusal("invalid_car", format!("unknown car {car}")));
        }
    }
    Ok(())
}

/// The most keys a profile's stored `garageCars` may hold after an UpdateAllTables merge (OD-175,
/// the structural cap of every mode): a merge that would leave more is refused for that table
/// alone. A real FH2 garage is far below it (the game's 354 cars, several of some).
pub const MAX_GARAGE_KEYS: i64 = 1_000;

/// Whether `car` is a car the game can have: the catalog's (base or known car pack, any id), or
/// an unknown car pack's within `1..=`[`MAX_CAR_ID`]. One definition for
/// [`check_garage_cars`] and [`keep_known_cars`].
pub fn known_car(catalog: &Catalog, car: i64) -> bool {
    catalog.car(car).is_some() || (1..=MAX_CAR_ID).contains(&car)
}

/// OD-175's structural rule on an UpdateAllTables garage (every mode, separate from the economic
/// gate [`assess_garage`]): keeps only the rows (already one per key, the request's duplicate keys
/// collapsed by the caller) whose `carId` is a car the game can have ([`known_car`]: car packs
/// kept, the OD-175 follow-up); returns how many it dropped. After [`check_garage_cars`] has
/// accepted the upload nothing is left to drop: it is the stored garage's own guarantee. The same
/// car under several keys stays: owning it more than once is legitimate.
pub fn keep_known_cars(catalog: &Catalog, rows: &mut serde_json::Map<String, Value>) -> usize {
    let before = rows.len();
    rows.retain(|_, row| row["carId"].as_i64().is_some_and(|c| known_car(catalog, c)));
    before - rows.len()
}

// --- The scores of `Scoreboard.SubmitScore` (the scoreboard lane, GAME_PLATFORM.md §3a). ---

/// The fastest stock car of the catalog (m/s; 122.1 in FH2's).
pub fn catalog_fastest(catalog: &Catalog) -> f64 {
    catalog.cars().map(|c| c.top_speed_mps).fold(0.0, f64::max)
}

/// The stock top speed a score's car is held to (m/s): the car's own (a known car pack's car's
/// own, [`Catalog::car`]); for an unknown car-pack id within `1..=`[`MAX_CAR_ID`],
/// [`catalog_fastest`] (no car is faster than the
/// game's fastest by more than [`SPEED_MARGIN`]). A car id outside that range is not a car the
/// game can have: refused (`race_time`).
pub fn car_reference(catalog: &Catalog, car: i64) -> Result<f64, Refusal> {
    if let Some(c) = catalog.car(car) {
        return Ok(c.top_speed_mps);
    }
    if (1..=MAX_CAR_ID).contains(&car) {
        return Ok(catalog_fastest(catalog));
    }
    Err(refusal("race_time", format!("score in unknown car {car}")))
}

/// The catalogue's placeholder car ordinal (no real car): never held to a per-car bound.
pub const PLACEHOLDER_CAR: i64 = 1215;

/// The speed a speed board's score (m/s) is held to for car `car` (OD-177): the car's own
/// upgraded top-speed bound ([`crate::topspeed`], km/h, OD-163) with [`SPEED_MARGIN`], never above
/// the game-wide ceiling (the catalog's fastest stock car with [`SPEED_MARGIN`]). The upgraded
/// bound is the car fully upgraded, engine swaps included, so a tuned car is never refused below
/// what tuning can reach (CLOUD_SAVES.md §7.1 rule 5, CR-L14). A car the table does not list (a
/// car pack), the placeholder, and a `low`-confidence row (the table's least sure bounds, which the
/// per-car speed limit of the peer observations never trusts below its flat limit either,
/// MATCHMAKING_API.md §3.9) keep the game-wide ceiling.
pub fn trap_ceiling(catalog: &Catalog, car: i64) -> f64 {
    let game = catalog_fastest(catalog) * SPEED_MARGIN;
    match crate::topspeed::TopSpeeds::fh2().get(car) {
        Some(row)
            if car != PLACEHOLDER_CAR && row.confidence != crate::topspeed::Confidence::Low =>
        {
            (f64::from(row.max_kmh) / 3.6 * SPEED_MARGIN).min(game)
        }
        _ => game,
    }
}

/// A speed board's score (score types 5 and 6, speed traps and zones, m/s): at most
/// [`trap_ceiling`] of its car (OD-177: the car's own upgraded bound with [`SPEED_MARGIN`], the
/// game-wide ceiling for a car pack's car).
pub fn trap_speed(catalog: &Catalog, car: i64, mps: f64) -> Result<(), Refusal> {
    let ceiling = trap_ceiling(catalog, car);
    if ceiling > 0.0 && mps > ceiling {
        return Err(refusal(
            "race_time",
            format!(
                "{:.0} km/h on a speed board in car {car}, over {:.0} km/h",
                mps * 3.6,
                ceiling * 3.6
            ),
        ));
    }
    Ok(())
}

/// The rule of a stats-board value over its board's ceiling ([`points`]): refused without a strike
/// and recorded once per profile and board (OD-178 follow-up: the boards send running totals, so a
/// strike would repeat on every later activity).
pub const STATS_CEILING_RULE: &str = "stats_ceiling";

/// The enforced stats-board ceilings by score type (scoreboard type 4): 4/8 barn finds 10, 4/9
/// bonus boards 150, 4/10 roads 315, 4/14 Storm Island roads 50 ([`crate::stats_boards`], T-PD-S5),
/// and since T-PD-S6 (`fh2/gate-confirmations.json` "statsBoards") 4/7 Horizon Promo (the
/// confirmed 354 base car models plus the 303 known car packs' cars, 657: a photographed pack car
/// counts as well; derived, not capture-confirmed) and 4/12 bucket list 63 (55 mainland + 8 Storm Island: safe whether the
/// counter is per world or cumulative). 4/15 (Storm Island boards: no safe cap), 4/11 and 4/13
/// have none. 4/9 stays 150 until T-PD-S6's likely 100 is confirmed. Derived from the game's
/// counts, so the game data's [`GATE_TABLES_FILE`].
pub static ENFORCED_STATS_CEILINGS: fi_gamedata::Lazy<Vec<(i32, i64)>> =
    fi_gamedata::Lazy::new(|| {
        GATE_TABLES
            .load()
            .map(|t| t.stats_ceilings.clone())
            .map_err(Clone::clone)
    });

/// A points board's score (OD-178 item 4 and its follow-up, T-PD-S5): on a stats board
/// (scoreboard type 4) a whole number, not negative (else rule `race_time`, struck like every
/// score refusal), and at most the board's own maximum where it is enforced
/// ([`struct@ENFORCED_STATS_CEILINGS`]; else rule [`STATS_CEILING_RULE`], never struck). Any other board
/// keeps only the boards' bound of 1e9. Keyed by `(scoreboardType, scoreType)`: the Beta-era
/// ceiling of 10,000 on score type 10 alone (its captures' trackless boards, which were 4/10 roads)
/// is gone.
pub fn points(scoreboard_type: i64, score_type: i64, value: f64) -> Result<(), Refusal> {
    if scoreboard_type != 4 {
        return Ok(());
    }
    let key = format!("4/{score_type}");
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(refusal(
            "race_time",
            format!("{value} on stats board {key}, not a whole number"),
        ));
    }
    let max = ENFORCED_STATS_CEILINGS
        .iter()
        .find(|(t, _)| i64::from(*t) == score_type)
        .map(|(_, max)| *max);
    match max {
        Some(max) if value > max as f64 => Err(refusal(
            STATS_CEILING_RULE,
            format!("{value:.0} on stats board {key}, over {max}"),
        )),
        _ => Ok(()),
    }
}

/// A submitted score on a gated (hardcore) profile: its car must be one the game can have
/// ([`car_reference`]); a time (score type 1) is [`race_time`], a speed (5, 6) [`trap_speed`]
/// (the car's own bound, OD-177), anything else [`points`] (a stats board's whole numbers and
/// ceilings). Every refusal is rule `race_time` (the strike's source) but a stats board's ceiling,
/// [`STATS_CEILING_RULE`] (never struck).
pub fn score(
    catalog: &Catalog,
    scoreboard_type: i64,
    score_type: i64,
    car: i64,
    track: i64,
    value: f64,
) -> Result<(), Refusal> {
    car_reference(catalog, car)?;
    match score_type {
        1 => race_time(catalog, car, track, value),
        5 | 6 => trap_speed(catalog, car, value),
        other => points(scoreboard_type, other, value),
    }
}

/// Car ordinals no player may drive: the 17 traffic cars SAURON mapped (V9, 2026-10-31), the game
/// data's [`GATE_TABLES_FILE`] (game car ids). The
/// peer observations count a `car` observation of one (`fi_db::observations`), and a gated
/// creation of one is refused ([`creation_car`], OD-178).
pub static BLOCKED_CARS: fi_gamedata::Lazy<Vec<i64>> = fi_gamedata::Lazy::new(|| {
    GATE_TABLES
        .load()
        .map(|t| t.blocked_cars.clone())
        .map_err(Clone::clone)
});

/// The game data file of [`struct@BLOCKED_CARS`] and [`struct@ENFORCED_STATS_CEILINGS`].
pub const GATE_TABLES_FILE: &str = "fh2/gate-tables.json";

/// The tables of [`GATE_TABLES_FILE`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateTables {
    /// [`struct@BLOCKED_CARS`], ascending.
    pub blocked_cars: Vec<i64>,
    /// [`struct@ENFORCED_STATS_CEILINGS`]: `(score type, max)`.
    pub stats_ceilings: Vec<(i32, i64)>,
}

impl GateTables {
    /// The tables of a gate tables JSON; blocked cars must be ascending and unique.
    pub fn from_json(text: &str) -> Result<GateTables, String> {
        let t: GateTables = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if !t.blocked_cars.windows(2).all(|w| w[0] < w[1]) {
            return Err("blockedCars must be ascending and unique".to_owned());
        }
        Ok(t)
    }
}

static GATE_TABLES: fi_gamedata::Lazy<GateTables> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(GATE_TABLES_FILE, GateTables::from_json));

/// The car of a creation a gated ticket saves or shares (OD-178: `UpdateLivery`, the versioned
/// livery and tune creates, `UpdateCarSetup`, the Storefront creates): one the game can have
/// ([`known_car`], car packs included) and not a traffic car ([`struct@BLOCKED_CARS`]). Rule
/// `creation_car` (never struck: no creation rule strikes).
pub fn creation_car(catalog: &Catalog, car: i64) -> Result<(), Refusal> {
    if !known_car(catalog, car) {
        return Err(refusal(
            "creation_car",
            format!("a creation for unknown car {car}"),
        ));
    }
    if BLOCKED_CARS.contains(&car) {
        return Err(refusal(
            "creation_car",
            format!("a creation for traffic car {car}"),
        ));
    }
    Ok(())
}

/// A submitted score's class and PI on a gated ticket (OD-178, its follow-up and T-PD-S6), rule
/// `race_time` like every score refusal: the class is one of the catalog's; the PI
/// (`carPerformanceIndex` as the game sends it, a raw index of 0 to 1: an X car is 1.0) is finite,
/// above 0 and at most 1; the class is the raw index's own band ([`crate::pi::class_of`], either
/// class within [`crate::pi::SUBMIT_EPSILON`] of a band edge); and on a Monthly Rivals event with a
/// class (`required_class`, `None` for an open event or another board) it is exactly that class
/// (the game's event filter admits only that band). No stock-class rule: FH2 lets players lower a
/// car's class.
pub fn score_class(
    catalog: &Catalog,
    class: i64,
    pi: f64,
    required_class: Option<i64>,
) -> Result<(), Refusal> {
    let bad = |why: String| Err(refusal("race_time", why));
    if catalog.class(class).is_none() {
        return bad(format!("score in unknown class {class}"));
    }
    if !(pi.is_finite() && pi > 0.0 && pi <= 1.0) {
        return bad(format!("score with PI {pi}"));
    }
    let (low, high) = crate::pi::classes_near(pi);
    if class != low && class != high {
        return bad(format!(
            "score in class {class} with a raw PI {pi:.6} of class {}",
            crate::pi::class_of(pi)
        ));
    }
    if let Some(required) = required_class.filter(|r| *r != class) {
        return bad(format!(
            "score in class {class} on an event of class {required}"
        ));
    }
    Ok(())
}

/// A shared tune's declared class on a gated ticket (`StorefrontTuning.Create`'s `ClassId`, OD-178
/// follow-up): one of the catalog's classes, rule `creation_class` (never struck).
pub fn tune_class(catalog: &Catalog, class: i64) -> Result<(), Refusal> {
    if catalog.class(class).is_none() {
        return Err(refusal(
            "creation_class",
            format!("a tune of unknown class {class}"),
        ));
    }
    Ok(())
}

/// OD-178: keeps the `purchasedParts` rows of an UpdateAllTables whose part the game has; returns
/// how many it dropped. A row is dropped when its numbers are missing or negative, or when its car
/// (`garageId` through `garage`, garage id → car ordinal: the upload's rows over the stored
/// garage) is a base-catalogue car ([`Catalog::stock_car`]) and the part (`partEnum`, `partId`) is not in that slot's list
/// ([`crate::parts::PartPrices`]) or `pricePaid` is above its list price. A part of a car pack's
/// car (known or not), or whose garage car is not known, is kept (the save audit's rule, `crate::audit`).
pub fn keep_known_parts(
    catalog: &Catalog,
    prices: &crate::parts::PartPrices,
    garage: &std::collections::BTreeMap<i64, i64>,
    rows: &mut serde_json::Map<String, Value>,
) -> usize {
    let before = rows.len();
    rows.retain(|_, row| {
        let n = |k: &str| row[k].as_i64();
        let (Some(slot), Some(id), Some(paid)) = (n("partEnum"), n("partId"), n("pricePaid"))
        else {
            return false;
        };
        if slot < 0 || id < 0 || paid < 0 {
            return false;
        }
        let base_car = n("garageId")
            .and_then(|g| garage.get(&g))
            .is_some_and(|car| catalog.stock_car(*car).is_some());
        if !base_car {
            return true;
        }
        prices.price(slot, id).is_some_and(|list| paid <= list)
    });
    before - rows.len()
}

/// The highest speed any car reaches (m/s): the catalog's fastest stock car with [`SPEED_MARGIN`],
/// the game-wide ceiling of [`trap_ceiling`].
pub fn game_speed_ceiling(catalog: &Catalog) -> f64 {
    catalog_fastest(catalog) * SPEED_MARGIN
}

/// The distance a `trackStats` row may carry with a `totalTimeDriven` of 0 (metres; the two
/// counters round on their own, so a short drive can show distance without a whole second).
pub const TRACK_DISTANCE_WITHOUT_TIME: i64 = 1_000;

/// OD-178: keeps the plausible `trackStats` rows of an UpdateAllTables; returns how many it
/// dropped. A row is dropped unless its `trackId` is a track of the catalog, its counters
/// (`totalRaces`, `totalDistanceDriven` in metres, `totalTimeDriven` in seconds, the ProfileData
/// counters' units, `fh2-stats-extras.json`) are whole and not negative, its `topSpeed` (m/s) is
/// finite, not negative and at most [`game_speed_ceiling`] (no car: the game-wide bound), and its
/// average speed (distance / time) is at most that ceiling too (a row with no time keeps at most
/// [`TRACK_DISTANCE_WITHOUT_TIME`]: a counter's rounding, OD-178 follow-up).
pub fn keep_plausible_track_stats(
    catalog: &Catalog,
    rows: &mut serde_json::Map<String, Value>,
) -> usize {
    let ceiling = game_speed_ceiling(catalog);
    let before = rows.len();
    rows.retain(|_, row| {
        let n = |k: &str| row[k].as_i64().filter(|v| *v >= 0);
        let (Some(track), Some(_), Some(distance), Some(time)) = (
            row["trackId"].as_i64(),
            n("totalRaces"),
            n("totalDistanceDriven"),
            n("totalTimeDriven"),
        ) else {
            return false;
        };
        let top = row["topSpeed"].as_f64().unwrap_or(f64::NAN);
        catalog.track(track).is_some()
            && top.is_finite()
            && (0.0..=ceiling).contains(&top)
            && if time == 0 {
                distance <= TRACK_DISTANCE_WITHOUT_TIME
            } else {
                distance as f64 / time as f64 <= ceiling
            }
    });
    before - rows.len()
}

/// OD-178, until data bounds them: keeps the rows of a table whose numbers (every field but the
/// nested ones) are finite and not negative (`drivatarTrackStats`, `raceFeats`); returns how many
/// it dropped.
pub fn keep_non_negative(rows: &mut serde_json::Map<String, Value>) -> usize {
    fn ok(v: &Value) -> bool {
        match v {
            Value::Number(n) => n.as_f64().is_some_and(|f| f.is_finite() && f >= 0.0),
            Value::Object(o) => o.values().all(ok),
            Value::Array(a) => a.iter().all(ok),
            _ => true,
        }
    }
    let before = rows.len();
    rows.retain(|_, row| ok(row));
    before - rows.len()
}

#[cfg(test)]
mod od178_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn creations_need_a_car_the_game_can_have_and_never_a_traffic_car() {
        let c = Catalog::fh2();
        assert!(creation_car(c, 247).is_ok());
        assert!(creation_car(c, 2284).is_ok(), "a car pack's car");
        for bad in [-1, 0, MAX_CAR_ID + 1, 631, 1674] {
            assert_eq!(
                creation_car(c, bad).unwrap_err().rule,
                "creation_car",
                "{bad}"
            );
        }
    }

    #[test]
    fn a_scores_class_and_pi_must_agree_with_the_band_and_the_event() {
        let c = Catalog::fh2();
        // The class is the raw PI's band, whatever the car (upgraded or lowered): play's raw
        // values (A 0.686, S1 0.749, X exactly 1.0), and a band edge either way.
        for (class, pi) in [
            (3, 0.686),
            (4, 0.749),
            (6, 1.0),
            (0, 0.329861),
            (2, 0.578917),
            (2, 0.6179),
            (3, 0.6179),
            (3, 0.6179005),
        ] {
            assert!(score_class(c, class, pi, None).is_ok(), "{class} {pi}");
        }
        // Unknown class; PI not finite, 0, negative or over 1; a class that is not the band's.
        for (class, pi) in [
            (8, 0.5),
            (0, f64::NAN),
            (0, 0.0),
            (0, -0.2),
            (6, 1.0001),
            (6, 608.0),
            (3, 0.608),
            (2, 0.686),
            (5, 1.0),
            (3, 0.61789),
        ] {
            assert_eq!(
                score_class(c, class, pi, None).unwrap_err().rule,
                "race_time",
                "{class} {pi}"
            );
        }
        // A Monthly event of class B takes exactly B (T-PD-S6).
        assert!(score_class(c, 2, 0.608, Some(2)).is_ok());
        assert!(score_class(c, 1, 0.5, Some(2)).is_err(), "a lower class");
        assert!(score_class(c, 3, 0.686, Some(2)).is_err());
    }

    #[test]
    fn track_stats_need_a_known_track_and_plausible_numbers() {
        let c = Catalog::fh2();
        let ceiling = game_speed_ceiling(c);
        assert!((ceiling - 158.6702).abs() < 1e-3);
        let row = |track: i64, races: i64, dist: i64, time: i64, top: f64| {
            json!({"trackId": track, "totalRaces": races, "totalDistanceDriven": dist,
                "totalTimeDriven": time, "topSpeed": top})
        };
        let mut rows = serde_json::Map::new();
        for (k, r) in [
            ("a", row(0, 3, 10_800, 360, 70.0)),          // 30 m/s: kept
            ("b", row(0, 0, 0, 0, 0.0)),                  // nothing driven: kept
            ("c", row(1, 3, 10_800, 360, 70.0)),          // track 1 is not the catalog's
            ("d", row(0, 3, 10_800, 360, ceiling + 1.0)), // top speed over the ceiling
            ("e", row(0, 3, 100_000, 60, 70.0)),          // 1667 m/s on average
            ("f", row(0, -1, 0, 0, 0.0)),                 // negative
            ("g", row(0, 0, 1_001, 0, 0.0)),              // a kilometre with no time
            ("h", json!({"trackId": 0})),                 // numbers missing
            ("i", row(0, 0, 1_000, 0, 0.0)),              // a counter's rounding: kept
        ] {
            rows.insert(k.into(), r);
        }
        assert!(c.track(1).is_none());
        assert_eq!(keep_plausible_track_stats(c, &mut rows), 6);
        assert_eq!(rows.keys().collect::<Vec<_>>(), ["a", "b", "i"]);
    }

    #[test]
    fn unbounded_tables_keep_rows_without_negative_numbers() {
        let mut rows = serde_json::Map::new();
        rows.insert("1".into(), json!({"trackId": 0, "perfectTurns": 4}));
        rows.insert("2".into(), json!({"trackId": 0, "perfectTurns": -4}));
        rows.insert("3".into(), json!({"base": {"x": -1.0}, "trackId": 0}));
        rows.insert(
            "4".into(),
            json!({"base": {"x": 1.5}, "trackId": 2, "totalTimeDriven": 3.5}),
        );
        assert_eq!(keep_non_negative(&mut rows), 2);
        assert_eq!(rows.keys().collect::<Vec<_>>(), ["1", "4"]);
    }
}

#[cfg(test)]
mod score_tests {
    use super::*;

    #[test]
    fn known_pack_cars_are_held_to_their_own_top_speed() {
        let c = Catalog::fh2();
        let garage = serde_json::json!({ "garageCars": [{ "id": 1, "carId": 9631 }, { "id": 2, "carId": 9674 }] });
        assert!(check_garage_cars(c, &garage).is_ok());
        let track = c.tracks().find(|t| t.length_m > 0).expect("a track");
        for id in [9631, 9674, 255] {
            let own = c.car(id).expect("a pack car").top_speed_mps;
            assert_eq!(car_reference(c, id).unwrap(), own);
            assert!(own < catalog_fastest(c));
            let floor = track.length_m as f64 / (own * SPEED_MARGIN);
            assert!(race_time(c, id, track.id, floor + 1.0).is_ok(), "{id}");
            // Faster than the pack car can go, though within the game's fastest car's reach.
            let game_floor = track.length_m as f64 / (catalog_fastest(c) * SPEED_MARGIN);
            assert!(game_floor < floor - 1.0);
            assert_eq!(
                race_time(c, id, track.id, floor - 1.0).unwrap_err().rule,
                "race_time"
            );
        }
        // Outside the range and not a known pack car: still refused.
        assert!(race_time(c, 9632, track.id, 1e6).is_err());
    }

    #[test]
    fn pack_cars_keep_their_parts_and_their_scores_in_every_board() {
        let c = Catalog::fh2();
        let prices =
            crate::parts::PartPrices::from_json(r#"{"partPrices":{"1":[[2,100]]}}"#).unwrap();
        let garage: std::collections::BTreeMap<i64, i64> = [(1, 255), (2, 9674)].into();
        let mut rows = serde_json::Map::new();
        for (k, g) in [("a", 1), ("b", 2)] {
            rows.insert(
                k.into(),
                serde_json::json!({ "garageId": g, "partEnum": 999, "partId": 999_999, "pricePaid": 5 }),
            );
        }
        assert_eq!(keep_known_parts(c, &prices, &garage, &mut rows), 0);
        assert_eq!(rows.len(), 2);
        let track = c.tracks().find(|t| t.length_m > 0).expect("a track");
        for id in [255, 9674] {
            assert!(known_car(c, id) && creation_car(c, id).is_ok());
            // A slow time, a speed below the pack car's own reach, and points boards.
            assert!(score(c, 1, 1, id, track.id, 1e4).is_ok(), "time {id}");
            assert!(score(c, 1, 5, id, track.id, 60.0).is_ok(), "trap {id}");
            assert!(score(c, 1, 6, id, track.id, 60.0).is_ok(), "zone {id}");
            assert!(score(c, 2, 2, id, track.id, 50_000.0).is_ok(), "skill {id}");
            assert!(trap_ceiling(c, id) == game_speed_ceiling(c));
            assert!(fastest(c, &[id]).is_some());
        }
    }

    #[test]
    fn scores_are_held_to_the_car_or_the_game() {
        let c = Catalog::fh2();
        let fastest = catalog_fastest(c);
        assert!((fastest - 122.1).abs() < 0.1, "{fastest}");
        // Car ids: the catalog's, a car pack's within range, nothing else.
        assert_eq!(
            car_reference(c, 247).unwrap(),
            c.car(247).unwrap().top_speed_mps
        );
        assert_eq!(car_reference(c, 3000).unwrap(), fastest);
        for bad in [-1, 0, MAX_CAR_ID + 1, 99_999] {
            assert_eq!(
                car_reference(c, bad).unwrap_err().rule,
                "race_time",
                "{bad}"
            );
        }
        // Times: the car's own speed; a car pack's car the game's fastest.
        assert!(score(c, 1, 1, 247, 0, 90.0).is_ok());
        assert!(score(c, 1, 1, 247, 0, 30.0).is_err());
        assert!(score(c, 1, 1, 3000, 0, 0.001).is_err());
        assert!(score(c, 1, 1, 3000, 0, 30.0).is_ok());
        assert!(score(c, 1, 1, -1, 0, 90.0).is_err());
        // Speeds: car 247's row is `low`, so the game-wide ceiling (a tuned starter is not refused).
        assert!(score(c, 9, 5, 247, -1, 69.6).is_ok());
        assert!(
            score(c, 9, 5, 247, 0, 150.0).is_ok(),
            "1.3x the game's fastest, not the car's"
        );
        assert!(score(c, 9, 6, 247, -1, fastest * SPEED_MARGIN + 0.1).is_err());
        assert!(score(c, 9, 5, 3000, -1, 1e9).is_err());
        assert!(score(c, 9, 5, 5000, -1, 50.0).is_err(), "an impossible car");
        // Points: type 10's ceiling; a type the captures do not show has none of its own.
        assert!(score(c, 4, 10, 247, -1, 66.0).is_ok());
        assert!(score(c, 4, 10, 247, -1, 315.0).is_ok());
        assert!(score(c, 4, 10, 247, -1, 316.0).is_err());
        assert!(
            score(c, 4, 42, 247, -1, 1e8).is_ok(),
            "a type v2 does not list"
        );
        assert!(
            score(c, 2, 10, 247, -1, 1e8).is_ok(),
            "type 10 elsewhere: no ceiling"
        );
        assert_eq!(
            score(c, 4, 10, 247, -1, 1e9).unwrap_err().rule,
            STATS_CEILING_RULE
        );
    }

    #[test]
    fn a_card_keeps_the_games_badge_and_title_ids() {
        assert_eq!(card_ids(1, 1), (1, 1));
        assert_eq!(card_ids(0, 0), (1, 1), "0 is no table id");
        assert_eq!(card_ids(461, 9999), (1, 1));
        // An id listed as a badge but not as a title (377 badges, 329 titles).
        let ids = crate::card_ids::CardIds::fh2();
        let badge_only = (1..=460u16)
            .find(|i| ids.badge(*i) && !ids.title(*i))
            .unwrap();
        assert_eq!(card_ids(badge_only, badge_only), (badge_only, 1));
    }

    #[test]
    fn a_card_keeps_at_most_the_games_level() {
        assert_eq!(card_level(0), 0);
        assert_eq!(card_level(999), 999);
        assert_eq!(card_level(1000), 999);
        assert_eq!(card_level(u32::MAX), 999);
    }

    #[test]
    fn stats_boards_take_whole_numbers_within_the_games_totals() {
        let c = Catalog::fh2();
        for &(st, max) in ENFORCED_STATS_CEILINGS.iter() {
            assert!(
                score(c, 4, i64::from(st), 247, -1, max as f64).is_ok(),
                "4/{st}"
            );
            assert_eq!(
                score(c, 4, i64::from(st), 247, -1, max as f64 + 1.0)
                    .unwrap_err()
                    .rule,
                STATS_CEILING_RULE,
                "4/{st}"
            );
        }
        // The ceilings are the data's: v2's for 4/7..4/10 and 4/14, T-PD-S6's 63 for 4/12.
        let v2 = crate::stats_boards::StatsBoards::fh2();
        for &(st, max) in ENFORCED_STATS_CEILINGS
            .iter()
            .filter(|(t, _)| *t != 12 && *t != 7)
        {
            assert_eq!(v2.board(4, st).and_then(|b| b.max), Some(max), "4/{st}");
        }
        let confirmed: serde_json::Value =
            fi_gamedata::parse(crate::pi::GATE_CONFIRMATIONS_FILE, |t| {
                serde_json::from_str(t)
            })
            .unwrap();
        assert_eq!(confirmed["statsBoards"]["4/12"]["max"], 63);
        assert_eq!(confirmed["statsBoards"]["4/7"]["max"], 354);
        // 4/7: the confirmed base models plus every known car pack's car.
        assert_eq!(v2.board(4, 7).and_then(|b| b.max), Some(354));
        assert_eq!(ENFORCED_STATS_CEILINGS[0], (7, c.all_cars().count() as i64));
        assert!(score(c, 4, 7, 9674, -1, 400.0).is_ok());
        assert!(confirmed["statsBoards"]["4/15"]["max"].is_null());
        // Not enforced: 4/15 (no safe cap).
        assert!(score(c, 4, 15, 247, -1, 420.0).is_ok());
        // Head-to-head wins and skill chains: no maximum, whole numbers still.
        assert!(score(c, 4, 11, 247, -1, 5e8).is_ok());
        assert!(score(c, 4, 13, 247, -1, 9e8).is_ok());
        assert_eq!(
            score(c, 4, 13, 247, -1, 100.5).unwrap_err().rule,
            "race_time"
        );
        assert!(score(c, 4, 9, 247, -1, 2.5).is_err());
        assert!(score(c, 4, 9, 247, -1, f64::NAN).is_err());
        assert!(points(4, 9, -1.0).is_err());
        // Another board's points: only the boards' 1e9.
        assert!(points(2, 9, 2.5).is_ok());
    }

    #[test]
    fn speed_boards_hold_each_car_to_its_own_upgraded_bound() {
        let c = Catalog::fh2();
        let game = catalog_fastest(c) * SPEED_MARGIN;
        let kmh = |k: f64| k / 3.6;
        // Lexus GS 350 (1535, medium, 466 km/h): 466 x 1.3 = 605.8 km/h, over the game-wide
        // 571.2 km/h (122.054 m/s x 1.3), so the game-wide ceiling holds. 361 km/h is accepted.
        assert_eq!(
            crate::topspeed::TopSpeeds::fh2().get(1535).unwrap().max_kmh,
            466
        );
        assert!((trap_ceiling(c, 1535) - game).abs() < 1e-9);
        assert!(score(c, 9, 5, 1535, -1, kmh(361.0)).is_ok());
        // Ford Transit (631, high, 200 km/h): 200 x 1.3 = 260 km/h (72.2 m/s), the m/s the score
        // carries; 259 km/h passes, 261 km/h is refused on both boards.
        let transit = trap_ceiling(c, 631);
        assert!((transit - 200.0 / 3.6 * 1.3).abs() < 1e-9, "{transit}");
        assert!((transit * 3.6 - 260.0).abs() < 1e-9);
        assert!(score(c, 9, 5, 631, -1, kmh(259.0)).is_ok());
        let refused = score(c, 9, 6, 631, -1, kmh(261.0)).unwrap_err();
        assert_eq!(refused.rule, "race_time");
        assert!(
            refused.reason.contains("261 km/h") && refused.reason.contains("260 km/h"),
            "{}",
            refused.reason
        );
        // A car pack's car (3000) and the placeholder keep the game-wide ceiling.
        assert_eq!(trap_ceiling(c, 3000), game);
        assert_eq!(trap_ceiling(c, PLACEHOLDER_CAR), game);
        assert!(score(c, 9, 5, 3000, -1, game - 0.1).is_ok());
        assert!(score(c, 9, 5, 3000, -1, game + 0.1).is_err());
        // A `low` row keeps the game-wide ceiling too (the table's least sure bounds).
        let low = crate::topspeed::TopSpeeds::fh2().get(2101).unwrap();
        assert_eq!(low.confidence, crate::topspeed::Confidence::Low);
        assert_eq!(trap_ceiling(c, 2101), game);
        // Every listed car is held at or under the game-wide ceiling.
        for (car, _) in crate::topspeed::TopSpeeds::fh2().iter() {
            assert!(trap_ceiling(c, car) <= game + 1e-9, "{car}");
        }
    }
}
