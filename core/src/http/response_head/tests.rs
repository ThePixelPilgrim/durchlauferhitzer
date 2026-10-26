use super::*;
use crate::config::model::Limits;
use crate::http::request::work;
use proptest::prelude::*;

fn defaults() -> HeadLimits {
    HeadLimits::from(&Limits::default())
}

fn tight() -> HeadLimits {
    HeadLimits {
        max_request_line: 20,
        max_head: 60,
        max_headers: 2,
    }
}

fn parse_with(
    input: &[u8],
    limits: &HeadLimits,
    kind: RequestKind,
) -> Result<ResponseStatus, ResponseError> {
    ResponseParser::new().advance(input, limits, kind)
}

fn parse(input: &[u8]) -> Result<ResponseStatus, ResponseError> {
    parse_with(input, &defaults(), RequestKind::Other)
}

fn head_of(input: &[u8], kind: RequestKind) -> ResponseHead {
    match parse_with(input, &defaults(), kind) {
        Ok(ResponseStatus::Complete { head, consumed }) => {
            assert_eq!(consumed, input.len());
            head
        }
        other => panic!("expected complete head, got {other:?}"),
    }
}

const SIMPLE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer:  x \t\r\n\r\n";

#[test]
fn parses_simple_200() {
    assert_eq!(
        head_of(SIMPLE, RequestKind::Other),
        ResponseHead {
            status: 200,
            reason: "OK".to_string(),
            headers: vec![
                Header {
                    name: "content-length".to_string(),
                    value: b"5".to_vec(),
                },
                Header {
                    name: "server".to_string(),
                    value: b"x".to_vec(),
                },
            ],
            body: ResponseBody::Length(5),
        }
    );
    // Body bytes after the head are left alone.
    let mut with_body = SIMPLE.to_vec();
    with_body.extend_from_slice(b"hello");
    match parse(&with_body) {
        Ok(ResponseStatus::Complete { consumed, .. }) => assert_eq!(consumed, SIMPLE.len()),
        other => panic!("unexpected {other:?}"),
    }
    // Every proper prefix is incomplete.
    for n in 0..SIMPLE.len() {
        assert_eq!(
            parse(SIMPLE.get(..n).unwrap_or_default()),
            Ok(ResponseStatus::Incomplete),
            "prefix of {n} bytes"
        );
    }
}

#[test]
fn reason_forms() {
    let reason = |line: &[u8]| {
        let mut input = line.to_vec();
        input.extend_from_slice(b"\r\nContent-Length: 0\r\n\r\n");
        head_of(&input, RequestKind::Other).reason
    };
    assert_eq!(reason(b"HTTP/1.1 200 "), "");
    assert_eq!(reason(b"HTTP/1.0 404 Not Found"), "Not Found");
    assert_eq!(reason(b"HTTP/1.1 599 a\tb ~!"), "a\tb ~!");
}

#[test]
fn interim_100_is_reported_and_skipped() {
    let interim: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
    let mut input = interim.to_vec();
    input.extend_from_slice(SIMPLE);
    assert_eq!(
        parse(&input),
        Ok(ResponseStatus::Interim {
            consumed: interim.len()
        })
    );
    // The caller restarts at `consumed` with a fresh parser.
    let rest = input.get(interim.len()..).unwrap_or_default();
    assert_eq!(
        parse(rest),
        Ok(ResponseStatus::Complete {
            head: head_of(SIMPLE, RequestKind::Other),
            consumed: SIMPLE.len(),
        })
    );
    // Other interim statuses, with headers, are reported the same way.
    for head in [
        &b"HTTP/1.1 102 Processing\r\n\r\n"[..],
        b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\n",
        b"HTTP/1.1 199 \r\n\r\n",
        b"HTTP/1.0 100 Continue\r\n\r\n",
    ] {
        assert_eq!(
            parse_with(head, &defaults(), RequestKind::Head),
            Ok(ResponseStatus::Interim {
                consumed: head.len()
            }),
            "{:?}",
            String::from_utf8_lossy(head)
        );
    }
    // An interim head is judged like any other: incomplete until its blank
    // line, and its header lines must be valid.
    assert_eq!(
        parse(b"HTTP/1.1 100 Continue\r\n"),
        Ok(ResponseStatus::Incomplete)
    );
    assert_eq!(
        parse(b"HTTP/1.1 100 Continue\r\nBad Name: x\r\n\r\n"),
        Err(ResponseError::BadHeaderName)
    );
}

#[test]
fn switching_protocols_is_an_error() {
    assert_eq!(
        parse(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"),
        Err(ResponseError::SwitchingProtocols)
    );
    // Decided as soon as the status line is judged.
    assert_eq!(
        parse(b"HTTP/1.1 101 Switching Protocols\r\n"),
        Err(ResponseError::SwitchingProtocols)
    );
    assert_eq!(
        parse(b"HTTP/1.1 101 Switching Protocols"),
        Ok(ResponseStatus::Incomplete)
    );
}

/// Interim heads are judged like final ones: header syntax line by line,
/// and the framing headers too, although a 1xx has no body. A 101 is
/// rejected at its status line, before any header is looked at.
#[test]
fn interim_heads_are_judged_like_final_ones() {
    use ResponseError::*;
    let rows: Vec<(&[u8], ResponseError)> = vec![
        (b"HTTP/1.1 100 Continue\r\nA : b\r\n\r\n", BadHeaderName),
        (b"HTTP/1.1 103 Early\r\nNoColon\r\n\r\n", BadHeaderName),
        (b"HTTP/1.1 100 Continue\r\nA: b\r\n c\r\n\r\n", ObsFold),
        (b"HTTP/1.1 100 Continue\r\nA: a\0b\r\n\r\n", BadHeaderValue),
        (b"HTTP/1.1 100 Continue\r\nA: b\n\r\n", BareLf),
        (b"HTTP/1.1 101 Switch\r\nA : b\r\n\r\n", SwitchingProtocols),
        (
            b"HTTP/1.1 103 Early\r\nContent-Length: x\r\n\r\n",
            BadContentLength,
        ),
        (
            b"HTTP/1.1 103 Early\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            DuplicateContentLength,
        ),
        (
            b"HTTP/1.1 103 Early\r\nTransfer-Encoding: gzip\r\n\r\n",
            UnsupportedTransferEncoding,
        ),
        (
            b"HTTP/1.1 103 Early\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            ContentLengthWithTransferEncoding,
        ),
    ];
    for (input, expected) in rows {
        for kind in [RequestKind::Other, RequestKind::Head] {
            assert_eq!(
                parse_with(input, &defaults(), kind),
                Err(expected),
                "input {:?}",
                String::from_utf8_lossy(input)
            );
        }
    }
}

/// The first bad line wins: a line-ending fault later in the head does not
/// mask an earlier header fault, nor the other way round.
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

/// An interim head followed by the final one, fed one byte at a time into
/// one buffer that drops consumed bytes; a fresh parser starts after the
/// interim head, as the contract requires.
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
        match parser.advance(&pending, &limits, RequestKind::Other) {
            Ok(ResponseStatus::Incomplete) => {}
            Ok(ResponseStatus::Interim { consumed }) => {
                verdicts.push(("interim", consumed));
                pending.drain(..consumed);
                parser = ResponseParser::new();
            }
            Ok(ResponseStatus::Complete { consumed, head }) => {
                assert_eq!(head.body, ResponseBody::Length(2));
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
fn rejects_malformed_responses() {
    use ResponseError::*;
    let d = defaults();
    let rows: Vec<(&[u8], HeadLimits, ResponseError)> = vec![
        (
            b"HTTP/1.1 200 OK\r\nA: 1234567890123456789012345678901234567890\r\n\r\n",
            tight(),
            HeadTooLarge,
        ),
        (
            b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n",
            tight(),
            TooManyHeaders,
        ),
        (b"HTTP/1.1 200 O\rK\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 200 OK\r\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 099 Low\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 600 High\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 20 OK\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 2000 OK\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 2x0 OK\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 +20 OK\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 200\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1  200 OK\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 200 a\0b\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 200 a\x7fb\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 200 caf\xc3\xa9\r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1\r\n\r\n", d, BadStatusLine),
        (b"\r\nHTTP/1.1 200 OK\r\n\r\n", d, BadStatusLine),
        (b"HTTP/2.0 200 OK\r\n\r\n", d, BadVersion),
        (b"HTTP/1.2 200 OK\r\n\r\n", d, BadVersion),
        (b"http/1.1 200 OK\r\n\r\n", d, BadVersion),
        (b"HTTP/1.1x 200 OK\r\n\r\n", d, BadVersion),
        (b"HTTP/1.1 200 OK\r\nBad Name: x\r\n\r\n", d, BadHeaderName),
        (b"HTTP/1.1 200 OK\r\n: x\r\n\r\n", d, BadHeaderName),
        (b"HTTP/1.1 200 OK\r\nNoColon\r\n\r\n", d, BadHeaderName),
        (b"HTTP/1.1 200 OK\r\nX: a\rb\r\n\r\n", d, BadHeaderValue),
        (b"HTTP/1.1 200 OK\r\nX: a\0b\r\n\r\n", d, BadHeaderValue),
        (b"HTTP/1.1 200 OK\n\n", d, BareLf),
        (b"HTTP/1.1 200 OK\r\nX: a\n\r\n", d, BareLf),
        (b"HTTP/1.1 200 OK\r\n\n", d, BareLf),
        (b"HTTP/1.1 200 OK\r\nX: a\r\n Y: b\r\n\r\n", d, ObsFold),
        (b"HTTP/1.1 200 OK\r\n\tY: b\r\n\r\n", d, ObsFold),
        (b"HTTP/1.1 200 OK\r\nA: b\r\n\tc\r\n\r\n", d, ObsFold),
        (b"HTTP/2 200 OK\r\n\r\n", d, BadVersion),
        (b"HTTP/1.1 \r\n\r\n", d, BadStatusLine),
        (b"HTTP/1.1 200\tOK\r\n\r\n", d, BadStatusLine),
        (b"\r\n", d, BadStatusLine),
        (b"HTTP/1.1 200 OK\x01\r\n\r\n", d, BadStatusLine),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length : 5\r\n\r\n",
            d,
            BadHeaderName,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\n",
            d,
            BadContentLength,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n",
            d,
            BadContentLength,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\n",
            d,
            BadContentLength,
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
            d,
            UnsupportedTransferEncoding,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
            d,
            ContentLengthWithTransferEncoding,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
            d,
            BadContentLength,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            d,
            BadContentLength,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551616\r\n\r\n",
            d,
            BadContentLength,
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n",
            d,
            DuplicateContentLength,
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            d,
            UnsupportedTransferEncoding,
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
            d,
            UnsupportedTransferEncoding,
        ),
        (
            b"HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            d,
            UnsupportedTransferEncoding,
        ),
        (
            b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
            d,
            UnsupportedTransferEncoding,
        ),
        // Framing headers are checked even where no body is allowed.
        (
            b"HTTP/1.1 204 No Content\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n",
            d,
            DuplicateContentLength,
        ),
        (
            b"HTTP/1.1 101 Switching Protocols\r\n\r\n",
            d,
            SwitchingProtocols,
        ),
    ];
    for (input, limits, expected) in rows {
        for kind in [RequestKind::Other, RequestKind::Head] {
            assert_eq!(
                parse_with(input, &limits, kind),
                Err(expected),
                "input {:?}",
                String::from_utf8_lossy(input)
            );
        }
    }
}

#[test]
fn limits_are_enforced() {
    let l = HeadLimits {
        max_request_line: 8,
        max_head: 40,
        max_headers: 1,
    };
    // A status line longer than max_request_line is fine: only max_head
    // applies to responses.
    let ok = b"HTTP/1.1 200 A long reason\r\n\r\n";
    assert!(matches!(
        parse_with(ok, &l, RequestKind::Other),
        Ok(ResponseStatus::Complete { .. })
    ));
    // An unterminated status line over max_head is certainly too large.
    let mut long = b"HTTP/1.1 200 ".to_vec();
    long.resize(41, b'x');
    assert_eq!(
        parse_with(&long, &l, RequestKind::Other),
        Err(ResponseError::HeadTooLarge)
    );
    long.truncate(40);
    assert_eq!(
        parse_with(&long, &l, RequestKind::Other),
        Ok(ResponseStatus::Incomplete)
    );
    // A head of exactly max_head bytes (blank line included) is accepted,
    // one byte more is not.
    let mut exact = b"HTTP/1.1 200 OK\r\nA: ".to_vec();
    let fill = 40 - exact.len() - 4;
    exact.extend(std::iter::repeat_n(b'x', fill));
    exact.extend_from_slice(b"\r\n\r\n");
    assert_eq!(exact.len(), 40);
    assert!(matches!(
        parse_with(&exact, &l, RequestKind::Other),
        Ok(ResponseStatus::Complete { .. })
    ));
    let mut over = exact.clone();
    over.insert(20, b'x');
    assert_eq!(
        parse_with(&over, &l, RequestKind::Other),
        Err(ResponseError::HeadTooLarge)
    );
    // Limits are judged before syntax.
    assert_eq!(
        parse_with(
            b"HTTP/1.1 200 OK\r\nA: 1\r\nBad Name\r\n\r\n",
            &l,
            RequestKind::Other
        ),
        Err(ResponseError::TooManyHeaders)
    );

    // Reported as soon as certain, also fed one byte at a time to one
    // parser, and with no blank line yet.
    let t = HeadLimits {
        max_request_line: 40,
        max_head: 40,
        max_headers: 2,
    };
    let mut parser = ResponseParser::new();
    let mut big = b"HTTP/1.1 200 OK\r\nA: ".to_vec();
    big.resize(41, b'x');
    for n in 1..=40 {
        assert_eq!(
            parser.advance(big.get(..n).unwrap_or_default(), &t, RequestKind::Other),
            Ok(ResponseStatus::Incomplete),
            "prefix of {n} bytes"
        );
    }
    assert_eq!(
        parser.advance(&big, &t, RequestKind::Other),
        Err(ResponseError::HeadTooLarge)
    );
    assert!(matches!(
        parse_with(
            b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\n\r\n",
            &t,
            RequestKind::Other
        ),
        Ok(ResponseStatus::Complete { .. })
    ));
    assert_eq!(
        parse_with(
            b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\nC: 3\r\n",
            &t,
            RequestKind::Other
        ),
        Err(ResponseError::TooManyHeaders)
    );
}

#[test]
fn body_framing_cases() {
    use RequestKind::{Head, Other};
    let rows: &[(&[u8], RequestKind, ResponseBody)] = &[
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
            Other,
            ResponseBody::Length(5),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 007\r\n\r\n",
            Other,
            ResponseBody::Length(7),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n",
            Other,
            ResponseBody::Length(u64::MAX),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            Other,
            ResponseBody::None,
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            Other,
            ResponseBody::Chunked,
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked \r\n\r\n",
            Other,
            ResponseBody::Chunked,
        ),
        (b"HTTP/1.1 200 OK\r\n\r\n", Other, ResponseBody::UntilClose),
        (
            b"HTTP/1.0 200 OK\r\nServer: x\r\n\r\n",
            Other,
            ResponseBody::UntilClose,
        ),
        (
            b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\n\r\n",
            Other,
            ResponseBody::Length(3),
        ),
        (
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\n",
            Other,
            ResponseBody::Length(3),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
            Head,
            ResponseBody::None,
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            Head,
            ResponseBody::None,
        ),
        (b"HTTP/1.1 200 OK\r\n\r\n", Head, ResponseBody::None),
        (
            b"HTTP/1.1 204 No Content\r\n\r\n",
            Other,
            ResponseBody::None,
        ),
        (
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            Other,
            ResponseBody::None,
        ),
        (
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 5\r\n\r\n",
            Other,
            ResponseBody::None,
        ),
        (
            b"HTTP/1.1 304 Not Modified\r\n\r\n",
            Other,
            ResponseBody::None,
        ),
    ];
    for (input, kind, body) in rows {
        assert_eq!(
            head_of(input, *kind).body,
            *body,
            "{:?} {kind:?}",
            String::from_utf8_lossy(input)
        );
    }
    // Framing headers stay in `headers` whatever the body is.
    let head = head_of(
        b"HTTP/1.1 304 Not Modified\r\nContent-Length: 5\r\n\r\n",
        Other,
    );
    assert_eq!(
        head.headers,
        vec![Header {
            name: "content-length".to_string(),
            value: b"5".to_vec(),
        }]
    );
    assert_eq!(
        head_of(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", Other)
            .headers
            .len(),
        1
    );
}

/// Feeds `input` to one `ResponseParser` in pieces ending at each of `cuts`
/// (and finally the whole input); returns the first verdict and the length
/// of the prefix that produced it.
fn feed(
    input: &[u8],
    cuts: &[usize],
    limits: &HeadLimits,
    kind: RequestKind,
) -> (Result<ResponseStatus, ResponseError>, usize) {
    let mut parser = ResponseParser::new();
    let mut ends: Vec<usize> = cuts.iter().map(|&c| c.min(input.len())).collect();
    ends.sort_unstable();
    ends.push(input.len());
    for end in ends {
        let result = parser.advance(input.get(..end).unwrap_or_default(), limits, kind);
        if result != Ok(ResponseStatus::Incomplete) {
            return (result, end);
        }
    }
    (Ok(ResponseStatus::Incomplete), input.len())
}

#[test]
fn parser_fed_line_by_line_does_linear_work() {
    let l = defaults();
    let mut head = b"HTTP/1.1 200 OK\r\n".to_vec();
    let mut cuts = vec![9, head.len()];
    while head.len() + 40 + 2 <= l.max_head && cuts.len() < l.max_headers + 1 {
        head.extend_from_slice(b"X-Filler: 0123456789012345678901234567\r\n");
        cuts.push(head.len());
    }
    head.extend_from_slice(b"\r\n");
    work::take();
    let (result, _) = feed(&head, &cuts, &l, RequestKind::Other);
    let used = work::take();
    assert!(matches!(result, Ok(ResponseStatus::Complete { .. })));
    let n = head.len();
    assert!(
        used.scanned <= n && used.judged <= n && used.built <= n,
        "{used:?} for {n} bytes"
    );
}

#[test]
fn parser_fed_byte_by_byte_does_linear_work() {
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
    let (result, _) = feed(&head, &cuts, &l, RequestKind::Other);
    let used = work::take();
    assert!(matches!(result, Ok(ResponseStatus::Complete { .. })));
    let n = head.len();
    assert!(
        used.scanned <= n && used.judged <= n && used.built <= n,
        "{used:?} for {n} bytes"
    );
    assert!(used.scanned >= n - 1, "{used:?} for {n} bytes");
}

fn limits_for(which: usize) -> HeadLimits {
    match which {
        0 => defaults(),
        1 => tight(),
        2 => HeadLimits {
            max_request_line: 20,
            max_head: 20,
            max_headers: 3,
        },
        _ => HeadLimits {
            max_request_line: 20,
            max_head: 22,
            max_headers: 3,
        },
    }
}

fn any_kind() -> impl Strategy<Value = RequestKind> {
    prop_oneof![Just(RequestKind::Head), Just(RequestKind::Other)]
}

proptest! {
    #[test]
    fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096), kind in any_kind()) {
        let _ = parse_with(&bytes, &defaults(), kind);
        let _ = parse_with(&bytes, &tight(), kind);
        let mut input = b"HTTP/1.1 200 OK\r\n".to_vec();
        input.extend_from_slice(&bytes);
        let _ = parse_with(&input, &defaults(), kind);
        let _ = parse_with(&input, &tight(), kind);
    }

    #[test]
    fn prefix_never_contradicts_full(
        pieces in proptest::collection::vec(
            prop_oneof![
                Just(b"HTTP/1.1".to_vec()),
                Just(b"HTTP/1.0".to_vec()),
                Just(b"HTTP/2.0".to_vec()),
                Just(b" ".to_vec()),
                Just(b"200".to_vec()),
                Just(b"100".to_vec()),
                Just(b"101".to_vec()),
                Just(b"204".to_vec()),
                Just(b"304".to_vec()),
                Just(b"99".to_vec()),
                Just(b"OK".to_vec()),
                Just(b"Content-Length: 5".to_vec()),
                Just(b"Content-Length: 0".to_vec()),
                Just(b"Transfer-Encoding: chunked".to_vec()),
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
        kind in any_kind(),
    ) {
        let input: Vec<u8> = pieces.concat();
        let limits = limits_for(which);
        let full = parse_with(&input, &limits, kind);
        for n in 0..=input.len() {
            let prefix = input.get(..n).unwrap_or_default();
            match parse_with(prefix, &limits, kind) {
                Ok(ResponseStatus::Incomplete) => {}
                other => prop_assert_eq!(&other, &full, "prefix of {} bytes", n),
            }
        }
    }

    #[test]
    fn incremental_parser_matches_stateless(
        pieces in proptest::collection::vec(
            prop_oneof![
                Just(b"HTTP/1.1 200 OK\r\n".to_vec()),
                Just(b"HTTP/1.0 100 Continue\r\n".to_vec()),
                Just(b"Content-Length: 5\r\n".to_vec()),
                Just(b"Transfer-Encoding: chunked\r\n".to_vec()),
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
        kind in any_kind(),
    ) {
        let input: Vec<u8> = pieces.concat();
        let limits = limits_for(which);
        let (verdict, at) = feed(&input, &cuts, &limits, kind);
        prop_assert_eq!(&verdict, &parse_with(input.get(..at).unwrap_or_default(), &limits, kind));
        match &verdict {
            Ok(ResponseStatus::Incomplete) => {}
            other => prop_assert_eq!(other, &parse_with(&input, &limits, kind)),
        }
    }
}
