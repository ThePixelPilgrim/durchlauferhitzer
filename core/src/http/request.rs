//! Strict, incremental parser for the HTTP/1.x request head.
//!
//! This is the main request-smuggling attack surface, so the rule is "when in
//! doubt, reject". Callers pass every byte received so far and get
//! `Incomplete` until the blank line arrives. A line is judged only once its
//! terminating LF is present, and a judged line's verdict never changes with
//! more bytes, so a prefix of an input yields `Incomplete` or the same error
//! as the whole input.
//!
//! [`HeadParser`] keeps its place between calls, so a head that arrives in
//! many pieces costs work linear in its length; [`parse_head`] is the
//! stateless form for a buffer parsed in one go.

use crate::config::model::Limits;
use crate::http::head_line::{
    ConnectionNamesFraming, DeclaredFraming, FramingError, LineError, LineScanner, Position,
    check_connection, check_line, check_unterminated, connection_tokens, declared_framing,
    head_lines, is_token, parse_headers, split_header, to_string, values,
};
// The work counters are shared by both head parsers; tests elsewhere reach
// them through this module.
#[cfg(test)]
pub(crate) use crate::http::head_line::work;
#[cfg(not(test))]
use crate::http::head_line::work;
use crate::http::target::{Target, TargetError, parse_target};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Other(String),
}

/// What a [`HeadParser`] knows of the request's method: nothing until the
/// request line has been judged valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownMethod {
    Unknown,
    Get,
    Head,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Lowercased.
    pub name: String,
    /// Optional whitespace trimmed on both sides.
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFraming {
    None,
    Length(u64),
    Chunked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    KeepAlive,
    Close,
}

/// What the client expects before it sends the body (FR-PROXY-25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// No `Expect` header, or an HTTP/1.0 request (which has none).
    None,
    /// Exactly one `Expect: 100-continue` (any case) on an HTTP/1.1
    /// request.
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: Method,
    pub target: Target,
    pub version: Version,
    /// Lowercased, including the port if one was sent.
    pub host: Option<String>,
    pub headers: Vec<Header>,
    pub body: BodyFraming,
    pub persistence: Persistence,
    pub expect: Expect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpError {
    RequestLineTooLong,
    HeadTooLarge,
    TooManyHeaders,
    BadRequestLine,
    BadMethod,
    BadVersion,
    UnsupportedVersion,
    BadTarget(TargetError),
    BareLf,
    BadHeaderName,
    BadHeaderValue,
    ObsFold,
    MissingHost,
    DuplicateHost,
    BadHost,
    ContentLengthWithTransferEncoding,
    BadContentLength,
    DuplicateContentLength,
    UnsupportedTransferEncoding,
    TransferEncodingInHttp10,
    /// A `Connection` header names `content-length`, `transfer-encoding`
    /// or `host` (FR-HTTP-09a).
    ConnectionNamesFraming,
    /// An HTTP/1.1 `Expect` other than exactly one `100-continue`: another
    /// value, a list, or more than one `Expect` line (FR-PROXY-25) → 417.
    BadExpect,
}

impl From<LineError> for HttpError {
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

impl From<FramingError> for HttpError {
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

impl From<ConnectionNamesFraming> for HttpError {
    fn from(_: ConnectionNamesFraming) -> Self {
        Self::ConnectionNamesFraming
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadLimits {
    pub max_request_line: usize,
    pub max_head: usize,
    pub max_headers: usize,
}

impl From<&Limits> for HeadLimits {
    fn from(limits: &Limits) -> Self {
        Self {
            max_request_line: limits.max_request_line,
            max_head: limits.max_head,
            max_headers: limits.max_headers,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseStatus {
    Incomplete,
    Complete { head: RequestHead, consumed: usize },
}

/// Parses the request head from all bytes received so far.
///
/// Returns `Incomplete` until the blank line arrives; limit violations are
/// reported as soon as they are certain. Lines are judged only once their LF
/// is present, so a prefix never contradicts the full input.
///
/// This is the stateless form of [`HeadParser`]; callers that receive a head
/// in pieces keep a `HeadParser` instead, so each byte is looked at a bounded
/// number of times however the head is split.
pub fn parse_head(buf: &[u8], limits: &HeadLimits) -> Result<ParseStatus, HttpError> {
    HeadParser::new().advance(buf, limits)
}

/// Which line of the head the parser expects next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    RequestLine,
    /// The request line is valid and named `method`; `count` header lines
    /// are valid so far.
    Headers {
        count: usize,
        method: KnownMethod,
    },
}

/// Resumable state of the request-head parser.
///
/// Feed it every byte received so far with [`HeadParser::advance`], each call
/// passing the previous bytes plus any new ones. It remembers how far it has
/// searched for an LF and which lines it has already judged, so each byte is
/// searched once and each complete line is validated once; the
/// [`RequestHead`] is built in one final pass when the blank line arrives.
/// The verdicts are exactly those of [`parse_head`] on the same bytes.
///
/// Once `advance` returned `Complete` or an error, the parser is spent: a new
/// head needs a new parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadParser {
    lines: LineScanner,
    stage: Stage,
}

impl Default for HeadParser {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadParser {
    /// A parser at the start of a head.
    pub const fn new() -> Self {
        Self {
            lines: LineScanner::new(),
            stage: Stage::RequestLine,
        }
    }

    /// The request's method once the request line was judged valid (also
    /// after a later error), [`KnownMethod::Unknown`] before that.
    pub fn method(&self) -> KnownMethod {
        match self.stage {
            Stage::RequestLine => KnownMethod::Unknown,
            Stage::Headers { method, .. } => method,
        }
    }

    /// Continues parsing `buf`, all bytes of the head received so far.
    ///
    /// `buf` must start with the bytes passed on earlier calls. Returns
    /// `Incomplete` until the blank line arrives; limit violations are
    /// reported as soon as they are certain.
    pub fn advance(&mut self, buf: &[u8], limits: &HeadLimits) -> Result<ParseStatus, HttpError> {
        loop {
            let Some(line) = self.lines.next_line(buf) else {
                return match self.stage {
                    // The request line decides between RequestLineTooLong
                    // and HeadTooLarge once its LF arrives, so until then
                    // only the request-line limit may fire (this keeps
                    // prefixes consistent for any limits, including
                    // max_head <= max_request_line).
                    Stage::RequestLine => {
                        if self.lines.pending(buf) >= limits.max_request_line.saturating_add(2) {
                            Err(HttpError::RequestLineTooLong)
                        } else {
                            Ok(ParseStatus::Incomplete)
                        }
                    }
                    Stage::Headers { .. } => {
                        check_unterminated(buf.len(), limits)?;
                        Ok(ParseStatus::Incomplete)
                    }
                };
            };

            let position = match self.stage {
                Stage::RequestLine => {
                    if line.bytes.len() > limits.max_request_line {
                        return Err(HttpError::RequestLineTooLong);
                    }
                    Position::StartLine
                }
                Stage::Headers { count, .. } => Position::Header { count },
            };
            check_line(&line, position, limits)?;

            match self.stage {
                Stage::RequestLine => {
                    let (method, _, _) = parse_request_line(line.bytes)?;
                    let method = match method {
                        Method::Get => KnownMethod::Get,
                        Method::Head => KnownMethod::Head,
                        Method::Other(_) => KnownMethod::Other,
                    };
                    self.stage = Stage::Headers { count: 0, method };
                }
                Stage::Headers { .. } if line.bytes.is_empty() => {
                    let head = build_head(buf.get(..line.end).unwrap_or_default())?;
                    return Ok(ParseStatus::Complete {
                        head,
                        consumed: line.end,
                    });
                }
                Stage::Headers { count, method } => {
                    split_header(line.bytes)?;
                    self.stage = Stage::Headers {
                        count: count.saturating_add(1),
                        method,
                    };
                }
            }
            self.lines.accept(&line);
        }
    }
}

/// Builds the request head from `head`, a complete head (blank line
/// included) whose lines [`HeadParser::advance`] has already judged valid,
/// and applies the checks that need all headers.
///
/// Those run after every per-line check, in this order, so the reported
/// error is deterministic: `Connection` naming a framing header or `Host`
/// (FR-HTTP-09a), then Host (FR-HTTP-07), then framing (FR-HTTP-08, in its
/// own order), then `Expect` (FR-PROXY-25). The `Connection` check comes first because it says the
/// header set itself is a smuggling attempt, which makes the Host and
/// framing verdicts on those very headers beside the point.
fn build_head(head: &[u8]) -> Result<RequestHead, HttpError> {
    work::built(head.len());
    let mut lines = head_lines(head);
    let (method, target, version) = parse_request_line(lines.next().unwrap_or_default())?;
    let headers = parse_headers(lines)?;
    check_connection(&headers)?;
    let host = check_host(&headers, version)?;
    let body = check_framing(&headers, version)?;
    let expect = check_expect(&headers, version)?;
    let persistence = persistence(&headers, version);
    Ok(RequestHead {
        method,
        target,
        version,
        host,
        headers,
        body,
        persistence,
        expect,
    })
}

fn parse_request_line(line: &[u8]) -> Result<(Method, Target, Version), HttpError> {
    let mut parts = line.split(|&b| b == b' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::BadRequestLine);
    };
    if !is_token(method) {
        return Err(HttpError::BadMethod);
    }
    let method = match method {
        b"GET" => Method::Get,
        b"HEAD" => Method::Head,
        other => Method::Other(to_string(other)),
    };
    let version = match version {
        b"HTTP/1.1" => Version::Http11,
        b"HTTP/1.0" => Version::Http10,
        [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
            if major.is_ascii_digit() && minor.is_ascii_digit() =>
        {
            return Err(HttpError::UnsupportedVersion);
        }
        _ => return Err(HttpError::BadVersion),
    };
    let target = parse_target(target).map_err(HttpError::BadTarget)?;
    Ok((method, target, version))
}

fn check_host(headers: &[Header], version: Version) -> Result<Option<String>, HttpError> {
    let mut hosts = values(headers, "host");
    let Some(value) = hosts.next() else {
        return match version {
            Version::Http11 => Err(HttpError::MissingHost),
            Version::Http10 => Ok(None),
        };
    };
    if hosts.next().is_some() {
        return Err(HttpError::DuplicateHost);
    }
    validate_host(value).map(Some)
}

fn is_port(bytes: &[u8]) -> bool {
    (1..=5).contains(&bytes.len())
        && bytes.iter().all(u8::is_ascii_digit)
        && to_string(bytes)
            .parse::<u32>()
            .is_ok_and(|p| (1..=65535).contains(&p))
}

/// Validates a `Host` value and returns it lowercased.
fn validate_host(value: &[u8]) -> Result<String, HttpError> {
    let port_ok = |port: Option<&[u8]>| port.is_none_or(is_port);
    let valid = if let Some(inner) = value.strip_prefix(b"[") {
        match inner.iter().position(|&b| b == b']') {
            Some(close) => {
                let literal = inner.get(..close).unwrap_or_default();
                let after = inner.get(close + 1..).unwrap_or_default();
                let port = match after {
                    [] => Some(None),
                    [b':', port @ ..] => Some(Some(port)),
                    _ => None,
                };
                !literal.is_empty()
                    && literal.contains(&b':')
                    && literal
                        .iter()
                        .all(|&b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
                    && port.is_some_and(port_ok)
            }
            None => false,
        }
    } else {
        let (name, port) = match value.iter().position(|&b| b == b':') {
            Some(i) => (value.get(..i).unwrap_or_default(), value.get(i + 1..)),
            None => (value, None),
        };
        !name.is_empty()
            && name
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            && port_ok(port)
    };
    if valid {
        Ok(to_string(value).to_ascii_lowercase())
    } else {
        Err(HttpError::BadHost)
    }
}

fn check_framing(headers: &[Header], version: Version) -> Result<BodyFraming, HttpError> {
    if version == Version::Http10 && values(headers, "transfer-encoding").next().is_some() {
        return Err(HttpError::TransferEncodingInHttp10);
    }
    Ok(match declared_framing(headers)? {
        DeclaredFraming::Absent | DeclaredFraming::Length(0) => BodyFraming::None,
        DeclaredFraming::Length(n) => BodyFraming::Length(n),
        DeclaredFraming::Chunked => BodyFraming::Chunked,
    })
}

/// FR-PROXY-25/25a: HTTP/1.0 ignores `Expect`; on HTTP/1.1 exactly one
/// `Expect` line whose whole value is `100-continue` (case-insensitive) is
/// `Continue`, anything else is an error ("when in doubt, reject").
fn check_expect(headers: &[Header], version: Version) -> Result<Expect, HttpError> {
    match version {
        Version::Http10 => Ok(Expect::None),
        Version::Http11 => {
            let mut expects = values(headers, "expect");
            match (expects.next(), expects.next()) {
                (None, _) => Ok(Expect::None),
                (Some(one), None) if one.eq_ignore_ascii_case(b"100-continue") => {
                    Ok(Expect::Continue)
                }
                (Some(_), _) => Err(HttpError::BadExpect),
            }
        }
    }
}

fn persistence(headers: &[Header], version: Version) -> Persistence {
    match version {
        Version::Http10 => Persistence::Close,
        Version::Http11 => {
            let close =
                connection_tokens(headers).any(|token| token.eq_ignore_ascii_case(b"close"));
            if close {
                Persistence::Close
            } else {
                Persistence::KeepAlive
            }
        }
    }
}

#[cfg(test)]
mod tests;
