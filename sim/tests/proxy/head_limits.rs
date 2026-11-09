//! A backend head over `max_head` while the client's upload stalls
//! (FR-PROXY-14, -18, -26, -29; nightly fix 2026-11-09, seed 99897): the
//! 502 comes as soon as the head is certainly too large, before it ends
//! and before the body idle timeout; a head that may still fit waits, and
//! the stalled body is then a 408.

use std::time::Duration;

use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::response_head::ResponseError;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{BackendFailure, BackendId, Log, RejectReason, TimeoutKind};
use durchlauferhitzer_sim::Sim;

use super::{fixed, only_upstream, peer, summary};

/// `max_head 4096` (and a request line limit within it); the body idle timeout (4 s) runs out long before a
/// head's own cap (10 s), so only the head limit can answer early.
const SMALL_HEAD: &str = "listen 127.0.0.1:8080;
limits { max_request_line 2048; max_head 4096; backend_connect_timeout 2s; backend_response_timeout 10s; body_idle_timeout 4s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; pool off; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

const PATCH_CHUNKED: &[u8] =
    b"PATCH /api/raw HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n";

/// The first 16 bytes of a 50-byte chunked body; the rest never comes.
const FIRST_16: &[u8] = b"005\r\nabcde\r\n1C\r\nf";

/// Eight interim heads (the most FR-PROXY-18 allows): `100` and `103`,
/// four times each.
fn interim_heads() -> Vec<u8> {
    let mut wire = Vec::new();
    for _ in 0..4 {
        wire.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
        wire.extend_from_slice(b"HTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\n");
    }
    wire
}

/// The first `n` bytes of a final head padded past any limit, its blank
/// line not among them.
fn final_head_prefix(n: usize) -> Vec<u8> {
    let mut wire = b"HTTP/1.0 299 Custom\r\nContent-Length: 52\r\nX-Pad: ".to_vec();
    wire.resize(n, b'p');
    wire
}

/// The client stalls after 16 body bytes; the backend sends its interim
/// heads at 1 s and, at 2 s, `final_len` bytes of its final head.
fn stalled_upload(final_len: usize) -> (Sim, durchlauferhitzer_core::ConnId) {
    let mut sim = Sim::new(SMALL_HEAD);
    let c = sim.connect(peer());
    sim.send(c, PATCH_CHUNKED);
    sim.send(c, FIRST_16);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    assert!(
        sim.take_backend_request(up).ends_with(FIRST_16),
        "{}",
        sim.trace()
    );
    sim.advance(Duration::from_millis(1000));
    sim.backend_send(up, &interim_heads());
    sim.run_until_idle();
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(1000));
    sim.backend_send(up, &final_head_prefix(final_len));
    sim.run_until_idle();
    (sim, c)
}

/// `max_head + 1` bytes of the final head, its blank line still to come:
/// certainly too large, so a 502 at once (FR-PROXY-14), at 2 s, not the
/// 408 the stalled body would get at 6 s. The body is unread: the client
/// closes (FR-PROXY-26, -28).
#[test]
fn head_over_max_head_beats_the_stalled_body() {
    let (mut sim, c) = stalled_upload(4097);
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadGateway, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    let logs = [
        Log::BackendFailed {
            conn: c,
            peer: peer(),
            backend: BackendId(0),
            kind: BackendFailure::BadResponse(ResponseError::HeadTooLarge),
        },
        Log::Response {
            conn: c,
            peer: peer(),
            status: Status::BadGateway,
            request: Some(summary(Method::Other("PATCH".to_owned()), "/api/raw")),
        },
    ];
    assert_eq!(sim.logs(), &logs, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    // Past the body idle timeout: nothing more happens.
    sim.advance(Duration::from_millis(5000));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.logs(), &logs, "{}", sim.trace());
}

/// Exactly `max_head` bytes of the final head: it may still end within
/// the limit, so no verdict yet. The head bytes restarted the body idle
/// timeout (FR-PROXY-29), which runs out at 6 s, before the head's own
/// cap (12 s): a 408 for the stalled body.
#[test]
fn head_at_max_head_waits_and_the_stalled_body_is_408() {
    let (mut sim, c) = stalled_upload(4096);
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert!(sim.logs().is_empty(), "{}", sim.trace());
    sim.advance(Duration::from_millis(3999));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(
        sim.take_received(c),
        fixed(Status::RequestTimeout, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
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
}
