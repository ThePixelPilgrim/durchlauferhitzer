//! Backend-directed files end to end (FR-PROXY-32 Trigger, Bad path,
//! Missing file, the client's response, the backend's body), under the
//! sim's contract checks: a backend's `200` naming a file is answered with
//! the file's exact bytes behind a merged head, every other answer as the
//! spec says. The expected heads are written out here, the file bytes
//! taken from the sim's disk. A slow disk and a slow client included.

use std::time::Duration;

use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnId, DrainTarget, FileError, FileStamp, Log, UpstreamHolder,
};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

use super::{fixed, only_upstream, peer, request, summary};

/// `/dl` (and `/grow`, with `allow_growth`) serve `x-accel-redirect
/// /protected/` below `/srv/files`, `/sf` `x-sendfile` below the same
/// root; pooled (the default), the response body capped at 10 bytes.
const SERVE: &str = "listen 127.0.0.1:8080;
host example.test {
  path /dl { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 10;
    internal_files x-accel-redirect /protected/ /srv/files; }
  path /grow { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 10;
    internal_files x-accel-redirect /protected/ /srv/files; on_file_change allow_growth; }
  path /sf { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 10;
    internal_files x-sendfile /srv/files; }
}";

const ROOT: &str = "/srv/files";

const GET: &[u8] = b"GET /dl/x HTTP/1.1\r\nHost: example.test\r\n\r\n";

/// A backend `200` naming `a.pdf`, with `Content-Length: 0`.
const NAMES_A: &[u8] =
    b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 0\r\n\r\n";

/// `n` bytes of a pattern no backend body here repeats.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| b'a' + (i % 23) as u8).collect()
}

/// A sim serving `SERVE`, its root holding `a.pdf` (1000 bytes),
/// `big.bin` (600 KiB, more than two chunks), a directory `d` and a
/// dotfile `.env`.
fn sim() -> Sim {
    let mut sim = Sim::new(SERVE);
    sim.add_file(ROOT, "a.pdf", &pattern(1000));
    sim.add_file(ROOT, "big.bin", &pattern(600 * 1024));
    sim.add_dir(ROOT, "d");
    sim.add_file(ROOT, ".env", b"secret");
    sim
}

/// The `ETag` and `Last-Modified` lines of `name` (FR-STATIC-18).
fn validators(sim: &Sim, name: &str) -> String {
    let FileStamp { size, modified_ns } = sim.file_stamp(ROOT, name);
    format!(
        "ETag: \"{size:x}-{modified_ns:x}\"\r\nLast-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\n"
    )
}

/// The client sends `wire`, the backend answers `backend`; pumped until
/// idle. The client and its one upstream.
fn served(sim: &mut Sim, wire: &[u8], backend: &[u8]) -> ConnId {
    let c = request(sim, wire);
    let up = only_upstream(sim);
    sim.take_backend_request(up);
    sim.backend_send(up, backend);
    sim.run_until_idle();
    c
}

fn backend_file(c: ConnId, status: Status, method: Method, path: &str) -> Log {
    Log::BackendFile {
        conn: c,
        peer: peer(),
        backend: BackendId(0),
        status,
        request: summary(method, path),
    }
}

fn response(c: ConnId, status: Status, method: Method, path: &str) -> Log {
    Log::Response {
        conn: c,
        peer: peer(),
        status,
        request: Some(summary(method, path)),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The merged `200` of `a.pdf` with no backend headers, keep-alive.
fn plain_200(sim: &Sim) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 1000\r\n{}\
         Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        validators(sim, "a.pdf")
    )
}

/// Trigger and the backend's body: the file's exact bytes behind the
/// merged head (backend headers kept, its framing replaced), the body
/// never sent; the upstream pooled after `Content-Length: 0`, closed
/// after a body; logged once.
#[test]
fn the_named_file_is_served() {
    let mut sim = sim();
    let c = served(
        &mut sim,
        GET,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-custom\r\n\
          Content-Disposition: attachment\r\nX-Accel-Redirect: /protected/a.pdf\r\n\
          ETag: \"b\"\r\nContent-Length: 0\r\n\r\n",
    );
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/x-custom\r\n\
         content-disposition: attachment\r\nContent-Length: 1000\r\n{}\
         Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        validators(&sim, "a.pdf")
    );
    let mut expected = head.into_bytes();
    expected.extend(pattern(1000));
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        [backend_file(c, Status::Ok, Method::Get, "/dl/x")],
        "{}",
        sim.trace()
    );
    let up = only_upstream(&sim);
    assert_eq!(sim.upstream_holder(up), Some(UpstreamHolder::Pool));
    assert_eq!(sim.state(c), SimConnState::Open);
    assert_eq!(sim.open_files(), 0);

    // A body: never read, never sent; the upstream closes.
    let mut sim = self::sim();
    let c = served(
        &mut sim,
        GET,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 5\r\n\r\nhello",
    );
    let mut expected = plain_200(&sim).into_bytes();
    expected.extend(pattern(1000));
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// `x-sendfile` names the file by its path below the root.
#[test]
fn x_sendfile_serves_the_file() {
    let mut sim = sim();
    let c = served(
        &mut sim,
        b"GET /sf/x HTTP/1.1\r\nHost: example.test\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nX-Sendfile: /srv/files/a.pdf\r\nContent-Length: 0\r\n\r\n",
    );
    let mut expected = plain_200(&sim).into_bytes();
    expected.extend(pattern(1000));
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );
}

/// Conditionals, ranges and HEAD as on a static route (FR-STATIC-13, -18,
/// -19); a POST gets the whole file.
#[test]
fn conditionals_ranges_head_and_methods() {
    let mut sim = sim();
    let etag = sim.file_stamp(ROOT, "a.pdf");
    let inm = format!(
        "GET /dl/x HTTP/1.1\r\nHost: example.test\r\nIf-None-Match: \"{:x}-{:x}\"\r\n\r\n",
        etag.size, etag.modified_ns
    );
    let backend = b"HTTP/1.1 200 OK\r\nCache-Control: private\r\n\
                    X-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 0\r\n\r\n";
    let c = served(&mut sim, inm.as_bytes(), backend);
    assert_eq!(
        text(&sim.take_received(c)),
        format!(
            "HTTP/1.1 304 Not Modified\r\ncache-control: private\r\n{}\r\n",
            validators(&sim, "a.pdf")
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        [backend_file(c, Status::NotModified, Method::Get, "/dl/x")]
    );
    assert_eq!(sim.open_files(), 0);

    let mut sim = self::sim();
    let c = served(
        &mut sim,
        b"GET /dl/x HTTP/1.1\r\nHost: example.test\r\nRange: bytes=100-199\r\n\r\n",
        backend,
    );
    let mut expected = format!(
        "HTTP/1.1 206 Partial Content\r\ncache-control: private\r\n\
         Content-Type: application/pdf\r\nContent-Length: 100\r\n\
         Content-Range: bytes 100-199/1000\r\n{}Accept-Ranges: bytes\r\n\
         X-Content-Type-Options: nosniff\r\n\r\n",
        validators(&sim, "a.pdf")
    )
    .into_bytes();
    expected.extend(&pattern(1000)[100..200]);
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );

    let mut sim = self::sim();
    let c = served(
        &mut sim,
        b"GET /dl/x HTTP/1.1\r\nHost: example.test\r\nRange: bytes=5000-\r\n\r\n",
        backend,
    );
    assert_eq!(
        text(&sim.take_received(c)),
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: 26\r\nContent-Range: bytes */1000\r\nAccept-Ranges: bytes\r\n\
         X-Content-Type-Options: nosniff\r\n\r\n416 Range Not Satisfiable\n"
    );
    assert_eq!(
        sim.logs(),
        [response(
            c,
            Status::RangeNotSatisfiable,
            Method::Get,
            "/dl/x"
        )]
    );

    let mut sim = self::sim();
    let c = served(
        &mut sim,
        b"HEAD /dl/x HTTP/1.1\r\nHost: example.test\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 99\r\n\r\n",
    );
    assert_eq!(
        text(&sim.take_received(c)),
        plain_200(&sim),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0);

    let mut sim = self::sim();
    let c = served(
        &mut sim,
        b"POST /dl/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 3\r\n\
          Range: bytes=0-1\r\n\r\nabc",
        NAMES_A,
    );
    let mut expected = plain_200(&sim).into_bytes();
    expected.extend(pattern(1000));
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open);
}

/// Missing file: whatever a static route answers 404 (missing, a
/// directory, a trailing `/`, a dotfile, a symlink) is the plain 404,
/// logged as a `Response` only; the upstream is still pooled.
#[test]
fn not_found_is_the_plain_404() {
    for value in [
        "/protected/missing",
        "/protected/d",
        "/protected/a.pdf/",
        "/protected/",
        "/protected/.env",
        "/protected/link",
    ] {
        let mut sim = sim();
        sim.add_symlink(ROOT, "link", "/etc/passwd");
        let backend =
            format!("HTTP/1.1 200 OK\r\nX-Accel-Redirect: {value}\r\nContent-Length: 0\r\n\r\n");
        let c = served(&mut sim, GET, backend.as_bytes());
        assert_eq!(
            sim.take_received(c),
            fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::Full),
            "{value}: {}",
            sim.trace()
        );
        assert_eq!(
            sim.logs(),
            [response(c, Status::NotFound, Method::Get, "/dl/x")],
            "{value}"
        );
        let up = only_upstream(&sim);
        assert_eq!(
            sim.upstream_holder(up),
            Some(UpstreamHolder::Pool),
            "{value}"
        );
        assert_eq!(sim.open_files(), 0, "{value}");
    }
}

/// Bad path: a `502`, logged as `BackendFailed` with `BadFilePath`, the
/// upstream closed, nothing of the root sent.
#[test]
fn bad_paths_are_502() {
    for lines in [
        "X-Accel-Redirect: /etc/passwd\r\n",
        "X-Accel-Redirect: /protected/../a.pdf\r\n",
        "X-Accel-Redirect: /protected/a%zz\r\n",
        "X-Accel-Redirect: /protected/a.pdf\r\nX-Accel-Redirect: /protected/a.pdf\r\n",
    ] {
        let mut sim = sim();
        let backend = format!("HTTP/1.1 200 OK\r\n{lines}Content-Length: 0\r\n\r\n");
        let c = served(&mut sim, GET, backend.as_bytes());
        assert_eq!(
            sim.take_received(c),
            fixed(Status::BadGateway, Persistence::KeepAlive, BodyMode::Full),
            "{lines}: {}",
            sim.trace()
        );
        assert_eq!(
            sim.logs(),
            [
                Log::BackendFailed {
                    conn: c,
                    peer: peer(),
                    backend: BackendId(0),
                    kind: BackendFailure::BadFilePath,
                },
                response(c, Status::BadGateway, Method::Get, "/dl/x"),
            ],
            "{lines}"
        );
        assert_eq!(sim.open_upstreams(), 0, "{lines}");
    }
}

/// No proxy limit applies: a chunked head toward an HTTP/1.0 client and a
/// body over `max_response_body` are no errors here.
#[test]
fn no_proxy_limits_on_the_file() {
    let mut sim = sim();
    let c = served(
        &mut sim,
        b"GET /dl/x HTTP/1.0\r\nHost: example.test\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
          X-Accel-Redirect: /protected/a.pdf\r\n\r\n",
    );
    let head = plain_200(&sim).replace("\r\n\r\n", "\r\nConnection: close\r\n\r\n");
    let mut expected = head.into_bytes();
    expected.extend(pattern(1000));
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );
    assert_ne!(sim.state(c), SimConnState::Open);

    let mut sim = self::sim();
    let c = served(
        &mut sim,
        GET,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 5000\r\n\r\n",
    );
    let mut expected = plain_200(&sim).into_bytes();
    expected.extend(pattern(1000));
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );
}

/// `on_file_change` is the route's: `abort` cuts a file that changes
/// mid-transfer (FR-STATIC-15), `allow_growth` sends exactly the size at
/// open.
#[test]
fn file_changes_follow_the_routes_setting() {
    let big = 600 * 1024;
    let grown = |sim: &Sim| FileStamp {
        size: big as u64 + 1,
        modified_ns: sim.file_stamp(ROOT, "big.bin").modified_ns + 1,
    };
    let names_big = b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/big.bin\r\n\
                      Content-Length: 0\r\n\r\n";
    let mut sim = sim();
    let new = grown(&sim);
    sim.change_during_send(ROOT, "big.bin", 1, new);
    let c = served(&mut sim, GET, names_big);
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        [
            backend_file(c, Status::Ok, Method::Get, "/dl/x"),
            Log::FileAborted {
                conn: c,
                peer: peer(),
                error: FileError::Changed,
            },
        ],
        "{}",
        sim.trace()
    );

    let mut sim = self::sim();
    let new = grown(&sim);
    sim.change_during_send(ROOT, "big.bin", 1, new);
    let c = served(
        &mut sim,
        b"GET /grow/x HTTP/1.1\r\nHost: example.test\r\n\r\n",
        names_big,
    );
    let received = sim.take_received(c);
    assert!(
        received.ends_with(&pattern(big)),
        "{}",
        text(&received[..200])
    );
    assert!(text(&received).contains(&format!("Content-Length: {big}\r\n")));
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// A slow disk and a slow client: the merged head and every byte of a
/// file of several chunks arrive in order, a few bytes per write.
#[test]
fn slow_disk_and_slow_client() {
    let mut sim = sim();
    sim.set_disk_delay(Duration::from_millis(50));
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Bytes(4096));
    sim.send(c, GET);
    sim.run_until_idle();
    let up = only_upstream(&sim);
    sim.take_backend_request(up);
    sim.backend_send(
        up,
        b"HTTP/1.1 200 OK\r\nCache-Control: private\r\nX-Accel-Redirect: /protected/big.bin\r\n\
          Content-Length: 0\r\n\r\n",
    );
    let mut received = Vec::new();
    for _ in 0..1000 {
        sim.run_until_idle();
        received.extend(sim.take_received(c));
        if sim.open_files() == 0 && !sim.opening_file(c) && received.len() > 600 * 1024 {
            break;
        }
        sim.advance(Duration::from_millis(50));
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\ncache-control: private\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n{}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        600 * 1024,
        validators(&sim, "big.bin")
    );
    let mut expected = head.into_bytes();
    expected.extend(pattern(600 * 1024));
    assert!(
        received == expected,
        "{} of {} bytes",
        received.len(),
        expected.len()
    );
    assert_eq!(
        sim.logs(),
        [backend_file(c, Status::Ok, Method::Get, "/dl/x")]
    );
}

/// A request pipelined behind a backend file is answered after it, on
/// the pooled upstream.
#[test]
fn pipelined_request_is_answered_after_the_file() {
    let mut sim = sim();
    let mut wire = GET.to_vec();
    wire.extend_from_slice(b"GET /dl/y HTTP/1.1\r\nHost: example.test\r\n\r\n");
    let c = served(&mut sim, &wire, NAMES_A);
    let up = only_upstream(&sim);
    assert!(
        text(&sim.take_backend_request(up)).starts_with("GET /dl/y HTTP/1.1\r\n"),
        "{}",
        sim.trace()
    );
    sim.backend_send(
        up,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 0\r\n\r\n",
    );
    sim.run_until_idle();
    let mut one = plain_200(&sim).into_bytes();
    one.extend(pattern(1000));
    let mut expected = one.clone();
    expected.extend(one);
    assert_eq!(
        text(&sim.take_received(c)),
        text(&expected),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        [
            backend_file(c, Status::Ok, Method::Get, "/dl/x"),
            backend_file(c, Status::Ok, Method::Get, "/dl/y"),
        ]
    );
}

/// A client taking `window` bytes per write asks for `big.bin`; the
/// backend names it. Pumped until idle: the head and part of the file
/// are out.
fn sending_big(sim: &mut Sim) -> ConnId {
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Budget(64 * 1024));
    sim.send(c, GET);
    sim.run_until_idle();
    let up = only_upstream(sim);
    sim.take_backend_request(up);
    sim.backend_send(
        up,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/big.bin\r\nContent-Length: 0\r\n\r\n",
    );
    sim.run_until_idle();
    c
}

/// Review Focus 5: a shutdown drain lets a backend file being sent
/// finish, then closes; the fast shutdown cuts a backend file being
/// opened or sent, and the late open is closed as an orphan; a client
/// reset mid-file closes the file once (the sim checks every file is
/// closed with its connection).
#[test]
fn drain_abort_and_reset_around_a_backend_file() {
    let mut sim = sim();
    let c = sending_big(&mut sim);
    assert_eq!(sim.open_files(), 1, "{}", sim.trace());
    sim.begin_drain(DrainTarget::Shutdown);
    sim.set_client_window(c, Window::Unlimited);
    sim.run_until_idle();
    let received = sim.take_received(c);
    assert!(received.ends_with(&pattern(600 * 1024)), "{}", sim.trace());
    assert_ne!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0);

    let mut sim = self::sim();
    let _ = sending_big(&mut sim);
    sim.abort_all();
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert!(sim.is_drained(), "{}", sim.trace());

    let mut sim = self::sim();
    sim.set_disk_delay(Duration::from_millis(50));
    let c = request(&mut sim, GET);
    let up = only_upstream(&sim);
    sim.take_backend_request(up);
    sim.backend_send(up, NAMES_A);
    sim.run_until_idle();
    assert!(sim.opening_file(c), "{}", sim.trace());
    sim.abort_all();
    assert!(!sim.is_drained(), "the open is unanswered");
    sim.advance(Duration::from_millis(50));
    sim.run_until_idle();
    assert!(sim.is_drained(), "{}", sim.trace());

    let mut sim = self::sim();
    let c = sending_big(&mut sim);
    sim.reset(c);
    sim.run_until_idle();
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed);
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Reset {
            conn: c,
            peer: peer(),
        })
    );
}
