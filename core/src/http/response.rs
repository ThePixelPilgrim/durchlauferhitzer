//! Response heads: fixed plain-text responses (status line plus short text
//! body) and the head of a `200 OK` file response.

use super::request::Persistence;
use super::status::Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    Full,
    HeadOnly,
}

/// Appends a fixed response for `status` to `out` (existing bytes are kept).
/// `Content-Length` always describes the full body, also for `HeadOnly`.
pub fn render_fixed(status: Status, persistence: Persistence, body: BodyMode, out: &mut Vec<u8>) {
    let text = format!("{} {}\n", status.code(), status.reason());
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\n",
        status.code(),
        status.reason(),
        text.len()
    );
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
}
