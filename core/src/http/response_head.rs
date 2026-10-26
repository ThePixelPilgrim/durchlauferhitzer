//! Strict, incremental parser for the HTTP/1.x response head a backend
//! sends.
//!
//! It follows the request parser's rule, "when in doubt, reject": the head
//! must be CRLF-framed, header lines are checked as for requests (interim
//! heads included; the first bad line wins), and the body framing is
//! decided from the status, the request method and the `Content-Length` /
//! `Transfer-Encoding` headers.
//!
//! [`ResponseParser`] looks for the blank line that ends the head
//! incrementally, so no byte is searched twice however the head is split.
//! The head is validated once, when the blank line arrives. Interim (`1xx`)
//! heads are reported so the caller can drop them and parse on.

use crate::http::request::{
    Declared, FramingError, HeadLimits, Header, HeaderError, Version, connection_names_framing,
    framing_headers, parse_header, values, work,
};
use crate::http::response::BodyMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub version: Version,
    pub status: u16,
    /// The reason phrase as sent; may be empty.
    pub reason: Vec<u8>,
    pub headers: Vec<Header>,
    pub framing: ResponseFraming,
}

/// How the response body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFraming {
    /// No body, whatever the headers say: the response to a HEAD request, a
    /// `204` or a `304`.
    NoBody,
    /// Exactly this many bytes (also `Length(0)`).
    Length(u64),
    Chunked,
    /// The body ends when the backend closes the connection.
    UntilClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseError {
    HeadTooLarge,
    TooManyHeaders,
    BadStatusLine,
    BadVersion,
    BadStatus,
    BadReason,
    BareLf,
    BadHeaderName,
    BadHeaderValue,
    ObsFold,
    /// A `Connection` token naming `content-length`, `transfer-encoding` or
    /// `host`.
    BadConnection,
    ContentLengthWithTransferEncoding,
    BadContentLength,
    DuplicateContentLength,
    /// Anything but a single `chunked`, and any `Transfer-Encoding` at all
    /// on an HTTP/1.0 response.
    UnsupportedTransferEncoding,
    /// A `101` head: this server never asks for an upgrade.
    SwitchingProtocols,
}

impl From<HeaderError> for ResponseError {
    fn from(error: HeaderError) -> Self {
        match error {
            HeaderError::BadName => ResponseError::BadHeaderName,
            HeaderError::BadValue => ResponseError::BadHeaderValue,
            HeaderError::ObsFold => ResponseError::ObsFold,
        }
    }
}

impl From<FramingError> for ResponseError {
    fn from(error: FramingError) -> Self {
        match error {
            FramingError::ContentLengthWithTransferEncoding => {
                ResponseError::ContentLengthWithTransferEncoding
            }
            FramingError::BadContentLength => ResponseError::BadContentLength,
            FramingError::DuplicateContentLength => ResponseError::DuplicateContentLength,
            FramingError::UnsupportedTransferEncoding => ResponseError::UnsupportedTransferEncoding,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseStatus {
    Incomplete,
    /// A 1xx (not 101) head of `consumed` bytes, to be dropped.
    Interim {
        consumed: usize,
    },
    Complete {
        head: ResponseHead,
        consumed: usize,
    },
}

/// Resumable state of the response-head parser.
///
/// Feed it every byte received since the previous head ended with
/// [`ResponseParser::advance`], each call passing the previous bytes plus
/// any new ones. After `Interim` the parser has reset itself: drop the first
/// `consumed` bytes and call it again with the rest. After `Complete` or an
/// error the parser is spent: a new head needs a new parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseParser {
    /// Start of the first line not yet complete.
    line_start: usize,
    /// `buf[line_start..scanned]` is known to hold no LF.
    scanned: usize,
    /// Non-blank lines complete so far: the status line and the headers.
    lines: usize,
}

impl Default for ResponseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseParser {
    /// A parser at the start of a head.
    pub const fn new() -> Self {
        Self {
            line_start: 0,
            scanned: 0,
            lines: 0,
        }
    }

    /// Continues parsing `buf`, every byte received since the previous head
    /// ended. `request` is `HeadOnly` when the response answers a HEAD
    /// request.
    ///
    /// Returns `Incomplete` until the blank line arrives; limit violations
    /// are reported as soon as they are certain.
    pub fn advance(
        &mut self,
        buf: &[u8],
        limits: &HeadLimits,
        request: BodyMode,
    ) -> Result<ResponseStatus, ResponseError> {
        loop {
            let unscanned = buf.get(self.scanned..).unwrap_or_default();
            let found = unscanned.iter().position(|&b| b == b'\n');
            work::scanned(found.map_or(unscanned.len(), |i| i + 1));
            let Some(offset) = found else {
                self.scanned = self.scanned.max(buf.len());
                return if buf.len() > limits.max_head {
                    Err(ResponseError::HeadTooLarge)
                } else {
                    Ok(ResponseStatus::Incomplete)
                };
            };
            let lf = self.scanned.saturating_add(offset);
            let end = lf.saturating_add(1);
            if end > limits.max_head {
                return Err(ResponseError::HeadTooLarge);
            }
            let line = buf.get(self.line_start..lf).unwrap_or_default();
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                let result = parse_complete(buf.get(..end).unwrap_or_default(), request);
                if matches!(result, Ok(ResponseStatus::Interim { .. })) {
                    *self = Self::new();
                }
                return result;
            }
            self.lines = self.lines.saturating_add(1);
            // The first line is the status line, not a header.
            if self.lines > limits.max_headers.saturating_add(1) {
                return Err(ResponseError::TooManyHeaders);
            }
            self.line_start = end;
            self.scanned = end;
        }
    }
}

/// Validates `head`, a complete head (blank line included) found by
/// [`ResponseParser::advance`], and decides its framing.
fn parse_complete(head: &[u8], request: BodyMode) -> Result<ResponseStatus, ResponseError> {
    work::built(head.len());
    let mut lines = head.split_inclusive(|&b| b == b'\n').map(|line| {
        line.strip_suffix(b"\n")
            .and_then(|line| line.strip_suffix(b"\r"))
            .ok_or(ResponseError::BareLf)
    });
    let (version, status, reason) = parse_status_line(lines.next().unwrap_or(Ok(b""))?)?;
    // Header syntax is checked line by line, so the first bad line wins (as
    // in the request parser), for interim heads as for final ones.
    let mut headers = Vec::new();
    for line in lines {
        let line = line?;
        if line.is_empty() {
            break;
        }
        work::judged(line.len());
        headers.push(parse_header(line)?);
    }
    match status {
        101 => return Err(ResponseError::SwitchingProtocols),
        // Interim heads get the syntax checks above, but no framing rules.
        100..=199 => {
            return Ok(ResponseStatus::Interim {
                consumed: head.len(),
            });
        }
        _ => {}
    }
    if connection_names_framing(&headers) {
        return Err(ResponseError::BadConnection);
    }
    if version == Version::Http10 && values(&headers, "transfer-encoding").next().is_some() {
        return Err(ResponseError::UnsupportedTransferEncoding);
    }
    let declared = framing_headers(&headers)?;
    let framing = if request == BodyMode::HeadOnly || status == 204 || status == 304 {
        ResponseFraming::NoBody
    } else {
        match declared {
            Declared::Length(n) => ResponseFraming::Length(n),
            Declared::Chunked => ResponseFraming::Chunked,
            Declared::Absent => ResponseFraming::UntilClose,
        }
    };
    Ok(ResponseStatus::Complete {
        head: ResponseHead {
            version,
            status,
            reason: reason.to_vec(),
            headers,
            framing,
        },
        consumed: head.len(),
    })
}

/// Splits `HTTP/1.x SP 3DIGIT SP reason` into its parts.
fn parse_status_line(line: &[u8]) -> Result<(Version, u16, &[u8]), ResponseError> {
    let version_of = |token: &[u8]| match token {
        b"HTTP/1.1" => Ok(Version::Http11),
        b"HTTP/1.0" => Ok(Version::Http10),
        _ => Err(ResponseError::BadVersion),
    };
    let Some(space) = line.iter().position(|&b| b == b' ') else {
        // Without the SP after the version there is no status at all.
        return Err(match version_of(line) {
            Err(_) if line.is_empty() => ResponseError::BadStatusLine,
            Err(error) => error,
            Ok(_) => ResponseError::BadStatusLine,
        });
    };
    let version = version_of(line.get(..space).unwrap_or_default())?;
    let rest = line.get(space + 1..).unwrap_or_default();
    let (digits, after) = (rest.get(..3), rest.get(3..));
    let status = match digits {
        Some([a, b, c]) if [a, b, c].iter().all(|d| d.is_ascii_digit()) => {
            u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0')
        }
        _ => return Err(ResponseError::BadStatus),
    };
    if !(100..=599).contains(&status) {
        return Err(ResponseError::BadStatus);
    }
    let reason = match after {
        Some([b' ', reason @ ..]) => reason,
        Some([d, ..]) if d.is_ascii_digit() => return Err(ResponseError::BadStatus),
        _ => return Err(ResponseError::BadStatusLine),
    };
    if !reason
        .iter()
        .all(|&b| b == b'\t' || b == b' ' || (0x21..=0x7E).contains(&b) || b >= 0x80)
    {
        return Err(ResponseError::BadReason);
    }
    Ok((version, status, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::Limits;
    use proptest::prelude::*;

    fn defaults() -> HeadLimits {
        HeadLimits::from(&Limits::default())
    }

    fn parse(input: &[u8]) -> Result<ResponseStatus, ResponseError> {
        ResponseParser::new().advance(input, &defaults(), BodyMode::Full)
    }

    fn head_of(input: &[u8]) -> ResponseHead {
        match parse(input) {
            Ok(ResponseStatus::Complete { head, consumed }) => {
                assert_eq!(consumed, input.len());
                head
            }
            other => panic!("expected complete head, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_simple_response() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\n";
        let mut input = head.to_vec();
        input.extend_from_slice(b"hello");
        assert_eq!(
            parse(&input),
            Ok(ResponseStatus::Complete {
                head: ResponseHead {
                    version: Version::Http11,
                    status: 200,
                    reason: b"OK".to_vec(),
                    headers: vec![
                        Header {
                            name: "content-length".to_string(),
                            value: b"5".to_vec(),
                        },
                        Header {
                            name: "x-a".to_string(),
                            value: b"b".to_vec(),
                        },
                    ],
                    framing: ResponseFraming::Length(5),
                },
                consumed: head.len(),
            })
        );
    }

    #[test]
    fn status_line_forms() {
        let h = head_of(b"HTTP/1.0 404 \r\n\r\n");
        assert_eq!(h.version, Version::Http10);
        assert_eq!(h.status, 404);
        assert_eq!(h.reason, b"");
        let h = head_of(b"HTTP/1.1 599 Odd \t Reason caf\xc3\xa9\r\n\r\n");
        assert_eq!(h.status, 599);
        assert_eq!(h.reason, b"Odd \t Reason caf\xc3\xa9");
        assert_eq!(
            head_of(b"HTTP/1.1 200 OK\r\nX:  v \t\r\n\r\n").headers[0].value,
            b"v"
        );
    }

    #[test]
    fn framing_table() {
        use ResponseFraming::*;
        let rows: Vec<(&[u8], BodyMode, ResponseFraming)> = vec![
            (
                b"HTTP/1.1 204 No Content\r\nContent-Length: 5\r\n\r\n",
                BodyMode::Full,
                NoBody,
            ),
            (b"HTTP/1.1 304 Not Modified\r\n\r\n", BodyMode::Full, NoBody),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
                BodyMode::HeadOnly,
                NoBody,
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                BodyMode::HeadOnly,
                NoBody,
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                BodyMode::Full,
                Chunked,
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked \r\n\r\n",
                BodyMode::Full,
                Chunked,
            ),
            (b"HTTP/1.1 200 OK\r\n\r\n", BodyMode::Full, UntilClose),
            (b"HTTP/1.0 200 OK\r\n\r\n", BodyMode::Full, UntilClose),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                BodyMode::Full,
                Length(0),
            ),
            (
                b"HTTP/1.0 200 OK\r\nContent-Length: 007\r\n\r\n",
                BodyMode::Full,
                Length(7),
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n",
                BodyMode::Full,
                Length(u64::MAX),
            ),
        ];
        for (input, request, expected) in rows {
            let got = ResponseParser::new().advance(input, &defaults(), request);
            match got {
                Ok(ResponseStatus::Complete { head, consumed }) => {
                    assert_eq!(
                        head.framing,
                        expected,
                        "{:?}",
                        String::from_utf8_lossy(input)
                    );
                    assert_eq!(consumed, input.len());
                }
                other => panic!("{:?}: {other:?}", String::from_utf8_lossy(input)),
            }
        }
    }

    #[test]
    fn header_errors_win_over_bodyless_statuses() {
        let got = ResponseParser::new().advance(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            &defaults(),
            BodyMode::HeadOnly,
        );
        assert_eq!(got, Err(ResponseError::DuplicateContentLength));
    }

    #[test]
    fn interim_heads_are_reported() {
        let cont = b"HTTP/1.1 100 Continue\r\n\r\n";
        assert_eq!(
            parse(cont),
            Ok(ResponseStatus::Interim {
                consumed: cont.len()
            })
        );
        let early = b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\n";
        assert_eq!(
            parse(early),
            Ok(ResponseStatus::Interim {
                consumed: early.len()
            })
        );
        assert_eq!(
            parse(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: x\r\n\r\n"),
            Err(ResponseError::SwitchingProtocols)
        );
    }

    #[test]
    fn parser_resets_after_an_interim_head() {
        let cont = b"HTTP/1.1 100 Continue\r\n\r\n";
        let fin = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n";
        let mut buf = cont.to_vec();
        buf.extend_from_slice(fin);
        buf.extend_from_slice(b"ok");
        let mut parser = ResponseParser::new();
        let limits = defaults();
        let consumed = match parser.advance(&buf, &limits, BodyMode::Full) {
            Ok(ResponseStatus::Interim { consumed }) => consumed,
            other => panic!("{other:?}"),
        };
        assert_eq!(consumed, cont.len());
        let rest = buf.get(consumed..).unwrap_or_default();
        match parser.advance(rest, &limits, BodyMode::Full) {
            Ok(ResponseStatus::Complete { head, consumed }) => {
                assert_eq!(consumed, fin.len());
                assert_eq!(head.status, 200);
                assert_eq!(head.framing, ResponseFraming::Length(2));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Interim heads are parsed as strictly as final ones (header syntax),
    /// but no framing rule applies to them.
    #[test]
    fn interim_heads_get_header_syntax_checks() {
        use ResponseError::*;
        let rows: Vec<(&[u8], ResponseError)> = vec![
            (b"HTTP/1.1 100 Continue\r\nA : b\r\n\r\n", BadHeaderName),
            (b"HTTP/1.1 103 Early\r\nNoColon\r\n\r\n", BadHeaderName),
            (b"HTTP/1.1 100 Continue\r\nA: b\r\n c\r\n\r\n", ObsFold),
            (b"HTTP/1.1 100 Continue\r\nA: a\0b\r\n\r\n", BadHeaderValue),
            (b"HTTP/1.1 100 Continue\r\nA: b\n\r\n", BareLf),
            (b"HTTP/1.1 101 Switch\r\nA : b\r\n\r\n", BadHeaderName),
        ];
        for (input, expected) in rows {
            assert_eq!(
                parse(input),
                Err(expected),
                "input {:?}",
                String::from_utf8_lossy(input)
            );
        }
        // Framing headers mean nothing on an interim head.
        let odd = b"HTTP/1.1 103 Early\r\nContent-Length: x\r\nContent-Length: 1\r\n\
            Transfer-Encoding: gzip\r\nConnection: host\r\n\r\n";
        assert_eq!(
            parse(odd),
            Ok(ResponseStatus::Interim {
                consumed: odd.len()
            })
        );
    }

    /// The first bad line wins, as in the request parser: a line-ending
    /// fault later in the head does not mask an earlier header fault.
    #[test]
    fn first_bad_line_wins() {
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\nA : b\r\nC: d\n\r\n"),
            Err(ResponseError::BadHeaderName)
        );
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\nA: b\nC : d\r\n\r\n"),
            Err(ResponseError::BareLf)
        );
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\nA : b\r\n\r\n"),
            Err(ResponseError::BareLf)
        );
    }

    /// The interim reset works however the bytes arrive: fed one byte at
    /// a time to one parser, dropping consumed bytes.
    #[test]
    fn interim_then_final_head_byte_by_byte() {
        let cont = b"HTTP/1.1 100 Continue\r\nX: y\r\n\r\n";
        let fin = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n";
        let input = [cont.as_slice(), fin].concat();
        let limits = defaults();
        let mut parser = ResponseParser::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut verdicts = Vec::new();
        for &byte in &input {
            pending.push(byte);
            match parser.advance(&pending, &limits, BodyMode::Full) {
                Ok(ResponseStatus::Incomplete) => {}
                Ok(ResponseStatus::Interim { consumed }) => {
                    verdicts.push(("interim", consumed));
                    pending.drain(..consumed);
                }
                Ok(ResponseStatus::Complete { consumed, head }) => {
                    assert_eq!(head.framing, ResponseFraming::Length(2));
                    verdicts.push(("complete", consumed));
                    pending.drain(..consumed);
                }
                Err(error) => panic!("{error:?}"),
            }
        }
        assert_eq!(verdicts, [("interim", cont.len()), ("complete", fin.len())]);
        assert!(pending.is_empty());
    }

    #[test]
    fn rejects_malformed_heads() {
        use ResponseError::*;
        let rows: Vec<(&[u8], ResponseError)> = vec![
            (b"HTTP/2 200 OK\r\n\r\n", BadVersion),
            (b"HTTP/2.0 200 OK\r\n\r\n", BadVersion),
            (b"http/1.1 200 OK\r\n\r\n", BadVersion),
            (b"HTTP/1.1x 200 OK\r\n\r\n", BadVersion),
            (b"HTTP/1.1 20 OK\r\n\r\n", BadStatus),
            (b"HTTP/1.1 600 X\r\n\r\n", BadStatus),
            (b"HTTP/1.1 099 X\r\n\r\n", BadStatus),
            (b"HTTP/1.1 2x0 X\r\n\r\n", BadStatus),
            (b"HTTP/1.1 2000 X\r\n\r\n", BadStatus),
            (b"HTTP/1.1 \r\n\r\n", BadStatus),
            (b"HTTP/1.1 200\r\n\r\n", BadStatusLine),
            (b"HTTP/1.1\r\n\r\n", BadStatusLine),
            (b"HTTP/1.1 200\tOK\r\n\r\n", BadStatusLine),
            (b"\r\n", BadStatusLine),
            (b"HTTP/1.1 200 OK\nA: b\r\n\r\n", BareLf),
            (b"HTTP/1.1 200 OK\r\nA: b\n\r\n", BareLf),
            (b"HTTP/1.1 200 OK\r\nA: b\r\n\n", BareLf),
            (b"HTTP/1.1 200 OK\r\n A: b\r\n\r\n", ObsFold),
            (b"HTTP/1.1 200 OK\r\nA: b\r\n\tc\r\n\r\n", ObsFold),
            (b"HTTP/1.1 200 OK\r\nA : b\r\n\r\n", BadHeaderName),
            (b"HTTP/1.1 200 OK\r\nNoColon\r\n\r\n", BadHeaderName),
            (b"HTTP/1.1 200 OK\r\nA: a\0b\r\n\r\n", BadHeaderValue),
            (b"HTTP/1.1 200 OK\x01\r\n\r\n", BadReason),
            (b"HTTP/1.1 200 O\x7fK\r\n\r\n", BadReason),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
                ContentLengthWithTransferEncoding,
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\n",
                BadContentLength,
            ),
            (b"HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n", BadContentLength),
            (b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\n", BadContentLength),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551616\r\n\r\n",
                BadContentLength,
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n",
                DuplicateContentLength,
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
                UnsupportedTransferEncoding,
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
                UnsupportedTransferEncoding,
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
                UnsupportedTransferEncoding,
            ),
            (
                b"HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                UnsupportedTransferEncoding,
            ),
            (
                b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
                UnsupportedTransferEncoding,
            ),
            (
                b"HTTP/1.1 200 OK\r\nConnection: content-length\r\n\r\n",
                BadConnection,
            ),
            (
                b"HTTP/1.1 200 OK\r\nConnection: close, Transfer-Encoding\r\n\r\n",
                BadConnection,
            ),
            (b"HTTP/1.1 200 OK\r\nConnection: HOST\r\n\r\n", BadConnection),
        ];
        for (input, expected) in rows {
            assert_eq!(
                parse(input),
                Err(expected),
                "input {:?}",
                String::from_utf8_lossy(input)
            );
        }
        assert!(matches!(
            parse(b"HTTP/1.1 200 OK\r\nConnection: keep-alive, x-foo\r\n\r\n"),
            Ok(ResponseStatus::Complete { .. })
        ));
    }

    fn tight() -> HeadLimits {
        HeadLimits {
            max_request_line: 40,
            max_head: 40,
            max_headers: 2,
        }
    }

    #[test]
    fn head_limits_apply() {
        let l = tight();
        let advance = |input: &[u8]| ResponseParser::new().advance(input, &l, BodyMode::Full);
        // More than max_head bytes without a blank line, at once ...
        let mut big = b"HTTP/1.1 200 OK\r\nA: ".to_vec();
        big.resize(41, b'x');
        assert_eq!(advance(&big), Err(ResponseError::HeadTooLarge));
        big.truncate(40);
        assert_eq!(advance(&big), Ok(ResponseStatus::Incomplete));
        // ... and one byte at a time.
        let mut parser = ResponseParser::new();
        let mut big = b"HTTP/1.1 200 OK\r\nA: ".to_vec();
        big.resize(41, b'x');
        for n in 1..=40 {
            let prefix = big.get(..n).unwrap_or_default();
            assert_eq!(
                parser.advance(prefix, &l, BodyMode::Full),
                Ok(ResponseStatus::Incomplete),
                "prefix of {n} bytes"
            );
        }
        assert_eq!(
            parser.advance(&big, &l, BodyMode::Full),
            Err(ResponseError::HeadTooLarge)
        );
        // A head of exactly max_head bytes is accepted, one more is not.
        let mut exact = b"HTTP/1.1 200 OK\r\nA: ".to_vec();
        let fill = 40 - exact.len() - 4;
        exact.extend(std::iter::repeat_n(b'x', fill));
        exact.extend_from_slice(b"\r\n\r\n");
        assert_eq!(exact.len(), 40);
        assert!(matches!(
            advance(&exact),
            Ok(ResponseStatus::Complete { .. })
        ));
        exact.insert(25, b'x');
        assert_eq!(advance(&exact), Err(ResponseError::HeadTooLarge));
        // Header lines: two are fine, three are not.
        assert!(matches!(
            advance(b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\n\r\n"),
            Ok(ResponseStatus::Complete { .. })
        ));
        assert_eq!(
            advance(b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n"),
            Err(ResponseError::TooManyHeaders)
        );
        // The same without a blank line yet: reported as soon as certain.
        assert_eq!(
            advance(b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\nC: 3\r\n"),
            Err(ResponseError::TooManyHeaders)
        );
    }

    /// Feeds `input` to one parser in pieces ending at each of `cuts` (and
    /// finally the whole input); returns the first verdict and the length of
    /// the prefix that produced it.
    fn feed(
        input: &[u8],
        cuts: &[usize],
        limits: &HeadLimits,
        request: BodyMode,
    ) -> (Result<ResponseStatus, ResponseError>, usize) {
        let mut parser = ResponseParser::new();
        let mut ends: Vec<usize> = cuts.iter().map(|&c| c.min(input.len())).collect();
        ends.sort_unstable();
        ends.push(input.len());
        for end in ends {
            let result = parser.advance(input.get(..end).unwrap_or_default(), limits, request);
            if result != Ok(ResponseStatus::Incomplete) {
                return (result, end);
            }
        }
        (Ok(ResponseStatus::Incomplete), input.len())
    }

    #[test]
    fn scanning_is_linear() {
        let l = HeadLimits {
            max_request_line: 64 * 1024,
            max_head: 64 * 1024,
            max_headers: 10_000,
        };
        let mut head = b"HTTP/1.1 200 OK\r\n".to_vec();
        while head.len() < 16 * 1024 {
            head.extend_from_slice(b"X-Filler: 0123456789012345678901234567\r\n");
        }
        head.extend_from_slice(b"\r\n");
        let cuts: Vec<usize> = (1..head.len()).collect();
        work::take();
        let (result, _) = feed(&head, &cuts, &l, BodyMode::Full);
        let used = work::take();
        assert!(matches!(result, Ok(ResponseStatus::Complete { .. })));
        let n = head.len();
        assert!(
            used.scanned <= n && used.judged <= n && used.built <= n,
            "{used:?} for {n} bytes"
        );
        assert!(used.scanned >= n - 1, "{used:?} for {n} bytes");
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = parse(&bytes);
            let _ = ResponseParser::new().advance(&bytes, &tight(), BodyMode::HeadOnly);
        }

        #[test]
        fn split_anywhere_gives_the_same_verdict(
            pieces in proptest::collection::vec(
                prop_oneof![
                    Just(b"HTTP/1.1 200 OK\r\n".to_vec()),
                    Just(b"HTTP/1.0 200 OK\r\n".to_vec()),
                    Just(b"HTTP/1.1 100 Continue\r\n\r\n".to_vec()),
                    Just(b"HTTP/1.1 103 Early\r\n".to_vec()),
                    Just(b"HTTP/1.1 101 Switch\r\n".to_vec()),
                    Just(b"HTTP/1.1 204 No\r\n".to_vec()),
                    Just(b"HTTP/2 200 OK\r\n".to_vec()),
                    Just(b"HTTP/1.1 200".to_vec()),
                    Just(b"Content-Length: 5\r\n".to_vec()),
                    Just(b"Transfer-Encoding: chunked\r\n".to_vec()),
                    Just(b"Connection: close\r\n".to_vec()),
                    Just(b"Connection: host\r\n".to_vec()),
                    Just(b"X-Y: z".to_vec()),
                    Just(b" ".to_vec()),
                    Just(b"\r".to_vec()),
                    Just(b"\n".to_vec()),
                    Just(b"\r\n".to_vec()),
                    Just(b"\r\n\r\n".to_vec()),
                    Just(b"\0".to_vec()),
                    proptest::collection::vec(any::<u8>(), 0..4),
                ],
                0..24,
            ),
            cuts in proptest::collection::vec(0..400usize, 0..12),
            which in 0..4usize,
            head_only in any::<bool>(),
        ) {
            let input: Vec<u8> = pieces.concat();
            let limits = match which {
                0 => defaults(),
                1 => tight(),
                2 => HeadLimits { max_request_line: 20, max_head: 20, max_headers: 3 },
                _ => HeadLimits { max_request_line: 20, max_head: 22, max_headers: 3 },
            };
            let request = if head_only { BodyMode::HeadOnly } else { BodyMode::Full };
            let whole = |bytes: &[u8]| ResponseParser::new().advance(bytes, &limits, request);
            let (verdict, at) = feed(&input, &cuts, &limits, request);
            prop_assert_eq!(&verdict, &whole(input.get(..at).unwrap_or_default()));
            match &verdict {
                Ok(ResponseStatus::Incomplete) => {}
                other => prop_assert_eq!(other, &whole(&input)),
            }
        }
    }
}
