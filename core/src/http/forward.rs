//! Renders the heads a reverse proxy forwards: the request head sent to the
//! backend and the response head sent to the client.
//!
//! Both renderers work from already validated, parsed heads, so they only
//! rewrite: header names are written lowercased as `name: value`, hop-by-hop
//! headers are dropped, and the proxy's own lines are appended. The parsers
//! reject a `Connection` that lists `content-length`, `transfer-encoding` or
//! `host` (see `connection_names_framing` in `request.rs`), so the renderers
//! need no guard for that.

use crate::http::request::{Header, Method, Persistence, RequestHead, Version};
use crate::http::response_head::ResponseHead;
use std::net::IpAddr;

/// Extra bytes either renderer may add beyond its input head's size, on top
/// of one byte per header (a header sent as `a:b` is rendered `a: b`).
pub const FORWARD_SLACK: usize = 128;

/// Appends the head sent to the backend. `path` is the raw path as
/// forwarded (already stripped under `strip_prefix`).
///
/// The request line keeps the client's method, query and version. The
/// client's headers follow in order, lowercased, without the hop-by-hop
/// ones, `expect` and `x-forwarded-proto`; the client's own `x-forwarded-for`
/// lines stay. Then come `x-forwarded-for` with the canonical peer address,
/// `x-forwarded-proto: http` and `connection: close`.
pub fn render_request_head(head: &RequestHead, path: &str, peer: IpAddr, out: &mut Vec<u8>) {
    out.extend_from_slice(match &head.method {
        Method::Get => b"GET",
        Method::Head => b"HEAD",
        Method::Other(name) => name.as_bytes(),
    });
    out.push(b' ');
    out.extend_from_slice(path.as_bytes());
    if let Some(query) = &head.target.query {
        out.push(b'?');
        out.extend_from_slice(query.as_bytes());
    }
    out.extend_from_slice(match head.version {
        Version::Http10 => b" HTTP/1.0\r\n",
        Version::Http11 => b" HTTP/1.1\r\n",
    });
    push_forwarded(&head.headers, &["expect", "x-forwarded-proto"], out);
    out.extend_from_slice(b"x-forwarded-for: ");
    out.extend_from_slice(peer.to_canonical().to_string().as_bytes());
    out.extend_from_slice(b"\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n");
}

/// Appends the head sent to the client: the backend's status line as
/// `HTTP/1.1`, its headers without the hop-by-hop ones, and
/// `connection: close` if `persistence` is `Close`.
pub fn render_response_head(head: &ResponseHead, persistence: Persistence, out: &mut Vec<u8>) {
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(&status_digits(head.status));
    out.push(b' ');
    out.extend_from_slice(&head.reason);
    out.extend_from_slice(b"\r\n");
    push_forwarded(&head.headers, &[], out);
    match persistence {
        Persistence::KeepAlive => {}
        Persistence::Close => out.extend_from_slice(b"connection: close\r\n"),
    }
    out.extend_from_slice(b"\r\n");
}

/// The three decimal digits of a status the parser guarantees to be
/// `100..=599`.
fn status_digits(status: u16) -> [u8; 3] {
    let digit = |place: u16| b'0'.saturating_add(u8::try_from(status / place % 10).unwrap_or(0));
    [digit(100), digit(10), digit(1)]
}

/// Appends `headers` as `name: value` lines, leaving out the hop-by-hop
/// ones and those named in `also_drop`.
fn push_forwarded(headers: &[Header], also_drop: &[&str], out: &mut Vec<u8>) {
    // Neither vector allocates when there is no `connection` header.
    let listed = listed_tokens(headers);
    let listed: Vec<&[u8]> = listed.iter().map(Vec::as_slice).collect();
    for header in headers {
        if !is_hop_by_hop(&header.name, &listed) && !also_drop.contains(&header.name.as_str()) {
            push_header(&header.name, &header.value, out);
        }
    }
}

fn push_header(name: &str, value: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

/// The tokens of every `connection` header, trimmed and lowercased.
fn listed_tokens(headers: &[Header]) -> Vec<Vec<u8>> {
    headers
        .iter()
        .filter(|header| header.name == "connection")
        .flat_map(|header| header.value.split(|&b| b == b','))
        .map(|token| {
            token
                .trim_ascii()
                .iter()
                .map(u8::to_ascii_lowercase)
                .collect()
        })
        .filter(|token: &Vec<u8>| !token.is_empty())
        .collect()
}

/// Whether a (lowercased) header is hop-by-hop: one of the fixed set, any
/// `proxy-*`, or named by a `connection` token in `listed`.
fn is_hop_by_hop(name: &str, listed: &[&[u8]]) -> bool {
    matches!(
        name,
        "connection" | "keep-alive" | "te" | "trailer" | "upgrade"
    ) || name.starts_with("proxy-")
        || listed.contains(&name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::Limits;
    use crate::http::request::{HeadLimits, ParseStatus, parse_head};
    use crate::http::response::BodyMode;
    use crate::http::response_head::{ResponseParser, ResponseStatus};
    use proptest::prelude::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
        match ResponseParser::new().advance(raw, &limits(), BodyMode::Full) {
            Ok(ResponseStatus::Complete { head, .. }) => head,
            other => panic!("expected complete head, got {other:?}"),
        }
    }

    fn rendered_request(raw: &[u8], path: &str, peer: IpAddr) -> Vec<u8> {
        let mut out = Vec::new();
        render_request_head(&request(raw), path, peer, &mut out);
        out
    }

    fn rendered_response(raw: &[u8], persistence: Persistence) -> Vec<u8> {
        let mut out = Vec::new();
        render_response_head(&response(raw), persistence, &mut out);
        out
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn request_head_is_rewritten() {
        let raw = b"POST /api/x?q=1 HTTP/1.1\r\n\
            Host: a.test\r\n\
            Connection: keep-alive, X-Drop\r\n\
            X-Drop: 1\r\n\
            Keep-Alive: 5\r\n\
            TE: trailers\r\n\
            Trailer: X\r\n\
            Upgrade: h2c\r\n\
            Proxy-Authorization: x\r\n\
            Expect: 100-continue\r\n\
            X-Forwarded-For: 10.0.0.1\r\n\
            X-Forwarded-Proto: https\r\n\
            Content-Length: 3\r\n\
            User-Agent: t\r\n\r\n";
        let peer = IpAddr::V6("::ffff:192.0.2.7".parse::<Ipv6Addr>().unwrap());
        assert_eq!(
            String::from_utf8(rendered_request(raw, "/x", peer)).unwrap(),
            "POST /x?q=1 HTTP/1.1\r\n\
             host: a.test\r\n\
             x-forwarded-for: 10.0.0.1\r\n\
             content-length: 3\r\n\
             user-agent: t\r\n\
             x-forwarded-for: 192.0.2.7\r\n\
             x-forwarded-proto: http\r\n\
             connection: close\r\n\
             \r\n"
        );
    }

    #[test]
    fn ipv6_peer_is_rendered_without_brackets() {
        let peer = IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap());
        let out = rendered_request(b"GET /p HTTP/1.1\r\nHost: a\r\n\r\n", "/p", peer);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "GET /p HTTP/1.1\r\nhost: a\r\nx-forwarded-for: 2001:db8::1\r\n\
             x-forwarded-proto: http\r\nconnection: close\r\n\r\n"
        );
    }

    #[test]
    fn request_line_keeps_method_version_and_query() {
        let out = rendered_request(b"GET /p HTTP/1.0\r\n\r\n", "/p", v4(1, 2, 3, 4));
        assert!(out.starts_with(b"GET /p HTTP/1.0\r\n"));
        let out = rendered_request(
            b"PURGE /p? HTTP/1.1\r\nHost: a\r\n\r\n",
            "/p",
            v4(1, 2, 3, 4),
        );
        assert!(out.starts_with(b"PURGE /p? HTTP/1.1\r\n"));
        let out = rendered_request(
            b"HEAD /p?a=b HTTP/1.1\r\nHost: a\r\n\r\n",
            "/q",
            v4(1, 2, 3, 4),
        );
        assert!(out.starts_with(b"HEAD /q?a=b HTTP/1.1\r\n"));
    }

    #[test]
    fn transfer_encoding_is_forwarded() {
        let out = rendered_request(
            b"POST /p HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n",
            "/p",
            v4(1, 2, 3, 4),
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\r\ntransfer-encoding: chunked\r\n"));
    }

    #[test]
    fn response_head_is_rewritten() {
        let raw = b"HTTP/1.0 404 Nope\r\nConnection: keep-alive\r\nKeep-Alive: 1\r\n\
            Proxy-Authenticate: x\r\nContent-Length: 2\r\nX-A: b\r\n\r\n";
        assert_eq!(
            rendered_response(raw, Persistence::KeepAlive),
            b"HTTP/1.1 404 Nope\r\ncontent-length: 2\r\nx-a: b\r\n\r\n"
        );
        assert_eq!(
            rendered_response(raw, Persistence::Close),
            b"HTTP/1.1 404 Nope\r\ncontent-length: 2\r\nx-a: b\r\nconnection: close\r\n\r\n"
        );
    }

    #[test]
    fn response_drops_headers_listed_in_connection() {
        let raw = b"HTTP/1.1 200 OK\r\nConnection: X-Gone ,Close\r\nX-Gone: 1\r\nX-Kept: 2\r\n\r\n";
        assert_eq!(
            rendered_response(raw, Persistence::KeepAlive),
            b"HTTP/1.1 200 OK\r\nx-kept: 2\r\n\r\n"
        );
    }

    #[test]
    fn several_connection_headers_all_list_names() {
        let raw = b"GET /p HTTP/1.1\r\nHost: a\r\nConnection: x-a\r\nX-A: 1\r\n\
            Connection: x-b\r\nX-B: 2\r\nX-C: 3\r\n\r\n";
        let out = String::from_utf8(rendered_request(raw, "/p", v4(1, 2, 3, 4))).unwrap();
        assert!(out.starts_with("GET /p HTTP/1.1\r\nhost: a\r\nx-c: 3\r\nx-forwarded-for: "));
    }

    #[test]
    fn empty_and_padded_connection_tokens_are_ignored() {
        let raw = b"HTTP/1.1 200 OK\r\nConnection: ,, X-A ,\r\nX-A: 1\r\nX-B: 2\r\n\r\n";
        assert_eq!(
            rendered_response(raw, Persistence::KeepAlive),
            b"HTTP/1.1 200 OK\r\nx-b: 2\r\n\r\n"
        );
    }

    #[test]
    fn response_drops_te_trailer_and_upgrade() {
        let raw = b"HTTP/1.1 200 OK\r\nTE: trailers\r\nTrailer: X\r\nUpgrade: h2c\r\n\
            Proxy-Connection: x\r\nX-A: 1\r\n\r\n";
        assert_eq!(
            rendered_response(raw, Persistence::KeepAlive),
            b"HTTP/1.1 200 OK\r\nx-a: 1\r\n\r\n"
        );
    }

    #[test]
    fn response_keeps_expect_and_x_forwarded_proto() {
        let raw = b"HTTP/1.1 200 OK\r\nExpect: x\r\nX-Forwarded-Proto: https\r\n\r\n";
        assert_eq!(
            rendered_response(raw, Persistence::KeepAlive),
            b"HTTP/1.1 200 OK\r\nexpect: x\r\nx-forwarded-proto: https\r\n\r\n"
        );
    }

    #[test]
    fn only_names_starting_with_proxy_dash_are_dropped() {
        let raw = b"HTTP/1.1 200 OK\r\nProxy: 1\r\nProxyx: 2\r\nProxy-X: 3\r\n\r\n";
        assert_eq!(
            rendered_response(raw, Persistence::KeepAlive),
            b"HTTP/1.1 200 OK\r\nproxy: 1\r\nproxyx: 2\r\n\r\n"
        );
        let out = rendered_request(
            b"GET /p HTTP/1.1\r\nHost: a\r\nProxy: 1\r\nProxyx: 2\r\nProxy-X: 3\r\n\r\n",
            "/p",
            v4(1, 2, 3, 4),
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\r\nproxy: 1\r\nproxyx: 2\r\nx-forwarded-for"));
    }

    #[test]
    fn status_digits_are_zero_padded_by_value() {
        assert_eq!(status_digits(100), *b"100");
        assert_eq!(status_digits(404), *b"404");
        assert_eq!(status_digits(599), *b"599");
    }

    #[test]
    fn empty_reason_is_kept() {
        assert_eq!(
            rendered_response(
                b"HTTP/1.1 200 \r\nContent-Length: 0\r\n\r\n",
                Persistence::KeepAlive
            ),
            b"HTTP/1.1 200 \r\ncontent-length: 0\r\n\r\n"
        );
    }

    const NAMES: &[&str] = &[
        "Connection",
        "Keep-Alive",
        "TE",
        "Trailer",
        "Upgrade",
        "Proxy-Authorization",
        "Expect",
        "X-Forwarded-For",
        "X-Forwarded-Proto",
        "X-A",
        "X-Drop",
        "User-Agent",
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
                let name = NAMES.get(n).copied().unwrap_or_default();
                let name = if upper {
                    name.to_ascii_uppercase()
                } else {
                    name.to_string()
                };
                raw.extend_from_slice(name.as_bytes());
                raw.extend_from_slice(SEPS.get(s).copied().unwrap_or_default().as_bytes());
                raw.extend_from_slice(VALUES.get(v).copied().unwrap_or_default().as_bytes());
                raw.extend_from_slice(b"\r\n");
            }
            raw
        })
    }

    const FIXED_HOP: &[&str] = &["connection", "keep-alive", "te", "trailer", "upgrade"];

    /// The header lines of a rendered head, split at `: `.
    fn rendered_headers(out: &[u8], skip: usize, trailing: usize) -> Vec<(String, Vec<u8>)> {
        let text: Vec<&[u8]> = out.split(|&b| b == b'\n').collect();
        // The final element is empty (after the last LF), and before it
        // comes the blank line.
        let lines = &text[skip..text.len() - 2 - trailing];
        lines
            .iter()
            .map(|line| {
                let line = line.strip_suffix(b"\r").unwrap();
                let at = line.windows(2).position(|w| w == b": ").unwrap();
                (
                    String::from_utf8(line[..at].to_vec()).unwrap(),
                    line[at + 2..].to_vec(),
                )
            })
            .collect()
    }

    /// The headers a renderer should keep, by an independent model.
    fn expected_headers(headers: &[Header], extra: &[&str]) -> Vec<(String, Vec<u8>)> {
        let listed: Vec<String> = headers
            .iter()
            .filter(|h| h.name == "connection")
            .flat_map(|h| {
                String::from_utf8(h.value.clone())
                    .unwrap()
                    .split(',')
                    .map(|t| t.trim().to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .collect();
        headers
            .iter()
            .filter(|h| {
                !FIXED_HOP.contains(&h.name.as_str())
                    && !h.name.starts_with("proxy-")
                    && !extra.contains(&h.name.as_str())
                    && !listed.contains(&h.name)
            })
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect()
    }

    proptest! {
        #[test]
        fn slack_bounds_the_growth(
            method in prop::sample::select(vec!["GET", "HEAD", "POST", "PURGE"]),
            query in proptest::option::of("[a-z=&]{0,6}"),
            version in prop::sample::select(vec!["HTTP/1.0", "HTTP/1.1"]),
            headers in header_lines(),
            peer in peers(),
            status in 200u16..600,
            reason in "[A-Za-z ]{0,8}",
        ) {
            let mut raw = format!("{method} /p");
            if let Some(q) = &query {
                raw.push('?');
                raw.push_str(q);
            }
            raw.push_str(&format!(" {version}\r\nHost: h\r\n"));
            let mut raw = raw.into_bytes();
            raw.extend_from_slice(&headers);
            raw.extend_from_slice(b"\r\n");
            let parsed = parse_head(&raw, &limits());
            prop_assume!(matches!(parsed, Ok(ParseStatus::Complete { .. })));
            let Ok(ParseStatus::Complete { head, .. }) = parsed else { unreachable!() };
            let mut out = Vec::new();
            render_request_head(&head, "/p", peer, &mut out);
            prop_assert!(out.len() <= raw.len() + head.headers.len() + FORWARD_SLACK);
            let rendered = rendered_headers(&out, 1, 3);
            prop_assert_eq!(
                rendered,
                expected_headers(&head.headers, &["expect", "x-forwarded-proto"])
            );
            let tail = format!(
                "x-forwarded-for: {}\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n",
                peer.to_canonical()
            );
            prop_assert!(out.ends_with(tail.as_bytes()));

            let mut raw = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
            raw.extend_from_slice(&headers);
            raw.extend_from_slice(b"\r\n");
            let parsed = ResponseParser::new().advance(&raw, &limits(), BodyMode::Full);
            prop_assume!(matches!(parsed, Ok(ResponseStatus::Complete { .. })));
            let Ok(ResponseStatus::Complete { head, .. }) = parsed else { unreachable!() };
            for (persistence, trailing) in [(Persistence::KeepAlive, 0), (Persistence::Close, 1)] {
                let mut out = Vec::new();
                render_response_head(&head, persistence, &mut out);
                prop_assert!(out.len() <= raw.len() + head.headers.len() + FORWARD_SLACK);
                prop_assert_eq!(
                    rendered_headers(&out, 1, trailing),
                    expected_headers(&head.headers, &[])
                );
                prop_assert_eq!(out.ends_with(b"connection: close\r\n\r\n"), trailing == 1);
            }
        }
    }
}
