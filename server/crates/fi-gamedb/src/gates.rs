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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_garage_upload_keeps_the_cars_the_game_can_have() {
        let c = Catalog::fh2();
        let mut rows = serde_json::Map::new();
        for (key, car) in [
            ("1", json!(247)),
            ("2", json!(247)),
            ("3", json!(2284)),
            ("4", json!("x")),
            ("5", json!(1652)),
            ("7", json!(MAX_CAR_ID + 1)),
            ("8", json!(0)),
        ] {
            rows.insert(key.into(), json!({"id": key, "carId": car}));
        }
        rows.insert("6".into(), json!({"id": 6}));
        assert_eq!(
            keep_known_cars(c, &mut rows),
            4,
            "a text id, no car, an id past the game's, 0"
        );
        let kept: Vec<&str> = rows.keys().map(String::as_str).collect();
        assert_eq!(
            kept,
            vec!["1", "2", "3", "5"],
            "the same car twice and a car pack's car stay"
        );
        let mut empty = serde_json::Map::new();
        assert_eq!(keep_known_cars(c, &mut empty), 0);
        assert!(known_car(c, 2284) && known_car(c, 247) && !known_car(c, 99_999));
        assert_eq!(MAX_GARAGE_KEYS, 1_000);
    }

    #[test]
    fn the_shown_level_is_the_stored_one_plus_one_up_to_999() {
        assert_eq!(shown_level(0), None);
        assert_eq!(shown_level(-5), None);
        assert_eq!(shown_level(1), Some(1));
        assert_eq!(shown_level(223_773), Some(15));
        assert_eq!(shown_level(128_412_500), Some(999));
        assert_eq!(shown_level(538_207), Some(driver_level(538_207) + 1));
        assert_eq!(shown_level(i64::MAX), Some(999));
    }
    use fi_guard::{CeilingCode, ReasonCode};
    use serde_json::json;

    const HOUR: i64 = 3_600_000;
    const MINUTE: i64 = 60_000;

    /// Numbers whose level is the one FH2's curve gives their XP, reported as in Yellow (the
    /// in-band step equal to the driver level).
    fn p(credits: i64, xp: i64, winnings: i64) -> Progress {
        let level = crate::rules::LEVEL_CURVE.level_of(xp);
        Progress {
            credits,
            xp,
            level,
            band_level: level,
            winnings,
            top_speed: 0.0,
        }
    }

    /// What [`progress`] reads from an UpdateAll reporting `xp` and the in-band `level`.
    fn reported(credits: i64, xp: i64, level: i64, winnings: i64) -> Progress {
        progress(&json!({
            "playerHistory": [{ "credits": credits }],
            "playerRaceHistory": [{ "xp": xp, "level": level, "totalWinnings": winnings }],
        }))
        .unwrap()
    }

    /// A baseline of `p` accepted at `at`, no recent growth (a seed's).
    fn seeded(p: Progress, at: i64) -> Baseline {
        Baseline {
            credits: p.credits,
            xp: p.xp,
            level: p.level,
            winnings: p.winnings,
            synced_ms: at,
            rates: Rates::default(),
            level_high: None,
            judged: Judged::default(),
        }
    }

    /// The baseline an accepted `next` at `now` leaves (panics on a ceiling).
    fn accept(b: &Baseline, next: Progress, now: i64) -> (Baseline, Assessment) {
        let a = assess_profile(Some(b), &next, Sections::ALL, now);
        assert_eq!(a.ceiling, None, "{:?}", a.reasons);
        (
            Baseline {
                rates: a.rates,
                level_high: a.level_high,
                ..seeded(next, now)
            },
            a,
        )
    }

    #[test]
    fn an_unseeded_maxed_career_is_refused_from_the_seed() {
        // Review E1: 900M credits (and a maxed level) over the seed's numbers, in shadow too.
        let now = 10 * HOUR;
        let seed = seeded(p(25_000, 0, 0), now - MINUTE);
        let maxed = Progress {
            credits: 900_000_000,
            xp: 199_000_000,
            level: driver_level(199_000_000),
            band_level: 900,
            winnings: 900_000_000,
            top_speed: 50.0,
        };
        let a = assess_profile(Some(&seed), &maxed, Sections::ALL, now);
        assert_eq!(a.ceiling.map(|c| c.code), Some(CeilingCode::Credits));
        let r = Refusal::from(a.refusal(false).expect("refused with the switch off"));
        assert_eq!(r.rule, "credits_ceiling");
        assert!(!r.reason.is_empty());
        // Measured without a baseline (none should exist for Hardcore): from zero, refused too.
        assert!(
            assess_profile(None, &maxed, Sections::ALL, now)
                .refusal(false)
                .is_some()
        );
    }

    #[test]
    fn saved_spins_and_championship_play_are_accepted() {
        let t0 = 10 * HOUR;
        let base = seeded(p(2_000_000, 1_000_000, 10_000_000), t0);
        // ~900k of saved spins five minutes later, no level gained, no winnings: scored, accepted
        // whatever the switch (the Beta's credits bucket of 540k refused it).
        let burst = p(2_900_000, 1_000_000, 10_000_000);
        let a = assess_profile(Some(&base), &burst, Sections::ALL, t0 + 5 * MINUTE);
        assert_eq!(a.reasons, vec![ReasonCode::CreditsWithoutWinnings]);
        assert!(a.score < fi_guard::BLOCK);
        assert_eq!((a.refusal(false), a.refusal(true)), (None, None));
        // Two hours of championships at 1.5M/h (credits and winnings together, 20k XP a race),
        // a sync every five minutes: every one accepted with score 0.
        let mut b = base;
        for step in 1..=24 {
            let next = p(
                2_000_000 + step * 125_000,
                1_000_000 + step * 20_000,
                10_000_000 + step * 125_000,
            );
            let (moved, a) = accept(&b, next, t0 + step * 5 * MINUTE);
            assert_eq!(a.score, 0, "step {step}: {:?}", a.reasons);
            assert_eq!(a.refusal(true), None);
            b = moved;
        }
        assert!(b.rates.credits > 1_000_000.0 && b.rates.credits < 1_500_000.0);
    }

    #[test]
    fn back_to_back_syncs_share_one_rate() {
        // The cadence hole of the Beta's per-sync allowance: 40 syncs of +540k within a second.
        // All accepted (no ceiling), but the last ones would block: the rate adds them up.
        let t0 = 10 * HOUR;
        let mut b = seeded(p(1_000_000, 0, 0), t0);
        let mut last = None;
        for i in 1..=40 {
            let (moved, a) = accept(&b, p(b.credits + 540_000, 0, 0), t0 + i * 20);
            b = moved;
            last = Some(a);
        }
        let last = last.unwrap();
        assert!(last.reasons.contains(&ReasonCode::CreditsRate10To25M));
        assert!(last.would_block());
        // +2M a minute without winnings: refused once the hour's rate passes 25M.
        let mut b = seeded(p(1_000_000, 0, 0), t0);
        let mut accepted = 0;
        for i in 1..=40 {
            let a = assess_profile(
                Some(&b),
                &p(b.credits + 2_000_000, 0, 0),
                Sections::ALL,
                t0 + i * MINUTE,
            );
            if let Some(c) = a.ceiling {
                assert_eq!(c.code, CeilingCode::CreditsUnexplained);
                break;
            }
            b = accept(&b, p(b.credits + 2_000_000, 0, 0), t0 + i * MINUTE).0;
            accepted += 1;
        }
        assert!((12..=15).contains(&accepted), "{accepted}");
    }

    #[test]
    fn the_reported_level_is_the_in_band_step_and_never_trusted() {
        // CR-S81 (SAURON 2026-10-31, decoded UpdateAll bodies, numbers only): the game reports 0
        // at 228,874 XP, 1 at 230,494 (just past the 230,000 step) and 1 at 232,194, where the
        // driver level is 14, 15, 15. The baseline before them is a pre-fix row: the seed's
        // in-band `Main/Level` 0 at 223,773 XP, with an in-band high-water mark of 0.
        let t0 = 10 * HOUR;
        let mut b = Baseline {
            credits: 500_000,
            xp: 223_773,
            level: 0,
            winnings: 400_000,
            synced_ms: t0,
            rates: Rates::default(),
            level_high: Some(0),
            judged: Judged::default(),
        };
        let mut highs = Vec::new();
        for (i, (xp, band, driver)) in [(228_874, 0, 14), (230_494, 1, 15), (232_194, 1, 15)]
            .into_iter()
            .enumerate()
        {
            let r = reported(
                500_000 + i as i64 * 2_000,
                xp,
                band,
                400_000 + i as i64 * 2_000,
            );
            assert_eq!((r.level, r.band_level), (driver, band), "{xp}");
            let now = t0 + (i as i64 + 1) * 5 * MINUTE;
            let (next, a) = accept(&b, r, now);
            assert_eq!(
                (a.score, a.reasons.clone()),
                (0, vec![]),
                "{xp}: {:?}",
                a.reasons
            );
            assert!(
                a.deltas.levels.is_some_and(|l| (0..=1).contains(&l)),
                "{xp}"
            );
            highs.push(a.level_high);
            b = next;
        }
        assert_eq!(highs, [Some(14), Some(15), Some(15)]);
        assert_eq!(b.level, 15, "the baseline stores the driver level");

        // A band change: reported 13 then 0 (Yellow's last step, then Green's first) is driver 13
        // then 14: a level gained, never a drop, and one spin's credits explained.
        let yellow = seeded(reported(100_000, 205_000, 13, 50_000), t0);
        assert_eq!(yellow.level, 13);
        let green = reported(150_000, 211_000, 0, 51_000);
        assert_eq!(green.level, 14);
        let a = assess_profile(Some(&yellow), &green, Sections::ALL, t0 + 5 * MINUTE);
        assert_eq!(a.reasons, vec![], "{:?}", a.reasons);
        assert_eq!(a.deltas.levels, Some(1));
        assert_eq!(a.level_high, Some(14));
        // A pre-fix row holding the in-band 0 at 212,000 XP (driver 14): re-derived from the stored
        // XP, the next sync in Green is no level gained (read as stored it would be +14).
        let stale = Baseline {
            level: 0,
            ..seeded(reported(100_000, 212_000, 0, 50_000), t0)
        };
        let a = assess_profile(
            Some(&stale),
            &reported(110_000, 214_000, 0, 52_000),
            Sections::ALL,
            t0 + 5 * MINUTE,
        );
        assert_eq!((a.score, a.deltas.levels), (0, Some(0)), "{:?}", a.reasons);

        // An edited reported level (anything up to the game's limit) changes nothing the server
        // measures: no LEVEL_XP_MISMATCH, no level rate, no spins.
        let base = seeded(p(100_000, 100_717, 0), t0);
        let edited = reported(100_000, 100_717, 60, 0);
        assert_eq!((edited.level, edited.band_level), (6, 60));
        let a = assess_profile(Some(&base), &edited, Sections::ALL, t0 + HOUR);
        assert_eq!((a.score, a.reasons.clone()), (0, vec![]));
        // Beyond the game's limit it is still refused by the limits.
        let over = json!({ "playerRaceHistory": [{ "xp": 0, "level": 1_000 }] });
        assert_eq!(progress(&over).unwrap_err().rule, "limits");
    }

    #[test]
    fn a_real_xp_drop_still_scores_and_levels_rate_on_driver_levels() {
        let t0 = 10 * HOUR;
        // A Green career (driver 20) brought back to an older save (driver 15): XP_LEVEL_DROP,
        // whatever in-band steps the game reports (6 then 1).
        let base = seeded(reported(500_000, 330_000, 6, 400_000), t0);
        assert_eq!(base.level, 20);
        let older = reported(500_000, 232_194, 1, 400_000);
        let a = assess_profile(Some(&base), &older, Sections::ALL, t0 + 5 * MINUTE);
        assert_eq!(a.reasons, vec![ReasonCode::XpLevelDrop]);
        assert_eq!(a.deltas.levels, Some(-5));
        assert_eq!(
            a.judged.drop_floor.map(|f| (f.xp, f.level)),
            Some((232_194, 15))
        );
        // A judged floor stored before the fix with an in-band level (read on the driver scale,
        // though a state at or above the floor's XP clears either way): the same drop is not
        // scored again, a deeper one is.
        let judged = Baseline {
            judged: Judged {
                drop_floor: Some(Floor {
                    xp: 232_194,
                    level: 1,
                }),
                ..Judged::default()
            },
            ..base
        };
        let again = assess_profile(Some(&judged), &older, Sections::ALL, t0 + 5 * MINUTE);
        assert_eq!(again.reasons, vec![]);
        let deeper = reported(500_000, 215_000, 0, 400_000);
        let a = assess_profile(Some(&judged), &deeper, Sections::ALL, t0 + 5 * MINUTE);
        assert_eq!(a.reasons, vec![ReasonCode::XpLevelDrop]);

        // Levels per hour count driver levels: 800k XP in five minutes from Green's start is 40
        // driver levels (14 -> 54; reported 0 -> 10, which would read as 10). With the one-hour
        // time constant that is about 38 levels/h: LEVELS_RATE_HIGH (above 32/h) without
        // XP_RATE_HIGH (about 770k/h, under 1M/h).
        let base = seeded(reported(100_000, 210_000, 0, 50_000), t0);
        let fast = reported(100_000, 1_010_000, 10, 50_000);
        assert_eq!(fast.level, 54);
        let a = assess_profile(Some(&base), &fast, Sections::ALL, t0 + 5 * MINUTE);
        assert_eq!(a.deltas.levels, Some(40));
        assert_eq!(a.ceiling, None);
        assert!(a.reasons.contains(&ReasonCode::LevelsRateHigh), "{a:?}");
        assert!(!a.reasons.contains(&ReasonCode::XpRateHigh), "{a:?}");
    }

    #[test]
    fn a_garage_only_scores() {
        let now = 10 * HOUR;
        let g = GarageBaseline {
            cars: 1,
            synced_ms: now - MINUTE,
            rate: 0.0,
        };
        // 60 new cars at once (the Beta refused more than 6): a score, never a refusal.
        let a = assess_garage(Some(&g), 61, now);
        assert_eq!(a.reasons, vec![ReasonCode::CarsRateHigh]);
        assert_eq!((a.ceiling, a.refusal(false)), (None, None));
        // A few cars: nothing; a car sold: nothing.
        assert!(assess_garage(Some(&g), 4, now).reasons.is_empty());
        assert!(assess_garage(Some(&g), 0, now).reasons.is_empty());
        // No baseline: from an empty garage.
        assert_eq!(assess_garage(None, 3, now).deltas.cars, Some(3));
    }

    #[test]
    fn an_hourly_sync_under_the_fast_ceilings_meets_a_day_ceiling() {
        // Review G2-1: ~49M an hour (24M unexplained credits and 25M of winnings), one sync a
        // wall hour, passed the fast ceilings for ever; a day ceiling refuses it within hours.
        let t0 = 10 * HOUR;
        let mut b = seeded(p(1_000_000, 0, 0), t0);
        let mut refused = None;
        for hour in 1..=24 {
            let next = p(b.credits + 48_900_000, 0, b.winnings + 24_900_000);
            let a = assess_profile(Some(&b), &next, Sections::ALL, t0 + hour * HOUR);
            if let Some(c) = a.ceiling {
                refused = Some((hour, c.code));
                break;
            }
            b = accept(&b, next, t0 + hour * HOUR).0;
        }
        let (hour, code) = refused.expect("refused within a day");
        assert!(
            matches!(code, CeilingCode::DayUnexplained | CeilingCode::DayWinnings),
            "{code:?}"
        );
        assert!(hour <= 9, "{hour}");
    }

    #[test]
    fn a_section_not_sent_yet_is_unchanged() {
        // A merged profile without `playerRaceHistory` reads XP, level and winnings as 0: measured
        // as the baseline's, never as a drop (XP_LEVEL_DROP) of the seeded numbers.
        let now = 10 * HOUR;
        let base = seeded(p(25_000, 100_717, 0), now - MINUTE);
        let partial = Progress {
            credits: 30_000,
            xp: 0,
            level: 0,
            band_level: 0,
            winnings: 0,
            top_speed: 0.0,
        };
        let sections = Sections::of(&json!({"playerHistory": [{"credits": 30_000}]}));
        assert_eq!(
            sections,
            Sections {
                history: true,
                race: false
            }
        );
        let a = assess_profile(Some(&base), &partial, sections, now);
        assert!(a.reasons.is_empty(), "{:?}", a.reasons);
        assert_eq!(a.deltas.credits, Some(5_000));
        assert_eq!(a.deltas.xp, Some(0));
        let a = assess_profile(Some(&base), &partial, Sections::ALL, now);
        assert!(a.reasons.contains(&ReasonCode::XpLevelDrop));
    }

    #[test]
    fn a_level_lowered_and_gained_back_earns_no_spin_on_the_sync() {
        // Review G2-3: level 6 accepted, then 1 (an older save; scored), then 6 again with 2.5M
        // credits and no winnings: five level-ups would have explained it as spins.
        let t0 = 10 * HOUR;
        let base = seeded(p(1_000_000, 100_717, 0), t0);
        let (low, a) = accept(&base, p(1_000_000, 19_706, 0), t0 + MINUTE);
        assert_eq!(a.reasons, vec![ReasonCode::XpLevelDrop]);
        assert_eq!(low.level_high, Some(6));
        let a = assess_profile(
            Some(&low),
            &p(3_500_000, 100_717, 0),
            Sections::ALL,
            t0 + 2 * MINUTE,
        );
        assert_eq!(a.deltas.unexplained, Some(2_500_000));
        assert!(a.reasons.contains(&ReasonCode::CreditsWithoutWinnings));
    }

    #[test]
    fn time_is_capped_and_never_negative() {
        let now = 400 * 24 * HOUR;
        assert_eq!(hours_since(0, now), fi_guard::MAX_HOURS);
        assert_eq!(hours_since(now, 0), 0.0);
        assert_eq!(hours_since(now - HOUR, now), 1.0);
        // A baseline from the future (clock skew) is measured at once.
        let future = seeded(p(0, 0, 0), now + HOUR);
        let a = assess_profile(Some(&future), &p(500_000, 0, 0), Sections::ALL, now);
        assert_eq!(a.hours, 0.0);
        // Waiting earns nothing beyond an hour of the fast rates: a day of silence counts one
        // hour there, so +200M then is still over the credits ceiling and +45M over the
        // unexplained one (the slow rates see the whole day).
        assert_eq!(SYNC_MAX_HOURS, 1.0);
        let day = seeded(p(0, 0, 0), now - 24 * HOUR);
        let a = assess_profile(Some(&day), &p(200_000_000, 0, 0), Sections::ALL, now);
        assert_eq!(a.hours, 24.0);
        assert_eq!(a.ceiling.map(|c| c.code), Some(CeilingCode::Credits));
        let a = assess_profile(Some(&day), &p(45_000_000, 0, 0), Sections::ALL, now);
        assert_eq!(
            a.ceiling.map(|c| c.code),
            Some(CeilingCode::CreditsUnexplained)
        );
        let a = assess_profile(Some(&day), &p(30_000_000, 0, 0), Sections::ALL, now);
        assert_eq!(a.ceiling, None, "{a:?}");
    }

    #[test]
    fn numbers_and_limits() {
        let bad = json!({ "playerHistory": [{ "credits": -5 }], "playerRaceHistory": [{}] });
        assert_eq!(progress(&bad).unwrap_err().rule, "numbers");
        assert_eq!(progress_numbers(&bad).unwrap_err().rule, "numbers");
        let huge = json!({ "playerHistory": [{ "credits": 5_000_000_000u64 }], "playerRaceHistory": [{}] });
        assert_eq!(progress(&huge).unwrap_err().rule, "limits");
        // A Noob profile: numbers only, no limit.
        assert_eq!(progress_numbers(&huge).unwrap().credits, 5_000_000_000);
        for (field, section, value) in [
            ("xp", "playerRaceHistory", MAX_XP),
            ("level", "playerRaceHistory", MAX_LEVEL),
            ("totalWinnings", "playerRaceHistory", MAX_CREDITS),
            ("credits", "playerHistory", MAX_CREDITS),
        ] {
            let mut at = json!({ "playerHistory": [{}], "playerRaceHistory": [{}] });
            at[section][0][field] = json!(value);
            assert!(progress(&at).is_ok(), "{field} at the limit");
            at[section][0][field] = json!(value + 1);
            assert_eq!(progress(&at).unwrap_err().rule, "limits", "{field}");
        }
        // Top speed: negative is unsound; not a finite number (the codec writes NaN and the
        // infinities as strings) is an absent record, never a refusal.
        let v = json!({ "playerRaceHistory": [{ "topSpeed": -1.0 }] });
        assert_eq!(progress_numbers(&v).unwrap_err().rule, "numbers");
        for speed in [
            json!("NaN"),
            json!("Infinity"),
            json!("-Infinity"),
            json!(null),
        ] {
            let v = json!({ "playerRaceHistory": [{ "topSpeed": speed, "xp": 7 }] });
            let p = progress(&v).unwrap();
            assert_eq!((p.top_speed, p.xp), (0.0, 7), "{speed}");
        }
        let full = json!({
            "playerHistory": [{ "credits": 1500 }],
            "playerRaceHistory": [{ "xp": 300, "level": 4, "totalWinnings": 9000, "topSpeed": 81.5 }],
        });
        assert_eq!(
            progress(&full).unwrap(),
            Progress {
                credits: 1500,
                xp: 300,
                level: 0,
                band_level: 4,
                winnings: 9000,
                top_speed: 81.5
            }
        );
        assert_eq!(progress(&json!({})).unwrap(), p(0, 0, 0));
    }

    #[test]
    fn only_a_complete_profile_seeds() {
        assert!(complete(
            &json!({ "playerHistory": [{}], "playerRaceHistory": [{}] })
        ));
        assert!(!complete(
            &json!({ "playerHistory": [], "playerRaceHistory": [{}] })
        ));
        assert!(!complete(&json!({ "playerHistory": [{}] })));
        assert!(!complete(&json!({})));
    }

    fn small() -> Catalog {
        Catalog::from_json(
            r#"{"source":{"sha256":"00"},
                "cars":[
                  {"id":1,"year":2000,"makeId":1,"make":"Make","model":"Slow","mediaName":"MK_Slow_00","classId":0,"pi":300,"topSpeedMps":50.0},
                  {"id":2,"year":2000,"makeId":1,"make":"Make","model":"Fast","mediaName":"MK_Fast_00","classId":5,"pi":900,"topSpeedMps":100.0}],
                "classes":[], "buckets":[], "tracks":[]}"#,
        )
        .unwrap()
    }

    #[test]
    fn top_speed_needs_every_car_known() {
        let c = small();
        assert_eq!(fastest(&c, &[1, 2]), Some(100.0));
        assert_eq!(fastest(&c, &[1]), Some(50.0));
        // No car, or a car-pack car among them: no reference.
        assert_eq!(fastest(&c, &[]), None);
        assert_eq!(fastest(&c, &[1, 3000]), None);
        assert!(check_top_speed(&c, &[1], 64.0).is_ok());
        let r = check_top_speed(&c, &[1], 66.0).unwrap_err();
        assert_eq!(r.rule, "top_speed");
        assert!(check_top_speed(&c, &[1, 2], 129.0).is_ok());
        assert!(check_top_speed(&c, &[1, 3000], 500.0).is_ok());
        assert!(check_top_speed(&c, &[], 500.0).is_ok());
        // The real catalog: the 2000GT's ~65 m/s.
        assert!(check_top_speed(Catalog::fh2(), &[247], 80.0).is_ok());
        assert!(check_top_speed(Catalog::fh2(), &[247], 90.0).is_err());
    }

    fn garage(ids: &[i64]) -> Value {
        json!({ "garageCars": ids.iter().map(|c| json!({ "id": c, "carId": c })).collect::<Vec<_>>() })
    }

    #[test]
    fn garage_cars_are_cars_the_game_can_have() {
        let empty = Catalog::default();
        // A car pack's cars (the catalog does not list them), within the game's ids.
        assert!(check_garage_cars(&empty, &garage(&[2284, 2546, 1, MAX_CAR_ID])).is_ok());
        assert!(check_garage_cars(Catalog::fh2(), &garage(&[247, 289, 461, 1218])).is_ok());
        for bad in [99_999, 0, -1, MAX_CAR_ID + 1] {
            let r = check_garage_cars(&empty, &garage(&[289, bad])).unwrap_err();
            assert_eq!(r.rule, "invalid_car", "{bad}");
        }
        let not_a_number = json!({ "garageCars": [{ "id": 1, "carId": "289" }] });
        assert!(check_garage_cars(&empty, &not_a_number).is_err());
        let missing = json!({ "garageCars": [{ "id": 1 }] });
        assert!(check_garage_cars(&empty, &missing).is_err());
        // No garage in the upload, or an empty one.
        assert!(check_garage_cars(&empty, &json!({ "trackStats": [] })).is_ok());
        assert!(check_garage_cars(&empty, &garage(&[])).is_ok());
    }

    /// A captured player as the gates replay them.
    #[derive(Default)]
    struct Replayed {
        profile: serde_json::Map<String, Value>,
        baseline: Option<Baseline>,
        top_speed: f64,
        /// Garage rows by key: their car ids.
        garage: std::collections::BTreeMap<String, i64>,
        garage_baseline: Option<GarageBaseline>,
    }

    /// The Beta's captured profile syncs (`server/captures/*.jsonl`, outside the repository:
    /// real XUIDs) through these gates, player by player (the captured `X-UserId`, a grouping
    /// key only) on the captured clock, Hardcore rules for every player: the profile merged as
    /// `fi_db::gameplane::merge_profile_in` merges it, the first complete sync taken as the seed
    /// (no rates), the garage by its size after each upload that carries one. Counts what refuses
    /// (ceilings and the technical rules), the guardrails' score bands and every reason code, so
    /// the shadow calibration and the curve's level scale can be checked on real syncs. Reads the
    /// captures' decoded `request_xtsw` (the Beta's decoder over the same schema) and prints counts
    /// only, never values:
    /// `FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-gamedb captured -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the Beta's captures from FI_FH2_CAPTURES_DIR (outside the repository)"]
    fn captured_syncs_pass_the_gates() {
        let dir = std::env::var("FI_FH2_CAPTURES_DIR")
            .expect("set FI_FH2_CAPTURES_DIR to a directory of Beta capture .jsonl files");
        let catalog = Catalog::fh2();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read FI_FH2_CAPTURES_DIR")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        files.sort();
        let mut syncs: Vec<(String, i64, i64, String, Value)> = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read a capture file");
            for line in text.lines() {
                let Ok(r) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let method = r["forza_method"].as_str().unwrap_or_default().to_owned();
                if !r["forza_class"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("Forza.WebServices.OnlineProfile"))
                    || !matches!(method.as_str(), "UpdateAll" | "UpdateAllTables")
                {
                    continue;
                }
                let player = r["request_headers"]
                    .as_object()
                    .and_then(|h| {
                        h.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("x-userid"))
                            .map(|(_, v)| v.to_string())
                    })
                    .unwrap_or_default();
                let time = r["time_ms"].as_i64().unwrap_or(0);
                let seq = r["seq"].as_i64().unwrap_or(0);
                syncs.push((player, time, seq, method, r["request_xtsw"].clone()));
            }
        }
        syncs.sort_by(|a, b| (&a.0, a.1, a.2).cmp(&(&b.0, b.1, b.2)));
        let mut players: std::collections::BTreeMap<String, Replayed> = Default::default();
        let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
        let mut count = |k: String| *tally.entry(k).or_default() += 1;
        let band = |a: &Assessment| match a.score {
            s if s >= fi_guard::BLOCK => "would block",
            s if s >= fi_guard::REVIEW => "review",
            s if s >= fi_guard::OBSERVE => "observe",
            _ => "ok",
        };
        for (player, now, _, method, params) in syncs {
            let s = players.entry(player).or_default();
            count(format!("{method} replayed"));
            if method == "UpdateAll" {
                let mut merged = s.profile.clone();
                for (k, v) in params.as_object().into_iter().flatten() {
                    if !v.as_array().is_some_and(Vec::is_empty) || !merged.contains_key(k) {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                let merged = Value::Object(merged);
                let p = match progress(&merged) {
                    Ok(p) => p,
                    Err(r) => {
                        count(format!("UpdateAll refused {}", r.rule));
                        continue;
                    }
                };
                let assessed = s
                    .baseline
                    .as_ref()
                    .map(|b| assess_profile(Some(b), &p, Sections::of(&merged), now));
                if let Some(a) = &assessed {
                    for r in &a.reasons {
                        count(format!("UpdateAll reason {r}"));
                    }
                    if let Some(r) = a.refusal(false) {
                        count(format!("UpdateAll refused {}", r.rule()));
                        continue;
                    }
                    count(format!("UpdateAll accepted, {}", band(a)));
                } else {
                    count("UpdateAll accepted as the seed".into());
                }
                if p.top_speed > s.top_speed + TOP_SPEED_RECORD_STEP {
                    let cars: std::collections::BTreeSet<i64> =
                        s.garage.values().copied().collect();
                    let cars: Vec<i64> = cars.into_iter().collect();
                    if check_top_speed(catalog, &cars, p.top_speed).is_err() {
                        count("UpdateAll top_speed signal".into());
                    }
                }
                if complete(&merged) {
                    s.baseline = Some(Baseline {
                        rates: assessed.as_ref().map(|a| a.rates).unwrap_or_default(),
                        level_high: assessed.and_then(|a| a.level_high),
                        ..seeded(p, now)
                    });
                    s.top_speed = p.top_speed;
                } else {
                    count("UpdateAll accepted without seeding".into());
                }
                if let Value::Object(m) = merged {
                    s.profile = m;
                }
                continue;
            }
            let rows: Vec<(String, i64)> = params["garageCars"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| (r["id"].to_string(), r["carId"].as_i64().unwrap_or(-1)))
                .collect();
            let deletes: Vec<String> = params["deleteGarageCars"]
                .as_array()
                .into_iter()
                .flatten()
                .map(Value::to_string)
                .collect();
            // Only an upload that carries a garage is gated or moves the garage's baseline.
            if rows.is_empty() && deletes.is_empty() {
                count("UpdateAllTables without a garage".into());
                continue;
            }
            if let Err(r) = check_garage_cars(catalog, &params) {
                count(format!("UpdateAllTables refused {}", r.rule));
                continue;
            }
            let mut after = s.garage.clone();
            after.extend(rows.iter().cloned());
            for k in &deletes {
                after.remove(k);
            }
            let size = after.len() as i64;
            let rate = match &s.garage_baseline {
                Some(g) => {
                    let a = assess_garage(Some(g), size, now);
                    for r in &a.reasons {
                        count(format!("UpdateAllTables reason {r}"));
                    }
                    count(format!("UpdateAllTables accepted, {}", band(&a)));
                    a.rates.cars
                }
                None => {
                    count("UpdateAllTables accepted as the seed".into());
                    0.0
                }
            };
            s.garage_baseline = Some(GarageBaseline {
                cars: size,
                synced_ms: now,
                rate,
            });
            s.garage = after;
        }
        println!("captured profile syncs of {} players:", players.len());
        for (k, n) in &tally {
            println!("  {k}: {n}");
        }
        assert!(
            tally.get("UpdateAll replayed").copied().unwrap_or(0) > 0,
            "no UpdateAll in the captures"
        );
    }
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
