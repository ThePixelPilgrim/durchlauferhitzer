//! Line scanning and header-line rules shared by the request-head parser
//! ([`super::request`]) and the backend response-head parser
//! ([`super::response_head`]).
//!
//! Both parsers judge a head line by line: a line is looked at only once its
//! LF is present, the limits are applied before any syntax check, and a
//! judged line's verdict never changes with more bytes. Keeping these rules
//! in one place means a request header and a response header are judged by
//! exactly the same code.

use super::request::{HeadLimits, Header};

/// Test-only work counters for the head parsers, per thread.
///
/// `scanned` counts bytes searched for an LF, `judged` the bytes of complete
/// lines handed to line validation, and `built` the bytes of finished heads
/// turned into a head value.
#[cfg(test)]
pub(crate) mod work {
    use std::cell::Cell;

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) struct Work {
        pub(crate) scanned: usize,
        pub(crate) judged: usize,
        pub(crate) built: usize,
    }

    thread_local! {
        static WORK: Cell<Work> = const {
            Cell::new(Work { scanned: 0, judged: 0, built: 0 })
        };
    }

    fn update(f: impl FnOnce(&mut Work)) {
        WORK.with(|cell| {
            let mut w = cell.get();
            f(&mut w);
            cell.set(w);
        });
    }

    pub(crate) fn scanned(n: usize) {
        update(|w| w.scanned = w.scanned.saturating_add(n));
    }

    pub(crate) fn judged(n: usize) {
        update(|w| w.judged = w.judged.saturating_add(n));
    }

    pub(crate) fn built(n: usize) {
        update(|w| w.built = w.built.saturating_add(n));
    }

    /// Returns the counts so far and resets them.
    pub(crate) fn take() -> Work {
        WORK.with(|cell| cell.replace(Work::default()))
    }
}

/// No-op stand-ins outside tests.
#[cfg(not(test))]
pub(crate) mod work {
    pub(crate) fn scanned(_: usize) {}
    pub(crate) fn judged(_: usize) {}
    pub(crate) fn built(_: usize) {}
}

/// A violation of the line rules shared by both heads; each parser maps it
/// onto its own error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineError {
    HeadTooLarge,
    TooManyHeaders,
    BareLf,
    BadHeaderName,
    BadHeaderValue,
    ObsFold,
}

/// A violation of the body-framing header rules shared by both heads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FramingError {
    ContentLengthWithTransferEncoding,
    BadContentLength,
    DuplicateContentLength,
    UnsupportedTransferEncoding,
}

/// How a line ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    CrLf,
    BareLf,
}

/// A complete line found by [`LineScanner::next_line`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Line<'a> {
    /// The line without its CR LF (or bare LF).
    pub(crate) bytes: &'a [u8],
    /// Offset in the buffer just past the LF.
    pub(crate) end: usize,
    pub(crate) ending: Ending,
}

/// Where a line sits in the head, for the header-count limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Position {
    /// The request line or status line.
    StartLine,
    /// A line after the start line, with `count` valid header lines before
    /// it.
    Header { count: usize },
}

/// Finds the lines of a head that arrives in pieces, searching each byte
/// for an LF once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineScanner {
    /// Start of the first line not yet judged.
    judged: usize,
    /// `buf[judged..scanned]` is known to hold no LF.
    scanned: usize,
}

impl LineScanner {
    pub(crate) const fn new() -> Self {
        Self {
            judged: 0,
            scanned: 0,
        }
    }

    /// The next complete line of `buf` after the lines already accepted, or
    /// `None` if its LF has not arrived yet. `buf` must start with the bytes
    /// passed on earlier calls.
    pub(crate) fn next_line<'a>(&mut self, buf: &'a [u8]) -> Option<Line<'a>> {
        let unscanned = buf.get(self.scanned..).unwrap_or_default();
        let found = unscanned.iter().position(|&b| b == b'\n');
        work::scanned(found.map_or(unscanned.len(), |i| i + 1));
        let Some(offset) = found else {
            self.scanned = self.scanned.max(buf.len());
            return None;
        };
        let lf = self.scanned.saturating_add(offset);
        let has_cr = lf > self.judged && buf.get(lf.saturating_sub(1)) == Some(&b'\r');
        let (line_end, ending) = if has_cr {
            (lf.saturating_sub(1), Ending::CrLf)
        } else {
            (lf, Ending::BareLf)
        };
        Some(Line {
            bytes: buf.get(self.judged..line_end).unwrap_or_default(),
            end: lf.saturating_add(1),
            ending,
        })
    }

    /// The number of bytes of `buf` from the start of the first line not
    /// yet accepted.
    pub(crate) fn pending(&self, buf: &[u8]) -> usize {
        buf.len().saturating_sub(self.judged)
    }

    /// Marks `line` (the last one returned) as judged valid.
    pub(crate) fn accept(&mut self, line: &Line<'_>) {
        self.judged = line.end;
        self.scanned = line.end;
    }
}

/// The limit for a head whose current line has no LF yet: with more than
/// `max_head` bytes received, the head is certainly too large.
pub(crate) fn check_unterminated(buf_len: usize, limits: &HeadLimits) -> Result<(), LineError> {
    if buf_len > limits.max_head {
        Err(LineError::HeadTooLarge)
    } else {
        Ok(())
    }
}

/// Applies the shared limits and the line-ending rule to a complete line, in
/// this order: head size, header count, bare LF. A line that passes goes on
/// to syntax checks.
pub(crate) fn check_line(
    line: &Line<'_>,
    position: Position,
    limits: &HeadLimits,
) -> Result<(), LineError> {
    if line.end > limits.max_head {
        return Err(LineError::HeadTooLarge);
    }
    match position {
        Position::Header { count } if !line.bytes.is_empty() && count >= limits.max_headers => {
            return Err(LineError::TooManyHeaders);
        }
        Position::StartLine | Position::Header { .. } => {}
    }
    match line.ending {
        Ending::BareLf => return Err(LineError::BareLf),
        Ending::CrLf => {}
    }
    work::judged(line.bytes.len());
    Ok(())
}

/// The lines of a complete head, each without its CR LF; only for heads
/// whose lines were already judged.
pub(crate) fn head_lines(head: &[u8]) -> impl Iterator<Item = &[u8]> {
    head.split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

pub(crate) fn is_token(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| is_tchar(b))
}

pub(crate) fn to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn is_ows(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

pub(crate) fn trim_ows(mut bytes: &[u8]) -> &[u8] {
    while let [first, tail @ ..] = bytes {
        if !is_ows(*first) {
            break;
        }
        bytes = tail;
    }
    while let [head @ .., last] = bytes {
        if !is_ows(*last) {
            break;
        }
        bytes = head;
    }
    bytes
}

/// Validates a header line and splits it into name and raw value, without
/// allocating.
pub(crate) fn split_header(line: &[u8]) -> Result<(&[u8], &[u8]), LineError> {
    if line.first().is_some_and(|&b| is_ows(b)) {
        return Err(LineError::ObsFold);
    }
    let colon = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(LineError::BadHeaderName)?;
    let name = line.get(..colon).unwrap_or_default();
    let value = line.get(colon + 1..).unwrap_or_default();
    if !is_token(name) {
        return Err(LineError::BadHeaderName);
    }
    let valid_value = |&b: &u8| b == b'\t' || b == b' ' || (0x21..=0x7E).contains(&b) || b >= 0x80;
    if !value.iter().all(valid_value) {
        return Err(LineError::BadHeaderValue);
    }
    Ok((name, value))
}

fn parse_header(line: &[u8]) -> Result<Header, LineError> {
    let (name, value) = split_header(line)?;
    Ok(Header {
        name: to_string(name).to_ascii_lowercase(),
        value: trim_ows(value).to_vec(),
    })
}

/// Parses the header lines from `lines` up to the blank line.
pub(crate) fn parse_headers<'a>(
    lines: impl Iterator<Item = &'a [u8]>,
) -> Result<Vec<Header>, LineError> {
    let mut headers = Vec::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        headers.push(parse_header(line)?);
    }
    Ok(headers)
}

pub(crate) fn values<'a>(headers: &'a [Header], name: &'a str) -> impl Iterator<Item = &'a [u8]> {
    headers
        .iter()
        .filter(move |h| h.name == name)
        .map(|h| h.value.as_slice())
}

/// The body framing the headers declare, before any version- or
/// status-specific rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclaredFraming {
    /// Neither `Content-Length` nor `Transfer-Encoding`.
    Absent,
    /// One valid `Content-Length` (possibly 0).
    Length(u64),
    /// `Transfer-Encoding: chunked` and nothing else.
    Chunked,
}

/// Checks the framing headers: no `Content-Length` together with
/// `Transfer-Encoding`, at most one `Content-Length` of decimal digits that
/// fits a `u64`, and `Transfer-Encoding` only as a single `chunked`.
pub(crate) fn declared_framing(headers: &[Header]) -> Result<DeclaredFraming, FramingError> {
    let lengths: Vec<&[u8]> = values(headers, "content-length").collect();
    let encodings: Vec<&[u8]> = values(headers, "transfer-encoding").collect();
    if !lengths.is_empty() && !encodings.is_empty() {
        return Err(FramingError::ContentLengthWithTransferEncoding);
    }
    if let Some(&first) = lengths.first() {
        if lengths.len() > 1 {
            return Err(FramingError::DuplicateContentLength);
        }
        if first.is_empty() || !first.iter().all(u8::is_ascii_digit) {
            return Err(FramingError::BadContentLength);
        }
        let n: u64 = to_string(first)
            .parse()
            .map_err(|_| FramingError::BadContentLength)?;
        return Ok(DeclaredFraming::Length(n));
    }
    match encodings.as_slice() {
        [] => Ok(DeclaredFraming::Absent),
        [one] if one.eq_ignore_ascii_case(b"chunked") => Ok(DeclaredFraming::Chunked),
        _ => Err(FramingError::UnsupportedTransferEncoding),
    }
}
