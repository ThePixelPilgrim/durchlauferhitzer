use super::*;
use crate::http::status::Status;
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
            expect: Expect::None,
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

/// FR-HTTP-09a: a `Connection` header may not name a framing header or
/// `Host`, in any `Connection` line, in any case, on either version.
#[test]
fn connection_naming_framing_headers_is_rejected() {
    use HttpError::ConnectionNamesFraming;
    const L: &str = "GET / HTTP/1.1\r\nHost: a\r\n";
    let with = |tail: &str| parse(format!("{L}{tail}\r\n").as_bytes());
    let rejected = [
        "Connection: close, Content-Length\r\n",
        "Connection: transfer-encoding\r\n",
        "Connection: HOST\r\n",
        "Connection: keep-alive\r\nConnection: host\r\n",
        "Connection:\tclose ,\tTransfer-Encoding \t\r\n",
        "Connection: ,content-length,\r\n",
        "cOnNeCtIoN: Host\r\n",
    ];
    for tail in rejected {
        assert_eq!(with(tail), Err(ConnectionNamesFraming), "{tail:?}");
    }
    let accepted = [
        "Connection: keep-alive, x-foo\r\n",
        "Connection: ,, close ,\r\n",
        "Connection: content-length-x, xhost, transfer-encodings\r\n",
        "X-Connection: host\r\n",
        "Connection:\r\n",
    ];
    for tail in accepted {
        assert!(
            matches!(with(tail), Ok(ParseStatus::Complete { .. })),
            "{tail:?}: {:?}",
            with(tail)
        );
    }
    assert_eq!(
        parse(b"GET / HTTP/1.0\r\nConnection: Content-Length\r\n\r\n"),
        Err(ConnectionNamesFraming)
    );
}

/// The `Connection` check is the first whole-head check: it wins over the
/// Host (FR-HTTP-07) and framing (FR-HTTP-08) checks, while every per-line
/// fault still wins over it.
#[test]
fn connection_naming_framing_headers_precedence() {
    use HttpError::*;
    let rows: &[(&[u8], HttpError)] = &[
        (
            b"GET / HTTP/1.1\r\nConnection: host\r\n\r\n",
            ConnectionNamesFraming,
        ),
        (
            b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\nConnection: host\r\n\r\n",
            ConnectionNamesFraming,
        ),
        (
            b"GET / HTTP/1.1\r\nHost: a_b\r\nConnection: host\r\n\r\n",
            ConnectionNamesFraming,
        ),
        (
            b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\nConnection: content-length\r\n\r\n",
            ConnectionNamesFraming,
        ),
        (
            b"GET / HTTP/1.0\r\nTransfer-Encoding: chunked\r\nConnection: transfer-encoding\r\n\r\n",
            ConnectionNamesFraming,
        ),
        (
            b"GET / HTTP/1.1\r\nConnection: host\r\nA : b\r\n\r\n",
            BadHeaderName,
        ),
        (
            b"GET / HTTP/1.1\r\nConnection: host\r\nA: b\n\r\n",
            BareLf,
        ),
        (
            b"GET / HTTP/9.9\r\nConnection: host\r\n\r\n",
            UnsupportedVersion,
        ),
    ];
    for &(input, expected) in rows {
        assert_eq!(
            parse(input),
            Err(expected),
            "{:?}",
            String::from_utf8_lossy(input)
        );
    }
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

/// FR-PROXY-25/25a: exactly one `Expect: 100-continue` (any case) on
/// HTTP/1.1 is `Continue`; any other value, a second `Expect` line or a
/// list is an error (417 on every route); HTTP/1.0 ignores `Expect`.
#[test]
fn expect_header() {
    let e = |version: &str, h: &str| {
        parse(format!("POST / {version}\r\nHost: a\r\n{h}\r\n").as_bytes()).map(|s| match s {
            ParseStatus::Complete { head, .. } => head.expect,
            ParseStatus::Incomplete => panic!("incomplete"),
        })
    };
    assert_eq!(e("HTTP/1.1", ""), Ok(Expect::None));
    assert_eq!(
        e("HTTP/1.1", "Expect: 100-continue\r\n"),
        Ok(Expect::Continue)
    );
    assert_eq!(
        e("HTTP/1.1", "expect:  100-Continue \r\n"),
        Ok(Expect::Continue)
    );
    for bad in [
        "Expect: foo\r\n",
        "Expect:\r\n",
        "Expect: 100-continue, foo\r\n",
        "Expect: 100-continue,\r\n",
        "Expect: \"100-continue\"\r\n",
        "Expect: 100-continue\r\nExpect: 100-continue\r\n",
        "Expect: 100-continue\r\nExpect: foo\r\n",
    ] {
        assert_eq!(e("HTTP/1.1", bad), Err(HttpError::BadExpect), "{bad:?}");
    }
    assert_eq!(HttpError::BadExpect.status(), Status::ExpectationFailed);
    // HTTP/1.0 has no `Expect`: ignored, whatever its value.
    assert_eq!(e("HTTP/1.0", "Expect: 100-continue\r\n"), Ok(Expect::None));
    assert_eq!(
        e("HTTP/1.0", "Expect: foo\r\nExpect: bar\r\n"),
        Ok(Expect::None)
    );
}

/// The `Expect` check runs after every other head check, so a framing
/// error wins over a bad `Expect`.
#[test]
fn expect_is_checked_last() {
    assert_eq!(
        err_of(b"POST / HTTP/1.1\r\nHost: a\r\nExpect: foo\r\nContent-Length: x\r\n\r\n"),
        HttpError::BadContentLength
    );
    assert_eq!(
        err_of(b"POST / HTTP/1.1\r\nExpect: foo\r\n\r\n"),
        HttpError::MissingHost
    );
}

/// FR-HTTP-05: obs-text (bytes >= 0x80) in a header value is rejected, so
/// it is never forwarded (FR-PROXY-13).
#[test]
fn obs_text_in_values_is_rejected() {
    for value in [
        &b"caf\xc3\xa9"[..],
        b"\x80",
        b"a\xffb",
        b"\xa0 x",
        b"x\t\x80",
    ] {
        let mut raw = b"GET / HTTP/1.1\r\nHost: a\r\nX: ".to_vec();
        raw.extend_from_slice(value);
        raw.extend_from_slice(b"\r\n\r\n");
        assert_eq!(err_of(&raw), HttpError::BadHeaderValue, "{value:?}");
    }
    // The lowest and highest VCHAR, with inner SP and HTAB, are fine.
    let head = head_of(b"GET / HTTP/1.1\r\nHost: a\r\nX: ! \t~\r\n\r\n");
    assert_eq!(
        head.headers.last().map(|h| h.value.clone()),
        Some(b"! \t~".to_vec())
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

/// The parser remembers the method once the request line is judged
/// valid, so an error found later can still be answered head-only for
/// HEAD; an error in the request line itself leaves it unknown.
#[test]
fn parser_knows_the_method_after_the_request_line() {
    let l = defaults();
    let rows: &[(&[u8], HttpError, KnownMethod)] = &[
        (
            b"HEAD / HTTP/1.1\r\n\r\n",
            HttpError::MissingHost,
            KnownMethod::Head,
        ),
        (
            b"GET / HTTP/1.1\r\n\r\n",
            HttpError::MissingHost,
            KnownMethod::Get,
        ),
        (
            b"PUT / HTTP/1.1\r\n\r\n",
            HttpError::MissingHost,
            KnownMethod::Other,
        ),
        (
            b"HEAD / HTTP/1.1\r\nHost: a\r\nContent-Length: x\r\n\r\n",
            HttpError::BadContentLength,
            KnownMethod::Head,
        ),
        (
            b"HEAD / HTTP/1.1\r\nBad Name: 1\r\n",
            HttpError::BadHeaderName,
            KnownMethod::Head,
        ),
        (
            b"HEAD / HTTP/1.1\r\nA: 1\n",
            HttpError::BareLf,
            KnownMethod::Head,
        ),
        (
            b"HEAD /a b HTTP/1.1\r\n",
            HttpError::BadRequestLine,
            KnownMethod::Unknown,
        ),
        (
            b"HEAD /%2e HTTP/1.1\r\n",
            HttpError::BadTarget(TargetError::EncodedDot),
            KnownMethod::Unknown,
        ),
        (
            b"HEAD / HTTP/1.2\r\n",
            HttpError::UnsupportedVersion,
            KnownMethod::Unknown,
        ),
        (
            b"HEAD / HTTP/1.1\n",
            HttpError::BareLf,
            KnownMethod::Unknown,
        ),
    ];
    for (input, error, method) in rows {
        let mut parser = HeadParser::new();
        assert_eq!(parser.method(), KnownMethod::Unknown);
        assert_eq!(
            parser.advance(input, &l),
            Err(*error),
            "{:?}",
            String::from_utf8_lossy(input)
        );
        assert_eq!(
            parser.method(),
            *method,
            "{:?}",
            String::from_utf8_lossy(input)
        );
    }
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
