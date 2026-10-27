//! The `Date` header value (FR-HTTP-22a): an IMF-fixdate (RFC 9110
//! section 5.6.7) rendered from the wall-clock second the driver supplies,
//! with no time library: the civil date comes from days-from-civil
//! arithmetic on the proleptic Gregorian calendar.

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
