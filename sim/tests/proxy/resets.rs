//! Backend resets behind readable bytes (the driver contract's upstream
//! error rule, DRV-10): bytes the backend sent before its reset are read
//! and judged first, so an early answer is still relayed and a body is cut
//! only after its last byte.

use durchlauferhitzer_core::http::request::Method;
use durchlauferhitzer_core::{BackendFailure, BackendId, Log};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

use super::{CONFIG, GET_X, only_upstream, peer, request, summary};

/// A backend refuses a large upload early: it answers 413 and resets the
/// connection over the body it did not read (a kernel then sends RST, not
/// FIN). The answer came before the reset, so it is relayed (FR-PROXY-27),
/// not replaced by a 502; no request byte is written after it.
#[test]
fn early_413_then_reset_during_the_upload_is_relayed() {
    let mut sim = Sim::new(CONFIG);
    let body = vec![b'u'; 100 * 1024];
    let c = sim.connect(peer());
    sim.set_default_backend_window(Window::Budget(1000));
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
    assert_eq!(sim.take_backend_request(up).len(), 1000, "{}", sim.trace());
    sim.backend_reset_after(up, b"HTTP/1.1 413 Too Big\r\nContent-Length: 2\r\n\r\nno");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 413 Too Big\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno",
        "{}",
        sim.trace()
    );
    assert!(sim.take_backend_request(up).is_empty(), "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
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

/// A backend sends a close-delimited response and then resets instead of
/// closing, while the client is still draining the relayed bytes (the
/// upstream's interest is `None`). The reset waits until the core reads
/// again: the client gets every byte the backend sent, then the connection
/// is cut (a reset is no end of a close-delimited body, FR-PROXY-26).
#[test]
fn reset_after_a_complete_close_delimited_tail_while_the_client_drains() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    sim.set_client_window(c, Window::Stalled);
    let up = only_upstream(&sim);
    let tail = vec![b't'; 5000];
    sim.backend_reset_after(up, &[b"HTTP/1.1 200 OK\r\n\r\n".as_slice(), &tail].concat());
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
    sim.set_client_window(c, Window::Unlimited);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        [
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".as_slice(),
            &tail
        ]
        .concat(),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::BackendFailed {
            conn: c,
            peer: peer(),
            backend: BackendId(0),
            kind: BackendFailure::Reset,
        }),
        "{}",
        sim.trace()
    );
}
