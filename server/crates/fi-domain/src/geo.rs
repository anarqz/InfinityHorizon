//! "Close to me" (OD-54; docs/contracts/MATCHMAKING_API.md §3.5): a coarse location and a
//! round-trip estimate between two players, from nothing finer than a 1°×1° cell.
//!
//! The edge tells the platform where a request comes from (Cloudflare's visitor location
//! headers); the service reduces it at once to a [`cell`] (about 111 km) and a [`region`] label
//! and keeps neither the coordinates nor the address. Two cells give a great-circle distance and
//! an estimated round trip of `10 ms + 0.015 ms/km` (fibre at about 200 km/ms with a 1.5× path
//! inflation), [`Estimate`]: the class (`local` ≤ 30 ms, `near` ≤ 60 ms, `far` ≤ 120 ms,
//! `remote` beyond) and the milliseconds rounded to 10. Two players on the same public address
//! are `local` (10 ms) whatever their location; without a location on either side the estimate
//! is `unknown`, and an unknown estimate never refuses anyone ([`Estimate::within`]).

/// Cells: 180 rows of latitude by 360 columns of longitude.
pub const CELLS: i32 = 180 * 360;
/// The estimate's fixed part (ms).
pub const RTT_BASE_MS: f64 = 10.0;
/// The estimate's part per kilometre (ms).
pub const RTT_MS_PER_KM: f64 = 0.015;
/// The latency limits a host may choose (OD-59: under 60, 100 or 150 ms).
pub const MAX_RTT_STEPS: [u32; 3] = [60, 100, 150];

const EARTH_KM: f64 = 6371.0;

/// The 1°×1° cell of a latitude and longitude in degrees (`(lat + 90) * 360 + (lon + 180)`,
/// 0 to 64799); `None` outside the globe or for a non-number.
pub fn cell(lat: f64, lon: f64) -> Option<i32> {
    if !(lat.is_finite() && lon.is_finite()) || !(-90.0..=90.0).contains(&lat) {
        return None;
    }
    if !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let row = ((lat + 90.0).floor() as i32).min(179);
    let col = ((lon + 180.0).floor() as i32).min(359);
    Some(row * 360 + col)
}

/// The centre of a cell (latitude, longitude in degrees); `None` for a cell out of range.
pub fn center(cell: i32) -> Option<(f64, f64)> {
    (0..CELLS).contains(&cell).then(|| {
        let (row, col) = (cell / 360, cell % 360);
        (f64::from(row) - 89.5, f64::from(col) - 179.5)
    })
}

/// Great-circle distance between the centres of two cells, in km.
pub fn distance_km(a: i32, b: i32) -> Option<f64> {
    let ((la1, lo1), (la2, lo2)) = (center(a)?, center(b)?);
    let (p1, p2) = (la1.to_radians(), la2.to_radians());
    let dp = (la2 - la1).to_radians();
    let dl = (lo2 - lo1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    Some(2.0 * EARTH_KM * h.sqrt().min(1.0).asin())
}

/// A coarse region from Cloudflare's continent code and the longitude: `na-east`, `na-west`,
/// `sa`, `eu`, `af`, `me`, `as-south`, `as-east`, `oc`, `an`; `None` when it says nothing.
pub fn region(continent: &str, lon: Option<f64>) -> Option<&'static str> {
    match continent.trim().to_ascii_uppercase().as_str() {
        "NA" => Some(match lon {
            Some(l) if l < -100.0 => "na-west",
            _ => "na-east",
        }),
        "SA" => Some("sa"),
        "EU" => Some("eu"),
        "AF" => Some("af"),
        "OC" => Some("oc"),
        "AN" => Some("an"),
        "AS" => match lon {
            Some(l) if l < 60.0 => Some("me"),
            Some(l) if l < 100.0 => Some("as-south"),
            Some(_) => Some("as-east"),
            None => None,
        },
        _ => None,
    }
}

/// The estimated class of a round trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PingClass {
    /// Same address, or at most 30 ms.
    Local,
    /// At most 60 ms.
    Near,
    /// At most 120 ms.
    Far,
    /// Beyond.
    Remote,
    /// A side has no location.
    Unknown,
}

impl PingClass {
    /// The wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            PingClass::Local => "local",
            PingClass::Near => "near",
            PingClass::Far => "far",
            PingClass::Remote => "remote",
            PingClass::Unknown => "unknown",
        }
    }
}

/// A caller → host estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// The class.
    pub class: PingClass,
    /// The round trip in ms, rounded to 10 (at least 10); `None` when unknown.
    pub rtt_ms: Option<u32>,
}

impl Estimate {
    /// The estimate between a caller and a host: `local` (10 ms) on the same public address, else
    /// from their cells, else unknown.
    pub fn between(caller: Option<i32>, host: Option<i32>, same_address: bool) -> Self {
        if same_address {
            return Self {
                class: PingClass::Local,
                rtt_ms: Some(10),
            };
        }
        let Some(km) = caller.zip(host).and_then(|(a, b)| distance_km(a, b)) else {
            return Self {
                class: PingClass::Unknown,
                rtt_ms: None,
            };
        };
        let rtt = RTT_BASE_MS + RTT_MS_PER_KM * km;
        let class = if rtt <= 30.0 {
            PingClass::Local
        } else if rtt <= 60.0 {
            PingClass::Near
        } else if rtt <= 120.0 {
            PingClass::Far
        } else {
            PingClass::Remote
        };
        let rounded = ((rtt / 10.0).round() * 10.0).max(10.0);
        Self {
            class,
            rtt_ms: Some(rounded as u32),
        }
    }

    /// Whether a host's limit (`max_rtt_ms`: under that many ms) lets this caller in: no limit,
    /// an unknown estimate, or an estimate under the limit.
    pub fn within(self, max_rtt_ms: Option<u32>) -> bool {
        match (max_rtt_ms, self.rtt_ms) {
            (Some(max), Some(rtt)) => rtt < max,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_cover_the_globe_once() {
        assert_eq!(cell(-90.0, -180.0), Some(0));
        assert_eq!(cell(90.0, 180.0), Some(CELLS - 1));
        assert_eq!(cell(0.0, 0.0), Some(90 * 360 + 180));
        assert_eq!(cell(91.0, 0.0), None);
        assert_eq!(cell(0.0, -181.0), None);
        assert_eq!(cell(f64::NAN, 0.0), None);
        assert_eq!(center(0), Some((-89.5, -179.5)));
        assert_eq!(center(CELLS), None);
        for c in [0, 1234, 32_580, CELLS - 1] {
            let (la, lo) = center(c).unwrap();
            assert_eq!(cell(la, lo), Some(c));
        }
    }

    #[test]
    fn distances_and_classes_follow_the_model() {
        // São Paulo, Rio de Janeiro (about 360 km), Lisbon (about 7900 km).
        let sp = cell(-23.55, -46.63).unwrap();
        let rio = cell(-22.91, -43.17).unwrap();
        let lisbon = cell(38.72, -9.14).unwrap();
        let km = distance_km(sp, rio).unwrap();
        assert!((250.0..450.0).contains(&km), "{km}");
        let near = Estimate::between(Some(sp), Some(rio), false);
        assert_eq!(near.class, PingClass::Local);
        assert!(
            near.rtt_ms.is_some_and(|ms| (10..=20).contains(&ms)),
            "{near:?}"
        );
        let far = Estimate::between(Some(sp), Some(lisbon), false);
        assert_eq!(far.class, PingClass::Remote);
        assert!(far.rtt_ms.unwrap() >= 120, "{far:?}");
        assert_eq!(
            Estimate::between(Some(sp), Some(sp), false).rtt_ms,
            Some(10)
        );
        let same = Estimate::between(None, None, true);
        assert_eq!((same.class, same.rtt_ms), (PingClass::Local, Some(10)));
        let unknown = Estimate::between(Some(sp), None, false);
        assert_eq!((unknown.class, unknown.rtt_ms), (PingClass::Unknown, None));
        // The thresholds: 30 ms ≈ 1333 km, 60 ms ≈ 3333 km, 120 ms ≈ 7333 km.
        let (a, b) = (cell(0.5, 0.5).unwrap(), cell(0.5, 20.5).unwrap()); // ≈ 2224 km
        assert_eq!(
            Estimate::between(Some(a), Some(b), false).class,
            PingClass::Near
        );
        let c = cell(0.5, 50.5).unwrap(); // ≈ 5560 km
        assert_eq!(
            Estimate::between(Some(a), Some(c), false).class,
            PingClass::Far
        );
    }

    #[test]
    fn a_limit_refuses_only_a_known_estimate_at_or_over_it() {
        let e = |rtt| Estimate {
            class: PingClass::Near,
            rtt_ms: rtt,
        };
        assert!(e(Some(50)).within(Some(60)));
        assert!(!e(Some(60)).within(Some(60)), "under the limit, strictly");
        assert!(e(Some(140)).within(Some(150)));
        assert!(e(None).within(Some(60)), "unknown is let in");
        assert!(e(Some(500)).within(None), "no limit");
    }

    #[test]
    fn regions_are_coarse_labels() {
        assert_eq!(region("NA", Some(-122.0)), Some("na-west"));
        assert_eq!(region("na", Some(-74.0)), Some("na-east"));
        assert_eq!(region("SA", None), Some("sa"));
        assert_eq!(region("AS", Some(35.0)), Some("me"));
        assert_eq!(region("AS", Some(77.0)), Some("as-south"));
        assert_eq!(region("AS", Some(139.0)), Some("as-east"));
        assert_eq!(region("AS", None), None);
        assert_eq!(region("T1", Some(0.0)), None);
        assert_eq!(region("", None), None);
    }
}
