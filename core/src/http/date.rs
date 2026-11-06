//! HTTP dates (RFC 9110 section 5.6.7) with no time library: the
//! IMF-fixdate of the `Date` header (FR-HTTP-22a) and of `Last-Modified`
//! (FR-STATIC-18), rendered from Unix seconds, and the parser of a
//! request's `If-Modified-Since`, which accepts all three HTTP-date forms.
//! The civil date comes from days-from-civil arithmetic on the proleptic
//! Gregorian calendar.

/// Length of an IMF-fixdate, `Sun, 06 Nov 1994 08:49:37 GMT`.
pub const IMF_FIXDATE_LEN: usize = 29;

/// The last second an IMF-fixdate can name (its year has four digits):
/// `Fri, 31 Dec 9999 23:59:59 GMT`.
pub const MAX_UNIX_SECS: u64 = 253_402_300_799;

/// An IMF-fixdate, as [`imf_fixdate`] renders it.
pub type ImfFixdate = [u8; IMF_FIXDATE_LEN];

/// The IMF-fixdate of `unix_secs` seconds after 1970-01-01 00:00:00 UTC.
/// Seconds beyond the year 9999 are clamped to its last second
/// ([`MAX_UNIX_SECS`]), so the result always has exactly
/// [`IMF_FIXDATE_LEN`] bytes.
pub fn imf_fixdate(unix_secs: u64) -> ImfFixdate {
    const WEEKDAYS: [&[u8; 3]; 7] = [b"Thu", b"Fri", b"Sat", b"Sun", b"Mon", b"Tue", b"Wed"];
    const MONTHS: [&[u8; 3]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let secs = unix_secs.min(MAX_UNIX_SECS);
    let days = secs / 86_400;
    let of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let mut out = *b"Thu, 01 Jan 1970 00:00:00 GMT";
    // 1970-01-01 was a Thursday.
    let weekday = WEEKDAYS.get(usize::try_from(days % 7).unwrap_or(0));
    let name = MONTHS.get(usize::try_from(month.saturating_sub(1)).unwrap_or(0));
    put(&mut out, 0, weekday.map_or(b"Thu", |w| *w));
    put(&mut out, 5, &digits::<2>(day));
    put(&mut out, 8, name.map_or(b"Jan", |m| *m));
    put(&mut out, 12, &digits::<4>(year));
    put(&mut out, 17, &digits::<2>(of_day / 3600));
    put(&mut out, 20, &digits::<2>(of_day / 60 % 60));
    put(&mut out, 23, &digits::<2>(of_day % 60));
    out
}

/// The proleptic Gregorian `(year, month, day)` of `days` days after
/// 1970-01-01 (H. Hinnant's `civil_from_days`, for non-negative days:
/// eras of 400 years that start on 1 March, so the leap day ends a year).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March first
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// `value` as `N` decimal digits, with leading zeros (higher digits are
/// dropped; callers pass values that fit).
fn digits<const N: usize>(mut value: u64) -> [u8; N] {
    let mut out = [b'0'; N];
    for slot in out.iter_mut().rev() {
        // `value % 10` is below 10, so the cast cannot truncate.
        *slot = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out
}

/// Copy `bytes` into `out` from `at` (a fixed position of the template).
fn put(out: &mut ImfFixdate, at: usize, bytes: &[u8]) {
    if let Some(dst) = out.get_mut(at..at.saturating_add(bytes.len())) {
        dst.copy_from_slice(bytes);
    }
}

/// The three-letter day names of an IMF-fixdate and of asctime.
const DAY_NAMES: [&[u8]; 7] = [b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun"];

/// The full day names of the obsolete RFC 850 form.
const LONG_DAY_NAMES: [&[u8]; 7] = [
    b"Monday",
    b"Tuesday",
    b"Wednesday",
    b"Thursday",
    b"Friday",
    b"Saturday",
    b"Sunday",
];

const MONTH_NAMES: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

/// The Unix seconds an HTTP-date names (RFC 9110 section 5.6.7), or
/// `None` if `value` is none of its three forms:
///
/// - IMF-fixdate `Sun, 06 Nov 1994 08:49:37 GMT`;
/// - RFC 850 `Sunday, 06-Nov-94 08:49:37 GMT`;
/// - asctime `Sun Nov  6 08:49:37 1994` (day as two digits or space and
///   digit).
///
/// Names are case-sensitive, as the grammar has them. The day name must be
/// one of the seven but is not checked against the date (it is redundant).
/// The date must exist (no 31 April, 29 February only in leap years); hour
/// 00–23, minute 00–59, second 00–60 (a leap second counts as the next
/// minute's first). Years before 1970 and after 9999 are `None`.
///
/// RFC 850's two-digit year is a fixed rule, 70–99 → 1970–1999 and
/// 00–69 → 2000–2069: RFC 9110's "most recent year with those digits no
/// more than 50 years ahead" at the year 2019, so it differs from the RFC
/// only for years read as 1970s that the RFC, from 2020 on, would place in
/// the 2070s, which are in the future and ignored there (FR-STATIC-18).
pub fn parse_http_date(value: &[u8]) -> Option<u64> {
    imf_fixdate_value(value)
        .or_else(|| rfc850_value(value))
        .or_else(|| asctime_value(value))
}

/// `Sun, 06 Nov 1994 08:49:37 GMT`.
fn imf_fixdate_value(value: &[u8]) -> Option<u64> {
    let [d0, d1, d2, b',', b' ', rest @ ..] = value else {
        return None;
    };
    DAY_NAMES.contains(&&[*d0, *d1, *d2][..]).then_some(())?;
    let [
        a,
        b,
        b' ',
        m0,
        m1,
        m2,
        b' ',
        y0,
        y1,
        y2,
        y3,
        b' ',
        time @ ..,
        b' ',
        b'G',
        b'M',
        b'T',
    ] = rest
    else {
        return None;
    };
    let day = number(&[*a, *b])?;
    let month = month(&[*m0, *m1, *m2])?;
    let year = number(&[*y0, *y1, *y2, *y3])?;
    unix_secs(year, month, day, time_of_day(time)?)
}

/// `Sunday, 06-Nov-94 08:49:37 GMT`.
fn rfc850_value(value: &[u8]) -> Option<u64> {
    let comma = value.iter().position(|&b| b == b',')?;
    let name = value.get(..comma)?;
    LONG_DAY_NAMES.contains(&name).then_some(())?;
    let rest = value.get(comma + 1..)?;
    let [
        b' ',
        a,
        b,
        b'-',
        m0,
        m1,
        m2,
        b'-',
        y0,
        y1,
        b' ',
        time @ ..,
        b' ',
        b'G',
        b'M',
        b'T',
    ] = rest
    else {
        return None;
    };
    let day = number(&[*a, *b])?;
    let month = month(&[*m0, *m1, *m2])?;
    let yy = number(&[*y0, *y1])?;
    let year = if yy < 70 { 2000 + yy } else { 1900 + yy };
    unix_secs(year, month, day, time_of_day(time)?)
}

/// `Sun Nov  6 08:49:37 1994`.
fn asctime_value(value: &[u8]) -> Option<u64> {
    let [
        d0,
        d1,
        d2,
        b' ',
        m0,
        m1,
        m2,
        b' ',
        a,
        b,
        b' ',
        time @ ..,
        b' ',
        y0,
        y1,
        y2,
        y3,
    ] = value
    else {
        return None;
    };
    DAY_NAMES.contains(&&[*d0, *d1, *d2][..]).then_some(())?;
    let month = month(&[*m0, *m1, *m2])?;
    let day = match a {
        b' ' => number(&[*b])?,
        _ => number(&[*a, *b])?,
    };
    let year = number(&[*y0, *y1, *y2, *y3])?;
    unix_secs(year, month, day, time_of_day(time)?)
}

/// The decimal value of `digits` (ASCII digits only, at least one).
fn number(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        digits
            .iter()
            .fold(0, |n, d| n * 10 + u64::from(d.saturating_sub(b'0'))),
    )
}

/// The month (1–12) a three-letter name names.
fn month(name: &[u8]) -> Option<u64> {
    let index = MONTH_NAMES.iter().position(|m| *m == name)?;
    u64::try_from(index).ok().map(|i| i + 1)
}

/// The seconds into the day of `HH:MM:SS` (second 60 allowed).
fn time_of_day(time: &[u8]) -> Option<u64> {
    let [h0, h1, b':', m0, m1, b':', s0, s1] = time else {
        return None;
    };
    let (hour, minute, second) = (
        number(&[*h0, *h1])?,
        number(&[*m0, *m1])?,
        number(&[*s0, *s1])?,
    );
    (hour <= 23 && minute <= 59 && second <= 60).then_some(hour * 3600 + minute * 60 + second)
}

/// The Unix seconds of `of_day` seconds into the given civil date, if the
/// date exists and lies in 1970–9999.
fn unix_secs(year: u64, month: u64, day: u64, of_day: u64) -> Option<u64> {
    if !(1970..=9999).contains(&year) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + of_day)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days after 1970-01-01 of a valid civil date from 1970 on (H. Hinnant's
/// `days_from_civil`, the inverse of [`civil_from_days`]).
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 }; // March first
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    (era * 146_097 + doe).saturating_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(unix_secs: u64) -> String {
        String::from_utf8(imf_fixdate(unix_secs).to_vec()).unwrap()
    }

    #[test]
    fn renders_the_table() {
        let rows: [(u64, &str); 8] = [
            (0, "Thu, 01 Jan 1970 00:00:00 GMT"),
            // The example of RFC 9110 section 5.6.7.
            (784_111_777, "Sun, 06 Nov 1994 08:49:37 GMT"),
            (951_782_399, "Mon, 28 Feb 2000 23:59:59 GMT"),
            (951_782_400, "Tue, 29 Feb 2000 00:00:00 GMT"),
            (951_868_800, "Wed, 01 Mar 2000 00:00:00 GMT"),
            (4_102_444_799, "Thu, 31 Dec 2099 23:59:59 GMT"),
            (4_102_444_800, "Fri, 01 Jan 2100 00:00:00 GMT"),
            (253_402_300_799, "Fri, 31 Dec 9999 23:59:59 GMT"),
        ];
        for (secs, expected) in rows {
            assert_eq!(text(secs), expected, "{secs}");
        }
    }

    #[test]
    fn clamps_beyond_the_year_9999() {
        for secs in [MAX_UNIX_SECS + 1, 300_000_000_000, u64::MAX] {
            assert_eq!(text(secs), "Fri, 31 Dec 9999 23:59:59 GMT", "{secs}");
        }
    }

    #[test]
    fn parses_the_three_forms_of_rfc_9110() {
        for form in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun Nov 06 08:49:37 1994",
        ] {
            assert_eq!(
                parse_http_date(form.as_bytes()),
                Some(784_111_777),
                "{form}"
            );
        }
    }

    #[test]
    fn rfc_850_years_70_to_99_are_1900s_and_00_to_69_are_2000s() {
        let rows: [(&str, u64); 4] = [
            ("Thursday, 01-Jan-70 00:00:00 GMT", 0),
            ("Friday, 31-Dec-99 23:59:59 GMT", 946_684_799),
            ("Saturday, 01-Jan-00 00:00:00 GMT", 946_684_800),
            ("Monday, 31-Dec-69 23:59:59 GMT", 3_155_759_999),
        ];
        for (text, secs) in rows {
            assert_eq!(parse_http_date(text.as_bytes()), Some(secs), "{text}");
        }
    }

    #[test]
    fn a_leap_second_counts_as_the_next_minute() {
        assert_eq!(
            parse_http_date(b"Sun, 06 Nov 1994 08:49:60 GMT"),
            Some(784_111_800)
        );
    }

    #[test]
    fn rejects_anything_else() {
        for bad in [
            "",
            "784111777",
            // Lowercase names, a wrong day name, a missing comma.
            "sun, 06 Nov 1994 08:49:37 GMT",
            "Sun, 06 nov 1994 08:49:37 GMT",
            "Sum, 06 Nov 1994 08:49:37 GMT",
            "Sun 06 Nov 1994 08:49:37 GMT",
            // Not GMT, extra or missing spaces, a one-digit day.
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Nov 1994 08:49:37 gmt",
            "Sun, 06 Nov 1994 08:49:37 GMT ",
            " Sun, 06 Nov 1994 08:49:37 GMT",
            "Sun,  6 Nov 1994 08:49:37 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            // Out of range fields and days that do not exist.
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1994 08:60:00 GMT",
            "Sun, 06 Nov 1994 08:49:61 GMT",
            "Sun, 00 Nov 1994 08:49:37 GMT",
            "Sun, 31 Nov 1994 08:49:37 GMT",
            "Tue, 29 Feb 2100 00:00:00 GMT",
            "Sun, 06 Nov 1969 08:49:37 GMT",
            "Sun, 06 Nov +994 08:49:37 GMT",
            // RFC 850 with a short day name or a four-digit year.
            "Sun, 06-Nov-94 08:49:37 GMT",
            "Sunday, 06-Nov-1994 08:49:37 GMT",
            "Sunday, 06 Nov 94 08:49:37 GMT",
            // asctime with a zone, or the day padded with a zero and space.
            "Sun Nov  6 08:49:37 1994 GMT",
            "Sun Nov 6 08:49:37 1994",
            "Sun Nov  6 08:49:37 94",
        ] {
            assert_eq!(parse_http_date(bad.as_bytes()), None, "{bad:?}");
        }
    }

    /// Rendering and parsing agree on every day from 1970 to past 2100.
    #[test]
    fn parses_what_it_renders() {
        for n in 0..(200 * 366) {
            let secs = n * 86_400 + (n * 7919) % 86_400;
            assert_eq!(parse_http_date(&imf_fixdate(secs)), Some(secs), "day {n}");
        }
        assert_eq!(
            parse_http_date(&imf_fixdate(MAX_UNIX_SECS)),
            Some(MAX_UNIX_SECS)
        );
    }

    /// Every day from 1970 to well past 2100 (leap years, the skipped leap
    /// day of 2100): the rendered date is the day after the previous one.
    #[test]
    fn consecutive_days_follow_the_calendar() {
        const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        let (mut year, mut month, mut day) = (1970u64, 1u64, 1u64);
        for n in 0..(200 * 366) {
            let rendered = text(n * 86_400 + 86_399);
            let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
            let length = match month {
                2 if leap => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            let months = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let expected = format!(
                "{}, {day:02} {} {year} 23:59:59 GMT",
                DAYS[(n % 7) as usize],
                months[(month - 1) as usize]
            );
            assert_eq!(rendered, expected, "day {n}");
            day += 1;
            if day > length {
                day = 1;
                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
            }
        }
    }
}
