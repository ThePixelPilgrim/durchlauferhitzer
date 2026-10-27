//! Response bodies on proxy routes (stage 3 Task 7): chunked and
//! close-delimited bodies relayed straight out of the upstream's receive
//! buffer (FR-PROXY-19…22, NFR-RES-04), bodies that never reach the client
//! (HEAD, 204, 304, bytes after a body's end), `max_response_body`
//! (FR-PROXY-24), interim heads and 101 (FR-PROXY-18), framing errors after
//! the head (FR-PROXY-26); HTTP/1.0 clients (FR-PROXY-04) in `http10.rs`.

mod http10;

use std::time::Duration;

use durchlauferhitzer_core::http::chunked::ChunkedError;
use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::response_head::ResponseError;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnId, Interest, Log, RejectReason, TimeoutKind, UpstreamId,
};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

use super::{
    CONFIG, GET_X, LARGE, SHORT_SEND_IDLE, assert_502, fixed, only_upstream, peer, request, summary,
};

/// As `CONFIG`, with a response body limit of 10 bytes.
const SMALL: &str = "listen 127.0.0.1:8080;
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 10; }
}";

const CHUNKED_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
const CHUNKED_RELAYED: &[u8] = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// The actions emitted since `before` bytes of the trace, without their
/// subject.
fn actions_since(sim: &Sim, before: usize) -> Vec<String> {
    sim.trace()
        .get(before..)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" -> "))
        .map(|l| l.split_once(" -> ").map_or(l, |(_, a)| a).to_owned())
        .collect()
}

fn proxied(c: ConnId, status: u16) -> Log {
    Log::ProxiedResponse {
        conn: c,
        peer: peer(),
        backend: BackendId(0),
        status,
        request: summary(Method::Get, "/api/x"),
    }
}

fn failed(c: ConnId, kind: BackendFailure) -> Log {
    Log::BackendFailed {
        conn: c,
        peer: peer(),
        backend: BackendId(0),
        kind,
    }
}

/// After a cut: the client is closed at once (no lingering close), the
/// upstream too, and the last actions were the log, `CloseUpstream`,
/// `Close`.
fn assert_cut(sim: &Sim, c: ConnId, before: usize, kind: BackendFailure) {
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[proxied(c, 200), failed(c, kind)],
        "{}",
        sim.trace()
    );
    let actions = actions_since(sim, before);
    let tail: Vec<&str> = actions
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(String::as_str)
        .collect();
    let log = tail.first().copied().unwrap_or_default();
    assert!(log.starts_with("Log BackendFailed"), "{}", sim.trace());
    let rest = tail.get(1..).unwrap_or_default();
    assert_eq!(rest, ["CloseUpstream", "Close"], "{}", sim.trace());
    assert!(
        !actions.iter().any(|a| a.starts_with("ShutdownWrite")),
        "{}",
        sim.trace()
    );
}

/// FR-PROXY-19, -21: a chunked body is validated in flight and forwarded
/// byte for byte, however the backend's bytes are split; the client stays
/// keep-alive.
#[test]
fn chunked_response_forwarded_verbatim() {
    let body: &[u8] = b"5\r\nhello\r\n000A\r\n0123456789\r\n0\r\n\r\n";
    let splits: Vec<Vec<&[u8]>> = vec![
        vec![body],
        vec![
            &body[..2],
            &body[2..8],
            &body[8..15],
            &body[15..30],
            &body[30..],
        ],
        body.chunks(1).collect(),
    ];
    for pieces in splits {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, CHUNKED_HEAD);
        sim.run_until_idle();
        for piece in &pieces {
            sim.backend_send(up, piece);
            sim.run_until_idle();
        }
        assert_eq!(
            sim.take_received(c),
            cat(&[CHUNKED_RELAYED, body]),
            "{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
        assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
        assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
        assert_eq!(sim.logs(), &[proxied(c, 200)], "{}", sim.trace());
    }
}

/// FR-PROXY-21, -28: a response without framing headers is forwarded with
/// `Connection: close` until the backend closes; then the client closes
/// (a lingering close: the response is complete).
#[test]
fn close_delimited_response_closes_client() {
    for (head, relayed) in [
        (
            b"HTTP/1.1 200 OK\r\nServer: b\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nserver: b\r\nconnection: close\r\n\r\n".as_slice(),
        ),
        (
            b"HTTP/1.0 200 OK\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n",
        ),
    ] {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, &cat(&[head, b"until"]));
        sim.run_until_idle();
        sim.backend_send(up, b" close");
        sim.run_until_idle();
        assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
        sim.backend_eof(up);
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            cat(&[relayed, b"until close"]),
            "{}",
            sim.trace()
        );
        assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
        assert_eq!(sim.logs(), &[proxied(c, 200)], "{}", sim.trace());
    }
}

/// FR-PROXY-16, -21: a response to HEAD has no body, whatever its framing
/// headers say; the backend's extra bytes are dropped and the upstream is
/// closed.
#[test]
fn head_response_carries_no_body() {
    for (response, relayed) in [
        (
            cat(&[CHUNKED_HEAD, b"5\r\nhello\r\n0\r\n\r\n"]),
            CHUNKED_RELAYED.to_vec(),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n".to_vec(),
        ),
        // No framing header: not close delimited for HEAD.
        (
            b"HTTP/1.1 200 OK\r\n\r\njunk".to_vec(),
            b"HTTP/1.1 200 OK\r\n\r\n".to_vec(),
        ),
    ] {
        let mut sim = Sim::new(CONFIG);
        let c = request(
            &mut sim,
            b"HEAD /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n",
        );
        let up = only_upstream(&sim);
        sim.backend_send(up, &response);
        sim.run_until_idle();
        assert_eq!(sim.take_received(c), relayed, "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
        assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    }
}

/// FR-PROXY-21: 204 and 304 never carry a body either.
#[test]
fn no_content_and_not_modified_have_no_body() {
    for (response, relayed) in [
        (
            b"HTTP/1.1 204 No Content\r\n\r\njunk".as_slice(),
            b"HTTP/1.1 204 No Content\r\n\r\n".as_slice(),
        ),
        (
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 5\r\n\r\nhello",
            b"HTTP/1.1 304 Not Modified\r\ncontent-length: 5\r\n\r\n",
        ),
        (
            b"HTTP/1.1 304 Not Modified\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 304 Not Modified\r\ntransfer-encoding: chunked\r\n\r\n",
        ),
    ] {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, response);
        sim.run_until_idle();
        assert_eq!(sim.take_received(c), relayed, "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
        assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    }
}

/// FR-PROXY-22: the read that completes a `Content-Length` body may carry
/// more bytes; they are dropped and the upstream closed.
#[test]
fn extra_bytes_after_content_length_never_forwarded() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\no");
    sim.run_until_idle();
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    sim.backend_send(up, b"kEXTRA");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// FR-PROXY-22: a second response after a chunked body (in the same read
/// or later) never reaches the client; the next request gets a fresh
/// upstream that sees only its own request.
#[test]
fn second_response_never_forwarded() {
    const SECOND: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nno";
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, &cat(&[CHUNKED_HEAD, b"2\r\nok\r\n0\r"]));
    sim.run_until_idle();
    sim.backend_send(up, &cat(&[b"\n\r\n", SECOND]));
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        cat(&[CHUNKED_RELAYED, b"2\r\nok\r\n0\r\n\r\n"]),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.send(c, GET_X);
    sim.run_until_idle();
    assert_eq!(sim.upstreams(), [up, UpstreamId(2)], "{}", sim.trace());
    sim.backend_send(
        UpstreamId(2),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nyy",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nyy",
        "{}",
        sim.trace()
    );
}

/// FR-PROXY-24: a `Content-Length` over `max_response_body` is a 502
/// before any byte reaches the client; exactly the limit is relayed.
#[test]
fn response_over_limit_is_502_or_cut() {
    let mut sim = Sim::new(SMALL);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello");
    sim.run_until_idle();
    let received = sim.take_received(c);
    assert_502(&sim, c, BackendFailure::BodyTooLarge, received);

    let mut sim = Sim::new(SMALL);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(
        up,
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n0123456789",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());

    // Chunked and close-delimited bodies of exactly the limit are fine.
    for (head, body, relayed_head) in [
        (
            CHUNKED_HEAD,
            b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n".as_slice(),
            CHUNKED_RELAYED,
        ),
        (
            b"HTTP/1.1 200 OK\r\n\r\n".as_slice(),
            b"helloworld".as_slice(),
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".as_slice(),
        ),
    ] {
        let mut sim = Sim::new(SMALL);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, head);
        sim.run_until_idle();
        sim.backend_send(up, body);
        sim.backend_eof(up);
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            cat(&[relayed_head, body]),
            "{}",
            sim.trace()
        );
        assert_eq!(sim.logs(), &[proxied(c, 200)], "{}", sim.trace());
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    }
}

/// FR-PROXY-24, -26: a chunked or close-delimited body that passes the
/// limit after its head was queued cuts both connections; the client holds
/// only the bytes up to the last piece within the limit.
#[test]
fn response_over_limit_after_the_head_cuts_both() {
    for (head, first, second, relayed_head) in [
        (
            CHUNKED_HEAD,
            b"5\r\nhello\r\n".as_slice(),
            b"6\r\nworld!\r\n0\r\n\r\n".as_slice(),
            CHUNKED_RELAYED,
        ),
        (
            b"HTTP/1.1 200 OK\r\n\r\n".as_slice(),
            b"hello".as_slice(),
            b"world!".as_slice(),
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".as_slice(),
        ),
    ] {
        let mut sim = Sim::new(SMALL);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, head);
        sim.run_until_idle();
        sim.backend_send(up, first);
        sim.run_until_idle();
        let before = sim.trace().len();
        sim.backend_send(up, second);
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            cat(&[relayed_head, first]),
            "{}",
            sim.trace()
        );
        assert_cut(&sim, c, before, BackendFailure::BodyTooLarge);
    }
}

/// Builder call (Task 7): body bytes that arrive with the head are judged
/// before the head is queued, so a body already over the limit, or with
/// bad chunked framing, is still a 502.
#[test]
fn body_errors_with_the_head_are_502() {
    for (response, kind) in [
        (
            cat(&[CHUNKED_HEAD, b"5\r\nhello\r\n6\r\nworld!\r\n"]),
            BackendFailure::BodyTooLarge,
        ),
        (
            b"HTTP/1.1 200 OK\r\n\r\nhelloworld!".to_vec(),
            BackendFailure::BodyTooLarge,
        ),
        (
            cat(&[CHUNKED_HEAD, b"5\r\nhello\r\nzz\r\n"]),
            BackendFailure::BadChunk(ChunkedError::BadSize),
        ),
    ] {
        let mut sim = Sim::new(SMALL);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, &response);
        sim.run_until_idle();
        let received = sim.take_received(c);
        assert_502(&sim, c, kind, received);
    }
}

/// Review Focus 4: bad chunked framing after the head cuts both
/// connections: nothing of the bad piece is forwarded.
#[test]
fn bad_chunk_after_head_cuts_both() {
    for (bad, error) in [
        (b"5;x=y\r\nworld\r\n".as_slice(), ChunkedError::Extension),
        (b"5\r\nworld!\r\n", ChunkedError::BadLineEnd),
        (b"0\r\nTrailer: x\r\n\r\n", ChunkedError::Trailer),
    ] {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, &cat(&[CHUNKED_HEAD, b"5\r\nhello\r\n"]));
        sim.run_until_idle();
        let before = sim.trace().len();
        sim.backend_send(up, bad);
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            cat(&[CHUNKED_RELAYED, b"5\r\nhello\r\n"]),
            "{}",
            sim.trace()
        );
        assert_cut(&sim, c, before, BackendFailure::BadChunk(error));
    }
}

/// A chunked body that stalls after its head is cut after the body idle
/// timeout, like a `Content-Length` one.
#[test]
fn stalled_chunked_body_cuts_after_the_body_idle_timeout() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, &cat(&[CHUNKED_HEAD, b"5\r\nhel"]));
    sim.run_until_idle();
    let before = sim.trace().len();
    sim.advance(Duration::from_secs(29));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(Duration::from_secs(1));
    assert_cut(&sim, c, before, BackendFailure::ResponseTimeout);
}

/// The bytes a large chunked body is made of: chunks of varying size (so
/// that framing lines straddle buffer boundaries at every offset), then
/// the last chunk.
fn large_chunked_body(payload: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut sent = 0;
    let mut size = 1;
    while sent < payload {
        let n = size.min(payload - sent);
        out.extend_from_slice(format!("{n:03x}\r\n").as_bytes());
        out.extend((sent..sent + n).map(|i| (i % 251) as u8));
        out.extend_from_slice(b"\r\n");
        sent += n;
        size = size * 7 % 113 + 1;
    }
    out.extend_from_slice(b"0000\r\n\r\n");
    out
}

/// Review Focus 3, NFR-RES-04: chunked and close-delimited bodies far
/// larger than the buffers, to a slow client. The sim checks after every
/// step that a body read is offered only once the buffer is drained, then
/// with its whole capacity but a chunked tail of at most 35 bytes; the
/// bytes arrive intact.
#[test]
fn slow_client_large_response_bounded() {
    let chunked = large_chunked_body(3 * 1024 * 1024);
    let raw: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    for (head, body, relayed_head, end) in [
        (CHUNKED_HEAD, &chunked, CHUNKED_RELAYED, SimConnState::Open),
        (
            b"HTTP/1.1 200 OK\r\n\r\n".as_slice(),
            &raw,
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".as_slice(),
            SimConnState::WriteShutdown,
        ),
    ] {
        let mut sim = Sim::new(LARGE);
        let c = request(&mut sim, GET_X);
        sim.set_client_window(c, Window::Bytes(1000));
        let up = only_upstream(&sim);
        sim.backend_send(up, &cat(&[head, body]));
        sim.backend_eof(up);
        sim.run_until_idle();
        let received = sim.take_received(c);
        assert_eq!(received.get(..relayed_head.len()), Some(relayed_head));
        assert!(
            received.get(relayed_head.len()..) == Some(body.as_slice()),
            "body differs: {} bytes received",
            received.len()
        );
        assert_eq!(sim.open_upstreams(), 0);
        assert_eq!(sim.state(c), end);
        let reads: Vec<usize> = sim
            .trace()
            .lines()
            .filter_map(|l| l.split_once(" UpstreamReceived len="))
            .map(|(_, len)| len.parse().unwrap())
            .collect();
        let capacity = reads.iter().copied().max().unwrap_or(0);
        assert!(capacity >= 16 * 1024, "{capacity}");
        let behind_a_tail = reads.split_last().map_or(0, |(_, full)| {
            full.iter().filter(|&&n| n < capacity).count()
        });
        if head == CHUNKED_HEAD {
            // Framing lines straddled reads: some reads went in behind a
            // kept tail (the sim checks each was the rest of the buffer).
            assert!(behind_a_tail > 0, "{reads:?}");
        } else {
            assert_eq!(behind_a_tail, 0, "{reads:?}");
        }
        assert_eq!(reads.iter().sum::<usize>(), head.len() + body.len());
    }
}

/// FR-PROXY-18: interim heads are dropped, in one read with the final head
/// or in reads of their own; up to 8 in a row are fine.
#[test]
fn interim_100_from_backend_dropped() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 100 Continue\r\n\r\n");
    sim.run_until_idle();
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 1, "{}", sim.trace());
    sim.backend_send(
        up,
        b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\n\
          HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.logs(), &[proxied(c, 200)], "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());

    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    for _ in 0..8 {
        sim.backend_send(up, b"HTTP/1.1 102 Processing\r\n\r\n");
    }
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        "{}",
        sim.trace()
    );
}

/// FR-PROXY-18: a ninth interim head in a row is a 502.
#[test]
fn more_than_eight_interim_heads_is_502() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    for _ in 0..9 {
        sim.backend_send(up, b"HTTP/1.1 100 Continue\r\n\r\n");
        sim.run_until_idle();
    }
    let received = sim.take_received(c);
    let kind = BackendFailure::BadResponse(ResponseError::TooManyInterimHeads);
    assert_502(&sim, c, kind, received);
}

/// FR-PROXY-18: `101 Switching Protocols` is a 502.
#[test]
fn switching_protocols_is_502() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_X);
    let up = only_upstream(&sim);
    sim.backend_send(
        up,
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n",
    );
    sim.run_until_idle();
    let received = sim.take_received(c);
    let kind = BackendFailure::BadResponse(ResponseError::SwitchingProtocols);
    assert_502(&sim, c, kind, received);
}

const UPLOAD_HEAD: &[u8] =
    b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 30000\r\n\r\n";

fn upload_body() -> Vec<u8> {
    (0..30000u32).map(|i| b'a' + (i % 26) as u8).collect()
}

/// Carry-forward 1: an interim `100` mid-upload is dropped and the upload
/// goes on, both while the core waits for the client's bytes (upstream
/// `Read`) and while body bytes wait for the backend (`ReadWrite`); the
/// final response is relayed and the client stays keep-alive.
#[test]
fn interim_100_mid_upload_neither_stalls_nor_breaks_the_upload() {
    let body = upload_body();
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.send(c, &cat(&[UPLOAD_HEAD, &body[..5000]]));
    sim.run_until_idle();
    let up = only_upstream(&sim);
    // The core waits for the client: the upstream is only read.
    sim.backend_send(up, b"HTTP/1.1 100 Continue\r\n\r\n");
    sim.run_until_idle();
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    // Body bytes wait for a backend that takes nothing: read and write.
    sim.set_backend_window(up, Window::Stalled);
    sim.send(c, &body[5000..12000]);
    sim.run_until_idle();
    sim.backend_send(up, b"HTTP/1.1 100 Continue\r\nX: y\r\n\r\n");
    sim.run_until_idle();
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    sim.set_backend_window(up, Window::Unlimited);
    sim.send(c, &body[12000..]);
    sim.run_until_idle();
    let forwarded = sim.take_backend_request(up);
    assert!(
        forwarded.ends_with(&body),
        "backend got {} bytes\n{}",
        forwarded.len(),
        sim.trace()
    );
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// Carry-forward 1: a close-delimited response that comes before the
/// upload is complete leaves the body unread; the client closes after it
/// with a lingering close.
#[test]
fn early_close_delimited_response_lingers() {
    let body = upload_body();
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.send(c, &cat(&[UPLOAD_HEAD, &body[..100]]));
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 413 Too Big\r\n\r\nno");
    sim.backend_eof(up);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 413 Too Big\r\nconnection: close\r\n\r\nno",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// FR-PROXY-29 (review fix M1): a read that only grows a chunked tail while
/// the client still has the rewritten head to take keeps the client's
/// `send_idle_timeout`: a client that takes nothing is cut after 5 s, not
/// after the 30 s body idle timeout. The read at 1 s is backend progress,
/// not the client's, so it does not restart the deadline either (final
/// review I2): the cut comes 5 s after the head was queued.
#[test]
fn tail_only_read_keeps_the_send_timeout_while_the_head_is_unsent() {
    let mut sim = Sim::new(SHORT_SEND_IDLE);
    let c = request(&mut sim, GET_X);
    sim.set_client_window(c, Window::Stalled);
    let up = only_upstream(&sim);
    sim.backend_send(up, &cat(&[CHUNKED_HEAD, b"5"]));
    sim.run_until_idle();
    sim.advance(Duration::from_secs(1));
    // Only the size line's CR: the tail grows, nothing new for the client.
    sim.backend_send(up, b"\r");
    sim.run_until_idle();
    sim.advance(Duration::from_millis(3999));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    sim.advance(Duration::from_millis(1));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
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

/// FR-PROXY-26 (review fix M3): a reset in the middle of a close-delimited
/// body, and a reset or end of stream in the middle of a chunked body,
/// after the head cut both connections (no lingering close): the client
/// holds the head and the bytes relayed so far.
#[test]
fn reset_or_eof_mid_body_after_the_head_cuts_both() {
    enum End {
        Reset,
        Eof,
    }
    impl End {
        /// The failure a cut after the head is logged as.
        fn kind(&self) -> BackendFailure {
            match self {
                End::Reset => BackendFailure::Reset,
                End::Eof => BackendFailure::ClosedEarly,
            }
        }
    }
    let close_head: &[u8] = b"HTTP/1.1 200 OK\r\n\r\n";
    let close_relayed: &[u8] = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n";
    for (head, piece, relayed_head, relayed, end) in [
        (
            close_head,
            b"hello".as_slice(),
            close_relayed,
            b"hello".as_slice(),
            End::Reset,
        ),
        (
            CHUNKED_HEAD,
            b"5\r\nhel",
            CHUNKED_RELAYED,
            b"5\r\nhel",
            End::Eof,
        ),
        (
            CHUNKED_HEAD,
            b"5\r\nhel",
            CHUNKED_RELAYED,
            b"5\r\nhel",
            End::Reset,
        ),
    ] {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, GET_X);
        let up = only_upstream(&sim);
        sim.backend_send(up, &cat(&[head, piece]));
        sim.run_until_idle();
        let before = sim.trace().len();
        match end {
            End::Reset => sim.backend_reset(up),
            End::Eof => sim.backend_eof(up),
        }
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            cat(&[relayed_head, relayed]),
            "{}",
            sim.trace()
        );
        assert_cut(&sim, c, before, end.kind());
    }
}
