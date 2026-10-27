use super::*;
use crate::config::model::Limits;
use crate::http::request::{HeadLimits, ParseStatus, Version, parse_head};
use crate::http::response_head::{RequestKind, ResponseBody, ResponseParser, ResponseStatus};
use crate::routing::strip_segments;
use proptest::prelude::*;
use std::net::{Ipv4Addr, Ipv6Addr};

fn limits() -> HeadLimits {
    HeadLimits::from(&Limits::default())
}

fn request(raw: &[u8]) -> RequestHead {
    match parse_head(raw, &limits()) {
        Ok(ParseStatus::Complete { head, .. }) => head,
        other => panic!("expected complete head, got {other:?}"),
    }
}

fn response(raw: &[u8]) -> ResponseHead {
    match ResponseParser::new().advance(raw, &limits(), RequestKind::Other) {
        Ok(ResponseStatus::Complete { head, .. }) => head,
        other => panic!("expected complete head, got {other:?}"),
    }
}

/// Renders the backend request head and checks that the announced length
/// is exactly the number of bytes rendered.
fn render_request(head: &RequestHead, path: &str, peer: IpAddr) -> String {
    let mut out = Vec::new();
    render_backend_request(head, path, peer, &mut out);
    assert_eq!(backend_request_len(head, path, peer), out.len());
    String::from_utf8(out).unwrap()
}

fn rendered_request(raw: &[u8], path: &str, peer: IpAddr) -> String {
    render_request(&request(raw), path, peer)
}

fn render_response(head: &ResponseHead, persistence: Persistence) -> String {
    let mut out = Vec::new();
    render_client_response(head, persistence, &mut out);
    assert_eq!(client_response_len(head, persistence), out.len());
    String::from_utf8(out).unwrap()
}

fn rendered_response(raw: &[u8], persistence: Persistence) -> String {
    render_response(&response(raw), persistence)
}

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

fn v6(text: &str) -> IpAddr {
    IpAddr::V6(text.parse::<Ipv6Addr>().unwrap())
}

const TAIL: &str = "x-forwarded-proto: http\r\nconnection: close\r\n\r\n";

#[test]
fn simple_get_with_strip() {
    let head = request(b"GET /api/x HTTP/1.1\r\nHost: a.test\r\nUser-Agent: t\r\n\r\n");
    let path = strip_segments(&head.target.path, 1);
    assert_eq!(
        render_request(&head, &path, v4(192, 0, 2, 1)),
        format!(
            "GET /x HTTP/1.1\r\nhost: a.test\r\nuser-agent: t\r\n\
             x-forwarded-for: 192.0.2.1\r\n{TAIL}"
        )
    );
    // Stripping the whole path leaves `/`.
    let head = request(b"GET /api HTTP/1.1\r\nHost: a\r\n\r\n");
    let path = strip_segments(&head.target.path, 1);
    assert!(render_request(&head, &path, v4(1, 2, 3, 4)).starts_with("GET / HTTP/1.1\r\n"));
}

#[test]
fn query_is_kept_unchanged() {
    let head = request(b"GET /api/x/?a=1&b=%20&c=%3F HTTP/1.1\r\nHost: a\r\n\r\n");
    let path = strip_segments(&head.target.path, 1);
    assert!(
        render_request(&head, &path, v4(1, 2, 3, 4))
            .starts_with("GET /x/?a=1&b=%20&c=%3F HTTP/1.1\r\n")
    );
    // An empty query keeps its `?`.
    let out = rendered_request(b"GET /p? HTTP/1.1\r\nHost: a\r\n\r\n", "/p", v4(1, 2, 3, 4));
    assert!(out.starts_with("GET /p? HTTP/1.1\r\n"));
}

/// FR-PROXY-04 (owner decision 2026-10-27): the request line mirrors the
/// client's version, so a backend never answers an HTTP/1.0 client with a
/// chunked body.
#[test]
fn request_line_mirrors_the_client_version_and_keeps_the_method() {
    let out = rendered_request(b"GET /p HTTP/1.0\r\nHost: a\r\n\r\n", "/p", v4(1, 2, 3, 4));
    assert!(out.starts_with("GET /p HTTP/1.0\r\nhost: a\r\n"));
    let out = rendered_request(b"HEAD /p HTTP/1.0\r\n\r\n", "/p", v4(1, 2, 3, 4));
    assert!(out.starts_with("HEAD /p HTTP/1.0\r\nx-forwarded-for: "));
    let out = rendered_request(b"GET /p HTTP/1.1\r\nHost: a\r\n\r\n", "/p", v4(1, 2, 3, 4));
    assert!(out.starts_with("GET /p HTTP/1.1\r\nhost: a\r\n"));
    let out = rendered_request(
        b"PURGE /p HTTP/1.1\r\nHost: a\r\n\r\n",
        "/p",
        v4(1, 2, 3, 4),
    );
    assert!(out.starts_with("PURGE /p HTTP/1.1\r\n"));
}

#[test]
fn connection_and_the_headers_it_names_are_removed() {
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, X-Secret\r\n\
        X-Secret: s\r\nx-secret: t\r\nX-Kept: k\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(192, 0, 2, 1)),
        format!("GET /p HTTP/1.1\r\nhost: a\r\nx-kept: k\r\nx-forwarded-for: 192.0.2.1\r\n{TAIL}")
    );
}

#[test]
fn several_connection_headers_and_odd_tokens() {
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nConnection: x-a\r\nX-A: 1\r\n\
        Connection: ,, X-B ,\r\nX-B: 2\r\nX-C: 3\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(1, 2, 3, 4)),
        format!("GET /p HTTP/1.1\r\nhost: a\r\nx-c: 3\r\nx-forwarded-for: 1.2.3.4\r\n{TAIL}")
    );
}

#[test]
fn fixed_hop_by_hop_headers_are_removed() {
    let raw = b"POST /p HTTP/1.1\r\nHost: a\r\nKeep-Alive: 5\r\nTE: trailers\r\n\
        Trailer: X\r\nUpgrade: h2c\r\nProxy-Authorization: x\r\nProxy-Connection: y\r\n\
        Expect: 100-continue\r\nProxy: 1\r\nProxyx: 2\r\nContent-Length: 3\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(1, 2, 3, 4)),
        format!(
            "POST /p HTTP/1.1\r\nhost: a\r\nproxy: 1\r\nproxyx: 2\r\ncontent-length: 3\r\n\
             x-forwarded-for: 1.2.3.4\r\n{TAIL}"
        )
    );
}

#[test]
fn forwarded_for_is_appended_to_the_client_value() {
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 10.0.0.1, 10.0.0.2\r\n\
        X-A: b\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(192, 0, 2, 1)),
        format!(
            "GET /p HTTP/1.1\r\nhost: a\r\nx-a: b\r\n\
             x-forwarded-for: 10.0.0.1, 10.0.0.2, 192.0.2.1\r\n{TAIL}"
        )
    );
}

#[test]
fn forwarded_for_shows_the_peer_in_canonical_form() {
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\n\r\n";
    let cases = [
        (v4(192, 0, 2, 1), "192.0.2.1"),
        (v6("2001:0db8:0000:0000:0000:0000:0000:0001"), "2001:db8::1"),
        (v6("::1"), "::1"),
        (v6("::ffff:192.0.2.7"), "192.0.2.7"),
        (v6("::ffff:c000:207"), "192.0.2.7"),
    ];
    for (peer, shown) in cases {
        assert_eq!(
            rendered_request(raw, "/p", peer),
            format!("GET /p HTTP/1.1\r\nhost: a\r\nx-forwarded-for: {shown}\r\n{TAIL}")
        );
    }
}

#[test]
fn forwarded_proto_from_the_client_is_replaced() {
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nX-Forwarded-Proto: https\r\nX-A: b\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(1, 2, 3, 4)),
        format!("GET /p HTTP/1.1\r\nhost: a\r\nx-a: b\r\nx-forwarded-for: 1.2.3.4\r\n{TAIL}")
    );
}

#[test]
fn content_length_is_rendered_without_leading_zeros() {
    for (sent, shown) in [
        ("007", "7"),
        ("7", "7"),
        ("000", "0"),
        ("0", "0"),
        ("100", "100"),
    ] {
        let raw = format!("POST /p HTTP/1.1\r\nHost: a\r\nContent-Length: {sent}\r\n\r\n");
        assert_eq!(
            rendered_request(raw.as_bytes(), "/p", v4(1, 2, 3, 4)),
            format!(
                "POST /p HTTP/1.1\r\nhost: a\r\ncontent-length: {shown}\r\n\
                 x-forwarded-for: 1.2.3.4\r\n{TAIL}"
            ),
            "{sent}"
        );
    }
}

#[test]
fn chunked_request_keeps_transfer_encoding() {
    let raw = b"POST /p HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(1, 2, 3, 4)),
        format!(
            "POST /p HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\n\
             x-forwarded-for: 1.2.3.4\r\n{TAIL}"
        )
    );
}

#[test]
fn rendering_appends_to_the_buffer() {
    let head = request(b"GET /p HTTP/1.1\r\nHost: a\r\n\r\n");
    let mut out = b"keep".to_vec();
    render_backend_request(&head, "/p", v4(1, 2, 3, 4), &mut out);
    assert!(out.starts_with(b"keepGET /p HTTP/1.1\r\n"));
    let head = response(b"HTTP/1.1 204 No Content\r\n\r\n");
    let mut out = b"keep".to_vec();
    render_client_response(&head, Persistence::KeepAlive, &mut out);
    assert_eq!(out, b"keepHTTP/1.1 204 No Content\r\n\r\n");
}

#[test]
fn check_forwardable_rejects_more_than_one_forwarded_for_line() {
    let ok = [
        &b"GET /p HTTP/1.1\r\nHost: a\r\n\r\n"[..],
        b"GET /p HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 1.1.1.1, 2.2.2.2\r\n\r\n",
    ];
    for raw in ok {
        assert_eq!(check_forwardable(&request(raw)), Ok(()));
    }
    let twice = b"GET /p HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 1.1.1.1\r\n\
        x-forwarded-for: 2.2.2.2\r\n\r\n";
    assert_eq!(
        check_forwardable(&request(twice)),
        Err(ForwardError::DuplicateForwardedFor)
    );
    let empty_twice = b"GET /p HTTP/1.1\r\nHost: a\r\nX-Forwarded-For:\r\nX-Forwarded-For:\r\n\r\n";
    assert_eq!(
        check_forwardable(&request(empty_twice)),
        Err(ForwardError::DuplicateForwardedFor)
    );
}

#[test]
fn unchecked_duplicate_forwarded_for_values_are_joined() {
    // Outside the contract (check_forwardable first), but still defined:
    // the client values are joined in order, then the peer follows.
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 1.1.1.1\r\n\
        X-Forwarded-For: 2.2.2.2\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(3, 3, 3, 3)),
        format!(
            "GET /p HTTP/1.1\r\nhost: a\r\n\
             x-forwarded-for: 1.1.1.1, 2.2.2.2, 3.3.3.3\r\n{TAIL}"
        )
    );
}

#[test]
fn empty_client_forwarded_for_counts_as_absent() {
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: \r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(1, 2, 3, 4)),
        format!("GET /p HTTP/1.1\r\nhost: a\r\nx-forwarded-for: 1.2.3.4\r\n{TAIL}")
    );
}

#[test]
fn forwarded_for_listed_in_connection_is_dropped_before_the_merge() {
    let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nConnection: X-Forwarded-For\r\n\
        X-Forwarded-For: 10.0.0.1\r\n\r\n";
    assert_eq!(
        rendered_request(raw, "/p", v4(192, 0, 2, 1)),
        format!("GET /p HTTP/1.1\r\nhost: a\r\nx-forwarded-for: 192.0.2.1\r\n{TAIL}")
    );
}

#[test]
fn hop_by_hop_bytes_fails_closed_on_non_utf8_tokens() {
    // Unreachable while the parsers reject obs-text, but a non-UTF-8 token
    // must still match, never be skipped.
    assert!(is_hop_by_hop_bytes(b"x-\xff", &[b"X-\xFF".as_slice()]));
    assert!(is_hop_by_hop_bytes(b"x-\xff", &[b"a", b"x-\xff"]));
    assert!(!is_hop_by_hop_bytes(b"x-\xfe", &[b"x-\xff".as_slice()]));
}

#[test]
fn is_hop_by_hop_covers_the_fixed_list_proxy_prefix_and_connection_tokens() {
    for name in [
        "connection",
        "Connection",
        "keep-alive",
        "te",
        "TE",
        "trailer",
        "upgrade",
        "expect",
        "Expect",
        "proxy-authorization",
        "Proxy-Connection",
        "proxy-",
    ] {
        assert!(is_hop_by_hop(name, &[]), "{name}");
    }
    for name in ["proxy", "proxyx", "x-te", "host", "content-length", "date"] {
        assert!(!is_hop_by_hop(name, &[]), "{name}");
    }
    assert!(is_hop_by_hop("x-secret", &["keep-alive", "X-Secret"]));
    assert!(is_hop_by_hop("X-SECRET", &["x-secret"]));
    assert!(!is_hop_by_hop("x-secret", &["x-secre", "x-secrets"]));
}

#[test]
fn response_head_is_rewritten() {
    let raw = b"HTTP/1.0 404 Nope\r\nConnection: keep-alive, X-Gone\r\nX-Gone: 1\r\n\
        Keep-Alive: 1\r\nProxy-Authenticate: x\r\nTE: t\r\nTrailer: x\r\nUpgrade: u\r\n\
        Expect: e\r\nContent-Length: 2\r\nX-A: b\r\n\r\n";
    assert_eq!(
        rendered_response(raw, Persistence::KeepAlive),
        "HTTP/1.1 404 Nope\r\ncontent-length: 2\r\nx-a: b\r\n\r\n"
    );
    assert_eq!(
        rendered_response(raw, Persistence::Close),
        "HTTP/1.1 404 Nope\r\ncontent-length: 2\r\nx-a: b\r\nconnection: close\r\n\r\n"
    );
}

#[test]
fn response_passes_content_length_date_and_server_through() {
    let raw = b"HTTP/1.1 200 OK\r\nDate: Mon, 26 Oct 2026 10:00:00 GMT\r\nServer: b/1\r\n\
        Content-Length: 007\r\nX-Forwarded-Proto: https\r\n\r\n";
    let head = response(raw);
    assert_eq!(head.body, ResponseBody::Length(7));
    assert_eq!(
        render_response(&head, Persistence::KeepAlive),
        "HTTP/1.1 200 OK\r\ndate: Mon, 26 Oct 2026 10:00:00 GMT\r\nserver: b/1\r\n\
         content-length: 007\r\nx-forwarded-proto: https\r\n\r\n"
    );
}

#[test]
fn response_reason_is_kept_or_empty() {
    assert_eq!(
        rendered_response(b"HTTP/1.1 200 \r\n\r\n", Persistence::KeepAlive),
        "HTTP/1.1 200 \r\n\r\n"
    );
    assert_eq!(
        rendered_response(b"HTTP/1.1 599 Any\tThing !\r\n\r\n", Persistence::KeepAlive),
        "HTTP/1.1 599 Any\tThing !\r\n\r\n"
    );
}

fn head_with_reason(status: u16, reason: &str) -> ResponseHead {
    ResponseHead {
        status,
        reason: reason.to_string(),
        headers: Vec::new(),
        body: ResponseBody::None,
    }
}

#[test]
fn bad_reason_is_replaced_by_the_standard_reason() {
    let cases = [
        (404, "a\r\nX-Injected: 1", "HTTP/1.1 404 Not Found\r\n\r\n"),
        (200, "caf\u{e9}", "HTTP/1.1 200 OK\r\n\r\n"),
        (502, "\0", "HTTP/1.1 502 Bad Gateway\r\n\r\n"),
        (103, "\x7f", "HTTP/1.1 103 Early Hints\r\n\r\n"),
        (308, "\n", "HTTP/1.1 308 Permanent Redirect\r\n\r\n"),
        (
            431,
            "\r",
            "HTTP/1.1 431 Request Header Fields Too Large\r\n\r\n",
        ),
        // No standard reason: an empty one.
        (599, "\r\n", "HTTP/1.1 599 \r\n\r\n"),
        (299, "\x01", "HTTP/1.1 299 \r\n\r\n"),
    ];
    for (status, reason, want) in cases {
        assert_eq!(
            render_response(&head_with_reason(status, reason), Persistence::KeepAlive),
            want,
            "{status} {reason:?}"
        );
    }
}

#[test]
fn standard_reasons_cover_the_registered_codes() {
    let cases = [
        (100, "Continue"),
        (101, "Switching Protocols"),
        (200, "OK"),
        (201, "Created"),
        (204, "No Content"),
        (206, "Partial Content"),
        (301, "Moved Permanently"),
        (304, "Not Modified"),
        (307, "Temporary Redirect"),
        (400, "Bad Request"),
        (413, "Content Too Large"),
        (417, "Expectation Failed"),
        (421, "Misdirected Request"),
        (422, "Unprocessable Content"),
        (426, "Upgrade Required"),
        (428, "Precondition Required"),
        (429, "Too Many Requests"),
        (500, "Internal Server Error"),
        (503, "Service Unavailable"),
        (504, "Gateway Timeout"),
        (505, "HTTP Version Not Supported"),
        (511, "Network Authentication Required"),
        (102, ""),
        (306, ""),
        (418, ""),
        (999, ""),
    ];
    for (code, reason) in cases {
        assert_eq!(standard_reason(code), reason, "{code}");
    }
}

#[test]
fn status_codes_are_rendered_as_decimal() {
    for status in [100, 200, 404, 599] {
        let out = render_response(&head_with_reason(status, "R"), Persistence::KeepAlive);
        assert_eq!(out, format!("HTTP/1.1 {status} R\r\n\r\n"));
    }
}

// --- Property: both renderers against an independent model -------------

const NAMES: &[&str] = &[
    "Connection",
    "Keep-Alive",
    "TE",
    "Trailer",
    "Upgrade",
    "Proxy-Authorization",
    "Proxy-Connection",
    "Expect",
    "X-Forwarded-For",
    "X-Forwarded-Proto",
    "X-A",
    "X-Drop",
    "User-Agent",
    "Date",
    "Proxy",
];
const VALUES: &[&str] = &[
    "",
    "a",
    "keep-alive",
    "close, x-drop",
    "X-Drop ,x-a",
    "a.test",
    "1.2.3.4",
    "https",
    "Upgrade, ,TE",
    "x-forwarded-for",
];
const SEPS: &[&str] = &[":", ": ", ":  ", ":\t"];

fn peers() -> impl Strategy<Value = IpAddr> {
    prop_oneof![
        any::<[u8; 4]>().prop_map(|o| IpAddr::V4(Ipv4Addr::from(o))),
        any::<[u8; 16]>().prop_map(|o| IpAddr::V6(Ipv6Addr::from(o))),
        any::<[u8; 4]>().prop_map(|o| {
            let [a, b, c, d] = o;
            IpAddr::V6(Ipv4Addr::new(a, b, c, d).to_ipv6_mapped())
        }),
    ]
}

fn header_lines() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        (
            0..NAMES.len(),
            0..SEPS.len(),
            0..VALUES.len(),
            proptest::bool::ANY,
        ),
        0..8,
    )
    .prop_map(|lines| {
        let mut raw = Vec::new();
        for (n, s, v, upper) in lines {
            let name = NAMES[n];
            let name = if upper {
                name.to_ascii_uppercase()
            } else {
                name.to_string()
            };
            raw.extend_from_slice(name.as_bytes());
            raw.extend_from_slice(SEPS[s].as_bytes());
            raw.extend_from_slice(VALUES[v].as_bytes());
            raw.extend_from_slice(b"\r\n");
        }
        raw
    })
}

/// Hop-by-hop by an independent model: the FR-PROXY-06 list, `proxy-*`
/// and every name a `Connection` header lists.
fn model_hop_by_hop(name: &str, headers: &[Header]) -> bool {
    let listed = headers
        .iter()
        .filter(|h| h.name == "connection")
        .flat_map(|h| {
            String::from_utf8(h.value.clone())
                .unwrap()
                .split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .any(|t| t == name);
    [
        "connection",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
        "expect",
    ]
    .contains(&name)
        || name.starts_with("proxy-")
        || listed
}

fn model_request(head: &RequestHead, path: &str, peer: IpAddr) -> String {
    let method = match &head.method {
        Method::Get => "GET",
        Method::Head => "HEAD",
        Method::Other(m) => m.as_str(),
    };
    let mut out = format!("{method} {path}");
    if let Some(q) = &head.target.query {
        out += &format!("?{q}");
    }
    out += match head.version {
        Version::Http10 => " HTTP/1.0\r\n",
        Version::Http11 => " HTTP/1.1\r\n",
    };
    let mut forwarded: Vec<String> = Vec::new();
    for h in &head.headers {
        let value = String::from_utf8(h.value.clone()).unwrap();
        if model_hop_by_hop(&h.name, &head.headers) || h.name == "x-forwarded-proto" {
            continue;
        }
        if h.name == "x-forwarded-for" {
            if !value.is_empty() {
                forwarded.push(value);
            }
        } else if h.name == "content-length" {
            out += &format!("content-length: {}\r\n", value.parse::<u64>().unwrap());
        } else {
            out += &format!("{}: {value}\r\n", h.name);
        }
    }
    forwarded.push(peer.to_canonical().to_string());
    out += &format!("x-forwarded-for: {}\r\n{TAIL}", forwarded.join(", "));
    out
}

fn model_response(head: &ResponseHead, persistence: Persistence) -> String {
    let mut out = format!("HTTP/1.1 {} {}\r\n", head.status, head.reason);
    for h in &head.headers {
        if !model_hop_by_hop(&h.name, &head.headers) {
            out += &format!(
                "{}: {}\r\n",
                h.name,
                String::from_utf8(h.value.clone()).unwrap()
            );
        }
    }
    if persistence == Persistence::Close {
        out += "connection: close\r\n";
    }
    out + "\r\n"
}

proptest! {
    #[test]
    fn renderers_match_the_model(
        method in prop::sample::select(vec!["GET", "HEAD", "POST", "PURGE"]),
        path in prop::sample::select(vec!["/", "/p", "/a/b/", "/x%20y"]),
        query in proptest::option::of("[a-z=&%0-9]{0,6}"),
        version in prop::sample::select(vec!["HTTP/1.0", "HTTP/1.1"]),
        headers in header_lines(),
        length in proptest::option::of("0{0,3}[0-9]{1,4}"),
        peer in peers(),
        status in 200u16..600,
        reason in "[ -~\t]{0,8}",
    ) {
        let mut raw = format!("{method} /p");
        if let Some(q) = &query {
            raw.push('?');
            raw.push_str(q);
        }
        raw.push_str(&format!(" {version}\r\nHost: h\r\n"));
        if let Some(n) = &length {
            raw.push_str(&format!("Content-Length: {n}\r\n"));
        }
        let mut raw = raw.into_bytes();
        raw.extend_from_slice(&headers);
        raw.extend_from_slice(b"\r\n");
        if let Ok(ParseStatus::Complete { head, .. }) = parse_head(&raw, &limits()) {
            let mut out = Vec::new();
            render_backend_request(&head, path, peer, &mut out);
            prop_assert_eq!(backend_request_len(&head, path, peer), out.len());
            prop_assert_eq!(String::from_utf8(out).unwrap(), model_request(&head, path, peer));
        }

        let mut raw = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
        raw.extend_from_slice(&headers);
        if let Some(n) = &length {
            raw.extend_from_slice(format!("Content-Length: {n}\r\n").as_bytes());
        }
        raw.extend_from_slice(b"\r\n");
        let parsed = ResponseParser::new().advance(&raw, &limits(), RequestKind::Other);
        if let Ok(ResponseStatus::Complete { head, .. }) = parsed {
            for persistence in [Persistence::KeepAlive, Persistence::Close] {
                let mut out = Vec::new();
                render_client_response(&head, persistence, &mut out);
                prop_assert_eq!(client_response_len(&head, persistence), out.len());
                prop_assert_eq!(
                    String::from_utf8(out).unwrap(),
                    model_response(&head, persistence)
                );
            }
        }
    }

    #[test]
    fn any_reason_renders_one_safe_status_line(
        status in 100u16..600,
        reason in proptest::collection::vec(any::<u8>(), 0..12),
    ) {
        let reason = String::from_utf8_lossy(&reason).into_owned();
        let head = head_with_reason(status, &reason);
        let mut out = Vec::new();
        render_client_response(&head, Persistence::KeepAlive, &mut out);
        prop_assert_eq!(client_response_len(&head, Persistence::KeepAlive), out.len());
        let text = String::from_utf8(out).unwrap();
        let line = text.strip_suffix("\r\n\r\n").unwrap();
        prop_assert!(!line.contains('\r') && !line.contains('\n'));
        prop_assert!(line.bytes().all(|b| b == b'\t' || (0x20..=0x7E).contains(&b)));
        let prefix = format!("HTTP/1.1 {status} ");
        prop_assert!(line.starts_with(&prefix));
        let shown = &line[prefix.len()..];
        prop_assert!(shown == reason || shown == standard_reason(status));
    }
}
