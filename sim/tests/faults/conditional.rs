//! Conditional requests in the static net (FR-STATIC-18): the model of
//! `If-None-Match` and `If-Modified-Since`, written from RFC 9110
//! (sections 5.6.7, 8.8.3, 13.1.2, 13.1.3, 13.2.2) and not from the core,
//! the conditional lines the random clients add, and their coverage.
//!
//! Every file of the random tree has an mtime around the net's clock
//! (`NET_DATE`); the clients send its real `ETag` and dates in each form,
//! so 304s are common.

use super::*;

/// What a `GET` or `HEAD` of an existing regular file must get, by its
/// conditional headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFreshness {
    Modified,
    NotModified,
}

/// The header lines of `head` (after the request line, up to the blank
/// line), as lowercased name and value without surrounding SP/HTAB.
fn header_lines(head: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(head);
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    head.split("\r\n")
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((
                name.to_ascii_lowercase(),
                value.trim_matches([' ', '\t']).to_owned(),
            ))
        })
        .collect()
}

fn named(head: &[u8], name: &str) -> Vec<String> {
    header_lines(head)
        .into_iter()
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v)
        .collect()
}

/// RFC 9110 section 13.1.2: `If-None-Match = "*" / #entity-tag`, all its
/// lines one list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelInm {
    Absent,
    /// Present but not of that grammar: ignored as a whole.
    Malformed,
    Any,
    /// The opaque tags (quotes kept), each with whether it was weak.
    Tags(Vec<(String, bool)>),
}

pub(crate) fn model_inm(head: &[u8]) -> ModelInm {
    let lines = named(head, "if-none-match");
    if lines.is_empty() {
        return ModelInm::Absent;
    }
    if lines.len() == 1 && lines.first().is_some_and(|l| l == "*") {
        return ModelInm::Any;
    }
    let mut tags = Vec::new();
    for line in &lines {
        // Commas outside quotes separate the elements.
        let mut elements = vec![String::new()];
        let mut quoted = false;
        for ch in line.chars() {
            match ch {
                '"' => quoted = !quoted,
                ',' if !quoted => {
                    elements.push(String::new());
                    continue;
                }
                _ => {}
            }
            if let Some(last) = elements.last_mut() {
                last.push(ch);
            }
        }
        for element in elements {
            let element = element.trim_matches([' ', '\t']);
            if element.is_empty() {
                continue;
            }
            let (weak, tag) = match element.strip_prefix("W/") {
                Some(tag) => (true, tag),
                None => (false, element),
            };
            let inner = tag.strip_prefix('"').and_then(|t| t.strip_suffix('"'));
            let valid =
                inner.is_some_and(|i| i.bytes().all(|b| b == 0x21 || (0x23..=0x7e).contains(&b)));
            if !valid {
                return ModelInm::Malformed;
            }
            tags.push((tag.to_owned(), weak));
        }
    }
    ModelInm::Tags(tags)
}

/// The seconds an HTTP-date names (RFC 9110 section 5.6.7): IMF-fixdate,
/// RFC 850 or asctime; `None` for anything else. RFC 850's two-digit year
/// is the latest year with those digits at most 50 years after `now`.
pub(crate) fn model_parse_date(text: &str, now: u64) -> Option<u64> {
    const SHORT: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const LONG: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let digits = |s: &str, n: usize| -> Option<u64> {
        (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
    };
    let month = |s: &str| MONTHS.iter().position(|m| *m == s);
    let (day, month, year, time) = if let Some((name, rest)) = text.split_once(", ") {
        if SHORT.contains(&name) {
            // IMF-fixdate: `06 Nov 1994 08:49:37 GMT`.
            let parts: Vec<&str> = rest.split(' ').collect();
            let [d, m, y, t, "GMT"] = parts.as_slice() else {
                return None;
            };
            (digits(d, 2)?, month(m)?, digits(y, 4)?, *t)
        } else if LONG.contains(&name) {
            // RFC 850: `06-Nov-94 08:49:37 GMT`.
            let parts: Vec<&str> = rest.split(' ').collect();
            let [date, t, "GMT"] = parts.as_slice() else {
                return None;
            };
            let dmy: Vec<&str> = date.split('-').collect();
            let [d, m, yy] = dmy.as_slice() else {
                return None;
            };
            let yy = digits(yy, 2)?;
            let now_year = model_year_of(now);
            let latest = now_year + 50;
            let year = latest - (latest - yy) % 100;
            (digits(d, 2)?, month(m)?, year, *t)
        } else {
            return None;
        }
    } else {
        // asctime: `Sun Nov  6 08:49:37 1994`.
        let (name, rest) = text.split_once(' ')?;
        if !SHORT.contains(&name) {
            return None;
        }
        let (m, rest) = rest.split_once(' ')?;
        let (d, rest) = match rest.strip_prefix(' ') {
            Some(rest) => (rest.get(..1)?, rest.get(1..)?),
            None => (rest.get(..2)?, rest.get(2..)?),
        };
        let parts: Vec<&str> = rest.split(' ').collect();
        let ["", t, y] = parts.as_slice() else {
            return None;
        };
        (digits(d, d.len().max(1))?, month(m)?, digits(y, 4)?, *t)
    };
    let hms: Vec<&str> = time.split(':').collect();
    let [h, mi, s] = hms.as_slice() else {
        return None;
    };
    let (h, mi, s) = (digits(h, 2)?, digits(mi, 2)?, digits(s, 2)?);
    if !(1970..=9999).contains(&year) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    if day == 0 || day > model_month_len(year, month) {
        return None;
    }
    let mut days = 0;
    for y in 1970..year {
        days += if model_leap(y) { 366 } else { 365 };
    }
    for m in 0..month {
        days += model_month_len(year, m);
    }
    days += day - 1;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

/// The civil year of `secs` after 1970.
fn model_year_of(secs: u64) -> u64 {
    let mut days = secs / 86_400;
    let mut year = 1970;
    loop {
        let len = if model_leap(year) { 366 } else { 365 };
        if days < len {
            return year;
        }
        days -= len;
        year += 1;
    }
}

/// RFC 9110 section 13.1.3: one valid HTTP-date, not after the server's
/// time; anything else is ignored.
pub(crate) fn model_ims(head: &[u8], now: u64) -> Option<u64> {
    let lines = named(head, "if-modified-since");
    let [one] = lines.as_slice() else {
        return None;
    };
    model_parse_date(one, now).filter(|date| *date <= now)
}

/// RFC 9110 section 13.2.2 for a `GET` or `HEAD` of a file with `stamp`,
/// the server's time being `now`: a present, well-formed `If-None-Match`
/// decides alone (`*` matches, a tag matches by weak comparison);
/// otherwise `If-Modified-Since`, comparing the mtime in whole seconds
/// (what `Last-Modified` says).
pub(crate) fn model_verdict(head: &[u8], stamp: FileStamp, now: u64) -> ModelFreshness {
    let etag = model_etag(stamp);
    let not_modified = match model_inm(head) {
        ModelInm::Any => true,
        ModelInm::Tags(tags) => tags.iter().any(|(tag, _)| *tag == etag),
        ModelInm::Absent | ModelInm::Malformed => {
            model_ims(head, now).is_some_and(|date| stamp.modified_ns / 1_000_000_000 <= date)
        }
    };
    if not_modified {
        ModelFreshness::NotModified
    } else {
        ModelFreshness::Modified
    }
}

/// The date `secs` in the three forms, from the model's IMF-fixdate.
pub(crate) fn date_forms(secs: u64) -> [String; 3] {
    const LONG: [(&str, &str); 7] = [
        ("Mon", "Monday"),
        ("Tue", "Tuesday"),
        ("Wed", "Wednesday"),
        ("Thu", "Thursday"),
        ("Fri", "Friday"),
        ("Sat", "Saturday"),
        ("Sun", "Sunday"),
    ];
    let imf = model_imf_fixdate(secs);
    // `Sun, 06 Nov 1994 08:49:37 GMT`
    let parts: Vec<&str> = imf.split([',', ' ']).filter(|p| !p.is_empty()).collect();
    let [name, d, m, y, t, _] = parts.as_slice() else {
        return [imf.clone(), imf.clone(), imf];
    };
    let long = LONG
        .iter()
        .find(|(s, _)| s == name)
        .map_or(*name, |(_, l)| *l);
    let yy = y.get(2..).unwrap_or_default();
    let rfc850 = format!("{long}, {d}-{m}-{yy} {t} GMT");
    let day = d.trim_start_matches('0');
    let asctime = format!("{name} {m} {day:>2} {t} {y}");
    [imf, rfc850, asctime]
}

/// Conditional header lines for a request of the file with `stamp` (or of
/// no file): none, a third of the time; else a mix of matching, weak,
/// listed, split, `*`, other and malformed tags, and dates equal to, before
/// and after its mtime (also after the server's clock) in each form, alone
/// or both. The lines are short: under the short limits the whole head
/// must stay within `max_head` (160 bytes), or the request goes without.
pub(crate) fn random_conditional(rng: &mut fastrand::Rng, stamp: Option<FileStamp>) -> String {
    if rng.u8(0..3) == 0 {
        return String::new();
    }
    let stamp = stamp.unwrap_or(FileStamp {
        size: rng.u64(0..2000),
        modified_ns: rng.u64(0..=NET_DATE.0) * 1_000_000_000,
    });
    let etag = model_etag(stamp);
    let inm = match rng.u8(0..10) {
        0 | 1 => format!("If-None-Match: {etag}\r\n"),
        2 => format!("If-None-Match: W/{etag}\r\n"),
        3 => format!("If-None-Match: \"a,b\", {etag}\r\n"),
        4 => format!("If-None-Match: \"x\"\r\nIf-None-Match: W/{etag}\r\n"),
        5 => "If-None-Match: *\r\n".to_owned(),
        6 => {
            let other = FileStamp {
                size: stamp.size + 1,
                ..stamp
            };
            format!("If-None-Match: {}\r\n", model_etag(other))
        }
        7 => "If-None-Match: \"x\"\r\n".to_owned(),
        8 => format!("If-None-Match: {}\r\n", etag.trim_matches('"')),
        _ => "If-None-Match: *, \"x\"\r\n".to_owned(),
    };
    let mtime = stamp.modified_ns / 1_000_000_000;
    let secs = match rng.u8(0..5) {
        0 | 1 => mtime,
        2 => mtime.saturating_sub(rng.u64(1..100_000)),
        3 => mtime + rng.u64(1..100),
        _ => NET_DATE.0 + rng.u64(1..1000),
    };
    let forms = date_forms(secs);
    let date = rng.choice(&forms).cloned().unwrap_or_default();
    let ims = match rng.u8(0..8) {
        0 => format!("If-Modified-Since: {date}\r\nIf-Modified-Since: {date}\r\n"),
        1 => format!("If-Modified-Since: {}\r\n", date.replace("GMT", "UTC")),
        _ => format!("If-Modified-Since: {date}\r\n"),
    };
    match rng.u8(0..4) {
        0 => inm,
        1 => ims,
        // A tag that does not match against a date that would.
        2 => format!(
            "If-Modified-Since: {}\r\nIf-None-Match: \"x\"\r\n",
            forms[0]
        ),
        _ => format!("{ims}{inm}"),
    }
}

/// What the conditional requests reached, summed over all seeds.
#[derive(Default)]
pub(crate) struct ConditionalCoverage {
    pub(crate) by_tag: usize,
    pub(crate) by_weak_tag: usize,
    pub(crate) by_star: usize,
    pub(crate) by_date: usize,
    pub(crate) by_rfc850_date: usize,
    pub(crate) by_asctime_date: usize,
    pub(crate) head_304: usize,
    pub(crate) closing_304: usize,
    /// 200s for a request with a well-formed `If-None-Match` that did not
    /// match.
    pub(crate) tag_mismatch: usize,
    /// 200s despite a malformed `If-None-Match` or `If-Modified-Since`.
    pub(crate) malformed_ignored: usize,
    /// 200s for an `If-Modified-Since` after the server's clock that
    /// would otherwise give a 304.
    pub(crate) future_ignored: usize,
    /// Both headers, `If-None-Match` deciding against what the date alone
    /// would give.
    pub(crate) tag_over_date: usize,
}

impl ConditionalCoverage {
    /// Count a complete response of `status` (200 or 304) to the file
    /// request `head` for a file with `stamp`.
    pub(crate) fn count(
        &mut self,
        head: &[u8],
        stamp: FileStamp,
        status: Status,
        persistence: Persistence,
    ) {
        let inm = model_inm(head);
        let dates = named(head, "if-modified-since");
        let date = dates.first().map(String::as_str).unwrap_or_default();
        let parsed = model_parse_date(date, NET_DATE.0);
        let mtime = stamp.modified_ns / 1_000_000_000;
        let date_alone = parsed.filter(|d| *d <= NET_DATE.0 && dates.len() == 1);
        if matches!(inm, ModelInm::Any | ModelInm::Tags(_)) {
            let by_date = date_alone.is_some_and(|d| mtime <= d);
            let by_tag = status == Status::NotModified;
            if !dates.is_empty() && by_date != by_tag {
                self.tag_over_date += 1;
            }
        }
        if status == Status::NotModified {
            {
                match &inm {
                    ModelInm::Any => self.by_star += 1,
                    ModelInm::Tags(tags) if tags.iter().any(|(_, weak)| *weak) => {
                        self.by_weak_tag += 1;
                    }
                    ModelInm::Tags(_) => self.by_tag += 1,
                    ModelInm::Absent | ModelInm::Malformed => {
                        self.by_date += 1;
                        if date.contains('-') {
                            self.by_rfc850_date += 1;
                        } else if !date.contains(',') {
                            self.by_asctime_date += 1;
                        }
                    }
                }
                if head.starts_with(b"HEAD ") {
                    self.head_304 += 1;
                }
                if persistence == Persistence::Close {
                    self.closing_304 += 1;
                }
            }
        } else {
            {
                match &inm {
                    ModelInm::Tags(_) => self.tag_mismatch += 1,
                    ModelInm::Malformed => self.malformed_ignored += 1,
                    ModelInm::Absent | ModelInm::Any => {}
                }
                if !dates.is_empty() && (parsed.is_none() || dates.len() > 1) {
                    self.malformed_ignored += 1;
                }
                if inm == ModelInm::Absent && parsed.is_some_and(|d| d > NET_DATE.0) {
                    self.future_ignored += 1;
                }
            }
        }
    }

    pub(crate) fn check(&self) {
        for (what, n) in [
            ("304 by a matching tag", self.by_tag),
            ("304 by a matching weak tag", self.by_weak_tag),
            ("304 by *", self.by_star),
            ("304 by If-Modified-Since", self.by_date),
            ("304 by an RFC 850 date", self.by_rfc850_date),
            ("304 by an asctime date", self.by_asctime_date),
            ("304 to HEAD", self.head_304),
            ("closing 304", self.closing_304),
            ("200 for a tag that does not match", self.tag_mismatch),
            (
                "200 despite a malformed conditional",
                self.malformed_ignored,
            ),
            (
                "200 despite a future If-Modified-Since",
                self.future_ignored,
            ),
            (
                "If-None-Match deciding against the date",
                self.tag_over_date,
            ),
        ] {
            assert!(n > 0, "no {what} in any seed");
        }
    }
}

#[test]
fn the_model_reads_the_three_date_forms() {
    for form in date_forms(784_111_777) {
        assert_eq!(
            model_parse_date(&form, 784_111_777),
            Some(784_111_777),
            "{form}"
        );
    }
    assert_eq!(
        date_forms(784_111_777),
        [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994"
        ]
    );
    // RFC 850's century: at most 50 years ahead of the clock.
    let at_1994 = 784_111_777;
    assert_eq!(model_year_of(at_1994), 1994);
    assert_eq!(
        model_parse_date("Monday, 01-Jan-44 00:00:00 GMT", at_1994).map(model_year_of),
        Some(2044)
    );
    assert_eq!(
        model_parse_date("Monday, 01-Jan-45 00:00:00 GMT", at_1994),
        None,
        "1945 is before 1970"
    );
}
