//! The loop on loopback relaying a proxied body: how many `epoll_ctl`
//! calls it costs (NFR-PERF-05, owner decision 2026-11-07). Counted by
//! the epoll wrapper itself, as `strace` is not at hand in a unit test.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::time::Duration;

use durchlauferhitzer_core::Core;
use durchlauferhitzer_core::config::load;

use super::{Driver, Exit};
use crate::log::LogSink;
use crate::sys::socket::{Probe, local_addr, tcp_listen};

/// The relayed body: 64 chunks of the 16 KiB upstream buffer.
const BODY: usize = 1024 * 1024;

/// A keep-alive backend answering every request head with a `BODY`-byte
/// 200, on its own thread.
fn backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {BODY}\r\n\r\n").into_bytes();
    response.resize(response.len() + BODY, b'b');
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let response = response.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut piece = [0u8; 4096];
                loop {
                    let Ok(n) = stream.read(&mut piece) else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&piece[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        head.clear();
                        if stream.write_all(&response).is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

/// One GET over `client`; the whole response is read.
fn fetch(client: &mut TcpStream) {
    client
        .write_all(b"GET /big HTTP/1.1\r\nHost: a.test\r\n\r\n")
        .unwrap();
    let mut response = Vec::new();
    let mut piece = vec![0u8; 256 * 1024];
    loop {
        let n = client.read(&mut piece).unwrap();
        assert_ne!(n, 0, "closed after {} bytes", response.len());
        response.extend_from_slice(&piece[..n]);
        if let Some(at) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            if response.len() >= at + 4 + BODY {
                assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
                assert_eq!(response.len(), at + 4 + BODY, "bytes after the body");
                return;
            }
        }
    }
}

/// Today's relay re-armed both sockets for every 16 KiB chunk (about 4
/// `epoll_ctl` per chunk, 263 per MiB in the stage 7 stop measurement).
/// A kept-alive client and a pooled backend connection relay 1 MiB with a
/// handful; a few more are allowed for the times a socket really blocks.
#[test]
fn a_proxied_body_is_relayed_without_epoll_ctl_per_chunk() {
    let backend = backend();
    let config = format!(
        "listen 127.0.0.1:8080; host a.test {{ path / {{ proxy {backend}; \
         max_request_body 1MiB; max_response_body unlimited; }} }}"
    );
    let core = Core::new(load(&config).unwrap());
    let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 16, Probe::First).unwrap();
    let addr = local_addr(listener.as_fd()).unwrap();
    let (log, _logs) = LogSink::channel();
    let mut driver = Driver::new(core, vec![listener], Vec::new(), 1, log).unwrap();
    let ctl_calls = driver.epoll.ctl_calls();
    let stop = driver.stop_handle();
    let server = std::thread::spawn(move || driver.run());

    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // The first exchange connects (and pools) the backend connection.
    fetch(&mut client);
    let mut counts = Vec::new();
    for _ in 0..3 {
        let before = ctl_calls.get();
        fetch(&mut client);
        counts.push(ctl_calls.get() - before);
    }
    stop.stop().unwrap();
    assert_eq!(server.join().unwrap().unwrap(), Exit::Stopped);
    let fewest = counts.iter().copied().min().unwrap();
    assert!(fewest <= 16, "epoll_ctl per proxied MiB: {counts:?}");
}
