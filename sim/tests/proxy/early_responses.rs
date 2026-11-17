//! A backend that answers while the request body is still being sent
//! (FR-PROXY-27): its response is relayed and the rest of the body is
//! never forwarded.

use durchlauferhitzer_core::http::request::Method;
use durchlauferhitzer_core::{BackendId, Log, UpstreamId};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

use super::bodies::{BODIES, OK, answer_ok_after, forwarded};
use super::{only_upstream, peer, request, summary};

/// FR-PROXY-27: a backend that answers before the body is complete: its
/// response is relayed with `Connection: close`, the rest of the body is
/// never forwarded, and the client closes after the response.
#[test]
fn early_backend_response_closes_client() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 100\r\n\r\n0123456789",
    );
    let up = only_upstream(&sim);
    let mut expected = forwarded("POST", "/x", "content-length: 100\r\n");
    expected.extend_from_slice(b"0123456789");
    assert_eq!(sim.take_backend_request(up), expected, "{}", sim.trace());
    sim.backend_send(
        up,
        b"HTTP/1.1 413 Too Large\r\nContent-Length: 4\r\n\r\nnope",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 413 Too Large\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.send(c, b"more body bytes");
    sim.run_until_idle();
    assert!(sim.take_backend_request(up).is_empty(), "{}", sim.trace());
}

/// FR-PROXY-27 with body bytes still waiting for the backend: the upstream
/// is read while it is written (`Interest::ReadWrite`), so a backend that
/// stops taking a large upload and answers is relayed at once, not after
/// the body idle timeout; no request byte is written after its head.
#[test]
fn early_response_while_body_bytes_are_unsent_is_relayed() {
    let mut sim = Sim::new(BODIES);
    let body = vec![b'u'; 100 * 1024];
    let head = forwarded("POST", "/x", &format!("content-length: {}\r\n", body.len()));
    sim.set_default_backend_window(Window::Budget(head.len() + 1000));
    let c = sim.connect(peer());
    sim.send(
        c,
        format!(
            "POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    sim.send(c, &body);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    assert_eq!(sim.take_backend_request(up).len(), head.len() + 1000);
    // The backend takes no more bytes, and answers; no time passes.
    sim.backend_send(up, b"HTTP/1.1 413 Nope\r\nContent-Length: 0\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 413 Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        "{}",
        sim.trace()
    );
    assert!(sim.take_backend_request(up).is_empty(), "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    let closes = sim.trace().matches("-> CloseUpstream").count();
    assert_eq!(closes, 1, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::ProxiedResponse {
            conn: c,
            peer: peer(),
            backend: BackendId(0),
            status: 413,
            request: summary(Method::Other("POST".to_owned()), "/api/x"),
        }],
        "{}",
        sim.trace()
    );
}

/// An early response after the whole body was read from the client, but
/// before the backend took all of it: the unsent body bytes are dropped,
/// the client keeps its persistence, and the request pipelined behind the
/// body is answered next (FR-PROXY-27, -28).
#[test]
fn early_response_after_the_whole_body_was_read_keeps_the_client() {
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
    sim.backend_send(up, OK);
    sim.run_until_idle();
    assert!(sim.take_backend_request(up).is_empty(), "{}", sim.trace());
    let second = UpstreamId(2);
    assert_eq!(sim.upstreams(), [up, second], "{}", sim.trace());
    assert_eq!(
        sim.take_backend_request(second),
        forwarded("GET", "/next", ""),
        "{}",
        sim.trace()
    );
    answer_ok_after(&mut sim, c, second);
}
