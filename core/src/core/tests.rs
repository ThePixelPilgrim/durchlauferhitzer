use crate::conn::SEND_CAPACITY;
use crate::core::test_support::{PEER, core};
use crate::core::{ConnError, ConnId, Event};
use crate::file::FileStamp;
use crate::http::conditional::Validators;
use crate::http::request::Persistence;
use crate::http::response::{
    BodyMode, Part, render_file_head, render_fixed, render_not_modified,
    render_range_not_satisfiable,
};
use crate::time::Instant;

/// The send buffer bound: every fixed response, every file head (200 and
/// 206, FR-STATIC-19), every 416 and every
/// `304` (FR-STATIC-18), in
/// every persistence and body mode, at its largest (longest content type,
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
