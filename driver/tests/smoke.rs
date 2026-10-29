//! The end-to-end smoke suite (stage 4 Task 10, NFR-TEST-07): one run of
//! the built binary serves a static host and a proxy host at once, over
//! real loopback sockets, against a test backend thread.
//!
//! The mix is 200 requests over 32 keep-alive connections, all running in
//! parallel: static 200 (a file and a directory index), 404 and 301; a
//! proxied 1 MiB GET (the backend sends no `Date`, so the core adds its
//! own) and 1 MiB POST (echoed back), a chunked upload answered chunked,
//! an early 413 (a `Content-Length` over `max_request_body`, nothing
//! forwarded), a refused backend (502) and a hanging one (504 after
//! `backend_response_timeout 1s`). Which connection sends what is fixed
//! ([`plan`]), so every assertion is deterministic however the threads
//! are scheduled:
//!
//! - each response is compared byte for byte: the core's own responses
//!   with the sim's spec-written model (`sim/tests/support/model_http.rs`,
//!   with the `Date` the response carried, always 29 bytes), relayed
//!   ones with the bytes FR-PROXY-04…12 and FR-HTTP-22a give; every
//!   request the backend gets is compared byte for byte too;
//! - every `Date` the core renders is the wall-clock second of its
//!   exchange, between sending the request and reading the head (DRV-13;
//!   the IMF-fixdate written here from the calendar rules);
//! - after the last response of every connection the client half-closes
//!   and must read the end of stream with no byte more;
//! - the server's stderr must hold exactly one line per expected entry
//!   (compared as multisets): every line exact except its `conn=` field,
//!   the one value that depends on the accept order, which is removed
//!   before comparing (its presence is checked);
//! - the descriptor count of the server (`/proc/<pid>/fd`) is back to its
//!   value before the load once every connection is closed;
//! - SIGTERM then gives `kind=shutdown phase=draining`, `kind=shutdown
//!   phase=done detached=0` and exit 0.
//!
//! The server runs as non-root (as `nobody` when the tests run as root).
//! A response comes before any signal (a SIGTERM before startup step 8
//! would kill the process by its default action).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

#[allow(dead_code, reason = "shared with the sim's test binaries; we use part")]
#[path = "../../sim/tests/support/model_http.rs"]
mod model_http;

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use child::{PATIENCE, Running, Setup, TempDir, free_port, text};
use durchlauferhitzer_core::http::request::Persistence;
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use model_http::{model_file_head, model_fixed, model_redirect};

const CONNECTIONS: usize = 32;
const REQUESTS: usize = 200;
const MIB: usize = 1024 * 1024;

/// The `Date` a backend sends where the test wants it kept byte for byte.
const BACKEND_DATE: &str = "Sun, 06 Nov 1994 08:49:37 GMT";

const PAGE_TYPE: &str = "text/html; charset=utf-8";
const INDEX: &[u8] = b"<!doctype html><title>index</title>\n";

const DRAINING: &str = "kind=shutdown phase=draining";
const DONE: &str = "kind=shutdown phase=done detached=0";

/// What one request of the mix is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `GET /page.html` on the static host: 200 with the file.
    File,
    /// `GET /dir/`: 200 with the directory's `index.html`.
    Index,
    /// `GET /missing.txt`: 404.
    NotFound,
    /// `GET /dir`: 301 to `/dir/`.
    Redirect,
    /// `GET /api/get`: a 1 MiB body from the backend, without `Date`.
    ProxyGet,
    /// `POST /api/echo` of 1 MiB (`Content-Length` with a leading zero),
    /// echoed back.
    ProxyPost,
    /// A chunked `POST /api/chunked`, answered chunked.
    Chunked,
    /// `POST /api/echo` with a `Content-Length` over `max_request_body`:
    /// 413 and close, nothing forwarded. Only ever last on a connection.
    TooLarge,
    /// `GET /down`: the route's backend refuses the connect, 502.
    Refused,
    /// `GET /api/hang`: the backend takes the request and never answers,
    /// 504 after `backend_response_timeout`.
    Hang,
}

/// The kinds every connection cycles through ([`Kind::TooLarge`] closes
/// the connection, so it is placed last instead).
const CYCLE: [Kind; 9] = [
    Kind::File,
    Kind::Index,
    Kind::NotFound,
    Kind::Redirect,
    Kind::ProxyGet,
    Kind::ProxyPost,
    Kind::Chunked,
    Kind::Refused,
    Kind::Hang,
];

/// The requests of each connection: 8 connections send 7, 24 send 6
/// (200 in all); every fourth connection ends with a 413, the others with
/// the next kind of [`CYCLE`].
fn plan() -> Vec<Vec<Kind>> {
    let mut next = 0;
    let plan: Vec<Vec<Kind>> = (0..CONNECTIONS)
        .map(|conn| {
            let count = if conn < 8 { 7 } else { 6 };
            (0..count)
                .map(|i| {
                    if i + 1 == count && conn % 4 == 3 {
                        Kind::TooLarge
                    } else {
                        next += 1;
                        CYCLE[(next - 1) % CYCLE.len()]
                    }
                })
                .collect()
        })
        .collect();
    assert_eq!(plan.iter().map(Vec::len).sum::<usize>(), REQUESTS);
    plan
}

/// Deterministic bytes for bodies and files.
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 31 + seed * 7 + i / 251) % 256).unwrap())
        .collect()
}

fn page() -> Vec<u8> {
    pattern(1, 300 * 1024)
}

fn get_body() -> Vec<u8> {
    pattern(2, MIB)
}

fn post_body() -> Vec<u8> {
    pattern(3, MIB)
}

/// A chunked body with `chunks` of these sizes, hex in mixed case and
/// with leading zeros (FR-PROXY-19 allows both), then the last chunk.
fn chunked(seed: usize, sizes: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, &size) in sizes.iter().enumerate() {
        let hex = if i % 2 == 0 {
            format!("{size:x}")
        } else {
            format!("00{size:X}")
        };
        out.extend_from_slice(hex.as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&pattern(seed + i, size));
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

fn chunked_request_body() -> Vec<u8> {
    chunked(10, &[1, 0xAB, 0x1_0000, 0x3_0000, 7, 0x2_0001])
}

fn chunked_response_body() -> Vec<u8> {
    chunked(20, &[0x8000, 3, 0x4_0000, 0xFF])
}

/// What the backend gets for each kind it sees (FR-PROXY-04…11): the
/// rewritten head, then the body as sent (verbatim, FR-PROXY-19).
fn backend_request(kind: Kind) -> Vec<u8> {
    const TAIL: &str = "x-forwarded-for: 127.0.0.1\r\nx-forwarded-proto: http\r\n\
                        connection: close\r\n\r\n";
    match kind {
        Kind::ProxyGet => format!("GET /get HTTP/1.1\r\nhost: proxy.test\r\n{TAIL}").into_bytes(),
        Kind::Hang => format!("GET /hang HTTP/1.1\r\nhost: proxy.test\r\n{TAIL}").into_bytes(),
        Kind::ProxyPost => [
            format!("POST /echo HTTP/1.1\r\nhost: proxy.test\r\ncontent-length: {MIB}\r\n{TAIL}")
                .as_bytes(),
            &post_body(),
        ]
        .concat(),
        Kind::Chunked => [
            format!(
                "POST /chunked HTTP/1.1\r\nhost: proxy.test\r\ntransfer-encoding: chunked\r\n\
                 {TAIL}"
            )
            .as_bytes(),
            &chunked_request_body(),
        ]
        .concat(),
        Kind::File
        | Kind::Index
        | Kind::NotFound
        | Kind::Redirect
        | Kind::TooLarge
        | Kind::Refused => panic!("{kind:?} never reaches the backend"),
    }
}

/// What the backend answers.
fn backend_response(kind: Kind) -> Vec<u8> {
    match kind {
        Kind::ProxyGet => [
            format!("HTTP/1.1 200 OK\r\nContent-Length: {MIB}\r\nConnection: keep-alive\r\n\r\n")
                .as_bytes(),
            &get_body(),
        ]
        .concat(),
        Kind::ProxyPost => [
            format!(
                "HTTP/1.1 200 Echo\r\nDate: {BACKEND_DATE}\r\nServer: smoke\r\n\
                 Content-Length: {MIB}\r\n\r\n"
            )
            .as_bytes(),
            &post_body(),
        ]
        .concat(),
        Kind::Chunked => [
            format!(
                "HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\nDate: {BACKEND_DATE}\r\n\
                 Keep-Alive: timeout=5\r\n\r\n"
            )
            .as_bytes(),
            &chunked_response_body(),
        ]
        .concat(),
        Kind::File
        | Kind::Index
        | Kind::NotFound
        | Kind::Redirect
        | Kind::TooLarge
        | Kind::Refused
        | Kind::Hang => panic!("the backend answers no {kind:?}"),
    }
}

/// The request the client sends.
fn client_request(kind: Kind) -> Vec<u8> {
    let get = |host: &str, path: &str| format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n");
    match kind {
        Kind::File => get("static.test", "/page.html").into_bytes(),
        Kind::Index => get("static.test", "/dir/").into_bytes(),
        Kind::NotFound => get("static.test", "/missing.txt").into_bytes(),
        Kind::Redirect => get("static.test", "/dir").into_bytes(),
        Kind::ProxyGet => get("proxy.test", "/api/get").into_bytes(),
        Kind::Refused => get("proxy.test", "/down").into_bytes(),
        Kind::Hang => get("proxy.test", "/api/hang").into_bytes(),
        Kind::ProxyPost => [
            format!(
                "POST /api/echo HTTP/1.1\r\nHost: proxy.test\r\nContent-Length: 0{MIB}\r\n\r\n"
            )
            .as_bytes(),
            &post_body(),
        ]
        .concat(),
        Kind::Chunked => [
            b"POST /api/chunked HTTP/1.1\r\nHost: proxy.test\r\nTransfer-Encoding: chunked\r\n\r\n"
                .as_slice(),
            &chunked_request_body(),
        ]
        .concat(),
        Kind::TooLarge => format!(
            "POST /api/echo HTTP/1.1\r\nHost: proxy.test\r\nContent-Length: {}\r\n\r\n",
            MIB + 1
        )
        .into_bytes(),
    }
}

/// The response the client must get, given the `Date` the response
/// carried (`None` for one that must carry the backend's).
fn client_response(kind: Kind, date: Option<&str>) -> Vec<u8> {
    let keep = Persistence::KeepAlive;
    let date = || date.expect("a core-rendered response carries Date");
    match kind {
        Kind::File => [
            model_file_head(PAGE_TYPE, page().len() as u64, keep, Some(date())),
            page(),
        ]
        .concat(),
        Kind::Index => [
            model_file_head(PAGE_TYPE, INDEX.len() as u64, keep, Some(date())),
            INDEX.to_vec(),
        ]
        .concat(),
        Kind::NotFound => model_fixed(Status::NotFound, keep, BodyMode::Full, Some(date())),
        Kind::Redirect => model_redirect("/dir/", keep, BodyMode::Full, Some(date())),
        Kind::TooLarge => model_fixed(
            Status::ContentTooLarge,
            Persistence::Close,
            BodyMode::Full,
            Some(date()),
        ),
        Kind::Refused => model_fixed(Status::BadGateway, keep, BodyMode::Full, Some(date())),
        Kind::Hang => model_fixed(Status::GatewayTimeout, keep, BodyMode::Full, Some(date())),
        // FR-PROXY-12, FR-HTTP-22a: hop-by-hop headers gone, names in
        // lower case, the core's `date` behind the backend's headers.
        Kind::ProxyGet => [
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {MIB}\r\ndate: {}\r\n\r\n",
                date()
            )
            .as_bytes(),
            &get_body(),
        ]
        .concat(),
        Kind::ProxyPost => [
            format!(
                "HTTP/1.1 200 Echo\r\ndate: {BACKEND_DATE}\r\nserver: smoke\r\n\
                 content-length: {MIB}\r\n\r\n"
            )
            .as_bytes(),
            &post_body(),
        ]
        .concat(),
        Kind::Chunked => [
            format!(
                "HTTP/1.1 201 Created\r\ntransfer-encoding: chunked\r\ndate: {BACKEND_DATE}\r\n\r\n"
            )
            .as_bytes(),
            &chunked_response_body(),
        ]
        .concat(),
    }
}

/// The log lines the request gives, without their `conn=` field.
fn log_lines(kind: Kind, backend: SocketAddr, down: SocketAddr) -> Vec<String> {
    let peer = "peer=127.0.0.1";
    let response = |status: u16, method: &str, host: &str, path: &str| {
        format!("kind=response {peer} status={status} method={method} host={host} path={path}")
    };
    let proxied = |status: u16, method: &str, path: &str| {
        format!(
            "kind=proxied_response {peer} backend={backend} status={status} method={method} \
             host=proxy.test path={path}"
        )
    };
    match kind {
        Kind::File => vec![response(200, "GET", "static.test", "/page.html")],
        Kind::Index => vec![response(200, "GET", "static.test", "/dir/")],
        Kind::NotFound => vec![response(404, "GET", "static.test", "/missing.txt")],
        Kind::Redirect => vec![response(301, "GET", "static.test", "/dir")],
        Kind::TooLarge => vec![response(413, "POST", "proxy.test", "/api/echo")],
        Kind::ProxyGet => vec![proxied(200, "GET", "/api/get")],
        Kind::ProxyPost => vec![proxied(200, "POST", "/api/echo")],
        Kind::Chunked => vec![proxied(201, "POST", "/api/chunked")],
        Kind::Refused => vec![
            format!("kind=backend_failed {peer} backend={down} failure=refused"),
            response(502, "GET", "proxy.test", "/down"),
        ],
        Kind::Hang => vec![
            format!("kind=backend_failed {peer} backend={backend} failure=response_timeout"),
            response(504, "GET", "proxy.test", "/api/hang"),
        ],
    }
}

/// A line without its ` conn=<digits>` field, which must be there for a
/// core entry (`response`, `proxied_response`, `backend_failed`, ...)
/// and absent for a driver one (`privileges`, `shutdown`).
fn without_conn(line: &str) -> String {
    let fields: Vec<&str> = line.split(' ').collect();
    let conn = fields
        .iter()
        .filter(|f| {
            f.strip_prefix("conn=")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .count();
    let driver = line.starts_with("kind=privileges ") || line.starts_with("kind=shutdown ");
    assert_eq!(conn, usize::from(!driver), "conn field in {line:?}");
    fields
        .into_iter()
        .filter(|f| !f.starts_with("conn="))
        .collect::<Vec<_>>()
        .join(" ")
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.set_write_timeout(Some(PATIENCE)).unwrap();
    stream
}

/// Reads up to and including the blank line that ends a head.
fn read_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("stream ended inside a head: {:?}", text(&head)),
            ));
        }
        head.push(byte[0]);
    }
    Ok(head)
}

/// The value of a `Date` or `date` line of `head`, checked for its
/// 29-byte IMF-fixdate shape.
fn date_of(head: &[u8]) -> Option<String> {
    let date = text(head).split("\r\n").find_map(|line| {
        line.strip_prefix("Date: ")
            .or_else(|| line.strip_prefix("date: "))
            .map(str::to_owned)
    })?;
    assert_eq!(date.len(), 29, "{date:?}");
    assert!(date.ends_with(" GMT"), "{date:?}");
    Some(date)
}

/// The wall-clock second now.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The IMF-fixdate of `secs` after the epoch (RFC 9110 section 5.6.7),
/// written here from the calendar rules (days to civil date), not taken
/// from the core.
fn imf_fixdate(secs: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs / 86_400;
    let time = secs % 86_400;
    // Proleptic Gregorian calendar in 400-year eras from 0000-03-01.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[usize::try_from(days % 7).unwrap()],
        MONTHS[usize::try_from(month - 1).unwrap()],
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

#[test]
fn imf_fixdate_matches_known_dates() {
    assert_eq!(imf_fixdate(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    assert_eq!(imf_fixdate(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    assert_eq!(imf_fixdate(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
    assert_eq!(imf_fixdate(1_798_761_599), "Thu, 31 Dec 2026 23:59:59 GMT");
}

/// Sends the request of `kind` and checks the response byte for byte.
fn exchange(stream: &mut TcpStream, kind: Kind, conn: usize, index: usize) {
    let sent = unix_now();
    stream.write_all(&client_request(kind)).unwrap();
    let head = read_head(stream).unwrap_or_else(|e| panic!("conn {conn} #{index} {kind:?}: {e}"));
    let date = date_of(&head);
    match kind {
        // The backend's `Date`, checked with the bytes.
        Kind::ProxyPost | Kind::Chunked => {}
        // DRV-13: the core's `Date` is the wall-clock second it was
        // rendered in, between sending the request and reading the head.
        Kind::File
        | Kind::Index
        | Kind::NotFound
        | Kind::Redirect
        | Kind::ProxyGet
        | Kind::TooLarge
        | Kind::Refused
        | Kind::Hang => {
            let received = unix_now();
            let date = date.as_deref().unwrap_or_default();
            assert!(
                (sent..=received).any(|secs| imf_fixdate(secs) == date),
                "conn {conn} #{index} {kind:?}: Date {date:?} outside {} .. {}",
                imf_fixdate(sent),
                imf_fixdate(received)
            );
        }
    }
    let expected = client_response(kind, date.as_deref());
    assert!(
        head.len() <= expected.len() && head == expected[..head.len()],
        "conn {conn} #{index} {kind:?}: head\n{}\nexpected\n{}",
        text(&head),
        text(&expected[..expected.len().min(400)])
    );
    let mut rest = vec![0u8; expected.len() - head.len()];
    stream
        .read_exact(&mut rest)
        .unwrap_or_else(|e| panic!("conn {conn} #{index} {kind:?}: body: {e}"));
    assert!(
        rest == expected[head.len()..],
        "conn {conn} #{index} {kind:?}: the body differs"
    );
}

/// One client connection: its requests in order, then a half-close, and
/// the end of stream must come with no byte more.
fn client(addr: SocketAddr, conn: usize, kinds: &[Kind]) {
    let mut stream = connect(addr);
    for (index, &kind) in kinds.iter().enumerate() {
        exchange(&mut stream, kind, conn, index);
    }
    stream.shutdown(Shutdown::Write).unwrap();
    let mut extra = Vec::new();
    stream.read_to_end(&mut extra).unwrap();
    assert_eq!(
        text(&extra),
        "",
        "conn {conn}: bytes after the last response"
    );
}

/// The backend: one thread per connection, each checking the request it
/// gets byte for byte and answering by its path; each sends `Ok(kind)`
/// or what was wrong.
fn backend(listener: TcpListener, results: Sender<Result<Kind, String>>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let results = results.clone();
        thread::spawn(move || {
            let _ = results.send(serve_backend(stream));
        });
    }
}

fn serve_backend(mut stream: TcpStream) -> Result<Kind, String> {
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.set_write_timeout(Some(PATIENCE)).unwrap();
    let head = read_head(&mut stream).map_err(|e| e.to_string())?;
    let line = text(&head);
    let kind = match line.split("\r\n").next() {
        Some("GET /get HTTP/1.1") => Kind::ProxyGet,
        Some("GET /hang HTTP/1.1") => Kind::Hang,
        Some("POST /echo HTTP/1.1") => Kind::ProxyPost,
        Some("POST /chunked HTTP/1.1") => Kind::Chunked,
        _ => return Err(format!("unexpected request {line:?}")),
    };
    let expected = backend_request(kind);
    if !expected.starts_with(&head) {
        return Err(format!(
            "{kind:?}: head {line:?}, expected {:?}",
            text(&expected[..expected.len().min(400)])
        ));
    }
    let mut body = vec![0u8; expected.len() - head.len()];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    if body != expected[head.len()..] {
        return Err(format!("{kind:?}: the request body differs"));
    }
    if kind == Kind::Hang {
        // Never answer: the proxy gives up and closes.
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).map_err(|e| e.to_string())?;
        return if rest.is_empty() {
            Ok(kind)
        } else {
            Err(format!("{kind:?}: {} bytes after the request", rest.len()))
        };
    }
    stream
        .write_all(&backend_response(kind))
        .map_err(|e| e.to_string())?;
    // The proxy closes the upstream once the body is complete.
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).map_err(|e| e.to_string())?;
    Ok(kind)
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

/// The inode of the listening socket on `addr` (`/proc/net/tcp`, state
/// `0A`).
fn listener_inode(addr: SocketAddr) -> String {
    let SocketAddr::V4(v4) = addr else {
        panic!("IPv4 only")
    };
    let local = format!(
        "{:08X}:{:04X}",
        u32::from_le_bytes(v4.ip().octets()),
        v4.port()
    );
    fs::read_to_string("/proc/net/tcp")
        .unwrap()
        .lines()
        .skip(1)
        .find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields[1] == local && fields[3] == "0A").then(|| fields[9].to_owned())
        })
        .expect("the listener in /proc/net/tcp")
}

/// The server's descriptors: how many, and how many of them are sockets
/// other than the listener (client and backend connections).
fn descriptors(pid: u32, listener: &str) -> (usize, usize) {
    let listener = format!("socket:[{listener}]");
    let mut total = 0;
    let mut connections = 0;
    for entry in fs::read_dir(format!("/proc/{pid}/fd")).unwrap() {
        let Ok(entry) = entry else { continue };
        // A descriptor closed while the directory is read is gone.
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        total += 1;
        let target = target.to_string_lossy();
        if target.starts_with("socket:") && target != listener {
            connections += 1;
        }
    }
    (total, connections)
}

/// Polls `cond` every 5 ms until it holds, at most [`PATIENCE`].
fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    cond()
}

/// The whole smoke run (one test: the server's descriptor count and its
/// log are shared by everything it serves).
#[test]
fn smoke_static_and_proxy_in_parallel_then_sigterm() {
    let dir = TempDir::new();
    let www = dir.dir("www");
    write_file(&www.join("page.html"), &page());
    fs::create_dir_all(www.join("dir")).unwrap();
    fs::set_permissions(www.join("dir"), fs::Permissions::from_mode(0o755)).unwrap();
    write_file(&www.join("dir").join("index.html"), INDEX);

    let backend_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    let (results_in, results) = mpsc::channel();
    thread::spawn(move || backend(backend_listener, results_in));
    // A port nothing listens on: the connect is refused (502).
    let down = SocketAddr::from(([127, 0, 0, 1], free_port()));

    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let conf = format!(
        "listen {addr};
limits {{ backend_response_timeout 1s; shutdown_timeout 5s; }}
host static.test {{
  path / {{ static {www}; }}
}}
host proxy.test {{
  path /api {{ proxy {backend_addr}; strip_prefix; max_request_body 1MiB; max_response_body unlimited; }}
  path /down {{ proxy {down}; max_request_body 1MiB; max_response_body unlimited; }}
}}
",
        www = www.display()
    );
    let path = dir.file("smoke.conf", &conf);
    let mut server = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    let lines = server.lines();
    let pid = server.pid();

    // A first response, before anything else (and before any signal).
    let mut warm = connect(addr);
    exchange(&mut warm, Kind::File, 0, 0);
    drop(warm);
    let listener = listener_inode(addr);
    assert!(
        eventually(|| descriptors(pid, &listener).1 == 0),
        "connections left before the load: {:?}",
        descriptors(pid, &listener)
    );
    let baseline = descriptors(pid, &listener).0;

    let plan = plan();
    thread::scope(|scope| {
        for (conn, kinds) in plan.iter().enumerate() {
            scope.spawn(move || client(addr, conn, kinds));
        }
    });

    // Every request that reached the backend was the expected one.
    let to_backend = plan
        .iter()
        .flatten()
        .filter(|k| {
            matches!(
                k,
                Kind::ProxyGet | Kind::ProxyPost | Kind::Chunked | Kind::Hang
            )
        })
        .count();
    let mut seen = Vec::new();
    for _ in 0..to_backend {
        match results.recv_timeout(PATIENCE) {
            Ok(Ok(kind)) => seen.push(kind),
            Ok(Err(error)) => panic!("backend: {error}"),
            Err(error) => panic!("backend: {error} after {} results", seen.len()),
        }
    }

    // Every connection is closed: the count is back to its baseline.
    assert!(
        eventually(|| descriptors(pid, &listener) == (baseline, 0)),
        "descriptors {:?}, {baseline} before the load",
        descriptors(pid, &listener)
    );

    server.signal("TERM");
    let status = server.wait_exit(PATIENCE);
    assert_eq!(status.code(), Some(0), "{status}");
    assert!(results.try_recv().is_err(), "a backend connection too many");

    let mut got: Vec<String> = lines.all().iter().map(|l| without_conn(l)).collect();
    // No `privileges` line: a non-root server without `user` logs none.
    let mut want = vec![DRAINING.to_owned(), DONE.to_owned()];
    want.extend(log_lines(Kind::File, backend_addr, down));
    for &kind in plan.iter().flatten() {
        want.extend(log_lines(kind, backend_addr, down));
    }
    assert_eq!(got.last().map(String::as_str), Some(DONE), "{got:#?}");
    got.sort();
    want.sort();
    assert!(got == want, "{}", difference(&got, &want));
}

/// The lines of two sorted lists that are not in both, for a failure.
fn difference(got: &[String], want: &[String]) -> String {
    let mut missing = want.to_vec();
    let mut extra = Vec::new();
    for line in got {
        match missing.iter().position(|w| w == line) {
            Some(at) => {
                missing.remove(at);
            }
            None => extra.push(line.clone()),
        }
    }
    format!("missing lines: {missing:#?}\nunexpected lines: {extra:#?}")
}
