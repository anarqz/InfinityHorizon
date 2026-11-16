//! Wire instants (ACCOUNTS.md §1): RFC 3339, UTC, exactly six fractional digits, `Z`
//! (`2026-10-17T14:00:00.000000Z`). The calendar arithmetic is written out so this crate keeps no
//! dependency beyond serde. One shape, so an API body and a token claim never disagree on how an
//! instant looks.

/// Formats Unix microseconds; `None` outside the years 0000 to 9999.
pub fn format_unix_micros(micros: i64) -> Option<String> {
    let seconds = micros.div_euclid(1_000_000);
    let fraction = micros.rem_euclid(1_000_000);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    if !(0..=9999).contains(&year) {
        return None;
    }
    let time = seconds.rem_euclid(86_400);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{fraction:06}Z",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    ))
}

/// Formats Unix milliseconds.
pub fn format_unix_ms(millis: i64) -> Option<String> {
    format_unix_micros(millis.checked_mul(1_000)?)
}

/// Formats Unix seconds.
pub fn format_unix_secs(secs: i64) -> Option<String> {
    format_unix_micros(secs.checked_mul(1_000_000)?)
}

/// Parses a wire instant into Unix microseconds; `None` for any other shape (another offset, fewer
/// or more fractional digits, an impossible date or time).
pub fn parse_unix_micros(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let separators = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
        (26, b'Z'),
    ];
    if b.len() != 27 || separators.iter().any(|&(i, c)| b[i] != c) {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<i64> {
        b[from..to].iter().try_fold(0i64, |n, &c| {
            c.is_ascii_digit().then(|| n * 10 + i64::from(c - b'0'))
        })
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let fraction = number(20, 26)?;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    seconds.checked_mul(1_000_000)?.checked_add(fraction)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (H. Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_six_fractional_digits() {
        // The contract's example instant (ACCOUNTS.md §1) and the ticket example's iat (§4.2).
        assert_eq!(
            format_unix_secs(1_792_245_600).unwrap(),
            "2026-10-17T14:00:00.000000Z"
        );
        assert_eq!(
            format_unix_ms(1_792_245_600_007).unwrap(),
            "2026-10-17T14:00:00.007000Z"
        );
        assert_eq!(
            format_unix_micros(951_868_799_999_999).unwrap(),
            "2000-02-29T23:59:59.999999Z",
            "leap day, last microsecond"
        );
        assert_eq!(
            format_unix_micros(0).unwrap(),
            "1970-01-01T00:00:00.000000Z"
        );
        assert_eq!(
            format_unix_micros(-1).unwrap(),
            "1969-12-31T23:59:59.999999Z"
        );
        for text in [
            format_unix_secs(1_792_245_600).unwrap(),
            format_unix_micros(-1).unwrap(),
        ] {
            let fraction = text.split_once('.').unwrap().1;
            assert_eq!(fraction.len(), 7, "six digits and Z: {text}");
            assert!(fraction.ends_with('Z'));
        }
        assert_eq!(format_unix_secs(253_402_300_800), None, "year 10000");
        assert_eq!(format_unix_ms(i64::MAX), None);
    }

    #[test]
    fn parses_only_the_one_shape() {
        assert_eq!(
            parse_unix_micros("2026-10-17T14:00:00.000000Z"),
            Some(1_792_245_600_000_000)
        );
        assert_eq!(
            parse_unix_micros("2000-02-29T23:59:59.999999Z"),
            Some(951_868_799_999_999)
        );
        assert_eq!(parse_unix_micros("1969-12-31T23:59:59.999999Z"), Some(-1));
        for bad in [
            "2026-10-17T14:00:00Z",
            "2026-10-17T14:00:00.000Z",
            "2026-10-17T14:00:00.000000+00:00",
            "2026-10-17 14:00:00.000000Z",
            "2026-13-17T14:00:00.000000Z",
            "2026-02-29T14:00:00.000000Z",
            "2026-10-17T24:00:00.000000Z",
            "2026-10-17T14:00:60.000000Z",
            "+026-10-17T14:00:00.000000Z",
            "2026-10-17T14:00:00.00000aZ",
        ] {
            assert_eq!(parse_unix_micros(bad), None, "{bad}");
        }
    }
}
