//! Backend connection pooling through the built binary (FR-PROXY-02,
//! FR-PROXY-26; stage 7 Task 5, R5), under the seccomp kill switch when
//! it is on: a keep-alive stub backend counts its connections. Sequential
//! requests (each on a new client connection) share one backend
//! connection, or get one each with `pool off`; an idle connection the
//! backend closes is not reused; a reused connection the backend kills as
//! the request arrives is retried once for a GET, and the retry is logged.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use child::{Containment, PATIENCE, Running, Setup, TempDir, free_port, get};

const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

/// What the stub backend does with each connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Serve {
    /// Answers every request, keeping the connection open.
    KeepAlive,
    /// Answers one request, then ends its stream while the connection is
    /// idle, and waits for the proxy to close its end.
    CloseWhenIdle,
    /// Closes the connection when its second request arrives, without an
    /// answer (a connection that died just as it was reused).
    DieOnSecond,
}

/// A keep-alive stub backend: counts the connections it accepted; each
/// connection's end is reported on `closed`.
struct Backend {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    closed: Receiver<()>,
}

impl Backend {
    fn start(serve: Serve) -> Backend {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let (sender, closed) = mpsc::channel();
        let count = Arc::clone(&accepted);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                count.fetch_add(1, Ordering::SeqCst);
                let sender = sender.clone();
                thread::spawn(move || {
                    serve_connection(stream, serve);
                    let _ = sender.send(());
                });
            }
        });
        Backend {
            addr,
            accepted,
            closed,
        }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// Answers each complete request head on `stream` (no bodies here) by
/// `serve`, until the proxy closes the connection.
fn serve_connection(mut stream: TcpStream, serve: Serve) {
    stream.set_read_timeout(Some(PATIENCE * 4)).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut served = 0;
    loop {
        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            buf.drain(..end + 4);
            if serve == Serve::DieOnSecond && served == 1 {
                return;
            }
            stream.write_all(RESPONSE).unwrap();
            served += 1;
            if serve == Serve::CloseWhenIdle {
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest);
                assert!(rest.is_empty(), "a request on a closed connection");
                return;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The server, proxying `/` to `backend` (`pool off` when `pooled` is
/// false).
fn server(dir: &TempDir, backend: SocketAddr, pooled: bool) -> (Running, SocketAddr) {
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let pool = if pooled { "" } else { " pool off;" };
    let conf = format!(
        "listen {addr};
host example.test {{
  path / {{ proxy {backend};{pool} max_request_body 1MiB; max_response_body 1MiB; }}
}}
"
    );
    let path = dir.config("pool.conf", &conf, Containment::Switch);
    let running = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    (running, addr)
}

fn ok(response: &str) {
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n") && response.ends_with("\r\n\r\nok"),
        "{response:?}"
    );
}

/// FR-PROXY-02: N requests, each from a new client connection, over one
/// backend connection; with `pool off` one each.
#[test]
fn sequential_requests_share_one_backend_connection() {
    const N: usize = 10;
    for (pooled, expected) in [(true, 1), (false, N)] {
        let dir = TempDir::new();
        let backend = Backend::start(Serve::KeepAlive);
        let (running, addr) = server(&dir, backend.addr, pooled);
        for _ in 0..N {
            ok(&get(addr, "/x"));
        }
        assert_eq!(backend.accepted(), expected, "pooled: {pooled}");
        let log = running.kill();
        assert!(!log.contains("kind=backend_"), "{log}");
    }
}

/// Review Focus 1: an idle connection the backend closes is closed, not
/// reused: the next request opens a new one, with no retry.
#[test]
fn an_idle_connection_the_backend_closes_is_not_reused() {
    let dir = TempDir::new();
    let backend = Backend::start(Serve::CloseWhenIdle);
    let (running, addr) = server(&dir, backend.addr, true);
    ok(&get(addr, "/x"));
    // The proxy closed its end of the idle connection.
    backend.closed.recv_timeout(PATIENCE).unwrap();
    ok(&get(addr, "/x"));
    assert_eq!(backend.accepted(), 2);
    let log = running.kill();
    assert!(!log.contains("kind=backend_"), "{log}");
}

/// FR-PROXY-26: a GET on a reused connection that dies before any
/// response byte is replayed once on a fresh connection, and logged
/// (conn 1 is the readiness probe of `Running::start`).
#[test]
fn a_get_on_a_dead_reused_connection_is_retried() {
    let dir = TempDir::new();
    let backend = Backend::start(Serve::DieOnSecond);
    let (running, addr) = server(&dir, backend.addr, true);
    ok(&get(addr, "/x"));
    ok(&get(addr, "/x"));
    assert_eq!(backend.accepted(), 2);
    let log = running.kill();
    assert!(
        log.lines().any(|line| {
            line.starts_with("kind=backend_retried conn=3 ")
                && (line.ends_with(" failure=closed_early") || line.ends_with(" failure=reset"))
        }),
        "{log}"
    );
    assert!(!log.contains("kind=backend_failed"), "{log}");
}
