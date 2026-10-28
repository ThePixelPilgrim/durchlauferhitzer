//! Signals and shutdown (FR-PROC-05, A9, DRV-14; Review Focus 4) in the
//! built binary, run as non-root (as `nobody` when the tests run as
//! root): the first SIGTERM closes the listeners and drains (in-flight
//! downloads and uploads finish, a request sent after it gets 503 with
//! `Connection: close`), then the server exits 0; a second SIGTERM, a
//! SIGINT or `shutdown_timeout` cut every connection and exit 0 at once;
//! SIGHUP and SIGPIPE are ignored.
//!
//! Every test first gets a response, so startup has passed the signal
//! mask (A11 step 8) before a signal is sent; each waits for the server's
//! `kind=shutdown phase=draining` line, written once the listeners are
//! closed, so what follows the SIGTERM is deterministic. Exit bounds have
//! 500 ms of margin for CI over what the server promises.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use child::{PATIENCE, Running, Setup, TempDir, config, free_port, get, text};

/// The size of the slow download: far more than loopback's socket
/// buffers hold, so the response is still in flight at the signal.
const BIG: usize = 50 * 1024 * 1024;

const DRAINING: &str = "kind=shutdown phase=draining";
const ABORTING: &str = "kind=shutdown phase=aborting";
const DONE: &str = "kind=shutdown phase=done detached=0";

/// The longest a fast shutdown may take here: a healthy teardown is
/// instant; this is the pool's 1 s bound plus 500 ms of margin for CI
/// (the worst case, a stuck worker and a stalled stderr, is 2 s, which
/// these tests do not create).
const FAST_EXIT: Duration = Duration::from_millis(1500);

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Deterministic contents for the big file.
fn big_contents() -> Vec<u8> {
    (0..BIG)
        .map(|i| u8::try_from((i * 31 + i / 4093) % 251).unwrap())
        .collect()
}

/// Writes `bytes` to `path`, readable by everyone.
fn write_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

/// A static server: `www/big.bin` ([`big_contents`]) and `www/small.txt`,
/// with `limits` as the inside of the `limits` block.
struct StaticServer {
    _dir: TempDir,
    addr: SocketAddr,
    server: Running,
    big: Vec<u8>,
}

fn static_server(limits: &str) -> StaticServer {
    let dir = TempDir::new();
    let www = dir.dir("www");
    let big = big_contents();
    write_file(&www.join("big.bin"), &big);
    write_file(&www.join("small.txt"), b"small\n");
    let addr = loopback(free_port());
    let conf = config(
        &addr.to_string(),
        &format!("limits {{ {limits} }}"),
        &format!("static {};", www.display()),
    );
    let path = dir.file("static.conf", &conf);
    let server = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    StaticServer {
        _dir: dir,
        addr,
        server,
        big,
    }
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.set_write_timeout(Some(PATIENCE)).unwrap();
    stream
}

/// Reads up to and including the blank line that ends a head.
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).unwrap();
        assert_eq!(n, 1, "stream ended inside a head: {:?}", text(&head));
        head.push(byte[0]);
    }
    text(&head)
}

const DOWNLOAD: &[u8] = b"GET /big.bin HTTP/1.1\r\nHost: example.test\r\n\r\n";

/// Starts a keep-alive download of `/big.bin` and reads its head, so the
/// response is in flight (and startup is over).
fn start_download(addr: SocketAddr) -> TcpStream {
    start_download_with(addr, b"")
}

/// Like [`start_download`], with `more` sent in the same write right
/// behind the request: the server reads both at once, so once the head is
/// back `more` is in the core's buffer.
fn start_download_with(addr: SocketAddr, more: &[u8]) -> TcpStream {
    let mut stream = connect(addr);
    stream.write_all(&[DOWNLOAD, more].concat()).unwrap();
    let head = read_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(
        head.contains(&format!("Content-Length: {BIG}\r\n")),
        "{head}"
    );
    assert!(!head.contains("Connection: close"), "{head}");
    stream
}

/// Reads `len` body bytes.
fn read_body(stream: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    body
}

/// Reads until the end of stream, or a reset, and returns the bytes.
fn read_rest(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == io::ErrorKind::ConnectionReset => return out,
            Err(err) => panic!("{err}"),
        }
    }
}

fn assert_refused(addr: SocketAddr) {
    match TcpStream::connect(addr) {
        Ok(_) => panic!("a connect after SIGTERM was accepted"),
        Err(err) => assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}"),
    }
}

/// Review Focus 4: SIGTERM during three slow 50 MiB downloads. The
/// listener is closed (a new connect is refused); an idle keep-alive
/// connection is closed at once (A4); all downloads finish byte-exact;
/// behind a response whose head said keep-alive (FR-PROC-05b), a
/// pipelined request gets 503 with `Connection: close` after the
/// download: on the second connection a head whose first bytes were
/// buffered before the SIGTERM and whose rest is sent after it (brief
/// Step 1, line 2, in the sense of "buffered" in FR-PROC-05b), on the
/// third a complete request buffered before the SIGTERM (D1); then the
/// server exits 0.
///
/// The pipelined bytes go in the download's own write, so they are in
/// the core's buffer once the download's head is back: the core reads a
/// pipelined request only from what it holds when the response ends (it
/// does not read while it sends), and a partly received head is
/// completed, then refused (FR-PROC-05b).
#[test]
fn sigterm_lets_slow_downloads_finish_and_refuses_new_work() {
    let StaticServer {
        _dir,
        addr,
        mut server,
        big,
    } = static_server("shutdown_timeout 20s; send_idle_timeout 20s; header_timeout 20s;");
    let mut lines = server.lines();
    let mut idle = connect(addr);
    let small = b"GET /small.txt HTTP/1.1\r\nHost: example.test\r\n\r\n";
    idle.write_all(small).unwrap();
    let response = read_head(&mut idle);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(read_body(&mut idle, 6), b"small\n");
    let mut first = start_download(addr);
    let (pipelined_start, pipelined_rest) = small.split_at(20);
    let mut second = start_download_with(addr, pipelined_start);
    let mut third = start_download_with(addr, small);
    server.signal("TERM");
    lines.wait_for(DRAINING);
    assert_refused(addr);
    assert_eq!(read_rest(&mut idle), b"", "the idle connection got bytes");
    second.write_all(pipelined_rest).unwrap();
    assert!(read_body(&mut first, BIG) == big, "first download differs");
    assert_eq!(read_rest(&mut first), b"");
    // The complete request (D1) first, so a mutation of that path fails
    // here and not at the partial head below.
    assert!(read_body(&mut third, BIG) == big, "third download differs");
    let complete = text(&read_rest(&mut third));
    assert!(
        complete.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{complete}"
    );
    assert!(complete.contains("\r\nConnection: close\r\n"), "{complete}");
    assert!(
        read_body(&mut second, BIG) == big,
        "second download differs"
    );
    let partial = text(&read_rest(&mut second));
    assert!(
        partial.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{partial}"
    );
    assert!(partial.contains("\r\nConnection: close\r\n"), "{partial}");
    drop((idle, first, second, third));
    let status = server.wait_exit(PATIENCE);
    assert_eq!(status.code(), Some(0), "{status}");
    let lines = lines.all();
    assert!(lines.iter().any(|l| l == DONE), "{lines:?}");
    assert!(!lines.iter().any(|l| l == ABORTING), "{lines:?}");
    let refusals = lines
        .iter()
        .filter(|l| {
            l.starts_with("kind=response ")
                && l.contains(" status=503 method=GET host=example.test path=/small.txt")
        })
        .count();
    assert_eq!(refusals, 2, "{lines:?}");
}

/// A backend that reads one request with a `Content-Length` body, says
/// `head` once its head is in, checks the body, answers `200 ok`.
fn upload_backend(len: usize) -> (SocketAddr, mpsc::Receiver<()>, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (head_in, head_seen) = mpsc::channel();
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        let head = read_head(&mut stream);
        assert!(head.starts_with("POST /up HTTP/1.1\r\n"), "{head}");
        assert!(
            head.contains(&format!("content-length: {len}\r\n")),
            "{head}"
        );
        head_in.send(()).unwrap();
        let body = read_body(&mut stream, len);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
        body
    });
    (addr, head_seen, thread)
}

/// Review Focus 4: SIGTERM during a proxied upload. The rest of the body
/// is sent after the drain began; the upload completes, and the
/// backend's response is relayed with `Connection: close` (a head
/// rendered during the drain, A4); then the server exits 0.
#[test]
fn sigterm_lets_a_proxied_upload_finish() {
    const LEN: usize = 4 * 1024 * 1024;
    let (backend, head_seen, backend_thread) = upload_backend(LEN);
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let conf = config(
        &addr.to_string(),
        "limits { shutdown_timeout 20s; }",
        &format!("proxy {backend}; max_request_body 8MiB; max_response_body 1MiB;"),
    );
    let path = dir.file("proxy.conf", &conf);
    let mut server = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    let mut lines = server.lines();
    let body: Vec<u8> = (0..LEN).map(|i| u8::try_from(i % 253).unwrap()).collect();
    let mut client = connect(addr);
    client
        .write_all(
            format!("POST /up HTTP/1.1\r\nHost: example.test\r\nContent-Length: {LEN}\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    client.write_all(&body[..64 * 1024]).unwrap();
    // The backend has the head: the exchange is in flight, and startup
    // is long over.
    head_seen.recv_timeout(PATIENCE).unwrap();
    server.signal("TERM");
    lines.wait_for(DRAINING);
    assert_refused(addr);
    client.write_all(&body[64 * 1024..]).unwrap();
    let response = text(&read_rest(&mut client));
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    // The proxy writes its own header names in lower case.
    assert!(response.contains("\r\nconnection: close\r\n"), "{response}");
    assert!(response.ends_with("\r\n\r\nok"), "{response}");
    assert!(backend_thread.join().unwrap() == body, "the upload differs");
    drop(client);
    let status = server.wait_exit(PATIENCE);
    assert_eq!(status.code(), Some(0), "{status}");
    let lines = lines.all();
    assert!(lines.iter().any(|l| l == DONE), "{lines:?}");
    assert!(!lines.iter().any(|l| l == ABORTING), "{lines:?}");
}

/// The lines of a fast shutdown: the cut download is logged
/// `rejected ... reason=shutting_down` (FR-PROC-05c) after `aborting`,
/// and `done` comes last.
fn assert_aborted(lines: &[String]) {
    let at = |want: &dyn Fn(&str) -> bool| {
        lines
            .iter()
            .position(|l| want(l))
            .unwrap_or_else(|| panic!("{lines:?}"))
    };
    let aborting = at(&|l| l == ABORTING);
    let rejected = at(&|l| {
        l.starts_with("kind=rejected ") && l.ends_with(" peer=127.0.0.1 reason=shutting_down")
    });
    let done = at(&|l| l == DONE);
    assert!(aborting < rejected && rejected < done, "{lines:?}");
    assert_eq!(done, lines.len() - 1, "{lines:?}");
}

/// A second SIGTERM during the drain cuts the download at once and the
/// server exits 0 within 1 s (DRV-14).
#[test]
fn a_second_sigterm_cuts_the_download_and_exits_at_once() {
    let StaticServer {
        _dir,
        addr,
        mut server,
        big: _,
    } = static_server("shutdown_timeout 20s; send_idle_timeout 20s;");
    let mut lines = server.lines();
    let mut download = start_download(addr);
    server.signal("TERM");
    lines.wait_for(DRAINING);
    server.signal("TERM");
    let sent = Instant::now();
    let status = server.wait_exit(FAST_EXIT);
    assert_eq!(status.code(), Some(0), "{status}");
    let elapsed = sent.elapsed();
    assert!(elapsed < FAST_EXIT, "{elapsed:?}");
    let got = read_rest(&mut download).len();
    assert!(got < BIG, "the download was not cut: {got} bytes");
    assert_aborted(&lines.all());
}

/// `shutdown_timeout 500ms` with a client that stopped reading: the
/// drain is cut when it expires, about 500 ms after the SIGTERM, and the
/// server exits 0.
#[test]
fn shutdown_timeout_cuts_a_stalled_client() {
    let StaticServer {
        _dir,
        addr,
        mut server,
        big: _,
    } = static_server("shutdown_timeout 500ms; send_idle_timeout 20s;");
    let mut lines = server.lines();
    let mut download = start_download(addr);
    let sent = Instant::now();
    server.signal("TERM");
    let status = server.wait_exit(PATIENCE);
    let elapsed = sent.elapsed();
    assert_eq!(status.code(), Some(0), "{status}");
    assert!(
        elapsed >= Duration::from_millis(500) && elapsed < Duration::from_millis(500) + FAST_EXIT,
        "{elapsed:?}"
    );
    let got = read_rest(&mut download).len();
    assert!(got < BIG, "the download was not cut: {got} bytes");
    let all = {
        lines.wait_for(DRAINING);
        lines.all()
    };
    assert_aborted(&all);
    let draining = all.iter().position(|l| l == DRAINING).unwrap();
    let aborting = all.iter().position(|l| l == ABORTING).unwrap();
    assert!(draining < aborting, "{all:?}");
}

/// SIGINT is the fast path at once, without a drain.
#[test]
fn sigint_exits_at_once() {
    let StaticServer {
        _dir,
        addr,
        mut server,
        big: _,
    } = static_server("shutdown_timeout 20s; send_idle_timeout 20s;");
    let lines = server.lines();
    let mut download = start_download(addr);
    let sent = Instant::now();
    server.signal("INT");
    let status = server.wait_exit(FAST_EXIT);
    let elapsed = sent.elapsed();
    assert_eq!(status.code(), Some(0), "{status}");
    assert!(elapsed < FAST_EXIT, "{elapsed:?}");
    let got = read_rest(&mut download).len();
    assert!(got < BIG, "the download was not cut: {got} bytes");
    let all = lines.all();
    assert!(!all.iter().any(|l| l == DRAINING), "{all:?}");
    assert_aborted(&all);
}

/// SIGHUP (stage 4: ignored, A9) and SIGPIPE are read and ignored: the
/// server keeps serving. An idle server then drains at once on SIGTERM.
#[test]
fn sighup_and_sigpipe_are_ignored_and_an_idle_server_exits_on_sigterm() {
    let StaticServer {
        _dir,
        addr,
        mut server,
        big: _,
    } = static_server("shutdown_timeout 20s;");
    let lines = server.lines();
    let response = get(addr, "/small.txt");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    server.signal("HUP");
    server.signal("PIPE");
    let response = get(addr, "/small.txt");
    assert!(response.ends_with("\r\n\r\nsmall\n"), "{response}");
    let sent = Instant::now();
    server.signal("TERM");
    let status = server.wait_exit(FAST_EXIT);
    assert_eq!(status.code(), Some(0), "{status}");
    assert!(sent.elapsed() < FAST_EXIT, "{:?}", sent.elapsed());
    let all = lines.all();
    let tail: Vec<&str> = all.iter().rev().take(2).rev().map(String::as_str).collect();
    assert_eq!(tail, [DRAINING, DONE], "{all:?}");
}
