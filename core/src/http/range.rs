//! Range requests on files (FR-STATIC-19, RFC 9110 sections 13.1.5, 14):
//! a request's one `Range` and its `If-Range`, read leniently, and
//! whether a file is answered whole, with one part (`206`) or `416`.
//!
//! Only a single `bytes` range is served. Several ranges (no
//! `multipart/byteranges`), several `Range` lines, a malformed `Range` or
//! another unit get the whole file, as RFC 9110 allows; so does every
//! method but `GET`. `If-Range` keeps the range only for the file's own
//! strong `ETag` or exactly its `Last-Modified` bytes, on one line
//! (planner calls, stage 8 plan).

use super::conditional::{EntityTag, Validators, entity_tag, own_stamp};
use super::date::ImfFixdate;
use super::head_line::trim_ows;
use super::request::{Header, Method};
use crate::file::FileStamp;

/// A request's `Range` and `If-Range` (FR-STATIC-19): fixed-size, so the
/// request keeps it without allocating whatever its head holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeRequest {
    range: RangeSpec,
    if_range: IfRange,
}

/// The one `bytes` range a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeSpec {
    /// No `Range` line.
    None,
    /// `a-b` (`b` not below `a`).
    FromTo(u64, u64),
    /// `a-`.
    From(u64),
    /// `-n`: the last `n` bytes.
    Suffix(u64),
    /// Not served: several ranges, a malformed one, another unit or
    /// several `Range` lines. The whole file is sent.
    Ignored,
}

/// A request's `If-Range` (RFC 9110 section 13.1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IfRange {
    /// No `If-Range` line.
    Absent,
    /// A strong tag in this server's `ETag` form: the stamp it names.
    Tag(FileStamp),
    /// A value of an IMF-fixdate's length: matches only a file whose
    /// `Last-Modified` has exactly these bytes.
    Date(ImfFixdate),
    /// Anything else (a weak or foreign tag, another date form, several
    /// lines): matches no file.
    Never,
}

/// How a file is answered, by the request's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ranged {
    /// The whole file (`200`).
    Whole,
    /// Bytes `first` to `last` (inclusive) of the file (`206`).
    Part { first: u64, last: u64 },
    /// The range does not overlap the file (`416`).
    Unsatisfiable,
}

impl RangeRequest {
    /// No `Range` line.
    pub const NONE: RangeRequest = RangeRequest {
        range: RangeSpec::None,
        if_range: IfRange::Absent,
    };

    /// The `Range` and `If-Range` among `headers` (names lowercased), in
    /// one pass over them.
    pub fn of(headers: &[Header]) -> RangeRequest {
        let mut range = Lines::None;
        let mut if_range = Lines::None;
        for header in headers {
            let lines = match header.name.as_str() {
                "range" => &mut range,
                "if-range" => &mut if_range,
                _ => continue,
            };
            *lines = match lines {
                Lines::None => Lines::One(&header.value),
                Lines::One(_) | Lines::Several => Lines::Several,
            };
        }
        let range = match range {
            Lines::None => RangeSpec::None,
            Lines::One(value) => range_spec(value),
            // Several lines are read like several ranges (planner call).
            Lines::Several => RangeSpec::Ignored,
        };
        let if_range = match if_range {
            Lines::None => IfRange::Absent,
            Lines::One(value) => if_range_of(value),
            Lines::Several => IfRange::Never,
        };
        RangeRequest { range, if_range }
    }

    /// How a file with `validators`, `size` bytes long, answers a request
    /// with `method` (after the conditional checks: a `304` wins,
    /// FR-STATIC-19). Only a `GET` is ranged. An `If-Range` that does not
    /// match sends the whole file, even for a range that would not be
    /// satisfiable (RFC 9110 section 13.1.5).
    pub fn judge(&self, method: &Method, validators: &Validators, size: u64) -> Ranged {
        match method {
            Method::Get => {}
            Method::Head | Method::Other(_) => return Ranged::Whole,
        }
        let (first, last) = match self.range {
            RangeSpec::None | RangeSpec::Ignored => return Ranged::Whole,
            RangeSpec::FromTo(first, last) => (first, last),
            RangeSpec::From(first) => (first, u64::MAX),
            RangeSpec::Suffix(len) => (size.saturating_sub(len), u64::MAX),
        };
        let kept = match self.if_range {
            IfRange::Absent => true,
            IfRange::Tag(stamp) => EntityTag::of(stamp) == validators.etag,
            IfRange::Date(date) => date == validators.last_modified,
            IfRange::Never => false,
        };
        if !kept {
            return Ranged::Whole;
        }
        // A suffix of no bytes, and every range of an empty file, overlap
        // nothing (the planner call: an empty file's ranges are all 416).
        let empty_suffix = matches!(self.range, RangeSpec::Suffix(0));
        match size.checked_sub(1) {
            Some(end) if first <= end && !empty_suffix => Ranged::Part {
                first,
                last: last.min(end),
            },
            Some(_) | None => Ranged::Unsatisfiable,
        }
    }
}

/// How many lines of one header a request has.
enum Lines<'h> {
    None,
    One(&'h [u8]),
    Several,
}

/// Reads one `Range` value (RFC 9110 section 14.2): `bytes` (any case),
/// `=`, then a list with exactly one element (empty elements and OWS
/// around elements allowed, section 5.6.1), `a-b` (`b` not below `a`),
/// `a-` or `-n`, digits only, each number within `u64`.
fn range_spec(value: &[u8]) -> RangeSpec {
    let Some(eq) = value.iter().position(|&b| b == b'=') else {
        return RangeSpec::Ignored;
    };
    let (unit, set) = (value.get(..eq), value.get(eq + 1..));
    let (Some(unit), Some(set)) = (unit, set) else {
        return RangeSpec::Ignored;
    };
    if !unit.eq_ignore_ascii_case(b"bytes") {
        return RangeSpec::Ignored;
    }
    let mut elements = set
        .split(|&b| b == b',')
        .map(trim_ows)
        .filter(|e| !e.is_empty());
    let (Some(one), None) = (elements.next(), elements.next()) else {
        return RangeSpec::Ignored;
    };
    let Some(dash) = one.iter().position(|&b| b == b'-') else {
        return RangeSpec::Ignored;
    };
    let (Some(first), Some(last)) = (one.get(..dash), one.get(dash + 1..)) else {
        return RangeSpec::Ignored;
    };
    match (number(first), number(last)) {
        (Number::Valid(first), Number::Valid(last)) if first <= last => {
            RangeSpec::FromTo(first, last)
        }
        (Number::Valid(first), Number::Empty) => RangeSpec::From(first),
        (Number::Empty, Number::Valid(len)) => RangeSpec::Suffix(len),
        (Number::Valid(_), Number::Valid(_))
        | (Number::Empty | Number::Invalid, Number::Empty | Number::Invalid)
        | (Number::Valid(_), Number::Invalid)
        | (Number::Invalid, Number::Valid(_)) => RangeSpec::Ignored,
    }
}

/// One side of a range's dash.
enum Number {
    Empty,
    Valid(u64),
    /// A byte that is not a digit, or a number over `u64`.
    Invalid,
}

fn number(digits: &[u8]) -> Number {
    if digits.is_empty() {
        return Number::Empty;
    }
    let value = digits.iter().try_fold(0u64, |n, &b| {
        if !b.is_ascii_digit() {
            return None;
        }
        n.checked_mul(10)?.checked_add(u64::from(b - b'0'))
    });
    match value {
        Some(n) => Number::Valid(n),
        None => Number::Invalid,
    }
}

/// Reads one `If-Range` value: a strong tag in this server's `ETag` form
/// (nothing after it), or a value as long as an IMF-fixdate, compared byte
/// for byte with `Last-Modified` later; anything else never matches.
fn if_range_of(value: &[u8]) -> IfRange {
    if value.starts_with(b"\"") {
        return match entity_tag(value) {
            Some((opaque, [])) => own_stamp(opaque).map_or(IfRange::Never, IfRange::Tag),
            Some(_) | None => IfRange::Never,
        };
    }
    match <ImfFixdate>::try_from(value) {
        Ok(date) => IfRange::Date(date),
        Err(_) => IfRange::Never,
    }
}

#[cfg(test)]
mod tests;
