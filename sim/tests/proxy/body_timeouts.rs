//! The request body going wrong or arriving late on proxy routes: the
//! stalled body (FR-PROXY-26), a backend that stops reading it, a client
//! that closes mid-body, and a request pipelined behind a body.

use std::time::Duration;

use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, Log, RejectReason, RequestBodyError, TimeoutKind, UpstreamId,
};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

use super::bodies::{BODIES, OK, OK_RELAYED, answer_ok_after, forwarded};
use super::{fixed, only_upstream, peer, request, summary};

/// Review Focus 1: a request pipelined right behind a body, in the same
/// read, never reaches the first backend; it is answered next, over its
/// own upstream. Both framings, and a request that looks like a body.
#[test]
fn pipelined_request_after_body_is_not_forwarded() {
    let next: &[u8] = b"GET /api/admin HTTP/1.1\r\nHost: example.test\r\n\r\n";
    let cases: [(&str, &[u8]); 2] = [
        ("Content-Length: 5\r\n", b"hello"),
        ("Transfer-Encoding: chunked\r\n", b"5\r\nhello\r\n0\r\n\r\n"),
    ];
    for (framing, body) in cases {
        let mut sim = Sim::new(BODIES);
        let mut raw =
            format!("POST /api/x HTTP/1.1\r\nHost: example.test\r\n{framing}\r\n").into_bytes();
        raw.extend_from_slice(body);
        raw.extend_from_slice(next);
        let c = request(&mut sim, &raw);
        let first = only_upstream(&sim);
        let mut expected = forwarded("POST", "/x", &framing.to_ascii_lowercase());
        expected.extend_from_slice(body);
        assert_eq!(
            sim.take_backend_request(first),
            expected,
            "{framing}\n{}",
            sim.trace()
        );
        sim.backend_send(first, OK);
        sim.run_until_idle();
        let second = UpstreamId(2);
        assert_eq!(sim.upstreams(), [first, second], "{}", sim.trace());
        assert_eq!(
            sim.take_backend_request(second),
            forwarded("GET", "/admin", ""),
            "{framing}\n{}",
            sim.trace()
        );
        assert!(sim.take_backend_request(first).is_empty());
        answer_ok_after(&mut sim, c, second);
    }
}

/// FR-PROXY-26: a client that stalls its body before the response head is
/// at fault: 408 after the body idle timeout (`body_idle_timeout`, 30 s
/// by default), and the upstream is closed.
#[test]
fn stalled_request_body_is_408() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 10\r\n\r\n012",
    );
    sim.advance(Duration::from_secs(20));
    sim.send(c, b"34");
    sim.run_until_idle();
    // Progress resets the deadline.
    sim.advance(Duration::from_millis(29_999));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(Status::RequestTimeout, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
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
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

/// A backend that stops taking the body is at fault: 504 after the body
/// idle timeout, and the client closes (the rest of its body is unread).
#[test]
fn backend_not_reading_the_body_is_504() {
    let mut sim = Sim::new(BODIES);
    let head = forwarded("POST", "/x", "content-length: 1000\r\n");
    sim.set_default_backend_window(Window::Budget(head.len() + 10));
    let mut raw =
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 1000\r\n\r\n".to_vec();
    raw.resize(raw.len() + 500, b'b');
    let c = request(&mut sim, &raw);
    let up = only_upstream(&sim);
    assert_eq!(sim.take_backend_request(up).len(), head.len() + 10);
    sim.advance(Duration::from_millis(29_999));
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
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

/// Once the whole request is written, the response timeout (60 s) runs
/// from there, not the body idle timeout.
#[test]
fn response_timeout_runs_once_the_body_is_written() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 2\r\n\r\nhi",
    );
    sim.advance(Duration::from_secs(59));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_secs(1));
    assert_eq!(
        sim.take_received(c),
        fixed(
            Status::GatewayTimeout,
            Persistence::KeepAlive,
            BodyMode::Full
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// A client that ends its stream in the middle of its body leaves an
/// incomplete request: both connections close at once, with no response,
/// and the truncated request the backend got is logged (owner decision
/// 2026-10-27, D-B: `Rejected { Body(ClosedEarly) }`), for a
/// `Content-Length` body and a chunked one alike.
#[test]
fn client_eof_mid_body_closes_both() {
    for framing in [
        "Content-Length: 10\r\n\r\n012",
        "Transfer-Encoding: chunked\r\n\r\n5\r\nab",
    ] {
        let mut sim = Sim::new(BODIES);
        let c = request(
            &mut sim,
            format!("POST /api/x HTTP/1.1\r\nHost: example.test\r\n{framing}").as_bytes(),
        );
        assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
        sim.send_eof(c);
        sim.run_until_idle();
        assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
        assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
        assert_eq!(
            sim.logs(),
            &[Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Body(RequestBodyError::ClosedEarly),
            }],
            "{}",
            sim.trace()
        );
    }
}

/// A backend that fails while the whole body is read but not yet written:
/// the client's stream is still in step, so the 502 keeps it open, and the
/// request pipelined behind the body is answered next (FR-PROXY-26, -28).
#[test]
fn backend_failure_after_the_whole_body_keeps_the_client() {
    let mut sim = Sim::new(BODIES);
    let head = forwarded("POST", "/x", "content-length: 5\r\n");
    sim.set_default_backend_window(Window::Budget(head.len() + 2));
    let mut raw =
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 5\r\n\r\nhello".to_vec();
    raw.extend_from_slice(b"GET /api/next HTTP/1.1\r\nHost: example.test\r\n\r\n");
    let c = request(&mut sim, &raw);
    let up = only_upstream(&sim);
    let mut expected = head;
    expected.extend_from_slice(b"he");
    assert_eq!(sim.take_backend_request(up), expected, "{}", sim.trace());
    sim.set_default_backend_window(Window::Unlimited);
    sim.backend_reset(up);
    sim.run_until_idle();
    let second = UpstreamId(2);
    assert_eq!(sim.upstreams(), [up, second], "{}", sim.trace());
    assert_eq!(
        sim.take_backend_request(second),
        forwarded("GET", "/next", ""),
        "{}",
        sim.trace()
    );
    sim.backend_send(second, OK);
    sim.run_until_idle();
    let mut both = fixed(Status::BadGateway, Persistence::KeepAlive, BodyMode::Full);
    both.extend_from_slice(OK_RELAYED);
    assert_eq!(sim.take_received(c), both, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}
