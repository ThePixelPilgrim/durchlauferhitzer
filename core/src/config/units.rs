//! Parsing of durations and sizes.

use std::time::Duration;

/// Splits `s` into a leading run of ASCII digits (as a number) and the rest.
/// `None` if there are no digits or the number overflows `u64`.
fn split_number(s: &str) -> Option<(u64, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let digits = s.get(..end)?;
    let rest = s.get(end..)?;
    if digits.is_empty() {
        return None;
    }
    Some((digits.parse().ok()?, rest))
}

/// Parses `<u64>ms`, `<u64>s` or `<u64>m`; no other forms.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let (n, unit) = split_number(s)?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n.checked_mul(60)?)),
        _ => None,
    }
}

/// Parses plain bytes or `<u64>KiB|MiB|GiB`; overflow gives `None`.
pub fn parse_size(s: &str) -> Option<u64> {
    let (n, unit) = split_number(s)?;
    let factor: u64 = match unit {
        "" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("10s"), Some(Duration::from_secs(10)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
    }

    #[test]
    fn rejects_bad_durations() {
        for s in ["10", "1h", "s", "", "ms", "-1s", "+1s", "1 s", "1.5s", "1S"] {
            assert_eq!(parse_duration(s), None, "{s:?}");
        }
        assert_eq!(parse_duration("18446744073709551615m"), None);
        assert_eq!(parse_duration("99999999999999999999s"), None);
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("16KiB"), Some(16384));
        assert_eq!(parse_size("1GiB"), Some(1 << 30));
        assert_eq!(parse_size("10MiB"), Some(10 << 20));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("0"), Some(0));
    }

    #[test]
    fn rejects_bad_sizes() {
        for s in ["1kb", "KiB", "", "-1", "+1", "1 KiB", "1.5MiB", "1KB", "1B"] {
            assert_eq!(parse_size(s), None, "{s:?}");
        }
        assert_eq!(parse_size("18446744073709551615GiB"), None);
        assert_eq!(parse_size("18446744073709551616"), None);
        assert_eq!(parse_size("18446744073709551615"), Some(u64::MAX));
    }
}
