//! The loop on loopback, from `tests.rs`: the socket options every
//! client and upstream gets, the accept batch, a response's segments
//! (`TCP_INFO` needs `unsafe`, which only `sys` holds) and heads that
//! must not wait for `MSG_MORE`'s cork.

use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;

use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::{BackendId, ConnId, Core, UpstreamId};

use super::Driver;
use super::tests::peer;
use crate::conns::{Answer, Phase};
use crate::log::LogSink;

/// NFR-PERF-04, DRV-11: every client socket the loop accepts and every
/// upstream socket it connects gets `TCP_NODELAY` (off by default), and
/// keeps the kernel's `SO_SNDBUF`/`SO_RCVBUF` (owner decision 2026-10-29,
/// O1: no buffer size is ever set).
#[test]
fn clients_and_upstreams_get_tcp_nodelay() {
    use std::net::{TcpListener, TcpStream};

    use crate::sys::socket::buffer_sizes;

    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = format!(
        "listen 127.0.0.1:8080; host a.test {{ path / {{ proxy {}; \
         max_request_body 1MiB; max_response_body 1MiB; }} }}",
        backend.local_addr().unwrap()
    );
    let core = Core::new(load(&config).unwrap());
    let (log, _logs) = LogSink::channel();
    let mut driver = Driver::new(core, Vec::new(), Vec::new(), 1, log).unwrap();
    let nodelay = |fd: &OwnedFd| TcpStream::from(fd.try_clone().unwrap()).nodelay().unwrap();
    let sizes = |fd: &OwnedFd| buffer_sizes(fd.as_fd()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    assert!(!accepted.nodelay().unwrap(), "the kernel's default");
    // The kernel's defaults for an established socket (it grows the send
    // buffer once a connection is set up, so an unconnected socket's
    // sizes differ): the same socket before the driver has it.
    let accepted = OwnedFd::from(accepted);
    let kernel = sizes(&accepted);
    driver.accepted(accepted, peer()).unwrap();
    let client = &driver.clients[&ConnId(1)].fd;
    assert!(nodelay(client));
    // NFR-PERF-04: no buffer size is set, so autotuning stays on.
    assert_eq!(sizes(client), kernel, "an accepted socket's buffer sizes");

    let up = UpstreamId(1);
    driver.connect(up, BackendId(0)).unwrap();
    let fd = match &driver.upstreams[&up].phase {
        Phase::Connecting(fd) | Phase::Answering(Answer::Connected(fd)) => fd,
        Phase::Answering(Answer::Failed) | Phase::Open { .. } | Phase::Failed => {
            panic!("the connect to a listening backend did not start")
        }
    };
    assert!(nodelay(fd));
    // A loopback connect is set up at once; compare with a plain socket
    // connected to the same backend, once both are connected.
    let upstream = TcpStream::from(fd.try_clone().unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while upstream.peer_addr().is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the upstream never connected"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let plain = OwnedFd::from(TcpStream::connect(backend.local_addr().unwrap()).unwrap());
    assert_eq!(
        sizes(fd),
        sizes(&plain),
        "an upstream socket's buffer sizes"
    );
}

/// A3 (review A, Minor 3): one readiness of a listener accepts at most
/// [`ACCEPT_BATCH`] (64) connections, so a flood of connects cannot hold
/// the loop from its other descriptors and timers; the rest wait in the
/// backlog, and the level-triggered listener is reported again.
///
/// [`ACCEPT_BATCH`]: super::clients::ACCEPT_BATCH
#[test]
fn one_readiness_accepts_at_most_64_connections() {
    use crate::sys::socket::{Probe, local_addr, tcp_listen};
    let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 1024, Probe::First).unwrap();
    let addr = local_addr(listener.as_fd()).unwrap();
    let config = "listen 127.0.0.1:8080; limits { max_connections 1000; \
        max_connections_per_ip 1000; } host a.test { path / { proxy 127.0.0.1:9; \
        max_request_body 1MiB; max_response_body 1MiB; } }";
    let core = Core::new(load(config).unwrap());
    let (log, _logs) = LogSink::channel();
    let mut driver = Driver::new(core, vec![listener], Vec::new(), 4, log).unwrap();
    // `connect` returns once the handshake is done: all 100 are queued.
    let _streams: Vec<std::net::TcpStream> = (0..100)
        .map(|_| std::net::TcpStream::connect(addr).unwrap())
        .collect();
    assert_eq!(super::clients::ACCEPT_BATCH, 64);
    driver.accept_all(0).unwrap();
    assert_eq!(driver.clients.len(), 64);
    driver.accept_all(0).unwrap();
    assert_eq!(driver.clients.len(), 100);
}

/// NFR-PERF-06: the head of a 200 whose file body follows goes with
/// `MSG_MORE`, so a 1 KiB file's head and body leave in one segment (on
/// loopback the MSS is far above the response), on a fresh connection and
/// on a kept-alive one. The client counts the data segments it received
/// (`TCP_INFO`'s `tcpi_data_segs_in`): exact on loopback, blind to pure
/// ACKs and their timing, and read from the test's own socket, where a
/// loopback capture would need privileges and a parser. A unit test, as
/// reading `TCP_INFO` takes `unsafe`, which only `sys` may hold.
#[test]
fn a_1_kib_file_leaves_in_one_segment() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use crate::sys::file::open_dir_path;
    use crate::sys::socket::{Probe, data_segments_in, local_addr, tcp_listen};

    /// The temp root, removed on drop (a failed assertion too).
    struct TempRoot(std::path::PathBuf);
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let root_dir =
        TempRoot(std::env::temp_dir().join(format!("dlh-segments-{}", std::process::id())));
    let dir = &root_dir.0;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let content: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("small.bin"), &content).unwrap();
    let config = format!(
        "listen 127.0.0.1:8080; host a.test {{ path / {{ static {}; }} }}",
        dir.display()
    );
    let core = Core::new(load(&config).unwrap());
    let root = open_dir_path(dir).unwrap();
    let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 16, Probe::First).unwrap();
    let addr = local_addr(listener.as_fd()).unwrap();
    let (log, _logs) = LogSink::channel();
    let mut driver = Driver::new(core, vec![listener], vec![root], 1, log).unwrap();
    let stop = driver.stop_handle();
    let server = std::thread::spawn(move || driver.run());

    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut segments = Vec::new();
    for _ in 0..3 {
        let before = data_segments_in(client.as_fd()).unwrap();
        client
            .write_all(b"GET /small.bin HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        let mut piece = [0u8; 4096];
        let body_at = loop {
            let n = client.read(&mut piece).unwrap();
            assert_ne!(n, 0, "closed after {response:?}");
            response.extend_from_slice(&piece[..n]);
            if let Some(at) = response.windows(4).position(|w| w == b"\r\n\r\n") {
                if response.len() >= at + 4 + content.len() {
                    break at + 4;
                }
            }
        };
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response[body_at..] == content[..], "the body differs");
        segments.push(data_segments_in(client.as_fd()).unwrap() - before);
    }
    assert_eq!(segments, [1, 1, 1], "data segments per response");

    stop.stop().unwrap();
    assert_eq!(server.join().unwrap().unwrap(), super::Exit::Stopped);
}

/// NFR-PERF-06, the other half: `MSG_MORE` only while a file body
/// follows. A head with no body behind it (a `HEAD`, a `304`, a proxied
/// response) goes out at once: with `MSG_MORE` the kernel would hold the
/// partial segment back (about 200 ms on Linux), so each such response on
/// a kept-alive loopback connection must arrive within 150 ms (slack for
/// a loaded CI machine).
#[test]
fn a_head_with_no_file_behind_it_is_not_held_back() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    use crate::sys::file::open_dir_path;
    use crate::sys::socket::{Probe, local_addr, tcp_listen};

    /// The temp root, removed on drop (a failed assertion too).
    struct TempRoot(std::path::PathBuf);
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Reads one response: its head, and `body` bytes after it.
    fn response(client: &mut TcpStream, body: usize) -> Vec<u8> {
        let mut got = Vec::new();
        let mut piece = [0u8; 4096];
        loop {
            if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                if got.len() >= at + 4 + body {
                    return got;
                }
            }
            let n = client.read(&mut piece).unwrap();
            assert_ne!(n, 0, "closed after {got:?}");
            got.extend_from_slice(&piece[..n]);
        }
    }

    // A keep-alive backend: each request head gets `ok`.
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_addr = backend.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in backend.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut got = Vec::new();
                let mut piece = [0u8; 4096];
                loop {
                    while let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        got.drain(..at + 4);
                        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                        if stream.write_all(ok).is_err() {
                            return;
                        }
                    }
                    match stream.read(&mut piece) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&piece[..n]),
                    }
                }
            });
        }
    });

    let root_dir =
        TempRoot(std::env::temp_dir().join(format!("dlh-no-hold-{}", std::process::id())));
    let dir = &root_dir.0;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("small.bin"), [7u8; 1024]).unwrap();
    let config = format!(
        "listen 127.0.0.1:8080; host a.test {{ path /p {{ proxy {backend_addr}; \
         max_request_body 1MiB; max_response_body 1MiB; }} path / {{ static {}; }} }}",
        dir.display()
    );
    let core = Core::new(load(&config).unwrap());
    let root = open_dir_path(dir).unwrap();
    let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 16, Probe::First).unwrap();
    let addr = local_addr(listener.as_fd()).unwrap();
    let (log, _logs) = LogSink::channel();
    let mut driver = Driver::new(core, vec![listener], vec![root], 1, log).unwrap();
    let stop = driver.stop_handle();
    let server = std::thread::spawn(move || driver.run());

    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // The file's ETag, from a first GET.
    client
        .write_all(b"GET /small.bin HTTP/1.1\r\nHost: a.test\r\n\r\n")
        .unwrap();
    let first = String::from_utf8_lossy(&response(&mut client, 1024)).into_owned();
    let etag = first
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("etag: "))
        .and_then(|l| l.get(6..))
        .unwrap_or_else(|| panic!("no ETag in {:?}", first.lines().next()))
        .to_owned();
    let conditional =
        format!("GET /small.bin HTTP/1.1\r\nHost: a.test\r\nIf-None-Match: {etag}\r\n\r\n");
    let cases: [(&[u8], &[u8], usize); 3] = [
        (
            b"HEAD /small.bin HTTP/1.1\r\nHost: a.test\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n",
            0,
        ),
        (conditional.as_bytes(), b"HTTP/1.1 304 Not Modified\r\n", 0),
        (
            b"GET /p/x HTTP/1.1\r\nHost: a.test\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n",
            2,
        ),
    ];
    for (request, status, body) in cases {
        for _ in 0..3 {
            let start = Instant::now();
            client.write_all(request).unwrap();
            let got = response(&mut client, body);
            let took = start.elapsed();
            assert!(got.starts_with(status), "{got:?}");
            assert!(
                took < Duration::from_millis(150),
                "{:?} took {took:?}",
                String::from_utf8_lossy(request)
            );
        }
    }

    stop.stop().unwrap();
    assert_eq!(server.join().unwrap().unwrap(), super::Exit::Stopped);
}
