//! Conditional requests on static files (FR-STATIC-18, RFC 9110 sections
//! 8.8, 13.1.2, 13.1.3, 13.2.2): the validators a file response carries
//! (`ETag`, `Last-Modified`), what a request's `If-None-Match` and
//! `If-Modified-Since` say, and whether they make the answer a `304`.
//!
//! `If-Match`, `If-Unmodified-Since` and `If-Range` are ignored (owner
//! decision 2026-11-06): no `412`, no ranges.

use super::date::{ImfFixdate, imf_fixdate, parse_http_date};
use super::head_line::{trim_ows, values};
use super::request::Header;
use crate::file::FileStamp;

/// The longest `ETag` value: two quotes, a dash and two 16-digit hex
/// numbers.
pub const ETAG_MAX_LEN: usize = 35;

/// The most distinct entity tags of an `If-None-Match` that count (owner
/// decision 2026-11-07, FR-STATIC-18): later ones are ignored, so a
/// request's conditions stay small whatever its head holds.
pub const MAX_TAGS: usize = 16;

/// A file's strong entity tag, `"<size hex>-<modified_ns hex>"` (lowercase
/// hex, no leading zeros): it names the file's stamp, so any change of
/// size or mtime changes it (no inode, FR-STATIC-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityTag {
    bytes: [u8; ETAG_MAX_LEN],
    len: usize,
}

impl EntityTag {
    /// The entity tag of a file with `stamp`.
    pub fn of(stamp: FileStamp) -> EntityTag {
        let mut tag = EntityTag {
            bytes: [0; ETAG_MAX_LEN],
            len: 0,
        };
        tag.push(b'"');
        tag.push_hex(stamp.size);
        tag.push(b'-');
        tag.push_hex(stamp.modified_ns);
        tag.push(b'"');
        tag
    }

    /// The tag with its quotes, as the `ETag` header carries it.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }

    fn push(&mut self, b: u8) {
        if let Some(slot) = self.bytes.get_mut(self.len) {
            *slot = b;
            self.len += 1;
        }
    }

    fn push_hex(&mut self, value: u64) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let digits = (64 - value.leading_zeros()).div_ceil(4).max(1);
        for i in (0..digits).rev() {
            let nibble = usize::try_from((value >> (i * 4)) & 0xf).unwrap_or(0);
            self.push(HEX.get(nibble).copied().unwrap_or(b'0'));
        }
    }
}

/// What a file response says about the file's version: its `ETag` and its
/// `Last-Modified` (the IMF-fixdate of its mtime in whole seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validators {
    pub etag: EntityTag,
    pub last_modified: ImfFixdate,
}

impl Validators {
    /// The validators of a file with `stamp`.
    pub fn of(stamp: FileStamp) -> Validators {
        Validators {
            etag: EntityTag::of(stamp),
            last_modified: imf_fixdate(mtime_secs(stamp)),
        }
    }
}

/// The whole seconds of `stamp`'s mtime, as `Last-Modified` names them.
fn mtime_secs(stamp: FileStamp) -> u64 {
    stamp.modified_ns / 1_000_000_000
}

/// A request's `If-None-Match` (RFC 9110 section 13.1.2), all its lines
/// read as one list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfNoneMatch {
    /// No `If-None-Match` line.
    Absent,
    /// Present but not of the grammar: it matches nothing, and the
    /// request still "contains" it (RFC 9110 section 13.1.3), so
    /// `If-Modified-Since` is ignored (controller ruling 2026-11-06).
    Malformed,
    /// `*`: matches any file that exists.
    Any,
    /// A list of entity tags (possibly empty). Only the first
    /// [`MAX_TAGS`] distinct tags count, and of those only the stamps of
    /// the tags written exactly in this server's `ETag` form are kept,
    /// strong or weak alike: no other tag can match one of its files.
    Tags(Vec<FileStamp>),
}

/// The conditional headers of a request (FR-STATIC-18). A static route
/// evaluates them for a `GET` or `HEAD` of a regular file; every other
/// request ignores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conditional {
    pub if_none_match: IfNoneMatch,
    /// The date of the request's one `If-Modified-Since`, if it has
    /// exactly one line and that is a valid HTTP-date; otherwise `None`
    /// (ignored).
    pub if_modified_since: Option<u64>,
}

impl Conditional {
    /// No conditional headers.
    pub const NONE: Conditional = Conditional {
        if_none_match: IfNoneMatch::Absent,
        if_modified_since: None,
    };

    /// The conditional headers among `headers` (names lowercased).
    pub fn of(headers: &[Header]) -> Conditional {
        let mut dates = values(headers, "if-modified-since");
        let if_modified_since = match (dates.next(), dates.next()) {
            (Some(one), None) => parse_http_date(one),
            (None, _) | (Some(_), Some(_)) => None,
        };
        Conditional {
            if_none_match: if_none_match(headers),
            if_modified_since,
        }
    }

    /// Whether a file with `stamp` is answered `304 Not Modified` (RFC
    /// 9110 section 13.2.2, `GET` and `HEAD` only; the caller checks the
    /// method). A present `If-None-Match`, even a malformed one, decides
    /// alone: `*`, or a tag
    /// that matches the file's `ETag` by weak comparison, gives 304.
    /// Without it, `If-Modified-Since` gives 304 when the file's mtime in
    /// whole seconds is not after its date. That date counts only if it is
    /// not after `now` (Unix seconds; a later date is invalid, RFC 9110
    /// section 13.1.3) and only once the driver has set the time (`now`
    /// is `Some`).
    pub fn not_modified(&self, stamp: FileStamp, now: Option<u64>) -> Freshness {
        let not_modified = match &self.if_none_match {
            IfNoneMatch::Any => true,
            IfNoneMatch::Tags(stamps) => stamps.contains(&stamp),
            IfNoneMatch::Malformed => false,
            IfNoneMatch::Absent => match (self.if_modified_since, now) {
                (Some(date), Some(now)) if date <= now => mtime_secs(stamp) <= date,
                (Some(_), Some(_)) | (Some(_), None) | (None, _) => false,
            },
        };
        if not_modified {
            Freshness::NotModified
        } else {
            Freshness::Modified
        }
    }
}

/// Whether the client's copy is current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Send the file (200).
    Modified,
    /// The conditions say the client has it: `304 Not Modified`.
    NotModified,
}

/// Reads every `If-None-Match` line as one list: `*` alone (one line,
/// nothing else), or entity tags `[W/]"<etagc>*"` separated by commas with
/// optional whitespace (empty elements allowed, RFC 9110 section 5.6.1).
/// Anything else is malformed: it matches nothing.
fn if_none_match(headers: &[Header]) -> IfNoneMatch {
    let lines = || values(headers, "if-none-match");
    let mut count = lines();
    match (count.next(), count.next()) {
        (None, _) => return IfNoneMatch::Absent,
        (Some(b"*"), None) => return IfNoneMatch::Any,
        (Some(_), _) if lines().any(|line| line == b"*") => return IfNoneMatch::Malformed,
        (Some(_), _) => {}
    }
    // The first MAX_TAGS distinct tags count (weak comparison: `W/` and
    // its absence alike), of any form; later ones are only checked
    // against the grammar. Of those, only tags in this server's form are
    // kept: no allocation unless the request names one, and never more
    // than MAX_TAGS.
    let mut seen: [&[u8]; MAX_TAGS] = [&[]; MAX_TAGS];
    let mut distinct = 0;
    let mut stamps = Vec::new();
    for line in lines() {
        let mut rest = line;
        loop {
            rest = trim_ows(rest);
            match rest {
                [] => break,
                [b',', tail @ ..] => {
                    rest = tail;
                    continue;
                }
                _ => {}
            }
            let Some((opaque, tail)) = entity_tag(rest) else {
                return IfNoneMatch::Malformed;
            };
            let counted = seen.get(..distinct).unwrap_or_default();
            if distinct < MAX_TAGS && !counted.contains(&opaque) {
                if let Some(slot) = seen.get_mut(distinct) {
                    *slot = opaque;
                }
                distinct += 1;
                if let Some(stamp) = own_stamp(opaque) {
                    stamps.push(stamp);
                }
            }
            rest = trim_ows(tail);
            match rest {
                [] => break,
                [b',', tail @ ..] => rest = tail,
                _ => return IfNoneMatch::Malformed,
            }
        }
    }
    IfNoneMatch::Tags(stamps)
}

/// Splits one entity tag off the front of `bytes`: its opaque tag between
/// the quotes (the `W/` prefix dropped: weak comparison ignores it) and
/// what follows the closing quote.
fn entity_tag(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let bytes = bytes.strip_prefix(b"W/").unwrap_or(bytes);
    let inner = bytes.strip_prefix(b"\"")?;
    let close = inner.iter().position(|&b| b == b'"')?;
    let opaque = inner.get(..close)?;
    // etagc: `!`, `#` to `~` (obs-text never reaches here: the parser
    // rejects it).
    if !opaque
        .iter()
        .all(|&b| b == 0x21 || (0x23..=0x7e).contains(&b))
    {
        return None;
    }
    Some((opaque, inner.get(close + 1..)?))
}

/// The stamp an opaque tag names if it is written exactly as
/// [`EntityTag::of`] writes one (lowercase hex, no leading zeros), so that
/// equal stamps mean equal tags.
fn own_stamp(opaque: &[u8]) -> Option<FileStamp> {
    let dash = opaque.iter().position(|&b| b == b'-')?;
    let size = canonical_hex(opaque.get(..dash)?)?;
    let modified_ns = canonical_hex(opaque.get(dash + 1..)?)?;
    Some(FileStamp { size, modified_ns })
}

/// The value of `digits` if they are 1–16 lowercase hex digits without a
/// leading zero (`0` itself is one digit).
fn canonical_hex(digits: &[u8]) -> Option<u64> {
    let valid = matches!(digits.len(), 1..=16)
        && digits
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        && (digits.len() == 1 || digits.first() != Some(&b'0'));
    if !valid {
        return None;
    }
    digits.iter().try_fold(0u64, |n, &b| {
        let d = char::from(b).to_digit(16)?;
        Some((n << 4) | u64::from(d))
    })
}

#[cfg(test)]
mod tests;
