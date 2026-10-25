//! Response heads: fixed plain-text responses (status line plus short text
//! body), `301` redirects and the head of a `200 OK` file response.

use super::request::Persistence;
use super::status::Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    Full,
    HeadOnly,
}

/// Appends a fixed response for `status` to `out` (existing bytes are kept).
/// `Content-Length` always describes the full body, also for `HeadOnly`.
/// A `405` names the allowed methods (`Allow: GET, HEAD`).
pub fn render_fixed(status: Status, persistence: Persistence, body: BodyMode, out: &mut Vec<u8>) {
    let text = format!("{} {}\n", status.code(), status.reason());
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\n",
        status.code(),
        status.reason(),
        text.len()
    );
    match status {
        Status::MethodNotAllowed => head.push_str("Allow: GET, HEAD\r\n"),
        Status::Ok
        | Status::MovedPermanently
        | Status::BadRequest
        | Status::NotFound
        | Status::RequestTimeout
        | Status::ContentTooLarge
        | Status::UriTooLong
        | Status::RequestHeaderFieldsTooLarge
        | Status::NotImplemented
        | Status::ServiceUnavailable
        | Status::HttpVersionNotSupported => {}
    }
    match persistence {
        Persistence::Close => head.push_str("Connection: close\r\n"),
        Persistence::KeepAlive => {}
    }
    head.push_str("\r\n");
    out.extend_from_slice(head.as_bytes());
    match body {
        BodyMode::Full => out.extend_from_slice(text.as_bytes()),
        BodyMode::HeadOnly => {}
    }
}

/// Body of every redirect.
const REDIRECT_BODY: &str = "301 Moved Permanently\n";

/// The redirect head up to the `Location` value.
const REDIRECT_START: &str = "HTTP/1.1 301 Moved Permanently\r\nLocation: ";

/// The redirect head after the `Location` value, up to the optional
/// `Connection` line (`22` is `REDIRECT_BODY.len()`, checked by a test).
const REDIRECT_REST: &str = "\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\n";

const CONNECTION_CLOSE: &str = "Connection: close\r\n";

/// Appends a `301 Moved Permanently` response pointing at `location` (a
/// validated ASCII path, plus `?query`) to `out` (existing bytes are kept).
/// The body is fixed; `Content-Length` describes it also for `HeadOnly`.
/// It appends exactly [`redirect_len`] bytes.
pub fn render_redirect(
    location: &str,
    persistence: Persistence,
    body: BodyMode,
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(REDIRECT_START.as_bytes());
    out.extend_from_slice(location.as_bytes());
    out.extend_from_slice(REDIRECT_REST.as_bytes());
    match persistence {
        Persistence::Close => out.extend_from_slice(CONNECTION_CLOSE.as_bytes()),
        Persistence::KeepAlive => {}
    }
    out.extend_from_slice(b"\r\n");
    match body {
        BodyMode::Full => out.extend_from_slice(REDIRECT_BODY.as_bytes()),
        BodyMode::HeadOnly => {}
    }
}

/// How many bytes [`render_redirect`] appends for a `Location` of
/// `location_len` bytes, so a caller can check that it fits before
/// rendering it.
pub fn redirect_len(location_len: usize, persistence: Persistence, body: BodyMode) -> usize {
    let connection = match persistence {
        Persistence::Close => CONNECTION_CLOSE.len(),
        Persistence::KeepAlive => 0,
    };
    let body = match body {
        BodyMode::Full => REDIRECT_BODY.len(),
        BodyMode::HeadOnly => 0,
    };
    REDIRECT_START
        .len()
        .saturating_add(location_len)
        .saturating_add(REDIRECT_REST.len())
        .saturating_add(connection)
        .saturating_add(2)
        .saturating_add(body)
}

/// Appends the head of a `200 OK` response whose body is `len` bytes of a
/// file of type `content_type` (existing bytes are kept).
pub fn render_file_head(content_type: &str, len: u64, persistence: Persistence, out: &mut Vec<u8>) {
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nX-Content-Type-Options: nosniff\r\n"
    );
    match persistence {
        Persistence::Close => head.push_str("Connection: close\r\n"),
        Persistence::KeepAlive => {}
    }
    head.push_str("\r\n");
    out.extend_from_slice(head.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_404_keep_alive() {
        let mut out = Vec::new();
        render_fixed(
            Status::NotFound,
            Persistence::KeepAlive,
            BodyMode::Full,
            &mut out,
        );
        assert_eq!(
            out,
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 14\r\nX-Content-Type-Options: nosniff\r\n\r\n404 Not Found\n"
        );
    }

    #[test]
    fn renders_400_close_head_only() {
        let mut out = Vec::new();
        render_fixed(
            Status::BadRequest,
            Persistence::Close,
            BodyMode::HeadOnly,
            &mut out,
        );
        assert_eq!(
            out,
            b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 16\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn rendering_twice_appends_both_responses() {
        let mut out = Vec::new();
        render_fixed(
            Status::NotFound,
            Persistence::KeepAlive,
            BodyMode::Full,
            &mut out,
        );
        let first = out.clone();
        render_fixed(
            Status::BadRequest,
            Persistence::Close,
            BodyMode::HeadOnly,
            &mut out,
        );
        let mut second = Vec::new();
        render_fixed(
            Status::BadRequest,
            Persistence::Close,
            BodyMode::HeadOnly,
            &mut second,
        );
        assert!(!second.is_empty());
        assert_eq!(out.get(..first.len()), Some(first.as_slice()));
        assert_eq!(out.get(first.len()..), Some(second.as_slice()));
    }

    #[test]
    fn renders_file_head_keep_alive() {
        let mut out = Vec::new();
        render_file_head(
            "text/css; charset=utf-8",
            1234,
            Persistence::KeepAlive,
            &mut out,
        );
        assert_eq!(
            out,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/css; charset=utf-8\r\nContent-Length: 1234\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        );
    }

    #[test]
    fn renders_file_head_close() {
        let mut out = b"x".to_vec();
        render_file_head("image/png", 0, Persistence::Close, &mut out);
        assert_eq!(
            out,
            b"xHTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 0\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn renders_405_with_allow() {
        let mut out = Vec::new();
        render_fixed(
            Status::MethodNotAllowed,
            Persistence::KeepAlive,
            BodyMode::Full,
            &mut out,
        );
        assert_eq!(
            out,
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 23\r\nX-Content-Type-Options: nosniff\r\nAllow: GET, HEAD\r\n\r\n405 Method Not Allowed\n"
        );
    }

    #[test]
    fn only_405_carries_allow() {
        for status in [
            Status::NotFound,
            Status::ContentTooLarge,
            Status::NotImplemented,
        ] {
            let mut out = Vec::new();
            render_fixed(status, Persistence::Close, BodyMode::Full, &mut out);
            let text = String::from_utf8(out).unwrap();
            assert!(!text.contains("Allow:"), "{text}");
        }
    }

    #[test]
    fn renders_redirect_keep_alive() {
        let mut out = b"x".to_vec();
        render_redirect(
            "/docs/?x=1",
            Persistence::KeepAlive,
            BodyMode::Full,
            &mut out,
        );
        assert_eq!(
            out,
            b"xHTTP/1.1 301 Moved Permanently\r\nLocation: /docs/?x=1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\n\r\n301 Moved Permanently\n"
        );
    }

    #[test]
    fn redirect_len_is_the_rendered_length() {
        assert!(REDIRECT_REST.contains(&format!("Content-Length: {}\r\n", REDIRECT_BODY.len())));
        for location in ["/", "/a/", "/docs/?x=1", &"/b".repeat(700)] {
            for persistence in [Persistence::KeepAlive, Persistence::Close] {
                for body in [BodyMode::Full, BodyMode::HeadOnly] {
                    let mut out = b"prefix".to_vec();
                    render_redirect(location, persistence, body, &mut out);
                    assert_eq!(
                        out.len() - 6,
                        redirect_len(location.len(), persistence, body),
                        "{location} {persistence:?} {body:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn renders_redirect_close_head_only() {
        let mut out = Vec::new();
        render_redirect("/a/", Persistence::Close, BodyMode::HeadOnly, &mut out);
        assert_eq!(
            out,
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: /a/\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        );
    }
}
