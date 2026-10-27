//! A driver on loopback for the event-loop tests: the real `Driver`
//! running in a thread on an ephemeral listener, a config with no static
//! roots (one proxy route to a std `TcpListener` backend), and a channel
//! log sink. Shared by `tests/loop_static_less.rs` and
//! `tests/loop_fd_leak.rs` (`#[path]` include).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code,
    reason = "test helpers outside #[test] functions; each test binary uses part"
)]

use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use durchlauferhitzer::sys::socket::{local_addr, tcp_listen};
use durchlauferhitzer::{Driver, Exit, LogEntry, LogSink, StopHandle};
use durchlauferhitzer_core::Core;
use durchlauferhitzer_core::config::load;

/// How long any single wait in these tests may take before it fails.
pub const PATIENCE: Duration = Duration::from_secs(5);

/// The config of every test: one host, one proxy route to `backend`, a
/// short header timeout (R5), no static roots.
pub fn config(backend: SocketAddr) -> String {
    format!(
        "listen 127.0.0.1:8080;
limits {{ header_timeout 400ms; }}
host example.test {{
  path /api {{ proxy {backend}; strip_prefix; max_request_body 8MiB; max_response_body unlimited; }}
}}"
    )
}

/// A running driver.
pub struct Server {
    pub addr: SocketAddr,
    pub logs: Receiver<LogEntry>,
    stop: StopHandle,
    thread: JoinHandle<io::Result<Exit>>,
}

impl Server {
    /// Starts a driver for `config` on an ephemeral loopback port.
    pub fn start(config: &str) -> Server {
        let core = Core::new(load(config).unwrap());
        let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 128).unwrap();
        let addr = local_addr(std::os::fd::AsFd::as_fd(&listener)).unwrap();
        let (log, logs) = LogSink::channel();
        let mut driver = Driver::new(core, vec![listener], Vec::new(), None, log).unwrap();
        let stop = driver.stop_handle();
        let thread = thread::spawn(move || driver.run());
        Server {
            addr,
            logs,
            stop,
            thread,
        }
    }

    pub fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        stream.set_write_timeout(Some(PATIENCE)).unwrap();
        stream
    }

    /// Stops the loop and returns how it ended, with every log entry it
    /// wrote that was not taken yet; the driver (and every descriptor it
    /// holds) is dropped when its thread ends.
    pub fn stop(self) -> (Exit, Vec<LogEntry>) {
        self.stop.stop().unwrap();
        let exit = self.thread.join().unwrap().unwrap();
        (exit, self.logs.try_iter().collect())
    }
}

/// A backend listener on an ephemeral loopback port.
pub fn backend() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Accepts one connection on `listener` (blocking, with timeouts set).
pub fn accept(listener: &TcpListener) -> TcpStream {
    let (stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.set_write_timeout(Some(PATIENCE)).unwrap();
    stream
}

/// Reads up to and including the blank line that ends a head.
pub fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).unwrap();
        assert_eq!(n, 1, "stream ended inside a head: {:?}", text(&head));
        head.push(byte[0]);
    }
    head
}

/// Reads one response with a `Content-Length` body: head and body bytes.
pub fn read_response(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = read_head(stream);
    let length = content_length(&response).expect("a response with Content-Length");
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    response.extend_from_slice(&body);
    response
}

/// The `Content-Length` of a head, any case.
pub fn content_length(head: &[u8]) -> Option<usize> {
    text(head).split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())?
    })
}

/// The value of the `Date` line of a response the core rendered
/// (`Date: <IMF-fixdate>`, FR-HTTP-22a); checks its shape.
pub fn date_of(response: &[u8]) -> String {
    let date = text(response)
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Date: "))
        .expect("a Date line")
        .to_owned();
    assert_eq!(date.len(), 29, "{date:?}");
    assert!(date.ends_with(" GMT"), "{date:?}");
    date
}

/// Reads until the peer's end of stream; `Err` if the stream was reset.
pub fn read_to_eof(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    stream.read_to_end(&mut out)?;
    Ok(out)
}

/// Polls `cond` until it holds or `PATIENCE` has passed.
pub fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < PATIENCE {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    cond()
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
