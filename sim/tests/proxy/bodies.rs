//! Request bodies on proxy routes: streaming client → backend straight
//! out of the client's receive buffer (FR-PROXY-19…23, -27, NFR-RES-04),
//! and `Expect: 100-continue` (FR-PROXY-25/25a). The stalled body is in
//! `body_timeouts.rs`, early backend responses in `early_responses.rs`;
//! both use the helpers here.

use durchlauferhitzer_core::http::chunked::ChunkedError;
use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    BackendId, ConnId, Interest, Log, RejectReason, RequestBodyError, UpstreamId,
};
use durchlauferhitzer_sim::{ConnectBehaviour, Sim, SimConnState, Window};

use super::{BACKEND, fixed, only_upstream, peer, request, summary};

pub(crate) const BODIES: &str = "listen 127.0.0.1:8080;
host example.test {
  path /api { proxy 127.0.0.1:3000; pool off; strip_prefix; max_request_body 8MiB; max_response_body 1MiB; }
  path /small { proxy 127.0.0.1:3000; pool off; max_request_body 10; max_response_body 1MiB; }
  path / { static /srv; }
}";

/// The head the backend gets for `method /x` with the client's `framing`
/// line (already rewritten, lowercased).
pub(crate) fn forwarded(method: &str, path: &str, framing: &str) -> Vec<u8> {
    format!(
        "{method} {path} HTTP/1.1\r\nhost: example.test\r\n{framing}\
         x-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n"
    )
    .into_bytes()
}

pub(crate) const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
pub(crate) const OK_RELAYED: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

/// The backend answers `OK`; the client gets it and stays open.
pub(crate) fn answer_ok(sim: &mut Sim, c: ConnId, up: UpstreamId) {
    sim.backend_send(up, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(c), OK_RELAYED, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
}

/// A `Content-Length` body follows the head, in pieces; the length is
/// re-rendered without leading zeros (FR-PROXY-10) and the client stays
/// keep-alive once its body was read (FR-PROXY-28).
#[test]
fn post_with_content_length_body() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 011\r\n\r\nhello",
    );
    let up = only_upstream(&sim);
    let mut expected = forwarded("POST", "/x", "content-length: 11\r\n");
    expected.extend_from_slice(b"hello");
    assert_eq!(sim.take_backend_request(up), expected, "{}", sim.trace());
    sim.send(c, b" wor");
    sim.run_until_idle();
    sim.send(c, b"ld");
    sim.run_until_idle();
    assert_eq!(sim.take_backend_request(up), b" world", "{}", sim.trace());
    answer_ok(&mut sim, c, up);
    assert_eq!(
        sim.logs(),
        &[Log::ProxiedResponse {
            conn: c,
            peer: peer(),
            backend: BackendId(0),
            status: 200,
            request: summary(Method::Other("POST".to_owned()), "/api/x"),
        }],
        "{}",
        sim.trace()
    );
}

/// A chunked body is validated and forwarded byte for byte, also when it
/// arrives one byte per read (every incomplete size line or CRLF is kept
/// as a tail of at most 35 bytes, NFR-RES-04).
#[test]
fn chunked_upload_forwarded_verbatim() {
    let body: &[u8] =
        b"5\r\nhello\r\n000A\r\n0123456789\r\n00000000000000000000000000000001\r\n!\r\n000\r\n\r\n";
    for chunk in [usize::MAX, 1, 7] {
        let mut sim = Sim::new(BODIES);
        let c = sim.connect(peer());
        sim.set_read_chunk(c, chunk);
        sim.send(
            c,
            b"PUT /api/up HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: Chunked\r\n\r\n",
        );
        sim.send(c, body);
        sim.run_until_idle();
        let up = only_upstream(&sim);
        let mut expected = forwarded("PUT", "/up", "transfer-encoding: Chunked\r\n");
        expected.extend_from_slice(body);
        assert_eq!(
            sim.take_backend_request(up),
            expected,
            "chunk={chunk}\n{}",
            sim.trace()
        );
        answer_ok(&mut sim, c, up);
    }
}

/// The `Received` lengths of `conn` in the trace, in order.
pub(crate) fn client_reads(sim: &Sim, conn: ConnId) -> Vec<usize> {
    let prefix = format!("conn={} Received len=", conn.0);
    sim.trace()
        .lines()
        .filter_map(|l| l.split_once(&prefix))
        .map(|(_, len)| len.parse().unwrap_or(0))
        .collect()
}

/// Review Focus 3: a body far larger than the buffers, to a backend that
/// takes 512 bytes per write. The sim checks after every step that the
/// client is offered a read only once its buffer holds no unsent body
/// byte, and then with its full capacity (NFR-RES-04); the trace shows
/// every read but the last filled the whole buffer.
#[test]
fn upload_larger_than_buffer_with_slow_backend() {
    let mut sim = Sim::new(BODIES);
    sim.set_default_backend_window(Window::Bytes(512));
    let body: Vec<u8> = (0..5 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let head = format!(
        "POST /api/big HTTP/1.1\r\nHost: example.test\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let c = sim.connect(peer());
    sim.send(c, head.as_bytes());
    sim.send(c, &body);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    let received = sim.take_backend_request(up);
    let expected_head = forwarded(
        "POST",
        "/big",
        &format!("content-length: {}\r\n", body.len()),
    );
    assert_eq!(
        received.get(..expected_head.len()),
        Some(expected_head.as_slice())
    );
    assert!(
        received.get(expected_head.len()..) == Some(body.as_slice()),
        "body differs: {} bytes received",
        received.len()
    );
    let reads = client_reads(&sim, c);
    let (last, full) = reads.split_last().unwrap_or((&0, &[]));
    assert!(full.len() > 300, "{reads:?}");
    assert!(full.iter().all(|&n| n == 16 * 1024), "{reads:?}");
    assert!(*last <= 16 * 1024);
    assert_eq!(reads.iter().sum::<usize>(), head.len() + body.len());
    answer_ok(&mut sim, c, up);
}

/// FR-PROXY-23: a `Content-Length` over the route's limit is answered 413
/// before any connect, and the connection closes (its body is unread).
#[test]
fn content_length_over_limit_is_413_before_connect() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /small HTTP/1.1\r\nHost: example.test\r\nContent-Length: 11\r\n\r\nhello",
    );
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.take_received(c),
        fixed(Status::ContentTooLarge, Persistence::Close, BodyMode::Full)
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown);
    // Exactly the limit is fine.
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /small HTTP/1.1\r\nHost: example.test\r\nContent-Length: 10\r\n\r\n0123456789",
    );
    let up = only_upstream(&sim);
    let mut expected = forwarded("POST", "/small", "content-length: 10\r\n");
    expected.extend_from_slice(b"0123456789");
    assert_eq!(sim.take_backend_request(up), expected, "{}", sim.trace());
    answer_ok(&mut sim, c, up);
}

/// The one `Response` entry of a 400 to `method /api/x` (FR-LOG-03).
fn logged_one_400(sim: &Sim, c: ConnId, method: Method) {
    let entry = Log::Response {
        conn: c,
        peer: peer(),
        status: Status::BadRequest,
        request: Some(summary(method, "/api/x")),
    };
    assert_eq!(sim.logs(), &[entry], "{}", sim.trace());
}

/// FR-PROXY-01: a `GET` whose head frames a body is answered 400 with
/// `Connection: close` before any connect (NFR-SEC-14, TE.0); one
/// `Response` entry is logged (FR-LOG-03) and the unread body lingers
/// until the client ends its stream (FR-CONN-05).
#[test]
fn get_with_a_body_is_400_without_connect() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"GET /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadRequest, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    logged_one_400(&sim, c, Method::Get);
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
}

/// FR-PROXY-01: a chunked body on `HEAD` is 400 too, before any connect;
/// the answer is head-only like every response to `HEAD` (FR-HTTP-21).
#[test]
fn head_with_a_chunked_body_is_400_without_connect() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"HEAD /api/x HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n\
          5\r\nhello\r\n0\r\n\r\n",
    );
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadRequest, Persistence::Close, BodyMode::HeadOnly),
        "{}",
        sim.trace()
    );
    logged_one_400(&sim, c, Method::Head);
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

/// FR-HTTP-08: `Content-Length: 0` is no body, so a `GET` carrying it is
/// forwarded, its length line re-rendered as FR-PROXY-10 renders it and
/// no body after the head. The parser yields `BodyFraming::None` for it
/// (core test `body_framing`), so `plan`'s FR-PROXY-01 arm never sees a
/// framed body here and this test passes without the arm; it pins that a
/// zero length is never mistaken for a body.
#[test]
fn get_with_content_length_zero_is_forwarded() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"GET /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 0\r\n\r\n",
    );
    let up = only_upstream(&sim);
    assert_eq!(
        sim.take_backend_request(up),
        forwarded("GET", "/x", "content-length: 0\r\n"),
        "{}",
        sim.trace()
    );
    answer_ok(&mut sim, c, up);
}

/// FR-PROXY-01 runs in `plan`, before the `100 Continue` of FR-PROXY-25:
/// a `HEAD` with a chunked body and `Expect: 100-continue` gets the 400
/// and never a 100.
#[test]
fn head_with_a_chunked_body_and_expect_gets_400_not_100() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"HEAD /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Transfer-Encoding: chunked\r\n\r\n",
    );
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadRequest, Persistence::Close, BodyMode::HeadOnly),
        "{}",
        sim.trace()
    );
    logged_one_400(&sim, c, Method::Head);
}

/// FR-PROXY-23: a chunked body that passes the limit cuts both
/// connections; what was within the limit was forwarded, nothing after.
#[test]
fn chunked_over_limit_cuts_both() {
    let head = b"POST /small HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n";
    let mut sim = Sim::new(BODIES);
    let c = request(&mut sim, head);
    let up = only_upstream(&sim);
    sim.send(c, b"5\r\nhello\r\n");
    sim.run_until_idle();
    let mut expected = forwarded("POST", "/small", "transfer-encoding: chunked\r\n");
    expected.extend_from_slice(b"5\r\nhello\r\n");
    assert_eq!(sim.take_backend_request(up), expected, "{}", sim.trace());
    sim.send(c, b"6\r\nworld!\r\n0\r\n\r\n");
    sim.run_until_idle();
    assert!(sim.take_backend_request(up).is_empty(), "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Body(RequestBodyError::TooLarge),
        }],
        "{}",
        sim.trace()
    );
    // Exactly the limit is fine.
    let mut sim = Sim::new(BODIES);
    let c = request(&mut sim, head);
    sim.send(c, b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n");
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.take_backend_request(up);
    answer_ok(&mut sim, c, up);
}

/// FR-PROXY-25: `100 Continue` comes from the core once the head passed
/// every check and the backend is connected; `Expect` is not forwarded.
#[test]
fn expect_100_continue_answered_and_stripped() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-Continue\r\n\
          Content-Length: 5\r\n\r\n",
    );
    let up = only_upstream(&sim);
    assert_eq!(
        sim.take_backend_request(up),
        forwarded("POST", "/x", "content-length: 5\r\n"),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 100 Continue\r\n\r\n",
        "{}",
        sim.trace()
    );
    sim.send(c, b"hello");
    sim.run_until_idle();
    assert_eq!(sim.take_backend_request(up), b"hello", "{}", sim.trace());
    answer_ok(&mut sim, c, up);
}

/// No `100 Continue` before the connect succeeded: a refused connect is a
/// 502 at once, and the connection closes (its body is unread).
#[test]
fn expect_100_continue_waits_for_the_backend() {
    let mut sim = Sim::new(BODIES);
    sim.set_connect(BACKEND, ConnectBehaviour::Refuse);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 5\r\n\r\n",
    );
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadGateway, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

/// FR-PROXY-25a: outside proxy routes, and on a proxy route whose checks
/// fail, `100-continue` gets the final response at once and never a 100.
#[test]
fn expect_100_continue_elsewhere_gets_the_final_response() {
    let cases: [(&str, &str, &str, Status); 4] = [
        (
            "GET",
            "/static.txt",
            "example.test",
            Status::ContentTooLarge,
        ),
        (
            "POST",
            "/static.txt",
            "example.test",
            Status::MethodNotAllowed,
        ),
        ("POST", "/x", "elsewhere.test", Status::NotFound),
        ("POST", "/small", "example.test", Status::ContentTooLarge),
    ];
    for (method, target, host, status) in cases {
        let mut sim = Sim::new(BODIES);
        let raw = format!(
            "{method} {target} HTTP/1.1\r\nHost: {host}\r\nExpect: 100-continue\r\n\
             Content-Length: 50\r\n\r\n"
        );
        let c = request(&mut sim, raw.as_bytes());
        assert!(sim.upstreams().is_empty(), "{target}\n{}", sim.trace());
        assert_eq!(
            sim.take_received(c),
            fixed(status, Persistence::Close, BodyMode::Full),
            "{target}\n{}",
            sim.trace()
        );
    }
}

/// The backend answers `OK` to a second request; the client gets both.
pub(crate) fn answer_ok_after(sim: &mut Sim, c: ConnId, up: UpstreamId) {
    sim.backend_send(up, OK);
    sim.run_until_idle();
    let mut both = OK_RELAYED.to_vec();
    both.extend_from_slice(OK_RELAYED);
    assert_eq!(sim.take_received(c), both, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// FR-PROXY-19: a chunk extension in the bytes that came with the head is
/// a 400 before any connect; one that arrives after the head was forwarded
/// cuts both connections.
#[test]
fn chunk_extension_from_client_is_400() {
    let head = b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n";
    let mut raw = head.to_vec();
    raw.extend_from_slice(b"5;ext=1\r\nhello\r\n0\r\n\r\n");
    let mut sim = Sim::new(BODIES);
    let c = request(&mut sim, &raw);
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadRequest, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown);
    let rejected = |c| Log::Rejected {
        conn: c,
        peer: peer(),
        reason: RejectReason::Body(RequestBodyError::Chunked(ChunkedError::Extension)),
    };
    assert_eq!(sim.logs(), &[rejected(c)], "{}", sim.trace());
    // After the head was forwarded: cut.
    let mut sim = Sim::new(BODIES);
    let c = request(&mut sim, head);
    let up = only_upstream(&sim);
    sim.send(c, b"5;ext=1\r\nhello\r\n0\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_backend_request(up),
        forwarded("POST", "/x", "transfer-encoding: chunked\r\n")
    );
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.logs(), &[rejected(c)], "{}", sim.trace());
}

/// FR-PROXY-23: a chunked body already over the limit in the bytes that
/// came with the head is cut before any connect, with no response.
#[test]
fn chunked_over_limit_with_the_head_is_cut_before_connect() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /small HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n\
          b\r\nhello world\r\n",
    );
    assert!(sim.upstreams().is_empty(), "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Body(RequestBodyError::TooLarge),
        }],
        "{}",
        sim.trace()
    );
}

const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// No `100 Continue` for a request without a body: there is nothing to
/// continue with (FR-PROXY-25).
#[test]
fn expect_100_continue_without_a_body_gets_no_100() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"GET /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\r\n",
    );
    let up = only_upstream(&sim);
    assert_eq!(sim.take_backend_request(up), forwarded("GET", "/x", ""));
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    answer_ok(&mut sim, c, up);
}

/// The `100 Continue` is sent also when the whole body came with the
/// head, before the relayed response.
#[test]
fn expect_100_continue_with_the_body_already_there() {
    let mut sim = Sim::new(BODIES);
    let c = request(
        &mut sim,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 5\r\n\r\nhello",
    );
    let up = only_upstream(&sim);
    let mut expected = forwarded("POST", "/x", "content-length: 5\r\n");
    expected.extend_from_slice(b"hello");
    assert_eq!(sim.take_backend_request(up), expected, "{}", sim.trace());
    sim.backend_send(up, OK);
    sim.run_until_idle();
    let mut both = CONTINUE.to_vec();
    both.extend_from_slice(OK_RELAYED);
    assert_eq!(sim.take_received(c), both, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// A `100 Continue` followed by a failure of the backend while the body is
/// still due: one well-ordered stream, the 100 then a closing 502.
#[test]
fn expect_100_continue_then_backend_failure() {
    let mut sim = Sim::new(BODIES);
    let c = sim.connect(peer());
    // The client takes 10 bytes, so the 100 is still being written when
    // the backend fails.
    sim.set_client_window(c, Window::Budget(10));
    sim.send(
        c,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 5\r\n\r\n",
    );
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.backend_reset(up);
    sim.set_client_window(c, Window::Unlimited);
    sim.run_until_idle();
    let mut both = CONTINUE.to_vec();
    both.extend_from_slice(&fixed(
        Status::BadGateway,
        Persistence::Close,
        BodyMode::Full,
    ));
    assert_eq!(sim.take_received(c), both, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}
