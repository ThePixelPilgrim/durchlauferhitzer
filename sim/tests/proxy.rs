//! Proxy routes end to end: the upstream interface between core and
//! driver, with scripted backends behind it.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use durchlauferhitzer_core::config::BackendAddr;
use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::{BodyMode, render_fixed};
use durchlauferhitzer_core::http::response_head::ResponseError;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnId, Interest, Log, RequestSummary, UpstreamId,
};
use durchlauferhitzer_sim::{ConnectBehaviour, Sim, SimConnState, Window};

const BACKEND: &str = "127.0.0.1:3000";

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

/// As `CONFIG`, with room for a response larger than the 1 MiB limit (the
/// limit itself is Task 7's).
const LARGE: &str = "listen 127.0.0.1:8080;
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 4MiB; }
}";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn fixed(status: Status, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    let mut out = Vec::new();
    render_fixed(status, persistence, body, &mut out);
    out
}

fn summary(method: Method, path: &str) -> RequestSummary {
    RequestSummary {
        method,
        host: Some("example.test".to_owned()),
        path: path.to_owned(),
    }
}

/// The one upstream the core opened so far.
fn only_upstream(sim: &Sim) -> UpstreamId {
    let upstreams = sim.upstreams();
    assert_eq!(upstreams.len(), 1, "{}", sim.trace());
    upstreams.first().copied().unwrap_or(UpstreamId(0))
}

/// Connect a client, send `request` and pump until idle.
fn request(sim: &mut Sim, request: &[u8]) -> ConnId {
    let c = sim.connect(peer());
    sim.send(c, request);
    sim.run_until_idle();
    c
}

const GET_X: &[u8] = b"GET /api/x?y=1 HTTP/1.1\r\nHost: example.test\r\n\
    Accept: */*\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\n\r\n";

/// The rewrite of `GET_X` (FR-PROXY-04…11): prefix stripped, query kept,
/// hop-by-hop headers gone, the proxy's own headers appended.
const GET_X_FORWARDED: &[u8] = b"GET /x?y=1 HTTP/1.1\r\nhost: example.test\r\n\
    accept: */*\r\nx-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\n\
    connection: close\r\n\r\n";

#[test]
fn forwards_get_and_relays_response() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    assert_eq!(up, UpstreamId(1), "{}", sim.trace());
    assert_eq!(
        sim.take_backend_request(up),
        GET_X_FORWARDED,
        "{}",
        sim.trace()
    );
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.backend_send(
        up,
        b"HTTP/1.1 200 Fine\r\nServer: b\r\nKeep-Alive: timeout=5\r\n\
          Connection: keep-alive\r\nContent-Length: 5\r\n\r\nhello",
    );
    sim.run_until_idle();
    // FR-PROXY-12: status line re-rendered with the backend's reason,
    // hop-by-hop headers removed, `Server` and `Content-Length` as sent.
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 Fine\r\nserver: b\r\ncontent-length: 5\r\n\r\nhello",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
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
    // Keep-alive: the next request gets a fresh upstream with the next id.
    sim.send(c, b"DELETE /api HTTP/1.1\r\nHost: example.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.upstreams(),
        [UpstreamId(1), UpstreamId(2)],
        "{}",
        sim.trace()
    );
    let up = UpstreamId(2);
    assert_eq!(
        sim.take_backend_request(up),
        b"DELETE / HTTP/1.1\r\nhost: example.test\r\nx-forwarded-for: 192.0.2.1\r\n\
          x-forwarded-proto: http\r\nconnection: close\r\n\r\n",
        "{}",
        sim.trace()
    );
    sim.backend_send(up, b"HTTP/1.0 204 \r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 204 \r\n\r\n",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// Review Focus 3: a body far larger than the buffers, to a slow client.
/// The sim checks after every step that buffers stay within capacity and
/// that a body read is offered only into a drained buffer, with its full
/// capacity (NFR-RES-04).
#[test]
fn large_content_length_body_is_streamed_intact() {
    let mut sim = Sim::new(LARGE);
    let c = request(&mut sim, GET_X);
    sim.set_client_window(c, Window::Bytes(1000));
    let up = only_upstream(&sim);
    let body: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
    let mut response = head.clone().into_bytes();
    response.extend_from_slice(&body);
    sim.backend_send(up, &response);
    sim.run_until_idle();
    let received = sim.take_received(c);
    let expected_head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
    assert_eq!(
        received.get(..expected_head.len()),
        Some(expected_head.as_bytes())
    );
    assert!(
        received.get(expected_head.len()..) == Some(body.as_slice()),
        "body differs: {} bytes received",
        received.len()
    );
    assert_eq!(sim.open_upstreams(), 0);
    assert_eq!(sim.state(c), SimConnState::Open);
    // Every read but the last filled the whole buffer.
    let reads: Vec<usize> = sim
        .trace()
        .lines()
        .filter_map(|l| l.split_once(" UpstreamReceived len="))
        .map(|(_, len)| len.parse().unwrap())
        .collect();
    let (last, full) = reads.split_last().unwrap();
    let capacity = full[0];
    assert!(capacity >= 16 * 1024, "{capacity}");
    assert!(full.iter().all(|&n| n == capacity), "{reads:?}");
    assert!(*last <= capacity);
    assert_eq!(reads.iter().sum::<usize>(), response.len());
}

fn assert_502(sim: &Sim, c: ConnId, kind: BackendFailure, received: Vec<u8>) {
    assert_eq!(
        received,
        fixed(Status::BadGateway, Persistence::KeepAlive, BodyMode::Full),
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
                kind,
            },
            Log::Response {
                conn: c,
                peer: peer(),
                status: Status::BadGateway,
                request: Some(summary(Method::Get, "/api/x")),
            },
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

#[test]
fn connect_refused_is_502() {
    let mut sim = Sim::new(CONFIG);
    sim.set_connect(BACKEND, ConnectBehaviour::Refuse);
    let c = request(&mut sim, GET_X);
    let received = sim.take_received(c);
    assert_502(&sim, c, BackendFailure::Refused, received);
}

#[test]
fn backend_reset_before_head_is_502() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Le");
    sim.run_until_idle();
    sim.backend_reset(up);
    sim.run_until_idle();
    let received = sim.take_received(c);
    assert_502(&sim, c, BackendFailure::Reset, received);
}

/// A backend that closes before its head is complete is a failed backend
/// too (logged as a reset: the connection ended early).
#[test]
fn backend_eof_before_head_is_502() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\n");
    sim.backend_eof(up);
    sim.run_until_idle();
    let received = sim.take_received(c);
    assert_502(&sim, c, BackendFailure::Reset, received);
}

#[test]
fn malformed_backend_head_is_502() {
    let cases: [(&[u8], ResponseError); 4] = [
        (
            b"HTTP/1.1 200 OK\nContent-Length: 0\r\n\r\n",
            ResponseError::BareLf,
        ),
        (b"HTTP/2 200 OK\r\n\r\n", ResponseError::BadVersion),
        // obs-text in a header value (FR-PROXY-13).
        (
            b"HTTP/1.1 200 OK\r\nX-Name: caf\xc3\xa9\r\nContent-Length: 0\r\n\r\n",
            ResponseError::BadHeaderValue,
        ),
        // FR-HTTP-09a.
        (
            b"HTTP/1.1 200 OK\r\nConnection: content-length\r\nContent-Length: 0\r\n\r\n",
            ResponseError::ConnectionNamesFraming,
        ),
    ];
    for (response, error) in cases {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, response);
        sim.run_until_idle();
        let received = sim.take_received(c);
        assert_502(&sim, c, BackendFailure::BadResponse(error), received);
    }
}

/// Review Focus 5: the client goes away while the connect is pending; the
/// upstream is closed at once and nothing more happens for it.
#[test]
fn client_reset_while_connecting_closes_upstream() {
    let mut sim = Sim::new(CONFIG);
    sim.set_connect(BACKEND, ConnectBehaviour::Hang);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    sim.reset(c);
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Reset {
            conn: c,
            peer: peer()
        }],
        "{}",
        sim.trace()
    );
    let trace = sim.trace();
    let close = format!("up={} -> CloseUpstream", up.0);
    let at = |needle: &str| trace.find(needle).unwrap_or(usize::MAX);
    assert!(at("-> Log Reset") < at(&close), "{trace}");
    assert!(at(&close) < at("conn=1 -> Close"), "{trace}");
    sim.advance(Duration::from_secs(120));
    assert!(sim.take_received(c).is_empty());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
}

/// The same while the connect has succeeded and the request was written.
#[test]
fn client_reset_while_awaiting_response_closes_upstream() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    assert_eq!(sim.take_backend_request(up), GET_X_FORWARDED);
    sim.reset(c);
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
}

#[test]
fn backends_are_listed() {
    let config = "listen 127.0.0.1:8080;
host a.test {
  path / { static /srv/www; }
  path /a { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 1MiB; }
  path /b { proxy [::1]:4000; max_request_body 1MiB; max_response_body 1MiB; }
  path /c { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 1MiB; }
}
host b.test {
  path /d { proxy backend.internal:80; max_request_body 1MiB; max_response_body 1MiB; }
}";
    let mut sim = Sim::new(config);
    let addr = |host: &str, port| BackendAddr {
        host: host.to_owned(),
        port,
    };
    assert_eq!(
        sim.backends(),
        [
            addr("127.0.0.1", 3000),
            addr("::1", 4000),
            addr("backend.internal", 80),
        ]
    );
    // Requests connect by the id of their route's backend.
    request(&mut sim, b"GET /d HTTP/1.1\r\nHost: b.test\r\n\r\n");
    request(&mut sim, b"GET /c HTTP/1.1\r\nHost: a.test\r\n\r\n");
    request(&mut sim, b"GET /b HTTP/1.1\r\nHost: a.test\r\n\r\n");
    let backends: Vec<BackendId> = sim
        .upstreams()
        .into_iter()
        .map(|up| sim.upstream_backend(up))
        .collect();
    assert_eq!(backends, [BackendId(2), BackendId(0), BackendId(1)]);
}

/// FR-PROXY-16: a response to HEAD has no body, whatever it declares.
#[test]
fn head_request_gets_head_only() {
    let mut sim = Sim::new(CONFIG);
    let c = request(
        &mut sim,
        b"HEAD /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// FR-PROXY-28: a request that asked to close gets `Connection: close` on
/// the proxied response, and the connection closes after it.
#[test]
fn close_requested_by_client_is_kept() {
    let mut sim = Sim::new(CONFIG);
    let c = request(
        &mut sim,
        b"GET /api/x HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// FR-PROXY-22: bytes after the `Content-Length` body are never forwarded.
#[test]
fn bytes_after_content_length_body_are_dropped() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(
        up,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokHTTP/1.1 200 OK\r\n\r\n",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// Controller ruling 1: a request that may not be forwarded is answered
/// 400 and closed, with no connect.
#[test]
fn duplicate_forwarded_for_is_400_without_connect() {
    let mut sim = Sim::new(CONFIG);
    let c = request(
        &mut sim,
        b"GET /api/x HTTP/1.1\r\nHost: example.test\r\nX-Forwarded-For: a\r\n\
          X-Forwarded-For: b\r\n\r\n",
    );
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadRequest, Persistence::Close, BodyMode::Full)
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown);
}

/// Request bodies are Task 6's: still 501, with no connect.
#[test]
fn request_with_body_is_still_501() {
    let mut sim = Sim::new(CONFIG);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 2\r\n\r\nhi",
    );
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotImplemented, Persistence::Close, BodyMode::Full)
    );
}

/// Chunked and close-delimited responses are Task 7's: still 501, and the
/// upstream is closed.
#[test]
fn other_response_framings_are_still_501() {
    for response in [
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\n\r\nuntil close",
    ] {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, response);
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            fixed(Status::NotImplemented, Persistence::Close, BodyMode::Full),
            "{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    }
}

fn assert_504(sim: &Sim, c: ConnId, kind: BackendFailure) {
    assert_eq!(
        sim.logs(),
        &[
            Log::BackendFailed {
                conn: c,
                peer: peer(),
                backend: BackendId(0),
                kind,
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
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// Every proxy state carries a deadline (the spec's defaults until Task 8
/// makes them limits): a connect that never completes is a 504 after 5 s.
#[test]
fn hung_connect_is_504() {
    let mut sim = Sim::new(CONFIG);
    sim.set_connect(BACKEND, ConnectBehaviour::Hang);
    let c = request(&mut sim, GET_X);
    sim.advance(Duration::from_millis(4999));
    assert!(sim.take_received(c).is_empty());
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

/// A backend that reads the request and never answers: 504 after 60 s.
#[test]
fn silent_backend_is_504() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    sim.advance(Duration::from_secs(59));
    assert!(sim.take_received(c).is_empty());
    sim.advance(Duration::from_secs(1));
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

/// A backend that stops reading the request head: the response timeout
/// applies too.
#[test]
fn backend_not_reading_the_request_is_504() {
    let mut sim = Sim::new(CONFIG);
    sim.set_connect(BACKEND, ConnectBehaviour::Accept);
    let c = sim.connect(peer());
    sim.send(c, GET_X);
    // The backend accepts the connection but takes no bytes.
    sim.set_default_backend_window(Window::Stalled);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    assert!(sim.take_backend_request(up).is_empty());
    sim.advance(Duration::from_secs(60));
    assert_504(&sim, c, BackendFailure::ResponseTimeout);
}

/// After the head reached the client, a stalled backend cuts the
/// connection: the client sees an incomplete body, never a short one.
#[test]
fn stalled_body_after_head_cuts_the_connection() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhal");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nhal"
    );
    sim.advance(Duration::from_secs(30));
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
