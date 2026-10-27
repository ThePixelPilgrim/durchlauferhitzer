//! Unit tests of proxy routes at the core level: the upstream side of the
//! contract (end-to-end behaviour is tested in `sim/tests/proxy.rs`).

use std::net::{IpAddr, Ipv6Addr};

use crate::conn::ConnState;
use crate::core::test_support::{PEER, accept, files_core, request, secs};
use crate::core::{Action, ConnError, ConnId, Core, Event, Interest};
use crate::log::{BackendFailure, Log};
use crate::upstream::{BackendId, UPSTREAM_RECV_MIN, UpstreamId};

const GET_P: &[u8] = b"GET /p/f HTTP/1.1\r\nHost: b.test\r\n\r\n";

const UP: UpstreamId = UpstreamId(1);
const CONN: ConnId = ConnId(1);

fn handle(core: &mut Core, secs_now: u64, event: Event) -> Vec<Action> {
    let mut actions = Vec::new();
    core.handle(secs(secs_now), event, &mut actions);
    actions
}

/// Upstream 1 connected for conn 1 and its request head written.
fn awaiting_response() -> Core {
    let mut core = files_core();
    request(&mut core, secs(0), 1, GET_P);
    handle(&mut core, 0, Event::UpstreamConnected { up: UP });
    let len = core.upstream_send_buffer(UP).len();
    handle(&mut core, 0, Event::UpstreamSent { up: UP, len });
    core
}

#[test]
fn proxy_request_connects_and_waits() {
    let mut core = files_core();
    let actions = request(&mut core, secs(0), 1, GET_P);
    assert_eq!(
        actions,
        [
            Action::Connect {
                up: UP,
                backend: BackendId(0)
            },
            Action::SetInterest {
                conn: CONN,
                interest: Interest::None
            }
        ]
    );
    // Nothing is offered before the connect completed.
    assert!(core.upstream_send_buffer(UP).is_empty());
    assert!(core.upstream_recv_buffer(UP).is_empty());
    assert!(core.recv_buffer(CONN).is_empty());
    assert_eq!(core.upstreams.len(), 1);
    assert_eq!(core.next_deadline(), Some(secs(5)));
}

#[test]
fn connected_upstream_writes_the_head_then_reads() {
    let mut core = files_core();
    request(&mut core, secs(0), 1, GET_P);
    let actions = handle(&mut core, 1, Event::UpstreamConnected { up: UP });
    assert_eq!(
        actions,
        [Action::SetUpstreamInterest {
            up: UP,
            interest: Interest::Write
        }]
    );
    let head = core.upstream_send_buffer(UP).to_vec();
    assert!(head.starts_with(b"GET /p/f HTTP/1.1\r\nhost: b.test\r\n"));
    assert_eq!(core.upstream_send_capacity(UP), core.head_capacity);
    // A partial write keeps the rest; a length beyond it is clamped.
    assert!(handle(&mut core, 2, Event::UpstreamSent { up: UP, len: 4 }).is_empty());
    assert_eq!(core.upstream_send_buffer(UP), head.get(4..).unwrap());
    let actions = handle(&mut core, 3, Event::UpstreamSent { up: UP, len: 9999 });
    assert_eq!(
        actions,
        [Action::SetUpstreamInterest {
            up: UP,
            interest: Interest::Read
        }]
    );
    assert!(core.upstream_send_buffer(UP).is_empty());
    assert_eq!(core.upstream_recv_buffer(UP).len(), UPSTREAM_RECV_MIN);
    assert_eq!(core.upstream_recv_capacity(UP), UPSTREAM_RECV_MIN);
    // The response timeout runs from the head being fully written.
    assert_eq!(core.next_deadline(), Some(secs(63)));
}

#[test]
fn received_beyond_the_offer_is_clamped() {
    let mut core = awaiting_response();
    // A full buffer (of zeros) without a complete head is a bad head.
    let actions = handle(
        &mut core,
        0,
        Event::UpstreamReceived {
            up: UP,
            len: 1 << 30,
        },
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            Action::Log(Log::BackendFailed {
                kind: BackendFailure::BadResponse(_),
                ..
            })
        )),
        "{actions:?}"
    );
    assert!(actions.contains(&Action::CloseUpstream { up: UP }));
}

/// Review Focus 5: once the client is gone, the upstream is closed exactly
/// once and its late events (and events for ids never handed out) change
/// nothing.
#[test]
fn late_and_stray_upstream_events_are_ignored() {
    let mut core = files_core();
    request(&mut core, secs(0), 1, GET_P);
    let error = ConnError::Reset;
    let actions = handle(&mut core, 0, Event::Failed { conn: CONN, error });
    assert_eq!(
        actions,
        [
            Action::Log(Log::Reset {
                conn: CONN,
                peer: PEER
            }),
            Action::CloseUpstream { up: UP },
            Action::Close { conn: CONN }
        ]
    );
    for up in [UP, UpstreamId(9)] {
        for event in [
            Event::UpstreamConnected { up },
            Event::UpstreamReceived { up, len: 5 },
            Event::UpstreamSent { up, len: 5 },
            Event::UpstreamReadClosed { up },
            Event::UpstreamFailed { up, error },
        ] {
            assert!(handle(&mut core, 1, event).is_empty());
        }
        assert!(core.upstream_recv_buffer(up).is_empty());
        assert!(core.upstream_send_buffer(up).is_empty());
        assert_eq!(core.upstream_recv_capacity(up), 0);
        assert_eq!(core.upstream_body_pending(up), None);
    }
    assert_eq!(core.upstreams.len(), 0);
    assert_eq!(core.next_deadline(), None);
}

/// NFR-ROB-04 (stage 3 final review M3): an end of stream is only learnt
/// from a read, so an `UpstreamReadClosed` while no read was offered (the
/// interest is `None` while connecting, `Write` while the request head is
/// written) is a driver mistake and ignored, like any upstream event that
/// does not fit the state.
#[test]
fn upstream_read_closed_without_a_read_offered_is_ignored() {
    let mut core = files_core();
    request(&mut core, secs(0), 1, GET_P);
    // Connecting: interest `None`.
    assert!(handle(&mut core, 0, Event::UpstreamReadClosed { up: UP }).is_empty());
    handle(&mut core, 0, Event::UpstreamConnected { up: UP });
    // Writing the request head: interest `Write`.
    assert!(handle(&mut core, 0, Event::UpstreamReadClosed { up: UP }).is_empty());
    assert_eq!(core.upstreams.len(), 1);
    // Once a read is offered, the same event is a 502 `ClosedEarly`.
    let len = core.upstream_send_buffer(UP).len();
    handle(&mut core, 0, Event::UpstreamSent { up: UP, len });
    let actions = handle(&mut core, 0, Event::UpstreamReadClosed { up: UP });
    assert_eq!(
        actions.first(),
        Some(&Action::Log(Log::BackendFailed {
            conn: CONN,
            peer: PEER,
            backend: BackendId(0),
            kind: BackendFailure::ClosedEarly,
        }))
    );
}

/// The head buffer's capacity holds the rewrite of any head within the
/// limits, in the worst case of each direction: as many header lines as
/// allowed, each as short as possible (`a:b`, which re-renders one byte
/// longer), an IPv6 peer, and toward the client `connection: close` and
/// the core's `date` for a backend that sent none (FR-HTTP-22a).
#[test]
fn rewritten_heads_at_the_limits_fit_the_head_buffer() {
    let config = "listen 127.0.0.1:8080; limits { max_request_line 256; max_head 512; max_headers 40; } \
        host b.test { path /p { proxy 127.0.0.1:3000; \
        max_request_body 1MiB; max_response_body 1MiB; } }";
    let mut core = Core::new(crate::config::load(config).unwrap());
    core.set_date(crate::http::date::MAX_UNIX_SECS);
    let mut req = b"GET /p HTTP/1.1\r\nHost: b.test\r\nConnection: close\r\n".to_vec();
    for _ in 0..38 {
        req.extend_from_slice(b"a:b\r\n");
    }
    req.extend_from_slice(b"\r\n");
    assert!(req.len() <= 512);
    let peer = IpAddr::V6(Ipv6Addr::new(
        0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xfffe,
    ));
    let mut actions = Vec::new();
    core.handle(secs(0), Event::Accepted { conn: CONN, peer }, &mut actions);
    core.recv_buffer(CONN)
        .get_mut(..req.len())
        .unwrap()
        .copy_from_slice(&req);
    let len = req.len();
    actions.clear();
    core.handle(secs(0), Event::Received { conn: CONN, len }, &mut actions);
    assert!(
        actions.contains(&Action::Connect {
            up: UP,
            backend: BackendId(0)
        }),
        "{actions:?}"
    );
    handle(&mut core, 0, Event::UpstreamConnected { up: UP });
    let sent = core.upstream_send_buffer(UP).len();
    assert!(sent > req.len() + 38, "{sent}");
    handle(&mut core, 0, Event::UpstreamSent { up: UP, len: sent });

    let mut response = b"HTTP/1.1 511 \r\nContent-Length:0\r\n".to_vec();
    for _ in 0..39 {
        response.extend_from_slice(b"a:b\r\n");
    }
    response.extend_from_slice(b"\r\n");
    assert!(response.len() <= 512);
    core.upstream_recv_buffer(UP)
        .get_mut(..response.len())
        .unwrap()
        .copy_from_slice(&response);
    let len = response.len();
    let actions = handle(&mut core, 0, Event::UpstreamReceived { up: UP, len });
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Log(Log::ProxiedResponse { status: 511, .. }))),
        "{actions:?}"
    );
    // Each of the 40 header lines grows by a byte, then `connection:
    // close` (19 bytes) and `date: <IMF-fixdate>` (37) are added.
    let head = core.send_buffer(CONN).len();
    assert_eq!(head, response.len() + 40 + 19 + 37);
    assert!(head <= core.head_capacity, "{head}");
    assert!(
        core.send_buffer(CONN)
            .ends_with(b"date: Fri, 31 Dec 9999 23:59:59 GMT\r\nconnection: close\r\n\r\n")
    );
}

/// While relaying, the connection's own send buffer goes out first (Task
/// 6's `100 Continue` will use it), then the rewritten head, then the body.
#[test]
fn own_send_buffer_goes_out_before_the_relayed_response() {
    let mut core = awaiting_response();
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    core.upstream_recv_buffer(UP)
        .get_mut(..response.len())
        .unwrap()
        .copy_from_slice(response);
    let len = response.len();
    handle(&mut core, 0, Event::UpstreamReceived { up: UP, len });
    let head = core.send_buffer(CONN).to_vec();
    assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"), "{head:?}");
    core.conns
        .get_mut(&CONN)
        .unwrap()
        .send
        .extend_from_slice(b"own");
    assert_eq!(core.send_buffer(CONN), b"own");
    assert_eq!(core.send_capacity(CONN), crate::conn::SEND_CAPACITY);
    // A length beyond the own bytes is clamped to them.
    handle(
        &mut core,
        0,
        Event::Sent {
            conn: CONN,
            len: 99,
        },
    );
    assert_eq!(core.send_buffer(CONN), head.as_slice());
    let len = head.len();
    handle(&mut core, 0, Event::Sent { conn: CONN, len });
    assert_eq!(core.send_buffer(CONN), b"ok");
    let actions = handle(&mut core, 0, Event::Sent { conn: CONN, len: 2 });
    assert!(core.send_buffer(CONN).is_empty(), "{actions:?}");
    assert!(
        !core.recv_buffer(CONN).is_empty(),
        "back to reading: {actions:?}"
    );
}

/// I1: `worst_case_per_connection` (the config's memory budget) equals the
/// buffers a real `Core` allocates, for the defaults and for limits that
/// make every term matter. A change to a core buffer constant or formula
/// that the budget does not follow fails here.
#[test]
fn worst_case_per_connection_equals_the_buffers_the_core_allocates() {
    use crate::config::{load, worst_case_per_connection};
    // The budget and the core share their capacity functions, so equality
    // alone follows any constant. The defaults are therefore also pinned to
    // the NFR-RES-01 numbers (also in the example config): changing a core
    // buffer constant must be a deliberate spec change.
    let defaults = crate::config::Limits::default();
    assert_eq!(worst_case_per_connection(&defaults, false), 17_408);
    assert_eq!(worst_case_per_connection(&defaults, true), 50_404);
    let get = b"GET /p/f HTTP/1.1\r\nHost: b.test\r\n\r\n";
    for limits in [
        "",
        "limits { max_head 40KiB; max_request_line 30KiB; }",
        "limits { max_head 2KiB; max_request_line 1KiB; max_headers 7; }",
    ] {
        let stat =
            format!("listen 127.0.0.1:8080; {limits} host a.test {{ path / {{ static /srv; }} }}");
        let proxy = format!(
            "listen 127.0.0.1:8080; {limits} host b.test {{ path /p {{ proxy 127.0.0.1:9; \
             max_request_body 1MiB; max_response_body 1MiB; }} }}"
        );

        // Static: a fresh connection's receive and send buffers, as
        // allocated (not as reported by the capacity accessors, which
        // would echo the same constants).
        let config = load(&stat).unwrap();
        let limits = config.limits.clone();
        let mut core = Core::new(config);
        accept(&mut core, secs(0), 1, PEER);
        let c = core.conns.get(&CONN).unwrap();
        let held = c.recv.len() + c.send.capacity();
        assert_eq!(worst_case_per_connection(&limits, false), held as u64);

        // Proxy: the same plus a connected exchange's upstream receive and
        // send ("head") buffers.
        let mut core = Core::new(load(&proxy).unwrap());
        request(&mut core, secs(0), 1, get);
        handle(&mut core, 0, Event::UpstreamConnected { up: UP });
        let c = core.conns.get(&CONN).unwrap();
        let ConnState::AwaitingResponse { buffers, .. } = &c.state else {
            panic!("expected AwaitingResponse, got {:?}", c.state);
        };
        let held = c.recv.len() + c.send.capacity() + buffers.recv.len() + buffers.head.capacity();
        assert_eq!(worst_case_per_connection(&limits, true), held as u64);
    }
}
