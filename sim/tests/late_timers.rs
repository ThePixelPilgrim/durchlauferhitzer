//! The late-timer mode (NFR-TEST-07): `Sim::set_timer_order` with
//! `TimerOrder::Late` makes `advance` wake once, at its end, as a real
//! loop that overslept does, and deliver the I/O and disk completions due
//! by then before the overdue `TimerFired`. Each test runs in both orders
//! and checks the outcome the spec gives, and where in the trace the
//! timer fired.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnId, Log, RejectReason, RequestSummary, TimeoutKind, UpstreamId,
};
use durchlauferhitzer_sim::{Sim, TimerOrder};

#[allow(dead_code, reason = "shared by all test binaries; each uses part")]
#[path = "support/model_http.rs"]
mod model_http;

use model_http::{model_file_head, model_fixed};

const ORDERS: [TimerOrder; 2] = [TimerOrder::OnTime, TimerOrder::Late];

/// `header_timeout` 10 s; waiting on the disk counts against
/// `send_idle_timeout` 30 s (FR-CONN-08).
const STATIC: &str = "listen 127.0.0.1:8080;
limits { header_timeout 10s; send_idle_timeout 30s; }
host example.test { path / { static /srv/www; } }";

/// The proxy timeouts, each distinct.
const PROXY: &str = "listen 127.0.0.1:8080;
limits { backend_connect_timeout 2s; backend_response_timeout 3s; body_idle_timeout 4s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// A simulation of `config` in `order`.
fn sim_in(config: &str, order: TimerOrder) -> Sim {
    let mut sim = Sim::new(config);
    sim.set_timer_order(order);
    sim
}

/// The position in the trace of the first line at `secs` seconds that
/// contains `what`.
fn at(sim: &Sim, secs: u64, what: &str) -> usize {
    let stamp = format!("t={}ns ", u128::from(secs) * 1_000_000_000);
    let found = sim
        .trace()
        .lines()
        .position(|l| l.starts_with(&stamp) && l.contains(what));
    assert!(found.is_some(), "no {what:?} at t={secs}s\n{}", sim.trace());
    found.unwrap_or(usize::MAX)
}

/// Where the timer fired against the event that arrived with it: on time
/// at its own deadline, before the event; late at the wake, after it.
fn assert_order(sim: &Sim, order: TimerOrder, event: &str, on_time: (u64, u64), late: u64) {
    match order {
        TimerOrder::OnTime => {
            let (timer, arrival) = on_time;
            assert!(
                at(sim, timer, "TimerFired") < at(sim, arrival, event),
                "{order:?}\n{}",
                sim.trace()
            );
        }
        TimerOrder::Late => assert!(
            at(sim, late, event) < at(sim, late, "TimerFired"),
            "{order:?}\n{}",
            sim.trace()
        ),
    }
}

fn header_timeout(conn: ConnId) -> Log {
    Log::Rejected {
        conn,
        peer: peer(),
        reason: RejectReason::Timeout(TimeoutKind::Header),
    }
}

fn request_timeout() -> Vec<u8> {
    model_fixed(
        Status::RequestTimeout,
        Persistence::Close,
        BodyMode::Full,
        None,
    )
}

/// Head bytes that do not complete the head, queued with the head's total
/// deadline (FR-CONN-06): a 408 in either order, trickling does not
/// extend the deadline. Late, the bytes are read at the wake, before the
/// timer fires.
#[test]
fn queued_client_bytes_come_before_an_overdue_timer() {
    for order in ORDERS {
        let mut sim = sim_in(STATIC, order);
        let c = sim.connect(peer());
        sim.send(c, b"GET / HTTP/1.1\r\n");
        sim.run_until_idle();
        sim.send(c, b"Host: exa");
        sim.advance(secs(12));
        assert_eq!(sim.take_received(c), request_timeout(), "{order:?}");
        assert_eq!(sim.logs(), &[header_timeout(c)], "{order:?}");
        assert_order(&sim, order, "conn=1 Received len=9", (10, 10), 12);
    }
}

/// A disk completion due before the wake, on one connection, and another
/// connection's overdue head deadline: late, `FileOpened` comes first,
/// both at the wake. Neither outcome depends on the order: the file is
/// served (its open took 11 s of the 30 s send idle timeout), the other
/// head is a 408.
#[test]
fn due_disk_completions_come_before_an_overdue_timer() {
    for order in ORDERS {
        let mut sim = sim_in(STATIC, order);
        sim.add_file("/srv/www", "a.txt", b"hello");
        sim.set_disk_delay(secs(11));
        let slow = sim.connect(peer());
        sim.send(slow, b"GET / HTTP/1.1\r\n");
        let file = sim.connect(peer());
        sim.send(file, b"GET /a.txt HTTP/1.1\r\nHost: example.test\r\n\r\n");
        sim.run_until_idle();
        sim.advance(secs(12));
        assert_eq!(sim.take_received(slow), request_timeout(), "{order:?}");
        assert_eq!(
            sim.take_received(file),
            model_file_head("text/plain; charset=utf-8", 5, Persistence::KeepAlive, None),
            "{order:?}\n{}",
            sim.trace()
        );
        let served = Log::Response {
            conn: file,
            peer: peer(),
            status: Status::Ok,
            request: Some(RequestSummary {
                method: Method::Get,
                host: Some("example.test".to_owned()),
                path: "/a.txt".to_owned(),
            }),
        };
        let logs = match order {
            TimerOrder::OnTime => [header_timeout(slow), served],
            TimerOrder::Late => [served, header_timeout(slow)],
        };
        assert_eq!(sim.logs(), &logs, "{order:?}\n{}", sim.trace());
        assert_order(&sim, order, "FileOpened kind=Regular size=5", (10, 11), 12);
    }
}

/// A backend's first response head byte queued with the response timeout
/// (FR-PROXY-29: once the whole request is written,
/// `backend_response_timeout` is a total deadline that head bytes do not
/// extend): a 504 `ResponseTimeout` in either order. Late, the byte is
/// read at the wake, before the timer fires.
#[test]
fn response_head_byte_with_the_response_timeout_is_504_either_way() {
    for order in ORDERS {
        let mut sim = sim_in(PROXY, order);
        let c = sim.connect(peer());
        sim.send(c, b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n");
        sim.run_until_idle();
        let upstreams = sim.upstreams();
        assert_eq!(upstreams.len(), 1, "{order:?}\n{}", sim.trace());
        let up = upstreams.first().copied().unwrap_or(UpstreamId(0));
        assert!(!sim.take_backend_request(up).is_empty(), "{order:?}");
        sim.backend_send(up, b"H");
        sim.advance(secs(5));
        assert_eq!(
            sim.take_received(c),
            model_fixed(
                Status::GatewayTimeout,
                Persistence::KeepAlive,
                BodyMode::Full,
                None
            ),
            "{order:?}\n{}",
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
                    request: Some(RequestSummary {
                        method: Method::Get,
                        host: Some("example.test".to_owned()),
                        path: "/api/x".to_owned(),
                    }),
                },
            ],
            "{order:?}\n{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 0, "{order:?}");
        match order {
            TimerOrder::OnTime => assert!(
                !sim.trace().contains("UpstreamReceived"),
                "{order:?}\n{}",
                sim.trace()
            ),
            TimerOrder::Late => assert_order(&sim, order, "up=1 UpstreamReceived len=1", (0, 0), 5),
        }
    }
}

// Owner decision 2026-10-27 (DRV-06): delivery order decides. An event
// handled before the `TimerFired` counts, even when `now` is past its
// connection's deadline; a deadline, total ones included, may be overrun
// by as long as the loop was late: the events handled in that one late
// wake, before `TimerFired`, still count. The four cases below differ
// between the orders
// for that reason, and each order's outcome is pinned.

fn summary(method: Method, path: &str) -> RequestSummary {
    RequestSummary {
        method,
        host: Some("example.test".to_owned()),
        path: path.to_owned(),
    }
}

/// The client's last body byte queued with the body idle deadline (4 s),
/// the wake 1 s past it. On time: 408 (`Timeout(RequestBody)`), the
/// backend saw 9 of 10 bytes and is closed. Late: the byte counts, the
/// whole body is forwarded and the response relayed.
#[test]
fn last_body_byte_with_the_body_idle_deadline() {
    for order in ORDERS {
        let mut sim = sim_in(PROXY, order);
        let c = sim.connect(peer());
        sim.send(
            c,
            b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 10\r\n\r\n012345678",
        );
        sim.run_until_idle();
        let upstreams = sim.upstreams();
        assert_eq!(upstreams.len(), 1, "{order:?}\n{}", sim.trace());
        let up = upstreams.first().copied().unwrap_or(UpstreamId(0));
        sim.send(c, b"9");
        sim.advance(secs(5));
        let forwarded = sim.take_backend_request(up);
        match order {
            TimerOrder::OnTime => {
                assert!(forwarded.ends_with(b"\r\n\r\n012345678"), "{}", sim.trace());
                assert_eq!(sim.take_received(c), request_timeout(), "{}", sim.trace());
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
                assert_order(&sim, order, "conn=1 Received len=1", (4, 4), 5);
            }
            TimerOrder::Late => {
                assert!(
                    forwarded.ends_with(b"\r\n\r\n0123456789"),
                    "{}",
                    sim.trace()
                );
                assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
                assert_eq!(sim.logs(), &[], "{}", sim.trace());
                assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
                assert!(sim.trace().contains("t=5000000000ns conn=1 Received len=1"));
                sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                sim.run_until_idle();
                assert_eq!(
                    sim.take_received(c),
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
                    "{}",
                    sim.trace()
                );
                assert_eq!(
                    sim.logs(),
                    &[Log::ProxiedResponse {
                        conn: c,
                        peer: peer(),
                        backend: BackendId(0),
                        status: 200,
                        request: summary(Method::Other("POST".to_owned()), "/api/x"),
                    }],
                    "{}",
                    sim.trace()
                );
            }
        }
    }
}

/// The backend's complete response queued with the response timeout
/// (3 s, a total deadline), the wake 2 s past it. On time: 504
/// `ResponseTimeout`. Late: the head counts and the response is relayed.
#[test]
fn complete_response_head_with_the_response_timeout() {
    for order in ORDERS {
        let mut sim = sim_in(PROXY, order);
        let c = sim.connect(peer());
        sim.send(c, b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n");
        sim.run_until_idle();
        let upstreams = sim.upstreams();
        assert_eq!(upstreams.len(), 1, "{order:?}\n{}", sim.trace());
        let up = upstreams.first().copied().unwrap_or(UpstreamId(0));
        assert!(!sim.take_backend_request(up).is_empty(), "{order:?}");
        sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        sim.advance(secs(5));
        assert_eq!(sim.open_upstreams(), 0, "{order:?}\n{}", sim.trace());
        match order {
            TimerOrder::OnTime => {
                assert_eq!(
                    sim.take_received(c),
                    model_fixed(
                        Status::GatewayTimeout,
                        Persistence::KeepAlive,
                        BodyMode::Full,
                        None
                    ),
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
                            request: Some(summary(Method::Get, "/api/x")),
                        },
                    ],
                    "{}",
                    sim.trace()
                );
                assert!(!sim.trace().contains("UpstreamReceived"), "{}", sim.trace());
            }
            TimerOrder::Late => {
                assert_eq!(
                    sim.take_received(c),
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
                    "{}",
                    sim.trace()
                );
                assert_eq!(
                    sim.logs(),
                    &[Log::ProxiedResponse {
                        conn: c,
                        peer: peer(),
                        backend: BackendId(0),
                        status: 200,
                        request: summary(Method::Get, "/api/x"),
                    }],
                    "{}",
                    sim.trace()
                );
            }
        }
    }
}

/// A `FileOpened` (the open takes 31 s) with its disk timeout (the send
/// idle timeout, 30 s), the wake 1 s after the open completed. On time:
/// a disk timeout, the late open answered with `CloseFile`. Late: the
/// open counts and the file's head is sent.
#[test]
fn file_opened_with_its_disk_timeout() {
    for order in ORDERS {
        let mut sim = sim_in(STATIC, order);
        sim.add_file("/srv/www", "a.txt", b"hello");
        sim.set_disk_delay(secs(31));
        let c = sim.connect(peer());
        sim.send(c, b"GET /a.txt HTTP/1.1\r\nHost: example.test\r\n\r\n");
        sim.run_until_idle();
        sim.advance(secs(32));
        match order {
            TimerOrder::OnTime => {
                assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
                assert_eq!(
                    sim.logs(),
                    &[Log::Rejected {
                        conn: c,
                        peer: peer(),
                        reason: RejectReason::Timeout(TimeoutKind::Disk),
                    }],
                    "{}",
                    sim.trace()
                );
                assert_eq!(sim.open_files(), 0, "{}", sim.trace());
                assert!(
                    sim.trace().contains("t=31000000000ns file=1 -> CloseFile"),
                    "{}",
                    sim.trace()
                );
                assert_order(&sim, order, "file=1 FileOpened", (30, 31), 32);
            }
            TimerOrder::Late => {
                assert_eq!(
                    sim.take_received(c),
                    model_file_head("text/plain; charset=utf-8", 5, Persistence::KeepAlive, None),
                    "{}",
                    sim.trace()
                );
                assert_eq!(
                    sim.logs(),
                    &[Log::Response {
                        conn: c,
                        peer: peer(),
                        status: Status::Ok,
                        request: Some(summary(Method::Get, "/a.txt")),
                    }],
                    "{}",
                    sim.trace()
                );
                assert_eq!(sim.open_files(), 1, "{}", sim.trace());
            }
        }
    }
}

/// The bytes that complete a request head queued with the head's total
/// deadline (`header_timeout` 10 s), the wake 2 s past it. On time: 408.
/// Late: the head counts and is answered (404, no such file).
#[test]
fn head_completed_after_its_header_timeout() {
    for order in ORDERS {
        let mut sim = sim_in(STATIC, order);
        let c = sim.connect(peer());
        sim.send(c, b"GET /x HTTP/1.1\r\n");
        sim.run_until_idle();
        sim.send(c, b"Host: example.test\r\n\r\n");
        sim.advance(secs(12));
        match order {
            TimerOrder::OnTime => {
                assert_eq!(sim.take_received(c), request_timeout(), "{}", sim.trace());
                assert_eq!(sim.logs(), &[header_timeout(c)], "{}", sim.trace());
            }
            TimerOrder::Late => {
                assert_eq!(
                    sim.take_received(c),
                    model_fixed(
                        Status::NotFound,
                        Persistence::KeepAlive,
                        BodyMode::Full,
                        None
                    ),
                    "{}",
                    sim.trace()
                );
                assert_eq!(
                    sim.logs(),
                    &[Log::Response {
                        conn: c,
                        peer: peer(),
                        status: Status::NotFound,
                        request: Some(summary(Method::Get, "/x")),
                    }],
                    "{}",
                    sim.trace()
                );
            }
        }
    }
}
