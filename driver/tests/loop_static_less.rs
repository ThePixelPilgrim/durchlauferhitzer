//! The epoll event loop on loopback with a config that has no static
//! roots (stage 4 Task 5): fixed responses, keep-alive, the lingering
//! close, a proxied exchange to a std backend, a backend's early answer
//! followed by a reset (the stage 3 I1 rule, DRV-10) and the header
//! timeout (DRV-06). The fd-count check after a client reset is in
//! `tests/loop_fd_leak.rs`, alone in its binary (R4).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown};
use std::thread;
use std::time::{Duration, Instant};

use durchlauferhitzer::{Exit, LogEntry};
use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    BackendId, ConnId, Log, RejectReason, RequestLine, RequestSummary, TimeoutKind,
};

#[path = "support/server.rs"]
mod server;

#[allow(dead_code, reason = "shared with the sim's test binaries; we use part")]
#[path = "../../sim/tests/support/model_http.rs"]
mod model_http;

use server::{
    Server, accept, backend, config, date_of, read_head, read_response, read_to_eof, text,
};

/// A fixed response by the sim's spec-written model, with the `Date` the
/// response carried (FR-HTTP-22a: the driver sets the wall-clock second).
fn fixed(status: Status, persistence: Persistence, response: &[u8]) -> Vec<u8> {
    let date = date_of(response);
    model_http::model_fixed(status, persistence, BodyMode::Full, Some(&date))
}

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn summary(method: Method, host: &str, path: &str) -> RequestSummary {
    RequestSummary {
        method,
        host: Some(host.to_owned()),
        path: path.to_owned(),
    }
}

/// Stops the server; checks how the loop ended and the core's log
/// entries for the test's one connection, `ConnId(1)`.
fn stop_expecting(server: Server, logs: &[Log]) {
    let (exit, entries) = server.stop();
    assert_eq!(exit, Exit::Stopped);
    let expected: Vec<LogEntry> = logs.iter().cloned().map(LogEntry::Core).collect();
    assert_eq!(entries, expected);
}

#[test]
fn unknown_host_is_404_twice_on_one_keep_alive_connection() {
    let (_backend, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let mut client = server.connect();
    for _ in 0..2 {
        client
            .write_all(b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n")
            .unwrap();
        let response = read_response(&mut client);
        assert_eq!(
            text(&response),
            text(&fixed(Status::NotFound, Persistence::KeepAlive, &response))
        );
    }
    let not_found = Log::Response {
        conn: ConnId(1),
        peer: peer(),
        status: Status::NotFound,
        request: Some(summary(Method::Get, "other.test", "/x")),
    };
    stop_expecting(server, &[not_found.clone(), not_found]);
}

#[test]
fn bad_request_is_400_then_a_lingering_close() {
    let (_backend, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let mut client = server.connect();
    // No Host: a 400 that closes the connection.
    client.write_all(b"GET /x HTTP/1.1\r\n\r\n").unwrap();
    let response = read_response(&mut client);
    assert_eq!(
        text(&response),
        text(&fixed(Status::BadRequest, Persistence::Close, &response))
    );
    // The server shut down its sending side: end of stream after the
    // response.
    assert_eq!(read_to_eof(&mut client).unwrap(), b"");
    // Lingering: input after the response is read and discarded, not
    // answered with a reset; our end of stream ends it cleanly.
    client.write_all(&[b'z'; 10_000]).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(read_to_eof(&mut client).unwrap(), b"");
    stop_expecting(
        server,
        &[Log::Rejected {
            conn: ConnId(1),
            peer: peer(),
            reason: RejectReason::Http {
                error: HttpError::MissingHost,
                request: Some(RequestLine {
                    method: Method::Get,
                    path: "/x".to_owned(),
                }),
            },
        }],
    );
}

#[test]
fn proxied_get_relays_the_exact_bytes_and_forwards_the_rewritten_head() {
    let (backend_listener, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let mut client = server.connect();
    client
        .write_all(
            b"GET /api/x?y=1 HTTP/1.1\r\nHost: example.test\r\nAccept: */*\r\n\
              Connection: keep-alive\r\nKeep-Alive: timeout=5\r\n\r\n",
        )
        .unwrap();
    let mut upstream = accept(&backend_listener);
    // FR-PROXY-04…11, as the backend sees it.
    assert_eq!(
        text(&read_head(&mut upstream)),
        "GET /x?y=1 HTTP/1.1\r\nhost: example.test\r\naccept: */*\r\n\
         x-forwarded-for: 127.0.0.1\r\nx-forwarded-proto: http\r\n\
         connection: close\r\n\r\n"
    );
    upstream
        .write_all(
            b"HTTP/1.1 200 Fine\r\nDate: Sun, 06 Nov 1994 08:49:37 GMT\r\nServer: b\r\n\
              Keep-Alive: timeout=5\r\nConnection: keep-alive\r\nContent-Length: 5\r\n\r\nhello",
        )
        .unwrap();
    // FR-PROXY-12: the backend's Date kept, hop-by-hop headers removed.
    assert_eq!(
        text(&read_response(&mut client)),
        "HTTP/1.1 200 Fine\r\ndate: Sun, 06 Nov 1994 08:49:37 GMT\r\nserver: b\r\n\
         content-length: 5\r\n\r\nhello"
    );
    // The body ended at its framing: the upstream is closed.
    assert_eq!(read_to_eof(&mut upstream).unwrap(), b"");
    stop_expecting(
        server,
        &[Log::ProxiedResponse {
            conn: ConnId(1),
            peer: peer(),
            backend: BackendId(0),
            status: 200,
            request: summary(Method::Get, "example.test", "/api/x"),
        }],
    );
}

/// DRV-10: a client's errors are reported whatever its interest. While
/// the request waits for the backend the client is not read (interest
/// `None`); it resets (it closes with our `100 Continue` unread), and the
/// driver reports it at once: the core logs the reset and closes the
/// upstream, without waiting for the backend.
#[test]
fn client_reset_while_awaiting_the_backend_is_reported_at_once() {
    let (backend_listener, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let mut client = server.connect();
    client
        .write_all(
            b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
              Content-Length: 5\r\n\r\nhello",
        )
        .unwrap();
    let mut upstream = accept(&backend_listener);
    read_head(&mut upstream);
    let mut body = [0u8; 5];
    upstream.read_exact(&mut body).unwrap();
    // The `100 Continue` arrived; closing with it unread resets.
    let mut probe = [0u8; 1];
    assert_eq!(client.peek(&mut probe).unwrap(), 1);
    drop(client);
    // The upstream was closed with it.
    assert_eq!(read_to_eof(&mut upstream).unwrap(), b"");
    stop_expecting(
        server,
        &[Log::Reset {
            conn: ConnId(1),
            peer: peer(),
        }],
    );
}

/// The stage 3 I1 rule on a real kernel (DRV-10): the backend takes the
/// head of a 4 MiB upload but none of its body, so the pipeline fills and
/// the driver waits to write with interest `ReadWrite`. Then the backend
/// answers 413 and closes with the body unread, so its kernel resets the
/// connection. The driver must read the 413 before it reports the reset
/// (a failing write under `ReadWrite` is not reported by itself), and the
/// client gets the 413, not a 502.
#[test]
fn early_413_then_reset_during_a_4_mib_upload_reaches_the_client() {
    let (backend_listener, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let body = vec![b'u'; 4 * 1024 * 1024];
    let total = body.len();
    let mut client = server.connect();
    let mut writer = client.try_clone().unwrap();
    let upload = thread::spawn(move || {
        let head = format!(
            "POST /api/up HTTP/1.1\r\nHost: example.test\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        // The server stops taking the body once it answered; errors from
        // here on are expected.
        let _ = writer.write_all(head.as_bytes());
        let _ = writer.write_all(&body);
    });
    let mut upstream = accept(&backend_listener);
    let head = read_head(&mut upstream);
    assert!(
        text(&head).starts_with("POST /up HTTP/1.1\r\n"),
        "{}",
        text(&head)
    );
    // Wait for the pipeline to fill: the body bytes queued unread at the
    // backend stop growing, short of the whole body, so the driver waits
    // to write. No event tells us, so watch the queue. Bytes queued unread
    // also make the close below a reset.
    let mut queue = vec![0u8; total];
    let queued = stalled(|| upstream.peek(&mut queue).unwrap());
    assert!(
        queued > 0 && queued < total,
        "{queued} of {total} bytes queued"
    );
    upstream
        .write_all(
            b"HTTP/1.1 413 Too Big\r\nDate: Sun, 06 Nov 1994 08:49:37 GMT\r\n\
              Content-Length: 2\r\n\r\nno",
        )
        .unwrap();
    drop(upstream);
    assert_eq!(
        text(&read_response(&mut client)),
        "HTTP/1.1 413 Too Big\r\ndate: Sun, 06 Nov 1994 08:49:37 GMT\r\n\
         content-length: 2\r\nconnection: close\r\n\r\nno"
    );
    client.shutdown(Shutdown::Both).unwrap();
    upload.join().unwrap();
    stop_expecting(
        server,
        &[Log::ProxiedResponse {
            conn: ConnId(1),
            peer: peer(),
            backend: BackendId(0),
            status: 413,
            request: summary(Method::Other("POST".to_owned()), "example.test", "/api/up"),
        }],
    );
}

/// What `measure` returns once it has not changed for 200 ms (at most
/// `PATIENCE`).
fn stalled(mut measure: impl FnMut() -> usize) -> usize {
    let start = Instant::now();
    let mut last = measure();
    let mut still = 0;
    while still < 4 && start.elapsed() < server::PATIENCE {
        thread::sleep(Duration::from_millis(50));
        let now = measure();
        if now == last {
            still += 1;
        } else {
            still = 0;
            last = now;
        }
    }
    last
}

/// DRV-06: the loop arms its wait from `next_deadline`; a head that does
/// not complete within `header_timeout` (400 ms here) is answered 408 at
/// about that time (±200 ms).
#[test]
fn header_timeout_answers_408_on_time() {
    let (_backend, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let start = Instant::now();
    let mut client = server.connect();
    client.write_all(b"GET /api/x HTTP/1.1\r\n").unwrap();
    let response = read_response(&mut client);
    let elapsed = start.elapsed();
    assert_eq!(
        text(&response),
        text(&fixed(
            Status::RequestTimeout,
            Persistence::Close,
            &response
        ))
    );
    assert!(
        elapsed >= Duration::from_millis(200) && elapsed <= Duration::from_millis(600),
        "408 after {elapsed:?}"
    );
    stop_expecting(
        server,
        &[Log::Rejected {
            conn: ConnId(1),
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::Header),
        }],
    );
}
