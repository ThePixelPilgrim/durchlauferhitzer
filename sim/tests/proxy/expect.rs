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
/// closed. A client that still takes nothing is then closed by
/// `send_idle_timeout`.
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
    sim.advance(Duration::from_secs(7));
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
