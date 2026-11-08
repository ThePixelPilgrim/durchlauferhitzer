use crate::conn::SEND_CAPACITY;
use crate::core::test_support::{PEER, core};
use crate::core::{ConnError, ConnId, Core, Event, InternalRouteId};
use crate::file::{FileStamp, RootId};
use crate::http::conditional::Validators;
use crate::http::request::Persistence;
use crate::http::response::{
    BodyMode, Part, render_file_head, render_fixed, render_not_modified,
    render_range_not_satisfiable,
};
use crate::time::Instant;
use std::path::PathBuf;

/// The send buffer bound: every fixed response, every file head (200 and
/// 206, FR-STATIC-19), every 416 and every `304` (FR-STATIC-18), in every
/// persistence and body mode, at its largest (longest content type,
/// largest length), with and without its `Date` line (FR-HTTP-22a), fits
/// `SEND_CAPACITY` even behind a queued `100 Continue` (the only bytes a
/// response is ever rendered behind). (Redirects are checked before they
/// are rendered.)
#[test]
fn every_rendered_head_fits_the_send_buffer() {
    use crate::http::date::{MAX_UNIX_SECS, imf_fixdate};
    use crate::http::status::Status as S;
    let statuses = [
        S::Ok,
        S::MovedPermanently,
        S::BadRequest,
        S::NotFound,
        S::MethodNotAllowed,
        S::RequestTimeout,
        S::ContentTooLarge,
        S::UriTooLong,
        S::ExpectationFailed,
        S::RequestHeaderFieldsTooLarge,
        S::BadGateway,
        S::ServiceUnavailable,
        S::GatewayTimeout,
        S::HttpVersionNotSupported,
    ];
    const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
    let date = imf_fixdate(MAX_UNIX_SECS);
    let mut largest = 0;
    for date in [None, Some(&date)] {
        for persistence in [Persistence::KeepAlive, Persistence::Close] {
            for body in [BodyMode::Full, BodyMode::HeadOnly] {
                for status in statuses {
                    let mut out = CONTINUE.to_vec();
                    render_fixed(status, persistence, body, date, &mut out);
                    largest = largest.max(out.len());
                    assert!(out.len() <= SEND_CAPACITY, "{status:?}");
                }
            }
            // Far longer than any content type in the table.
            let content_type = "x".repeat(200);
            // The longest validators: every hex digit used.
            let validators = Validators::of(FileStamp {
                size: u64::MAX,
                modified_ns: u64::MAX,
            });
            // The longest `Content-Range` too (FR-STATIC-19): three 20-digit
            // numbers (and a 19-digit length).
            let range = Part::Range {
                first: 10_000_000_000_000_000_000,
                last: u64::MAX - 1,
                size: u64::MAX,
            };
            for part in [Part::Whole { size: u64::MAX }, range] {
                let mut out = CONTINUE.to_vec();
                render_file_head(
                    &content_type,
                    part,
                    &validators,
                    persistence,
                    date,
                    &mut out,
                );
                largest = largest.max(out.len());
                assert!(out.len() <= SEND_CAPACITY, "{part:?}");
            }
            for body in [BodyMode::Full, BodyMode::HeadOnly] {
                let mut out = CONTINUE.to_vec();
                render_range_not_satisfiable(u64::MAX, persistence, body, date, &mut out);
                largest = largest.max(out.len());
                assert!(out.len() <= SEND_CAPACITY);
            }
            let mut out = CONTINUE.to_vec();
            render_not_modified(&validators, persistence, date, &mut out);
            assert!(out.len() <= SEND_CAPACITY);
        }
    }
    // The arithmetic of NFR-RES-01: well within the 1 KiB buffer.
    assert_eq!(largest, LARGEST_HEAD + CONTINUE.len());
}

/// The largest head the core renders into the send buffer (NFR-RES-01):
/// a `206` with `Date`, a 200-byte content type, the longest
/// `Content-Range`, `ETag` and `Last-Modified`, and `Connection: close`.
const LARGEST_HEAD: usize = 570;

#[test]
fn capacities_are_reported_per_connection() {
    let mut core = core();
    let mut actions = Vec::new();
    let conn = ConnId(1);
    let now = Instant::from_nanos(0);
    assert_eq!(core.recv_capacity(conn), 0);
    assert_eq!(core.send_capacity(conn), 0);
    core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
    assert_eq!(core.recv_capacity(conn), 10);
    assert_eq!(core.send_capacity(conn), 1024);
    core.handle(now, Event::Received { conn, len: 4 }, &mut actions);
    assert_eq!(core.recv_capacity(conn), 10);
    core.handle(now, Event::ReadClosed { conn }, &mut actions);
    assert_eq!(core.recv_capacity(conn), 0);
    assert_eq!(core.send_capacity(conn), 0);
}

#[test]
fn duplicate_accept_and_unknown_events_are_ignored() {
    let mut core = core();
    let mut actions = Vec::new();
    let conn = ConnId(1);
    let now = Instant::from_nanos(0);
    core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
    core.handle(now, Event::Received { conn, len: 3 }, &mut actions);
    actions.clear();
    core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
    assert!(actions.is_empty());
    assert_eq!(core.recv_buffer(conn).len(), 7);
    let other = ConnId(2);
    for event in [
        Event::Received {
            conn: other,
            len: 1,
        },
        Event::Sent {
            conn: other,
            len: 1,
        },
        Event::ReadClosed { conn: other },
        Event::Failed {
            conn: other,
            error: ConnError::Other,
        },
        Event::Sent { conn, len: 99 },
        Event::TimerFired,
    ] {
        core.handle(now, event, &mut actions);
    }
    assert!(actions.is_empty());
    assert!(core.recv_buffer(other).is_empty());
    assert!(core.send_buffer(other).is_empty());
    // Only conn 1 remains, waiting for its head (default 10 s).
    assert_eq!(
        core.next_deadline(),
        Some(now.saturating_add(std::time::Duration::from_secs(10)))
    );
}

#[test]
fn closed_connection_is_forgotten() {
    let mut core = core();
    let mut actions = Vec::new();
    let conn = ConnId(1);
    let now = Instant::from_nanos(0);
    core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
    core.handle(now, Event::ReadClosed { conn }, &mut actions);
    assert!(core.conns.is_empty());
    assert!(core.recv_buffer(conn).is_empty());
    actions.clear();
    core.handle(now, Event::ReadClosed { conn }, &mut actions);
    assert!(actions.is_empty());
}

/// FR-CFG-26: an `internal_files` root is a root the driver opens, listed
/// after the static ones, each path once.
#[test]
fn static_roots_include_internal_roots() {
    let text = "listen 127.0.0.1:80;\n\
        host a.test {\n\
          path /p { proxy 127.0.0.1:3000; max_request_body 1; max_response_body 1;\n\
            internal_files x-sendfile /srv/files; }\n\
          path / { static /srv/www; }\n\
        }\n";
    let core = Core::new(crate::config::load(text).unwrap());
    assert_eq!(
        core.static_roots(),
        [PathBuf::from("/srv/www"), PathBuf::from("/srv/files")]
    );
    // An internal root equal to a static one is listed once.
    let text = text.replace("/srv/files", "/srv/www");
    let core = Core::new(crate::config::load(&text).unwrap());
    assert_eq!(core.static_roots(), [PathBuf::from("/srv/www")]);
}

/// Four routes on `a.test`: `x-accel-redirect`, two identical
/// `x-sendfile`, a static one; then a plain proxy route on `b.test`.
const INTERNAL: &str = "listen 127.0.0.1:80;\n\
    host a.test {\n\
      path /p { proxy 127.0.0.1:3000; max_request_body 1; max_response_body 1;\n\
        internal_files x-accel-redirect /x/ /srv/files; }\n\
      path /q { proxy 127.0.0.1:3000; max_request_body 1; max_response_body 1;\n\
        internal_files x-sendfile /srv/www; }\n\
      path /r { proxy 127.0.0.1:3000; max_request_body 1; max_response_body 1;\n\
        internal_files x-sendfile /srv/www; }\n\
      path / { static /srv/www; }\n\
    }\n\
    host b.test {\n\
      path /p { proxy 127.0.0.1:3000; max_request_body 1; max_response_body 1; }\n\
    }\n";

/// FR-PROXY-32: each route's `internal_files` id is resolved once, by
/// its place in the config; identical settings share one.
#[test]
fn each_route_maps_to_its_internal_files() {
    let core = Core::new(crate::config::load(INTERNAL).unwrap());
    let id = |n| Some(InternalRouteId(n));
    assert_eq!(
        core.route_internal,
        [vec![id(0), id(1), id(1), None], vec![None]]
    );
}

/// FR-PROXY-32: each distinct `internal_files` setting is listed once, in
/// config order, with its root's id in `static_roots`.
#[test]
fn internal_routes_point_at_their_roots() {
    let core = Core::new(crate::config::load(INTERNAL).unwrap());
    let listed: Vec<(PathBuf, RootId)> = core
        .internal_routes
        .iter()
        .map(|r| (r.files.root.clone(), r.root))
        .collect();
    assert_eq!(
        listed,
        [
            (PathBuf::from("/srv/files"), RootId(1)),
            (PathBuf::from("/srv/www"), RootId(0)),
        ]
    );
}
