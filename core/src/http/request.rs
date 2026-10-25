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
                Stage::Headers { count, .. } if !line.is_empty() && count >= limits.max_headers => {
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
                    let (method, _, _) = parse_request_line(line)?;
                    let method = match method {
                        Method::Get => KnownMethod::Get,
                        Method::Head => KnownMethod::Head,
                        Method::Other(_) => KnownMethod::Other,
                    };
                    self.stage = Stage::Headers { count: 0, method };
                }
                Stage::Headers { .. } if line.is_empty() => {
                    let head = build_head(buf.get(..end).unwrap_or_default())?;
                    return Ok(ParseStatus::Complete {
                        head,
                        consumed: end,
                    });
                }
                Stage::Headers { count, method } => {
                    split_header(line)?;
                    self.stage = Stage::Headers {
                        count: count.saturating_add(1),
                        method,
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
mod tests;
