//! One CSV row per scenario and run (header [`HEADER`]), and today's
//! date.

use std::time::{SystemTime, UNIX_EPOCH};

pub const HEADER: &str =
    "date,commit,runner,scenario,requests,errors,seconds,cpu_us_per_req,syscalls_per_req,rss_kib";

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub date: String,
    pub commit: String,
    pub runner: String,
    pub scenario: &'static str,
    pub requests: u64,
    pub errors: u64,
    pub seconds: f64,
    pub cpu_us_per_req: Option<f64>,
    pub syscalls_per_req: Option<f64>,
    pub rss_kib: Option<u64>,
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

impl Row {
    /// The CSV line (no newline). A per-request value without requests
    /// is empty.
    pub fn line(&self) -> String {
        format!(
            "{},{},{},{},{},{},{:.3},{},{},{}",
            self.date,
            self.commit,
            self.runner,
            self.scenario,
            self.requests,
            self.errors,
            self.seconds,
            opt(self.cpu_us_per_req.map(|v| format!("{v:.3}"))),
            opt(self.syscalls_per_req.map(|v| format!("{v:.3}"))),
            opt(self.rss_kib),
        )
    }
}

/// `YYYY-MM-DD` (UTC) of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
pub fn date_of_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Today's date (UTC).
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    date_of_days(i64::try_from(secs / 86_400).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date_of_days(0), "1970-01-01");
        assert_eq!(date_of_days(11_016), "2000-02-29");
        assert_eq!(date_of_days(20_398), "2025-11-06");
        assert_eq!(date_of_days(20_763), "2026-11-06");
    }

    #[test]
    fn a_line() {
        let mut row = Row {
            date: "2026-11-06".into(),
            commit: "71d3867".into(),
            runner: "local".into(),
            scenario: "static-small",
            requests: 1000,
            errors: 0,
            seconds: 10.0,
            cpu_us_per_req: Some(12.3456),
            syscalls_per_req: Some(4.0),
            rss_kib: Some(3172),
        };
        assert_eq!(
            row.line(),
            "2026-11-06,71d3867,local,static-small,1000,0,10.000,12.346,4.000,3172"
        );
        row.requests = 0;
        row.errors = 3;
        row.cpu_us_per_req = None;
        row.syscalls_per_req = None;
        assert_eq!(
            row.line(),
            "2026-11-06,71d3867,local,static-small,0,3,10.000,,,3172"
        );
        assert_eq!(HEADER.split(',').count(), row.line().split(',').count());
    }
}
