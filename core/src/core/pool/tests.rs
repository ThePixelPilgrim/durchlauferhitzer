//! Unit tests of backend connection pooling and the safe retry
//! (FR-PROXY-02, FR-PROXY-26): reuse, the rules that end it, idle hygiene,
//! the retry rule, drains and budgets.

use std::net::IpAddr;

use crate::conn::LINGER_SCRATCH;
use crate::core::test_support::{PEER, accept, secs};
use crate::core::{Action, ConnError, ConnId, Core, DrainTarget, Event, Interest};
use crate::log::{BackendFailure, Log};
use crate::upstream::{BackendId, UpstreamHolder, UpstreamId};

/// Two routes to one backend (one pooled, one not), one to another; at
/// most 2 idle connections per backend for 2 s, at most 4 connections.
const POOLED: &str = "listen 127.0.0.1:8080; \
    limits { backend_idle_timeout 2s; backend_idle_max 2; max_connections 4; } \
    host b.test { \
    path /p { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body unlimited; } \
    path /q { proxy 127.0.0.1:3000; pool off; max_request_body 1MiB; \
    max_response_body unlimited; } \
    path /r { proxy 127.0.0.1:3001; max_request_body 1MiB; max_response_body unlimited; } }";

fn pooled_core() -> Core {
    Core::new(crate::config::load(POOLED).unwrap())
}

const GET: &[u8] = b"GET /p/f HTTP/1.1\r\nHost: b.test\r\n\r\n";
const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
const RELAYED: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

fn handle(core: &mut Core, t: u64, event: Event) -> Vec<Action> {
    let mut actions = Vec::new();
    core.handle(secs(t), event, &mut actions);
    actions
}

/// The client of `conn` sends `bytes` (one read).
fn client_sends(core: &mut Core, t: u64, conn: ConnId, bytes: &[u8]) -> Vec<Action> {
    let spare = core.recv_buffer(conn);
    spare.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    handle(
        core,
        t,
        Event::Received {
            conn,
            len: bytes.len(),
        },
    )
}

/// The backend of `up` sends `bytes` (one read).
fn backend_sends(core: &mut Core, t: u64, up: UpstreamId, bytes: &[u8]) -> Vec<Action> {
    let spare = core.upstream_recv_buffer(up);
    assert!(spare.len() >= bytes.len(), "no room to read {up:?}");
    spare.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    handle(
        core,
        t,
        Event::UpstreamReceived {
            up,
            len: bytes.len(),
        },
    )
}

/// The backend takes everything the core has for `up`; returns it.
fn backend_takes(core: &mut Core, t: u64, up: UpstreamId) -> Vec<u8> {
    let mut got = Vec::new();
    loop {
        let bytes = core.upstream_send_buffer(up).to_vec();
        if bytes.is_empty() {
            return got;
        }
        got.extend_from_slice(&bytes);
        handle(
            core,
            t,
            Event::UpstreamSent {
                up,
                len: bytes.len(),
            },
        );
    }
}

/// The client takes everything the core has for `conn`; returns it.
fn client_takes(core: &mut Core, t: u64, conn: ConnId) -> Vec<u8> {
    let mut got = Vec::new();
    loop {
        let bytes = core.send_buffer(conn).to_vec();
        if bytes.is_empty() {
            return got;
        }
        got.extend_from_slice(&bytes);
        handle(
            core,
            t,
            Event::Sent {
                conn,
                len: bytes.len(),
            },
        );
    }
}

fn connects(actions: &[Action]) -> Vec<UpstreamId> {
    actions
        .iter()
        .filter_map(|a| {
            if let Action::Connect { up, .. } = a {
                Some(*up)
            } else {
                None
            }
        })
        .collect()
}

fn closes(actions: &[Action]) -> Vec<UpstreamId> {
    actions
        .iter()
        .filter_map(|a| {
            if let Action::CloseUpstream { up } = a {
                Some(*up)
            } else {
                None
            }
        })
        .collect()
}

/// `conn` sends `request`; the upstream it gets (a fresh one connected
/// at once, or a reused one) takes the request; returns the upstream,
/// whether it was fresh, and what the backend got.
fn forward(core: &mut Core, t: u64, conn: ConnId, request: &[u8]) -> (UpstreamId, bool, Vec<u8>) {
    let actions = client_sends(core, t, conn, request);
    let fresh = connects(&actions);
    let up = match fresh.as_slice() {
        [up] => {
            handle(core, t, Event::UpstreamConnected { up: *up });
            *up
        }
        [] => match core.conns.get(&conn).and_then(|c| c.state.upstream()) {
            Some(up) => up,
            None => panic!("no upstream for {conn:?}: {actions:?}"),
        },
        more => panic!("several connects {more:?}"),
    };
    let got = backend_takes(core, t, up);
    (up, !fresh.is_empty(), got)
}

/// A whole exchange: `request` forwarded, `response` relayed and taken.
/// Returns the upstream, whether it was fresh, the actions of the
/// response's read and what the client got.
fn exchange(
    core: &mut Core,
    t: u64,
    conn: ConnId,
    request: &[u8],
    response: &[u8],
) -> (UpstreamId, bool, Vec<Action>, Vec<u8>) {
    let (up, fresh, _) = forward(core, t, conn, request);
    let actions = backend_sends(core, t, up, response);
    let got = client_takes(core, t, conn);
    (up, fresh, actions, got)
}

fn conn(core: &mut Core, id: u64) -> ConnId {
    accept(core, secs(0), id, PEER);
    ConnId(id)
}

#[test]
fn a_second_request_reuses_the_idle_upstream() {
    let mut core = pooled_core();
    let c = conn(&mut core, 1);
    let (up, fresh, got) = forward(&mut core, 0, c, GET);
    assert!(fresh);
    // FR-PROXY-02, -11: no `Connection: close` toward a pooled backend.
    assert_eq!(
        got,
        b"GET /p/f HTTP/1.1\r\nhost: b.test\r\nx-forwarded-for: 127.0.0.1\r\n\
          x-forwarded-proto: http\r\n\r\n"
    );
    let actions = backend_sends(&mut core, 0, up, OK);
    assert!(closes(&actions).is_empty(), "{actions:?}");
    // The complete response parks the upstream at once, reading (any
    // byte while idle closes it) into the shared scratch buffer: no
    // buffer of its own.
    assert_eq!(core.upstream_holder(up), Some(UpstreamHolder::Pool));
    assert_eq!(core.upstream_recv_buffer(up).len(), LINGER_SCRATCH);
    assert!(core.upstream_send_buffer(up).is_empty());
    assert_eq!(client_takes(&mut core, 0, c), RELAYED);
    let actions = client_sends(&mut core, 1, c, GET);
    assert!(connects(&actions).is_empty(), "{actions:?}");
    assert!(
        actions.contains(&Action::SetUpstreamInterest {
            up,
            interest: Interest::Write
        }),
        "{actions:?}"
    );
    assert_eq!(core.upstream_holder(up), Some(UpstreamHolder::Conn(c)));
    assert!(backend_takes(&mut core, 1, up).starts_with(b"GET /p/f HTTP/1.1\r\n"));
    backend_sends(&mut core, 1, up, OK);
    assert_eq!(client_takes(&mut core, 1, c), RELAYED);
    assert_eq!(core.upstreams.len(), 1);
}

#[test]
fn another_client_reuses_it() {
    let mut core = pooled_core();
    let a = conn(&mut core, 1);
    let b = conn(&mut core, 2);
    let (up, fresh, _, got) = exchange(&mut core, 0, a, GET, OK);
    assert!(fresh);
    assert_eq!(got, RELAYED);
    let (again, fresh, _, got) = exchange(&mut core, 0, b, GET, OK);
    assert_eq!((again, fresh), (up, false));
    assert_eq!(got, RELAYED);
    // A request to another backend never takes it.
    let (other, fresh, _, _) = exchange(
        &mut core,
        0,
        a,
        b"GET /r HTTP/1.1\r\nHost: b.test\r\n\r\n",
        OK,
    );
    assert!(fresh && other != up);
}

/// A backend response that says close, is HTTP/1.0 or ends with the
/// connection ends reuse.
#[test]
fn connection_close_from_the_backend_prevents_reuse() {
    for response in [
        &b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok"[..],
        b"HTTP/1.1 200 OK\r\nConnection: x, Close\r\nContent-Length: 2\r\n\r\nok",
        b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok",
        b"HTTP/1.0 200 OK\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\nok",
    ] {
        let mut core = pooled_core();
        let c = conn(&mut core, 1);
        let (up, _, actions, _) = exchange(&mut core, 0, c, GET, response);
        assert_eq!(closes(&actions), [up], "{actions:?}");
        let (next, fresh, _) = forward(&mut core, 0, c, GET);
        assert!(fresh && next != up);
    }
}

#[test]
fn unread_bytes_or_an_unfinished_chunked_body_prevent_reuse() {
    // Bytes after the body's end, in the same read.
    let mut core = pooled_core();
    let c = conn(&mut core, 1);
    let mut extra = OK.to_vec();
    extra.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    let (up, _, actions, _) = exchange(&mut core, 0, c, GET, &extra);
    assert_eq!(closes(&actions), [up], "{actions:?}");
    // A response to HEAD with bytes after its head.
    let (up, _, actions, _) = exchange(
        &mut core,
        0,
        c,
        b"HEAD /p HTTP/1.1\r\nHost: b.test\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    );
    assert_eq!(closes(&actions), [up], "{actions:?}");
    // A chunked body not yet finished keeps its upstream: another client
    // connects afresh.
    let mut core = pooled_core();
    let a = conn(&mut core, 1);
    let b = conn(&mut core, 2);
    let (up, _, _) = forward(&mut core, 0, a, GET);
    backend_sends(
        &mut core,
        0,
        up,
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n",
    );
    client_takes(&mut core, 0, a);
    assert_eq!(core.upstream_holder(up), Some(UpstreamHolder::Conn(a)));
    let (other, fresh, _) = forward(&mut core, 0, b, GET);
    assert!(fresh && other != up);
    // Its end parks it.
    let actions = backend_sends(&mut core, 0, up, b"0\r\n\r\n");
    assert!(closes(&actions).is_empty(), "{actions:?}");
    assert_eq!(core.upstream_holder(up), Some(UpstreamHolder::Pool));
    client_takes(&mut core, 0, a);
    // A close-delimited body never does.
    let (up, _, _) = forward(&mut core, 0, a, b"GET /r HTTP/1.1\r\nHost: b.test\r\n\r\n");
    backend_sends(&mut core, 0, up, b"HTTP/1.1 200 OK\r\n\r\nok");
    client_takes(&mut core, 0, a);
    let actions = handle(&mut core, 0, Event::UpstreamReadClosed { up });
    assert_eq!(closes(&actions), [up], "{actions:?}");
}

/// Owner decision 2026-11-07 (FR-PROXY-02): never pooled after a
/// response to HEAD, or a 204 or 304: a buggy backend may send a body
/// anyway. The review's scenario: A's HEAD is answered head-only, B's GET
/// must not get the late `evil` response.
#[test]
fn no_pooling_after_head_204_or_304() {
    for (request, response) in [
        (
            &b"HEAD /p HTTP/1.1\r\nHost: b.test\r\n\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n"[..],
        ),
        (GET, b"HTTP/1.1 204 No Content\r\n\r\n"),
        (
            GET,
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 4\r\n\r\n",
        ),
    ] {
        let mut core = pooled_core();
        let a = conn(&mut core, 1);
        let b = conn(&mut core, 2);
        let (up, _, actions, _) = exchange(&mut core, 0, a, request, response);
        assert_eq!(closes(&actions), [up], "{response:?}: {actions:?}");
        let (next, fresh, _) = forward(&mut core, 0, b, GET);
        assert!(fresh && next != up);
        // The late bytes on the old connection go nowhere.
        assert!(core.upstream_recv_buffer(up).is_empty());
        backend_sends(&mut core, 0, next, OK);
        assert_eq!(client_takes(&mut core, 0, b), RELAYED);
    }
    // `Content-Length: 0` is a framed, empty body: pooled.
    let mut core = pooled_core();
    let a = conn(&mut core, 1);
    let (up, _, _, _) = exchange(
        &mut core,
        0,
        a,
        GET,
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(core.upstream_holder(up), Some(UpstreamHolder::Pool));
}

/// A response the backend sent before the whole request body was
/// written leaves the backend with a partial request: no reuse.
#[test]
fn an_early_response_prevents_reuse() {
    let mut core = pooled_core();
    let c = conn(&mut core, 1);
    let actions = client_sends(
        &mut core,
        0,
        c,
        b"POST /p HTTP/1.1\r\nHost: b.test\r\nContent-Length: 4\r\n\r\nab",
    );
    let up = connects(&actions)[0];
    handle(&mut core, 0, Event::UpstreamConnected { up });
    backend_takes(&mut core, 0, up);
    let actions = backend_sends(&mut core, 0, up, OK);
    assert_eq!(closes(&actions), [up], "{actions:?}");
}

#[test]
fn the_idle_timeout_and_the_per_backend_cap_close_connections() {
    let mut core = pooled_core();
    let cs: Vec<ConnId> = (1..=3).map(|i| conn(&mut core, i)).collect();
    // Three exchanges in flight at once, then their responses: the third
    // to park evicts the oldest idle one (at most 2 per backend).
    let ups: Vec<UpstreamId> = cs
        .iter()
        .map(|&c| forward(&mut core, 0, c, GET).0)
        .collect();
    assert!(closes(&backend_sends(&mut core, 0, ups[0], OK)).is_empty());
    assert!(closes(&backend_sends(&mut core, 1, ups[1], OK)).is_empty());
    let actions = backend_sends(&mut core, 1, ups[2], OK);
    assert_eq!(closes(&actions), [ups[0]], "{actions:?}");
    // LIFO: the newest is reused first.
    for &c in &cs {
        client_takes(&mut core, 1, c);
    }
    let (up, fresh, _) = forward(&mut core, 1, cs[0], GET);
    assert_eq!((up, fresh), (ups[2], false));
    // The idle deadline is one of the core's deadlines; at it the idle
    // connection closes.
    assert_eq!(core.upstream_holder(ups[1]), Some(UpstreamHolder::Pool));
    assert!(core.next_deadline() <= Some(secs(3)));
    assert!(closes(&handle(&mut core, 2, Event::TimerFired)).is_empty());
    let actions = handle(&mut core, 3, Event::TimerFired);
    assert_eq!(closes(&actions), [ups[1]], "{actions:?}");
    assert_eq!(core.upstream_holder(ups[1]), None);
}

/// Review Focus 1: a byte, an end of stream or an error while idle closes
/// the connection; it is never reused.
#[test]
fn a_byte_eof_or_error_while_idle_closes_it() {
    let events: [fn(UpstreamId) -> Event; 3] = [
        |up| Event::UpstreamReceived { up, len: 1 },
        |up| Event::UpstreamReadClosed { up },
        |up| Event::UpstreamFailed {
            up,
            error: ConnError::Reset,
        },
    ];
    for event in events {
        let mut core = pooled_core();
        let c = conn(&mut core, 1);
        let (up, _, _, _) = exchange(&mut core, 0, c, GET, OK);
        assert_eq!(core.upstream_holder(up), Some(UpstreamHolder::Pool));
        let actions = handle(&mut core, 0, event(up));
        assert_eq!(actions, [Action::CloseUpstream { up }]);
        let (next, fresh, _) = forward(&mut core, 0, c, GET);
        assert!(fresh && next != up);
    }
}

fn retried(actions: &[Action]) -> Vec<BackendFailure> {
    actions
        .iter()
        .filter_map(|a| {
            if let Action::Log(Log::BackendRetried { kind, .. }) = a {
                Some(*kind)
            } else {
                None
            }
        })
        .collect()
}

fn failed(actions: &[Action]) -> Vec<BackendFailure> {
    actions
        .iter()
        .filter_map(|a| {
            if let Action::Log(Log::BackendFailed { kind, .. }) = a {
                Some(*kind)
            } else {
                None
            }
        })
        .collect()
}

/// A pooled upstream for conn 1 of a fresh core, idle after one exchange.
fn with_idle() -> (Core, ConnId, UpstreamId) {
    let mut core = pooled_core();
    let c = conn(&mut core, 1);
    let (up, _, _, _) = exchange(&mut core, 0, c, GET, OK);
    (core, c, up)
}

#[test]
fn a_get_on_a_dead_reused_connection_is_retried_once_and_succeeds() {
    for kill in [
        Event::UpstreamReadClosed { up: UpstreamId(1) },
        Event::UpstreamFailed {
            up: UpstreamId(1),
            error: ConnError::Reset,
        },
    ] {
        let (mut core, c, up) = with_idle();
        let (again, fresh, _) = forward(&mut core, 1, c, GET);
        assert_eq!((again, fresh), (up, false));
        let actions = handle(&mut core, 1, kill.clone());
        let expected = if matches!(kill, Event::UpstreamReadClosed { .. }) {
            BackendFailure::ClosedEarly
        } else {
            BackendFailure::Reset
        };
        assert_eq!(retried(&actions), [expected], "{actions:?}");
        assert_eq!(
            actions.first(),
            Some(&Action::Log(Log::BackendRetried {
                conn: c,
                peer: PEER,
                backend: BackendId(0),
                kind: expected,
            }))
        );
        assert_eq!(closes(&actions), [up]);
        let fresh = connects(&actions);
        assert_eq!(fresh, [UpstreamId(2)], "{actions:?}");
        assert!(failed(&actions).is_empty());
        // The replay is the whole head again, on the fresh connection.
        handle(&mut core, 1, Event::UpstreamConnected { up: fresh[0] });
        assert!(backend_takes(&mut core, 1, fresh[0]).starts_with(b"GET /p/f HTTP/1.1\r\n"));
        backend_sends(&mut core, 1, fresh[0], OK);
        assert_eq!(client_takes(&mut core, 1, c), RELAYED);
    }
}

/// The replay goes out on a fresh connection, which is never retried.
#[test]
fn a_retried_request_that_fails_again_is_a_502() {
    let (mut core, c, up) = with_idle();
    forward(&mut core, 1, c, GET);
    let actions = handle(&mut core, 1, Event::UpstreamReadClosed { up });
    let next = connects(&actions)[0];
    handle(&mut core, 1, Event::UpstreamConnected { up: next });
    backend_takes(&mut core, 1, next);
    let actions = handle(&mut core, 1, Event::UpstreamReadClosed { up: next });
    assert!(retried(&actions).is_empty(), "{actions:?}");
    assert!(connects(&actions).is_empty());
    assert_eq!(failed(&actions), [BackendFailure::ClosedEarly]);
    assert!(client_takes(&mut core, 1, c).starts_with(b"HTTP/1.1 502 "));
}

/// Only a GET or HEAD without a body is replayed: a POST (with or
/// without a body) and a GET with a body get the 502 of FR-PROXY-26.
#[test]
fn a_post_on_a_dead_reused_connection_gives_502() {
    for request in [
        &b"POST /p HTTP/1.1\r\nHost: b.test\r\nContent-Length: 2\r\n\r\nab"[..],
        b"POST /p HTTP/1.1\r\nHost: b.test\r\n\r\n",
        b"GET /p HTTP/1.1\r\nHost: b.test\r\nContent-Length: 2\r\n\r\nab",
        b"DELETE /p HTTP/1.1\r\nHost: b.test\r\n\r\n",
    ] {
        let (mut core, c, up) = with_idle();
        let (again, fresh, _) = forward(&mut core, 1, c, request);
        assert_eq!((again, fresh), (up, false));
        let actions = handle(&mut core, 1, Event::UpstreamReadClosed { up });
        assert!(retried(&actions).is_empty(), "{actions:?}");
        assert!(connects(&actions).is_empty(), "{actions:?}");
        assert_eq!(failed(&actions), [BackendFailure::ClosedEarly]);
        assert!(client_takes(&mut core, 1, c).starts_with(b"HTTP/1.1 502 "));
    }
}

#[test]
fn a_failure_after_a_response_byte_is_not_retried() {
    for bytes in [&b"HTT"[..], b"HTTP/1.1 100 Continue\r\n\r\n"] {
        let (mut core, c, up) = with_idle();
        forward(&mut core, 1, c, GET);
        backend_sends(&mut core, 1, up, bytes);
        let actions = handle(
            &mut core,
            1,
            Event::UpstreamFailed {
                up,
                error: ConnError::Reset,
            },
        );
        assert!(retried(&actions).is_empty(), "{actions:?}");
        assert!(connects(&actions).is_empty());
        assert_eq!(failed(&actions), [BackendFailure::Reset]);
    }
}

/// A response timeout on a reused connection is no reset: a 504, no
/// retry.
#[test]
fn a_timeout_on_a_reused_connection_is_not_retried() {
    let (mut core, c, up) = with_idle();
    forward(&mut core, 1, c, GET);
    let actions = handle(&mut core, 100, Event::TimerFired);
    assert!(retried(&actions).is_empty(), "{actions:?}");
    assert_eq!(failed(&actions), [BackendFailure::ResponseTimeout]);
    assert_eq!(closes(&actions), [up]);
}

#[test]
fn pool_off_never_reuses() {
    let mut core = pooled_core();
    let c = conn(&mut core, 1);
    let q = b"GET /q HTTP/1.1\r\nHost: b.test\r\n\r\n";
    let (up, _, got) = forward(&mut core, 0, c, q);
    assert!(got.ends_with(b"connection: close\r\n\r\n"), "{got:?}");
    let actions = backend_sends(&mut core, 0, up, OK);
    assert_eq!(closes(&actions), [up]);
    client_takes(&mut core, 0, c);
    // Nor does it take a pooled connection to the same backend.
    let (pooled, _, _, _) = exchange(&mut core, 0, c, GET, OK);
    let (next, fresh, _) = forward(&mut core, 0, c, q);
    assert!(fresh && next != pooled);
    assert_eq!(core.upstream_holder(pooled), Some(UpstreamHolder::Pool));
}

/// FR-PROXY-04 mirrors HTTP/1.0 toward the backend: such requests say
/// close and are not pooled.
#[test]
fn http10_clients_are_not_pooled() {
    let (mut core, _, idle) = with_idle();
    let c = conn(&mut core, 2);
    let (up, fresh, got) = forward(&mut core, 0, c, b"GET /p HTTP/1.0\r\nHost: b.test\r\n\r\n");
    assert!(fresh && up != idle);
    assert!(got.starts_with(b"GET /p HTTP/1.0\r\n"));
    assert!(got.ends_with(b"connection: close\r\n\r\n"), "{got:?}");
    let actions = backend_sends(&mut core, 0, up, OK);
    assert_eq!(closes(&actions), [up]);
}

/// Review Focus 5: either drain closes the idle pool at once, and nothing
/// is parked or reused afterwards.
#[test]
fn drains_close_the_pool() {
    for target in [DrainTarget::Shutdown, DrainTarget::Restart] {
        let (mut core, c, up) = with_idle();
        let b = conn(&mut core, 2);
        let (busy, _, _) = forward(&mut core, 0, b, b"GET /r HTTP/1.1\r\nHost: b.test\r\n\r\n");
        let mut actions = Vec::new();
        core.begin_drain(secs(0), target, &mut actions);
        assert_eq!(closes(&actions), [up], "{target:?}: {actions:?}");
        // The exchange in flight finishes; its upstream closes after it.
        let actions = backend_sends(&mut core, 0, busy, OK);
        assert_eq!(closes(&actions), [busy], "{target:?}: {actions:?}");
        assert!(client_takes(&mut core, 0, b).starts_with(b"HTTP/1.1 200 OK\r\n"));
        // In a restart a new request connects afresh.
        if target == DrainTarget::Restart {
            let (next, fresh, _) = forward(&mut core, 0, c, GET);
            assert!(fresh && next != up);
        }
    }
    let (mut core, _, up) = with_idle();
    let mut actions = Vec::new();
    core.abort_all(&mut actions);
    assert_eq!(closes(&actions), [up]);
    assert!(core.is_drained());
}

/// Every upstream, active or idle, counts against `max_connections`: a
/// new connect beyond it first closes the oldest idle one, so the
/// descriptor budget's bound (one upstream per counted connection) holds.
#[test]
fn idle_upstreams_count_against_max_connections() {
    let mut core = pooled_core();
    let cs: Vec<ConnId> = (1..=4).map(|i| conn(&mut core, i)).collect();
    // Two idle to backend 0, two idle to backend 1: four in all.
    let to_p: Vec<UpstreamId> = cs[..2]
        .iter()
        .map(|&c| forward(&mut core, 0, c, GET).0)
        .collect();
    let r = b"GET /r HTTP/1.1\r\nHost: b.test\r\n\r\n";
    let to_r: Vec<UpstreamId> = cs[2..]
        .iter()
        .map(|&c| forward(&mut core, 0, c, r).0)
        .collect();
    for (i, &up) in to_p.iter().chain(&to_r).enumerate() {
        backend_sends(&mut core, i as u64, up, OK);
    }
    for &c in &cs {
        client_takes(&mut core, 4, c);
    }
    assert_eq!(core.upstreams.len(), 4);
    // A `pool off` request needs a fifth: the oldest idle one goes first.
    let actions = client_sends(
        &mut core,
        4,
        cs[0],
        b"GET /q HTTP/1.1\r\nHost: b.test\r\n\r\n",
    );
    assert_eq!(closes(&actions), [to_p[0]], "{actions:?}");
    assert_eq!(connects(&actions).len(), 1);
    let close_at = actions
        .iter()
        .position(|a| matches!(a, Action::CloseUpstream { .. }));
    let connect_at = actions
        .iter()
        .position(|a| matches!(a, Action::Connect { .. }));
    assert!(close_at < connect_at, "{actions:?}");
    assert_eq!(core.upstreams.len(), 4);
}

/// The idle pool needs no client: it is held across connections and
/// closes on its own timer once every client is gone.
#[test]
fn the_pool_outlives_its_clients() {
    let (mut core, c, up) = with_idle();
    let actions = handle(&mut core, 0, Event::ReadClosed { conn: c });
    assert!(closes(&actions).is_empty(), "{actions:?}");
    assert!(!core.is_drained());
    assert_eq!(core.next_deadline(), Some(secs(2)));
    let actions = handle(&mut core, 2, Event::TimerFired);
    assert_eq!(actions, [Action::CloseUpstream { up }]);
    assert!(core.is_drained());
    assert_eq!(core.next_deadline(), None);
}

/// A peer other than the one that opened the connection may reuse it
/// (owner decision 2026-11-06: shared across clients); the forwarded
/// identity is always the current client's.
#[test]
fn the_reused_request_carries_its_own_client() {
    let (mut core, _, up) = with_idle();
    let other: IpAddr = "192.0.2.9".parse().unwrap();
    accept(&mut core, secs(0), 5, other);
    let (again, fresh, got) = forward(&mut core, 0, ConnId(5), GET);
    assert_eq!((again, fresh), (up, false));
    assert!(
        got.windows(28)
            .any(|w| w == b"x-forwarded-for: 192.0.2.9\r\n"),
        "{got:?}"
    );
}
