//! Review Focus 1 on the event loop: a client that resets mid-relay. No
//! panic, the upstream is closed, and the process's descriptor count
//! returns to its value before the exchange, while the driver runs.
//!
//! The count (`/proc/self/fd`) is shared by every thread of the process,
//! so this binary holds exactly this one test (R4): no other test can
//! open or close descriptors while it counts.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::thread;

use durchlauferhitzer::{Exit, LogEntry};
use durchlauferhitzer_core::http::request::Method;
use durchlauferhitzer_core::{BackendId, ConnId, Log, RequestSummary};

#[path = "support/server.rs"]
mod server;

use server::{Server, accept, backend, config, eventually, read_head};

fn open_fds() -> usize {
    fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn client_reset_mid_relay_closes_the_upstream_and_leaks_no_fd() {
    let (backend_listener, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let baseline = open_fds();

    let mut client = server.connect();
    client
        .write_all(b"GET /api/big HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .unwrap();
    let mut upstream = accept(&backend_listener);
    read_head(&mut upstream);
    // A long body the backend keeps writing until its socket fails.
    let feeder = thread::spawn(move || {
        upstream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1073741824\r\n\r\n")
            .unwrap();
        let chunk = vec![b'b'; 64 * 1024];
        while upstream.write_all(&chunk).is_ok() {}
    });
    // Take part of the relayed body, then close with relayed bytes still
    // unread: the client's kernel resets the connection.
    let mut part = vec![0u8; 256 * 1024];
    client.read_exact(&mut part).unwrap();
    drop(client);

    // The upstream was closed: the backend's writes fail and its feeder
    // ends.
    feeder.join().unwrap();
    assert!(
        eventually(|| open_fds() == baseline),
        "descriptors: {} open, {baseline} before the exchange",
        open_fds()
    );
    let (exit, logs) = server.stop();
    assert_eq!(exit, Exit::Stopped);
    // The core saw the reset, after the proxied response's entry.
    let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
    assert_eq!(
        logs,
        [
            LogEntry::Core(Log::ProxiedResponse {
                conn: ConnId(1),
                peer,
                backend: BackendId(0),
                status: 200,
                request: RequestSummary {
                    method: Method::Get,
                    host: Some("example.test".to_owned()),
                    path: "/api/big".to_owned(),
                },
            }),
            LogEntry::Core(Log::Reset {
                conn: ConnId(1),
                peer,
            }),
        ]
    );
}
