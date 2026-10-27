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

/// As `TIMEOUTS`, but the response timeout (10 s) is above the body idle
/// timeout, so a response head's own deadline (FR-PROXY-29) passes only
/// after body idle timeouts restarted by head bytes.
const LONG_RESPONSE: &str = "listen 127.0.0.1:8080;
limits { backend_connect_timeout 2s; backend_response_timeout 10s; body_idle_timeout 4s; }
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
/// that stopped taking them (504). (With `LONG_RESPONSE`, so the head's
/// own deadline comes later.)
#[test]
fn partial_response_head_during_upload_restarts_body_idle_timeout() {
    for backend_stalls in [false, true] {
        let mut sim = Sim::new(LONG_RESPONSE);
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

/// What happens at a given time (in ms) in
/// `each_response_head_has_its_own_deadline`.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// The client sends these body bytes.
    Client(u64, &'static [u8]),
    /// The backend sends these response head bytes.
    Backend(u64, &'static [u8]),
}

/// A complete interim head, as a backend sends it before its response.
const EARLY_HINTS: &[u8] = b"HTTP/1.1 103 Early Hints\r\n\r\n";

/// FR-PROXY-29 (owner decision 2026-10-27, refined): each response head,
/// interim or final, must be complete within `backend_response_timeout`
/// of its own first byte, even while the request body streams. Later bytes
/// of the same head and body progress do not move that cap; a completed
/// interim head ends it, and the next head's first byte starts a fresh
/// one. The earlier of the cap and the deadline that otherwise applies
/// wins.
#[test]
fn each_response_head_has_its_own_deadline() {
    let cases: [(&str, &[Step], u64, Persistence); 5] = [
        // TIMEOUTS (response 3 s): the client keeps its body streaming;
        // the cap passes 3 s after the first head byte at 1 s.
        (
            TIMEOUTS,
            &[
                Step::Client(0, b"0"),
                Step::Backend(1000, b"H"),
                Step::Client(1000, b"1"),
                Step::Client(2000, b"2"),
                Step::Client(3000, b"3"),
            ],
            4000,
            Persistence::Close,
        ),
        // The body completes after the first head byte: the response
        // timeout from the first head byte (1 s) passes before the one
        // from the fully written request (2 s), so it wins. The whole body
        // was read, so the 504 keeps the client (FR-PROXY-26, -28).
        (
            TIMEOUTS,
            &[
                Step::Client(0, b"0"),
                Step::Backend(1000, b"H"),
                Step::Client(2000, b"123456789"),
            ],
            4000,
            Persistence::KeepAlive,
        ),
        // An interim head completes at 1 s and the upload goes on past
        // 1 s + 3 s. The final head starts at 5 s and trickles: its own
        // cap passes at 8 s, later bytes of it do not move it.
        (
            TIMEOUTS,
            &[
                Step::Client(0, b"0"),
                Step::Backend(1000, EARLY_HINTS),
                Step::Client(2000, b"1"),
                Step::Client(3000, b"2"),
                Step::Client(4000, b"3"),
                Step::Backend(5000, b"H"),
                Step::Client(6000, b"4"),
                Step::Backend(7000, b"TTP/1.1"),
                Step::Client(7000, b"5"),
            ],
            8000,
            Persistence::Close,
        ),
        // The next head's first byte comes with the interim head: its cap
        // starts with it.
        (
            TIMEOUTS,
            &[
                Step::Client(0, b"0"),
                Step::Backend(1000, b"HTTP/1.1 103 Early Hints\r\n\r\nH"),
                Step::Client(2000, b"1"),
                Step::Client(3000, b"2"),
            ],
            4000,
            Persistence::Close,
        ),
        // An interim head that ends its cap does not end the response
        // timeout from the fully written request (2 s + 3 s): the cap from
        // 1 s was the earlier deadline, then that one applies again.
        (
            TIMEOUTS,
            &[
                Step::Client(0, b"0"),
                Step::Backend(1000, b"H"),
                Step::Client(2000, b"123456789"),
                Step::Backend(3000, b"TTP/1.1 100 Continue\r\n\r\n"),
            ],
            5000,
            Persistence::KeepAlive,
        ),
    ];
    for (config, steps, expiry, persistence) in cases {
        let mut sim = Sim::new(config);
        let c = sim.connect(peer());
        sim.send(c, POST_10);
        let mut at = 0;
        for &step in steps {
            let (Step::Client(when, _) | Step::Backend(when, _)) = step;
            sim.advance(Duration::from_millis(when - at));
            at = when;
            match step {
                Step::Client(_, bytes) => sim.send(c, bytes),
                Step::Backend(_, bytes) => sim.backend_send(only_upstream(&sim), bytes),
            }
            sim.run_until_idle();
        }
        sim.advance(Duration::from_millis(expiry - 1 - at));
        assert!(
            sim.take_received(c).is_empty(),
            "expiry={expiry} {persistence:?}\n{}",
            sim.trace()
        );
        assert_eq!(
            sim.open_upstreams(),
            1,
            "expiry={expiry} {persistence:?}\n{}",
            sim.trace()
        );
        sim.advance(Duration::from_millis(1));
        assert_eq!(
            sim.take_received(c),
            fixed(Status::GatewayTimeout, persistence, BodyMode::Full),
            "expiry={expiry} {persistence:?}\n{}",
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
                    request: Some(summary(Method::Other("POST".to_owned()), "/api/x")),
                },
            ],
            "expiry={expiry} {persistence:?}\n{}",
            sim.trace()
        );
        assert_eq!(
            sim.open_upstreams(),
            0,
            "expiry={expiry} {persistence:?}\n{}",
            sim.trace()
        );
    }
}

/// FR-PROXY-18, -29 (owner decision 2026-10-27, refined): a completed
/// interim head ends its cap, so a slow but healthy upload behind a
/// backend that sent `103 Early Hints` goes on well past the interim
/// head's first byte + `backend_response_timeout`, and its response is
/// relayed.
#[test]
fn slow_upload_goes_on_after_an_interim_head() {
    let mut sim = Sim::new(TIMEOUTS);
    let c = sim.connect(peer());
    sim.send(c, POST_10);
    sim.send(c, b"0");
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.advance(Duration::from_millis(1000));
    sim.backend_send(up, EARLY_HINTS);
    sim.run_until_idle();
    // One body byte a second, 9 s in all, each within the body idle
    // timeout (4 s); the interim head's cap (4 s) passes on the way.
    for byte in b"123456789".chunks(1) {
        sim.advance(Duration::from_millis(1000));
        sim.send(c, byte);
        sim.run_until_idle();
        assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    }
    assert!(
        sim.take_backend_request(up)
            .ends_with(b"\r\n\r\n0123456789"),
        "{}",
        sim.trace()
    );
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert!(
        matches!(sim.logs(), [Log::ProxiedResponse { status: 200, .. }]),
        "{}",
        sim.trace()
    );
}

/// FR-PROXY-29 (owner decision 2026-10-27, refined): a completed interim
/// head ends its cap, so a client that then stalls its body is blamed as
/// before: a 408 exactly `body_idle_timeout` (4 s) after its last body
/// byte (at 2 s), not a 504 at the interim head's first byte + 3 s. A 408
/// to HEAD is head-only (FR-PROXY-26).
#[test]
fn client_stall_after_an_interim_head_is_408() {
    let head: &[u8] = b"HEAD /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 10\r\n\r\n";
    for (request, mode) in [(POST_10, BodyMode::Full), (head, BodyMode::HeadOnly)] {
        let mut sim = Sim::new(TIMEOUTS);
        let c = sim.connect(peer());
        sim.send(c, request);
        sim.send(c, b"0");
        sim.run_until_idle();
        let up = only_upstream(&sim);
        sim.advance(Duration::from_millis(1000));
        sim.backend_send(up, b"HTTP/1.1 100 Continue\r\n\r\n");
        sim.run_until_idle();
        sim.advance(Duration::from_millis(1000));
        sim.send(c, b"1");
        sim.run_until_idle();
        sim.advance(Duration::from_millis(3999));
        assert!(sim.take_received(c).is_empty(), "{mode:?}\n{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 1, "{mode:?}\n{}", sim.trace());
        sim.advance(Duration::from_millis(1));
        assert_eq!(
            sim.take_received(c),
            fixed(Status::RequestTimeout, Persistence::Close, mode),
            "{mode:?}\n{}",
            sim.trace()
        );
        assert_eq!(
            sim.logs(),
            &[Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Timeout(TimeoutKind::RequestBody),
            }],
            "{mode:?}\n{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 0, "{mode:?}\n{}", sim.trace());
        assert_eq!(
            sim.trace().matches("CloseUpstream").count(),
            1,
            "{mode:?}\n{}",
            sim.trace()
        );
    }
}

/// `backend_response_timeout` equal to `body_idle_timeout` (both 4 s).
const TIE: &str = "listen 127.0.0.1:8080;
limits { backend_connect_timeout 2s; backend_response_timeout 4s; body_idle_timeout 4s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

/// FR-PROXY-29 (builder call, stage 3 Task 9): the client stalls its
/// body while the backend has sent part of a response head, and the
/// head's own deadline falls on the same instant as the body idle
/// timeout. The head's deadline wins: a 504 (`ResponseTimeout`), not a
/// 408; the unread body closes the client.
#[test]
fn head_deadline_tied_with_body_idle_is_504() {
    let mut sim = Sim::new(TIE);
    let c = sim.connect(peer());
    sim.send(c, POST_10);
    sim.send(c, b"012");
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-");
    sim.run_until_idle();
    sim.advance(Duration::from_millis(3999));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(Status::GatewayTimeout, Persistence::Close, BodyMode::Full),
        "{}",
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
                request: Some(summary(Method::Other("POST".to_owned()), "/api/x")),
            },
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}
