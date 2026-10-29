//! The stage 4 log line format (FR-LOG-05): one `key=value` line per
//! entry, keys in a fixed order per kind, untrusted strings escaped
//! (NFR-SEC-07).
//!
//! ```text
//! line   = "kind=" kind *( " " key "=" value ) LF
//! value  = plain / quoted
//! plain  = 1*( ALPHA / DIGIT / "." / "_" / ":" / "/" / "@" / "%" / "+" / "-"
//!            / "[" / "]" )
//! quoted = DQUOTE *( "\" DQUOTE / "\\" / "\x" 2HEXLOWER / safe ) DQUOTE
//! safe   = any byte 0x21-0x7E except DQUOTE, "\" and "="
//! ```
//!
//! A value that is a plain token is written as it is; any other value
//! (the empty one too) is quoted. Inside quotes `"` is `\"`, `\` is `\\`,
//! and every control byte (0x00–0x1F, 0x7F), every byte ≥ 0x80, `=` and
//! space is `\xHH` (lowercase hex). So a line holds spaces only between
//! fields and no byte outside 0x20–0x7E but its final LF.
//!
//! An untrusted value (method, host, path) is at most 4 KiB as written,
//! quotes included (owner decision 2026-10-29, OA4): a longer one is cut
//! between two escapes (a quoted one closed with its quote) and followed
//! by `<key>_truncated=N`, `N` the bytes of the raw value not shown. So
//! every entry fits the sink's 64 KiB buffer.

use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};

use durchlauferhitzer_core::http::chunked::ChunkedError;
use durchlauferhitzer_core::http::request::{HttpError, Method};
use durchlauferhitzer_core::http::response_head::ResponseError;
use durchlauferhitzer_core::http::target::TargetError;
use durchlauferhitzer_core::routing::RouteMiss;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnId, FileError, LimitKind, Log, RejectReason, RequestBodyError,
    RequestLine, RequestSummary, TimeoutKind,
};

use super::{DriverLog, KeptReason, ShutdownPhase};

/// Appends the line of a core entry to `out`, its `\n` included.
///
/// `backends` is [`Core::backends`](durchlauferhitzer_core::Core::backends):
/// a [`BackendId`] is written as the address it indexes there.
pub fn format_entry(entry: &Log, backends: &[SocketAddr], out: &mut Vec<u8>) {
    match entry {
        Log::Response {
            conn,
            peer,
            status,
            request,
        } => {
            head(out, "response", *conn, *peer);
            number(out, "status", status.code());
            if let Some(request) = request {
                summary(out, request);
            }
        }
        Log::Rejected { conn, peer, reason } => {
            head(out, "rejected", *conn, *peer);
            out.extend_from_slice(b" reason=");
            reject_reason(out, reason);
            match reason {
                RejectReason::Http {
                    request: Some(RequestLine { method, path }),
                    ..
                } => {
                    method_field(out, method);
                    text(out, "path", path);
                }
                RejectReason::Http { request: None, .. }
                | RejectReason::Route(_)
                | RejectReason::Timeout(_)
                | RejectReason::Limit(_)
                | RejectReason::ShuttingDown
                | RejectReason::Body(_) => {}
            }
        }
        Log::Reset { conn, peer } => head(out, "reset", *conn, *peer),
        Log::FileAborted { conn, peer, error } => {
            head(out, "file_aborted", *conn, *peer);
            out.extend_from_slice(b" error=");
            out.extend_from_slice(file_error(*error).as_bytes());
        }
        Log::ProxiedResponse {
            conn,
            peer,
            backend,
            status,
            request,
        } => {
            head(out, "proxied_response", *conn, *peer);
            backend_field(out, backends, *backend);
            number(out, "status", *status);
            summary(out, request);
        }
        Log::BackendFailed {
            conn,
            peer,
            backend,
            kind,
        } => {
            head(out, "backend_failed", *conn, *peer);
            backend_field(out, backends, *backend);
            out.extend_from_slice(b" failure=");
            backend_failure(out, *kind);
        }
    }
    out.push(b'\n');
}

/// Appends the line of a driver entry to `out`, its `\n` included.
pub fn format_driver(entry: &DriverLog, out: &mut Vec<u8>) {
    let (kind, key, value) = match entry {
        DriverLog::AcceptFailed { errno } => ("accept_failed", "errno", i64::from(*errno)),
        DriverLog::SocketFailed { errno } => ("socket_failed", "errno", i64::from(*errno)),
        DriverLog::OpenFailed { errno } => ("open_failed", "errno", i64::from(*errno)),
        DriverLog::RegisterFailed { errno } => ("register_failed", "errno", i64::from(*errno)),
        DriverLog::Dropped { count } => {
            out.extend_from_slice(b"kind=dropped");
            number(out, "count", *count);
            out.push(b'\n');
            return;
        }
        DriverLog::PrivilegesKept { reason } => {
            out.extend_from_slice(b"kind=privileges action=kept reason=");
            out.extend_from_slice(match reason {
                KeptReason::NotRoot => b"not_root",
            });
            out.push(b'\n');
            return;
        }
        DriverLog::Shutdown { phase } => {
            out.extend_from_slice(b"kind=shutdown phase=");
            match phase {
                ShutdownPhase::Draining => out.extend_from_slice(b"draining"),
                ShutdownPhase::Aborting => out.extend_from_slice(b"aborting"),
                ShutdownPhase::Done { detached } => {
                    out.extend_from_slice(b"done");
                    number(out, "detached", detached);
                }
            }
            out.push(b'\n');
            return;
        }
    };
    out.extend_from_slice(b"kind=");
    out.extend_from_slice(kind.as_bytes());
    number(out, key, value);
    out.push(b'\n');
}

/// `kind=<kind> conn=<id> peer=<ip>`.
fn head(out: &mut Vec<u8>, kind: &str, conn: ConnId, peer: IpAddr) {
    out.extend_from_slice(b"kind=");
    out.extend_from_slice(kind.as_bytes());
    number(out, "conn", conn.0);
    // An `IpAddr` prints as digits, `.`, `:` and hex letters: plain.
    let _ = write!(out, " peer={peer}");
}

fn number(out: &mut Vec<u8>, key: &str, value: impl std::fmt::Display) {
    // Writing to a `Vec` never fails.
    let _ = write!(out, " {key}={value}");
}

/// ` backend=<address>`. A `SocketAddr` prints as an IPv4 address or a
/// bracketed IPv6 one (`%` and the scope id if it has one), `:` and the
/// port: plain. An id outside `backends` (never from the core) is written
/// as the index it is.
fn backend_field(out: &mut Vec<u8>, backends: &[SocketAddr], backend: BackendId) {
    let address = usize::try_from(backend.0)
        .ok()
        .and_then(|index| backends.get(index));
    match address {
        Some(address) => {
            let _ = write!(out, " backend={address}");
        }
        None => number(out, "backend", backend.0),
    }
}

/// ` method=… [host=…] path=…`; `host` is left out when the request had
/// none.
fn summary(out: &mut Vec<u8>, request: &RequestSummary) {
    method_field(out, &request.method);
    if let Some(host) = &request.host {
        text(out, "host", host);
    }
    text(out, "path", &request.path);
}

fn method_field(out: &mut Vec<u8>, method: &Method) {
    match method {
        Method::Get => out.extend_from_slice(b" method=GET"),
        Method::Head => out.extend_from_slice(b" method=HEAD"),
        Method::Other(name) => text(out, "method", name),
    }
}

/// The longest an untrusted value may be as written, quotes included
/// (OA4).
const MAX_VALUE: usize = 4096;

/// ` key=value` with an untrusted value, escaped and capped at
/// [`MAX_VALUE`] bytes as written (OA4); a value cut short is followed by
/// ` <key>_truncated=N`, `N` the bytes of the raw value not shown.
fn text(out: &mut Vec<u8>, key: &str, value: &str) {
    out.push(b' ');
    out.extend_from_slice(key.as_bytes());
    out.push(b'=');
    let shown = escape(out, value.as_bytes(), MAX_VALUE);
    let left = value.len().saturating_sub(shown);
    if left > 0 {
        let _ = write!(out, " {key}_truncated={left}");
    }
}

fn is_plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._:/@%+-[]".contains(&byte)
}

/// Writes `value` as a plain token or quoted (the module's grammar), in
/// at most `cap` bytes (at least 2): as much of it as fits, a quoted one
/// cut between two escapes and closed with its quote. Returns how many
/// bytes of `value` were written.
fn escape(out: &mut Vec<u8>, value: &[u8], cap: usize) -> usize {
    if !value.is_empty() && value.iter().copied().all(is_plain) {
        let shown = value.get(..cap).unwrap_or(value);
        out.extend_from_slice(shown);
        return shown.len();
    }
    out.push(b'"');
    // Room for the escapes, between the two quotes.
    let mut room = cap.saturating_sub(2);
    let mut shown = 0;
    for &byte in value {
        let mut unit = [0u8; 4];
        let len = match byte {
            b'"' | b'\\' => {
                unit = [b'\\', byte, 0, 0];
                2
            }
            0x21..=0x7e if byte != b'=' => {
                unit[0] = byte;
                1
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let hex = |nibble: u8| HEX.get(usize::from(nibble)).copied().unwrap_or(b'0');
                unit = [b'\\', b'x', hex(byte >> 4), hex(byte & 0x0f)];
                4
            }
        };
        if len > room {
            break;
        }
        room -= len;
        out.extend_from_slice(unit.get(..len).unwrap_or(&unit));
        shown += 1;
    }
    out.push(b'"');
    shown
}

fn reject_reason(out: &mut Vec<u8>, reason: &RejectReason) {
    let mut put = |parts: &[&str]| dotted(out, parts);
    match reason {
        RejectReason::Http {
            error: HttpError::BadTarget(target),
            ..
        } => put(&["http", "bad_target", target_error(*target)]),
        RejectReason::Http { error, .. } => put(&["http", http_error(*error)]),
        RejectReason::Route(miss) => put(&[
            "route",
            match miss {
                RouteMiss::UnknownHost => "unknown_host",
                RouteMiss::NoMatchingPath => "no_matching_path",
            },
        ]),
        RejectReason::Timeout(kind) => put(&[
            "timeout",
            match kind {
                TimeoutKind::Header => "header",
                TimeoutKind::Send => "send",
                TimeoutKind::Disk => "disk",
                TimeoutKind::RequestBody => "request_body",
            },
        ]),
        RejectReason::Limit(kind) => put(&[
            "limit",
            match kind {
                LimitKind::Connections => "connections",
                LimitKind::ConnectionsPerIp => "connections_per_ip",
            },
        ]),
        RejectReason::ShuttingDown => put(&["shutting_down"]),
        RejectReason::Body(error) => match error {
            RequestBodyError::Chunked(chunked) => {
                put(&["body", "chunked", chunked_error(*chunked)])
            }
            RequestBodyError::TooLarge => put(&["body", "too_large"]),
            RequestBodyError::ClosedEarly => put(&["body", "closed_early"]),
        },
    }
}

fn backend_failure(out: &mut Vec<u8>, failure: BackendFailure) {
    let mut put = |parts: &[&str]| dotted(out, parts);
    match failure {
        BackendFailure::Refused => put(&["refused"]),
        BackendFailure::Reset => put(&["reset"]),
        BackendFailure::ClosedEarly => put(&["closed_early"]),
        BackendFailure::BadResponse(error) => put(&["bad_response", response_error(error)]),
        BackendFailure::ConnectTimeout => put(&["connect_timeout"]),
        BackendFailure::ResponseTimeout => put(&["response_timeout"]),
        BackendFailure::BodyTimeout => put(&["body_timeout"]),
        BackendFailure::BadChunk(error) => put(&["bad_chunk", chunked_error(error)]),
        BackendFailure::BodyTooLarge => put(&["body_too_large"]),
        BackendFailure::TransferEncodingToHttp10 => put(&["transfer_encoding_to_http10"]),
    }
}

/// A nested enum value as one plain token: its levels joined by `.`.
fn dotted(out: &mut Vec<u8>, parts: &[&str]) {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(b'.');
        }
        out.extend_from_slice(part.as_bytes());
    }
}

fn http_error(error: HttpError) -> &'static str {
    match error {
        HttpError::RequestLineTooLong => "request_line_too_long",
        HttpError::HeadTooLarge => "head_too_large",
        HttpError::TooManyHeaders => "too_many_headers",
        HttpError::BadRequestLine => "bad_request_line",
        HttpError::BadMethod => "bad_method",
        HttpError::BadVersion => "bad_version",
        HttpError::UnsupportedVersion => "unsupported_version",
        HttpError::BadTarget(_) => "bad_target",
        HttpError::BareLf => "bare_lf",
        HttpError::BadHeaderName => "bad_header_name",
        HttpError::BadHeaderValue => "bad_header_value",
        HttpError::ObsFold => "obs_fold",
        HttpError::MissingHost => "missing_host",
        HttpError::DuplicateHost => "duplicate_host",
        HttpError::BadHost => "bad_host",
        HttpError::ContentLengthWithTransferEncoding => "content_length_with_transfer_encoding",
        HttpError::BadContentLength => "bad_content_length",
        HttpError::DuplicateContentLength => "duplicate_content_length",
        HttpError::UnsupportedTransferEncoding => "unsupported_transfer_encoding",
        HttpError::TransferEncodingInHttp10 => "transfer_encoding_in_http10",
        HttpError::ConnectionNamesFraming => "connection_names_framing",
        HttpError::BadExpect => "bad_expect",
    }
}

fn target_error(error: TargetError) -> &'static str {
    match error {
        TargetError::NotOriginForm => "not_origin_form",
        TargetError::Empty => "empty",
        TargetError::DotSegment => "dot_segment",
        TargetError::EmptySegment => "empty_segment",
        TargetError::EncodedDot => "encoded_dot",
        TargetError::EncodedUnreserved => "encoded_unreserved",
        TargetError::EncodedSlash => "encoded_slash",
        TargetError::EncodedBackslash => "encoded_backslash",
        TargetError::EncodedNul => "encoded_nul",
        TargetError::BadPercentEncoding => "bad_percent_encoding",
        TargetError::Backslash => "backslash",
        TargetError::NonAscii => "non_ascii",
        TargetError::ControlOrSpace => "control_or_space",
        TargetError::Fragment => "fragment",
        TargetError::Semicolon => "semicolon",
        TargetError::EncodedControl => "encoded_control",
        TargetError::QueryInPath => "query_in_path",
        TargetError::EncodedQuestionMark => "encoded_question_mark",
    }
}

fn chunked_error(error: ChunkedError) -> &'static str {
    match error {
        ChunkedError::BadSize => "bad_size",
        ChunkedError::SizeOverflow => "size_overflow",
        ChunkedError::Extension => "extension",
        ChunkedError::Trailer => "trailer",
        ChunkedError::BadLineEnd => "bad_line_end",
    }
}

fn response_error(error: ResponseError) -> &'static str {
    match error {
        ResponseError::HeadTooLarge => "head_too_large",
        ResponseError::TooManyHeaders => "too_many_headers",
        ResponseError::BadStatusLine => "bad_status_line",
        ResponseError::BadVersion => "bad_version",
        ResponseError::BadHeaderName => "bad_header_name",
        ResponseError::BadHeaderValue => "bad_header_value",
        ResponseError::BareLf => "bare_lf",
        ResponseError::ObsFold => "obs_fold",
        ResponseError::ContentLengthWithTransferEncoding => "content_length_with_transfer_encoding",
        ResponseError::BadContentLength => "bad_content_length",
        ResponseError::DuplicateContentLength => "duplicate_content_length",
        ResponseError::UnsupportedTransferEncoding => "unsupported_transfer_encoding",
        ResponseError::ConnectionNamesFraming => "connection_names_framing",
        ResponseError::SwitchingProtocols => "switching_protocols",
        ResponseError::TooManyInterimHeads => "too_many_interim_heads",
    }
}

fn file_error(error: FileError) -> &'static str {
    match error {
        FileError::NotFound => "not_found",
        FileError::Symlink => "symlink",
        FileError::PermissionDenied => "permission_denied",
        FileError::Truncated => "truncated",
        FileError::Changed => "changed",
        FileError::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use durchlauferhitzer_core::http::request::{HttpError, Method};
    use durchlauferhitzer_core::http::status::Status;
    use durchlauferhitzer_core::{ConnId, Log, RejectReason, RequestLine, RequestSummary};

    use super::{MAX_VALUE, format_entry};
    use crate::log::buffer::CAPACITY;

    /// `assert_eq!` for long strings: names the lengths and where they
    /// first differ, not both strings in full.
    fn same(left: impl AsRef<str>, right: impl AsRef<str>) {
        let (left, right) = (left.as_ref(), right.as_ref());
        if left != right {
            let at = left
                .bytes()
                .zip(right.bytes())
                .position(|(a, b)| a != b)
                .unwrap_or(left.len().min(right.len()));
            panic!(
                "lengths {} and {}, first difference at {at}: {:?} vs {:?}",
                left.len(),
                right.len(),
                left.get(at.saturating_sub(8)..)
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect::<String>(),
                right
                    .get(at.saturating_sub(8)..)
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect::<String>(),
            );
        }
    }

    fn line(entry: &Log) -> String {
        let mut out = Vec::new();
        format_entry(entry, &[], &mut out);
        String::from_utf8(out).unwrap()
    }

    fn response(method: Method, host: &str, path: &str) -> Log {
        Log::Response {
            conn: ConnId(1),
            peer: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)),
            status: Status::Ok,
            request: Some(RequestSummary {
                method,
                host: Some(host.to_owned()),
                path: path.to_owned(),
            }),
        }
    }

    /// The `path` value and what follows it.
    fn path_field(path: &str) -> String {
        let full = line(&response(Method::Get, "h", path));
        full.strip_prefix("kind=response conn=1 peer=192.0.2.7 status=200 method=GET host=h path=")
            .unwrap()
            .strip_suffix('\n')
            .unwrap()
            .to_owned()
    }

    /// OA4 (owner decision 2026-10-29): a value is at most 4 KiB as
    /// written, its quotes included. One that fits exactly is whole.
    #[test]
    fn a_value_of_exactly_4_kib_is_whole() {
        assert_eq!(MAX_VALUE, 4096);
        let plain = "/".to_owned() + &"a".repeat(4095);
        same(path_field(&plain), plain);
        // Quoted: 4094 bytes and two quotes.
        let quoted = "/".to_owned() + &"a".repeat(4092) + "?";
        same(path_field(&quoted), format!("\"{quoted}\""));
    }

    /// OA4: a longer plain value is cut at 4 KiB; `path_truncated` counts
    /// the raw bytes left out.
    #[test]
    fn a_long_plain_value_is_cut_and_counted() {
        let path = "/".to_owned() + &"a".repeat(4999);
        same(
            path_field(&path),
            format!("{} path_truncated=904", &path[..4096]),
        );
    }

    /// OA4: a quoted value is cut on an escape boundary, never inside
    /// `\xHH` or `\"`, and closed with its quote.
    #[test]
    fn a_long_quoted_value_is_cut_on_an_escape_boundary() {
        // One byte too many for the quotes.
        let path = "/".to_owned() + &"a".repeat(4093) + "?";
        same(
            path_field(&path),
            format!("\"{}\" path_truncated=1", &path[..4094]),
        );
        // `\x0a` (four bytes) does not fit in the two left: cut before it.
        let path = "/".to_owned() + &"a".repeat(4091) + "\nb";
        same(
            path_field(&path),
            format!("\"{}\" path_truncated=2", &path[..4092]),
        );
        // All escaped: 1023 of `\x0a` are 4092 bytes, plus the quotes.
        let path = "\n".repeat(2000);
        same(
            path_field(&path),
            format!("\"{}\" path_truncated=977", "\\x0a".repeat(1023)),
        );
        // `\"` (two bytes) fits exactly in the last two.
        let path = "/".to_owned() + &"a".repeat(4091) + "\"\"";
        same(
            path_field(&path),
            format!("\"{}\\\"\" path_truncated=1", &path[..4092]),
        );
    }

    /// OA4: method and host are capped the same way, each with its own
    /// `<key>_truncated` right after it.
    #[test]
    fn method_and_host_are_capped_too() {
        let method = "M".repeat(5000);
        let host = " ".repeat(2000);
        let entry = response(Method::Other(method.clone()), &host, "/");
        same(
            line(&entry),
            format!(
                "kind=response conn=1 peer=192.0.2.7 status=200 method={} \
                 method_truncated=904 host=\"{}\" host_truncated=977 path=/\n",
                &method[..4096],
                "\\x20".repeat(1023)
            ),
        );
    }

    /// OA4: so every entry fits the sink's 64 KiB buffer, however long
    /// its untrusted strings (each written four bytes per byte).
    #[test]
    fn every_entry_fits_the_buffer() {
        let long = "\u{1}".repeat(CAPACITY);
        let entry = response(Method::Other(long.clone()), &long, &long);
        assert!(line(&entry).len() < 3 * (MAX_VALUE + 64));
        let rejected = Log::Rejected {
            conn: ConnId(u64::MAX),
            peer: IpAddr::V4(Ipv4Addr::BROADCAST),
            reason: RejectReason::Http {
                error: HttpError::BadHeaderValue,
                request: Some(RequestLine {
                    method: Method::Other(long.clone()),
                    path: long,
                }),
            },
        };
        assert!(line(&rejected).len() < CAPACITY);
    }
}
