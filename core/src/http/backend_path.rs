//! Backend file paths (FR-PROXY-32 Path, Bad path): turns an `X-Sendfile`
//! or `X-Accel-Redirect` value into the segments to open below the route's
//! root, a `404`, or a bad path (the backend misbehaving, a `502`).
//!
//! Safe on arbitrary bytes. The checks run in this order, each over the
//! whole value before the next starts:
//!
//! 1. an empty value is [`BadPath::Empty`];
//! 2. a byte outside visible ASCII (0x21 to 0x7E) is [`BadPath::BadByte`];
//! 3. a `?` is [`BadPath::Query`];
//! 4. the start must match the root (`x-sendfile`) or the prefix
//!    (`x-accel-redirect`), else [`BadPath::NoMatch`];
//! 5. the rest's raw segments: an empty segment other than the trailing
//!    one, `.` or `..` is [`BadPath::BadSegment`];
//! 6. `x-accel-redirect` only: each segment is percent-decoded once
//!    (FR-STATIC-02); a malformed escape is [`BadPath::BadEscape`];
//! 7. the static rules: a segment starting with `.`, or holding a decoded
//!    `/` or NUL, is [`Located::NotFound`] (FR-STATIC-09).
//!
//! Step 5 sees the raw segments, so `%2e%2e` passes it and is a dotfile
//! (404) at step 7, while a raw `..` is a bad path (502).

pub use super::static_path::TrailingSlash;
use super::static_path::{StaticPathError, decode_segment};

/// Where a value that is not a bad path points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// Open these segments below the root. No segment is empty, `.` or
    /// `..`, starts with `.`, or holds `/` or NUL. No segments at all
    /// means the root itself (a directory, so a 404 later).
    Open {
        segments: Vec<Vec<u8>>,
        /// Whether the value ended in `/` after the root or prefix.
        slash: TrailingSlash,
    },
    /// A name a static route answers 404 for (FR-STATIC-09): a dotfile, or
    /// a decoded `/` or NUL.
    NotFound,
}

/// Why a value is a bad path (FR-PROXY-32: a `502`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadPath {
    /// The value is empty.
    Empty,
    /// A byte outside visible ASCII (0x21 to 0x7E).
    BadByte,
    /// A `?`.
    Query,
    /// The value does not start with the route's root or prefix.
    NoMatch,
    /// An empty segment other than the trailing one, or a raw `.` or `..`.
    BadSegment,
    /// `x-accel-redirect`: a `%` not followed by two hex digits.
    BadEscape,
}

/// An `X-Sendfile` value: a file-system path, taken as its bytes (never
/// decoded), that is `root` followed by `/` and the file's path, or `root`
/// alone. `root` is an `internal_files` root (FR-CFG-26: never `/`).
pub fn sendfile_path(root: &[u8], value: &[u8]) -> Result<Located, BadPath> {
    check_bytes(value)?;
    let (rest, slash) = if value == root {
        (&b""[..], TrailingSlash::Absent)
    } else {
        let rest = value
            .strip_prefix(root)
            .and_then(|after| after.strip_prefix(b"/"))
            .ok_or(BadPath::NoMatch)?;
        // The `/` after the root counts as a trailing one when nothing
        // follows it.
        let slash = match rest.last() {
            None | Some(b'/') => TrailingSlash::Present,
            Some(_) => TrailingSlash::Absent,
        };
        (rest, slash)
    };
    let segments = raw_segments(rest)?
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    Ok(locate(segments, slash))
}

/// An `X-Accel-Redirect` value: `prefix` (raw bytes, case-sensitive; it
/// starts and ends with `/`, FR-CFG-26) followed by the file's path below
/// the root, percent-decoded once.
pub fn accel_path(prefix: &[u8], value: &[u8]) -> Result<Located, BadPath> {
    check_bytes(value)?;
    let rest = value.strip_prefix(prefix).ok_or(BadPath::NoMatch)?;
    let slash = match rest.last() {
        Some(b'/') => TrailingSlash::Present,
        None | Some(_) => TrailingSlash::Absent,
    };
    let segments = raw_segments(rest)?
        .into_iter()
        .map(decode)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(locate(segments, slash))
}

/// Steps 1 to 3: empty, a byte outside visible ASCII, a `?`.
fn check_bytes(value: &[u8]) -> Result<(), BadPath> {
    if value.is_empty() {
        return Err(BadPath::Empty);
    }
    if !value.iter().all(|b| (0x21..=0x7E).contains(b)) {
        return Err(BadPath::BadByte);
    }
    if value.contains(&b'?') {
        return Err(BadPath::Query);
    }
    Ok(())
}

/// Step 5: splits `rest` (what follows the root's `/` or the prefix) into
/// raw segments, dropping the empty trailing one. An empty `rest` has none.
fn raw_segments(rest: &[u8]) -> Result<Vec<&[u8]>, BadPath> {
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut segments = rest.split(|&b| b == b'/').peekable();
    while let Some(segment) = segments.next() {
        match segment {
            b"" if segments.peek().is_none() => {}
            b"" | b"." | b".." => return Err(BadPath::BadSegment),
            _ => out.push(segment),
        }
    }
    Ok(out)
}

/// Step 6: one percent-decoding pass over a raw segment.
fn decode(segment: &[u8]) -> Result<Vec<u8>, BadPath> {
    decode_segment(segment).map_err(|err| match err {
        StaticPathError::BadEscape => BadPath::BadEscape,
        // `decode_segment` reports nothing else; were it to, the value is
        // still not a path to open.
        StaticPathError::DotSegment | StaticPathError::BadSegment => BadPath::BadEscape,
    })
}

/// Step 7: the static rules (FR-STATIC-09).
fn locate(segments: Vec<Vec<u8>>, slash: TrailingSlash) -> Located {
    let hidden = |segment: &Vec<u8>| {
        segment.first() == Some(&b'.') || segment.iter().any(|&b| b == b'/' || b == 0)
    };
    if segments.iter().any(hidden) {
        Located::NotFound
    } else {
        Located::Open { segments, slash }
    }
}

#[cfg(test)]
mod tests;
