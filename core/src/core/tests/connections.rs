use super::*;

const CONFIG: &str = "listen 127.0.0.1:8080; \
    limits { max_request_line 8; max_head 8; } \
    host a.test { path / { static /srv; } }";

fn core() -> Core {
    Core::new(crate::config::load(CONFIG).unwrap())
}

/// The send buffer bound: every fixed response and every file head,
/// in every persistence and body mode, at its largest (longest content
/// type, largest length), fits `SEND_CAPACITY` on its own. (Redirects
/// are checked before they are rendered.)
#[test]
fn every_rendered_head_fits_the_send_buffer() {
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
        S::RequestHeaderFieldsTooLarge,
        S::NotImplemented,
        S::ServiceUnavailable,
        S::HttpVersionNotSupported,
    ];
    for persistence in [Persistence::KeepAlive, Persistence::Close] {
        for body in [BodyMode::Full, BodyMode::HeadOnly] {
            for status in statuses {
                let mut out = Vec::new();
                render_fixed(status, persistence, body, &mut out);
                assert!(out.len() <= SEND_CAPACITY, "{status:?}");
            }
        }
        let mut out = Vec::new();
        // Far longer than any content type in the table.
        let content_type = "x".repeat(200);
        render_file_head(&content_type, u64::MAX, persistence, &mut out);
        assert!(out.len() <= SEND_CAPACITY);
    }
}

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
fn oversized_received_is_clamped() {
    let mut core = core();
    let mut actions = Vec::new();
    let conn = ConnId(1);
    let now = Instant::from_nanos(0);
    core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
    assert_eq!(core.recv_buffer(conn).len(), 10);
    actions.clear();
    core.handle(now, Event::Received { conn, len: 1000 }, &mut actions);
    // Clamped to the 10 spare bytes: the full buffer of zeros is an
    // over-long request line, answered at once.
    assert_eq!(
        actions,
        [
            Action::Log(Log::Rejected {
                conn,
                peer: PEER,
                reason: RejectReason::Http(HttpError::RequestLineTooLong),
            }),
            Action::SetInterest {
                conn,
                interest: Interest::Write
            }
        ]
    );
    assert!(core.recv_buffer(conn).is_empty());
    actions.clear();
    core.handle(now, Event::Received { conn, len: 5 }, &mut actions);
    assert!(actions.is_empty());
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
fn limit_counts_return_to_zero_on_every_close_path() {
    let mut core = core();
    let mut actions = Vec::new();
    let now = secs(0);
    let other = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9));
    for (id, peer) in [(1, PEER), (2, PEER), (3, other), (4, other), (5, other)] {
        accept(&mut core, now, id, peer);
    }
    assert_eq!(core.counted, 5);
    assert_eq!(core.counted_per_ip.get(&PEER), Some(&2));
    // conns 4 and 5: a full buffer of zeros is answered 400 at once.
    for id in [4, 5] {
        let conn = ConnId(id);
        core.handle(now, Event::Received { conn, len: 10 }, &mut actions);
    }
    // conn 5's answer is written: it lingers (deadline 2 s); conn 4's is
    // never taken (send deadline 30 s).
    let conn = ConnId(5);
    core.handle(now, Event::Sent { conn, len: 999 }, &mut actions);
    core.handle(now, Event::ReadClosed { conn: ConnId(1) }, &mut actions);
    let error = ConnError::Reset;
    let conn = ConnId(2);
    core.handle(now, Event::Failed { conn, error }, &mut actions);
    assert_eq!(core.counted, 3);
    assert_eq!(core.counted_per_ip.get(&PEER), None);
    // Lingering expiry.
    core.handle(secs(2), Event::TimerFired, &mut actions);
    assert_eq!(core.counted, 2);
    // conn 3 sent nothing: it times out silently at the header deadline.
    core.handle(secs(10), Event::TimerFired, &mut actions);
    assert_eq!(core.counted, 1);
    // Send idle expiry.
    core.handle(secs(30), Event::TimerFired, &mut actions);
    assert_eq!(core.counted, 0);
    assert!(core.counted_per_ip.is_empty());
    assert!(core.conns.is_empty());
    assert_eq!(core.next_deadline(), None);
}

/// One counted connection allowed, so at most one refused one. The long
/// header timeout keeps the counted connection open throughout.
const CAPPED: &str = "listen 127.0.0.1:8080; \
    limits { max_connections 1; header_timeout 60m; } \
    host a.test { path / { static /srv; } }";

fn capped() -> Core {
    Core::new(crate::config::load(CAPPED).unwrap())
}

fn refused(conn: ConnId) -> Action {
    Action::Log(Log::Rejected {
        conn,
        peer: PEER,
        reason: RejectReason::Limit(LimitKind::Connections),
    })
}

#[test]
fn refused_connection_has_no_receive_buffer() {
    let mut core = capped();
    accept(&mut core, secs(0), 1, PEER);
    accept(&mut core, secs(0), 2, PEER);
    let conn = ConnId(2);
    assert!(!core.send_buffer(conn).is_empty(), "a 503 is waiting");
    assert_eq!(core.recv_capacity(conn), 0);
    assert!(core.recv_buffer(conn).is_empty());
    assert_eq!(core.send_capacity(conn), SEND_CAPACITY);
}

#[test]
fn lingering_connections_share_a_scratch_buffer() {
    let mut core = capped();
    let mut actions = Vec::new();
    let now = secs(0);
    accept(&mut core, now, 1, PEER);
    accept(&mut core, now, 2, PEER);
    // conn 1 (counted): no Host header, answered 400, then lingers.
    let c1 = ConnId(1);
    let request = b"GET / HTTP/1.1\r\n\r\n";
    let spare = core.recv_buffer(c1);
    assert!(spare.len() > 4096);
    if let Some(dst) = spare.get_mut(..request.len()) {
        dst.copy_from_slice(request);
    }
    let len = request.len();
    core.handle(now, Event::Received { conn: c1, len }, &mut actions);
    for id in [1, 2] {
        let conn = ConnId(id);
        core.handle(now, Event::Sent { conn, len: 999 }, &mut actions);
        assert!(actions.contains(&Action::ShutdownWrite { conn }));
        assert_eq!(core.recv_capacity(conn), 4096);
        assert_eq!(core.recv_buffer(conn).len(), 4096);
        assert_eq!(core.conns.get(&conn).map(|c| c.recv.len()), Some(0));
        actions.clear();
        core.handle(now, Event::Received { conn, len: 4096 }, &mut actions);
        assert!(actions.is_empty());
    }
}

#[test]
fn refused_connections_beyond_the_cap_are_closed_at_once() {
    let mut core = capped();
    let now = secs(0);
    accept(&mut core, now, 1, PEER);
    let second = accept(&mut core, now, 2, PEER);
    assert!(!second.contains(&Action::Close { conn: ConnId(2) }));
    let conn = ConnId(3);
    assert_eq!(
        accept(&mut core, now, 3, PEER),
        [refused(conn), Action::Close { conn }]
    );
    assert!(!core.conns.contains_key(&conn));
    assert!(core.send_buffer(conn).is_empty());
    assert_eq!(core.recv_capacity(conn), 0);
}

#[test]
fn refused_cap_is_released_on_every_close_path() {
    let mut core = capped();
    let mut actions = Vec::new();
    accept(&mut core, secs(0), 1, PEER);
    let mut id = 2;
    // Each closing path, applied to a refused connection accepted at `t`,
    // frees the single refused slot for the next round.
    for path in 0..4 {
        let t = secs(100 * path);
        let conn = ConnId(id);
        id += 1;
        let accepted = accept(&mut core, t, conn.0, PEER);
        assert!(!accepted.contains(&Action::Close { conn }), "path {path}");
        let over = ConnId(id);
        id += 1;
        assert_eq!(
            accept(&mut core, t, over.0, PEER),
            [refused(over), Action::Close { conn: over }],
            "path {path}"
        );
        actions.clear();
        match path {
            0 => core.handle(t, Event::ReadClosed { conn }, &mut actions),
            1 => {
                let error = ConnError::Reset;
                core.handle(t, Event::Failed { conn, error }, &mut actions);
            }
            2 => {
                core.handle(t, Event::Sent { conn, len: 999 }, &mut actions);
                let linger_end = t.saturating_add(LINGER_TIMEOUT);
                core.handle(linger_end, Event::TimerFired, &mut actions);
            }
            _ => {
                let stalled = t.saturating_add(std::time::Duration::from_secs(30));
                core.handle(stalled, Event::TimerFired, &mut actions);
            }
        }
        assert!(actions.contains(&Action::Close { conn }), "path {path}");
        assert_eq!(core.conns.len(), 1, "path {path}");
    }
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

/// A maximal head sent one line per read costs linear parser work: every
/// byte is searched for an LF once and judged once, and the finished head
/// is built once (regression test for rescanning the whole buffer on
/// every new line).
#[test]
fn head_sent_line_by_line_is_parsed_in_linear_work() {
    use crate::http::request::work;
    let mut core = Core::new(
        crate::config::load("listen 127.0.0.1:8080; host a.test { path / { static /srv; } }")
            .unwrap(),
    );
    let limits = core.config.limits.clone();
    let mut lines: Vec<Vec<u8>> =
        vec![b"GET / HTTP/1.1\r\n".to_vec(), b"Host: a.test\r\n".to_vec()];
    let fixed: usize = lines.iter().map(Vec::len).sum();
    let count = limits.max_headers - 1;
    let width = (limits.max_head - fixed - 2) / count;
    for i in 0..count {
        let mut line = format!("X-Pad-{i:03}: ").into_bytes();
        line.resize(width - 2, b'x');
        line.extend_from_slice(b"\r\n");
        lines.push(line);
    }
    lines.push(b"\r\n".to_vec());
    let total: usize = lines.iter().map(Vec::len).sum();
    assert!(total <= limits.max_head && total > limits.max_head - width);

    let conn = ConnId(1);
    let now = Instant::from_nanos(0);
    let mut actions = Vec::new();
    core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
    work::take();
    for line in &lines {
        let spare = core.recv_buffer(conn);
        spare.get_mut(..line.len()).unwrap().copy_from_slice(line);
        core.handle(
            now,
            Event::Received {
                conn,
                len: line.len(),
            },
            &mut actions,
        );
    }
    let used = work::take();
    // The complete head was answered: `/` is opened as a file.
    assert!(
        actions.iter().any(|a| matches!(a, Action::OpenFile { .. })),
        "{actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::Log(Log::Rejected { .. }))),
        "{actions:?}"
    );
    assert!(used.scanned <= total, "{used:?} for a {total}-byte head");
    assert!(used.judged <= total, "{used:?} for a {total}-byte head");
    assert!(used.built <= total, "{used:?} for a {total}-byte head");
}
