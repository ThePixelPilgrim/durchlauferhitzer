//! The kernel memory backstop (NFR-RES-09) in the built binary: after
//! startup the soft and hard `RLIMIT_DATA` are both
//! `backstop::limit(&limits)` and the log names it, whatever was
//! inherited above it; an inherited hard limit one byte short is exit 2
//! naming both numbers; and the measured allowances are large enough:
//! 64 disk threads start and serve, 65536 connections start, and the
//! smoke load (6 static GETs of 1 MiB and 6 proxied POSTs of 64 KiB on
//! 16 slots) is served, each under its own backstop, the server alive
//! afterwards (an allocation the kernel refuses would abort it); and the
//! worst request on every slot, static and proxied, is survived.
//!
//! Every server runs as non-root (as `nobody` when the tests run as
//! root), except the 65536-connection one, which needs about 262 000
//! descriptors and so root with `CAP_SYS_RESOURCE` to raise
//! `RLIMIT_NOFILE`: as root it starts as root with `user nobody` and,
//! when the inherited hard `RLIMIT_NOFILE` is lower, a raised one;
//! otherwise it skips unless the inherited hard
//! limit is high enough (CI's root step and its container jobs, which get
//! a raised `nofile` ulimit, run it, and fail on a skip with
//! `DURCHLAUFERHITZER_REQUIRE_ROOT=1`). The configs carry their own
//! `containment` block (`best_effort` for both layers), so the tests run
//! on any kernel.

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
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use child::{Containment, PATIENCE, Running, Setup, TempDir, assert_exit, free_port, run, text};
use durchlauferhitzer::process::backstop::{
    self, BASE_ALLOWANCE, DISK_THREAD_OVERHEAD, DISK_THREAD_STACK, PER_CONNECTION_FIXED,
    REQUEST_STATE_FACTOR,
};
use durchlauferhitzer_core::config::{Limits, load, worst_case_per_connection};

const MIB: u64 = 1024 * 1024;

/// A static root with `big.bin` (1 MiB) and a proxy route to a backend
/// of this test, on a free loopback port.
struct Site {
    dir: TempDir,
    addr: SocketAddr,
    root: String,
    backend: SocketAddr,
}

impl Site {
    fn new() -> Site {
        let dir = TempDir::new();
        let root = dir.dir("www").to_str().unwrap().to_owned();
        dir.file("www/big.bin", &"y".repeat(1 << 20));
        Site {
            dir,
            addr: SocketAddr::from(([127, 0, 0, 1], free_port())),
            root,
            backend: backend(),
        }
    }

    /// The `both` config: `/` static, `/api` proxied; `limits` and
    /// `extra` top-level lines as given.
    fn text(&self, limits: &str, extra: &str) -> String {
        format!(
            "listen {};\n{extra}\nlimits {{ {limits} }}\nhost a {{\n  path / {{ static {}; }}\n  \
             path /api {{ proxy {}; max_request_body 1MiB; max_response_body unlimited; }}\n}}\n\
             containment {{ landlock best_effort; seccomp best_effort; }}\n",
            self.addr, self.root, self.backend
        )
    }

    /// Writes `text` as the config and returns its path and limits.
    fn config(&self, text: &str) -> (String, Limits) {
        let limits = load(text).unwrap().limits;
        let path = self
            .dir
            .config("backstop.conf", text, Containment::AsWritten);
        (path.to_str().unwrap().to_owned(), limits)
    }
}

/// `max_connections 16; memory_limit 2MiB; disk_threads <threads>`.
fn small(threads: u32) -> String {
    format!("max_connections 16; memory_limit 2MiB; disk_threads {threads};")
}

/// A backend answering every request (with `Content-Length`, keep-alive)
/// `200` with the body `ok`.
fn backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            thread::spawn(move || answer_requests(stream));
        }
    });
    addr
}

fn answer_requests(mut stream: TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut fill = |stream: &mut TcpStream, buf: &mut Vec<u8>| match stream.read(&mut chunk) {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
    };
    loop {
        let head_end = loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            if !fill(&mut stream, &mut buf) {
                return;
            }
        };
        let head = text(&buf[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse().unwrap());
        while buf.len() < head_end + length {
            if !fill(&mut stream, &mut buf) {
                return;
            }
        }
        buf.drain(..head_end + length);
        if stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .is_err()
        {
            return;
        }
    }
}

/// Sends `request` (and `body`) on a fresh connection and returns the
/// whole response (the request asks for `Connection: close`).
fn exchange(addr: SocketAddr, request: &str, body: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

/// A static `GET` of `big.bin`: the whole MiB comes back.
fn get_big(addr: SocketAddr) {
    let response = exchange(
        addr,
        "GET /big.bin HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n",
        &[],
    );
    let head = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(
        response.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        text(&response[..head])
    );
    assert_eq!(
        response.len() - head,
        1 << 20,
        "{}",
        text(&response[..head])
    );
}

/// A proxied `POST` of 64 KiB: the backend's `ok` comes back.
fn post(addr: SocketAddr) {
    let body = vec![b'x'; 64 * 1024];
    let request = format!(
        "POST /api/x HTTP/1.1\r\nHost: a\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let response = exchange(addr, &request, &body);
    assert!(
        response.starts_with(b"HTTP/1.1 200 OK\r\n") && response.ends_with(b"\r\n\r\nok"),
        "{}",
        text(&response)
    );
}

/// The server's soft and hard `Max data size`.
fn data_limits(pid: u32) -> (String, String) {
    let limits = fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max data size"))
        .unwrap();
    let numbers: Vec<&str> = line.split_whitespace().skip(3).take(2).collect();
    (numbers[0].to_owned(), numbers[1].to_owned())
}

/// Fails the test if the server has exited.
fn assert_alive(server: &mut Running) {
    let status = server.child.try_wait().unwrap();
    assert!(status.is_none(), "the server exited: {status:?}");
}

#[test]
fn startup_sets_rlimit_data_to_the_backstop() {
    let site = Site::new();
    let (path, limits) = site.config(&site.text(&small(4), ""));
    let expected = backstop::limit(&limits);
    let stack = u64::try_from(DISK_THREAD_STACK).unwrap();
    assert_eq!(
        expected,
        2 * MIB
            + BASE_ALLOWANCE
            + 4 * (stack + DISK_THREAD_OVERHEAD)
            + 16 * (PER_CONNECTION_FIXED + REQUEST_STATE_FACTOR * 16 * 1024)
    );
    let inherited = [
        None,
        Some((expected + MIB, expected + MIB)),
        Some((expected, expected)),
    ];
    for data in inherited {
        let setup = Setup {
            data,
            ..Setup::non_root()
        };
        let mut server = Running::start(&[&path], setup, site.addr);
        let mut lines = server.lines();
        lines.wait_for(&format!("kind=memory_backstop bytes={expected}"));
        assert_eq!(
            data_limits(server.pid()),
            (expected.to_string(), expected.to_string()),
            "{data:?}"
        );
    }
}

#[test]
fn an_inherited_hard_limit_below_the_backstop_is_exit_2() {
    let site = Site::new();
    let (path, limits) = site.config(&site.text(&small(4), ""));
    let needed = backstop::limit(&limits);
    let short = needed - 1;
    let output = run(
        &[&path],
        Setup {
            data: Some((short, short)),
            ..Setup::non_root()
        },
    );
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        format!(
            "durchlauferhitzer: memory backstop: the server needs {needed} bytes of \
             RLIMIT_DATA, {short} are available\n"
        )
    );
}

#[test]
fn starts_and_serves_with_64_disk_threads_under_the_backstop() {
    let site = Site::new();
    let (path, _) = site.config(&site.text(&small(64), ""));
    let mut server = Running::start(&[&path], Setup::non_root(), site.addr);
    child::threads(server.pid(), 65);
    get_big(site.addr);
    post(site.addr);
    assert_alive(&mut server);
}

/// The descriptors the 65536-connection server needs (NFR-RES-05: both
/// route kinds, about 262 000), with room for whatever is inherited.
const NOFILE_65536: u64 = 300_000;

/// Whether this process has `CAP_SYS_RESOURCE` (bit 24) in its
/// effective set: only then may a child raise a hard limit.
fn may_raise_limits() -> bool {
    const CAP_SYS_RESOURCE: u32 = 24;
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(|caps| u64::from_str_radix(caps.trim(), 16).unwrap())
        .unwrap();
    effective & (1 << CAP_SYS_RESOURCE) != 0
}

#[test]
fn starts_with_65536_connections_under_the_backstop() {
    const REQUIRE_ROOT: &str = "DURCHLAUFERHITZER_REQUIRE_ROOT";
    let site = Site::new();
    let probe = site.text(
        "max_connections 65536; memory_limit 1024GiB; disk_threads 4;",
        "",
    );
    let config = load(&probe).unwrap();
    let smallest = 65536 * worst_case_per_connection(&config.limits, config.route_mix());
    let limits = format!("max_connections 65536; memory_limit {smallest}; disk_threads 4;");
    let (_, hard) = durchlauferhitzer::sys::rlimit::nofile().unwrap();
    let setup = if child::is_root() && hard < NOFILE_65536 && may_raise_limits() {
        // Raised before `exec` as root; the server drops to nobody itself.
        Setup {
            nofile: Some((NOFILE_65536, NOFILE_65536)),
            ..Setup::default()
        }
    } else if hard >= NOFILE_65536 {
        Setup::default()
    } else {
        assert!(
            std::env::var_os(REQUIRE_ROOT).is_none_or(|v| v != "1"),
            "a hard RLIMIT_NOFILE of {hard}, but {REQUIRE_ROOT}=1 requires the test"
        );
        eprintln!(
            "skipped: needs a hard RLIMIT_NOFILE of {NOFILE_65536} (it is {hard}), or root \
             with CAP_SYS_RESOURCE to raise it"
        );
        return;
    };
    // As root the server drops to nobody itself; as non-root it stays.
    let user = if child::is_root() { "user nobody;" } else { "" };
    let (path, limits) = site.config(&site.text(&limits, user));
    assert_eq!(limits.memory_limit, smallest);
    let mut server = Running::start(&[&path], setup, site.addr);
    let mut lines = server.lines();
    let expected = backstop::limit(&limits);
    lines.wait_for(&format!("kind=memory_backstop bytes={expected}"));
    get_big(site.addr);
    assert_alive(&mut server);
}

/// 6 static GETs of 1 MiB and 6 proxied POSTs of 64 KiB at once, on the
/// 16 slots of the smallest budget. The 4 spare slots are slack: a slot
/// can still be held by the readiness probe's connection or by a client
/// that finished early while its close lingers (FR-CONN-05, up to 2 s),
/// and a late client meeting 16 held slots would get a 503.
#[test]
fn serves_the_smoke_load_under_the_smallest_budget() {
    let site = Site::new();
    let (path, _) = site.config(&site.text(&small(4), ""));
    let mut server = Running::start(&[&path], Setup::non_root(), site.addr);
    let addr = site.addr;
    let clients: Vec<_> = (0..6)
        .flat_map(|_| {
            [
                thread::spawn(move || get_big(addr)),
                thread::spawn(move || post(addr)),
            ]
        })
        .collect();
    for client in clients {
        client.join().unwrap();
    }
    assert_alive(&mut server);
}

/// The worst-case requests on every slot (the stage 9 review's C1): this
/// many connections, all from 127.0.0.1, each holding the largest
/// per-request state the core keeps outside `memory_limit`, at the
/// smallest `memory_limit` FR-CFG-21 accepts for the config.
const SLOTS: usize = 60;

/// `limits` for [`SLOTS`] connections from one address, `extra` added
/// (`max_request_line` and `max_head`), with the smallest `memory_limit`
/// FR-CFG-21 accepts for `text_of`'s routes.
fn every_slot_limits(text_of: impl Fn(&str) -> String, extra: &str) -> String {
    let counts = format!("max_connections {SLOTS}; max_connections_per_ip {SLOTS}; {extra}");
    let probe = load(&text_of(&format!("{counts} memory_limit 1024GiB;"))).unwrap();
    let smallest =
        (SLOTS as u64 * worst_case_per_connection(&probe.limits, probe.route_mix())).max(MIB);
    format!("{counts} memory_limit {smallest}; disk_threads 4;")
}

/// Opens one connection per request, writes each but its last byte, then
/// (after a pause, so the heads complete in as few loop iterations as
/// possible) every last byte, and returns the streams.
fn send_together(addr: SocketAddr, requests: &[Vec<u8>]) -> Vec<TcpStream> {
    let mut streams: Vec<TcpStream> = requests
        .iter()
        .map(|_| {
            let stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(PATIENCE)).unwrap();
            stream
        })
        .collect();
    for (stream, request) in streams.iter_mut().zip(requests) {
        stream.write_all(&request[..request.len() - 1]).unwrap();
    }
    thread::sleep(std::time::Duration::from_millis(300));
    for (stream, request) in streams.iter_mut().zip(requests) {
        // A server that died meanwhile shows in the responses below.
        let _ = stream.write_all(&request[request.len() - 1..]);
    }
    streams
}

/// What `stream` receives until the end of stream, an error or the
/// timeout (a dead server's reset reads as what came before it).
fn read_all(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n @ 1..) = stream.read(&mut chunk) {
        response.extend_from_slice(&chunk[..n]);
    }
    response
}

/// A request line of `/a` repeated to just under `max_request_line`
/// (`line` bytes, without CRLF) for `method`: the most segments a path
/// of that line can have.
fn many_segments(method: &str, line: usize) -> String {
    let fixed = method.len() + " ".len() + " HTTP/1.1".len();
    "/a".repeat((line - fixed) / 2)
}

#[test]
fn survives_worst_case_static_requests_on_every_slot() {
    // The default 8 KiB line (4089 one-byte segments: the review's
    // case), and a 256 KiB one, whose request state the per-connection
    // term must carry.
    for line in ["", "max_request_line 256KiB; max_head 260KiB;"] {
        static_requests_on_every_slot(line);
    }
}

/// Every slot of a static route holds a `GET` of the most segments its
/// line allows, released together; the open of a path no root holds is
/// a 404 for each, and the server serves on.
fn static_requests_on_every_slot(line: &str) {
    let dir = TempDir::new();
    let root = dir.dir("www").to_str().unwrap().to_owned();
    dir.file("www/ok.txt", "ok");
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let text_of = |limits: &str| {
        format!(
            "listen {addr};\nlimits {{ {limits} }}\nhost a {{ path / {{ static {root}; }} }}\n\
             containment {{ landlock best_effort; seccomp best_effort; }}\n"
        )
    };
    let config = text_of(&every_slot_limits(text_of, line));
    let path = dir.config("slots.conf", &config, Containment::AsWritten);
    let limits = load(&config).unwrap().limits;
    let mut server = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    let target = many_segments("GET", limits.max_request_line);
    let request = format!("GET {target} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    let requests = vec![request.into_bytes(); SLOTS];
    for (i, mut stream) in send_together(addr, &requests).into_iter().enumerate() {
        let response = read_all(&mut stream);
        assert!(
            response.starts_with(b"HTTP/1.1 404 Not Found\r\n"),
            "{line} client {i}: {:?}; the server: {:?}",
            text(&response),
            server.child.try_wait().unwrap()
        );
    }
    assert_alive(&mut server);
    let response = exchange(
        addr,
        "GET /ok.txt HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n",
        &[],
    );
    assert!(
        response.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        text(&response)
    );
    assert!(response.ends_with(b"\r\n\r\nok"), "{}", text(&response));
}

/// A backend that reads each request (head and `Content-Length` body),
/// counts it, and answers `200` `ok` only once `release` is set.
fn stalling_backend(arrived: Arc<AtomicUsize>, release: Arc<(Mutex<bool>, Condvar)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let arrived = Arc::clone(&arrived);
            let release = Arc::clone(&release);
            thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let complete = |buf: &[u8]| {
                    let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        return false;
                    };
                    let head = text(&buf[..at]).to_ascii_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map_or(0, |value| value.trim().parse().unwrap());
                    buf.len() >= at + 4 + length
                };
                while !complete(&buf) {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                arrived.fetch_add(1, Ordering::SeqCst);
                let (lock, cvar) = &*release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
                drop(released);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });
    addr
}

#[test]
fn survives_worst_case_proxy_requests_on_every_slot() {
    let arrived = Arc::new(AtomicUsize::new(0));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let backend = stalling_backend(Arc::clone(&arrived), Arc::clone(&release));
    let dir = TempDir::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    // A long host name (it is kept for the log with the raw path).
    let host = format!("{}.example", "h".repeat(240));
    let text_of = |limits: &str| {
        format!(
            "listen {addr};\nlimits {{ {limits} }}\nhost {host} {{ path / {{ proxy {backend}; max_request_body 1KiB; max_response_body 1KiB; }} }}\n\
             containment {{ landlock best_effort; seccomp best_effort; }}\n"
        )
    };
    // A long request line: the raw path each exchange keeps for its log
    // is as long as the line allows.
    let line = "max_request_line 256KiB; max_head 260KiB;";
    let config = text_of(&every_slot_limits(text_of, line));
    let path = dir.config("slots.conf", &config, Containment::AsWritten);
    let limits = load(&config).unwrap().limits;
    let mut server = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    let target = many_segments("POST", limits.max_request_line);
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
    );
    let requests = vec![request.into_bytes(); SLOTS];
    let mut streams = send_together(addr, &requests);
    // Every exchange waits on the backend at once.
    let deadline = std::time::Instant::now() + PATIENCE;
    while arrived.load(Ordering::SeqCst) < SLOTS && std::time::Instant::now() < deadline {
        assert_alive(&mut server);
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_alive(&mut server);
    assert_eq!(arrived.load(Ordering::SeqCst), SLOTS);
    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    for (i, stream) in streams.iter_mut().enumerate() {
        let response = read_all(stream);
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n") && response.ends_with(b"\r\n\r\nok"),
            "client {i}: {:?}",
            text(&response)
        );
    }
    assert_alive(&mut server);
    // Closed, the clients leave their lingering slots; then a plain
    // request is served again.
    drop(streams);
    let deadline = std::time::Instant::now() + PATIENCE;
    let request = format!(
        "POST /x HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
    );
    let mut response = exchange(addr, &request, b"ok");
    while response.starts_with(b"HTTP/1.1 503 ") && std::time::Instant::now() < deadline {
        thread::sleep(std::time::Duration::from_millis(10));
        response = exchange(addr, &request, b"ok");
    }
    assert!(
        response.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        text(&response)
    );
}
