//! HTTP/1.0 clients on proxy routes (FR-PROXY-04): the request line goes
//! out as HTTP/1.0, and no `Transfer-Encoding` ever reaches the client.

use super::*;

const GET_10: &[u8] = b"GET /api/x HTTP/1.0\r\nHost: example.test\r\n\r\n";

/// FR-PROXY-04 (owner decision 2026-10-27): an HTTP/1.0 client's request
/// goes to the backend as HTTP/1.0, and a `Content-Length` answer is
/// relayed as usual.
#[test]
fn http10_request_line_is_http10() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_10);
    let up = only_upstream(&sim);
    assert_eq!(
        sim.take_backend_request(up),
        b"GET /x HTTP/1.0\r\nhost: example.test\r\nx-forwarded-for: 192.0.2.1\r\n\
          x-forwarded-proto: http\r\nconnection: close\r\n\r\n",
        "{}",
        sim.trace()
    );
    sim.backend_send(up, b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

/// FR-PROXY-04, -21 (owner decision 2026-10-27): a backend that still
/// answers an HTTP/1.0 client's request with a chunked body gets a 502
/// before any byte reaches the client.
#[test]
fn chunked_response_to_http10_client_is_502() {
    let mut sim = Sim::new(CONFIG);
    let c = request(&mut sim, GET_10);
    let up = only_upstream(&sim);
    sim.backend_send(up, &cat(&[CHUNKED_HEAD, b"2\r\nok\r\n0\r\n\r\n"]));
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::BadGateway, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[
            failed(c, BackendFailure::TransferEncodingToHttp10),
            Log::Response {
                conn: c,
                peer: peer(),
                status: Status::BadGateway,
                request: Some(summary(Method::Get, "/api/x")),
            },
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

/// FR-PROXY-04 (owner decision 2026-10-27): any `Transfer-Encoding` toward
/// an HTTP/1.0 client is a 502, also on a reply without a body (to HEAD, a
/// 304): the backend is at fault. Nothing of the reply reaches the client.
#[test]
fn transfer_encoding_to_http10_client_is_502_also_without_a_body() {
    for (request_bytes, method, response, mode) in [
        (
            b"HEAD /api/x HTTP/1.0\r\nHost: example.test\r\n\r\n".as_slice(),
            Method::Head,
            CHUNKED_HEAD,
            BodyMode::HeadOnly,
        ),
        (
            GET_10,
            Method::Get,
            b"HTTP/1.1 304 Not Modified\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
            BodyMode::Full,
        ),
        (
            GET_10,
            Method::Get,
            b"HTTP/1.1 204 No Content\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
            BodyMode::Full,
        ),
    ] {
        let mut sim = Sim::new(CONFIG);
        let c = request(&mut sim, request_bytes);
        let up = only_upstream(&sim);
        sim.backend_send(up, response);
        sim.run_until_idle();
        assert_eq!(
            sim.take_received(c),
            fixed(Status::BadGateway, Persistence::Close, mode),
            "{}",
            sim.trace()
        );
        assert_eq!(
            sim.logs(),
            &[
                failed(c, BackendFailure::TransferEncodingToHttp10),
                Log::Response {
                    conn: c,
                    peer: peer(),
                    status: Status::BadGateway,
                    request: Some(summary(method, "/api/x")),
                },
            ],
            "{}",
            sim.trace()
        );
        assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
        let closes = sim.trace().matches("-> CloseUpstream").count();
        assert_eq!(closes, 1, "{}", sim.trace());
    }
}
