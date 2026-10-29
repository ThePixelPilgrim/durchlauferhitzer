//! FR-LOG-07 and DRV-10 on a real kernel: a backend `socket()` that runs
//! out of descriptors is logged `socket_failed` (`DriverLog::SocketFailed`)
//! and is a failed connect, so the client gets a 502 (`BackendFailed {
//! Refused }`) on its keep-alive connection, and the loop goes on.
//!
//! The descriptor limit and table are process-wide, so this binary holds
//! exactly this one test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs::{self, File};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};

use durchlauferhitzer::sys::rlimit::{nofile, set_soft_nofile};
use durchlauferhitzer::{DriverLog, Exit, LogEntry};
use durchlauferhitzer_core::http::request::{Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{BackendFailure, BackendId, ConnId, Log, RequestSummary};

#[path = "support/server.rs"]
mod server;

#[allow(dead_code, reason = "shared with the sim's test binaries; we use part")]
#[path = "../../sim/tests/support/model_http.rs"]
mod model_http;

use server::{Server, backend, config, date_of, read_response, text};

#[test]
fn a_backend_socket_out_of_descriptors_is_logged_and_answered_502() {
    let (_backend, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let mut client = server.connect();
    // A first answer: the connection is accepted and its socket open.
    client
        .write_all(b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n")
        .unwrap();
    let response = read_response(&mut client);
    assert!(
        text(&response).starts_with("HTTP/1.1 404 "),
        "{}",
        text(&response)
    );

    // Take every free descriptor.
    let (soft, hard) = nofile().unwrap();
    let open = u64::try_from(fs::read_dir("/proc/self/fd").unwrap().count()).unwrap();
    set_soft_nofile(open + 16).unwrap();
    let mut fillers = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(err) if err.raw_os_error() == Some(libc::EMFILE) => break,
            Err(err) => panic!("{err}"),
        }
    }

    // The proxied request needs a socket the driver cannot get.
    client
        .write_all(b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .unwrap();
    let response = read_response(&mut client);
    drop(fillers);
    set_soft_nofile(soft).unwrap();
    assert_eq!(nofile().unwrap(), (soft, hard));
    let date = date_of(&response);
    assert_eq!(
        text(&response),
        text(&model_http::model_fixed(
            Status::BadGateway,
            Persistence::KeepAlive,
            BodyMode::Full,
            Some(&date)
        ))
    );

    drop(client);
    let (exit, entries) = server.stop();
    assert_eq!(exit, Exit::Stopped);
    let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let request = |host: &str, path: &str| RequestSummary {
        method: Method::Get,
        host: Some(host.to_owned()),
        path: path.to_owned(),
    };
    assert_eq!(
        entries,
        [
            LogEntry::Core(Log::Response {
                conn: ConnId(1),
                peer,
                status: Status::NotFound,
                request: Some(request("other.test", "/x")),
            }),
            LogEntry::Driver(DriverLog::SocketFailed {
                errno: libc::EMFILE
            }),
            LogEntry::Core(Log::BackendFailed {
                conn: ConnId(1),
                peer,
                backend: BackendId(0),
                kind: BackendFailure::Refused,
            }),
            LogEntry::Core(Log::Response {
                conn: ConnId(1),
                peer,
                status: Status::BadGateway,
                request: Some(request("example.test", "/api/x")),
            }),
        ]
    );
}
