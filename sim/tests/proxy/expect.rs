//! `Expect` on every route (FR-PROXY-25, FR-PROXY-25a).

use durchlauferhitzer_core::http::request::{HttpError, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{Log, RejectReason};
use durchlauferhitzer_sim::{Sim, SimConnState};

use super::{fixed, peer, request};

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
