//! The log format (FR-LOG-05, NFR-SEC-07): one exact line per kind, the
//! escaping table, and a seeded property test that no entry, whatever its
//! untrusted strings, writes more than one line or a raw control byte.
//! The sink tests (FR-LOG-06) are in `log_sink.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

use durchlauferhitzer::log::{format_driver, format_entry};
use durchlauferhitzer::{DriverLog, KeptReason, ShutdownPhase};
use durchlauferhitzer_core::http::chunked::ChunkedError;
use durchlauferhitzer_core::http::request::{HttpError, Method};
use durchlauferhitzer_core::http::response_head::ResponseError;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::http::target::TargetError;
use durchlauferhitzer_core::routing::RouteMiss;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnId, FileError, LimitKind, Log, RejectReason, RequestBodyError,
    RequestLine, RequestSummary, TimeoutKind,
};

/// The backends the core entries name by index (`Core::backends`).
fn backends() -> Vec<SocketAddr> {
    vec![
        "127.0.0.1:9000".parse().unwrap(),
        "[::1]:9000".parse().unwrap(),
        SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 8080, 0, 2)),
    ]
}

fn line(entry: &Log) -> String {
    let mut out = Vec::new();
    format_entry(entry, &backends(), &mut out);
    String::from_utf8(out).unwrap()
}

fn driver_line(entry: &DriverLog) -> String {
    let mut out = Vec::new();
    format_driver(entry, &mut out);
    String::from_utf8(out).unwrap()
}

const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7));

fn summary(method: Method, host: Option<&str>, path: &str) -> RequestSummary {
    RequestSummary {
        method,
        host: host.map(str::to_owned),
        path: path.to_owned(),
    }
}

// ---- one exact line per kind ---------------------------------------------

#[test]
fn response_with_a_request() {
    let entry = Log::Response {
        conn: ConnId(12),
        peer: PEER,
        status: Status::NotFound,
        request: Some(summary(Method::Get, Some("example.test"), "/a/b.html")),
    };
    assert_eq!(
        line(&entry),
        "kind=response conn=12 peer=192.0.2.7 status=404 method=GET host=example.test path=/a/b.html\n"
    );
}

#[test]
fn response_without_a_request_or_host() {
    let entry = Log::Response {
        conn: ConnId(1),
        peer: IpAddr::V6(Ipv6Addr::LOCALHOST),
        status: Status::BadRequest,
        request: None,
    };
    assert_eq!(line(&entry), "kind=response conn=1 peer=::1 status=400\n");
    let entry = Log::Response {
        conn: ConnId(2),
        peer: PEER,
        status: Status::Ok,
        request: Some(summary(Method::Head, None, "/")),
    };
    assert_eq!(
        line(&entry),
        "kind=response conn=2 peer=192.0.2.7 status=200 method=HEAD path=/\n"
    );
}

#[test]
fn rejected_with_and_without_a_request_line() {
    let entry = Log::Rejected {
        conn: ConnId(3),
        peer: PEER,
        reason: RejectReason::Http {
            error: HttpError::BadHeaderValue,
            request: Some(RequestLine {
                method: Method::Other("PURGE".to_owned()),
                path: "/x".to_owned(),
            }),
        },
    };
    assert_eq!(
        line(&entry),
        "kind=rejected conn=3 peer=192.0.2.7 reason=http.bad_header_value method=PURGE path=/x\n"
    );
    let entry = Log::Rejected {
        conn: ConnId(4),
        peer: PEER,
        reason: RejectReason::Http {
            error: HttpError::BadTarget(TargetError::DotSegment),
            request: None,
        },
    };
    assert_eq!(
        line(&entry),
        "kind=rejected conn=4 peer=192.0.2.7 reason=http.bad_target.dot_segment\n"
    );
}

#[test]
fn reset() {
    let entry = Log::Reset {
        conn: ConnId(5),
        peer: PEER,
    };
    assert_eq!(line(&entry), "kind=reset conn=5 peer=192.0.2.7\n");
}

#[test]
fn file_aborted() {
    let entry = Log::FileAborted {
        conn: ConnId(6),
        peer: PEER,
        error: FileError::Changed,
    };
    assert_eq!(
        line(&entry),
        "kind=file_aborted conn=6 peer=192.0.2.7 error=changed\n"
    );
}

#[test]
fn proxied_response() {
    let entry = Log::ProxiedResponse {
        conn: ConnId(7),
        peer: PEER,
        backend: BackendId(2),
        status: 299,
        request: summary(Method::Get, Some("api.test:8080"), "/v1/items"),
    };
    assert_eq!(
        line(&entry),
        "kind=proxied_response conn=7 peer=192.0.2.7 backend=[fe80::1%2]:8080 status=299 \
         method=GET host=api.test:8080 path=/v1/items\n"
    );
}

#[test]
fn backend_failed() {
    let entry = Log::BackendFailed {
        conn: ConnId(8),
        peer: PEER,
        backend: BackendId(0),
        kind: BackendFailure::BadResponse(ResponseError::ObsFold),
    };
    assert_eq!(
        line(&entry),
        "kind=backend_failed conn=8 peer=192.0.2.7 backend=127.0.0.1:9000 \
         failure=bad_response.obs_fold\n"
    );
    let entry = Log::BackendFailed {
        conn: ConnId(9),
        peer: PEER,
        backend: BackendId(1),
        kind: BackendFailure::Refused,
    };
    assert_eq!(
        line(&entry),
        "kind=backend_failed conn=9 peer=192.0.2.7 backend=[::1]:9000 failure=refused\n"
    );
}

/// A backend id outside `Core::backends` (never from the core) is
/// written as the index it is, still a plain token.
#[test]
fn a_backend_id_outside_the_list_is_its_index() {
    let entry = Log::BackendFailed {
        conn: ConnId(1),
        peer: PEER,
        backend: BackendId(3),
        kind: BackendFailure::Reset,
    };
    assert_eq!(
        line(&entry),
        "kind=backend_failed conn=1 peer=192.0.2.7 backend=3 failure=reset\n"
    );
}

#[test]
fn driver_kinds() {
    let cases = [
        (
            DriverLog::AcceptFailed {
                errno: libc::EMFILE,
            },
            "kind=accept_failed errno=24\n",
        ),
        (
            DriverLog::SocketFailed {
                errno: libc::ENFILE,
            },
            "kind=socket_failed errno=23\n",
        ),
        (
            DriverLog::OpenFailed {
                errno: libc::EMFILE,
            },
            "kind=open_failed errno=24\n",
        ),
        (
            DriverLog::RegisterFailed { errno: libc::EPERM },
            "kind=register_failed errno=1\n",
        ),
        (DriverLog::Dropped { count: 17 }, "kind=dropped count=17\n"),
        (
            DriverLog::PrivilegesKept {
                reason: KeptReason::NotRoot,
            },
            "kind=privileges action=kept reason=not_root\n",
        ),
        (
            DriverLog::Shutdown {
                phase: ShutdownPhase::Draining,
            },
            "kind=shutdown phase=draining\n",
        ),
        (
            DriverLog::Shutdown {
                phase: ShutdownPhase::Aborting,
            },
            "kind=shutdown phase=aborting\n",
        ),
        (
            DriverLog::Shutdown {
                phase: ShutdownPhase::Done { detached: 3 },
            },
            "kind=shutdown phase=done detached=3\n",
        ),
    ];
    for (entry, expected) in cases {
        assert_eq!(driver_line(&entry), expected, "{entry:?}");
    }
}

// ---- every reason, failure and file error value ---------------------------

fn reason(reason: RejectReason) -> String {
    let full = line(&Log::Rejected {
        conn: ConnId(1),
        peer: PEER,
        reason,
    });
    full.strip_prefix("kind=rejected conn=1 peer=192.0.2.7 reason=")
        .unwrap()
        .strip_suffix('\n')
        .unwrap()
        .to_owned()
}

fn http(error: HttpError) -> RejectReason {
    RejectReason::Http {
        error,
        request: None,
    }
}

#[test]
fn every_http_error_value() {
    let cases = [
        (HttpError::RequestLineTooLong, "request_line_too_long"),
        (HttpError::HeadTooLarge, "head_too_large"),
        (HttpError::TooManyHeaders, "too_many_headers"),
        (HttpError::BadRequestLine, "bad_request_line"),
        (HttpError::BadMethod, "bad_method"),
        (HttpError::BadVersion, "bad_version"),
        (HttpError::UnsupportedVersion, "unsupported_version"),
        (HttpError::BareLf, "bare_lf"),
        (HttpError::BadHeaderName, "bad_header_name"),
        (HttpError::BadHeaderValue, "bad_header_value"),
        (HttpError::ObsFold, "obs_fold"),
        (HttpError::MissingHost, "missing_host"),
        (HttpError::DuplicateHost, "duplicate_host"),
        (HttpError::BadHost, "bad_host"),
        (
            HttpError::ContentLengthWithTransferEncoding,
            "content_length_with_transfer_encoding",
        ),
        (HttpError::BadContentLength, "bad_content_length"),
        (
            HttpError::DuplicateContentLength,
            "duplicate_content_length",
        ),
        (
            HttpError::UnsupportedTransferEncoding,
            "unsupported_transfer_encoding",
        ),
        (
            HttpError::TransferEncodingInHttp10,
            "transfer_encoding_in_http10",
        ),
        (
            HttpError::ConnectionNamesFraming,
            "connection_names_framing",
        ),
        (HttpError::BadExpect, "bad_expect"),
    ];
    for (error, expected) in cases {
        assert_eq!(reason(http(error)), format!("http.{expected}"));
    }
}

#[test]
fn every_target_error_value() {
    let cases = [
        (TargetError::NotOriginForm, "not_origin_form"),
        (TargetError::Empty, "empty"),
        (TargetError::DotSegment, "dot_segment"),
        (TargetError::EmptySegment, "empty_segment"),
        (TargetError::EncodedDot, "encoded_dot"),
        (TargetError::EncodedUnreserved, "encoded_unreserved"),
        (TargetError::EncodedSlash, "encoded_slash"),
        (TargetError::EncodedBackslash, "encoded_backslash"),
        (TargetError::EncodedNul, "encoded_nul"),
        (TargetError::BadPercentEncoding, "bad_percent_encoding"),
        (TargetError::Backslash, "backslash"),
        (TargetError::NonAscii, "non_ascii"),
        (TargetError::ControlOrSpace, "control_or_space"),
        (TargetError::Fragment, "fragment"),
        (TargetError::Semicolon, "semicolon"),
        (TargetError::EncodedControl, "encoded_control"),
        (TargetError::QueryInPath, "query_in_path"),
        (TargetError::EncodedQuestionMark, "encoded_question_mark"),
    ];
    for (error, expected) in cases {
        assert_eq!(
            reason(http(HttpError::BadTarget(error))),
            format!("http.bad_target.{expected}")
        );
    }
}

const CHUNKED: [(ChunkedError, &str); 5] = [
    (ChunkedError::BadSize, "bad_size"),
    (ChunkedError::SizeOverflow, "size_overflow"),
    (ChunkedError::Extension, "extension"),
    (ChunkedError::Trailer, "trailer"),
    (ChunkedError::BadLineEnd, "bad_line_end"),
];

#[test]
fn every_other_reject_reason_value() {
    let mut cases = vec![
        (
            RejectReason::Route(RouteMiss::UnknownHost),
            "route.unknown_host".to_owned(),
        ),
        (
            RejectReason::Route(RouteMiss::NoMatchingPath),
            "route.no_matching_path".to_owned(),
        ),
        (
            RejectReason::Timeout(TimeoutKind::Header),
            "timeout.header".to_owned(),
        ),
        (
            RejectReason::Timeout(TimeoutKind::Send),
            "timeout.send".to_owned(),
        ),
        (
            RejectReason::Timeout(TimeoutKind::Disk),
            "timeout.disk".to_owned(),
        ),
        (
            RejectReason::Timeout(TimeoutKind::RequestBody),
            "timeout.request_body".to_owned(),
        ),
        (
            RejectReason::Limit(LimitKind::Connections),
            "limit.connections".to_owned(),
        ),
        (
            RejectReason::Limit(LimitKind::ConnectionsPerIp),
            "limit.connections_per_ip".to_owned(),
        ),
        (RejectReason::ShuttingDown, "shutting_down".to_owned()),
        (
            RejectReason::Body(RequestBodyError::TooLarge),
            "body.too_large".to_owned(),
        ),
        (
            RejectReason::Body(RequestBodyError::ClosedEarly),
            "body.closed_early".to_owned(),
        ),
    ];
    for (error, name) in CHUNKED {
        cases.push((
            RejectReason::Body(RequestBodyError::Chunked(error)),
            format!("body.chunked.{name}"),
        ));
    }
    for (value, expected) in cases {
        assert_eq!(reason(value), expected);
    }
}

fn failure(kind: BackendFailure) -> String {
    let full = line(&Log::BackendFailed {
        conn: ConnId(1),
        peer: PEER,
        backend: BackendId(0),
        kind,
    });
    full.strip_prefix("kind=backend_failed conn=1 peer=192.0.2.7 backend=127.0.0.1:9000 failure=")
        .unwrap()
        .strip_suffix('\n')
        .unwrap()
        .to_owned()
}

#[test]
fn every_backend_failure_value() {
    let mut cases = vec![
        (BackendFailure::Refused, "refused".to_owned()),
        (BackendFailure::Reset, "reset".to_owned()),
        (BackendFailure::ClosedEarly, "closed_early".to_owned()),
        (BackendFailure::ConnectTimeout, "connect_timeout".to_owned()),
        (
            BackendFailure::ResponseTimeout,
            "response_timeout".to_owned(),
        ),
        (BackendFailure::BodyTimeout, "body_timeout".to_owned()),
        (BackendFailure::BodyTooLarge, "body_too_large".to_owned()),
        (
            BackendFailure::TransferEncodingToHttp10,
            "transfer_encoding_to_http10".to_owned(),
        ),
    ];
    for (error, name) in CHUNKED {
        cases.push((BackendFailure::BadChunk(error), format!("bad_chunk.{name}")));
    }
    let responses = [
        (ResponseError::HeadTooLarge, "head_too_large"),
        (ResponseError::TooManyHeaders, "too_many_headers"),
        (ResponseError::BadStatusLine, "bad_status_line"),
        (ResponseError::BadVersion, "bad_version"),
        (ResponseError::BadHeaderName, "bad_header_name"),
        (ResponseError::BadHeaderValue, "bad_header_value"),
        (ResponseError::BareLf, "bare_lf"),
        (ResponseError::ObsFold, "obs_fold"),
        (
            ResponseError::ContentLengthWithTransferEncoding,
            "content_length_with_transfer_encoding",
        ),
        (ResponseError::BadContentLength, "bad_content_length"),
        (
            ResponseError::DuplicateContentLength,
            "duplicate_content_length",
        ),
        (
            ResponseError::UnsupportedTransferEncoding,
            "unsupported_transfer_encoding",
        ),
        (
            ResponseError::ConnectionNamesFraming,
            "connection_names_framing",
        ),
        (ResponseError::SwitchingProtocols, "switching_protocols"),
        (ResponseError::TooManyInterimHeads, "too_many_interim_heads"),
    ];
    for (error, name) in responses {
        cases.push((
            BackendFailure::BadResponse(error),
            format!("bad_response.{name}"),
        ));
    }
    for (value, expected) in cases {
        assert_eq!(failure(value), expected);
    }
}

#[test]
fn every_file_error_value() {
    let cases = [
        (FileError::NotFound, "not_found"),
        (FileError::Symlink, "symlink"),
        (FileError::PermissionDenied, "permission_denied"),
        (FileError::Truncated, "truncated"),
        (FileError::Changed, "changed"),
        (FileError::Other, "other"),
    ];
    for (error, name) in cases {
        let entry = Log::FileAborted {
            conn: ConnId(1),
            peer: PEER,
            error,
        };
        assert_eq!(
            line(&entry),
            format!("kind=file_aborted conn=1 peer=192.0.2.7 error={name}\n")
        );
    }
}

// ---- escaping (FR-LOG-05, NFR-SEC-07) -------------------------------------

/// How the `path` value of a response comes out.
fn escaped_path(path: &str) -> String {
    let full = line(&Log::Response {
        conn: ConnId(1),
        peer: PEER,
        status: Status::Ok,
        request: Some(summary(Method::Get, Some("h"), path)),
    });
    full.strip_prefix("kind=response conn=1 peer=192.0.2.7 status=200 method=GET host=h path=")
        .unwrap()
        .strip_suffix('\n')
        .unwrap()
        .to_owned()
}

#[test]
fn the_escaping_table() {
    let cases = [
        // Plain tokens stay as they are.
        ("/a-b_c.d~", "\"/a-b_c.d~\""),
        ("/A9._:/@%+-[]", "/A9._:/@%+-[]"),
        // Anything else is quoted.
        ("", "\"\""),
        ("/a?b", "\"/a?b\""),
        ("/a\rb", "\"/a\\x0db\""),
        ("/a\nb", "\"/a\\x0ab\""),
        ("/a\0b", "\"/a\\x00b\""),
        ("/a\"b", "\"/a\\\"b\""),
        ("/a\\b", "\"/a\\\\b\""),
        ("/a=b", "\"/a\\x3db\""),
        ("/a b", "\"/a\\x20b\""),
        ("/a\x7fb", "\"/a\\x7fb\""),
        ("/a\u{ff}b", "\"/a\\xc3\\xbfb\""),
        ("\t", "\"\\x09\""),
    ];
    for (raw, expected) in cases {
        assert_eq!(escaped_path(raw), expected, "{raw:?}");
    }
}

#[test]
fn untrusted_host_and_method_are_escaped_too() {
    let entry = Log::Response {
        conn: ConnId(1),
        peer: PEER,
        status: Status::Ok,
        request: Some(summary(
            Method::Other("GE T\n".to_owned()),
            Some("evil.test\r\nkind=reset"),
            "/",
        )),
    };
    assert_eq!(
        line(&entry),
        "kind=response conn=1 peer=192.0.2.7 status=200 method=\"GE\\x20T\\x0a\" \
         host=\"evil.test\\x0d\\x0akind\\x3dreset\" path=/\n"
    );
}

// ---- the property: one line, no raw control byte (Review Focus 3) ----------

/// A seeded xorshift64* generator: deterministic, so a failure names the
/// seed that reproduces it, and no dev-dependency is needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A string mixing plain bytes, the escaped ASCII bytes and arbitrary
    /// code points (all of them non-ASCII bytes once encoded).
    fn string(&mut self) -> String {
        let len = self.below(24);
        (0..len)
            .map(|_| match self.below(4) {
                0 => char::from(b"aZ09._:/@%+-"[self.below(12) as usize]),
                1 => char::from(b"\r\n\0\t\x1b\x7f\"\\= ?#"[self.below(12) as usize]),
                2 => char::from_u32(self.below(0x11_0000) as u32).unwrap_or('\u{fffd}'),
                _ => char::from(self.below(128) as u8),
            })
            .collect()
    }

    fn method(&mut self) -> Method {
        match self.below(3) {
            0 => Method::Get,
            1 => Method::Head,
            _ => Method::Other(self.string()),
        }
    }

    fn entry(&mut self) -> Log {
        let conn = ConnId(self.next());
        let peer = IpAddr::V4(Ipv4Addr::from(self.next() as u32));
        let request = RequestSummary {
            method: self.method(),
            host: (self.below(2) == 0).then(|| self.string()),
            path: self.string(),
        };
        match self.below(3) {
            0 => Log::Response {
                conn,
                peer,
                status: Status::Ok,
                request: Some(request),
            },
            1 => Log::ProxiedResponse {
                conn,
                peer,
                // One past the list too: the index fallback.
                backend: BackendId(self.below(4) as u32),
                status: self.below(600) as u16,
                request,
            },
            _ => Log::Rejected {
                conn,
                peer,
                reason: RejectReason::Http {
                    error: HttpError::BadHeaderValue,
                    request: Some(RequestLine {
                        method: request.method,
                        path: request.path,
                    }),
                },
            },
        }
    }
}

/// The grammar's plain bytes (FR-LOG-05).
fn is_plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._:/@%+-[]".contains(&byte)
}

/// Splits a formatted line into its values, undoing the escaping; `None`
/// if the line breaks the grammar.
fn parse(line: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let body = line.strip_suffix(b"\n")?;
    let mut fields = Vec::new();
    let mut rest = body;
    loop {
        let eq = rest.iter().position(|&b| b == b'=')?;
        let key = std::str::from_utf8(&rest[..eq]).ok()?.to_owned();
        rest = &rest[eq + 1..];
        let mut value = Vec::new();
        if rest.first() == Some(&b'"') {
            let mut i = 1;
            loop {
                match *rest.get(i)? {
                    b'"' => break,
                    b'\\' => match *rest.get(i + 1)? {
                        b'"' => {
                            value.push(b'"');
                            i += 2;
                        }
                        b'\\' => {
                            value.push(b'\\');
                            i += 2;
                        }
                        b'x' => {
                            let hex = std::str::from_utf8(rest.get(i + 2..i + 4)?).ok()?;
                            value.push(u8::from_str_radix(hex, 16).ok()?);
                            i += 4;
                        }
                        _ => return None,
                    },
                    b => {
                        if !(0x21..0x7f).contains(&b) || b == b'=' {
                            return None;
                        }
                        value.push(b);
                        i += 1;
                    }
                }
            }
            rest = &rest[i + 1..];
        } else {
            let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
            value.extend_from_slice(&rest[..end]);
            if value.is_empty() || !value.iter().all(|&b| is_plain(b)) {
                return None;
            }
            rest = &rest[end..];
        }
        fields.push((key, value));
        match rest.first() {
            None => return Some(fields),
            Some(b' ') => rest = &rest[1..],
            Some(_) => return None,
        }
    }
}

#[test]
fn any_entry_is_one_line_without_raw_control_bytes() {
    for seed in 1..=20_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let entry = rng.entry();
        let mut out = Vec::new();
        format_entry(&entry, &backends(), &mut out);
        assert_eq!(
            out.iter().filter(|&&b| b == b'\n').count(),
            1,
            "seed {seed}: {entry:?}"
        );
        assert_eq!(out.last(), Some(&b'\n'), "seed {seed}");
        assert!(
            out[..out.len() - 1]
                .iter()
                .all(|&b| (0x20..0x7f).contains(&b)),
            "seed {seed}: {:?}",
            String::from_utf8_lossy(&out)
        );
        // The line parses back to the untrusted strings, byte for byte.
        let fields = parse(&out)
            .unwrap_or_else(|| panic!("seed {seed}: {:?}", String::from_utf8_lossy(&out)));
        // The keys of the kind, in the fixed order (FR-LOG-05).
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        let host_key = |r: &RequestSummary| r.host.as_ref().map(|_| "host");
        let expected: Vec<&str> = match &entry {
            Log::Response {
                request: Some(r), ..
            } => ["kind", "conn", "peer", "status", "method"]
                .into_iter()
                .chain(host_key(r))
                .chain(["path"])
                .collect(),
            Log::ProxiedResponse { request: r, .. } => {
                ["kind", "conn", "peer", "backend", "status", "method"]
                    .into_iter()
                    .chain(host_key(r))
                    .chain(["path"])
                    .collect()
            }
            Log::Rejected { .. } => vec!["kind", "conn", "peer", "reason", "method", "path"],
            other @ (Log::Response { request: None, .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::BackendFailed { .. }) => panic!("not generated: {other:?}"),
        };
        assert_eq!(keys, expected, "seed {seed}");
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let (method, host, path) = match &entry {
            Log::Response {
                request: Some(r), ..
            }
            | Log::ProxiedResponse { request: r, .. } => (&r.method, r.host.clone(), &r.path),
            Log::Rejected {
                reason:
                    RejectReason::Http {
                        request: Some(r), ..
                    },
                ..
            } => (&r.method, None, &r.path),
            other @ (Log::Response { .. }
            | Log::Rejected { .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::BackendFailed { .. }) => panic!("not generated: {other:?}"),
        };
        let method = match method {
            Method::Get => "GET".to_owned(),
            Method::Head => "HEAD".to_owned(),
            Method::Other(m) => m.clone(),
        };
        assert_eq!(get("method"), Some(method.into_bytes()), "seed {seed}");
        assert_eq!(get("path"), Some(path.clone().into_bytes()), "seed {seed}");
        assert_eq!(get("host"), host.map(String::into_bytes), "seed {seed}");
        if let Log::ProxiedResponse { backend, .. } = &entry {
            let expected = usize::try_from(backend.0)
                .ok()
                .and_then(|index| backends().get(index).copied())
                .map_or(backend.0.to_string(), |address| address.to_string());
            assert_eq!(get("backend"), Some(expected.into_bytes()), "seed {seed}");
        }
    }
}
