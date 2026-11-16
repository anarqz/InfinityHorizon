//! The game's performance index (T-PD-S6, `fh2/gate-confirmations.json` "pi";
//! docs/web/PLAY_GATE_CONFIRMATIONS_METHOD.md; SAURON's reference `fh2_pi.py`, sha256
//! `ab1c8ec7…b1e0`). `carPerformanceIndex` and `Data_Car.PerformanceIndex` are a **raw** index,
//! 0 to 1 (the game clamps it there). The class is the first band whose raw maximum is not below it
//! ([`class_of`], `FUN_1410499b0`); the PI the game shows is linear inside the band between the
//! previous band's display maximum + 1 and the band's own ([`display_of`], `FUN_14104c8d0`): the
//! Toyota 2000GT's raw 0.329861 is D 404, the Ferrari 250 GTO's 0.578917 is B 658.
//!
//! The catalog's `Car::pi` is `round(raw × 1000)` (`tools/gamedata/extract_fh2.py`): three decimals.
//! Read against the bands it flips three cars at a band edge (1325, 1658 and 2254: e.g. a raw of
//! 0.6179x stored as 618, above B's 0.6179). A catalog car is therefore never classed from its
//! rounded raw: its class is the catalog's own `class_id` and [`display_in`] places the rounded raw
//! inside that class's band ([`catalog_class_agrees`] proves the two agree within the rounding).

use serde::Deserialize;

/// The game data file of the confirmations (`FI_GAME_DATA_DIR`; never in the crate).
pub const GATE_CONFIRMATIONS_FILE: &str = "fh2/gate-confirmations.json";

static BANDS: fi_gamedata::Lazy<Vec<Band>> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(GATE_CONFIRMATIONS_FILE, bands_from_json));

/// How far a submitted raw PI may sit across a band edge and still count for either class: a float
/// submission's noise. The game computes and sends the index as an f32, whose spacing near 1.0 is
/// 1.19e-7, and the band edges carry six decimals; 1e-6 covers both several times over while
/// staying a thousandth of one shown PI point (one point is about 0.001 of raw).
pub const SUBMIT_EPSILON: f64 = 1e-6;

/// Half a unit of the catalog's three-decimal raw (`Car::pi` = round(raw × 1000)).
pub const CATALOG_ROUNDING: f64 = 0.0005;

/// One class band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    /// `CarClasses.Id`.
    pub class_id: i64,
    /// `MaxPerformanceIndex`: the band's highest raw index.
    pub raw_max: f64,
    /// `MaxDisplayPerformanceIndex`: the band's highest shown PI.
    pub display_max: i64,
    /// Whether a car can be in it (R, above 1.0, cannot).
    pub reachable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBand {
    class_id: i64,
    raw_max: f64,
    display_max: i64,
    reachable: bool,
}

#[derive(Deserialize)]
struct RawPi {
    bands: Vec<RawBand>,
}

#[derive(Deserialize)]
struct Raw {
    pi: RawPi,
}

/// The bands, D to R in order: the game data's [`GATE_CONFIRMATIONS_FILE`] "pi", parsed once;
/// panics with the game data's error ([`try_bands`]).
pub fn bands() -> &'static [Band] {
    &BANDS
}

/// [`bands`], or why the game data has none.
pub fn try_bands() -> Result<&'static [Band], &'static fi_gamedata::Error> {
    BANDS.load().map(Vec::as_slice)
}

/// The bands of a confirmations JSON ("pi"); bands out of order are an error.
pub fn bands_from_json(text: &str) -> Result<Vec<Band>, String> {
    let raw: Raw = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let bands: Vec<Band> = raw
        .pi
        .bands
        .into_iter()
        .map(|b| Band {
            class_id: b.class_id,
            raw_max: b.raw_max,
            display_max: b.display_max,
            reachable: b.reachable,
        })
        .collect();
    if !bands
        .windows(2)
        .all(|w| w[0].raw_max < w[1].raw_max && w[0].class_id + 1 == w[1].class_id)
    {
        return Err("pi.bands are not in order".to_owned());
    }
    Ok(bands)
}

fn clamp(raw: f64) -> f64 {
    if raw.is_nan() {
        0.0
    } else {
        raw.clamp(0.0, 1.0)
    }
}

/// The class of a raw index (clamped to 0..=1): the first band with `raw <= raw_max`.
pub fn class_of(raw: f64) -> i64 {
    let r = clamp(raw);
    let b = bands();
    b.iter()
        .find(|band| r <= band.raw_max)
        .map_or(b[b.len() - 1].class_id, |band| band.class_id)
}

/// The classes a submitted raw index may stand for: [`class_of`] of it moved by
/// [`SUBMIT_EPSILON`] either way (one class, or the two at a band edge).
pub fn classes_near(raw: f64) -> (i64, i64) {
    (
        class_of(raw - SUBMIT_EPSILON),
        class_of(raw + SUBMIT_EPSILON),
    )
}

/// The PI the game shows for raw index `raw` inside class `class`'s band (the raw clamped to the
/// band first); `None` for a class without a reachable band.
pub fn display_in(class: i64, raw: f64) -> Option<i64> {
    let b = bands();
    let k = b.iter().position(|band| band.class_id == class)?;
    let band = b[k];
    if !band.reachable {
        return None;
    }
    let (raw_lo, disp_lo) = if k == 0 {
        (0.0, 100)
    } else {
        (b[k - 1].raw_max, b[k - 1].display_max + 1)
    };
    let hi = band.display_max + 1;
    let r = clamp(raw).clamp(raw_lo, band.raw_max);
    let v = ((r - raw_lo) / (band.raw_max - raw_lo) * (hi - disp_lo) as f64 + disp_lo as f64)
        .trunc() as i64;
    Some(v.clamp(disp_lo, hi - 1))
}

/// The PI the game shows for a raw index (`FUN_14104c8d0`): [`display_in`] its own
/// [`class_of`].
pub fn display_of(raw: f64) -> i64 {
    display_in(class_of(raw), raw).unwrap_or(999)
}

/// Whether a catalog car's stock class agrees with its three-decimal raw (`Car::pi`): some raw
/// within [`CATALOG_ROUNDING`] of it is in that class.
pub fn catalog_class_agrees(pi_thousandths: i64, class: i64) -> bool {
    let raw = pi_thousandths as f64 / 1000.0;
    let (lo, hi) = (
        class_of(raw - CATALOG_ROUNDING),
        class_of(raw + CATALOG_ROUNDING),
    );
    (lo..=hi).contains(&class)
}

/// A catalog car's shown PI: its three-decimal raw placed in its own stock class's band
/// ([`display_in`]), never re-classed across a band edge by the rounding.
pub fn catalog_display(car: &crate::Car) -> i64 {
    display_in(car.class_id, car.pi as f64 / 1000.0).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bands_are_sauron_s() {
        let b: Vec<(i64, f64, i64)> = bands()
            .iter()
            .map(|b| (b.class_id, b.raw_max, b.display_max))
            .collect();
        assert_eq!(
            b,
            [
                (0, 0.4345, 500),
                (1, 0.525628, 600),
                (2, 0.6179, 700),
                (3, 0.748432, 800),
                (4, 0.846368, 900),
                (5, 0.997086, 998),
                (6, 1.0, 999),
                (7, 1.1, 999),
            ]
        );
    }

    #[test]
    fn sauron_s_examples() {
        for (raw, class, shown) in [
            (0.329861, 0, 404), // Toyota 2000GT
            (0.578917, 2, 658), // Ferrari 250 GTO
            (0.368242, 0, 439),
            (0.001466, 0, 101),
            (0.996437, 5, 998),
        ] {
            assert_eq!((class_of(raw), display_of(raw)), (class, shown), "{raw}");
        }
        // Edges and clamps: the game clamps raw to 0..=1; X is 999; nothing reaches R.
        assert_eq!((class_of(1.0), display_of(1.0)), (6, 999));
        assert_eq!(class_of(1.2), 6);
        assert_eq!((class_of(-1.0), display_of(-1.0)), (0, 100));
        assert_eq!((class_of(0.4345), class_of(0.434501)), (0, 1));
        assert_eq!(display_of(0.4345), 500);
        assert_eq!(display_of(0.434501), 501);
        assert_eq!(display_of(0.748432), 800);
        assert_eq!(display_in(7, 1.0), None);
        // At a band edge both classes count for a submission.
        assert_eq!(classes_near(0.6179), (2, 3));
        assert_eq!(classes_near(0.6), (2, 2));
    }

    #[test]
    fn every_catalog_car_s_stock_class_agrees_with_its_raw() {
        let c = crate::Catalog::fh2();
        assert_eq!(c.cars().count(), 354);
        let mut rounded = Vec::new();
        for car in c.cars() {
            assert!(
                catalog_class_agrees(car.pi, car.class_id),
                "car {} pi {} class {}",
                car.id,
                car.pi,
                car.class_id
            );
            if class_of(car.pi as f64 / 1000.0) != car.class_id {
                rounded.push(car.id);
            }
            let shown = catalog_display(car);
            assert_eq!(
                display_in(car.class_id, car.pi as f64 / 1000.0),
                Some(shown)
            );
        }
        // Only the three cars T-PD-S6 names flip on the three-decimal rounding, and none is
        // re-classed by catalog_display.
        assert_eq!(rounded, [1325, 1658, 2254]);
        // The 2000GT's catalog raw (0.330) shows D 404; the 250 GTO's (0.579) B 658.
        assert_eq!(catalog_display(c.car(247).unwrap()), 404);
        assert_eq!(catalog_display(c.car(249).unwrap()), 658);
    }
}
