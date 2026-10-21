//! Request handling through the simulation driver: parse, route, answer.

use std::net::{IpAddr, Ipv4Addr};

use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::{BodyMode, render_fixed};
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{Interest, Log, RejectReason, RequestSummary};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test www.example.test {
  path / { static /srv/www; }
  path /api/ { proxy 127.0.0.1:3000; strip_prefix /api; max_request_body 1MiB; max_response_body unlimited; }
}";

/// Capacity 24 = max_head, while max_request_line + 2 = 18.
const SMALL_HEAD: &str = "listen 127.0.0.1:8080;
limits { max_request_line 16; max_head 24; }
host example.test { path / { static /srv/www; } }";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn fixed(status: Status, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    let mut out = Vec::new();
    render_fixed(status, persistence, body, &mut out);
    out
}

fn not_found_keep_alive() -> Vec<u8> {
    fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::Full)
}

fn summary(method: Method, host: &str, path: &str) -> Option<RequestSummary> {
    Some(RequestSummary {
        method,
        host: Some(host.to_owned()),
        path: path.to_owned(),
    })
}

#[test]
fn unknown_host_gets_404_and_keeps_connection() {
    let mut sim = Sim::new(CONFIG);
    let p = peer();
    let c = sim.connect(p);
    sim.send(c, b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Response {
            conn: c,
            peer: p,
            status: Status::NotFound,
            request: summary(Method::Get, "other.test", "/x"),
        }],
        "{}",
        sim.trace()
    );
}

#[test]
fn matched_route_gets_501_and_closes() {
    let mut sim = Sim::new(CONFIG);
    let p = peer();
    let c = sim.connect(p);
    sim.send(c, b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotImplemented, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Response {
            conn: c,
            peer: p,
            status: Status::NotImplemented,
            request: summary(Method::Get, "example.test", "/api/x"),
        }],
        "{}",
        sim.trace()
    );
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
}

#[test]
fn malformed_request_gets_400_and_closes() {
    let mut sim = Sim::new(CONFIG);
    let p = peer();
    let c = sim.connect(p);
    sim.send(c, b"GET / HTTP/1.1\nHost: a\n\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadRequest, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: p,
            reason: RejectReason::Http(HttpError::BareLf),
        }],
        "{}",
        sim.trace()
    );
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
}

#[test]
fn head_request_gets_no_body() {
    let mut sim = Sim::new(CONFIG);
    let p = peer();
    let c = sim.connect(p);
    sim.send(c, b"HEAD / HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::HeadOnly),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Response {
            conn: c,
            peer: p,
            status: Status::NotFound,
            request: summary(Method::Head, "other.test", "/"),
        }],
        "{}",
        sim.trace()
    );
}

#[test]
fn keep_alive_serves_sequential_requests() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.send(c, b"GET /a HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    sim.send(c, b"GET /b HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.logs().len(), 2, "{}", sim.trace());
}

#[test]
fn pipelined_requests_answered_in_order() {
    let mut sim = Sim::new(CONFIG);
    let p = peer();
    let c = sim.connect(p);
    sim.send(
        c,
        b"GET /a HTTP/1.1\r\nHost: other.test\r\n\r\n\
          HEAD /b HTTP/1.1\r\nHost: other.test\r\n\r\n",
    );
    sim.run_until_idle();
    let mut expected = not_found_keep_alive();
    expected.extend(fixed(
        Status::NotFound,
        Persistence::KeepAlive,
        BodyMode::HeadOnly,
    ));
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            Log::Response {
                conn: c,
                peer: p,
                status: Status::NotFound,
                request: summary(Method::Get, "other.test", "/a"),
            },
            Log::Response {
                conn: c,
                peer: p,
                status: Status::NotFound,
                request: summary(Method::Head, "other.test", "/b"),
            },
        ],
        "{}",
        sim.trace()
    );
}

#[test]
fn unread_body_closes_after_response() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    // An unknown host, so the 404 path (which keeps persistence) is taken
    // and only the unread body forces the close.
    sim.send(
        c,
        b"GET /nowhere HTTP/1.1\r\nHost: unknown.test\r\nContent-Length: 10\r\n\r\n0123456789",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.send(c, b"GET / HTTP/1.1\r\nHost: unknown.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(sim.queued_to_server(c), 0, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
}

#[test]
fn slow_client_window_still_gets_whole_response() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Bytes(3));
    sim.send(c, b"GET / HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

#[test]
fn byte_by_byte_request_is_answered() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.set_read_chunk(c, 1);
    sim.send(c, b"GET / HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

#[test]
fn full_buffer_without_complete_head_gets_431() {
    let mut sim = Sim::new(SMALL_HEAD);
    let p = peer();
    let c = sim.connect(p);
    // 16 + 8 = 24 bytes: the request line fits, the head never ends.
    sim.send(c, b"GET / HTTP/1.1\r\nHost: ex");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(
            Status::RequestHeaderFieldsTooLarge,
            Persistence::Close,
            BodyMode::Full
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: p,
            reason: RejectReason::Http(HttpError::HeadTooLarge),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

#[test]
fn reset_while_lingering_closes_without_reset_log() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.send(c, b"GET / HTTP/1.1\nHost: a\n\n");
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.reset(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
}
