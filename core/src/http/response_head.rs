//! Strict, incremental parser for the head of a backend's HTTP/1.x response.
//!
//! Backend responses are untrusted: this parser is the defence against
//! response splitting and desynchronisation, so as for requests the rule is
//! "when in doubt, reject". Header lines are judged by exactly the rules of
//! the request parser (shared in `head_line`), the limits are judged before
//! any syntax, and a line is judged only once its LF is present. A judged
//! line's verdict never changes with more bytes, so a prefix of an input
//! yields `Incomplete` or the same verdict as the whole input.
//!
//! [`ResponseParser`] keeps its place between calls like
//! [`HeadParser`](super::request::HeadParser), so a head that arrives in
//! many pieces costs work linear in its length.
//!
//! The status line is `HTTP/1.1` or `HTTP/1.0`, SP, exactly three digits
//! 100–599, SP (required even when the reason is empty), a reason of
//! printable ASCII and HTAB, CRLF. `101 Switching Protocols` is rejected as
//! soon as its status line is judged. Any other 1xx head is reported as
//! [`ResponseStatus::Interim`] once complete: the caller drops those bytes
//! and parses what follows from `consumed` on with a **fresh** parser, which
//! applies the limits to the next head anew.

use crate::http::head_line::{
    ConnectionNamesFraming, DeclaredFraming, FramingError, LineError, LineScanner, Position,
    check_connection, check_line, check_unterminated, declared_framing, head_lines, parse_headers,
    split_header, to_string, values, work,
};
use crate::http::request::{HeadLimits, Header, Version};

/// The head of a backend response that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    /// 100–599.
    pub status: u16,
    /// Printable ASCII and HTAB only; may be empty.
    pub reason: String,
    pub headers: Vec<Header>,
    pub body: ResponseBody,
}

impl ResponseHead {
    /// Whether the head has a `Transfer-Encoding` header, whatever its
    /// value and whether or not a body follows (an HTTP/1.0 client must
    /// not get one, FR-PROXY-04).
    pub fn names_transfer_encoding(&self) -> bool {
        self.headers.iter().any(|h| h.name == "transfer-encoding")
    }
}

/// How the response body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseBody {
    /// No body: a response to HEAD, a 1xx, 204 or 304 status, or
    /// `Content-Length: 0`.
    None,
    /// Exactly this many bytes (more than 0).
    Length(u64),
    /// `Transfer-Encoding: chunked` (HTTP/1.1 only).
    Chunked,
    /// No framing header: the body ends when the backend closes.
    UntilClose,
}

/// The kind of request the response answers; a response to HEAD never has
/// a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Head,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseError {
    HeadTooLarge,
    TooManyHeaders,
    BadStatusLine,
    BadVersion,
    BadHeaderName,
    BadHeaderValue,
    BareLf,
    ObsFold,
    ContentLengthWithTransferEncoding,
    BadContentLength,
    DuplicateContentLength,
    UnsupportedTransferEncoding,
    /// A `Connection` header names `content-length`, `transfer-encoding`
    /// or `host` (FR-HTTP-09a).
    ConnectionNamesFraming,
    /// `101 Switching Protocols`: upgrades are not supported.
    SwitchingProtocols,
    /// More than [`MAX_INTERIM_HEADS`] interim heads in a row
    /// (FR-PROXY-18). The parser judges one head at a time, so its caller
    /// counts them and reports this.
    TooManyInterimHeads,
}

/// The most interim (1xx) heads a backend may send before its final head
/// (FR-PROXY-18, owner-confirmed): real backends send at most a `100` and
/// a `103`.
pub const MAX_INTERIM_HEADS: u8 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseStatus {
    Incomplete,
    /// A complete, valid interim (1xx other than 101) head of `consumed`
    /// bytes. The caller drops it and parses the bytes from `consumed` on
    /// with a fresh [`ResponseParser`].
    Interim {
        consumed: usize,
    },
    Complete {
        head: ResponseHead,
        consumed: usize,
    },
}

impl From<LineError> for ResponseError {
    fn from(e: LineError) -> Self {
        match e {
            LineError::HeadTooLarge => Self::HeadTooLarge,
            LineError::TooManyHeaders => Self::TooManyHeaders,
            LineError::BareLf => Self::BareLf,
            LineError::BadHeaderName => Self::BadHeaderName,
            LineError::BadHeaderValue => Self::BadHeaderValue,
            LineError::ObsFold => Self::ObsFold,
        }
    }
}

impl From<FramingError> for ResponseError {
    fn from(e: FramingError) -> Self {
        match e {
            FramingError::ContentLengthWithTransferEncoding => {
                Self::ContentLengthWithTransferEncoding
            }
            FramingError::BadContentLength => Self::BadContentLength,
            FramingError::DuplicateContentLength => Self::DuplicateContentLength,
            FramingError::UnsupportedTransferEncoding => Self::UnsupportedTransferEncoding,
        }
    }
}

impl From<ConnectionNamesFraming> for ResponseError {
    fn from(_: ConnectionNamesFraming) -> Self {
        Self::ConnectionNamesFraming
    }
}

/// Which line of the head the parser expects next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    StatusLine,
    /// The status line is valid; `count` header lines are valid so far.
    Headers {
        count: usize,
    },
}

/// Resumable state of the response-head parser.
///
/// Feed it every byte received so far with [`ResponseParser::advance`],
/// each call passing the previous bytes plus any new ones. Each byte is
/// searched for an LF once and each complete line is validated once; the
/// [`ResponseHead`] is built in one final pass when the blank line arrives.
///
/// Once `advance` returned `Interim`, `Complete` or an error, the parser is
/// spent: the next head (also the one after an interim head) needs a new
/// parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseParser {
    lines: LineScanner,
    stage: Stage,
}

impl Default for ResponseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseParser {
    /// A parser at the start of a response head.
    pub const fn new() -> Self {
        Self {
            lines: LineScanner::new(),
            stage: Stage::StatusLine,
        }
    }

    /// Continues parsing `buf`, all bytes of the response head received so
    /// far; `kind` is the request the response answers.
    ///
    /// `buf` must start with the bytes passed on earlier calls. Returns
    /// `Incomplete` until the blank line arrives; limit violations are
    /// reported as soon as they are certain. Only `max_head` and
    /// `max_headers` of `limits` apply.
    pub fn advance(
        &mut self,
        buf: &[u8],
        limits: &HeadLimits,
        kind: RequestKind,
    ) -> Result<ResponseStatus, ResponseError> {
        loop {
            let Some(line) = self.lines.next_line(buf) else {
                check_unterminated(buf.len(), limits)?;
                return Ok(ResponseStatus::Incomplete);
            };
            let position = match self.stage {
                Stage::StatusLine => Position::StartLine,
                Stage::Headers { count } => Position::Header { count },
            };
            check_line(&line, position, limits)?;

            match self.stage {
                Stage::StatusLine => {
                    let status = parse_status_line(line.bytes)?;
                    if status.code == 101 {
                        return Err(ResponseError::SwitchingProtocols);
                    }
                    self.stage = Stage::Headers { count: 0 };
                }
                Stage::Headers { .. } if line.bytes.is_empty() => {
                    return build_head(buf.get(..line.end).unwrap_or_default(), kind);
                }
                Stage::Headers { count } => {
                    split_header(line.bytes)?;
                    self.stage = Stage::Headers {
                        count: count.saturating_add(1),
                    };
                }
            }
            self.lines.accept(&line);
        }
    }
}

/// A status line that passed [`parse_status_line`].
struct StatusLine<'a> {
    version: Version,
    code: u16,
    reason: &'a [u8],
}

fn digit_value(d: u8) -> u16 {
    u16::from(d.saturating_sub(b'0'))
}

fn parse_status_line(line: &[u8]) -> Result<StatusLine<'_>, ResponseError> {
    let sp = line
        .iter()
        .position(|&b| b == b' ')
        .ok_or(ResponseError::BadStatusLine)?;
    let version = match line.get(..sp).unwrap_or_default() {
        b"HTTP/1.1" => Version::Http11,
        b"HTTP/1.0" => Version::Http10,
        _ => return Err(ResponseError::BadVersion),
    };
    let rest = line.get(sp.saturating_add(1)..).unwrap_or_default();
    let [d1, d2, d3, b' ', reason @ ..] = rest else {
        return Err(ResponseError::BadStatusLine);
    };
    if ![d1, d2, d3].iter().all(|d| d.is_ascii_digit()) {
        return Err(ResponseError::BadStatusLine);
    }
    let code = digit_value(*d1) * 100 + digit_value(*d2) * 10 + digit_value(*d3);
    if !(100..=599).contains(&code) {
        return Err(ResponseError::BadStatusLine);
    }
    if !reason
        .iter()
        .all(|&b| b == b'\t' || (0x20..=0x7E).contains(&b))
    {
        return Err(ResponseError::BadStatusLine);
    }
    Ok(StatusLine {
        version,
        code,
        reason,
    })
}

/// Builds the result for `head`, a complete head (blank line included)
/// whose lines [`ResponseParser::advance`] has already judged valid, and
/// applies the checks that need all headers.
///
/// As for requests, those run after every per-line check: first
/// `Connection` naming a framing header or `Host` (FR-HTTP-09a), then
/// `Transfer-Encoding` in HTTP/1.0, then the framing headers. All of them
/// apply to every head, whatever its status or request kind (FR-PROXY-16).
fn build_head(head: &[u8], kind: RequestKind) -> Result<ResponseStatus, ResponseError> {
    work::built(head.len());
    let mut lines = head_lines(head);
    let status = parse_status_line(lines.next().unwrap_or_default())?;
    let headers = parse_headers(lines)?;
    check_connection(&headers)?;
    if status.version == Version::Http10 && values(&headers, "transfer-encoding").next().is_some() {
        return Err(ResponseError::UnsupportedTransferEncoding);
    }
    // Framing headers are checked even where the status allows no body.
    let declared = declared_framing(&headers)?;
    if (100..=199).contains(&status.code) {
        return Ok(ResponseStatus::Interim {
            consumed: head.len(),
        });
    }
    let body = match (kind, status.code, declared) {
        (RequestKind::Head, _, _) | (RequestKind::Other, 204 | 304, _) => ResponseBody::None,
        (RequestKind::Other, _, DeclaredFraming::Length(0)) => ResponseBody::None,
        (RequestKind::Other, _, DeclaredFraming::Length(n)) => ResponseBody::Length(n),
        (RequestKind::Other, _, DeclaredFraming::Chunked) => ResponseBody::Chunked,
        (RequestKind::Other, _, DeclaredFraming::Absent) => ResponseBody::UntilClose,
    };
    Ok(ResponseStatus::Complete {
        head: ResponseHead {
            status: status.code,
            reason: to_string(status.reason),
            headers,
            body,
        },
        consumed: head.len(),
    })
}

#[cfg(test)]
mod tests;
