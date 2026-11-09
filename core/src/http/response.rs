//! Response heads: fixed plain-text responses (status line plus short text
//! body), `301` redirects, the head of a `200 OK` or `206 Partial Content`
//! file response, the `416 Range Not Satisfiable` of a range request and
//! the `304 Not Modified` of a conditional request, and the merged head of
//! a backend-directed file (FR-PROXY-32).
//!
//! Each takes the core's current IMF-fixdate, if the driver has set one,
//! and then carries `Date` right after the status line (FR-HTTP-22a).

use std::io::Write as _;

use super::conditional::{ETAG_MAX_LEN, Validators};
use super::date::{IMF_FIXDATE_LEN, ImfFixdate};
use super::forward::{Kept, Sent};
use super::mime::LONGEST_CONTENT_TYPE;
use super::request::{Method, Persistence};
use super::status::Status;

/// Length of a `Date: <IMF-fixdate>\r\n` line: 37 bytes.
pub const DATE_LINE_LEN: usize = "Date: \r\n".len() + IMF_FIXDATE_LEN;

/// The line every file response but a `304` carries (FR-STATIC-19): the
/// server serves single byte ranges.
pub const ACCEPT_RANGES: &str = "Accept-Ranges: bytes\r\n";

/// Appends the `Date` line for `date`, if there is one.
fn put_date(date: Option<&ImfFixdate>, out: &mut Vec<u8>) {
    if let Some(date) = date {
        out.extend_from_slice(b"Date: ");
        out.extend_from_slice(date);
        out.extend_from_slice(b"\r\n");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    Full,
    HeadOnly,
}

impl BodyMode {
    /// How a response to a request with `method` is sent: every response
    /// to a HEAD request is head only (FR-HTTP-21).
    pub fn of(method: &Method) -> BodyMode {
        match method {
            Method::Head => BodyMode::HeadOnly,
            Method::Get | Method::Other(_) => BodyMode::Full,
        }
    }
}

/// Appends a fixed response for `status` to `out` (existing bytes are kept).
/// Never used for `304`, which has no body ([`render_not_modified`]), nor
/// for `416`, which names the file's size
/// ([`render_range_not_satisfiable`]).
/// `Content-Length` always describes the full body, also for `HeadOnly`.
/// A `405` names the allowed methods (`Allow: GET, HEAD`). `Date` follows
/// the status line when `date` is set.
pub fn render_fixed(
    status: Status,
    persistence: Persistence,
    body: BodyMode,
    date: Option<&ImfFixdate>,
    out: &mut Vec<u8>,
) {
    render_plain(status, Unsatisfiable::No, persistence, body, date, out);
}

/// Appends the `416 Range Not Satisfiable` of a `GET` whose one range
/// does not overlap a file of `size` bytes (FR-STATIC-19): the form of
/// [`render_fixed`] with body `"416 Range Not Satisfiable\n"`, plus
/// `Content-Range: bytes */<size>` and `Accept-Ranges: bytes` right after
/// `Content-Length`.
pub fn render_range_not_satisfiable(
    size: u64,
    persistence: Persistence,
    body: BodyMode,
    date: Option<&ImfFixdate>,
    out: &mut Vec<u8>,
) {
    render_plain(
        Status::RangeNotSatisfiable,
        Unsatisfiable::Size(size),
        persistence,
        body,
        date,
        out,
    );
}

/// Whether a plain response is a `416` naming the file's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unsatisfiable {
    No,
    Size(u64),
}

/// A fixed response for `status`, written straight into `out` (nothing
/// else is allocated); a `416` carries `Content-Range` and
/// `Accept-Ranges` right after `Content-Length`.
fn render_plain(
    status: Status,
    unsatisfiable: Unsatisfiable,
    persistence: Persistence,
    body: BodyMode,
    date: Option<&ImfFixdate>,
    out: &mut Vec<u8>,
) {
    let (code, reason) = (status.code(), status.reason());
    // Writing into a `Vec` never fails.
    let _infallible = write!(out, "HTTP/1.1 {code} {reason}\r\n");
    put_date(date, out);
    // The body is `<code> <reason>\n`; every code has three digits.
    let text_len = reason.len().saturating_add(5);
    let _infallible = write!(
        out,
        "Content-Type: text/plain; charset=utf-8\r\nContent-Length: {text_len}\r\n"
    );
    match unsatisfiable {
        Unsatisfiable::No => {}
        Unsatisfiable::Size(size) => {
            let _infallible = write!(out, "Content-Range: bytes */{size}\r\n{ACCEPT_RANGES}");
        }
    }
    out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
    match status {
        Status::MethodNotAllowed => out.extend_from_slice(b"Allow: GET, HEAD\r\n"),
        Status::Ok
        | Status::PartialContent
        | Status::MovedPermanently
        | Status::NotModified
        | Status::BadRequest
        | Status::NotFound
        | Status::RequestTimeout
        | Status::ContentTooLarge
        | Status::UriTooLong
        | Status::RangeNotSatisfiable
        | Status::ExpectationFailed
        | Status::RequestHeaderFieldsTooLarge
        | Status::BadGateway
        | Status::ServiceUnavailable
        | Status::GatewayTimeout
        | Status::HttpVersionNotSupported => {}
    }
    match persistence {
        Persistence::Close => out.extend_from_slice(CONNECTION_CLOSE.as_bytes()),
        Persistence::KeepAlive => {}
    }
    out.extend_from_slice(b"\r\n");
    match body {
        BodyMode::Full => {
            let _infallible = writeln!(out, "{code} {reason}");
        }
        BodyMode::HeadOnly => {}
    }
}

/// Body of every redirect.
const REDIRECT_BODY: &str = "301 Moved Permanently\n";

/// The redirect's status line.
const REDIRECT_STATUS: &str = "HTTP/1.1 301 Moved Permanently\r\n";

/// The redirect head after the `Date` line, up to the `Location` value.
const REDIRECT_START: &str = "Location: ";

/// The redirect head after the `Location` value, up to the optional
/// `Connection` line (`22` is `REDIRECT_BODY.len()`, checked by a test).
const REDIRECT_REST: &str = "\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\n";

const CONNECTION_CLOSE: &str = "Connection: close\r\n";

/// Appends a `301 Moved Permanently` response pointing at `location` (a
/// validated ASCII path, plus `?query`) to `out` (existing bytes are kept).
/// The body is fixed; `Content-Length` describes it also for `HeadOnly`.
/// `Date` follows the status line when `date` is set. It appends exactly
/// [`redirect_len`] bytes.
pub fn render_redirect(
    location: &str,
    persistence: Persistence,
    body: BodyMode,
    date: Option<&ImfFixdate>,
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(REDIRECT_STATUS.as_bytes());
    put_date(date, out);
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
pub fn redirect_len(
    location_len: usize,
    persistence: Persistence,
    body: BodyMode,
    date: Option<&ImfFixdate>,
) -> usize {
    let connection = match persistence {
        Persistence::Close => CONNECTION_CLOSE.len(),
        Persistence::KeepAlive => 0,
    };
    let body = match body {
        BodyMode::Full => REDIRECT_BODY.len(),
        BodyMode::HeadOnly => 0,
    };
    let date = match date {
        Some(_) => DATE_LINE_LEN,
        None => 0,
    };
    REDIRECT_STATUS
        .len()
        .saturating_add(date)
        .saturating_add(REDIRECT_START.len())
        .saturating_add(location_len)
        .saturating_add(REDIRECT_REST.len())
        .saturating_add(connection)
        .saturating_add(2)
        .saturating_add(body)
}

/// Appends the `ETag` and `Last-Modified` lines of `validators`.
fn put_validators(validators: &Validators, out: &mut Vec<u8>) {
    out.extend_from_slice(b"ETag: ");
    out.extend_from_slice(validators.etag.as_bytes());
    out.extend_from_slice(b"\r\nLast-Modified: ");
    out.extend_from_slice(&validators.last_modified);
    out.extend_from_slice(b"\r\n");
}

/// What of a file a file response carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// All `size` bytes (`200 OK`).
    Whole { size: u64 },
    /// Bytes `first` to `last` (inclusive, `first <= last < size`) of a
    /// file of `size` bytes (`206 Partial Content`, FR-STATIC-19).
    Range { first: u64, last: u64, size: u64 },
}

/// Appends the head of a file response of type `content_type` carrying
/// `part` (existing bytes are kept), in the order of FR-STATIC-11 and
/// FR-STATIC-19: status line (`200 OK` or `206 Partial Content`), `Date`
/// (when `date` is set), `Content-Type`, `Content-Length`,
/// `Content-Range` (206 only), `ETag`, `Last-Modified`, `Accept-Ranges`,
/// `X-Content-Type-Options`, `Connection: close` (only when closing).
pub fn render_file_head(
    content_type: &str,
    part: Part,
    validators: &Validators,
    persistence: Persistence,
    date: Option<&ImfFixdate>,
    out: &mut Vec<u8>,
) {
    match part {
        Part::Whole { size } => {
            out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
            put_date(date, out);
            out.extend_from_slice(
                format!("Content-Type: {content_type}\r\nContent-Length: {size}\r\n").as_bytes(),
            );
        }
        Part::Range { first, last, size } => {
            out.extend_from_slice(b"HTTP/1.1 206 Partial Content\r\n");
            put_date(date, out);
            let len = last.saturating_sub(first).saturating_add(1);
            out.extend_from_slice(
                format!(
                    "Content-Type: {content_type}\r\nContent-Length: {len}\r\n\
                     Content-Range: bytes {first}-{last}/{size}\r\n"
                )
                .as_bytes(),
            );
        }
    }
    put_validators(validators, out);
    out.extend_from_slice(ACCEPT_RANGES.as_bytes());
    out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
    match persistence {
        Persistence::Close => out.extend_from_slice(CONNECTION_CLOSE.as_bytes()),
        Persistence::KeepAlive => {}
    }
    out.extend_from_slice(b"\r\n");
}

/// Appends a `304 Not Modified` (FR-STATIC-18): status line, `Date` (when
/// `date` is set), `ETag`, `Last-Modified`, `Connection: close` (only when
/// closing). No content headers and no body, for `GET` and `HEAD` alike.
pub fn render_not_modified(
    validators: &Validators,
    persistence: Persistence,
    date: Option<&ImfFixdate>,
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(b"HTTP/1.1 304 Not Modified\r\n");
    put_date(date, out);
    put_validators(validators, out);
    match persistence {
        Persistence::Close => out.extend_from_slice(CONNECTION_CLOSE.as_bytes()),
        Persistence::KeepAlive => {}
    }
    out.extend_from_slice(b"\r\n");
}

/// The most bytes [`render_backend_file_head`] adds around a backend's
/// kept lines (FR-PROXY-32, NFR-RES-01): the longest status line (`206
/// Partial Content`, 30), `Date` (37), `Content-Type` with the longest
/// type of FR-STATIC-12 (46), `Content-Length` (38) and `Content-Range`
/// (85) with 20-digit numbers (`u64::MAX`), the longest `ETag` (43),
/// `Last-Modified` (46), `Accept-Ranges` (22), `X-Content-Type-Options`
/// (33), `Connection: close` (19) and the blank line (2): 401 bytes.
pub const FILE_HEAD_EXTRA: usize = "HTTP/1.1 206 Partial Content\r\n".len()
    + DATE_LINE_LEN
    + "Content-Type: \r\n".len()
    + LONGEST_CONTENT_TYPE
    + "Content-Length: \r\n".len()
    + U64_DIGITS
    + "Content-Range: bytes -/\r\n".len()
    + 3 * U64_DIGITS
    + "ETag: \r\n".len()
    + ETAG_MAX_LEN
    + "Last-Modified: \r\n".len()
    + IMF_FIXDATE_LEN
    + ACCEPT_RANGES.len()
    + NOSNIFF.len()
    + CONNECTION_CLOSE.len()
    + "\r\n".len();

/// The digits of `u64::MAX`.
const U64_DIGITS: usize = 20;

const NOSNIFF: &str = "X-Content-Type-Options: nosniff\r\n";

/// What a backend-directed file's response is (FR-PROXY-32): the file (a
/// `200` or a `206`), or a `304`. A `416` and a `404` are plain fixed
/// responses instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    File(Part),
    NotModified,
}

/// Turns `head`, which holds exactly a backend's kept lines
/// ([`render_kept_lines`](super::forward::render_kept_lines), whose
/// [`Kept`] is `has`), into the merged head of a backend-directed file
/// (FR-PROXY-32), in place: the core's status line and `Date` (only when
/// `date` is set and the backend sent none) go before the kept lines, the
/// core's lines in the order of FR-STATIC-11 after them. A `200` or `206`
/// adds `Content-Type` (`content_type`, only if the backend sent none),
/// `Content-Length`, `Content-Range` (206 only), `ETag`, `Last-Modified`,
/// `Accept-Ranges` and `X-Content-Type-Options` (only if the backend sent
/// none); a `304` adds `ETag` and `Last-Modified`. Then `Connection:
/// close` (only when closing) and the blank line.
///
/// It adds at most [`FILE_HEAD_EXTRA`] bytes, so a caller that checked
/// that `head` has that much spare capacity never makes it grow
/// (NFR-RES-03), and it allocates nothing else.
pub fn render_backend_file_head(
    status: FileStatus,
    head: &mut Vec<u8>,
    has: Kept,
    content_type: &str,
    validators: &Validators,
    persistence: Persistence,
    date: Option<&ImfFixdate>,
) {
    let line: &[u8] = match status {
        FileStatus::File(Part::Whole { .. }) => b"HTTP/1.1 200 OK\r\n",
        FileStatus::File(Part::Range { .. }) => b"HTTP/1.1 206 Partial Content\r\n",
        FileStatus::NotModified => b"HTTP/1.1 304 Not Modified\r\n",
    };
    let date = match has.date {
        Sent::Yes => None,
        Sent::No => date,
    };
    // The status line and `Date` go in front of the kept lines, which move
    // up within the buffer: nothing is allocated.
    let kept = head.len();
    let front = line.len() + date.map_or(0, |_| DATE_LINE_LEN);
    head.resize(kept + front, 0);
    head.copy_within(..kept, front);
    if let Some(mut slot) = head.get_mut(..front) {
        // Exactly `front` bytes: writing them into the slot never fails.
        let _infallible = slot.write_all(line);
        if let Some(date) = date {
            let _infallible = slot.write_all(b"Date: ");
            let _infallible = slot.write_all(date);
            let _infallible = slot.write_all(b"\r\n");
        }
    }
    match status {
        FileStatus::File(part) => {
            if let Sent::No = has.content_type {
                head.extend_from_slice(b"Content-Type: ");
                head.extend_from_slice(content_type.as_bytes());
                head.extend_from_slice(b"\r\n");
            }
            // Writing to a `Vec` never fails.
            let _infallible = match part {
                Part::Whole { size } => write!(head, "Content-Length: {size}\r\n"),
                Part::Range { first, last, size } => {
                    let len = last.saturating_sub(first).saturating_add(1);
                    write!(
                        head,
                        "Content-Length: {len}\r\nContent-Range: bytes {first}-{last}/{size}\r\n"
                    )
                }
            };
            put_validators(validators, head);
            head.extend_from_slice(ACCEPT_RANGES.as_bytes());
            if let Sent::No = has.nosniff {
                head.extend_from_slice(NOSNIFF.as_bytes());
            }
        }
        FileStatus::NotModified => put_validators(validators, head),
    }
    match persistence {
        Persistence::Close => head.extend_from_slice(CONNECTION_CLOSE.as_bytes()),
        Persistence::KeepAlive => {}
    }
    head.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileStamp;
    use crate::http::date::imf_fixdate;

    /// A file of `size` bytes last modified at the RFC's example second
    /// (plus 5 ns): `ETag` `"<size hex>-ae1b981bc490a05"`.
    fn validators(size: u64) -> Validators {
        Validators::of(FileStamp {
            size,
            modified_ns: 784_111_777_000_000_005,
        })
    }

    const VALIDATORS_1234: &str =
        "ETag: \"4d2-ae1b981bc490a05\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n";

    /// The example date of RFC 9110 section 5.6.7.
    fn rfc_date() -> ImfFixdate {
        imf_fixdate(784_111_777)
    }

    const DATE_LINE: &str = "Date: Sun, 06 Nov 1994 08:49:37 GMT\r\n";

    /// FR-HTTP-22a: once the driver set the time, every response the core
    /// renders carries `Date` right after its status line.
    #[test]
    fn renders_date_after_the_status_line() {
        let date = rfc_date();
        let mut out = Vec::new();
        render_fixed(
            Status::NotFound,
            Persistence::Close,
            BodyMode::Full,
            Some(&date),
            &mut out,
        );
        let expected = format!(
            "HTTP/1.1 404 Not Found\r\n{DATE_LINE}Content-Type: text/plain; charset=utf-8\r\nContent-Length: 14\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n404 Not Found\n"
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);

        let mut out = Vec::new();
        render_file_head(
            "image/png",
            Part::Whole { size: 0x4d2 },
            &validators(0x4d2),
            Persistence::KeepAlive,
            Some(&date),
            &mut out,
        );
        let expected = format!(
            "HTTP/1.1 200 OK\r\n{DATE_LINE}Content-Type: image/png\r\nContent-Length: 1234\r\n{VALIDATORS_1234}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);

        let mut out = Vec::new();
        render_redirect(
            "/a/",
            Persistence::KeepAlive,
            BodyMode::HeadOnly,
            Some(&date),
            &mut out,
        );
        let expected = format!(
            "HTTP/1.1 301 Moved Permanently\r\n{DATE_LINE}Location: /a/\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    /// The `Date` line is the 37 bytes the send-buffer arithmetic counts.
    #[test]
    fn date_line_is_37_bytes() {
        assert_eq!(DATE_LINE.len(), DATE_LINE_LEN);
        assert_eq!(DATE_LINE_LEN, 37);
    }

    #[test]
    fn renders_404_keep_alive() {
        let mut out = Vec::new();
        render_fixed(
            Status::NotFound,
            Persistence::KeepAlive,
            BodyMode::Full,
            None,
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
            None,
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
            None,
            &mut out,
        );
        let first = out.clone();
        render_fixed(
            Status::BadRequest,
            Persistence::Close,
            BodyMode::HeadOnly,
            None,
            &mut out,
        );
        let mut second = Vec::new();
        render_fixed(
            Status::BadRequest,
            Persistence::Close,
            BodyMode::HeadOnly,
            None,
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
            Part::Whole { size: 1234 },
            &validators(1234),
            Persistence::KeepAlive,
            None,
            &mut out,
        );
        let expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/css; charset=utf-8\r\nContent-Length: 1234\r\n{VALIDATORS_1234}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    /// FR-STATIC-19: a 206 names its part's length and `Content-Range`
    /// right after `Content-Length`, in the order of FR-STATIC-11.
    #[test]
    fn renders_partial_content_head() {
        let date = rfc_date();
        let mut out = b"x".to_vec();
        let part = Part::Range {
            first: 10,
            last: 19,
            size: 1234,
        };
        render_file_head(
            "image/png",
            part,
            &validators(1234),
            Persistence::Close,
            Some(&date),
            &mut out,
        );
        let expected = format!(
            "xHTTP/1.1 206 Partial Content\r\n{DATE_LINE}Content-Type: image/png\r\nContent-Length: 10\r\nContent-Range: bytes 10-19/1234\r\n{VALIDATORS_1234}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    /// FR-STATIC-19: a 416 is a fixed response naming the file's size.
    #[test]
    fn renders_range_not_satisfiable() {
        let mut out = Vec::new();
        render_range_not_satisfiable(1234, Persistence::KeepAlive, BodyMode::Full, None, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 26\r\nContent-Range: bytes */1234\r\nAccept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n416 Range Not Satisfiable\n"
        );
        let date = rfc_date();
        let mut out = Vec::new();
        render_range_not_satisfiable(
            0,
            Persistence::Close,
            BodyMode::HeadOnly,
            Some(&date),
            &mut out,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\n{DATE_LINE}Content-Type: text/plain; charset=utf-8\r\nContent-Length: 26\r\nContent-Range: bytes */0\r\nAccept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
            )
        );
    }

    /// A 416 and a fixed response are appended after the bytes `out`
    /// holds, the 416 naming any size, the 405 its `Allow` line after
    /// `X-Content-Type-Options`.
    #[test]
    fn plain_responses_append_their_exact_bytes() {
        let mut out = b"x".to_vec();
        render_range_not_satisfiable(u64::MAX, Persistence::Close, BodyMode::Full, None, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "xHTTP/1.1 416 Range Not Satisfiable\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 26\r\nContent-Range: bytes */18446744073709551615\r\n\
             Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\
             Connection: close\r\n\r\n416 Range Not Satisfiable\n"
        );
        let mut out = b"y".to_vec();
        render_fixed(
            Status::MethodNotAllowed,
            Persistence::KeepAlive,
            BodyMode::Full,
            None,
            &mut out,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "yHTTP/1.1 405 Method Not Allowed\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 23\r\nX-Content-Type-Options: nosniff\r\nAllow: GET, HEAD\r\n\
             \r\n405 Method Not Allowed\n"
        );
    }

    #[test]
    fn renders_file_head_close() {
        let mut out = b"x".to_vec();
        let zero = Validators::of(FileStamp {
            size: 0,
            modified_ns: 0,
        });
        render_file_head(
            "image/png",
            Part::Whole { size: 0 },
            &zero,
            Persistence::Close,
            None,
            &mut out,
        );
        assert_eq!(
            out,
            b"xHTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 0\r\nETag: \"0-0\"\r\nLast-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\nAccept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        );
    }

    /// FR-STATIC-18: a 304 carries `Date`, `ETag` and `Last-Modified`,
    /// `Connection: close` only when closing, and nothing else.
    #[test]
    fn renders_not_modified() {
        let date = rfc_date();
        let mut out = b"x".to_vec();
        render_not_modified(
            &validators(1234),
            Persistence::KeepAlive,
            Some(&date),
            &mut out,
        );
        let expected = format!("xHTTP/1.1 304 Not Modified\r\n{DATE_LINE}{VALIDATORS_1234}\r\n");
        assert_eq!(String::from_utf8(out).unwrap(), expected);
        let mut out = Vec::new();
        render_not_modified(&validators(1234), Persistence::Close, None, &mut out);
        let expected =
            format!("HTTP/1.1 304 Not Modified\r\n{VALIDATORS_1234}Connection: close\r\n\r\n");
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn renders_405_with_allow() {
        let mut out = Vec::new();
        render_fixed(
            Status::MethodNotAllowed,
            Persistence::KeepAlive,
            BodyMode::Full,
            None,
            &mut out,
        );
        assert_eq!(
            out,
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 23\r\nX-Content-Type-Options: nosniff\r\nAllow: GET, HEAD\r\n\r\n405 Method Not Allowed\n"
        );
    }

    /// Every fixed response's `Content-Length` is its body's length.
    #[test]
    fn content_length_is_the_body_length_for_every_status() {
        for status in [
            Status::Ok,
            Status::PartialContent,
            Status::BadRequest,
            Status::MovedPermanently,
            Status::NotModified,
            Status::NotFound,
            Status::MethodNotAllowed,
            Status::RequestTimeout,
            Status::ContentTooLarge,
            Status::UriTooLong,
            Status::RangeNotSatisfiable,
            Status::ExpectationFailed,
            Status::RequestHeaderFieldsTooLarge,
            Status::BadGateway,
            Status::ServiceUnavailable,
            Status::GatewayTimeout,
            Status::HttpVersionNotSupported,
        ] {
            let mut out = Vec::new();
            render_fixed(status, Persistence::Close, BodyMode::Full, None, &mut out);
            let text = String::from_utf8(out).unwrap();
            let (head, body) = text.split_once("\r\n\r\n").unwrap();
            assert_eq!(
                body,
                format!("{} {}\n", status.code(), status.reason()),
                "{status:?}"
            );
            assert!(
                head.contains(&format!("\r\nContent-Length: {}\r\n", body.len())),
                "{text}"
            );
        }
    }

    #[test]
    fn only_405_carries_allow() {
        for status in [
            Status::NotFound,
            Status::ContentTooLarge,
            Status::BadGateway,
        ] {
            let mut out = Vec::new();
            render_fixed(status, Persistence::Close, BodyMode::Full, None, &mut out);
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
            None,
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
        for date in [None, Some(imf_fixdate(0))] {
            for location in ["/", "/a/", "/docs/?x=1", &"/b".repeat(700)] {
                for persistence in [Persistence::KeepAlive, Persistence::Close] {
                    for body in [BodyMode::Full, BodyMode::HeadOnly] {
                        let mut out = b"prefix".to_vec();
                        render_redirect(location, persistence, body, date.as_ref(), &mut out);
                        assert_eq!(
                            out.len() - 6,
                            redirect_len(location.len(), persistence, body, date.as_ref()),
                            "{location} {persistence:?} {body:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn renders_redirect_close_head_only() {
        let mut out = Vec::new();
        render_redirect(
            "/a/",
            Persistence::Close,
            BodyMode::HeadOnly,
            None,
            &mut out,
        );
        assert_eq!(
            out,
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: /a/\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        );
    }
}
