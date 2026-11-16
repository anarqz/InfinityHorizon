//! Monthly Rivals (S2, OD-144; other projects used as reference): the events of the
//! Rivals > Monthly page, `Scoreboard.GetTimeTrials` (category 4), and their rules,
//! `Hopper.GetGameOptions` (keyed on `hopperId` = the event id).
//!
//! Each calendar month (UTC) has [`EVENTS_PER_MONTH`] events, a pure function of the month and the
//! catalog: road circuits the game itself races over more than one lap ([`fi_gamedb::Track`]
//! `circuit_laps`, not cross-country), each with a car class (or open) and a time of day, one lap.
//! An event id is `1000 + month * 16 + index` (months since 2026-01), so any event, this month's or
//! an older one, is rebuilt from its id alone and its board and ghosts keep resolving; nothing is
//! stored. Only rules verified in game are used (laps, class, time of day); other restrictions
//! wait for an in-game check (V6).

use fi_gamedb::{Catalog, Track};
use serde_json::{Value as Json, json};

use super::super::forza::Lang;
use super::boards::game_time;
use fi_domain::UnixMs;

/// The Monthly Rivals page lists GetTimeTrials entries of this category.
pub const MONTHLY_CATEGORY: i64 = 4;
/// Events per month.
pub const EVENTS_PER_MONTH: usize = 12;
/// The first event id.
pub const ID_BASE: i64 = 1000;
/// Ids per month (room for [`EVENTS_PER_MONTH`]).
const IDS_PER_MONTH: i64 = 16;
/// The rotation's first month, 2026-01: month index 0.
const EPOCH_YEAR: i64 = 2026;
/// Class ids D to S2; [`OPEN`] for any car.
const CLASSES: [i64; 6] = [0, 1, 2, 3, 4, 5];
/// Any car.
const OPEN: i64 = -1;
/// Times of day the events start at, seconds since midnight (GameOptions.TimeOfDay).
const TIMES: [i64; 6] = [
    8 * 3600,
    11 * 3600,
    13 * 3600,
    16 * 3600,
    19 * 3600,
    22 * 3600,
];

/// One Monthly Rivals event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RivalEvent {
    /// The event id (also the GetTimeTrials `hopperId` and the board's event).
    pub id: i64,
    /// Month index since 2026-01.
    pub month: i64,
    /// Position in the month's list.
    pub index: usize,
    /// The track.
    pub track: i64,
    /// Laps.
    pub laps: i64,
    /// The car class id, or [`OPEN`].
    pub class: i64,
    /// Start time, seconds since midnight.
    pub time_of_day: i64,
}

/// The month index (since 2026-01) of a Unix millisecond instant, UTC.
pub fn month_of(at: UnixMs) -> i64 {
    let (year, month) = civil_month(at.millis().div_euclid(86_400_000));
    (year - EPOCH_YEAR) * 12 + (month - 1)
}

/// The first instant of a month index, Unix milliseconds.
pub fn month_start(month: i64) -> i64 {
    let year = EPOCH_YEAR + month.div_euclid(12);
    let m = month.rem_euclid(12) + 1;
    days_from_civil(year, m) * 86_400_000
}

/// The road circuits the rotation draws from, by track id; of two with one English name (the
/// game names two layouts "San Giovanni East Wall Scramble"), the lower id only. Changing this
/// pool changes past months' events: `the_october_2026_rotation_is_pinned` guards it.
fn circuits(catalog: &Catalog) -> Vec<&Track> {
    let mut out: Vec<&Track> = Vec::new();
    for t in catalog
        .tracks()
        .filter(|t| t.circuit_laps.is_some() && !t.cross_country)
    {
        if !out.iter().any(|o| o.name("en") == t.name("en")) {
            out.push(t);
        }
    }
    out.sort_by_key(|t| t.id);
    out
}

/// SplitMix64: the rotation's deterministic mixing (no randomness, no state).
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A month's events: [`EVENTS_PER_MONTH`] distinct circuits (fewer when the catalog has fewer),
/// classes D to S2 and open in turn from a month offset, times of day mixed.
pub fn month_events(catalog: &Catalog, month: i64) -> Vec<RivalEvent> {
    let pool = circuits(catalog);
    if month < 0 || pool.is_empty() {
        return Vec::new();
    }
    // A month-seeded order of the circuits (a Fisher-Yates shuffle driven by `mix`).
    let mut order: Vec<usize> = (0..pool.len()).collect();
    let seed = mix(month as u64 ^ 0x5249_5641_4C53); // "RIVALS"
    for i in (1..order.len()).rev() {
        let j = (mix(seed.wrapping_add(i as u64)) % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    let classes: Vec<i64> = CLASSES.iter().copied().chain([OPEN]).collect();
    order
        .into_iter()
        .take(EVENTS_PER_MONTH)
        .enumerate()
        .map(|(index, k)| {
            let class = classes[(month as usize + index) % classes.len()];
            let time = TIMES[(mix(seed ^ (index as u64 + 1)) % TIMES.len() as u64) as usize];
            RivalEvent {
                id: ID_BASE + month * IDS_PER_MONTH + index as i64,
                month,
                index,
                track: pool[k].id,
                laps: 1,
                class,
                time_of_day: time,
            }
        })
        .collect()
}

/// Any event by id (this month's or an older one); `None` for an id that is not a Monthly event.
pub fn event(catalog: &Catalog, id: i64) -> Option<RivalEvent> {
    let offset = id.checked_sub(ID_BASE).filter(|o| *o >= 0)?;
    let (month, index) = (offset / IDS_PER_MONTH, (offset % IDS_PER_MONTH) as usize);
    month_events(catalog, month).into_iter().nth(index)
}

/// The class a Monthly event requires of its cars (OD-178), `None` for an open event or an id that
/// is not a Monthly event.
pub fn event_class(catalog: &Catalog, id: i64) -> Option<i64> {
    event(catalog, id).map(|e| e.class).filter(|c| *c != OPEN)
}

/// A class's letter ("S1"), from the catalog.
fn class_name(catalog: &Catalog, class: i64) -> Option<String> {
    (class != OPEN).then(|| {
        catalog
            .class(class)
            .map_or_else(|| class.to_string(), |c| c.name.clone())
    })
}

/// "morning", "manhã", ... for a start time.
fn time_word(seconds: i64, lang: Lang) -> &'static str {
    let hour = seconds / 3600;
    match (hour, lang) {
        (5..=10, Lang::En) => "morning",
        (5..=10, Lang::PtBr) => "manhã",
        (11..=14, Lang::En) => "midday",
        (11..=14, Lang::PtBr) => "meio-dia",
        (15..=17, Lang::En) => "afternoon",
        (15..=17, Lang::PtBr) => "tarde",
        (18..=20, Lang::En) => "evening",
        (18..=20, Lang::PtBr) => "fim de tarde",
        (_, Lang::En) => "night",
        (_, Lang::PtBr) => "noite",
    }
}

/// The event's title and description in `lang`.
pub fn texts(catalog: &Catalog, e: &RivalEvent, lang: Lang) -> (String, String, String) {
    let code = match lang {
        Lang::En => "en",
        Lang::PtBr => "pt-BR",
    };
    let track = catalog
        .track(e.track)
        .map_or_else(|| format!("#{}", e.track), |t| t.name(code).to_owned());
    let class = class_name(catalog, e.class);
    let when = time_word(e.time_of_day, lang);
    let (title, rule, desc) = match (lang, &class) {
        (Lang::En, Some(c)) => (
            format!("{c} Class Rival"),
            format!("{c} class cars only."),
            "One lap",
        ),
        (Lang::En, None) => (
            "Open Class Rival".to_owned(),
            "Any car.".to_owned(),
            "One lap",
        ),
        (Lang::PtBr, Some(c)) => (
            format!("Rival Classe {c}"),
            format!("Somente carros classe {c}."),
            "Uma volta",
        ),
        (Lang::PtBr, None) => (
            "Rival Classe Livre".to_owned(),
            "Qualquer carro.".to_owned(),
            "Uma volta",
        ),
    };
    let description = match lang {
        Lang::En => format!("{desc} of {track}, {when}. {rule} Beat the rival's time."),
        Lang::PtBr => format!("{desc} em {track}, {when}. {rule} Bata o tempo do rival."),
    };
    (
        cut(&format!("{title}: {track}"), 128),
        cut(&description, 512),
        cut(&track, 64),
    )
}

/// At most `chars` characters.
fn cut(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

/// GetTimeTrials' reply: this month's events whose track `raced` (a real rival can be found
/// there: the game cannot start an event without one), at most `max` (all for 0 or less).
pub fn time_trials(
    catalog: &Catalog,
    now: UnixMs,
    lang: Lang,
    max: i64,
    raced: impl Fn(i64) -> bool,
) -> Json {
    let month = month_of(now);
    let start = game_time(UnixMs::from_millis(month_start(month)));
    let end = game_time(UnixMs::from_millis(month_start(month + 1) - 1000));
    let mut events: Vec<RivalEvent> = month_events(catalog, month)
        .into_iter()
        .filter(|e| raced(e.track))
        .collect();
    if let Ok(max) = usize::try_from(max)
        && max > 0
    {
        events.truncate(max);
    }
    let rows: Vec<Json> = events
        .iter()
        .map(|e| {
            let (name, description, track_name) = texts(catalog, e, lang);
            json!({
                "id": e.id,
                "category": MONTHLY_CATEGORY,
                "scoreType": 1,
                "name": name,
                "description": description,
                "notRegisteredMessage": "",
                "trackId": e.track,
                "trackName": track_name,
                "hopperId": e.id,
                "gameOptionsIndex": 0,
                "startDate": start,
                "endDate": end,
                "isUserRegistered": true,
                "isNew": e.index < 3,
                "isFeatured": e.index == 0,
                "isActive": true,
            })
        })
        .collect();
    json!({ "timeTrials": rows })
}

/// An event's `<GameOptions>`, in the game's own format (CareerRaceModes "Laps Race"): the lap
/// count, the class bucket and the start time; the track is GetTimeTrials' `trackId`.
pub fn game_options_xml(e: &RivalEvent) -> String {
    let bucket = if e.class == OPEN {
        String::new()
    } else {
        format!(
            "<Buckets><Bucket><CarRestrictions CarClassId=\"{}\"/></Bucket></Buckets>",
            e.class
        )
    };
    format!(
        "<GameOptions UiRacificationMode=\"Race\" DrivatarTrainingDisabled=\"false\" \
         UseOnlineAntiGriefing=\"true\" UsePFXCheckpoints=\"true\" \
         PositionCPFlaresFromCentre=\"false\" HasTraffic=\"false\" TimeOfDay=\"{}\">{bucket}\
         <StartingGridOptions GridOrderingType=\"Random\" Ascending=\"1\"/>\
         <EndConditionOptions Type=\"NumLaps\" NumLaps=\"{}\"/>\
         <ScoringOptions Type=\"TimeBased\" TargetScoreEnabled=\"1\" BiggerIsBetter=\"0\" \
         FinishingPlaceRelevant=\"true\"/></GameOptions>",
        e.time_of_day, e.laps
    )
}

/// Hopper.GetGameOptions' reply: a Monthly event's options (cut to `optionXmlMaxLength`), else
/// empty.
pub fn game_options(catalog: &Catalog, params: &Json) -> Json {
    let xml = params["hopperId"]
        .as_i64()
        .and_then(|id| event(catalog, id))
        .map(|e| game_options_xml(&e))
        .unwrap_or_default();
    let max = params["optionXmlMaxLength"]
        .as_i64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    let bytes = &xml.as_bytes()[..xml.len().min(max)];
    json!({ "gameOptionsXml": fi_xtsw::hex(bytes) })
}

/// (year, month 1-12) of a day count since 1970-01-01 (H. Hinnant's `civil_from_days`).
fn civil_month(days: i64) -> (i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month)
}

/// Days since 1970-01-01 of the first day of `year`-`month`.
fn days_from_civil(year: i64, month: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_xtsw::{Schema, encode_results};

    fn catalog() -> &'static Catalog {
        Catalog::fh2()
    }

    #[test]
    fn a_monthly_score_must_be_in_its_events_class() {
        use fi_db::scores::{Assists, BoardKey, Submission};
        // An event with a class, and an open one, among the first months.
        let events: Vec<RivalEvent> = (0..24).flat_map(|m| month_events(catalog(), m)).collect();
        let classed = events
            .iter()
            .find(|e| e.class != OPEN)
            .expect("a classed event");
        assert_eq!(event_class(catalog(), classed.id), Some(classed.class));
        if let Some(open) = events.iter().find(|e| e.class == OPEN) {
            assert_eq!(event_class(catalog(), open.id), None);
        }
        assert_eq!(event_class(catalog(), 1), None, "not a Monthly event");
        let submit = |class: i64, board: i32| {
            Submission {
                key: BoardKey {
                    scoreboard_type: board,
                    score_type: 1,
                    game_scoreboard_id: i32::try_from(classed.id).unwrap(),
                    track_id: i32::try_from(classed.track).unwrap(),
                },
                // A car pack's car (no stock class) and an hour on any track.
                score: 3600.0,
                car_id: 3000,
                car_class_id: i16::try_from(class).unwrap(),
                // A raw PI inside the class's own band (T-PD-S6).
                car_pi: {
                    let bands = fi_gamedb::pi::bands();
                    let k = usize::try_from(class).unwrap();
                    let lo = if k == 0 { 0.0 } else { bands[k - 1].raw_max };
                    ((lo + bands[k].raw_max) / 2.0) as f32
                },
                assists: Assists::default(),
                data: serde_json::json!({}),
            }
        };
        let monthly = super::super::boards::MONTHLY_BOARD;
        let gate = |class: i64, board: i32| super::super::boards::gate(&submit(class, board));
        assert!(gate(classed.class, monthly).is_ok());
        // Exactly the event's class (T-PD-S6): above and below are refused.
        let above = classed.class + 1;
        assert_eq!(gate(above, monthly).unwrap_err().rule, "race_time");
        if classed.class > 0 {
            assert!(gate(classed.class - 1, monthly).is_err(), "a lower class");
        }
        // Another board is not the event's business.
        assert!(gate(above, monthly + 1000).is_ok());
    }

    #[test]
    fn months_count_from_2026_01_utc() {
        assert_eq!(month_of(UnixMs::from_millis(month_start(0))), 0);
        // 2026-10-30T12:00Z is month 9.
        assert_eq!(month_of(UnixMs::from_millis(1_793_361_600_000)), 9);
        assert_eq!(month_start(9), 1_790_812_800_000); // 2026-10-01T00:00Z
        for m in [0, 1, 11, 12, 13, 25, 400] {
            assert_eq!(month_of(UnixMs::from_millis(month_start(m))), m, "{m}");
            assert_eq!(
                month_of(UnixMs::from_millis(month_start(m + 1) - 1)),
                m,
                "{m}"
            );
        }
    }

    #[test]
    fn a_month_is_twelve_distinct_road_circuits_and_stable() {
        let a = month_events(catalog(), 9);
        assert_eq!(a.len(), EVENTS_PER_MONTH);
        assert_eq!(
            a,
            month_events(catalog(), 9),
            "a pure function of the month"
        );
        let mut tracks: Vec<i64> = a.iter().map(|e| e.track).collect();
        tracks.sort_unstable();
        tracks.dedup();
        assert_eq!(tracks.len(), EVENTS_PER_MONTH);
        for e in &a {
            let t = catalog().track(e.track).unwrap();
            assert!(t.circuit_laps.is_some() && !t.cross_country, "{}", t.id);
            assert_eq!(e.laps, 1);
            assert!(e.class == OPEN || CLASSES.contains(&e.class));
            assert!(e.id < i64::from(i16::MAX));
        }
        assert_ne!(
            a.iter().map(|e| e.track).collect::<Vec<_>>(),
            month_events(catalog(), 10)
                .iter()
                .map(|e| e.track)
                .collect::<Vec<_>>(),
            "the next month differs"
        );
        assert!(month_events(catalog(), -1).is_empty());
    }

    #[test]
    fn any_event_is_rebuilt_from_its_id() {
        for month in [0, 9, 40] {
            for e in month_events(catalog(), month) {
                assert_eq!(event(catalog(), e.id), Some(e));
            }
        }
        assert_eq!(event(catalog(), 999), None);
        assert_eq!(event(catalog(), ID_BASE + 9 * IDS_PER_MONTH + 15), None);
        assert_eq!(event(catalog(), -5), None);
    }

    #[test]
    fn the_replies_encode_and_speak_both_languages() {
        let schema = Schema::fh2();
        let now = UnixMs::from_millis(1_793_361_600_000);
        for lang in [Lang::En, Lang::PtBr] {
            let reply = time_trials(catalog(), now, lang, 50, |_| true);
            let rows = reply["timeTrials"].as_array().unwrap();
            assert_eq!(rows.len(), EVENTS_PER_MONTH);
            assert_eq!(rows[0]["category"], MONTHLY_CATEGORY);
            assert_eq!(rows[0]["hopperId"], rows[0]["id"]);
            assert!(rows[0]["isFeatured"].as_bool().unwrap());
            assert!(rows[0]["startDate"].is_string() && rows[0]["endDate"].is_string());
            encode_results(
                schema,
                "Forza.WebServices.Scoreboard",
                "GetTimeTrials",
                &reply,
            )
            .unwrap();
        }
        let pt = time_trials(catalog(), now, Lang::PtBr, 0, |_| true);
        assert!(
            pt["timeTrials"][0]["description"]
                .as_str()
                .unwrap()
                .contains("Bata o tempo do rival")
        );
        assert_eq!(
            time_trials(catalog(), now, Lang::En, 3, |_| true)["timeTrials"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        // Only events whose track has a real time are listed.
        let first = month_events(catalog(), month_of(now))[0].track;
        let only = time_trials(catalog(), now, Lang::En, 0, |t| t == first);
        assert_eq!(only["timeTrials"].as_array().unwrap().len(), 1);
        assert_eq!(only["timeTrials"][0]["trackId"], first);
        let e = &month_events(catalog(), month_of(now))[0];
        let params = json!({"hopperId": e.id, "roundNumber": 0, "optionXmlMaxLength": 4096});
        let reply = game_options(catalog(), &params);
        encode_results(schema, "Forza.WebServices.Hopper", "GetGameOptions", &reply).unwrap();
        let xml = String::from_utf8(
            (0..reply["gameOptionsXml"].as_str().unwrap().len())
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(&reply["gameOptionsXml"].as_str().unwrap()[i..i + 2], 16)
                        .unwrap()
                })
                .collect(),
        )
        .unwrap();
        assert_eq!(xml, game_options_xml(e));
        assert!(xml.contains("NumLaps=\"1\"") && xml.contains("TimeOfDay="));
        let none = game_options(
            catalog(),
            &json!({"hopperId": 7, "optionXmlMaxLength": 4096}),
        );
        assert_eq!(none["gameOptionsXml"], "");
        let short = game_options(
            catalog(),
            &json!({"hopperId": e.id, "optionXmlMaxLength": 10}),
        );
        assert_eq!(short["gameOptionsXml"].as_str().unwrap().len(), 20);
    }

    #[test]
    fn the_october_2026_rotation_is_pinned() {
        let got: Vec<(i64, i64, i64, i64)> = month_events(catalog(), 9)
            .iter()
            .map(|e| (e.id, e.track, e.class, e.time_of_day))
            .collect();
        let want = vec![
            (1144, 574, 2, 28800),
            (1145, 862, 3, 79200),
            (1146, 860, 4, 28800),
            (1147, 832, 5, 46800),
            (1148, 573, OPEN, 39600),
            (1149, 859, 0, 79200),
            (1150, 839, 1, 28800),
            (1151, 830, 2, 68400),
            (1152, 851, 3, 68400),
            (1153, 840, 4, 79200),
            (1154, 567, 5, 68400),
            (1155, 865, OPEN, 68400),
        ];
        assert_eq!(got, want, "past months must never change");
    }

    #[test]
    fn class_events_restrict_the_class_and_open_ones_do_not() {
        let e = RivalEvent {
            id: 1000,
            month: 0,
            index: 0,
            track: 560,
            laps: 1,
            class: 4,
            time_of_day: 8 * 3600,
        };
        assert!(game_options_xml(&e).contains("<CarRestrictions CarClassId=\"4\"/>"));
        let open = RivalEvent { class: OPEN, ..e };
        assert!(!game_options_xml(&open).contains("CarRestrictions"));
        let (title, _, _) = texts(catalog(), &open, Lang::En);
        assert!(title.starts_with("Open Class Rival"), "{title}");
    }
}
