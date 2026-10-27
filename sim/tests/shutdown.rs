//! Shutdown in the core (FR-PROC-05, stage 4 amendment A4): the graceful
//! drain (`begin_drain`) and the fast path (`abort_all`), driven through
//! the simulation, which checks after every step that each file and
//! upstream is closed exactly once and never left open after its
//! connection.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{Log, RejectReason, RequestSummary};
use durchlauferhitzer_sim::{ConnectBehaviour, Sim, SimConnState, Window};

#[allow(dead_code, reason = "shared by all test binaries; each uses part")]
#[path = "support/model_http.rs"]
mod model_http;

use model_http::{RFC_DATE, model_file_head, model_fixed};

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

const GET_F: &[u8] = b"GET /f.txt HTTP/1.1\r\nHost: example.test\r\n\r\n";
const MISSING: &[u8] = b"GET /nope HTTP/1.1\r\nHost: example.test\r\n\r\n";
const OTHER_HOST: &[u8] = b"GET / HTTP/1.1\r\nHost: other.test\r\n\r\n";
const TEXT: &str = "text/plain; charset=utf-8";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn sim() -> Sim {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "f.txt", &[b'x'; 3000]);
    sim
}

fn summary(method: Method, path: &str) -> Option<RequestSummary> {
    Some(RequestSummary {
        method,
        host: Some("example.test".to_owned()),
        path: path.to_owned(),
    })
}

fn not_found_keep_alive() -> Vec<u8> {
    model_fixed(
        Status::NotFound,
        Persistence::KeepAlive,
        BodyMode::Full,
        None,
    )
}

/// The client ends its stream: a lingering connection then closes.
fn finish_client(sim: &mut Sim, conn: durchlauferhitzer_core::ConnId) {
    sim.send_eof(conn);
    sim.run_until_idle();
}

#[test]
fn idle_and_fresh_connections_close_at_once() {
    let mut sim = sim();
    let idle = sim.connect(peer());
    sim.send(idle, MISSING);
    sim.run_until_idle();
    assert_eq!(sim.take_received(idle), not_found_keep_alive());
    let fresh = sim.connect(peer());
    sim.run_until_idle();
    let logs = sim.logs().len();
    sim.begin_drain();
    assert_eq!(sim.state(idle), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.state(fresh), SimConnState::Closed, "{}", sim.trace());
    // Closed silently, like a keep-alive or header timeout without bytes.
    assert_eq!(sim.logs().len(), logs, "{}", sim.trace());
    assert!(sim.is_drained(), "{}", sim.trace());
}

#[test]
fn a_head_completed_during_the_drain_gets_503_with_close() {
    let mut sim = sim();
    let c = sim.connect(peer());
    sim.send(c, b"HEAD /f.txt HTTP/1.1\r\nHo");
    sim.run_until_idle();
    sim.begin_drain();
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert!(!sim.is_drained());
    sim.send(c, b"st: example.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        model_fixed(
            Status::ServiceUnavailable,
            Persistence::Close,
            BodyMode::HeadOnly,
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
            status: Status::ServiceUnavailable,
            request: summary(Method::Head, "/f.txt"),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "no file was opened");
    finish_client(&mut sim, c);
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// A static download in flight finishes (its head was queued keep-alive),
/// then the connection closes instead of waiting for the next request.
#[test]
fn an_inflight_download_completes_then_closes() {
    let mut sim = sim();
    let c = sim.connect(peer());
    // Part of the response only: the client takes 1000 bytes, then stalls.
    sim.set_client_window(c, Window::Budget(1000));
    sim.send(c, GET_F);
    sim.run_until_idle();
    sim.begin_drain();
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 1);
    sim.set_client_window(c, Window::Unlimited);
    sim.run_until_idle();
    let mut expected = model_file_head(TEXT, 3000, Persistence::KeepAlive, None);
    expected.extend_from_slice(&[b'x'; 3000]);
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    finish_client(&mut sim, c);
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// A file whose open is still in flight when the drain begins: its head is
/// rendered afterwards, so it says `Connection: close`.
#[test]
fn a_response_rendered_after_the_drain_began_says_close() {
    let mut sim = sim();
    sim.set_disk_delay(Duration::from_millis(5));
    let c = sim.connect(peer());
    sim.send(c, GET_F);
    sim.run_until_idle();
    sim.begin_drain();
    sim.advance(Duration::from_millis(50));
    let mut expected = model_file_head(TEXT, 3000, Persistence::Close, None);
    expected.extend_from_slice(&[b'x'; 3000]);
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    finish_client(&mut sim, c);
    assert!(sim.is_drained(), "{}", sim.trace());
}

#[test]
fn a_proxied_exchange_completes_then_closes() {
    let mut sim = sim();
    let c = sim.connect(peer());
    sim.send(c, b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n");
    sim.run_until_idle();
    let up = *sim.upstreams().first().unwrap();
    sim.begin_drain();
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    finish_client(&mut sim, c);
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// `is_drained` turns true exactly when the last resource goes: here an
/// orphaned open, answered by the disk after its connection reset.
#[test]
fn is_drained_flips_when_the_last_resource_goes() {
    let mut sim = sim();
    assert!(sim.is_drained(), "nothing open yet");
    sim.set_disk_delay(Duration::from_millis(10));
    let orphan = sim.connect(peer());
    sim.send(orphan, GET_F);
    sim.run_until_idle();
    sim.reset(orphan);
    let lingering = sim.connect(peer());
    sim.send(lingering, b"GET / HTTP/1.1\nHost: a\n\n");
    sim.run_until_idle();
    assert_eq!(sim.state(lingering), SimConnState::WriteShutdown);
    sim.begin_drain();
    assert!(!sim.is_drained(), "{}", sim.trace());
    finish_client(&mut sim, lingering);
    assert_eq!(sim.state(lingering), SimConnState::Closed);
    // Only the orphaned open is left.
    assert!(!sim.is_drained(), "{}", sim.trace());
    sim.advance(Duration::from_millis(9));
    assert!(!sim.is_drained(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// A connection the driver delivers after the drain began (it should have
/// closed its listeners) is answered 503 and closed.
#[test]
fn a_connection_accepted_during_the_drain_is_refused() {
    let mut sim = sim();
    sim.begin_drain();
    let c = sim.connect(peer());
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        model_fixed(
            Status::ServiceUnavailable,
            Persistence::Close,
            BodyMode::Full,
            None
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::ShuttingDown,
        }],
        "{}",
        sim.trace()
    );
    finish_client(&mut sim, c);
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// The fast path: every connection closes at once, every upstream and
/// every open file exactly once (the sim checks it); each cut exchange is
/// logged `Rejected { ShuttingDown }`, idle and lingering connections are
/// cut silently (owner decision 2026-10-27, FR-PROC-05c); an orphaned open
/// stays until the disk answers it.
#[test]
fn abort_all_closes_everything_and_waits_for_orphans() {
    let mut sim = sim();
    // A download stalled mid-body: an open file.
    let download = sim.connect(peer());
    sim.set_client_window(download, Window::Budget(100));
    sim.send(download, GET_F);
    sim.run_until_idle();
    assert_eq!(sim.open_files(), 1);
    // A proxied request waiting for its response, and one connecting.
    let proxied = sim.connect(peer());
    sim.send(
        proxied,
        b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    sim.run_until_idle();
    // A proxied response being relayed to a stalled client.
    let relaying = sim.connect(peer());
    sim.set_client_window(relaying, Window::Budget(10));
    sim.send(
        relaying,
        b"GET /api/r HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    sim.run_until_idle();
    let relayed_up = *sim.upstreams().last().unwrap();
    // Half of the body: the upstream stays open.
    sim.backend_send(
        relayed_up,
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nok",
    );
    sim.run_until_idle();
    sim.set_connect("127.0.0.1:3000", ConnectBehaviour::Hang);
    let connecting = sim.connect(peer());
    sim.send(
        connecting,
        b"GET /api/y HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    sim.run_until_idle();
    assert_eq!(sim.open_upstreams(), 3);
    // A fixed response (a host miss, no disk) to a stalled client, a head
    // partly received, an idle connection; then an open in flight, a fresh
    // and a lingering connection.
    let responding = sim.connect(peer());
    sim.set_client_window(responding, Window::Budget(10));
    sim.send(responding, OTHER_HOST);
    let partial = sim.connect(peer());
    sim.send(partial, b"GET /f.txt HT");
    let idle = sim.connect(peer());
    sim.send(idle, OTHER_HOST);
    sim.run_until_idle();
    sim.set_disk_delay(Duration::from_millis(10));
    let opening = sim.connect(peer());
    sim.send(opening, GET_F);
    let fresh = sim.connect(peer());
    let lingering = sim.connect(peer());
    sim.send(lingering, b"GET / HTTP/1.1\nHost: a\n\n");
    sim.run_until_idle();
    assert_eq!(sim.state(lingering), SimConnState::WriteShutdown);
    let logs = sim.logs().len();
    sim.abort_all();
    let all = [
        download, proxied, relaying, connecting, responding, partial, idle, opening, fresh,
        lingering,
    ];
    for c in all {
        assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
        assert!(!sim.core_knows(c));
    }
    let cut: Vec<Log> = [
        download, proxied, relaying, connecting, responding, partial, opening,
    ]
    .into_iter()
    .map(|conn| Log::Rejected {
        conn,
        peer: peer(),
        reason: RejectReason::ShuttingDown,
    })
    .collect();
    // In `ConnId` order, as `abort_all` closes them.
    assert_eq!(&sim.logs()[logs..], cut.as_slice(), "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert!(!sim.is_drained(), "the orphaned open stays");
    sim.advance(Duration::from_millis(10));
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// A connection refused with 503 had no exchange: `abort_all` cuts it
/// silently, its refusal was logged already.
#[test]
fn abort_all_cuts_a_refused_connection_silently() {
    let mut sim = sim();
    sim.begin_drain();
    let refused = sim.connect(peer());
    sim.set_client_window(refused, Window::Budget(0));
    sim.run_until_idle();
    assert_eq!(sim.state(refused), SimConnState::Open, "{}", sim.trace());
    let logs = sim.logs().len();
    sim.abort_all();
    assert_eq!(sim.state(refused), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.logs().len(), logs, "{}", sim.trace());
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// Owner decision 2026-10-27 (FR-PROC-05b): a complete request pipelined
/// behind the response in flight when the drain begins is answered 503
/// with `Connection: close`, after that response (whose head was queued
/// keep-alive).
#[test]
fn a_request_pipelined_behind_the_drain_gets_503() {
    let mut sim = sim();
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Budget(1000));
    sim.send(c, &[GET_F, GET_F].concat());
    sim.run_until_idle();
    sim.begin_drain();
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    let logs = sim.logs().len();
    sim.set_client_window(c, Window::Unlimited);
    sim.run_until_idle();
    let mut expected = model_file_head(TEXT, 3000, Persistence::KeepAlive, None);
    expected.extend_from_slice(&[b'x'; 3000]);
    expected.extend(model_fixed(
        Status::ServiceUnavailable,
        Persistence::Close,
        BodyMode::Full,
        None,
    ));
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(
        &sim.logs()[logs..],
        &[Log::Response {
            conn: c,
            peer: peer(),
            status: Status::ServiceUnavailable,
            request: summary(Method::Get, "/f.txt"),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    finish_client(&mut sim, c);
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// A request with a body on a proxy route, completed during the drain: 503
/// with `Connection: close`, no upstream, the body is not read.
#[test]
fn a_proxy_request_with_a_body_during_the_drain_gets_503() {
    let mut sim = sim();
    let c = sim.connect(peer());
    sim.send(c, b"POST /api/x HTTP/1.1\r\n");
    sim.run_until_idle();
    sim.begin_drain();
    sim.send(c, b"Host: example.test\r\nContent-Length: 5\r\n\r\nhello");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        model_fixed(
            Status::ServiceUnavailable,
            Persistence::Close,
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
            status: Status::ServiceUnavailable,
            request: Some(RequestSummary {
                method: Method::Other("POST".to_owned()),
                host: Some("example.test".to_owned()),
                path: "/api/x".to_owned(),
            }),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert!(sim.upstreams().is_empty(), "no connect: {}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    finish_client(&mut sim, c);
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// The drain's 503 carries the `Date` the driver set (FR-HTTP-22a).
#[test]
fn the_drain_503_carries_the_date() {
    let mut sim = sim();
    sim.set_date(RFC_DATE.0);
    let c = sim.connect(peer());
    sim.send(c, b"GET /f.txt HTTP/1.1\r\n");
    sim.run_until_idle();
    sim.begin_drain();
    sim.send(c, b"Host: example.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        model_fixed(
            Status::ServiceUnavailable,
            Persistence::Close,
            BodyMode::Full,
            Some(RFC_DATE.1)
        ),
        "{}",
        sim.trace()
    );
}

/// A proxied response being relayed when the drain begins: its head was
/// queued keep-alive, it completes, then the connection closes; no new
/// exchange starts.
#[test]
fn a_relayed_response_completes_then_closes() {
    let mut sim = sim();
    let c = sim.connect(peer());
    sim.send(c, b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n");
    sim.run_until_idle();
    let up = *sim.upstreams().first().unwrap();
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nok");
    sim.run_until_idle();
    sim.begin_drain();
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.backend_send(up, b"ok");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\nokok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.upstreams().len(), 1, "no new exchange: {}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    finish_client(&mut sim, c);
    assert!(sim.is_drained(), "{}", sim.trace());
}

/// `handle` keeps working after `abort_all`: a stray accept is refused.
#[test]
fn the_core_still_answers_after_abort_all() {
    let mut sim = sim();
    sim.abort_all();
    assert!(sim.is_drained());
    let c = sim.connect(peer());
    sim.run_until_idle();
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::ShuttingDown,
        }],
        "{}",
        sim.trace()
    );
}
