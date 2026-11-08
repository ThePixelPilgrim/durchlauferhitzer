//! `internal_files` routes (FR-PROXY-32 Stripping and Hints): the backend
//! is told which file header the route trusts, a client's forged file
//! headers never reach it, and a response's file headers never reach the
//! client. (Serving the file a `200` names comes with the trigger.)

use durchlauferhitzer_core::http::request::Method;
use durchlauferhitzer_core::{BackendId, Log};
use durchlauferhitzer_sim::{Sim, SimConnState};

use super::{only_upstream, peer, request, summary};

/// An `x-sendfile` route under `/send`, an `x-accel-redirect` one under
/// `/accel`, both to the same backend.
const FILES: &str = "listen 127.0.0.1:8080;
host example.test {
  path /send { proxy 127.0.0.1:3000; pool off; strip_prefix; max_request_body 1MiB; max_response_body 1MiB;
    internal_files x-sendfile /srv/files; }
  path /accel { proxy 127.0.0.1:3000; pool off; strip_prefix; max_request_body 1MiB; max_response_body 1MiB;
    internal_files x-accel-redirect /protected/ /srv/private; }
}";

fn proxied(c: durchlauferhitzer_core::ConnId, status: u16, path: &str) -> Log {
    Log::ProxiedResponse {
        conn: c,
        peer: peer(),
        backend: BackendId(0),
        status,
        request: summary(Method::Get, path),
    }
}

/// FR-PROXY-32 Hints: `X-Sendfile-Type` names the route's header and, on
/// an `x-accel-redirect` route, `X-Accel-Mapping` maps its root to its
/// prefix; the client's forged copies (in any case, named in `Connection`)
/// are gone.
#[test]
fn backend_sees_the_hints() {
    let cases: [(&str, &[u8]); 2] = [
        (
            "/send/a",
            b"GET /a HTTP/1.1\r\nhost: example.test\r\nx-forwarded-for: 192.0.2.1\r\n\
              x-forwarded-proto: http\r\nx-sendfile-type: X-Sendfile\r\n\
              connection: close\r\n\r\n",
        ),
        (
            "/accel/a",
            b"GET /a HTTP/1.1\r\nhost: example.test\r\nx-forwarded-for: 192.0.2.1\r\n\
              x-forwarded-proto: http\r\nx-sendfile-type: X-Accel-Redirect\r\n\
              x-accel-mapping: /srv/private/=/protected/\r\nconnection: close\r\n\r\n",
        ),
    ];
    for (path, forwarded) in cases {
        let mut sim = Sim::new(FILES);
        let wire = format!(
            "GET {path} HTTP/1.1\r\nHost: example.test\r\nX-Sendfile-Type: X-Accel-Redirect\r\n\
             x-accel-mapping: /=/protected/\r\nX-SENDFILE: /etc/passwd\r\n\
             X-Accel-Redirect: /protected/x\r\nConnection: X-Accel-Mapping\r\n\r\n"
        );
        let c = request(&mut sim, wire.as_bytes());
        let up = only_upstream(&sim);
        assert_eq!(
            String::from_utf8_lossy(&sim.take_backend_request(up)),
            String::from_utf8_lossy(forwarded),
            "{}",
            sim.trace()
        );
        sim.backend_send(up, b"HTTP/1.1 204 No Content\r\n\r\n");
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            b"HTTP/1.1 204 No Content\r\n\r\n",
            "{}",
            sim.trace()
        );
        assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    }
}

/// FR-PROXY-32 Trigger: on any status but `200` the route's header is
/// removed and the response passes unchanged, body and all.
#[test]
fn non_200_with_the_route_header_is_relayed_without_it() {
    let mut sim = Sim::new(FILES);
    let c = request(
        &mut sim,
        b"GET /accel/a HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    let up = only_upstream(&sim);
    sim.backend_send(
        up,
        b"HTTP/1.1 404 Not Found\r\nX-Accel-Redirect: /protected/a\r\n\
          Content-Type: text/plain\r\nContent-Length: 4\r\n\r\nnope",
    );
    sim.run_until_idle();
    assert_eq!(
        String::from_utf8_lossy(&sim.take_received(c)),
        "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\ncontent-length: 4\r\n\r\nnope",
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[proxied(c, 404, "/accel/a")],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// FR-PROXY-32 Trigger: the header the route does not trust is removed and
/// ignored, a `200` relayed as it is.
#[test]
fn the_other_header_on_a_200_is_dropped_and_ignored() {
    let mut sim = Sim::new(FILES);
    let c = request(
        &mut sim,
        b"GET /send/a HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    let up = only_upstream(&sim);
    sim.backend_send(
        up,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /srv/files/a\r\nX-Accel-Expires: 10\r\n\
          Content-Length: 5\r\n\r\nhello",
    );
    sim.run_until_idle();
    // Not an `x-accel-redirect` route: other `X-Accel-*` pass.
    assert_eq!(
        String::from_utf8_lossy(&sim.take_received(c)),
        "HTTP/1.1 200 OK\r\nx-accel-expires: 10\r\ncontent-length: 5\r\n\r\nhello",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.logs(), &[proxied(c, 200, "/send/a")], "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}
