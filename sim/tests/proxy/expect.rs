//! `Expect` on every route (FR-PROXY-25, FR-PROXY-25a).

use std::time::Duration;

use durchlauferhitzer_core::http::request::{HttpError, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{Log, RejectReason, TimeoutKind};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

use super::{fixed, only_upstream, peer, request};

/// A proxy route next to a static one.
const MIXED: &str = "listen 127.0.0.1:8080;
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
  path / { static /srv; }
}";

/// Any `Expect` other than exactly one `100-continue` is answered 417 and
/// closed, on proxy and static routes and for route misses alike, with no
/// connect (owner decision D3: when in doubt, reject).
#[test]
fn other_expect_is_417() {
    let expects = [
        "Expect: foo\r\n",
        "Expect: 100-continue, foo\r\n",
        "Expect: 100-continue\r\nExpect: 100-continue\r\n",
    ];
    for target in ["/api/x", "/static.txt"] {
        for host in ["example.test", "elsewhere.test"] {
            for expect in expects {
                let mut sim = Sim::new(MIXED);
                let raw = format!(
                    "POST {target} HTTP/1.1\r\nHost: {host}\r\n{expect}Content-Length: 2\r\n\r\nhi"
                );
                let c = request(&mut sim, raw.as_bytes());
                let context = format!("{target} {host} {expect:?}\n{}", sim.trace());
                assert!(sim.upstreams().is_empty(), "{context}");
                assert_eq!(
                    sim.take_received(c),
                    fixed(
                        Status::ExpectationFailed,
                        Persistence::Close,
                        BodyMode::Full
                    ),
                    "{context}"
                );
                assert_eq!(
                    sim.logs(),
                    &[Log::Rejected {
                        conn: c,
                        peer: peer(),
                        reason: RejectReason::Http(HttpError::BadExpect),
                    }],
                    "{context}"
                );
                assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{context}");
            }
        }
    }
}

/// HTTP/1.0 has no `Expect`: whatever its value, it is ignored, and the
/// request is forwarded as usual (without the header, FR-PROXY-06).
#[test]
fn http10_ignores_expect() {
    let mut sim = Sim::new(MIXED);
    let c = request(
        &mut sim,
        b"GET /api/x HTTP/1.0\r\nHost: example.test\r\nExpect: foo\r\n\r\n",
    );
    let up = super::only_upstream(&sim);
    assert_eq!(
        sim.take_backend_request(up),
        b"GET /x HTTP/1.0\r\nhost: example.test\r\nx-forwarded-for: 192.0.2.1\r\n\
          x-forwarded-proto: http\r\nconnection: close\r\n\r\n",
        "{}",
        sim.trace()
    );
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
}

const SHORT: &str = "listen 127.0.0.1:8080;
limits { send_idle_timeout 7s; body_idle_timeout 4s; backend_response_timeout 20s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

/// FR-PROXY-25 (builder call, stage 3 Task 9): a client that never reads
/// its `100 Continue` is not read either while it is queued, so its body
/// stays unread; after `body_idle_timeout` without body progress the
/// client is at fault: a 408 (queued behind the `100`), the upstream
/// closed. A client that still takes nothing is closed when the send idle
/// timeout that started with the `100` runs out: it carries over to the
/// 408, which restarts it only once the client takes a byte. (Here the
/// send idle timeout, 7 s, is the later one; see
/// `unread_100_continue_hits_the_send_idle_timeout_first`.)
#[test]
fn unread_100_continue_stalls_the_body_into_a_408() {
    let mut sim = Sim::new(SHORT);
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    sim.send(
        c,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 10\r\n\r\n",
    );
    sim.run_until_idle();
    sim.send(c, b"0123456789");
    sim.run_until_idle();
    let up = only_upstream(&sim);
    let forwarded = sim.take_backend_request(up);
    assert!(forwarded.ends_with(b"\r\n\r\n"), "{}", sim.trace());
    sim.advance(Duration::from_millis(3999));
    assert!(sim.logs().is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    let stalled = Log::Rejected {
        conn: c,
        peer: peer(),
        reason: RejectReason::Timeout(TimeoutKind::RequestBody),
    };
    assert_eq!(
        sim.logs(),
        std::slice::from_ref(&stalled),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    // No body byte reached the backend.
    assert!(sim.take_backend_request(up).is_empty(), "{}", sim.trace());
    // The send idle deadline runs on from the `100` (queued at 0 s): the
    // client has taken nothing, so it closes at 7 s, not 4 s + 7 s.
    sim.advance(Duration::from_millis(2999));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            stalled,
            Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Timeout(TimeoutKind::Send),
            },
        ],
        "{}",
        sim.trace()
    );
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
}

/// As `SHORT`, but the client's send idle timeout (3 s) comes before the
/// body idle timeout (6 s).
const SHORT_SEND: &str = "listen 127.0.0.1:8080;
limits { send_idle_timeout 3s; body_idle_timeout 6s; backend_response_timeout 20s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

/// FR-PROXY-29: the client's `send_idle_timeout` still applies while the
/// response is awaited. A client that takes nothing of its queued `100
/// Continue` for 3 s is cut then (a send timeout, the upstream released),
/// not left until the 6 s body idle timeout.
#[test]
fn unread_100_continue_hits_the_send_idle_timeout_first() {
    let mut sim = Sim::new(SHORT_SEND);
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    sim.send(
        c,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 10\r\n\r\n",
    );
    sim.run_until_idle();
    sim.send(c, b"0123456789");
    sim.run_until_idle();
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    sim.advance(Duration::from_millis(2999));
    assert!(sim.logs().is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::Send),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
}

/// A client that takes its `100 Continue` slowly but steadily keeps the
/// exchange alive: each byte taken restarts the send idle timeout (5 s
/// pass, more than the 3 s send idle timeout, less than the 6 s body
/// idle timeout).
#[test]
fn slowly_read_100_continue_restarts_the_send_idle_timeout() {
    let mut sim = Sim::new(SHORT_SEND);
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    sim.send(
        c,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 10\r\n\r\n",
    );
    sim.run_until_idle();
    for _ in 0..2 {
        sim.advance(Duration::from_millis(2500));
        sim.set_client_window(c, Window::Budget(1));
        sim.run_until_idle();
    }
    assert!(sim.logs().is_empty(), "{}", sim.trace());
    assert_eq!(sim.take_received(c), b"HT", "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
}

/// Send idle 7 s, a response head cap of 3 s, body idle 20 s.
const CAP_FIRST: &str = "listen 127.0.0.1:8080;
limits { send_idle_timeout 7s; body_idle_timeout 20s; backend_response_timeout 3s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

/// The send idle deadline of an unread `100 Continue` carries over to the
/// 504 queued behind it (stage 3 Task 9 builder call): the backend's
/// partial head runs out its 3 s cap, and the client, which has taken
/// nothing since the `100` was queued, is closed at 7 s.
#[test]
fn unread_100_continue_carries_its_send_deadline_into_a_504() {
    let mut sim = Sim::new(CAP_FIRST);
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    sim.send(
        c,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 10\r\n\r\n",
    );
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\n");
    sim.run_until_idle();
    sim.advance(Duration::from_secs(3));
    assert_eq!(sim.logs().len(), 2, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    sim.advance(Duration::from_millis(3999));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::Send),
        }),
        "{}",
        sim.trace()
    );
}

/// The same carry-over when a proxied response is queued behind the
/// unread `100 Continue`: the client is closed 7 s after the `100` was
/// queued, though the response came at 2 s.
#[test]
fn unread_100_continue_carries_its_send_deadline_into_a_relay() {
    let mut sim = Sim::new(CAP_FIRST);
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    sim.send(
        c,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 10\r\n\r\n",
    );
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.advance(Duration::from_secs(2));
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    sim.run_until_idle();
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
    sim.advance(Duration::from_millis(4999));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::Send),
        }),
        "{}",
        sim.trace()
    );
}

/// A config with the given send idle, response and body idle timeouts.
fn timeouts(send: u64, response: u64, body: u64) -> String {
    format!(
        "listen 127.0.0.1:8080;
limits {{ send_idle_timeout {send}s; backend_response_timeout {response}s; body_idle_timeout {body}s; }}
host example.test {{
  path /api {{ proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }}
}}"
    )
}

const POST_EXPECT: &[u8] =
    b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
    Content-Length: 10\r\n\r\n";

/// The client never reads its `100 Continue`; whatever else falls due at
/// the same instant, the send timeout is the outcome (FR-PROXY-29 builder
/// call): exactly `Rejected { Timeout(Send) }`, no 504 or 408 logged.
fn assert_send_timeout_at(sim: &mut Sim, c: durchlauferhitzer_core::ConnId, ms: u64) {
    sim.advance(Duration::from_millis(ms - 1));
    assert!(sim.logs().is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::Send),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// Send idle and the head cap both at 3 s (body idle 20 s): the send
/// timeout wins.
#[test]
fn send_timeout_tied_with_head_cap_wins() {
    let mut sim = Sim::new(&timeouts(3, 3, 20));
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    sim.send(c, POST_EXPECT);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\n");
    sim.run_until_idle();
    assert_send_timeout_at(&mut sim, c, 3000);
}

/// Send idle, the head cap and body idle all at 3 s: the send timeout
/// wins.
#[test]
fn three_way_tie_goes_to_the_send_timeout() {
    let mut sim = Sim::new(&timeouts(3, 3, 3));
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    sim.send(c, POST_EXPECT);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\n");
    sim.run_until_idle();
    assert_send_timeout_at(&mut sim, c, 3000);
}

/// The whole request (its body came with the head) is written, so
/// `backend_response_timeout` runs; send idle and it are both 3 s: the
/// send timeout wins.
#[test]
fn send_timeout_tied_with_response_timeout_wins() {
    let mut sim = Sim::new(&timeouts(3, 3, 20));
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Stalled);
    let mut whole = POST_EXPECT.to_vec();
    whole.extend_from_slice(b"0123456789");
    sim.send(c, &whole);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    assert!(
        sim.take_backend_request(up).ends_with(b"0123456789"),
        "{}",
        sim.trace()
    );
    assert_send_timeout_at(&mut sim, c, 3000);
}

/// Once the client has taken its whole `100 Continue`, the send idle
/// timeout stops: a body uploaded slowly (a byte every 2.5 s, past the
/// 3 s send idle timeout, within the 5 s body idle timeout) goes on.
#[test]
fn continue_taken_stops_the_send_idle_timeout() {
    let mut sim = Sim::new(&timeouts(3, 20, 5));
    let c = sim.connect(peer());
    sim.send(c, POST_EXPECT);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 100 Continue\r\n\r\n",
        "{}",
        sim.trace()
    );
    let up = only_upstream(&sim);
    for byte in b"0123" {
        sim.advance(Duration::from_millis(2500));
        sim.send(c, &[*byte]);
        sim.run_until_idle();
    }
    assert!(sim.logs().is_empty(), "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    assert!(
        sim.take_backend_request(up).ends_with(b"0123"),
        "{}",
        sim.trace()
    );
}
