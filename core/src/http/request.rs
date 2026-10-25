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
use crate::http::target::{Target, TargetError, parse_target};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Other(String),
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

/// Test-only work counters for the head parser, per thread.
///
/// `scanned` counts bytes searched for an LF, `judged` the bytes of complete
/// lines handed to line validation, and `built` the bytes of finished heads
/// turned into a [`RequestHead`].
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
mod work {
    pub(crate) fn scanned(_: usize) {}
    pub(crate) fn judged(_: usize) {}
    pub(crate) fn built(_: usize) {}
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
    /// The request line is valid; `count` header lines are valid so far.
    Headers {
        count: usize,
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
    /// Start of the first line not yet judged.
    judged: usize,
    /// `buf[judged..scanned]` is known to hold no LF.
    scanned: usize,
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
            judged: 0,
            scanned: 0,
            stage: Stage::RequestLine,
        }
    }

    /// Continues parsing `buf`, all bytes of the head received so far.
    ///
    /// `buf` must start with the bytes passed on earlier calls. Returns
    /// `Incomplete` until the blank line arrives; limit violations are
    /// reported as soon as they are certain.
    pub fn advance(&mut self, buf: &[u8], limits: &HeadLimits) -> Result<ParseStatus, HttpError> {
        loop {
            let unscanned = buf.get(self.scanned..).unwrap_or_default();
            let found = unscanned.iter().position(|&b| b == b'\n');
            work::scanned(found.map_or(unscanned.len(), |i| i + 1));
            let Some(offset) = found else {
                self.scanned = self.scanned.max(buf.len());
                let rest = buf.get(self.judged..).unwrap_or_default();
                return match self.stage {
                    // The request line decides between RequestLineTooLong
                    // and HeadTooLarge once its LF arrives, so until then
                    // only the request-line limit may fire (this keeps
                    // prefixes consistent for any limits, including
                    // max_head <= max_request_line).
                    Stage::RequestLine => {
                        if rest.len() >= limits.max_request_line.saturating_add(2) {
                            Err(HttpError::RequestLineTooLong)
                        } else {
                            Ok(ParseStatus::Incomplete)
                        }
                    }
                    Stage::Headers { .. } => {
                        if buf.len() > limits.max_head {
                            Err(HttpError::HeadTooLarge)
                        } else {
                            Ok(ParseStatus::Incomplete)
                        }
                    }
                };
            };
            let lf = self.scanned.saturating_add(offset);
            let end = lf.saturating_add(1);
            let has_cr = lf > self.judged && buf.get(lf.saturating_sub(1)) == Some(&b'\r');
            let line_end = if has_cr { lf.saturating_sub(1) } else { lf };
            let line = buf.get(self.judged..line_end).unwrap_or_default();

            match self.stage {
                Stage::RequestLine => {
                    if line.len() > limits.max_request_line {
                        return Err(HttpError::RequestLineTooLong);
                    }
                }
                Stage::Headers { .. } => {}
            }
            if end > limits.max_head {
                return Err(HttpError::HeadTooLarge);
            }
            match self.stage {
                Stage::Headers { count } if !line.is_empty() && count >= limits.max_headers => {
                    return Err(HttpError::TooManyHeaders);
                }
                Stage::RequestLine | Stage::Headers { .. } => {}
            }
            if !has_cr {
                return Err(HttpError::BareLf);
            }

            work::judged(line.len());
            match self.stage {
                Stage::RequestLine => {
                    parse_request_line(line)?;
                    self.stage = Stage::Headers { count: 0 };
                }
                Stage::Headers { .. } if line.is_empty() => {
                    let head = build_head(buf.get(..end).unwrap_or_default())?;
                    return Ok(ParseStatus::Complete {
                        head,
                        consumed: end,
                    });
                }
                Stage::Headers { count } => {
                    split_header(line)?;
                    self.stage = Stage::Headers {
                        count: count.saturating_add(1),
                    };
                }
            }
            self.judged = end;
            self.scanned = end;
        }
    }
}

/// Builds the request head from `head`, a complete head (blank line
/// included) whose lines [`HeadParser::advance`] has already judged valid,
/// and applies the checks that need all headers.
fn build_head(head: &[u8]) -> Result<RequestHead, HttpError> {
    work::built(head.len());
    let mut lines = head
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    let (method, target, version) = parse_request_line(lines.next().unwrap_or_default())?;
    let mut headers = Vec::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        headers.push(parse_header(line)?);
    }
    let host = check_host(&headers, version)?;
    let body = check_framing(&headers, version)?;
    let persistence = persistence(&headers, version);
    Ok(RequestHead {
        method,
        target,
        version,
        host,
        headers,
        body,
        persistence,
    })
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

fn is_token(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| is_tchar(b))
}

fn to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

fn is_ows(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

fn trim_ows(mut bytes: &[u8]) -> &[u8] {
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
fn split_header(line: &[u8]) -> Result<(&[u8], &[u8]), HttpError> {
    if line.first().is_some_and(|&b| is_ows(b)) {
        return Err(HttpError::ObsFold);
    }
    let colon = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(HttpError::BadHeaderName)?;
    let name = line.get(..colon).unwrap_or_default();
    let value = line.get(colon + 1..).unwrap_or_default();
    if !is_token(name) {
        return Err(HttpError::BadHeaderName);
    }
    let valid_value = |&b: &u8| b == b'\t' || b == b' ' || (0x21..=0x7E).contains(&b) || b >= 0x80;
    if !value.iter().all(valid_value) {
        return Err(HttpError::BadHeaderValue);
    }
    Ok((name, value))
}

fn parse_header(line: &[u8]) -> Result<Header, HttpError> {
    let (name, value) = split_header(line)?;
    Ok(Header {
        name: to_string(name).to_ascii_lowercase(),
        value: trim_ows(value).to_vec(),
    })
}

fn values<'a>(headers: &'a [Header], name: &'a str) -> impl Iterator<Item = &'a [u8]> {
    headers
        .iter()
        .filter(move |h| h.name == name)
        .map(|h| h.value.as_slice())
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
    let lengths: Vec<&[u8]> = values(headers, "content-length").collect();
    let encodings: Vec<&[u8]> = values(headers, "transfer-encoding").collect();
    if version == Version::Http10 && !encodings.is_empty() {
        return Err(HttpError::TransferEncodingInHttp10);
    }
    if !lengths.is_empty() && !encodings.is_empty() {
        return Err(HttpError::ContentLengthWithTransferEncoding);
    }
    if let Some(&first) = lengths.first() {
        if lengths.len() > 1 {
            return Err(HttpError::DuplicateContentLength);
        }
        if first.is_empty() || !first.iter().all(u8::is_ascii_digit) {
            return Err(HttpError::BadContentLength);
        }
        let n: u64 = to_string(first)
            .parse()
            .map_err(|_| HttpError::BadContentLength)?;
        return Ok(if n == 0 {
            BodyFraming::None
        } else {
            BodyFraming::Length(n)
        });
    }
    match encodings.as_slice() {
        [] => Ok(BodyFraming::None),
        [one] if one.eq_ignore_ascii_case(b"chunked") => Ok(BodyFraming::Chunked),
        _ => Err(HttpError::UnsupportedTransferEncoding),
    }
}

fn persistence(headers: &[Header], version: Version) -> Persistence {
    match version {
        Version::Http10 => Persistence::Close,
        Version::Http11 => {
            let close = values(headers, "connection").any(|v| {
                v.split(|&b| b == b',')
                    .any(|token| trim_ows(token).eq_ignore_ascii_case(b"close"))
            });
            if close {
                Persistence::Close
            } else {
                Persistence::KeepAlive
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn defaults() -> HeadLimits {
        HeadLimits::from(&Limits::default())
    }

    fn parse(input: &[u8]) -> Result<ParseStatus, HttpError> {
        parse_head(input, &defaults())
    }

    fn head_of(input: &[u8]) -> RequestHead {
        match parse(input) {
            Ok(ParseStatus::Complete { head, consumed }) => {
                assert_eq!(consumed, input.len());
                head
            }
            other => panic!("expected complete head, got {other:?}"),
        }
    }

    fn err_of(input: &[u8]) -> HttpError {
        match parse(input) {
            Err(e) => e,
            other => panic!("expected error, got {other:?}"),
        }
    }

    const VALID: &[u8] =
        b"GET /index.html?q=1 HTTP/1.1\r\nHost: Example.COM:8080\r\nUser-Agent:  curl \t\r\n\r\n";

    #[test]
    fn parses_simple_get() {
        let head = head_of(VALID);
        assert_eq!(
            head,
            RequestHead {
                method: Method::Get,
                target: Target {
                    path: "/index.html".to_string(),
                    query: Some("q=1".to_string()),
                },
                version: Version::Http11,
                host: Some("example.com:8080".to_string()),
                headers: vec![
                    Header {
                        name: "host".to_string(),
                        value: b"Example.COM:8080".to_vec(),
                    },
                    Header {
                        name: "user-agent".to_string(),
                        value: b"curl".to_vec(),
                    },
                ],
                body: BodyFraming::None,
                persistence: Persistence::KeepAlive,
            }
        );
    }

    #[test]
    fn incomplete_until_blank_line() {
        for n in 0..VALID.len() {
            assert_eq!(
                parse(VALID.get(..n).unwrap_or_default()),
                Ok(ParseStatus::Incomplete),
                "prefix of {n} bytes"
            );
        }
        assert!(matches!(parse(VALID), Ok(ParseStatus::Complete { .. })));
    }

    #[test]
    fn leaves_pipelined_bytes_unconsumed() {
        let mut two = VALID.to_vec();
        two.extend_from_slice(b"GET /b HTTP/1.1\r\nHost: a\r\n\r\n");
        match parse(&two) {
            Ok(ParseStatus::Complete { head, consumed }) => {
                assert_eq!(consumed, VALID.len());
                assert_eq!(head.target.path, "/index.html");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn tight() -> HeadLimits {
        HeadLimits {
            max_request_line: 20,
            max_head: 60,
            max_headers: 2,
        }
    }

    #[test]
    fn rejects_malformed_heads() {
        use HttpError::*;
        let d = defaults();
        let rows: Vec<(&[u8], HeadLimits, HttpError)> = vec![
            (
                b"GET /aaaaaaaaaaaaaaaaaaaa HTTP/1.1\r\nHost: a\r\n\r\n",
                tight(),
                RequestLineTooLong,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nA: 1234567890123456789012345678901234567890\r\n\r\n",
                tight(),
                HeadTooLarge,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nA: 1\r\nB: 2\r\n\r\n",
                tight(),
                TooManyHeaders,
            ),
            (b"GET /\r\nHost: a\r\n\r\n", d, BadRequestLine),
            (b"GET  / HTTP/1.1\r\nHost: a\r\n\r\n", d, BadRequestLine),
            (b"GET / HTTP/1.1 \r\nHost: a\r\n\r\n", d, BadRequestLine),
            (b"\r\nGET / HTTP/1.1\r\nHost: a\r\n\r\n", d, BadRequestLine),
            (b"G(T / HTTP/1.1\r\nHost: a\r\n\r\n", d, BadMethod),
            (b" / HTTP/1.1\r\nHost: a\r\n\r\n", d, BadMethod),
            (b"get\x00 / HTTP/1.1\r\nHost: a\r\n\r\n", d, BadMethod),
            (b"GET / HTTP/1.1x\r\nHost: a\r\n\r\n", d, BadVersion),
            (b"GET / http/1.1\r\nHost: a\r\n\r\n", d, BadVersion),
            (b"GET / HTTP/2.0\r\nHost: a\r\n\r\n", d, UnsupportedVersion),
            (b"GET / HTTP/0.9\r\nHost: a\r\n\r\n", d, UnsupportedVersion),
            (
                b"GET /a/../b HTTP/1.1\r\nHost: a\r\n\r\n",
                d,
                BadTarget(TargetError::DotSegment),
            ),
            (b"GET / HTTP/1.1\nHost: a\n\n", d, BareLf),
            (b"GET / HTTP/1.1\r\nHost: a\n\r\n", d, BareLf),
            (b"GET / HTTP/1.1\r\nHost: a\r\n\n", d, BareLf),
            (b"GET / HTTP/1.1\r\nHost : a\r\n\r\n", d, BadHeaderName),
            (b"GET / HTTP/1.1\r\nHost: a\r\n: x\r\n\r\n", d, BadHeaderName),
            (b"GET / HTTP/1.1\r\nHost: a\r\nNoColon\r\n\r\n", d, BadHeaderName),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nX: a\rb\r\n\r\n",
                d,
                BadHeaderValue,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nX: a\0b\r\n\r\n",
                d,
                BadHeaderValue,
            ),
            (b"GET / HTTP/1.1\r\nHost: a\r\n X: a\r\n\r\n", d, ObsFold),
            (b"GET / HTTP/1.1\r\nHost: a\r\n\tX: a\r\n\r\n", d, ObsFold),
            (b"GET / HTTP/1.1\r\n\r\n", d, MissingHost),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nHost: a\r\n\r\n",
                d,
                DuplicateHost,
            ),
            (b"GET / HTTP/1.1\r\nHost: a b\r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.1\r\nHost: \r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.1\r\nHost: a:\r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.1\r\nHost: a:123456\r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.1\r\nHost: a:80x\r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.1\r\nHost: a@b\r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.1\r\nHost: [::1\r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.1\r\nHost: [::1]x\r\n\r\n", d, BadHost),
            (b"GET / HTTP/1.0\r\nHost: a/b\r\n\r\n", d, BadHost),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
                d,
                ContentLengthWithTransferEncoding,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: x\r\n\r\n",
                d,
                BadContentLength,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: \r\n\r\n",
                d,
                BadContentLength,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: +5\r\n\r\n",
                d,
                BadContentLength,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: 18446744073709551616\r\n\r\n",
                d,
                BadContentLength,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n",
                d,
                DuplicateContentLength,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
                d,
                UnsupportedTransferEncoding,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
                d,
                UnsupportedTransferEncoding,
            ),
            (
                b"GET / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
                d,
                TransferEncodingInHttp10,
            ),
            (
                b"GET / HTTP/1.0\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
                d,
                TransferEncodingInHttp10,
            ),
        ];
        for (input, limits, expected) in rows {
            assert_eq!(
                parse_head(input, &limits),
                Err(expected),
                "input {:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn limits_are_enforced() {
        let l = HeadLimits {
            max_request_line: 16,
            max_head: 64,
            max_headers: 2,
        };
        // No LF within max + 2 bytes: the line is certainly too long.
        let mut line = b"GET /".to_vec();
        line.resize(18, b'a');
        assert_eq!(parse_head(&line, &l), Err(HttpError::RequestLineTooLong));
        // Max + 1 bytes could still be max bytes plus the CR.
        line.truncate(17);
        assert_eq!(parse_head(&line, &l), Ok(ParseStatus::Incomplete));
        // A judged line of max + 1 bytes is too long.
        line.truncate(16);
        line.extend_from_slice(b"a\r\n");
        assert_eq!(parse_head(&line, &l), Err(HttpError::RequestLineTooLong));
        // Exactly max bytes passes the limit (and fails on syntax instead).
        line.truncate(16);
        line.extend_from_slice(b"\r\n");
        assert_eq!(parse_head(&line, &l), Err(HttpError::BadRequestLine));
        // Too many headers.
        assert_eq!(
            parse_head(b"GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n", &l),
            Err(HttpError::TooManyHeaders)
        );
        // Head over max_head, still incomplete.
        let mut big = b"GET / HTTP/1.1\r\nA: ".to_vec();
        big.resize(65, b'x');
        assert_eq!(parse_head(&big, &l), Err(HttpError::HeadTooLarge));
        big.truncate(64);
        assert_eq!(parse_head(&big, &l), Ok(ParseStatus::Incomplete));
        // A head of exactly max_head bytes (blank line included) is accepted.
        let mut exact = b"GET / HTTP/1.1\r\nHost: a\r\nA: ".to_vec();
        let fill = 64 - exact.len() - 4;
        exact.extend(std::iter::repeat_n(b'x', fill));
        exact.extend_from_slice(b"\r\n\r\n");
        assert_eq!(exact.len(), 64);
        assert!(matches!(
            parse_head(&exact, &l),
            Ok(ParseStatus::Complete { .. })
        ));
        // One byte more is not.
        let mut over = exact.clone();
        over.insert(30, b'x');
        assert_eq!(parse_head(&over, &l), Err(HttpError::HeadTooLarge));
    }

    #[test]
    fn request_line_limit_wins_when_head_limit_is_not_larger() {
        for (r, h) in [(16, 16), (16, 17), (16, 19), (16, 8)] {
            let l = HeadLimits {
                max_request_line: r,
                max_head: h,
                max_headers: 4,
            };
            let mut full = b"GET /".to_vec();
            full.resize(r + 5, b'a');
            full.extend_from_slice(b"\r\n\r\n");
            let expected = parse_head(&full, &l);
            assert_eq!(expected, Err(HttpError::RequestLineTooLong), "R={r} H={h}");
            for n in 0..full.len() {
                match parse_head(full.get(..n).unwrap_or_default(), &l) {
                    Ok(ParseStatus::Incomplete) => {}
                    other => assert_eq!(other, expected, "R={r} H={h} prefix {n}"),
                }
            }
        }
    }

    #[test]
    fn host_port_range() {
        let h = |v: &str| {
            parse(format!("GET / HTTP/1.1\r\nHost: {v}\r\n\r\n").as_bytes())
                .map(|s| matches!(s, ParseStatus::Complete { .. }))
        };
        assert_eq!(h("a:0"), Err(HttpError::BadHost));
        assert_eq!(h("a:00"), Err(HttpError::BadHost));
        assert_eq!(h("a:65536"), Err(HttpError::BadHost));
        assert_eq!(h("a:99999"), Err(HttpError::BadHost));
        assert_eq!(h("[::1]:0"), Err(HttpError::BadHost));
        assert_eq!(h("a:65535"), Ok(true));
        assert_eq!(h("a:080"), Ok(true));
    }

    #[test]
    fn pins_edge_cases() {
        use HttpError::*;
        const L: &str = "GET / HTTP/1.1\r\nHost: a\r\n";
        let with = |tail: &[u8]| {
            let mut v = L.as_bytes().to_vec();
            v.extend_from_slice(tail);
            v
        };
        assert_eq!(parse(&with(b"\r\r\n")), Err(BadHeaderName));
        assert_eq!(parse(&with(b"\r")), Ok(ParseStatus::Incomplete));
        assert_eq!(parse(&with(b"X\t: a\r\n\r\n")), Err(BadHeaderName));
        assert_eq!(parse(&with(b"X\0: a\r\n\r\n")), Err(BadHeaderName));
        assert_eq!(parse(&with(b"X: a\x7fb\r\n\r\n")), Err(BadHeaderValue));
        assert_eq!(
            head_of(&with(b"Transfer-Encoding: chunked \r\n\r\n")).body,
            BodyFraming::Chunked
        );
        assert_eq!(
            parse(&with(b"Transfer-Encoding: chunked,\r\n\r\n")),
            Err(UnsupportedTransferEncoding)
        );
        assert_eq!(
            head_of(&with(b"Content-Length:  5\r\n\r\n")).body,
            BodyFraming::Length(5)
        );
        assert_eq!(
            parse(&with(b"content-length: 5\r\nContent-Length: 5\r\n\r\n")),
            Err(DuplicateContentLength)
        );
    }

    #[test]
    fn connection_close_and_http10_persistence() {
        let p = |input: &[u8]| head_of(input).persistence;
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"),
            Persistence::KeepAlive
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n"),
            Persistence::Close
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, CLOSE\r\n\r\n"),
            Persistence::Close
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive\r\nConnection: Close\r\n\r\n"),
            Persistence::Close
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: closed\r\n\r\n"),
            Persistence::KeepAlive
        );
        assert_eq!(p(b"GET / HTTP/1.0\r\n\r\n"), Persistence::Close);
        assert_eq!(
            p(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"),
            Persistence::Close
        );
    }

    #[test]
    fn http10_host_is_optional_but_validated() {
        let head = head_of(b"GET / HTTP/1.0\r\n\r\n");
        assert_eq!(head.version, Version::Http10);
        assert_eq!(head.host, None);
        assert_eq!(
            head_of(b"GET / HTTP/1.0\r\nHost: A.b\r\n\r\n").host,
            Some("a.b".to_string())
        );
    }

    #[test]
    fn host_forms() {
        let h = |v: &str| head_of(format!("GET / HTTP/1.1\r\nHost: {v}\r\n\r\n").as_bytes()).host;
        assert_eq!(h("example.com"), Some("example.com".to_string()));
        assert_eq!(h("Ex-Ample.COM:80"), Some("ex-ample.com:80".to_string()));
        assert_eq!(h("[::1]"), Some("[::1]".to_string()));
        assert_eq!(
            h("[2001:DB8::1]:8080"),
            Some("[2001:db8::1]:8080".to_string())
        );
        assert_eq!(h("127.0.0.1:65535"), Some("127.0.0.1:65535".to_string()));
    }

    #[test]
    fn methods() {
        let m = |s: &str| head_of(format!("{s} / HTTP/1.1\r\nHost: a\r\n\r\n").as_bytes()).method;
        assert_eq!(m("GET"), Method::Get);
        assert_eq!(m("HEAD"), Method::Head);
        assert_eq!(m("get"), Method::Other("get".to_string()));
        assert_eq!(m("POST"), Method::Other("POST".to_string()));
        assert_eq!(m("M-SEARCH"), Method::Other("M-SEARCH".to_string()));
    }

    #[test]
    fn body_framing() {
        let b = |h: &str| head_of(format!("POST / HTTP/1.1\r\nHost: a\r\n{h}").as_bytes()).body;
        assert_eq!(b("\r\n"), BodyFraming::None);
        assert_eq!(b("Content-Length: 0\r\n\r\n"), BodyFraming::None);
        assert_eq!(b("Content-Length: 007\r\n\r\n"), BodyFraming::Length(7));
        assert_eq!(
            b("Content-Length: 18446744073709551615\r\n\r\n"),
            BodyFraming::Length(u64::MAX)
        );
        assert_eq!(
            b("Transfer-Encoding: Chunked\r\n\r\n"),
            BodyFraming::Chunked
        );
        assert_eq!(
            err_of(b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: \r\n\r\n"),
            HttpError::UnsupportedTransferEncoding
        );
    }

    #[test]
    fn obs_text_in_values_is_allowed() {
        let head = head_of(b"GET / HTTP/1.1\r\nHost: a\r\nX: caf\xc3\xa9\r\n\r\n");
        assert_eq!(
            head.headers.last().map(|h| h.value.clone()),
            Some(b"caf\xc3\xa9".to_vec())
        );
    }

    /// Feeds `input` to one `HeadParser` in pieces ending at each of `cuts`
    /// (and finally the whole input); returns the first verdict and the
    /// length of the prefix that produced it.
    fn feed(
        input: &[u8],
        cuts: &[usize],
        limits: &HeadLimits,
    ) -> (Result<ParseStatus, HttpError>, usize) {
        let mut parser = HeadParser::new();
        let mut ends: Vec<usize> = cuts.iter().map(|&c| c.min(input.len())).collect();
        ends.sort_unstable();
        ends.push(input.len());
        for end in ends {
            let result = parser.advance(input.get(..end).unwrap_or_default(), limits);
            if result != Ok(ParseStatus::Incomplete) {
                return (result, end);
            }
        }
        (Ok(ParseStatus::Incomplete), input.len())
    }

    #[test]
    fn parser_fed_line_by_line_does_linear_work() {
        let l = defaults();
        let mut head = b"GET / HTTP/1.1\r\nHost: a\r\n".to_vec();
        let mut cuts = vec![16, head.len()];
        while head.len() + 40 + 2 <= l.max_head && cuts.len() < l.max_headers + 1 {
            head.extend_from_slice(b"X-Filler: 0123456789012345678901234567\r\n");
            cuts.push(head.len());
        }
        head.extend_from_slice(b"\r\n");
        work::take();
        let (result, _) = feed(&head, &cuts, &l);
        let used = work::take();
        assert_eq!(result, parse_head(&head, &l));
        assert!(matches!(result, Ok(ParseStatus::Complete { .. })));
        let n = head.len();
        assert!(
            used.scanned <= n && used.judged <= n && used.built <= n,
            "{used:?} for {n} bytes"
        );
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = parse(&bytes);
            let _ = parse_head(&bytes, &tight());
        }

        #[test]
        fn never_panics_after_valid_request_line(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut input = b"GET / HTTP/1.1\r\n".to_vec();
            input.extend_from_slice(&bytes);
            let _ = parse(&input);
            let _ = parse_head(&input, &tight());
        }

        #[test]
        fn prefix_never_contradicts_full(
            pieces in proptest::collection::vec(
                prop_oneof![
                    Just(b"GET".to_vec()),
                    Just(b"HEAD".to_vec()),
                    Just(b" ".to_vec()),
                    Just(b"/".to_vec()),
                    Just(b"/a/b?x=1".to_vec()),
                    Just(b"HTTP/1.1".to_vec()),
                    Just(b"HTTP/1.0".to_vec()),
                    Just(b"HTTP/2.0".to_vec()),
                    Just(b"Host".to_vec()),
                    Just(b"Host: a".to_vec()),
                    Just(b"Content-Length: 5".to_vec()),
                    Just(b"Transfer-Encoding: chunked".to_vec()),
                    Just(b"Connection: close".to_vec()),
                    Just(b": ".to_vec()),
                    Just(b"X-Y: z".to_vec()),
                    Just(b"\r".to_vec()),
                    Just(b"\n".to_vec()),
                    Just(b"\r\n".to_vec()),
                    Just(b"\r\n\r\n".to_vec()),
                    Just(b"\t".to_vec()),
                    Just(b"\0".to_vec()),
                    proptest::collection::vec(any::<u8>(), 0..4),
                ],
                0..24,
            ),
            which in 0..4usize,
        ) {
            let input: Vec<u8> = pieces.concat();
            let limits = match which {
                0 => defaults(),
                1 => tight(),
                2 => HeadLimits { max_request_line: 20, max_head: 20, max_headers: 3 },
                _ => HeadLimits { max_request_line: 20, max_head: 22, max_headers: 3 },
            };
            let full = parse_head(&input, &limits);
            for n in 0..=input.len() {
                let prefix = input.get(..n).unwrap_or_default();
                match parse_head(prefix, &limits) {
                    Ok(ParseStatus::Incomplete) => {}
                    other => prop_assert_eq!(&other, &full, "prefix of {} bytes", n),
                }
            }
        }

        #[test]
        fn incremental_parser_matches_stateless(
            pieces in proptest::collection::vec(
                prop_oneof![
                    Just(b"GET / HTTP/1.1\r\n".to_vec()),
                    Just(b"GET / HTTP/1.0\r\n".to_vec()),
                    Just(b"Host: a\r\n".to_vec()),
                    Just(b"Content-Length: 5\r\n".to_vec()),
                    Just(b"Transfer-Encoding: chunked\r\n".to_vec()),
                    Just(b"Connection: close\r\n".to_vec()),
                    Just(b"X-Y: z".to_vec()),
                    Just(b" ".to_vec()),
                    Just(b"\r".to_vec()),
                    Just(b"\n".to_vec()),
                    Just(b"\r\n".to_vec()),
                    Just(b"\0".to_vec()),
                    proptest::collection::vec(any::<u8>(), 0..4),
                ],
                0..24,
            ),
            cuts in proptest::collection::vec(0..400usize, 0..12),
            which in 0..4usize,
        ) {
            let input: Vec<u8> = pieces.concat();
            let limits = match which {
                0 => defaults(),
                1 => tight(),
                2 => HeadLimits { max_request_line: 20, max_head: 20, max_headers: 3 },
                _ => HeadLimits { max_request_line: 20, max_head: 22, max_headers: 3 },
            };
            let (verdict, at) = feed(&input, &cuts, &limits);
            prop_assert_eq!(&verdict, &parse_head(input.get(..at).unwrap_or_default(), &limits));
            match &verdict {
                Ok(ParseStatus::Incomplete) => {}
                other => prop_assert_eq!(other, &parse_head(&input, &limits)),
            }
        }
    }
}
