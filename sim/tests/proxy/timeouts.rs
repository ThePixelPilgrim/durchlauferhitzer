//! The proxy timeouts as `limits` keys (FR-CFG-20, FR-PROXY-29): each is
//! set to a non-default value here, so the tests prove the configured
//! value is the one used.

use std::time::Duration;

use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{BackendFailure, BackendId, Log, RejectReason, TimeoutKind};
use durchlauferhitzer_sim::{ConnectBehaviour, Sim, SimConnState, Window};

use super::{BACKEND, GET_X, assert_504, fixed, only_upstream, peer, request, summary};

/// Every backend timeout off its default (5 s, 60 s, 30 s), and each
/// distinct from the others, so a key read in place of another fails.
const TIMEOUTS: &str = "listen 127.0.0.1:8080;
limits { backend_connect_timeout 2s; backend_response_timeout 3s; body_idle_timeout 4s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

const POST_10: &[u8] = b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 10\r\n\r\n";

/// `backend_connect_timeout 2s`: a hung connect is a 504 after exactly
/// 2 s.
#[test]
fn configured_connect_timeout_is_504() {
    let mut sim = Sim::new(TIMEOUTS);
    sim.set_connect(BACKEND, ConnectBehaviour::Hang);
    let c = request(&mut sim, GET_X);
    sim.advance(Duration::from_millis(1999));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(
            Status::GatewayTimeout,
            Persistence::KeepAlive,
            BodyMode::Full
        )
    );
    assert_504(&sim, c, BackendFailure::ConnectTimeout);
}

/// `backend_response_timeout 3s`: a backend that reads the request and
/// never answers is a 504 after exactly 3 s.
#[test]
fn configured_response_timeout_is_504() {
    let mut sim = Sim::new(TIMEOUTS);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    assert!(!sim.take_backend_request(up).is_empty());
    sim.advance(Duration::from_millis(2999));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(
            Status::GatewayTimeout,
            Persistence::KeepAlive,
            BodyMode::Full
        )
    );
    assert_504(&sim, c, BackendFailure::ResponseTimeout);
}

/// `body_idle_timeout 4s`, client → backend, the client stalls: 408 after
/// exactly 4 s without a body byte.
#[test]
fn configured_body_idle_timeout_cuts_a_stalled_upload() {
    let mut sim = Sim::new(TIMEOUTS);
    let c = sim.connect(peer());
    sim.send(c, POST_10);
    sim.send(c, b"012");
    sim.run_until_idle();
    sim.advance(Duration::from_millis(3999));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(Status::RequestTimeout, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::RequestBody),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// `body_idle_timeout 4s`, client → backend, the backend stops taking the
/// body: 504 after exactly 4 s.
#[test]
fn configured_body_idle_timeout_cuts_a_backend_not_reading() {
    let mut sim = Sim::new(TIMEOUTS);
    // The backend takes the head and three body bytes, then stalls.
    let head = b"POST /x HTTP/1.1\r\nhost: example.test\r\ncontent-length: 10\r\n\
        x-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n";
    sim.set_default_backend_window(Window::Budget(head.len() + 3));
    let c = sim.connect(peer());
    sim.send(c, POST_10);
    sim.send(c, b"0123456789");
    sim.run_until_idle();
    let up = only_upstream(&sim);
    let mut expected = head.to_vec();
    expected.extend_from_slice(b"012");
    assert_eq!(sim.take_backend_request(up), expected, "{}", sim.trace());
    sim.advance(Duration::from_millis(3999));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.logs(),
        &[
            Log::BackendFailed {
                conn: c,
                peer: peer(),
                backend: BackendId(0),
                kind: BackendFailure::BodyTimeout,
            },
            Log::Response {
                conn: c,
                peer: peer(),
                status: Status::GatewayTimeout,
                request: Some(summary(Method::Other("POST".to_owned()), "/api/x")),
            },
        ],
        "{}",
        sim.trace()
    );
}

/// `body_idle_timeout 4s`, backend → client: a response body that stalls
/// after its head cuts both after exactly 4 s.
#[test]
fn configured_body_idle_timeout_cuts_a_stalled_response() {
    let mut sim = Sim::new(TIMEOUTS);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhal");
    sim.run_until_idle();
    assert!(!sim.take_received(c).is_empty());
    sim.advance(Duration::from_millis(3999));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::BackendFailed {
            conn: c,
            peer: peer(),
            backend: BackendId(0),
            kind: BackendFailure::ResponseTimeout,
        }),
        "{}",
        sim.trace()
    );
}

/// FR-PROXY-29: while the request body streams, response head bytes from
/// the backend are progress too ("no body progress in either direction"):
/// they restart `body_idle_timeout`. Who is blamed on expiry still follows
/// what the core waited for: the client's body bytes (408) or a backend
/// that stopped taking them (504).
#[test]
fn partial_response_head_during_upload_restarts_body_idle_timeout() {
    for backend_stalls in [false, true] {
        let mut sim = Sim::new(TIMEOUTS);
        let head = b"POST /x HTTP/1.1\r\nhost: example.test\r\ncontent-length: 10\r\n\
            x-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n";
        sim.set_default_backend_window(Window::Budget(head.len() + 3));
        let c = sim.connect(peer());
        sim.send(c, POST_10);
        if backend_stalls {
            sim.send(c, b"0123456789");
        } else {
            sim.send(c, b"012");
        }
        sim.run_until_idle();
        let up = only_upstream(&sim);
        sim.advance(Duration::from_millis(1500));
        sim.backend_send(up, b"HTTP/1.1 200 OK\r\n");
        sim.run_until_idle();
        // 4 s after the head bytes, not after the last body byte.
        sim.advance(Duration::from_millis(3999));
        assert!(
            sim.take_received(c).is_empty(),
            "backend_stalls={backend_stalls}\n{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
        sim.advance(Duration::from_millis(1));
        // The whole body was read when only the backend stalls, so that
        // 504 keeps the client (FR-PROXY-26, -28).
        let (status, persistence, log) = if backend_stalls {
            (
                Status::GatewayTimeout,
                Persistence::KeepAlive,
                Log::BackendFailed {
                    conn: c,
                    peer: peer(),
                    backend: BackendId(0),
                    kind: BackendFailure::BodyTimeout,
                },
            )
        } else {
            (
                Status::RequestTimeout,
                Persistence::Close,
                Log::Rejected {
                    conn: c,
                    peer: peer(),
                    reason: RejectReason::Timeout(TimeoutKind::RequestBody),
                },
            )
        };
        assert_eq!(
            sim.take_received(c),
            fixed(status, persistence, BodyMode::Full),
            "backend_stalls={backend_stalls}\n{}",
            sim.trace()
        );
        assert_eq!(sim.logs().first(), Some(&log), "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    }
}

/// FR-PROXY-18, -29: once the whole request is written,
/// `backend_response_timeout` is a total deadline. Response head bytes (an
/// interim head, part of the final head) do not extend it: the 504 comes
/// exactly 3 s after the request was written, for a request without a body
/// and for one whose body was fully written.
#[test]
fn head_bytes_after_the_request_do_not_extend_the_response_timeout() {
    let get: &[u8] = b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n";
    let body: &[u8] = b"0123456789";
    for (raw, tail, method) in [
        (get.to_vec(), b"\r\n\r\n".as_slice(), Method::Get),
        (
            [POST_10, body].concat(),
            body,
            Method::Other("POST".to_owned()),
        ),
    ] {
        let mut sim = Sim::new(TIMEOUTS);
        let c = request(&mut sim, &raw);
        let up = only_upstream(&sim);
        // The whole request (head, and body if any) went out at t = 0.
        let forwarded = sim.take_backend_request(up);
        assert!(forwarded.ends_with(tail), "{method:?}\n{}", sim.trace());
        sim.advance(Duration::from_secs(2));
        sim.backend_send(up, b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n");
        sim.run_until_idle();
        sim.advance(Duration::from_millis(999));
        assert!(
            sim.take_received(c).is_empty(),
            "{method:?}\n{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 1, "{method:?}\n{}", sim.trace());
        sim.advance(Duration::from_millis(1));
        assert_eq!(
            sim.take_received(c),
            fixed(
                Status::GatewayTimeout,
                Persistence::KeepAlive,
                BodyMode::Full
            ),
            "{method:?}\n{}",
            sim.trace()
        );
        assert_eq!(
            sim.logs(),
            &[
                Log::BackendFailed {
                    conn: c,
                    peer: peer(),
                    backend: BackendId(0),
                    kind: BackendFailure::ResponseTimeout,
                },
                Log::Response {
                    conn: c,
                    peer: peer(),
                    status: Status::GatewayTimeout,
                    request: Some(summary(method.clone(), "/api/x")),
                },
            ],
            "{method:?}\n{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    }
}

/// When the client half-closes in `client_half_close_still_gets_response`.
#[derive(Debug, Clone, Copy)]
enum HalfClose {
    /// Right behind the request (with its body, if any).
    WithRequest,
    /// Behind the last body bytes, which come after the head was forwarded.
    AfterStreamedBody,
    /// Once the response head and part of its body reached the client.
    AfterResponseHead,
}

/// FR-PROXY-26 (owner decision): a client that only closes its sending
/// side after a complete request keeps its upstream; the response is
/// delivered in full, then both close. The core does not read the client
/// while it proxies (except for the body), so its end of stream is only
/// seen once the response is out.
#[test]
fn client_half_close_still_gets_response() {
    let get: &[u8] = b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n";
    let response: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
    let relayed: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
    for (request, body, when) in [
        (get, b"".as_slice(), HalfClose::WithRequest),
        (get, b"", HalfClose::AfterResponseHead),
        (POST_10, b"0123456789", HalfClose::WithRequest),
        (POST_10, b"0123456789", HalfClose::AfterStreamedBody),
        (POST_10, b"0123456789", HalfClose::AfterResponseHead),
    ] {
        let mut sim = Sim::new(TIMEOUTS);
        let c = sim.connect(peer());
        sim.send(c, request);
        match when {
            HalfClose::WithRequest => {
                sim.send(c, body);
                sim.send_eof(c);
            }
            HalfClose::AfterStreamedBody => {
                sim.run_until_idle();
                sim.send(c, body);
                sim.send_eof(c);
            }
            HalfClose::AfterResponseHead => sim.send(c, body),
        }
        sim.run_until_idle();
        let up = only_upstream(&sim);
        let forwarded = sim.take_backend_request(up);
        assert!(forwarded.ends_with(body), "{when:?}\n{}", sim.trace());
        sim.backend_send(up, response.get(..response.len() - 3).unwrap_or_default());
        sim.run_until_idle();
        if let HalfClose::AfterResponseHead = when {
            sim.send_eof(c);
            sim.run_until_idle();
        }
        assert_eq!(
            sim.state(c),
            SimConnState::Open,
            "{when:?}\n{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 1, "{when:?}\n{}", sim.trace());
        sim.backend_send(up, response.get(response.len() - 3..).unwrap_or_default());
        sim.run_until_idle();
        assert_eq!(sim.take_received(c), relayed, "{when:?}\n{}", sim.trace());
        assert_eq!(
            sim.state(c),
            SimConnState::Closed,
            "{when:?}\n{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 0, "{when:?}\n{}", sim.trace());
        assert!(
            matches!(sim.logs(), [Log::ProxiedResponse { status: 200, .. }]),
            "{when:?}\n{}",
            sim.trace()
        );
    }
}
