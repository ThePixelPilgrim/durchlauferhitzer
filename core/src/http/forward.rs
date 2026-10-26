//! Renders the heads a reverse proxy forwards: the request head sent to the
//! backend (FR-PROXY-04…11) and the response head sent to the client
//! (FR-PROXY-12).
//!
//! Both renderers work from heads the parsers already validated, so they
//! only rewrite. Header names are written lowercased as `name: value`, in
//! the order received; the hop-by-hop headers ([`is_hop_by_hop`], one list
//! for both directions) are dropped and the proxy's own lines are appended.
//! The parsers reject a `Connection` that lists `content-length`,
//! `transfer-encoding` or `host` (FR-HTTP-09a) and every header value with
//! obs-text (FR-HTTP-05, FR-PROXY-13), so the renderers need no guard for
//! those.
//!
//! Each renderer has a `*_len` twin that returns exactly the number of
//! bytes it would append, so a caller can check that a head fits its buffer
//! before rendering. Both are built on the same code path (a `Sink` that
//! either counts or appends), so they cannot disagree.

use crate::http::head_line::connection_tokens;
use crate::http::request::{Header, Method, Persistence, RequestHead};
use crate::http::response_head::ResponseHead;
use std::fmt::{self, Write as _};
use std::net::IpAddr;

/// Why a request may not be forwarded to a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardError {
    /// More than one `X-Forwarded-For` header line (FR-PROXY-08) → 400.
    DuplicateForwardedFor,
}

/// The checks a request must pass before it is forwarded, beyond those of
/// the request parser: at most one `X-Forwarded-For` header line
/// (FR-PROXY-08). Only proxy routes call it, before connecting to the
/// backend; static routes are unaffected. Its errors map to 400
/// ([`ForwardError::status`]).
pub fn check_forwardable(head: &RequestHead) -> Result<(), ForwardError> {
    let lines = head
        .headers
        .iter()
        .filter(|h| h.name == "x-forwarded-for")
        .count();
    if lines > 1 {
        Err(ForwardError::DuplicateForwardedFor)
    } else {
        Ok(())
    }
}

/// Whether a header is hop-by-hop (FR-PROXY-06, FR-PROXY-12): `connection`,
/// `keep-alive`, any `proxy-*`, `te`, `trailer`, `upgrade`, `expect`, or a
/// name listed in `connection_tokens` (the tokens of the head's
/// `Connection` headers). All comparisons are ASCII case-insensitive.
pub fn is_hop_by_hop(name: &str, connection_tokens: &[&str]) -> bool {
    const FIXED: [&str; 6] = [
        "connection",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
        "expect",
    ];
    const PROXY: &str = "proxy-";
    FIXED.iter().any(|fixed| fixed.eq_ignore_ascii_case(name))
        || name
            .get(..PROXY.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PROXY))
        || connection_tokens
            .iter()
            .any(|token| token.eq_ignore_ascii_case(name))
}

/// Appends the head sent to the backend (FR-PROXY-04…11). `path` is the
/// raw path as forwarded (after `strip_prefix`, see
/// [`strip_segments`](crate::routing::strip_segments)); `peer` is the
/// client's address.
///
/// The request line is `METHOD <path>[?query] HTTP/1.1` with the query
/// unchanged. The client's headers follow in order without the hop-by-hop
/// ones, `x-forwarded-for` and `x-forwarded-proto`; `content-length` is
/// re-rendered without leading zeros, everything else (`host`,
/// `transfer-encoding`, …) as received. Then come `x-forwarded-for` (the
/// client's non-empty value, `, `, the peer in canonical form),
/// `x-forwarded-proto: http` and `connection: close`.
///
/// Callers run [`check_forwardable`] first. Should a head still carry
/// several `x-forwarded-for` lines, their non-empty values are joined in
/// order with `, `.
pub fn render_backend_request(head: &RequestHead, path: &str, peer: IpAddr, out: &mut Vec<u8>) {
    backend_request(head, path, peer, out);
}

/// The exact number of bytes [`render_backend_request`] appends.
pub fn backend_request_len(head: &RequestHead, path: &str, peer: IpAddr) -> usize {
    let mut count = Count(0);
    backend_request(head, path, peer, &mut count);
    count.0
}

/// Appends the head sent to the client (FR-PROXY-12): the status line as
/// `HTTP/1.1 <code> <reason>`, the backend's headers in order without the
/// hop-by-hop ones (`content-length`, `date`, `server` and all others pass
/// through as received), and `connection: close` when `persistence` is
/// `Close`.
///
/// The backend's reason is kept, also when empty. The parser admits only
/// printable ASCII and HTAB; a reason with any other byte is replaced by
/// the standard reason for the code, or by an empty one for a code that has
/// none (defence in depth).
pub fn render_client_response(head: &ResponseHead, persistence: Persistence, out: &mut Vec<u8>) {
    client_response(head, persistence, out);
}

/// The exact number of bytes [`render_client_response`] appends.
pub fn client_response_len(head: &ResponseHead, persistence: Persistence) -> usize {
    let mut count = Count(0);
    client_response(head, persistence, &mut count);
    count.0
}

/// Where a head is rendered: appended to a buffer, or only counted.
trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// Counts the bytes a head would take.
struct Count(usize);

impl Sink for Count {
    fn put(&mut self, bytes: &[u8]) {
        self.0 = self.0.saturating_add(bytes.len());
    }
}

/// Lets `write!` render into a [`Sink`] without an intermediate `String`.
struct Formatter<'a, S>(&'a mut S);

impl<S: Sink> fmt::Write for Formatter<'_, S> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.put(s.as_bytes());
        Ok(())
    }
}

/// Writes `value` in its `Display` form. Neither sink ever fails, so the
/// result carries no information.
fn put_display(sink: &mut impl Sink, value: impl fmt::Display) {
    let _infallible = write!(Formatter(sink), "{value}");
}

fn put_header(sink: &mut impl Sink, name: &str, value: &[u8]) {
    sink.put(name.as_bytes());
    sink.put(b": ");
    sink.put(value);
    sink.put(b"\r\n");
}

/// The tokens of every `Connection` header. Validated values are ASCII
/// (obs-text is rejected), so every token is valid UTF-8.
fn listed_tokens(headers: &[Header]) -> Vec<&str> {
    connection_tokens(headers)
        .filter_map(|token| std::str::from_utf8(token).ok())
        .collect()
}

fn backend_request(head: &RequestHead, path: &str, peer: IpAddr, sink: &mut impl Sink) {
    sink.put(match &head.method {
        Method::Get => b"GET",
        Method::Head => b"HEAD",
        Method::Other(name) => name.as_bytes(),
    });
    sink.put(b" ");
    sink.put(path.as_bytes());
    if let Some(query) = &head.target.query {
        sink.put(b"?");
        sink.put(query.as_bytes());
    }
    sink.put(b" HTTP/1.1\r\n");

    let listed = listed_tokens(&head.headers);
    let forwardable = head
        .headers
        .iter()
        .filter(|h| !is_hop_by_hop(&h.name, &listed) && h.name != "x-forwarded-proto");
    for header in forwardable.clone() {
        match header.name.as_str() {
            "x-forwarded-for" => {}
            "content-length" => {
                put_header(sink, &header.name, without_leading_zeros(&header.value))
            }
            other => put_header(sink, other, &header.value),
        }
    }

    sink.put(b"x-forwarded-for: ");
    let client_values = forwardable
        .filter(|h| h.name == "x-forwarded-for" && !h.value.is_empty())
        .map(|h| h.value.as_slice());
    for value in client_values {
        sink.put(value);
        sink.put(b", ");
    }
    put_display(sink, peer.to_canonical());
    sink.put(b"\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n");
}

/// A validated `Content-Length` value (decimal digits only) without its
/// leading zeros; all zeros become `0`.
fn without_leading_zeros(value: &[u8]) -> &[u8] {
    let first_nonzero = value.iter().position(|&b| b != b'0');
    match first_nonzero {
        Some(i) => value.get(i..).unwrap_or(b"0"),
        None => b"0",
    }
}

fn client_response(head: &ResponseHead, persistence: Persistence, sink: &mut impl Sink) {
    sink.put(b"HTTP/1.1 ");
    put_display(sink, head.status);
    sink.put(b" ");
    let reason_ok = head
        .reason
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7E).contains(&b));
    if reason_ok {
        sink.put(head.reason.as_bytes());
    } else {
        sink.put(standard_reason(head.status).as_bytes());
    }
    sink.put(b"\r\n");

    let listed = listed_tokens(&head.headers);
    for header in &head.headers {
        if !is_hop_by_hop(&header.name, &listed) {
            put_header(sink, &header.name, &header.value);
        }
    }
    match persistence {
        Persistence::KeepAlive => {}
        Persistence::Close => sink.put(b"connection: close\r\n"),
    }
    sink.put(b"\r\n");
}

/// The standard reason phrase for `code`: those of RFC 9110 section 15,
/// plus 103 (RFC 8297) and 428, 429, 431 and 511 (RFC 6585). Empty for any
/// other code (FR-PROXY-12).
fn standard_reason(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        511 => "Network Authentication Required",
        _ => "",
    }
}

#[cfg(test)]
mod tests;
