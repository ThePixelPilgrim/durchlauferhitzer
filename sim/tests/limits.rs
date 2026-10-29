//! Timeouts, connection caps and lingering close through the simulation.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{ConnId, LimitKind, Log, RejectReason, RequestSummary, TimeoutKind};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

const CONFIG: &str = "listen 127.0.0.1:8080;
limits {
  header_timeout 10s;
  keepalive_timeout 60s;
  send_idle_timeout 30s;
  max_connections 4;
  max_connections_per_ip 2;
}
host example.test { path / { static /srv/www; } }";

/// A request for an unknown host: answered 404, connection kept alive.
const REQUEST: &[u8] = b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n";

fn peer(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn millis(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[allow(dead_code, reason = "shared by all test binaries; each uses part")]
#[path = "support/model_http.rs"]
mod model_http;

/// A fixed response, by the spec-written model, not the core's renderer.
fn fixed(status: Status, persistence: Persistence) -> Vec<u8> {
    model_http::model_fixed(status, persistence, BodyMode::Full, None)
}

fn not_found_response(conn: ConnId, peer: IpAddr) -> Log {
    Log::Response {
        conn,
        peer,
        status: Status::NotFound,
        request: Some(RequestSummary {
            method: Method::Get,
            host: Some("other.test".to_owned()),
            path: "/x".to_owned(),
        }),
    }
}

/// Send `REQUEST` on `c` and check it is answered 404 keep-alive.
fn assert_served(sim: &mut Sim, c: ConnId) {
    sim.send(c, REQUEST);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::KeepAlive),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// Check `c` from `peer` was refused with 503 and is lingering.
fn assert_refused(sim: &mut Sim, c: ConnId, peer: IpAddr, kind: LimitKind) {
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::ServiceUnavailable, Persistence::Close),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: c,
            peer,
            reason: RejectReason::Limit(kind),
        }),
        "{}",
        sim.trace()
    );
}

#[test]
fn trickling_head_times_out_at_total_deadline() {
    let mut sim = Sim::new(CONFIG);
    let p = peer(1);
    let c = sim.connect(p);
    // One byte every 2 s: t = 0, 2, 4, 6, 8. None of them extends the
    // deadline set at accept.
    for byte in b"GET /" {
        sim.send(c, &[*byte]);
        sim.run_until_idle();
        sim.advance(secs(2) - millis(1));
        assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
        assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
        sim.advance(millis(1));
    }
    // Now t = 10 s: the total header deadline has passed.
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::RequestTimeout, Persistence::Close),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: p,
            reason: RejectReason::Timeout(TimeoutKind::Header),
        }],
        "{}",
        sim.trace()
    );
    // The 408 is followed by a lingering close: 2 s later the socket closes.
    sim.advance(secs(2) - millis(1));
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.advance(millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
}

#[test]
fn silent_idle_connection_is_closed_without_response() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    sim.run_until_idle();
    sim.advance(secs(10) - millis(1));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert!(sim.logs().is_empty(), "{}", sim.trace());
}

#[test]
fn keepalive_idle_closes_after_60s() {
    let mut sim = Sim::new(CONFIG);
    let p = peer(1);
    let c = sim.connect(p);
    sim.advance(secs(5));
    assert_served(&mut sim, c);
    sim.advance(secs(59));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(secs(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.logs(), &[not_found_response(c, p)], "{}", sim.trace());
}

#[test]
fn pipelined_leftover_gets_a_fresh_header_deadline() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    sim.advance(secs(8));
    // The first request is answered at t = 8; the leftover starts the next
    // head, whose deadline is t = 18, not the accept deadline t = 10.
    let mut bytes = REQUEST.to_vec();
    bytes.extend_from_slice(b"GET");
    sim.send(c, &bytes);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::KeepAlive),
        "{}",
        sim.trace()
    );
    sim.advance(secs(10) - millis(1));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(Status::RequestTimeout, Persistence::Close),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

#[test]
fn first_byte_after_idle_starts_a_fresh_header_deadline() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    assert_served(&mut sim, c);
    sim.advance(secs(30));
    sim.send(c, b"G");
    sim.run_until_idle();
    sim.advance(secs(10) - millis(1));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(Status::RequestTimeout, Persistence::Close),
        "{}",
        sim.trace()
    );
}

#[test]
fn stalled_reader_is_closed_after_send_idle_timeout() {
    let mut sim = Sim::new(CONFIG);
    let p = peer(1);
    let c = sim.connect(p);
    sim.set_client_window(c, Window::Stalled);
    sim.send(c, REQUEST);
    sim.run_until_idle();
    sim.advance(secs(30) - millis(1));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            not_found_response(c, p),
            Log::Rejected {
                conn: c,
                peer: p,
                reason: RejectReason::Timeout(TimeoutKind::Send),
            }
        ],
        "{}",
        sim.trace()
    );
}

#[test]
fn slow_reader_stays_open_while_bytes_keep_moving() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    sim.set_client_window(c, Window::Stalled);
    sim.send(c, REQUEST);
    sim.run_until_idle();
    // The client takes one byte every 20 s: each `Sent` resets the 30 s
    // send idle deadline, so after 100 s the connection is still open.
    let mut got = Vec::new();
    for _ in 0..5 {
        sim.advance(secs(20));
        assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
        sim.set_client_window(c, Window::Budget(1));
        sim.run_until_idle();
        got.extend(sim.take_received(c));
    }
    assert_eq!(got.len(), 5, "{}", sim.trace());
    // With `Bytes(1)` the rest flows one byte per write and arrives whole.
    sim.set_client_window(c, Window::Bytes(1));
    sim.run_until_idle();
    got.extend(sim.take_received(c));
    assert_eq!(
        got,
        fixed(Status::NotFound, Persistence::KeepAlive),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

#[test]
fn per_ip_cap_answers_503() {
    let mut sim = Sim::new(CONFIG);
    let a = peer(1);
    let a1 = sim.connect(a);
    let a2 = sim.connect(a);
    let a3 = sim.connect(a);
    assert_refused(&mut sim, a3, a, LimitKind::ConnectionsPerIp);
    let b1 = sim.connect(peer(2));
    assert_served(&mut sim, b1);
    assert_served(&mut sim, a1);
    assert_served(&mut sim, a2);
    // Closing one of the first two frees a slot for that IP (the refused,
    // still lingering a3 does not count).
    sim.send_eof(a1);
    sim.run_until_idle();
    assert_eq!(sim.state(a1), SimConnState::Closed, "{}", sim.trace());
    let a4 = sim.connect(a);
    assert_served(&mut sim, a4);
    let a5 = sim.connect(a);
    assert_refused(&mut sim, a5, a, LimitKind::ConnectionsPerIp);
}

#[test]
fn global_cap_answers_503() {
    let mut sim = Sim::new(CONFIG);
    let first = sim.connect(peer(1));
    let counted = [
        first,
        sim.connect(peer(2)),
        sim.connect(peer(3)),
        sim.connect(peer(4)),
    ];
    let refused = sim.connect(peer(5));
    assert_refused(&mut sim, refused, peer(5), LimitKind::Connections);
    let refused_again = sim.connect(peer(6));
    assert_refused(&mut sim, refused_again, peer(6), LimitKind::Connections);
    for c in &counted {
        assert_served(&mut sim, *c);
    }
    // Refused connections linger with the normal deadline and close.
    sim.advance(secs(2));
    assert_eq!(sim.state(refused), SimConnState::Closed, "{}", sim.trace());
    // Closing a counted connection frees a slot.
    sim.send_eof(first);
    sim.run_until_idle();
    let next = sim.connect(peer(7));
    assert_served(&mut sim, next);
    let over = sim.connect(peer(8));
    assert_refused(&mut sim, over, peer(8), LimitKind::Connections);
}

#[test]
fn counts_are_released_on_reset_and_timeout() {
    let mut sim = Sim::new(CONFIG);
    let a = peer(1);
    let a1 = sim.connect(a);
    let a2 = sim.connect(a);
    sim.reset(a1);
    let a3 = sim.connect(a);
    assert_served(&mut sim, a3);
    let over = sim.connect(a);
    assert_refused(&mut sim, over, a, LimitKind::ConnectionsPerIp);
    // a2 never sends and times out silently at t = 10; a3 stays (keep-alive
    // deadline t = 60).
    sim.advance(secs(10));
    assert_eq!(sim.state(a2), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.state(a3), SimConnState::Open, "{}", sim.trace());
    let a4 = sim.connect(a);
    assert_served(&mut sim, a4);
}

#[test]
fn unread_body_is_drained_before_close() {
    let mut sim = Sim::new(CONFIG);
    let p = peer(1);
    let c = sim.connect(p);
    sim.send(
        c,
        b"GET /nowhere HTTP/1.1\r\nHost: other.test\r\nContent-Length: 100000\r\n\r\n",
    );
    let piece = [b'x'; 1024];
    sim.send(c, &piece);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::Close),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    let shutdown = sim.trace().find("-> ShutdownWrite").unwrap_or(0);
    let last_sent = sim.trace().rfind("Sent len=").unwrap_or(usize::MAX);
    assert!(last_sent < shutdown, "{}", sim.trace());
    // Further body bytes are read and discarded; they do not extend the 2 s
    // linger deadline.
    for _ in 0..19 {
        sim.advance(millis(100));
        sim.send(c, &piece);
        sim.run_until_idle();
        assert_eq!(sim.queued_to_server(c), 0, "{}", sim.trace());
        assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    }
    sim.advance(millis(99));
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.advance(millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
}

#[test]
fn unread_body_linger_ends_on_client_eof() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    sim.send(
        c,
        b"GET /nowhere HTTP/1.1\r\nHost: other.test\r\nContent-Length: 100000\r\n\r\n",
    );
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.send(c, &[b'x'; 1024]);
    sim.run_until_idle();
    assert_eq!(sim.queued_to_server(c), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
}

#[test]
fn refused_connections_beyond_the_refused_cap_are_closed_at_once() {
    let mut sim = Sim::new(CONFIG);
    for i in 1..=4 {
        sim.connect(peer(i));
    }
    // max_connections (4) refused connections may linger at once.
    for i in 5..=8 {
        let c = sim.connect(peer(i));
        assert_refused(&mut sim, c, peer(i), LimitKind::Connections);
    }
    let over = sim.connect(peer(9));
    assert_eq!(sim.state(over), SimConnState::Closed, "{}", sim.trace());
    sim.run_until_idle();
    assert!(sim.take_received(over).is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: over,
            peer: peer(9),
            reason: RejectReason::Limit(LimitKind::Connections),
        }),
        "{}",
        sim.trace()
    );
    // Once the lingering ones are gone, refusals get their 503 again.
    sim.advance(secs(2));
    let next = sim.connect(peer(10));
    assert_refused(&mut sim, next, peer(10), LimitKind::Connections);
}

#[test]
fn ipv4_mapped_peer_shares_the_ipv4_slot() {
    let mut sim = Sim::new(CONFIG);
    let v4 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let mapped = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped());
    let a = sim.connect(mapped);
    let b = sim.connect(v4);
    assert_served(&mut sim, a);
    assert_served(&mut sim, b);
    let c = sim.connect(mapped);
    assert_refused(&mut sim, c, mapped, LimitKind::ConnectionsPerIp);
}

/// Two connections per peer group, ten in all: only the per-IP limit
/// binds.
const GROUPED: &str = "listen 127.0.0.1:8080;
limits {
  header_timeout 10s;
  keepalive_timeout 60s;
  send_idle_timeout 30s;
  max_connections 10;
  max_connections_per_ip 2;
}
host example.test { path / { static /srv/www; } }";

/// Address `host` in the /64 `2001:db8:0:<net>::/64`.
fn v6(net: u16, host: u16) -> IpAddr {
    IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, net, 0, 0, 0, host))
}

/// FR-CONN-13: IPv6 peers in one /64 (the default `ipv6_prefix`) share
/// `max_connections_per_ip`, and hold at most that many refused
/// connections at once; another /64 is counted apart.
#[test]
fn ipv6_peers_are_counted_by_prefix() {
    let mut sim = Sim::new(GROUPED);
    let a1 = sim.connect(v6(1, 1));
    let a2 = sim.connect(v6(1, 0xffff));
    // Two refusals of group A get their 503s.
    for host in [3, 4] {
        let c = sim.connect(v6(1, host));
        assert_refused(&mut sim, c, v6(1, host), LimitKind::ConnectionsPerIp);
    }
    // The third is closed at once, unanswered, logged as rejected.
    let over = sim.connect(v6(1, 5));
    assert_eq!(sim.state(over), SimConnState::Closed, "{}", sim.trace());
    sim.run_until_idle();
    assert!(sim.take_received(over).is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: over,
            peer: v6(1, 5),
            reason: RejectReason::Limit(LimitKind::ConnectionsPerIp),
        }),
        "{}",
        sim.trace()
    );
    // Group B: its own two slots, then its own 503.
    let b1 = sim.connect(v6(2, 1));
    let b2 = sim.connect(v6(2, 1));
    let b3 = sim.connect(v6(2, 2));
    assert_refused(&mut sim, b3, v6(2, 2), LimitKind::ConnectionsPerIp);
    for c in [a1, a2, b1, b2] {
        assert_served(&mut sim, c);
    }
    let served: Vec<Log> = [
        (a1, v6(1, 1)),
        (a2, v6(1, 0xffff)),
        (b1, v6(2, 1)),
        (b2, v6(2, 1)),
    ]
    .into_iter()
    .map(|(c, p)| not_found_response(c, p))
    .collect();
    assert!(sim.logs().ends_with(&served), "{}", sim.trace());
    // Closing a1 frees a slot for any address of group A.
    sim.send_eof(a1);
    sim.run_until_idle();
    assert_eq!(sim.state(a1), SimConnState::Closed, "{}", sim.trace());
    let a6 = sim.connect(v6(1, 6));
    assert_served(&mut sim, a6);
}

/// A head of nearly `max_head` bytes arriving one header line per read is
/// parsed correctly; the parser resumes rather than starting over on each
/// read.
#[test]
fn large_head_sent_line_by_line_is_answered() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    let mut head = b"GET /x HTTP/1.1\r\nHost: other.test\r\n".to_vec();
    let line_len = 160;
    // 16 KiB default max_head, 100 headers max: 98 padding lines fit.
    for i in 0..98 {
        let mut line = format!("X-Pad-{i:03}: ").into_bytes();
        line.resize(line_len - 2, b'p');
        line.extend_from_slice(b"\r\n");
        head.extend_from_slice(&line);
    }
    head.extend_from_slice(b"\r\n");
    assert!(head.len() <= 16 * 1024);
    sim.set_read_chunk(c, line_len);
    sim.send(c, &head);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::KeepAlive),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.logs().last(), Some(&not_found_response(c, peer(1))));
    // The connection is reusable for a normal request afterwards.
    sim.set_read_chunk(c, 4096);
    assert_served(&mut sim, c);
}
